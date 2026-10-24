# systemwide

System-wide audio processing subsystem for SOTF. Captures audio from the OS mixer, processes it through the plugin chain, and outputs to physical audio devices.

## Architecture

For a full component review, state-ownership analysis, and Mermaid use-case
diagrams, see [ARCHITECTURE.md](ARCHITECTURE.md).

```text
macOS Audio Apps (Safari, Spotify, ...)
         |
         v
  HAL Driver (virtual audio device, Swift)
         |
    shared memory (/tmp/sotf-{uid}/audio.shm)
         |
         v
  Daemon (Rust, sotf-daemon binary)
    - reads audio via AudioDriver trait
    - runs plugin chain (EQ, upmixer, compressor, ...)
    - writes to output device via cpal
         |
         v
  Physical speakers / headphones
```

The daemon remembers the last physical output device that completed a
successful pipeline transition. On the next cold start it restores that device
before opening playback; `SOTF_OUTPUT_DEVICE` remains the explicit override.
Persisted virtual/loopback device names are ignored to preserve the no-feedback
invariant.

External processes (Swift menubar app, GPUI configbar) control the daemon over a Unix domain socket with a JSON line protocol.
Configbar mutations are serialized off the main thread; its status and
metering polls reuse a reconnecting client connection. A live daemon started by
launchd or a developer is adopted rather than killed and replaced.

The macOS installer registers daemon and Configbar as per-user LaunchAgents.
Runtime logs are `~/Library/Logs/SotF/sotf-daemon.log`,
`~/Library/Logs/SotF/sotf-systemwide.log`, and
`~/Library/Logs/SotF/sotf-systemwide.error.log`; files at least 10 MiB are
rolled to `.1` during upgrades. The final installer component starts Configbar
through the console user's launchd domain and verifies the installed executable
stays running.

Configbar device, channel, sample-rate, and buffer-size changes are submitted as
one generation-checked `apply_configuration` transaction. The daemon validates
the complete requested format and physical output before mutating HAL or the
engine, temporarily ramps active output to mute during replacement, and restores
the complete previous driver format and pipeline if the requested transition
fails.

## Crates

| Crate | Lib name | Purpose |
|---|---|---|
| `driver-common` | `driver_common` | Platform-agnostic `AudioDriver` trait + `NullDriver` fallback |
| `driver-hal` | `driver_hal` | macOS CoreAudio HAL shared-memory bridge (encrypted via ChaCha20-Poly1305) |
| `daemon` | `sotf-daemon` | Background daemon binary; coordinates driver, engine, plugins, IPC |

### Platform support

- **macOS**: Full support via CoreAudio HAL driver (`--features hal`)
- **Linux**: The daemon captures through the cpal-backed driver
  (`CpalCaptureDriver`, bounded drop-oldest queue; `SOTF_CAPTURE_DEVICE`
  selects the endpoint, default input otherwise — typically a PipeWire
  monitor source). `NullDriver` remains the status-only fallback.
- **Windows**: Same cpal capture path (WASAPI capture endpoint). Render-
  endpoint loopback is not exposed via cpal, so system-audio capture needs
  a virtual-cable device selected with `SOTF_CAPTURE_DEVICE`.
- **Fallback**: `NullDriver` compiles everywhere, reports `platform_supported: false`

## Building

```bash
# macOS with HAL support
cargo build -p sotf-daemon --features hal --release

# Any platform (NullDriver fallback)
cargo build -p sotf-daemon --release
```

## Running

Distribution filenames and installer platform restrictions come from the
assembled app, daemon, and required HAL executable slices. ARM64-only payloads
use `macos-arm64`; `macos-universal` requires both ARM64 and x86_64 in every
required executable. Missing or mixed-architecture payloads stop packaging.
The explicit `--no-hal` build option omits HAL; a missing HAL build does not
silently select that option.

```bash
# Start the daemon
cargo run --bin sotf-daemon --features hal --release

# The daemon listens on a Unix socket at:
#   /tmp/sotf-{uid}/daemon.sock  (secure, default)
#   /tmp/autoeq_audio.sock       (legacy, SOTF_LEGACY_SOCKET=1)
```

### Automated tests

`just bench-hal-transport` runs optimized Swift/Rust transport fixtures across
2/8/32 channels, 48/96/192 kHz and 64/128/512 frames, both encrypted and plaintext,
idle and under control churn. It checks every accepted sample and writes
`target/hal-transport-metrics/report.json`, alongside isolated HAL callback
allocation/timing measurements. These host fixtures do not qualify physical
audio deadlines or installed-HAL lifecycle behavior.

