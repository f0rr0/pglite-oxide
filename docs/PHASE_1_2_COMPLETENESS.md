# Phase 1/2 Completion Criteria

This document is the no-compromise checklist for finishing the production
WASIX/Wasmer foundation before startup-performance work becomes the main focus.
Phase 1 proves that the source/build spine is real. Phase 2 proves that the
resulting artifacts behave like embedded Postgres for the direct Rust API,
server mode, extensions, and private `pg_dump`.

The standard here is intentionally stricter than "works on my machine":

- no production-owned build inputs under `spikes/`
- no manual asset copying or stale work-directory assumptions
- no ambiguous Rust/C ABI ownership
- no unproven divergence from upstream PGlite fixes
- no path mirroring, timezone rewrite, or asset-mixing fallbacks
- no public API promoted before the corresponding runtime test passes

## Current Audit Status

Last audited: 2026-04-29 on branch `f0rr0/extensions-plan`.

Verdict:

- Phase 1 is locally productionized for the macOS arm64 asset set: the source
  spine, build-output registry, deterministic packaging, AOT metadata, source
  audit, and package-size gates all pass locally.
- Phase 2 is locally productionized for the direct API, server API, `vector`,
  `pg_trgm`, private `pg_dump`, root locking, and protocol error recovery on
  the local host.
- Phase 1/2 is not release-complete until the same matrix passes on Linux
  x64/arm64, macOS x64, and Windows x64, and until the broader generated
  extension catalog is promoted through smoke tests.

Evidence already in this branch:

- local `postgres-pglite` checkout under `assets/checkouts` is on
  `REL_17_5_WASM-pglite-builder` at
  `51e222cc5f799675b8dd098f5cb7bf46cbad75a2`
- local `pglite-build` checkout under `assets/checkouts` is on `portable` at
  `c195113dbaf09488f8d5eeb2db91dacd123b74d0`
- local `pgvector` checkout under `assets/checkouts` is at
  `d238409becebb8172fe696ffa776badfad4b631c`
- maintained WASIX build files now live under `assets/wasix-build`
- `xtask assets build --execute` can produce main PGlite, runtime support
  modules, `vector`, `pg_trgm`, and `pg_dump` from the pinned local tree
- `xtask assets package` emits deterministic `.tar.zst` archives and generated
  manifest metadata
- `xtask assets aot` emits local target Wasmer LLVM AOT artifacts
- `xtask assets package` now records source module hashes for runtime,
  runtime-support, extension, and `pg_dump` modules; target AOT packs record the
  Wasmer engine/version and source module hash used to produce every artifact
- `xtask assets check --strict-generated`, `source-spine --check-patch-applies`,
  and `audit-upstream --strict` pass locally
- direct runtime, persistence/restart, stale-state cleanup, cross-process root
  locking, SQLx, tokio-postgres, raw Bind-error synchronization, partial TCP
  reads, startup identity validation, explicit `COPY FROM STDIN` rejection,
  `vector`, `pg_trgm`, demand-driven extension persistence, and private
  `pg_dump` round trips pass locally

Remaining Phase 1 blockers:

1. `xtask assets release-build` exists as the release orchestration command, but
   CI still needs to run it from a clean checkout on every supported target.
2. Extension packaging still has per-extension staging functions for `vector`
   and `pg_trgm`. This is acceptable for the first two promoted extensions, but
   not for the broader extension catalog. Port the useful install-delta model
   from upstream `pack_extension.py` into `xtask` while preserving deterministic
   `.tar.zst` output and canonical paths.
3. Deterministic archive generation exists, but there is no clean two-build
   comparison command that proves identical source pins produce identical
   archives/manifests and prints the first divergent file/hash.
4. The dynamic-link closure guard checks the current runtime-support,
   extension, and `pg_dump` outputs against the main module export set; CI still
   needs negative fixture tests that prove wrong-core side modules fail before
   runtime startup.

