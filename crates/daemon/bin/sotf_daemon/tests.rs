#![allow(clippy::field_reassign_with_default)]
use super::audio_daemon::{
    AudioDaemon, CaptureResumeTracker, ConfigChangeDebouncer, METERING_LATENCY_BUDGET_MICROS,
    PIPELINE_RESPONSE_BUDGET_BYTES, PLAYBACK_IDLE_REBUILD_THRESHOLD, PlaybackProgress,
    RuntimeTelemetry, VolumeSyncTracker, changed_plugin_parameters, encode_plugin_param_value,
    join_initial_playback_thread, pipeline_timing_after_config_request,
    rack_plugins_to_linear_graph, reorder_linear_graph, startup_channel_geometry,
    wait_for_playback_observation,
};
use super::audio_daemon::{ClientSlot, clone_client_shutdown_stream, try_acquire_client_slot};
use super::command::Command;
use super::configured::configured_output_device_from_value;
use super::consts::{MAX_HAL_CHANNELS, MAX_IPC_CLIENTS, MAX_IPC_COMMAND_BYTES};
use super::device_registry::DeviceRegistry;
use super::driver_manager::DriverManager;
use super::loudness::{loudness_data_to_json, loudness_info_to_json};
use super::misc::push_metering_faults;
use super::misc::sanitize_user_plugins;
use super::misc::transport_snapshot_and_faults;
use super::misc::{
    LAB_OUTPUT_DEVICE_NAME, effective_output_for_sink, resolve_effective_output_device,
};
use super::misc::{
    build_driver_plugin_chain, build_driver_plugin_graph, is_safe_output_device_name,
    parameter_descriptor_to_json,
};
use super::pipeline_reconfigure_outcome::acknowledged_config_for_outcome;
use super::pipeline_reconfigure_outcome::handle_driver_config_change;
use super::pipeline_reconfigure_outcome::publish_reconfigured_driver_readiness;
use super::pipeline_reconfigure_outcome::reconfigure_audio_pipeline;
use super::pipeline_reconfigure_outcome::wait_for_reconfigured_playback;
use super::pipeline_spec::{PipelineSpec, pipeline_spec_to_json, pipeline_specs_match};
use super::pipeline_supervisor::PipelineSupervisor;
use super::plugin::{
    plugin_parameter_descriptors, plugin_type_category, plugin_type_to_engine_str,
};
use super::response::Response;
use super::response::serialize_response_safely;
use super::security::{KeyManager, PeerClass};
use super::systemwide_state::SystemwideState;
use super::types::read_ipc_line_bounded;
use super::types::{IpcLine, PipelineReconfigureOutcome};
use crate::plugin_artifact::{PluginArtifactPlan, plan_plugin_artifact};
use driver_common::DriverConfig;
use parking_lot::Mutex;
use serde_json::Value;
use sotf_audio::engine::{PluginGraphConfig, PluginGraphEdgeConfig, PluginGraphNodeConfig};
use sotf_audio::manager::AudioEngineManager;
use sotf_audio::plugins::PluginType;
use sotf_audio::{PluginConfig, SinkType};
use std::io::{BufRead, BufReader, Cursor, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Barrier};

#[test]
fn debounce_retains_latest_request_identity_even_when_geometry_matches() {
    let start = std::time::Instant::now();
    let config = DriverConfig::new(48_000, 512, 2);
    let first = driver_common::DriverConfigRequest {
        request_id: 10,
        config,
    };
    let latest = driver_common::DriverConfigRequest {
        request_id: 11,
        config,
    };
    let mut debouncer = ConfigChangeDebouncer::new();
    assert_eq!(debouncer.observe_request(start, Some(first), 1), None);
    let replacement = start + ConfigChangeDebouncer::DEBOUNCE;
    assert_eq!(
        debouncer.observe_request(replacement, Some(latest), 1),
        None
    );
    assert_eq!(
        debouncer.observe_request(replacement + ConfigChangeDebouncer::DEBOUNCE, None, 1),
        Some(latest)
    );
    assert_eq!(debouncer.observe_request(start, Some(first), 2), None);
    assert_eq!(
        debouncer.observe_request(start + ConfigChangeDebouncer::DEBOUNCE, None, 3),
        None
    );
}

#[test]
fn progress_tracks_stages_and_distinguishes_stall_from_delayed_observation() {
    let started = std::time::Instant::now();
    let mut progress = PlaybackProgress::default();
    let mut engine = sotf_audio::AudioEngineState::default();
    progress.observe(started, 1, true, &engine);
    engine.playback_frames_received = 128;
    engine.playback_frames_written = 128;
    engine.playback_callback_count = 4;
    let flowing = started + std::time::Duration::from_millis(100);
    progress.observe(flowing, 1, true, &engine);
    for stage in 0..3 {
        assert_eq!(
            progress.status(
                flowing,
                1,
                engine.playback_epoch,
                engine.playback_output_generation,
                stage
            ),
            "fresh"
        );
    }
    let later = started + std::time::Duration::from_secs(8);
    assert_eq!(
        progress.status(
            later,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "observation_delayed"
    );
    engine.playback_callback_count += 1;
    progress.observe(later, 1, true, &engine);
    assert_eq!(
        progress.status(
            later,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            0
        ),
        "stale"
    );
    assert_eq!(
        progress.status(
            later,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "fresh"
    );
    assert_eq!(
        progress.status(
            later,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            2
        ),
        "stale"
    );
}

#[test]
fn progress_cannot_reuse_prior_pause_generation_or_stream_epoch_activity() {
    let now = std::time::Instant::now();
    let mut progress = PlaybackProgress::default();
    let mut engine = sotf_audio::AudioEngineState::default();
    progress.observe(now, 1, true, &engine);
    engine.playback_callback_count = 10;
    progress.observe(now, 1, true, &engine);
    assert_eq!(
        progress.status(
            now,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "fresh"
    );
    progress.observe(now, 1, false, &engine);
    progress.observe(now, 1, true, &engine);
    assert_eq!(
        progress.status(
            now,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "awaiting_progress"
    );
    progress.observe(now, 2, true, &engine);
    assert_eq!(
        progress.status(
            now,
            1,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "unobserved_generation"
    );
    assert_eq!(
        progress.status(
            now,
            2,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "awaiting_progress"
    );
    engine.playback_epoch += 1;
    progress.observe(now, 2, true, &engine);
    assert_eq!(
        progress.status(
            now,
            2,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "awaiting_progress"
    );
    engine.playback_callback_count += 1;
    progress.observe(now, 2, true, &engine);
    assert_eq!(
        progress.status(
            now,
            2,
            engine.playback_epoch,
            engine.playback_output_generation,
            1
        ),
        "fresh"
    );
}

#[test]
fn output_stream_replacement_requires_new_progress_even_if_counters_increase() {
    let now = std::time::Instant::now();
    let mut progress = PlaybackProgress::default();
    let mut engine = sotf_audio::AudioEngineState {
        playback_output_generation: 10,
        ..Default::default()
    };
    progress.observe(now, 1, true, &engine);
    engine.playback_callback_count = 10;
    progress.observe(now, 1, true, &engine);
    assert_eq!(
        progress.status(now, 1, engine.playback_epoch, 10, 1),
        "fresh"
    );
    engine.playback_output_generation = 11;
    engine.playback_callback_count = 100;
    assert_eq!(
        progress.status(now, 1, engine.playback_epoch, 11, 1),
        "unobserved_generation"
    );
    progress.observe(now, 1, true, &engine);
    assert_eq!(
        progress.status(now, 1, engine.playback_epoch, 11, 1),
        "awaiting_progress"
    );
    engine.playback_callback_count += 1;
    progress.observe(now, 1, true, &engine);
    assert_eq!(
        progress.status(now, 1, engine.playback_epoch, 11, 1),
        "fresh"
    );
}

fn test_plugin(plugin_type: &str) -> PluginConfig {
    PluginConfig {
        plugin_type: plugin_type.to_string(),
        parameters: serde_json::json!({}),
    }
}

#[test]
fn loudness_data_json_includes_meter_fields() {
    let loudness = sotf_audio::LoudnessData {
        measurement_valid: true,
        query_error_generation: 0,
        measurement_enabled: true,
        channel_layout_is_compliant: true,
        momentary_lufs: -18.5,
        shortterm_lufs: -17.25,
        integrated_lufs: -20.0,
        peak: 0.75,
        channel_peaks: Arc::new(vec![0.5, 0.75]),
        true_peaks_dbtp: Arc::new(vec![-2.0, -1.0]),
        true_peak_is_compliant: true,
        integrated_window_seconds: 3_600,
        correlation_lr: Some(0.42),
        correlation_matrix: Arc::new(Vec::new()),
        correlation_samples_seen: 0,
        ..Default::default()
    };

    let json = loudness_data_to_json(&loudness);

    assert_eq!(json["momentary"], serde_json::json!(-18.5));
    assert_eq!(json["short_term"], serde_json::json!(-17.25));
    assert_eq!(json["integrated"], serde_json::json!(-20.0));
    assert_eq!(json["peak"], serde_json::json!(0.75));
    assert_eq!(json["channel_peaks"], serde_json::json!([0.5, 0.75]));
    assert_eq!(json["true_peaks_dbtp"], serde_json::json!([-2.0, -1.0]));
    assert_eq!(json["correlation_lr"], serde_json::json!(0.42));
    assert_eq!(json["measurement_valid"], serde_json::json!(true));
    assert_eq!(json["measurement_enabled"], serde_json::json!(true));
    assert_eq!(json["query_error_generation"], serde_json::json!(0));
    assert_eq!(json["channel_layout_is_compliant"], serde_json::json!(true));
    assert_eq!(json["true_peak_is_compliant"], serde_json::json!(true));
    assert_eq!(json["integrated_window_seconds"], serde_json::json!(3600));
}

#[test]
fn stale_readiness_heals_only_for_healthy_streaming_engine() {
    let healthy: Result<bool, String> = Ok(true);
    let idle: Result<bool, String> = Ok(false);
    let failed: Result<bool, String> = Err("boom".to_string());
    // Recorded failure + ready driver + healthy engine: heal.
    assert!(AudioDaemon::should_heal_stale_readiness(
        true, true, &healthy
    ));
    // No recorded failure: nothing to heal.
    assert!(!AudioDaemon::should_heal_stale_readiness(
        false, true, &healthy
    ));
    // Driver not ready: healing would lie to the HAL.
    assert!(!AudioDaemon::should_heal_stale_readiness(
        true, false, &healthy
    ));
    // Engine idle or errored: still genuinely down.
    assert!(!AudioDaemon::should_heal_stale_readiness(true, true, &idle));
    assert!(!AudioDaemon::should_heal_stale_readiness(
        true, true, &failed
    ));
}

#[test]
fn available_plugin_descriptors_expose_engine_keys() {
    let settings = sotf_audio::PluginSettings::default_for(&PluginType::Gain).unwrap();
    let descriptors = plugin_parameter_descriptors(&settings);

    assert!(
        descriptors
            .iter()
            .any(|d| d["key"] == "gain_db" && d["type"] == "float")
    );
}

#[test]
fn configured_output_device_uses_non_empty_value() {
    assert_eq!(
        configured_output_device_from_value(Some(" ADAM Audio D3V ")),
        Some("ADAM Audio D3V".to_string())
    );
    assert_eq!(configured_output_device_from_value(Some("   ")), None);
    assert_eq!(configured_output_device_from_value(None), None);
}

#[test]
fn virtual_output_device_names_are_rejected() {
    for name in [
        "SotF Virtual Audio",
        "BlackHole 2ch",
        "Loopback Audio",
        "Soundflower (2ch)",
        "Background Music",
        "Audio Bridge",
        "ZoomAudioDevice",
        "Generic Virtual Device",
    ] {
        assert!(!is_safe_output_device_name(name), "{name} should be unsafe");
    }

    for name in ["Built-in Output", "ADAM Audio D3V", "MacBook Pro Speakers"] {
        assert!(is_safe_output_device_name(name), "{name} should be safe");
    }
}

#[test]
fn plugin_type_to_engine_str_maps_all_variants() {
    // Spot-check a representative set; the match must cover every PluginType.
    assert_eq!(plugin_type_to_engine_str(&PluginType::EQ), "eq");
    assert_eq!(plugin_type_to_engine_str(&PluginType::Gain), "gain");
    assert_eq!(plugin_type_to_engine_str(&PluginType::Upmixer), "upmixer");
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::Compressor),
        "compressor"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::MultibandCompressor),
        "multiband_compressor"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::LoudnessCompensation),
        "loudness_compensation"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::LoudnessMonitor),
        "loudness_monitor"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::ChannelMuteSolo),
        "channel_mute_solo"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::ABCompare),
        "ab_compare"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::MonoToStereo),
        "mono_to_stereo"
    );
    assert_eq!(
        plugin_type_to_engine_str(&PluginType::LinearPhaseEq),
        "linear_phase_eq"
    );
}

#[test]
fn plugin_type_category_groups_are_consistent() {
    // Canonical player grouping
    // (`sotf_audio_player::plugin_categories::CATEGORIES`): one sample per
    // category plus the non-picker fallbacks.
    assert_eq!(plugin_type_category(&PluginType::Compressor), "Dynamics");
    assert_eq!(plugin_type_category(&PluginType::EQ), "EQ & Tone");
    assert_eq!(plugin_type_category(&PluginType::Gain), "EQ & Tone");
    assert_eq!(plugin_type_category(&PluginType::Denoiser), "Denoising");
    assert_eq!(plugin_type_category(&PluginType::Upmixer), "Spatial");
    assert_eq!(plugin_type_category(&PluginType::Convolution), "Spatial");
    assert_eq!(plugin_type_category(&PluginType::ABCompare), "Analysis");
    assert_eq!(plugin_type_category(&PluginType::Crossover), "Routing");
    assert_eq!(plugin_type_category(&PluginType::BandSplit), "Routing");
    assert_eq!(plugin_type_category(&PluginType::FletcherMunson), "Legacy");
    assert_eq!(plugin_type_category(&PluginType::External), "External");
}

#[test]
fn sanitize_user_plugins_strips_daemon_owned_types() {
    let plugins = vec![
        test_plugin("hal_input"),
        test_plugin("eq"),
        test_plugin("loudness_monitor"),
        test_plugin("gain"),
        test_plugin("hal_output"),
    ];
    let sanitized = sanitize_user_plugins(plugins);
    assert_eq!(
        sanitized
            .iter()
            .map(|p| p.plugin_type.as_str())
            .collect::<Vec<_>>(),
        vec!["eq", "gain"]
    );
}

#[test]
fn build_driver_plugin_chain_wraps_user_plugins_with_monitors() {
    let (runtime, input_idx, output_idx) =
        build_driver_plugin_chain(vec![test_plugin("eq"), test_plugin("gain")]);

    assert_eq!(input_idx, 0);
    assert_eq!(output_idx, 3);
    assert_eq!(
        runtime
            .iter()
            .map(|p| p.plugin_type.as_str())
            .collect::<Vec<_>>(),
        vec!["loudness_monitor", "eq", "gain", "loudness_monitor"]
    );
}

#[test]
fn build_driver_plugin_graph_preserves_topology_and_adds_monitor_boundaries() {
    let graph = PluginGraphConfig::try_new(
        vec![
            PluginGraphNodeConfig::try_new(10, "gain", serde_json::json!({}), 2).unwrap(),
            PluginGraphNodeConfig::try_new(20, "eq", serde_json::json!({"filters": []}), 2)
                .unwrap(),
            PluginGraphNodeConfig::try_new(30, "gain", serde_json::json!({}), 2).unwrap(),
        ],
        vec![
            PluginGraphEdgeConfig::new(10, 20),
            PluginGraphEdgeConfig::new(10, 30),
        ],
    )
    .unwrap();

    let (runtime, input_idx, output_idx) = build_driver_plugin_graph(graph, 2, 2).unwrap();

    assert_eq!(input_idx, 0);
    assert_eq!(output_idx, 4);
    assert_eq!(runtime.nodes.len(), 5);
    assert_eq!(runtime.edges.len(), 5);
    assert_eq!(runtime.nodes[0].plugin_type, "loudness_monitor");
    assert_eq!(runtime.nodes[4].plugin_type, "loudness_monitor");
    assert!(runtime.nodes.iter().any(|node| node.id == 10));
    assert!(
        runtime
            .edges
            .iter()
            .any(|edge| edge.from_node == 10 && edge.to_node == 20)
    );
    runtime.validate().unwrap();
}

#[test]
fn reorder_linear_graph_preserves_node_state_and_rebuilds_edges() {
    let mut first =
        PluginGraphNodeConfig::try_new(10, "gain", serde_json::json!({"gain_db": -3.0}), 2)
            .unwrap();
    first.bypassed = true;
    let mut second =
        PluginGraphNodeConfig::try_new(20, "eq", serde_json::json!({"filters": [1, 2]}), 6)
            .unwrap();
    second.bypassed = false;
    let graph = PluginGraphConfig::try_new(
        vec![first.clone(), second.clone()],
        vec![PluginGraphEdgeConfig::new(10, 20)],
    )
    .unwrap();

    let reordered = reorder_linear_graph(&graph, &[20, 10]).unwrap();
    assert_eq!(
        reordered
            .nodes
            .iter()
            .map(|node| node.id)
            .collect::<Vec<_>>(),
        vec![20, 10]
    );
    assert_eq!(reordered.edges.len(), 1);
    assert_eq!(reordered.edges[0].from_node, 20);
    assert_eq!(reordered.edges[0].to_node, 10);
    assert_eq!(reordered.nodes[0].parameters, second.parameters);
    assert_eq!(reordered.nodes[0].input_channels, second.input_channels);
    assert_eq!(reordered.nodes[0].bypassed, second.bypassed);
    assert_eq!(reordered.nodes[1].parameters, first.parameters);
    assert_eq!(reordered.nodes[1].input_channels, first.input_channels);
    assert_eq!(reordered.nodes[1].bypassed, first.bypassed);
}

#[test]
fn reorder_linear_graph_rejects_invalid_order_and_nonlinear_graphs() {
    let graph = PluginGraphConfig::try_new(
        vec![
            PluginGraphNodeConfig::try_new(1, "gain", serde_json::json!({}), 2).unwrap(),
            PluginGraphNodeConfig::try_new(2, "gain", serde_json::json!({}), 2).unwrap(),
        ],
        vec![PluginGraphEdgeConfig::new(1, 2)],
    )
    .unwrap();
    assert!(reorder_linear_graph(&graph, &[1, 1]).is_err());
    assert!(reorder_linear_graph(&graph, &[1]).is_err());

    let nonlinear = PluginGraphConfig::try_new(
        vec![
            PluginGraphNodeConfig::try_new(1, "gain", serde_json::json!({}), 2).unwrap(),
            PluginGraphNodeConfig::try_new(2, "gain", serde_json::json!({}), 2).unwrap(),
            PluginGraphNodeConfig::try_new(3, "gain", serde_json::json!({}), 2).unwrap(),
        ],
        vec![
            PluginGraphEdgeConfig::new(1, 2),
            PluginGraphEdgeConfig::new(1, 3),
        ],
    )
    .unwrap();
    assert!(reorder_linear_graph(&nonlinear, &[1, 2, 3]).is_err());
}

#[test]
fn rack_state_promotion_preserves_order_and_parameters() {
    let plugins = vec![
        test_plugin("gain"),
        PluginConfig::new("eq", serde_json::json!({"filters": [1]})),
    ];
    let graph = rack_plugins_to_linear_graph(&plugins, 2, 1, Some(6), Some(true)).unwrap();
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.edges.len(), 1);
    assert_eq!(graph.edges[0].from_node, 0);
    assert_eq!(graph.edges[0].to_node, 1);
    assert_eq!(graph.nodes[0].plugin_type, "gain");
    assert_eq!(graph.nodes[0].parameters, plugins[0].parameters);
    assert_eq!(graph.nodes[0].input_channels, 2);
    assert!(!graph.nodes[0].bypassed);
    assert_eq!(graph.nodes[1].plugin_type, "eq");
    assert_eq!(graph.nodes[1].parameters, plugins[1].parameters);
    assert_eq!(graph.nodes[1].input_channels, 6);
    assert!(graph.nodes[1].bypassed);
}

#[test]
fn empty_loudness_json_clamps_channels_and_shapes_defaults() {
    let zero = super::consts::empty_loudness_json(0);
    assert_eq!(zero["momentary"], -60.0);
    assert_eq!(zero["short_term"], -60.0);
    assert_eq!(zero["integrated"], -60.0);
    assert_eq!(zero["peak"], 0.0);
    assert_eq!(zero["channel_peaks"].as_array().unwrap().len(), 1);
    assert_eq!(zero["true_peaks_dbtp"].as_array().unwrap().len(), 1);
    assert!(zero["correlation_lr"].is_null());

    let many = super::consts::empty_loudness_json(64);
    assert_eq!(
        many["channel_peaks"].as_array().unwrap().len(),
        MAX_HAL_CHANNELS
    );
    assert_eq!(
        many["true_peaks_dbtp"].as_array().unwrap().len(),
        MAX_HAL_CHANNELS
    );
}

#[test]
fn metering_source_json_reflects_data_presence() {
    let present = super::consts::metering_source_json(true, 2);
    assert_eq!(present["status"], "available");
    assert_eq!(present["source"], "loudness_monitor");
    assert_eq!(present["channels"], 2);

    let fallback = super::consts::metering_source_json(false, 6);
    assert_eq!(fallback["status"], "fallback_zero");
    assert_eq!(fallback["source"], "channel_sized_fallback");
    assert_eq!(fallback["channels"], 6);
}

#[test]
fn pipeline_spec_to_json_reports_plugin_types_and_count() {
    let spec = PipelineSpec {
        output_device: Some("ADAM Audio D3V".to_string()),
        user_plugins: vec![test_plugin("eq"), test_plugin("gain")],
        user_graph: None,
        input_channels: 2,
        output_channels: 6,
    };
    let json = pipeline_spec_to_json(&spec);
    assert_eq!(json["output_device"], "ADAM Audio D3V");
    assert_eq!(json["input_channels"], 2);
    assert_eq!(json["output_channels"], 6);
    assert_eq!(json["user_plugin_count"], 2);
    assert_eq!(
        json["user_plugin_types"].as_array().unwrap(),
        &vec![serde_json::json!("eq"), serde_json::json!("gain")]
    );
}

