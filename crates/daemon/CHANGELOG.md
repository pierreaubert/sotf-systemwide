# 0.1.38 (unreleased)

## Encrypted realtime transport can be enabled

- `set_encryption` with `enabled: true` is honored again on macOS HAL builds:
  the HAL realtime path uses the allocation-free caller-buffer AEAD core, so
  the stale "Encrypted realtime transport is unavailable" rejection is gone.
  Builds without a session cipher still refuse with an explicit error.
- The `SOTF_ENABLE_ALLOCATING_REALTIME_ENCRYPTION` compile guards are removed
  from the HAL shared-memory read/write paths; encrypted frames roundtrip
  through the ring instead of being dropped.

## Plugin picker serves every user-insertable type with canonical categories

- `get_available_plugins` now also serves `ab_compare`, `band_split`, and
  `band_merge`, matching the player pickers; the ConfigBar add menus offer
  43 types instead of 40.
- Plugin categories follow the canonical player grouping (Dynamics, EQ &
  Tone, Denoising, Spatial, Analysis, Routing), replacing the daemon's
  divergent names. ConfigBar's graph-editor menus group by category.
- `loudness_monitor` stays hidden: it is daemon-owned metering, injected at
  both chain boundaries, and user instances are stripped from racks and
  rejected from graphs. `spectrum_analyzer` stays hidden: it is a
  daemon-owned system type with no daemon data path exposing its readings.

## Fresh installs resolve a concrete default output

