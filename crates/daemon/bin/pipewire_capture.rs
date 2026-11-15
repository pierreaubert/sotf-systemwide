//! Native PipeWire system-audio capture driver (Linux).
//!
//! Systemwide needs the *system mix*, i.e. a sink's monitor ports.
//! Through ALSA those are unreachable (the default input is a
//! microphone), so on PipeWire desktops this driver captures the
//! SotF Virtual sink's monitor directly: an input stream targeted at
//! the sink node with `stream.capture-sink=true`.
//!
//! Shape mirrors `cpal_capture`: a realtime callback pushes f32
//! samples into the same bounded drop-oldest queue, the control
//! thread owns lifecycle, and `CaptureSource` exposes the queue to
//! the engine decoder. If PipeWire or the virtual sink is missing,
//! the driver stays *inactive* (initialized, not installed) instead
//! of capturing a microphone by mistake; the cpal driver remains
//! available via `SOTF_SYSTEMWIDE_DRIVER=cpal` for explicit setups.

use crate::cpal_capture::{
    CAPTURE_DEVICE_ENV, CaptureQueue, drain_frames, push_samples, validate_config,
};
use driver_common::{
    AudioDriver, CaptureInput, CaptureSource, ConfigResult, DriverConfig, DriverError, DriverStatus,
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

/// Node name of the installer-provisioned virtual sink (see
/// builds/linux/10-sotf-virtual-sink.conf). Keep in sync.
pub(crate) const SOTF_VIRTUAL_SINK_NAME: &str = "sotf-virtual";
/// Its human-readable description, as shown in sound settings.
pub(crate) const SOTF_VIRTUAL_SINK_DESCRIPTION: &str = "SotF Virtual";

const DRIVER_NAME: &str = "PipeWire monitor (native)";
const DEFAULT_BUFFER_FRAMES: u32 = 512;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// SPA audio channel ids in ALSA's fixed order (FL FR RL RR FC LFE
/// SL SR), the same contract the ALSA measurement path uses, so a
/// stream index means the same position on both backends.
const ALSA_SPA_POSITIONS: [u32; 8] = [3, 4, 12, 13, 5, 6, 7, 8];

#[derive(Debug)]
struct PwCaptureState {
    initialized: bool,
    installed: bool,
    engine_ready: bool,
    wanted_rate: u32,
    wanted_channels: u32,
    wanted_frames: u32,
    actual_rate: u32,
    actual_channels: u32,
    actual_frames: u32,
}

impl Default for PwCaptureState {
    fn default() -> Self {
        Self {
            initialized: false,
            installed: false,
            engine_ready: false,
            wanted_rate: 0,
            wanted_channels: 0,
            wanted_frames: DEFAULT_BUFFER_FRAMES,
            actual_rate: 48_000,
            actual_channels: 2,
            actual_frames: DEFAULT_BUFFER_FRAMES,
        }
    }
}

#[derive(Debug)]
struct PwCaptureReader {
    state: Arc<Mutex<PwCaptureState>>,
    queue: Arc<CaptureQueue>,
    failed: Arc<AtomicBool>,
}

impl CaptureInput for PwCaptureReader {
    fn configuration(&self) -> DriverConfig {
        let state = self.state.lock();
        DriverConfig::new(
            state.actual_rate,
            state.actual_frames,
            state.actual_channels,
        )
    }

    fn available_frames(&self) -> usize {
        let state = self.state.lock();
        if !state.installed || self.failed.load(Ordering::Acquire) {
            return 0;
        }
        self.queue.lock().len() / state.actual_channels.max(1) as usize
    }

    fn read_frames(&self, buffer: &mut [f32], expected: DriverConfig) -> usize {
        let state = self.state.lock();
        let actual = DriverConfig::new(
            state.actual_rate,
            state.actual_frames,
            state.actual_channels,
        );
        if !state.installed || self.failed.load(Ordering::Acquire) || actual != expected {
            return 0;
        }
        drain_frames(&self.queue, state.actual_channels as usize, buffer)
    }
}

enum ThreadCommand {
    Quit,
}

struct Running {
    join: Option<std::thread::JoinHandle<()>>,
    quit: pipewire::channel::Sender<ThreadCommand>,
}

pub struct PipeWireCaptureDriver {
    state: Arc<Mutex<PwCaptureState>>,
    queue: Arc<CaptureQueue>,
    queue_capacity_samples: Arc<AtomicUsize>,
    overflow_drops: Arc<AtomicU64>,
    stream_failed: Arc<AtomicBool>,
    running: Option<Running>,
    next_reconnect: Instant,
}

impl std::fmt::Debug for PipeWireCaptureDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("PipeWireCaptureDriver")
            .field("installed", &state.installed)
            .field("actual_rate", &state.actual_rate)
            .field("actual_channels", &state.actual_channels)
            .field("running", &self.running.is_some())
            .finish_non_exhaustive()
    }
}

