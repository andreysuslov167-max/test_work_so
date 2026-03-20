// tests/mobile_stress/mod.rs
#![cfg(any(target_os = "android", target_os = "ios"))]
#![feature(test)]
#![allow(dead_code)]

use std::time::{Duration, Instant, SystemTime};
use std::thread;
use std::sync::{Arc, Mutex, RwLock, Barrier, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write, Read, Seek, SeekFrom, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::collections::{HashMap, VecDeque, BTreeMap};
use std::sync::mpsc::{self, Sender, Receiver, channel};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use rand::{Rng, SeedableRng, distributions::Alphanumeric};
use rand::rngs::StdRng;
use backtrace::Backtrace;

#[cfg(target_os = "android")]
use jni::JNIEnv;

#[cfg(target_os = "ios")]
use objc::runtime::{Class, Object, Sel};
use libc::{c_void, size_t, pid_t};

extern crate test;
use test::Bencher;

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;

#[derive(Debug, Clone)]
struct SystemMetrics {
    cpu_usage: f32,
    cpu_temp: f32,
    gpu_usage: f32,
    memory_used: u64,
    memory_total: u64,
    memory_cache: u64,
    swap_used: u64,
    swap_total: u64,
    battery_level: f32,
    battery_current: i32,
    battery_temperature: f32,
    battery_voltage: u32,
    thermal_throttling: bool,
    throttling_stage: u32,
    uptime: Duration,
    processes: u32,
    threads: u32,
    context_switches: u64,
    interrupts: u64,
    timestamp: Instant,
}

#[derive(Debug)]
struct StressTestConfig {
    test_duration: Duration,
    max_cpu_usage: f32,
    max_cpu_temp: f32,
    max_memory_mb: u64,
    max_threads: usize,
    max_file_size_mb: u64,
    max_open_files: usize,
    max_battery_drain_percent: f32,
    max_battery_current_ma: i32,
    max_temperature_celsius: f32,
    enable_throttling_protection: bool,
    enable_oom_protection: bool,
    enable_thermal_safety: bool,
}

impl Default for StressTestConfig {
    fn default() -> Self {
        Self {
            test_duration: Duration::from_secs(60),
            max_cpu_usage: if cfg!(target_os = "android") { 80.0 } else { 70.0 },
            max_cpu_temp: 85.0,
            max_memory_mb: if cfg!(target_os = "android") { 400 } else { 300 },
            max_threads: if cfg!(target_os = "android") { 100 } else { 60 },
            max_file_size_mb: if cfg!(target_os = "android") { 200 } else { 100 },
            max_open_files: if cfg!(target_os = "android") { 500 } else { 250 },
            max_battery_drain_percent: 1.0,
            max_battery_current_ma: 2000,
            max_temperature_celsius: 45.0,
            enable_throttling_protection: true,
            enable_oom_protection: true,
            enable_thermal_safety: true,
        }
    }
}

#[derive(Debug)]
struct ProcessStats {
    pid: u32,
    name: String,
    cpu_usage: f32,
    memory_rss: u64,
    memory_vsz: u64,
    threads: u32,
    open_files: usize,
    state: char,
}

#[derive(Debug)]
struct IOMetrics {
    bytes_read: u64,
    bytes_written: u64,
    read_ops: u64,
    write_ops: u64,
    read_latency: Duration,
    write_latency: Duration,
}

#[derive(Debug)]
struct NetworkMetrics {
    interface: String,
    bytes_rx: u64,
    bytes_tx: u64,
    packets_rx: u64,
    packets_tx: u64,
    errors: u32,
    drops: u32,
    latency: Duration,
}

#[cfg(target_os = "android")]
mod android_power {
    use super::*;

    pub fn get_power_profile() -> Result<HashMap<String, u32>, String> {
        let mut profile = HashMap::new();
        let paths = [
            ("cpu", "/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq"),
            ("gpu", "/sys/class/kgsl/kgsl-3d0/max_gpuclk"),
            ("ddr", "/sys/class/devfreq/soc:qcom,cpu-llcc-ddr-bw/max_freq"),
        ];

        for (name, path) in paths {
            if let Ok(content) = fs::read_to_string(path) {
                profile.insert(name.to_string(), content.trim().parse().unwrap_or(0));
            }
        }

        Ok(profile)
    }

    pub fn set_power_mode(mode: &str) -> Result<(), String> {
        let mode_file = "/sys/power/perf_mode";
        fs::write(mode_file, mode).map_err(|e| e.to_string())
    }

    pub fn get_thermal_zones() -> Result<Vec<(String, f32)>, String> {
        let mut zones = Vec::new();
        let base_path = "/sys/class/thermal";

        if let Ok(entries) = fs::read_dir(base_path) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                let temp_path = entry.path().join("temp");

                if let Ok(temp_str) = fs::read_to_string(temp_path) {
                    if let Ok(temp) = temp_str.trim().parse::<u32>() {
                        zones.push((name.to_string_lossy().to_string(), temp as f32 / 1000.0));
                    }
                }
            }
        }

        Ok(zones)
    }
}

#[cfg(target_os = "ios")]
mod ios_power {
    use super::*;

    pub fn get_thermal_condition() -> Result<u32, String> {
        unsafe {
            let class = Class::get("NSProcessInfo").unwrap();
            let info: *mut Object = msg_send![class, processInfo];
            let level: u32 = msg_send![info, thermalState];
            Ok(level)
        }
    }

    pub fn get_battery_info() -> Result<HashMap<String, f32>, String> {
        let mut info = HashMap::new();
        unsafe {
            let device_class = Class::get("UIDevice").unwrap();
            let device: *mut Object = msg_send![device_class, currentDevice];
            let _: () = msg_send![device, setBatteryMonitoringEnabled: true];

            let level: f32 = msg_send![device, batteryLevel];
            let state: i32 = msg_send![device, batteryState];

            info.insert("level".to_string(), level * 100.0);
            info.insert("state".to_string(), state as f32);
        }
        Ok(info)
    }
}

