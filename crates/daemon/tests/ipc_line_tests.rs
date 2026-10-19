//! End-to-end IPC round-trip tests for the sotf-daemon binary.
//!
//! These tests spawn the daemon as a subprocess with the null HAL driver and
//! a private IPC endpoint, then send JSON commands and verify JSON
//! responses. They do not require a real audio capture driver.
//!
//! The endpoint is `SOTF_DAEMON_SOCKET_PATH` on every platform: a Unix
//! socket path on Unix, a loopback-TCP port file on Windows (see
//! `ipc_transport`).

use serial_test::serial;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static DAEMON_PROCESS_LOCK: Mutex<()> = Mutex::new(());

#[cfg(windows)]
#[path = "../bin/ipc_transport.rs"]
#[allow(dead_code)] // The integration fixture uses only the client-side API.
mod ipc_transport;

/// Connect to the fixture daemon's IPC endpoint: dial the Unix socket at
/// `socket_path` on Unix; on Windows read the loopback port back from the
/// port file at `socket_path` and dial `127.0.0.1:port` with a timeout.
#[cfg(unix)]
fn connect_daemon(socket_path: &Path) -> std::os::unix::net::UnixStream {
    std::os::unix::net::UnixStream::connect(socket_path).expect("connect to daemon socket")
}

#[cfg(windows)]
fn connect_daemon(socket_path: &Path) -> std::net::TcpStream {
    ipc_transport::connect_windows(socket_path).expect("authenticate fixture connection")
}

/// Reap subprocesses created outside `DaemonFixture`, including on assertion panic.
struct OwnedChild(Child);

impl Deref for OwnedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DaemonFixture {
    child: Child,
    socket_path: PathBuf,
    stderr_path: PathBuf,
    _temp_dir: tempfile::TempDir,
    _process_guard: MutexGuard<'static, ()>,
}

impl DaemonFixture {
    // The `Child` handle is stored in the fixture and reaped by `shutdown()`;
    // clippy cannot see the cross-method lifecycle.
    #[allow(clippy::zombie_processes)]
    fn start() -> Self {
        Self::start_with_driver("lab")
    }