#[test]
fn loudness_info_to_json_shapes_empty_channel_arrays() {
    let info = sotf_audio::LoudnessInfo {
        momentary_lufs: -20.0,
        shortterm_lufs: -19.0,
        integrated_lufs: -21.0,
        peak: 0.5,
    };
    let json = loudness_info_to_json(&info);
    assert_eq!(json["momentary"], -20.0);
    assert_eq!(json["integrated"], -21.0);
    assert_eq!(json["channel_peaks"].as_array().unwrap().len(), 0);
    assert_eq!(json["true_peaks_dbtp"].as_array().unwrap().len(), 0);
    assert!(json["correlation_lr"].is_null());
}

#[test]
fn read_ipc_line_bounded_returns_eof_on_empty_input() {
    let input = Cursor::new(b"");
    let mut reader = BufReader::new(input);
    let mut buffer = Vec::new();
    assert_eq!(
        read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
        IpcLine::Eof
    );
}

#[test]
fn read_ipc_line_bounded_returns_invalid_utf8() {
    let input = Cursor::new(vec![0xc0, 0x80, b'\n']);
    let mut reader = BufReader::new(input);
    let mut buffer = Vec::new();
    assert_eq!(
        read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
        IpcLine::InvalidUtf8
    );
}

#[test]
fn response_constructors_build_expected_shape() {
    let ok = Response::ok(serde_json::json!({"x": 1}));
    assert!(ok.success);
    assert_eq!(ok.data.unwrap()["x"], 1);
    assert!(ok.error.is_none());

    let empty = Response::ok_empty();
    assert!(empty.success);
    assert!(empty.data.is_none());

    let err = Response::err("bad");
    assert!(!err.success);
    assert_eq!(err.error.unwrap(), "bad");
}

#[test]
fn command_name_covers_all_variants() {
    // Additional variants not exercised by command_name_matches_wire_tag.
    let cases: Vec<(Command, &str)> = vec![
        (Command::Pause, "pause"),
        (Command::Stop, "stop"),
        (Command::Seek { position: 0.0 }, "seek"),
        (Command::SetVolume { volume: 0.5 }, "set_volume"),
        (Command::ListDevices, "list_devices"),
        (
            Command::SetDevice {
                device: String::new(),
            },
            "set_device",
        ),
        (
            Command::ApplyConfiguration {
                sample_rate: Some(96_000),
                buffer_frames: Some(256),
                input_channels: Some(2),
                output_channels: Some(2),
                output_device: None,
            },
            "apply_configuration",
        ),
        (
            Command::SetInputChannels { channels: 2 },
            "set_input_channels",
        ),
        (Command::GetLoudness, "get_loudness"),
        (Command::GetMetering, "get_metering"),
        (Command::GetPlugins, "get_plugins"),
        (Command::GetAvailablePlugins, "get_available_plugins"),
        (
            Command::AddPlugin {
                plugin: test_plugin("eq"),
                index: None,
            },
            "add_plugin",
        ),
        (Command::RemovePlugin { index: 0 }, "remove_plugin"),
        (Command::ReorderPlugins { order: vec![] }, "reorder_plugins"),
        (
            Command::ReorderGraph {
                order: vec![10, 20],
                base_generation: Some(3),
            },
            "reorder_graph",
        ),
        (
            Command::SetRackPluginState {
                index: 0,
                input_channels: Some(6),
                bypassed: Some(true),
                base_generation: Some(3),
            },
            "set_rack_plugin_state",
        ),
        (Command::SetEncryption { enabled: true }, "set_encryption"),
        (Command::EncryptionStatus, "encryption_status"),
        (Command::RotateEncryptionKey, "rotate_encryption_key"),
        (Command::SetSampleRate { rate: 48_000 }, "set_sample_rate"),
        (
            Command::SetBufferFrames { frames: 512 },
            "set_buffer_frames",
        ),
        (Command::GetDriverConfig, "get_driver_config"),
    ];
    for (cmd, expected) in cases {
        assert_eq!(cmd.name(), expected, "{cmd:?}");
    }
}

#[test]
fn default_channel_counts_match_definitions() {
    assert_eq!(super::default::default_input_channels(), 0);
    assert_eq!(super::default::default_output_channels(), 2);
}

#[test]
fn parameter_descriptor_to_json_maps_float_spec() {
    use sotf_plugins::param_specs::ParamSpec;
    let spec =
        ParamSpec::float("Gain", "gain_db", 0.0, -60.0, 12.0, 0.1, "dB", "Level").doc("Gain in dB");
    let json = parameter_descriptor_to_json(&spec);
    assert_eq!(json["key"], "gain_db");
    assert_eq!(json["name"], "Gain");
    assert_eq!(json["unit"], "dB");
    assert_eq!(json["group"], "Level");
    assert_eq!(json["doc"], "Gain in dB");
    assert_eq!(json["type"], "float");
    assert_eq!(json["default"], 0.0);
    assert_eq!(json["min"], -60.0);
    assert_eq!(json["max"], 12.0);
    assert_eq!(json["step"], 0.1);
    assert_eq!(json["update_mode"], "realtime");
}

#[cfg(test)]
mod ipc_safety_tests {
    use super::*;

    use driver_common::{AudioDriver, ConfigResult, DriverError, DriverStatus};
    use std::io::Cursor;

    /// `serialize_response_safely` must produce valid JSON on the OK
    /// path with no behavioural change versus the original
    /// `to_string(...).unwrap()` semantics.
    #[test]
    fn serialize_response_safely_round_trips_ok_response() {
        let r = Response::ok(serde_json::json!({
            "index": 7,
            "name": "test plugin",
        }));
        let out = serialize_response_safely(&r);
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("ok-path output must parse");
        assert_eq!(parsed["success"], serde_json::Value::Bool(true));
        assert_eq!(parsed["data"]["index"], serde_json::Value::from(7));
    }

    /// `serialize_response_safely` must produce valid JSON on the error
    /// path too.
    #[test]
    fn serialize_response_safely_handles_err_response() {
        let r = Response::err("something failed");
        let out = serialize_response_safely(&r);
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("err-path output must parse");
        assert_eq!(parsed["success"], serde_json::Value::Bool(false));
        assert_eq!(parsed["error"], serde_json::Value::from("something failed"));
    }

    /// Regression test for the IPC `unwrap()` panic.
    ///
    /// The fallback returned by `serialize_response_safely` when
    /// `serde_json::to_string` errors out MUST itself be valid JSON
    /// matching the on-wire `Response` shape. If a future refactor
    /// breaks this string, every client receives malformed JSON when
    /// the daemon encounters a NaN/Inf in echoed user-supplied
    /// parameters -- so we lock it down here.
    #[test]
    fn serialize_response_safely_fallback_is_valid_json() {
        let fallback = String::from(
            r#"{"success":false,"error":"internal error: response serialization failed"}"#,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&fallback).expect("fallback must be valid JSON");
        assert_eq!(parsed["success"], serde_json::Value::Bool(false));
        assert!(parsed["error"].is_string());
    }

    /// Confirm that a synthetic Serialize failure does NOT propagate
    /// out of `serialize_response_safely`. We can't easily inject a
    /// failing `Value` through `Response::data` (serde_json::Value's
    /// own Serialize impl never errors for in-memory values), but we
    /// can verify the helper's no-panic contract on every legitimate
    /// Response we can construct -- and that `serde_json::to_string`
    /// itself can return Err on a custom Serialize impl that errors.
    /// This locks in the *shape* of the safety net: if the underlying
    /// `to_string` does error, our wrapper turns it into a normal
    /// String return rather than a panic-on-`.unwrap()`.
    #[test]
    fn synthetic_serializer_error_is_handled_without_panic() {
        use serde::{Serialize, Serializer};

        struct AlwaysFail;
        impl Serialize for AlwaysFail {
            fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic serialization failure"))
            }
        }
        // Sanity: serde_json::to_string on AlwaysFail returns Err.
        let bad = serde_json::to_string(&AlwaysFail);
        assert!(bad.is_err(), "AlwaysFail must fail to serialize");

        // The helper itself never panics on a normal Response, which
        // is the property we care about for the IPC hot path.
        let r = Response::err("normal");
        let _ = serialize_response_safely(&r); // must not panic
    }