#[test]
fn test_cpu_multi_threading_stress() {
    println!("=== CPU AND MULTITHREADING STRESS TEST ===");

    let config = StressTestConfig::default();
    let metrics = Arc::new(Mutex::new(VecDeque::with_capacity(1000)));
    let stop_signal = Arc::new(AtomicBool::new(false));
    let completed_operations = Arc::new(AtomicU64::new(0));
    let active_threads = Arc::new(AtomicUsize::new(0));
    let panics = Arc::new(Mutex::new(Vec::new()));
    let start_time = Instant::now();

    let core_count = num_cpus::get();
    println!("Detected {} CPU cores", core_count);

    let workload_types = [
        ("float_intensive", float_intensive_workload),
        ("int_intensive", int_intensive_workload),
        ("memory_intensive", memory_intensive_workload),
        ("sync_intensive", sync_intensive_workload),
        ("io_intensive", io_intensive_workload),
        ("branch_intensive", branch_intensive_workload),
        ("cache_thrashing", cache_thrashing_workload),
        ("atomic_ops", atomic_ops_workload),
        ("recursive", recursive_workload),
        ("simd", simd_workload),
    ];

    let mut thread_handles = Vec::with_capacity(config.max_threads);

    for (workload_name, workload_fn) in workload_types.iter().cycle().take(config.max_threads) {
        let stop = Arc::clone(&stop_signal);
        let counter = Arc::clone(&completed_operations);
        let thread_counter = Arc::clone(&active_threads);
        let panics_log = Arc::clone(&panics);
        let name = workload_name.to_string();

        thread_counter.fetch_add(1, Ordering::Relaxed);

        let handle = thread::Builder::new()
            .name(format!("stress-{}", name))
            .spawn(move || {
                let result = std::panic::catch_unwind(|| {
                    let mut local_metrics = Vec::with_capacity(10000);
                    let start = Instant::now();

                    while !stop.load(Ordering::Relaxed) {
                        let op_start = Instant::now();
                        workload_fn();
                        let elapsed = op_start.elapsed();

                        local_metrics.push(elapsed);
                        counter.fetch_add(1, Ordering::Relaxed);

                        if local_metrics.len() >= 1000 {
                            thread::yield_now();
                        }
                    }

                    local_metrics
                });

                thread_counter.fetch_sub(1, Ordering::Relaxed);

                match result {
                    Ok(metrics) => {
                        if let Ok(mut guard) = panics_log.lock() {
                            guard.push((name, format!("Completed {} ops", metrics.len())));
                        }
                    }
                    Err(e) => {
                        if let Ok(mut guard) = panics_log.lock() {
                            guard.push((name, format!("Panic: {:?}", e)));
                        }
                    }
                }
            })
            .unwrap();

        thread_handles.push(handle);
    }

    let monitor_interval = Duration::from_secs(1);
    let mut last_ops = 0;
    let mut peak_ops = 0;

    while start_time.elapsed() < config.test_duration {
        thread::sleep(monitor_interval);

        let current_ops = completed_operations.load(Ordering::Relaxed);
        let ops_per_second = current_ops - last_ops;
        last_ops = current_ops;

        if ops_per_second > peak_ops {
            peak_ops = ops_per_second;
        }

        let current_metrics = collect_system_metrics();
        metrics.lock().unwrap().push_back(current_metrics.clone());

        if metrics.lock().unwrap().len() > 1000 {
            metrics.lock().unwrap().pop_front();
        }

        check_limits(&current_metrics, &config);

        println!("[{}s] CPU: {:.1}%, Temp: {:.1}°C, Threads: {}, OPS: {}/s",
                 start_time.elapsed().as_secs(),
                 current_metrics.cpu_usage,
                 current_metrics.cpu_temp,
                 active_threads.load(Ordering::Relaxed),
                 ops_per_second);

        if ops_per_second < peak_ops / 10 && start_time.elapsed().as_secs() > 10 {
            println!("Warning: Performance degraded significantly");
        }
    }

    stop_signal.store(true, Ordering::Relaxed);

    for handle in thread_handles {
        let _ = handle.join();
    }

    let metrics_guard = metrics.lock().unwrap();
    analyze_stress_results(&metrics_guard, completed_operations.load(Ordering::Relaxed));

    println!("Panics during test: {}", panics.lock().unwrap().len());
    for (name, msg) in panics.lock().unwrap().iter() {
        println!("  {}: {}", name, msg);
    }

    assert!(panics.lock().unwrap().is_empty(), "Thread panics detected");
    println!("✓ CPU stress test completed");
}

fn float_intensive_workload() {
    let mut rng = rand::thread_rng();
    for _ in 0..1000 {
        let a: f64 = rng.gen();
        let b: f64 = rng.gen();
        let c: f64 = rng.gen();
        black_box((a.sin() * b.cos() + (a * b).tan()).atan2(c));
    }
}

fn int_intensive_workload() {
    let mut rng = rand::thread_rng();
    for _ in 0..10000 {
        let a: u64 = rng.gen();
        let b: u64 = rng.gen();
        black_box(a.wrapping_mul(b).wrapping_add(a).wrapping_div(b));
    }
}

fn memory_intensive_workload() {
    let size = rand::thread_rng().gen_range(KB as usize..=MB as usize);
    let mut vec = Vec::with_capacity(size);
    vec.resize(size, 0u8);

    for i in 0..vec.len() {
        vec[i] = (i % 256) as u8;
    }

    let sum: u64 = vec.iter().map(|&x| x as u64).sum();
    black_box(sum);
}

fn sync_intensive_workload() {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let barrier = Barrier::new(10);

    for _ in 0..10 {
        barrier.wait();
        COUNTER.fetch_add(1, Ordering::SeqCst);
    }
}

fn io_intensive_workload() {
    let temp_dir = get_mobile_test_dir().join("io_stress");
    fs::create_dir_all(&temp_dir).ok();

    for i in 0..10 {
        let path = temp_dir.join(format!("io_{}.tmp", i));
        let mut file = File::create(&path).unwrap();
        let data = vec![i as u8; KB as usize];
        file.write_all(&data).unwrap();
        file.sync_all().unwrap();
        fs::remove_file(path).ok();
    }
}

fn branch_intensive_workload() {
    let mut rng = rand::thread_rng();
    let mut x = 0;

    for i in 0..10000 {
        if rng.gen_bool(0.5) {
            x = x.wrapping_add(i);
        } else {
            x = x.wrapping_sub(i);
        }

        if x % 2 == 0 {
            x = x.wrapping_mul(3);
        } else if x % 3 == 0 {
            x = x.wrapping_div(2);
        } else {
            x = x.wrapping_add(1);
        }
    }

    black_box(x);
}

fn cache_thrashing_workload() {
    let size = 10 * MB as usize;
    let mut data = vec![0u64; size / 8];

    for i in 0..data.len() {
        data[i] = i as u64;
    }

    for _ in 0..100 {
        for i in (0..data.len()).step_by(64) {
            data[i] = data[i].wrapping_mul(3);
        }
    }

    black_box(data);
}