`just bench-ipc-control` runs the release lab daemon through fresh-connection
control-load measurements and writes `target/ipc-control-metrics/report.json`
(under `CARGO_TARGET_DIR` when set). It uses private runtime state and keys,
measures ping/snapshot/metering tails, and verifies post-load audio progress.
This also runs in the native macOS/Linux gate; physical audio latency and
Windows authentication load require separate qualification.

Run the committed systemwide contract/component gates from the repository root:

```bash
just test-systemwide-macos
just test-systemwide-linux-arm64

# Run both sequentially, with macOS first.
just test-systemwide-macos-linux-arm64
```

This runs the daemon/state, real Unix-socket IPC, HAL protocol/streaming, and
Configbar model suites. The process-level scenarios start `sotf-daemon` with
`SOTF_SYSTEMWIDE_DRIVER=lab` in isolated temporary runtime directories. They
exercise coherent snapshots, 2 → 10 → 2 channel changes, transactional plugin
artifact rejection, shutdown, and restart without installing or touching the
CoreAudio HAL bundle.
The macOS gate runs the Rust HAL, shared-protocol, daemon, and real local
Unix-socket IPC tests; package-scoped strict Clippy; and Swift type-checks for
the menu-bar client and HAL driver. Its expanded Swift/Rust sequence workload
defaults to the release Rust worker; set `SOTF_HAL_WORKER_PROFILE=debug` for a
separate debug-worker diagnostic run. The Linux recipe builds a pinned test image
and runs the portable HAL streaming guards, shared-protocol tests, daemon
component/IPC tests, and package-scoped strict Clippy natively under
`linux/arm64`. Its Cargo registry, Git checkout, and target caches live in named
Docker volumes; the source tree is mounted read-only.

On native Windows, with Python 3.11+, the Rust MSVC toolchain and Windows SDK,
run from the same sibling-checkout layout:

```powershell
py -3 scripts/test_systemwide_windows.py
```

This entry point uses isolated lab-daemon state, runs the Windows ACL and
loopback-authentication tests, engine regressions, capture-closure and latency
measurements, and strict lint. It writes per-step logs and a source-checked
component report under `target/systemwide-windows-gate`. It refuses other
platforms and Wine before creating output or starting processes. Its success
does not complete two-account ACL, physical capture/output, device-event or
deadline acceptance; these remain explicit pending fields in the report.
Native Windows execution of this new gate has not yet been performed.

