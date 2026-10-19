use driver_common::DriverStatus;
use serde_json::Value;
use sotf_audio::PluginConfig;
use sotf_audio::SinkType;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

/// Process-lifetime locks serialize ownership of every runtime resource that
/// can affect the HAL transport.
///
/// The control socket is only one resource. Shared memory and both session-key
/// copies can be overridden independently, so locking beside the socket alone
/// allows a daemon with a different socket to rotate another daemon's key.
/// Canonical lock paths and sorted acquisition also prevent alias-based bypasses
/// and overlapping-resource deadlocks.
#[derive(Debug)]
pub(super) struct DaemonInstanceLock {
    _files: Vec<std::fs::File>,
}

pub(super) fn acquire_daemon_instance_lock(
    resource_paths: &[std::path::PathBuf],
) -> std::io::Result<DaemonInstanceLock> {
    if resource_paths.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon ownership requires at least one runtime resource",
        ));
    }

    let mut lock_paths = Vec::with_capacity(resource_paths.len());
    for resource_path in resource_paths {
        let parent = resource_path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "runtime resource {} has no parent directory",
                    resource_path.display()
                ),
            )
        })?;
        let file_name = resource_path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "runtime resource {} has no file name",
                    resource_path.display()
                ),
            )
        })?;
        let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to resolve runtime resource parent {}: {error}",
                    parent.display()
                ),
            )
        })?;
        let mut lock_name = file_name.to_os_string();
        lock_name.push(".owner.lock");
        lock_paths.push(canonical_parent.join(lock_name));
    }
    lock_paths.sort();
    lock_paths.dedup();

    let mut files = Vec::with_capacity(lock_paths.len());
    for lock_path in lock_paths {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .mode(0o600);
        }

        let file = options.open(&lock_path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to open ownership lock {}: {error}",
                    lock_path.display()
                ),
            )
        })?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "ownership lock {} is not a regular file",
                    lock_path.display()
                ),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }

        file.try_lock().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "another sotf-daemon instance owns runtime resource lock {}: {}",
                    lock_path.display(),
                    error
                ),
            )
        })?;
        files.push(file);
    }

    Ok(DaemonInstanceLock { _files: files })
}

pub(super) fn env_path_is_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|value| !value.as_os_str().is_empty())
}

pub(super) fn transport_snapshot_and_faults(
    state_name: &str,
    driver_status: &DriverStatus,
    engine_state: &sotf_audio::AudioEngineState,
) -> (Value, Vec<Value>) {
    let playing = state_name == "Playing";
    let driver_unavailable =
        playing && (!driver_status.driver_installed || !driver_status.driver_ready);
    let hal_stream_inactive = playing && !driver_unavailable && !driver_status.capture_active;
    let input_frames_missing =
        playing && !driver_unavailable && engine_state.playback_frames_received == 0;
    let output_callbacks_missing = playing && engine_state.playback_callback_count == 0;
    let output_frames_missing = playing
        && engine_state.playback_frames_received > 0
        && engine_state.playback_frames_written == 0;
    let output_device_unresolved = playing && engine_state.playback_output_device.is_none();

    let input_status = if !playing {
        "idle"
    } else if driver_unavailable {
        "driver_unavailable"
    } else if hal_stream_inactive {
        "hal_stream_inactive"
    } else if input_frames_missing {
        "input_frames_missing"
    } else {
        "flowing"
    };

    let output_status = if !playing {
        "idle"
    } else if output_callbacks_missing {
        "output_callbacks_missing"
    } else if output_frames_missing {
        "output_frames_missing"
    } else if output_device_unresolved {
        "output_device_unresolved"
    } else {
        "flowing"
    };

    let mut faults = Vec::new();
    if playing {
        if driver_unavailable {
            faults.push(serde_json::json!({
                "code": "driver_unavailable",
                "severity": "error",
                "message": "Playback is marked Playing but the HAL driver is not ready.",
            }));
        }
        if hal_stream_inactive {
            faults.push(serde_json::json!({
                "code": "hal_stream_inactive",
                "severity": "warning",
                "message": "Playback is marked Playing but the HAL stream is not active.",
            }));
        }
        if input_frames_missing {
            faults.push(serde_json::json!({
                "code": "input_frames_missing",
                "severity": "error",
                "message": "Playback is marked Playing but no frames have reached the playback thread.",
            }));
        }

        if output_callbacks_missing {
            faults.push(serde_json::json!({
                "code": "output_callbacks_missing",
                "severity": "error",
                "message": "Playback is marked Playing but no hardware output callbacks have been observed.",
            }));
        }
        if output_frames_missing {
            faults.push(serde_json::json!({
                "code": "output_frames_missing",
                "severity": "error",
                "message": "The playback thread received frames but has not written any to the hardware ring.",
            }));
        }
        if output_device_unresolved {
            faults.push(serde_json::json!({
                "code": "output_device_unresolved",
                "severity": "warning",
                "message": "Playback is active but no hardware output device has been resolved yet.",
            }));
        }
    }

    (
        serde_json::json!({
            "input_status": input_status,
            "output_status": output_status,
            "input": {
                "status": input_status,
                "frames_received": engine_state.playback_frames_received,
                "hal_capture_active": driver_status.capture_active,
                "driver_installed": driver_status.driver_installed,
                "driver_ready": driver_status.driver_ready,
            },
            "output": {
                "status": output_status,
                "callbacks": engine_state.playback_callback_count,
                "frames_written": engine_state.playback_frames_written,
                "frames_dropped": engine_state.playback_frames_dropped,
                "device": engine_state.playback_output_device.as_deref(),
                "effective_sample_rate": engine_state.playback_effective_sample_rate,
            },
            "input_frames_received": engine_state.playback_frames_received,
            "output_callbacks": engine_state.playback_callback_count,
            "output_frames_written": engine_state.playback_frames_written,
            "frames_dropped": engine_state.playback_frames_dropped,
            "hal_capture_active": driver_status.capture_active,
        }),
        faults,
    )
}

