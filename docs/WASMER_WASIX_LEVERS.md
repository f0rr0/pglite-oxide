# Wasmer/WASIX Runtime Levers

This is the experiment backlog for making `pglite-oxide` fast and correct on
the WASIX/Wasmer path. Experimental Wasmer and WASIX features are allowed here.
The rule is not "stable only"; the rule is "pinned, reproducible, measured, and
guarded by CI."

## Current Wasmer Baseline

Use the latest stable Wasmer 7 line as the default baseline, with pre-releases
eligible when they contain a specific WASIX or dynamic-linking fix we need.

- Current stable reference: Wasmer `7.1.0`
- Current pre-release reference: Wasmer `7.2.0-alpha.*`
- Current crate target in this repo: Wasmer `7.2.0-alpha.2`

Wasmer 7 is the important line for us because it brings:

- WebAssembly exception support in Cranelift, using system `libunwind`
- first-class WASIX dynamic linking
- LLVM 21 in the Wasmer 7.0/7.1 LLVM backend, with Wasmer `7.2.0-alpha.2`
  already moving to LLVM 22.1
- better large-module compiler scaling in Wasmer 7.1
- rewritten WASIX epoll and TTY work in Wasmer 7.1
- MountFS replacing UnionFS in Wasmer `7.2.0-alpha.2`, with nested mount support
  and changed timestamp behavior
- runtime feature support for caching, profiling, metering, SIMD, relaxed SIMD,
  threads, exceptions, and extended const expressions across relevant backends
- a moving target: `7.2.0-alpha.2` drops WAMR/Wasmi support and the Wasmer CLI
  distribution no longer publishes an `x86_64-darwin` target

Wasmer 6 is useful historical evidence for why `setjmp` / `longjmp` should use
WebAssembly exceptions instead of Asyncify. The production framing should be
Wasmer 7+.

## Non-Negotiable Runtime Invariants

- Build from `electric-sql/postgres-pglite`
  `REL_17_5_WASM-pglite-builder`.
- Add our WASIX path as a maintained `wasix-dl` build personality on top of
  that branch.
- Build PGlite main, extension side modules, and `pg_dump` from the same
  configured source tree and sysroot.
- Use WebAssembly exceptions for Postgres error recovery.
- Keep normal users compiler-free: no local LLVM, no local Cranelift, no Docker.
- Ship target-specific AOT artifacts and load them through headless Wasmer.
- Verify artifact hashes before unsafe Wasmer deserialization.
- Time each startup phase separately before claiming performance wins.
- Keep the normal user dependency graph narrow. If Wasmer requires base
  `wasmer-compiler` metadata for `sys`/headless loading, allow that, but block
  backend compiler crates and any avoidable host-networking dependencies.

## Build And ABI Levers

### WebAssembly Exceptions

Use `-fwasm-exceptions` everywhere: main module, extension side modules, and
tools such as `pg_dump`.

Why: Postgres relies on `sigsetjmp` / `longjmp` style error recovery. Wasmer's
PHP/WordPress work shows that exceptions are the right direction for this class
of C runtime, and Wasmer 7 now supports exceptions in both LLVM and Cranelift.

Check:

- SQL errors recover without corrupting backend state.
- Extension load failures recover cleanly.
- Transaction abort paths still reach `ReadyForQuery`.
- Cranelift and LLVM both pass the same longjmp/error smoke suite.

### WASIX Dynamic Linking

Use Wasmer 7's WASIX dynamic-linking model. Side modules should be built like:

```sh
wasixcc -fwasm-exceptions -fPIC -Wl,-shared ...
```

Why: PostgreSQL SQL extensions are shared libraries loaded at runtime. This is
the core reason to move beyond the previous static WASI path.

Check:

- main and side modules contain `dylink.0`
- side modules import memory/table as expected
- `dlopen`/`dlsym` resolve symbols through the real WASIX dynamic linker
- `RUNPATH` or canonical `/lib/postgresql` lookup works without host hacks
- extension artifacts from different source builds cannot be mixed
- TLS and libc state are not duplicated across main and side modules; the
  dynamic-linking sysroot must use the matching EH/PIC profile
