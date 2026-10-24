//! Inspectable source-to-post-clamp latency through the real engine workers.
//!
//! Opt-in host fixture, never a hardware deadline or physical latency result.

// Rust guideline compliant 2026-02-21

use driver_common::{CaptureInput, CaptureSource, DriverConfig};
use serde::Serialize;
use serde_json::json;
use sotf_audio::lab_pcm::{LabPcmObserver, TestOutput};
use sotf_audio::{AudioEngineManager, PluginConfig, SinkType};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// Eighteen mantissa bits identify each frame in the one-second window at
// 192 kHz. The other five identify up to 32 channels, all below clipping.
const FRAME_BITS: u32 = 18;
const MARKER_EXPONENT: u32 = 122;
const CASE_TIMEOUT: Duration = Duration::from_secs(15);

fn marker(frame: usize, channel: usize, half_gain: bool) -> f32 {
    f32::from_bits(
        ((MARKER_EXPONENT - u32::from(half_gain)) << 23)
            | ((channel as u32) << FRAME_BITS)
            | frame as u32,
    )
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct CaptureClock {
    config: DriverConfig,
    warmup_frames: usize,
    marked_frames: usize,
    consumed: AtomicUsize,
    maximum_available: AtomicUsize,
    origin: OnceLock<Instant>,
    read_times: Box<[AtomicU64]>,
    corrupt_channel: bool,
}

impl CaptureClock {
    fn available(&self) -> usize {
        let origin = self.origin.get_or_init(Instant::now);
        let generated =
            (origin.elapsed().as_nanos() * u128::from(self.config.sample_rate) / 1_000_000_000)
                .min((self.warmup_frames + self.marked_frames) as u128) as usize;
        let available = generated.saturating_sub(self.consumed.load(Ordering::Acquire));
        self.maximum_available
            .fetch_max(available, Ordering::Relaxed);
        available
    }
}

#[derive(Clone, Debug)]
struct PacedCapture(Arc<CaptureClock>);

impl CaptureInput for PacedCapture {
    fn configuration(&self) -> DriverConfig {
        self.0.config
    }

    fn available_frames(&self) -> usize {
        self.0.available()
    }

    fn read_frames(&self, output: &mut [f32], expected: DriverConfig) -> usize {
        let clock = &self.0;
        let channels = clock.config.channel_count as usize;
        let frames = clock.config.buffer_frames as usize;
        if expected != clock.config
            || output.len() / channels < frames
            || clock.available() < frames
        {
            return 0;
        }
        let begin = clock.consumed.load(Ordering::Relaxed);
        let at = nanos(clock.origin.get().unwrap().elapsed());
        clock.read_times[begin / frames].store(at, Ordering::Release);
        for (offset, frame) in output[..frames * channels]
            .chunks_exact_mut(channels)
            .enumerate()
        {
            let absolute = begin + offset;
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = if absolute < clock.warmup_frames {
                    0.0
                } else {
                    let encoded_channel = if clock.corrupt_channel {
                        channel ^ 1
                    } else {
                        channel
                    };
                    marker(absolute - clock.warmup_frames + 1, encoded_channel, false)
                };
            }
        }
        clock.consumed.store(begin + frames, Ordering::Release);
        frames
    }
}

#[derive(Clone, Debug, Serialize)]
struct FirstError {
    kind: &'static str,
    frame: usize,
    channel: usize,
    expected_bits: u32,
    actual_bits: u32,
}

#[derive(Debug, Serialize)]
struct TimingPair {
    first_marker_frame: usize,
    virtual_capture_ns: u64,
    source_read_ns: u64,
    sink_observed_ns: u64,
}

#[derive(Debug, Serialize)]
struct Observations {
    marked_frames: usize,
    verified_samples: usize,
    leading_silent_frames: usize,
    silent_frames_after_signal: usize,
    invalid_samples: usize,
    first_error: Option<FirstError>,
    first_generation: Option<u64>,
    last_generation: Option<u64>,
    timing_pairs: Vec<TimingPair>,
}

#[derive(Debug)]
struct LatencyObserver {
    clock: Arc<CaptureClock>,
    output_channels: usize,
    half_gain: bool,
    complete: AtomicBool,
    observations: Mutex<Observations>,
}

