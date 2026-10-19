use super::audio_daemon::{AudioDaemon, wait_for_playback_observation};
use super::configured::configured_output_device;
use super::consts::MAX_HAL_CHANNELS;
use super::consts::SUPPORTED_SAMPLE_RATES;
use super::device_registry::DeviceRegistry;
use super::driver_manager::DriverManager;
use super::misc::OutputDeviceFallback;
use super::output_profiles::OutputProfileStore;
use super::systemwide_state::SystemwideState;
use super::types::PipelinePlan;
use super::types::PipelineReconfigureOutcome;
use driver_common::DriverConfig;
use parking_lot::Mutex;
use sotf_audio::SinkType;
use sotf_audio::manager::AudioEngineManager;
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(test))]
const DRIVER_RECONFIGURE_READY_TIMEOUT: Duration = Duration::from_secs(12);
#[cfg(test)]
const DRIVER_RECONFIGURE_READY_TIMEOUT: Duration = Duration::from_millis(200);

pub(super) fn wait_for_reconfigured_playback(
    audio_manager: &Arc<Mutex<AudioEngineManager>>,
    running: &Arc<Mutex<bool>>,
) -> Result<(), String> {
    wait_for_playback_observation(
        running,
        DRIVER_RECONFIGURE_READY_TIMEOUT,
        "driver reconfiguration",
        || {
            let state = audio_manager.lock().get_engine_state();
            AudioDaemon::playback_startup_observation(&state)
        },
    )
}

pub(super) fn acknowledged_config_for_outcome(
    requested_rate: u32,
    requested_frames: u32,
    requested_channels: u32,
    actual_rate: u32,
    outcome: PipelineReconfigureOutcome,
) -> (DriverConfig, driver_common::ConfigResult) {
    let active_channels = match outcome {
        PipelineReconfigureOutcome::Restored { input_channels } => input_channels as u32,
        PipelineReconfigureOutcome::IdleUpdated | PipelineReconfigureOutcome::Restarted => {
            requested_channels
        }
    };
    let actual = DriverConfig::new(actual_rate, requested_frames, active_channels);
    let result = if actual_rate != requested_rate || active_channels != requested_channels {
        driver_common::ConfigResult::negotiated(actual_rate, requested_frames, active_channels)
    } else {
        driver_common::ConfigResult::Accepted
    };
    (actual, result)
}

/// Publish HAL readiness only after the configuration acknowledgement has
/// completed. A failed callback observation records recovery and keeps HAL
/// unready so the caller can acknowledge the failure instead.
pub(super) fn publish_reconfigured_driver_readiness(
    driver_manager: &Arc<Mutex<DriverManager>>,
    system_state: &Arc<Mutex<SystemwideState>>,
    active_config: DriverConfig,
    readiness: Result<(), String>,
) -> Result<(), String> {
    if let Err(error) = readiness {
        system_state.lock().mark_pipeline_recovery(error.clone());
        driver_manager.lock().acknowledge_config_change(
            active_config,
            driver_common::ConfigResult::error(error.clone()),
        );
        return Err(error);
    }

    if !driver_manager.lock().set_engine_ready(true) {
        let error = "Driver readiness publication failed after reconfiguration".to_string();
        system_state.lock().mark_pipeline_recovery(error.clone());
        return Err(error);
    }
    Ok(())
}

/// Test entry point without a persisted per-output profile store.
#[cfg(test)]
pub(super) fn handle_driver_config_change(
    driver_manager: &Arc<Mutex<DriverManager>>,
    audio_manager: &Arc<Mutex<AudioEngineManager>>,
    device_registry: &Arc<Mutex<DeviceRegistry>>,
    config: DriverConfig,
    system_state: &Arc<Mutex<SystemwideState>>,
    running: &Arc<Mutex<bool>>,
) {
    handle_driver_config_change_inner(
        driver_manager,
        audio_manager,
        device_registry,
        config,
        system_state,
        running,
        None,
    );
}