    #[allow(clippy::zombie_processes)]
    fn start_with_driver(driver: &str) -> Self {
        let process_guard = DAEMON_PROCESS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let socket_path = temp_dir.path().join("daemon.sock");
        let stderr_path = temp_dir.path().join("daemon.stderr.log");
        let stderr = File::create(&stderr_path).expect("create private daemon stderr log");

        let mut child = Command::new(env!("CARGO_BIN_EXE_sotf-daemon"))
            .env("SOTF_DAEMON_SOCKET_PATH", &socket_path)
            .env("SOTF_SYSTEMWIDE_RUNTIME_DIR", temp_dir.path())
            .env(
                "SOTF_SYSTEMWIDE_STATE_PATH",
                temp_dir.path().join("systemwide-state.json"),
            )
            .env_remove("SOTF_OUTPUT_DEVICE")
            .env("SOTF_SYSTEMWIDE_DRIVER", driver)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn sotf-daemon");
        eprintln!(
            "lab fixture: daemon pid={} driver={driver} waiting for socket",
            child.id()
        );

        // Wait for the daemon to bind its socket.
        for _ in 0..100 {
            if socket_path.exists() {
                eprintln!("lab fixture: daemon socket ready pid={}", child.id());
                return Self {
                    child,
                    socket_path,
                    stderr_path,
                    _temp_dir: temp_dir,
                    _process_guard: process_guard,
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = child.kill();
        let status = child.wait().ok();
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        panic!("daemon did not create socket in time (status={status:?}): {stderr}");
    }

    fn stderr_tail(&self) -> String {
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        stderr
            .lines()
            .rev()
            .take(80)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn send(&self, command_json: &str) -> serde_json::Value {
        let command_name = serde_json::from_str::<serde_json::Value>(command_json)
            .ok()
            .and_then(|command| command["command"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_string());
        eprintln!("lab fixture: sending {command_name}");
        let mut stream = connect_daemon(&self.socket_path);
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("set daemon response timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set daemon request timeout");
        writeln!(stream, "{command_json}").unwrap_or_else(|error| {
            panic!(
                "daemon {command_name} request failed: {error}; daemon stderr:\n{}",
                self.stderr_tail()
            )
        });

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).unwrap_or_else(|error| {
            panic!(
                "daemon {command_name} response failed: {error}; daemon stderr:\n{}",
                self.stderr_tail()
            )
        });
        assert!(
            bytes_read > 0,
            "daemon {command_name} closed without a response; daemon stderr:\n{}",
            self.stderr_tail()
        );
        eprintln!("lab fixture: received {command_name}");

        serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!(
                "daemon {command_name} response is invalid JSON: {error}; daemon stderr:\n{}",
                self.stderr_tail()
            )
        })
    }

    fn wait_for_lab_playback(&self) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let snapshot = self.send(r#"{"command":"get_snapshot"}"#);
            if snapshot["data"]["observed"]["engine"]["playback_output_device"]
                == "Systemwide Lab Output"
                && snapshot["data"]["observed"]["engine"]["playback_callback_count"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
            {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "lab playback did not start: {snapshot}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[allow(clippy::zombie_processes)]
    fn restart(&mut self) {
        assert_eq!(self.send(r#"{"command":"shutdown"}"#)["success"], true);
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "daemon did not stop before restart"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let stderr = File::create(&self.stderr_path).unwrap();
        self.child = Command::new(env!("CARGO_BIN_EXE_sotf-daemon"))
            .env("SOTF_DAEMON_SOCKET_PATH", &self.socket_path)
            .env("SOTF_SYSTEMWIDE_RUNTIME_DIR", self._temp_dir.path())
            .env(
                "SOTF_SYSTEMWIDE_STATE_PATH",
                self._temp_dir.path().join("systemwide-state.json"),
            )
            .env_remove("SOTF_OUTPUT_DEVICE")
            .env("SOTF_SYSTEMWIDE_DRIVER", "lab")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap();
        while !self.socket_path.exists() {
            assert!(Instant::now() < deadline, "restarted daemon did not bind");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn shutdown(mut self) {
        eprintln!(
            "lab fixture: requesting daemon shutdown pid={}",
            self.child.id()
        );
        let _ = self.send(r#"{"command":"shutdown"}"#);

        for _ in 0..100 {
            if self.child.try_wait().ok().flatten().is_some() {
                eprintln!("lab fixture: daemon shutdown complete");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[serial]
fn profile_switch_preserves_both_saved_chains_and_failed_route() {
    let mut fixture = DaemonFixture::start();
    // Fence asynchronous startup with a serialized explicit playback intent.
    assert_eq!(fixture.send(r#"{"command":"play"}"#)["success"], true);
    let mut ids = Vec::new();
    for (name, gain, graph) in [("A", -3.0, false), ("B", -9.0, true)] {
        let response = fixture.send(
            &serde_json::json!({
                "command": "set_output_profile",
                "profile": {
                    "id": "", "name": name, "topology": if graph { "graph" } else { "rack" },
                    "plugins": if graph { serde_json::json!([]) } else { serde_json::json!([{"plugin_type":"gain","parameters":{"gain_db":gain}}]) },
                    "graph": if graph { serde_json::json!({"nodes":[{"id":5,"plugin_type":"gain","parameters":{"gain_db":gain},"input_channels":2}],"edges":[]}) } else { serde_json::Value::Null }, "input_channels": 2, "output_channels": 2,
                    "updated_at_unix_ms": 0,
                },
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "{response}");
        ids.push(response["data"]["profile_id"].as_str().unwrap().to_owned());
    }
    let saved_gain = |profile: &serde_json::Value| {
        if profile["topology"] == "graph" {
            profile["graph"]["nodes"][0]["parameters"]["gain_db"].clone()
        } else {
            profile["plugins"][0]["parameters"]["gain_db"].clone()
        }
    };
    for index in [0, 1, 0] {
        let response = fixture.send(
            &serde_json::json!({
                "command": "set_output_route", "output_device": "Systemwide Lab Output",
                "profile_id": ids[index],
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "{response}");
        let profiles = fixture.send(r#"{"command":"get_output_profiles"}"#);
        assert_eq!(profiles["data"]["current"]["profile_id"], ids[index]);
        for (id, expected_gain) in ids.iter().zip([-3.0, -9.0]) {
            let saved = profiles["data"]["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .find(|profile| profile["id"] == *id)
                .unwrap();
            assert_eq!(saved_gain(saved), expected_gain);
        }
    }
    let failed = fixture.send(
        &serde_json::json!({
            "command": "set_output_route", "output_device": "Missing Output", "profile_id": ids[1],
        })
        .to_string(),
    );
    assert_eq!(failed["success"], false);
    let persisted: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture._temp_dir.path().join("output-profiles.json")).unwrap(),
    )
    .unwrap();
    for (id, expected_gain) in ids.iter().zip([-3.0, -9.0]) {
        let saved = persisted["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|profile| profile["id"] == *id)
            .unwrap();
        assert_eq!(saved_gain(saved), expected_gain);
    }
    fixture.restart();
    let restarted = fixture.wait_for_lab_playback();
    assert_eq!(restarted["data"]["applied"]["profile_id"], ids[0]);
    let live = fixture.send(r#"{"command":"get_plugins"}"#);
    assert_eq!(live["data"]["plugins"][0]["parameters"]["gain_db"], -3.0);
    fixture.shutdown();
}

#[test]
#[serial]
fn daemon_status_roundtrip_over_ipc() {
    let daemon = DaemonFixture::start();

    let response = daemon.send(r#"{"command":"status"}"#);

    assert_eq!(response["success"], true);
    assert!(response["data"]["state"].is_string());
    assert!(response["data"]["volume"].is_number());
    assert!(response["data"]["input_channels"].is_number());
    assert!(response["data"]["output_channels"].is_number());

    daemon.shutdown();
}

#[test]
#[serial]
fn daemon_ping_roundtrip_over_ipc() {
    let daemon = DaemonFixture::start();
    let response = daemon.send(r#"{"command":"ping"}"#);
    assert_eq!(response["success"], true, "{response}");
    daemon.shutdown();
}

#[test]
#[serial]
fn second_daemon_cannot_take_ownership_of_a_live_runtime() {
    let daemon = DaemonFixture::start();

    let second_stderr_path = daemon._temp_dir.path().join("second.stderr.log");
    let second_stderr = File::create(&second_stderr_path).expect("create second daemon stderr log");
    let mut second = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_sotf-daemon"))
            .env("SOTF_DAEMON_SOCKET_PATH", &daemon.socket_path)
            .env("SOTF_SYSTEMWIDE_RUNTIME_DIR", daemon._temp_dir.path())
            .env(
                "SOTF_SYSTEMWIDE_STATE_PATH",
                daemon._temp_dir.path().join("systemwide-state.json"),
            )
            .env_remove("SOTF_OUTPUT_DEVICE")
            .env("SOTF_SYSTEMWIDE_DRIVER", "lab")
            .stdout(Stdio::null())
            .stderr(Stdio::from(second_stderr))
            .spawn()
            .expect("spawn second sotf-daemon"),
    );

    let mut second_status = None;
    for _ in 0..100 {
        if let Some(status) = second.try_wait().expect("poll second daemon") {
            second_status = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if second_status.is_none() {
        let _ = second.kill();
        let _ = second.wait();
        panic!("second daemon must exit while the first owns the runtime lock");
    }

    let status = second_status.expect("second daemon exit status");
    assert!(!status.success(), "second daemon unexpectedly became owner");
    assert!(
        daemon.socket_path.exists(),
        "second daemon must not remove the live daemon socket"
    );
    let first_status = daemon.send(r#"{"command":"status"}"#);
    assert_eq!(first_status["success"], true);

    daemon.shutdown();
}

#[cfg(all(target_os = "macos", feature = "hal"))]
#[test]
#[serial]
fn second_daemon_with_distinct_socket_cannot_rotate_shared_transport_key() {
    let daemon = DaemonFixture::start();
    let first_encryption = daemon.send(r#"{"command":"encryption_status"}"#);
    assert_eq!(first_encryption["success"], true, "{first_encryption}");
    let first_fingerprint = first_encryption["data"]["fingerprint"]
        .as_str()
        .expect("HAL daemon publishes a key fingerprint")
        .to_string();
    let hal_key_path = daemon._temp_dir.path().join("session.key");
    let first_hal_key = std::fs::read(&hal_key_path).expect("read first HAL key");
    let second_socket_path = daemon._temp_dir.path().join("alternate-daemon.sock");

    let second_stderr_path = daemon._temp_dir.path().join("alternate-daemon.stderr.log");
    let second_stderr =
        File::create(&second_stderr_path).expect("create alternate daemon stderr log");
    let mut second = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_sotf-daemon"))
            .env("SOTF_DAEMON_SOCKET_PATH", &second_socket_path)
            .env("SOTF_SYSTEMWIDE_RUNTIME_DIR", daemon._temp_dir.path())
            .env(
                "SOTF_SYSTEMWIDE_STATE_PATH",
                daemon._temp_dir.path().join("systemwide-state.json"),
            )
            .env_remove("SOTF_OUTPUT_DEVICE")
            .env("SOTF_SYSTEMWIDE_DRIVER", "lab")
            .stdout(Stdio::null())
            .stderr(Stdio::from(second_stderr))
            .spawn()
            .expect("spawn second sotf-daemon"),
    );

    let mut second_status = None;
    for _ in 0..100 {
        if let Some(status) = second.try_wait().expect("poll second daemon") {
            second_status = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if second_status.is_none() {
        let _ = second.kill();
        let _ = second.wait();
        panic!("second daemon must exit while the first owns the HAL transport");
    }

    assert!(
        !second_status.expect("second daemon exit status").success(),
        "second daemon unexpectedly acquired the shared HAL transport"
    );
    assert!(
        !second_socket_path.exists(),
        "losing daemon must exit before binding its distinct control socket"
    );
    assert_eq!(
        std::fs::read(&hal_key_path).expect("read HAL key after rejected startup"),
        first_hal_key,
        "losing daemon must not rotate the active HAL key"
    );

    let first_after = daemon.send(r#"{"command":"encryption_status"}"#);
    assert_eq!(first_after["success"], true, "{first_after}");
    assert_eq!(
        first_after["data"]["fingerprint"].as_str(),
        Some(first_fingerprint.as_str()),
        "active daemon fingerprint must remain unchanged"
    );
    let first_status = daemon.send(r#"{"command":"status"}"#);
    assert_eq!(first_status["success"], true);

    daemon.shutdown();
}

#[test]
#[serial]
fn daemon_get_metering_roundtrip_over_ipc() {
    let daemon = DaemonFixture::start();

    let response = daemon.send(r#"{"command":"get_metering"}"#);

    assert_eq!(response["success"], true);
    assert!(response["data"]["input"].is_object());
    assert!(response["data"]["output"].is_object());
    assert!(response["data"]["sources"]["input"].is_object());
    assert!(response["data"]["sources"]["output"].is_object());

    daemon.shutdown();
}

#[test]
#[serial]
fn daemon_set_volume_roundtrip_over_ipc() {
    let daemon = DaemonFixture::start();

    let response = daemon.send(r#"{"command":"set_volume","volume":0.37}"#);

    assert_eq!(response["success"], true, "{response}");

    daemon.shutdown();
}

#[test]
#[serial]
fn daemon_shutdown_over_ipc_stops_process() {
    let mut daemon = DaemonFixture::start();

    // Send shutdown; the daemon may exit before writing a response, so we
    // only verify that the process terminates afterwards.
    let mut stream = connect_daemon(&daemon.socket_path);
    writeln!(stream, r#"{{"command":"shutdown"}}"#).expect("write shutdown");
    drop(stream);

    for _ in 0..100 {
        if daemon.child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    panic!("daemon did not exit after shutdown");
}

#[test]
#[serial]
fn daemon_shutdown_drains_clients_and_allows_immediate_restart() {
    let mut daemon = DaemonFixture::start();
    let mut idle_clients = Vec::new();
    for _ in 0..32 {
        idle_clients.push(connect_daemon(&daemon.socket_path));
    }
    for _ in 0..32 {
        let mut closing = connect_daemon(&daemon.socket_path);
        closing
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        closing.shutdown(std::net::Shutdown::Write).unwrap();
        // Fence server-observed EOF. Local close alone does not mean its
        // handler released admission; otherwise the shutdown client can be
        // the rejected 65th connection during a fast accept burst.
        let mut discarded = Vec::new();
        std::io::Read::read_to_end(&mut closing, &mut discarded)
            .expect("server observes disconnected client");
    }

    let started = Instant::now();
    let mut shutdown = connect_daemon(&daemon.socket_path);
    writeln!(shutdown, r#"{{"command":"shutdown"}}"#).expect("write shutdown");
    drop(shutdown);

    for _ in 0..100 {
        if daemon.child.try_wait().ok().flatten().is_some() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "daemon shutdown waited for client idle timeouts"
            );
            assert!(
                !daemon.socket_path.exists(),
                "shutdown with active clients must remove the socket"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        daemon.child.try_wait().ok().flatten().is_some(),
        "daemon did not exit while persistent clients were connected"
    );
    drop(idle_clients);

    let restart_stderr_path = daemon._temp_dir.path().join("restart.stderr.log");
    let restart_stderr = File::create(&restart_stderr_path).expect("create restart stderr log");
    let mut restarted = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_sotf-daemon"))
            .env("SOTF_DAEMON_SOCKET_PATH", &daemon.socket_path)
            .env("SOTF_SYSTEMWIDE_RUNTIME_DIR", daemon._temp_dir.path())
            .env(
                "SOTF_SYSTEMWIDE_STATE_PATH",
                daemon._temp_dir.path().join("systemwide-state.json"),
            )
            .env_remove("SOTF_OUTPUT_DEVICE")
            .env("SOTF_SYSTEMWIDE_DRIVER", "lab")
            .stdout(Stdio::null())
            .stderr(Stdio::from(restart_stderr))
            .spawn()
            .expect("restart sotf-daemon"),
    );

    for _ in 0..100 {
        if daemon.socket_path.exists() {
            break;
        }
        if let Some(status) = restarted.try_wait().expect("poll restarted daemon") {
            let stderr = std::fs::read_to_string(&restart_stderr_path).unwrap_or_default();
            panic!("restarted daemon exited early ({status}): {stderr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        daemon.socket_path.exists(),
        "restarted daemon did not reclaim its runtime"
    );
    let status = daemon.send(r#"{"command":"status"}"#);
    assert_eq!(status["success"], true, "{status}");
    let _ = daemon.send(r#"{"command":"shutdown"}"#);

    for _ in 0..100 {
        if restarted.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = restarted.kill();
    let _ = restarted.wait();
    panic!("restarted daemon did not shut down");
}

// SIGTERM delivery via `/bin/kill` is Unix-only; graceful shutdown itself is
// covered on every platform by the shutdown-command tests above.
#[cfg(unix)]
#[test]
#[serial]
fn daemon_sigterm_clears_runtime_socket_and_reaps_process() {
    let mut daemon = DaemonFixture::start();
    let pid = daemon.child.id().to_string();

    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid])
        .status()
        .expect("send SIGTERM to daemon");
    assert!(status.success(), "kill should deliver SIGTERM: {status}");

    for _ in 0..100 {
        if daemon.child.try_wait().ok().flatten().is_some() {
            assert!(
                !daemon.socket_path.exists(),
                "SIGTERM shutdown must remove the daemon-owned socket"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    panic!("daemon did not exit after SIGTERM");
}

#[test]
#[serial]
fn systemwide_lab_scenario_matrix_over_ipc() {
    let daemon = DaemonFixture::start_with_driver("lab");

    let initial = daemon.wait_for_lab_playback();
    assert_eq!(initial["success"], true);
    assert_eq!(
        initial["data"]["observed"]["driver"]["driver_name"],
        "Systemwide Lab Driver"
    );
    assert_eq!(
        initial["data"]["observed"]["driver"]["platform_supported"],
        true
    );
    assert_eq!(
        initial["data"]["observed"]["driver"]["driver_installed"],
        true
    );
    assert_eq!(
        initial["data"]["observed"]["driver"]["capture_active"],
        true
    );
    assert!(initial["data"]["desired"]["input_channels"].is_number());
    assert!(initial["data"]["desired"]["output_channels"].is_number());
    assert!(initial["data"]["diagnostics"]["faults"].is_array());
    let devices = daemon.send(r#"{"command":"list_devices"}"#);
    assert_eq!(devices["success"], true, "{devices}");
    assert_eq!(devices["data"]["devices"].as_array().unwrap().len(), 1);
    assert_eq!(
        devices["data"]["devices"][0]["name"],
        "Systemwide Lab Output"
    );
    assert_eq!(devices["data"]["devices"][0]["channels"], 16);
    let physical = daemon.send(r#"{"command":"set_device","device":"EVO8"}"#);
    assert_eq!(physical["success"], false, "{physical}");
    assert!(
        physical["error"]
            .as_str()
            .is_some_and(|error| error.contains("Lab backend"))
    );

    // The marker affects only this private lab runtime, never the installed HAL.
    let idle_marker = daemon._temp_dir.path().join("lab-capture-idle");
    std::fs::write(&idle_marker, b"").expect("mark lab capture idle");
    let idle = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(idle["data"]["observed"]["driver"]["capture_active"], false);
    assert_eq!(
        idle["data"]["observed"]["transport"]["hal_capture_active"],
        false
    );
    std::fs::remove_file(&idle_marker).expect("resume lab capture");
    let resumed = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(
        resumed["data"]["observed"]["driver"]["capture_active"],
        true
    );
    assert_eq!(
        resumed["data"]["observed"]["transport"]["hal_capture_active"],
        true
    );

    let current_generation = resumed["data"]["generation"]
        .as_u64()
        .expect("lab snapshot generation");
    let stale_intent = format!(
        r#"{{"command":"set_pipeline_channels","input_channels":2,"output_channels":2,"base_generation":{}}}"#,
        current_generation.saturating_add(1)
    );
    let stale_response = daemon.send(&stale_intent);
    assert_eq!(stale_response["success"], false);
    assert!(
        stale_response["error"]
            .as_str()
            .is_some_and(|error| error.contains("generation conflict")),
        "unexpected stale-intent response: {stale_response}"
    );

    let initial_driver_config = daemon.send(r#"{"command":"get_driver_config"}"#);
    assert_eq!(initial_driver_config["success"], true);
    assert_eq!(initial_driver_config["data"]["sample_rate"], 48_000);
    assert_eq!(initial_driver_config["data"]["buffer_frames"], 512);

    // Live timing changes are intentionally rejected while the engine is
    // active; stop first, then verify the idle configuration path.
    let stopped = daemon.send(r#"{"command":"stop"}"#);
    assert_eq!(
        stopped["success"], true,
        "lab playback engine must be available for the remaining scenario: {stopped}"
    );

    let sample_rate = daemon.send(r#"{"command":"set_sample_rate","rate":96000}"#);
    assert_eq!(sample_rate["success"], true, "{sample_rate}");
    let buffer_frames = daemon.send(r#"{"command":"set_buffer_frames","frames":256}"#);
    assert_eq!(buffer_frames["success"], true, "{buffer_frames}");

    let reconfigured_driver = daemon.send(r#"{"command":"get_driver_config"}"#);
    assert_eq!(reconfigured_driver["data"]["sample_rate"], 96_000);
    assert_eq!(reconfigured_driver["data"]["actual_sample_rate"], 96_000);
    assert_eq!(reconfigured_driver["data"]["buffer_frames"], 256);
    assert_eq!(reconfigured_driver["data"]["actual_buffer_frames"], 256);
    assert_eq!(reconfigured_driver["data"]["active"], true);

    let invalid_sample_rate = daemon.send(r#"{"command":"set_sample_rate","rate":12345}"#);
    assert_eq!(invalid_sample_rate["success"], false);
    let invalid_buffer = daemon.send(r#"{"command":"set_buffer_frames","frames":32}"#);
    assert_eq!(invalid_buffer["success"], false);
    let config_after_rejection = daemon.send(r#"{"command":"get_driver_config"}"#);
    assert_eq!(
        config_after_rejection["data"], reconfigured_driver["data"],
        "invalid transport requests must preserve the active lab-driver config"
    );

    let encryption_enabled = daemon.send(r#"{"command":"set_encryption","enabled":true}"#);
    #[cfg(all(target_os = "macos", feature = "hal"))]
    assert_eq!(
        encryption_enabled["success"], true,
        "enable response: {encryption_enabled}"
    );
    #[cfg(not(all(target_os = "macos", feature = "hal")))]
    {
        assert_eq!(encryption_enabled["success"], false);
        assert!(
            encryption_enabled["error"]
                .as_str()
                .is_some_and(|error| { error.contains("no session cipher") })
        );
    }
    let rotation = daemon.send(r#"{"command":"rotate_encryption_key"}"#);
    #[cfg(all(target_os = "macos", feature = "hal"))]
    assert_eq!(rotation["success"], true, "rotation response: {rotation}");
    #[cfg(not(all(target_os = "macos", feature = "hal")))]
    {
        assert_eq!(rotation["success"], false, "rotation response: {rotation}");
        assert!(
            rotation["error"].as_str().is_some_and(|error| error
                .contains("encryption key rotation requires the macOS HAL-enabled daemon build")),
            "rotation response: {rotation}"
        );
    }
    let encryption_status = daemon.send(r#"{"command":"encryption_status"}"#);
    assert_eq!(encryption_status["success"], true);
    #[cfg(all(target_os = "macos", feature = "hal"))]
    assert_eq!(encryption_status["data"]["enabled"], true);
    #[cfg(not(all(target_os = "macos", feature = "hal")))]
    assert_eq!(encryption_status["data"]["enabled"], false);
    #[cfg(all(target_os = "macos", feature = "hal"))]
    assert_eq!(encryption_status["data"]["transport_state"], "unavailable");
    #[cfg(not(all(target_os = "macos", feature = "hal")))]
    assert_eq!(
        encryption_status["data"]["transport_state"],
        "not_applicable"
    );
    // Restore the disabled state for the rest of the lab session.
    let encryption_disabled = daemon.send(r#"{"command":"set_encryption","enabled":false}"#);
    assert_eq!(
        encryption_disabled["success"], true,
        "disable response: {encryption_disabled}"
    );

    let reconfigured = daemon
        .send(r#"{"command":"set_pipeline_channels","input_channels":10,"output_channels":2}"#);
    assert_eq!(reconfigured["success"], true);

    let after_reconfigure = daemon.wait_for_lab_playback();
    assert_eq!(
        after_reconfigure["data"]["observed"]["engine"]["playback_output_device"],
        "Systemwide Lab Output"
    );
    assert!(
        after_reconfigure["data"]["observed"]["engine"]["playback_callback_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_eq!(after_reconfigure["data"]["desired"]["input_channels"], 10);
    assert_eq!(after_reconfigure["data"]["desired"]["output_channels"], 2);

    let loaded = daemon.send(
        r#"{"command":"load_plugin_artifact","artifact":{"plugins":[{"plugin_type":"gain","parameters":{"gain_db":-3.0}},{"plugin_type":"eq","parameters":{}},{"plugin_type":"gain","parameters":{"gain_db":-6.0}}]}}"#,
    );
    assert_eq!(loaded["success"], true, "{loaded}");

    let after_load = daemon.wait_for_lab_playback();
    assert_eq!(
        after_load["data"]["observed"]["engine"]["playback_output_device"],
        "Systemwide Lab Output"
    );
    assert!(
        after_load["data"]["observed"]["engine"]["playback_callback_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_eq!(after_load["data"]["desired"]["input_channels"], 10);
    assert_eq!(after_load["data"]["desired"]["output_channels"], 2);
    assert_eq!(after_load["data"]["desired"]["user_plugin_count"], 3);
    assert_eq!(
        after_load["data"]["desired"]["user_plugin_types"],
        serde_json::json!(["gain", "eq", "gain"])
    );
    assert_eq!(
        after_load["data"]["applied"]["spec"]["user_plugin_count"],
        3
    );
    let loaded_generation = after_load["data"]["applied"]["generation"]
        .as_u64()
        .expect("loaded pipeline generation");

    let reloaded = daemon.send(
        r#"{"command":"load_plugin_artifact","artifact":{"plugins":[{"plugin_type":"gain","parameters":{"gain_db":-9.0}},{"plugin_type":"eq","parameters":{}},{"plugin_type":"gain","parameters":{"gain_db":-12.0}}]}}"#,
    );
    assert_eq!(reloaded["success"], true, "{reloaded}");
    let after_reload = daemon.wait_for_lab_playback();
    assert_eq!(
        after_reload["data"]["observed"]["engine"]["playback_output_device"],
        "Systemwide Lab Output"
    );
    assert!(
        after_reload["data"]["observed"]["engine"]["playback_callback_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_eq!(after_reload["data"]["desired"]["input_channels"], 10);
    assert_eq!(after_reload["data"]["desired"]["output_channels"], 2);
    assert_eq!(
        after_reload["data"]["desired"]["user_plugin_types"],
        serde_json::json!(["gain", "eq", "gain"])
    );
    assert_eq!(
        after_reload["data"]["applied"]["spec"]["input_channels"],
        10
    );
    assert_eq!(
        after_reload["data"]["applied"]["spec"]["output_channels"],
        2
    );
    assert_eq!(
        after_reload["data"]["applied"]["spec"]["user_plugin_count"],
        3
    );
    assert!(
        after_reload["data"]["applied"]["generation"]
            .as_u64()
            .is_some_and(|generation| generation > loaded_generation)
    );
    let reloaded_plugins = daemon.send(r#"{"command":"get_plugins"}"#);
    assert_eq!(
        reloaded_plugins["data"]["plugins"][0]["parameters"]["gain_db"],
        -9.0
    );
    assert_eq!(reloaded_plugins["data"]["plugins"][1]["plugin_type"], "eq");
    assert_eq!(
        reloaded_plugins["data"]["plugins"][2]["parameters"]["gain_db"],
        -12.0
    );

    let before_rejected_artifact = daemon.send(r#"{"command":"get_snapshot"}"#);
    let rejected = daemon.send(
        r#"{"command":"load_plugin_artifact","artifact":{"global_plugins":[{"plugin_type":"eq","parameters":{}}],"channels":{"L":{"plugins":[{"plugin_type":"gain","parameters":{}}]}}}}"#,
    );
    assert_eq!(rejected["success"], false);
    assert!(
        rejected["error"]
            .as_str()
            .is_some_and(|error| error.contains("Unsupported graph plugin artifact")),
        "{rejected}"
    );

    let after_rejected_artifact = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(
        after_rejected_artifact["data"]["desired"], before_rejected_artifact["data"]["desired"],
        "a rejected artifact must preserve desired pipeline state"
    );
    assert_eq!(
        after_rejected_artifact["data"]["applied"], before_rejected_artifact["data"]["applied"],
        "a rejected artifact must preserve the active pipeline"
    );

    let restored = daemon
        .send(r#"{"command":"set_pipeline_channels","input_channels":2,"output_channels":2}"#);
    assert_eq!(restored["success"], true);

    let final_snapshot = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(final_snapshot["data"]["desired"]["input_channels"], 2);
    assert_eq!(final_snapshot["data"]["desired"]["output_channels"], 2);
    assert_eq!(
        final_snapshot["data"]["observed"]["driver"]["driver_name"],
        "Systemwide Lab Driver"
    );
    assert_eq!(
        final_snapshot["data"]["observed"]["driver"]["sample_rate"],
        96_000
    );
    assert_eq!(
        final_snapshot["data"]["observed"]["driver"]["buffer_frames"],
        256
    );

    let diagnostic_dump = daemon.send(r#"{"command":"dump_state"}"#);
    assert_eq!(diagnostic_dump["success"], true);
    assert!(diagnostic_dump["data"]["snapshot"]["diagnostics"]["health"].is_string());
    assert!(diagnostic_dump["data"]["snapshot"]["diagnostics"]["faults"].is_array());
    assert!(diagnostic_dump["data"]["plugins"].is_array());

    daemon.shutdown();
}

#[test]
#[serial]
fn systemwide_lab_restarts_with_a_fresh_coherent_snapshot() {
    let first = DaemonFixture::start_with_driver("lab");
    let changed =
        first.send(r#"{"command":"set_pipeline_channels","input_channels":6,"output_channels":2}"#);
    assert_eq!(changed["success"], true, "{changed}");
    first.shutdown();

    let restarted = DaemonFixture::start_with_driver("lab");
    let snapshot = restarted.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(snapshot["success"], true);
    assert_eq!(
        snapshot["data"]["observed"]["driver"]["driver_name"],
        "Systemwide Lab Driver"
    );
    assert!(snapshot["data"]["desired"]["input_channels"].is_number());
    assert!(snapshot["data"]["desired"]["output_channels"].is_number());
    assert!(snapshot["data"]["diagnostics"]["health"].is_string());
    assert!(snapshot["data"]["diagnostics"]["faults"].is_array());
    restarted.shutdown();
}

#[test]
#[serial]
fn paused_play_resumes_and_long_idle_replaces_the_pipeline() {
    let daemon = DaemonFixture::start_with_driver("lab");
    let initial = daemon.wait_for_lab_playback();
    let generation = initial["data"]["generation"]
        .as_u64()
        .expect("pipeline generation");
    assert_eq!(daemon.send(r#"{"command":"pause"}"#)["success"], true);
    let paused = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(paused["data"]["observed"]["engine"]["state"], "Paused");
    assert_eq!(daemon.send(r#"{"command":"play"}"#)["success"], true);
    let resumed = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(resumed["data"]["observed"]["engine"]["state"], "Playing");
    assert_eq!(
        resumed["data"]["generation"], generation,
        "resume must keep the existing stream"
    );
    assert_eq!(daemon.send(r#"{"command":"play"}"#)["success"], true);
    let unchanged = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(unchanged["data"]["generation"], generation);
    assert_eq!(unchanged["data"]["observed"]["engine"]["muted"], false);

    let marker = daemon._temp_dir.path().join("lab-capture-idle");
    std::fs::write(&marker, b"").expect("idle capture");
    // Cross the production threshold; this proves the whole recovery path,
    // beyond the separate unit test of the tracker decision predicate.
    std::thread::sleep(Duration::from_secs(31));
    std::fs::remove_file(&marker).expect("resume capture");
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let snapshot = daemon.send(r#"{"command":"get_snapshot"}"#);
        if snapshot["data"]["generation"]
            .as_u64()
            .is_some_and(|new| new > generation)
            && snapshot["data"]["observed"]["engine"]["playback_callback_count"]
                .as_u64()
                .is_some_and(|count| count > 0)
        {
            assert_eq!(snapshot["data"]["observed"]["engine"]["state"], "Playing");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recovery never replaced the pipeline: {snapshot}; {}",
            daemon.stderr_tail()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let logs = std::fs::read_to_string(&daemon.stderr_path).expect("read recovery log");
    assert!(
        logs.contains("Long-idle physical playback rebuild succeeded"),
        "{}",
        daemon.stderr_tail()
    );
    daemon.shutdown();
}

#[cfg(windows)]
#[test]
#[serial]
fn silent_authentication_clients_do_not_delay_authenticated_status() {
    let daemon = DaemonFixture::start_with_driver("lab");
    let port = ipc_transport::read_port_file(&daemon.socket_path).expect("published port");
    let silent: Vec<_> = (0..3)
        .map(|_| ipc_transport::connect_loopback(port).expect("silent client"))
        .collect();
    let started = Instant::now();
    let response = daemon.send(r#"{"command":"get_snapshot"}"#);
    assert_eq!(response["success"], true);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "authentication serialized acceptance"
    );
    drop(silent);
    daemon.shutdown();
}
