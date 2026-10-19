use super::command::Command;
use super::configured::{configured_output_device, persist_output_device};
use super::consts::LEGACY_SOCKET_PATH;
use super::consts::MAX_HAL_CHANNELS;
use super::consts::SUPPORTED_SAMPLE_RATES;
use super::consts::empty_loudness_json;
use super::consts::get_socket_path;
use super::consts::metering_source_json;
use super::device_registry::DeviceRegistry;
use super::driver_manager::{DriverManager, get_driver_status};
use super::loudness::loudness_data_to_json;
use super::loudness::loudness_info_to_json;
#[cfg(unix)]
use super::misc::bind_unix_socket;
use super::misc::build_driver_plugin_chain;
use super::misc::elevate_daemon_thread_to_audio_work;
use super::misc::is_safe_output_device_name;
use super::misc::push_metering_faults;
#[cfg(unix)]
use super::misc::socket_is_unix_socket;
use super::misc::transport_snapshot_and_faults;
use super::output_profiles::{OutputProfile, OutputProfileStore, StartupChain};
use super::pipeline_reconfigure_outcome::handle_driver_config_change_with_profiles as handle_driver_config_change;
use super::pipeline_spec::pipeline_spec_to_json;
use super::pipeline_spec::pipeline_specs_match;
use super::plugin::plugin_parameter_descriptors;
use super::plugin::plugin_type_category;
use super::plugin::plugin_type_to_engine_str;
use super::plugin_artifact::{PluginArtifactPlan, plan_plugin_artifact};
use super::response::Response;
use super::response::serialize_response_safely;
use super::security::{
    KeyManager, PeerClass, classify_peer, current_uid as security_current_uid,
    ensure_secure_socket_dir, peer_allows_command, validate_user_load_path,
    verify_peer_credentials,
};
use super::systemwide_state::SystemwideState;
use super::types::IpcLine;
use super::types::PipelinePlan;
use super::types::read_ipc_line_bounded;
use crate::ipc_transport::IpcStream;
#[cfg(windows)]
use crate::ipc_transport::bind_windows_listener;
#[cfg(windows)]
use crate::ipc_transport::windows_port_file_is_live as socket_is_unix_socket;
use driver_common::DriverConfig;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use sotf_audio::PluginConfig;
use sotf_audio::SinkType;
use sotf_audio::engine::{PluginGraphConfig, PluginGraphEdgeConfig, PluginGraphNodeConfig};
use sotf_audio::manager::AudioEngineManager;
use sotf_audio::plugins::PluginType;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::consts::MAX_IPC_CLIENTS;

/// Stable wire shape returned by `get_driver_config`/`get_hal_config`.
///
/// The daemon historically exposed configuration through a hand-built JSON
/// object with aliases such as `active` and `actual_sample_rate`. Keep those
/// aliases stable, but make the shape explicit so additions to
/// `DriverStatus` cannot silently change this separate endpoint.
#[derive(Debug, Serialize)]
struct DriverConfigWire {
    sample_rate: u32,
    actual_sample_rate: u32,
    buffer_frames: u32,
    actual_buffer_frames: u32,
    channel_count: u32,
    active: bool,
    driver_name: &'static str,
    driver_installed: bool,
    driver_ready: bool,
    platform_supported: bool,
    /// HAL config-negotiation state (0=pending, 1=accepted, 2=negotiated,
    /// 3=error). Zero on non-HAL drivers; enriched from the shared header
    /// by `handle_get_driver_config` on macOS HAL builds.
    config_status: u32,
    /// Who initiated the last config change (1=HAL, 2=daemon).
    config_source: u32,
    /// Every sample rate the daemon accepts, so clients populate rate
    /// pickers from the daemon's table instead of a hardcoded subset.
    supported_sample_rates: [u32; 6],
}

/// Reusing a stream, replacing output, and renegotiating capture are distinct intents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PipelineApplyIntent {
    EnsurePlaying,
    RebuildOutput,
    ReconfigureDriver,
    SelectRoute,
    RestoreLastWorking,
}

/// Temporarily uses the engine's click-free mute ramp around a pipeline
/// teardown. The manager preserves this mute state while the replacement
/// engine starts, then the guard restores the user's unmuted state.
struct ReconfigurationMuteGuard {
    manager: Arc<Mutex<AudioEngineManager>>,
    restore_unmuted: bool,
}

impl ReconfigurationMuteGuard {
    fn begin(manager: &Arc<Mutex<AudioEngineManager>>) -> Result<Self, String> {
        let restore_unmuted = {
            let manager = manager.lock();
            if manager.get_state() == sotf_audio::manager::StreamingState::Playing
                && manager.get_playback_state() == sotf_audio::PlaybackState::Playing
                && !manager.is_muted()
            {
                if let Err(error) = manager.set_mute(true) {
                    // AudioEngineManager records the requested mute state
                    // before sending it to the engine. Restore that cache even
                    // when the output thread has already disappeared.
                    if let Err(restore_error) = manager.set_mute(false) {
                        log::error!(
                            "Failed to restore mute state after reconfiguration mute error: {restore_error}"
                        );
                    }
                    return Err(format!(
                        "Failed to mute output before reconfiguration: {error}"
                    ));
                }
                true
            } else {
                false
            }
        };

        if restore_unmuted {
            // Match the engine output gain ramp before tearing down CoreAudio.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        Ok(Self {
            manager: Arc::clone(manager),
            restore_unmuted,
        })
    }
}

impl Drop for ReconfigurationMuteGuard {
    fn drop(&mut self) {
        if self.restore_unmuted
            && let Err(error) = self.manager.lock().set_mute(false)
        {
            log::error!("Failed to restore output mute after reconfiguration: {error}");
        }
    }
}

impl From<&driver_common::DriverStatus> for DriverConfigWire {
    fn from(status: &driver_common::DriverStatus) -> Self {
        Self {
            sample_rate: status.sample_rate,
            actual_sample_rate: status.sample_rate,
            buffer_frames: status.buffer_frames,
            actual_buffer_frames: status.buffer_frames,
            channel_count: status.channel_count,
            active: status.capture_active,
            driver_name: status.driver_name,
            driver_installed: status.driver_installed,
            driver_ready: status.driver_ready,
            platform_supported: status.platform_supported,
            config_status: 0,
            config_source: 0,
            supported_sample_rates: SUPPORTED_SAMPLE_RATES,
        }
    }
}

pub(super) fn pipeline_timing_after_config_request(
    result: &driver_common::ConfigResult,
    requested_sample_rate: u32,
    requested_buffer_frames: u32,
) -> (u32, u32) {
    match result {
        driver_common::ConfigResult::Negotiated {
            actual_rate,
            actual_frames,
            ..
        } => (*actual_rate, *actual_frames),
        driver_common::ConfigResult::Accepted | driver_common::ConfigResult::Error(_) => {
            (requested_sample_rate, requested_buffer_frames)
        }
        _ => (requested_sample_rate, requested_buffer_frames),
    }
}

/// Return the node IDs of a graph when it is exactly one linear chain.
fn linear_graph_node_ids(graph: &PluginGraphConfig) -> Option<Vec<usize>> {
    if graph.nodes.is_empty() {
        return graph.edges.is_empty().then_some(Vec::new());
    }
    if graph.edges.len() != graph.nodes.len().saturating_sub(1) {
        return None;
    }

    let node_ids: HashSet<usize> = graph.nodes.iter().map(|node| node.id).collect();
    if node_ids.len() != graph.nodes.len() {
        return None;
    }

    let mut incoming = HashMap::<usize, usize>::with_capacity(graph.nodes.len());
    let mut outgoing = HashMap::<usize, usize>::with_capacity(graph.nodes.len());
    for &id in &node_ids {
        incoming.insert(id, 0);
    }
    for edge in &graph.edges {
        if !node_ids.contains(&edge.from_node) || !node_ids.contains(&edge.to_node) {
            return None;
        }
        *incoming.get_mut(&edge.to_node)? += 1;
        if outgoing.insert(edge.from_node, edge.to_node).is_some() {
            return None;
        }
    }

    let mut roots = incoming
        .iter()
        .filter_map(|(&id, &count)| (count == 0).then_some(id));
    let root = roots.next()?;
    if roots.next().is_some() {
        return None;
    }

    let mut order = Vec::with_capacity(graph.nodes.len());
    let mut current = Some(root);
    while let Some(id) = current {
        order.push(id);
        current = outgoing.get(&id).copied();
    }
    (order.len() == graph.nodes.len()).then_some(order)
}

/// Reorder a linear graph without changing node IDs, parameters, channel
/// counts, or bypass state. The order is expressed as node IDs, not positions.
pub(super) fn reorder_linear_graph(
    graph: &PluginGraphConfig,
    order: &[usize],
) -> Result<PluginGraphConfig, String> {
    graph
        .validate()
        .map_err(|error| format!("Invalid plugin graph: {error}"))?;
    let current_order = linear_graph_node_ids(graph)
        .ok_or_else(|| "Graph reorder requires a single linear graph".to_string())?;
    if order.len() != current_order.len() {
        return Err(format!(
            "Order length {} doesn't match graph node count {}",
            order.len(),
            current_order.len()
        ));
    }

    let expected: HashSet<usize> = current_order.iter().copied().collect();
    let mut seen = HashSet::with_capacity(order.len());
    for &id in order {
        if !expected.contains(&id) || !seen.insert(id) {
            return Err(format!(
                "Invalid graph order: duplicate or unknown node ID {id}"
            ));
        }
    }

    let nodes_by_id: HashMap<usize, &PluginGraphNodeConfig> =
        graph.nodes.iter().map(|node| (node.id, node)).collect();
    let Some(nodes) = order
        .iter()
        .map(|id| nodes_by_id.get(id).map(|node| (*node).clone()))
        .collect::<Option<Vec<_>>>()
    else {
        return Err("Invalid graph order: node lookup failed".to_string());
    };
    let edges = order
        .windows(2)
        .map(|pair| PluginGraphEdgeConfig::new(pair[0], pair[1]))
        .collect::<Vec<_>>();

    PluginGraphConfig::try_new(nodes, edges)
        .map_err(|error| format!("Reordered graph is invalid: {error}"))
}

/// Convert the legacy rack representation into a linear graph so per-node
/// channel and bypass state has a durable owner. Existing rack order and
/// plugin parameters are preserved; unspecified state uses the pipeline's
/// current input geometry and enabled-by-default behavior.
pub(super) fn rack_plugins_to_linear_graph(
    plugins: &[PluginConfig],
    pipeline_input_channels: usize,
    selected_index: usize,
    input_channels: Option<usize>,
    bypassed: Option<bool>,
) -> Result<PluginGraphConfig, String> {
    if input_channels.is_none() && bypassed.is_none() {
        return Err("set_rack_plugin_state requires input_channels or bypassed".to_string());
    }
    if selected_index >= plugins.len() {
        return Err(format!(
            "Plugin index {} out of range (have {})",
            selected_index,
            plugins.len()
        ));
    }
    if let Some(channels) = input_channels
        && !(1..=MAX_HAL_CHANNELS).contains(&channels)
    {
        return Err(format!(
            "Invalid plugin input channel count {}. Must be between 1 and {}.",
            channels, MAX_HAL_CHANNELS
        ));
    }

    let default_channels = pipeline_input_channels.max(1);
    let nodes = plugins
        .iter()
        .enumerate()
        .map(|(index, plugin)| PluginGraphNodeConfig {
            id: index,
            plugin_type: plugin.plugin_type.clone(),
            parameters: plugin.parameters.clone(),
            input_channels: if index == selected_index {
                input_channels.unwrap_or(default_channels)
            } else {
                default_channels
            },
            bypassed: if index == selected_index {
                bypassed.unwrap_or(false)
            } else {
                false
            },
        })
        .collect::<Vec<_>>();
    let edges = (0..nodes.len().saturating_sub(1))
        .map(|index| PluginGraphEdgeConfig::new(index, index + 1))
        .collect::<Vec<_>>();

    PluginGraphConfig::try_new(nodes, edges)
        .map_err(|error| format!("Rack state cannot be represented as a graph: {error}"))
}

/// Encode one plugin parameter JSON value in the string form the engine's
/// `set_plugin_parameter` path expects: plain strings pass through raw,
/// primitives render bare, and complex values travel as JSON.
pub(super) fn encode_plugin_param_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

/// Changed top-level parameter keys when both sides are JSON objects.
/// Returns `None` when the shapes are not diffable key-by-key (non-object
/// sides, or removed keys), in which case the caller must fall back to a
/// full chain rebuild.
pub(super) fn changed_plugin_parameters(old: &Value, new: &Value) -> Option<Vec<(String, Value)>> {
    let (old_map, new_map) = match (old.as_object(), new.as_object()) {
        (Some(old_map), Some(new_map)) => (old_map, new_map),
        _ => return None,
    };
    if old_map.keys().any(|key| !new_map.contains_key(key)) {
        return None;
    }
    let mut changed = Vec::new();
    for (key, new_value) in new_map {
        if old_map.get(key) != Some(new_value) {
            changed.push((key.clone(), new_value.clone()));
        }
    }
    Some(changed)
}

/// Cold-start channel geometry: capture at the HAL transport's actual channel
/// count so startup does not force an immediate stop/start reconfigure cycle
/// (audible gap plus a bounded callback wait) on every launch, while playback
/// keeps the daemon's desired output channels. Unusable HAL values fall back
/// to stereo in and clamped desired output.
pub(super) fn startup_channel_geometry(
    hal_channel_count: u32,
    desired_output_channels: usize,
) -> (usize, usize) {
    let input_channels = usize::try_from(hal_channel_count)
        .ok()
        .filter(|channels| (1..=MAX_HAL_CHANNELS).contains(channels))
        .unwrap_or(2);
    (
        input_channels,
        desired_output_channels.clamp(1, MAX_HAL_CHANNELS),
    )
}

pub(super) const METERING_LATENCY_BUDGET_MICROS: u64 = 5_000;
pub(super) const PLAYBACK_IDLE_REBUILD_THRESHOLD: Duration = Duration::from_secs(30);
const PIPELINE_LATENCY_BUDGET_MICROS: u64 = 1_000_000;
const METERING_RESPONSE_BUDGET_BYTES: usize = 64 * 1024;
pub(super) const PIPELINE_RESPONSE_BUDGET_BYTES: usize = 256 * 1024;

#[derive(Debug, Default)]
struct OperationTelemetry {
    requests: AtomicU64,
    total_micros: AtomicU64,
    max_micros: AtomicU64,
    max_response_bytes: AtomicU64,
    budget_exceeded: AtomicU64,
}