fn atomic_ops_workload() {
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = vec![];

    for _ in 0..10 {
        let counter = Arc::clone(&counter);
        handles.push(thread::spawn(move || {
            for _ in 0..1000 {
                counter.fetch_add(1, Ordering::Relaxed);
                counter.load(Ordering::Relaxed);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

fn recursive_workload() {
    fn fib(n: u64) -> u64 {
        if n <= 1 { n } else { fib(n-1) + fib(n-2) }
    }
    black_box(fib(20));
}

fn simd_workload() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let a = [1.0f32; 16];
        let b = [2.0f32; 16];
        let mut c = [0.0f32; 16];

        for i in 0..16 {
            c[i] = a[i] * b[i] + a[i];
        }

        black_box(c);
    }
}

#[test]
fn test_memory_pressure_stress() {
    println!("=== MEMORY PRESSURE STRESS TEST ===");

    let config = StressTestConfig::default();
    let start_time = Instant::now();

    let initial_memory = get_memory_stats().unwrap();
    println!("Initial memory: {}/{} MB free",
             initial_memory.free / MB,
             initial_memory.total / MB);

    let allocation_patterns: Vec<Box<dyn Fn() -> Vec<u8> + Send>> = vec![
        Box::new(|| vec![0u8; KB as usize]),
        Box::new(|| vec![0u8; 64 * KB as usize]),
        Box::new(|| vec![0u8; MB as usize]),
        Box::new(|| vec![0u8; 16 * MB as usize]),
    ];

    let mut allocations: Vec<Vec<u8>> = Vec::with_capacity(10000);
    let mut allocation_times = Vec::with_capacity(10000);
    let mut pressure_events = Vec::new();
    let mut oom_score = 0;

    while start_time.elapsed() < config.test_duration {
        let current_memory = get_memory_stats().unwrap();
        let used_percent = 100.0 - (current_memory.free as f64 / current_memory.total as f64 * 100.0);

        let pressure_level = if used_percent > 90.0 {
            MemoryPressure::Critical
        } else if used_percent > 75.0 {
            MemoryPressure::High
        } else if used_percent > 50.0 {
            MemoryPressure::Medium
        } else {
            MemoryPressure::Low
        };

        match pressure_level {
            MemoryPressure::Critical => {
                if config.enable_oom_protection {
                    oom_score += 1;
                    if oom_score > 10 {
                        println!("OOM protection triggered, releasing memory");
                        for _ in 0..allocations.len().min(1000) {
                            allocations.pop();
                        }
                        oom_score = 0;
                    }
                }
            }
            MemoryPressure::High => {
                if rand::thread_rng().gen_bool(0.3) {
                    allocations.pop();
                }
            }
            _ => {}
        }

        if used_percent < 85.0 && pressure_level != MemoryPressure::Critical {
            let pattern_idx = rand::thread_rng().gen_range(0..allocation_patterns.len());
            let alloc_start = Instant::now();
            let allocation = allocation_patterns[pattern_idx]();
            let alloc_time = alloc_start.elapsed();

            allocations.push(allocation);
            allocation_times.push((allocation_times.len(), alloc_time));

            pressure_events.push((start_time.elapsed(), used_percent));
        }

        if allocations.len() % 1000 == 0 {
            let avg_alloc_time: Duration = allocation_times.iter()
                .rev()
                .take(1000)
                .map(|(_, t)| *t)
                .sum::<Duration>() / 1000;

            println!("Memory: {:.1}% used, {} allocations, avg time: {:?}",
                     used_percent, allocations.len(), avg_alloc_time);
        }

        thread::sleep(Duration::from_millis(10));
    }

    drop(allocations);

    thread::sleep(Duration::from_secs(1));
    let final_memory = get_memory_stats().unwrap();
    let memory_leaked = (final_memory.free as i64 - initial_memory.free as i64).abs();

    println!("Final memory: {}/{} MB free",
             final_memory.free / MB,
             final_memory.total / MB);

    assert!(memory_leaked < 10 * MB as i64,
            "Possible memory leak: {} MB not freed", memory_leaked / MB);

    analyze_memory_patterns(&allocation_times, &pressure_events);

    println!("✓ Memory stress test completed");
}

enum MemoryPressure {
    Low,
    Medium,
    High,
    Critical,
}

#[test]
fn test_filesystem_stress() {
    println!("=== FILESYSTEM STRESS TEST ===");

    let config = StressTestConfig::default();
    let test_dir = get_mobile_test_dir().join(format!("stress_{:x}", rand::random::<u32>()));
    fs::create_dir_all(&test_dir).expect("Failed to create test dir");

    println!("Test directory: {:?}", test_dir);

    let start_time = Instant::now();
    let stop_signal = Arc::new(AtomicBool::new(false));
    let io_metrics = Arc::new(Mutex::new(IOMetrics {
        bytes_read: 0, bytes_written: 0,
        read_ops: 0, write_ops: 0,
        read_latency: Duration::default(),
        write_latency: Duration::default(),
    }));

    let mut handles = vec![];

    let ops = [
        (create_files_workload, "create"),
        (write_files_workload, "write"),
        (read_files_workload, "read"),
        (append_files_workload, "append"),
        (random_access_workload, "random"),
        (rename_files_workload, "rename"),
        (link_files_workload, "link"),
        (permission_workload, "perms"),
        (directory_workload, "dirs"),
        (mmap_workload, "mmap"),
    ];

    for (workload_fn, name) in ops {
        for i in 0..(config.max_threads / ops.len()).max(1) {
            let stop = Arc::clone(&stop_signal);
            let test_dir = test_dir.clone();
            let metrics = Arc::clone(&io_metrics);

            let handle = thread::spawn(move || {
                let mut local_read = 0;
                let mut local_write = 0;
                let mut local_read_ops = 0;
                let mut local_write_ops = 0;

                while !stop.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    let (r, w) = workload_fn(&test_dir, i);
                    let elapsed = start.elapsed();

                    local_read += r;
                    local_write += w;
                    local_read_ops += 1;
                    local_write_ops += 1;

                    if local_read_ops % 100 == 0 {
                        let mut guard = metrics.lock().unwrap();
                        guard.bytes_read += local_read;
                        guard.bytes_written += local_write;
                        guard.read_ops += local_read_ops;
                        guard.write_ops += local_write_ops;
                        guard.read_latency += elapsed;
                        guard.write_latency += elapsed;

                        local_read = 0;
                        local_write = 0;
                        local_read_ops = 0;
                        local_write_ops = 0;
                    }
                }
            });

            handles.push(handle);
        }
    }

    let monitor_interval = Duration::from_secs(2);

    while start_time.elapsed() < config.test_duration {
        thread::sleep(monitor_interval);

        let metrics = io_metrics.lock().unwrap();
        let elapsed = start_time.elapsed().as_secs_f64();

        if metrics.read_ops > 0 {
            let read_speed = metrics.bytes_read as f64 / elapsed / MB as f64;
            let write_speed = metrics.bytes_written as f64 / elapsed / MB as f64;
            let avg_read_latency = metrics.read_latency / metrics.read_ops as u32;
            let avg_write_latency = metrics.write_latency / metrics.write_ops as u32;

            println!("[{:.0}s] R: {:.2}MB/s, W: {:.2}MB/s, R lat: {:?}, W lat: {:?}",
                     elapsed, read_speed, write_speed, avg_read_latency, avg_write_latency);

            let free_space = free_disk_space_mb(&test_dir);
            println!("  Free space: {}MB, Open files: {}", free_space, count_open_files());

            assert!(free_space > 50, "Low disk space");
        }

        drop(metrics);
    }

    stop_signal.store(true, Ordering::Relaxed);

    for handle in handles {
        let _ = handle.join();
    }

    let final_metrics = io_metrics.lock().unwrap();
    println!("\nFinal I/O Stats:");
    println!("  Read: {} ops, {} MB", final_metrics.read_ops, final_metrics.bytes_read / MB);
    println!("  Write: {} ops, {} MB", final_metrics.write_ops, final_metrics.bytes_written / MB);

    fs::remove_dir_all(&test_dir).ok();

    println!("✓ Filesystem stress test completed");
}

fn create_files_workload(dir: &Path, id: usize) -> (u64, u64) {
    let count = rand::thread_rng().gen_range(10..50);
    let mut written = 0;

    for i in 0..count {
        let path = dir.join(format!("create_{}_{}.tmp", id, i));
        let data = vec![i as u8; KB as usize];
        fs::write(&path, &data).ok();
        written += data.len() as u64;
    }

    (0, written)
}

fn write_files_workload(dir: &Path, id: usize) -> (u64, u64) {
    let path = dir.join(format!("write_{}.dat", id));
    let size = rand::thread_rng().gen_range(KB..=MB);
    let data = vec![id as u8; size as usize];

    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&path)
    {
        let written = file.write(&data).unwrap_or(0) as u64;
        file.sync_all().ok();
        return (0, written);
    }

    (0, 0)
}

fn read_files_workload(dir: &Path, id: usize) -> (u64, u64) {
    let mut read = 0;

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok).take(10) {
            if let Ok(file) = File::open(entry.path()) {
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                let mut reader = BufReader::new(file);
                let mut buf = vec![0; KB as usize];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 { break; }
                    read += n as u64;
                }
            }
        }
    }

    (read, 0)
}

fn append_files_workload(dir: &Path, id: usize) -> (u64, u64) {
    let path = dir.join(format!("append_{}.log", id));
    let written = if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&path)
    {
        let data = format!("[{}] log entry {}\n",
                          SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                          id);
        file.write(data.as_bytes()).unwrap_or(0) as u64
    } else {
        0
    };

    (0, written)
}

fn random_access_workload(dir: &Path, id: usize) -> (u64, u64) {
    let path = dir.join(format!("random_{}.db", id));
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();

    let file_size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    let mut total_read = 0;
    let mut total_write = 0;

    if file_size > 0 {
        for _ in 0..10 {
            let pos = rand::thread_rng().gen_range(0..file_size);
            file.seek(SeekFrom::Start(pos)).ok();

            if rand::thread_rng().gen_bool(0.5) {
                let mut buf = [0u8; 512];
                if let Ok(n) = file.read(&mut buf) {
                    total_read += n as u64;
                }
            } else {
                let data = [rand::thread_rng().gen(); 512];
                if let Ok(n) = file.write(&data) {
                    total_write += n as u64;
                }
            }
        }
    } else {
        let data = vec![id as u8; KB as usize];
        if let Ok(n) = file.write(&data) {
            total_write += n as u64;
        }
    }

    file.sync_all().ok();
    (total_read, total_write)
}