Remaining Phase 2 blockers:

1. Server protocol coverage now includes partial TCP reads, prepared-statement
   reuse, transaction error recovery, disconnect during extended query,
   startup/control packets, and mixed success/error/success pipelining. True
   streaming `COPY FROM STDIN` is not supported by the current call/return
   WASIX protocol ABI; server mode now rejects it with backend-owned SQLSTATE
   `0A000` and keeps the connection usable. Direct Rust blob COPY through
   `/dev/blob` remains covered.
2. Root-lock coverage includes direct/direct, server/server, server/direct, and
   cross-process conflicts. A real child-process kill during initdb/open still
   needs to be added on top of the current interrupted-PGDATA recovery tests.
3. Extension lifecycle tests now cover archive-hash mismatch rejection,
   idempotent `enable_extension`, reopen after extension install, and "not
   installed until requested". Dependency/load-order negative cases are still
   required as soon as dependent extensions are promoted.
4. Private `pg_dump` now covers tables, indexes, views, sequences,
   `--schema-only`, `--quote-all-identifiers`, server usability after dump, and
   `vector` dump/restore. Public dump APIs still wait for CI matrix coverage.
5. Asset-mixing rejection now covers runtime archive hash mismatch, extension
   archive hash mismatch, and AOT artifact/source-module mismatch. Negative
   fixture tests are still needed for wrong-core side modules and unsupported
   target errors.
6. Phase 2 is only locally proven on macOS arm64. Linux x64/arm64, macOS x64,
   and Windows x64 need the same direct/server/extension/pg_dump/AOT smoke matrix
   before any target is advertised.

## Phase 1: Source And Build Spine

Phase 1 is complete only when one maintained command can generate the main
PGlite WASIX module, runtime support modules, `vector`, one contrib extension,
and `pg_dump` from the same pinned configured source tree, then package
deterministic assets and manifests.

### 1. Move Production Build Inputs Out Of `spikes/`

Production-owned files should live under stable asset/build paths, not under
historical spike directories.

Required moves:

- maintained WASIX source patch
- WASIX bridge C source
- bridge ABI harness
- pgl_stubs link-symbol analysis script
- Dockerfile and build wrappers used by `xtask assets build`
- configure wrapper and `pg_config` wrapper used for WASIX builds

`spikes/` may keep notes, logs, and obsolete experiments, but `xtask` must not
depend on `spikes/wasix-postgres-build` for production asset generation.

Acceptance:

- `xtask assets check` points at the stable asset/build paths
- source-spine guards fail if production build inputs regress to `spikes/`
- `spikes/` remains usable only as historical evidence

### 2. Close The Required Upstream Audit

The required upstream audit items in [UPSTREAM_AUDIT.md](UPSTREAM_AUDIT.md) must
be resolved before Phase 1 is called complete.

Each item must be marked as one of:

- **ported**: the upstream fix is present in the maintained `wasix-dl` patch
- **included**: the active builder branch already contains it
- **replaced**: the WASIX implementation provides equivalent behavior and tests
  prove it
- **rejected**: the fix is irrelevant to this architecture, with a written
  reason and a guard preventing accidental reliance on it

Required audit items:

- stable PGlite protocol exports and startup HBA load
- checkpointer disable
- imported-memory build fix
- default `postgres` user and `/home/postgres`

Acceptance:

- `cargo run -p xtask -- assets audit-upstream --strict` passes or fails only
  on items explicitly rejected with tests/guards
- `docs/UPSTREAM_AUDIT.md` records the decision for every required item

### 3. Make `xtask assets build` The Only Production Build Path

The production build path must be one command, not a sequence of manually run
scripts.

The command must:

- validate `assets/sources.toml`
- verify or fetch pinned source checkouts
- apply the maintained `wasix-dl` patch
- configure one source tree
- build the main PGlite WASIX module
- build runtime support modules
- build `vector`
- build one contrib extension, initially `pg_trgm`
- build `pg_dump`
- package deterministic runtime and extension archives
- generate manifests
- optionally generate target-local AOT artifacts

