# To Check

Temporary working notes for decisions and measurements that should be verified
before the WASIX/Wasmer path is treated as production-ready.

## First Query Latency

Current WIP measurement from `preload_runtime_then_open_smoke` on local macOS
arm64 after clean-template packaging:

```text
Pglite::preload()  ~= 6.5s
temporary().open() ~= 4.7s
first query        ~= dominated by open time
```

Interpretation:

- cold process with explicit preload plus first open/query is roughly 11s today
- warm-ish cached first query is still roughly 4-6s today
- this validates the architecture path, not the desired developer experience

What to check next:

- measure on GitHub Ubuntu with bundled AOT artifacts
- separate AOT deserialization, runtime extraction, PGDATA/template creation,
  backend startup, and first protocol query timings
- prove template PGDATA and raw AOT cache reduce warm open to under 1s
- keep release gates at first open under 5s, warm open under 1s, and vector
  enable plus first query under 2s
- work through the lever backlog in
  `docs/WASMER_WASIX_LEVERS.md`

## Package And Publish Checks

Current local package-size results:

- `pglite-oxide-0.3.0.crate`: 6.8 MiB compressed
- `pglite-oxide-assets-0.3.0.crate`: 6.1 MiB compressed
- `pglite-oxide-aot-aarch64-apple-darwin-0.3.0.crate`: 6.3 MiB compressed
- placeholder AOT crates for other targets are about 1.3 KiB until their native
  AOT artifacts are generated
- `cargo run -p xtask -- package-size --enforce` passes locally after excluding
  production source checkouts from the root crate package

Current publish checks:

- `cargo publish -p pglite-oxide-assets --dry-run --locked --allow-dirty`
  passes
- `cargo publish -p pglite-oxide-aot-aarch64-apple-darwin --dry-run --locked
  --allow-dirty` passes
- root `pglite-oxide` packaging still requires the internal asset/AOT package
  names to exist on crates.io first; that is a release-management task, not an
  asset-size failure

## Postgres/PGlite Source Baseline

Use `electric-sql/postgres-pglite` branch
`REL_17_5_WASM-pglite-builder` as the upstream Postgres/PGlite foundation.

Reason:

- it has the current PGlite backend lifecycle shape: `pgl_initdb`,
  `pgl_backend`, `pgl_shutdown`
- it has initdb, boot, and single-user backend handling
- it has the current protocol-loop structure in `interactive_one.c` and
  `pg_proto.c`
- it has the extension source catalog and packaging layout
- it has symbol import/export discovery for dynamically loaded extensions
- it has hook/export learnings for extensions such as AGE and vector

Layering decision:

- keep that branch's PGlite lifecycle, protocol loop, extension catalog, and
  symbol/export machinery as upstream-aligned input
- port our WASIX dynamic-linking work onto that branch as a maintained
  `wasix-dl` build personality
- build the main module, extension side modules, and `pg_dump` from the same
  configured tree
- keep Rust responsible for Wasmer/WASIX execution, verified AOT loading,
  cache layout, asset packaging, and public API

What to check next:

- run `xtask assets release-build` from a clean checkout in CI for every
  supported target
- continue preserving the builder branch's PGlite C entrypoints instead of
  carrying a separate runtime fork
- audit/cherry-pick newer PGlite fixes that landed outside the builder branch:
  checkpointer disable, background-worker disable, default `postgres`
  user/database, imported memory sizing, artifact download/cache fixes, and
  data-directory locking
- port or intentionally reject upstream startup `postgresConfig` / `pgoutput`
  work before claiming support for preload or logical-decoding style extensions
- verify `vector` and at least one contrib extension as WASIX side modules
- verify `pg_dump` from the same source/configured tree before exposing public
  dump APIs

## pglite-bindings Startup Comparison

`pglite-bindings` is useful because it shows the C lifecycle shape without the
JavaScript package loader:

- instantiate the WASI module and run `_start`
- call `pgl_initdb()`
- apply config through the PGlite configuration surface
- call `pgl_backend()`
- process wire or REPL input through `interactive_one()`
- use `clear_error()` only as the C-side recovery boundary after a trapped
  backend error

What we aligned now:

- the Rust host no longer probes the JS/Emscripten-era `pgl_startPGlite` or
  `pgl_setPGliteActive` exports
