# Systemwide release notes

Workspace (pkg) versions and the daemon crate version are independent: the
installer is `0.7.x` while `sotf-daemon` is `0.1.x`. The mapping for each
release is recorded here.

## Unreleased

- Apply owner-only Windows security descriptors when creating runtime directories
  and both IPC rendezvous files. Validate port files before connecting, bound
  reads, and preserve planted staging entries on exclusive-creation failure.
- Add a native Windows lab component gate and a portable capture measurement
  runner with complete-matrix, observer and source-provenance validation.

- Preserve the HAL sequence fixture's synthetic consumer-clock phase across
  scheduler oversleep. Retain partial read/verification/wait diagnostics on
  timeout, with the existing sample workload and liveness bounds unchanged.

- Remove duplicate decoder pacing for explicit capture sources. Add a release
  source-to-sink latency benchmark with exact sample/channel verification and
  raw monotonic timestamps. Preserve the simulated output sample clock when
  its feeder wakes late, with bounded catch-up work.

- Keep the HAL callback benchmark's simulated daemon heartbeat fresh outside
  measured callbacks. Verify that missing/expired heartbeats suppress capture
  and a fresh heartbeat restores it without reopening the mapping.

- Expand Swift/Rust sequence verification to 108 rate/block/channel/encryption
  and load cases, checking every accepted sample across 32 ring lengths per
  case. Require acknowledged control churn and complete geometry reports.

- Keep meter polling responsive during reconfiguration and volume changes by
  returning complete, generation-tagged cached observations with explicit age
  and availability. Add release IPC latency/load measurements to native gates.

- Install the selected driver graph before capture starts. This prevents a
  monitor-only bootstrap/crossfade from briefly leaking the wrong gain during
  graph-profile fallback or restoration. Verify distinct A/B outputs against
  startup PCM and persisted rack/graph profiles through the lab backend.
  Commit driver-source activation before reporting `Playing` to direct callers.

- Verify healthy pipeline reapply against every post-volume lab output sample
  and mute-command count, using a shared-engine observer gated by `test-util`.

- Remove CPAL callback-thread lock-tracking allocations with a control-initialized
  capture mutex. Preserve bounded frame handling and discard poisoned partial
  frames. Add native cold-thread allocation measurements to macOS/Linux gates.

- Extend the release HAL gate to actual audio/timestamp callback allocation,
  free and wall/thread-CPU observations across rates/channel widths. Keep probe
  code out of production bundles and report host deadline outliers explicitly.

- Add reproducible release transport allocation/timing measurements with source
  hashes and concurrent volume/configuration-error churn (`just bench-hal-transport`).

- Reconcile output recovery through typed incidents and explicit callback-proven
  resolution. Reject retired-stream metadata/statistics and withdraw readiness
  while an output incident is active, preserving unrelated fatal failures.
- Coordinated HAL protocol v8 across Rust and Swift: identified request/ACK
  publication, actual negotiated channels, retained ACK retries, and one atomic
  volume/mute tuple with independent control patches. HAL and daemon require a
  matching protocol update.
- Replace file-backed audio mappings with immutable Darwin shared-memory
  objects and authenticated descriptor handoff. Truncating `audio.shm` now
  invalidates its rendezvous without revoking mapped audio pages. Preserve HAL
  cached volume across daemon replacement before readiness is published.
- Fence staged decrypted samples with a ring identity across same-format resets,
  geometry A→B→A and encryption changes. Publish encryption and optional queue
  flush together under the existing IO barrier.
- Fixed live capture resampling to emit DSP blocks within the negotiated frame
  limit. Added shipping-direction Swift/Rust sequence stress for plaintext and
  encrypted audio, and corrected cross-workspace worker paths in the Swift gate.
- Hardened Swift HAL mapping validation, full-ring ownership, cipher
  publication, and timestamp snapshots. Added malformed-mapping, held-reader,
  concurrent key-rotation, and clock-update regressions, including TSan runs.
- Fixed outgoing-profile corruption during route changes and stale startup
  work overwriting a newer Stop intent. Recovery now runs in the supervisor;
  status reads are observational. Failed volume/mute synchronization retries.
- Installer user files and log access run as the console user, and readiness
  detection includes messages emitted before launchctl returns.
- Preallocated the CPAL callback queue, preserved complete interleaved frames
  on overflow, and moved stream-error recovery off the callback. The audit
  ledger records the verification and remaining shared-protocol dependencies.