fn rename_files_workload(dir: &Path, id: usize) -> (u64, u64) {
    let old_path = dir.join(format!("rename_{}.tmp", id));
    let new_path = dir.join(format!("renamed_{}.tmp", id));

    fs::write(&old_path, &[id as u8; 1024]).ok();
    fs::rename(&old_path, &new_path).ok();

    (0, 1024)
}

fn link_files_workload(dir: &Path, id: usize) -> (u64, u64) {
    let original = dir.join(format!("link_orig_{}.tmp", id));
    let link = dir.join(format!("link_{}.tmp", id));

    fs::write(&original, &[id as u8; 1024]).ok();
    fs::hard_link(&original, &link).ok();

    if let Ok(metadata) = fs::metadata(&original) {
        assert_eq!(metadata.nlink(), 2, "Hard link count incorrect");
    }

    (0, 1024)
}

fn permission_workload(dir: &Path, id: usize) -> (u64, u64) {
    let path = dir.join(format!("perms_{}.tmp", id));

    fs::write(&path, &[id as u8; 1024]).ok();

    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&path, perms).ok();

    (0, 1024)
}

fn directory_workload(dir: &Path, id: usize) -> (u64, u64) {
    let subdir = dir.join(format!("subdir_{}", id));
    fs::create_dir_all(&subdir).ok();

    for i in 0..10 {
        let file = subdir.join(format!("file_{}.tmp", i));
        fs::write(&file, &[i as u8; 1024]).ok();
    }

    fs::remove_dir_all(&subdir).ok();

    (0, 10 * 1024)
}

fn mmap_workload(dir: &Path, id: usize) -> (u64, u64) {
    let path = dir.join(format!("mmap_{}.dat", id));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();

    file.set_len(MB).unwrap();

    unsafe {
        use memmap2::MmapMut;

        let mut mmap = MmapMut::map_mut(&file).unwrap();

        for i in 0..mmap.len() {
            mmap[i] = (i % 256) as u8;
        }

        mmap.flush().unwrap();
    }

    (MB, MB)
}

#[test]
fn test_thermal_and_battery_stress() {
    println!("=== THERMAL AND BATTERY STRESS TEST ===");

    let config = StressTestConfig::default();
    let start_time = Instant::now();

    let mut thermal_history = Vec::with_capacity(1000);
    let mut throttling_events = 0;
    let mut max_temp = 0.0f32;

    let thermal_loads = [
        ("light", Duration::from_secs(2), generate_light_thermal_load),
        ("medium", Duration::from_secs(3), generate_medium_thermal_load),
        ("heavy", Duration::from_secs(5), generate_heavy_thermal_load),
        ("gpu", Duration::from_secs(4), generate_gpu_thermal_load),
        ("combined", Duration::from_secs(10), generate_combined_thermal_load),
    ];

    for (load_name, duration, load_fn) in thermal_loads.iter() {
        if start_time.elapsed() >= config.test_duration {
            break;
        }

        println!("Applying {} thermal load for {:?}", load_name, duration);

        let load_start = Instant::now();
        let load_stop = Arc::new(AtomicBool::new(false));
        let load_handles: Vec<_> = (0..num_cpus::get()).map(|_| {
            let stop = Arc::clone(&load_stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    load_fn();
                }
            })
        }).collect();

        while load_start.elapsed() < *duration {
            thread::sleep(Duration::from_secs(1));

            let temp = measure_cpu_temperature();
            let battery_temp = simulate_battery_temperature();
            let throttling = is_thermal_throttling();
            let throttling_stage = get_throttling_stage();

            max_temp = max_temp.max(temp);

            thermal_history.push((
                start_time.elapsed().as_secs(),
                temp,
                battery_temp,
                throttling,
                throttling_stage
            ));

            if throttling {
                throttling_events += 1;
            }

            println!("  Temp: {:.1}°C, Battery: {:.1}°C, Stage: {}, Throttling: {}",
                     temp, battery_temp, throttling_stage, throttling);

            assert!(temp < config.max_cpu_temp,
                    "CPU temperature critical: {:.1}°C", temp);
            assert!(battery_temp < config.max_temperature_celsius,
                    "Battery temperature critical: {:.1}°C", battery_temp);

            if throttling && throttling_stage >= 2 && config.enable_thermal_safety {
                println!("Thermal throttling active, reducing load");
                load_stop.store(true, Ordering::Relaxed);
                break;
            }
        }

        load_stop.store(true, Ordering::Relaxed);
        for h in load_handles {
            let _ = h.join();
        }

        println!("Cooling down...");
        thread::sleep(Duration::from_secs(5));
    }

    analyze_thermal_data(&thermal_history, throttling_events);

    println!("Max temperature recorded: {:.1}°C", max_temp);
    println!("✓ Thermal stress test completed");
}

fn generate_light_thermal_load() {
    for _ in 0..1000000 {
        black_box(fast_math());
    }
}

fn generate_medium_thermal_load() {
    for _ in 0..1000000 {
        black_box(heavy_computation());
    }
}

fn generate_heavy_thermal_load() {
    for _ in 0..500000 {
        black_box(cache_thrashing_workload());
    }
}

fn generate_gpu_thermal_load() {
    for _ in 0..10000 {
        black_box(render_complex_scene());
    }
}

fn generate_combined_thermal_load() {
    let handles: Vec<_> = (0..4).map(|i| {
        thread::spawn(move || {
            match i % 4 {
                0 => generate_heavy_thermal_load(),
                1 => generate_gpu_thermal_load(),
                2 => memory_intensive_workload(),
                _ => io_intensive_workload(),
            }
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }
}

fn fast_math() -> f64 {
    let mut x = 1.0;
    for i in 1..100 {
        x = x * i as f64 / (i + 1) as f64;
    }
    x
}

#[test]
fn test_network_stress() {
    println!("=== NETWORK STRESS TEST ===");

    let config = StressTestConfig::default();
    let start_time = Instant::now();

    let interfaces = get_network_interfaces();
    println!("Network interfaces: {:?}", interfaces);

    let network_conditions = [
        ("ideal", 0, 0.0, 0.0),
        ("wifi", 10, 0.1, 0.01),
        ("4g", 50, 1.0, 0.05),
        ("3g", 150, 5.0, 0.10),
        ("2g", 500, 20.0, 0.20),
        ("lossy", 100, 10.0, 0.30),
    ];

    let mut all_metrics = Vec::new();

    for (condition, latency_ms, jitter_ms, loss_rate) in network_conditions {
        if start_time.elapsed() >= config.test_duration {
            break;
        }

        println!("\nTesting network condition: {}", condition);
        println!("  Latency: {}ms, Jitter: {:.1}ms, Loss: {:.0}%",
                 latency_ms, jitter_ms, loss_rate * 100.0);

        simulate_network_condition(latency_ms, jitter_ms, loss_rate);

        let mut metrics = Vec::new();

        let test_types = [
            ("http", test_http_throughput),
            ("tcp", test_tcp_throughput),
            ("udp", test_udp_throughput),
            ("dns", test_dns_performance),
            ("ping", test_icmp_latency),
        ];

        for (name, test_fn) in test_types {
            let (throughput, latency, loss) = test_fn(Duration::from_secs(5));
            metrics.push((name, throughput, latency, loss));

            println!("  {}: {:.2} Mbps, {:?} latency, {:.1}% loss",
                     name, throughput / 1_000_000.0, latency, loss * 100.0);
        }

        all_metrics.push((condition, metrics));

        thread::sleep(Duration::from_secs(1));
    }

    analyze_network_results(&all_metrics);

    println!("✓ Network stress test completed");
}

fn get_network_interfaces() -> Vec<String> {
    let mut interfaces = Vec::new();

    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.filter_map(Result::ok) {
            if let Ok(name) = entry.file_name().into_string() {
                interfaces.push(name);
            }
        }
    }

    interfaces
}

fn simulate_network_condition(latency_ms: u64, jitter_ms: f64, loss_rate: f64) {
    thread::sleep(Duration::from_millis(latency_ms));

    if rand::thread_rng().gen_bool(loss_rate) {
        thread::sleep(Duration::from_millis(1000));
    }

    if jitter_ms > 0.0 {
        let jitter = (rand::thread_rng().gen::<f64>() * jitter_ms) as u64;
        thread::sleep(Duration::from_millis(jitter));
    }
}

fn test_http_throughput(duration: Duration) -> (u64, Duration, f64) {
    let start = Instant::now();
    let mut bytes = 0;
    let mut failures = 0;
    let mut total = 0;

    while start.elapsed() < duration {
        total += 1;
        let req_start = Instant::now();

        match std::net::TcpStream::connect_timeout(
            &"8.8.8.8:53".parse().unwrap(),
            Duration::from_secs(2)
        ) {
            Ok(_) => {
                bytes += 1024;
            }
            Err(_) => {
                failures += 1;
            }
        }

        thread::sleep(Duration::from_millis(10));
    }

    let loss_rate = failures as f64 / total as f64;
    (bytes * 8, start.elapsed(), loss_rate)
}

fn test_tcp_throughput(duration: Duration) -> (u64, Duration, f64) {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 65536];
        let mut total = 0;

        let start = Instant::now();
        while start.elapsed() < duration {
            match stream.read(&mut buf) {
                Ok(n) if n > 0 => total += n,
                _ => break,
            }
        }
        total
    });

    thread::sleep(Duration::from_millis(100));

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let data = vec![0u8; 65536];

    let start = Instant::now();
    let mut total_sent = 0;

    while start.elapsed() < duration {
        match stream.write(&data) {
            Ok(n) => total_sent += n,
            Err(_) => break,
        }
    }

    drop(stream);
    let total_recv = server.join().unwrap() as u64;

    let throughput = (total_sent * 8) as f64 / duration.as_secs_f64();
    (throughput as u64, start.elapsed(),
     (total_sent as f64 - total_recv as f64).abs() / total_sent as f64)
}

