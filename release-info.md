## What is SotF Systemwide?

SotF Systemwide applies SOTF's audio processing to **everything your
computer plays** — browsers, Spotify, games, video calls, media players —
instead of only inside one app. It installs as a virtual audio device: your
apps keep playing to their normal output, their audio is transparently
routed through the SOTF plugin chain, and the processed result is sent to
your real speakers or headphones.

### How it works

```
Apps (browser, Spotify, games, …)
        │
        ▼
SotF Virtual Audio device
        │
        ▼
sotf-daemon (background service)
  · captures the system mix
  · runs your plugin chain (EQ, room correction, dynamics, upmix, …)
  · per-output DSP profiles (each device keeps its own chain)
        │
        ▼
Your speakers / headphones / audio interface
```

- On **macOS**, a CoreAudio HAL driver (`SotFHAL.driver`) presents the
  virtual device and hands audio to the daemon over encrypted shared
  memory (ChaCha20-Poly1305). A menu-bar app (**SotF Systemwide**) controls
  output device, volume, and the plugin chain.
- On **Linux**, the daemon captures through the default input — typically a
  PipeWire/PulseAudio monitor source (`SOTF_CAPTURE_DEVICE` selects
  another endpoint) — and is managed as a systemd user service.
- On **Windows**, the daemon captures a WASAPI endpoint. WASAPI render
  loopback is not exposed, so system-audio capture needs a virtual-cable
  device selected with `SOTF_CAPTURE_DEVICE`.

Highlights:

- Same plugin chain and per-device profiles as the SOTF players: pick a
  different output and its stored EQ/DSP is recalled automatically.
- Feedback-safe by construction: virtual/loopback devices can never be
  selected as the physical output, and if your chosen interface is
  unplugged or powered off, playback falls back to a safe physical
  device (with that device's own profile) until yours comes back.
- Sample-rate aware: if the output interface won't run the configured
  rate, the daemon negotiates one it supports instead of playing at the
  wrong speed.
- Runs as a per-user background service (LaunchAgent on macOS, systemd
  user unit on Linux), starts at login, and is controlled over a local
  JSON IPC socket — no network exposure.

### Downloads

| Platform | File |
| --- | --- |
| macOS (Apple Silicon) | `sotf-systemwide-*-macos-arm64.pkg` |
| Linux x86_64 / ARM64 | `sotf-systemwide-*-linux-*.AppImage`, `sotf-systemwide_*.deb`, or `.tar.gz` |
| Windows x64 / ARM64 | `sotf-systemwide-*-windows-*.zip` (`sotf-daemon.exe`) |

Verify downloads against `SHA256SUMS-*.txt` published with the release.

### Install notes

- **macOS**: run the `.pkg`, then select **SotF Virtual Audio** as the
  system output (System Settings → Sound) and pick your real output
  device in the SotF Systemwide menu-bar app. Logs live in
  `~/Library/Logs/SotF/`.
- **Linux**: install the `.deb` (or unpack the tarball / run the
  AppImage), then `systemctl --user enable --now sotf-daemon.service`.
- **Windows**: unzip and run `sotf-daemon.exe` from a terminal; see the
  bundled `README.txt` for capture-device setup.

SotF Systemwide is part of the [SOTF](https://github.com/pierreaubert/sotf)
project and is licensed under GPL-3.0-or-later.