- manifest captures `runtime-path`/RUNPATH, needed modules, imported globals,
  imported functions, and load-order dependencies

### EH + PIC Sysroot

Use the WASIX dynamic-linking sysroot/profile that matches exception handling
plus position-independent code, often described by Wasmer as the `ehpic` style
configuration.

Why: PIC without exceptions is not a valid dynamic-linking configuration in the
current WASIX toolchain model.

Check:

- no accidental fallback to non-DL WASIX libc
- no duplicate libc/static state across main and side modules
- import/export manifests capture the sysroot identity

### Canonical PostgreSQL Paths

Build and package with canonical paths:

- `pkglibdir=/lib/postgresql`
- extension SQL/control files under `/share/postgresql/extension`
- `PGDATA=/base`
- runtime executable under `/bin/pglite`

Why: PostgreSQL extension lookup expects these layouts. Keeping them canonical
reduces host-side patching.

Check:

- `CREATE EXTENSION` works after only copying/linking files into those paths
- extension archives reject paths outside these canonical locations

### Spike Shims That Must Stay Removed

The earlier WIP had host-side compatibility shims that were useful for proving
the WASIX path but are not acceptable as product architecture:

- mirroring `share/postgresql/*` into `share/*`
- mirroring `lib/postgresql/*` into `lib/*`
- writing a minimal `UTC` / `GMT` timezoneset from Rust
- rewriting `timezone = UTC` and `log_timezone = UTC` to `GMT` after PGDATA
  creation
- carrying `emscripten_*` variable names in the WASIX dynamic-linking patch

Those were signs that the asset layout and build personality were still mixed
between older PGlite/Emscripten expectations and canonical PostgreSQL install
paths. The runtime install path now uses canonical PostgreSQL locations directly,
and `xtask assets check --strict-generated` fails if the source or packaged
assets reintroduce those shims.

Production rule:

- package timezone data from the pinned PostgreSQL/PGlite build output or a
  pinned tzdata/zic input
- compile timezone data inside the pinned Docker build using PostgreSQL's
  pinned `src/timezone/data/tzdata.zi`, then copy the compiled directory during
  asset packaging
- do not execute a host-local `zic` during packaging, and do not execute the
  cross-built tree's `zic`; that binary targets WASIX
- generate PGDATA with the desired timezone instead of text-patching config
- keep extension archives in canonical PostgreSQL paths only
- load extensions from `/lib/postgresql` and
  `/share/postgresql/extension` without duplicated fallback locations
- rename reused build-script variables to neutral WASM/WASIX names

CI should keep failing if the production runtime path depends on path mirroring,
minimal timezone file generation, or post-init timezone string replacement.
Portability shims for unsupported Postgres process primitives, such as
single-process `fork()` behavior, are different: they are expected in a WASIX
port when isolated under the maintained `wasix-dl` build personality.

The WASIX host bridge must not return broad fake success for system calls.
Protocol socket behavior is an allowlist for the embedded protocol fd: known
`fcntl` and socket options may be emulated, unsupported options fail with
`ENOPROTOOPT` or `EINVAL`, protocol `connect` fails closed with `ENOSYS`, and
non-protocol descriptors delegate to WASIX libc for frontend tools such as
`pg_dump`.

### Upstream Symbol Pipeline

Reuse the builder branch import/export machinery:

- `pglite-wasm/included.pglite.imports`
- `pglite-wasm/excluded.pglite.imports`
- `wasm-build/getsyms.py`
- `wasm-build/linkimports.sh`
- `wasm-build/linkexport.sh`

Why: extensions such as AGE need hook and `_invoke_*` coverage that is easy to
miss by hand.

Check:

- generated exports are minimal but sufficient
- AGE/vector-style function-pointer paths pass smoke tests
- missing-symbol failures become CI failures with actionable output