- startup is `_start` plus explicit PGlite lifecycle/protocol exports
- the Rust host separates the upstream lifecycle ABI from WASIX protocol ABI in
  code: `PgliteLifecycleExports` contains only `pgl_initdb` and `pgl_backend`,
  while `WasixProtocolExports` contains `ProcessStartupPacket`,
  `PostgresMainLoopOnce`, `PostgresRecoverProtocolError`, and related direct
  protocol helpers
- `xtask assets check` now guards this separation so the Rust host cannot drift
  back to a generic export bucket or depend on JS/Emscripten lifecycle probes

What still needs source-spine work:

- decide whether Rust should keep the current `ProcessStartupPacket` bridge or
  move startup packet handling under `interactive_one()` like `pglite-bindings`
- keep config changes flowing through a proper config API or pinned
  initdb/template generation; do not reintroduce host-side string rewrites
- keep runtime prefix files packaged from the pinned configured tree, including
  real timezone data, without host-side mirroring after extraction

Current status:

- `cargo run -p xtask -- assets source-spine --check-patch-applies` now passes
  against `REL_17_5_WASM-pglite-builder`
- `cargo run -p xtask -- assets release-build --profile release --target-triple
  <target>` exists as the one-command release build path; CI still needs to run
  it from a clean checkout on each supported native target
- `cargo run -p xtask -- assets build --profile release --target-triple
  aarch64-apple-darwin --execute` now produces the main PGlite WASIX module,
  `vector`, `pg_trgm`, and `pg_dump` from the pinned builder-branch source spine
- `cargo run -p xtask -- assets package --target-triple aarch64-apple-darwin`
  now packages the runtime, `vector`, `pg_trgm`, `pg_dump`, support side modules,
  a clean PGDATA template, and local AOT artifacts
- `cargo run -p xtask -- assets aot --target-triple aarch64-apple-darwin` now
  regenerates the local Wasmer LLVM AOT artifacts from the built main/support/
  extension modules
- `cargo run -p xtask -- assets check --strict-generated` now passes against
  the regenerated manifest/source pins and canonical asset-layout guard
- `cargo run -p xtask -- package-size --enforce` now passes with root,
  assets, and macOS arm64 AOT crates below crates.io's 10 MiB compressed limit

Current audit commands:

- `cargo run -p xtask -- assets audit-upstream` shows which builder-branch and
  stable-branch fixes are included
- required stable-branch items are now decision-complete in strict mode:
  protocol exports/startup behavior, imported-memory behavior, and default
  `postgres` identity are replaced by explicit WASIX behavior plus tests/guards;
  checkpointer disable is ported into the maintained `wasix-dl` patch
- `cargo run -p xtask -- assets check --strict-generated` validates generated
  source pins against `assets/sources.toml`

See `docs/UPSTREAM_AUDIT.md` for the maintained audit.

## `pglite-build` Comparison

Current conclusion:

- `pglite-build` branch `portable` is pinned as build evidence, not as a second
  runtime source tree
- `REL_17_5_WASM-pglite-builder` already contains the important `pglite-build`
  patch results and shared build scripts
- `wasm-build/build-ext.sh`, `build-pgcore.sh`, `extension.sh`, `getsyms.py`,
  `linkimports.sh`, `pack_extension.py`, and `reqsym.py` are identical across
  the two local checkouts
- the remaining differences are not missing WASIX runtime work:
  `pglite-build` has an extra Docker wrapper and older Emscripten SDK patching;
  the builder branch has a newer include tree and cleaner `sdk_port-wasi.c`

Cleanup decision:

- keep one active source root: the configured `postgres-pglite` builder branch
- use `pglite-build` to audit provenance and drift only
- port the upstream install-delta packaging and `wasm-objdump` import/export
  extraction ideas into `xtask`
- do not use upstream `pack_extension.py` directly for published crates because
  it produces non-deterministic `.tar` files, mutates `PGROOT`, and preserves
  non-canonical `/lib/*` paths

Current guard:

- `cargo run -p xtask -- assets source-spine --check-patch-applies` checks the
  pinned `pglite-build` checkout, verifies the shared scripts match the builder
  branch, and validates source markers for dynamic-linking and extension support

What to check next:

- replace the remaining hand-maintained `wasix-dl` export list with a generated
  list based on upstream `getsyms.py` / `linkimports.sh` plus the explicit Rust
  bridge ABI exports
- make `xtask` package extensions from install deltas and captured imports rather
  than hard-coded artifact paths
- add manifest fields for extension import lists and core export lists so dynamic
  linking failures are diagnosed before runtime startup

## Protocol Error Fidelity

Current WIP behavior:

- direct Rust extended-query calls now batch `Parse + Describe + Sync` and
  `Bind + Describe + Execute + Sync`
- this prevents the WASIX backend from panicking on a missing-table Parse error
  and keeps the backend usable after the error
- the WASIX bridge now has an explicit `PostgresRecoverProtocolError` export
  that is called only when `PostgresMainLoopOnce` unwinds to Wasmer as a trap
- recovery still uses PostgreSQL's live `ErrorData` and `EmitErrorReport()`;
  Rust does not synthesize SQLSTATEs or parse stderr
- direct Rust syntax errors now downcast to `PgliteError` with SQLSTATE `42601`
  and direct Rust extended missing-table errors downcast to `PgliteError` with
  SQLSTATE `42P01`
- SQLx server-mode Parse, Bind, and Execute errors preserve SQLSTATE and recover
  after Sync
- `tokio-postgres` server-mode Parse and Execute errors preserve SQLSTATE and
  recover after Sync; Bind errors that normal clients pre-validate are covered
  by the raw wire-protocol test instead
- raw wire-protocol Bind errors emit `ErrorResponse`, skip `BindComplete`, and
  recover after `ReadyForQuery`
- vector-loaded direct and SQLx server paths preserve SQLSTATE for
  extension-originated invalid-input and dimension errors and recover afterward

Interpretation:

- this is the right architectural boundary for the current Wasmer/WASIX path:
  the host owns trap observation, while C owns PostgreSQL error formatting and
  transaction/protocol cleanup
- it is not a Rust-side monkey patch because no error fields are re-created in
  Rust; the original frontend `ErrorResponse` bytes are emitted by Postgres
- it remains a maintained ABI contract, so stale artifacts must fail export
  loading rather than silently falling back to `clear_error` or a host-side
  generic parse error

What to check next:

- broaden extension-specific error tests through the generated extension suite,
  especially load-order and missing native dependency failures
- add Python/Go/Node proxy examples that verify the same recovery behavior from
  non-Rust client libraries
- keep the existing guard that treats a missing `ParseComplete` as an error
  because it is still a useful invariant for corrupted protocol output
- add CI/export checks that require `PostgresRecoverProtocolError`,
  `PostgresMainLoopOnce`, `PostgresSendReadyForQueryIfNecessary`, and the
  `pgl_wasix_input_*` / `pgl_wasix_output_*` bridge functions
- keep monitoring Wasmer exception behavior: if a future version resumes the C
  `sigsetjmp` boundary directly, the explicit recovery export should still pass
  as a no-op fallback path but should not be removed without test evidence

Implementation notes:

- `PGLITE_WASIX_DL` must be in `PGLITE_CFLAGS`, not only `CPPFLAGS`, because the
  upstream PGlite `pglite-wasm/pg_main.c` custom Makefile rule compiles that
  translation unit with `$(CFLAGS)` directly
- the old `longjmp`/`siglongjmp` macro remapping was removed from the PGlite
  CFLAGS; the runtime should use native WASIX/LLVM exception support plus the
  explicit recovery ABI instead of out-of-line jump wrappers
- the unused `pgl_longjmp` and `pgl_siglongjmp` helpers were removed from the
  bridge source; the next artifact rebuild should verify those symbols are gone
  from the shipped main module

## Current Wasmer/WASIX Gaps

Fresh research against Wasmer `7.1.0` and `7.2.0-alpha.2` surfaced a few
runtime assumptions that need explicit checks:

- `wasmer-wasix` default features currently pull a broad host surface including
  host networking, `reqwest`, journaling, Ctrl-C handling, and thread helpers
- `wasmer` headless loading still brings the base `wasmer-compiler` crate through
  `sys`; the invariant should block backend compilers such as LLVM/Cranelift in
  normal user builds, not the base compiler metadata crate if Wasmer requires it