- Connected explicit capture sources to playback and separated native input
  timing from output processing. Fixed fallback/restoration profile ownership,
  paused resume, forced idle rebuilds and generation-scoped progress tracking.
  Snapshots stay responsive during transitions and identify cached observations.
- Added console-session transport retirement, protected Windows token ACLs,
  bounded authentication workers and readiness-driven IPC acceptance.

- Added an exhaustive startup combination matrix (exchange-file state ×
  crypto-key state × daemon/HAL start order, plus config-negotiation
  round-trips) in driver-hal, and UI-before-daemon / daemon-dies polling
  tests in ConfigBar. The matrix exposed and fixed one wedge: a legacy
  loose-permission exchange file made every daemon start fail; the
  create path now repairs the mode to 0600 (readers still fail closed).

- Startup now verifies the output interface supports the configured
  sample rate (it can differ between devices and change on one device):
  the daemon negotiates to a device-supported rate instead of opening a
  stream whose clock the hardware will not run, which played at the
  wrong speed until the interface was re-selected.
- ConfigBar gains an automated UI test stack: `just test-configbar`
  (real XCTest execution with zero-discovery guard, wired into the macOS
  gate), ViewInspector hosted-view regression tests over a fixture
  daemon socket, image snapshot tests, and `just test-configbar-e2e`
  (lab daemon + real menu-bar binary smoke).

- Fixed the remaining cold-start stall that required switching the audio
  interface once or twice: the driver-config debounce waited for the
  applied geometry to match the HAL's request before applying it, which
  can never happen (the request is consumed once and the geometry only
  changes after the reconfigure), so a 96 kHz startup request from
  coreaudiod was dropped and the engine stayed at 48 kHz. Stability is
  now judged by the request staying unchanged for the debounce window.
- Plugin editor sheets no longer close while a slider is being dragged:
  graph refreshes that echo the editor's own mutation no longer rebuild
  the editor, and rack plugin instances keep their identity across
  refreshes.
- Generic plugin parameter sliders (e.g. Upmixer) use a high-contrast
  thumb that stays visible in light and dark appearances.
- Upmixer/AAE chain rows show the speaker-layout output count
  (2 ch in → 10 ch out) instead of only the stereo input count; the
  channel-compatibility warning now also covers graph nodes, and when
  the selected interface can supply the channels a layout needs, the
  applied output-channel count is raised to match automatically — and
  lowered back when the wide plugin is removed, instead of leaving the
  pipeline running wide on stereo content (which mis-sized the monitor
  taps and distorted playback until a manual reset). Manual channel
  choices are never overridden.
- Outputs & DSP Profiles now stays in sync: the detail pane follows the
  live route when the output is switched from anywhere, the interface name
  in the detail header is a menu for switching interfaces, and virtual
  devices (SotF Virtual Audio and other loopbacks) are no longer offered
  as outputs, where they would create a feedback loop.
- Restored the HAL Input, Audio Output, Outputs & DSP Profiles, and Security
  sections of the configuration window, which were dropped accidentally in
  0.7.15 (only the dead Audio Source selector was meant to be removed), and
  widened the sample-rate segmented control so six daemon-reported rates no
  longer overflow over their label.
- macOS media keys (and the menu-bar/Touch Bar volume sliders) now control
  systemwide volume: the HAL driver exposes volume and mute controls on
  SotF Virtual Audio, so the system volume OSD works instead of showing
  "no volume control", and the daemon applies the OS-visible gain to the
  engine. Volume changes made in the UI publish back to the driver
  control, so keys, sliders, and ConfigBar stay in sync, and the volume
  persists across daemon restarts. The shared-memory protocol moves to
  version 7 for the volume fields (placed in former header padding), so
  the daemon and HAL driver from this release must be installed together;
  mismatched pairings fail closed at startup, as with previous bumps.
- The daemon now listens for CoreAudio property changes on the applied
  output interface. Changing its sample rate in Audio MIDI Setup (or from
  another host) triggers the same debounced, negotiated reconfigure as a
  HAL request instead of playing at the wrong speed until the next manual
  change; device plug/unplug and default-output changes invalidate the
  cached device list immediately. Set `SOTF_DEVICE_LISTENER=0` to disable.