impl PipeWireCaptureDriver {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(PwCaptureState::default())),
            queue: Arc::new(CaptureQueue::new(
                std::collections::VecDeque::with_capacity(DEFAULT_BUFFER_FRAMES as usize * 2 * 8),
            )),
            queue_capacity_samples: Arc::new(AtomicUsize::new(
                DEFAULT_BUFFER_FRAMES as usize * 2 * 8,
            )),
            overflow_drops: Arc::new(AtomicU64::new(0)),
            stream_failed: Arc::new(AtomicBool::new(false)),
            running: None,
            next_reconnect: Instant::now(),
        }
    }

    fn refresh_capacity(&self, frames: u32, channels: u32) {
        let capacity = (frames as usize)
            .saturating_mul(channels.max(1) as usize)
            .saturating_mul(8)
            .max(1);
        let mut queue = self.queue.lock();
        queue.clear();
        let additional = capacity.saturating_sub(queue.len());
        queue.reserve(additional);
        self.queue_capacity_samples
            .store(capacity, Ordering::Relaxed);
    }

    fn capture_active(&self) -> bool {
        let state = self.state.lock();
        state.installed
            && state.engine_ready
            && self.running.is_some()
            && !self.stream_failed.load(Ordering::Acquire)
    }

    fn stop_thread(&mut self) {
        if let Some(mut running) = self.running.take() {
            let _ = running.quit.send(ThreadCommand::Quit);
            if let Some(join) = running.join.take() {
                let _ = join.join();
            }
        }
    }
}

struct StreamData {
    queue: Arc<CaptureQueue>,
    capacity: Arc<AtomicUsize>,
    drops: Arc<AtomicU64>,
    channels: usize,
    process_calls: u64,
}

/// Resolve the capture sink: explicit `SOTF_CAPTURE_DEVICE` match by
/// node.name/description first, then the SotF Virtual sink.
fn resolve_sink_id(
    core: &pipewire::core::Core,
    mainloop: &pipewire::main_loop::MainLoop,
    explicit: Option<&str>,
) -> Option<u32> {
    let registry = core.get_registry().ok()?;
    let found: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let found_clone = Arc::clone(&found);
    let explicit_owned = explicit.map(str::to_string);
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pipewire::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props else { return };
            if !props.get("media.class").unwrap_or("").contains("Sink") {
                return;
            }
            let name = props.get("node.name").unwrap_or("");
            let description = props.get("node.description").unwrap_or("");
            let matches = match explicit_owned.as_deref() {
                Some(wanted) => name == wanted || description == wanted,
                None => {
                    name == SOTF_VIRTUAL_SINK_NAME || description == SOTF_VIRTUAL_SINK_DESCRIPTION
                }
            };
            if matches {
                *found_clone.lock() = Some(global.id);
            }
        })
        .register();
    let pending = core.sync(0).ok()?;
    let synced = Arc::new(AtomicBool::new(false));
    let synced_clone = Arc::clone(&synced);
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pipewire::core::PW_ID_CORE && seq == pending {
                synced_clone.store(true, Ordering::Release);
            }
        })
        .register();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !synced.load(Ordering::Acquire) && Instant::now() < deadline {
        mainloop.loop_().iterate(Duration::from_millis(100));
    }
    *found.lock()
}