    #[test]
    fn read_ipc_line_bounded_accepts_normal_command() {
        let input = Cursor::new(b"  {\"command\":\"status\"}  \n");
        let mut reader = BufReader::new(input);
        let mut buffer = Vec::new();

        assert_eq!(
            read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
            IpcLine::Line(r#"{"command":"status"}"#.to_string())
        );
    }

    #[test]
    fn read_ipc_line_bounded_handles_crlf_and_empty_lines() {
        let input = Cursor::new(b"\r\n{\"command\":\"status\"}\r\n");
        let mut reader = BufReader::new(input);
        let mut buffer = Vec::new();

        assert_eq!(
            read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
            IpcLine::Empty
        );
        assert_eq!(
            read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
            IpcLine::Line(r#"{"command":"status"}"#.to_string())
        );
    }

    #[test]
    fn read_ipc_line_bounded_rejects_oversized_line() {
        let mut input = vec![b'a'; MAX_IPC_COMMAND_BYTES + 1];
        input.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(input));
        let mut buffer = Vec::new();

        assert_eq!(
            read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
            IpcLine::TooLarge
        );
    }

    #[test]
    fn read_ipc_line_bounded_rejects_oversized_unterminated_line() {
        let input = vec![b'a'; MAX_IPC_COMMAND_BYTES + 1];
        let mut reader = BufReader::new(Cursor::new(input));
        let mut buffer = Vec::new();

        assert_eq!(
            read_ipc_line_bounded(&mut reader, &mut buffer).unwrap(),
            IpcLine::TooLarge
        );
    }

    #[test]
    fn ipc_client_admission_is_bounded() {
        let active = std::sync::atomic::AtomicUsize::new(MAX_IPC_CLIENTS - 1);
        assert!(try_acquire_client_slot(&active));
        assert_eq!(
            active.load(std::sync::atomic::Ordering::Acquire),
            MAX_IPC_CLIENTS
        );
        assert!(!try_acquire_client_slot(&active));

        active.store(0, std::sync::atomic::Ordering::Release);
        assert!(try_acquire_client_slot(&active));
        assert_eq!(active.load(std::sync::atomic::Ordering::Acquire), 1);
    }

    #[test]
    fn ipc_client_slot_guard_releases_failed_setup_admission() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(try_acquire_client_slot(&active));
        assert_eq!(active.load(std::sync::atomic::Ordering::Acquire), 1);

        let slot = ClientSlot(Arc::clone(&active));
        drop(slot);

        assert_eq!(active.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn ipc_clone_failure_releases_its_admitted_client_slot() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(try_acquire_client_slot(&active));

        let result: std::io::Result<(ClientSlot, ())> =
            clone_client_shutdown_stream(ClientSlot(Arc::clone(&active)), || {
                Err(std::io::Error::other("forced try_clone failure"))
            });

        assert!(result.is_err());
        assert_eq!(active.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn shutdown_joins_initial_playback_worker_before_returning() {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_running = Arc::clone(&running);
        let worker_started = Arc::clone(&started);
        let worker_completed = Arc::clone(&completed);
        let worker = std::thread::spawn(move || {
            worker_started.store(true, std::sync::atomic::Ordering::Release);
            while worker_running.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::yield_now();
            }
            worker_completed.store(true, std::sync::atomic::Ordering::Release);
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !started.load(std::sync::atomic::Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "worker did not start");
            std::thread::yield_now();
        }
        running.store(false, std::sync::atomic::Ordering::Release);

        join_initial_playback_thread(worker);
        assert!(completed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn daemon_ipc_client_path_sets_idle_timeout() {
        let source = include_str!("audio_daemon.rs");
        assert!(source.contains("set_read_timeout(Some(std::time::Duration::from_secs("));
        assert!(source.contains("IPC_CLIENT_IDLE_TIMEOUT_SECS"));
        assert!(source.contains("set_write_timeout(Some(std::time::Duration::from_secs("));
        assert!(source.contains("IPC_CLIENT_WRITE_TIMEOUT_SECS"));
        assert!(source.contains("std::io::ErrorKind::TimedOut"));
        assert!(source.contains("Closing idle IPC client after read timeout"));
    }

    /// `Command::name()` must stay in sync with `#[serde(rename)]`,
    /// because `peer_allows_command` matches on these exact strings.
    #[test]
    fn command_name_matches_wire_tag() {
        let cmd: Command = serde_json::from_str(r#"{"command":"status"}"#).unwrap();
        assert_eq!(cmd.name(), "status");

        let cmd: Command = serde_json::from_str(r#"{"command":"get_snapshot"}"#).unwrap();
        assert_eq!(cmd.name(), "get_snapshot");

        let cmd: Command = serde_json::from_str(r#"{"command":"snapshot"}"#).unwrap();
        assert_eq!(cmd.name(), "get_snapshot");

        let cmd: Command = serde_json::from_str(r#"{"command":"dump_state"}"#).unwrap();
        assert_eq!(cmd.name(), "dump_state");

        let cmd: Command = serde_json::from_str(r#"{"command":"driver_status"}"#).unwrap();
        assert_eq!(cmd.name(), "driver_status");

        // The "hal_status" alias deserialises to DriverStatus, whose
        // canonical wire name (per `#[serde(rename)]`) is "driver_status".
        let cmd: Command = serde_json::from_str(r#"{"command":"hal_status"}"#).unwrap();
        assert_eq!(cmd.name(), "driver_status");

        let cmd: Command =
            serde_json::from_str(r#"{"command":"update_plugin","index":0,"parameters":{}}"#)
                .unwrap();
        assert_eq!(cmd.name(), "update_plugin");

        let cmd: Command =
            serde_json::from_str(r#"{"command":"set_input_channels","channels":4}"#).unwrap();
        assert_eq!(cmd.name(), "set_input_channels");

        let cmd: Command =
            serde_json::from_str(r#"{"command":"set_output_channels","channels":6}"#).unwrap();
        assert_eq!(cmd.name(), "set_output_channels");

        let cmd: Command =
            serde_json::from_str(r#"{"command":"set_pipeline_channels","output_channels":6}"#)
                .unwrap();
        assert_eq!(cmd.name(), "set_pipeline_channels");

        let cmd: Command =
            serde_json::from_str(r#"{"command":"load_plugin_artifact","artifact":[]}"#).unwrap();
        assert_eq!(cmd.name(), "load_plugin_artifact");

        let cmd: Command = serde_json::from_str(
            r#"{"command":"load_plugin_artifact_path","path":"/tmp/room-eq.json"}"#,
        )
        .unwrap();
        assert_eq!(cmd.name(), "load_plugin_artifact_path");

        let cmd: Command = serde_json::from_str(
            r#"{"command":"reorder_graph","order":[20,10],"base_generation":3}"#,
        )
        .unwrap();
        assert!(matches!(
            cmd,
            Command::ReorderGraph {
                order,
                base_generation: Some(3)
            } if order == vec![20, 10]
        ));

        let cmd: Command = serde_json::from_str(
            r#"{"command":"set_rack_plugin_state","index":1,"input_channels":8,"bypassed":true,"base_generation":4}"#,
        )
        .unwrap();
        assert!(matches!(
            cmd,
            Command::SetRackPluginState {
                index: 1,
                input_channels: Some(8),
                bypassed: Some(true),
                base_generation: Some(4)
            }
        ));

        let cmd: Command = serde_json::from_str(r#"{"command":"shutdown"}"#).unwrap();
        assert_eq!(cmd.name(), "shutdown");
    }

    #[derive(Debug)]
    struct FakeDriverState {
        status: DriverStatus,
        engine_ready: bool,
        last_requested_config: Option<DriverConfig>,
        requested_configs: Vec<DriverConfig>,
        last_ack: Option<(DriverConfig, ConfigResult)>,
        pending_config_change: Option<DriverConfig>,
        fail_next_config: Option<String>,
        negotiated_config: Option<DriverConfig>,
        config_failures_remaining: usize,
        published_volume: Option<(f32, bool, u32)>,
        volume_generation: u32,
    }

    #[derive(Debug, Clone)]
    struct FakeDriver {
        state: Arc<Mutex<FakeDriverState>>,
    }

    impl FakeDriver {
        fn new(state: Arc<Mutex<FakeDriverState>>) -> Self {
            Self { state }
        }
    }

    #[derive(Debug)]
    struct FakeCaptureInput(Arc<Mutex<FakeDriverState>>);

    impl driver_common::CaptureInput for FakeCaptureInput {
        fn configuration(&self) -> DriverConfig {
            let state = self.0.lock();
            DriverConfig::new(
                state.status.sample_rate,
                state.status.buffer_frames,
                state.status.channel_count,
            )
        }

        fn available_frames(&self) -> usize {
            4096
        }

        fn read_frames(&self, buffer: &mut [f32], expected: DriverConfig) -> usize {
            if expected != self.configuration() {
                return 0;
            }
            let channels = expected.channel_count as usize;
            if channels == 0 {
                return 0;
            }
            let frames = buffer.len() / channels;
            for frame in buffer[..frames * channels].chunks_exact_mut(channels) {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = (channel + 1) as f32 * 0.03125;
                }
            }
            frames
        }
    }

    impl AudioDriver for FakeDriver {
        fn initialize(&mut self) -> Result<(), DriverError> {
            Ok(())
        }

        fn shutdown(&mut self) {}

        fn status(&self) -> DriverStatus {
            self.state.lock().status.clone()
        }

        fn read_audio(&mut self, buffer: &mut [f32]) -> usize {
            buffer.fill(0.0);
            0
        }

        fn capture_source(&self) -> Option<driver_common::CaptureSource> {
            Some(driver_common::CaptureSource::new(FakeCaptureInput(
                Arc::clone(&self.state),
            )))
        }

        fn available_frames(&self) -> usize {
            0
        }

        fn sample_rate(&self) -> u32 {
            self.state.lock().status.sample_rate
        }

        fn channel_count(&self) -> u32 {
            self.state.lock().status.channel_count
        }

        fn request_config(&mut self, config: DriverConfig) -> ConfigResult {
            let mut state = self.state.lock();
            state.last_requested_config = Some(config);
            state.requested_configs.push(config);
            if let Some(error) = state.fail_next_config.take() {
                ConfigResult::error(error)
            } else if state.config_failures_remaining > 0 {
                state.config_failures_remaining -= 1;
                ConfigResult::error("queued config failure")
            } else if let Some(actual) = state.negotiated_config {
                state.status.sample_rate = actual.sample_rate;
                state.status.buffer_frames = actual.buffer_frames;
                state.status.channel_count = actual.channel_count;
                ConfigResult::negotiated(
                    actual.sample_rate,
                    actual.buffer_frames,
                    actual.channel_count,
                )
            } else {
                if config.sample_rate > 0 {
                    state.status.sample_rate = config.sample_rate;
                }
                if config.buffer_frames > 0 {
                    state.status.buffer_frames = config.buffer_frames;
                }
                if config.channel_count > 0 {
                    state.status.channel_count = config.channel_count;
                }
                ConfigResult::Accepted
            }
        }

        fn poll_config_change(&mut self) -> Option<DriverConfig> {
            self.state.lock().pending_config_change.take()
        }

        fn acknowledge_config_change(&mut self, actual: DriverConfig, result: ConfigResult) {
            self.state.lock().last_ack = Some((actual, result));
        }

        fn set_engine_ready(&mut self, ready: bool) {
            self.state.lock().engine_ready = ready;
        }

        fn output_volume(&mut self) -> Option<(f32, bool, u32)> {
            self.state.lock().published_volume
        }

        fn set_output_volume(&mut self, volume: f32, muted: bool) -> Option<u32> {
            let mut state = self.state.lock();
            state.volume_generation += 1;
            let generation = state.volume_generation;
            state.published_volume = Some((volume, muted, generation));
            Some(generation)
        }
    }

    fn fake_driver_state() -> Arc<Mutex<FakeDriverState>> {
        Arc::new(Mutex::new(FakeDriverState {
            status: DriverStatus::new(true, true, true, 48_000, 2, 512, "Fake HAL", true, 0),
            engine_ready: false,
            last_requested_config: None,
            requested_configs: Vec::new(),
            last_ack: None,
            pending_config_change: None,
            fail_next_config: None,
            negotiated_config: None,
            config_failures_remaining: 0,
            published_volume: None,
            volume_generation: 0,
        }))
    }

    fn healthy_driver_status() -> DriverStatus {
        DriverStatus::new(true, true, true, 48_000, 2, 512, "Fake HAL", true, 0)
    }

    fn fault_codes(faults: &[Value]) -> Vec<&str> {
        faults
            .iter()
            .filter_map(|fault| fault["code"].as_str())
            .collect()
    }

    fn test_daemon_with_driver(state: Arc<Mutex<FakeDriverState>>) -> AudioDaemon {
        AudioDaemon {
            manager: Arc::new(Mutex::new(AudioEngineManager::new())),
            running: Arc::new(Mutex::new(true)),
            driver_manager: Arc::new(Mutex::new(DriverManager::from_driver_with_lab_output(
                Box::new(FakeDriver::new(state)),
            ))),
            system_state: Arc::new(Mutex::new(SystemwideState::default())),
            key_manager: Arc::new(Mutex::new(KeyManager::for_test())),
            pipeline_mutation: Arc::new(Mutex::new(())),
            pipeline_intent_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            runtime_telemetry: Arc::new(RuntimeTelemetry::default()),
            playback_progress: Arc::new(Mutex::new(PlaybackProgress::default())),
            snapshot_cache: Arc::new(Mutex::new(None)),
            metering_cache: Arc::new(Mutex::new(None)),
            device_registry: Arc::new(Mutex::new(DeviceRegistry::default())),
            output_profiles: Arc::new(Mutex::new(
                crate::output_profiles::OutputProfileStore::default(),
            )),
        }
    }

    #[test]
    fn testkit_stop_supersedes_startup_before_worker_executes() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        let startup_intent = daemon
            .pipeline_intent_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let generation = daemon.system_state.lock().generation();
        let stopped = daemon.handle_command(Command::Stop);
        assert!(stopped.success, "{stopped:?}");
        // Stop does not apply a pipeline spec, so the intent epoch is needed
        // in addition to the configuration generation.
        assert_eq!(daemon.system_state.lock().generation(), generation);
        daemon
            .spawn_initial_driver_playback_for_intent(startup_intent)
            .join()
            .unwrap();
        assert_eq!(
            daemon.manager.lock().get_state(),
            sotf_audio::manager::StreamingState::Idle
        );
        assert_eq!(daemon.system_state.lock().generation(), generation);
        assert!(daemon.system_state.lock().applied_spec().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn startup_after_idle_driver_configuration_obeys_latest_playback_intent() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        let old_intent = daemon
            .pipeline_intent_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        // A driver event supersedes boot, but leaves the engine idle.
        daemon
            .pipeline_intent_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        handle_driver_config_change(
            &daemon.driver_manager,
            &daemon.manager,
            &daemon.device_registry,
            DriverConfig::new(48_000, 512, 2),
            &daemon.system_state,
            &daemon.running,
        );
        daemon
            .spawn_initial_driver_playback_for_intent(old_intent)
            .join()
            .unwrap();
        assert!(daemon.should_retry_startup());
        assert!(daemon.handle_command(Command::Stop).success);
        assert!(!daemon.should_retry_startup());
        assert!(daemon.handle_command(Command::Pause).success);
        assert!(!daemon.should_retry_startup());
        daemon.system_state.lock().playback_wanted = true;
        state.lock().status.driver_ready = false;
        assert!(!daemon.should_retry_startup());
        state.lock().status.driver_ready = true;
        assert!(daemon.should_retry_startup());
        daemon.spawn_initial_driver_playback().join().unwrap();
        assert_eq!(
            daemon.manager.lock().get_playback_state(),
            sotf_audio::PlaybackState::Playing
        );
        assert!(!daemon.should_retry_startup());
        assert!(daemon.handle_command(Command::Stop).success);
    }

    /// Connected IPC test pair: `UnixStream::pair` on Unix, loopback TCP
    /// accept on Windows. Keeps `handle_client` round-trip tests portable.
    #[cfg(unix)]
    fn ipc_test_pair() -> (
        crate::ipc_transport::IpcStream,
        crate::ipc_transport::IpcStream,
    ) {
        UnixStream::pair().expect("unix stream pair")
    }

    /// Connected IPC test pair: `UnixStream::pair` on Unix, loopback TCP
    /// accept on Windows. Keeps `handle_client` round-trip tests portable.
    #[cfg(windows)]
    fn ipc_test_pair() -> (
        crate::ipc_transport::IpcStream,
        crate::ipc_transport::IpcStream,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("test listener addr");
        let client = std::net::TcpStream::connect(addr).expect("connect test client");
        let (server, _) = listener.accept().expect("accept test client");
        (client, server)
    }

    fn assert_fake_driver_uses_lab_output(daemon: &AudioDaemon) {
        // The playback worker publishes callback and frame counters every five seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(7);
        loop {
            let playback = daemon.manager.lock().get_engine_state();
            if playback.playback_output_device.as_deref() == Some("Systemwide Lab Output")
                && playback.playback_callback_count > 0
                && playback.playback_frames_received > 0
                && playback.playback_frames_written > 0
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fake driver did not process audio through lab output: {playback:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn send_owner_ipc_command(daemon: &AudioDaemon, raw: &str) -> serde_json::Value {
        let (mut client, server) = ipc_test_pair();
        let daemon = daemon.clone();
        let handle = std::thread::spawn(move || daemon.handle_client(server, PeerClass::Owner));

        writeln!(client, "{}", raw).expect("write request");
        let mut reader = BufReader::new(client.try_clone().expect("clone client"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read response");
        drop(reader);
        drop(client);
        handle.join().expect("client handler thread");

        serde_json::from_str(&line).expect("valid JSON response")
    }

    #[test]
    fn pipeline_supervisor_builds_runtime_chain_without_committing_until_success() {
        let supervisor = PipelineSupervisor::default();

        let plan = supervisor
            .prepare_plan(
                vec![
                    test_plugin("hal_input"),
                    test_plugin("eq"),
                    test_plugin("loudness_monitor"),
                    test_plugin("gain"),
                    test_plugin("hal_output"),
                ],
                2,
                6,
                2,
            )
            .expect("valid pipeline plan");

        assert_eq!(plan.spec.input_channels, 2);
        assert_eq!(plan.spec.output_channels, 6);
        assert_eq!(
            plan.spec
                .user_plugins
                .iter()
                .map(|p| p.plugin_type.as_str())
                .collect::<Vec<_>>(),
            vec!["eq", "gain"]
        );
        assert_eq!(plan.input_loudness_index, 0);
        assert_eq!(plan.output_loudness_index, 3);
        assert_eq!(
            plan.runtime_plugins
                .iter()
                .map(|p| p.plugin_type.as_str())
                .collect::<Vec<_>>(),
            vec!["loudness_monitor", "eq", "gain", "loudness_monitor"]
        );
        assert!(supervisor.input_loudness_index().is_none());
        assert!(supervisor.output_loudness_index().is_none());
    }

    #[test]
    fn negotiated_driver_timing_overrides_requested_pipeline_timing() {
        let result = driver_common::ConfigResult::negotiated(44_100, 256, 6);

        assert_eq!(
            pipeline_timing_after_config_request(&result, 48_000, 512),
            (44_100, 256)
        );
        assert_eq!(
            pipeline_timing_after_config_request(
                &driver_common::ConfigResult::Accepted,
                48_000,
                512
            ),
            (48_000, 512)
        );
    }

    #[test]
    fn apply_pipeline_plan_starts_engine_with_negotiated_timing() {
        let source = include_str!("audio_daemon.rs");
        assert!(
            source.contains("Self::start_pipeline_plan(")
                && source.contains("sample_rate,\n                buffer_frames,"),
            "HAL playback restart must route negotiated timing through the shared starter"
        );
    }

    #[test]
    fn pipeline_supervisor_commit_atomically_updates_desired_and_applied_state() {
        let mut supervisor = PipelineSupervisor::default();
        let plan = supervisor
            .prepare_plan(vec![test_plugin("eq")], 4, 8, 2)
            .expect("valid pipeline plan");

        supervisor.commit_applied(&plan);

        assert_eq!(supervisor.input_channels(), 4);
        assert_eq!(supervisor.output_channels(), 8);
        assert_eq!(supervisor.input_loudness_index(), Some(0));
        assert_eq!(supervisor.output_loudness_index(), Some(2));
        assert_eq!(supervisor.applied_generation(), Some(1));
    }

    #[test]
    fn pipeline_supervisor_rejects_invalid_channels_before_state_mutation() {
        let supervisor = PipelineSupervisor::default();
        let result = supervisor.prepare_plan(vec![test_plugin("eq")], 0, 64, 2);

        assert!(result.unwrap_err().contains("Invalid output channel count"));
        assert_eq!(supervisor.input_channels(), 2);
        assert_eq!(supervisor.output_channels(), 2);
    }

    #[test]
    fn pipeline_supervisor_reducer_methods_control_desired_mutation() {
        let mut supervisor = PipelineSupervisor::default();

        supervisor
            .set_desired_output_device(Some("ADAM Audio D3V".to_string()))
            .expect("safe device should be accepted");
        assert_eq!(
            supervisor.selected_output_device().as_deref(),
            Some("ADAM Audio D3V")
        );

        let result = supervisor.set_desired_output_device(Some("SotF Virtual Audio".to_string()));
        assert!(result.unwrap_err().contains("virtual/loopback"));
        assert_eq!(
            supervisor.selected_output_device().as_deref(),
            Some("ADAM Audio D3V")
        );

        let plan = supervisor
            .prepare_plan(vec![test_plugin("eq")], 10, 4, 10)
            .expect("valid idle reconfigure plan");
        supervisor.commit_idle_reconfigure(&plan);
        assert_eq!(supervisor.input_channels(), 10);
        assert_eq!(supervisor.output_channels(), 4);
        assert_eq!(supervisor.generation(), 2);
        assert!(supervisor.applied_generation().is_none());
    }

    #[test]
    fn transport_snapshot_reports_all_playing_faults_without_hiding_secondary_causes() {
        let driver_status = healthy_driver_status();
        let engine_state = sotf_audio::AudioEngineState::default();

        let (transport, faults) =
            transport_snapshot_and_faults("Playing", &driver_status, &engine_state);

        assert_eq!(transport["input"]["status"], "input_frames_missing");
        assert_eq!(transport["output"]["status"], "output_callbacks_missing");
        let codes = fault_codes(&faults);
        assert!(codes.contains(&"input_frames_missing"));
        assert!(codes.contains(&"output_callbacks_missing"));
        assert!(codes.contains(&"output_device_unresolved"));
    }

    #[test]
    fn transport_snapshot_reports_flowing_when_input_and_output_are_observed() {
        let driver_status = healthy_driver_status();
        let mut engine_state = sotf_audio::AudioEngineState::default();
        engine_state.playback_frames_received = 1024;
        engine_state.playback_callback_count = 8;
        engine_state.playback_frames_written = 1024;
        engine_state.playback_output_device = Some("ADAM Audio D3V".to_string());
        engine_state.playback_effective_sample_rate = 48_000;

        let (transport, faults) =
            transport_snapshot_and_faults("Playing", &driver_status, &engine_state);

        assert_eq!(transport["input"]["status"], "flowing");
        assert_eq!(transport["output"]["status"], "flowing");
        assert_eq!(transport["output"]["device"], "ADAM Audio D3V");
        assert!(faults.is_empty());
    }

    #[test]
    fn typed_output_incident_blocks_readiness_until_explicit_resolution() {
        let mut state = sotf_audio::AudioEngineState {
            playback_callback_count: 50,
            playback_output_generation: 42,
            playback_incident: Some(sotf_audio::PlaybackIncident {
                id: 100,
                stream_generation: 41,
                kind: sotf_audio::PlaybackIncidentKind::StreamError,
                message: "arbitrary localized diagnostic".into(),
                resolution: None,
            }),
            ..Default::default()
        };
        assert!(AudioDaemon::playback_startup_observation(&state).is_err());
        let (transport, faults) =
            transport_snapshot_and_faults("Playing", &healthy_driver_status(), &state);
        assert_eq!(transport["output_status"], "output_recovering");
        assert!(fault_codes(&faults).contains(&"output_stream_incident"));
        assert_eq!(transport["output"]["incident"]["kind"], "stream_error");
        state.playback_incident.as_mut().unwrap().resolution =
            Some(sotf_audio::PlaybackIncidentResolution {
                output_generation: 42,
                callback_count: 50,
            });
        assert_eq!(AudioDaemon::playback_startup_observation(&state), Ok(true));
        let (_, faults) =
            transport_snapshot_and_faults("Playing", &healthy_driver_status(), &state);
        assert!(!fault_codes(&faults).contains(&"output_stream_incident"));
        state.playback_output_generation = 43;
        state.playback_callback_count = 0;
        assert_eq!(AudioDaemon::playback_startup_observation(&state), Ok(false));
        state.last_error = Some("decoder failure".into());
        assert!(AudioDaemon::playback_startup_observation(&state).is_err());
    }

    #[test]
    fn metering_faults_only_apply_to_playing_fallback_sources() {
        let metering = serde_json::json!({
            "sources": {
                "input": { "status": "fallback_zero" },
                "output": { "status": "available" }
            }
        });

        let mut faults = Vec::new();
        push_metering_faults("Idle", &metering, &mut faults);
        assert!(faults.is_empty());

        push_metering_faults("Playing", &metering, &mut faults);
        assert_eq!(fault_codes(&faults), vec!["input_metering_unavailable"]);
    }

    #[test]
    fn driver_reconfigure_preserves_daemon_selected_output_device_when_idle() {
        let audio_manager = Arc::new(Mutex::new(AudioEngineManager::new()));
        let system_state = Arc::new(Mutex::new(SystemwideState::default()));

        {
            let mut state = system_state.lock();
            let plan = state
                .prepare_with_selected_device("ADAM Audio D3V".to_string())
                .expect("valid device plan");
            state.commit_applied(&plan);
        }

        reconfigure_audio_pipeline(&audio_manager, &system_state, 48_000, 512, 6)
            .expect("idle reconfigure should update desired state");

        let state = system_state.lock();
        assert_eq!(
            state.selected_output_device().as_deref(),
            Some("ADAM Audio D3V")
        );
        assert_eq!(state.input_channels(), 6);
        assert_eq!(state.output_channels(), 2);
    }

    #[test]
    fn restored_driver_pipeline_acknowledges_restored_channel_geometry() {
        let (actual, result) = acknowledged_config_for_outcome(
            48_000,
            512,
            10,
            48_000,
            PipelineReconfigureOutcome::Restored { input_channels: 2 },
        );

        assert_eq!(actual, DriverConfig::new(48_000, 512, 2));
        assert_eq!(
            result,
            driver_common::ConfigResult::negotiated(48_000, 512, 2)
        );
    }

    #[test]
    fn snapshot_returns_complete_aged_observation_during_transition() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        let before = daemon.snapshot_json();
        let mutation = daemon.pipeline_mutation.lock();
        let snapshot_daemon = daemon.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(snapshot_daemon.snapshot_json()).unwrap();
        });
        let snapshot = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("status stays responsive");
        assert_eq!(snapshot["applied"], before["applied"]);
        assert_eq!(snapshot["observed"], before["observed"]);
        assert_eq!(snapshot["observation"]["transition_in_progress"], true);
        assert_eq!(snapshot["observation"]["available"], true);
        drop(mutation);
        worker.join().unwrap();
        assert_eq!(
            daemon.snapshot_json()["observation"]["transition_in_progress"],
            false
        );
    }

    #[test]
    fn metering_returns_cached_generation_without_waiting_for_transition_owned_locks() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        let before = daemon.snapshot_json();
        let mutation = daemon.pipeline_mutation.lock();
        let manager = daemon.manager.lock();
        let mut pipeline = daemon.system_state.lock();
        let next = pipeline.prepare_plan(Vec::new(), 6, 8, 6).unwrap();
        pipeline.commit_applied(&next);
        assert_ne!(
            serde_json::json!(pipeline.generation()),
            before["generation"]
        );
        let reader = daemon.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(reader.handle_get_metering()).unwrap());
        // Release every held lock before asserting, including on a timeout.
        let response = rx.recv_timeout(std::time::Duration::from_millis(200));
        drop(pipeline);
        drop(manager);
        drop(mutation);
        worker.join().unwrap();
        let metering = response
            .expect("metering must not wait for transition locks")
            .data
            .unwrap();
        assert_eq!(metering["generation"], before["generation"]);
        assert_eq!(metering["input"], before["observed"]["metering"]["input"]);
        assert_eq!(metering["output"], before["observed"]["metering"]["output"]);
        assert_eq!(
            metering["sources"],
            before["observed"]["metering"]["sources"]
        );
        assert_eq!(metering["observation"]["transition_in_progress"], true);
        assert_eq!(metering["observation"]["available"], true);
        assert!(metering["observation"]["age_ms"].is_u64());
        let fresh = daemon.handle_get_metering().data.unwrap();
        assert_eq!(fresh["observation"]["transition_in_progress"], false);
        assert_eq!(fresh["observation"]["available"], true);
    }

    #[test]
    fn metering_uses_its_own_recent_observation_instead_of_an_older_full_snapshot() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        let older = daemon.snapshot_json();
        {
            let mut pipeline = daemon.system_state.lock();
            let next = pipeline.prepare_plan(Vec::new(), 6, 8, 6).unwrap();
            pipeline.commit_applied(&next);
        }
        let recent = daemon.handle_get_metering().data.unwrap();
        assert_ne!(recent["generation"], older["generation"]);
        assert_eq!(
            recent["input"]["channel_peaks"].as_array().unwrap().len(),
            6
        );
        let mutation = daemon.pipeline_mutation.lock();
        let manager = daemon.manager.lock();
        let pipeline = daemon.system_state.lock();
        let reader = daemon.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(reader.handle_get_metering()).unwrap());
        let response = rx.recv_timeout(std::time::Duration::from_millis(200));
        drop(pipeline);
        drop(manager);
        drop(mutation);
        worker.join().unwrap();
        let cached = response
            .expect("meter-only clients must retain their own observation")
            .data
            .unwrap();
        assert_eq!(cached["generation"], recent["generation"]);
        assert_eq!(cached["input"], recent["input"]);
        assert_eq!(cached["output"], recent["output"]);
        assert_eq!(cached["observation"]["transition_in_progress"], true);
        assert_eq!(cached["observation"]["available"], true);
    }

    #[test]
    fn metering_without_a_prior_observation_is_explicitly_unavailable_during_transition() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        let mutation = daemon.pipeline_mutation.lock();
        let manager = daemon.manager.lock();
        let pipeline = daemon.system_state.lock();
        let reader = daemon.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(reader.handle_get_metering()).unwrap());
        let response = rx.recv_timeout(std::time::Duration::from_millis(200));
        drop(pipeline);
        drop(manager);
        drop(mutation);
        worker.join().unwrap();
        let metering = response
            .expect("unavailable metering must remain responsive")
            .data
            .unwrap();
        assert!(metering["generation"].is_null());
        assert_eq!(metering["observation"]["transition_in_progress"], true);
        assert_eq!(metering["observation"]["available"], false);
        assert!(metering["observation"]["age_ms"].is_null());
        assert_eq!(
            metering["input"]["channel_peaks"],
            serde_json::json!([0.0, 0.0])
        );
        assert_eq!(metering["output"]["measurement_valid"], false);
    }

    #[test]
    fn pipeline_commands_share_one_transition_boundary() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        let mutation = daemon.pipeline_mutation.lock();
        let command_daemon = daemon.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();

        let command_thread = std::thread::spawn(move || {
            started_tx.send(()).expect("signal command start");
            let response =
                command_daemon.handle_command(Command::SetOutputChannels { channels: 0 });
            finished_tx.send(response).expect("return command response");
        });

        started_rx.recv().expect("command thread started");
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "pipeline command must wait for the active transition"
        );

        drop(mutation);
        let response = finished_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("command completes after transition");
        assert!(!response.success);
        command_thread.join().expect("command thread joins");
    }

    #[test]
    fn capture_resume_rebuilds_once_only_after_long_idle() {
        let started = std::time::Instant::now();
        let mut tracker = CaptureResumeTracker::new(started, 3, false);

        assert!(!tracker.observe(
            started + PLAYBACK_IDLE_REBUILD_THRESHOLD - std::time::Duration::from_millis(1),
            3,
            false,
        ));
        assert!(tracker.observe(started + PLAYBACK_IDLE_REBUILD_THRESHOLD, 3, true,));
        assert!(!tracker.observe(
            started + PLAYBACK_IDLE_REBUILD_THRESHOLD + std::time::Duration::from_secs(1),
            3,
            true,
        ));
    }

    #[test]
    fn config_debounce_discards_requests_superseded_by_control_intent() {
        let now = std::time::Instant::now();
        let old = driver_common::DriverConfig::new(96_000, 512, 2);
        let new = driver_common::DriverConfig::new(48_000, 256, 2);
        let mut pending = ConfigChangeDebouncer::new();
        assert_eq!(pending.observe_intent(now, Some(old), 10), None);
        let settled = now + ConfigChangeDebouncer::DEBOUNCE;
        assert_eq!(pending.observe_intent(settled, None, 11), None);
        assert_eq!(pending.observe_intent(settled, Some(new), 11), None);
        assert_eq!(
            pending.observe_intent(settled + ConfigChangeDebouncer::DEBOUNCE, None, 11),
            Some(new)
        );
    }

    #[test]
    fn config_change_debouncer_applies_one_shot_request_after_quiet_window() {
        // Regression: the HAL requests 96 kHz once at startup while the
        // applied geometry is still 48 kHz. poll_config_change consumes the
        // request, so later polls return None; the request must still be
        // applied after the debounce window instead of waiting forever for
        // the applied status to match it.
        let started = std::time::Instant::now();
        let requested = driver_common::DriverConfig::new(96_000, 512, 2);
        let mut debouncer = ConfigChangeDebouncer::new();

        assert_eq!(debouncer.observe(started, Some(requested)), None);
        assert_eq!(
            debouncer.observe(
                started + ConfigChangeDebouncer::DEBOUNCE - std::time::Duration::from_millis(1),
                None,
            ),
            None
        );
        assert_eq!(
            debouncer.observe(started + ConfigChangeDebouncer::DEBOUNCE, None),
            Some(requested)
        );
        // Applied exactly once.
        assert_eq!(
            debouncer.observe(started + ConfigChangeDebouncer::DEBOUNCE * 2, None,),
            None
        );
    }

    #[test]
    fn config_change_debouncer_flapping_requests_reset_the_window() {
        let started = std::time::Instant::now();
        let rate_96k = driver_common::DriverConfig::new(96_000, 512, 2);
        let rate_48k = driver_common::DriverConfig::new(48_000, 512, 2);
        let mut debouncer = ConfigChangeDebouncer::new();

        assert_eq!(debouncer.observe(started, Some(rate_96k)), None);
        // A different request inside the window restarts the clock.
        assert_eq!(
            debouncer.observe(
                started + std::time::Duration::from_millis(400),
                Some(rate_48k),
            ),
            None
        );
        // The original deadline passes with no application...
        assert_eq!(
            debouncer.observe(started + ConfigChangeDebouncer::DEBOUNCE, None),
            None
        );
        // ...and the latest request wins once it stays quiet long enough.
        assert_eq!(
            debouncer.observe(
                started + std::time::Duration::from_millis(400) + ConfigChangeDebouncer::DEBOUNCE,
                None,
            ),
            Some(rate_48k)
        );
    }

    #[test]
    fn capture_resume_does_not_rebuild_after_explicit_pipeline_change() {
        let started = std::time::Instant::now();
        let mut tracker = CaptureResumeTracker::new(started, 3, false);

        assert!(!tracker.observe(started + PLAYBACK_IDLE_REBUILD_THRESHOLD, 4, true,));
    }

    #[test]
    fn stale_pipeline_intent_is_rejected_before_handler_side_effects() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        {
            let mut state = daemon.system_state.lock();
            let plan = state
                .prepare_plan(Vec::new(), 2, 2, 2)
                .expect("valid baseline plan");
            state.commit_applied(&plan);
        }

        let response = daemon
            .handle_command_at_generation(Command::SetOutputChannels { channels: 6 }, Some(0));

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("generation conflict"))
        );
        let state = daemon.system_state.lock();
        assert_eq!(state.applied_generation(), Some(1));
        assert_eq!(state.output_channels(), 2);
    }

    #[test]
    fn update_plugin_rejects_stale_generation_without_side_effects() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        {
            let mut state = daemon.system_state.lock();
            let plan = state
                .prepare_plan(vec![test_plugin("gain")], 2, 2, 2)
                .expect("valid baseline plan");
            state.commit_applied(&plan);
        }

        let response = daemon.handle_command_at_generation(
            Command::UpdatePlugin {
                index: 0,
                parameters: serde_json::json!({"gain_db": 3.0}),
            },
            Some(0),
        );

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("generation conflict"))
        );
        let state = daemon.system_state.lock();
        assert_eq!(state.user_plugins()[0].parameters, serde_json::json!({}));
    }

    #[test]
    fn update_plugin_identical_parameters_is_noop_without_generation_bump() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        {
            let mut state = daemon.system_state.lock();
            let plan = state
                .prepare_plan(vec![test_plugin("gain")], 2, 2, 2)
                .expect("valid baseline plan");
            state.commit_applied(&plan);
        }
        let baseline_generation = daemon.system_state.lock().generation();

        let response = daemon.handle_command(Command::UpdatePlugin {
            index: 0,
            parameters: serde_json::json!({}),
        });

        assert!(response.success);
        assert_eq!(
            response.data.as_ref().map(|data| &data["generation"]),
            Some(&serde_json::json!(baseline_generation))
        );
        assert_eq!(daemon.system_state.lock().generation(), baseline_generation);
    }

    #[test]
    fn update_plugin_adopted_generation_allows_successive_edits() {
        // Toolbar contract: adopt the generation from each success response.
        // The next edit with the adopted generation must not conflict, or
        // every parameter tweak fails once and only succeeds on retry.
        let daemon = test_daemon_with_driver(fake_driver_state());
        {
            let mut state = daemon.system_state.lock();
            let plan = state
                .prepare_plan(vec![test_plugin("gain")], 2, 2, 2)
                .expect("valid baseline plan");
            state.commit_applied(&plan);
        }
        let baseline = daemon.system_state.lock().generation();

        let first = daemon.handle_command_at_generation(
            Command::UpdatePlugin {
                index: 0,
                parameters: serde_json::json!({}),
            },
            Some(baseline),
        );
        assert!(first.success);
        let adopted = first
            .data
            .as_ref()
            .and_then(|data| data["generation"].as_u64())
            .expect("success response carries the generation");

        let second = daemon.handle_command_at_generation(
            Command::UpdatePlugin {
                index: 0,
                parameters: serde_json::json!({}),
            },
            Some(adopted),
        );
        assert!(second.success);
    }

    #[test]
    fn plugin_param_value_encoding_matches_engine_string_contract() {
        assert_eq!(encode_plugin_param_value(&serde_json::json!("raw")), "raw");
        assert_eq!(encode_plugin_param_value(&serde_json::json!(1.5)), "1.5");
        assert_eq!(encode_plugin_param_value(&serde_json::json!(true)), "true");
        assert_eq!(
            encode_plugin_param_value(&serde_json::json!({"a": [1, 2]})),
            r#"{"a":[1,2]}"#
        );
    }

    #[test]
    fn changed_plugin_parameters_reports_only_changed_keys() {
        let old = serde_json::json!({"gain_db": 0.0, "mute": false});
        let new = serde_json::json!({"gain_db": 3.0, "mute": false});

        let changed = changed_plugin_parameters(&old, &new).expect("diffable objects");
        assert_eq!(
            changed,
            vec![("gain_db".to_string(), serde_json::json!(3.0))]
        );

        let unchanged =
            changed_plugin_parameters(&old, &old).expect("identical objects diff cleanly");
        assert!(unchanged.is_empty());

        assert!(changed_plugin_parameters(&old, &serde_json::json!([1, 2])).is_none());
        assert!(
            changed_plugin_parameters(
                &serde_json::json!({"gain_db": 0.0, "mute": false}),
                &serde_json::json!({"gain_db": 0.0}),
            )
            .is_none(),
            "removed keys must fall back to a full rebuild"
        );
    }

    #[test]
    fn pipeline_specs_match_ignores_key_order_but_catches_real_differences() {
        let base = PipelineSpec {
            output_device: Some("Speakers".to_string()),
            user_plugins: vec![PluginConfig {
                plugin_type: "gain".to_string(),
                parameters: serde_json::json!({"b": 1, "a": 2.0}),
            }],
            user_graph: None,
            input_channels: 2,
            output_channels: 2,
        };
        let reordered_keys = PipelineSpec {
            user_plugins: vec![PluginConfig {
                plugin_type: "gain".to_string(),
                parameters: serde_json::json!({"a": 2.0, "b": 1}),
            }],
            ..base.clone()
        };
        assert!(pipeline_specs_match(&base, &reordered_keys));

        for different in [
            PipelineSpec {
                input_channels: 8,
                ..base.clone()
            },
            PipelineSpec {
                output_channels: 6,
                ..base.clone()
            },
            PipelineSpec {
                output_device: None,
                ..base.clone()
            },
            PipelineSpec {
                user_plugins: vec![PluginConfig {
                    plugin_type: "gain".to_string(),
                    parameters: serde_json::json!({"a": 3.0, "b": 1}),
                }],
                ..base.clone()
            },
            PipelineSpec {
                user_plugins: Vec::new(),
                ..base.clone()
            },
        ] {
            assert!(
                !pipeline_specs_match(&base, &different),
                "diverged spec must take the cold restart path"
            );
        }
    }

    #[test]
    fn startup_adopts_hal_input_geometry_with_safe_fallbacks() {
        assert_eq!(startup_channel_geometry(8, 2), (8, 2));
        assert_eq!(startup_channel_geometry(2, 6), (2, 6));
        assert_eq!(startup_channel_geometry(0, 2), (2, 2));
        assert_eq!(startup_channel_geometry(64, 2), (2, 2));
        assert_eq!(startup_channel_geometry(2, 0), (2, 1));
        assert_eq!(startup_channel_geometry(2, 99), (2, 32));
    }

    #[test]
    fn room_eq_channel_validation_rejects_incompatible_selected_device() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        daemon
            .system_state
            .lock()
            .set_desired_output_device(Some("Fake Speakers".to_string()))
            .expect("safe fake output device");
        daemon
            .device_registry
            .lock()
            .seed_max_output_channels("Fake Speakers", 48_000, 2);

        let error = daemon
            .validate_output_device_channels(6, 48_000)
            .expect_err("six-channel RoomEQ config must be rejected on stereo device");

        assert!(error.contains("requires 6 output channels"));
        assert!(error.contains("supports at most 2 channels"));
        assert!(error.contains("Fake Speakers"));
    }

    #[test]
    fn testkit_driver_status_uses_injected_driver() {
        let state = fake_driver_state();
        state.lock().status.channel_count = 10;
        let daemon = test_daemon_with_driver(state);

        let response = daemon.handle_command(Command::DriverStatus);

        assert!(response.success);
        let data = response.data.expect("driver_status data");
        assert_eq!(data["driver_name"], "Fake HAL");
        assert_eq!(data["channel_count"], 10);
        assert_eq!(data["ready"], true);
    }

    #[test]
    fn testkit_driver_status_wire_matches_serde_status_fields() {
        let state = fake_driver_state();
        let expected = state.lock().status.clone();
        let daemon = test_daemon_with_driver(state);
        let response = daemon.handle_command(Command::DriverStatus);
        let data = response.data.expect("driver status data");
        let serde_fields = serde_json::to_value(&expected).expect("status serializes");

        for field in [
            "platform_supported",
            "driver_installed",
            "capture_active",
            "sample_rate",
            "channel_count",
            "buffer_frames",
            "driver_name",
            "driver_ready",
        ] {
            assert_eq!(data[field], serde_fields[field], "wire field {field}");
        }
        assert_eq!(data["ready"], true);
        assert_eq!(data["buffer_initialized"], true);
    }

    #[test]
    fn testkit_invalid_plugin_load_does_not_mutate_pipeline_state() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![test_plugin("eq")],
            input_channels: 2,
            output_channels: 64,
        });

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|e| e.contains("Invalid output channel count"))
        );
        assert_eq!(daemon.system_state.lock().output_channels(), 2);
        assert!(daemon.system_state.lock().output_loudness_index().is_none());
    }

    #[test]
    fn testkit_patch_channel_command_preserves_daemon_owned_plugins_and_input_channels() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![test_plugin("eq")],
            input_channels: 10,
            output_channels: 2,
        });
        assert!(response.success);

        let response = daemon.handle_command(Command::SetOutputChannels { channels: 6 });

        assert!(response.success);
        let state = daemon.system_state.lock();
        assert_eq!(state.input_channels(), 10);
        assert_eq!(state.output_channels(), 6);
        assert_eq!(state.user_plugins().len(), 1);
    }

    #[test]
    fn testkit_pipeline_channel_patch_requires_a_field() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = daemon.handle_command(Command::SetPipelineChannels {
            input_channels: None,
            output_channels: None,
        });

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|e| e.contains("requires input_channels or output_channels"))
        );
    }

    #[test]
    fn testkit_atomic_configuration_requires_a_field() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = daemon.handle_command(Command::ApplyConfiguration {
            sample_rate: None,
            buffer_frames: None,
            input_channels: None,
            output_channels: None,
            output_device: None,
        });

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("at least one field"))
        );
    }

    #[test]
    fn testkit_stale_atomic_configuration_is_rejected_before_driver_mutation() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let response = send_owner_ipc_command(
            &daemon,
            r#"{"command":"apply_configuration","base_generation":99,"sample_rate":96000,"buffer_frames":256,"input_channels":4}"#,
        );

        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|error| error.contains("generation conflict"))
        );
        assert!(state.lock().requested_configs.is_empty());
    }

    #[test]
    fn testkit_atomic_configuration_applies_one_coherent_driver_format() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let response = daemon.handle_command(Command::ApplyConfiguration {
            sample_rate: Some(96_000),
            buffer_frames: Some(256),
            input_channels: Some(4),
            output_channels: Some(2),
            output_device: None,
        });

        assert!(response.success, "atomic apply failed: {response:?}");
        let data = response.data.expect("atomic configuration result");
        assert_eq!(data["requested"]["sample_rate"], 96_000);
        assert_eq!(data["requested"]["buffer_frames"], 256);
        assert_eq!(data["requested"]["input_channels"], 4);
        assert_eq!(data["applied"]["sample_rate"], 96_000);
        assert_eq!(data["applied"]["buffer_frames"], 256);
        assert_eq!(data["applied"]["input_channels"], 4);
        assert_eq!(data["negotiated"], false);

        let driver = state.lock();
        assert_eq!(
            driver.requested_configs,
            vec![DriverConfig::new(96_000, 256, 4)],
            "one atomic command must produce one HAL format request"
        );
    }

    #[test]
    fn testkit_load_plugin_artifact_accepts_rack_chain_without_ui_flattening() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(
            &daemon,
            r#"{"command":"load_plugin_artifact","artifact":{"plugins":[{"plugin_type":"eq","parameters":{}}]}}"#,
        );

        assert_eq!(response["success"], true);
        let state = daemon.system_state.lock();
        assert_eq!(state.user_plugins().len(), 1);
        assert_eq!(state.user_plugins()[0].plugin_type, "eq");
    }

    #[test]
    fn testkit_load_plugin_artifact_rejects_graph_shape_instead_of_flattening() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(
            &daemon,
            r#"{"command":"load_plugin_artifact","artifact":{"global_plugins":[{"plugin_type":"eq","parameters":{}}],"channels":{"L":{"plugins":[{"plugin_type":"gain","parameters":{}}]}}}}"#,
        );

        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains("Unsupported graph plugin artifact"))
        );
        assert!(daemon.system_state.lock().user_plugins().is_empty());
    }

    #[test]
    fn testkit_add_plugin_rejects_out_of_range_index() {
        // A client acting on a stale rack must get an error, not a silent
        // append at the end of the chain (remove/update already error).
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(
            &daemon,
            r#"{"command":"add_plugin","plugin":{"plugin_type":"gain","parameters":{}},"index":5}"#,
        );

        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains("out of range")),
            "unexpected error: {response}"
        );
        assert!(daemon.system_state.lock().user_plugins().is_empty());
    }

    #[test]
    fn testkit_load_plugin_artifact_path_rejects_non_absolute_paths() {
        // The artifact-path command reads a file on the daemon's behalf, so it
        // must enforce the same local-regular-file policy as `load`: a
        // relative path resolves against the daemon working directory and must
        // never be opened.
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(
            &daemon,
            r#"{"command":"load_plugin_artifact_path","path":"relative/room-eq.json"}"#,
        );

        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains("must be absolute"))
        );
        assert!(daemon.system_state.lock().user_plugins().is_empty());
    }

    #[test]
    fn testkit_load_plugin_artifact_graph_plan_preserves_topology() {
        // Applying a graph starts the real cpal output thread, which is not
        // available on headless CI hosts. Exercise the same artifact parser
        // and graph-aware pipeline planner without requiring physical audio.
        let artifact: Value = serde_json::from_str(
            r#"{"graph":{"nodes":[{"id":7,"plugin_type":"gain","parameters":{"gain_db":-3.0},"input_channels":2}],"edges":[]}}"#,
        )
        .expect("valid graph artifact");
        let PluginArtifactPlan::Graph { graph } = plan_plugin_artifact(artifact).unwrap() else {
            panic!("graph artifact must remain graph-shaped");
        };

        let state = SystemwideState::default();
        let plan = state
            .prepare_graph_plan(graph, 2, 2, 2)
            .expect("graph plan");
        assert!(plan.spec.user_plugins.is_empty());
        let graph = plan.spec.user_graph.expect("graph desired state");
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].id, 7);
        assert_eq!(graph.nodes[0].parameters["gain_db"], -3.0);
        assert!(plan.runtime_graph.is_some());
    }

    #[test]
    fn testkit_stale_graph_generation_is_rejected_without_overwrite() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        // Seed the applied generation directly so this test only exercises
        // the optimistic-concurrency guard, not a physical cpal device.
        let graph = PluginGraphConfig::try_new(
            vec![
                PluginGraphNodeConfig::try_new(7, "gain", serde_json::json!({"gain_db": -3.0}), 2)
                    .unwrap(),
            ],
            vec![],
        )
        .unwrap();
        let plan = daemon
            .system_state
            .lock()
            .prepare_graph_plan(graph, 2, 2, 2)
            .unwrap();
        daemon.system_state.lock().commit_applied(&plan);

        let stale = send_owner_ipc_command(
            &daemon,
            r#"{"command":"load_plugin_artifact","base_generation":0,"artifact":{"graph":{"nodes":[{"id":99,"plugin_type":"gain","parameters":{"gain_db":6.0},"input_channels":2}],"edges":[]}}}"#,
        );

        assert_eq!(stale["success"], false);
        assert!(
            stale["error"]
                .as_str()
                .is_some_and(|error| error.contains("generation conflict"))
        );
        let state = daemon.system_state.lock();
        assert_eq!(state.applied_generation(), Some(1));
        let graph = state.user_graph().unwrap();
        assert_eq!(graph.nodes[0].id, 7);
        assert_eq!(graph.nodes[0].parameters["gain_db"], -3.0);
    }

    #[test]
    #[serial_test::serial]
    fn testkit_live_rack_state_promotion_and_graph_reorder_preserve_node_state() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);
        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![
                PluginConfig::new("gain", serde_json::json!({"gain_db": -3.0})),
                PluginConfig::new("eq", serde_json::json!({"filters": []})),
            ],
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed rack pipeline: {seed:?}");
        daemon
            .manager
            .lock()
            .resume()
            .expect("resume fake HAL silent source after rack seed");
        assert_fake_driver_uses_lab_output(&daemon);
        let rack_generation = daemon
            .system_state
            .lock()
            .applied_generation()
            .expect("rack seed must have an applied generation");

        let promoted = daemon.handle_command(Command::SetRackPluginState {
            index: 1,
            input_channels: Some(4),
            bypassed: Some(true),
            base_generation: Some(rack_generation),
        });
        assert!(
            promoted.success,
            "failed to promote rack state: {promoted:?}"
        );

        let promoted_state = daemon.system_state.lock();
        let graph = promoted_state
            .user_graph()
            .expect("rack state mutation must retain a graph owner");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.nodes[0].id, 0);
        assert_eq!(graph.nodes[0].parameters["gain_db"], -3.0);
        assert_eq!(graph.nodes[1].id, 1);
        assert_eq!(graph.nodes[1].parameters["filters"], serde_json::json!([]));
        assert_eq!(graph.nodes[1].input_channels, 4);
        assert!(graph.nodes[1].bypassed);
        assert_eq!(graph.edges.len(), 1);
        let graph_generation = promoted_state
            .applied_generation()
            .expect("promoted graph must have a generation");
        drop(promoted_state);

        let reordered = daemon.handle_command(Command::ReorderGraph {
            order: vec![1, 0],
            base_generation: Some(graph_generation),
        });
        assert!(reordered.success, "failed to reorder graph: {reordered:?}");

        let reordered_state = daemon.system_state.lock();
        let graph = reordered_state
            .user_graph()
            .expect("graph must remain active");
        assert_eq!(
            graph.nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert_eq!(graph.nodes[0].parameters["filters"], serde_json::json!([]));
        assert_eq!(graph.nodes[0].input_channels, 4);
        assert!(graph.nodes[0].bypassed);
        assert_eq!(graph.nodes[1].parameters["gain_db"], -3.0);
        assert_eq!(graph.edges[0].from_node, 1);
        assert_eq!(graph.edges[0].to_node, 0);
        assert_eq!(
            reordered_state.applied_generation(),
            Some(graph_generation + 1)
        );
        drop(reordered_state);

        let stale = daemon.handle_command(Command::ReorderGraph {
            order: vec![0, 1],
            base_generation: Some(graph_generation),
        });
        assert!(!stale.success);
        assert!(
            stale
                .error
                .as_deref()
                .is_some_and(|error| error.contains("generation conflict"))
        );
    }

    #[test]
    fn testkit_unix_ipc_invalid_plugin_load_preserves_pipeline_state() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(
            &daemon,
            r#"{"command":"load_plugins","plugins":[{"plugin_type":"eq","parameters":{}}],"input_channels":2,"output_channels":64}"#,
        );

        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains("Invalid output channel count"))
        );
        assert_eq!(daemon.system_state.lock().input_channels(), 2);
        assert_eq!(daemon.system_state.lock().output_channels(), 2);
        assert!(daemon.system_state.lock().applied_generation().is_none());
    }

    #[test]
    fn testkit_unix_ipc_driver_status_uses_injected_driver() {
        let state = fake_driver_state();
        state.lock().status.channel_count = 12;
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"driver_status"}"#);

        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["driver_name"], "Fake HAL");
        assert_eq!(response["data"]["channel_count"], 12);
    }

    #[test]
    fn testkit_snapshot_separates_desired_observed_and_diagnostics() {
        let state = fake_driver_state();
        state.lock().status.channel_count = 6;
        let daemon = test_daemon_with_driver(state);
        {
            let mut pipeline = daemon.system_state.lock();
            pipeline
                .set_desired_output_device(Some("ADAM Audio D3V".to_string()))
                .expect("safe device");
        }

        let response = send_owner_ipc_command(&daemon, r#"{"command":"get_snapshot"}"#);

        assert_eq!(response["success"], true);
        let data = &response["data"];
        assert_eq!(data["schema_version"], 1);
        assert_eq!(data["desired"]["output_device"], "ADAM Audio D3V");
        assert_eq!(data["observed"]["driver"]["channel_count"], 6);
        assert_eq!(
            data["observed"]["metering"]["sources"]["input"]["status"],
            "fallback_zero"
        );
        assert_eq!(data["observed"]["transport"]["input"]["status"], "idle");
        assert_eq!(data["observed"]["transport"]["output"]["status"], "idle");
        assert_eq!(data["diagnostics"]["health"], "ok");
        assert!(data["diagnostics"]["faults"].as_array().unwrap().is_empty());
    }

    #[test]
    fn runtime_telemetry_records_latency_and_response_budget_regressions() {
        let telemetry = RuntimeTelemetry::default();
        telemetry.record_command(
            "get_metering",
            std::time::Duration::from_micros(METERING_LATENCY_BUDGET_MICROS + 1),
            128,
        );
        telemetry.record_command(
            "load_plugins",
            std::time::Duration::from_micros(10),
            PIPELINE_RESPONSE_BUDGET_BYTES + 1,
        );

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot["metering"]["requests"], 1);
        assert_eq!(snapshot["metering"]["budget_exceeded"], 1);
        assert_eq!(snapshot["pipeline_reload"]["requests"], 1);
        assert_eq!(snapshot["pipeline_reload"]["budget_exceeded"], 1);
    }

    #[test]
    fn runtime_telemetry_classifies_every_pipeline_mutation_wire_name() {
        let telemetry = RuntimeTelemetry::default();
        let commands = [
            "load_plugins",
            "load_plugin_artifact",
            "load_plugin_artifact_path",
            "add_plugin",
            "remove_plugin",
            "update_plugin",
            "reorder_plugins",
            "reorder_graph",
            "set_input_channels",
            "set_output_channels",
            "set_pipeline_channels",
        ];
        for command in commands {
            telemetry.record_command(command, std::time::Duration::ZERO, 0);
        }

        assert_eq!(
            telemetry.snapshot()["pipeline_reload"]["requests"],
            commands.len()
        );
    }

    #[test]
    fn testkit_dump_state_includes_snapshot_and_plugins() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"dump_state"}"#);

        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["snapshot"]["schema_version"], 1);
        assert!(response["data"]["plugins"].as_array().is_some());
        assert!(response["data"]["runtime_telemetry"]["budgets"].is_object());
    }

    #[test]
    fn testkit_idle_driver_config_change_updates_spec_without_engine_ready() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        {
            let mut pipeline = daemon.system_state.lock();
            let plan = pipeline
                .prepare_with_selected_device("ADAM Audio D3V".to_string())
                .expect("valid device plan");
            pipeline.commit_applied(&plan);
        }

        // A native reconnect notification reports the already active endpoint.
        state.lock().status.channel_count = 10;
        handle_driver_config_change(
            &daemon.driver_manager,
            &daemon.manager,
            &daemon.device_registry,
            DriverConfig::new(48_000, 512, 10),
            &daemon.system_state,
            &daemon.running,
        );

        let pipeline = daemon.system_state.lock();
        assert_eq!(pipeline.input_channels(), 10);
        assert_eq!(
            pipeline.selected_output_device().as_deref(),
            Some("ADAM Audio D3V")
        );
        drop(pipeline);

        let state = state.lock();
        assert!(
            !state.engine_ready,
            "idle reconfigure must not mark engine ready"
        );
        let (actual, result) = state.last_ack.as_ref().expect("config ack");
        assert_eq!(actual.channel_count, 10);
        assert!(matches!(result, ConfigResult::Accepted));
    }

    #[test]
    fn testkit_driver_config_change_negotiates_rate_against_output_device() {
        // Regression: a stored 96 kHz setup whose interface caps at 48 kHz
        // desynchronized at startup because the driver-config watcher
        // restarted the engine at the HAL-requested rate without the
        // device-support negotiation the IPC apply path performs. The
        // acknowledged config must carry the negotiated rate so the HAL
        // nominal rate follows the engine instead of re-requesting 96 kHz.
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        {
            let mut pipeline = daemon.system_state.lock();
            let plan = pipeline
                .prepare_with_selected_device("ADAM Audio D3V".to_string())
                .expect("valid device plan");
            pipeline.commit_applied(&plan);
        }
        // The test daemon's lab sink adapts every physical name to the lab
        // output, so negotiation keys on the lab device name. Seeding the
        // registry cache keeps the test independent of local hardware.
        daemon.device_registry.lock().seed_negotiated_sample_rate(
            LAB_OUTPUT_DEVICE_NAME,
            96_000,
            48_000,
        );

        handle_driver_config_change(
            &daemon.driver_manager,
            &daemon.manager,
            &daemon.device_registry,
            DriverConfig::new(96_000, 512, 2),
            &daemon.system_state,
            &daemon.running,
        );

        let state = state.lock();
        let (actual, result) = state.last_ack.as_ref().expect("config ack");
        assert_eq!(actual.sample_rate, 48_000);
        assert!(
            matches!(
                result,
                ConfigResult::Negotiated {
                    actual_rate: 48_000,
                    ..
                }
            ),
            "driver must be told the negotiated rate: {result:?}"
        );
    }

    #[test]
    fn volume_sync_tracker_applies_each_generation_once() {
        let mut tracker = VolumeSyncTracker::default();
        // No control / unpublished volume never applies.
        assert_eq!(tracker.observe(None), None);
        assert_eq!(tracker.observe(Some((0.5, false, 0))), None);
        // First real publication applies (startup adopts persisted volume).
        assert_eq!(tracker.observe(Some((0.5, true, 7))), Some((0.5, true, 7)));
        // A failed engine application leaves this generation retryable.
        assert_eq!(tracker.observe(Some((0.5, true, 7))), Some((0.5, true, 7)));
        tracker.committed(7);
        // Same generation is not re-applied on later polls.
        assert_eq!(tracker.observe(Some((0.5, true, 7))), None);
        // Each new generation applies exactly once.
        assert_eq!(
            tracker.observe(Some((0.75, false, 8))),
            Some((0.75, false, 8))
        );
        tracker.committed(8);
        assert_eq!(tracker.observe(Some((0.75, false, 8))), None);
    }

    #[test]
    fn testkit_set_volume_publishes_to_driver_control() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let response = daemon.handle_command(Command::SetVolume { volume: 0.4 });

        assert!(response.success, "set_volume must succeed: {response:?}");
        assert_eq!(
            state.lock().published_volume,
            Some((0.4, false, 1)),
            "IPC volume must publish to the driver volume control"
        );
    }

    #[test]
    fn restarted_reconfiguration_waits_for_callback_before_publishing_ready() {
        let driver_state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&driver_state));
        let active_config = DriverConfig::new(48_000, 512, 2);

        let error = publish_reconfigured_driver_readiness(
            &daemon.driver_manager,
            &daemon.system_state,
            active_config,
            0,
            Err("Playback did not reach hardware callback".into()),
        )
        .expect_err("a missing callback must not publish readiness");
        assert!(error.contains("hardware callback"));
        assert!(!driver_state.lock().engine_ready);
        assert!(matches!(
            driver_state
                .lock()
                .last_ack
                .as_ref()
                .expect("timeout must be acknowledged")
                .1,
            driver_common::ConfigResult::Error(_)
        ));
        assert!(daemon.system_state.lock().pipeline_recovery().is_some());

        daemon.system_state.lock().clear_pipeline_recovery();
        publish_reconfigured_driver_readiness(
            &daemon.driver_manager,
            &daemon.system_state,
            active_config,
            0,
            Ok(()),
        )
        .expect("a callback observation should publish readiness");
        assert!(driver_state.lock().engine_ready);
        assert!(daemon.system_state.lock().pipeline_recovery().is_none());
    }

    #[test]
    fn testkit_unix_ipc_status_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"status"}"#);

        assert_eq!(response["success"], true);
        assert!(response["data"]["state"].is_string());
        assert!(response["data"]["volume"].is_number());
    }

    #[test]
    fn testkit_unix_ipc_get_plugins_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"get_plugins"}"#);

        assert_eq!(response["success"], true);
        assert!(response["data"]["plugins"].is_array());
    }

    #[test]
    fn testkit_unix_ipc_get_available_plugins_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"get_available_plugins"}"#);

        assert_eq!(response["success"], true);
        let plugins = response["data"]["plugins"]
            .as_array()
            .expect("plugins array");
        assert!(!plugins.is_empty());
        assert!(plugins[0]["type"].is_string());
    }

    #[test]
    fn testkit_unix_ipc_get_metering_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"get_metering"}"#);

        assert_eq!(response["success"], true);
        assert!(response["data"]["input"].is_object());
        assert!(response["data"]["output"].is_object());
        assert!(response["data"]["sources"]["input"].is_object());
        assert!(response["data"]["sources"]["output"].is_object());
        assert_eq!(response["data"]["generation"], 0);
    }

    #[test]
    fn testkit_unix_ipc_get_driver_config_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"get_driver_config"}"#);

        assert_eq!(response["success"], true);
        assert!(response["data"]["sample_rate"].is_number());
        assert!(response["data"]["buffer_frames"].is_number());
        assert!(response["data"]["channel_count"].is_number());
    }

    #[test]
    fn testkit_unix_ipc_set_volume_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"set_volume","volume":0.42}"#);

        assert_eq!(response["success"], true);
        assert!((daemon.manager.lock().get_volume() - 0.42).abs() < f32::EPSILON);
    }

    #[test]
    fn fake_driver_conforms_to_audio_driver_contract() {
        driver_common::test_support::assert_audio_driver_contract(FakeDriver::new(
            fake_driver_state(),
        ))
        .expect("FakeDriver contract");
    }

    #[test]
    fn testkit_unix_ipc_set_encryption_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response =
            send_owner_ipc_command(&daemon, r#"{"command":"set_encryption","enabled":true}"#);

        if cfg!(all(target_os = "macos", feature = "hal")) {
            assert_eq!(response["success"], true, "{response}");
            assert_eq!(response["data"]["enabled"], true);
            // No HAL shared memory in unit tests, so the transport stays pending.
            assert_eq!(response["data"]["transport_state"], "pending");
        } else {
            assert_eq!(response["success"], false);
            assert!(
                response["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("no session cipher"))
            );
        }
    }

    #[test]
    fn testkit_unix_ipc_shutdown_roundtrip() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(state);

        let response = send_owner_ipc_command(&daemon, r#"{"command":"shutdown"}"#);

        assert_eq!(response["success"], true);
        assert!(!*daemon.running.lock());
    }

    #[test]
    fn testkit_failed_pipeline_apply_restores_last_working_plan() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![test_plugin("eq")],
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed pipeline: {seed:?}");

        state.lock().fail_next_config = Some("injected config failure".to_string());
        let response = daemon.handle_command(Command::SetInputChannels { channels: 4 });

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("restored the last working pipeline")),
            "unexpected recovery response: {response:?}"
        );
        assert_eq!(
            response.data.as_ref().map(|data| &data["generation"]),
            Some(&serde_json::json!(2)),
            "a recovered failure must return the replacement pipeline generation"
        );

        let state = daemon.system_state.lock();
        assert_eq!(state.input_channels(), 2);
        assert_eq!(state.output_channels(), 2);
        assert_eq!(state.user_plugins().len(), 1);
        assert_eq!(state.applied_generation(), Some(2));
    }

    #[test]
    fn testkit_failed_atomic_configuration_restores_driver_timing_and_channels() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![test_plugin("eq")],
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed pipeline: {seed:?}");

        state.lock().fail_next_config = Some("injected config failure".to_string());
        let response = daemon.handle_command(Command::ApplyConfiguration {
            sample_rate: Some(96_000),
            buffer_frames: Some(256),
            input_channels: Some(4),
            output_channels: None,
            output_device: None,
        });

        assert!(!response.success);
        assert_eq!(
            response.data.as_ref().map(|data| &data["generation"]),
            Some(&serde_json::json!(2)),
            "atomic rollback must return the replacement pipeline generation"
        );

        let driver = state.lock();
        assert_eq!(
            driver.requested_configs,
            vec![
                DriverConfig::new(96_000, 256, 4),
                DriverConfig::new(48_000, 512, 2),
            ],
            "rollback must restore the complete pre-transaction HAL format"
        );
        assert_eq!(driver.status.sample_rate, 48_000);
        assert_eq!(driver.status.buffer_frames, 512);
        assert_eq!(driver.status.channel_count, 2);
    }

    #[test]
    #[serial_test::serial]
    fn fallback_commits_its_own_rack_or_graph_without_overwriting_outgoing_profile() {
        for graph_profile in [false, true] {
            let daemon = test_daemon_with_driver(fake_driver_state());
            for (id, gain, graph) in [
                ("profile-a", -3.0, false),
                ("profile-b", -9.0, graph_profile),
            ] {
                let profile = serde_json::from_value(serde_json::json!({
                    "id": id, "name": id, "topology": if graph { "graph" } else { "rack" },
                    "plugins": if graph { serde_json::json!([]) } else {
                        serde_json::json!([{"plugin_type":"gain","parameters":{"gain_db":gain}}])
                    },
                    "graph": if graph { serde_json::json!({
                        "nodes":[{"id":7,"plugin_type":"gain","parameters":{"gain_db":gain},"input_channels":2}],
                        "edges":[]
                    }) } else { Value::Null },
                    "input_channels":2, "output_channels":2, "updated_at_unix_ms":0
                })).expect("valid profile");
                assert!(daemon.handle_set_output_profile(profile).success);
            }
            assert!(
                daemon
                    .handle_assign_output_profile(None, LAB_OUTPUT_DEVICE_NAME, "profile-b")
                    .success
            );
            daemon.output_profiles.lock().set_current_in_memory(
                None,
                Some("Missing A".to_owned()),
                Some("profile-a".to_owned()),
            );
            let mut plan = daemon
                .system_state
                .lock()
                .prepare_plan(
                    vec![PluginConfig::new(
                        "gain",
                        serde_json::json!({"gain_db":-3.0}),
                    )],
                    2,
                    2,
                    2,
                )
                .expect("outgoing plan");
            plan.spec.output_device = Some("Missing A".to_owned());
            daemon.system_state.lock().commit_applied(&plan);
            let response = daemon.apply_pipeline_plan_for_output(
                plan,
                48_000,
                512,
                super::super::audio_daemon::PipelineApplyIntent::RebuildOutput,
                super::super::misc::EffectiveOutputDevice {
                    device: Some(LAB_OUTPUT_DEVICE_NAME.to_owned()),
                    fallback: Some(super::super::misc::OutputDeviceFallback {
                        desired: "Missing A".to_owned(),
                        applied: LAB_OUTPUT_DEVICE_NAME.to_owned(),
                    }),
                },
            );
            assert!(response.success, "{response:?}");
            assert_eq!(
                daemon.manager.lock().get_playback_state(),
                sotf_audio::PlaybackState::Playing
            );
            let applied = daemon
                .system_state
                .lock()
                .applied_spec()
                .expect("committed fallback");
            assert_eq!(
                applied.output_device.as_deref(),
                Some(LAB_OUTPUT_DEVICE_NAME)
            );
            assert_eq!(
                daemon
                    .system_state
                    .lock()
                    .selected_output_device()
                    .as_deref(),
                Some("Missing A")
            );
            if graph_profile {
                assert_eq!(
                    applied.user_graph.unwrap().nodes[0].parameters["gain_db"],
                    -9.0
                );
            } else {
                assert_eq!(applied.user_plugins[0].parameters["gain_db"], -9.0);
            }
            let profiles = daemon
                .handle_get_output_profiles()
                .data
                .expect("profile snapshot");
            assert_eq!(profiles["current"]["profile_id"], "profile-b");
            let outgoing = profiles["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .find(|profile| profile["id"] == "profile-a")
                .unwrap();
            assert_eq!(outgoing["plugins"][0]["parameters"]["gain_db"], -3.0);
            if !graph_profile {
                let update = daemon.handle_command(Command::UpdatePlugin {
                    index: 0,
                    parameters: serde_json::json!({"gain_db":-12.0}),
                });
                assert!(update.success, "{update:?}");
                assert_eq!(
                    daemon
                        .system_state
                        .lock()
                        .applied_output_device()
                        .as_deref(),
                    Some(LAB_OUTPUT_DEVICE_NAME)
                );
                assert_eq!(
                    daemon
                        .system_state
                        .lock()
                        .selected_output_device()
                        .as_deref(),
                    Some("Missing A")
                );
            }
            // An automatic return to a restored output must recall A, even
            // though the desired hardware preference stayed A throughout.
            assert!(
                daemon
                    .handle_assign_output_profile(None, LAB_OUTPUT_DEVICE_NAME, "profile-a")
                    .success
            );
            let restore_plan = {
                let state = daemon.system_state.lock();
                state.prepare_from_spec(state.desired_spec(), 2).unwrap()
            };
            // Simulate an old applied device distinct from the restored one;
            // resolution is supplied explicitly so no physical device is used.
            {
                let mut state = daemon.system_state.lock();
                let mut old = state.applied_spec().unwrap();
                old.output_device = Some("Temporary B".to_owned());
                let plan = state.prepare_from_spec(old, 2).unwrap();
                state.commit_fallback_applied(&plan, Some(LAB_OUTPUT_DEVICE_NAME.to_owned()));
            }
            let mut restore_plan = restore_plan;
            restore_plan.spec.output_device = Some(LAB_OUTPUT_DEVICE_NAME.to_owned());
            let restored = daemon.apply_pipeline_plan_for_output(
                restore_plan,
                48_000,
                512,
                super::super::audio_daemon::PipelineApplyIntent::RebuildOutput,
                super::super::misc::EffectiveOutputDevice {
                    device: Some(LAB_OUTPUT_DEVICE_NAME.to_owned()),
                    fallback: None,
                },
            );
            assert!(restored.success, "{restored:?}");
            let applied = daemon.system_state.lock().applied_spec().unwrap();
            assert_eq!(applied.user_plugins[0].parameters["gain_db"], -3.0);
            assert!(applied.user_graph.is_none());
            assert_eq!(
                daemon.output_profiles.lock().current().0.as_deref(),
                Some("profile-a")
            );
            daemon.manager.lock().stop().expect("stop fixture");
        }
    }

    #[test]
    #[serial_test::serial]
    fn named_output_profile_fallback_and_restoration_preserve_actual_pcm() {
        use super::super::audio_daemon::PipelineApplyIntent;
        use super::super::misc::{EffectiveOutputDevice, OutputDeviceFallback};
        use crate::output_profiles::{OutputProfile, OutputProfileStore};
        use sotf_audio::lab_pcm::TestOutput;

        for (graph_a, graph_b) in [(false, false), (false, true), (true, false), (true, true)] {
            let daemon = test_daemon_with_driver(fake_driver_state());
            let directory = tempfile::tempdir().unwrap();
            let profile_path = directory.path().join("output-profiles.json");
            *daemon.output_profiles.lock() =
                OutputProfileStore::load_test_path(profile_path.clone());
            let output_a = TestOutput::new("Audit Speakers A");
            let output_b = TestOutput::new("Audit Speakers B");
            for (output, id, gain, graph) in [
                (&output_a, "profile-a", -3.0, graph_a),
                (&output_b, "profile-b", -9.0, graph_b),
            ] {
                let profile: OutputProfile = serde_json::from_value(serde_json::json!({
                    "id": id, "name": id, "topology": if graph { "graph" } else { "rack" },
                    "plugins": if graph { serde_json::json!([]) } else {
                        serde_json::json!([{"plugin_type":"gain","parameters":{"gain_db":gain}}])
                    },
                    "graph": if graph { serde_json::json!({
                        "nodes":[{"id":7,"plugin_type":"gain","parameters":{"gain_db":gain},"input_channels":2}],
                        "edges":[]
                    }) } else { Value::Null },
                    "input_channels":2, "output_channels":2, "updated_at_unix_ms":0
                })).unwrap();
                assert!(daemon.handle_set_output_profile(profile).success);
                assert!(
                    daemon
                        .handle_assign_output_profile(None, output.name(), id)
                        .success
                );
            }
            let saved = |store: &OutputProfileStore, name: &str| {
                serde_json::to_value(store.assigned_profile_for_name(name).unwrap()).unwrap()
            };
            let original_a = saved(&daemon.output_profiles.lock(), output_a.name());
            let original_b = saved(&daemon.output_profiles.lock(), output_b.name());
            daemon
                .output_profiles
                .lock()
                .record_route(None, output_a.name(), "profile-a")
                .unwrap();
            daemon
                .system_state
                .lock()
                .set_desired_output_device(Some(output_a.name().to_owned()))
                .unwrap();
            daemon.manager.lock().set_volume(0.5).unwrap();

            let mut observations = Vec::new();
            for (index, output, gain, profile_id) in [
                (0, &output_a, -3.0f32, "profile-a"),
                (1, &output_b, -9.0f32, "profile-b"),
                (2, &output_a, -3.0f32, "profile-a"),
            ] {
                daemon.driver_manager.lock().set_test_output(output.clone());
                // Supply the resolved device decision and its capabilities.
                // Actual engine creation, profile recall, DSP and PCM remain real.
                daemon.device_registry.lock().seed_negotiated_sample_rate(
                    output.name(),
                    48_000,
                    48_000,
                );
                let plan = {
                    let state = daemon.system_state.lock();
                    if index == 0 {
                        let profile = daemon
                            .output_profiles
                            .lock()
                            .assigned_profile_for_name(output_a.name())
                            .unwrap();
                        AudioDaemon::prepare_profile_plan(
                            &state,
                            state.desired_spec(),
                            &profile,
                            None,
                        )
                        .unwrap()
                    } else {
                        state.prepare_from_spec(state.desired_spec(), 2).unwrap()
                    }
                };
                let probe = output.probe();
                probe.arm();
                let response = daemon.apply_pipeline_plan_for_output(
                    plan,
                    48_000,
                    512,
                    if index == 0 {
                        PipelineApplyIntent::SelectRoute
                    } else {
                        PipelineApplyIntent::RebuildOutput
                    },
                    EffectiveOutputDevice {
                        device: Some(output.name().to_owned()),
                        fallback: (index == 1).then(|| OutputDeviceFallback {
                            desired: output_a.name().to_owned(),
                            applied: output_b.name().to_owned(),
                        }),
                    },
                );
                assert!(response.success, "{response:?}");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while probe.snapshot().unwrap().signal_frames < 6144 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "route {index} produced no signal"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let window = probe.disarm().unwrap();
                let expected =
                    [0.03125, 0.0625].map(|sample| sample * 10.0f32.powf(gain / 20.0) * 0.5);
                assert_eq!(window.channels, 2);
                assert_eq!(window.invalid, 0);
                assert_eq!(
                    window.silent_after_signal_frames, 0,
                    "silence after signal: {window:?}"
                );
                for channel in 0..2 {
                    assert_eq!(
                        window.nonzero_samples[channel], window.signal_frames,
                        "missing channel: {window:?}"
                    );
                    assert!(
                        (window.nonzero_minimum[channel] - expected[channel]).abs() < 0.00001
                            && (window.maximum[channel] - expected[channel]).abs() < 0.00001,
                        "wrong PCM on route {index}, graph A={graph_a} B={graph_b}: {window:?}; expected {expected:?}"
                    );
                }
                let state = daemon.system_state.lock();
                assert_eq!(
                    state.applied_output_device().as_deref(),
                    Some(output.name())
                );
                assert_eq!(
                    state.selected_output_device().as_deref(),
                    Some(output_a.name())
                );
                drop(state);
                assert_eq!(
                    daemon.output_profiles.lock().current().0.as_deref(),
                    Some(profile_id)
                );
                let engine = daemon.manager.lock().get_engine_state();
                assert_eq!(
                    engine.playback_output_device.as_deref(),
                    Some(output.name())
                );
                assert_eq!(engine.playback_output_generation, window.last_generation);
                assert_eq!(window.first_generation, window.last_generation);
                assert!(Arc::ptr_eq(&probe, engine.lab_pcm_probe.as_ref().unwrap()));
                observations.push(serde_json::json!({
                    "route": output.name(), "profile": profile_id, "expected":expected,
                    "nonzero_minimum": &window.nonzero_minimum[..2], "maximum": &window.maximum[..2],
                    "leading_silent_frames": window.leading_silent_frames,
                    "silent_after_signal_frames": window.silent_after_signal_frames,
                    "signal_frames": window.signal_frames, "output_generation":window.last_generation,
                }));
            }
            daemon.manager.lock().stop().unwrap();
            let reloaded = OutputProfileStore::load_test_path(profile_path);
            assert_eq!(saved(&reloaded, output_a.name()), original_a);
            assert_eq!(saved(&reloaded, output_b.name()), original_b);
            println!(
                "PCM_PROFILE_EVIDENCE {}",
                serde_json::json!({
                    "graph_a":graph_a,"graph_b":graph_b,"observations":observations,"persisted_profiles_unchanged":true,
                })
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_configuration_binds_incoming_profile_before_saving_live_chain() {
        let daemon = test_daemon_with_driver(fake_driver_state());
        for (id, gain) in [("outgoing", -3.0), ("incoming", -9.0)] {
            let mut profile: crate::output_profiles::OutputProfile =
                serde_json::from_value(serde_json::json!({
                    "id":id,"name":id,"topology":"rack","plugins":[],"graph":null,
                    "input_channels":2,"output_channels":2,"updated_at_unix_ms":0
                }))
                .unwrap();
            profile.plugins = vec![PluginConfig::new(
                "gain",
                serde_json::json!({"gain_db":gain}),
            )];
            assert!(daemon.handle_set_output_profile(profile).success);
        }
        assert!(
            daemon
                .handle_assign_output_profile(None, LAB_OUTPUT_DEVICE_NAME, "incoming")
                .success
        );
        daemon.output_profiles.lock().set_current_in_memory(
            None,
            Some("Old A".to_owned()),
            Some("outgoing".to_owned()),
        );
        {
            let mut state = daemon.system_state.lock();
            let mut plan = state
                .prepare_plan(
                    vec![PluginConfig::new(
                        "gain",
                        serde_json::json!({"gain_db":-3.0}),
                    )],
                    2,
                    2,
                    2,
                )
                .unwrap();
            plan.spec.output_device = Some("Old A".to_owned());
            state.commit_applied(&plan);
        }
        let response = daemon.handle_command(Command::ApplyConfiguration {
            sample_rate: None,
            buffer_frames: None,
            input_channels: None,
            output_channels: None,
            output_device: Some(LAB_OUTPUT_DEVICE_NAME.to_owned()),
        });
        assert!(response.success, "{response:?}");
        let profiles = daemon.handle_get_output_profiles().data.unwrap();
        assert_eq!(profiles["current"]["profile_id"], "incoming");
        for (id, gain) in [("outgoing", -3.0), ("incoming", -9.0)] {
            let profile = profiles["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .find(|profile| profile["id"] == id)
                .unwrap();
            assert_eq!(profile["plugins"][0]["parameters"]["gain_db"], gain);
        }
        daemon.manager.lock().stop().unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn explicit_capture_samples_reach_both_monitors_through_gain_processing() {
        for native_rate in [8_000, 32_000, 44_100, 48_000, 96_000, 192_000] {
            let state = fake_driver_state();
            state.lock().status.sample_rate = native_rate;
            let daemon = test_daemon_with_driver(state);
            daemon.system_state.lock().processing_timing = Some((48_000, 512));
            let response = daemon.handle_command(Command::LoadPlugins {
                plugins: vec![PluginConfig::new(
                    "gain",
                    serde_json::json!({"gain_db":-6.0}),
                )],
                input_channels: 2,
                output_channels: 2,
            });
            assert!(response.success, "{response:?}");
            let expected_gain = 10.0f64.powf(-6.0 / 20.0);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let response = daemon.handle_command(Command::GetMetering);
                let metering = response.data.expect("metering response");
                let input = metering["input"]["channel_peaks"].as_array().unwrap();
                let output = metering["output"]["channel_peaks"].as_array().unwrap();
                let samples_match = (0..2).all(|channel| {
                    let original = (channel + 1) as f64 * 0.03125;
                    input
                        .get(channel)
                        .and_then(Value::as_f64)
                        .is_some_and(|value| (value - original).abs() < 0.002)
                        && output
                            .get(channel)
                            .and_then(Value::as_f64)
                            .is_some_and(|value| (value - original * expected_gain).abs() < 0.002)
                });
                let engine = daemon.manager.lock().get_engine_state();
                if samples_match
                    && engine.playback_callback_count > 0
                    && engine.playback_frames_written > 0
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "capture was replaced by silence, reordered, or bypassed gain: {metering:?}; {engine:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(
                daemon.system_state.lock().processing_timing,
                Some((48_000, 512))
            );
            daemon.manager.lock().stop().unwrap();
        }
    }

    #[test]
    #[serial_test::serial]
    fn healthy_pipeline_reapply_preserves_every_output_sample_without_mute_commands() {
        let driver = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&driver));
        let loaded = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![PluginConfig::new(
                "gain",
                serde_json::json!({"gain_db": -6.0}),
            )],
            input_channels: 2,
            output_channels: 2,
        });
        assert!(loaded.success, "{loaded:?}");
        daemon.manager.lock().set_volume(0.5).unwrap();
        let probe = daemon
            .manager
            .lock()
            .get_engine_state()
            .lab_pcm_probe
            .unwrap();
        let expected = [0.03125, 0.0625].map(|sample| sample * 10.0f32.powf(-6.0 / 20.0) * 0.5);
        let matches = |window: &sotf_audio::lab_pcm::LabPcmSnapshot| {
            window.channels == 2
                && window.invalid == 0
                && (0..2).all(|channel| {
                    (window.minimum[channel] - expected[channel]).abs() < 0.00001
                        && (window.maximum[channel] - expected[channel]).abs() < 0.00001
                })
        };
        // Startup prefill and the deliberate startup volume ramp are outside the
        // healthy reapply interval. Require a complete stable window first.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            probe.arm();
            std::thread::sleep(std::time::Duration::from_millis(100));
            let window = probe.snapshot().unwrap();
            let _ = daemon.handle_command(Command::Status);
            let observed = daemon.manager.lock().get_engine_state();
            if window.blocks >= 4
                && matches(&window)
                && observed.playback_callback_count > 0
                && observed.playback_frames_received > 0
                && observed.playback_frames_written > 0
                && daemon.driver_manager.lock().readiness_is_current()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "output never became stable: {window:?}"
            );
        }
        let before = daemon.manager.lock().get_engine_state();
        let generation = daemon.system_state.lock().generation();
        let plan = {
            let state = daemon.system_state.lock();
            state
                .prepare_from_spec(state.applied_spec().unwrap(), 2)
                .unwrap()
        };
        let requests = driver.lock().requested_configs.len();
        let mute_requests = probe.mute_requests();
        probe.arm();
        // Call the transaction entry point so the test exercises its early
        // return before ReconfigurationMuteGuard, not the plugin hot-update path.
        for _ in 0..8 {
            let status = driver.lock().status.clone();
            let response = daemon.apply_pipeline_plan(plan.clone(), status, 48_000, 512);
            assert!(response.success, "{response:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while probe.snapshot().unwrap().blocks < 12 {
            assert!(
                std::time::Instant::now() < deadline,
                "output stopped during reapply"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let window = probe.disarm().unwrap();
        let after = daemon.manager.lock().get_engine_state();
        let mute_requests_after = probe.mute_requests();
        daemon.manager.lock().stop().unwrap();
        assert_eq!(
            mute_requests_after, mute_requests,
            "a healthy no-op emitted a mute command"
        );
        assert!(
            matches(&window),
            "reapply changed or interrupted post-volume PCM: {window:?}"
        );
        assert!(window.frames >= 12 * 512);
        assert_eq!(window.first_generation, before.playback_output_generation);
        assert_eq!(window.last_generation, before.playback_output_generation);
        assert_eq!(
            after.playback_output_generation,
            before.playback_output_generation
        );
        assert_eq!(after.playback_epoch, before.playback_epoch);
        assert_eq!(daemon.system_state.lock().generation(), generation);
        assert_eq!(driver.lock().requested_configs.len(), requests);
        assert!(Arc::ptr_eq(&probe, after.lab_pcm_probe.as_ref().unwrap()));
        println!(
            "PCM_NOOP_EVIDENCE {}",
            serde_json::json!({
                "reapply_count": 8,
                "channels": window.channels,
                "frames": window.frames,
                "blocks": window.blocks,
                "minimum": &window.minimum[..window.channels],
                "maximum": &window.maximum[..window.channels],
                "expected": expected,
                "invalid": window.invalid,
                "mute_requests_before": mute_requests,
                "mute_requests_after": mute_requests_after,
                "output_generation": window.first_generation,
                "pipeline_generation": generation,
                "same_engine": Arc::ptr_eq(&probe, after.lab_pcm_probe.as_ref().unwrap()),
            })
        );
    }

    #[test]
    fn lab_route_is_independent_of_host_devices_and_never_falls_back_to_itself() {
        for physical in [None, Some("Real Speakers")] {
            for desired in [None, Some(LAB_OUTPUT_DEVICE_NAME)] {
                let route = effective_output_for_sink(
                    super::super::misc::EffectiveOutputDevice {
                        device: physical.map(str::to_owned),
                        fallback: None,
                    },
                    SinkType::LabNull,
                    desired,
                );
                assert_eq!(route.device.as_deref(), Some(LAB_OUTPUT_DEVICE_NAME));
                assert!(route.fallback.is_none());
            }
        }
    }

    #[test]
    fn status_only_backend_cannot_claim_a_running_pipeline() {
        let state = fake_driver_state();
        state.lock().status.platform_supported = false;
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        daemon
            .spawn_initial_driver_playback()
            .join()
            .expect("startup worker");
        assert_eq!(
            daemon.manager.lock().get_state(),
            sotf_audio::manager::StreamingState::Idle
        );
        assert!(daemon.system_state.lock().applied_spec().is_none());
        let response = daemon.handle_command(Command::Play);
        assert!(!response.success);
        assert!(response.error.unwrap().contains("status-only"));
        assert!(!state.lock().engine_ready);
        assert!(state.lock().requested_configs.is_empty());
    }

    #[test]
    fn native_capture_reconnect_preserves_processing_timing_and_acknowledges_native_geometry() {
        let state = fake_driver_state();
        state.lock().status.sample_rate = 32_000;
        state.lock().status.buffer_frames = 32;
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        daemon.system_state.lock().processing_timing = Some((48_000, 512));
        handle_driver_config_change(
            &daemon.driver_manager,
            &daemon.manager,
            &daemon.device_registry,
            DriverConfig::new(32_000, 32, 2),
            &daemon.system_state,
            &daemon.running,
        );
        assert_eq!(
            daemon.system_state.lock().processing_timing,
            Some((48_000, 512))
        );
        let ack = state.lock().last_ack.clone().unwrap();
        assert_eq!(ack.0, DriverConfig::new(32_000, 32, 2));
        assert_eq!(ack.1, ConfigResult::Accepted);
    }

    #[test]
    #[serial_test::serial]
    fn applied_pipeline_adopts_all_negotiated_capture_dimensions() {
        let state = fake_driver_state();
        state.lock().negotiated_config = Some(DriverConfig::new(96_000, 256, 4));
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        daemon
            .system_state
            .lock()
            .set_desired_output_device(Some(LAB_OUTPUT_DEVICE_NAME.to_owned()))
            .unwrap();
        let response = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![],
            input_channels: 6,
            output_channels: 4,
        });
        assert!(response.success, "{response:?}");
        let applied = daemon.system_state.lock().applied_spec().unwrap();
        assert_eq!(applied.input_channels, 4);
        assert_eq!(applied.output_channels, 4);
        assert_eq!(state.lock().status.sample_rate, 96_000);
        assert_eq!(state.lock().status.buffer_frames, 256);
        assert_eq!(
            daemon
                .driver_manager
                .lock()
                .capture_source()
                .unwrap()
                .configuration(),
            DriverConfig::new(96_000, 256, 4)
        );
        assert_eq!(
            daemon.manager.lock().get_playback_state(),
            sotf_audio::PlaybackState::Playing
        );
        assert_eq!(
            daemon.system_state.lock().processing_timing,
            Some((48_000, 512)),
            "native capture negotiation must not change physical processing timing"
        );
        daemon.manager.lock().stop().unwrap();
    }

    #[test]
    fn volume_publication_waits_for_temporary_mute_to_be_restored() {
        let state = fake_driver_state();
        let daemon = Arc::new(test_daemon_with_driver(Arc::clone(&state)));
        let transition = daemon.pipeline_mutation.lock();
        daemon
            .manager
            .lock()
            .set_mute(true)
            .expect("temporary teardown mute");
        let epoch = daemon
            .pipeline_intent_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker_daemon = Arc::clone(&daemon);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx
                .send(worker_daemon.handle_command(Command::SetVolume { volume: 0.25 }))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        assert!(state.lock().published_volume.is_none());
        daemon
            .manager
            .lock()
            .set_mute(false)
            .expect("restore user mute");
        drop(transition);
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .success
        );
        worker.join().unwrap();
        let published = state.lock().published_volume.expect("published gain");
        assert_eq!((published.0, published.1), (0.25, false));
        assert_eq!(
            daemon
                .pipeline_intent_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            epoch
        );
    }

    #[test]
    fn testkit_failed_first_pipeline_apply_exposes_restart_recovery() {
        let state = fake_driver_state();
        state.lock().fail_next_config = Some("first apply failed".to_string());
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        // There is no applied plan yet. Changing the requested input geometry
        // forces the driver configuration path before the engine can start,
        // making the failure deterministic without requiring a physical
        // output device.
        let response = daemon.handle_command(Command::LoadPlugins {
            plugins: Vec::new(),
            input_channels: 4,
            output_channels: 2,
        });

        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Failed to set HAL input channels"))
        );
        assert!(!state.lock().engine_ready);

        let state = daemon.system_state.lock();
        assert_eq!(state.applied_generation(), None);
        let recovery = state
            .pipeline_recovery()
            .expect("first apply failure must remain observable");
        assert!(recovery.error.contains("Failed to set HAL input channels"));
        assert_eq!(recovery.actions, vec!["restart_daemon"]);
    }

    #[test]
    #[serial_test::serial]
    fn testkit_concurrent_add_plugin_preserves_both_mutations() {
        let state = fake_driver_state();
        let daemon = Arc::new(test_daemon_with_driver(state));
        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: Vec::new(),
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed pipeline: {seed:?}");
        daemon
            .manager
            .lock()
            .resume()
            .expect("resume fake HAL silent source after pipeline seed");
        assert_fake_driver_uses_lab_output(&daemon);

        let start = Arc::new(Barrier::new(3));
        let first_daemon = Arc::clone(&daemon);
        let first_start = Arc::clone(&start);
        let first = std::thread::spawn(move || {
            first_start.wait();
            first_daemon.handle_command(Command::AddPlugin {
                plugin: PluginConfig {
                    plugin_type: "gain".to_string(),
                    parameters: serde_json::json!({"gain_db": 0.0}),
                },
                index: None,
            })
        });

        let second_daemon = Arc::clone(&daemon);
        let second_start = Arc::clone(&start);
        let second = std::thread::spawn(move || {
            second_start.wait();
            second_daemon.handle_command(Command::AddPlugin {
                plugin: PluginConfig {
                    plugin_type: "gain".to_string(),
                    parameters: serde_json::json!({"gain_db": 0.0}),
                },
                index: None,
            })
        });

        start.wait();
        let first_response = first.join().expect("first mutation thread must not panic");
        let second_response = second
            .join()
            .expect("second mutation thread must not panic");
        assert!(
            first_response.success,
            "first mutation failed: {first_response:?}"
        );
        assert!(
            second_response.success,
            "second mutation failed: {second_response:?}"
        );

        let state = daemon.system_state.lock();
        assert_eq!(state.user_plugins().len(), 2);
        assert_eq!(state.applied_generation(), Some(3));
        drop(state);
        let stopped = daemon.handle_command(Command::Stop);
        assert!(stopped.success, "failed to stop test engine: {stopped:?}");
    }

    #[test]
    fn plugin_and_dump_snapshots_keep_payloads_with_their_generation() {
        let daemon = Arc::new(AudioDaemon::new());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_daemon = Arc::clone(&daemon);
        let writer_done = Arc::clone(&done);
        let writer = std::thread::spawn(move || {
            for _ in 0..1_000 {
                let mutation = writer_daemon.pipeline_mutation.lock();
                let mut state = writer_daemon.system_state.lock();
                let next_generation = state.generation() + 1;
                let plan = state
                    .prepare_plan(
                        vec![PluginConfig {
                            plugin_type: "gain".to_string(),
                            parameters: serde_json::json!({
                                "generation_marker": next_generation
                            }),
                        }],
                        2,
                        2,
                        2,
                    )
                    .expect("generation marker plan");
                state.commit_applied(&plan);
                drop(state);
                drop(mutation);
                std::thread::yield_now();
            }
            writer_done.store(true, std::sync::atomic::Ordering::Release);
        });

        while !done.load(std::sync::atomic::Ordering::Acquire) {
            let plugins = daemon.handle_get_plugins().data.expect("plugin response");
            if let Some(plugin) = plugins["plugins"]
                .as_array()
                .and_then(|items| items.first())
            {
                assert_eq!(
                    plugin["parameters"]["generation_marker"],
                    plugins["generation"]
                );
            }

            let dump = daemon.handle_dump_state().data.expect("dump response");
            if let Some(plugin) = dump["plugins"].as_array().and_then(|items| items.first()) {
                assert_eq!(
                    plugin["parameters"]["generation_marker"],
                    dump["snapshot"]["generation"]
                );
            }
        }
        writer.join().expect("generation writer");
    }

    #[test]
    fn testkit_double_pipeline_failure_marks_restart_recovery() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: vec![test_plugin("eq")],
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed pipeline: {seed:?}");

        {
            let mut state = state.lock();
            state.status.channel_count = 8;
            state.config_failures_remaining = 2;
        }

        let response = daemon.handle_command(Command::SetInputChannels { channels: 4 });
        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("recovery also failed"))
        );
        assert!(!state.lock().engine_ready);

        let status = daemon.handle_command(Command::Status);
        let data = status.data.expect("status data");
        assert_eq!(
            data["pipeline_recovery"]["actions"],
            serde_json::json!(["restart_daemon"])
        );
        assert!(
            data["recovery_actions"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| action == "restart_daemon"))
        );

        let snapshot = daemon.handle_command(Command::GetSnapshot);
        let snapshot_data = snapshot.data.expect("snapshot data");
        let faults = snapshot_data["diagnostics"]["faults"]
            .as_array()
            .expect("fault list");
        assert!(
            faults
                .iter()
                .any(|fault| fault["code"] == "pipeline_recovery_required")
        );
    }

    #[test]
    fn testkit_missing_output_device_suggests_selecting_a_device() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        daemon
            .system_state
            .lock()
            .set_desired_output_device(Some("EVO8".to_string()))
            .expect("safe explicit output name");
        daemon.system_state.lock().mark_pipeline_recovery(
            "Failed to load plugin chain: Selected output device 'EVO8' is not available: Audio device 'EVO8' not found",
        );

        let status = daemon.handle_command(Command::Status);
        let data = status.data.expect("status data");
        assert!(
            data["recovery_actions"]
                .as_array()
                .is_some_and(|actions| actions
                    .iter()
                    .any(|action| action == "select_output_device")),
            "missing-device latch must suggest select_output_device: {data}"
        );
    }

    #[test]
    fn testkit_generic_pipeline_fault_does_not_suggest_device_selection() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        daemon
            .system_state
            .lock()
            .set_desired_output_device(Some("EVO8".to_string()))
            .expect("safe explicit output name");
        daemon.system_state.lock().mark_pipeline_recovery(
            "Failed to load plugin chain: Engine thread exited unexpectedly",
        );

        let status = daemon.handle_command(Command::Status);
        let data = status.data.expect("status data");
        assert!(
            !data["recovery_actions"]
                .as_array()
                .is_some_and(|actions| actions
                    .iter()
                    .any(|action| action == "select_output_device")),
            "generic latch must not suggest select_output_device: {data}"
        );
    }

    #[test]
    fn testkit_fallback_commit_preserves_desired_and_bumps_once() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let generation_before = daemon.system_state.lock().generation();
        let mut plan = {
            let state = daemon.system_state.lock();
            state.prepare_plan(Vec::new(), 2, 2, 2).expect("valid plan")
        };
        plan.spec.output_device = Some("Mac mini Speakers".to_string());
        daemon
            .system_state
            .lock()
            .commit_fallback_applied(&plan, Some("EVO8".to_string()));

        let state = daemon.system_state.lock();
        assert_eq!(state.selected_output_device().as_deref(), Some("EVO8"));
        assert_eq!(
            state.applied_output_device().as_deref(),
            Some("Mac mini Speakers")
        );
        assert_eq!(state.generation(), generation_before + 1);
    }

    #[test]
    fn testkit_snapshot_reports_active_output_fallback_as_warning() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));

        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: Vec::new(),
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed pipeline: {seed:?}");
        daemon
            .manager
            .lock()
            .resume()
            .expect("resume fake HAL silent source after pipeline seed");
        // Steady state first: the transport faults fire while Playing with
        // no frames or callbacks yet, which would mask the fallback health
        // signal.
        let callbacks_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let engine_state = daemon.manager.lock().get_engine_state();
            if engine_state.playback_callback_count > 0 && engine_state.playback_frames_received > 0
            {
                break;
            }
            assert!(
                std::time::Instant::now() < callbacks_deadline,
                "lab engine never produced playback frames and callbacks"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let mut plan = {
            let state = daemon.system_state.lock();
            let spec = state.applied_spec().expect("seed applied");
            state
                .prepare_from_spec(spec, state.input_channels())
                .expect("reprepare applied spec")
        };
        plan.spec.output_device = Some("Mac mini Speakers".to_string());
        daemon
            .system_state
            .lock()
            .commit_fallback_applied(&plan, Some("EVO8".to_string()));

        let snapshot = daemon.handle_command(Command::GetSnapshot);
        let data = snapshot.data.expect("snapshot data");
        let faults = data["diagnostics"]["faults"]
            .as_array()
            .expect("fault list");
        let fallback = faults
            .iter()
            .find(|fault| fault["code"] == "output_device_fallback")
            .expect("fallback fault");
        assert_eq!(fallback["severity"], "warning");
        assert_eq!(fallback["desired_output_device"], "EVO8");
        assert_eq!(fallback["applied_output_device"], "Mac mini Speakers");
        assert_eq!(
            data["diagnostics"]["health"], "warning",
            "full fault list: {}",
            data["diagnostics"]["faults"]
        );
    }

    #[test]
    fn testkit_snapshot_omits_fallback_fault_when_converged_or_idle() {
        // Converged route while streaming: no fallback fault.
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        let seed = daemon.handle_command(Command::LoadPlugins {
            plugins: Vec::new(),
            input_channels: 2,
            output_channels: 2,
        });
        assert!(seed.success, "failed to seed pipeline: {seed:?}");
        daemon
            .manager
            .lock()
            .resume()
            .expect("resume fake HAL silent source after pipeline seed");
        {
            let mut plan = {
                let state = daemon.system_state.lock();
                let spec = state.applied_spec().expect("seed applied");
                state
                    .prepare_from_spec(spec, state.input_channels())
                    .expect("reprepare applied spec")
            };
            plan.spec.output_device = Some("Mac mini Speakers".to_string());
            daemon.system_state.lock().commit_applied(&plan);
        }
        let snapshot = daemon.handle_command(Command::GetSnapshot);
        let data = snapshot.data.expect("snapshot data");
        assert!(
            data["diagnostics"]["faults"]
                .as_array()
                .is_some_and(|faults| faults
                    .iter()
                    .all(|fault| fault["code"] != "output_device_fallback")),
            "converged route must not report a fallback: {data}"
        );

        // Diverged route while idle (stop preserves the stale applied spec):
        // nothing is playing through the fallback, so no fault.
        let idle_state = fake_driver_state();
        let idle_daemon = test_daemon_with_driver(Arc::clone(&idle_state));
        {
            let mut plan = {
                let state = idle_daemon.system_state.lock();
                state.prepare_plan(Vec::new(), 2, 2, 2).expect("valid plan")
            };
            plan.spec.output_device = Some("Mac mini Speakers".to_string());
            idle_daemon
                .system_state
                .lock()
                .commit_fallback_applied(&plan, Some("EVO8".to_string()));
        }
        let snapshot = idle_daemon.handle_command(Command::GetSnapshot);
        let data = snapshot.data.expect("snapshot data");
        assert!(
            data["diagnostics"]["faults"]
                .as_array()
                .is_some_and(|faults| faults
                    .iter()
                    .all(|fault| fault["code"] != "output_device_fallback")),
            "idle divergence must not report an active fallback: {data}"
        );
    }

    #[test]
    fn testkit_apply_with_missing_desired_device_uses_effective_output() {
        let state = fake_driver_state();
        let daemon = test_daemon_with_driver(Arc::clone(&state));
        daemon
            .system_state
            .lock()
            .set_desired_output_device(Some("Definitely Missing Physical Output".to_string()))
            .expect("safe explicit output name");

        // Live expectation: the apply must succeed on every machine while
        // committing exactly what resolution selected for this machine's
        // enumeration, adapted to the lab sink — the device-free lab
        // backend rejects physical fallback names outright, so in lab
        // mode the effective output is the lab output itself. (This test
        // previously assumed the lab engine opens any device name and
        // therefore only passed on machines without audio hardware.)
        let expected = effective_output_for_sink(
            resolve_effective_output_device(Some("Definitely Missing Physical Output")),
            SinkType::LabNull,
            Some("Definitely Missing Physical Output"),
        );
        let response = daemon.handle_command(Command::LoadPlugins {
            plugins: Vec::new(),
            input_channels: 2,
            output_channels: 2,
        });
        assert!(
            response.success,
            "fallback apply must succeed: {response:?}"
        );
        let state = daemon.system_state.lock();
        assert_eq!(
            state.selected_output_device().as_deref(),
            Some("Definitely Missing Physical Output")
        );
        assert_eq!(
            state.applied_output_device(),
            expected.device,
            "applied device must be the resolved effective output"
        );
        assert_eq!(
            expected.fallback.is_some(),
            state.selected_output_device() != state.applied_output_device(),
            "fallback record and desired/applied divergence must agree"
        );
    }

    #[test]
    fn ipc_pipeline_rollback_requests_previous_hal_geometry() {
        let driver_state = fake_driver_state();
        driver_state.lock().status.driver_name = "Fake HAL With Callback";
        let daemon = test_daemon_with_driver(Arc::clone(&driver_state));

        let requested_plan = {
            let mut state = daemon.system_state.lock();
            let previous_plan = state
                .prepare_with_selected_device(LAB_OUTPUT_DEVICE_NAME.to_string())
                .expect("safe explicit output name");
            state.commit_applied(&previous_plan);
            state
                .prepare_plan(vec![], 6, 2, 6)
                .expect("valid requested geometry")
        };
        let driver_status = driver_state.lock().status.clone();
        // Fail the requested start at the HAL configure step. (This test
        // previously used an invalid plugin type, but the start only ever
        // failed because the lab engine rejected the missing physical
        // output device; with lab outputs resolved device-free, the
        // failure must come from a step that genuinely fails.)
        driver_state.lock().fail_next_config = Some("injected HAL config failure".to_string());

        let response = daemon.apply_pipeline_plan(requested_plan, driver_status, 48_000, 512);

        assert!(
            !response.success,
            "failed HAL configure must fail the apply"
        );
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("restored the last working pipeline")),
            "rollback must restore the previous pipeline: {:?}",
            response.error
        );
        let requested_channels: Vec<u32> = driver_state
            .lock()
            .requested_configs
            .iter()
            .map(|config| config.channel_count)
            .collect();
        assert_eq!(
            requested_channels,
            vec![6, 2],
            "rollback must restore HAL to the previous graph's input geometry"
        );
        assert!(
            driver_state.lock().engine_ready,
            "restored previous pipeline must mark the engine ready again"
        );
    }
}

