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
//!   "socket path" names a port file holding the ASCII port number, plus a
//!   sibling auth file holding a per-bind random credential; clients read
//!   both, connect to `127.0.0.1:port`, and send the credential as the first
//!   line. Loopback-only peering proves locality while the credential proves
//!   owner identity (see `is_loopback_addr`, `authenticate_client`, and
//!   `security::verify_peer_credentials`).
//!
//! Call sites use [`IpcStream`] for per-client handlers on both platforms.
//! The Windows accept loop also uses `IpcListener`.

#[cfg(windows)]
#[path = "windows_acl.rs"]
mod windows_acl;

#[cfg(any(windows, test))]
use std::ffi::OsString;
#[cfg(any(windows, test))]
use std::net::SocketAddr;
#[cfg(any(windows, test))]
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;

/// Connected IPC stream: `UnixStream` on Unix, loopback `TcpStream` on Windows.
#[cfg(unix)]
pub type IpcStream = std::os::unix::net::UnixStream;

/// Connected IPC stream: `UnixStream` on Unix, loopback `TcpStream` on Windows.
#[cfg(windows)]
pub type IpcStream = std::net::TcpStream;

/// Unix IPC listener, shared with readiness polling.
#[cfg(unix)]
pub type IpcListener = std::os::unix::net::UnixListener;

/// Listening IPC socket for the Windows loopback transport.
#[cfg(windows)]
pub type IpcListener = std::net::TcpListener;

/// Wait for a connection or a bounded shutdown-check deadline.
pub fn wait_for_listener(
    listener: &IpcListener,
    timeout: std::time::Duration,
) -> std::io::Result<bool> {
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut descriptor = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live listener descriptor and one writable pollfd; polling
        // neither closes nor transfers ownership of the borrowed descriptor.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            };
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other("IPC listener readiness failed"));
        }
        Ok(descriptor.revents & libc::POLLIN != 0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            POLLERR, POLLHUP, POLLIN, POLLNVAL, WSAGetLastError, WSAPOLLFD, WSAPoll,
        };
        let mut descriptor = WSAPOLLFD {
            fd: listener.as_raw_socket() as usize,
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: one live Winsock listener and one writable WSAPOLLFD; the
        // listener's lifetime covers the call and ownership remains unchanged.
        let result = unsafe { WSAPoll(&mut descriptor, 1, timeout_ms) };
        if result < 0 {
            // SAFETY: reads this thread's last Winsock error after WSAPoll.
            return Err(std::io::Error::from_raw_os_error(unsafe {
                WSAGetLastError()
            }));
        }
        if descriptor.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
            return Err(std::io::Error::other("IPC listener readiness failed"));
        }
        Ok(descriptor.revents & POLLIN != 0)
    }
}

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
///
/// Also generates a fresh [`IpcAuthToken`] per bind and publishes it at the
/// token file beside the port file, so every daemon lifetime has its own
/// client credential and a stale token never authenticates a new listener.
#[cfg(windows)]
pub fn bind_windows_listener(port_file: &Path) -> std::io::Result<(IpcListener, IpcAuthToken)> {
    if let Ok(port) = read_port_file(port_file)
        && connect_loopback(port).is_ok()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "Another daemon instance is already running",
        ));
    }
    let token_file = token_file_for_port_file(port_file);
    for rendezvous_file in [port_file, token_file.as_path()] {
        if rendezvous_file.exists()
            && !std::fs::symlink_metadata(rendezvous_file)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "{} exists and is not a regular file; refusing to remove",
                    rendezvous_file.display()
                ),
            ));
        }
    }
    if port_file.exists() {
        let _ = std::fs::remove_file(port_file);
    }
    if token_file.exists() {
        let _ = std::fs::remove_file(&token_file);
    }
    let listener = IpcListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    write_port_file(port_file, port)?;
    let token = generate_auth_token()?;
    write_token_file(&token_file, &token)?;
    Ok((listener, token))
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
///
/// Reads the auth token published beside the port file and sends it as the
/// first line so the daemon can tell the owning user apart from any other
/// local user able to reach the loopback listener.
#[cfg(windows)]
pub fn connect_windows(port_file: &Path) -> std::io::Result<IpcStream> {
    let port = read_port_file(port_file)?;
    let token = read_token_file(&token_file_for_port_file(port_file))?;
    let mut stream = connect_loopback(port)?;
    {
        use std::io::Write;
        stream.write_all(encode_auth_token_file_contents(&token).as_bytes())?;
        stream.flush()?;
    }
    Ok(stream)
}