#[allow(clippy::too_many_arguments)]
fn run_capture_thread(
    rate: u32,
    channels: u32,
    frames: u32,
    explicit_device: Option<String>,
    state: Arc<Mutex<PwCaptureState>>,
    queue: Arc<CaptureQueue>,
    capacity: Arc<AtomicUsize>,
    drops: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    ready: SyncSender<Result<(), String>>,
    quit_rx: pipewire::channel::Receiver<ThreadCommand>,
) {
    let fail = |ready: &SyncSender<Result<(), String>>, message: String| {
        let _ = ready.send(Err(message));
    };
    pipewire::init();
    let mainloop = match pipewire::main_loop::MainLoop::new(None) {
        Ok(mainloop) => mainloop,
        Err(error) => return fail(&ready, format!("PipeWire main loop: {error}")),
    };
    let context = match pipewire::context::Context::new(&mainloop) {
        Ok(context) => context,
        Err(error) => return fail(&ready, format!("PipeWire context: {error}")),
    };
    let core = match context.connect(None) {
        Ok(core) => core,
        Err(error) => {
            return fail(
                &ready,
                format!("PipeWire connect failed (is pipewire running?): {error}"),
            );
        }
    };

    let Some(sink_id) = resolve_sink_id(&core, &mainloop, explicit_device.as_deref()) else {
        return fail(
            &ready,
            "no system-audio sink found: the SotF Virtual sink is missing \
             (installer adds it as a PipeWire drop-in; restart PipeWire \
             after installing), and no SOTF_CAPTURE_DEVICE sink matches"
                .to_string(),
        );
    };

    let stream = match pipewire::stream::Stream::new(
        &core,
        "sotf-systemwide-capture",
        pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::NODE_DESCRIPTION => "SotF Systemwide Capture",
            *pipewire::keys::STREAM_CAPTURE_SINK => "true",
        },
    ) {
        Ok(stream) => stream,
        Err(error) => return fail(&ready, format!("PipeWire stream create: {error}")),
    };

    let data = StreamData {
        queue: Arc::clone(&queue),
        capacity: Arc::clone(&capacity),
        drops: Arc::clone(&drops),
        channels: channels as usize,
        process_calls: 0,
    };
    let failed_clone = Arc::clone(&failed);
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, _old, new| {
            if matches!(new, pipewire::stream::StreamState::Error(_)) {
                failed_clone.store(true, Ordering::Release);
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let chunk = datas[0].chunk();
            let offset = chunk.offset() as usize;
            let size = chunk.size() as usize;
            data.process_calls += 1;
            if data.process_calls <= 3 {
                log::debug!(
                    "[PipeWireCapture] process #{}: chunk offset={offset} size={size}",
                    data.process_calls
                );
            }
            let Some(slice) = datas[0].data() else {
                return;
            };
            let end = (offset + size).min(slice.len());
            if offset >= end {
                return;
            }
            let samples: Vec<f32> = slice[offset..end]
                .chunks_exact(4)
                .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .collect();
            push_samples(
                &data.queue,
                &data.capacity,
                &data.drops,
                data.channels,
                samples.into_iter(),
            );
        })
        .register();
    let _listener = match listener {
        Ok(listener) => listener,
        Err(error) => return fail(&ready, format!("PipeWire stream listener: {error}")),
    };

    let mut position = [0_u32; 64];
    for (slot, spa_channel) in position
        .iter_mut()
        .zip(ALSA_SPA_POSITIONS.iter().cycle())
        .take(channels as usize)
    {
        *slot = *spa_channel;
    }
    let mut audio_info = libspa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(libspa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(rate);
    audio_info.set_channels(channels);
    audio_info.set_position(position);
    let _ = frames;

    let values: Vec<u8> = match libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &libspa::pod::Value::Object(libspa::pod::Object {
            type_: libspa_sys::SPA_TYPE_OBJECT_Format,
            id: libspa_sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    ) {
        Ok((cursor, _)) => cursor.into_inner(),
        Err(error) => return fail(&ready, format!("PipeWire format pod: {error}")),
    };
    let mut params =
        [libspa::pod::Pod::from_bytes(&values).expect("serialized pod bytes must parse back")];

    if let Err(error) = stream.connect(
        libspa::utils::Direction::Input,
        Some(sink_id),
        pipewire::stream::StreamFlags::AUTOCONNECT
            | pipewire::stream::StreamFlags::MAP_BUFFERS
            | pipewire::stream::StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        return fail(&ready, format!("PipeWire capture connect: {error}"));
    }

    {
        let mut state = state.lock();
        state.actual_rate = rate;
        state.actual_channels = channels;
        state.actual_frames = frames;
        state.installed = true;
    }
    log::info!(
        "[PipeWireCapture] Capturing system audio from sink node {sink_id}: {rate}Hz, {channels}ch"
    );
    let _ = ready.send(Ok(()));

    let mainloop_quit = mainloop.clone();
    let _quit_receiver = quit_rx.attach(mainloop.loop_(), move |_command| {
        mainloop_quit.quit();
    });
    mainloop.run();
}

impl AudioDriver for PipeWireCaptureDriver {
    fn initialize(&mut self) -> Result<(), DriverError> {
        if self.state.lock().initialized {
            return Ok(());
        }
        let (rate, channels, frames) = {
            let state = self.state.lock();
            (
                if state.wanted_rate == 0 {
                    48_000
                } else {
                    state.wanted_rate
                },
                if state.wanted_channels == 0 {
                    2
                } else {
                    state.wanted_channels
                },
                state.wanted_frames,
            )
        };
        self.refresh_capacity(frames, channels);
        self.stream_failed.store(false, Ordering::Release);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<ThreadCommand>();
        let explicit_device = std::env::var(CAPTURE_DEVICE_ENV)
            .ok()
            .filter(|name| !name.trim().is_empty());
        let thread_state = Arc::clone(&self.state);
        let thread_queue = Arc::clone(&self.queue);
        let thread_capacity = Arc::clone(&self.queue_capacity_samples);
        let thread_drops = Arc::clone(&self.overflow_drops);
        let thread_failed = Arc::clone(&self.stream_failed);
        let join = std::thread::Builder::new()
            .name("sotf-pipewire-capture".to_string())
            .spawn(move || {
                run_capture_thread(
                    rate,
                    channels,
                    frames,
                    explicit_device,
                    thread_state,
                    thread_queue,
                    thread_capacity,
                    thread_drops,
                    thread_failed,
                    ready_tx,
                    quit_rx,
                );
            })
            .map_err(|e| {
                DriverError::not_available(format!("pipewire capture thread spawn: {e}"))
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    join: Some(join),
                    quit: quit_tx,
                });
                self.state.lock().initialized = true;
                Ok(())
            }
            Ok(Err(message)) => {
                let _ = join.join();
                log::warn!("[PipeWireCapture] Capture inactive: {message}");
                self.state.lock().initialized = true;
                Ok(())
            }
            Err(_) => {
                let _ = quit_tx.send(ThreadCommand::Quit);
                let _ = join.join();
                log::warn!("[PipeWireCapture] Capture inactive: stream not ready within 10s");
                self.state.lock().initialized = true;
                Ok(())
            }
        }
    }

    fn shutdown(&mut self) {
        self.stop_thread();
        let mut state = self.state.lock();
        state.initialized = false;
        state.installed = false;
        self.queue.lock().clear();
    }

    fn status(&self) -> DriverStatus {
        let state = self.state.lock();
        let healthy = state.installed && !self.stream_failed.load(Ordering::Acquire);
        let capture_active = healthy && state.engine_ready && self.running.is_some();
        DriverStatus::new(
            true,
            healthy,
            capture_active,
            state.actual_rate,
            state.actual_channels,
            state.actual_frames,
            DRIVER_NAME,
            healthy,
            0,
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

    fn capture_source(&self) -> Option<CaptureSource> {
        Some(CaptureSource::new(PwCaptureReader {
            state: Arc::clone(&self.state),
            queue: Arc::clone(&self.queue),
            failed: Arc::clone(&self.stream_failed),
        }))
    }

    fn available_frames(&self) -> usize {
        if self.stream_failed.load(Ordering::Acquire) {
            return 0;
        }
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
        let changed = {
            let mut state = self.state.lock();
            let mut changed = false;
            if config.sample_rate != 0 && config.sample_rate != state.wanted_rate {
                state.wanted_rate = config.sample_rate;
                changed = true;
            }
            if config.channel_count != 0 && config.channel_count != state.wanted_channels {
                state.wanted_channels = config.channel_count;
                changed = true;
            }
            if config.buffer_frames != 0 {
                state.wanted_frames = config.buffer_frames;
                self.refresh_capacity(state.wanted_frames, state.actual_channels.max(1));
            }
            changed
        };
        if changed && self.running.is_some() {
            // Force the poll-driven reconnect to apply the new format.
            self.stream_failed.store(true, Ordering::Release);
        }
        ConfigResult::Accepted
    }

    fn poll_config_change(&mut self) -> Option<DriverConfig> {
        let now = Instant::now();
        let needs_reconnect = self.state.lock().initialized
            && (self.running.is_none() || self.stream_failed.load(Ordering::Acquire));
        if needs_reconnect && now >= self.next_reconnect {
            self.next_reconnect = now + RECONNECT_INTERVAL;
            self.stop_thread();
            {
                let mut state = self.state.lock();
                state.initialized = false;
                state.installed = false;
            }
            if let Err(error) = self.initialize() {
                self.state.lock().initialized = true;
                log::warn!("[PipeWireCapture] Reconnect pending: {error}");
            }
            if self.running.is_some() && !self.stream_failed.load(Ordering::Acquire) {
                let state = self.state.lock();
                return Some(DriverConfig::new(
                    state.actual_rate,
                    state.actual_frames,
                    state.actual_channels,
                ));
            }
        }
        None
    }

    fn acknowledge_config_change(&mut self, _actual: DriverConfig, _result: ConfigResult) {}

    fn set_engine_ready(&mut self, ready: bool) {
        self.state.lock().engine_ready = ready;
    }
}

impl Drop for PipeWireCaptureDriver {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driver_common::AudioDriver;

    /// Manual live test: requires the SotF Virtual sink to exist
    /// (installer drop-in + PipeWire restart) and something playing
    /// into it, e.g. `pw-play --target sotf-virtual tone.wav`.
    /// Asserts monitor samples actually reach the capture queue.
    #[test]
    #[ignore = "manual: needs PipeWire session with the SotF Virtual sink"]
    fn captures_sink_monitor_samples() {
        let _ = env_logger::try_init();
        let mut driver = PipeWireCaptureDriver::new();
        driver.initialize().expect("initialize");
        let status = driver.status();
        assert!(status.driver_installed, "driver must find the virtual sink");
        driver.set_engine_ready(true);

        // Give the graph a moment, then require non-silent samples.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut peak = 0.0_f32;
        let mut buffer = vec![0.0_f32; 4096];
        while Instant::now() < deadline {
            let frames = driver.read_audio(&mut buffer);
            if frames > 0 {
                peak = peak.max(
                    buffer[..frames * 2]
                        .iter()
                        .fold(0.0_f32, |acc, sample| acc.max(sample.abs())),
                );
                if peak > 0.05 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        driver.shutdown();
        assert!(peak > 0.05, "no audible samples captured (peak {peak})");
    }
}