- Fix the installer smoke failure on machines with no previously selected
  or persisted output device. Resolution used to pass `None` ("engine
  chooses") into the applied spec, so the daemon played through the
  built-in speakers while status reported `applied_output_device: null`
  forever and the postinstall smoke test polled `settling` until it failed
  the install. An unselected output now resolves to the same default-safe
  device used for missing-device fallback (system default when safe,
  otherwise first safe enumerated; virtual devices never qualify) before
  the engine starts, which also gives cold start the real device name for
  DSP-profile recall and sample-rate negotiation.
- If enumeration is still empty at resolve time (e.g. right after the
  installer's CoreAudio restart) the engine starts unresolved as before,
  and the apply commit backfills the applied spec with the safe device
  the engine actually opened — `applied_output_device` is never `null`
  for a playing pipeline.

## Missing output devices fall back to the default output

- Engine starts whose desired output device is not enumerated now degrade to
  the default-safe output (system default when it is a safe physical device,
  otherwise the first safe enumerated device) instead of failing. The desired
  device is preserved, so status shows the divergence and a replugged device
  can be re-selected; the fallback is never persisted over it.
- The fallback plays its own stored DSP chain: cold start recalls the profile
  for the device playback will actually open, so a missing interface's
  correction is never applied to another output.
- Snapshots report an active fallback as an `output_device_fallback` warning
  fault (health `warning`, audio flowing) while streaming; idle divergence
  stays silent. Explicit route changes still validate the device upfront and
  error on missing devices.
- Virtual/loopback devices can never be selected as the fallback: processed
  audio is never routed back into the capture device.
- Installer component order is HAL first, so the driver payload lands and
  CoreAudio restarts before the daemon starts and the smoke test validates
  the final system state.

## Offline output devices no longer hang or fail installs

- Name the missing output device when pipeline start fails while the desired
  device is not enumerated. The engine only reports "Engine thread exited
  unexpectedly" once its manager thread is gone, which bypassed the cold-start
  retry matcher and latched an unactionable fault; the clarified error keeps
  the engine's original message, lists the available outputs, and retries
  while the absence still looks like a late-enumeration race.
- Suggest `select_output_device` in status `recovery_actions` when a latched
  device-availability fault leaves nothing playing, so ConfigBar and the
  installer steer at the powered-off/unplugged device instead of a bare
  daemon restart.
- Clear a stale pipeline-recovery latch when cold start commits to another
  retry attempt, so a terminal latch is never observed mid-retry.
- Installer postinstall smoke test (`scripts/lib/daemon_smoke.py`) streams
  per-poll progress to the install log, fails fast on confirmed terminal
  faults, and lets the install proceed with a loud warning when the daemon
  is healthy but the desired output device is offline.
- Bound every osascript call made under Installer with a timeout so a wedged
  app or headless session fails visibly instead of hanging the install.

## Shared-memory resurrection diagnostics

- Detect when the live Rust or Swift mmap no longer matches the published
  `audio.shm` inode. Daemon status stops reporting stale HAL readiness, and the
  HAL maintenance queue publishes a replacement mapping generation while
  active CoreAudio clients continue running.
- Stop recommending the nonexistent `reset_shared_memory` recovery action for
  every historical underrun; an unavailable transport now uses the supported
  daemon-restart recovery path.
- Reject attempts to enable encrypted realtime transport until the Swift HAL
  has an allocation-free, caller-buffer AEAD implementation.
- Run Configbar launchctl, fallback spawn, terminate, and restart work on one
  serial utility queue so `Process.waitUntilExit()` cannot block AppKit.
- Keep driver-initiated reconfiguration unready until the restarted physical
  output produces its first callback, and retain/join the initial playback
  worker during shutdown.
- Make plugin and diagnostic responses generation-coherent, classify graph
  reorder telemetry by its wire name, and release IPC admission slots on every
  setup failure.
- Mark legacy `status` as `best_effort`; diagnostics and UI use `get_snapshot`.

## Atomic live configuration

- Add a generation-checked `apply_configuration` transaction for Configbar
  device, channel, sample-rate, and buffer-size changes.
- Validate the complete device/format request before teardown, use the engine's
  mute ramp around live stream replacement, and restore the previous HAL timing,
  channel geometry, device, and pipeline after a failed apply. Recovery errors
  return the restored generation so the next Configbar mutation stays current.
- Keep legacy configuration commands response-compatible while routing channel
  and timing changes through the same daemon-owned transaction.

## Transactional systemwide DSP graphs

- `load_plugin_artifact` now accepts validated engine DAG artifacts while
  retaining rack compatibility and rejecting unconverted per-channel schemas.
- Desired/applied state records rack versus graph topology; channel, device,
  and HAL reconfiguration preserve the active artifact kind.
- Configbar selects the existing rack for linear chains or a graph editor for
  nonlinear topology, with stable IDs, connections, settings, transactional
  edits, save/load round trips, and true per-node bypass.

## Review-driven startup and IPC hardening

- Install and manage both daemon and Configbar LaunchAgents, retain their logs
  under `~/Library/Logs/SotF` with bounded upgrade-time rollover, and verify the
  newly installed Configbar is running from the current app bundle before the
  installer reports success.
- Fix the macOS distribution package so the daemon LaunchAgent plist is part
  of the installed app component instead of being staged outside its pkg root.
- Show the release version in the native Installer title, publish descriptive
  lifecycle phases while scripts run, and retain diagnostics in
  `/Library/Logs/SotF/installer.log`. Script failures now offer an **Open Logs**
  dialog action in the console user's session.
- Align the macOS menu-bar pill convention: green while music is playing,
  orange while recording, and transparent/icon-only while idle, disconnected,
  or reporting an error.
- Acquire sorted, deduplicated ownership locks for the control socket, HAL
  shared memory, HAL-readable key, and daemon-private key before startup key
  rotation, preventing split-socket daemons from sharing one transport.
- Track, drain, and join all IPC handler threads during shutdown; stress tests
  cover idle and rapidly reconnecting clients followed by immediate restart.
- Atomically publish both key copies, reject symlink redirection, and keep key
  paths and fingerprints out of normal production logs.
- Validate user audio load paths as bounded, canonical, same-owner regular files
  and verify their device/inode before engine handoff.
- Replace string-matched idle-engine control flow with typed streaming state.
- Add command latency/serialized-response telemetry and explicit metering and
  pipeline-reload diagnostic budgets to `dump_state`.
- Publish the HAL-readable session key through an atomic same-directory
  replacement instead of remove-then-create, closing the local symlink race
  during key rotation and keeping the final file owner/mode verified.
- Include explicit shared-transport encryption state in daemon command/status
  responses; Configbar now surfaces pending or mismatched HAL synchronization.
- Serialize daemon startup with a process-lifetime lock acquired before
  encryption-key rotation or stale-socket handling.
- Make startup key rotation daemon-owned so every process lifetime uses a fresh
  key without desynchronizing `KeyManager` from shared memory.
- Remove the unused Tokio runtime and call the synchronous engine/driver command
  handlers directly from client threads.
- Cache available-plugin descriptors with `OnceLock` instead of rebuilding
  plugin defaults for every discovery request.
- Extend regression coverage for startup ordering, real Unix-socket IPC, and
  Swift atomic shared-memory publication.
- Standardize daemon `driver_status` responses from the shared `DriverStatus`
  shape while retaining legacy aliases, and add a wire-shape regression test.
- Add a checked-in shared-memory ABI manifest consumed by Rust and packaged for
  Swift HAL validation, plus plain/encrypted reconfiguration stress coverage.
- Add a lock-independent `ping` command for Configbar startup and watchdog
  probes, so a healthy daemon is not killed while full status waits behind
  CoreAudio or pipeline locks.
- Keep status and metering IPC replies on a one-second deadline while allowing
  thirty seconds for the bounded start-and-recovery pipeline transaction.
  Config files are size-bounded and parsed off the AppKit main thread.
- Publish `engine_ready=true` only after the physical output stream produces a
  hardware callback; startup errors and the twelve-second readiness deadline
  enter the existing transactional recovery path.
- Bound Configbar IPC responses by command class: 64 KiB normally, 256 KiB for
  snapshots/plugin state/dumps, and 1 MiB only for the plugin catalog.

## Diagnostics and recovery UX (QA-SYS-003)

- Persist the last successfully applied physical output device and restore it before daemon cold-start playback, so interfaces such as EVO8 do not require an away-and-back device toggle after every launch.
- Suppress `SIGPIPE` at every Configbar daemon-socket creation and write. Plugin and RoomEQ configuration mutations now return a recoverable error if the daemon connection closes instead of terminating Configbar.
- Display and edit RoomEQ per-channel EQ parameters (`channel_filters`, `freq`, and `db_gain`) without emitting graph mutations while SwiftUI controls initialize.

- Recover physical output streams that remain callback-active but silent after
  a long idle by rebuilding the applied pipeline once when HAL capture resumes.
- Coalesce Configbar plugin-rack refreshes and replace raw generation-conflict
  errors with an authoritative refresh plus explicit retry guidance.
- Pipeline apply failures now preserve an explicit process-lifetime recovery
  state. If restoring the previous plan also fails, `engine_ready` remains
  false and `status`/`get_snapshot` expose `pipeline_recovery_required` and
  `restart_daemon` instead of reporting a healthy stopped engine.
- Configbar now keeps reachability and optimistic device, volume, and channel
  controls honest: adopted daemons are presented with a reconnect action, the
  outage banner shows retry timing, and rejected mutations restore the last
  confirmed value.
- Added the executable `just systemwide-lab` macOS gate. It combines daemon
  state tests, real Unix-socket lab-driver scenarios, HAL protocol/streaming
  regressions, and Configbar model tests without installing the HAL bundle.
- Process-level lab coverage now verifies coherent snapshots, transactional
  artifact rejection, 2 → 10 → 2 channel changes, transport configuration
  rejection/commit behavior, encryption enablement and key rotation, diagnostic
  dumps, shutdown, and clean restart.
- Encryption enable/rotation requests now return an explicit capability error
  when the daemon has no session cipher instead of reporting success while
  silently leaving encryption disabled.
- `SOTF_SYSTEMWIDE_RUNTIME_DIR` now isolates the daemon-private key and the
  Rust/Swift HAL-readable key copy alongside the lab socket and shared memory;
  focused path tests and the real subprocess scenario reject writes outside the
  temporary lab directory.
- Enriched the daemon `Status` response with explicit systemwide diagnostics:
  - `driver`: `installed`, `ready`, `capture_active`, `frame_size`, `sample_rate`,
    and `channel_count`.
  - `encryption`: `enabled` and `fingerprint`.
  - `active_route`: `desired_output_device`, `applied_output_device`,
    `playback_output_device`, and `capture_active`.
- `recovery_actions`: deterministic list of suggested recovery steps such as
  `reinstall_driver`, `restart_daemon`, `select_output_device`,
  and `rotate_encryption_key`.
- Updated unit tests and the `snapshot_status_response_shape` snapshot to cover
  the new fields.

## Security and IPC hardening (QA-SYS-001)

- Added focused non-device tests for daemon security boundaries:
  - `KeyManager::force_rotate` produces a new session fingerprint and keeps the
    HAL key copy at mode `0600`.
  - Session-key rotation remains daemon-owned across daemon restart and
    driver reconnection, preventing a stale HAL copy from being reused.
  - Peer classification falls back to the restricted `CoreAudioD` class for any
    UID that is not the daemon owner, root, or the macOS `_coreaudiod` user.
  - Socket path construction is deterministic, honors explicit absolute lab
    overrides, and defaults to a user-isolated path under `$TMPDIR`,
    `$XDG_RUNTIME_DIR`, or `/tmp/sotf-{uid}/`.
- Documented that real-device HAL install, reload, recovery, and audio-callback
  timing QA remain release blockers tracked separately under `QA-SYS-002`.

## Daemon startup cleanup

- ConfigBar startup cleanup now asks existing `sotf-daemon` processes to exit
  with an exact-name `TERM` signal instead of using destructive fuzzy
  `pkill -9 -f` matching.
- Added regression coverage to keep the toolbar cleanup path non-forceful and
  exact-name matched.
- Daemon IPC clients now get a bounded idle read timeout, and timeout/`WouldBlock`
  reads close the client instead of letting an idle socket block the client
  handler indefinitely.

## HAL channel capacity

- The Systemwide HAL path now carries the requested input channel count through
  `load_plugins` and daemon-to-HAL configuration, with validation up to 32
  channels.
- The daemon pre-sizes HAL shared memory for 32-channel growth while preserving
  the current advertised channel count, so switching from stereo to immersive
  layouts does not leave the HAL stuck at 2 channels.
- The toolbar input/output channel controls now expose up to 32 channels and
  send the selected HAL input channel count to the daemon.
- The Systemwide configuration meters now keep N-channel meter slots visible
  before analyzer data arrives, the loudness analyzer reports up to 32
  per-channel meter values, and runtime Downmix creation adapts to the current
  chain width so adding Downmix reduces the post-chain monitor to 2 channels.

## Systemwide package upgrade and app identity

- The Systemwide package preinstall now asks a running `sotf-daemon` to shut
  down over its Unix control socket, waits for it to exit, then escalates to
  `TERM` and finally `KILL` only if it is still alive. This avoids replacing the
  daemon while CoreAudio is still using the old process during package upgrades.
- The Systemwide build entry point is now `scripts/build-systemwide.sh`.
- The Systemwide app icon is generated from the configbar SVG artwork instead
  of reusing the GPUI app artwork.

## Toolbar output device and plugin editing

- The toolbar now selects the current system default physical output device
  when CoreAudio reports one, and keeps the previous selection only as a
  fallback.
- Added a refresh button next to the output device picker.
- The output channel picker is now constrained by the selected output
  interface's reported channel count, and refresh/selection changes clamp the
  requested HAL output channels when needed.
- The plugin rack now warns when the plugin chain or a plugin's output layout
  asks for more channels than the selected output device exposes.
- Plugin edit sheets now keep parameter edits in a local draft until `Apply` or
  `Close`; `Cancel` dismisses without applying. The bottom button row now
  provides `Load`, `Save`, `Apply`, `Cancel`, and `Close`, with JSON as the
  default parameter file format and supported JSON shapes available from the
  save dialog.
- Added regression coverage for daemon package quiescing, Systemwide icon
  source selection, output-device channel clamping, default-device selection,
  plugin channel warnings, and batched plugin parameter edits.

Verified:
- `CARGO_TARGET_DIR=/Volumes/home_tmp/tmp/target-sotf cargo test -p sotf-daemon --test daemon_state_tests`
- `swiftc -typecheck crates/systemwide/crates/daemon/configbar/src/*.swift -framework SwiftUI -framework AppKit -framework UserNotifications -framework CoreAudio -framework WebKit`
- `bash -n scripts/build-systemwide.sh scripts/build-dmg-sotf.sh scripts/sign-macos.sh scripts/build-release-local.sh`

# 0.1.36

## Driver HAL streaming configuration

- Driver playback startup now reads the HAL driver's reported sample rate and
  channel count before starting the engine, falling back to 48 kHz stereo only
  when the driver has not reported a format yet.
- Driver reconfiguration now passes the negotiated HAL sample rate into the
  engine restart path instead of restarting through the default 48 kHz HAL
  helper.
- Sample-rate and buffer-frame commands now rely on an acknowledged HAL config
  request; if the HAL does not apply the change, `driver-hal` returns an error
  instead of reporting success optimistically.
- Reconfiguration now preserves the HAL input channel count when available and
  uses the explicit driver-format engine startup helper.
- Added regression coverage through `driver-hal`'s streaming guard tests to
  ensure the negotiated sample rate is wired into `reconfigure_audio_pipeline`.

Verified:
- `cargo test -p sotf-daemon`
- `cargo check -p sotf-daemon -p driver-hal -p sotf-engine --features
  sotf-daemon/hal,sotf-engine/hal`