Acceptance:

- `cargo run -p xtask -- assets build --profile release --target-triple <triple>
  --execute` owns the build sequence
- no release instruction tells maintainers to run production scripts manually
- all build outputs are verified before packaging starts

### 4. Replace Hard-Coded Artifact Paths With Build Metadata

The current packaging path should stop knowing exact output paths that happen to
exist in the local work tree. It should derive installable files from the
configured source tree and build metadata.

Current implementation state:

- production build inputs live under `assets/wasix-build`
- `xtask assets build --execute` writes
  `assets/wasix-build/build/outputs.json` after the pinned build succeeds
- that output manifest records each built module name, kind, path, SHA-256, and
  parsed WASM link metadata
- packaging and AOT generation now consume the centralized build-output
  registry instead of maintaining separate path lists
- staging still contains per-extension file selection for `vector` and
  `pg_trgm`; broaden this through generated extension metadata before adding
  more extensions

Required work:

- port the useful install-delta model from upstream `pack_extension.py`
- keep deterministic `.tar.zst` output instead of upstream nondeterministic
  `.tar`
- keep canonical PostgreSQL paths only
- record extension SQL/control files, native libraries, and runtime dependencies

Acceptance:

- adding a new smoke-tested extension does not require hard-coding every output
  path in `xtask`
- extension archives contain only canonical paths under `/lib/postgresql` and
  `/share/postgresql/extension`

### 5. Generate Import/Export And Memory Metadata

Dynamic linking must fail early with a useful diagnostic if the main module and
side modules do not match.

Current implementation state:

- `xtask` parses WASM modules with `wasmparser`
- build-output and asset manifests include ordinary imports, ordinary exports,
  memory declarations, `dylink.0` presence, needed side modules, runtime paths,
  dylink import info, dylink export info, and dylink memory reservation info
- the runtime asset crate can parse both old manifests and new manifests with
  link metadata

The generated manifest must include:

- main module exports
- side-module imports
- explicit Rust/WASIX ABI exports
- `dylink.0` presence
- memory import/export shape
- exception/EH requirements
- side-module native dependency order
- source pins and toolchain identity

Acceptance:

- asset checks fail before runtime startup when an extension imports an
  unresolved symbol
- asset checks fail when AOT artifacts were generated from a different module
  hash or Wasmer engine identity

### 6. Prove Deterministic Packaging

Two clean builds from the same pins should produce identical publishable
archives and manifests.

Acceptance:

- deterministic ordering, mtimes, uid/gid, names, and modes
- no host-local timezone generation during packaging
- no debug/build-id section drift that changes release assets unexpectedly
- `xtask` can compare two build outputs and print the first divergent file/hash

### 7. Keep Canonical Runtime Layout Final

The runtime layout is:

- `/bin/pglite`
- `/bin/pg_dump`
- `/lib/postgresql`
- `/share/postgresql/extension`
- `/share/postgresql/timezonesets`
- real timezone files packaged from pinned PostgreSQL tzdata

Acceptance:

- no host-side mirroring of `share/postgresql` to `share`
- no host-side mirroring of `lib/postgresql` to `lib`
- no Rust-generated minimal timezone set
- no PGDATA timezone rewrite after extraction

### 8. Classify And Reduce C Portability Code

Every remaining non-upstream C function must have an owner and evidence.

Categories:

- upstream PGlite lifecycle
- WASIX portability
- frontend-tool support
- Rust protocol ABI

Acceptance:

- `pgl_stubs.h` entries are kept only if link-symbol analysis proves they are
  needed
- `pgl_os.h` `popen` replacement remains limited to initdb boot/single commands
- bridge socket/fd/shared-memory behavior is covered by the C ABI harness or an
  integration test