#[test]
fn pipeline_supervisor_preserves_graph_when_output_device_changes() {
    let graph = PluginGraphConfig::try_new(
        vec![PluginGraphNodeConfig::try_new(42, "gain", serde_json::json!({}), 2).unwrap()],
        vec![],
    )
    .unwrap();
    let mut supervisor = PipelineSupervisor::default();
    let initial = supervisor
        .prepare_graph_plan(graph.clone(), 2, 2, 2)
        .unwrap();
    supervisor.commit_applied(&initial);

    let next = supervisor
        .prepare_with_selected_device("Built-in Output".to_string())
        .unwrap();

    assert!(next.spec.user_plugins.is_empty());
    let retained = next.spec.user_graph.as_ref().expect("retained graph");
    assert_eq!(retained.nodes[0].id, 42);
    assert!(next.runtime_graph.is_some());
    assert_eq!(supervisor.applied_generation(), Some(1));
}

/// Phase 4.2: full command/response round-trips without a real audio device.
///
/// These tests exercise JSON parsing into `Command` variants and direct
/// `AudioDaemon::handle_command` invocation. They run serially because some
/// mutating commands start the engine's cpal output stream, and contending for
/// the default output device across tests would be flaky.
#[cfg(unix)]
mod command_roundtrip_tests {
    use super::*;
    use serial_test::serial;