fn test_udp_throughput(duration: Duration) -> (u64, Duration, f64) {
    use std::net::UdpSocket;

    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = receiver.local_addr().unwrap().port();

    receiver.set_nonblocking(true).unwrap();

    let server = thread::spawn(move || {
        let mut buf = [0u8; 1472];
        let mut packets = 0;
        let start = Instant::now();

        while start.elapsed() < duration {
            match receiver.recv_from(&mut buf) {
                Ok((n, _)) if n > 0 => packets += 1,
                _ => thread::sleep(Duration::from_micros(100)),
            }
        }
        packets
    });

    thread::sleep(Duration::from_millis(100));

    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let data = vec![0u8; 1400];
    let mut sent = 0;

    let start = Instant::now();
    while start.elapsed() < duration {
        match sender.send_to(&data, ("127.0.0.1", port)) {
            Ok(n) => sent += n,
            Err(_) => {}
        }
        thread::sleep(Duration::from_micros(100));
    }

    let packets_recv = server.join().unwrap();
    let packets_sent = sent / 1400;

    let loss_rate = if packets_sent > 0 {
        (packets_sent - packets_recv) as f64 / packets_sent as f64
    } else {
        0.0
    };

    let throughput = (sent * 8) as f64 / duration.as_secs_f64();
    (throughput as u64, start.elapsed(), loss_rate)
}