impl OperationTelemetry {
    fn record(
        &self,
        elapsed: std::time::Duration,
        response_bytes: usize,
        latency_budget_micros: u64,
        response_budget_bytes: usize,
    ) {
        let elapsed_micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        let response_bytes = response_bytes.min(u64::MAX as usize) as u64;
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.total_micros
            .fetch_add(elapsed_micros, Ordering::Relaxed);
        self.max_micros.fetch_max(elapsed_micros, Ordering::Relaxed);
        self.max_response_bytes
            .fetch_max(response_bytes, Ordering::Relaxed);
        if elapsed_micros > latency_budget_micros || response_bytes > response_budget_bytes as u64 {
            self.budget_exceeded.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> Value {
        let requests = self.requests.load(Ordering::Relaxed);
        let total_micros = self.total_micros.load(Ordering::Relaxed);
        serde_json::json!({
            "requests": requests,
            "average_micros": total_micros.checked_div(requests).unwrap_or(0),
            "max_micros": self.max_micros.load(Ordering::Relaxed),
            "max_response_bytes": self.max_response_bytes.load(Ordering::Relaxed),
            "budget_exceeded": self.budget_exceeded.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct RuntimeTelemetry {
    metering: OperationTelemetry,
    pipeline_reload: OperationTelemetry,
}

impl RuntimeTelemetry {
    pub(super) fn record_command(
        &self,
        command: &str,
        elapsed: std::time::Duration,
        response_bytes: usize,
    ) {
        match command {
            "get_metering" | "get_loudness" => self.metering.record(
                elapsed,
                response_bytes,
                METERING_LATENCY_BUDGET_MICROS,
                METERING_RESPONSE_BUDGET_BYTES,
            ),
            "load_plugins"
            | "load_plugin_artifact"
            | "load_plugin_artifact_path"
            | "add_plugin"
            | "remove_plugin"
            | "update_plugin"
            | "reorder_plugins"
            | "reorder_graph"
            | "set_input_channels"
            | "set_output_channels"
            | "set_pipeline_channels"
            | "set_output_route"
            | "set_device"
            | "apply_configuration"
            | "set_sample_rate"
            | "set_buffer_frames"
            | "play"
            | "pause"
            | "stop"
            | "set_encryption"
            | "rotate_encryption_key" => self.pipeline_reload.record(
                elapsed,
                response_bytes,
                PIPELINE_LATENCY_BUDGET_MICROS,
                PIPELINE_RESPONSE_BUDGET_BYTES,
            ),
            _ => {}
        }
    }

    pub(super) fn snapshot(&self) -> Value {
        serde_json::json!({
            "measurement": "complete_request_read_to_response_write_attempt",
            "metering": self.metering.snapshot(),
            "pipeline_reload": self.pipeline_reload.snapshot(),
            "budgets": {
                "metering_latency_micros": METERING_LATENCY_BUDGET_MICROS,
                "metering_response_bytes": METERING_RESPONSE_BUDGET_BYTES,
                "pipeline_latency_micros": PIPELINE_LATENCY_BUDGET_MICROS,
                "pipeline_response_bytes": PIPELINE_RESPONSE_BUDGET_BYTES,
            }
        })
    }
}

/// The shared engine currently batches counter reports at a five-second cadence.
/// Allow one report interval plus scheduling margin; never call an old nonzero
/// counter proof of current flow. This is control-plane freshness, not an RT SLA.
const PROGRESS_STALE_AFTER: Duration = Duration::from_secs(7);

#[derive(Debug, Default)]
pub(super) struct PlaybackProgress {
    key: Option<(u64, u64)>,
    counters: [u64; 3],
    changed: [Option<Instant>; 3],
    started: Option<Instant>,
    observed: Option<Instant>,
}

impl PlaybackProgress {
    /// The control owner knows this is a newly created engine, whose counters
    /// began at zero. A reported callback is fresh evidence for that instance.
    fn record_new_stream(
        &mut self,
        now: Instant,
        generation: u64,
        engine: &sotf_audio::AudioEngineState,
    ) {
        self.key = Some((generation, engine.playback_epoch));
        self.counters = [0; 3];
        self.changed = [None; 3];
        self.started = Some(now);
        self.observed = Some(now);
        self.observe(now, generation, true, engine);
    }

    pub(super) fn observe(
        &mut self,
        now: Instant,
        generation: u64,
        playing: bool,
        engine: &sotf_audio::AudioEngineState,
    ) {
        let key = (generation, engine.playback_epoch);
        let counters = [
            engine.playback_frames_received,
            engine.playback_callback_count,
            engine.playback_frames_written,
        ];
        if !playing
            || self.key != Some(key)
            || counters
                .iter()
                .zip(self.counters)
                .any(|(new, old)| *new < old)
        {
            self.key = Some(key);
            self.counters = counters;
            self.changed = [None; 3];
            self.started = Some(now);
        } else {
            for (index, value) in counters.into_iter().enumerate() {
                if value > self.counters[index] {
                    self.changed[index] = Some(now);
                }
                self.counters[index] = value;
            }
        }
        self.observed = Some(now);
    }

    fn age(now: Instant, then: Option<Instant>) -> Option<Duration> {
        then.map(|then| now.saturating_duration_since(then))
    }

    pub(super) fn status(
        &self,
        now: Instant,
        generation: u64,
        epoch: u64,
        stage: usize,
    ) -> &'static str {
        if self.key != Some((generation, epoch)) {
            return "unobserved_generation";
        }
        if Self::age(now, self.observed).is_some_and(|age| age >= PROGRESS_STALE_AFTER) {
            return "observation_delayed";
        }
        if Self::age(now, self.changed[stage]).is_some_and(|age| age < PROGRESS_STALE_AFTER) {
            return "fresh";
        }
        if Self::age(now, self.started).is_some_and(|age| age >= PROGRESS_STALE_AFTER) {
            "stale"
        } else {
            "awaiting_progress"
        }
    }

    fn snapshot(&self, now: Instant, generation: u64, epoch: u64) -> Value {
        let millis =
            |time| Self::age(now, time).map(|age| age.as_millis().min(u64::MAX as u128) as u64);
        serde_json::json!({
            "pipeline_generation": generation, "playback_epoch": epoch,
            "generation_matches": self.key == Some((generation, epoch)),
            "stale_after_ms": PROGRESS_STALE_AFTER.as_millis() as u64,
            "observation_age_ms": millis(self.observed),
            "frames_received_age_ms": millis(self.changed[0]),
            "callbacks_age_ms": millis(self.changed[1]),
            "frames_written_age_ms": millis(self.changed[2]),
            "input": self.status(now, generation, epoch, 0),
            "callbacks": self.status(now, generation, epoch, 1),
            "output": self.status(now, generation, epoch, 2),
        })
    }
}

#[derive(Debug)]
pub(super) struct CaptureResumeTracker {
    generation: u64,
    capture_active: bool,
    inactive_since: Option<Instant>,
}

impl CaptureResumeTracker {
    pub(super) fn new(now: Instant, generation: u64, capture_active: bool) -> Self {
        Self {
            generation,
            capture_active,
            inactive_since: (!capture_active).then_some(now),
        }
    }

    /// Returns true once when capture resumes after a sufficiently long idle.
    /// A pipeline generation change represents an explicit user/driver
    /// transition and resets the timer, so its own stream rebuild is never
    /// followed by a redundant automatic rebuild.
    pub(super) fn observe(&mut self, now: Instant, generation: u64, capture_active: bool) -> bool {
        if generation != self.generation {
            self.generation = generation;
            self.capture_active = capture_active;
            self.inactive_since = (!capture_active).then_some(now);
            return false;
        }

        let resumed_after_long_idle = capture_active
            && !self.capture_active
            && self.inactive_since.is_some_and(|inactive_since| {
                now.saturating_duration_since(inactive_since) >= PLAYBACK_IDLE_REBUILD_THRESHOLD
            });

        if capture_active {
            self.inactive_since = None;
        } else if self.capture_active || self.inactive_since.is_none() {
            self.inactive_since = Some(now);
        }
        self.capture_active = capture_active;
        resumed_after_long_idle
    }
}

/// Tracks the last driver volume generation applied to the engine so
/// driver-originated volume changes (macOS media keys adjusting the HAL
/// virtual device's volume control) apply exactly once per publication.
///
/// Daemon-originated changes echo back through the same generation counter
/// and are reported too; re-applying the identical volume/mute is a cheap
/// no-op on the manager. The first observation after startup deliberately
/// counts as a change so the persisted driver volume is adopted.
#[derive(Debug, Default)]
pub(super) struct VolumeSyncTracker {
    applied_generation: u32,
}

impl VolumeSyncTracker {
    pub(super) fn observe(&self, published: Option<(f32, bool, u32)>) -> Option<(f32, bool, u32)> {
        let (volume, muted, generation) = published?;
        if generation == 0 || generation == self.applied_generation {
            return None;
        }
        Some((volume, muted, generation))
    }

    pub(super) fn committed(&mut self, generation: u32) {
        self.applied_generation = generation;
    }
}

/// Debounces HAL-initiated config requests so an interface clock flapping
/// between rates while it renegotiates does not tear the pipeline down on
/// every poll.
///
/// Stability is judged by the *requested* config staying unchanged for the
/// debounce interval. It must never be judged against the driver's applied
/// status: `poll_config_change` consumes each request exactly once, and the
/// applied geometry only changes *after* the reconfigure this debouncer
/// gates — requiring status to match the request first deadlocks startup
/// (HAL requests 96 kHz, status stays 48 kHz, request dropped forever).
#[derive(Debug)]
pub(super) struct ConfigChangeDebouncer {
    pending: Option<(driver_common::DriverConfig, Instant)>,
    intent_epoch: Option<u64>,
}

impl ConfigChangeDebouncer {
    pub(super) const DEBOUNCE: Duration = Duration::from_millis(500);

    pub(super) fn new() -> Self {
        Self {
            pending: None,
            intent_epoch: None,
        }
    }

    /// Discard work queued before a newer serialized control intent.
    pub(super) fn observe_intent(
        &mut self,
        now: Instant,
        polled: Option<driver_common::DriverConfig>,
        intent_epoch: u64,
    ) -> Option<driver_common::DriverConfig> {
        if self.intent_epoch != Some(intent_epoch) {
            self.pending = None;
            self.intent_epoch = Some(intent_epoch);
        }
        self.observe(now, polled)
    }

    /// Feed one `poll_config_change` result; returns the config to apply
    /// once it has been the latest distinct request for the debounce window.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        polled: Option<driver_common::DriverConfig>,
    ) -> Option<driver_common::DriverConfig> {
        if let Some(config) = polled {
            match &self.pending {
                Some((pending, _)) if *pending == config => {}
                _ => self.pending = Some((config, now)),
            }
        }
        let (config, since) = self.pending?;
        if now.saturating_duration_since(since) >= Self::DEBOUNCE {
            self.pending = None;
            Some(config)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub(super) struct SystemwideController {
    pub(super) manager: Arc<Mutex<AudioEngineManager>>,
    pub(super) running: Arc<Mutex<bool>>,
    pub(super) driver_manager: Arc<Mutex<DriverManager>>,
    /// Desired and applied systemwide daemon state.
    pub(super) system_state: Arc<Mutex<SystemwideState>>,
    /// Encryption key manager
    pub(super) key_manager: Arc<Mutex<KeyManager>>,
    /// Serializes read-modify-apply pipeline mutations across IPC clients.
    pub(super) pipeline_mutation: Arc<Mutex<()>>,
    /// Supersedes background startup even for Stop/Pause, which do not apply a spec.
    pub(super) pipeline_intent_epoch: Arc<AtomicU64>,
    /// Low-overhead IPC latency and serialized-size regression telemetry.
    pub(super) runtime_telemetry: Arc<RuntimeTelemetry>,
    pub(super) playback_progress: Arc<Mutex<PlaybackProgress>>,
    pub(super) snapshot_cache: Arc<Mutex<Option<(Instant, Value)>>>,
    /// Bounded view of synchronous CoreAudio/CPAL discovery and capabilities.
    pub(super) device_registry: Arc<Mutex<DeviceRegistry>>,
    /// Per-output DSP profiles keyed by stable device identity.
    pub(super) output_profiles: Arc<Mutex<OutputProfileStore>>,
}

/// Compatibility name for the daemon process entry point. Runtime ownership
/// lives in `SystemwideController`; the process/socket layer only hosts it.
pub(super) use SystemwideController as AudioDaemon;

/// Try to reserve one bounded client-handler slot without taking a mutex in
/// the accept loop. The matching permit releases it when the handler exits.
pub(super) fn try_acquire_client_slot(active: &AtomicUsize) -> bool {
    try_acquire_slot(active, MAX_IPC_CLIENTS)
}

fn try_acquire_slot(active: &AtomicUsize, limit: usize) -> bool {
    loop {
        let current = active.load(Ordering::Acquire);
        if current >= limit {
            return false;
        }
        if active
            .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
}

pub(super) struct ClientSlot(pub(super) Arc<AtomicUsize>);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Keep the admission guard alive only when cloning the shutdown handle
/// succeeds.  In particular, an error must release the slot before the
/// accept loop proceeds to the next client.
pub(super) fn clone_client_shutdown_stream<T>(
    client_slot: ClientSlot,
    clone_stream: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<(ClientSlot, T)> {
    clone_stream().map(|stream| (client_slot, stream))
}

pub(super) fn join_initial_playback_thread(thread: std::thread::JoinHandle<()>) {
    if thread.join().is_err() {
        log::warn!("Initial playback worker panicked during shutdown");
    }
}

pub(super) fn wait_for_playback_observation(
    running: &Arc<Mutex<bool>>,
    timeout: Duration,
    shutdown_context: &str,
    mut observe: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + timeout;
    loop {
        if !*running.lock() {
            return Err(format!(
                "Daemon shutdown requested during {shutdown_context}"
            ));
        }
        if observe()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Playback did not reach a hardware callback within {}ms",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

impl SystemwideController {
    pub(super) fn requires_playback_callback(driver_status: &driver_common::DriverStatus) -> bool {
        if !driver_status.platform_supported || driver_status.driver_name == "Systemwide Lab Driver"
        {
            return false;
        }
        #[cfg(test)]
        if driver_status.driver_name == "Fake HAL" {
            return false;
        }
        true
    }

    /// Whether a recorded pipeline recovery may be healed without a restart.
    ///
    /// A driver-initiated reconfigure clears the HAL `engine_ready` flag and
    /// only restores it after the readiness wait observes healthy playback.
    /// When that wait fails on a transient fault (e.g. CoreAudio re-enumerates
    /// the output device mid-restart) the engine typically recovers on its own
    /// seconds later, but nothing re-publishes readiness: the HAL keeps
    /// holding input, meters stay at zero, and only a manual daemon restart
    /// clears it. Heal exactly when the recorded failure is stale, the driver
    /// is ready, and the engine meets the same health bar as startup
    /// readiness (no recorded error, callbacks flowing).
    pub(super) fn should_heal_stale_readiness(
        recovery_recorded: bool,
        driver_ready: bool,
        startup_observation: &Result<bool, String>,
    ) -> bool {
        recovery_recorded && driver_ready && matches!(startup_observation, Ok(true))
    }

    fn reconcile_readiness(&self, previous: &mut Option<(u64, u64)>) {
        let Some(_mutation) = self.pipeline_mutation.try_lock() else {
            return;
        };
        let (generation, recovery, applied) = {
            let state = self.system_state.lock();
            (
                state.generation(),
                state.pipeline_recovery(),
                state.applied_spec(),
            )
        };
        let (streaming, engine) = {
            let manager = self.manager.lock();
            (manager.get_state(), manager.get_engine_state())
        };
        let now = Instant::now();
        let fresh_callbacks = {
            let mut progress = self.playback_progress.lock();
            progress.observe(
                now,
                generation,
                streaming == sotf_audio::manager::StreamingState::Playing,
                &engine,
            );
            progress.status(now, generation, engine.playback_epoch, 1) == "fresh"
        };
        let progressed = previous.is_some_and(|(epoch, callbacks)| {
            epoch == generation && engine.playback_callback_count > callbacks
        });
        *previous = Some((generation, engine.playback_callback_count));
        if !progressed
            || !fresh_callbacks
            || streaming != sotf_audio::manager::StreamingState::Playing
        {
            return;
        }
        let route_matches = applied.is_some_and(|spec| {
            spec.output_device.is_none() || spec.output_device == engine.playback_output_device
        });
        let mut driver = self.driver_manager.lock();
        if route_matches
            && Self::should_heal_stale_readiness(
                recovery.is_some() || !driver.readiness_is_current(),
                driver.status().driver_ready,
                &Self::playback_startup_observation(&engine),
            )
            && driver.set_engine_ready(true)
        {
            self.system_state.lock().clear_pipeline_recovery();
            log::info!("Supervisor healed readiness after observing fresh playback progress");
        }
    }

    /// Reconcile the selected physical route and its profile under the caller's
    /// transition guard. A stopped/paused user session never auto-starts here.
    fn reconcile_output_route(&self) {
        let (desired, applied) = {
            let state = self.system_state.lock();
            (state.selected_output_device(), state.applied_spec())
        };
        let Some(desired) = desired else { return };
        let Some(applied) = applied else { return };
        let engine = {
            let manager = self.manager.lock();
            if manager.get_state() != sotf_audio::manager::StreamingState::Playing {
                return;
            }
            manager.get_engine_state()
        };
        let sink = self.driver_manager.lock().output_sink_type();
        let effective = super::misc::effective_output_for_sink(
            super::misc::resolve_effective_output_device(Some(&desired)),
            sink,
            Some(&desired),
        );
        let Some(target) = effective.device.as_deref() else {
            self.driver_manager.lock().set_engine_ready(false);
            self.system_state
                .lock()
                .mark_pipeline_recovery("No safe physical output is available".to_owned());
            return;
        };
        let actual_diverged = engine
            .playback_output_device
            .as_deref()
            .is_some_and(|actual| Some(actual) != applied.output_device.as_deref());
        if Some(target) == applied.output_device.as_deref() && !actual_diverged {
            return;
        }
        self.device_registry.lock().invalidate();
        self.pipeline_intent_epoch.fetch_add(1, Ordering::AcqRel);
        if target == desired && applied.output_device.as_deref() != Some(desired.as_str()) {
            let response = self.handle_set_output_route(&desired, None, None, None, None, None);
            if !response.success {
                log::warn!("Desired output restoration failed: {:?}", response.error);
            }
            return;
        }
        let driver = self.driver_manager.lock().status();
        let plan = {
            let state = self.system_state.lock();
            state.prepare_from_spec(state.desired_spec(), driver.channel_count.max(1) as usize)
        };
        if let Ok(plan) = plan {
            let (rate, frames) = self.processing_timing(&driver);
            let response = self.apply_pipeline_plan_with_driver_config(
                plan,
                driver,
                rate,
                frames,
                PipelineApplyIntent::RebuildOutput,
            );
            if !response.success {
                log::warn!("Output route reconciliation failed: {:?}", response.error);
            }
        }
    }

    pub(super) fn playback_startup_observation(
        state: &sotf_audio::engine::AudioEngineState,
    ) -> Result<bool, String> {
        if let Some(error) = state
            .last_error
            .as_deref()
            .filter(|error| !error.is_empty())
        {
            return Err(format!("Playback startup failed: {error}"));
        }
        Ok(state.playback_callback_count > 0)
    }

    fn wait_for_playback_ready(&self) -> Result<(), String> {
        const READY_TIMEOUT: Duration = Duration::from_secs(12);
        wait_for_playback_observation(&self.running, READY_TIMEOUT, "playback startup", || {
            let state = self.manager.lock().get_engine_state();
            Self::playback_startup_observation(&state)
        })
    }

    pub(super) fn new() -> Self {
        Self {
            manager: Arc::new(Mutex::new(AudioEngineManager::new())),
            running: Arc::new(Mutex::new(true)),
            driver_manager: Arc::new(Mutex::new(DriverManager::new())),
            system_state: Arc::new(Mutex::new(SystemwideState::default())),
            key_manager: Arc::new(Mutex::new(KeyManager::default())),
            pipeline_mutation: Arc::new(Mutex::new(())),
            pipeline_intent_epoch: Arc::new(AtomicU64::new(0)),
            runtime_telemetry: Arc::new(RuntimeTelemetry::default()),
            playback_progress: Arc::new(Mutex::new(PlaybackProgress::default())),
            snapshot_cache: Arc::new(Mutex::new(None)),
            device_registry: Arc::new(Mutex::new(DeviceRegistry::default())),
            output_profiles: Arc::new(Mutex::new(OutputProfileStore::load())),
        }
    }

    pub(super) fn should_retry_startup(&self) -> bool {
        let state = self.system_state.lock();
        if !state.playback_wanted || state.pipeline_recovery().is_some() {
            return false;
        }
        drop(state);
        let driver = self.driver_manager.lock().status();
        driver.platform_supported
            && driver.driver_installed
            && driver.driver_ready
            && self.manager.lock().get_state() == sotf_audio::manager::StreamingState::Idle
            && *self.running.lock()
    }

    pub(super) fn spawn_initial_driver_playback(&self) -> std::thread::JoinHandle<()> {
        let startup_intent = self.pipeline_intent_epoch.load(Ordering::Acquire);
        self.spawn_initial_driver_playback_for_intent(startup_intent)
    }

    pub(super) fn spawn_initial_driver_playback_for_intent(
        &self,
        startup_intent: u64,
    ) -> std::thread::JoinHandle<()> {
        let daemon = self.clone();
        std::thread::spawn(move || {
            elevate_daemon_thread_to_audio_work("startup driver playback");
            println!("Auto-starting driver playback...");

            let initial_mutation = daemon.pipeline_mutation.lock();
            if daemon.pipeline_intent_epoch.load(Ordering::Acquire) != startup_intent {
                return;
            }
            if !daemon.should_retry_startup() {
                return;
            }
            daemon.cache_stable_snapshot();
            let output_device = daemon
                .system_state
                .lock()
                .selected_output_device()
                .or_else(configured_output_device);
            println!("   Output device: {:?}", output_device);

            if let Some(ref device) = output_device {
                if let Err(e) = daemon
                    .system_state
                    .lock()
                    .set_desired_output_device(Some(device.clone()))
                {
                    println!("   Ignoring configured output device {:?}: {}", device, e);
                }
            } else {
                println!("   No output device override; playback thread will choose a safe device");
            }

            // Recall the stored DSP profile for the device playback will
            // actually open. When the configured output is missing, the
            // apply path falls back to the default-safe device, and the
            // fallback must play its own chain — never the missing
            // interface's correction. Fresh daemons (no profiles yet) start
            // empty and adopt it as `default` on first profile use.
            // Post-boot enumeration churn makes the 2 s registry cache a
            // liar at cold start; invalidate before resolving anything.
            daemon.device_registry.lock().invalidate();
            let sink = daemon.driver_manager.lock().output_sink_type();
            let mut effective_device = super::misc::effective_output_for_sink(
                super::misc::resolve_effective_output_device(output_device.as_deref()),
                sink.clone(),
                output_device.as_deref(),
            );
            let mut startup_chain = daemon.resolve_startup_chain(
                effective_device.device.as_deref(),
                effective_device.fallback.is_some(),
            );

            // Cold-start at the HAL transport's actual geometry. The timing
            // path below already adopts the HAL sample rate and buffer size;
            // starting capture at a different channel count than coreaudiod
            // advertises would force an immediate stop/start reconfigure.
            let driver_status = daemon.driver_manager.lock().status();
            let (startup_input_channels, mut startup_output_channels) = startup_channel_geometry(
                driver_status.channel_count,
                daemon.startup_profile_output_channels(),
            );
            println!(
                "   Channels: {} in / {} out (HAL reports {}ch)",
                startup_input_channels, startup_output_channels, driver_status.channel_count
            );
            let mut expected_generation = daemon.system_state.lock().generation();
            drop(initial_mutation);

            // USB devices can enumerate late (re-scan after reboot/reinstall,
            // CoreAudio device-id churn). Retry a bounded number of times
            // while the failure looks like a device-availability race instead
            // of latching `restart_daemon` for a device that appears seconds
            // later. The mutation guard is held per attempt only, so IPC
            // mutations can proceed (and supersede this loop) while waiting.
            for attempt in 1..=super::misc::COLD_START_MAX_ATTEMPTS {
                let result = {
                    // Startup is a pipeline mutation just like an IPC
                    // request or a driver-initiated reconfiguration.
                    // Holding this guard for the entire transition
                    // prevents startup from interleaving its
                    // stop/configure/start/commit sequence with either of
                    // those paths.
                    let _mutation = daemon.pipeline_mutation.lock();
                    if daemon.pipeline_intent_epoch.load(Ordering::Acquire) != startup_intent
                        || daemon.system_state.lock().generation() != expected_generation
                    {
                        return;
                    }
                    // Refresh fallback/profile identity only inside the same
                    // transaction that will consume it.
                    daemon.device_registry.lock().invalidate();
                    let next_effective = super::misc::effective_output_for_sink(
                        super::misc::resolve_effective_output_device(output_device.as_deref()),
                        sink.clone(),
                        output_device.as_deref(),
                    );
                    if next_effective.device != effective_device.device {
                        effective_device = next_effective;
                        startup_chain = daemon.resolve_startup_chain(
                            effective_device.device.as_deref(),
                            effective_device.fallback.is_some(),
                        );
                        startup_output_channels = daemon.startup_profile_output_channels();
                    }
                    let result = match &startup_chain {
                        StartupChain::Graph(graph) => daemon
                            .handle_load_plugin_graph_with_channels(
                                graph.clone(),
                                startup_input_channels,
                                startup_output_channels,
                            ),
                        StartupChain::Rack(plugins) => daemon.handle_load_plugins_with_channels(
                            plugins.clone(),
                            startup_input_channels,
                            startup_output_channels,
                        ),
                    };
                    expected_generation = daemon.system_state.lock().generation();
                    result
                };
                if result.success {
                    println!("   Driver playback started successfully");
                    return;
                }
                println!(
                    "   Driver playback attempt {attempt}/{} failed: {:?}",
                    super::misc::COLD_START_MAX_ATTEMPTS,
                    result.error
                );
                if attempt == super::misc::COLD_START_MAX_ATTEMPTS {
                    println!("   Driver playback failed: {:?}", result.error);
                    return;
                }
                if daemon.pipeline_intent_epoch.load(Ordering::Acquire) != startup_intent
                    || daemon.system_state.lock().generation() != expected_generation
                {
                    println!("   Startup superseded by a newer pipeline mutation; standing down");
                    return;
                }
                if !super::misc::startup_error_is_device_availability(result.error.as_deref()) {
                    println!("   Driver playback failed: {:?}", result.error);
                    return;
                }
                // Committing to another attempt: the latch from the previous
                // failure is stale while a retry is pending, so clear it. A
                // terminal failure re-latches on the final attempt.
                {
                    let _mutation = daemon.pipeline_mutation.lock();
                    if daemon.pipeline_intent_epoch.load(Ordering::Acquire) != startup_intent
                        || daemon.system_state.lock().generation() != expected_generation
                    {
                        return;
                    }
                    daemon.system_state.lock().clear_pipeline_recovery();
                }
                println!("   Cleared stale pipeline recovery before startup retry");
                // Wait for the desired device with the guard released. A
                // concurrent IPC mutation changes the generation and aborts
                // this loop on the next check above.
                let selected_device = daemon.system_state.lock().selected_output_device();
                match selected_device {
                    Some(device) => {
                        println!("   Waiting for output device {device:?} to enumerate...");
                        if !super::misc::wait_for_output_device(
                            &device,
                            super::misc::COLD_START_DEVICE_WAIT,
                            &|| {
                                *daemon.running.lock()
                                    && daemon.pipeline_intent_epoch.load(Ordering::Acquire)
                                        == startup_intent
                            },
                        ) {
                            println!(
                                "   Device {device:?} did not enumerate; giving up startup retry"
                            );
                            let _mutation = daemon.pipeline_mutation.lock();
                            if daemon.pipeline_intent_epoch.load(Ordering::Acquire)
                                == startup_intent
                                && daemon.system_state.lock().generation() == expected_generation
                                && *daemon.running.lock()
                            {
                                daemon.system_state.lock().mark_pipeline_recovery(format!(
                                    "Output device '{device}' did not enumerate before startup timed out"
                                ));
                            }
                            return;
                        }
                    }
                    None => {
                        println!("   No desired device; pausing before startup retry...");
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(2);
                        while *daemon.running.lock()
                            && daemon.pipeline_intent_epoch.load(Ordering::Acquire)
                                == startup_intent
                            && std::time::Instant::now() < deadline
                        {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        if !*daemon.running.lock()
                            || daemon.pipeline_intent_epoch.load(Ordering::Acquire)
                                != startup_intent
                        {
                            return;
                        }
                    }
                }
            }
        })
    }

    fn spawn_driver_config_watcher(&self) -> std::thread::JoinHandle<()> {
        let daemon = self.clone();
        std::thread::spawn(move || {
            elevate_daemon_thread_to_audio_work("driver config watcher");
            let poll_interval = Duration::from_millis(100);
            let initial_driver_status = daemon.driver_manager.lock().status();
            let initial_generation = daemon.system_state.lock().generation();
            let mut capture_resume = CaptureResumeTracker::new(
                Instant::now(),
                initial_generation,
                initial_driver_status.capture_active,
            );
            log::info!("Driver config watcher thread started");
            let mut config_debouncer = ConfigChangeDebouncer::new();
            let mut volume_sync = VolumeSyncTracker::default();
            let mut device_listener = super::device_listener::OutputDeviceListener::new();
            let mut external_rate_pending_since: Option<Instant> = None;
            let mut previous_playback_progress = None;
            let mut startup_retry: Option<std::thread::JoinHandle<()>> = None;
            let mut last_startup_retry = Instant::now() - Duration::from_secs(2);
            let mut route_reconcile_pending = false;
            let mut last_route_reconcile = Instant::now();
            // A nominal-rate change lands while the device resets its IO
            // cycle; re-read only after the rate has settled briefly.
            const EXTERNAL_RATE_SETTLE: Duration = Duration::from_millis(250);
            loop {
                if !*daemon.running.lock() {
                    break;
                }

                if startup_retry
                    .as_ref()
                    .is_some_and(|worker| worker.is_finished())
                    && let Some(worker) = startup_retry.take()
                {
                    let _ = worker.join();
                }
                if startup_retry.is_none()
                    && last_startup_retry.elapsed() >= Duration::from_secs(2)
                    && daemon.should_retry_startup()
                {
                    last_startup_retry = Instant::now();
                    startup_retry = Some(daemon.spawn_initial_driver_playback());
                }
                let polled_intent = daemon.pipeline_intent_epoch.load(Ordering::Acquire);
                let polled = daemon.driver_manager.lock().poll_config_change();
                if let Some(config) =
                    config_debouncer.observe_intent(Instant::now(), polled, polled_intent)
                {
                    // Driver callbacks are intents handled by the same
                    // serialized control boundary as startup and IPC.
                    let _mutation = daemon.pipeline_mutation.lock();
                    if daemon.pipeline_intent_epoch.load(Ordering::Acquire) == polled_intent {
                        daemon.pipeline_intent_epoch.fetch_add(1, Ordering::AcqRel);
                        handle_driver_config_change(
                            &daemon.driver_manager,
                            &daemon.manager,
                            &daemon.device_registry,
                            config,
                            &daemon.system_state,
                            &daemon.running,
                            &daemon.output_profiles,
                        );
                    }
                }

                let driver_status = daemon.driver_manager.lock().status();
                let generation = daemon.system_state.lock().generation();
                if capture_resume.observe(Instant::now(), generation, driver_status.capture_active)
                {
                    // A long-idle CoreAudio stream can keep accepting callback
                    // buffers while emitting silence. Rebuild the already
                    // applied plan once when HAL capture resumes. The
                    // generation recheck under the transition lock prevents a
                    // concurrent explicit mutation from being replayed.
                    let _mutation = daemon.pipeline_mutation.lock();
                    let current_driver_status = daemon.driver_manager.lock().status();
                    let current_generation = daemon.system_state.lock().generation();
                    if current_generation == generation
                        && current_driver_status.capture_active
                        && daemon.manager.lock().get_state()
                            == sotf_audio::manager::StreamingState::Playing
                    {
                        let fallback_input_channels =
                            usize::try_from(current_driver_status.channel_count)
                                .ok()
                                .filter(|channels| *channels > 0)
                                .unwrap_or(2);
                        let plan = {
                            let state = daemon.system_state.lock();
                            state.applied_spec().and_then(|spec| {
                                state.prepare_from_spec(spec, fallback_input_channels).ok()
                            })
                        };
                        if let Some(plan) = plan {
                            let (sample_rate, buffer_frames) =
                                daemon.processing_timing(&current_driver_status);
                            log::info!(
                                "HAL capture resumed after long idle; rebuilding physical playback stream"
                            );
                            let response = daemon.apply_pipeline_plan_with_driver_config(
                                plan,
                                current_driver_status,
                                sample_rate,
                                buffer_frames,
                                PipelineApplyIntent::RebuildOutput,
                            );
                            if response.success {
                                log::info!("Long-idle physical playback rebuild succeeded");
                            } else {
                                log::error!(
                                    "Long-idle physical playback rebuild failed: {}",
                                    response.error.as_deref().unwrap_or("unknown error")
                                );
                            }
                        }
                    }
                }

                // Physical-device property events (macOS CoreAudio
                // listeners): sample-rate changes made in Audio MIDI Setup
                // or another host, plus device/default churn. Device
                // churn only invalidates cached enumeration; a settled
                // nominal-rate change on the applied output is treated
                // like a HAL config request so it gets the same
                // negotiation, acknowledgement, and rollback handling.
                let applied_device = daemon.system_state.lock().applied_output_device();
                device_listener.watch_output_device(applied_device.as_deref());
                for event in device_listener.drain_events() {
                    match event {
                        super::device_listener::DevicePropertyEvent::NominalRate => {
                            daemon.device_registry.lock().invalidate();
                            external_rate_pending_since = Some(Instant::now());
                        }
                        super::device_listener::DevicePropertyEvent::DeviceList
                        | super::device_listener::DevicePropertyEvent::DefaultOutput => {
                            daemon.device_registry.lock().invalidate();
                            device_listener.invalidate_watch();
                            route_reconcile_pending = true;
                        }
                    }
                }
                if external_rate_pending_since
                    .is_some_and(|since| since.elapsed() >= EXTERNAL_RATE_SETTLE)
                {
                    external_rate_pending_since = None;
                    let _mutation = daemon.pipeline_mutation.lock();
                    let driver_status = daemon.driver_manager.lock().status();
                    let applied_device = daemon.system_state.lock().applied_output_device();
                    let request = applied_device.as_deref().and_then(|name| {
                        super::device_listener::external_rate_request(
                            &driver_status,
                            device_listener.nominal_sample_rate(name),
                        )
                    });
                    if let Some(config) = request {
                        log::info!(
                            "Output device nominal sample rate changed externally; reconfiguring to {} Hz",
                            config.sample_rate
                        );
                        handle_driver_config_change(
                            &daemon.driver_manager,
                            &daemon.manager,
                            &daemon.device_registry,
                            config,
                            &daemon.system_state,
                            &daemon.running,
                            &daemon.output_profiles,
                        );
                    }
                }

                if (route_reconcile_pending
                    || last_route_reconcile.elapsed() >= Duration::from_secs(1))
                    && let Some(_mutation) = daemon.pipeline_mutation.try_lock()
                {
                    route_reconcile_pending = false;
                    last_route_reconcile = Instant::now();
                    daemon.reconcile_output_route();
                }
                daemon.reconcile_readiness(&mut previous_playback_progress);

                // Driver-published volume (HAL media keys/sliders) follows
                // the engine master volume. Lock order: driver_manager is
                // released before manager is acquired.
                let published_volume = daemon.driver_manager.lock().output_volume();
                if let Some(_mutation) = daemon.pipeline_mutation.try_lock()
                    && let Some((volume, muted, generation)) = volume_sync.observe(published_volume)
                {
                    let manager = daemon.manager.lock();
                    match manager
                        .set_volume(volume)
                        .and_then(|()| manager.set_mute(muted))
                    {
                        Ok(()) => volume_sync.committed(generation),
                        Err(error) => log::warn!("Driver volume/mute remains pending: {error}"),
                    }
                }

                std::thread::sleep(poll_interval);
            }
            if let Some(worker) = startup_retry {
                let _ = worker.join();
            }
            log::info!("Driver config watcher thread stopped");
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn handle_command(&self, cmd: Command) -> Response {
        self.handle_command_at_generation(cmd, None)
    }

    pub(super) fn handle_command_at_generation(
        &self,
        cmd: Command,
        base_generation: Option<u64>,
    ) -> Response {
        // Volume publication must not observe the temporary teardown mute.
        // Unlike route/chain intent, a gain change does not supersede startup.
        let _volume_mutation =
            matches!(cmd, Command::SetVolume { .. }).then(|| self.pipeline_mutation.lock());
        let _mutation = cmd
            .requires_pipeline_serialization()
            .then(|| self.pipeline_mutation.lock());
        if cmd.accepts_pipeline_base_generation()
            && let Some(base_generation) = base_generation
        {
            let current_generation = self.system_state.lock().generation();
            if base_generation != current_generation {
                return Response::err(format!(
                    "Pipeline generation conflict: intent was based on generation {base_generation}, current generation is {current_generation}. Refresh and retry."
                ));
            }
        }
        if cmd.requires_pipeline_serialization() && !matches!(cmd, Command::GetOutputProfiles) {
            self.pipeline_intent_epoch.fetch_add(1, Ordering::AcqRel);
        }

        match cmd {
            // Lifecycle probes must remain independent of engine, pipeline,
            // driver, and key-manager locks so legitimate reconfiguration
            // cannot be mistaken for a dead daemon.
            Command::Ping => Response::ok_empty(),
            Command::Status => self.handle_status(),
            Command::GetSnapshot => self.handle_get_snapshot(),
            Command::DumpState => self.handle_dump_state(),
            Command::Load { path } => self.handle_load(&path),
            Command::Play => self.handle_play(),
            Command::Pause => self.handle_pause(),
            Command::Stop => self.handle_stop(),
            Command::Seek { position } => self.handle_seek(position),
            Command::SetVolume { volume } => self.handle_set_volume(volume),
            Command::ListDevices => self.handle_list_devices(),
            Command::SetDevice { device } => self.handle_set_device(&device),
            Command::ApplyConfiguration {
                sample_rate,
                buffer_frames,
                input_channels,
                output_channels,
                output_device,
            } => self.handle_apply_configuration(
                sample_rate,
                buffer_frames,
                input_channels,
                output_channels,
                output_device,
            ),
            Command::LoadPlugins {
                plugins,
                input_channels,
                output_channels,
            } => self.handle_load_plugins_with_channels(plugins, input_channels, output_channels),
            Command::LoadPluginArtifact {
                artifact,
                base_generation,
            } => self.handle_load_plugin_artifact(artifact, base_generation),
            Command::LoadPluginArtifactPath {
                path,
                base_generation,
            } => self.handle_load_plugin_artifact_path(&path, base_generation),
            Command::ReorderGraph {
                order,
                base_generation,
            } => self.handle_reorder_graph(order, base_generation),
            Command::SetInputChannels { channels } => {
                self.handle_set_pipeline_channels(Some(channels), None)
            }
            Command::SetOutputChannels { channels } => {
                self.handle_set_pipeline_channels(None, Some(channels))
            }
            Command::SetPipelineChannels {
                input_channels,
                output_channels,
            } => self.handle_set_pipeline_channels(input_channels, output_channels),
            Command::GetLoudness => self.handle_get_loudness(),
            Command::GetMetering => self.handle_get_metering(),
            Command::GetPlugins => self.handle_get_plugins(),
            Command::GetAvailablePlugins => self.handle_get_available_plugins(),
            Command::AddPlugin { plugin, index } => self.handle_add_plugin(plugin, index),
            Command::RemovePlugin { index } => self.handle_remove_plugin(index),
            Command::UpdatePlugin { index, parameters } => {
                self.handle_update_plugin(index, parameters)
            }
            Command::ReorderPlugins { order } => self.handle_reorder_plugins(order),
            Command::SetRackPluginState {
                index,
                input_channels,
                bypassed,
                base_generation,
            } => {
                self.handle_set_rack_plugin_state(index, input_channels, bypassed, base_generation)
            }
            Command::GetOutputProfiles => self.handle_get_output_profiles(),
            Command::SetOutputProfile { profile } => self.handle_set_output_profile(profile),
            Command::DeleteOutputProfile { profile_id } => {
                self.handle_delete_output_profile(&profile_id)
            }
            Command::AssignOutputProfile {
                device_uid,
                device_name,
                profile_id,
            } => {
                self.handle_assign_output_profile(device_uid.as_deref(), &device_name, &profile_id)
            }
            Command::SetOutputRoute {
                output_device,
                output_device_uid,
                profile_id,
                input_channels,
                output_channels,
                base_generation,
            } => self.handle_set_output_route(
                &output_device,
                output_device_uid.as_deref(),
                profile_id.as_deref(),
                input_channels,
                output_channels,
                base_generation,
            ),
            Command::DriverStatus => self.handle_driver_status(),
            Command::Shutdown => {
                *self.running.lock() = false;
                Response::ok_empty()
            }
            // Encryption commands
            Command::SetEncryption { enabled } => self.handle_set_encryption(enabled),
            Command::EncryptionStatus => self.handle_encryption_status(),
            Command::RotateEncryptionKey => self.handle_rotate_encryption_key(),
            // Driver config commands
            Command::SetSampleRate { rate } => self.handle_set_sample_rate(rate),
            Command::SetBufferFrames { frames } => self.handle_set_buffer_frames(frames),
            Command::GetDriverConfig => self.handle_get_driver_config(),
        }
    }

    pub(super) fn metering_snapshot(&self) -> Value {
        let (input_idx, output_idx, fallback_input_channels) = {
            let pipeline = self.system_state.lock();
            (
                pipeline.input_loudness_index(),
                pipeline.output_loudness_index(),
                pipeline.input_channels(),
            )
        };

        // Snapshot the manager-owned values only after releasing the daemon
        // state lock. This keeps the lock order one-way and prevents the UI's
        // polling path from extending a cross-component lock hold while it
        // clones analyzer payloads.
        let fallback_output_channels = {
            let manager = self.manager.lock();
            manager.get_engine_state().num_channels
        };

        // `get_cached_plugin_data` returns an Arc-backed snapshot from the
        // engine's lock-free cache. Clone that Arc while the manager mutex is
        // held, then downcast/clone the analyzer payload after releasing the
        // mutex. Meter polling must never hold the daemon's manager lock while
        // copying the (potentially large) loudness vectors.
        let snapshot_loudness = |index: Option<usize>| {
            let data = index.and_then(|idx| {
                let manager = self.manager.lock();
                manager.get_cached_plugin_data(idx)
            });
            data.and_then(|data| data.downcast_ref::<sotf_audio::LoudnessData>().cloned())
        };
        let input_data = snapshot_loudness(input_idx);
        let output_data = snapshot_loudness(output_idx);

        let input_json = input_data
            .as_ref()
            .map(loudness_data_to_json)
            .unwrap_or_else(|| empty_loudness_json(fallback_input_channels));
        let output_json = output_data
            .as_ref()
            .map(loudness_data_to_json)
            .unwrap_or_else(|| empty_loudness_json(fallback_output_channels));

        serde_json::json!({
            "input": input_json,
            "output": output_json,
            "sources": {
                "input": metering_source_json(input_data.is_some(), fallback_input_channels),
                "output": metering_source_json(output_data.is_some(), fallback_output_channels),
            },
        })
    }

    pub(super) fn snapshot_json(&self) -> Value {
        if let Some(_mutation) = self.pipeline_mutation.try_lock() {
            let mut snapshot = self.snapshot_json_while_pipeline_stable();
            snapshot["observation"] =
                serde_json::json!({"transition_in_progress":false,"age_ms":0,"available":true});
            *self.snapshot_cache.lock() = Some((Instant::now(), snapshot.clone()));
            return snapshot;
        }
        // Never block status behind a multi-second readiness wait. Return one
        // complete prior observation, clearly labeled with its age.
        let cached = self.snapshot_cache.lock();
        let mut snapshot = cached
            .as_ref()
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| serde_json::json!({"schema_version":1}));
        snapshot["observation"] = serde_json::json!({"transition_in_progress":true,
            "age_ms":cached.as_ref().map(|(at,_)|at.elapsed().as_millis()),"available":cached.is_some()});
        snapshot
    }

    fn cache_stable_snapshot(&self) {
        *self.snapshot_cache.lock() =
            Some((Instant::now(), self.snapshot_json_while_pipeline_stable()));
    }

    fn snapshot_json_while_pipeline_stable(&self) -> Value {
        let driver_status = self.driver_manager.lock().status();
        let key_status = self.key_manager.lock().status();

        let manager = self.manager.lock();
        let state = manager.get_state();
        let state_name = format!("{:?}", state);
        let engine_state = manager.get_engine_state();
        let volume = manager.get_volume();
        let muted = manager.is_muted();
        drop(manager);

        let pipeline = self.system_state.lock();
        let desired = pipeline.desired_spec();
        let applied = pipeline.applied_spec();
        let applied_generation = pipeline.applied_generation();
        let generation = pipeline.generation();
        let applied_output_device = pipeline.applied_output_device();
        let pipeline_recovery = pipeline.pipeline_recovery();
        drop(pipeline);

        // Current output route (device UID + profile) for per-output DSP
        // clients. Sequenced after the pipeline guard drops (leaf lock).
        let (route_profile_id, route_device_uid, route_device_name) =
            self.output_profiles.lock().current();

        let (mut transport, mut faults) =
            transport_snapshot_and_faults(&state_name, &driver_status, &engine_state);
        let progress = self.playback_progress.lock().snapshot(
            Instant::now(),
            generation,
            engine_state.playback_epoch,
        );
        if state_name == "Playing" {
            if transport["input_status"] == "flowing" && progress["input"] != "fresh" {
                let status = if progress["input"] == "stale" {
                    "input_progress_stale"
                } else {
                    progress["input"].as_str().unwrap_or("awaiting_progress")
                };
                transport["input_status"] = serde_json::json!(status);
                transport["input"]["status"] = serde_json::json!(status);
                if status == "input_progress_stale" {
                    faults.push(serde_json::json!({"code":status,"severity":"error",
                        "message":"Capture is active but no new frames have reached playback within the progress window."}));
                }
            }
            if transport["output_status"] == "flowing" {
                let stage = if progress["callbacks"] != "fresh" {
                    "callbacks"
                } else if driver_status.capture_active {
                    "output"
                } else {
                    "callbacks"
                };
                if progress[stage] != "fresh" {
                    let status = if progress[stage] == "stale" {
                        "output_progress_stale"
                    } else {
                        progress[stage].as_str().unwrap_or("awaiting_progress")
                    };
                    transport["output_status"] = serde_json::json!(status);
                    transport["output"]["status"] = serde_json::json!(status);
                    if status == "output_progress_stale" {
                        faults.push(serde_json::json!({"code":status,"severity":"error",
                            "message":"Output callbacks or written frames stopped advancing within the progress window.","stage":stage}));
                    }
                }
            }
            if progress["callbacks"] == "observation_delayed" {
                faults.push(serde_json::json!({"code":"progress_observation_delayed","severity":"warning",
                    "message":"The control owner has not sampled current playback progress within its observation window."}));
            }
        }
        transport["progress"] = progress;
        let (processing_rate, processing_frames) = self.processing_timing(&driver_status);
        transport["processing"] =
            serde_json::json!({"sample_rate":processing_rate,"buffer_frames":processing_frames});

        if desired
            .output_device
            .as_ref()
            .is_some_and(|device| !is_safe_output_device_name(device))
        {
            faults.push(serde_json::json!({
                "code": "unsafe_desired_output_device",
                "severity": "error",
                "message": "Desired output device is virtual/loopback and would create a feedback risk.",
            }));
        }
        if engine_state
            .playback_output_device
            .as_ref()
            .is_some_and(|device| !is_safe_output_device_name(device))
        {
            faults.push(serde_json::json!({
                "code": "unsafe_observed_output_device",
                "severity": "error",
                "message": "Observed playback output device is virtual/loopback and risks feedback.",
            }));
        }
        if let Some(recovery) = &pipeline_recovery {
            faults.push(serde_json::json!({
                "code": "pipeline_recovery_required",
                "severity": "error",
                "message": recovery.error,
                "actions": recovery.actions,
            }));
        }
        // Desired/applied divergence while streaming means fallback is
        // active: the remembered device is gone and playback moved to the
        // default-safe output. Warning, not error — audio is flowing. Gated
        // on streaming so an idle engine with a stale applied spec (stop
        // preserves it) does not report a fallback that is not playing.
        if let (Some(desired_device), Some(applied_device)) = (
            desired.output_device.as_ref(),
            applied_output_device.as_ref(),
        ) && desired_device != applied_device
            && matches!(
                state,
                sotf_audio::manager::StreamingState::Playing
                    | sotf_audio::manager::StreamingState::Paused
            )
        {
            faults.push(serde_json::json!({
                "code": "output_device_fallback",
                "severity": "warning",
                "message": format!(
                    "Desired output device '{desired_device}' is unavailable; playing through '{applied_device}'."
                ),
                "desired_output_device": desired_device,
                "applied_output_device": applied_device,
                "actions": ["select_output_device"],
            }));
        }

        let metering = self.metering_snapshot();
        push_metering_faults(&state_name, &metering, &mut faults);
        let health = if faults
            .iter()
            .any(|fault| fault["severity"].as_str() == Some("error"))
        {
            "fault"
        } else if faults.is_empty() {
            "ok"
        } else {
            "warning"
        };

        serde_json::json!({
            "schema_version": 1,
            "generation": generation,
            "desired": pipeline_spec_to_json(&desired),
            "applied": {
                "generation": applied_generation,
                "output_device": applied_output_device,
                "output_device_uid": route_device_uid,
                "output_device_name": route_device_name,
                "profile_id": route_profile_id,
                "spec": applied.as_ref().map(pipeline_spec_to_json),
            },
            "observed": {
                "engine": {
                    "state": state_name,
                    "volume": volume,
                    "muted": muted,
                    "sample_rate": engine_state.sample_rate,
                    "channels": engine_state.num_channels,
                    "underruns": engine_state.underruns,
                    "playback_output_device": engine_state.playback_output_device,
                    "playback_callback_count": engine_state.playback_callback_count,
                    "playback_buffer_fill_percent": engine_state.playback_buffer_fill_percent,
                    "playback_stream_error_count": engine_state.playback_stream_error_count,
                    "playback_frames_received": engine_state.playback_frames_received,
                    "playback_frames_written": engine_state.playback_frames_written,
                    "playback_frames_dropped": engine_state.playback_frames_dropped,
                    "playback_effective_sample_rate": engine_state.playback_effective_sample_rate,
                    "last_error": engine_state.last_error,
                },
                "driver": {
                    "platform_supported": driver_status.platform_supported,
                    "driver_installed": driver_status.driver_installed,
                    "driver_ready": driver_status.driver_ready,
                    "capture_active": driver_status.capture_active,
                    "sample_rate": driver_status.sample_rate,
                    "channel_count": driver_status.channel_count,
                    "buffer_frames": driver_status.buffer_frames,
                    "driver_name": driver_status.driver_name,
                },
                "encryption": {
                    "enabled": key_status.enabled,
                    "fingerprint": key_status.fingerprint,
                    "key_path": key_status.key_path,
                    // Encrypted records dropped on ring overflow; non-zero
                    // means the consumer fell behind (the 96 kHz scratch
                    // signature) and is otherwise invisible in status.
                    "overflow_count": driver_status.encryption_overflow_count,
                },
                "transport": transport,
                "metering": metering,
            },
            "diagnostics": {
                "health": health,
                "faults": faults,
                "pipeline_recovery": pipeline_recovery.as_ref().map(|recovery| {
                    serde_json::json!({
                        "error": recovery.error,
                        "actions": recovery.actions,
                    })
                }),
            },
        })
    }

    pub(super) fn handle_get_snapshot(&self) -> Response {
        Response::ok(self.snapshot_json())
    }

    pub(super) fn handle_dump_state(&self) -> Response {
        let _mutation = self.pipeline_mutation.lock();
        let state = self.system_state.lock();
        let user_graph = state.user_graph();
        let user_plugins = state.user_plugins();
        drop(state);
        Response::ok(serde_json::json!({
            "snapshot": self.snapshot_json_while_pipeline_stable(),
            "topology": if user_graph.is_some() { "graph" } else { "rack" },
            "plugins": user_plugins,
            "graph": user_graph,
            "runtime_telemetry": self.runtime_telemetry.snapshot(),
        }))
    }

    pub(super) fn handle_status(&self) -> Response {
        let (state, engine_state, volume, muted) = {
            let manager = self.manager.lock();
            (
                manager.get_state(),
                manager.get_engine_state(),
                manager.get_volume(),
                manager.is_muted(),
            )
        };
        let (
            selected_device,
            input_channels,
            output_channels,
            pipeline_generation,
            pipeline_applied_output_device,
            pipeline_recovery,
        ) = {
            let pipeline = self.system_state.lock();
            (
                pipeline.selected_output_device(),
                pipeline.input_channels(),
                pipeline.output_channels(),
                pipeline.applied_generation(),
                pipeline.applied_output_device(),
                pipeline.pipeline_recovery(),
            )
        };
        let driver_status = self.driver_manager.lock().status();
        let key_status = self.key_manager.lock().status();

        let mut recovery_actions = Vec::<String>::new();
        if !driver_status.platform_supported {
            recovery_actions.push("driver_not_supported".to_string());
        }
        if !driver_status.driver_installed {
            recovery_actions.push("reinstall_driver".to_string());
        }
        if driver_status.driver_installed && !driver_status.driver_ready {
            recovery_actions.push("restart_daemon".to_string());
        }
        // A latched device-availability fault with nothing playing means the
        // desired output is gone (powered off, unplugged): the operator must
        // pick a reachable device, not just restart the daemon.
        if pipeline_applied_output_device.is_none()
            && engine_state.playback_output_device.is_none()
            && (selected_device.is_none()
                || pipeline_recovery.as_ref().is_some_and(|recovery| {
                    super::misc::startup_error_is_device_availability(Some(&recovery.error))
                }))
        {
            recovery_actions.push("select_output_device".to_string());
        }
        if key_status.enabled && key_status.fingerprint.len() < 16 {
            recovery_actions.push("rotate_encryption_key".to_string());
        }
        if let Some(recovery) = &pipeline_recovery {
            for action in &recovery.actions {
                if !recovery_actions.iter().any(|existing| existing == action) {
                    recovery_actions.push(action.clone());
                }
            }
        }

        Response::ok(serde_json::json!({
            "consistency": "best_effort",
            "state": format!("{:?}", state),
            "volume": volume,
            "muted": muted,
            "selected_device": selected_device,
            "pipeline_generation": pipeline_generation,
            "pipeline_applied_output_device": pipeline_applied_output_device,
            "sample_rate": engine_state.sample_rate,
            "input_channels": input_channels,
            "output_channels": output_channels,
            "channels": engine_state.num_channels,
            "underruns": engine_state.underruns,
            "playback_output_device": engine_state.playback_output_device,
            "playback_callback_count": engine_state.playback_callback_count,
            "playback_buffer_fill_percent": engine_state.playback_buffer_fill_percent,
            "playback_stream_error_count": engine_state.playback_stream_error_count,
            "playback_frames_received": engine_state.playback_frames_received,
            "playback_frames_written": engine_state.playback_frames_written,
            "playback_frames_dropped": engine_state.playback_frames_dropped,
            "playback_effective_sample_rate": engine_state.playback_effective_sample_rate,
            "last_error": engine_state.last_error,
            "driver": {
                "installed": driver_status.driver_installed,
                "ready": driver_status.driver_ready,
                "capture_active": driver_status.capture_active,
                "frame_size": driver_status.buffer_frames,
                "sample_rate": driver_status.sample_rate,
                "channel_count": driver_status.channel_count,
            },
            "encryption": {
                "enabled": key_status.enabled,
                "fingerprint": key_status.fingerprint,
            },
            "active_route": {
                "desired_output_device": selected_device,
                "applied_output_device": pipeline_applied_output_device,
                "playback_output_device": engine_state.playback_output_device,
                "capture_active": driver_status.capture_active,
            },
            "pipeline_recovery": pipeline_recovery.as_ref().map(|recovery| {
                serde_json::json!({
                    "error": recovery.error,
                    "actions": recovery.actions,
                })
            }),
            "recovery_actions": recovery_actions,
        }))
    }

    pub(super) fn handle_load(&self, path: &str) -> Response {
        let path = match validate_user_load_path(std::path::Path::new(path)) {
            Ok(path) => path,
            Err(error) => {
                return Response::err(format!("Refusing to load audio file: {error}"));
            }
        };

        let mut manager = self.manager.lock();
        match manager.load_file(&path) {
            Ok(_) => Response::ok_empty(),
            Err(e) => Response::err(format!("Failed to load file: {}", e)),
        }
    }

    fn processing_timing(&self, driver: &driver_common::DriverStatus) -> (u32, u32) {
        self.system_state
            .lock()
            .processing_timing
            .unwrap_or_else(|| {
                let rate = if SUPPORTED_SAMPLE_RATES.contains(&driver.sample_rate) {
                    driver.sample_rate
                } else {
                    48_000
                };
                let frames = if (64..=4096).contains(&driver.buffer_frames) {
                    driver.buffer_frames
                } else {
                    512
                };
                (rate, frames)
            })
    }

    pub(super) fn handle_play(&self) -> Response {
        self.system_state.lock().playback_wanted = true;
        let driver_status = self.driver_manager.lock().status();
        let (driver_sample_rate, driver_buffer_frames) = self.processing_timing(&driver_status);
        let fallback_input_channels = if driver_status.channel_count > 0 {
            driver_status.channel_count as usize
        } else {
            2
        };

        let plan = {
            let state = self.system_state.lock();
            state.prepare_from_spec(state.desired_spec(), fallback_input_channels)
        };
        if let Ok(plan) = &plan {
            let applied_matches = self
                .system_state
                .lock()
                .applied_spec()
                .is_some_and(|applied| {
                    let mut resolved = plan.spec.clone();
                    resolved.output_device = self.effective_output_for_plan(plan).device;
                    pipeline_specs_match(&applied, &resolved)
                });
            let manager = self.manager.lock();
            if applied_matches && manager.get_state() == sotf_audio::manager::StreamingState::Paused
            {
                let resumed = manager.resume();
                drop(manager);
                if resumed.is_ok() && self.driver_manager.lock().set_engine_ready(true) {
                    return Response::ok(serde_json::json!({
                        "generation": self.system_state.lock().generation(),
                    }));
                }
                log::warn!("Paused stream could not resume safely; rebuilding playback");
            }
        }
        match plan {
            Ok(plan) => self.apply_pipeline_plan(
                plan,
                driver_status,
                driver_sample_rate,
                driver_buffer_frames,
            ),
            Err(error) => Response::err(format!("Failed to prepare playback pipeline: {error}")),
        }
    }

    pub(super) fn handle_pause(&self) -> Response {
        self.system_state.lock().playback_wanted = false;
        let manager = self.manager.lock();
        match manager.pause() {
            Ok(_) => Response::ok_empty(),
            Err(e) => Response::err(format!("Failed to pause: {}", e)),
        }
    }

    pub(super) fn handle_stop(&self) -> Response {
        self.system_state.lock().playback_wanted = false;
        // Lock-order invariant: driver_manager -> manager. The config
        // watcher thread also acquires them in this order. Using the
        // `lock_order::lock_with_order_warning` helper turns silent
        // contention with the watcher into a logged warning so a future
        // contributor who introduces an inverse acquisition order has a
        // diagnostic to follow instead of an undetectable deadlock.
        super::lock_order::lock_with_order_warning(&self.driver_manager, "driver_manager")
            .set_engine_ready(false);
        log::debug!("Cleared engine_ready flag via driver");

        let mut manager = super::lock_order::lock_with_order_warning(&self.manager, "manager");
        match manager.stop() {
            Ok(_) => Response::ok_empty(),
            Err(e) => Response::err(format!("Failed to stop: {}", e)),
        }
    }

    pub(super) fn handle_seek(&self, position: f64) -> Response {
        let manager = self.manager.lock();
        match manager.seek(position) {
            Ok(_) => Response::ok_empty(),
            Err(e) => Response::err(format!("Failed to seek: {}", e)),
        }
    }

    pub(super) fn handle_set_volume(&self, volume: f32) -> Response {
        let muted = {
            let manager = self.manager.lock();
            match manager.set_volume(volume) {
                Ok(()) => manager.is_muted(),
                Err(error) => {
                    return Response::err(format!("Failed to set volume: {error}"));
                }
            }
        };
        // Keep the driver's OS-visible volume control (media-key OSD,
        // sliders) in sync with IPC-originated changes. The watcher sees
        // the new generation and re-applies the identical values, which is
        // a no-op on the manager. Lock order: manager released above
        // before driver_manager is acquired.
        self.driver_manager.lock().set_output_volume(volume, muted);
        Response::ok_empty()
    }

    pub(super) fn handle_list_devices(&self) -> Response {
        if self.driver_manager.lock().is_lab_backend() {
            let sample_rate = self.driver_manager.lock().status().sample_rate;
            return Response::ok(serde_json::json!({
                "generation": 1,
                "devices": [{
                    "name": "Systemwide Lab Output",
                    "is_default": true,
                    "channels": 16,
                    "sample_rate": sample_rate,
                    "backend": "lab_null",
                }],
            }));
        }
        match self.device_registry.lock().list_devices() {
            Ok((generation, devices)) => Response::ok(serde_json::json!({
                "generation": generation,
                "devices": devices,
            })),
            Err(e) => Response::err(format!("Failed to list devices: {}", e)),
        }
    }

    fn resolve_safe_output_device(&self, device: &str) -> Result<String, String> {
        if self.driver_manager.lock().output_sink_type() == SinkType::LabNull {
            return (device == "Systemwide Lab Output")
                .then(|| device.to_string())
                .ok_or_else(|| "Lab backend cannot select a physical output device".to_string());
        }
        use cpal::traits::DeviceTrait;

        let is_asio = sotf_audio::devices::is_asio_device(device);
        let host = sotf_audio::devices::get_host_for_device(Some(device));
        let device_name = sotf_audio::devices::strip_asio_prefix(device);
        let cpal_device = sotf_audio::devices::find_device(&host, device_name, false)
            .map_err(|error| format!("Device '{device}' not found. {error}"))?;
        let resolved_name = cpal_device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| "Unknown Device".to_string());

        if !is_safe_output_device_name(&resolved_name) {
            return Err(format!(
                "'{resolved_name}' is a virtual/loopback device and cannot be used as the Systemwide speaker output. Select hardware speakers/headphones here, and select SotF Virtual Audio in macOS Sound Output."
            ));
        }

        Ok(if is_asio {
            format!(
                "{}{}",
                sotf_audio::devices::ASIO_DEVICE_PREFIX,
                resolved_name
            )
        } else {
            resolved_name
        })
    }

    pub(super) fn handle_set_device(&self, device: &str) -> Response {
        if self.driver_manager.lock().output_sink_type() == SinkType::LabNull {
            return self.handle_apply_configuration(
                None,
                None,
                None,
                None,
                Some(device.to_string()),
            );
        }
        use cpal::traits::DeviceTrait;
        let is_asio = sotf_audio::devices::is_asio_device(device);
        let host = sotf_audio::devices::get_host_for_device(Some(device));
        let device_name = sotf_audio::devices::strip_asio_prefix(device);

        match sotf_audio::devices::find_device(&host, device_name, false) {
            Ok(cpal_device) => {
                let resolved_name = cpal_device
                    .description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "Unknown Device".to_string());

                if !is_safe_output_device_name(&resolved_name) {
                    log::warn!(
                        "Rejected virtual output device '{}' (requested '{}') to prevent feedback",
                        resolved_name,
                        device
                    );
                    return Response::err(format!(
                        "'{}' is a virtual/loopback device and cannot be used as Systemwide speaker output. Select hardware speakers/headphones here, and select SotF Virtual Audio in macOS Sound Output.",
                        resolved_name
                    ));
                }

                // Store with ASIO prefix preserved so playback thread selects the right host
                let stored_name = if is_asio {
                    format!(
                        "{}{}",
                        sotf_audio::devices::ASIO_DEVICE_PREFIX,
                        resolved_name
                    )
                } else {
                    resolved_name.clone()
                };
                log::info!(
                    "Output device set to: {} (matched from '{}')",
                    resolved_name,
                    device
                );

                let driver_status = self.driver_manager.lock().status();
                let (driver_sample_rate, driver_buffer_frames) =
                    self.processing_timing(&driver_status);
                // Every route change carries its output's DSP profile so a
                // device never inherits another interface's chain. On stores
                // without profiles this adopts (and applies) the live chain.
                let profile = match self.resolve_route_profile(None, &resolved_name, None) {
                    Ok(profile) => profile,
                    Err(error) => return Response::err(error),
                };
                let plan = {
                    let state = self.system_state.lock();
                    let mut next = state.desired_spec();
                    next.output_device = Some(stored_name.clone());
                    next.input_channels = profile.input_channels;
                    next.output_channels = profile.output_channels;
                    next.user_plugins = profile.plugins.clone();
                    next.user_graph = profile.graph.clone();
                    match state.prepare_from_spec(next, state.input_channels()) {
                        Ok(plan) => plan,
                        Err(error) => return Response::err(error),
                    }
                };

                log::info!(
                    "Starting/restarting driver playback with output device: {}",
                    resolved_name
                );
                let resp = self.apply_pipeline_transaction(
                    plan,
                    driver_status,
                    driver_sample_rate,
                    driver_buffer_frames,
                    PipelineApplyIntent::SelectRoute,
                );
                if !resp.success {
                    return resp;
                }

                if let Err(error) = persist_output_device(&stored_name) {
                    log::error!(
                        "Output device '{}' is active but could not be persisted for the next daemon start: {}",
                        stored_name,
                        error
                    );
                }
                if let Err(error) = self.output_profiles.lock().record_route_if_active(
                    None,
                    &resolved_name,
                    &profile.id,
                    self.system_state.lock().applied_output_device().as_deref(),
                ) {
                    log::warn!("Output device applied but profile memory failed: {error}");
                }
                self.sync_live_chain_to_current_profile(true);

                Response::ok_empty()
            }
            Err(e) => {
                self.device_registry.lock().invalidate();
                log::warn!("Failed to set device '{}': {}", device, e);
                Response::err(format!("Device '{}' not found. {}", device, e))
            }
        }
    }

    pub(super) fn apply_pipeline_plan(
        &self,
        plan: PipelinePlan,
        driver_status: driver_common::DriverStatus,
        driver_sample_rate: u32,
        driver_buffer_frames: u32,
    ) -> Response {
        self.apply_pipeline_plan_with_driver_config(
            plan,
            driver_status,
            driver_sample_rate,
            driver_buffer_frames,
            PipelineApplyIntent::EnsurePlaying,
        )
    }

    fn apply_pipeline_plan_with_driver_config(
        &self,
        plan: PipelinePlan,
        driver_status: driver_common::DriverStatus,
        driver_sample_rate: u32,
        driver_buffer_frames: u32,
        intent: PipelineApplyIntent,
    ) -> Response {
        let before = self.system_state.lock().applied_spec();
        let response = self.apply_pipeline_transaction(
            plan,
            driver_status,
            driver_sample_rate,
            driver_buffer_frames,
            intent,
        );
        let after = self.system_state.lock().applied_spec();
        if response.success
            && before
                .as_ref()
                .zip(after.as_ref())
                .is_none_or(|(before, after)| {
                    !super::pipeline_spec::pipeline_dsp_matches(before, after)
                })
        {
            self.sync_live_chain_to_current_profile(true);
        }
        response
    }

    // Applying and rolling back must not persist a chain under a profile
    // identity owned by the caller's still-pending route transaction.
    fn apply_pipeline_transaction(
        &self,
        plan: PipelinePlan,
        driver_status: driver_common::DriverStatus,
        driver_sample_rate: u32,
        driver_buffer_frames: u32,
        intent: PipelineApplyIntent,
    ) -> Response {
        if !driver_status.platform_supported {
            return Response::err(
                "The selected capture backend is status-only; select a supported driver before starting playback",
            );
        }
        if !driver_status.driver_installed || !driver_status.driver_ready {
            return Response::err("Capture driver is not ready; playback remains pending");
        }
        if intent == PipelineApplyIntent::EnsurePlaying
            && self.processing_timing(&driver_status) == (driver_sample_rate, driver_buffer_frames)
            && (self.driver_manager.lock().capture_source().is_some()
                || (driver_status.sample_rate, driver_status.buffer_frames)
                    == (driver_sample_rate, driver_buffer_frames))
            && self.pipeline_plan_already_applied(&plan)
        {
            return Response::ok(serde_json::json!({
                "generation": self.system_state.lock().generation(),
            }));
        }
        self.cache_stable_snapshot();
        let _mute_guard = match ReconfigurationMuteGuard::begin(&self.manager) {
            Ok(guard) => guard,
            Err(error) => return Response::err(error),
        };
        let (restore_driver_sample_rate, restore_driver_buffer_frames) =
            self.processing_timing(&driver_status);
        let fallback_input_channels = if driver_status.channel_count > 0 {
            driver_status.channel_count as usize
        } else {
            2
        };
        let preferred_output = self.system_state.lock().selected_output_device();
        let previous_profile = self.output_profiles.lock().current();
        let previous_plan = {
            let state = self.system_state.lock();
            state
                .applied_spec()
                .and_then(|spec| state.prepare_from_spec(spec, fallback_input_channels).ok())
        };

        let response =
            self.apply_pipeline_plan_once(plan, driver_sample_rate, driver_buffer_frames, intent);
        if response.success {
            self.system_state.lock().clear_pipeline_recovery();
            return response;
        }

        // Shutdown cancellation is terminal for this daemon lifetime. Do not
        // restart the old plan while the accept loop is trying to stop and join
        // workers.
        if !*self.running.lock() {
            return response;
        }

        if previous_plan.is_none() {
            let error = response
                .error
                .clone()
                .unwrap_or_else(|| "pipeline apply failed".to_string());
            self.system_state.lock().mark_pipeline_recovery(error);
            return response;
        }

        let Some(previous_plan) = previous_plan else {
            return response;
        };
        let restore = self.apply_pipeline_plan_once(
            previous_plan,
            restore_driver_sample_rate,
            restore_driver_buffer_frames,
            PipelineApplyIntent::RestoreLastWorking,
        );
        if restore.success {
            let mut state = self.system_state.lock();
            let restored_device = state.applied_output_device();
            state.restore_output_preference(preferred_output);
            state.clear_pipeline_recovery();
            drop(state);
            if restored_device == previous_profile.2 {
                self.output_profiles.lock().set_current_in_memory(
                    previous_profile.1,
                    previous_profile.2,
                    previous_profile.0,
                );
            }
            Response::err(format!(
                "{}; restored the last working pipeline; retry the requested change",
                response
                    .error
                    .unwrap_or_else(|| "pipeline apply failed".to_string())
            ))
        } else {
            let error = format!(
                "{}; pipeline recovery also failed, restart the daemon",
                response
                    .error
                    .unwrap_or_else(|| "pipeline apply failed".to_string())
            );
            self.system_state
                .lock()
                .mark_pipeline_recovery(error.clone());
            Response::err(error)
        }
    }

    /// Whether `plan` describes exactly the pipeline already running on a
    /// healthy engine: same applied spec, HAL transport already at the
    /// plan's input geometry, engine streaming without a recorded error and
    /// with observed callbacks. Anything else (idle, errored, pre-callback,
    /// diverged) needs the cold path below so recovery is never skipped.
    fn pipeline_plan_already_applied(&self, plan: &PipelinePlan) -> bool {
        let applied = {
            let state = self.system_state.lock();
            match state.applied_spec() {
                Some(applied) => applied,
                None => return false,
            }
        };
        let mut resolved_spec = plan.spec.clone();
        resolved_spec.output_device = self.effective_output_for_plan(plan).device;
        if !pipeline_specs_match(&applied, &resolved_spec) {
            return false;
        }
        if self.driver_manager.lock().status().channel_count != plan.spec.input_channels as u32 {
            return false;
        }
        if self.driver_manager.lock().output_sink_type() != SinkType::LabNull
            && resolved_spec.output_device.is_none()
        {
            return false;
        }
        let manager = self.manager.lock();
        if manager.get_state() != sotf_audio::manager::StreamingState::Playing {
            return false;
        }
        if manager.get_playback_state() != sotf_audio::PlaybackState::Playing {
            return false;
        }
        let engine_state = manager.get_engine_state();
        drop(manager);
        if engine_state
            .last_error
            .as_deref()
            .is_some_and(|error| !error.is_empty())
        {
            return false;
        }
        let generation = self.system_state.lock().generation();
        let driver = self.driver_manager.lock().status();
        if !driver.driver_installed
            || !driver.driver_ready
            || !self.driver_manager.lock().readiness_is_current()
        {
            return false;
        }
        if resolved_spec.output_device.is_some()
            && engine_state.playback_output_device != resolved_spec.output_device
        {
            return false;
        }
        let now = Instant::now();
        let mut progress = self.playback_progress.lock();
        progress.observe(now, generation, true, &engine_state);
        let input_ok = matches!(
            progress.status(now, generation, engine_state.playback_epoch, 0),
            "fresh" | "awaiting_progress"
        );
        let written_ok = matches!(
            progress.status(now, generation, engine_state.playback_epoch, 2),
            "fresh" | "awaiting_progress"
        );
        progress.status(now, generation, engine_state.playback_epoch, 1) == "fresh"
            && (!driver.capture_active || (input_ok && written_ok))
    }

    fn apply_pipeline_plan_once(
        &self,
        plan: PipelinePlan,
        driver_sample_rate: u32,
        driver_buffer_frames: u32,
        intent: PipelineApplyIntent,
    ) -> Response {
        // A stale desired device (unplugged since it was selected) degrades
        // to the default-safe output instead of failing the start. Resolved
        // before the fast path so a no-op re-apply still skips the restart
        // while diverged. Explicit route changes validate the device before
        // reaching here, so substitution never masks a live user intent.
        let effective = self.effective_output_for_plan(&plan);
        self.apply_pipeline_plan_for_output(
            plan,
            driver_sample_rate,
            driver_buffer_frames,
            intent,
            effective,
        )
    }

    fn effective_output_for_plan(&self, plan: &PipelinePlan) -> super::misc::EffectiveOutputDevice {
        super::misc::effective_output_for_sink(
            super::misc::resolve_effective_output_device(plan.spec.output_device.as_deref()),
            self.driver_manager.lock().output_sink_type(),
            plan.spec.output_device.as_deref(),
        )
    }

    fn resolve_fallback_profile(
        &self,
        name: &str,
        requested_input: usize,
        requested_output: usize,
    ) -> Result<OutputProfile, String> {
        let (driver, sink) = {
            let manager = self.driver_manager.lock();
            (manager.status(), manager.output_sink_type())
        };
        let input = usize::try_from(driver.channel_count)
            .ok()
            .filter(|channels| (1..=MAX_HAL_CHANNELS).contains(channels))
            .unwrap_or(requested_input);
        Self::fallback_profile_for_store(&self.output_profiles, name, input, requested_output, sink)
    }

    pub(super) fn fallback_profile_for_store(
        profiles: &Arc<Mutex<OutputProfileStore>>,
        name: &str,
        input: usize,
        requested_output: usize,
        sink: SinkType,
    ) -> Result<OutputProfile, String> {
        if let Some(profile) = profiles.lock().assigned_profile_for_name(name) {
            return Ok(profile);
        }
        let output = if sink == SinkType::LabNull {
            requested_output
        } else {
            use cpal::traits::DeviceTrait;
            let host = sotf_audio::devices::get_host_for_device(Some(name));
            let device = sotf_audio::devices::find_device(
                &host,
                sotf_audio::devices::strip_asio_prefix(name),
                false,
            )
            .map_err(|error| format!("Fallback output '{name}' is unavailable: {error}"))?;
            usize::from(
                device
                    .default_output_config()
                    .map_err(|error| format!("Cannot inspect fallback output '{name}': {error}"))?
                    .channels(),
            )
        };
        profiles.lock().ensure_fallback_profile(name, input, output)
    }

    pub(super) fn prepare_profile_plan(
        state: &SystemwideState,
        mut spec: super::pipeline_spec::PipelineSpec,
        profile: &OutputProfile,
        input_override: Option<usize>,
    ) -> Result<PipelinePlan, String> {
        spec.user_plugins = profile.plugins.clone();
        spec.user_graph = profile.graph.clone();
        spec.input_channels = input_override.unwrap_or(profile.input_channels);
        spec.output_channels = profile.output_channels;
        let input = spec.input_channels;
        state.prepare_from_spec(spec, input)
    }

    /// Apply one resolved route. Keeping resolution separate permits device-free
    /// tests of the actual fallback/profile/engine transaction.
    pub(super) fn apply_pipeline_plan_for_output(
        &self,
        plan: PipelinePlan,
        driver_sample_rate: u32,
        driver_buffer_frames: u32,
        intent: PipelineApplyIntent,
        effective: super::misc::EffectiveOutputDevice,
    ) -> Response {
        if let Some(name) = effective.device.as_deref() {
            if !is_safe_output_device_name(name) {
                return Response::err("Refusing a virtual/loopback device as physical output");
            }
        } else if let Some(desired) = plan.spec.output_device.as_deref() {
            return Response::err(format!(
                "Selected output device '{desired}' is not available: Audio device '{desired}' not found, and no safe physical fallback is available"
            ));
        }
        if let Some(fallback) = &effective.fallback {
            log::warn!(
                "Desired output device '{}' is not available; falling back to default output '{}'",
                fallback.desired,
                fallback.applied
            );
        }
        let requested_device = plan.spec.output_device.clone();
        let fallback = effective.fallback;
        let mut plan = plan;
        plan.spec.output_device = effective.device;
        let changed_output =
            self.system_state.lock().applied_output_device() != plan.spec.output_device;
        let previous = self.system_state.lock().applied_spec();
        if changed_output
            && intent == PipelineApplyIntent::EnsurePlaying
            && previous
                .as_ref()
                .is_some_and(|old| !super::pipeline_spec::pipeline_dsp_matches(old, &plan.spec))
        {
            return Response::err(
                "Output route changed during the edit; refresh the active route and retry",
            );
        }
        let fallback_profile = if changed_output
            && requested_device.is_some()
            && (fallback.is_some()
                || (previous.is_some()
                    && !matches!(
                        intent,
                        PipelineApplyIntent::SelectRoute | PipelineApplyIntent::RestoreLastWorking
                    ))) {
            let Some(name) = plan.spec.output_device.clone() else {
                return Response::err("No safe physical output is available for fallback");
            };
            let profile = match self.resolve_fallback_profile(
                &name,
                plan.spec.input_channels,
                plan.spec.output_channels,
            ) {
                Ok(profile) => profile,
                Err(error) => {
                    return Response::err(format!("Cannot resolve fallback profile: {error}"));
                }
            };
            plan = match Self::prepare_profile_plan(
                &self.system_state.lock(),
                plan.spec.clone(),
                &profile,
                None,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    return Response::err(format!("Cannot prepare fallback profile: {error}"));
                }
            };
            Some((name, profile.id))
        } else {
            None
        };

        // The HAL/driver request rate can outlive the interface it was set
        // for: sample rates change between devices and even on one device
        // (Audio MIDI Setup, another host's session). Verify the resolved
        // output device actually supports the requested rate before the
        // engine opens a stream at a rate the hardware clock will not run.
        let mut driver_sample_rate = driver_sample_rate;
        let mut negotiated_rate: Option<u32> = None;
        if let Some(device_name) = plan.spec.output_device.as_deref() {
            let lab_backend = self.driver_manager.lock().output_sink_type() == SinkType::LabNull;
            match self.device_registry.lock().negotiate_sample_rate(
                device_name,
                driver_sample_rate,
                lab_backend,
            ) {
                Ok(rate) if rate != driver_sample_rate => {
                    log::warn!(
                        "Output device '{device_name}' does not support {driver_sample_rate} Hz; using {rate} Hz",
                    );
                    negotiated_rate = Some(rate);
                    driver_sample_rate = rate;
                }
                Ok(_) => {}
                Err(error) => {
                    log::warn!("Sample-rate check skipped: {error}");
                }
            }
        }
        self.driver_manager.lock().set_engine_ready(false);

        {
            let mut manager = self.manager.lock();
            let _ = manager.stop();
        }

        let mut effective_driver_sample_rate = driver_sample_rate;
        let mut effective_driver_buffer_frames = driver_buffer_frames;

        let current_driver_status = self.driver_manager.lock().status();
        let external_capture = self.driver_manager.lock().capture_source().is_some();
        if current_driver_status.driver_installed
            && (matches!(
                intent,
                PipelineApplyIntent::ReconfigureDriver
                    | PipelineApplyIntent::RestoreLastWorking
                    | PipelineApplyIntent::SelectRoute
            ) || negotiated_rate.is_some()
                || current_driver_status.channel_count != plan.spec.input_channels as u32)
        {
            let result = self.driver_manager.lock().request_config(DriverConfig::new(
                driver_sample_rate,
                driver_buffer_frames,
                plan.spec.input_channels as u32,
            ));

            match result {
                driver_common::ConfigResult::Accepted
                | driver_common::ConfigResult::Negotiated { .. } => {
                    if let driver_common::ConfigResult::Negotiated {
                        actual_rate,
                        actual_frames,
                        actual_channels,
                    } = &result
                        && (!(1..=MAX_HAL_CHANNELS as u32).contains(actual_channels)
                            || if external_capture {
                                !(8_000..=192_000).contains(actual_rate)
                                    || !(1..=8192).contains(actual_frames)
                            } else {
                                !SUPPORTED_SAMPLE_RATES.contains(actual_rate)
                                    || !(64..=4096).contains(actual_frames)
                            })
                    {
                        return Response::err("Driver negotiated invalid capture geometry");
                    }
                    if !external_capture {
                        (effective_driver_sample_rate, effective_driver_buffer_frames) =
                            pipeline_timing_after_config_request(
                                &result,
                                driver_sample_rate,
                                driver_buffer_frames,
                            );
                    }
                    if let driver_common::ConfigResult::Negotiated {
                        actual_channels, ..
                    } = &result
                        && *actual_channels as usize != plan.spec.input_channels
                    {
                        let mut spec = plan.spec.clone();
                        spec.input_channels = *actual_channels as usize;
                        plan = match self
                            .system_state
                            .lock()
                            .prepare_from_spec(spec, *actual_channels as usize)
                        {
                            Ok(plan) => plan,
                            Err(error) => {
                                return Response::err(format!(
                                    "Invalid negotiated capture geometry: {error}"
                                ));
                            }
                        };
                    }
                    log::info!(
                        "HAL input channel count set to {} via driver config",
                        plan.spec.input_channels
                    );
                }
                driver_common::ConfigResult::Error(e) => {
                    log::error!("Failed to set HAL input channels: {}", e);
                    return Response::err(format!("Failed to set HAL input channels: {}", e));
                }
                _ => return Response::err("Driver returned an unknown configuration result"),
            }
        }

        log::info!(
            "Loading driver plugin chain: {} user plugins + 2 monitors = {} total, {}Hz {}ch input, {} output channels, device: {:?}",
            plan.spec.user_plugins.len(),
            plan.runtime_plugins.len(),
            effective_driver_sample_rate,
            plan.spec.input_channels,
            plan.spec.output_channels,
            plan.spec.output_device
        );

        let sink_type = self.driver_manager.lock().output_sink_type();
        let capture_source = self.driver_manager.lock().capture_source();
        let result = {
            let mut manager = self.manager.lock();
            Self::start_pipeline_plan(
                &mut manager,
                &plan,
                effective_driver_sample_rate,
                effective_driver_buffer_frames,
                sink_type,
                capture_source,
            )
            .map_err(|error| error.to_string())
        };
        let result = if Self::requires_playback_callback(&current_driver_status) {
            result.and_then(|()| self.wait_for_playback_ready())
        } else {
            result
        };

        match result {
            Ok(_) => {
                match &fallback {
                    Some(fallback) => self
                        .system_state
                        .lock()
                        .commit_fallback_applied(&plan, Some(fallback.desired.clone())),
                    None => self.system_state.lock().commit_applied(&plan),
                }
                if let Some((name, profile_id)) = fallback_profile {
                    // Set identity before the outer wrapper syncs the live chain.
                    // Keep the missing desired route in SystemwideState.
                    self.output_profiles.lock().set_current_in_memory(
                        None,
                        Some(name),
                        Some(profile_id),
                    );
                }
                self.system_state.lock().processing_timing =
                    Some((effective_driver_sample_rate, effective_driver_buffer_frames));
                let generation = self.system_state.lock().generation();
                let engine = self.manager.lock().get_engine_state();
                self.playback_progress.lock().record_new_stream(
                    Instant::now(),
                    generation,
                    &engine,
                );
                self.system_state.lock().playback_wanted = true;
                log::info!("Driver plugin chain loaded successfully");

                if !self.driver_manager.lock().set_engine_ready(true) {
                    return Response::err("Engine started but driver readiness publication failed");
                }
                log::info!("Set engine_ready=true via driver");
                if let Err(e) = self.sync_encryption_to_shared_memory(false) {
                    log::warn!("{}", e);
                }

                Response::ok_empty()
            }
            Err(e) => {
                // The engine reports a missing output device generically once
                // its manager thread has exited. Name the absent device so the
                // cold-start retry matcher, the latched fault, and installers
                // see the actionable cause instead of "Engine thread exited
                // unexpectedly".
                let clarified = super::misc::clarify_device_absence_error(
                    plan.spec.output_device.as_deref(),
                    &e,
                );
                log::error!("Failed to load driver plugins: {}", clarified);
                Response::err(format!("Failed to load plugin chain: {}", clarified))
            }
        }
    }

    /// Start one prepared pipeline, including the graph bootstrap/update
    /// sequence and loudness monitor selection. Both IPC mutations and the
    /// driver reconfiguration watcher use this helper so their recovery paths
    /// cannot drift apart.
    pub(super) fn start_pipeline_plan(
        manager: &mut AudioEngineManager,
        plan: &PipelinePlan,
        sample_rate: u32,
        buffer_frames: u32,
        sink_type: SinkType,
        capture_source: Option<driver_common::CaptureSource>,
    ) -> Result<(), String> {
        if let Some(source) = capture_source.as_ref() {
            let actual = source.configuration();
            if actual.sample_rate == 0 || actual.channel_count as usize != plan.spec.input_channels
            {
                return Err(format!(
                    "Capture source geometry does not match pipeline input: {} Hz/{} channels, expected {} channels",
                    actual.sample_rate, actual.channel_count, plan.spec.input_channels
                ));
            }
        }
        let bootstrap_plugins = if plan.runtime_graph.is_some() {
            build_driver_plugin_chain(Vec::new()).0
        } else {
            plan.runtime_plugins.clone()
        };
        manager
            .start_driver_playback_with_source(
                plan.spec.output_device.clone(),
                bootstrap_plugins,
                plan.spec.output_channels,
                sample_rate,
                buffer_frames,
                plan.spec.input_channels,
                sink_type,
                capture_source,
            )
            .map_err(|error| error.to_string())?;

        if let Some(graph) = plan.runtime_graph.clone()
            && let Err(error) = manager.update_plugin_graph(graph)
        {
            let _ = manager.stop();
            return Err(error);
        }

        // Some engine revisions configure the driver source while leaving the
        // committed playback state Stopped. Activate through acknowledged
        // transport commands before reporting the pipeline ready.
        if manager.get_playback_state() == sotf_audio::PlaybackState::Stopped {
            manager.pause().map_err(|error| error.to_string())?;
        }
        manager.resume().map_err(|error| error.to_string())?;
        manager.set_loudness_plugin_index(plan.output_loudness_index);
        Ok(())
    }

    pub(super) fn handle_load_plugins_with_channels(
        &self,
        plugins: Vec<PluginConfig>,
        input_channels: usize,
        output_channels: usize,
    ) -> Response {
        let driver_status = self.driver_manager.lock().status();
        let (driver_sample_rate, driver_buffer_frames) = self.processing_timing(&driver_status);
        let stored_input_channels = self.system_state.lock().input_channels();
        let fallback_input_channels = if driver_status.channel_count > 0 {
            driver_status.channel_count as usize
        } else if stored_input_channels > 0 {
            stored_input_channels
        } else {
            2
        };

        let plan = match self.system_state.lock().prepare_plan(
            plugins,
            input_channels,
            output_channels,
            fallback_input_channels,
        ) {
            Ok(plan) => plan,
            Err(e) => return Response::err(e),
        };

        self.apply_pipeline_plan(
            plan,
            driver_status,
            driver_sample_rate,
            driver_buffer_frames,
        )
    }

    pub(super) fn handle_load_plugin_artifact(
        &self,
        artifact: Value,
        base_generation: Option<u64>,
    ) -> Response {
        if let Some(base_generation) = base_generation {
            let current_generation = self.system_state.lock().generation();
            if base_generation != current_generation {
                return Response::err(format!(
                    "Plugin artifact generation conflict: editor based on generation {base_generation}, current generation is {current_generation}. Refresh before applying."
                ));
            }
        }
        match plan_plugin_artifact(artifact) {
            Ok(PluginArtifactPlan::RackChain { plugins }) => {
                let (input_channels, output_channels) = {
                    let state = self.system_state.lock();
                    (state.input_channels(), state.output_channels())
                };
                self.handle_load_plugins_with_channels(plugins, input_channels, output_channels)
            }
            Ok(PluginArtifactPlan::Graph { graph }) => {
                let (input_channels, output_channels) = {
                    let state = self.system_state.lock();
                    (state.input_channels(), state.output_channels())
                };
                self.handle_load_plugin_graph_with_channels(graph, input_channels, output_channels)
            }
            Ok(PluginArtifactPlan::UnsupportedGraph { reason }) => Response::err(format!(
                "Unsupported graph plugin artifact: {}. Use a graph-aware loader instead of flattening it into the rack.",
                reason
            )),
            Err(e) => Response::err(format!("Invalid plugin artifact: {}", e)),
        }
    }

    pub(super) fn handle_load_plugin_artifact_path(
        &self,
        path: &str,
        base_generation: Option<u64>,
    ) -> Response {
        if let Some(base_generation) = base_generation {
            let current_generation = self.system_state.lock().generation();
            if base_generation != current_generation {
                return Response::err(format!(
                    "Plugin artifact generation conflict: editor based on generation {base_generation}, current generation is {current_generation}. Refresh before applying."
                ));
            }
        }

        // The artifact path command reads a file on the daemon's behalf, so it
        // enforces the same local-regular-file policy as the `load` command:
        // absolute, same-owner, non-symlink regular files only.
        let canonical_path = match validate_user_load_path(std::path::Path::new(path)) {
            Ok(canonical_path) => canonical_path,
            Err(error) => return Response::err(format!("Invalid plugin artifact: {error}")),
        };
        let driver_status = self.driver_manager.lock().status();
        let sample_rate = if driver_status.sample_rate > 0 {
            driver_status.sample_rate
        } else {
            48_000
        };
        let file_plan = match crate::plugin_artifact::plan_plugin_artifact_file(
            &canonical_path,
            f64::from(sample_rate),
        ) {
            Ok(plan) => plan,
            Err(error) => return Response::err(format!("Invalid plugin artifact: {error}")),
        };

        if let Some(required_channels) = file_plan.required_channels
            && let Err(error) = self.validate_output_device_channels(required_channels, sample_rate)
        {
            return Response::err(error);
        }

        match file_plan.plan {
            PluginArtifactPlan::RackChain { plugins } => {
                let channels = {
                    let state = self.system_state.lock();
                    (state.input_channels(), state.output_channels())
                };
                self.handle_load_plugins_with_channels(plugins, channels.0, channels.1)
            }
            PluginArtifactPlan::Graph { graph } => {
                let required_channels = file_plan.required_channels.unwrap_or_else(|| {
                    let state = self.system_state.lock();
                    state.output_channels()
                });
                self.handle_load_plugin_graph_with_channels(
                    graph,
                    required_channels,
                    required_channels,
                )
            }
            PluginArtifactPlan::UnsupportedGraph { reason } => Response::err(format!(
                "Unsupported graph plugin artifact: {reason}. Use a graph-aware loader instead of flattening it into a rack."
            )),
        }
    }

    pub(super) fn validate_output_device_channels(
        &self,
        required_channels: usize,
        sample_rate: u32,
    ) -> Result<(), String> {
        let selected_device = self
            .system_state
            .lock()
            .selected_output_device()
            .or_else(|| {
                self.manager
                    .lock()
                    .get_engine_state()
                    .playback_output_device
            });
        let Some(selected_device) = selected_device else {
            return Ok(());
        };
        let device_name = sotf_audio::devices::strip_asio_prefix(&selected_device);
        let lab_backend = {
            let driver = self.driver_manager.lock();
            driver.is_lab_backend()
                || (driver.output_sink_type() == SinkType::LabNull
                    && selected_device == super::misc::LAB_OUTPUT_DEVICE_NAME)
        };
        let max_channels = self.device_registry.lock().max_output_channels(
            &selected_device,
            sample_rate,
            lab_backend,
        )?;

        if required_channels > max_channels {
            return Err(format!(
                "This RoomEQ configuration requires {required_channels} output channels, but '{device_name}' supports at most {max_channels} channels at {sample_rate} Hz. Select a compatible audio device before loading it."
            ));
        }
        Ok(())
    }

    pub(super) fn handle_reorder_graph(
        &self,
        order: Vec<usize>,
        base_generation: Option<u64>,
    ) -> Response {
        if let Some(base_generation) = base_generation {
            let current_generation = self.system_state.lock().generation();
            if base_generation != current_generation {
                return Response::err(format!(
                    "Graph generation conflict: editor based on generation {base_generation}, current generation is {current_generation}. Refresh before reordering."
                ));
            }
        }

        let (graph, input_channels, output_channels) = {
            let state = self.system_state.lock();
            let Some(graph) = state.user_graph() else {
                return Response::err(
                    "Graph reorder requires an active graph pipeline; use reorder_plugins for a rack.",
                );
            };
            (graph, state.input_channels(), state.output_channels())
        };

        let reordered = match reorder_linear_graph(&graph, &order) {
            Ok(graph) => graph,
            Err(error) => return Response::err(error),
        };
        self.handle_load_plugin_graph_with_channels(reordered, input_channels, output_channels)
    }

    fn handle_load_plugin_graph_with_channels(
        &self,
        graph: sotf_audio::engine::PluginGraphConfig,
        input_channels: usize,
        output_channels: usize,
    ) -> Response {
        let (current_input_channels, current_output_channels) = {
            let state = self.system_state.lock();
            (state.input_channels(), state.output_channels())
        };
        let channel_geometry_changed =
            input_channels != current_input_channels || output_channels != current_output_channels;
        let driver_status = self.driver_manager.lock().status();
        let (driver_sample_rate, driver_buffer_frames) = self.processing_timing(&driver_status);
        let fallback_input_channels = if driver_status.channel_count > 0 {
            driver_status.channel_count as usize
        } else {
            self.system_state.lock().input_channels().max(2)
        };
        let plan = match self.system_state.lock().prepare_graph_plan(
            graph,
            input_channels,
            output_channels,
            fallback_input_channels,
        ) {
            Ok(plan) => plan,
            Err(error) => return Response::err(error),
        };

        if !channel_geometry_changed
            && self.manager.lock().get_state() != sotf_audio::manager::StreamingState::Idle
        {
            let plan = match self.prepare_live_update(plan) {
                Ok(plan) => plan,
                Err(error) => return Response::err(error),
            };
            let Some(runtime_graph) = plan.runtime_graph.clone() else {
                return Response::err("Graph plan did not contain a runtime graph");
            };
            let mut manager = self.manager.lock();
            if let Err(error) = manager.update_plugin_graph(runtime_graph) {
                return Response::err(format!(
                    "Failed to apply plugin graph; previous pipeline remains active: {error}"
                ));
            }
            manager.set_loudness_plugin_index(plan.output_loudness_index);
            drop(manager);
            self.commit_live_update(&plan);
            self.sync_live_chain_to_current_profile(true);
            return Response::ok(serde_json::json!({
                "topology": "graph",
                "nodes": plan.spec.user_graph.as_ref().map_or(0, |graph| graph.nodes.len()),
                "edges": plan.spec.user_graph.as_ref().map_or(0, |graph| graph.edges.len()),
                "generation": self.system_state.lock().generation(),
            }));
        }

        self.apply_pipeline_plan(
            plan,
            driver_status,
            driver_sample_rate,
            driver_buffer_frames,
        )
    }

    pub(super) fn handle_set_pipeline_channels(
        &self,
        input_channels: Option<usize>,
        output_channels: Option<usize>,
    ) -> Response {
        if input_channels.is_none() && output_channels.is_none() {
            return Response::err(
                "set_pipeline_channels requires input_channels or output_channels",
            );
        }
        self.handle_apply_configuration(None, None, input_channels, output_channels, None)
    }

    pub(super) fn handle_get_loudness(&self) -> Response {
        let manager = self.manager.lock();
        match manager.get_loudness() {
            Some(loudness) => Response::ok(loudness_info_to_json(&loudness)),
            None => Response::err("Loudness monitoring not enabled"),
        }
    }

    pub(super) fn handle_get_metering(&self) -> Response {
        let _mutation = self.pipeline_mutation.lock();
        let generation = self.system_state.lock().generation();
        let mut metering = self.metering_snapshot();
        metering["generation"] = serde_json::json!(generation);
        Response::ok(metering)
    }

    // =========================================================================
    // Plugin management handlers
    // =========================================================================

    pub(super) fn handle_get_plugins(&self) -> Response {
        let state = self.system_state.lock();
        if let Some(graph) = state.user_graph() {
            return Response::ok(serde_json::json!({
                "topology": "graph",
                "graph": graph,
                "plugins": [],
            "generation": state.generation(),
            }));
        }
        let input_channels = state.input_channels().max(1);
        let plugins = state.user_plugins();
        let generation = state.generation();
        drop(state);
        let result: Vec<Value> = plugins
            .iter()
            .enumerate()
            .map(|(i, p)| {
                serde_json::json!({
                    "index": i,
                    "plugin_type": p.plugin_type,
                    "parameters": p.parameters,
                    // Legacy rack entries have no per-node metadata. These
                    // defaults are made explicit so the Configbar can issue a
                    // state patch that promotes the rack to a graph.
                    "input_channels": input_channels,
                    "bypassed": false,
                })
            })
            .collect();
        Response::ok(serde_json::json!({
            "topology": "rack",
            "plugins": result,
            "generation": generation,
        }))
    }

    pub(super) fn handle_get_available_plugins(&self) -> Response {
        // Deliberately uncached: defaults must be computed at the live
        // driver rate (a once-per-process cache at a hardcoded 48 kHz handed
        // wrong defaults to 96 kHz sessions) and a type whose defaults fail
        // once must not be omitted for the daemon's whole lifetime. This is
        // a picker-open path, not a hot path.
        let sample_rate = {
            let status = self.driver_manager.lock().status();
            if status.sample_rate > 0 {
                f64::from(status.sample_rate)
            } else {
                48_000.0
            }
        };
        let available = {
            // Types withheld from the user-facing picker. Everything else in
            // `PluginType::all()` is served, matching the player pickers:
            // - `loudness_monitor`: daemon-owned metering, auto-injected at
            //   both chain boundaries; user instances are stripped from racks
            //   and rejected from graphs, so offering it would be dishonest.
            // - `spectrum_analyzer`: daemon-owned system type, rejected from
            //   graphs, with no daemon data path exposing spectrum readings.
            // - `resampler` / `hal_input` / `hal_output`: infrastructure
            //   with non-generic picker exposure, never in `all()`; kept as
            //   defense if that ever changes.
            // - `fletcher_munson`: superseded by `loudness_compensation`
            //   (also omitted from the canonical player categories).
            let excluded = [
                "loudness_monitor",
                "spectrum_analyzer",
                "resampler",
                "hal_input",
                "hal_output",
                "fletcher_munson",
            ];

            let plugins: Vec<Value> = PluginType::all()
                .into_iter()
                .filter(|pt| {
                    let engine_type = plugin_type_to_engine_str(pt);
                    !excluded.contains(&engine_type)
                })
                .filter_map(|pt| {
                    let engine_type = plugin_type_to_engine_str(&pt);
                    let category = plugin_type_category(&pt);
                    let default_settings =
                        match sotf_audio::PluginSettings::default_for(&pt) {
                            Ok(settings) => settings,
                            Err(error) => {
                                log::warn!(
                                    "Skipping plugin type {} because default settings are unavailable: {}",
                                    engine_type,
                                    error
                                );
                                return None;
                            }
                        };
                    let default_parameters = default_settings.to_plugin_config(sample_rate).parameters;
                    Some(serde_json::json!({
                        "type": engine_type,
                        "name": pt.name(),
                        "description": pt.description(),
                        "category": category,
                        "maturity": format!("{:?}", pt.maturity()),
                        "default_parameters": default_parameters,
                        "parameters": plugin_parameter_descriptors(&default_settings),
                    }))
                })
                .collect();

            serde_json::json!({ "plugins": plugins })
        };

        Response::ok(available)
    }

    pub(super) fn handle_add_plugin(&self, plugin: PluginConfig, index: Option<usize>) -> Response {
        let mut plugins = {
            let state = self.system_state.lock();
            if state.user_graph().is_some() {
                return Response::err(
                    "A graph pipeline is active; edit and reload the graph artifact instead of using rack mutation commands.",
                );
            }
            state.user_plugins()
        };
        match index {
            Some(i) if i <= plugins.len() => plugins.insert(i, plugin),
            Some(i) => {
                // A client acting on a stale rack must learn its index was
                // wrong (remove/update already error); silently appending
                // here used to report success at the wrong position.
                return Response::err(format!(
                    "Plugin index {i} out of range (have {})",
                    plugins.len()
                ));
            }
            None => plugins.push(plugin),
        }
        self.reload_plugins_with_user_plugins(plugins)
    }

    pub(super) fn handle_remove_plugin(&self, index: usize) -> Response {
        let mut plugins = {
            let state = self.system_state.lock();
            if state.user_graph().is_some() {
                return Response::err(
                    "A graph pipeline is active; edit and reload the graph artifact instead of using rack mutation commands.",
                );
            }
            state.user_plugins()
        };
        if index >= plugins.len() {
            return Response::err(format!(
                "Plugin index {} out of range (have {})",
                index,
                plugins.len()
            ));
        }
        plugins.remove(index);
        self.reload_plugins_with_user_plugins(plugins)
    }

    pub(super) fn handle_update_plugin(&self, index: usize, parameters: Value) -> Response {
        let (old_parameters, input_channels, output_channels) = {
            let state = self.system_state.lock();
            if state.user_graph().is_some() {
                return Response::err(
                    "A graph pipeline is active; edit and reload the graph artifact instead of using rack mutation commands.",
                );
            }
            let plugins = state.user_plugins();
            if index >= plugins.len() {
                return Response::err(format!(
                    "Plugin index {} out of range (have {})",
                    index,
                    plugins.len()
                ));
            }
            (
                plugins[index].parameters.clone(),
                state.input_channels(),
                state.output_channels(),
            )
        };
        if old_parameters == parameters {
            let generation = self.system_state.lock().generation();
            return Response::ok(serde_json::json!({ "generation": generation }));
        }

        let mut plugins = self.system_state.lock().user_plugins();
        plugins[index].parameters = parameters.clone();
        let plan = match self.system_state.lock().prepare_plan(
            plugins.clone(),
            input_channels,
            output_channels,
            input_channels,
        ) {
            Ok(plan) => plan,
            Err(error) => return Response::err(error),
        };

        // Fast path: zero-dropout per-parameter update while the engine runs.
        // The runtime chain injects the input loudness monitor at position 0,
        // so user plugin `index` runs at runtime position `index + 1`. Any
        // failure (unknown parameter, engine busy, non-object shapes) falls
        // back to the full chain rebuild below, which remains the path for
        // structural changes and for starting an idle engine.
        let engine_running =
            self.manager.lock().get_state() != sotf_audio::manager::StreamingState::Idle;
        let plan = if engine_running {
            match self.prepare_live_update(plan) {
                Ok(plan) => plan,
                Err(error) => return Response::err(error),
            }
        } else {
            plan
        };
        if let Some(changed) = changed_plugin_parameters(&old_parameters, &parameters)
            && engine_running
        {
            let runtime_index = index + 1;
            let mut hot_error: Option<String> = None;
            for (param_id, value) in &changed {
                if let Err(error) = self.manager.lock().set_plugin_parameter(
                    runtime_index,
                    param_id.clone(),
                    encode_plugin_param_value(value),
                ) {
                    hot_error = Some(error);
                    break;
                }
            }
            if hot_error.is_none() {
                self.manager
                    .lock()
                    .set_loudness_plugin_index(plan.output_loudness_index);
                self.commit_live_update(&plan);
                self.sync_live_chain_to_current_profile(false);
                let generation = self.system_state.lock().generation();
                log::info!("Driver plugin parameters hot-updated successfully");
                return Response::ok(serde_json::json!({ "generation": generation }));
            }
            log::warn!(
                "Parameter hot-update failed ({}); falling back to chain rebuild",
                hot_error.unwrap_or_else(|| "unknown error".to_string())
            );
        }
        self.reload_plugins_with_user_plugins(plugins)
    }

    pub(super) fn handle_reorder_plugins(&self, order: Vec<usize>) -> Response {
        let plugins = {
            let state = self.system_state.lock();
            if state.user_graph().is_some() {
                return Response::err(
                    "A graph pipeline is active; edit and reload the graph artifact instead of using rack mutation commands.",
                );
            }
            state.user_plugins()
        };
        let n = plugins.len();

        if order.len() != n {
            return Response::err(format!(
                "Order length {} doesn't match plugin count {}",
                order.len(),
                n
            ));
        }
        let mut seen = vec![false; n];
        for &idx in &order {
            if idx >= n || seen[idx] {
                return Response::err(format!(
                    "Invalid order: duplicate or out-of-range index {}",
                    idx
                ));
            }
            seen[idx] = true;
        }

        let old = plugins.clone();
        let mut reordered = plugins;
        for (new_pos, &old_pos) in order.iter().enumerate() {
            reordered[new_pos] = old[old_pos].clone();
        }
        self.reload_plugins_with_user_plugins(reordered)
    }

    pub(super) fn handle_set_rack_plugin_state(
        &self,
        index: usize,
        input_channels: Option<usize>,
        bypassed: Option<bool>,
        base_generation: Option<u64>,
    ) -> Response {
        if let Some(base_generation) = base_generation {
            let current_generation = self.system_state.lock().generation();
            if base_generation != current_generation {
                return Response::err(format!(
                    "Rack generation conflict: editor based on generation {base_generation}, current generation is {current_generation}. Refresh before changing plugin state."
                ));
            }
        }

        let (plugins, graph, input_geometry, output_channels) = {
            let state = self.system_state.lock();
            (
                state.user_plugins(),
                state.user_graph(),
                state.input_channels(),
                state.output_channels(),
            )
        };
        if graph.is_some() {
            return Response::err(
                "Rack plugin state requires a rack pipeline; use graph commands for graph nodes.",
            );
        }

        let graph = match rack_plugins_to_linear_graph(
            &plugins,
            input_geometry,
            index,
            input_channels,
            bypassed,
        ) {
            Ok(graph) => graph,
            Err(error) => return Response::err(error),
        };
        self.handle_load_plugin_graph_with_channels(graph, input_geometry, output_channels)
    }

    fn prepare_live_update(&self, mut plan: PipelinePlan) -> Result<PipelinePlan, String> {
        let Some(applied) = self.system_state.lock().applied_spec() else {
            return Ok(plan);
        };
        let target = self.effective_output_for_plan(&plan).device;
        let engine = self.manager.lock().get_engine_state();
        if target != applied.output_device
            || engine
                .playback_output_device
                .as_ref()
                .is_some_and(|actual| Some(actual) != applied.output_device.as_ref())
        {
            return Err(
                "Output route changed during the edit; refresh the active route and retry"
                    .to_owned(),
            );
        }
        plan.spec.output_device = applied.output_device;
        Ok(plan)
    }

    fn commit_live_update(&self, plan: &PipelinePlan) {
        let mut state = self.system_state.lock();
        let preferred = state.selected_output_device();
        state.commit_fallback_applied(plan, preferred);
    }

    pub(super) fn reload_plugins_with_user_plugins(&self, plugins: Vec<PluginConfig>) -> Response {
        let prepared_plan = {
            let pipeline = self.system_state.lock();
            pipeline.prepare_plan(
                plugins,
                pipeline.input_channels(),
                pipeline.output_channels(),
                pipeline.input_channels(),
            )
        };
        let plan = match prepared_plan {
            Ok(plan) => plan,
            Err(e) => return Response::err(e),
        };

        if self.manager.lock().get_state() == sotf_audio::manager::StreamingState::Idle {
            log::info!("No running driver engine; starting driver playback");
            return self.handle_load_plugins_with_channels(
                plan.spec.user_plugins.clone(),
                plan.spec.input_channels,
                plan.spec.output_channels,
            );
        }

        let plan = match self.prepare_live_update(plan) {
            Ok(plan) => plan,
            Err(error) => return Response::err(error),
        };
        let result = {
            let manager = self.manager.lock();
            manager.update_plugin_chain(&plan.runtime_plugins)
        };

        match result {
            Ok(()) => {
                self.manager
                    .lock()
                    .set_loudness_plugin_index(plan.output_loudness_index);
                self.commit_live_update(&plan);
                self.sync_live_chain_to_current_profile(true);
                let generation = self.system_state.lock().generation();
                log::info!("Driver plugin chain hot-updated successfully");
                Response::ok(serde_json::json!({ "generation": generation }))
            }
            Err(e) => {
                log::error!("Failed to hot-update plugin chain: {}", e);
                Response::err(format!("Failed to update plugin chain: {}", e))
            }
        }
    }

    pub(super) fn handle_driver_status(&self) -> Response {
        let status = get_driver_status(&self.driver_manager.lock());
        let mut data = match serde_json::to_value(&status) {
            Ok(serde_json::Value::Object(data)) => data,
            Ok(_) => return Response::err("Driver status did not serialize as an object"),
            Err(error) => {
                return Response::err(format!("Failed to serialize driver status: {error}"));
            }
        };
        // Preserve the historical aliases while deriving the canonical fields
        // from DriverStatus itself. This keeps the JSON wire shape aligned with
        // the serde contract whenever a new status field is added.
        data.insert(
            "buffer_initialized".to_string(),
            serde_json::Value::Bool(status.capture_active || status.driver_installed),
        );
        data.insert(
            "ready".to_string(),
            serde_json::Value::Bool(
                status.platform_supported && status.driver_installed && status.driver_ready,
            ),
        );
        Response::ok(serde_json::Value::Object(data))
    }

    // =========================================================================
    // Per-output DSP profiles
    // =========================================================================

    /// Snapshot the live chain for profile adoption and write-through sync.
    /// Locks are sequenced, never nested: the state guard drops before the
    /// caller touches the profile store.
    fn live_chain_snapshot(&self) -> (Vec<PluginConfig>, Option<PluginGraphConfig>, usize, usize) {
        let state = self.system_state.lock();
        (
            state.user_plugins(),
            state.user_graph(),
            state.input_channels(),
            state.output_channels(),
        )
    }

    /// Write the live chain through to the current profile so edits made on
    /// the live route survive output switches. Structural callers persist to
    /// disk; the per-parameter hot path only updates memory (picked up by
    /// the next persisted mutation) to stay fsync-free on slider ticks.
    fn sync_live_chain_to_current_profile(&self, persist: bool) {
        let (plugins, graph, input_channels, output_channels) = self.live_chain_snapshot();
        let result = self.output_profiles.lock().sync_live_chain(
            plugins,
            graph,
            input_channels,
            output_channels,
            persist,
        );
        if let Err(error) = result {
            log::warn!("Failed to sync live chain to output profile: {error}");
        }
    }

    /// Adopt the live chain as the `default` profile on first profile use.
    /// Pre-profile daemons keep their chain; it simply gains a name.
    fn ensure_default_profile(&self) -> Result<bool, String> {
        let (plugins, graph, input_channels, output_channels) = self.live_chain_snapshot();
        self.output_profiles
            .lock()
            .ensure_default(plugins, graph, input_channels, output_channels)
    }

    fn output_profile_wire(&self, profile: &OutputProfile) -> Value {
        if profile.is_graph() {
            serde_json::json!({
                "id": profile.id,
                "name": profile.name,
                "topology": profile.topology,
                "plugins": [],
                "graph": profile.graph,
                "input_channels": profile.input_channels,
                "output_channels": profile.output_channels,
                "updated_at_unix_ms": profile.updated_at_unix_ms,
            })
        } else {
            let plugins: Vec<Value> = profile
                .plugins
                .iter()
                .enumerate()
                .map(|(index, plugin)| {
                    serde_json::json!({
                        "index": index,
                        "plugin_type": plugin.plugin_type,
                        "parameters": plugin.parameters,
                        "input_channels": profile.input_channels.max(1),
                        "bypassed": false,
                    })
                })
                .collect();
            serde_json::json!({
                "id": profile.id,
                "name": profile.name,
                "topology": profile.topology,
                "plugins": plugins,
                "graph": Value::Null,
                "input_channels": profile.input_channels,
                "output_channels": profile.output_channels,
                "updated_at_unix_ms": profile.updated_at_unix_ms,
            })
        }
    }

    pub(super) fn handle_get_output_profiles(&self) -> Response {
        if let Err(error) = self.ensure_default_profile() {
            return Response::err(format!("Failed to adopt default output profile: {error}"));
        }
        let (profiles, assignments, current) = {
            let store = self.output_profiles.lock();
            let profiles: Vec<Value> = store
                .profiles_sorted()
                .iter()
                .map(|profile| self.output_profile_wire(profile))
                .collect();
            (profiles, store.assignments().clone(), store.current())
        };
        let (current_profile_id, current_device_uid, current_device_name) = current;
        let response = serde_json::json!({
            "profiles": profiles,
            "assignments": assignments,
            "current": {
                "profile_id": current_profile_id,
                "device_uid": current_device_uid,
                "device_name": current_device_name,
            },
            "generation": self.system_state.lock().generation(),
        });
        Response::ok(response)
    }

    pub(super) fn handle_set_output_profile(&self, profile: OutputProfile) -> Response {
        if let Err(error) = self.ensure_default_profile() {
            return Response::err(format!("Failed to adopt default output profile: {error}"));
        }
        let id = match self.output_profiles.lock().upsert(profile) {
            Ok(id) => id,
            Err(error) => return Response::err(error),
        };
        Response::ok(serde_json::json!({
            "profile_id": id,
            "generation": self.system_state.lock().generation(),
        }))
    }

    pub(super) fn handle_delete_output_profile(&self, profile_id: &str) -> Response {
        if let Err(error) = self.ensure_default_profile() {
            return Response::err(format!("Failed to adopt default output profile: {error}"));
        }
        if let Err(error) = self.output_profiles.lock().remove(profile_id) {
            return Response::err(error);
        }
        Response::ok(serde_json::json!({
            "generation": self.system_state.lock().generation(),
        }))
    }

    pub(super) fn handle_assign_output_profile(
        &self,
        device_uid: Option<&str>,
        device_name: &str,
        profile_id: &str,
    ) -> Response {
        if let Err(error) = self.ensure_default_profile() {
            return Response::err(format!("Failed to adopt default output profile: {error}"));
        }
        if let Err(error) = self
            .output_profiles
            .lock()
            .assign(device_uid, device_name, profile_id)
        {
            return Response::err(error);
        }
        Response::ok(serde_json::json!({
            "generation": self.system_state.lock().generation(),
        }))
    }

    /// Chain to load at cold start: the stored profile for the configured
    /// output, so restarts recall per-output DSP instead of starting empty.
    fn startup_profile_output_channels(&self) -> usize {
        let store = self.output_profiles.lock();
        let channels = store
            .current()
            .0
            .and_then(|id| store.get(&id).map(|profile| profile.output_channels));
        drop(store);
        channels.unwrap_or_else(|| self.system_state.lock().output_channels())
    }

    fn resolve_startup_chain(
        &self,
        configured_device: Option<&str>,
        fallback: bool,
    ) -> StartupChain {
        let (persisted_uid, persisted_name) = {
            let store = self.output_profiles.lock();
            let (_, uid, name) = store.current();
            (uid, name)
        };
        let device_name = configured_device
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or(persisted_name.clone());
        let Some(device_name) = device_name else {
            return StartupChain::Rack(Vec::new());
        };
        let uid_matches = persisted_name.as_deref() == Some(device_name.as_str());
        let uid = uid_matches
            .then(|| persisted_uid.clone())
            .flatten()
            .filter(|uid| !uid.trim().is_empty());
        let resolved = if fallback {
            self.resolve_fallback_profile(&device_name, 2, 2)
        } else {
            self.resolve_route_profile(uid.as_deref(), &device_name, None)
        };
        let profile = match resolved {
            Ok(profile) => profile,
            Err(error) => {
                log::warn!("Cold start falls back to an empty chain: {error}");
                return StartupChain::Rack(Vec::new());
            }
        };
        // Re-affirm the recalled route in memory; the on-disk state already
        // holds it (or nothing, for fresh daemons).
        self.output_profiles.lock().set_current_in_memory(
            uid,
            Some(device_name),
            Some(profile.id.clone()),
        );
        if profile.is_graph() {
            match profile.graph {
                Some(graph) => StartupChain::Graph(graph),
                None => StartupChain::Rack(profile.plugins),
            }
        } else {
            StartupChain::Rack(profile.plugins)
        }
    }

    /// Resolve the stored profile for a route change, adopting the live
    /// chain as `default` first so the lookup cannot miss on fresh stores.
    fn resolve_route_profile(
        &self,
        device_uid: Option<&str>,
        device_name: &str,
        profile_id: Option<&str>,
    ) -> Result<OutputProfile, String> {
        self.ensure_default_profile()?;
        let store = self.output_profiles.lock();
        if let Some(requested) = profile_id.map(str::trim).filter(|id| !id.is_empty()) {
            return store
                .get(requested)
                .cloned()
                .ok_or_else(|| format!("unknown output profile \"{requested}\""));
        }
        let resolved = store.resolve_profile_id(device_uid, device_name);
        resolved
            .and_then(|id| store.get(&id).cloned())
            .ok_or_else(|| "no output profile available for this device".to_string())
    }

    /// Apply an output route and its DSP chain in one pipeline transaction:
    /// the resolved profile's chain becomes the desired chain for the new
    /// device, prepared and applied under a single generation.
    pub(super) fn handle_set_output_route(
        &self,
        output_device: &str,
        output_device_uid: Option<&str>,
        profile_id: Option<&str>,
        input_channels: Option<usize>,
        output_channels: Option<usize>,
        base_generation: Option<u64>,
    ) -> Response {
        use cpal::traits::DeviceTrait;
        if let Some(base_generation) = base_generation {
            let current_generation = self.system_state.lock().generation();
            if base_generation != current_generation {
                return Response::err(format!(
                    "Output route generation conflict: intent was based on generation {base_generation}, current generation is {current_generation}. Refresh and retry."
                ));
            }
        }
        let device = output_device.trim();
        if device.is_empty() {
            return Response::err("Output device must not be empty");
        }
        let lab_backend = self.driver_manager.lock().output_sink_type() == SinkType::LabNull;
        let is_asio = !lab_backend && sotf_audio::devices::is_asio_device(device);
        let resolved_name = if lab_backend {
            match self.resolve_safe_output_device(device) {
                Ok(name) => name,
                Err(error) => return Response::err(error),
            }
        } else {
            let host = sotf_audio::devices::get_host_for_device(Some(device));
            let device_name = sotf_audio::devices::strip_asio_prefix(device);
            match sotf_audio::devices::find_device(&host, device_name, false) {
                Ok(cpal_device) => cpal_device
                    .description()
                    .map(|description| description.name().to_string())
                    .unwrap_or_else(|_| "Unknown Device".to_string()),
                Err(error) => {
                    self.device_registry.lock().invalidate();
                    return Response::err(format!("Device '{device}' not found. {error}"));
                }
            }
        };
        if !is_safe_output_device_name(&resolved_name) {
            return Response::err(format!(
                "'{resolved_name}' is a virtual/loopback device and cannot be used as Systemwide speaker output. Select hardware speakers/headphones here, and select SotF Virtual Audio in macOS Sound Output."
            ));
        }
        let stored_name = if is_asio {
            format!(
                "{}{}",
                sotf_audio::devices::ASIO_DEVICE_PREFIX,
                resolved_name
            )
        } else {
            resolved_name.clone()
        };

        let profile =
            match self.resolve_route_profile(output_device_uid, &resolved_name, profile_id) {
                Ok(profile) => profile,
                Err(error) => return Response::err(error),
            };

        let plan = {
            let state = self.system_state.lock();
            let mut next = state.desired_spec();
            next.output_device = Some(stored_name.clone());
            // A profile saved at one geometry used to run silently at the
            // current spec's geometry; apply the profile's channels unless
            // the route call explicitly overrides them.
            if let Some(channels) = input_channels {
                next.input_channels = channels;
            } else if profile.input_channels > 0 {
                next.input_channels = profile.input_channels;
            }
            if let Some(channels) = output_channels {
                next.output_channels = channels;
            } else if profile.output_channels > 0 {
                next.output_channels = profile.output_channels;
            }
            next.user_plugins = profile.plugins.clone();
            next.user_graph = profile.graph.clone();
            match state.prepare_from_spec(next, state.input_channels()) {
                Ok(plan) => plan,
                Err(error) => return Response::err(error),
            }
        };

        let driver_status = self.driver_manager.lock().status();
        let (driver_sample_rate, driver_buffer_frames) = self.processing_timing(&driver_status);
        log::info!(
            "Switching output route to {} with profile '{}' ({})",
            resolved_name,
            profile.name,
            profile.id
        );
        let response = self.apply_pipeline_transaction(
            plan,
            driver_status,
            driver_sample_rate,
            driver_buffer_frames,
            PipelineApplyIntent::SelectRoute,
        );
        if !response.success {
            return response;
        }

        if let Err(error) = persist_output_device(&stored_name) {
            log::error!(
                "Output device '{}' is active but could not be persisted for the next daemon start: {}",
                stored_name,
                error
            );
        }
        if let Err(error) = self.output_profiles.lock().record_route_if_active(
            output_device_uid,
            &resolved_name,
            &profile.id,
            self.system_state.lock().applied_output_device().as_deref(),
        ) {
            log::warn!("Output route applied but profile memory failed: {error}");
        }
        self.sync_live_chain_to_current_profile(true);

        let generation = self.system_state.lock().generation();
        Response::ok(serde_json::json!({
            "generation": generation,
            "profile_id": profile.id,
            "output_device": stored_name,
        }))
    }

    // =========================================================================
    // Encryption handlers
    // =========================================================================

    #[cfg(all(target_os = "macos", feature = "hal"))]
    pub(super) fn sync_encryption_to_shared_memory(&self, flush_audio: bool) -> Result<(), String> {
        let key_manager = self.key_manager.lock();
        Self::apply_encryption_to_shared_memory(&key_manager, flush_audio)
    }

    #[cfg(not(all(target_os = "macos", feature = "hal")))]
    pub(super) fn sync_encryption_to_shared_memory(
        &self,
        _flush_audio: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    #[cfg(all(target_os = "macos", feature = "hal"))]
    pub(super) fn apply_encryption_to_shared_memory(
        key_manager: &KeyManager,
        flush_audio: bool,
    ) -> Result<(), String> {
        match driver_hal::SharedAudioBuffer::open_default() {
            Ok(buffer) => {
                if flush_audio {
                    buffer.flush_audio();
                }
                if key_manager.is_enabled() {
                    buffer.set_key_fingerprint(*key_manager.fingerprint());
                }
                buffer.set_encrypted(key_manager.is_enabled());
                buffer.set_config_changed();
                Ok(())
            }
            Err(e) => {
                let message = format!("Failed to sync encryption state to shared memory: {}", e);
                log::warn!("{}", message);
                Err(message)
            }
        }
    }

    pub(super) fn handle_set_encryption(&self, enabled: bool) -> Response {
        // The HAL realtime path is allocation-free (caller buffers +
        // RealtimeChaChaPoly), so enabling is a supported operation. Builds
        // without a session cipher still refuse via the guard below.
        let mut key_manager = self.key_manager.lock();
        key_manager.set_enabled(enabled);

        if enabled && !key_manager.is_enabled() {
            return Response::err(
                "Encryption unavailable: the daemon has no session cipher; use the macOS HAL-enabled build and verify session-key runtime access",
            );
        }

        // On macOS with HAL, update shared memory encryption flag if the HAL
        // shared memory is available. Missing shared memory is normal when the
        // HAL driver is not currently running; the daemon-side encryption state
        // remains set and will be synced when the driver reconnects.
        #[cfg(all(target_os = "macos", feature = "hal"))]
        let (transport_state, transport_error) = match Self::apply_encryption_to_shared_memory(
            &key_manager,
            true,
        ) {
            Ok(()) => ("synced", None),
            Err(error) => {
                log::warn!(
                    "Encryption state is pending shared-memory sync (HAL may not be running): {}",
                    error
                );
                ("pending", Some(error))
            }
        };
        #[cfg(not(all(target_os = "macos", feature = "hal")))]
        let (transport_state, transport_error): (&str, Option<String>) = ("not_applicable", None);

        Response::ok(serde_json::json!({
            "enabled": key_manager.is_enabled(),
            "fingerprint": key_manager.fingerprint_hex(),
            "transport_state": transport_state,
            "transport_error": transport_error,
        }))
    }

    pub(super) fn handle_encryption_status(&self) -> Response {
        let key_manager = self.key_manager.lock();
        let status = key_manager.status();

        #[cfg(all(target_os = "macos", feature = "hal"))]
        let (transport_state, transport_error) = match driver_hal::SharedAudioBuffer::open_default()
        {
            Ok(buffer) => {
                let fingerprint_matches =
                    !status.enabled || buffer.key_fingerprint() == *key_manager.fingerprint();
                if buffer.is_encrypted() == status.enabled && fingerprint_matches {
                    ("synced", None)
                } else {
                    (
                        "mismatch",
                        Some(
                            "shared-memory encryption state differs from daemon state".to_string(),
                        ),
                    )
                }
            }
            Err(error) => ("unavailable", Some(error.to_string())),
        };
        #[cfg(not(all(target_os = "macos", feature = "hal")))]
        let (transport_state, transport_error): (&str, Option<String>) = ("not_applicable", None);

        Response::ok(serde_json::json!({
            "enabled": status.enabled,
            "fingerprint": status.fingerprint,
            "key_path": status.key_path,
            "transport_state": transport_state,
            "transport_error": transport_error,
        }))
    }

    pub(super) fn handle_rotate_encryption_key(&self) -> Response {
        let mut key_manager = self.key_manager.lock();

        match key_manager.force_rotate() {
            Ok(()) => {
                // On macOS with HAL, update shared memory fingerprint if the HAL
                // shared memory is available. Missing shared memory is normal when
                // the HAL driver is not currently running.
                #[cfg(all(target_os = "macos", feature = "hal"))]
                let (transport_state, transport_error) =
                    match Self::apply_encryption_to_shared_memory(&key_manager, true) {
                        Ok(()) => ("synced", None),
                        Err(error) => {
                            log::warn!(
                                "Rotated encryption key is pending shared-memory sync (HAL may not be running): {}",
                                error
                            );
                            ("pending", Some(error))
                        }
                    };
                #[cfg(not(all(target_os = "macos", feature = "hal")))]
                let (transport_state, transport_error): (&str, Option<String>) =
                    ("not_applicable", None);

                Response::ok(serde_json::json!({
                    "fingerprint": key_manager.fingerprint_hex(),
                    "transport_state": transport_state,
                    "transport_error": transport_error,
                }))
            }
            Err(e) => Response::err(format!("Failed to rotate key: {}", e)),
        }
    }

    // =========================================================================
    // Driver config handlers
    // =========================================================================

    pub(super) fn handle_apply_configuration(
        &self,
        sample_rate: Option<u32>,
        buffer_frames: Option<u32>,
        input_channels: Option<usize>,
        output_channels: Option<usize>,
        output_device: Option<String>,
    ) -> Response {
        if sample_rate.is_none()
            && buffer_frames.is_none()
            && input_channels.is_none()
            && output_channels.is_none()
            && output_device.is_none()
        {
            return Response::err("Configuration patch must contain at least one field");
        }

        if let Some(rate) = sample_rate
            && !SUPPORTED_SAMPLE_RATES.contains(&rate)
        {
            return Response::err(format!(
                "Unsupported sample rate: {rate}. Supported: {SUPPORTED_SAMPLE_RATES:?}"
            ));
        }
        if let Some(frames) = buffer_frames
            && !(super::misc::MIN_BUFFER_FRAMES..=super::misc::MAX_BUFFER_FRAMES).contains(&frames)
        {
            return Response::err(format!(
                "Buffer frames must be between {} and {}, got: {frames}",
                super::misc::MIN_BUFFER_FRAMES,
                super::misc::MAX_BUFFER_FRAMES,
            ));
        }
        for (name, channels) in [("input", input_channels), ("output", output_channels)] {
            if let Some(channels) = channels
                && !(1..=MAX_HAL_CHANNELS).contains(&channels)
            {
                return Response::err(format!(
                    "Invalid {name} channel count: {channels}. Must be between 1 and {MAX_HAL_CHANNELS}."
                ));
            }
        }

        let driver_status = self.driver_manager.lock().status();
        let (current_rate, current_frames) = self.processing_timing(&driver_status);
        let requested_sample_rate = sample_rate.unwrap_or(current_rate);
        let requested_buffer_frames = buffer_frames.unwrap_or_else(|| match sample_rate {
            Some(new_rate) if new_rate != current_rate => {
                super::misc::scaled_buffer_frames_for_rate_change(
                    current_rate,
                    new_rate,
                    current_frames,
                )
            }
            _ => current_frames,
        });
        let fallback_input_channels = if driver_status.channel_count > 0 {
            driver_status.channel_count as usize
        } else {
            2
        };

        let resolved_output_device = match output_device.as_deref() {
            Some(device) if device.trim().is_empty() => {
                return Response::err("Output device must not be empty");
            }
            Some(device) => match self.resolve_safe_output_device(device) {
                Ok(device) => Some(device),
                Err(error) => return Response::err(error),
            },
            None => None,
        };

        // A device change also swaps in that output's DSP profile so the new
        // route never inherits another interface's chain. Resolved before
        // the state lock: the profile store is a leaf lock and must not
        // nest inside system_state.
        let route_profile = match resolved_output_device.clone() {
            Some(device) => {
                let previous = self.system_state.lock().desired_spec().output_device;
                let applied = self.system_state.lock().applied_output_device();
                if previous.as_deref() == Some(device.as_str())
                    && applied.as_deref() == Some(device.as_str())
                {
                    None
                } else {
                    match self.resolve_route_profile(None, &device, None) {
                        Ok(profile) => Some(profile),
                        Err(error) => return Response::err(error),
                    }
                }
            }
            None => None,
        };

        let plan = {
            let state = self.system_state.lock();
            let mut next = state.desired_spec();
            if let Some(channels) = input_channels {
                next.input_channels = channels;
            }
            if let Some(channels) = output_channels {
                next.output_channels = channels;
            }
            if let Some(device) = resolved_output_device.clone() {
                next.output_device = Some(device);
            }
            if let Some(profile) = &route_profile {
                next.input_channels = input_channels.unwrap_or(profile.input_channels);
                next.output_channels = output_channels.unwrap_or(profile.output_channels);
                next.user_plugins = profile.plugins.clone();
                next.user_graph = profile.graph.clone();
            }
            match state.prepare_from_spec(next, fallback_input_channels) {
                Ok(plan) => plan,
                Err(error) => return Response::err(error),
            }
        };

        if let Some(device) = plan.spec.output_device.as_deref() {
            let lab_backend = self.driver_manager.lock().output_sink_type() == SinkType::LabNull;
            let max_channels = match self.device_registry.lock().max_output_channels(
                device,
                requested_sample_rate,
                lab_backend,
            ) {
                Ok(channels) => channels,
                Err(error) => return Response::err(error),
            };
            if plan.spec.output_channels > max_channels {
                return Response::err(format!(
                    "Output device '{}' supports at most {} channels at {}Hz, but configuration requires {}",
                    sotf_audio::devices::strip_asio_prefix(device),
                    max_channels,
                    requested_sample_rate,
                    plan.spec.output_channels
                ));
            }
        }

        let requested_input_channels = plan.spec.input_channels;
        let requested_output_channels = plan.spec.output_channels;
        let requested_output_device = plan.spec.output_device.clone();
        let intent = if output_device.is_some() {
            PipelineApplyIntent::SelectRoute
        } else if sample_rate.is_some() || buffer_frames.is_some() {
            PipelineApplyIntent::ReconfigureDriver
        } else {
            PipelineApplyIntent::EnsurePlaying
        };
        let response = self.apply_pipeline_transaction(
            plan,
            driver_status,
            requested_sample_rate,
            requested_buffer_frames,
            intent,
        );
        if !response.success {
            // A failed apply can still restart and commit the previous plan.
            // Return that recovery generation so optimistic clients do not
            // issue their next mutation against the pre-recovery generation.
            let generation = self.system_state.lock().generation();
            return Response {
                data: Some(serde_json::json!({ "generation": generation })),
                ..response
            };
        }

        if output_device.is_some()
            && let Some(device) = requested_output_device.as_deref()
            && let Err(error) = persist_output_device(device)
        {
            log::error!(
                "Output device '{}' active but could not be persisted for the next daemon start: {}",
                device,
                error
            );
        }
        if let (Some(profile), Some(device)) =
            (route_profile.as_ref(), requested_output_device.as_deref())
            && let Err(error) = self.output_profiles.lock().record_route_if_active(
                None,
                device,
                &profile.id,
                self.system_state.lock().applied_output_device().as_deref(),
            )
        {
            log::warn!("Output device applied but profile memory failed: {error}");
        }

        if route_profile.is_some() || input_channels.is_some() || output_channels.is_some() {
            self.sync_live_chain_to_current_profile(true);
        }
        let applied_driver = self.driver_manager.lock().status();
        let (processing_rate, processing_frames) = self.processing_timing(&applied_driver);
        let actual = self.system_state.lock().applied_spec();
        let generation = self.system_state.lock().generation();
        Response::ok(serde_json::json!({
            "generation": generation,
            // Preserve the legacy set_sample_rate/set_buffer_frames response
            // fields while exposing the richer transactional result.
            "sample_rate": requested_sample_rate,
            "buffer_frames": requested_buffer_frames,
            "requested": {
                "sample_rate": requested_sample_rate,
                "buffer_frames": requested_buffer_frames,
                "input_channels": requested_input_channels,
                "output_channels": requested_output_channels,
                "output_device": requested_output_device,
            },
            "applied": {
                "sample_rate": processing_rate,
                "buffer_frames": processing_frames,
                "capture_sample_rate": applied_driver.sample_rate,
                "capture_buffer_frames": applied_driver.buffer_frames,
                "input_channels": applied_driver.channel_count,
                "output_channels": actual.as_ref().map(|spec| spec.output_channels),
                "output_device": actual.as_ref().and_then(|spec| spec.output_device.as_ref()),
            },
            "negotiated": applied_driver.sample_rate != requested_sample_rate
                || applied_driver.buffer_frames != requested_buffer_frames
                || applied_driver.channel_count != requested_input_channels as u32,
        }))
    }

    pub(super) fn handle_set_sample_rate(&self, rate: u32) -> Response {
        self.handle_apply_configuration(Some(rate), None, None, None, None)
    }

    pub(super) fn handle_set_buffer_frames(&self, frames: u32) -> Response {
        self.handle_apply_configuration(None, Some(frames), None, None, None)
    }

    pub(super) fn handle_get_driver_config(&self) -> Response {
        let driver = self.driver_manager.lock();
        let status = driver.status();
        // Mutated only by the HAL enrichment below.
        #[cfg_attr(not(all(target_os = "macos", feature = "hal")), allow(unused_mut))]
        let mut wire = DriverConfigWire::from(&status);

        // The negotiation state lives in the shared header, not in
        // DriverStatus; read it like handle_encryption_status does so the
        // client's negotiation row can actually display. Without this the
        // fields stayed 0 forever and the row never appeared.
        #[cfg(all(target_os = "macos", feature = "hal"))]
        if let Ok(buffer) = driver_hal::SharedAudioBuffer::open_default() {
            wire.config_status = buffer.config_status();
            wire.config_source = buffer.config_source();
        }

        match serde_json::to_value(wire) {
            Ok(data) => Response::ok(data),
            Err(error) => Response::err(format!("Failed to serialize driver config: {error}")),
        }
    }

    pub(super) fn handle_client(&self, mut stream: IpcStream, peer_class: PeerClass) {
        if let Err(e) = stream.set_read_timeout(Some(std::time::Duration::from_secs(
            super::consts::IPC_CLIENT_IDLE_TIMEOUT_SECS,
        ))) {
            log::warn!("Failed to set IPC client idle timeout: {}", e);
        }
        if let Err(e) = stream.set_write_timeout(Some(std::time::Duration::from_secs(
            super::consts::IPC_CLIENT_WRITE_TIMEOUT_SECS,
        ))) {
            log::warn!("Failed to set IPC client write timeout: {}", e);
        }
        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to clone stream for reading: {}", e);
                return;
            }
        };
        let mut reader = BufReader::new(reader_stream);
        let mut line = Vec::new();

        loop {
            match read_ipc_line_bounded(&mut reader, &mut line) {
                Ok(IpcLine::Eof) => break,
                Ok(IpcLine::Empty) => continue,
                Ok(IpcLine::TooLarge) => {
                    let response = Response::err("Request too large");
                    let json = serialize_response_safely(&response);
                    let _ = writeln!(stream, "{}", json);
                    break;
                }
                Ok(IpcLine::InvalidUtf8) => {
                    let response = Response::err("Invalid UTF-8 in command");
                    let json = serialize_response_safely(&response);
                    if let Err(e) = writeln!(stream, "{}", json) {
                        log::error!("Failed to write response: {}", e);
                        break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    log::debug!("Closing idle IPC client after read timeout");
                    break;
                }
                Err(e) => {
                    log::warn!("IPC client read failed: {}", e);
                    break;
                }
                Ok(IpcLine::Line(command_line)) => {
                    let mut command_telemetry = None;
                    let command_started = Instant::now();
                    // `base_generation` is a protocol-wide concurrency token.
                    // Serde enum variants intentionally remain backwards
                    // compatible, so extract it before decoding the command.
                    let base_generation = serde_json::from_str::<Value>(&command_line)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("base_generation")
                                .and_then(serde_json::Value::as_u64)
                        });
                    let response = match serde_json::from_str::<Command>(&command_line) {
                        Ok(cmd) => {
                            let command_name = cmd.name();
                            // Defense-in-depth: gate which commands the
                            // peer's UID class may invoke. The macOS HAL
                            // (UID 202) is authenticated but should only
                            // be allowed to query status -- NOT issue
                            // arbitrary plugin loads, shutdowns, etc.
                            let response = if !peer_allows_command(peer_class, cmd.name()) {
                                log::warn!(
                                    "Rejecting command '{}' from peer class {:?}: not allowed",
                                    cmd.name(),
                                    peer_class
                                );
                                Response::err(format!(
                                    "Command '{}' not permitted for this peer",
                                    cmd.name()
                                ))
                            } else {
                                self.handle_command_at_generation(cmd, base_generation)
                            };
                            command_telemetry = Some(command_name);
                            response
                        }
                        Err(e) => Response::err(format!("Invalid command: {}", e)),
                    };

                    // Hot-path IPC writer: serialization can fail if a
                    // client managed to inject NaN / Infinity into a
                    // `Value::Number` via UpdatePlugin parameters that
                    // gets reflected back through get_plugins. Never
                    // panic the client thread -- emit a static, safe
                    // fallback instead.
                    let json = serialize_response_safely(&response);
                    let written = writeln!(stream, "{}", json);
                    if let Some(command_name) = command_telemetry {
                        self.runtime_telemetry.record_command(
                            command_name,
                            command_started.elapsed(),
                            json.len(),
                        );
                    }
                    if let Err(e) = written {
                        log::error!("Failed to write response: {}", e);
                        break;
                    }
                }
            }
        }
    }

    pub(super) fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        // External-plugin sandbox options must exist before the first chain
        // load (spawn_initial_driver_playback below).
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        super::plugin::install_plugin_sandbox();

        let socket_path = get_socket_path();

        // Ensure socket directory exists with secure permissions
        ensure_secure_socket_dir(&socket_path).map_err(|error| {
            format!(
                "failed to prepare daemon socket directory {}: {error}",
                socket_path.display()
            )
        })?;

        // Start driver config watcher thread
        let config_watcher = self.spawn_driver_config_watcher();

        // Bind the IPC transport. On Unix this is an `AF_UNIX` socket with
        // a TOCTOU-safe stale-socket discipline (try `bind()` first, only
        // unlink a positively confirmed stale socket -- never a regular
        // file, FIFO, or symlink; see `bind_unix_socket`). On Windows it is
        // a loopback TCP listener whose OS-assigned port is published in
        // the port file at `socket_path` (see `bind_windows_listener`).
        #[cfg(unix)]
        let listener = bind_unix_socket(&socket_path).map_err(|error| {
            format!(
                "failed to bind daemon socket {}: {error}",
                socket_path.display()
            )
        })?;
        #[cfg(windows)]
        let (listener, ipc_auth_token) = bind_windows_listener(&socket_path).map_err(|error| {
            format!(
                "failed to bind daemon socket {}: {error}",
                socket_path.display()
            )
        })?;
        println!("Audio daemon listening on {}", socket_path.display());

        // NOTE: the legacy `/tmp/autoeq_audio.sock` symlink that previous
        // versions of the daemon created on each startup has been
        // removed. `/tmp` is world-writable on macOS/Linux, and the prior
        // `remove_file(LEGACY_SOCKET_PATH)` would happily unlink whatever
        // a same-host attacker pre-staged at that path (regular file,
        // FIFO, symlink-to-/etc/passwd, etc.). The `SOTF_LEGACY_SOCKET`
        // opt-in still works for callers that *must* use the legacy
        // path: they get a real socket bound at `LEGACY_SOCKET_PATH`,
        // not a symlink. New clients should use `get_secure_socket_path`.
        let _ = LEGACY_SOCKET_PATH; // keep the constant referenced

        // Accept connections (non-blocking so Ctrl-C can interrupt)
        listener.set_nonblocking(true)?;
        let initial_playback_thread = self.spawn_initial_driver_playback();
        let active_clients = Arc::new(AtomicUsize::new(0));
        #[cfg(windows)]
        let authenticating_clients = Arc::new(AtomicUsize::new(0));
        let mut client_threads: Vec<(std::thread::JoinHandle<()>, IpcStream)> = Vec::new();

        loop {
            if !*self.running.lock() {
                println!("Shutdown requested, exiting");
                break;
            }

            let mut client_index = 0;
            while client_index < client_threads.len() {
                if client_threads[client_index].0.is_finished() {
                    let (thread, _shutdown_stream) = client_threads.swap_remove(client_index);
                    if thread.join().is_err() {
                        log::warn!("IPC client handler panicked");
                    }
                } else {
                    client_index += 1;
                }
            }

            match crate::ipc_transport::wait_for_listener(&listener, Duration::from_millis(100)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    log::error!("Failed to wait for IPC listener: {error}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
            match listener.accept() {
                Ok((stream, _addr)) => {
                    if let Err(e) = stream.set_nonblocking(false) {
                        log::error!("Failed to set client stream to blocking: {}", e);
                        continue;
                    }

                    let peer_class = match verify_peer_credentials(&stream) {
                        Ok(peer_uid) => {
                            let class = classify_peer(peer_uid, security_current_uid());
                            log::debug!(
                                "Accepted connection from UID {} (class {:?})",
                                peer_uid,
                                class
                            );
                            class
                        }
                        Err(e) => {
                            log::warn!("Rejected unauthorized connection: {}", e);
                            continue;
                        }
                    };

                    // Never read authentication bytes in the accept loop. A
                    // separate small admission budget bounds slow handshakes;
                    // retained shutdown streams also unblock these workers.
                    #[cfg(windows)]
                    let auth_slot = {
                        const MAX_PENDING_AUTHENTICATIONS: usize = 8;
                        if !try_acquire_slot(&authenticating_clients, MAX_PENDING_AUTHENTICATIONS) {
                            log::warn!("Rejecting IPC connection: authentication workers busy");
                            continue;
                        }
                        ClientSlot(Arc::clone(&authenticating_clients))
                    };

                    if !try_acquire_client_slot(&active_clients) {
                        log::warn!(
                            "Rejecting IPC client: maximum of {} active clients reached",
                            MAX_IPC_CLIENTS
                        );
                        continue;
                    }
                    let client_slot = ClientSlot(Arc::clone(&active_clients));

                    let (client_slot, shutdown_stream) =
                        match clone_client_shutdown_stream(client_slot, || stream.try_clone()) {
                            Ok(parts) => parts,
                            Err(error) => {
                                log::warn!("Failed to clone IPC client for shutdown: {error}");
                                continue;
                            }
                        };
                    let daemon = self.clone();
                    #[cfg(windows)]
                    let expected_token = ipc_auth_token.clone();

                    let thread = std::thread::spawn(move || {
                        let _client_slot = client_slot;
                        #[cfg(windows)]
                        {
                            let authenticated =
                                crate::ipc_transport::authenticate_client(&stream, &expected_token);
                            drop(auth_slot);
                            if let Err(error) = authenticated {
                                log::warn!("Rejected unauthenticated IPC client: {error}");
                                return;
                            }
                        }
                        elevate_daemon_thread_to_audio_work("IPC client handler");
                        daemon.handle_client(stream, peer_class);
                    });
                    client_threads.push((thread, shutdown_stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Readiness can be consumed before accept; wait again.
                }
                Err(e) => {
                    // A persistent listener error must not turn the accept
                    // loop into a hot spin. Keep the daemon responsive to
                    // shutdown while applying a small bounded backoff.
                    log::error!("Failed to accept connection: {}", e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }

        // Actively unblock and join every daemon-owned client handler. Persistent
        // polling connections otherwise outlive the accept loop until their
        // idle timeout and can retain daemon resources during restart.
        for (_thread, stream) in &client_threads {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for (thread, _shutdown_stream) in client_threads {
            if thread.join().is_err() {
                log::warn!("IPC client handler panicked during shutdown");
            }
        }
        join_initial_playback_thread(initial_playback_thread);

        // Cleanup -- only remove our own socket entry, after re-verifying
        // it is still a socket. We deliberately do NOT unlink the legacy
        // `/tmp/autoeq_audio.sock` here: if it exists and is not ours,
        // it's not our business to remove (avoid the prior TOCTOU /
        // symlink-following hazard at shutdown).
        if socket_is_unix_socket(&socket_path) {
            let _ = std::fs::remove_file(&socket_path);
        }

        let _ = config_watcher.join();

        Ok(())
    }
}