## Compiler Backend Levers

### LLVM AOT

Use Wasmer LLVM AOT as the release artifact generator.

Why: Wasmer still positions LLVM as the production/highest-runtime-performance
backend, and Wasmer 7/7.1 improved LLVM for large modules.

Try:

- target-specific AOT for macOS arm64/x64, Linux arm64/x64, Windows x64
- conservative CPU baseline first
- native CPU tuning only as an optional experiment if artifacts remain portable

Check:

- first open with bundled AOT under 5s on GitHub Ubuntu
- warm open under 1s after cache/template work
- vector enable plus first query under 2s
- artifact identity includes Wasmer version, engine id, target triple, and CPU
  baseline

### Cranelift

Keep Cranelift as a serious experimental backend, not as a dismissed fallback.

Why: Wasmer 7 added Cranelift exception support. Cranelift should compile faster
and may be useful for maintainer/dev loops or future user opt-in paths.

Try:

- full direct `SELECT 1` smoke
- SQL error recovery smoke
- `CREATE EXTENSION vector`
- vector insert/query
- server-mode SQLx smoke

Check:

- same correctness suite as LLVM
- much faster local compile time
- acceptable runtime speed for dev profile
- no exception/dynamic-linking regressions on macOS, Linux, and Windows

### macOS Multi-Module Exceptions

Treat macOS as a separate promotion gate for LLVM AOT.

Why: Wasmer has an open issue where multiple LLVM-compiled modules in the same
process can break exception handling on macOS. `pglite-oxide` is exactly a
multi-module workload once extensions are enabled.

Check:

- macOS arm64 and x64 load the main module plus at least two side modules
- SQL error recovery works after each extension has been loaded
- the same suite passes with normal parallel test scheduling, not only
  `--test-threads=1`
- failures are treated as target-support blockers, not documentation footnotes

### Singlepass

Track but do not prioritize for Postgres until exception/dynamic-linking support
matches our needs.

Why: Singlepass is attractive for fast compilation, but Postgres needs the full
exception and dynamic-linking surface.

Check:

- only promote after the same longjmp and extension suite passes

## WebAssembly Feature Levers

### SIMD And Relaxed SIMD

Evaluate SIMD for vector-heavy workloads, especially pgvector distance
operations.

Why: Wasmer 7.1 supports relaxed SIMD in LLVM and Cranelift. pgvector local RAG
queries are one of the main product stories, so vector math is a real workload.

Try:

- build with wasm SIMD enabled where the toolchain supports it
- compare vector insert/query and distance scans with and without SIMD
- inspect generated wasm for SIMD instructions

Check:

- no portability regression across supported targets
- measurable improvement for representative pgvector queries
- no correctness drift in distance operators

### Threads

Evaluate threads only after the single-backend path is stable.

Why: Wasmer's feature matrix supports threads in native backends, and WASIX has
threading primitives, but Postgres/PGlite's embedded model is currently a
single-backend runtime.

Check:

- no requirement for atomics/shared-memory flags that break dynamic linking
- no startup regression
- no unsafe interaction with Postgres process-global state

### Tail Calls, Extended Const Expressions, And Wide Arithmetic

Track these as compiler/runtime capabilities rather than active product
features.

Why: Wasmer 7.1 added or improved these in relevant backends. They may affect
generated code quality or compatibility even if we do not explicitly program
against them.

Check:

- artifact inspection records which features are present
- feature usage is consistent across target artifacts

### V8, JavaScriptCore, And Other Engines

Treat these as exploratory target-specific runtimes.

Why: V8/JSC may matter for iOS/mobile or special embedded environments. Wasmer
`7.2.0-alpha.2` dropped Wasmi and WAMR support, so those are no longer Wasmer
production candidates for this plan unless evaluated as separate non-Wasmer
runtimes.

Check:

- WASIX support level
- dynamic-linking support
- filesystem behavior
- exception behavior
- whether headless/AOT packaging still applies

## AOT And Headless Loading Levers

### Headless Wasmer

Normal user builds should use headless Wasmer to load serialized modules.

Why: headless mode removes compiler backends from user builds and makes loading
more portable.

Check:

- normal `cargo tree` has no `wasmer-compiler-llvm`,
  `wasmer-compiler-cranelift`, `llvm-sys`, or local compiler backend
- normal `cargo tree` has no avoidable host-networking/client dependencies from
  `wasmer-wasix`; if current Wasmer features force them, track that as an
  upstream feature-split item
- unsupported targets fail with a clear missing-AOT-artifact error

### Raw Artifact Cache

Prefer raw serialized artifacts for steady-state loading. Use `.zst` only as a
crates.io package-size workaround, then expand once into the cache.

Why: decompression and repeat hashing are visible startup costs.

Check:

- first expansion is measured separately
- warm path reads the raw cached artifact
- no repeated decompression on warm opens

### Mmap / Zero-Copy Deserialization

Evaluate Wasmer's file deserialization and native engine mmap paths for the
fastest safe loading path available in the chosen Wasmer version.

Why: module load time is a major startup phase once compilation is removed.

Check:

- compare `deserialize_from_file` against mmap/native-engine APIs available in
  the selected Wasmer crate version
- verify SHA before any unsafe deserialization
- keep artifacts immutable and content-addressed

### Process Module Cache

Implement a process-wide module cache keyed by artifact hash.

Why: current `Pglite::preload()` can deserialize and then drop the module. The
intended behavior is to keep the loaded module hot for subsequent opens.

Check:

- `Pglite::preload()` reduces the next `open()`
- repeated temporary opens do not deserialize the same runtime module again
- extension modules are cached by extension artifact hash

### Shared Engine And Runtime

Reuse the Wasmer engine, Tokio runtime, WASIX runtime, and module cache across
database opens where safe.

Why: creating these per `open()` wastes startup time.

Check:

- shared runtime does not leak mutable database state across instances
- side-module cache still resolves against the correct main module
- close/drop semantics remain deterministic

## WASIX Runtime Levers

### PluggableRuntime Shared Cache

Use `PluggableRuntime` with a shared module cache for runtime and extension
side modules.

Why: dynamic extension loading should not recompile or re-resolve the same side
module repeatedly.

Check:

- `CREATE EXTENSION vector` is faster after preload
- `dlopen` uses the seeded/cached module

### Filesystem Strategy

Compare host filesystem mounts, immutable extracted caches, hardlinks, and
copy-on-write layouts.

Why: runtime extraction, PGDATA creation, and extension installation dominate
startup once AOT loading is fast. Wasmer `7.2.0-alpha.2` replaces UnionFS with
MountFS and claims nested mount support, so mount behavior is a version-sensitive
performance and correctness lever.

Try:

- content-addressed runtime cache
- content-addressed extension cache
- hardlink runtime files into instance roots
- copy fallback when hardlink fails
- template PGDATA copy or reflink where available

Check:

- no unsafe archive paths
- temp DB open avoids full runtime extraction
- persistent DB upgrades stay explicit
- nested mounts behave correctly for `/`, `/base`, `/tmp`, `/lib/postgresql`,
  and `/share/postgresql`
- hardlink/reflink strategies are measured against plain copies on each target

### WASIX Journaling / Store Snapshots

Evaluate Wasmer WASIX journaling and `StoreSnapshot`-style APIs even if marked
experimental.

Why: this is the embedded analogue of Wasmer Edge InstaBoot. A restored backend
snapshot may be the largest possible cold-start win.

Experiment:

1. start PGlite against a template root
2. run initdb/startup until `ReadyForQuery`
3. snapshot memory/runtime state
4. restore into a fresh instance root
5. run `SELECT 1`
6. repeat with `CREATE EXTENSION vector`

Correctness risks:

- WAL and file descriptor state
- absolute paths baked into backend memory
- time/random/PID assumptions
- locks and process-local Postgres globals
- persistent database safety after restore

Promotion criteria:

- safe for temporary databases first
- deterministic first query after restore
- no corruption across repeated restore/drop cycles
- clear invalidation key based on asset manifest, PGDATA template hash, enabled
  extensions, Wasmer version, and host target

### Context Switching And Async APIs

Evaluate Wasmer 7's WASIX context switching and experimental async API only
after the single-backend path is stable.

Why: these may matter for concurrent server mode or reducing host blocking, but
they add complexity before basic first-query latency is solved.

Check:

- no regression in direct single-connection mode
- server-mode cancellation and shutdown remain safe

### Asyncify

Do not use Asyncify for Postgres error handling. Keep it only as an isolated
experiment if a specific Wasmer snapshot or journaling path requires it.

Why: Wasmer's own performance story moved `setjmp` / `longjmp` away from
Asyncify and onto WebAssembly exceptions. Postgres should follow the exception
path.

Check:

- any Asyncify experiment is separate from the production artifact
- Asyncify never becomes required for normal extension loading or SQL error
  recovery

### CPU Backoff And Idle Behavior

Evaluate runtime CPU backoff or idle controls for long-running local app/server
mode.

Why: tests care about startup; Tauri/local apps also care about idle CPU.

Check:

- no extra latency on next query after idle
- no background spin in server mode

## Postgres/PGlite Startup Levers

### PGDATA Template Cache

Create a pre-initialized PGDATA template keyed by runtime manifest and init
options.

Why: `initdb` should not run per temporary test database.

Check:

- temporary open clones template instead of running initdb
- template invalidates when runtime/catalog changes
- first query remains correct after clone

### Catalog And Syscache Warmup

Run targeted warmup SQL before creating templates or snapshots.

Why: WordPress gets large wins from OPCache because runtime parsing/compilation
work is already done. The Postgres analogue is not PHP bytecode; it is catalog
state, extension metadata, relation cache/syscache state, and common type I/O
paths.

Try:

- `SELECT 1`
- representative prepared/extended query
- `CREATE EXTENSION vector`
- vector type input/output
- vector distance query
- SQLx/tokio-postgres startup query

Check:

- warmup state survives only where intended
- template files remain safe to clone
- snapshot experiments preserve warmed memory without corrupting database state

### Extension Templates

Create optional templates for common extension sets, especially `vector`.

Why: local RAG/tests should not pay full extension install cost every time.

Check:

- `Pglite::builder().extension(extensions::VECTOR).temporary().open()` reuses a
  vector-enabled template
- extension template key includes extension versions and dependencies

### Startup Config And Preload Extensions

Classify extensions by startup requirements before exposing constants.

Why: extensions such as `pg_stat_statements` need `shared_preload_libraries`;
logical-decoding paths need startup-time GUCs and `pgoutput` symbols. The
upstream PGlite work has open `postgresConfig` / `pgoutput` PRs, so our manifest
cannot treat every extension as just "copy files then CREATE EXTENSION".

Check:

- manifest fields include `requires_preload`, `postgres_config`,
  `shared_memory`, `restart_required`, `dependencies`, and `load_order`
- builder API applies startup config before template creation and backend start
- constants are generated only for extensions whose startup mode has passed
  direct and server-mode smoke tests
- `PgliteBuilder` exposes a small config API only if extension metadata needs it

### Temporary Fast Settings

Consider opt-in temporary-only Postgres settings for test speed.

Examples to evaluate:

- `fsync=off`
- `synchronous_commit=off`
- reduced WAL durability
- smaller buffers where appropriate

Rule: never make unsafe durability settings the default for persistent roots.

Check:

- temporary profile is explicit in docs/API
- persistent profile remains conservative

### Data Directory Locking

Add a persistent-root lock before opening a disk-backed database.