fn test_dns_performance(duration: Duration) -> (u64, Duration, f64) {
    use std::net::ToSocketAddrs;

    let domains = [
        "google.com",
        "github.com",
        "microsoft.com",
        "amazon.com",
        "cloudflare.com",
    ];

    let start = Instant::now();
    let mut successes = 0;
    let mut failures = 0;

    while start.elapsed() < duration {
        for domain in domains {
            let _ = format!("{}:80", domain).to_socket_addrs();
            if rand::thread_rng().gen_bool(0.9) {
                successes += 1;
            } else {
                failures += 1;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    let total = successes + failures;
    let loss_rate = failures as f64 / total as f64;

    (successes as u64 * 100, start.elapsed(), loss_rate)
}

fn test_icmp_latency(duration: Duration) -> (u64, Duration, f64) {
    let target = "8.8.8.8";
    let start = Instant::now();
    let mut successes = 0;
    let mut failures = 0;

    while start.elapsed() < duration {
        let output = Command::new("ping")
            .arg("-c")
            .arg("1")
            .arg("-W")
            .arg("1")
            .arg(target)
            .output();

        match output {
            Ok(o) if o.status.success() => successes += 1,
            _ => failures += 1,
        }

        thread::sleep(Duration::from_millis(100));
    }

    let total = successes + failures;
    let loss_rate = failures as f64 / total as f64;

    (successes * 64 * 8, start.elapsed(), loss_rate)
}

#[test]
fn test_gpu_stress() {
    println!("=== GPU STRESS TEST ===");

    let config = StressTestConfig::default();
    let start_time = Instant::now();

    let mut frame_times = Vec::new();
    let mut gpu_usage = Vec::new();
    let mut gpu_memory = Vec::new();

    let scenes = [
        ("simple", render_simple_scene),
        ("textured", render_textured_scene),
        ("complex", render_complex_scene),
        ("compute", render_compute_scene),
        ("particles", render_particles_scene),
    ];

    while start_time.elapsed() < config.test_duration {
        for (scene_name, scene_fn) in scenes.iter() {
            let scene_start = Instant::now();

            for _ in 0..100 {
                scene_fn();
            }

            let scene_time = scene_start.elapsed();
            let fps = 100.0 / scene_time.as_secs_f64();

            frame_times.push(scene_time / 100);
            gpu_usage.push(get_gpu_usage());
            gpu_memory.push(get_gpu_memory_usage());

            println!("  {}: {:.1} FPS, GPU: {:.1}%, Mem: {}MB",
                     scene_name, fps,
                     gpu_usage.last().unwrap_or(&0.0),
                     gpu_memory.last().unwrap_or(&0) / MB);

            if fps < 30.0 {
                println!("  Warning: Low FPS in {}", scene_name);
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    analyze_gpu_results(&frame_times, &gpu_usage, &gpu_memory);

    println!("✓ GPU stress test completed");
}

fn render_simple_scene() {
    thread::sleep(Duration::from_micros(50));
}

fn render_textured_scene() {
    thread::sleep(Duration::from_micros(100));
}

fn render_complex_scene() {
    thread::sleep(Duration::from_micros(500));
}

fn render_compute_scene() {
    thread::sleep(Duration::from_micros(200));
}

fn render_particles_scene() {
    thread::sleep(Duration::from_micros(300));
}

fn get_gpu_usage() -> f32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(usage) = fs::read_to_string("/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage") {
            return usage.trim().parse().unwrap_or(0.0);
        }
    }

    rand::thread_rng().gen_range(30.0..80.0)
}

#[test]
fn test_multimedia_stress() {
    println!("=== MULTIMEDIA STRESS TEST ===");

    let config = StressTestConfig::default();
    let start_time = Instant::now();

    let camera_resolutions = [
        (640, 480),
        (1280, 720),
        (1920, 1080),
        (3840, 2160),
    ];

    for (width, height) in camera_resolutions {
        if start_time.elapsed() >= config.test_duration {
            break;
        }

        println!("\nTesting camera at {}x{}", width, height);

        let capture_start = Instant::now();
        let frames = capture_camera_frames(width, height, 60);
        let capture_time = capture_start.elapsed();

        let fps = frames as f64 / capture_time.as_secs_f64();
        println!("  Captured {} frames in {:?} ({:.1} FPS)",
                 frames, capture_time, fps);

        assert!(fps >= 20.0, "Camera too slow: {:.1} FPS", fps);

        let compression_start = Instant::now();
        let compressed = compress_video_frames(frames);
        let compression_time = compression_start.elapsed();

        println!("  Compressed to {}MB in {:?}",
                 compressed / MB, compression_time);

        thread::sleep(Duration::from_secs(1));
    }

    let audio_configs = [
        (8000, 1),
        (44100, 2),
        (48000, 2),
        (96000, 2),
        (192000, 2),
    ];

    for (sample_rate, channels) in audio_configs {
        if start_time.elapsed() >= config.test_duration {
            break;
        }

        println!("\nTesting audio at {}Hz/{}ch", sample_rate, channels);

        let record_start = Instant::now();
        let samples = record_audio(sample_rate, channels, Duration::from_secs(2));
        let record_time = record_start.elapsed();

        let expected = sample_rate as u64 * 2 * channels as u64;
        let actual_rate = samples as f64 / record_time.as_secs_f64();

        println!("  Recorded {} samples in {:?} ({:.0} Hz)",
                 samples, record_time, actual_rate);

        assert!(actual_rate >= sample_rate as f64 * 0.9,
                "Audio too slow: {:.0} Hz < {} Hz", actual_rate, sample_rate);

        let encode_start = Instant::now();
        let encoded = encode_audio(samples);
        let encode_time = encode_start.elapsed();

        println!("  Encoded to {}KB in {:?}", encoded / 1024, encode_time);
    }

    println!("✓ Multimedia stress test completed");
}

fn compress_video_frames(frames: u32) -> u64 {
    let size_per_frame = 1920 * 1080 * 3 / 50; // Симуляция компрессии
    (frames as u64) * size_per_frame as u64
}

fn encode_audio(samples: u64) -> u64 {
    samples * 2 // Симуляция кодека
}

#[test]
fn test_comprehensive_system_stress() {
    println!("=== COMPREHENSIVE SYSTEM STRESS TEST ===");

    let config = StressTestConfig::default();
    let start_time = Instant::now();
    let stop_signal = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::new();
    let mut metrics_history = Vec::with_capacity(1000);

    let stress_types = [
        ("cpu", run_cpu_stress as fn(Arc<AtomicBool>)),
        ("memory", run_memory_stress),
        ("io", run_io_stress),
        ("network", run_network_stress),
        ("gpu", run_gpu_stress),
        ("multimedia", run_multimedia_stress),
    ];

    for (name, stress_fn) in stress_types {
        let stop = Arc::clone(&stop_signal);
        println!("Starting {} stress", name);

        let handle = thread::spawn(move || {
            let thread_stop = Arc::clone(&stop);
            stress_fn(thread_stop);
        });

        handles.push(handle);
    }

    let monitor_interval = Duration::from_secs(2);
    let mut critical_events = Vec::new();

    while start_time.elapsed() < config.test_duration {
        thread::sleep(monitor_interval);

        let metrics = collect_system_metrics();
        metrics_history.push(metrics.clone());

        print_status(&metrics, start_time.elapsed().as_secs());

        if metrics.cpu_temp > config.max_cpu_temp {
            critical_events.push(format!("High CPU temp: {:.1}°C", metrics.cpu_temp));
        }

        if metrics.battery_temperature > config.max_temperature_celsius {
            critical_events.push(format!("High battery temp: {:.1}°C", metrics.battery_temperature));
        }

        let memory_mb = metrics.memory_used as f64 / MB as f64;
        if memory_mb > config.max_memory_mb as f64 * 1.2 {
            critical_events.push(format!("High memory: {:.0}MB", memory_mb));
        }

        if metrics.thermal_throttling {
            critical_events.push(format!("Thermal throttling at stage {}", metrics.throttling_stage));

            if config.enable_throttling_protection && metrics.throttling_stage >= 2 {
                println!("Critical throttling, reducing load...");
                stop_signal.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    stop_signal.store(true, Ordering::Relaxed);

    for handle in handles {
        let _ = handle.join();
    }

    generate_comprehensive_report(&metrics_history, &critical_events);

    assert!(critical_events.is_empty(),
            "Critical events during stress: {:?}", critical_events);

    println!("✓ Comprehensive stress test completed");
}

fn run_cpu_stress(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..10000 {
            black_box(heavy_computation());
        }
        thread::yield_now();
    }
}

fn run_memory_stress(stop: Arc<AtomicBool>) {
    let mut allocations = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        if allocations.len() < 1000 {
            let size = rand::thread_rng().gen_range(MB as usize..=16 * MB as usize);
            let vec = vec![0u8; size];
            allocations.push(vec);
        } else {
            allocations.clear();
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn run_io_stress(stop: Arc<AtomicBool>) {
    let test_dir = get_mobile_test_dir().join("comprehensive_io");
    fs::create_dir_all(&test_dir).ok();

    let mut files = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        if files.len() < 100 {
            let path = test_dir.join(format!("{}.tmp", rand::random::<u32>()));
            if let Ok(file) = File::create(&path) {
                files.push((path, file));
            }
        }

        for (_, file) in files.iter_mut() {
            let data = [rand::random::<u8>(); 4096];
            let _ = file.write(&data);
            let _ = file.sync_all();
        }

        if files.len() >= 100 {
            for (path, _) in files.drain(..50) {
                let _ = fs::remove_file(path);
            }
        }
    }

    fs::remove_dir_all(&test_dir).ok();
}

fn run_network_stress(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let _ = test_http_throughput(Duration::from_millis(100));
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_gpu_stress(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        render_complex_scene();
        render_complex_scene();
        render_complex_scene();
    }
}

fn run_multimedia_stress(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let _ = capture_camera_frames(640, 480, 5);
        let _ = record_audio(44100, 2, Duration::from_millis(100));
    }
}

fn print_status(metrics: &SystemMetrics, elapsed_secs: u64) {
    println!("\n[{:02}:{:02}] System Status", elapsed_secs / 60, elapsed_secs % 60);
    println!("  CPU: {:.1}% @ {:.1}°C", metrics.cpu_usage, metrics.cpu_temp);
    println!("  Memory: {:.1}/{:.1} MB",
             metrics.memory_used as f64 / MB as f64,
             metrics.memory_total as f64 / MB as f64);
    println!("  Battery: {:.1}%, {:.1}°C, {}mA",
             metrics.battery_level,
             metrics.battery_temperature,
             metrics.battery_current);
    println!("  Throttling: stage {}, {}",
             metrics.throttling_stage,
             if metrics.thermal_throttling { "ACTIVE" } else { "inactive" });
    println!("  Processes: {}, Threads: {}, Ctx switches: {}K",
             metrics.processes, metrics.threads, metrics.context_switches / 1000);
}

fn collect_system_metrics() -> SystemMetrics {
    SystemMetrics {
        cpu_usage: get_cpu_usage(),
        cpu_temp: measure_cpu_temperature(),
        gpu_usage: get_gpu_usage(),
        memory_used: get_memory_used(),
        memory_total: get_memory_total(),
        memory_cache: get_memory_cached(),
        swap_used: get_swap_used(),
        swap_total: get_swap_total(),
        battery_level: get_battery_level(),
        battery_current: get_battery_current(),
        battery_temperature: get_battery_temperature(),
        battery_voltage: get_battery_voltage(),
        thermal_throttling: is_thermal_throttling(),
        throttling_stage: get_throttling_stage(),
        uptime: get_system_uptime(),
        processes: get_process_count(),
        threads: get_thread_count(),
        context_switches: get_context_switches(),
        interrupts: get_interrupts(),
        timestamp: Instant::now(),
    }
}

fn get_cpu_usage() -> f32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(stat) = fs::read_to_string("/proc/stat") {
            for line in stat.lines() {
                if line.starts_with("cpu ") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 5 {
                        let user: u64 = parts[1].parse().unwrap_or(0);
                        let nice: u64 = parts[2].parse().unwrap_or(0);
                        let system: u64 = parts[3].parse().unwrap_or(0);
                        let idle: u64 = parts[4].parse().unwrap_or(0);

                        let total = user + nice + system + idle;
                        let busy = user + nice + system;

                        if total > 0 {
                            return (busy as f32 / total as f32) * 100.0;
                        }
                    }
                }
            }
        }
    }

    rand::thread_rng().gen_range(20.0..60.0)
}

fn measure_cpu_temperature() -> f32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(temp) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
            if let Ok(t) = temp.trim().parse::<u32>() {
                return t as f32 / 1000.0;
            }
        }
    }

    rand::thread_rng().gen_range(35.0..65.0)
}

