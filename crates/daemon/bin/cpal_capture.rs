//! Cross-platform audio capture driver backed by `cpal` input streams.
//!
//! This is the capture path for Linux and Windows, where no privileged
//! system-audio tap exists like the macOS CoreAudio HAL driver:
//!
//! - Linux: `cpal` opens the default (or `SOTF_CAPTURE_DEVICE`-selected) input,
//!   which on PipeWire/PulseAudio setups is typically a monitor source.
//! - Windows: `cpal` opens the WASAPI default input (or selected endpoint).
//!   WASAPI render-endpoint loopback is not exposed through `cpal`, so
//!   system-audio capture needs a virtual-cable/monitor device named via
//!   `SOTF_CAPTURE_DEVICE`.
//! - macOS without the `hal` feature: only via `SOTF_SYSTEMWIDE_DRIVER=cpal`,
//!   then the default input (microphone/line-in); the manager default there
//!   is `NullDriver`.
//!
//! The realtime input callback converts every hardware sample format to `f32`
//! and pushes it into a bounded queue; `read_audio` drains complete
//! interleaved frames. The queue never grows without bound: on overflow the
//! oldest samples are dropped and counted (`overflow_count`), so a stalled
//! consumer degrades to latency instead of memory growth. The callback uses
//! `try_lock` so it never blocks the realtime thread on the consumer.
//!
//! The module depends only on external crates (`cpal`, `driver_common`,
//! `parking_lot`, `log`), never on other daemon modules, so the full file
//! compiles standalone against the real crates for verification.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use driver_common::{AudioDriver, ConfigResult, DriverConfig, DriverError, DriverStatus};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Environment override selecting the capture endpoint by name substring
/// (case-insensitive). Unset or empty means the default input device.
pub const CAPTURE_DEVICE_ENV: &str = "SOTF_CAPTURE_DEVICE";

const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 192_000;
const MIN_BUFFER_FRAMES: u32 = 64;
const MAX_BUFFER_FRAMES: u32 = 8_192;
const MAX_CHANNELS: u32 = 32;
const DEFAULT_BUFFER_FRAMES: u32 = 512;
/// Queue capacity as a multiple of one driver buffer (`frames * channels`).
const RING_CAPACITY_MULTIPLE: usize = 8;

#[cfg(target_os = "linux")]
const DRIVER_NAME: &str = "PipeWire monitor (cpal)";
#[cfg(target_os = "windows")]
const DRIVER_NAME: &str = "WASAPI capture (cpal)";
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
const DRIVER_NAME: &str = "CoreAudio capture (cpal)";

#[derive(Debug)]
struct CaptureState {
    initialized: bool,
    engine_ready: bool,
    installed: bool,
    wanted_rate: u32,
    wanted_channels: u32,
    wanted_frames: u32,
    actual_rate: u32,
    actual_channels: u32,
    actual_frames: u32,
}

/// `cpal`-backed [`AudioDriver`] for Linux/Windows (/macOS without HAL).
///
/// See the module docs for the platform mapping and queue discipline.
pub struct CpalCaptureDriver {
    state: Mutex<CaptureState>,
    queue: Arc<Mutex<VecDeque<f32>>>,
    queue_capacity_samples: Arc<AtomicUsize>,
    overflow_drops: Arc<AtomicU64>,
    stream: Option<Stream>,
}

impl std::fmt::Debug for CpalCaptureDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        formatter
            .debug_struct("CpalCaptureDriver")
            .field("state", &*state)
            .field("queued_samples", &self.queue.lock().len())
            .field("stream_live", &self.stream.is_some())
            .finish()
    }
}