pub(super) fn push_metering_faults(state_name: &str, metering: &Value, faults: &mut Vec<Value>) {
    if state_name != "Playing" {
        return;
    }

    if metering["sources"]["input"]["status"].as_str() == Some("fallback_zero") {
        faults.push(serde_json::json!({
            "code": "input_metering_unavailable",
            "severity": "warning",
            "message": "Input meters are channel-shaped fallback zeros, not analyzer data.",
        }));
    }
    if metering["sources"]["output"]["status"].as_str() == Some("fallback_zero") {
        faults.push(serde_json::json!({
            "code": "output_metering_unavailable",
            "severity": "warning",
            "message": "Output meters are channel-shaped fallback zeros, not analyzer data.",
        }));
    }
}

pub(super) fn parameter_descriptor_to_json(spec: &sotf_plugins::param_specs::ParamSpec) -> Value {
    use sotf_plugins::param_specs::{ParamType, UpdateMode};

    let mut descriptor = serde_json::json!({
        "key": spec.engine_key,
        "name": spec.name,
        "unit": spec.unit,
        "group": spec.group,
        "doc": spec.doc,
        "update_mode": match spec.update_mode {
            UpdateMode::Realtime => "realtime",
            UpdateMode::Structural => "structural",
        },
    });

    let Some(object) = descriptor.as_object_mut() else {
        // The value is constructed above with json! and should always be an
        // object. Keep this helper total anyway so malformed future edits do
        // not panic an IPC handler while building plugin metadata.
        return descriptor;
    };

    match spec.param_type {
        ParamType::Float {
            default,
            min,
            max,
            step,
        } => {
            object.insert("type".to_string(), serde_json::json!("float"));
            object.insert("default".to_string(), serde_json::json!(default));
            object.insert("min".to_string(), serde_json::json!(min));
            object.insert("max".to_string(), serde_json::json!(max));
            object.insert("step".to_string(), serde_json::json!(step));
        }
        ParamType::Int {
            default,
            min,
            max,
            step,
        } => {
            object.insert("type".to_string(), serde_json::json!("int"));
            object.insert("default".to_string(), serde_json::json!(default));
            object.insert("min".to_string(), serde_json::json!(min));
            object.insert("max".to_string(), serde_json::json!(max));
            object.insert("step".to_string(), serde_json::json!(step));
        }
        ParamType::Bool {
            default,
            true_label,
            false_label,
        } => {
            object.insert("type".to_string(), serde_json::json!("bool"));
            object.insert("default".to_string(), serde_json::json!(default));
            object.insert("true_label".to_string(), serde_json::json!(true_label));
            object.insert("false_label".to_string(), serde_json::json!(false_label));
        }
        ParamType::Choice {
            default_index,
            labels,
        } => {
            object.insert("type".to_string(), serde_json::json!("choice"));
            object.insert("default".to_string(), serde_json::json!(default_index));
            object.insert("choices".to_string(), serde_json::json!(labels));
        }
        ParamType::FilePath => {
            object.insert("type".to_string(), serde_json::json!("file_path"));
        }
    }

    descriptor
}

pub(super) fn sanitize_user_plugins(plugins: Vec<PluginConfig>) -> Vec<PluginConfig> {
    plugins
        .into_iter()
        .filter(|p| {
            let pt = p.plugin_type.as_str();
            if pt == "hal_input" || pt == "hal_output" {
                log::warn!(
                    "Stripping obsolete '{}' plugin from chain - decoder thread handles driver I/O directly",
                    pt
                );
                false
            } else if pt == "loudness_monitor" {
                log::warn!("Stripping user-supplied loudness_monitor - daemon injects metering");
                false
            } else if pt == "spectrum_analyzer" || pt == "resampler" {
                // Daemon-owned analysis/resampling stages with no user data
                // path: graph building and artifact parsing strip these, so
                // direct rack loads must not insert them either.
                log::warn!(
                    "Stripping daemon-owned '{}' plugin from chain - not user-insertable",
                    pt
                );
                false
            } else {
                true
            }
        })
        .collect()
}

pub(super) fn build_driver_plugin_chain(
    plugins: Vec<PluginConfig>,
) -> (Vec<PluginConfig>, usize, usize) {
    let mut final_plugins = Vec::with_capacity(plugins.len() + 2);

    let input_monitor_index = 0;
    final_plugins.push(PluginConfig {
        plugin_type: "loudness_monitor".to_string(),
        parameters: serde_json::json!({}),
    });

    final_plugins.extend(plugins);

    let output_monitor_index = final_plugins.len();
    final_plugins.push(PluginConfig {
        plugin_type: "loudness_monitor".to_string(),
        parameters: serde_json::json!({}),
    });

    (final_plugins, input_monitor_index, output_monitor_index)
}