/// Production driver events use the same profile ownership as IPC and startup.
pub(super) fn handle_driver_config_change_with_profiles(
    driver_manager: &Arc<Mutex<DriverManager>>,
    audio_manager: &Arc<Mutex<AudioEngineManager>>,
    device_registry: &Arc<Mutex<DeviceRegistry>>,
    config: DriverConfig,
    system_state: &Arc<Mutex<SystemwideState>>,
    running: &Arc<Mutex<bool>>,
    profiles: &Arc<Mutex<OutputProfileStore>>,
) {
    handle_driver_config_change_inner(
        driver_manager,
        audio_manager,
        device_registry,
        config,
        system_state,
        running,
        Some(profiles),
    );
}

/// Handle a driver-initiated config change.
fn handle_driver_config_change_inner(
    driver_manager: &Arc<Mutex<DriverManager>>,
    audio_manager: &Arc<Mutex<AudioEngineManager>>,
    device_registry: &Arc<Mutex<DeviceRegistry>>,
    config: DriverConfig,
    system_state: &Arc<Mutex<SystemwideState>>,
    running: &Arc<Mutex<bool>>,
    profiles: Option<&Arc<Mutex<OutputProfileStore>>>,
) {
    let capture_source = driver_manager.lock().capture_source();
    let capture_config = capture_source.as_ref().map(|source| source.configuration());
    let native_event = capture_config == Some(config);
    let requested_rate = config.sample_rate;
    let requested_frames = config.buffer_frames;
    let requested_channels = config.channel_count;

    log::info!(
        "Driver config change request: sample_rate={}, buffer_frames={}, channels={}",
        requested_rate,
        requested_frames,
        requested_channels
    );

    // Validate requested values
    if requested_rate == 0 {
        log::warn!("Invalid config request: sample_rate=0, ignoring");
        driver_manager.lock().acknowledge_config_change(
            DriverConfig::new(48000, requested_frames, config.channel_count),
            driver_common::ConfigResult::error(driver_common::DriverError::invalid_config(
                "sample_rate",
                "Invalid sample rate",
            )),
        );
        return;
    }
    // One bound everywhere (IPC, watcher, HAL): the shared-memory layout
    // caps buffer_frames at MAX_HAL_BUFFER_FRAMES and Swift refuses larger
    // values, so accepting 4097..=65536 here diverged daemon/HAL geometry.
    let max_frames = if native_event {
        8192
    } else {
        super::misc::MAX_BUFFER_FRAMES
    };
    if requested_frames == 0 || requested_frames > max_frames {
        log::warn!(
            "Invalid config request: buffer_frames={}, out of range",
            requested_frames
        );
        driver_manager.lock().acknowledge_config_change(
            DriverConfig::new(requested_rate, 512, config.channel_count),
            driver_common::ConfigResult::error(driver_common::DriverError::invalid_config(
                "buffer_frames",
                "Invalid buffer frames",
            )),
        );
        return;
    }
    if requested_channels == 0 || requested_channels as usize > MAX_HAL_CHANNELS {
        log::warn!(
            "Invalid config request: channel_count={}, out of range",
            requested_channels
        );
        driver_manager.lock().acknowledge_config_change(
            DriverConfig::new(requested_rate, requested_frames, 2),
            driver_common::ConfigResult::error(driver_common::DriverError::invalid_config(
                "channel_count",
                "Invalid channel count",
            )),
        );
        return;
    }

    // Determine actual rate to use
    let mut actual_rate = if SUPPORTED_SAMPLE_RATES.contains(&requested_rate) {
        requested_rate
    } else {
        SUPPORTED_SAMPLE_RATES
            .iter()
            .min_by_key(|&&r| (r as i32 - requested_rate as i32).abs())
            .copied()
            .unwrap_or(48000)
    };

    // A native endpoint reconnect is an input fact, not an output rate intent.
    let mut processing_frames = requested_frames;
    if native_event {
        let fallback_rate = if SUPPORTED_SAMPLE_RATES.contains(&requested_rate) {
            requested_rate
        } else {
            48_000
        };
        let fallback_frames = if (64..=4096).contains(&requested_frames) {
            requested_frames
        } else {
            512
        };
        (actual_rate, processing_frames) = system_state
            .lock()
            .processing_timing
            .unwrap_or((fallback_rate, fallback_frames));
    }

    // Same device-support negotiation as the IPC apply path. A HAL rate
    // request can outlive the interface it was set for: the stored setup
    // says 96 kHz while the interface now runs at (or caps at) 48 kHz.
    // Starting the engine above the device maximum leaves the hardware
    // clock at its own rate while processing assumes the requested one,
    // which desynchronizes playback until the user switches rates back
    // and forth manually. The acknowledgement below reports the
    // negotiated rate to the driver, so HAL and engine stay coherent.
    let output_sink_type = driver_manager.lock().output_sink_type();
    let desired_device = system_state
        .lock()
        .selected_output_device()
        .or_else(configured_output_device);
    let effective = super::misc::effective_output_for_sink(
        super::misc::resolve_effective_output_device(desired_device.as_deref()),
        output_sink_type.clone(),
        desired_device.as_deref(),
    );
    if let Some(device_name) = effective.device.as_deref() {
        let lab_backend = driver_manager.lock().is_lab_backend();
        match device_registry
            .lock()
            .negotiate_sample_rate(device_name, actual_rate, lab_backend)
        {
            Ok(rate) if rate != actual_rate => {
                log::warn!(
                    "Output device '{device_name}' does not support {actual_rate} Hz; using {rate} Hz",
                );
                actual_rate = rate;
            }
            Ok(_) => {}
            Err(error) => {
                log::warn!("Sample-rate check skipped: {error}");
            }
        }
    }

    // Reconfigure audio pipeline
    // Stop publishing input frames while the old engine is being torn down.
    // A failed restart must not leave HAL believing that a stopped engine is
    // still ready to consume audio.
    driver_manager.lock().set_engine_ready(false);
    match reconfigure_audio_pipeline_with_sink(
        audio_manager,
        system_state,
        actual_rate,
        processing_frames,
        capture_config.map_or(requested_channels, |native| native.channel_count) as usize,
        ReconfigureBackend {
            sink_type: output_sink_type,
            capture_source,
            profiles,
        },
    ) {
        Ok(outcome) => {
            let (mut active_config, mut result) = acknowledged_config_for_outcome(
                requested_rate,
                requested_frames,
                requested_channels,
                actual_rate,
                outcome,
            );
            if let Some(native) = capture_config {
                active_config = native;
                result = if native == config {
                    driver_common::ConfigResult::Accepted
                } else {
                    driver_common::ConfigResult::negotiated(
                        native.sample_rate,
                        native.buffer_frames,
                        native.channel_count,
                    )
                };
            }
            system_state.lock().processing_timing = Some((actual_rate, processing_frames));
            if matches!(
                outcome,
                PipelineReconfigureOutcome::Restarted | PipelineReconfigureOutcome::Restored { .. }
            ) && let Err(error) = wait_for_reconfigured_playback(audio_manager, running)
            {
                log::error!("Driver reconfiguration readiness failed: {error}");
                let _ = publish_reconfigured_driver_readiness(
                    driver_manager,
                    system_state,
                    active_config,
                    Err(error.clone()),
                );
                return;
            }
            system_state.lock().clear_pipeline_recovery();
            if result != driver_common::ConfigResult::Accepted {
                log::info!(
                    "Config negotiated: requested {}Hz/{}ch, using {}Hz/{}ch",
                    requested_rate,
                    requested_channels,
                    active_config.sample_rate,
                    active_config.channel_count
                );
            }

            driver_manager
                .lock()
                .acknowledge_config_change(active_config, result);

            if matches!(
                outcome,
                PipelineReconfigureOutcome::Restarted | PipelineReconfigureOutcome::Restored { .. }
            ) {
                // Publish readiness only after HAL has acknowledged the exact
                // geometry consumed by the running engine.
                let _ = publish_reconfigured_driver_readiness(
                    driver_manager,
                    system_state,
                    active_config,
                    Ok(()),
                );
            }
            log::info!(
                "Config accepted: {}Hz, {} frames, {} channels, outcome={:?}",
                actual_rate,
                requested_frames,
                active_config.channel_count,
                outcome
            );
        }
        Err(e) => {
            log::error!("Pipeline reconfiguration failed: {}", e);
            system_state.lock().mark_pipeline_recovery(e.clone());
            driver_manager.lock().acknowledge_config_change(
                DriverConfig::new(actual_rate, requested_frames, config.channel_count),
                driver_common::ConfigResult::error(e),
            );
        }
    }
}

