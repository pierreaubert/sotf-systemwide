//! Portable IPC transport for `sotf-daemon`.
//!
//! The daemon speaks one JSON object per line on every platform, but the
//! underlying byte transport differs:
//!
//! - Unix (macOS/Linux): `AF_UNIX` socket at the per-user path from
//!   [`crate::security::get_secure_socket_path`]. Binding keeps the
//!   TOCTOU-safe stale-socket discipline in
//!   `sotf_daemon::misc::bind_unix_socket`.
//! - Windows: TCP loopback (`127.0.0.1`, OS-assigned port). The per-user
//!   "socket path" names a port file holding the ASCII port number; clients
//!   read the file and connect to `127.0.0.1:port`. Loopback-only peering is
//!   the credential-check equivalent (see `is_loopback_addr` and
//!   `security::verify_peer_credentials`).
//!
//! Call sites use [`IpcStream`] for per-client handlers on both platforms.
//! The Windows accept loop also uses `IpcListener`.

#[cfg(any(windows, test))]
use std::ffi::OsString;
#[cfg(any(windows, test))]
use std::net::SocketAddr;
#[cfg(windows)]
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;

/// Connected IPC stream: `UnixStream` on Unix, loopback `TcpStream` on Windows.
#[cfg(unix)]
pub type IpcStream = std::os::unix::net::UnixStream;

/// Connected IPC stream: `UnixStream` on Unix, loopback `TcpStream` on Windows.
#[cfg(windows)]
pub type IpcStream = std::net::TcpStream;

/// Listening IPC socket for the Windows loopback transport.
#[cfg(windows)]
pub type IpcListener = std::net::TcpListener;

/// Encode a bound loopback port for the port file (Windows).
///
/// The file holds the ASCII decimal port with a single trailing newline, so it
/// stays human-inspectable and trivially parseable by clients.
#[cfg(any(windows, test))]
pub fn encode_port_file_contents(port: u16) -> String {
    format!("{port}\n")
}

/// Decode port-file contents back to a TCP port.
///
/// Returns `None` for empty input, trailing garbage, or port `0` (never a
/// valid bound listener port). Pure function so clients and tests share the
/// exact acceptance rule.
#[cfg(any(windows, test))]
pub fn decode_port_file_contents(text: &str) -> Option<u16> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > 5 {
        return None;
    }
    if !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port: u16 = trimmed.parse().ok()?;
    if port == 0 { None } else { Some(port) }
}

/// True when `addr` is a loopback peer (IPv4 `127.0.0.0/8` or IPv6 `::1`).
///
/// The Windows daemon binds `127.0.0.1` and refuses non-loopback peers; this
/// predicate is the shared definition used by the credential check and its
/// tests. The port is ignored.
#[cfg(any(windows, test))]
pub fn is_loopback_addr(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

#[cfg(any(windows, test))]
fn non_empty_path(value: Option<OsString>) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Resolve the Windows per-user IPC path (port-file location).
///
/// Priority mirrors the Unix `secure_socket_path_from_env` ordering:
/// explicit socket override, then runtime-dir override, then the per-user
/// `%LOCALAPPDATA%\sotf\daemon.sock` default. Pure function for testability;
/// `security::get_secure_socket_path` selects it under `cfg(windows)`.
#[cfg(any(windows, test))]
pub fn windows_socket_path_from_env(
    socket_override: Option<OsString>,
    runtime_dir: Option<OsString>,
    local_app_data: Option<OsString>,
) -> PathBuf {
    if let Some(path) = non_empty_path(socket_override) {
        return path;
    }
    if let Some(path) = non_empty_path(runtime_dir) {
        return path.join("daemon.sock");
    }
    non_empty_path(local_app_data)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
        .join("sotf")
        .join("daemon.sock")
}

/// Atomically publish `port` at `port_file` (Windows).
///
/// Writes to a sibling temporary file and renames it over the destination so
/// readers never observe a torn port number. Refuses to replace directories;
/// a stale symlink is never followed — the rename lands on the link path
/// itself only when the existing entry is a regular file or absent.
#[cfg(windows)]
pub fn write_port_file(port_file: &Path, port: u16) -> std::io::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(port_file)
        && !metadata.is_file()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "IPC port file {} exists and is not a regular file; refusing to replace",
                port_file.display()
            ),
        ));
    }
    let parent = port_file.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "IPC port file {} has no parent directory",
                port_file.display()
            ),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let temp_path = port_file.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0)
    ));
    (|| {
        std::fs::write(&temp_path, encode_port_file_contents(port))?;
        std::fs::rename(&temp_path, port_file)?;
        Ok(())
    })()
    .map_err(|error: std::io::Error| {
        let _ = std::fs::remove_file(&temp_path);
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to publish IPC port file {}: {error}",
                port_file.display()
            ),
        )
    })
}