impl LabPcmObserver for LatencyObserver {
    fn observe(&self, samples: &[f32], channels: usize, generation: u64, at: Instant) {
        if self.complete.load(Ordering::Relaxed) {
            return;
        }
        let mut state = self.observations.lock().unwrap();
        if channels != self.output_channels || !samples.len().is_multiple_of(channels) {
            state.invalid_samples += 1;
            return;
        }
        for frame in samples.chunks_exact(channels) {
            if state.marked_frames == self.clock.marked_frames {
                self.complete.store(true, Ordering::Release);
                return;
            }
            if frame.iter().all(|&sample| sample == 0.0) {
                if state.marked_frames == 0 {
                    state.leading_silent_frames += 1;
                } else {
                    state.silent_frames_after_signal += 1;
                }
                continue;
            }
            let index = state.marked_frames;
            for (channel, &sample) in frame.iter().enumerate() {
                let expected = marker(index + 1, channel, self.half_gain).to_bits();
                if sample.to_bits() != expected {
                    state.invalid_samples += 1;
                    state.first_error.get_or_insert(FirstError {
                        kind: "PCM marker/channel/gain mismatch",
                        frame: index,
                        channel,
                        expected_bits: expected,
                        actual_bits: sample.to_bits(),
                    });
                } else {
                    state.verified_samples += 1;
                }
            }
            if state.first_generation.is_none() {
                state.first_generation = Some(generation);
            } else if state.last_generation != Some(generation) {
                state.invalid_samples += 1;
            }
            state.last_generation = Some(generation);
            if index.is_multiple_of(self.clock.config.buffer_frames as usize) {
                let absolute = self.clock.warmup_frames + index;
                let source_read = self.clock.read_times
                    [absolute / self.clock.config.buffer_frames as usize]
                    .load(Ordering::Acquire);
                let sink = self
                    .clock
                    .origin
                    .get()
                    .map(|origin| nanos(at.duration_since(*origin)))
                    .unwrap_or(0);
                let generated =
                    (absolute as u64 * 1_000_000_000) / u64::from(self.clock.config.sample_rate);
                if source_read == 0 || sink < source_read || source_read < generated {
                    state.invalid_samples += 1;
                }
                state.timing_pairs.push(TimingPair {
                    first_marker_frame: index,
                    virtual_capture_ns: generated,
                    source_read_ns: source_read,
                    sink_observed_ns: sink,
                });
            }
            state.marked_frames += 1;
        }
        if state.marked_frames == self.clock.marked_frames {
            self.complete.store(true, Ordering::Release);
        }
    }
}

fn distribution(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    let quantile =
        |percent: usize| values[(values.len() * percent).div_ceil(100).saturating_sub(1)];
    json!({"count":values.len(), "p50_ns":quantile(50), "p95_ns":quantile(95),
           "p99_ns":quantile(99), "max_ns":values.last().unwrap()})
}