/// Bound for one Windows IPC authentication exchange, in seconds.
///
/// The daemon applies this as a deadline over the whole token line rather
/// than per read so scanners holding connections open cannot pin handler
/// threads. It mirrors the `IPC_CLIENT_*_TIMEOUT_SECS` discipline so no IPC
/// path blocks forever.
#[cfg(windows)]
pub const WINDOWS_AUTH_TIMEOUT_SECS: u64 = 5;

/// Length of the Windows IPC auth token in bytes.
///
/// 128 bits from the OS RNG is far beyond guessing over loopback, and the
/// token file itself lives in the per-user directory so only the owning user
/// can read it. Together they bind "can reach the listener" back to "owns
/// the daemon", which loopback alone cannot express on a multi-user host.
#[cfg(any(windows, test))]
pub const IPC_AUTH_TOKEN_BYTES: usize = 32;

/// Upper bound for one hex-encoded auth line: 64 hex digits plus a
/// trailing newline, with headroom. Anything longer is rejected before it can
/// grow an allocation.
#[cfg(windows)]
pub const IPC_AUTH_TOKEN_LINE_MAX_BYTES: usize = 80;

/// Auth token proving a Windows IPC client runs as the daemon owner.
///
/// Loopback TCP has no `SO_PEERCRED` equivalent, so any local user can open
/// the socket. The daemon publishes a fresh random token per listener bind
/// in a user-private file next to the port file; clients send it back as the
/// first line and the daemon drops connections that do not.
#[cfg(any(windows, test))]
#[derive(Clone)]
pub struct IpcAuthToken([u8; IPC_AUTH_TOKEN_BYTES]);

#[cfg(any(windows, test))]
impl IpcAuthToken {
    /// Wrap raw token bytes (parsed or generated, never logged).
    pub fn from_bytes(bytes: [u8; IPC_AUTH_TOKEN_BYTES]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes for file publication or comparison.
    pub fn bytes(&self) -> &[u8; IPC_AUTH_TOKEN_BYTES] {
        &self.0
    }
}

#[cfg(any(windows, test))]
impl std::fmt::Debug for IpcAuthToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a credential: a Debug leak would print it into logs.
        write!(formatter, "IpcAuthToken(...)")
    }
}