/// Read a bound port back from `port_file` (Windows).
#[cfg(windows)]
pub fn read_port_file(port_file: &Path) -> std::io::Result<u16> {
    let text = std::fs::read_to_string(port_file).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to read IPC port file {}: {error}",
                port_file.display()
            ),
        )
    })?;
    decode_port_file_contents(&text).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "IPC port file {} holds an invalid port number",
                port_file.display()
            ),
        )
    })
}

/// Bind a loopback TCP listener and publish its port at `port_file` (Windows).
///
/// Stale-port discipline mirrors `bind_unix_socket`: when the port file names
/// a live daemon (connect succeeds) the bind fails with `AddrInUse`; only a
/// dead entry is reclaimed, and only when it is a regular file.
#[cfg(windows)]
pub fn bind_windows_listener(port_file: &Path) -> std::io::Result<IpcListener> {
    if let Ok(port) = read_port_file(port_file)
        && connect_loopback(port).is_ok()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "Another daemon instance is already running",
        ));
    }
    if port_file.exists()
        && !std::fs::symlink_metadata(port_file)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "{} exists and is not a regular file; refusing to remove",
                port_file.display()
            ),
        ));
    }
    if port_file.exists() {
        let _ = std::fs::remove_file(port_file);
    }
    let listener = IpcListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    write_port_file(port_file, port)?;
    Ok(listener)
}

/// Upper bound for a Windows loopback TCP connect, in seconds.
///
/// Loopback connects normally succeed or refuse instantly; the bound only
/// matters when a SYN hangs (stale port file, filtered stack). It mirrors
/// the `IPC_CLIENT_*_TIMEOUT_SECS` discipline so no IPC path blocks forever.
#[cfg(any(windows, test))]
pub const WINDOWS_LOOPBACK_CONNECT_TIMEOUT_SECS: u64 = 5;

/// Connect to `127.0.0.1:port` with [`WINDOWS_LOOPBACK_CONNECT_TIMEOUT_SECS`].
///
/// Shared by the Windows client path and the liveness probes so every
/// loopback dial is bounded. Plain `TcpStream` (identical to `IpcStream` on
/// Windows) so the helper stays testable on any platform; the `any` gate
/// keeps Unix daemon builds free of dead code while tests exercise it.
#[cfg(any(windows, test))]
pub fn connect_loopback(port: u16) -> std::io::Result<std::net::TcpStream> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(
        &addr,
        std::time::Duration::from_secs(WINDOWS_LOOPBACK_CONNECT_TIMEOUT_SECS),
    )
}

/// Connect to the daemon through the Windows port file.
#[cfg(windows)]
pub fn connect_windows(port_file: &Path) -> std::io::Result<IpcStream> {
    let port = read_port_file(port_file)?;
    connect_loopback(port)
}