pub(super) fn build_driver_plugin_graph(
    mut graph: sotf_audio::engine::PluginGraphConfig,
    input_channels: usize,
    output_channels: usize,
) -> Result<(sotf_audio::engine::PluginGraphConfig, usize, usize), String> {
    use sotf_audio::engine::{PluginGraphEdgeConfig, PluginGraphNodeConfig};
    use std::collections::HashSet;

    graph
        .validate()
        .map_err(|error| format!("Invalid plugin graph: {error}"))?;
    if graph.nodes.is_empty() {
        return Err("Plugin graph must contain at least one user node".to_string());
    }
    if let Some(node) = graph.nodes.iter().find(|node| {
        matches!(
            node.plugin_type.as_str(),
            "hal_input" | "hal_output" | "loudness_monitor" | "spectrum_analyzer"
        )
    }) {
        return Err(format!(
            "Plugin graph node {} uses daemon-owned system plugin type '{}'",
            node.id, node.plugin_type
        ));
    }

    let incoming = graph
        .edges
        .iter()
        .map(|edge| edge.to_node)
        .collect::<HashSet<_>>();
    let outgoing = graph
        .edges
        .iter()
        .map(|edge| edge.from_node)
        .collect::<HashSet<_>>();
    let roots = graph
        .nodes
        .iter()
        .filter_map(|node| (!incoming.contains(&node.id)).then_some(node.id))
        .collect::<Vec<_>>();
    let leaves = graph
        .nodes
        .iter()
        .filter_map(|node| (!outgoing.contains(&node.id)).then_some(node.id))
        .collect::<Vec<_>>();
    if roots.is_empty() || leaves.is_empty() {
        return Err("Plugin graph must have at least one input and output path".to_string());
    }

    let input_monitor_id = graph
        .nodes
        .iter()
        .map(|node| node.id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "Plugin graph node id space exhausted".to_string())?;
    let output_monitor_id = input_monitor_id
        .checked_add(1)
        .ok_or_else(|| "Plugin graph node id space exhausted".to_string())?;
    let input_monitor = PluginGraphNodeConfig::try_new(
        input_monitor_id,
        "loudness_monitor",
        serde_json::json!({}),
        input_channels,
    )
    .map_err(|error| error.to_string())?;
    let output_monitor = PluginGraphNodeConfig::try_new(
        output_monitor_id,
        "loudness_monitor",
        serde_json::json!({}),
        output_channels,
    )
    .map_err(|error| error.to_string())?;

    graph.nodes.insert(0, input_monitor);
    graph.nodes.push(output_monitor);
    graph.edges.extend(
        roots
            .into_iter()
            .map(|root| PluginGraphEdgeConfig::new(input_monitor_id, root)),
    );
    graph.edges.extend(
        leaves
            .into_iter()
            .map(|leaf| PluginGraphEdgeConfig::new(leaf, output_monitor_id)),
    );
    graph
        .validate()
        .map_err(|error| format!("Invalid monitored plugin graph: {error}"))?;

    let output_monitor_index = graph.nodes.len() - 1;
    Ok((graph, 0, output_monitor_index))
}

/// Bind a `UnixListener` at `socket_path` defending against TOCTOU on
/// stale-socket cleanup.
///
/// Strategy:
/// 1. Try `bind` directly -- if it succeeds, we own a fresh socket.
/// 2. On `AddrInUse`, probe with `connect`. If something accepts, another
///    daemon is alive; bail out.
/// 3. Otherwise, `lstat` the existing entry. Only unlink it if it is
///    *actually* a Unix socket (`S_ISSOCK`). Regular files, FIFOs, and
///    symlinks (which an attacker could plant) are left alone and bind
///    fails. This means we never follow a symlink at the socket path,
///    and never unlink unrelated files.
/// 4. Retry `bind` once after a successful unlink. If a racing process
///    re-creates the entry between unlink and bind, the second bind
///    fails and we return the error to the caller (no infinite retry).
///
/// Unix-only. On Windows the daemon binds a loopback TCP listener instead;
/// see `crate::ipc_transport::bind_windows_listener`.
#[cfg(unix)]
pub(super) fn bind_unix_socket(socket_path: &std::path::Path) -> std::io::Result<UnixListener> {
    match UnixListener::bind(socket_path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Existing entry. See if it's a live daemon.
            if let Ok(_stream) = UnixStream::connect(socket_path) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "Another daemon instance is already running",
                ));
            }
            // Refuse to unlink anything that isn't an AF_UNIX socket.
            // `lstat` -- explicitly do NOT follow symlinks.
            if !socket_is_unix_socket(socket_path) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "{} exists and is not a Unix socket; refusing to remove",
                        socket_path.display()
                    ),
                ));
            }
            std::fs::remove_file(socket_path)?;
            UnixListener::bind(socket_path)
        }
        Err(e) => Err(e),
    }
}

/// Return true iff `path` is a Unix-domain socket (lstat, does NOT
/// follow symlinks).
///
/// Unix-only. On Windows the daemon uses a port file instead; see
/// `crate::ipc_transport::windows_port_file_is_live`.
#[cfg(unix)]
pub(super) fn socket_is_unix_socket(path: &std::path::Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(md) => md.file_type().is_socket(),
        Err(_) => false,
    }
}

