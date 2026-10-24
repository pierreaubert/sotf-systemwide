//! Native host measurements of the actual production capture closure.
//!
//! No device is opened. Cold callbacks run on fresh threads with the same
//! parking_lot feature set as the daemon, including deadlock detection.

// Rust guideline compliant 2026-02-21

mod allocation;
mod clock;

#[expect(
    dead_code,
    reason = "The standalone probe imports the complete production driver without opening devices."
)]
#[path = "../../daemon/bin/cpal_capture.rs"]
mod cpal_capture;

use allocation::Window;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: sotf-callback-metrics <report.json>")?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some("poison-fixture") {
            previous_hook(info);
        }
    }));
    let mut reports = Vec::new();
    for channels in [2, 8, 32] {
        for frames in [64, 512] {
            for mode in ["drain", "overflow", "contended", "poisoned"] {
                reports.push(run_case::<f32>("f32", channels, frames, mode)?);
                reports.push(run_case::<f64>("f64", channels, frames, mode)?);
                reports.push(run_case::<i16>("i16", channels, frames, mode)?);
                reports.push(run_case::<u16>("u16", channels, frames, mode)?);
                reports.push(run_case::<i8>("i8", channels, frames, mode)?);
                reports.push(run_case::<cpal::I24>("i24", channels, frames, mode)?);
                reports.push(run_case::<i32>("i32", channels, frames, mode)?);
                reports.push(run_case::<i64>("i64", channels, frames, mode)?);
                reports.push(run_case::<u8>("u8", channels, frames, mode)?);
                reports.push(run_case::<cpal::U24>("u24", channels, frames, mode)?);
                reports.push(run_case::<u32>("u32", channels, frames, mode)?);
                reports.push(run_case::<u64>("u64", channels, frames, mode)?);
            }
        }
    }
    let failed: u64 = reports
        .iter()
        .map(|case| case["allocations"].as_u64().unwrap() + case["deallocations"].as_u64().unwrap())
        .sum();
    let report = json!({"scope":"production CPAL data closure; native host fixture, no devices", "cases":reports});
    let path = std::path::PathBuf::from(path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    if failed != 0 {
        return Err(format!("capture callbacks allocated/freed {failed} times").into());
    }
    println!(
        "CPAL capture callback: {} cases, no allocations",
        reports.len()
    );
    Ok(())
}

fn run_case<T>(
    name: &'static str,
    channels: usize,
    frames: usize,
    mode: &'static str,
) -> Result<Value, Box<dyn std::error::Error>>
where
    T: cpal::Sample + cpal::FromSample<f32> + Copy + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let samples = frames * channels;
    let capacity = samples * 8;
    let queue = Arc::new(cpal_capture::CaptureQueue::new(VecDeque::with_capacity(
        capacity,
    )));
    let drops = Arc::new(AtomicU64::new(0));
    let mut callback = cpal_capture::make_input_callback(
        Arc::clone(&queue),
        Arc::new(AtomicUsize::new(capacity)),
        Arc::clone(&drops),
        channels,
        |sample: T| sample.to_sample::<f32>(),
    );
    let data = vec![T::from_sample(0.25_f32); samples];
    // The control thread prepares storage. The callback thread has never used
    // this mutex (or any parking_lot lock) before its first observed call.
    let reserved = queue.lock().capacity();
    if mode == "poisoned" {
        let poisoned = Arc::clone(&queue);
        assert!(
            std::thread::Builder::new()
                .name("poison-fixture".into())
                .spawn(move || {
                    let mut queued = poisoned.lock();
                    queued.extend([-9.0; 3]);
                    panic!("partial frame from an interrupted consumer");
                })?
                .join()
                .is_err()
        );
    }
    let held = (mode == "contended").then(|| queue.lock());
    let worker_queue = Arc::clone(&queue);
    let (finished, finished_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let queue = worker_queue;
        let canary = Window::begin();
        drop(std::hint::black_box(Box::new([0u8; 64])));
        let canary = canary.finish();
        assert!(canary.allocations > 0 && canary.deallocations > 0);
        let mut allocations = 0;
        let mut deallocations = 0;
        let mut times = Vec::with_capacity(2_000);
        let mut cpu_times = Vec::with_capacity(2_000);
        let mut cold = allocation::Counts::default();
        for iteration in 0..2_000 {
            let cpu_start = clock::thread_cpu_ns();
            let start = Instant::now();
            let window = Window::begin();
            std::hint::black_box(&mut callback)(std::hint::black_box(&data));
            let counts = window.finish();
            let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap();
            cpu_times.push(clock::thread_cpu_ns() - cpu_start);
            if iteration == 0 {
                cold = counts;
            }
            allocations += counts.allocations;
            deallocations += counts.deallocations;
            times.push(elapsed);
            if mode == "drain" {
                queue.lock().clear();
            }
        }
        finished.send(()).unwrap();
        let queued = queue.lock();
        assert!(queued.len().is_multiple_of(channels));
        assert_eq!(queued.capacity(), reserved);
        assert!(
            queued
                .iter()
                .all(|sample| sample.is_finite() && (*sample - 0.25).abs() < 0.001)
        );
        if mode == "contended" {
            assert!(queued.is_empty());
            assert_eq!(drops.load(Ordering::Relaxed), (frames * 2_000) as u64);
        } else if mode == "overflow" || mode == "poisoned" {
            assert_eq!(queued.len(), capacity);
            assert_eq!(drops.load(Ordering::Relaxed), (frames * 1_992) as u64);
        } else {
            assert_eq!(drops.load(Ordering::Relaxed), 0);
        }
        times.sort_unstable();
        cpu_times.sort_unstable();
        json!({"format":name,"channels":channels,"frames":frames,"mode":mode,"calls":times.len(),"allocations":allocations,"deallocations":deallocations,"p50_ns":times[999],"p95_ns":times[1899],"p99_ns":times[1979],"max_ns":times[1999],"first_callback_allocations":cold.allocations,"first_callback_deallocations":cold.deallocations,"thread_cpu_p50_ns":cpu_times[999],"thread_cpu_p95_ns":cpu_times[1899],"thread_cpu_p99_ns":cpu_times[1979],"thread_cpu_max_ns":cpu_times[1999],"observer_canary_allocations":canary.allocations,"observer_canary_deallocations":canary.deallocations})
    });
    finished_rx.recv_timeout(std::time::Duration::from_secs(30))?;
    drop(held);
    worker
        .join()
        .map_err(|_| "capture callback measurement panicked".into())
}
