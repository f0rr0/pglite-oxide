# Performance

The default goal is a compiler-free user path:

- CI compiles PGlite and extension side modules with the pinned WASIX toolchain.
- CI precompiles target-specific Wasmer LLVM artifacts.
- Applications load target artifacts through a headless Wasmer path and use
  persistent cache directories for staged native artifacts.

Startup work is Phase 3 of
[WASIX_WASMER_ROADMAP.md](WASIX_WASMER_ROADMAP.md). Snapshot/journaling,
MountFS, mmap/native deserialization, SIMD, Cranelift, and hardlink/reflink/copy
experiments are promoted runtime-architecture work, not optional side notes.

The runtime has four cache layers:

- process cache for loaded modules
- runtime asset cache for extracted immutable files
- template PGDATA cache for temporary databases
- maintainer-only compile cache for `xtask` and CI

The full experiment backlog for Wasmer/WASIX runtime, compiler, AOT, snapshot,
filesystem, extension, and profiling levers lives in
[WASMER_WASIX_LEVERS.md](WASMER_WASIX_LEVERS.md).

## Current WIP Baseline

The current implementation is correctness-first and is not yet at the release
latency target. On a local macOS arm64 development machine, the current
`preload_runtime_then_open_smoke` measurement is:

```text
Pglite::preload()  ~= 19.3s
temporary().open() ~= 10.7s
first query        ~= dominated by open time
```

That means a test that performs first initialization and then immediately runs
its first query should currently expect about 30s in a cold process when it
explicitly calls `Pglite::preload()` before opening. After the AOT/runtime cache
already exists, the first query is still roughly open-time dominated and should
be treated as about 10-12s in this WIP.

These numbers are not acceptable for per-test setup. They document the present
state so we do not confuse the proven architecture with the final developer
experience.

Native AOT artifacts should stay raw for steady-state loads. If a target crate
would exceed crates.io's 10 MB compressed package limit, CI may ship compressed
artifacts and expand them once into the runtime cache before mmap/deserialization.

Initial release gates:

- precompiled first open under 5s on GitHub Ubuntu
- warm open under 1s
- vector enable plus first query under 2s
- no more than 25% regression after the first stable baseline