/// Reconfigure the audio pipeline with new sample rate and buffer size
#[cfg(test)]
pub(super) fn reconfigure_audio_pipeline(
    audio_manager: &Arc<Mutex<AudioEngineManager>>,
    system_state: &Arc<Mutex<SystemwideState>>,
    hal_sample_rate: u32,
    hal_buffer_frames: u32,
    input_channels: usize,
) -> Result<PipelineReconfigureOutcome, String> {
    reconfigure_audio_pipeline_with_sink(
        audio_manager,
        system_state,
        hal_sample_rate,
        hal_buffer_frames,
        input_channels,
        ReconfigureBackend {
            sink_type: SinkType::LabNull,
            capture_source: None,
            profiles: None,
        },
    )
}

struct ResolvedReconfigureRoute {
    plan: PipelinePlan,
    fallback: Option<OutputDeviceFallback>,
    profile: Option<(String, String)>,
}

fn resolve_reconfigure_route(
    mut plan: PipelinePlan,
    state: &Arc<Mutex<SystemwideState>>,
    profiles: Option<&Arc<Mutex<OutputProfileStore>>>,
    sink: SinkType,
) -> Result<ResolvedReconfigureRoute, String> {
    let desired = plan.spec.output_device.clone();
    let effective = super::misc::effective_output_for_sink(
        super::misc::resolve_effective_output_device(desired.as_deref()),
        sink.clone(),
        desired.as_deref(),
    );
    if let Some(name) = effective.device.as_deref() {
        if !super::misc::is_safe_output_device_name(name) {
            return Err("Refusing virtual physical output".to_owned());
        }
    } else if let Some(desired) = desired.as_deref() {
        return Err(format!(
            "Selected output device '{desired}' is not available: Audio device '{desired}' not found, and no safe physical fallback is available"
        ));
    }
    plan.spec.output_device = effective.device;
    let mut profile = None;
    if (effective.fallback.is_some()
        || state.lock().applied_output_device() != plan.spec.output_device)
        && desired.is_some()
        && let (Some(profiles), Some(name)) = (profiles, plan.spec.output_device.clone())
    {
        let input = plan.spec.input_channels;
        let selected = AudioDaemon::fallback_profile_for_store(
            profiles,
            &name,
            input,
            plan.spec.output_channels,
            sink,
        )?;
        plan = AudioDaemon::prepare_profile_plan(&state.lock(), plan.spec, &selected, Some(input))?;
        profile = Some((name, selected.id));
    }
    Ok(ResolvedReconfigureRoute {
        plan,
        fallback: effective.fallback,
        profile,
    })
}