/// Cold-start tuning for devices that enumerate late (USB re-scan after
/// reboot/reinstall, CoreAudio device-id churn). Startup retries a bounded
/// number of times while the failure looks like a device-availability race.
pub(super) const COLD_START_MAX_ATTEMPTS: u32 = 4;
/// Upper bound for waiting on one desired device to appear per attempt.
pub(super) const COLD_START_DEVICE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Poll interval while waiting for a device to enumerate.
const DEVICE_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// True when a startup failure looks like a device-availability race rather
/// than a permanent configuration error. Unknown failures are never
/// retried: only the engine's device-absence markers and two known transient
/// races qualify.
///
/// The transient races are the playback-callback timeout (device enumerated
/// but its stream not delivering callbacks yet) and the HAL config-ack
/// timeout (CoreAudio still applying a requested configuration). Both are
/// cold-start timing races that a manual output-device switch used to mask
/// by simply trying again later.
pub(super) fn startup_error_is_device_availability(error: Option<&str>) -> bool {
    let Some(message) = error else {
        return false;
    };
    let folded = message.to_ascii_lowercase();
    [
        "not available",
        "not found",
        "needs playback stream recovery",
        "stream error reported",
        "device id changed",
        "did not reach a hardware callback",
        "timed out waiting for hal to apply config request",
    ]
    .iter()
    .any(|marker| folded.contains(marker))
}

/// Block duration bounds the daemon enforces on every driver configuration.
///
/// Kept as named constants so the IPC validation, the rate-scaling rule
/// below, and the HAL negotiation clamp all agree on one range.
pub(super) const MIN_BUFFER_FRAMES: u32 = 64;
pub(super) const MAX_BUFFER_FRAMES: u32 = 4096;

/// Scale a frame count across a sample-rate change so the *block duration*
/// stays constant.
///
/// Buffer frames are meaningless without the rate: 512 frames is 10.67 ms at
/// 48 kHz but only 5.33 ms at 96 kHz. A rate-only change that keeps the
/// frame count doubles the callback and plugin invocations and halves every
/// time-based headroom, which showed up as scratchy noise at 96 kHz. Returns
/// the clamped 64..=4096 frame count for `to_rate`; `from_rate == 0` means
/// "unknown", in which case the frame count is kept unchanged.
pub(super) fn scaled_buffer_frames_for_rate_change(
    from_rate: u32,
    to_rate: u32,
    frames: u32,
) -> u32 {
    if from_rate == 0 || frames == 0 {
        return frames.clamp(MIN_BUFFER_FRAMES, MAX_BUFFER_FRAMES);
    }
    let scaled = ((f64::from(frames) * f64::from(to_rate) / f64::from(from_rate)).round()) as u32;
    scaled.clamp(MIN_BUFFER_FRAMES, MAX_BUFFER_FRAMES)
}

/// Poll device enumeration until `name` appears, the timeout expires, or
/// `keep_going` reports shutdown. Returns true only when the device was
/// observed. Enumeration errors count as "absent" and keep polling.
pub(super) fn wait_for_output_device(
    name: &str,
    timeout: std::time::Duration,
    keep_going: &dyn Fn() -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !keep_going() {
            return false;
        }
        if let Ok(devices) = list_audio_devices()
            && devices
                .iter()
                .any(|device| device.get("name").and_then(Value::as_str) == Some(name))
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(DEVICE_WAIT_POLL.min(deadline - std::time::Instant::now()));
    }
}

/// Rewrite a pipeline-start failure into a device-availability error when the
/// requested output device is not enumerated.
///
/// The engine reports device absence generically once its manager thread has
/// exited ("Engine thread exited unexpectedly"): the response channel
/// disconnects before the thread records its detailed `last_error`, so the
/// detail never reaches the caller. Without the rewrite, the cold-start retry
/// matcher cannot recognise a device race and the latched fault names no
/// device. Errors that already carry device-absence markers, empty requests,
/// and enumeration failures pass through unchanged.
pub(super) fn clarify_device_absence_error(
    desired_device: Option<&str>,
    engine_error: &str,
) -> String {
    if startup_error_is_device_availability(Some(engine_error)) {
        return engine_error.to_string();
    }
    let Some(desired) = desired_device
        .map(str::trim)
        .filter(|desired| !desired.is_empty())
    else {
        return engine_error.to_string();
    };
    let enumerated = match list_audio_devices() {
        Ok(devices) => devices,
        Err(_) => return engine_error.to_string(),
    };
    let names: Vec<&str> = enumerated
        .iter()
        .filter_map(|device| device.get("name").and_then(Value::as_str))
        .collect();
    clarify_device_absence_error_with_devices(desired, engine_error, &names)
}

/// Pure core of [`clarify_device_absence_error`] over an already-enumerated
/// device-name list, so the rewrite rules stay unit-testable without audio
/// hardware.
fn clarify_device_absence_error_with_devices(
    desired: &str,
    engine_error: &str,
    enumerated_names: &[&str],
) -> String {
    if device_is_enumerated(desired, enumerated_names) {
        return engine_error.to_string();
    }
    format!(
        "Selected output device '{desired}' is not available: Audio device '{desired}' not found. Available output devices ({} total): {}. Engine reported: {engine_error}",
        enumerated_names.len(),
        enumerated_names.join(", "),
    )
}

/// True when `name` matches an enumerated device, tolerating the ASIO prefix
/// gap between the stored name and the enumerated name.
pub(super) fn device_is_enumerated(name: &str, enumerated_names: &[&str]) -> bool {
    let stripped = sotf_audio::devices::strip_asio_prefix(name);
    enumerated_names
        .iter()
        .any(|candidate| *candidate == name || *candidate == stripped)
}

/// One enumerated output device for fallback selection.
#[derive(Clone, Debug)]
pub(super) struct FallbackCandidate {
    pub(super) name: String,
    pub(super) is_default: bool,
}

/// A substituted output route: the desired device was missing, so the engine
/// opened the fallback instead. Callers preserve the desired device so a
/// replugged device can be re-selected without losing the intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputDeviceFallback {
    pub(super) desired: String,
    pub(super) applied: String,
}