fn run_case(
    channels: usize,
    rate: u32,
    block: usize,
    half_gain: bool,
) -> Result<serde_json::Value, String> {
    let output_channels = channels.min(sotf_audio::EngineConfig::MAX_OUTPUT_CHANNELS);
    let warmup_frames = (rate as usize / 4).div_ceil(block) * block;
    let marked_frames = (rate as usize).div_ceil(block) * block;
    assert!(marked_frames < 1 << FRAME_BITS);
    let clock = Arc::new(CaptureClock {
        config: DriverConfig::new(rate, block as u32, channels as u32),
        warmup_frames,
        marked_frames,
        consumed: AtomicUsize::new(0),
        maximum_available: AtomicUsize::new(0),
        origin: OnceLock::new(),
        read_times: (0..(warmup_frames + marked_frames) / block)
            .map(|_| AtomicU64::new(0))
            .collect(),
        corrupt_channel: std::env::var_os("SOTF_LAB_LATENCY_SWAP_CHANNELS").is_some(),
    });
    let observer = Arc::new(LatencyObserver {
        clock: Arc::clone(&clock),
        output_channels,
        half_gain,
        complete: AtomicBool::new(false),
        observations: Mutex::new(Observations {
            marked_frames: 0,
            verified_samples: 0,
            leading_silent_frames: 0,
            silent_frames_after_signal: 0,
            invalid_samples: 0,
            first_error: None,
            first_generation: None,
            last_generation: None,
            timing_pairs: Vec::with_capacity(marked_frames / block),
        }),
    });
    let output = TestOutput::new("Latency measurement output");
    output
        .probe()
        .set_observer(observer.clone())
        .map_err(|_| "observer already installed")?;
    let mut plugins = vec![PluginConfig::new(
        "gain",
        json!({"gain_db": if half_gain { -6.020599913279624 } else { 0.0 }}),
    )];
    if channels != output_channels {
        let mut matrix = vec![0.0f32; channels * output_channels];
        for channel in 0..output_channels {
            matrix[channel * channels + channel] = 1.0;
        }
        plugins.push(PluginConfig::new(
            "matrix",
            json!({"input_channels":channels,
            "output_channels":output_channels,"matrix":matrix}),
        ));
    }
    let mut manager = AudioEngineManager::new();
    manager.set_volume(1.0).map_err(|error| error.to_string())?;
    manager
        .start_driver_playback_with_source(
            Some(output.name().to_owned()),
            plugins,
            output_channels,
            rate,
            block as u32,
            channels,
            SinkType::TestOutput(output),
            Some(CaptureSource::new(PacedCapture(Arc::clone(&clock)))),
        )
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + CASE_TIMEOUT;
    while !observer.complete.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let finished = observer.complete.load(Ordering::Acquire);
    manager.stop().map_err(|error| error.to_string())?;
    let state = observer.observations.lock().unwrap();
    if !finished
        || state.invalid_samples != 0
        || state.verified_samples != marked_frames * output_channels
    {
        return Err(format!(
            "{channels}ch->{output_channels}ch/{rate}Hz/{block}/half={half_gain}: finished={finished}, observed_frames={}, expected_frames={marked_frames}, invalid_samples={}, first_error={:?}, silent_after_signal={}",
            state.marked_frames,
            state.invalid_samples,
            state.first_error,
            state.silent_frames_after_signal
        ));
    }
    if state.timing_pairs.len() != marked_frames / block {
        return Err("missing source/sink timestamp pairs".into());
    }
    let virtual_latency = distribution(
        state
            .timing_pairs
            .iter()
            .map(|pair| pair.sink_observed_ns - pair.virtual_capture_ns)
            .collect(),
    );
    let read_latency = distribution(
        state
            .timing_pairs
            .iter()
            .map(|pair| pair.sink_observed_ns - pair.source_read_ns)
            .collect(),
    );
    println!(
        "verified {channels}->{output_channels}ch {rate}Hz/{block} half={half_gain}: source-read to sink p99={}ns, silent frames after signal={}",
        read_latency["p99_ns"], state.silent_frames_after_signal
    );
    Ok(
        json!({"input_channels":channels,"output_channels":output_channels,"sample_rate":rate,
        "block_frames":block,"gain_db":if half_gain { -6.020599913279624 } else { 0.0 },
        "warmup_frames":warmup_frames,"expected_marker_frames":marked_frames,
        "source_frames_consumed":clock.consumed.load(Ordering::Relaxed),
        "maximum_virtual_source_backlog_frames":clock.maximum_available.load(Ordering::Relaxed),
        "virtual_capture_to_sink":virtual_latency,"source_read_to_sink":read_latency,
        "observations":&*state}),
    )
}

fn run() -> Result<(), String> {
    let path = std::env::var_os("SOTF_LAB_LATENCY_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/lab-latency/report.json")
        });
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let smoke = std::env::args()
        .skip(1)
        .any(|argument| argument == "--smoke");
    let mut cases = Vec::new();
    if smoke {
        cases.push(run_case(2, 48_000, 128, false)?);
        cases.push(run_case(32, 96_000, 512, true)?);
    } else {
        for channels in [2, 8, 32] {
            for rate in [48_000, 96_000, 192_000] {
                for block in [64, 128, 512] {
                    for half_gain in [false, true] {
                        cases.push(run_case(channels, rate, block, half_gain)?);
                    }
                }
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let report = json!({"scope":"paced synthetic CaptureInput -> real decoder/DSP/queues/playback -> post-clamp simulated output",
        "limits":"No HAL/CPAL device queues, native backend callbacks, hardware clocks or physical loopback. Observer overhead is included. Two static gain profiles; no device-event or live profile-churn qualification.",
        "buffer_ms":200,"timestamp_origin":"shared monotonic Instant; raw block pairs retained",
        "smoke":smoke,"complete":true,"cases":cases});
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
