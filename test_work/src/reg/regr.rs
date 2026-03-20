use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time;
use reqwest;
use sysinfo::{System, SystemExt, CpuExt, ProcessExt, NetworkExt, NetworksExt, DiskExt};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use chrono::{DateTime, Local};
use serde_json::json;
use tracing::{info, warn, error, debug, Level};
use tracing_subscriber;
use plotters::prelude::*;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchmarkResult {
    benchmark_id: String,
    commit_hash: String,
    device_id: String,
    timestamp: u64,
    metrics: Vec<Metric>,
    environment: EnvironmentInfo,
    build_info: BuildInfo,
    tags: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildInfo {
    rust_version: String,
    target: String,
    optimization_level: String,
    debug_symbols: bool,
    build_time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Metric {
    name: String,
    value: f64,
    unit: MetricUnit,
    tags: HashMap<String, String>,
    confidence_interval: Option<(f64, f64)>,
    samples: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum MetricUnit {
    Nanoseconds,
    Microseconds,
    Milliseconds,
    Seconds,
    Bytes,
    BytesPerSecond,
    Percentage,
    FPS,
    Count,
    Ratio,
    Temperature,
    Frequency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvironmentInfo {
    os_version: String,
    kernel_version: String,
    cpu_cores: usize,
    cpu_model: String,
    cpu_freq_mhz: u64,
    total_ram_mb: u64,
    battery_level: u8,
    temperature_celsius: Option<f32>,
    thermal_throttling: bool,
    disk_free_mb: u64,
    network_interfaces: Vec<String>,
    process_count: usize,
    uptime_seconds: u64,
}

#[derive(Debug, Clone)]
struct SystemSnapshot {
    cpu_usage_percent: f64,
    cpu_temp_celsius: Option<f32>,
    memory_used_mb: u64,
    memory_total_mb: u64,
    swap_used_mb: u64,
    processes: usize,
    threads: usize,
    disk_read_mb_s: f64,
    disk_write_mb_s: f64,
    network_rx_kb_s: f64,
    network_tx_kb_s: f64,
    timestamp: u64,
    context_switches: u64,
    interrupts: u64,
}

struct SystemProfiler {
    sample_interval: Duration,
    system: System,
    history: VecDeque<SystemSnapshot>,
    max_history: usize,
}

#[derive(Debug)]
struct BenchmarkConfig {
    id: String,
    command: String,
    args: Vec<String>,
    env_vars: HashMap<String, String>,
    working_dir: Option<PathBuf>,
    warmup_iterations: usize,
    measure_iterations: usize,
    timeout: Duration,
    required_resources: Vec<String>,
    min_samples: usize,
    max_sample_variance: f64,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            command: String::new(),
            args: Vec::new(),
            env_vars: HashMap::new(),
            working_dir: None,
            warmup_iterations: 3,
            measure_iterations: 10,
            timeout: Duration::from_secs(30),
            required_resources: Vec::new(),
            min_samples: 5,
            max_sample_variance: 0.15,
        }
    }
}

struct Benchmark {
    config: BenchmarkConfig,
    profiler: Arc<Mutex<SystemProfiler>>,
}

impl SystemProfiler {
    fn new(interval_ms: u64, max_history: usize) -> Self {
        Self {
            sample_interval: Duration::from_millis(interval_ms),
            system: System::new_all(),
            history: VecDeque::with_capacity(max_history),
            max_history,
        }
    }

    async fn start_profiling(&mut self) -> mpsc::Receiver<SystemSnapshot> {
        let (tx, rx) = mpsc::channel(1000);
        let interval = self.sample_interval;
        let max_history = self.max_history;

        tokio::spawn(async move {
            let mut interval_timer = time::interval(interval);
            let mut system = System::new_all();
            let mut last_net_rx = 0u64;
            let mut last_net_tx = 0u64;
            let mut last_disk_read = 0u64;
            let mut last_disk_write = 0u64;
            let mut last_time = Instant::now();

            loop {
                interval_timer.tick().await;
                system.refresh_all();

                let now = Instant::now();
                let elapsed = now.duration_since(last_time).as_secs_f64();

                let networks = system.networks();
                let mut total_rx = 0;
                let mut total_tx = 0;
                for (_, data) in networks.iter() {
                    total_rx += data.received();
                    total_tx += data.transmitted();
                }

                let disks = system.disks();
                let mut total_read = 0;
                let mut total_write = 0;
                for disk in disks {
                    total_read += disk.read_bytes();
                    total_write += disk.write_bytes();
                }

                let net_rx_rate = if elapsed > 0.0 {
                    (total_rx - last_net_rx) as f64 / elapsed / 1024.0
                } else {
                    0.0
                };

                let net_tx_rate = if elapsed > 0.0 {
                    (total_tx - last_net_tx) as f64 / elapsed / 1024.0
                } else {
                    0.0
                };

                let disk_read_rate = if elapsed > 0.0 {
                    (total_read - last_disk_read) as f64 / elapsed / 1024.0 / 1024.0
                } else {
                    0.0
                };

                let disk_write_rate = if elapsed > 0.0 {
                    (total_write - last_disk_write) as f64 / elapsed / 1024.0 / 1024.0
                } else {
                    0.0
                };

                last_net_rx = total_rx;
                last_net_tx = total_tx;
                last_disk_read = total_read;
                last_disk_write = total_write;
                last_time = now;

                let ctx_switches = get_context_switches();
                let interrupts = get_interrupts();

                let snapshot = SystemSnapshot {
                    cpu_usage_percent: system.global_cpu_info().cpu_usage() as f64,
                    cpu_temp_celsius: get_cpu_temperature(),
                    memory_used_mb: system.used_memory() / 1024 / 1024,
                    memory_total_mb: system.total_memory() / 1024 / 1024,
                    swap_used_mb: system.used_swap() / 1024 / 1024,
                    processes: system.processes().len(),
                    threads: system.processes().values().map(|p| p.threads().len()).sum(),
                    disk_read_mb_s: disk_read_rate,
                    disk_write_mb_s: disk_write_rate,
                    network_rx_kb_s: net_rx_rate,
                    network_tx_kb_s: net_tx_rate,
                    timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                    context_switches: ctx_switches,
                    interrupts: interrupts,
                };

                if tx.send(snapshot.clone()).await.is_err() {
                    break;
                }
            }
        });

        rx
    }

    fn add_to_history(&mut self, snapshot: SystemSnapshot) {
        self.history.push_back(snapshot);
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
    }

    fn get_stats(&self, duration: Duration) -> HashMap<String, f64> {
        let mut stats = HashMap::new();
        let cutoff = Instant::now() - duration;

        let relevant: Vec<_> = self.history
            .iter()
            .filter(|s| Duration::from_secs(s.timestamp) > cutoff.elapsed())
            .collect();

        if relevant.is_empty() {
            return stats;
        }

        let cpu_avg: f64 = relevant.iter().map(|s| s.cpu_usage_percent).sum::<f64>() / relevant.len() as f64;
        let mem_avg: f64 = relevant.iter().map(|s| s.memory_used_mb as f64).sum::<f64>() / relevant.len() as f64;
        let cpu_max = relevant.iter().map(|s| s.cpu_usage_percent).fold(0.0, f64::max);

        stats.insert("cpu.avg".to_string(), cpu_avg);
        stats.insert("cpu.max".to_string(), cpu_max);
        stats.insert("memory.avg_mb".to_string(), mem_avg);

        stats
    }
}

impl Benchmark {
    fn new(config: BenchmarkConfig) -> Self {
        Self {
            config,
            profiler: Arc::new(Mutex::new(SystemProfiler::new(100, 1000))),
        }
    }

    async fn run(&self) -> Result<Vec<Metric>, Box<dyn std::error::Error>> {
        info!("Running benchmark: {}", self.config.id);

        let mut metrics = Vec::new();
        let mut profiler = self.profiler.lock().await;
        let mut snapshot_rx = profiler.start_profiling().await;

        self.check_resources().await?;

        info!("Warmup iterations: {}", self.config.warmup_iterations);
        for i in 0..self.config.warmup_iterations {
            debug!("Warmup {}/{}", i + 1, self.config.warmup_iterations);
            let _ = self.execute_once().await?;
            time::sleep(Duration::from_millis(100)).await;
        }

        let mut execution_times = Vec::with_capacity(self.config.measure_iterations);
        let mut cpu_samples = Vec::new();
        let mut mem_samples = Vec::new();
        let mut temp_samples = Vec::new();

        info!("Measurement iterations: {}", self.config.measure_iterations);
        for i in 0..self.config.measure_iterations {
            debug!("Measurement {}/{}", i + 1, self.config.measure_iterations);

            let start = Instant::now();
            let output = self.execute_once().await?;
            let duration = start.elapsed();

            execution_times.push(duration.as_nanos() as f64);

            time::sleep(Duration::from_millis(50)).await;

            while let Ok(snapshot) = snapshot_rx.try_recv() {
                cpu_samples.push(snapshot.cpu_usage_percent);
                mem_samples.push(snapshot.memory_used_mb as f64);
                if let Some(temp) = snapshot.cpu_temp_celsius {
                    temp_samples.push(temp as f64);
                }
                profiler.add_to_history(snapshot);
            }
        }

        metrics.extend(self.analyze_execution_times(&execution_times)?);
        metrics.extend(self.analyze_resource_usage(&cpu_samples, &mem_samples, &temp_samples)?);

        let stability = self.check_stability(&execution_times);
        if stability > 0.15 {
            warn!("High variance in benchmark {}: {:.2}%", self.config.id, stability * 100.0);
        }

        Ok(metrics)
    }

    async fn execute_once(&self) -> Result<String, Box<dyn std::error::Error>> {
        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        for (key, value) in &self.config.env_vars {
            cmd.env(key, value);
        }

        if let Some(dir) = &self.config.working_dir {
            cmd.current_dir(dir);
        }

        let output = tokio::task::spawn_blocking(move || {
            cmd.output()
        }).await??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Benchmark failed: {}", stderr);
            return Err(format!("Benchmark failed: {}", stderr).into());
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn check_resources(&self) -> Result<(), Box<dyn std::error::Error>> {
        let system = System::new_all();

        for resource in &self.config.required_resources {
            match resource.as_str() {
                "root" => {
                    if !nix::unistd::Uid::effective().is_root() {
                        return Err("Root privileges required".into());
                    }
                }
                "network" => {
                    if system.networks().iter().count() == 0 {
                        return Err("No network interfaces available".into());
                    }
                }
                "disk_space" => {
                    let free = fs2::statvfs("/").map(|s| s.free_space()).unwrap_or(0);
                    if free < 100 * 1024 * 1024 {
                        return Err("Insufficient disk space".into());
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn analyze_execution_times(&self, times: &[f64]) -> Result<Vec<Metric>, Box<dyn std::error::Error>> {
        if times.is_empty() {
            return Ok(Vec::new());
        }

        let mut metrics = Vec::new();
        let mut tags = HashMap::new();
        tags.insert("benchmark".to_string(), self.config.id.clone());

        let mean = times.iter().sum::<f64>() / times.len() as f64;
        let median = percentile(times, 0.5);
        let p90 = percentile(times, 0.9);
        let p95 = percentile(times, 0.95);
        let p99 = percentile(times, 0.99);
        let min = times.iter().fold(f64::INFINITY, |a, &b| a.min(b));
        let max = times.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));

        let std_dev = (times.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / times.len() as f64).sqrt();
        let ci = 1.96 * std_dev / (times.len() as f64).sqrt();

        metrics.push(Metric {
            name: format!("{}.exec_time.mean", self.config.id),
            value: mean,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: Some((mean - ci, mean + ci)),
            samples: Some(times.to_vec()),
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.median", self.config.id),
            value: median,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.p90", self.config.id),
            value: p90,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.p95", self.config.id),
            value: p95,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.p99", self.config.id),
            value: p99,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.min", self.config.id),
            value: min,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.max", self.config.id),
            value: max,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        metrics.push(Metric {
            name: format!("{}.exec_time.std_dev", self.config.id),
            value: std_dev,
            unit: MetricUnit::Nanoseconds,
            tags: tags.clone(),
            confidence_interval: None,
            samples: None,
        });

        Ok(metrics)
    }

    fn analyze_resource_usage(&self, cpu: &[f64], mem: &[f64], temp: &[f32]) -> Result<Vec<Metric>, Box<dyn std::error::Error>> {
        let mut metrics = Vec::new();
        let mut tags = HashMap::new();
        tags.insert("benchmark".to_string(), self.config.id.clone());

        if !cpu.is_empty() {
            let cpu_mean = cpu.iter().sum::<f64>() / cpu.len() as f64;
            let cpu_max = cpu.iter().fold(0.0, |a, &b| a.max(b));

            metrics.push(Metric {
                name: format!("{}.cpu.mean", self.config.id),
                value: cpu_mean,
                unit: MetricUnit::Percentage,
                tags: tags.clone(),
                confidence_interval: None,
                samples: Some(cpu.to_vec()),
            });

            metrics.push(Metric {
                name: format!("{}.cpu.max", self.config.id),
                value: cpu_max,
                unit: MetricUnit::Percentage,
                tags: tags.clone(),
                confidence_interval: None,
                samples: None,
            });
        }

        if !mem.is_empty() {
            let mem_mean = mem.iter().sum::<f64>() / mem.len() as f64;

            metrics.push(Metric {
                name: format!("{}.memory.mean_mb", self.config.id),
                value: mem_mean,
                unit: MetricUnit::Bytes,
                tags: tags.clone(),
                confidence_interval: None,
                samples: Some(mem.to_vec()),
            });
        }

        if !temp.is_empty() {
            let temp_mean = temp.iter().map(|&t| t as f64).sum::<f64>() / temp.len() as f64;
            let temp_max = temp.iter().fold(0.0f32, |a, &b| a.max(b)) as f64;

            metrics.push(Metric {
                name: format!("{}.temperature.mean", self.config.id),
                value: temp_mean,
                unit: MetricUnit::Temperature,
                tags: tags.clone(),
                confidence_interval: None,
                samples: None,
            });

            metrics.push(Metric {
                name: format!("{}.temperature.max", self.config.id),
                value: temp_max,
                unit: MetricUnit::Temperature,
                tags: tags,
                confidence_interval: None,
                samples: None,
            });
        }

        Ok(metrics)
    }

    fn check_stability(&self, times: &[f64]) -> f64 {
        if times.len() < 2 {
            return 1.0;
        }

        let mean = times.iter().sum::<f64>() / times.len() as f64;
        let variance = times.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / times.len() as f64;
        let std_dev = variance.sqrt();

        std_dev / mean
    }
}

struct BenchmarkRunner {
    profiler: Arc<Mutex<SystemProfiler>>,
    client: reqwest::Client,
    server_url: String,
    device_id: String,
    results_dir: PathBuf,
}

impl BenchmarkRunner {
    fn new(server_url: &str, device_id: &str, results_dir: &Path) -> Self {
        fs::create_dir_all(results_dir).unwrap();

        Self {
            profiler: Arc::new(Mutex::new(SystemProfiler::new(100, 10000))),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            server_url: server_url.to_string(),
            device_id: device_id.to_string(),
            results_dir: results_dir.to_path_buf(),
        }
    }

    async fn run_benchmark(&self, benchmark: &Benchmark, commit_hash: &str, tags: HashMap<String, String>) -> Result<BenchmarkResult, Box<dyn std::error::Error>> {
        info!("Starting benchmark: {}", benchmark.config.id);

        let start_time = Instant::now();
        let metrics = benchmark.run().await?;
        let duration = start_time.elapsed();

        info!("Benchmark completed in {:?}", duration);

        let environment = self.collect_environment_info().await?;

        let build_info = BuildInfo {
            rust_version: rustc_version::version().unwrap_or_default().to_string(),
            target: std::env::consts::ARCH.to_string(),
            optimization_level: std::env::var("OPT_LEVEL").unwrap_or_else(|_| "debug".to_string()),
            debug_symbols: cfg!(debug_assertions),
            build_time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        };

        let result = BenchmarkResult {
            benchmark_id: benchmark.config.id.clone(),
            commit_hash: commit_hash.to_string(),
            device_id: self.device_id.clone(),
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            metrics,
            environment,
            build_info,
            tags,
        };

        self.save_result_local(&result).await?;
        self.send_result_remote(&result).await?;

        Ok(result)
    }

    async fn collect_environment_info(&self) -> Result<EnvironmentInfo, Box<dyn std::error::Error>> {
        let system = System::new_all();

        let cpu_model = if let Some(cpu) = system.cpus().first() {
            cpu.brand().to_string()
        } else {
            "unknown".to_string()
        };

        let uptime = get_system_uptime();

        let networks: Vec<String> = system.networks()
            .iter()
            .map(|(name, _)| name.clone())
            .collect();

        Ok(EnvironmentInfo {
            os_version: std::env::consts::OS.to_string(),
            kernel_version: get_kernel_version(),
            cpu_cores: num_cpus::get(),
            cpu_model,
            cpu_freq_mhz: 0,
            total_ram_mb: system.total_memory() / 1024 / 1024,
            battery_level: get_battery_level(),
            temperature_celsius: get_cpu_temperature(),
            thermal_throttling: is_thermal_throttling(),
            disk_free_mb: fs2::statvfs("/").map(|s| s.free_space() / 1024 / 1024).unwrap_or(0),
            network_interfaces: networks,
            process_count: system.processes().len(),
            uptime_seconds: uptime.as_secs(),
        })
    }

    async fn save_result_local(&self, result: &BenchmarkResult) -> Result<(), Box<dyn std::error::Error>> {
        let filename = format!("{}_{}_{}.json",
            result.benchmark_id,
            result.commit_hash,
            result.timestamp
        );

        let path = self.results_dir.join(filename);
        let json = serde_json::to_string_pretty(result)?;
        fs::write(path, json)?;

        info!("Results saved locally");
        Ok(())
    }

    async fn send_result_remote(&self, result: &BenchmarkResult) -> Result<(), Box<dyn std::error::Error>> {
        if self.server_url.is_empty() {
            return Ok(());
        }

        let response = self.client
            .post(&format!("{}/api/results", self.server_url))
            .json(result)
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                info!("Results sent to server");
            }
            Ok(resp) => {
                warn!("Server returned error: {}", resp.status());
            }
            Err(e) => {
                warn!("Failed to send results: {}", e);
            }
        }

        Ok(())
    }
}

struct RegressionDetector {
    baseline: HashMap<String, HistoricalData>,
    threshold_percent: f64,
    min_samples: usize,
}

struct HistoricalData {
    values: VecDeque<f64>,
    timestamps: VecDeque<u64>,
    mean: f64,
    std_dev: f64,
}

impl RegressionDetector {
    fn new(threshold_percent: f64, min_samples: usize) -> Self {
        Self {
            baseline: HashMap::new(),
            threshold_percent,
            min_samples,
        }
    }

    fn add_sample(&mut self, benchmark_id: &str, value: f64, timestamp: u64) {
        let data = self.baseline
            .entry(benchmark_id.to_string())
            .or_insert_with(|| HistoricalData {
                values: VecDeque::with_capacity(1000),
                timestamps: VecDeque::with_capacity(1000),
                mean: 0.0,
                std_dev: 0.0,
            });

        data.values.push_back(value);
        data.timestamps.push_back(timestamp);

        while data.values.len() > 1000 {
            data.values.pop_front();
            data.timestamps.pop_front();
        }

        self.update_stats(benchmark_id);
    }

    fn update_stats(&mut self, benchmark_id: &str) {
        if let Some(data) = self.baseline.get_mut(benchmark_id) {
            if data.values.len() >= self.min_samples {
                let sum: f64 = data.values.iter().sum();
                data.mean = sum / data.values.len() as f64;

                let variance = data.values.iter()
                    .map(|&x| (x - data.mean).powi(2))
                    .sum::<f64>() / data.values.len() as f64;
                data.std_dev = variance.sqrt();
            }
        }
    }

    fn check_regression(&self, benchmark_id: &str, new_value: f64) -> Option<RegressionInfo> {
        if let Some(data) = self.baseline.get(benchmark_id) {
            if data.values.len() < self.min_samples {
                return None;
            }

            let change_percent = ((new_value - data.mean) / data.mean) * 100.0;
            let z_score = (new_value - data.mean) / data.std_dev;

            if change_percent.abs() > self.threshold_percent {
                let severity = if change_percent.abs() > self.threshold_percent * 3.0 {
                    "CRITICAL"
                } else if change_percent.abs() > self.threshold_percent * 2.0 {
                    "SEVERE"
                } else {
                    "WARNING"
                };

                return Some(RegressionInfo {
                    benchmark_id: benchmark_id.to_string(),
                    baseline_mean: data.mean,
                    new_value,
                    change_percent,
                    z_score,
                    severity: severity.to_string(),
                    confidence: self.calculate_confidence(&data.values, new_value),
                });
            }
        }

        None
    }

    fn calculate_confidence(&self, samples: &VecDeque<f64>, new_value: f64) -> f64 {
        let n = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / n;
        let variance = samples.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        let std_err = (variance / n).sqrt();

        let t_stat = (new_value - mean).abs() / std_err;
        let df = n - 1.0;

        1.0 - t_stat / (t_stat + df)
    }

    fn get_trend(&self, benchmark_id: &str, window: usize) -> Option<TrendInfo> {
        if let Some(data) = self.baseline.get(benchmark_id) {
            if data.values.len() < window {
                return None;
            }

            let recent: Vec<f64> = data.values.iter().rev().take(window).cloned().collect();
            let older: Vec<f64> = data.values.iter().rev().skip(window).take(window).cloned().collect();

            if recent.is_empty() || older.is_empty() {
                return None;
            }

            let recent_mean = recent.iter().sum::<f64>() / recent.len() as f64;
            let older_mean = older.iter().sum::<f64>() / older.len() as f64;

            let slope = (recent_mean - older_mean) / window as f64;
            let direction = if slope > 0.01 { "increasing" } else if slope < -0.01 { "decreasing" } else { "stable" };

            Some(TrendInfo {
                benchmark_id: benchmark_id.to_string(),
                slope,
                direction: direction.to_string(),
                recent_mean,
                older_mean,
            })
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct RegressionInfo {
    benchmark_id: String,
    baseline_mean: f64,
    new_value: f64,
    change_percent: f64,
    z_score: f64,
    severity: String,
    confidence: f64,
}

#[derive(Debug)]
struct TrendInfo {
    benchmark_id: String,
    slope: f64,
    direction: String,
    recent_mean: f64,
    older_mean: f64,
}

struct BenchmarkScheduler {
    benchmarks: Vec<Benchmark>,
    runner: Arc<BenchmarkRunner>,
    detector: Arc<Mutex<RegressionDetector>>,
    schedule: Vec<ScheduledRun>,
    watcher: Option<RecommendedWatcher>,
}

struct ScheduledRun {
    benchmark_id: String,
    interval: Duration,
    last_run: Option<Instant>,
    next_run: Instant,
    tags: HashMap<String, String>,
}

impl BenchmarkScheduler {
    fn new(server_url: &str, device_id: &str, results_dir: &Path) -> Self {
        Self {
            benchmarks: Vec::new(),
            runner: Arc::new(BenchmarkRunner::new(server_url, device_id, results_dir)),
            detector: Arc::new(Mutex::new(RegressionDetector::new(5.0, 10))),
            schedule: Vec::new(),
            watcher: None,
        }
    }

    fn add_benchmark(&mut self, config: BenchmarkConfig) {
        let benchmark = Benchmark::new(config);
        self.benchmarks.push(benchmark);
    }

    fn schedule_benchmark(&mut self, benchmark_id: String, interval: Duration, tags: HashMap<String, String>) {
        let scheduled = ScheduledRun {
            benchmark_id,
            interval,
            last_run: None,
            next_run: Instant::now(),
            tags,
        };
        self.schedule.push(scheduled);
    }

    async fn run_once(&self, commit_hash: &str) -> Vec<BenchmarkResult> {
        let mut results = Vec::new();
        let mut handles = Vec::new();

        for benchmark in &self.benchmarks {
            let runner = Arc::clone(&self.runner);
            let detector = Arc::clone(&self.detector);
            let benchmark_id = benchmark.config.id.clone();
            let commit = commit_hash.to_string();
            let tags = HashMap::new();

            let handle = tokio::spawn(async move {
                match runner.run_benchmark(benchmark, &commit, tags).await {
                    Ok(result) => {
                        for metric in &result.metrics {
                            if metric.name.contains("exec_time.mean") {
                                let mut det = detector.lock().await;
                                det.add_sample(&benchmark_id, metric.value, result.timestamp);

                                if let Some(regression) = det.check_regression(&benchmark_id, metric.value) {
                                    warn!("⚠️  REGRESSION DETECTED: {:?}", regression);
                                }

                                if let Some(trend) = det.get_trend(&benchmark_id, 10) {
                                    debug!("Trend for {}: {:?}", benchmark_id, trend);
                                }
                            }
                        }
                        Some(result)
                    }
                    Err(e) => {
                        error!("Benchmark failed: {}", e);
                        None
                    }
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            if let Ok(Some(result)) = handle.await {
                results.push(result);
            }
        }

        results
    }

    async fn run_continuous(&self) {
        info!("Starting continuous benchmark mode");

        loop {
            let now = Instant::now();
            let mut to_run = Vec::new();

            for scheduled in &self.schedule {
                if now >= scheduled.next_run {
                    to_run.push(scheduled.benchmark_id.clone());
                }
            }

            for benchmark_id in to_run {
                if let Some(benchmark) = self.benchmarks.iter().find(|b| b.config.id == benchmark_id) {
                    info!("Running scheduled benchmark: {}", benchmark_id);

                    let runner = Arc::clone(&self.runner);
                    let benchmark = benchmark;
                    let commit = "scheduled-run".to_string();
                    let tags = HashMap::new();

                    tokio::spawn(async move {
                        if let Err(e) = runner.run_benchmark(benchmark, &commit, tags).await {
                            error!("Scheduled benchmark failed: {}", e);
                        }
                    });
                }
            }

            time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn watch_files(&mut self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut watcher: RecommendedWatcher = Watcher::new(tx, notify::Config::default())?;
        watcher.watch(path, RecursiveMode::Recursive)?;

        self.watcher = Some(watcher);

        std::thread::spawn(move || {
            for event in rx {
                match event {
                    Ok(event) => {
                        info!("File change detected: {:?}", event);
                    }
                    Err(e) => {
                        error!("Watch error: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    async fn generate_report(&self, results: &[BenchmarkResult], output_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        use plotters::prelude::*;

        let root = BitMapBackend::new(output_path, (1024, 768)).into_drawing_area();
        root.fill(&WHITE)?;

        let mut chart = ChartBuilder::on(&root)
            .caption("Benchmark Results", ("sans-serif", 30))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(0..results.len(), 0.0..100.0)?;

        chart.configure_mesh().draw()?;

        Ok(())
    }
}

fn percentile(data: &[f64], p: f64) -> f64 {
    if data.is_empty() {
        return 0.0;
    }

    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let index = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[index]
}

fn get_cpu_temperature() -> Option<f32> {
    #[cfg(target_os = "android")]
    {
        if let Ok(temp) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
            if let Ok(t) = temp.trim().parse::<u32>() {
                return Some(t as f32 / 1000.0);
            }
        }
    }

    None
}

fn get_battery_level() -> u8 {
    #[cfg(target_os = "android")]
    {
        if let Ok(capacity) = fs::read_to_string("/sys/class/power_supply/battery/capacity") {
            if let Ok(level) = capacity.trim().parse::<u8>() {
                return level;
            }
        }
    }

    100
}

fn is_thermal_throttling() -> bool {
    #[cfg(target_os = "android")]
    {
        if let Ok(cur_state) = fs::read_to_string("/sys/class/thermal/thermal_message/cur_state") {
            if let Ok(state) = cur_state.trim().parse::<u32>() {
                return state > 0;
            }
        }
    }

    false
}

fn get_kernel_version() -> String {
    if let Ok(output) = Command::new("uname").arg("-r").output() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        "unknown".to_string()
    }
}

fn get_system_uptime() -> Duration {
    #[cfg(target_os = "android")]
    {
        if let Ok(stat) = fs::read_to_string("/proc/uptime") {
            if let Some(uptime_secs) = stat.split_whitespace().next() {
                if let Ok(secs) = uptime_secs.parse::<f64>() {
                    return Duration::from_secs_f64(secs);
                }
            }
        }
    }

    Duration::from_secs(0)
}

fn get_context_switches() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(stat) = fs::read_to_string("/proc/stat") {
            for line in stat.lines() {
                if line.starts_with("ctxt ") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse().unwrap_or(0);
                    }
                }
            }
        }
    }

    0
}

fn get_interrupts() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(stat) = fs::read_to_string("/proc/stat") {
            for line in stat.lines() {
                if line.starts_with("intr ") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse().unwrap_or(0);
                    }
                }
            }
        }
    }

    0
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    info!("Mobile OS Performance Regression Testing Agent v{}", VERSION);
    info!("==============================================");

    let device_id = format!("{}-{}",
        hostname::get()?.to_string_lossy(),
        std::process::id()
    );

    let server_url = std::env::var("BENCH_SERVER_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string());

    let commit_hash = std::env::var("COMMIT_HASH")
        .unwrap_or_else(|_| "local-dev".to_string());

    let results_dir = PathBuf::from(std::env::var("RESULTS_DIR")
        .unwrap_or_else(|_| "./benchmark_results".to_string()));

    let mode = std::env::var("RUN_MODE")
        .unwrap_or_else(|_| "once".to_string());

    let mut scheduler = BenchmarkScheduler::new(&server_url, &device_id, &results_dir);

    scheduler.add_benchmark(BenchmarkConfig {
        id: "syscall.getpid".to_string(),
        command: "perf".to_string(),
        args: vec![
            "stat".to_string(),
            "-e".to_string(),
            "cycles,instructions,cache-misses".to_string(),
            "--".to_string(),
            "getpid".to_string(),
        ],
        env_vars: HashMap::new(),
        working_dir: None,
        warmup_iterations: 10,
        measure_iterations: 100,
        timeout: Duration::from_secs(30),
        required_resources: vec![],
        min_samples: 10,
        max_sample_variance: 0.1,
    });

    scheduler.add_benchmark(BenchmarkConfig {
        id: "fileio.read".to_string(),
        command: "dd".to_string(),
        args: vec![
            "if=/dev/zero".to_string(),
            "of=/dev/null".to_string(),
            "bs=4096".to_string(),
            "count=100000".to_string(),
        ],
        env_vars: HashMap::new(),
        working_dir: None,
        warmup_iterations: 3,
        measure_iterations: 20,
        timeout: Duration::from_secs(60),
        required_resources: vec!["disk_space".to_string()],
        min_samples: 10,
        max_sample_variance: 0.15,
    });

    scheduler.add_benchmark(BenchmarkConfig {
        id: "cpu.prime".to_string(),
        command: "openssl".to_string(),
        args: vec![
            "prime".to_string(),
            "generate".to_string(),
            "4096".to_string(),
        ],
        env_vars: HashMap::new(),
        working_dir: None,
        warmup_iterations: 2,
        measure_iterations: 5,
        timeout: Duration::from_secs(300),
        required_resources: vec![],
        min_samples: 5,
        max_sample_variance: 0.2,
    });

    scheduler.add_benchmark(BenchmarkConfig {
        id: "memory.alloc".to_string(),
        command: "stress".to_string(),
        args: vec![
            "--vm".to_string(),
            "2".to_string(),
            "--vm-bytes".to_string(),
            "256M".to_string(),
            "--timeout".to_string(),
            "10s".to_string(),
        ],
        env_vars: HashMap::new(),
        working_dir: None,
        warmup_iterations: 1,
        measure_iterations: 3,
        timeout: Duration::from_secs(30),
        required_resources: vec![],
        min_samples: 3,
        max_sample_variance: 0.25,
    });

    scheduler.add_benchmark(BenchmarkConfig {
        id: "network.tcp".to_string(),
        command: "iperf3".to_string(),
        args: vec![
            "-c".to_string(),
            "localhost".to_string(),
            "-t".to_string(),
            "10".to_string(),
            "-P".to_string(),
            "4".to_string(),
        ],
        env_vars: HashMap::new(),
        working_dir: None,
        warmup_iterations: 1,
        measure_iterations: 3,
        timeout: Duration::from_secs(60),
        required_resources: vec!["network".to_string()],
        min_samples: 3,
        max_sample_variance: 0.3,
    });

    match mode.as_str() {
        "continuous" => {
            info!("Running in continuous mode");

            scheduler.schedule_benchmark(
                "syscall.getpid".to_string(),
                Duration::from_secs(60),
                HashMap::new()
            );

            scheduler.schedule_benchmark(
                "fileio.read".to_string(),
                Duration::from_secs(300),
                HashMap::new()
            );

            scheduler.schedule_benchmark(
                "cpu.prime".to_string(),
                Duration::from_secs(3600),
                HashMap::new()
            );

            scheduler.run_continuous().await;
        }

        "regression" => {
            info!("Running regression test mode");

            let baseline_commit = std::env::var("BASELINE_COMMIT")
                .expect("BASELINE_COMMIT required for regression mode");

            info!("Baseline: {}", baseline_commit);
            info!("Current: {}", commit_hash);

            let baseline_results = scheduler.run_once(&baseline_commit).await;
            let current_results = scheduler.run_once(&commit_hash).await;

            let detector = RegressionDetector::new(5.0, 10);

            for result in &baseline_results {
                for metric in &result.metrics {
                    if metric.name.contains("exec_time.mean") {
                        detector.add_sample(&result.benchmark_id, metric.value, result.timestamp);
                    }
                }
            }

            for result in &current_results {
                for metric in &result.metrics {
                    if metric.name.contains("exec_time.mean") {
                        if let Some(regression) = detector.check_regression(&result.benchmark_id, metric.value) {
                            warn!("⚠️  REGRESSION: {:?}", regression);
                        }
                    }
                }
            }
        }

        _ => {
            info!("Running single-run mode for commit: {}", commit_hash);
            let results = scheduler.run_once(&commit_hash).await;

            info!("Completed {} benchmarks", results.len());

            let report_path = results_dir.join(format!("report_{}.png", commit_hash));
            scheduler.generate_report(&results, &report_path).await?;
            info!("Report generated: {:?}", report_path);
        }
    }

    info!("Benchmarking complete!");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percentile_calculation() {
        let data = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&data, 0.5), 5.0);
        assert_eq!(percentile(&data, 0.9), 9.0);
        assert_eq!(percentile(&data, 0.95), 10.0);
    }

    #[test]
    fn test_regression_detector() {
        let mut detector = RegressionDetector::new(10.0, 5);

        for i in 0..10 {
            detector.add_sample("test", 100.0 + (i as f64 * 0.1), i as u64);
        }

        let regression = detector.check_regression("test", 110.0);
        assert!(regression.is_some());
        let reg = regression.unwrap();
        assert_eq!(reg.change_percent, 9.9);

        let regression = detector.check_regression("test", 101.0);
        assert!(regression.is_none());
    }

    #[test]
    fn test_trend_detection() {
        let mut detector = RegressionDetector::new(5.0, 10);

        for i in 0..20 {
            detector.add_sample("trend", 100.0 + i as f64, i as u64);
        }

        let trend = detector.get_trend("trend", 5);
        assert!(trend.is_some());
        let trend = trend.unwrap();
        assert_eq!(trend.direction, "increasing");
    }

    #[test]
    fn test_benchmark_stability() {
        let config = BenchmarkConfig {
            id: "test".to_string(),
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            env_vars: HashMap::new(),
            working_dir: None,
            warmup_iterations: 1,
            measure_iterations: 3,
            timeout: Duration::from_secs(5),
            required_resources: vec![],
            min_samples: 3,
            max_sample_variance: 0.1,
        };

        let benchmark = Benchmark::new(config);
        let times = vec![100.0, 102.0, 98.0];
        let stability = benchmark.check_stability(&times);
        assert!(stability < 0.05);

        let times = vec![100.0, 150.0, 80.0, 120.0, 90.0];
        let stability = benchmark.check_stability(&times);
        assert!(stability > 0.1);
    }
}

#[cfg(feature = "bench")]
mod benches {
    use super::*;
    use test::Bencher;

    #[bench]
    fn bench_percentile(b: &mut Bencher) {
        let data: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        b.iter(|| {
            percentile(&data, 0.95)
        });
    }

    #[bench]
    fn bench_regression_detection(b: &mut Bencher) {
        let detector = RegressionDetector::new(5.0, 100);
        let values: Vec<f64> = (0..100).map(|i| 100.0 + (i as f64 * 0.1)).collect();

        b.iter(|| {
            for &v in &values {
                detector.check_regression("test", v);
            }
        });
    }
}