/// The only output-device name the device-free lab sink accepts besides
/// `None`; any physical name is rejected by the engine.
pub(super) const LAB_OUTPUT_DEVICE_NAME: &str = "Systemwide Lab Output";

/// Adapt a resolved output to the sink that will actually open it.
///
/// The lab sink is device-free by design, so substituting a physical
/// fallback device for a missing desired one only guarantees the engine
/// rejects the start ("Lab backend cannot open a physical output
/// device") — on machines with real audio hardware, while device-less CI
/// resolved to `None` and passed. In lab mode the effective device is
/// the lab output itself, recorded as a fallback from the desired name.
pub(super) fn effective_output_for_sink(
    effective: EffectiveOutputDevice,
    sink_type: SinkType,
    desired_device: Option<&str>,
) -> EffectiveOutputDevice {
    if sink_type != SinkType::LabNull {
        return effective;
    }
    let fallback = desired_device
        .filter(|name| *name != LAB_OUTPUT_DEVICE_NAME)
        .map(|desired| OutputDeviceFallback {
            desired: desired.to_owned(),
            applied: LAB_OUTPUT_DEVICE_NAME.to_owned(),
        });
    EffectiveOutputDevice {
        device: Some(LAB_OUTPUT_DEVICE_NAME.to_owned()),
        fallback,
    }
}

/// Engine-start device resolution: what to open, plus the fallback record
/// when a missing desired device was substituted.
pub(super) struct EffectiveOutputDevice {
    pub(super) device: Option<String>,
    pub(super) fallback: Option<OutputDeviceFallback>,
}

/// Pick the fallback output: the system default when it is a safe physical
/// device, otherwise the first safe enumerated device. Virtual/loopback
/// devices never qualify: processed audio must not be routed back into the
/// capture device.
pub(super) fn select_fallback_output_device(candidates: &[FallbackCandidate]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| candidate.is_default && is_safe_output_device_name(&candidate.name))
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| is_safe_output_device_name(&candidate.name))
        })
        .map(|candidate| candidate.name.clone())
}