Why: Postgres assumes exclusive control of `PGDATA`, and upstream PGlite has an
open data-directory locking PR after real-world corruption reports.

Check:

- second process/open against the same persistent root fails clearly
- temporary databases do not contend on shared global locks
- lock cleanup handles process crash as well as the host OS allows

### Backend Pooling

Evaluate a small process-local pool for temporary databases or server-mode
tests.

Why: some test frameworks can amortize startup across many tests.

Check:

- reset semantics are stronger than users expect
- no data bleed between tests
- pool is opt-in

### Protocol Path

Keep using the direct Postgres protocol bridge, but measure every step:

- exported startup call
- startup packet
- auth response
- first `ReadyForQuery`
- first simple query
- first extended query with parameters

Why: once module and filesystem costs fall, protocol startup becomes visible.

Protocol correctness gates:

- extended-query errors emit `ReadyForQuery` only at protocol-correct Sync
  boundaries
- server mode handles SSLRequest and CancelRequest messages before normal startup
  packet parsing
- Prisma, SQLx, tokio-postgres, node-postgres, psycopg, and pgx smoke tests do
  not desynchronize after parse/bind errors

## Extension Levers

### Demand-Driven Install

Keep per-instance extension install demand-driven even if asset packs contain
many extensions.

Why: default install can be rich while each database pays only for requested
extensions.

Check:

- no unrequested extension files copied into instance roots
- extension extraction cache is shared

### AOT Side Modules

Precompile side modules as AOT artifacts where Wasmer supports it.

Why: `CREATE EXTENSION` should not compile extension wasm on first use.

Check:

- vector side module loads from AOT
- repeated extension enable reuses module cache
- side-module artifact identity is tied to main runtime identity

### Extension Dependency Graph

Generate dependency and load-order metadata from `.control` files and smoke
results.

Why: packages such as PostGIS or AGE may depend on other SQL extensions and
native libraries.

Check:

- dependency errors are caught at manifest generation time
- public constants are emitted only for passing extensions

## Profiling And Measurement Levers

### Phase Timers

Add structured timings for:

- asset manifest validation
- runtime extraction/cache hit
- PGDATA template clone
- AOT artifact install/decompress/hash
- module deserialization
- WASIX runtime construction
- instance creation
- `_start` / PGlite start exports
- startup packet
- first query
- `CREATE EXTENSION`
- first vector query
- `pg_dump`

Why: aggregate first-query time is not actionable.

### Wasmer Profiling

Use Wasmer profiler support where available:

- CLI `--profiler perfmap`
- Wasmer 7.1 `perf annotate`-style tooling
- `--compiler-debug-dir` for compiler diagnostics
- `--enable-verifier` in compiler/debug CI lanes
- coredumps on traps where useful

Why: if time is inside Wasm execution, Rust-side flamegraphs will not be enough.

### Rust Host Profiling

Use host profiling for the Rust side:

- `cargo flamegraph` or platform profiler
- Linux `perf`
- Instruments on macOS
- tracing spans around startup phases

Why: current WIP likely spends meaningful time in host artifact/cache/runtime
setup.

### Artifact Inspection

Gate artifacts with:

- `wasm-tools objdump` / section inspection
- `wasm-tools strip` or equivalent strip pass
- `dylink.0` presence
- debug/name section checks
- import/export diffing
- size regression checks

Why: build flags silently change performance and dynamic-link behavior.

### Binaryen / wasm-opt

Use `wasm-opt` conservatively and only after correctness is proven without it.

Why: upstream PGlite/Emscripten build notes already showed that aggressive
optimization can corrupt or destabilize output. For WASIX we should first prove
strip-only correctness, then add optimization passes one by one.

Try:

- strip-only baseline
- size-only `-Oz` experiment
- speed-oriented pass experiment for runtime and side modules separately

Check:

- no change in SQL smoke behavior
- no extension load regression
- no longjmp/error recovery regression
- size and startup improvements justify the extra build complexity

### Wasmer CLI Tooling