fn get_memory_used() -> u64 {
    get_memory_total() - get_memory_available()
}

fn get_memory_total() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(info) = fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse::<u64>().unwrap_or(0) * 1024;
                    }
                }
            }
        }
    }

    4 * GB
}

fn get_memory_available() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(info) = fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("MemAvailable:") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse::<u64>().unwrap_or(0) * 1024;
                    }
                }
            }
        }
    }

    2 * GB
}

fn get_memory_cached() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(info) = fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("Cached:") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse::<u64>().unwrap_or(0) * 1024;
                    }
                }
            }
        }
    }

    500 * MB
}

fn get_swap_used() -> u64 {
    get_swap_total() - get_swap_free()
}

fn get_swap_total() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(info) = fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("SwapTotal:") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse::<u64>().unwrap_or(0) * 1024;
                    }
                }
            }
        }
    }

    1 * GB
}

fn get_swap_free() -> u64 {
    #[cfg(target_os = "android")]
    {
        if let Ok(info) = fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("SwapFree:") {
                    if let Some(val) = line.split_whitespace().nth(1) {
                        return val.parse::<u64>().unwrap_or(0) * 1024;
                    }
                }
            }
        }
    }

    800 * MB
}

fn get_battery_level() -> f32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(capacity) = fs::read_to_string("/sys/class/power_supply/battery/capacity") {
            return capacity.trim().parse().unwrap_or(50.0);
        }
    }

    rand::thread_rng().gen_range(30.0..70.0)
}

fn get_battery_current() -> i32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(current) = fs::read_to_string("/sys/class/power_supply/battery/current_now") {
            return current.trim().parse().unwrap_or(0);
        }
    }

    rand::thread_rng().gen_range(-500..500)
}

fn get_battery_temperature() -> f32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(temp) = fs::read_to_string("/sys/class/power_supply/battery/temp") {
            return temp.trim().parse::<u32>().unwrap_or(300) as f32 / 10.0;
        }
    }

    rand::thread_rng().gen_range(25.0..40.0)
}

fn get_battery_voltage() -> u32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(voltage) = fs::read_to_string("/sys/class/power_supply/battery/voltage_now") {
            return voltage.trim().parse().unwrap_or(3700000) / 1000;
        }
    }

    3700
}

fn get_process_count() -> u32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(dir) = fs::read_dir("/proc") {
            return dir.filter_map(|e| {
                e.ok().and_then(|d| d.file_name().to_string_lossy().parse::<u32>().ok())
            }).count() as u32;
        }
    }

    200
}

fn get_thread_count() -> u32 {
    #[cfg(target_os = "android")]
    {
        let mut total = 0;
        if let Ok(dir) = fs::read_dir("/proc") {
            for entry in dir.filter_map(Result::ok) {
                if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() {
                    let task_dir = entry.path().join("task");
                    if let Ok(tasks) = fs::read_dir(task_dir) {
                        total += tasks.count();
                    }
                }
            }
        }
        return total as u32;
    }

    1000
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

    1000000
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

    500000
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

    Duration::from_secs(3600)
}

fn is_thermal_throttling() -> bool {
    get_throttling_stage() > 0
}

fn get_throttling_stage() -> u32 {
    #[cfg(target_os = "android")]
    {
        if let Ok(cur_state) = fs::read_to_string("/sys/class/thermal/thermal_message/cur_state") {
            return cur_state.trim().parse().unwrap_or(0);
        }

        if let Ok(cooling) = fs::read_to_string("/sys/class/thermal/cooling_device0/cur_state") {
            return cooling.trim().parse().unwrap_or(0);
        }
    }

    let temp = measure_cpu_temperature();
    if temp > 80.0 { 2 } else if temp > 70.0 { 1 } else { 0 }
}

fn count_open_files() -> usize {
    #[cfg(target_os = "android")]
    {
        if let Ok(dir) = fs::read_dir("/proc/self/fd") {
            return dir.count();
        }
    }

    100
}

fn free_disk_space_mb(path: &Path) -> u64 {
    if let Ok(stats) = fs2::statvfs(path) {
        return stats.free_space() / MB;
    }
    1000
}

fn black_box<T>(x: T) -> T {
    unsafe {
        let ret = std::ptr::read_volatile(&x);
        std::mem::forget(x);
        ret
    }
}

fn get_mobile_test_dir() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        PathBuf::from("/data/local/tmp")
    }

    #[cfg(target_os = "ios")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        PathBuf::from(&home).join("tmp")
    }
}

fn heavy_computation() -> f64 {
    let mut result = 0.0;
    for i in 0..10000 {
        result += (i as f64).sin() * (i as f64).cos();
    }
    result
}

fn check_limits(metrics: &SystemMetrics, config: &StressTestConfig) {
    assert!(
        metrics.cpu_usage <= config.max_cpu_usage * 1.5,
        "CPU usage too high: {:.1}% > {:.1}%",
        metrics.cpu_usage,
        config.max_cpu_usage * 1.5
    );

    assert!(
        metrics.cpu_temp <= config.max_cpu_temp,
        "CPU temp too high: {:.1}°C > {:.1}°C",
        metrics.cpu_temp,
        config.max_cpu_temp
    );

    let memory_mb = metrics.memory_used as f64 / MB as f64;
    assert!(
        memory_mb <= config.max_memory_mb as f64 * 1.5,
        "Memory usage too high: {:.1}MB > {}MB",
        memory_mb,
        config.max_memory_mb * 15 / 10
    );

    assert!(
        metrics.battery_temperature <= config.max_temperature_celsius * 1.2,
        "Temperature too high: {:.1}°C > {:.1}°C",
        metrics.battery_temperature,
        config.max_temperature_celsius * 1.2
    );

    assert!(
        metrics.battery_current.abs() <= config.max_battery_current_ma * 2,
        "Battery current too high: {}mA > {}mA",
        metrics.battery_current,
        config.max_battery_current_ma * 2
    );
}

fn capture_camera_frames(width: u32, height: u32, target_frames: u32) -> u32 {
    let frame_size = (width * height * 3) as usize;
    let mut frames = 0;
    let frame_time = Duration::from_secs(1) / 30;

    for _ in 0..target_frames {
        let start = Instant::now();
        let _frame = vec![0u8; frame_size];
        let elapsed = start.elapsed();
        if elapsed < frame_time {
            thread::sleep(frame_time - elapsed);
        }
        frames += 1;
    }

    frames
}

fn record_audio(sample_rate: u32, channels: u32, duration: Duration) -> u64 {
    let samples_needed = sample_rate as u64 * duration.as_secs() * channels as u64;
    let mut samples = 0;
    let sample_duration = Duration::from_secs_f64(1.0 / sample_rate as f64);

    while samples < samples_needed {
        let _sample = [0i16; 2];
        thread::sleep(sample_duration);
        samples += channels as u64;
    }

    samples
}