## Phase 2: End-To-End Correctness Proof

Phase 2 is complete only when the Rust host is wired to the new assets without
mixing old artifacts, and direct API, server API, vector, one contrib extension,
and private dump/restore all pass from the production build path.

### 1. Direct API Correctness

Required coverage:

- fresh temporary open and `SELECT 1`
- persistent open, close, and reopen
- DDL/DML
- transactions and rollback
- typed params
- arrays, JSON/JSONB, bytea
- `COPY FROM` and `COPY TO`
- LISTEN/NOTIFY
- syntax, Parse, Bind, and Execute errors
- original PostgreSQL SQLSTATE/ErrorResponse fields
- recovery after every error

### 2. Server Protocol Correctness

Required coverage:

- SQLx connection and parameterized query
- tokio-postgres connection and parameterized query
- startup packets are parsed at the Rust proxy boundary and accepted only for
  `user=postgres` plus `database=template1`; unsupported users/databases must
  receive startup `ErrorResponse` messages with SQLSTATEs instead of being
  silently accepted
- SSLRequest returns `N`
- CancelRequest closes safely
- extended-query Parse/Bind/Execute errors do not emit premature
  `ReadyForQuery`
- raw-wire Bind errors emit `ErrorResponse`, skip `BindComplete`, and recover
  after `ReadyForQuery`
- partial TCP reads/writes
- multiple frontend messages in one packet
- prepared statement reuse
- transaction error recovery
- client disconnect during extended query
- COPY through server mode
- pipelined success/error/success batches

### 3. Root Locking And Interrupted Initdb

Required coverage:

- direct/direct lock conflict
- server/server lock conflict
- server/direct lock conflict
- lock conflict across separate OS processes
- stale `postmaster.pid` / `postmaster.opts` cleanup
- interrupted initdb with missing `PG_VERSION`
- interrupted initdb with `PG_VERSION` but missing `global/pg_control`
- real kill/abort during initdb/open followed by clean recovery or clear failure

### 4. Extension Correctness

Required for every promoted extension constant:

- archive SHA validation
- safe archive extraction
- files installed only when requested
- `CREATE EXTENSION IF NOT EXISTS`
- idempotent enable
- reopen after extension install
- direct API smoke
- server API smoke
- extension-originated errors preserve SQLSTATE
- dependency/load-order failures are clean PostgreSQL errors

Initial required extensions:

- `vector`
- `pg_trgm`

### 5. Private `pg_dump` Correctness

Before any public dump API:

- run WASIX `pg_dump`
- dump tables, indexes, views, and sequences
- dump extension-created objects
- restore into a fresh database
- verify schema and rows
- verify `CREATE EXTENSION vector`
- verify the server remains usable after dump

### 6. Asset Mixing Rejection

Tests must fail clearly when:

- runtime module hash does not match manifest
- AOT artifact hash does not match manifest
- extension archive hash does not match manifest
- extension side module was built against a different core export set
- unsupported host target has no AOT artifact

### 7. Cross-Platform Proof

Phase 2 cannot be complete based only on local macOS arm64.

Required before advertising a target:

- direct smoke
- server smoke
- vector smoke
- pg_trgm smoke
- private `pg_dump` smoke
- AOT hash verification
- no local compiler fallback

Targets that do not pass stay unadvertised and return a clear unsupported-target
error.

## Implementation Order

1. Add the one-command release asset build orchestration.
2. Replace hard-coded extension staging with generated install-delta metadata.
3. Add deterministic rebuild comparison and unresolved side-module import
   validation.
4. Add AOT source-module hash and Wasmer engine identity validation.
5. Finish cross-process lock and real interrupted-initdb tests.
6. Finish server protocol stress tests.
7. Finish extension lifecycle and private `pg_dump` correctness breadth.
8. Run the supported target matrix.

Phase 3 performance work should not optimize around behavior that has not passed
this checklist.