Use the CLI for isolated repros even if the library path is the product path.

Useful commands/features to exercise:

- `wasmer run --llvm`
- `wasmer run --cranelift`
- `wasmer compile --llvm`
- `wasmer compile --cranelift`
- `wasmer run --profiler perfmap`
- `wasmer run --compiler-debug-dir`
- `wasmer run --enable-verifier`
- `wasmer run --journal`
- `wasmer run --snapshot-on`
- `wasmer run --stack-size`

Why: CLI repros are faster to share upstream when Wasmer/WASIX issues appear.

Check:

- every failing library-path issue has a minimal CLI reproducer when practical
- CLI and embedded Rust behavior do not diverge silently

## Packaging And CI Levers

### Exact Pins

Pin:

- Wasmer crate version
- wasmer CLI/tool version used for AOT
- wasixcc/toolchain version
- wasix-libc/ehpic sysroot identity
- LLVM version
- postgres-pglite commit
- pglite-build commit
- extension repo commits

Why: serialized Wasmer artifacts are not generic wasm files; they are tied to
the engine/toolchain identity.

### Reproducible Builds

Use Wasmer's reproducible distribution support where applicable, including
`WASMER_REPRODUCIBLE_BUILD=1` for Wasmer builds.

Why: asset and AOT crates need reliable hashes and auditability.

### Target Matrix

First-class targets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

Experimental targets:

- Linux musl
- Android
- iOS through V8/JSC/interpreter paths if feasible
- RISC-V after Wasmer 7 target support matures

### Package Size Strategy

Try in order:

1. raw AOT artifact if crate stays under the crates.io limit
2. `.zst` compressed artifact with one-time expansion
3. deterministic split AOT/asset packs

Do not optimize package size by making user startup slow unless no other option
exists.

## Experiment Priority

### P0: Required Before Calling The Path Fast

- switch the source patch onto `REL_17_5_WASM-pglite-builder`
- enforce `-fwasm-exceptions` and WASIX dynamic-linking flags
- implement real process module cache for `Pglite::preload()`
- avoid repeated AOT decompression/hash work on warm opens
- reuse engine/runtime/module cache safely
- implement PGDATA template clone for temporary DBs
- time all startup phases

### P1: Required For Rich Extension DX

- AOT side-module cache for `vector`
- vector-enabled template cache
- Cranelift correctness/perf matrix
- SIMD/relaxed-SIMD pgvector benchmark
- profiler/perfmap integration
- `pg_dump` WASIX runner proof
- smoke-generated extension constants

### P2: High-Upside Experimental Work

- WASIX journaling / store snapshot restore
- context switching / experimental async APIs
- backend pooling for tests
- mobile/runtime alternatives through V8/JSC or separate non-Wasmer runtimes
- target-specific CPU tuning

## References

- [Wasmer 7](https://wasmer.io/posts/wasmer-7)
- [Wasmer 7.1.0 release](https://github.com/wasmerio/wasmer/releases/tag/v7.1.0)
- [Wasmer 7.2.0-alpha.2 release](https://github.com/wasmerio/wasmer/releases/tag/v7.2.0-alpha.2)
- [Wasmer releases](https://github.com/wasmerio/wasmer/releases)
- [Wasmer runtime features](https://docs.wasmer.io/runtime/features/)
- [WASIX dynamic linking](https://wasmer.io/es/posts/dynamic-linking-in-wasm-wasix)
- [Wasmer WordPress case study](https://wasmer.io/posts/how-webassembly-is-powering-wordpress)
- [InstaBoot docs](https://docs.wasmer.io/edge/learn/instaboot/)
- [Wasmer macOS multi-module LLVM exception issue](https://github.com/wasmerio/wasmer/issues/6324)
- [Wasmer embedded/iOS tracking issue](https://github.com/wasmerio/wasmer/issues/6381)
- [Wasmer Rust API docs](https://wasmerio.github.io/wasmer/crates/doc/wasmer/)