impl CpalCaptureDriver {
    /// Create an uninitialized driver with device-default format wants.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CaptureState {
                initialized: false,
                engine_ready: false,
                installed: false,
                wanted_rate: 0,
                wanted_channels: 0,
                wanted_frames: DEFAULT_BUFFER_FRAMES,
                actual_rate: 48_000,
                actual_channels: 2,
                actual_frames: DEFAULT_BUFFER_FRAMES,
            }),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            queue_capacity_samples: Arc::new(AtomicUsize::new(
                DEFAULT_BUFFER_FRAMES as usize * 2 * RING_CAPACITY_MULTIPLE,
            )),
            overflow_drops: Arc::new(AtomicU64::new(0)),
            stream: None,
        }
    }

    /// Number of captured samples dropped because the consumer stalled.
    pub fn overflow_count(&self) -> u64 {
        self.overflow_drops.load(Ordering::Relaxed)
    }

    /// Push samples as the realtime callback would (test seam).
    ///
    /// Applies the same bound-and-drop-oldest rule as the input callback so
    /// queue discipline is testable without audio hardware.
    #[cfg(test)]
    fn push_test_samples(&self, samples: &[f32]) {
        push_samples(
            &self.queue,
            &self.queue_capacity_samples,
            &self.overflow_drops,
            samples.iter().copied(),
        );
    }

    fn capture_active(&self) -> bool {
        let state = self.state.lock();
        state.installed && state.engine_ready && self.stream.is_some()
    }

    fn refresh_capacity(&self, frames: u32, channels: u32) {
        let capacity = (frames as usize)
            .saturating_mul(channels.max(1) as usize)
            .saturating_mul(RING_CAPACITY_MULTIPLE)
            .max(1);
        self.queue_capacity_samples
            .store(capacity, Ordering::Relaxed);
    }
}

impl Default for CpalCaptureDriver {
    fn default() -> Self {
        Self::new()
    }
}

