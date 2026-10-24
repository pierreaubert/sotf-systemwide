# Capture callback measurements

Run `just bench-cpal-capture` from the repository root. The macOS and Linux
aggregate gates run it automatically. Reports default to
`${CARGO_TARGET_DIR:-target}/cpal-callback-metrics/report.json`;
`SOTF_CPAL_METRICS_DIR` overrides the destination. The collector uses Python 3.11+.

The executable imports the production CPAL driver module and invokes the same
typed conversion/queue closure used by `build_input_stream`. It opens no device.
Each case runs 2,000 calls on a fresh Rust thread, including the first call in
the allocation verdict. Cases cover all 12 supported sample types, 2/8/32
channels, 64/512 frames, normal draining, overflow, control-held contention and
a consumer panic leaving a partial frame. Storage, frame drops and sample
values are checked outside the observed window.

The test-only allocator forwards to `System`, matching the daemon. A known
allocation/free must be observed on every worker before a zero count is
accepted. Wall and thread CPU intervals are recorded separately; CPU intervals
include clock, timer and observer overhead. Source hashes, toolchain and active
workspace lock features accompany the reports.

These are measurements of the daemon's closure, excluding the native CPAL
backend/trampoline, foreign C allocator activity, DSP/output callbacks and
physical audio latency. They do not certify hardware deadlines. Windows ARM64
sources compile, but Windows execution requires a native host.

The allocator's memory, zeroing, realloc, saturation and unwind checks can run
without native audio dependencies:

```sh
cargo +nightly miri test --manifest-path crates/callback-metrics/miri/Cargo.toml --lib
```

Neither the observer nor this executable is linked into the daemon. The
installer selects the daemon binary explicitly rather than packaging all
workspace executables.