- Wasmer has an open macOS multi-module LLVM exception-handling issue; our main
  module plus extension side modules must pass macOS arm64/x64 parallel tests
- Wasmer `7.2.0-alpha.2` moves WASIX filesystem internals from UnionFS to
  MountFS and changes mount timestamp behavior, so cache and filesystem tests
  should pin the exact Wasmer version and rerun on upgrades
- Wasmer `7.2.0-alpha.2` drops WAMR/Wasmi support and drops the distributed
  `x86_64-darwin` target, so those should not be treated as first-class runtime
  targets without direct crate-level proof

What to check next:

- try `wasmer = { default-features = false, features = ["sys", "headless",
  "wasmer-artifact-load"] }` without the explicit `compiler` feature
- try `wasmer-wasix = { default-features = false, features = [...] }` with the
  smallest feature set that still supports host filesystem mounts, WASIX env,
  dynamic linking, and module cache seeding
- add `cargo tree` CI gates for `wasmtime`, `wasmer-compiler-llvm`,
  `wasmer-compiler-cranelift`, `llvm-sys`, and any avoidable network/client
  dependencies in the end-user feature set
- add macOS arm64/x64 tests that load main plus at least two extension modules,
  run SQL error recovery, and execute under normal parallel test scheduling

## Layout And Timezone Shims

Removed from the current WIP runtime path:

- mirroring `share/postgresql/*` into `share/*`
- mirroring `lib/postgresql/*` into `lib/*`
- writing a minimal timezone set from Rust
- rewriting PGDATA timezone settings after init

Interpretation:

- those were spike smells, not final architecture
- they came from mixing canonical PostgreSQL paths with older PGlite artifact
  expectations while proving the WASIX runtime
- the runtime now has one canonical layout and no duplicated extension
  locations

Current guard:

- `src/pglite/base.rs` no longer contains the path-mirroring or timezone rewrite
  helpers
- extension installation unpacks only into the canonical runtime root
- packaged runtime assets contain:
  `/lib/postgresql`, `/share/postgresql/extension`, `/share/postgresql/timezonesets`
- packaged runtime assets do not contain flat duplicate `share/extension`,
  `share/timezonesets`, or `lib/*.so` paths
- `xtask assets check --strict-generated` fails if these source or asset shims
  return

## Diff Audit: Remaining C Portability Layer

The current diff is still a spike-to-production transition, but the worst
debug/scaffold pieces have been removed from the maintained patch:

- `pglite-wasm/pg_proto.c` no longer carries added `#pragma warning "TEST"`
  blocks
- `pglite-wasm/pgl_os.h` no longer returns `stderr` for unknown `popen` commands
  and no longer prints boot/single diagnostic lines
- `pglite-wasm/pgl_stubs.h` no longer logs the timezone stub path; it returns
  `TZ` or a deterministic `GMT` fallback
- the fallback `ProcessStartupPacket` export no longer emits an upstream
  "STUB" debug line; ignored parameters are explicit
- bridge-level `system()` emulation no longer returns the magic status `123`; it
  fails closed with `ENOSYS`
- the source patch no longer carries self-referential
  `.pglite-oxide-patch-sha256` / `.pglite-oxide-source-head` files; the
  preparation script writes those cache markers after applying the patch
- `pglite_wasix_bridge.c` changes now invalidate the configured build through
  `.pglite-oxide-bridge-sha256`, so stale bridge objects cannot be silently
  reused
- timezone data is compiled inside the pinned Docker build from PostgreSQL's
  pinned `tzdata.zi` and then copied during packaging; packaging no longer runs
  host-local `zic`, and it does not try to execute the cross-built WASIX `zic`

Ownership status of the remaining C portability layer:

| Area | Status | Why it still exists | Current ownership | Remaining release gates |
| --- | --- | --- | --- | --- |
| `pglite-wasm/pg_proto.c` `send_ready_for_query` | Production-owned for the current protocol path; not pending as a shim cleanup item. | The host drives Postgres one frontend-message batch at a time, so ReadyForQuery has to be coordinated with extended-protocol pipelining instead of emitted blindly after every host call. | SQLx and tokio-postgres tests cover Parse-time errors, Execute-time errors, SQLSTATE preservation, recovery after Sync, successful pipelined extended queries, SSLRequest refusal, and safe CancelRequest close. Raw wire-protocol tests now cover Bind errors with exact `ErrorResponse -> ReadyForQuery` synchronization and no `BindComplete`. | Broader raw-wire/fuzz coverage can be added later, but the current known contract has regression tests. |
| `pglite-wasm/pg_main.c` initdb boot/single flow | Owned for the current runtime path. | PGlite embeds initdb and replay phases into one process because WASIX does not spawn child postgres processes for bootstrap/single-user mode. | The patch names `pglite_run_initdb_boot_phase`, `pglite_restore_stdin_after_initdb_boot`, and `pglite_run_initdb_single_phase`. Runtime smoke covers direct init, stable `postgres` identity, `template1`, deterministic `UTC` timezone, fresh persistent initdb without template cloning, stale `postmaster.pid`/`postmaster.opts` cleanup, interrupted PGDATA cleanup when `PG_VERSION` is missing, interrupted PGDATA cleanup when `PG_VERSION` exists but `global/pg_control` is missing, persistence/restart, and persistent root locks for direct/direct, server/direct, and server/server conflicts. | Broaden failure-injection later if new boot/single files are discovered, but the known release blockers are now covered. |
| `pglite-wasm/pgl_os.h` boot/single `popen()` emulation | Owned as a narrow WASIX portability layer; not a generic stub. | Upstream initdb writes generated SQL through `popen()` to child postgres. In this build, those two child invocations are replayed in-process. | The replacement is `PGLITE_WASIX_DL`-only, named `pglite_open_initdb_pipe`, accepts only `--boot` and `--single`, and returns `ENOSYS` for anything else. Source-spine checks enforce the gate and name. | Add a C/link audit showing only initdb boot/single paths call this replacement. |
| `pglite-wasm/pgl_stubs.h` frontend utility replacements | Link-analysis owned; still trim only with evidence. | Pulling frontend initdb code into the embedded backend requires frontend utility functions that are normally provided by separate frontend objects. | `assets/wasix-build/analyze_pgl_stubs.sh` records symbol use and separates runtime link inputs from frontend-tool inputs such as `pg_dump`, so tool symbols cannot justify keeping broad `pglite-wasm` replacements. Memory helpers are proven required by frontend/common objects and now follow upstream `fe_memutils` semantics for zero-size allocation, `MCXT_ALLOC_ZERO`, and `MCXT_ALLOC_NO_OOM`; `PostgresMain`, `simple_prompt`, and the exported protocol/timezone helpers remain evidence-owned. | Delete entries only when the runtime link-symbol report proves no remaining object needs them. |
| `pglite_wasix_bridge.c` host ABI | Owned as the WASIX host ABI. | The module needs host-callable buffers, stable user identity, locale command emulation, socket-to-buffer transport, fail-closed `system()`, frontend-tool socket delegation, and single-process SysV shared memory. | Source-spine checks enforce protocol input, `pgl_recv`, `pgl_shmget`, locale-only `locale -a`, stable `postgres` identity, fail-closed `system()`, bridge-hash build invalidation, non-protocol `recv`/`send`/`connect` delegation to WASIX libc, and a compiled C ABI harness for locale output, passwd fields, socket/fd options, poll readiness, mmap, and shared-memory create/attach/stat/remove. Runtime/client tests exercise direct SQL, server SQLx/tokio-postgres, control packets, vector, pg_trgm, persistence, reopen, and private WASIX `pg_dump` dump/restore. | Add integration coverage only for less common Postgres wait/socket paths discovered by future extension smoke tests. |

So the answer is: no, these are not all pending. `send_ready_for_query` and the
boot/single `popen()` replacement, the initdb flow, and the WASIX host bridge
are now owned for the current path. The frontend utility replacements still
have link-analysis cleanup gates, but they are documented surfaces rather than
hidden shims.

Current guard:

- `cargo run -p xtask -- assets source-spine --check-patch-applies` fails if the
  maintained patch adds the old `#pragma TEST`, debug `popen`, `return stderr`,
  `pg_pclose` diagnostic, generic `ProcessStartupPacket: STUB`, or
  timezone-stub logging patterns. It also compiles/runs the C bridge ABI
  harness and verifies bridge-hash invalidation markers in the asset build
  scripts.