/// True when `path` currently names a live Windows daemon port file.
#[cfg(windows)]
pub fn windows_port_file_is_live(path: &Path) -> bool {
    read_port_file(path)
        .map(|port| connect_loopback(port).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_file_roundtrip_preserves_valid_ports() {
        for port in [1u16, 80, 443, 8000, 65535] {
            let encoded = encode_port_file_contents(port);
            assert_eq!(decode_port_file_contents(&encoded), Some(port));
        }
    }

    #[test]
    fn port_file_decode_rejects_garbage_and_zero() {
        for text in [
            "", "   \n", "0", "0\n", "abc", "80a", "12 34", "65536", "123456", "-80", "+80", "0x50",
        ] {
            assert_eq!(decode_port_file_contents(text), None, "input: {text:?}");
        }
    }

    #[test]
    fn port_file_decode_trims_surrounding_whitespace() {
        assert_eq!(decode_port_file_contents("  8000\r\n"), Some(8000));
    }

    #[test]
    fn loopback_predicate_accepts_only_loopback() {
        use std::str::FromStr;

        for addr in ["127.0.0.1:1", "127.12.34.56:8000", "[::1]:443"] {
            let parsed = SocketAddr::from_str(addr).expect("valid test address");
            assert!(is_loopback_addr(&parsed), "expected loopback: {addr}");
        }
        for addr in [
            "192.168.1.10:8000",
            "10.0.0.2:80",
            "[::2]:443",
            "[fe80::1]:80",
        ] {
            let parsed = SocketAddr::from_str(addr).expect("valid test address");
            assert!(!is_loopback_addr(&parsed), "expected non-loopback: {addr}");
        }
    }

    #[test]
    fn windows_socket_path_prefers_explicit_override() {
        let path = windows_socket_path_from_env(
            Some(OsString::from("C:\\custom\\daemon.sock")),
            Some(OsString::from("C:\\run")),
            Some(OsString::from("C:\\Users\\u\\AppData\\Local")),
        );
        assert_eq!(path, PathBuf::from("C:\\custom\\daemon.sock"));
    }

    #[test]
    fn windows_socket_path_uses_runtime_dir_next() {
        // Built with `join` on both sides so the priority assertion holds
        // regardless of the native path separator.
        let path = windows_socket_path_from_env(
            None,
            Some(OsString::from("C:\\run")),
            Some(OsString::from("C:\\Users\\u\\AppData\\Local")),
        );
        assert_eq!(path, PathBuf::from("C:\\run").join("daemon.sock"));
        assert_eq!(path.file_name().unwrap(), "daemon.sock");
    }

    #[test]
    fn windows_socket_path_falls_back_to_local_app_data() {
        let path = windows_socket_path_from_env(
            None,
            None,
            Some(OsString::from("C:\\Users\\u\\AppData\\Local")),
        );
        assert_eq!(
            path,
            PathBuf::from("C:\\Users\\u\\AppData\\Local")
                .join("sotf")
                .join("daemon.sock")
        );
    }

    #[test]
    fn loopback_connect_reaches_live_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener addr").port();
        let stream = connect_loopback(port).expect("bounded loopback connect");
        assert_eq!(stream.peer_addr().expect("peer addr").port(), port);
    }

    #[test]
    fn loopback_connect_to_closed_port_fails_fast() {
        // Bind-then-drop yields a port nothing listens on, so the SYN is
        // refused instead of hanging: the call must fail well inside the
        // timeout, proving the bound only caps hangs instead of pacing dials.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind test listener")
            .local_addr()
            .expect("listener addr")
            .port();
        let started = std::time::Instant::now();
        assert!(connect_loopback(port).is_err());
        assert!(
            started.elapsed()
                < std::time::Duration::from_secs(WINDOWS_LOOPBACK_CONNECT_TIMEOUT_SECS),
            "refused connect must not wait out the timeout"
        );
    }

    #[test]
    fn windows_socket_path_ignores_empty_values() {
        let path = windows_socket_path_from_env(
            Some(OsString::from("")),
            Some(OsString::from("")),
            Some(OsString::from("")),
        );
        assert_eq!(
            path,
            PathBuf::from("C:\\ProgramData")
                .join("sotf")
                .join("daemon.sock")
        );
    }
}
