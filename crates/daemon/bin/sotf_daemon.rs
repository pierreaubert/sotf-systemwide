//! Audio Engine Control Daemon
//!
//! An IPC daemon that provides control for the AudioEngineManager.
//! This allows external processes (like the Swift menubar app or GPUI configbar)
//! to control audio playback, query status, and configure plugins via JSON messages.
//!
//! Protocol: JSON messages (one JSON object per line) over a Unix domain
//! socket on Unix and a loopback TCP listener on Windows (see `ipc_transport`).
//!
//! The daemon is cross-platform:
//! - macOS: Uses CoreAudio HAL driver for system audio capture
//! - Linux: Uses the cpal capture driver (PipeWire monitor source)
//! - Windows: Uses the cpal capture driver (WASAPI capture endpoint)
//! - Fallback: NullDriver (no capture, status-only)

use std::sync::Arc;

mod cpal_capture;
mod driver_manager;
mod ipc_transport;
mod lock_order;
mod plugin_artifact;
mod security;

#[path = "sotf_daemon/audio_daemon.rs"]
mod audio_daemon;
#[path = "sotf_daemon/command.rs"]
mod command;
#[path = "sotf_daemon/configured.rs"]
mod configured;
#[path = "sotf_daemon/consts.rs"]
mod consts;
#[path = "sotf_daemon/default.rs"]
mod default;
#[path = "sotf_daemon/device_registry.rs"]
mod device_registry;
#[path = "sotf_daemon/loudness.rs"]
mod loudness;
#[path = "sotf_daemon/misc.rs"]
mod misc;
#[path = "sotf_daemon/output_profiles.rs"]
mod output_profiles;
#[path = "sotf_daemon/pipeline_reconfigure_outcome.rs"]
mod pipeline_reconfigure_outcome;
#[path = "sotf_daemon/pipeline_spec.rs"]
mod pipeline_spec;
#[path = "sotf_daemon/pipeline_supervisor.rs"]
mod pipeline_supervisor;
#[path = "sotf_daemon/plugin.rs"]
mod plugin;
#[path = "sotf_daemon/response.rs"]
mod response;
#[path = "sotf_daemon/systemwide_state.rs"]
mod systemwide_state;
#[cfg(test)]
#[path = "sotf_daemon/tests.rs"]
mod tests;
#[path = "sotf_daemon/types.rs"]
mod types;

use audio_daemon::AudioDaemon;
use misc::acquire_daemon_instance_lock;
use misc::elevate_daemon_thread_to_audio_work;
use security::{ensure_secure_socket_dir, get_secure_socket_path};
#[cfg(all(target_os = "macos", feature = "hal"))]
use security::{get_hal_key_path, get_key_path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // The accept loop runs on this thread; keep it at audio-work priority so
    // IPC control and metering stay responsive under load. Engine audio
    // threads elevate themselves separately.
    elevate_daemon_thread_to_audio_work("main accept loop");

    // Serialize ownership before touching a session key, shared memory, or a
    // stale socket. The control socket is independently configurable, so it
    // cannot be the identity of the HAL transport owner.
    let secure_socket_path = get_secure_socket_path();
    #[cfg_attr(not(all(target_os = "macos", feature = "hal")), allow(unused_mut))]
    let mut ownership_resources = vec![secure_socket_path.clone()];
    #[cfg(all(target_os = "macos", feature = "hal"))]
    ownership_resources.extend([
        driver_hal::get_shared_memory_path(),
        get_hal_key_path(),
        get_key_path(),
    ]);

    for resource_path in &ownership_resources {
        ensure_secure_socket_dir(resource_path).map_err(|error| {
            format!(
                "failed to prepare runtime resource directory for {}: {error}",
                resource_path.display()
            )
        })?;
    }
    let _instance_lock = acquire_daemon_instance_lock(&ownership_resources)?;

    let daemon = AudioDaemon::new();

    // A fresh key per HAL-enabled daemon lifetime guarantees that resetting
    // the shared frame counter during mmap initialization cannot reuse an AEAD
    // nonce. Non-HAL builds have no encrypted shared-memory transport, so they
    // must still start for the null/lab driver instead of treating the
    // deliberately unsupported manual rotation API as a startup failure.
    #[cfg(all(target_os = "macos", feature = "hal"))]
    daemon
        .key_manager
        .lock()
        .force_rotate()
        .map_err(|error| {
            format!(
                "cannot initialize the systemwide runtime/key directory; verify it is owned by the current user and accessible: {error}"
            )
        })?;

    // Setup signal handling for graceful shutdown — use the daemon's own
    // running flag so Ctrl-C actually stops the accept loop.
    {
        let running = Arc::clone(&daemon.running);
        ctrlc::set_handler(move || {
            println!("\nReceived interrupt signal, shutting down...");
            *running.lock() = false;
        })?;
    }

    println!("===============================================================================");
    println!("SotF Audio Control Daemon");
    println!("===============================================================================");

    // Initialize driver
    println!();
    {
        let mut driver = daemon.driver_manager.lock();
        match driver.initialize() {
            Ok(()) => {
                let status = driver.status();
                println!("Driver Status:");
                println!("   Driver:             {}", status.driver_name);
                println!(
                    "   Platform supported: {}",
                    if status.platform_supported {
                        "Yes"
                    } else {
                        "No"
                    }
                );
                println!(
                    "   Driver installed:   {}",
                    if status.driver_installed {
                        "Yes"
                    } else {
                        "No (optional)"
                    }
                );
                println!(
                    "   Capture active:     {}",
                    if status.capture_active { "Yes" } else { "No" }
                );

                if status.platform_supported && status.driver_installed {
                    println!();
                    println!("Audio flow (capture mode):");
                    // The HAL build moves audio through the encrypted shared
                    // memory transport; cpal/NullDriver builds capture
                    // straight from the input device.
                    #[cfg(all(target_os = "macos", feature = "hal"))]
                    println!(
                        "   System Audio -> Driver -> SharedMemory -> Daemon -> cpal -> Hardware"
                    );
                    #[cfg(not(all(target_os = "macos", feature = "hal")))]
                    println!("   Input Device -> Driver -> Daemon -> Hardware");
                }
            }
            Err(e) => {
                log::warn!("Failed to initialize driver: {}", e);
                log::warn!("Audio capture will not be available");
            }
        }
    }

    // Show encryption status
    {
        let key_manager = daemon.key_manager.lock();
        let status = key_manager.status();
        println!();
        println!("Encryption Status:");
        println!(
            "   Enabled:     {}",
            if status.enabled { "Yes" } else { "No" }
        );
        log::debug!("Encryption fingerprint: {}", status.fingerprint);
        log::debug!("Encryption key path: {}", status.key_path);
    }

    println!();
    println!("===============================================================================");
    println!("Starting daemon...");
    println!("===============================================================================");

    // Always stop the engine before releasing the driver, including bind or
    // accept-loop failures. This clears engine_ready and lets the shared
    // memory transport observe a clean owner shutdown after SIGTERM/SIGINT.
    let run_result = daemon.run();

    if let Err(error) = daemon.manager.lock().stop() {
        log::warn!(
            "Failed to stop audio engine during daemon shutdown: {}",
            error
        );
    }

    // Explicit driver cleanup
    {
        let mut driver = daemon.driver_manager.lock();
        driver.shutdown();
    }

    println!();
    println!("Daemon stopped cleanly");
    run_result
}