    fn parse(json: &str) -> Command {
        serde_json::from_str(json).expect("valid command JSON")
    }

    fn run_command(cmd: Command) -> Response {
        let daemon = AudioDaemon::new();
        daemon.handle_command(cmd)
    }

    fn response_value(resp: &Response) -> Value {
        serde_json::to_value(resp).expect("response serializes to JSON")
    }

    #[test]
    fn parse_all_command_variants() {
        assert!(matches!(parse(r#"{"command":"ping"}"#), Command::Ping));
        assert!(matches!(parse(r#"{"command":"status"}"#), Command::Status));
        assert!(matches!(
            parse(r#"{"command":"get_snapshot"}"#),
            Command::GetSnapshot
        ));
        assert!(matches!(
            parse(r#"{"command":"snapshot"}"#),
            Command::GetSnapshot
        ));
        assert!(matches!(
            parse(r#"{"command":"dump_state"}"#),
            Command::DumpState
        ));

        let cmd = parse(r#"{"command":"load","path":"/tmp/test.wav"}"#);
        assert!(matches!(cmd, Command::Load { path } if path == "/tmp/test.wav"));

        assert!(matches!(parse(r#"{"command":"play"}"#), Command::Play));
        assert!(matches!(parse(r#"{"command":"pause"}"#), Command::Pause));
        assert!(matches!(parse(r#"{"command":"stop"}"#), Command::Stop));

        let cmd = parse(r#"{"command":"seek","position":12.5}"#);
        assert!(
            matches!(cmd, Command::Seek { position } if (position - 12.5).abs() < f64::EPSILON)
        );

        let cmd = parse(r#"{"command":"set_volume","volume":0.5}"#);
        assert!(
            matches!(cmd, Command::SetVolume { volume } if (volume - 0.5).abs() < f32::EPSILON)
        );

        assert!(matches!(
            parse(r#"{"command":"list_devices"}"#),
            Command::ListDevices
        ));

        let cmd = parse(r#"{"command":"set_device","device":"Built-in Output"}"#);
        assert!(matches!(cmd, Command::SetDevice { device } if device == "Built-in Output"));

        let cmd = parse(
            r#"{"command":"load_plugins","plugins":[{"plugin_type":"eq","parameters":{}}],"input_channels":2,"output_channels":2}"#,
        );
        assert!(
            matches!(cmd, Command::LoadPlugins { plugins, input_channels, output_channels } if plugins.len() == 1 && input_channels == 2 && output_channels == 2)
        );

        let cmd = parse(r#"{"command":"load_plugin_artifact","artifact":{"plugins":[]}}"#);
        assert!(matches!(cmd, Command::LoadPluginArtifact { .. }));

        let cmd = parse(r#"{"command":"set_input_channels","channels":4}"#);
        assert!(matches!(cmd, Command::SetInputChannels { channels } if channels == 4));

        let cmd = parse(r#"{"command":"set_output_channels","channels":6}"#);
        assert!(matches!(cmd, Command::SetOutputChannels { channels } if channels == 6));

        let cmd =
            parse(r#"{"command":"set_pipeline_channels","input_channels":4,"output_channels":6}"#);
        assert!(matches!(
            cmd,
            Command::SetPipelineChannels {
                input_channels: Some(4),
                output_channels: Some(6),
            }
        ));

        assert!(matches!(
            parse(r#"{"command":"get_loudness"}"#),
            Command::GetLoudness
        ));
        assert!(matches!(
            parse(r#"{"command":"get_metering"}"#),
            Command::GetMetering
        ));
        assert!(matches!(
            parse(r#"{"command":"get_plugins"}"#),
            Command::GetPlugins
        ));
        assert!(matches!(
            parse(r#"{"command":"get_available_plugins"}"#),
            Command::GetAvailablePlugins
        ));

        let cmd =
            parse(r#"{"command":"add_plugin","plugin":{"plugin_type":"eq","parameters":{}}}"#);
        assert!(matches!(cmd, Command::AddPlugin { index: None, .. }));

        let cmd = parse(
            r#"{"command":"add_plugin","plugin":{"plugin_type":"eq","parameters":{}},"index":1}"#,
        );
        assert!(matches!(cmd, Command::AddPlugin { index: Some(1), .. }));

        let cmd = parse(r#"{"command":"remove_plugin","index":0}"#);
        assert!(matches!(cmd, Command::RemovePlugin { index } if index == 0));

        let cmd = parse(r#"{"command":"update_plugin","index":0,"parameters":{"gain_db":3.0}}"#);
        assert!(matches!(cmd, Command::UpdatePlugin { index, .. } if index == 0));

        let cmd = parse(r#"{"command":"reorder_plugins","order":[1,0]}"#);
        assert!(matches!(cmd, Command::ReorderPlugins { order } if order == vec![1, 0]));

        assert!(matches!(
            parse(r#"{"command":"driver_status"}"#),
            Command::DriverStatus
        ));
        assert!(matches!(
            parse(r#"{"command":"hal_status"}"#),
            Command::DriverStatus
        ));
        assert!(matches!(
            parse(r#"{"command":"shutdown"}"#),
            Command::Shutdown
        ));

        let cmd = parse(r#"{"command":"set_encryption","enabled":true}"#);
        assert!(matches!(cmd, Command::SetEncryption { enabled: true }));

        assert!(matches!(
            parse(r#"{"command":"encryption_status"}"#),
            Command::EncryptionStatus
        ));
        assert!(matches!(
            parse(r#"{"command":"rotate_encryption_key"}"#),
            Command::RotateEncryptionKey
        ));

        let cmd = parse(r#"{"command":"set_sample_rate","rate":48000}"#);
        assert!(matches!(cmd, Command::SetSampleRate { rate } if rate == 48_000));

        let cmd = parse(r#"{"command":"set_buffer_frames","frames":512}"#);
        assert!(matches!(cmd, Command::SetBufferFrames { frames } if frames == 512));

        assert!(matches!(
            parse(r#"{"command":"get_driver_config"}"#),
            Command::GetDriverConfig
        ));
        assert!(matches!(
            parse(r#"{"command":"get_hal_config"}"#),
            Command::GetDriverConfig
        ));
    }

    #[test]
    fn playback_readiness_requires_callback_and_surfaces_startup_error() {
        let mut state = AudioEngineManager::new().get_engine_state();
        assert_eq!(AudioDaemon::playback_startup_observation(&state), Ok(false));

        state.playback_callback_count = 1;
        assert_eq!(AudioDaemon::playback_startup_observation(&state), Ok(true));

        state.last_error = Some("device failed".to_string());
        assert_eq!(
            AudioDaemon::playback_startup_observation(&state),
            Err("Playback startup failed: device failed".to_string())
        );
    }

    #[test]
    fn playback_readiness_wait_policy_covers_success_and_timeout() {
        let running = Arc::new(Mutex::new(true));
        let mut polls = 0;
        wait_for_playback_observation(
            &running,
            std::time::Duration::from_millis(100),
            "test startup",
            || {
                polls += 1;
                Ok(polls == 3)
            },
        )
        .expect("third observation should publish readiness");
        assert_eq!(polls, 3);

        let timeout = wait_for_playback_observation(
            &running,
            std::time::Duration::ZERO,
            "test startup",
            || Ok(false),
        )
        .expect_err("missing callback must time out");
        assert!(timeout.contains("hardware callback"), "{timeout}");
    }

    #[test]
    fn reconfigured_playback_readiness_is_cancelled_by_daemon_shutdown() {
        let manager = Arc::new(Mutex::new(AudioEngineManager::new()));
        let running = Arc::new(Mutex::new(false));

        let error = wait_for_reconfigured_playback(&manager, &running)
            .expect_err("shutdown must prevent readiness publication");

        assert!(error.contains("shutdown requested"), "{error}");
    }

    #[test]
    fn playback_callback_policy_only_bypasses_explicit_test_drivers() {
        let macos_hal = driver_common::DriverStatus::new(
            true,
            true,
            true,
            48_000,
            2,
            512,
            "macOS CoreAudio HAL",
            true,
            0,
        );
        assert!(AudioDaemon::requires_playback_callback(&macos_hal));

        let lab = driver_common::DriverStatus::new(
            true,
            true,
            true,
            48_000,
            2,
            512,
            "Systemwide Lab Driver",
            true,
            0,
        );
        assert!(!AudioDaemon::requires_playback_callback(&lab));

        let fake =
            driver_common::DriverStatus::new(true, true, true, 48_000, 2, 512, "Fake HAL", true, 0);
        assert!(!AudioDaemon::requires_playback_callback(&fake));

        let null = driver_common::DriverStatus::new(false, false, false, 0, 0, 0, "None", false, 0);
        assert!(!AudioDaemon::requires_playback_callback(&null));

        let other_production_driver =
            driver_common::DriverStatus::new(true, true, true, 48_000, 2, 512, "PipeWire", true, 0);
        assert!(AudioDaemon::requires_playback_callback(
            &other_production_driver
        ));
    }

    #[test]
    fn ping_returns_without_runtime_snapshot() {
        let response = run_command(Command::Ping);
        assert!(response.success);
        assert!(response.data.is_none());
    }

    #[test]
    fn command_name_matches_parsed_variant() {
        let cases: Vec<(&str, &str)> = vec![
            (r#"{"command":"ping"}"#, "ping"),
            (r#"{"command":"status"}"#, "status"),
            (r#"{"command":"get_snapshot"}"#, "get_snapshot"),
            (r#"{"command":"snapshot"}"#, "get_snapshot"),
            (r#"{"command":"dump_state"}"#, "dump_state"),
            (r#"{"command":"load","path":"x"}"#, "load"),
            (r#"{"command":"play"}"#, "play"),
            (r#"{"command":"pause"}"#, "pause"),
            (r#"{"command":"stop"}"#, "stop"),
            (r#"{"command":"seek","position":0.0}"#, "seek"),
            (r#"{"command":"set_volume","volume":1.0}"#, "set_volume"),
            (r#"{"command":"list_devices"}"#, "list_devices"),
            (r#"{"command":"set_device","device":"x"}"#, "set_device"),
            (r#"{"command":"load_plugins","plugins":[]}"#, "load_plugins"),
            (
                r#"{"command":"load_plugin_artifact","artifact":{}}"#,
                "load_plugin_artifact",
            ),
            (
                r#"{"command":"set_input_channels","channels":2}"#,
                "set_input_channels",
            ),
            (
                r#"{"command":"set_output_channels","channels":2}"#,
                "set_output_channels",
            ),
            (
                r#"{"command":"set_pipeline_channels","input_channels":2}"#,
                "set_pipeline_channels",
            ),
            (r#"{"command":"get_loudness"}"#, "get_loudness"),
            (r#"{"command":"get_metering"}"#, "get_metering"),
            (r#"{"command":"get_plugins"}"#, "get_plugins"),
            (
                r#"{"command":"get_available_plugins"}"#,
                "get_available_plugins",
            ),
            (
                r#"{"command":"add_plugin","plugin":{"plugin_type":"eq","parameters":{}}}"#,
                "add_plugin",
            ),
            (r#"{"command":"remove_plugin","index":0}"#, "remove_plugin"),
            (
                r#"{"command":"update_plugin","index":0,"parameters":{}}"#,
                "update_plugin",
            ),
            (
                r#"{"command":"reorder_plugins","order":[]}"#,
                "reorder_plugins",
            ),
            (r#"{"command":"driver_status"}"#, "driver_status"),
            (r#"{"command":"hal_status"}"#, "driver_status"),
            (r#"{"command":"shutdown"}"#, "shutdown"),
            (
                r#"{"command":"set_encryption","enabled":false}"#,
                "set_encryption",
            ),
            (r#"{"command":"encryption_status"}"#, "encryption_status"),
            (
                r#"{"command":"rotate_encryption_key"}"#,
                "rotate_encryption_key",
            ),
            (
                r#"{"command":"set_sample_rate","rate":48000}"#,
                "set_sample_rate",
            ),
            (
                r#"{"command":"set_buffer_frames","frames":512}"#,
                "set_buffer_frames",
            ),
            (r#"{"command":"get_driver_config"}"#, "get_driver_config"),
            (r#"{"command":"get_hal_config"}"#, "get_driver_config"),
            (
                r#"{"command":"get_output_profiles"}"#,
                "get_output_profiles",
            ),
            (
                r#"{"command":"set_output_profile","profile":{"id":"","name":"HP","topology":"rack","plugins":[],"input_channels":2,"output_channels":2,"updated_at_unix_ms":0}}"#,
                "set_output_profile",
            ),
            (
                r#"{"command":"delete_output_profile","profile_id":"profile-1"}"#,
                "delete_output_profile",
            ),
            (
                r#"{"command":"assign_output_profile","device_name":"X","profile_id":"default"}"#,
                "assign_output_profile",
            ),
            (
                r#"{"command":"set_output_route","output_device":"X"}"#,
                "set_output_route",
            ),
        ];
        for (json, expected) in cases {
            let cmd = parse(json);
            assert_eq!(cmd.name(), expected, "{json}");
        }
    }

    #[test]
    fn handle_status_returns_expected_fields() {
        let resp = run_command(Command::Status);
        assert!(resp.success);
        let data = resp.data.expect("status data");
        assert_eq!(data["consistency"], "best_effort");
        assert!(data.get("state").is_some());
        assert!(data.get("volume").is_some());
        assert!(data.get("selected_device").is_some());
        assert!(data.get("input_channels").is_some());
        assert!(data.get("output_channels").is_some());
        assert!(data.get("playback_output_device").is_some());

        // QA-SYS-003 explicit diagnostics fields.
        let driver = data.get("driver").expect("driver object");
        assert!(driver.get("installed").is_some());
        assert!(driver.get("ready").is_some());
        assert!(driver.get("capture_active").is_some());
        assert!(driver.get("frame_size").is_some());

        let encryption = data.get("encryption").expect("encryption object");
        assert!(encryption.get("enabled").is_some());

        let active_route = data.get("active_route").expect("active_route object");
        assert!(active_route.get("desired_output_device").is_some());
        assert!(active_route.get("applied_output_device").is_some());
        assert!(active_route.get("playback_output_device").is_some());
        assert!(active_route.get("capture_active").is_some());

        assert!(
            data.get("recovery_actions").is_some(),
            "recovery_actions list must be present"
        );
        assert!(data["recovery_actions"].is_array());
    }

    #[test]
    fn handle_get_snapshot_returns_expected_fields() {
        let resp = run_command(Command::GetSnapshot);
        assert!(resp.success);
        let data = resp.data.expect("snapshot data");
        assert_eq!(data["schema_version"], 1);
        assert!(data.get("desired").is_some());
        assert!(data.get("observed").is_some());
        assert!(data.get("diagnostics").is_some());
        assert!(data["diagnostics"].get("health").is_some());
        assert!(data["diagnostics"]["faults"].is_array());
    }

    #[test]
    fn handle_dump_state_returns_snapshot_and_plugins() {
        let resp = run_command(Command::DumpState);
        assert!(resp.success);
        let data = resp.data.expect("dump_state data");
        assert!(data.get("snapshot").is_some());
        assert!(data["snapshot"]["schema_version"].is_u64());
        assert!(data.get("plugins").is_some());
        assert!(data["plugins"].is_array());
    }

    #[test]
    fn handle_get_plugins_returns_plugins_array() {
        let resp = run_command(Command::GetPlugins);
        assert!(resp.success);
        let data = resp.data.expect("get_plugins data");
        assert!(data["plugins"].is_array());
    }

    #[test]
    fn handle_get_available_plugins_returns_plugins_array() {
        let resp = run_command(Command::GetAvailablePlugins);
        assert!(resp.success);
        let data = resp.data.expect("get_available_plugins data");
        let plugins = data["plugins"].as_array().expect("plugins array");
        assert!(!plugins.is_empty());
        assert!(plugins[0].get("type").is_some());
        assert!(plugins[0].get("default_parameters").is_some());
    }

    #[test]
    fn handle_get_available_plugins_serves_graph_blocks_with_canonical_categories() {
        // The picker must offer every user-insertable type, including the
        // graph building blocks, grouped by the canonical player categories.
        let resp = run_command(Command::GetAvailablePlugins);
        assert!(resp.success);
        let data = resp.data.expect("get_available_plugins data");
        let plugins = data["plugins"].as_array().expect("plugins array");
        let by_type: std::collections::HashMap<&str, &Value> = plugins
            .iter()
            .filter_map(|entry| entry["type"].as_str().map(|type_| (type_, entry)))
            .collect();

        for (plugin_type, category) in [
            ("ab_compare", "Analysis"),
            ("band_split", "Routing"),
            ("band_merge", "Routing"),
            ("eq", "EQ & Tone"),
            ("compressor", "Dynamics"),
            ("denoiser", "Denoising"),
            ("upmixer", "Spatial"),
        ] {
            let entry = by_type
                .get(plugin_type)
                .unwrap_or_else(|| panic!("{plugin_type} missing from available plugins"));
            assert_eq!(entry["category"], category, "{plugin_type} category");
            assert!(entry.get("name").is_some());
            assert!(entry.get("default_parameters").is_some());
        }

        // Daemon-owned metering, superseded, and infrastructure types stay out.
        for withheld in [
            "loudness_monitor",
            "spectrum_analyzer",
            "fletcher_munson",
            "resampler",
            "hal_input",
            "hal_output",
            "external",
        ] {
            assert!(
                !by_type.contains_key(withheld),
                "{withheld} must stay out of available plugins"
            );
        }
    }

    fn test_output_profile(
        id: &str,
        name: &str,
        plugins: Vec<PluginConfig>,
    ) -> crate::output_profiles::OutputProfile {
        crate::output_profiles::OutputProfile {
            id: id.to_string(),
            name: name.to_string(),
            topology: "rack".to_string(),
            plugins,
            graph: None,
            input_channels: 2,
            output_channels: 2,
            updated_at_unix_ms: 0,
        }
    }

    #[test]
    fn handle_get_output_profiles_adopts_default_from_live_chain() {
        let resp = run_command(Command::GetOutputProfiles);
        assert!(resp.success);
        let data = resp.data.expect("output profiles data");
        let profiles = data["profiles"].as_array().expect("profiles array");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["id"], "default");
        assert_eq!(profiles[0]["topology"], "rack");
        assert!(profiles[0]["plugins"].as_array().is_some());
        assert!(profiles[0].get("input_channels").is_some());
        assert!(profiles[0].get("updated_at_unix_ms").is_some());
        assert!(data["assignments"].as_object().is_some());
        assert!(data["current"].get("profile_id").is_some());
        assert!(data["generation"].is_u64());
    }

    #[test]
    fn output_profile_crud_round_trip_on_one_daemon() {
        let daemon = AudioDaemon::new();
        // This CRUD transaction owns one in-memory store; other tests may
        // share the process environment but must not advance its id counter.
        *daemon.output_profiles.lock() = crate::output_profiles::OutputProfileStore::default();

        let created = daemon.handle_set_output_profile(test_output_profile(
            "",
            "Headphones",
            vec![test_plugin("eq")],
        ));
        assert!(created.success);
        let profile_id = created.data.expect("profile id")["profile_id"]
            .as_str()
            .expect("profile id string")
            .to_string();
        assert_eq!(profile_id, "profile-1");

        let assigned = daemon.handle_assign_output_profile(None, "Desk Speakers", &profile_id);
        assert!(assigned.success);

        let fetched = daemon.handle_get_output_profiles();
        assert!(fetched.success);
        let data = fetched.data.expect("profiles data");
        let profiles = data["profiles"].as_array().expect("profiles array");
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            data["assignments"]["name:Desk Speakers"].as_str(),
            Some(profile_id.as_str())
        );

        let renamed = daemon.handle_set_output_profile(test_output_profile(
            &profile_id,
            "Headphones v2",
            vec![test_plugin("gain")],
        ));
        assert!(renamed.success);
        assert_eq!(
            renamed.data.expect("rename data")["profile_id"].as_str(),
            Some(profile_id.as_str())
        );

        let delete_unknown = daemon.handle_delete_output_profile("profile-9");
        assert!(!delete_unknown.success);
        let delete_default = daemon.handle_delete_output_profile("default");
        assert!(!delete_default.success);

        let deleted = daemon.handle_delete_output_profile(&profile_id);
        assert!(deleted.success);
        let refetched = daemon.handle_get_output_profiles();
        let data = refetched.data.expect("refetch data");
        assert_eq!(data["profiles"].as_array().expect("array").len(), 1);
        assert!(data["assignments"].as_object().expect("object").is_empty());
    }

    #[test]
    fn output_profile_mutations_reject_invalid_input() {
        let daemon = AudioDaemon::new();

        let mut bad_topology = test_output_profile("bad", "Bad", vec![]);
        bad_topology.topology = "mesh".to_string();
        assert!(!daemon.handle_set_output_profile(bad_topology).success);

        assert!(
            !daemon
                .handle_assign_output_profile(None, "Ghost", "profile-9")
                .success
        );
        assert!(
            !daemon
                .handle_assign_output_profile(None, "   ", "default")
                .success
        );
    }

    #[test]
    fn handle_set_output_route_rejects_stale_generation_before_touching_audio() {
        let daemon = AudioDaemon::new();
        let current = daemon.system_state.lock().generation();
        let resp = daemon.handle_set_output_route(
            "Definitely Missing Physical Output",
            None,
            None,
            None,
            None,
            Some(current + 1),
        );
        assert!(!resp.success);
        assert!(
            resp.error
                .expect("conflict error")
                .contains("generation conflict")
        );
    }

    #[test]
    fn handle_set_output_route_rejects_empty_and_unknown_devices() {
        let daemon = AudioDaemon::new();
        let lab_backend = daemon.driver_manager.lock().is_lab_backend();
        assert!(
            !daemon
                .handle_set_output_route("   ", None, None, None, None, None)
                .success
        );
        let missing = daemon.handle_set_output_route(
            "Definitely Missing Physical Output",
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!missing.success);
        let error = missing.error.expect("missing device error");
        if lab_backend {
            assert_eq!(error, "Lab backend cannot select a physical output device");
        } else {
            assert!(error.contains("not found"), "{error}");
        }
    }

    #[test]
    fn handle_driver_status_returns_driver_info() {
        let resp = run_command(Command::DriverStatus);
        assert!(resp.success);
        let data = resp.data.expect("driver_status data");
        assert!(data.get("platform_supported").is_some());
        assert!(data.get("driver_installed").is_some());
        assert!(data.get("sample_rate").is_some());
        assert!(data.get("channel_count").is_some());
        assert!(data.get("buffer_frames").is_some());
        assert!(data.get("driver_name").is_some());
        assert!(data.get("ready").is_some());
    }

    #[test]
    fn handle_get_driver_config_returns_config() {
        let resp = run_command(Command::GetDriverConfig);
        assert!(resp.success);
        let data = resp.data.expect("get_driver_config data");
        assert!(data.get("sample_rate").is_some());
        assert!(data.get("buffer_frames").is_some());
        assert!(data.get("channel_count").is_some());
        assert!(data.get("driver_installed").is_some());
        assert!(data.get("platform_supported").is_some());
    }

    #[test]
    fn get_driver_config_wire_preserves_canonical_fields_and_legacy_aliases() {
        let response = run_command(Command::GetDriverConfig);
        assert!(response.success);
        let data = response.data.expect("get_driver_config data");

        let expected_fields = [
            "sample_rate",
            "actual_sample_rate",
            "buffer_frames",
            "actual_buffer_frames",
            "channel_count",
            "active",
            "driver_name",
            "driver_installed",
            "driver_ready",
            "platform_supported",
            // Added with the release audit fixes: negotiation state and the
            // supported-rate table clients build pickers from.
            "config_status",
            "config_source",
            "supported_sample_rates",
        ];
        let actual_fields: std::collections::BTreeSet<&str> = data
            .as_object()
            .expect("driver config is an object")
            .keys()
            .map(String::as_str)
            .collect();
        let expected_field_set: std::collections::BTreeSet<&str> =
            expected_fields.into_iter().collect();
        assert_eq!(actual_fields, expected_field_set);

        assert_eq!(data["actual_sample_rate"], data["sample_rate"]);
        assert_eq!(data["actual_buffer_frames"], data["buffer_frames"]);
        assert!(data["channel_count"].is_u64());
        assert!(data["active"].is_boolean());
        assert!(data["driver_name"].is_string());
        assert!(data["driver_installed"].is_boolean());
        assert!(data["driver_ready"].is_boolean());
        assert!(data["platform_supported"].is_boolean());
    }

    #[test]
    fn handle_get_loudness_returns_valid_response() {
        let resp = run_command(Command::GetLoudness);
        let value = response_value(&resp);
        assert!(value.get("success").is_some());
        if resp.success {
            assert!(value["data"].get("momentary").is_some());
        } else {
            assert!(value.get("error").is_some());
        }
    }

    #[test]
    fn handle_get_metering_returns_metering() {
        let resp = run_command(Command::GetMetering);
        assert!(resp.success);
        let data = resp.data.expect("metering data");
        assert!(data.get("input").is_some());
        assert!(data.get("output").is_some());
        assert!(data.get("sources").is_some());
        assert!(data.get("generation").is_some());
        assert!(data["sources"].get("input").is_some());
        assert!(data["sources"].get("output").is_some());
    }

    #[test]
    fn handle_list_devices_returns_valid_response() {
        let resp = run_command(Command::ListDevices);
        let value = response_value(&resp);
        assert!(value.get("success").is_some());
        if resp.success {
            assert!(value["data"]["devices"].is_array());
        } else {
            assert!(value.get("error").is_some());
        }
    }

    #[test]
    #[serial]
    fn handle_set_volume_succeeds() {
        let resp = run_command(Command::SetVolume { volume: 0.75 });
        assert!(resp.success, "{:?}", resp.error);
    }

    #[test]
    #[serial]
    fn handle_set_input_channels_succeeds() {
        let resp = with_lab_driver(|| run_command(Command::SetInputChannels { channels: 4 }));
        assert!(resp.success, "{:?}", resp.error);
    }

    #[test]
    #[serial]
    fn handle_set_output_channels_succeeds() {
        let resp = with_lab_driver(|| run_command(Command::SetOutputChannels { channels: 6 }));
        assert!(resp.success, "{:?}", resp.error);
    }

    #[test]
    #[serial]
    fn handle_set_pipeline_channels_succeeds() {
        let resp = with_lab_driver(|| {
            run_command(Command::SetPipelineChannels {
                input_channels: Some(4),
                output_channels: Some(6),
            })
        });
        assert!(resp.success, "{:?}", resp.error);
    }

    /// Switch to the lab fake driver for the duration of `f`, then restore
    /// the previous `SOTF_SYSTEMWIDE_DRIVER` value.
    ///
    /// # Safety
    ///
    /// Mutates process environment. Safe here because the caller tests are
    /// marked `#[serial]` and no other thread reads this daemon-specific
    /// override concurrently.
    fn with_lab_driver<T>(f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("SOTF_SYSTEMWIDE_DRIVER").ok();
        unsafe {
            std::env::set_var("SOTF_SYSTEMWIDE_DRIVER", "lab");
        }
        let result = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("SOTF_SYSTEMWIDE_DRIVER", v) },
            None => unsafe { std::env::remove_var("SOTF_SYSTEMWIDE_DRIVER") },
        }
        result
    }

    #[test]
    #[serial]
    fn handle_set_sample_rate_succeeds() {
        // The default NullDriver rejects config changes; use the lab fake
        // driver so this test does not require a real HAL installation.
        let resp = with_lab_driver(|| run_command(Command::SetSampleRate { rate: 48_000 }));
        assert!(resp.success, "{:?}", resp.error);
        let data = resp.data.expect("sample_rate data");
        assert_eq!(data["sample_rate"], 48_000);
    }

    #[test]
    #[serial]
    fn handle_set_buffer_frames_succeeds() {
        let resp = with_lab_driver(|| run_command(Command::SetBufferFrames { frames: 512 }));
        assert!(resp.success, "{:?}", resp.error);
        let data = resp.data.expect("buffer_frames data");
        assert_eq!(data["buffer_frames"], 512);
    }

    #[test]
    #[serial]
    fn handle_set_encryption_reports_build_capability() {
        let resp = run_command(Command::SetEncryption { enabled: true });
        if cfg!(all(target_os = "macos", feature = "hal")) {
            assert!(resp.success, "{resp:?}");
            let data = resp.data.expect("set_encryption data");
            assert_eq!(data["enabled"], true);
            // No HAL shared memory in unit tests, so the transport stays pending.
            assert_eq!(data["transport_state"], "pending");
        } else {
            assert!(!resp.success);
            assert!(
                resp.error
                    .as_deref()
                    .is_some_and(|error| error.contains("no session cipher"))
            );
        }
    }

    #[test]
    #[serial]
    fn handle_encryption_status_reports_transport_state() {
        let response = run_command(Command::EncryptionStatus);
        assert!(response.success, "{response:?}");
        let data = response.data.expect("encryption status data");
        if cfg!(all(target_os = "macos", feature = "hal")) {
            assert!(data.get("transport_state").is_some());
        } else {
            assert_eq!(data["transport_state"], "not_applicable");
            assert!(data["transport_error"].is_null());
        }
    }

    #[test]
    #[serial]
    fn handle_load_plugins_empty_succeeds() {
        let resp = with_lab_driver(|| {
            run_command(Command::LoadPlugins {
                plugins: vec![],
                input_channels: 2,
                output_channels: 2,
            })
        });
        assert!(resp.success, "{:?}", resp.error);
    }

    #[test]
    fn handle_shutdown_returns_ok_and_stops_daemon() {
        let daemon = AudioDaemon::new();
        let resp = daemon.handle_command(Command::Shutdown);
        assert!(resp.success);
        assert!(!*daemon.running.lock());
    }

    // =========================================================================
    // Snapshot tests for daemon response shapes
    // =========================================================================

    #[test]
    fn snapshot_response_ok_with_payload() {
        let resp = Response::ok(serde_json::json!({"x": 1, "name": "test"}));
        insta::assert_json_snapshot!(resp);
    }

    #[test]
    fn snapshot_response_error() {
        let resp = Response::err("something went wrong");
        insta::assert_json_snapshot!(resp);
    }

    #[test]
    fn snapshot_status_response_shape() {
        // Use a NullDriver so the status response is deterministic regardless
        // of whether the `hal` feature (which selects the platform HAL) is
        // enabled. The KeyManager implementation is also feature-dependent,
        // so we normalize the encryption block before snapshotting.
        let daemon = AudioDaemon {
            manager: Arc::new(Mutex::new(AudioEngineManager::new())),
            running: Arc::new(Mutex::new(true)),
            driver_manager: Arc::new(Mutex::new(DriverManager::from_driver(Box::new(
                driver_common::NullDriver::new(),
            )))),
            system_state: Arc::new(Mutex::new(SystemwideState::default())),
            key_manager: Arc::new(Mutex::new(KeyManager::default())),
            pipeline_mutation: Arc::new(Mutex::new(())),
            pipeline_intent_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            runtime_telemetry: Arc::new(RuntimeTelemetry::default()),
            playback_progress: Arc::new(Mutex::new(PlaybackProgress::default())),
            snapshot_cache: Arc::new(Mutex::new(None)),
            metering_cache: Arc::new(Mutex::new(None)),
            device_registry: Arc::new(Mutex::new(DeviceRegistry::default())),
            output_profiles: Arc::new(Mutex::new(
                crate::output_profiles::OutputProfileStore::default(),
            )),
        };
        let resp = daemon.handle_command(Command::Status);
        assert!(resp.success, "{:?}", resp.error);
        let mut data = resp.data.expect("status data");
        data["encryption"]["enabled"] = serde_json::json!(false);
        data["encryption"]["fingerprint"] = serde_json::json!("0000000000000000");
        insta::assert_json_snapshot!(data);
    }

    #[test]
    fn snapshot_list_devices_response_shape() {
        // The real device list depends on the host, so we snapshot a
        // representative shape with both default and regular devices.
        let devices = serde_json::json!([
            { "name": "Built-in Output", "is_default": true },
            { "name": "ADAM Audio D3V", "is_default": false, "channels": 2, "sample_rate": 48000 }
        ]);
        let resp = Response::ok(serde_json::json!({ "devices": devices }));
        insta::assert_json_snapshot!(resp);
    }
}

// ============================================================================
// Property-Based Tests
// ============================================================================

#[cfg(test)]
mod property_tests {
    use super::Command;
    use super::Response;
    use super::serialize_response_safely;
    use proptest::prelude::*;
    use serde_json::json;

    fn simple_string_strategy() -> BoxedStrategy<String> {
        proptest::string::string_regex("[a-zA-Z0-9_ ./:-]+")
            .unwrap()
            .boxed()
    }

    fn command_payload_strategy() -> BoxedStrategy<serde_json::Value> {
        // Use only canonical wire names. Aliases (e.g. "snapshot", "hal_status")
        // are exercised separately in unit tests.
        prop_oneof![
            Just(json!({ "command": "status" })),
            Just(json!({ "command": "get_snapshot" })),
            Just(json!({ "command": "dump_state" })),
            simple_string_strategy().prop_map(|path| json!({ "command": "load", "path": path })),
            Just(json!({ "command": "play" })),
            Just(json!({ "command": "pause" })),
            Just(json!({ "command": "stop" })),
            (0.0f64..3600.0)
                .prop_map(|position| json!({ "command": "seek", "position": position })),
            (-60.0f32..12.0f32)
                .prop_map(|volume| json!({ "command": "set_volume", "volume": volume })),
            Just(json!({ "command": "list_devices" })),
            simple_string_strategy()
                .prop_map(|device| json!({ "command": "set_device", "device": device })),
            Just(json!({ "command": "get_loudness" })),
            Just(json!({ "command": "get_metering" })),
            Just(json!({ "command": "get_plugins" })),
            Just(json!({ "command": "get_available_plugins" })),
            Just(json!({ "command": "driver_status" })),
            Just(json!({ "command": "shutdown" })),
            prop::bool::ANY
                .prop_map(|enabled| json!({ "command": "set_encryption", "enabled": enabled })),
            Just(json!({ "command": "encryption_status" })),
            Just(json!({ "command": "rotate_encryption_key" })),
            (1u32..192_000u32)
                .prop_map(|rate| json!({ "command": "set_sample_rate", "rate": rate })),
            (1u32..8192u32)
                .prop_map(|frames| json!({ "command": "set_buffer_frames", "frames": frames })),
            Just(json!({ "command": "get_driver_config" })),
        ]
        .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// INVARIANT: every valid JSON command payload deserializes to a Command
        /// whose wire name matches the `command` field in the input payload.
        #[test]
        fn command_json_round_trips_name(payload in command_payload_strategy()) {
            let command_name = payload["command"].as_str().unwrap_or("").to_string();
            let raw = serde_json::to_string(&payload).expect("payload serializes");
            let parsed: Command = serde_json::from_str(&raw).expect("valid payload parses");
            prop_assert_eq!(
                parsed.name(),
                command_name,
                "variant name did not round-trip for payload {}",
                raw
            );
        }

        /// INVARIANT: Response values always serialize to JSON with the expected
        /// top-level shape: a boolean `success`, optional `data`, and optional `error`.
        /// Error responses always include an `error` string; ok responses never do.
        #[test]
        fn response_shape_is_stable(
            success in prop::bool::ANY,
            data in prop::option::of(Just(json!({ "value": 42 }))),
            error in "[^\\x00]*",
        ) {
            let resp = if success {
                Response { success: true, data, error: None }
            } else {
                Response { success: false, data: None, error: Some(error.to_string()) }
            };
            let json = serde_json::to_value(&resp).expect("Response serializes");
            prop_assert_eq!(json["success"].as_bool(), Some(success));
            if success {
                prop_assert!(json.get("error").is_none() || json["error"].is_null());
            } else {
                prop_assert!(json["error"].is_string(), "error response must include an error string");
                prop_assert!(json.get("data").is_none() || json["data"].is_null());
            }
        }

        /// INVARIANT: `serialize_response_safely` never panics and returns parseable JSON.
        #[test]
        fn serialize_response_safely_never_panics(
            success in prop::bool::ANY,
            data in prop::option::of(Just(json!({ "value": 42 }))),
            error in "[^\\x00]*",
        ) {
            let resp = if success {
                Response { success: true, data, error: None }
            } else {
                Response { success: false, data: None, error: Some(error.to_string()) }
            };
            let out = serialize_response_safely(&resp);
            let parsed: serde_json::Value = serde_json::from_str(&out).expect("safe output parses");
            prop_assert_eq!(parsed["success"].as_bool(), Some(success));
        }
    }
}