Observed build warnings that are not runtime shims:

- `wasixcc`/configure prints a SIGPIPE while probing the underlying Clang
  version, but configure continues and the generated runtime passes smoke tests.
  Treat this as toolchain-noise to revisit when pinning the final CI image.
- Docker Desktop can report sub-second "Clock skew detected" warnings after the
  source worktree is prepared through a bind mount. Do not paper this over with
  source-touching in production scripts unless it becomes reproducible CI
  breakage; source-touching would make incremental builds less predictable.

End-to-end build-spine status from the first production-owned run:

- `DOCKER=/usr/local/bin/docker JOBS=8 cargo run -p xtask -- assets build
  --profile release --target-triple aarch64-apple-darwin --execute` completed
  from `assets/wasix-build`.
- The build produced `assets/wasix-build/build/outputs.json` with parsed WASM
  link metadata for the main runtime, runtime support modules, `vector`,
  `pg_trgm`, and `pg_dump`.
- `cargo run -p xtask -- assets aot --target-triple aarch64-apple-darwin`
  initially exposed a real bug: the serializer received repo-relative paths
  while running from `spikes/wasmer-wasix-eval`. `xtask` now canonicalizes AOT
  input/output paths before invoking the serializer.
- Fresh AOT generation then passed for all six artifacts. The main runtime
  serialized to 27,752,728 raw bytes before packaging; side modules and
  `pg_dump` serialized successfully afterward.
- `cargo run -p xtask -- assets package --target-triple
  aarch64-apple-darwin` regenerated runtime, extension, `pg_dump`, manifest,
  and AOT crate artifacts from that build.
- Package-size spot checks are under crates.io's 10 MiB compressed package
  limit: `pglite-oxide-assets` is 6.1 MiB compressed and
  `pglite-oxide-aot-aarch64-apple-darwin` is 6.3 MiB compressed.
- Root `cargo package -p pglite-oxide` still cannot complete until
  `pglite-oxide-assets` is published or available in the registry, which is the
  expected release ordering for exact internal dependency versions.

Runtime validation after regenerating WASIX assets and AOT:

- direct fresh temporary `SELECT 1` passed
- persistent fresh initdb restart plus stale `postmaster.pid` /
  `postmaster.opts` cleanup passed
- SQLx server connection/query passed
- direct `vector` extension smoke passed
- private plain SQL `pg_dump` round-trip passed
- private vector-extension `pg_dump` round-trip passed
- startup packet validation is now owned in the Rust proxy: unsupported users
  fail with SQLSTATE `28000`, unsupported databases fail with SQLSTATE `3D000`,
  and supported `postgres/template1` startup still passes SQLx/client tests

The `PostgresRecoverProtocolError` export is different from the above: it is a
deliberate host/runtime ABI. It should stay unless Wasmer can prove direct
`sigsetjmp` recovery for main and side modules across all supported targets, and
even then removal needs SQLSTATE and protocol synchronization tests first.

## Phase 1/2 Remaining Audit

The authoritative current checklist is
[`docs/PHASE_1_2_COMPLETENESS.md`](docs/PHASE_1_2_COMPLETENESS.md). Current
state:

- Phase 1 is locally proven for macOS arm64 asset generation, but not
  release-complete.
- Phase 2 is substantially proven for direct API, server API, `vector`,
  `pg_trgm`, and private `pg_dump` locally, but not release-complete.

Remaining Phase 1 work:

- add one-command release asset orchestration across source validation, build,
  package, AOT, manifest regeneration, and checks
- replace hard-coded extension staging with generated install-delta metadata
- add deterministic rebuild comparison
- add unresolved side-module import validation
- add AOT source-module hash and Wasmer engine identity validation

Remaining Phase 2 work:

- finish server protocol stress tests: partial TCP integration, prepared
  statement reuse, transaction error recovery, disconnect during extended query,
  server COPY, and mixed success/error/success pipelining
- add cross-process root-lock and real interrupted-initdb kill/abort tests
- add extension lifecycle negative and idempotency tests
- broaden private `pg_dump` round trips to indexes, views, sequences,
  extension-created objects, and server usability after dump
- add asset-mixing negative tests
- run the supported target matrix before advertising any target