/// Resolve the device an engine start should open. A missing desired device
/// degrades to the default-safe fallback instead of failing the start; the
/// fallback record lets the caller preserve the desired device. `None`
/// (engine chooses), present devices, unsafe names (owned by the
/// prepare-level virtual-device rule), and enumeration failures pass through
/// unchanged.
pub(super) fn resolve_effective_output_device(
    desired_device: Option<&str>,
) -> EffectiveOutputDevice {
    let passthrough = |device: Option<String>| EffectiveOutputDevice {
        device,
        fallback: None,
    };
    let Some(desired) = desired_device
        .map(str::trim)
        .filter(|desired| !desired.is_empty())
    else {
        return passthrough(None);
    };
    if !is_safe_output_device_name(desired) {
        return passthrough(Some(desired.to_string()));
    }
    let enumerated = match list_audio_devices() {
        Ok(devices) => devices,
        Err(_) => return passthrough(Some(desired.to_string())),
    };
    let mut candidates = Vec::new();
    for device in &enumerated {
        let Some(name) = device.get("name").and_then(Value::as_str) else {
            continue;
        };
        candidates.push(FallbackCandidate {
            name: name.to_string(),
            is_default: device
                .get("is_default")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    resolve_effective_output_device_with_candidates(desired, &candidates)
}

/// Pure core of [`resolve_effective_output_device`] over an
/// already-enumerated candidate list, so substitution stays unit-testable
/// without audio hardware.
fn resolve_effective_output_device_with_candidates(
    desired: &str,
    candidates: &[FallbackCandidate],
) -> EffectiveOutputDevice {
    let names: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.name.as_str())
        .collect();
    if device_is_enumerated(desired, &names) {
        return EffectiveOutputDevice {
            device: Some(desired.to_string()),
            fallback: None,
        };
    }
    match select_fallback_output_device(candidates) {
        Some(applied) => EffectiveOutputDevice {
            device: Some(applied.clone()),
            fallback: Some(OutputDeviceFallback {
                desired: desired.to_string(),
                applied,
            }),
        },
        None => EffectiveOutputDevice {
            device: Some(desired.to_string()),
            fallback: None,
        },
    }
}

/// Elevate the calling daemon-owned thread to audio-work priority.
///
/// The engine's decoder/processing/playback threads already do this for
/// themselves. The daemon's own threads (accept loop, IPC handlers, driver
/// config watcher, startup playback) run at default priority otherwise, which
/// leaves control-plane latency — and therefore pipeline-mutation and
/// metering responsiveness — at the mercy of default scheduling. Uses the
/// engine's existing elevation helper, so no new platform `unsafe` is
/// introduced here. Best effort: logs and continues at default priority when
/// elevation is unavailable.
pub(super) fn elevate_daemon_thread_to_audio_work(context: &str) {
    match sotf_audio::engine::rt_priority::set_realtime_priority(
        sotf_audio::engine::rt_priority::RtPriority::Processing,
        None,
    ) {
        Ok(true) => log::info!("[Daemon] Audio-work priority set for {context}"),
        Ok(false) => log::debug!("[Daemon] Audio-work priority unavailable for {context}"),
        Err(error) => log::debug!("[Daemon] Audio-work priority failed for {context}: {error}"),
    }
}

pub(super) fn is_safe_output_device_name(name: &str) -> bool {
    let lowercase_name = name.to_ascii_lowercase();
    ![
        "sotf",
        "blackhole",
        "zoomaudio",
        "loopback",
        "virtual",
        "soundflower",
        "background music",
        "audio bridge",
    ]
    .iter()
    .any(|virtual_name| lowercase_name.contains(virtual_name))
}

pub(super) fn list_audio_devices() -> Result<Vec<serde_json::Value>, String> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let mut devices = Vec::new();

    if let Some(default_out) = host.default_output_device() {
        let name = default_out
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "Unknown".to_string());
        devices.push(serde_json::json!({
            "name": name,
            "is_default": true,
        }));
    }

    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| "Unknown".to_string());

            if devices
                .iter()
                .any(|d| d["name"] == name && d["is_default"] == true)
            {
                continue;
            }

            let mut device_info = serde_json::json!({
                "name": name,
                "is_default": false,
            });

            if let Ok(config) = device.default_output_config() {
                device_info["channels"] = config.channels().into();
                device_info["sample_rate"] = config.sample_rate().into();
            }

            devices.push(device_info);
        }
    }

    // Include ASIO devices (Windows only, requires asio feature)
    for asio_name in sotf_audio::devices::list_asio_devices() {
        devices.push(serde_json::json!({
            "name": asio_name,
            "is_default": false,
            "backend": "ASIO",
        }));
    }

    if devices.is_empty() {
        Err("No audio devices found".to_string())
    } else {
        Ok(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EffectiveOutputDevice, FallbackCandidate, acquire_daemon_instance_lock,
        clarify_device_absence_error, clarify_device_absence_error_with_devices,
        device_is_enumerated, effective_output_for_sink, list_audio_devices,
        resolve_effective_output_device, resolve_effective_output_device_with_candidates,
        scaled_buffer_frames_for_rate_change, select_fallback_output_device,
        startup_error_is_device_availability, wait_for_output_device,
    };
    use sotf_audio::SinkType;

    #[test]
    fn rate_change_scales_frames_to_keep_block_duration() {
        // 512 frames at 48 kHz is 10.67 ms; the same duration at 96 kHz is
        // 1024 frames. Keeping 512 frames would halve callback headroom.
        assert_eq!(
            scaled_buffer_frames_for_rate_change(48_000, 96_000, 512),
            1024
        );
        assert_eq!(
            scaled_buffer_frames_for_rate_change(96_000, 48_000, 1024),
            512
        );
        assert_eq!(
            scaled_buffer_frames_for_rate_change(48_000, 48_000, 512),
            512
        );
        // Non-integer ratios round to nearest; bounds clamp huge swings.
        assert_eq!(
            scaled_buffer_frames_for_rate_change(44_100, 96_000, 512),
            1115
        );
        assert_eq!(
            scaled_buffer_frames_for_rate_change(48_000, 192_000, 4096),
            4096
        );
        // Unknown current rate keeps the frame count, never zero.
        assert_eq!(scaled_buffer_frames_for_rate_change(0, 96_000, 512), 512);
    }
    #[cfg(unix)]
    use super::{bind_unix_socket, socket_is_unix_socket};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn daemon_ownership_lock_rejects_overlapping_runtime_resources() {
        let directory = tempdir().expect("create temporary directory");
        let shared_memory = directory.path().join("audio.shm");
        let session_key = directory.path().join("session.key");
        let alternate_socket = directory.path().join("alternate.sock");

        let first = acquire_daemon_instance_lock(&[shared_memory.clone(), session_key.clone()])
            .expect("acquire first transport ownership");
        let error = acquire_daemon_instance_lock(&[alternate_socket, session_key.clone()])
            .expect_err("overlapping session key must have one owner");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);

        drop(first);
        acquire_daemon_instance_lock(&[shared_memory, session_key])
            .expect("ownership must be released when the daemon lock drops");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_ownership_lock_rejects_a_symlink_lock_file() {
        let directory = tempdir().expect("create temporary directory");
        let resource = directory.path().join("audio.shm");
        let target = directory.path().join("attacker-target");
        let lock_path = directory.path().join("audio.shm.owner.lock");
        fs::write(&target, b"preserve target").expect("create symlink target");
        std::os::unix::fs::symlink(&target, &lock_path).expect("plant lock symlink");

        let error = acquire_daemon_instance_lock(&[resource])
            .expect_err("ownership lock must not follow a symlink");
        assert_ne!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert_eq!(
            fs::read(&target).expect("read preserved target"),
            b"preserve target"
        );
        assert!(
            fs::symlink_metadata(&lock_path)
                .expect("read preserved lock symlink")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn bind_unix_socket_accepts_a_fresh_path_and_reports_socket_type() {
        let directory = tempdir().expect("create temporary directory");
        let path = directory.path().join("daemon.sock");

        let listener = bind_unix_socket(&path).expect("bind fresh socket");
        assert!(socket_is_unix_socket(&path));
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn bind_unix_socket_rejects_a_live_daemon_without_unlinking_it() {
        let directory = tempdir().expect("create temporary directory");
        let path = directory.path().join("daemon.sock");
        let listener = bind_unix_socket(&path).expect("bind live socket");

        let error = bind_unix_socket(&path).expect_err("live daemon must remain authoritative");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(socket_is_unix_socket(&path));
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn bind_unix_socket_rejects_regular_files_without_removing_them() {
        let directory = tempdir().expect("create temporary directory");
        let path = directory.path().join("daemon.sock");
        fs::write(&path, b"do not remove").expect("create regular file");

        assert!(bind_unix_socket(&path).is_err());
        assert_eq!(
            fs::read(&path).expect("read preserved file"),
            b"do not remove"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bind_unix_socket_rejects_symlinks_without_following_or_removing_them() {
        let directory = tempdir().expect("create temporary directory");
        let target = directory.path().join("target");
        let path = directory.path().join("daemon.sock");
        fs::write(&target, b"target file").expect("create symlink target");
        std::os::unix::fs::symlink(&target, &path).expect("create symlink");

        assert!(bind_unix_socket(&path).is_err());
        assert!(
            fs::symlink_metadata(&path)
                .expect("read preserved symlink")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn bind_unix_socket_reclaims_a_stale_socket_only() {
        let directory = tempdir().expect("create temporary directory");
        let path = directory.path().join("daemon.sock");
        let stale = bind_unix_socket(&path).expect("bind stale fixture");
        drop(stale);

        let replacement = bind_unix_socket(&path).expect("replace stale socket");
        assert!(socket_is_unix_socket(&path));
        drop(replacement);
    }

    #[test]
    fn startup_retry_triggers_only_on_device_availability_errors() {
        // Observed cold-start races: USB re-scan, CoreAudio id churn.
        assert!(startup_error_is_device_availability(Some(
            "Selected output device 'EVO8' is not available: Audio device 'EVO8' not found"
        )));
        assert!(startup_error_is_device_availability(Some(
            "Audio device 'EVO8' needs playback stream recovery: stream error reported by CoreAudio (1 total)"
        )));
        assert!(startup_error_is_device_availability(Some(
            "CoreAudio device id changed for 'EVO8' (Some(120) -> Some(286))"
        )));
        // Cold-start timing races the retry loop must absorb: CoreAudio
        // still applying the HAL config, device stream not yet delivering
        // callbacks. The user-visible workaround used to be switching the
        // output device away and back, i.e. exactly one manual retry.
        assert!(startup_error_is_device_availability(Some(
            "Failed to load plugin chain: Playback did not reach a hardware callback within 12000ms"
        )));
        assert!(startup_error_is_device_availability(Some(
            "Failed to set HAL input channels: Timed out waiting for HAL to apply config request"
        )));
        // Permanent failures must surface immediately, never burn retries.
        assert!(!startup_error_is_device_availability(None));
        assert!(!startup_error_is_device_availability(Some("")));
        assert!(!startup_error_is_device_availability(Some(
            "Failed to parse loudness compensation params: invalid type"
        )));
    }

    #[test]
    fn clarify_device_absence_rewrites_generic_errors_for_missing_devices() {
        let rewritten = clarify_device_absence_error_with_devices(
            "EVO8",
            "Engine thread exited unexpectedly",
            &["Mac mini Speakers", "UltraFine Pro"],
        );
        assert!(rewritten.contains("Selected output device 'EVO8' is not available"));
        assert!(rewritten.contains("Audio device 'EVO8' not found"));
        assert!(rewritten.contains("Mac mini Speakers, UltraFine Pro"));
        assert!(rewritten.contains("Engine reported: Engine thread exited unexpectedly"));
        assert!(startup_error_is_device_availability(Some(&rewritten)));
    }

    #[test]
    fn clarify_device_absence_passes_through_when_no_rewrite_applies() {
        // Errors that already name the device race keep the engine wording.
        let detailed =
            "Selected output device 'EVO8' is not available: Audio device 'EVO8' not found";
        assert_eq!(
            clarify_device_absence_error(Some("EVO8"), detailed),
            detailed
        );
        // Empty requests cannot name a missing device.
        assert_eq!(clarify_device_absence_error(None, "boom"), "boom");
        assert_eq!(clarify_device_absence_error(Some("  "), "boom"), "boom");
        // A present device means the engine failed for another reason.
        assert_eq!(
            clarify_device_absence_error_with_devices(
                "EVO8",
                "Engine thread exited unexpectedly",
                &["EVO8", "Mac mini Speakers"],
            ),
            "Engine thread exited unexpectedly"
        );
        // ASIO prefix differences must not read as absence.
        assert_eq!(
            clarify_device_absence_error_with_devices(
                "ASIO:EVO8",
                "Engine thread exited unexpectedly",
                &["EVO8"],
            ),
            "Engine thread exited unexpectedly"
        );
    }

    fn candidate(name: &str, is_default: bool) -> FallbackCandidate {
        FallbackCandidate {
            name: name.to_string(),
            is_default,
        }
    }

    #[test]
    fn fallback_prefers_the_safe_default_output() {
        let candidates = vec![
            candidate("EVO8", false),
            candidate("Mac mini Speakers", true),
        ];
        assert_eq!(
            select_fallback_output_device(&candidates).as_deref(),
            Some("Mac mini Speakers")
        );
    }

    #[test]
    fn fallback_skips_a_virtual_default_output() {
        // The system default is routinely the SotF virtual device itself;
        // opening it as the real output would feed processed audio back
        // into capture.
        let candidates = vec![
            candidate("SotF Virtual Audio", true),
            candidate("USB audio CODEC", false),
        ];
        assert_eq!(
            select_fallback_output_device(&candidates).as_deref(),
            Some("USB audio CODEC")
        );
    }

    #[test]
    fn fallback_rejects_all_virtual_or_empty_enumeration() {
        let candidates = vec![
            candidate("SotF Virtual Audio", true),
            candidate("BlackHole", false),
        ];
        assert_eq!(select_fallback_output_device(&candidates), None);
        assert_eq!(select_fallback_output_device(&[]), None);
    }

    #[test]
    fn effective_device_substitutes_only_a_missing_safe_desired_device() {
        let candidates = vec![
            candidate("SotF Virtual Audio", true),
            candidate("Mac mini Speakers", false),
        ];
        // Present desired passes through untouched.
        let effective =
            resolve_effective_output_device_with_candidates("Mac mini Speakers", &candidates);
        assert_eq!(effective.device.as_deref(), Some("Mac mini Speakers"));
        assert!(effective.fallback.is_none());
        // Missing desired degrades to the default-safe output.
        let effective = resolve_effective_output_device_with_candidates("EVO8", &candidates);
        assert_eq!(effective.device.as_deref(), Some("Mac mini Speakers"));
        let fallback = effective.fallback.expect("fallback record");
        assert_eq!(fallback.desired, "EVO8");
        assert_eq!(fallback.applied, "Mac mini Speakers");
        // Nothing safe enumerated: pass through so the start fails loudly.
        let effective = resolve_effective_output_device_with_candidates("EVO8", &[]);
        assert_eq!(effective.device.as_deref(), Some("EVO8"));
        assert!(effective.fallback.is_none());
        // ASIO prefix gap still counts as present.
        let candidates = vec![candidate("EVO8", false)];
        let effective = resolve_effective_output_device_with_candidates("ASIO:EVO8", &candidates);
        assert_eq!(effective.device.as_deref(), Some("ASIO:EVO8"));
        assert!(effective.fallback.is_none());
    }

    #[test]
    fn lab_sink_never_receives_a_physical_device() {
        let candidates = vec![candidate("Mac mini Speakers", true)];
        // Missing desired resolved to a physical fallback: the lab sink
        // would reject that name, so it degrades to the lab output with
        // the original desired preserved in the fallback record.
        let resolved = resolve_effective_output_device_with_candidates("EVO8", &candidates);
        let adapted = effective_output_for_sink(resolved, SinkType::LabNull, Some("EVO8"));
        assert_eq!(
            adapted.device.as_deref(),
            Some(super::LAB_OUTPUT_DEVICE_NAME)
        );
        let fallback = adapted.fallback.expect("fallback record");
        assert_eq!(fallback.desired, "EVO8");
        assert_eq!(fallback.applied, super::LAB_OUTPUT_DEVICE_NAME);

        // A present physical desired device is substituted the same way.
        let resolved =
            resolve_effective_output_device_with_candidates("Mac mini Speakers", &candidates);
        let adapted =
            effective_output_for_sink(resolved, SinkType::LabNull, Some("Mac mini Speakers"));
        assert_eq!(
            adapted.device.as_deref(),
            Some(super::LAB_OUTPUT_DEVICE_NAME)
        );
        assert_eq!(
            adapted.fallback.expect("fallback record").desired,
            "Mac mini Speakers"
        );

        // An implicit lab route is canonicalized without a self-fallback;
        // the CPAL sink is never adapted.
        let none = EffectiveOutputDevice {
            device: None,
            fallback: None,
        };
        let adapted = effective_output_for_sink(none, SinkType::LabNull, None);
        assert_eq!(
            adapted.device.as_deref(),
            Some(super::LAB_OUTPUT_DEVICE_NAME)
        );
        assert!(adapted.fallback.is_none());

        let resolved =
            resolve_effective_output_device_with_candidates("Mac mini Speakers", &candidates);
        let unadapted =
            effective_output_for_sink(resolved, SinkType::Cpal, Some("Mac mini Speakers"));
        assert_eq!(unadapted.device.as_deref(), Some("Mac mini Speakers"));
        assert!(unadapted.fallback.is_none());
    }

    #[test]
    fn effective_device_passes_through_without_enumeration() {
        // None and unsafe names never consult enumeration: the engine owns
        // the former, the prepare-level virtual-device rule the latter.
        let effective = resolve_effective_output_device(None);
        assert_eq!(effective.device, None);
        assert!(effective.fallback.is_none());
        let effective = resolve_effective_output_device(Some("SotF Virtual Audio"));
        assert_eq!(effective.device.as_deref(), Some("SotF Virtual Audio"));
        assert!(effective.fallback.is_none());
        assert!(!device_is_enumerated("EVO8", &[]));
        assert!(device_is_enumerated(
            "ASIO:EVO8",
            &["Mac mini Speakers", "EVO8"]
        ));
    }

    #[test]
    fn clarify_device_absence_uses_live_enumeration_when_available() {
        // End-to-end through real CoreAudio/CPAL enumeration. Headless
        // machines enumerate nothing and must pass the error through; audio
        // machines must rewrite a generic failure for an absent device into
        // a retryable, actionable fault.
        let engine_error = "Engine thread exited unexpectedly";
        let result =
            clarify_device_absence_error(Some("definitely-not-a-real-device-xyz"), engine_error);
        match list_audio_devices() {
            Ok(_) => {
                assert!(
                    result.contains("definitely-not-a-real-device-xyz' is not available"),
                    "live enumeration must drive the rewrite: {result}"
                );
                assert!(startup_error_is_device_availability(Some(&result)));
            }
            Err(_) => assert_eq!(result, engine_error),
        }
    }

    #[test]
    fn wait_for_output_device_times_out_on_absent_device() {
        let present = wait_for_output_device(
            "definitely-not-a-real-device-xyz",
            std::time::Duration::from_millis(600),
            &|| true,
        );
        assert!(!present);
    }

    #[test]
    fn wait_for_output_device_aborts_on_shutdown() {
        let started = std::time::Instant::now();
        let present = wait_for_output_device(
            "definitely-not-a-real-device-xyz",
            std::time::Duration::from_secs(30),
            &|| false,
        );
        assert!(!present);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "shutdown must abort the wait, took {:?}",
            started.elapsed()
        );
    }
}