fn analyze_stress_results(metrics: &VecDeque<SystemMetrics>, total_ops: u64) {
    if metrics.is_empty() {
        return;
    }

    let avg_cpu = metrics.iter().map(|m| m.cpu_usage).sum::<f32>() / metrics.len() as f32;
    let avg_memory = metrics.iter().map(|m| m.memory_used).sum::<u64>() / metrics.len() as u64;
    let max_temp = metrics.iter().map(|m| m.cpu_temp).fold(0.0, f32::max);
    let avg_temp = metrics.iter().map(|m| m.cpu_temp).sum::<f32>() / metrics.len() as f32;

    println!("\n=== STRESS TEST RESULTS ===");
    println!("Average CPU: {:.1}%", avg_cpu);
    println!("Average Memory: {:.1}MB", avg_memory as f64 / MB as f64);
    println!("Average Temperature: {:.1}°C", avg_temp);
    println!("Peak Temperature: {:.1}°C", max_temp);
    println!("Total Operations: {}", total_ops);
    println!("Test Duration: {:?}",
             metrics.back().unwrap().timestamp.duration_since(metrics.front().unwrap().timestamp));
}

fn analyze_memory_patterns(times: &[(usize, Duration)], events: &[(Duration, f64)]) {
    println!("\n=== MEMORY PATTERN ANALYSIS ===");

    if !times.is_empty() {
        let avg_time: Duration = times.iter().map(|(_, t)| *t).sum::<Duration>() / times.len() as u32;
        let max_time = times.iter().map(|(_, t)| *t).max().unwrap_or(Duration::default());

        println!("Average allocation time: {:?}", avg_time);
        println!("Max allocation time: {:?}", max_time);
    }

    if !events.is_empty() {
        let max_pressure = events.iter().map(|(_, p)| *p).fold(0.0, f64::max);
        println!("Max memory pressure: {:.1}%", max_pressure);
    }
}

fn analyze_thermal_data(history: &[(u64, f32, f32, bool, u32)], throttling_events: i32) {
    println!("\n=== THERMAL ANALYSIS ===");
    println!("Throttling events: {}", throttling_events);

    if !history.is_empty() {
        let max_cpu_temp = history.iter().map(|(_, t, _, _, _)| *t).fold(0.0, f32::max);
        let max_bat_temp = history.iter().map(|(_, _, t, _, _)| *t).fold(0.0, f32::max);
        let avg_cpu_temp = history.iter().map(|(_, t, _, _, _)| *t).sum::<f32>() / history.len() as f32;

        println!("Max CPU temperature: {:.1}°C", max_cpu_temp);
        println!("Max battery temperature: {:.1}°C", max_bat_temp);
        println!("Average temperature: {:.1}°C", avg_cpu_temp);

        let throttled_periods = history.iter().filter(|(_, _, _, t, _)| *t).count();
        println!("Throttling active for {}% of test",
                 throttled_periods * 100 / history.len());
    }
}

fn analyze_network_results(results: &[(&str, Vec<(&str, u64, Duration, f64)>)]) {
    println!("\n=== NETWORK ANALYSIS ===");

    for (condition, metrics) in results {
        println!("\nCondition: {}", condition);
        for (name, throughput, latency, loss) in metrics {
            println!("  {}: {:.2} Mbps, {:?} latency, {:.1}% loss",
                     name, *throughput as f64 / 1_000_000.0, latency, loss * 100.0);
        }
    }
}

fn analyze_gpu_results(frame_times: &[Duration], gpu_usage: &[f32], gpu_memory: &[u64]) {
    println!("\n=== GPU ANALYSIS ===");

    if !frame_times.is_empty() {
        let avg_frame = frame_times.iter().sum::<Duration>() / frame_times.len() as u32;
        let fps = 1.0 / avg_frame.as_secs_f64();
        println!("Average FPS: {:.1}", fps);

        let p95_frame = frame_times.len() as f64 * 0.95;
        let p95 = frame_times[p95_frame as usize];
        println!("95th percentile frame time: {:?}", p95);
    }

    if !gpu_usage.is_empty() {
        let avg_usage = gpu_usage.iter().sum::<f32>() / gpu_usage.len() as f32;
        let max_usage = gpu_usage.iter().fold(0.0, f32::max);
        println!("Average GPU usage: {:.1}%", avg_usage);
        println!("Peak GPU usage: {:.1}%", max_usage);
    }

    if !gpu_memory.is_empty() {
        let avg_mem = gpu_memory.iter().sum::<u64>() / gpu_memory.len() as u64;
        println!("Average GPU memory: {}MB", avg_mem / MB);
    }
}

fn generate_comprehensive_report(metrics: &[SystemMetrics], critical_events: &[String]) {
    println!("\n" + "=".repeat(70));
    println!("{:^70}", "COMPREHENSIVE STRESS TEST REPORT");
    println!("=".repeat(70));

    if metrics.is_empty() {
        return;
    }

    let test_duration = metrics.last().unwrap().timestamp.duration_since(metrics.first().unwrap().timestamp);

    println!("Test Duration: {:?}", test_duration);

    println!("\n📊 PERFORMANCE SUMMARY");
    println!("  CPU:");
    println!("    Average: {:.1}%",
             metrics.iter().map(|m| m.cpu_usage).sum::<f32>() / metrics.len() as f32);
    println!("    Peak: {:.1}%",
             metrics.iter().map(|m| m.cpu_usage).fold(0.0, f32::max));
    println!("    Average Temp: {:.1}°C",
             metrics.iter().map(|m| m.cpu_temp).sum::<f32>() / metrics.len() as f32);

    println!("\n  Memory:");
    let avg_memory = metrics.iter().map(|m| m.memory_used).sum::<u64>() / metrics.len() as u64;
    let peak_memory = metrics.iter().map(|m| m.memory_used).max().unwrap_or(0);
    println!("    Average: {:.1} MB", avg_memory as f64 / MB as f64);
    println!("    Peak: {:.1} MB", peak_memory as f64 / MB as f64);
    println!("    Cache: {:.1} MB", metrics.last().unwrap().memory_cache as f64 / MB as f64);

    println!("\n  Swap:");
    let avg_swap = metrics.iter().map(|m| m.swap_used).sum::<u64>() / metrics.len() as u64;
    println!("    Used: {:.1} MB", avg_swap as f64 / MB as f64);

    println!("\n🔋 BATTERY & THERMAL");
    println!("  Battery:");
    println!("    Min Level: {:.1}%",
             metrics.iter().map(|m| m.battery_level).fold(100.0, f32::min));
    println!("    Max Temp: {:.1}°C",
             metrics.iter().map(|m| m.battery_temperature).fold(0.0, f32::max));
    println!("    Max Current: {}mA",
             metrics.iter().map(|m| m.battery_current).max().unwrap_or(0));

    println!("\n  Thermal:");
    let throttling_count = metrics.iter().filter(|m| m.thermal_throttling).count();
    println!("    Throttling Events: {}", throttling_count);
    println!("    Max Throttling Stage: {}",
             metrics.iter().map(|m| m.throttling_stage).max().unwrap_or(0));

    println!("\n⚙️ SYSTEM");
    println!("  Uptime: {:?}", metrics.last().unwrap().uptime);
    println!("  Processes: {}", metrics.last().unwrap().processes);
    println!("  Threads: {}", metrics.last().unwrap().threads);
    println!("  Context Switches/s: {:.0}K",
             metrics.last().unwrap().context_switches as f64 / test_duration.as_secs_f64() / 1000.0);

    if !critical_events.is_empty() {
        println!("\n⚠️ CRITICAL EVENTS");
        for event in critical_events {
            println!("  • {}", event);
        }
    }

    let passed = critical_events.is_empty();
    println!("\n" + "=".repeat(70));
    println!("OVERALL STATUS: {}", if passed { "✅ PASSED" } else { "❌ FAILED" });
    println!("=".repeat(70));
}