- Fixed the remaining startup sample-rate desync (stored 96 kHz setup,
  interface at 48 kHz): the device-support check above only ran on the IPC
  apply path, so the HAL's startup request restarted the engine at 96 kHz
  on a 48 kHz-max interface and the bad rate was acknowledged back,
  leaving playback desynchronized until the rate was switched manually.
  The driver-config watcher now negotiates against the output device the
  same way and reports the negotiated rate to the HAL.
- Fixed a HAL driver crash that made SotF Virtual Audio disappear from the
  system device list: after a sample-rate re-anchor, the zero-timestamp
  computation converted a negative host-time offset to `UInt64` and
  trapped inside coreaudiod. The timing math is signed end to end now.
- Playback, processing, and decoder threads join the CoreAudio device
  workgroup on macOS, so the daemon keeps realtime scheduling under heavy
  CPU load instead of crackling (opt out with `SOTF_AUDIO_WORKGROUP=0`).
- Lab/sandbox mode no longer fails applies whose saved output device is
  missing: the resolver substituted a physical fallback device that the
  device-free lab sink then rejected. Resolved outputs are now adapted to
  the sink that opens them — the desired device is preserved and the
  divergence reported — in both the IPC apply path and the driver-config
  reconfigure path.
- Daemon tests that enumerate live audio devices or start real engine
  pipelines now run in a serial nextest lane (`.config/nextest.toml`,
  `live-audio` group), so parallel nextest processes no longer flake them
  by contending for device state. Swift sources (ConfigBar and HAL
  driver) are warning-free, and `just lint` type-checks them with
  warnings as errors alongside clippy.

## 0.7.15 (daemon 0.1.38)

### Cold start

- Startup retries now absorb the two transient cold-start races that forced
  users to switch the output interface away and back: the playback-callback
  timeout and the HAL config-ack timeout (budget raised 750 ms -> 3 s).
- ConfigBar's watchdog escalates a reachable-but-latched
  (`pipeline_recovery`) daemon to a restart after two probes instead of
  adopting it as healthy forever.
- Daemon restarts adopt the live HAL header geometry instead of resetting
  the transport to 48 kHz/512/stereo, and header re-init preserves the
  HAL-owned `driver_ready` flag.
- Cold start invalidates the 2 s device-registry cache and re-resolves the
  startup chain when the fallback device changes mid-retry.

### 96 kHz

- Buffer frames scale across rate-only changes so block *duration* stays
  constant (512 frames at 48 kHz becomes 1024 at 96 kHz).
- The HAL ring is sized in seconds on both sides (Rust `ring_block_count`,
  Swift `ringBufferCount(forSampleRate:)`); creation sizes the file for the
  deepest supported rate.
- Encrypted ring overflow drops only the incoming record instead of the
  whole backlog, and `encryption.overflow_count` is surfaced in status.

### Interface

- Removed the Audio Source picker: it mutated local state while the daemon
  has no capture-source concept, and the footer presented it as daemon state.
- Stored-profile parameter edits are no longer discarded; profile rows hide
  the per-plugin channel/bypass controls the profile schema cannot persist.
- Output-route switches get the 30 s pipeline-mutation timeout and a
  generation guard with one conflict retry (previously 1 s + false failure).
- `get_output_profiles` gets the 256 KiB response budget; HAL Driver menu
  status refreshes on the 0.5 s tick; destructive deletes ask first; the
  rate picker offers every daemon-supported rate; the HAL negotiation row
  now receives real `config_status`/`config_source`.

### Plugins

- The daemon installs authorized-runtime sandbox options at startup
  (grants + preset root), matching `sotf-player`.
- `spectrum_analyzer`/`resampler` are stripped from direct rack loads like
  everywhere else; `add_plugin` errors on out-of-range index; available
  plugin defaults are computed at the live driver rate; output profiles are
  validated at upsert and their channel geometry applies on route switch.

### Release mechanics

- Root changelog created (this file); dual versioning documented above.
- `scripts/sign-macos.sh`: desktop/plugin-worker entitlement plists vendored
  into `builds/macos/` with fail-loud checks.
- LaunchAgent plists marked as templates; the postinstall refuses to install
  one still carrying a `__SOTF_` placeholder.
- Security hardening from the 0.7.14 audit cycle (see `SECURITY_REPORT.md`):
  exchange-file permissions, Windows IPC credential, artifact-path policy.

## 0.7.14 and earlier

See `crates/daemon/CHANGELOG.md` for daemon-crate history and git history
for the workspace. Release notes were not kept at the root before 0.7.15.