/// Constant-time token equality so a wrong guess reveals nothing about how
/// close it was. Lengths are fixed, so only the byte loop matters.
#[cfg(any(windows, test))]
pub fn auth_tokens_equal(left: &IpcAuthToken, right: &IpcAuthToken) -> bool {
    let mut difference = 0u8;
    for (a, b) in left.0.iter().zip(right.0.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Generate a fresh auth token from the OS RNG.
#[cfg(any(windows, test))]
pub fn generate_auth_token() -> std::io::Result<IpcAuthToken> {
    use rand::TryRng;
    let mut bytes = [0u8; IPC_AUTH_TOKEN_BYTES];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| std::io::Error::other("OS RNG failed while generating IPC auth token"))?;
    Ok(IpcAuthToken(bytes))
}

#[cfg(any(windows, test))]
const TOKEN_HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Encode a token for its file: lowercase hex with one trailing newline, so
/// it stays human-inspectable like the adjacent port file.
#[cfg(any(windows, test))]
pub fn encode_auth_token_file_contents(token: &IpcAuthToken) -> String {
    let mut text = String::with_capacity(IPC_AUTH_TOKEN_BYTES * 2 + 1);
    for byte in token.0.iter() {
        text.push(TOKEN_HEX_DIGITS[(byte >> 4) as usize] as char);
        text.push(TOKEN_HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    text.push('\n');
    text
}

/// Decode token-file contents back to a token.
///
/// Returns `None` for empty input, wrong length, or non-hex digits. Pure
/// function so daemon, clients, and tests share the exact acceptance rule.
#[cfg(any(windows, test))]
pub fn decode_auth_token_file_contents(text: &str) -> Option<IpcAuthToken> {
    let trimmed = text.trim();
    if trimmed.len() != IPC_AUTH_TOKEN_BYTES * 2 {
        return None;
    }
    if !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    // The length check above guarantees an even split, so the remainder is
    // always empty; the debug assertion pins that coupling.
    let (pairs, remainder) = trimmed.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    let mut bytes = [0u8; IPC_AUTH_TOKEN_BYTES];
    for (index, chunk) in pairs.iter().enumerate() {
        bytes[index] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(IpcAuthToken(bytes))
}

/// Token-file location for a Windows port file.
///
/// `daemon.sock` publishes `daemon.token` beside it so one directory holds
/// the whole loopback rendezvous and stale cleanup removes both together.
#[cfg(any(windows, test))]
pub fn token_file_for_port_file(port_file: &Path) -> PathBuf {
    port_file.with_extension("token")
}

/// Atomically publish `token` at `token_file` (Windows).
///
/// Same discipline as [`write_port_file`]: a sibling temporary file renamed
/// over the destination, never following a stale symlink, refusing to
/// replace non-regular files.
#[cfg(windows)]
pub fn write_token_file(token_file: &Path, token: &IpcAuthToken) -> std::io::Result<()> {
    use std::io::Write;
    if let Ok(metadata) = std::fs::symlink_metadata(token_file)
        && !metadata.is_file()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "IPC token file {} exists and is not a regular file; refusing to replace",
                token_file.display()
            ),
        ));
    }
    let parent = token_file.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "IPC token file {} has no parent directory",
                token_file.display()
            ),
        )
    })?;
    let acl = windows_acl::PrivateAcl::new()?;
    let _directory = acl.secure_directory(parent)?;
    let temp_path = token_file.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0)
    ));
    (|| {
        let mut file = acl.create_file(&temp_path)?;
        file.write_all(encode_auth_token_file_contents(token).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp_path, token_file)?;
        Ok(())
    })()
    .map_err(|error: std::io::Error| {
        let _ = std::fs::remove_file(&temp_path);
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to publish IPC token file {}: {error}",
                token_file.display()
            ),
        )
    })
}

/// Read a auth token back from `token_file` (Windows).
#[cfg(windows)]
pub fn read_token_file(token_file: &Path) -> std::io::Result<IpcAuthToken> {
    use std::io::Read;
    let acl = windows_acl::PrivateAcl::new()?;
    let mut text = String::new();
    acl.open_file(token_file)?
        .take(IPC_AUTH_TOKEN_LINE_MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to read IPC token file {}: {error}",
                    token_file.display()
                ),
            )
        })?;
    decode_auth_token_file_contents(&text).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "IPC token file {} holds an invalid auth token",
                token_file.display()
            ),
        )
    })
}

/// Verify the auth token opening a Windows IPC connection.
///
/// Reads exactly one bounded line under a whole-exchange deadline and
/// constant-time compares it against `expected`. Every failure — timeout,
/// EOF, overlong line, undecodable token, mismatch — maps to one static
/// error so callers and logs never echo a guess or distinguish why it was
/// wrong.
#[cfg(windows)]
pub fn authenticate_client(
    mut stream: &std::net::TcpStream,
    expected: &IpcAuthToken,
) -> std::io::Result<()> {
    use std::io::Read;

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(WINDOWS_AUTH_TIMEOUT_SECS);
    let mut line = Vec::with_capacity(IPC_AUTH_TOKEN_LINE_MAX_BYTES);
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(auth_failed());
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut byte) {
            Ok(0) => return Err(auth_failed()),
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() > IPC_AUTH_TOKEN_LINE_MAX_BYTES {
                    return Err(auth_failed());
                }
            }
            Err(error) => return Err(auth_failed_with_kind(error)),
        }
    }
    let presented = String::from_utf8(line).map_err(|_| auth_failed())?;
    let presented = decode_auth_token_file_contents(&presented).ok_or_else(auth_failed)?;
    if auth_tokens_equal(&presented, expected) {
        Ok(())
    } else {
        Err(auth_failed())
    }
}