Neither gate installs the HAL bundle or changes the system output device. The
ignored installed-HAL tests remain a manual macOS smoke layer. Run
`just systemwide-lab` for the implemented isolated real-daemon scenarios and
HAL/Configbar simulator coverage described in
[ARCHITECTURE.md](ARCHITECTURE.md#debugging-without-installing-and-manual-testing).
Device-loss, sleep/wake, helper-resurrection, and captured-audio scenarios still
require additional simulator and installed-system evidence.

The macOS package reports its active lifecycle step in Installer. Package
script output is also retained at `/Library/Logs/SotF/installer.log`; on a
script failure, a SotF dialog offers to open that log in Console. The system
PackageKit record remains available at `/var/log/install.log`. The
postinstall daemon smoke test polls with per-poll progress lines and fails
fast on confirmed terminal faults. When the previously selected output
device is offline, the daemon falls back to the default-safe output and
keeps the missing selection as the desired device (visible as an
`output_device_fallback` warning in snapshots); power the device on or pick
another output in SotF Systemwide. Only a daemon with no safe output at all
warns without failing the install.

## Runtime safety contract

- Configbar lifecycle probes use daemon `ping`, which does not acquire engine,
  pipeline, driver, or key-manager state locks. Full status and mutation
  requests retain separate bounded deadlines; configuration parsing is
  size-bounded and off the main thread.
- Pipeline-changing IPC commands, automatic playback startup, and
  driver-initiated reconfiguration share one transition lock. Snapshot reads
  take the same lock, and Configbar renders configuration status from
  `get_snapshot`; generation-tagged UI intents are rejected if a newer
  pipeline committed before they execute.
- Daemon device enumeration and RoomEQ output-channel capability checks use a
  short-lived, generation-tagged registry, avoiding repeated synchronous CPAL
  probes during Configbar recovery polling and consecutive configuration loads.
- Daemon `engine_ready` is committed only after the physical output stream
  reports its first hardware callback. Startup failure leaves readiness false
  and enters transactional pipeline recovery.

- When HAL capture resumes after at least thirty seconds idle, the daemon
  transactionally rebuilds the currently applied physical playback stream once.
  Configbar also coalesces overlapping plugin-rack refreshes; stale-generation
  mutations refresh daemon-owned state and require an explicit retry.
- A daemon acquires process-lifetime ownership locks for its canonicalized
  control socket, HAL shared memory, HAL-readable key copy, and daemon-private
  key before construction, key rotation, or stale-socket cleanup. Distinct
  control sockets therefore cannot share and rotate one active transport.
- Runtime parents are checked for ownership and tightened to `0700` when they
  are user-owned; the explicit legacy `/tmp/autoeq_audio.sock` mode is the
  only shared-sticky-directory exception and never uses a symlink.
- SIGINT/SIGTERM performs the same shutdown path as an IPC shutdown: clear HAL
  readiness, stop the engine, close retained client sockets, join every client
  handler and the watcher, and remove only the daemon's verified Unix socket.
- Shared-memory protocol v8 identifies coherent requests and acknowledgements,
  carries actual/restored geometry, and publishes volume/mute atomically. Rust
  and Swift IO paths honor bounded quiescence and preserve interleaved frames;
  the HAL and daemon require matching protocol builds.
- The daemon rotates the shared-audio key once per process lifetime before
  opening the transport. It atomically publishes a daemon-private copy and a
  mode-0600 HAL-readable copy because sandboxed CoreAudio cannot read the
  private path; both remain inside the same-UID trust boundary.
- Audio load requests accept only canonicalized, same-owner, non-symlink
  regular files from 1 byte through 64 GiB and verify device/inode before the
  engine opens them.
- `dump_state` reports metering and pipeline-reload request latency, response
  size, and budget-exceed telemetry for release diagnostics.
- `get_metering` returns a complete cached observation during a pipeline
  transition, with its original generation and `observation` age/availability.
  Before any observation exists, the zero payload is explicitly unavailable
  and its generation is null. Clients should honor those observation fields.
- Rust and Swift access the shared-memory header with matching acquire/release
  atomics and a tested C layout.
- CoreAudio callback staging is preallocated to the maximum supported HAL
  geometry; encrypted IO refuses to allocate on the real-time path.
- The HAL output boundary publishes readiness only after flushing and priming
  one negotiated buffer of silence. Its fixed latency contract is that target
  fill plus the Swift device latency and safety offset; failed priming leaves
  the ring empty and quiesced.
- Compile Swift with `SOTF_AUDIO_TRACE` only for explicit audio-path tracing.

## IPC protocol

JSON-over-Unix-socket, one object per line. Example:

Plugin artifacts may be linear racks or validated DAGs. Engine graph artifacts
use stable node IDs, explicit edges, channel counts, parameters, and bypass:

```json
{"command":"load_plugin_artifact","artifact":{"graph":{
  "nodes":[
    {"id":1,"plugin_type":"gain","parameters":{"gain_db":-3.0},"input_channels":2,"bypassed":false},
    {"id":2,"plugin_type":"eq","parameters":{"filters":[]},"input_channels":2,"bypassed":false}
  ],
  "edges":[{"from_node":1,"to_node":2}]
}}}
```

Graph candidates validate and apply transactionally. Configbar retains the rack
for linear topology and opens its graph editor for DAG topology.

```json
{"command": "load_plugins", "plugins": [
  {"plugin_type": "hal_input",  "parameters": {"channels": 2}},
  {"plugin_type": "eq",         "parameters": {"filters": [{"filter_type": "peak", "frequency": 1000, "q": 1.5, "gain_db": 3.0}]}},
  {"plugin_type": "hal_output", "parameters": {"channels": 2}}
]}
```

## Sub-crate documentation

`just bench-lab-latency` runs the real engine with a paced synthetic capture
source and an inspectable simulated output. It verifies sequence-marked PCM at
2/8/32 input channels, 48/96/192 kHz and 64/128/512 frames with unity and half
gain. The 32-channel cases select 16 outputs. Reports in
`target/lab-latency/report.json` retain raw timestamp pairs, source/binary hashes,
latency quantiles, silence counts and process CPU time. These measurements
include observer overhead and do not qualify hardware latency or deadlines.
Matching `sources-before.json` and `sources-after.json` manifests accompany a
qualified report. Source changes withhold qualification, retain both snapshots
and name the changed paths; a stale qualified report is removed before a run.

This repo contains only the daemon crate; driver crates live in the sibling
`sotf-daw` repo (see `ARCHITECTURE.md` for the full layout):

- [daemon/README.md](crates/daemon/README.md)
- [driver-hal README](../sotf-daw/crates/driver-hal/README.md)
- [daemon/configbar/README_CONFIGURATION.md](crates/daemon/configbar/README_CONFIGURATION.md)