struct ReconfigureBackend<'a> {
    sink_type: SinkType,
    capture_source: Option<driver_common::CaptureSource>,
    profiles: Option<&'a Arc<Mutex<OutputProfileStore>>>,
}

fn reconfigure_audio_pipeline_with_sink(
    audio_manager: &Arc<Mutex<AudioEngineManager>>,
    system_state: &Arc<Mutex<SystemwideState>>,
    hal_sample_rate: u32,
    hal_buffer_frames: u32,
    input_channels: usize,
    backend: ReconfigureBackend<'_>,
) -> Result<PipelineReconfigureOutcome, String> {
    let ReconfigureBackend {
        sink_type,
        capture_source,
        profiles,
    } = backend;
    let plan = {
        let state = system_state.lock();
        if let Some(graph) = state.user_graph() {
            state.prepare_graph_plan(
                graph,
                input_channels,
                state.output_channels(),
                input_channels,
            )?
        } else {
            state.prepare_plan(
                state.user_plugins(),
                input_channels,
                state.output_channels(),
                input_channels,
            )?
        }
    };

    // Keep a fully prepared copy of the last applied pipeline before
    // stopping the engine. If the new driver timing or graph cannot start,
    // the daemon can restore audio instead of leaving the process silent.
    let previous_plan = {
        let state = system_state.lock();
        state
            .applied_spec()
            .and_then(|spec| state.prepare_from_spec(spec, input_channels).ok())
    };

    let state = audio_manager.lock().get_state();
    if state == sotf_audio::manager::StreamingState::Idle {
        log::debug!("No active playback, acknowledging config change");
        system_state.lock().commit_idle_reconfigure(&plan);
        return Ok(PipelineReconfigureOutcome::IdleUpdated);
    }

    let resolved = resolve_reconfigure_route(plan, system_state, profiles, sink_type.clone())?;
    let plan = resolved.plan;
    let fallback = resolved.fallback;
    let current_profile = resolved.profile;
    // Resolve restoration before stopping too: it must never put A's DSP on B.
    let previous = previous_plan
        .map(|plan| resolve_reconfigure_route(plan, system_state, profiles, sink_type.clone()))
        .transpose()?;
    let mut manager = audio_manager.lock();

    log::info!("Reconfiguring driver playback pipeline");

    if let Err(e) = manager.stop() {
        log::warn!("Failed to stop current playback: {}", e);
    }

    log::info!(
        "Restarting driver playback with {} plugins (incl. 2 monitors), {} output channels, device: {:?}",
        plan.runtime_plugins.len(),
        plan.spec.output_channels,
        plan.spec.output_device
    );

    let result = AudioDaemon::start_pipeline_plan(
        &mut manager,
        &plan,
        hal_sample_rate,
        hal_buffer_frames,
        sink_type.clone(),
        capture_source.clone(),
    );

    match result {
        Ok(_) => {
            match &fallback {
                Some(fallback) => system_state
                    .lock()
                    .commit_fallback_applied(&plan, Some(fallback.desired.clone())),
                None => system_state.lock().commit_applied(&plan),
            }
            if let (Some(profiles), Some((name, id))) = (profiles, current_profile) {
                profiles
                    .lock()
                    .set_current_in_memory(None, Some(name), Some(id));
            }
            log::info!("Driver playback restarted successfully");
            Ok(PipelineReconfigureOutcome::Restarted)
        }
        Err(error) => {
            // Same device-absence aliasing as the load path: the engine's
            // generic exit message would otherwise latch an unactionable
            // fault when the desired output disappears mid-session.
            let e = super::misc::clarify_device_absence_error(
                plan.spec.output_device.as_deref(),
                &error,
            );
            log::error!("Failed to restart driver playback: {}", e);

            let Some(previous) = previous else {
                return Err(format!("Failed to restart driver playback: {}", e));
            };

            let previous_plan = previous.plan;
            if let Some(source) = capture_source.as_ref()
                && source.configuration().channel_count as usize
                    != previous_plan.spec.input_channels
            {
                return Err(format!(
                    "Failed to restart driver playback: {e}; previous capture geometry is no longer available"
                ));
            }
            log::warn!("Attempting to restore the last working driver pipeline");
            let restore = AudioDaemon::start_pipeline_plan(
                &mut manager,
                &previous_plan,
                hal_sample_rate,
                hal_buffer_frames,
                sink_type,
                capture_source,
            );
            if restore.is_ok() {
                match previous.fallback {
                    Some(fallback) => system_state
                        .lock()
                        .commit_fallback_applied(&previous_plan, Some(fallback.desired)),
                    None => system_state.lock().commit_applied(&previous_plan),
                }
                if let (Some(profiles), Some((name, id))) = (profiles, previous.profile) {
                    profiles
                        .lock()
                        .set_current_in_memory(None, Some(name), Some(id));
                }
                log::warn!(
                    "Restored the last working driver pipeline after reconfiguration failure"
                );
                return Ok(PipelineReconfigureOutcome::Restored {
                    input_channels: previous_plan.spec.input_channels,
                });
            }

            Err(format!(
                "Failed to restart driver playback: {}; pipeline recovery also failed: {}",
                e,
                restore
                    .err()
                    .unwrap_or_else(|| "unknown recovery error".to_string())
            ))
        }
    }
}