/// Static authentication failure: never echoes the presented guess.
#[cfg(windows)]
fn auth_failed() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "IPC client authentication failed",
    )
}

/// Map a transport error to the static failure while preserving its kind so
/// timeouts stay distinguishable from refusals for logging.
#[cfg(windows)]
fn auth_failed_with_kind(error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), "IPC client authentication failed")
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
    fn listener_wait_observes_connections_and_clears_after_accept() {
        #[cfg(unix)]
        let directory = tempfile::tempdir().expect("private endpoint");
        #[cfg(unix)]
        let path = directory.path().join("poll.sock");
        #[cfg(unix)]
        let listener = IpcListener::bind(&path).expect("bind IPC listener");
        #[cfg(windows)]
        let listener =
            IpcListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        assert!(!wait_for_listener(&listener, std::time::Duration::ZERO).unwrap());
        std::thread::scope(|scope| {
            let waiting =
                scope.spawn(|| wait_for_listener(&listener, std::time::Duration::from_secs(5)));
            #[cfg(unix)]
            let _client = IpcStream::connect(&path).expect("connect while waiting");
            #[cfg(windows)]
            let _client =
                IpcStream::connect(listener.local_addr().unwrap()).expect("connect while waiting");
            assert!(waiting.join().expect("poll worker").expect("listener wake"));
            let _accepted = listener.accept().expect("ready connection");
            assert!(!wait_for_listener(&listener, std::time::Duration::ZERO).unwrap());
        });
    }

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
    fn auth_token_file_roundtrip_preserves_token() {
        let token = generate_auth_token().expect("OS RNG must work in tests");
        let encoded = encode_auth_token_file_contents(&token);
        let decoded = decode_auth_token_file_contents(&encoded).expect("encoded token must decode");
        assert!(auth_tokens_equal(&token, &decoded));
    }

    #[test]
    fn auth_token_decode_rejects_garbage() {
        let token = generate_auth_token().expect("OS RNG must work in tests");
        let valid = encode_auth_token_file_contents(&token);
        assert!(decode_auth_token_file_contents("").is_none());
        assert!(decode_auth_token_file_contents("   \n").is_none());
        assert!(decode_auth_token_file_contents("zz").is_none());
        assert!(decode_auth_token_file_contents(&"zz".repeat(IPC_AUTH_TOKEN_BYTES)).is_none());
        assert!(
            decode_auth_token_file_contents(&valid[..valid.len() - 2]).is_none(),
            "truncated credential must not decode"
        );
        let mut overlong = valid.clone();
        overlong.push_str("00");
        assert!(
            decode_auth_token_file_contents(&overlong).is_none(),
            "overlong credential must not decode"
        );
    }

    #[test]
    fn auth_token_equality_detects_single_bit_flip() {
        let token = generate_auth_token().expect("OS RNG must work in tests");
        assert!(auth_tokens_equal(&token, &token));
        let mut bytes = *token.bytes();
        bytes[0] ^= 1;
        assert!(
            !auth_tokens_equal(&token, &IpcAuthToken::from_bytes(bytes)),
            "a one-bit guess must not authenticate"
        );
    }

    #[test]
    fn auth_token_debug_never_leaks_credential() {
        let token = generate_auth_token().expect("OS RNG must work in tests");
        let rendered = format!("{token:?}");
        assert!(rendered.contains("IpcAuthToken"));
        let hex = encode_auth_token_file_contents(&token);
        assert!(
            !rendered.contains(hex.trim()),
            "Debug must not print the credential"
        );
    }

    #[test]
    fn token_file_sits_beside_port_file() {
        let port = PathBuf::from("/tmp/sotf-auth-test/daemon.sock");
        assert_eq!(
            token_file_for_port_file(&port),
            PathBuf::from("/tmp/sotf-auth-test/daemon.token")
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