/// Push converted samples, dropping the oldest on overflow (shared by the
/// realtime callback and the test seam so both obey one discipline).
fn push_samples(
    queue: &Arc<Mutex<VecDeque<f32>>>,
    capacity: &Arc<AtomicUsize>,
    drops: &Arc<AtomicU64>,
    samples: impl Iterator<Item = f32>,
) {
    let Some(mut guard) = queue.try_lock() else {
        drops.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let cap = capacity.load(Ordering::Relaxed).max(1);
    for sample in samples {
        if guard.len() >= cap {
            guard.pop_front();
            drops.fetch_add(1, Ordering::Relaxed);
        }
        guard.push_back(sample);
    }
}

/// Drain complete interleaved frames from `queue` into `buffer`.
///
/// Writes exactly `frames * channels` samples and never touches the
/// partial-frame tail; returns the frame count. An empty queue (underrun)
/// returns `0` and leaves `buffer` untouched.
fn drain_frames(queue: &Mutex<VecDeque<f32>>, channels: usize, buffer: &mut [f32]) -> usize {
    let channels = channels.max(1);
    let complete_samples = buffer.len() - (buffer.len() % channels);
    if complete_samples == 0 {
        return 0;
    }
    let mut guard = queue.lock();
    let frames = (complete_samples / channels).min(guard.len() / channels);
    if frames == 0 {
        return 0;
    }
    for slot in buffer[..frames * channels].iter_mut() {
        // `frames` is bounded by `guard.len() / channels`, so the queue
        // cannot empty mid-drain; the fallback is unreachable in practice.
        *slot = guard.pop_front().unwrap_or(0.0);
    }
    frames
}

/// Build an input stream converting hardware samples of type `T` to `f32`.
///
/// Conversion is a `fn` parameter rather than a generic `Sample` bound because
/// `f32: FromSample<T>` only holds per concrete format, not generically; each
/// `build_stream` arm below passes a closure over its concrete sample type.
fn build_stream_for_format<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    queue: Arc<Mutex<VecDeque<f32>>>,
    capacity: Arc<AtomicUsize>,
    drops: Arc<AtomicU64>,
    convert: fn(T) -> f32,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: SizedSample + Copy + 'static,
{
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            push_samples(
                &queue,
                &capacity,
                &drops,
                data.iter().map(|sample| convert(*sample)),
            );
        },
        |error| log::warn!("[CpalCapture] Input stream error: {error}"),
        None,
    )
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    queue: Arc<Mutex<VecDeque<f32>>>,
    capacity: Arc<AtomicUsize>,
    drops: Arc<AtomicU64>,
) -> Result<Stream, cpal::BuildStreamError> {
    // `SampleFormat` is non-exhaustive, so the wildcard arm is required: a
    // future format degrades to "unsupported config" instead of breaking the
    // match.
    match format {
        SampleFormat::I8 => {
            build_stream_for_format::<i8>(device, config, queue, capacity, drops, |s| s.to_sample())
        }
        SampleFormat::I16 => {
            build_stream_for_format::<i16>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::I24 => {
            build_stream_for_format::<cpal::I24>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::I32 => {
            build_stream_for_format::<i32>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::I64 => {
            build_stream_for_format::<i64>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::U8 => {
            build_stream_for_format::<u8>(device, config, queue, capacity, drops, |s| s.to_sample())
        }
        SampleFormat::U16 => {
            build_stream_for_format::<u16>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::U24 => {
            build_stream_for_format::<cpal::U24>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::U32 => {
            build_stream_for_format::<u32>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::U64 => {
            build_stream_for_format::<u64>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::F32 => {
            build_stream_for_format::<f32>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        SampleFormat::F64 => {
            build_stream_for_format::<f64>(device, config, queue, capacity, drops, |s| {
                s.to_sample()
            })
        }
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}

/// Validate a configuration request; `0` keeps the current value.
fn validate_config(config: DriverConfig) -> Result<(), DriverError> {
    if config.sample_rate != 0 && !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&config.sample_rate)
    {
        return Err(DriverError::invalid_config(
            "sample_rate",
            format!(
                "unsupported capture rate {} (allowed: 0 or {MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE})",
                config.sample_rate
            ),
        ));
    }
    if config.channel_count != 0 && !(1..=MAX_CHANNELS).contains(&config.channel_count) {
        return Err(DriverError::invalid_config(
            "channel_count",
            format!(
                "unsupported capture channels {} (allowed: 0 or 1..={MAX_CHANNELS})",
                config.channel_count
            ),
        ));
    }
    if config.buffer_frames != 0
        && !(MIN_BUFFER_FRAMES..=MAX_BUFFER_FRAMES).contains(&config.buffer_frames)
    {
        return Err(DriverError::invalid_config(
            "buffer_frames",
            format!(
                "unsupported capture buffer {} (allowed: 0 or {MIN_BUFFER_FRAMES}..={MAX_BUFFER_FRAMES})",
                config.buffer_frames
            ),
        ));
    }
    Ok(())
}

impl AudioDriver for CpalCaptureDriver {
    fn initialize(&mut self) -> Result<(), DriverError> {
        if self.state.lock().initialized {
            return Ok(());
        }
        let host = cpal::default_host();
        let wanted_device = std::env::var(CAPTURE_DEVICE_ENV)
            .ok()
            .filter(|name| !name.trim().is_empty());

        let device = match wanted_device {
            Some(wanted) => {
                let needle = wanted.to_ascii_lowercase();
                let found = host.input_devices().ok().and_then(|devices| {
                    devices
                        .filter_map(|device| {
                            device
                                .description()
                                .ok()
                                .map(|description| (device, description.name().to_string()))
                        })
                        .find(|(_, name)| name.to_ascii_lowercase().contains(&needle))
                        .map(|(device, _)| device)
                });
                match found {
                    Some(device) => device,
                    None => {
                        return Err(DriverError::invalid_config(
                            "capture_device",
                            format!("capture device '{wanted}' not found among input devices"),
                        ));
                    }
                }
            }
            None => match host.default_input_device() {
                Some(device) => device,
                None => {
                    log::warn!("[CpalCapture] No default input device; capture inactive");
                    self.state.lock().initialized = true;
                    return Ok(());
                }
            },
        };

        let device_name = device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        let supported: Vec<cpal::SupportedStreamConfigRange> = match device
            .supported_input_configs()
        {
            Ok(configs) => configs.collect(),
            Err(error) => {
                log::warn!("[CpalCapture] Cannot query input configs for '{device_name}': {error}");
                self.state.lock().initialized = true;
                return Ok(());
            }
        };
        if supported.is_empty() {
            log::warn!("[CpalCapture] Device '{device_name}' reports no input configs");
            self.state.lock().initialized = true;
            return Ok(());
        }

        let (wanted_rate, wanted_channels, wanted_frames) = {
            let state = self.state.lock();
            (
                state.wanted_rate,
                state.wanted_channels,
                state.wanted_frames,
            )
        };

        // Prefer ranges with an exact channel match, then any range, so a
        // stereo want does not land on an 8-channel config while a stereo
        // one exists.
        let mut ordered: Vec<&cpal::SupportedStreamConfigRange> = supported.iter().collect();
        ordered.sort_by_key(|range| {
            if wanted_channels == 0 || range.channels() as u32 == wanted_channels {
                0u8
            } else {
                1u8
            }
        });
        // Prefer a range covering the wanted rate; otherwise fall back to
        // the device default config, then to the first range at its max.
        let mut negotiated = ordered
            .iter()
            .filter_map(|range| {
                if wanted_rate == 0 {
                    None
                } else {
                    range
                        .try_with_sample_rate(wanted_rate)
                        .map(|config| (*range, config))
                }
            })
            .next();
        if negotiated.is_none() {
            if let Ok(default) = device.default_input_config() {
                negotiated = ordered
                    .iter()
                    .find(|range| {
                        range.channels() == default.channels()
                            && range.sample_format() == default.sample_format()
                    })
                    .map(|range| (*range, default));
            }
            if negotiated.is_none() {
                let range = ordered[0];
                negotiated = Some((range, range.with_max_sample_rate()));
            }
        }
        let Some((range, supported_config)) = negotiated else {
            // Unreachable: `supported` is non-empty and every fallback above
            // produces a candidate. Degrade to inactive instead of panicking.
            log::warn!("[CpalCapture] No usable input config for '{device_name}'");
            self.state.lock().initialized = true;
            return Ok(());
        };

        // A config range carries a fixed channel count: requesting fewer
        // channels than the range offers is not a valid stream config, so an
        // unset want captures the device-native layout (bounded for safety)
        // and an explicit want clamps down to what the range supports.
        let range_channels = range.channels() as u32;
        let actual_channels = if wanted_channels == 0 {
            range_channels.clamp(1, MAX_CHANNELS)
        } else {
            wanted_channels.min(range_channels).max(1)
        };
        let stream_config = StreamConfig {
            channels: actual_channels as cpal::ChannelCount,
            sample_rate: supported_config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        // The built config carries the honored rate (wanted, device default,
        // or range maximum) — report exactly what the stream runs at.
        let actual_rate = stream_config.sample_rate;
        let actual_frames = if wanted_frames == 0 {
            DEFAULT_BUFFER_FRAMES
        } else {
            wanted_frames
        };

        self.refresh_capacity(actual_frames, actual_channels);
        match build_stream(
            &device,
            &stream_config,
            supported_config.sample_format(),
            Arc::clone(&self.queue),
            Arc::clone(&self.queue_capacity_samples),
            Arc::clone(&self.overflow_drops),
        ) {
            Ok(stream) => {
                if let Err(error) = stream.play() {
                    log::warn!("[CpalCapture] Cannot start '{device_name}': {error}");
                    self.state.lock().initialized = true;
                    return Ok(());
                }
                // The stream must be owned or capture stops when this arm
                // returns: dropping it here silently degrades every read to
                // zeros (`stream.is_none()` gates `capture_active`).
                self.stream = Some(stream);
                {
                    let mut state = self.state.lock();
                    state.initialized = true;
                    state.installed = true;
                    state.actual_rate = actual_rate;
                    state.actual_channels = actual_channels;
                    state.actual_frames = actual_frames;
                }
                self.queue.lock().clear();
                log::info!(
                    "[CpalCapture] Capturing from '{device_name}': {actual_rate}Hz, {actual_channels}ch"
                );
                Ok(())
            }
            Err(error) => {
                log::warn!("[CpalCapture] Cannot open '{device_name}': {error}");
                self.state.lock().initialized = true;
                Ok(())
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
        }
        let mut state = self.state.lock();
        state.initialized = false;
        state.installed = false;
        self.queue.lock().clear();
    }

    fn status(&self) -> DriverStatus {
        let state = self.state.lock();
        let capture_active = state.installed && state.engine_ready && self.stream.is_some();
        DriverStatus::new(
            true,
            state.installed,
            capture_active,
            state.actual_rate,
            state.actual_channels,
            state.actual_frames,
            DRIVER_NAME,
            state.installed,
        )
    }

    fn read_audio(&mut self, buffer: &mut [f32]) -> usize {
        if !self.capture_active() {
            buffer.fill(0.0);
            return 0;
        }
        let channels = self.state.lock().actual_channels as usize;
        drain_frames(&self.queue, channels, buffer)
    }

    fn available_frames(&self) -> usize {
        let channels = self.state.lock().actual_channels.max(1) as usize;
        self.queue.lock().len() / channels
    }

    fn sample_rate(&self) -> u32 {
        self.state.lock().actual_rate
    }

    fn channel_count(&self) -> u32 {
        self.state.lock().actual_channels
    }

    fn request_config(&mut self, config: DriverConfig) -> ConfigResult {
        if let Err(error) = validate_config(config) {
            return ConfigResult::Error(error);
        }
        let (rate, channels, frames) = {
            let mut state = self.state.lock();
            if config.sample_rate != 0 {
                state.wanted_rate = config.sample_rate;
            }
            if config.channel_count != 0 {
                state.wanted_channels = config.channel_count;
            }
            if config.buffer_frames != 0 {
                state.wanted_frames = config.buffer_frames;
                self.refresh_capacity(state.wanted_frames, state.actual_channels.max(1));
            }
            (
                state.wanted_rate,
                state.wanted_channels,
                state.wanted_frames,
            )
        };

        // Without a live stream there is nothing to renegotiate yet; the
        // wants apply at the next `initialize`.
        if self.stream.is_none() {
            return ConfigResult::Accepted;
        }

        // Rebuild the stream so the running capture honors the request.
        self.stream = None;
        self.queue.lock().clear();
        // Temporarily clear `initialized` so `initialize` rebuilds.
        self.state.lock().initialized = false;
        self.state.lock().installed = false;
        if let Err(error) = self.initialize() {
            return ConfigResult::Error(error);
        }
        let state = self.state.lock();
        let negotiated_rate = if rate != 0 { rate } else { state.actual_rate };
        let negotiated_channels = if channels != 0 {
            channels
        } else {
            state.actual_channels
        };
        let negotiated_frames = if frames != 0 {
            frames
        } else {
            state.actual_frames
        };
        if state.actual_rate == negotiated_rate
            && state.actual_channels == negotiated_channels
            && state.actual_frames == negotiated_frames
        {
            ConfigResult::Accepted
        } else {
            ConfigResult::negotiated(
                state.actual_rate,
                state.actual_frames,
                state.actual_channels,
            )
        }
    }

    fn poll_config_change(&mut self) -> Option<DriverConfig> {
        None
    }

    fn acknowledge_config_change(&mut self, _actual: DriverConfig, _result: ConfigResult) {}

    fn set_engine_ready(&mut self, ready: bool) {
        self.state.lock().engine_ready = ready;
    }
}

impl Drop for CpalCaptureDriver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_config_with_field_names() {
        let mut driver = CpalCaptureDriver::new();
        let rate = driver.request_config(DriverConfig::with_sample_rate(7_999));
        assert!(matches!(
            rate,
            ConfigResult::Error(DriverError::InvalidConfig { .. })
        ));
        let channels = driver.request_config(DriverConfig::with_channel_count(33));
        assert!(matches!(
            channels,
            ConfigResult::Error(DriverError::InvalidConfig { .. })
        ));
        let frames = driver.request_config(DriverConfig::with_buffer_frames(63));
        assert!(matches!(
            frames,
            ConfigResult::Error(DriverError::InvalidConfig { .. })
        ));
        // Zeros keep current values and are always accepted.
        assert_eq!(
            driver.request_config(DriverConfig::keep_current()),
            ConfigResult::Accepted
        );
    }

    #[test]
    fn drain_writes_complete_frames_and_preserves_tail() {
        let queue = Mutex::new(VecDeque::from([1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]));
        let mut buffer = vec![f32::NAN; 7];
        // 7 queued samples at 2 channels: 3 frames written, tail untouched.
        assert_eq!(drain_frames(&queue, 2, &mut buffer), 3);
        assert_eq!(buffer[..6], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert!(buffer[6].is_nan(), "partial-frame tail must stay untouched");
        assert_eq!(queue.lock().len(), 1);
    }

    #[test]
    fn drain_reports_underrun_without_touching_buffer() {
        let queue = Mutex::new(VecDeque::from([1.0f32]));
        let mut buffer = vec![9.0; 4];
        assert_eq!(drain_frames(&queue, 2, &mut buffer), 0);
        assert_eq!(buffer, vec![9.0; 4]);
        assert_eq!(queue.lock().len(), 1);
    }

    #[test]
    fn inactive_driver_silences_output() {
        // No stream can exist without hardware; reads degrade to silence.
        let mut driver = CpalCaptureDriver::new();
        driver.push_test_samples(&[0.5, 0.5]);
        let mut buffer = vec![f32::NAN; 2];
        assert_eq!(driver.read_audio(&mut buffer), 0);
        assert_eq!(buffer, vec![0.0; 2]);
    }

    #[test]
    fn overflow_drops_oldest_and_counts() {
        let driver = CpalCaptureDriver::new();
        driver.state.lock().actual_channels = 1;
        driver.refresh_capacity(64, 1);
        let samples: Vec<f32> = (0..600).map(|index| index as f32).collect();
        driver.push_test_samples(&samples);
        // Capacity is 64 * 1 * 8 = 512; the first 88 samples were dropped.
        assert_eq!(driver.overflow_count(), 88);
        assert_eq!(driver.available_frames(), 512);
        let queue = driver.queue.lock();
        assert_eq!(queue[0], 88.0);
        assert_eq!(queue[511], 599.0);
    }

    #[test]
    fn respects_engine_ready_gate() {
        let mut driver = CpalCaptureDriver::new();
        {
            let mut state = driver.state.lock();
            state.installed = true;
            state.engine_ready = false;
            state.actual_channels = 1;
        }
        driver.push_test_samples(&[0.5, 0.5]);
        let mut buffer = vec![9.0; 2];
        assert_eq!(driver.read_audio(&mut buffer), 0);
        // Inactive drivers report silence.
        assert_eq!(buffer, vec![0.0; 2]);
    }

    #[test]
    fn installed_capture_always_holds_live_stream() {
        // Invariant: `installed` must imply an owned input stream. If the
        // built stream is ever dropped instead of stored, capture silently
        // degrades to zeros while reporting itself installed. Headless CI
        // exercises the degrade arm (`installed == false`); hardware hosts
        // exercise the live arm.
        let mut driver = CpalCaptureDriver::new();
        let _ = driver.initialize();
        let state = driver.state.lock();
        assert_eq!(
            driver.stream.is_some(),
            state.installed,
            "installed capture must own its input stream"
        );
    }

    #[test]
    fn conforms_to_audio_driver_contract_without_hardware() {
        // Headless CI has no input device; initialize degrades to
        // installed=false and the trait contract must still hold.
        driver_common::test_support::assert_audio_driver_contract(CpalCaptureDriver::new())
            .expect("CpalCaptureDriver contract");
    }
}
