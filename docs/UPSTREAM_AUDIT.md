# Upstream PGlite Source Audit

This audit tracks the upstream `postgres-pglite` work that must be included or
ported before the WASIX/Wasmer production source spine is considered real.

The active source baseline is:

- repo: `electric-sql/postgres-pglite`
- branch: `REL_17_5_WASM-pglite-builder`
- pinned commit: `51e222cc5f799675b8dd098f5cb7bf46cbad75a2`

The comparison branch is `REL_17_5-pglite`. That branch contains several newer
runtime fixes that are not automatically present in the builder branch because
the branches diverged after `5e2f3df49d4298c6097789364a5a53be172f6e85`.

`electric-sql/pglite-build` branch `portable` is also pinned as build evidence,
but it is not a second production source tree. The active builder branch already
vendors the important `pglite-build` scripts and patch results. The source-spine
check verifies that the shared `wasm-build` scripts still match so we can treat
`postgres-pglite` as the single configured tree for runtime, extensions, and
`pg_dump`.

## How To Run

```sh
cargo run -p xtask -- assets audit-upstream
```

Use strict mode when a build or release candidate claims Phase 1 is complete:

```sh
cargo run -p xtask -- assets audit-upstream --strict
```

Strict mode must pass before claiming Phase 1 complete. It can pass either
because a fix is included by ancestry or because the WASIX path has an explicit,
tested replacement recorded below.

## Included In Builder Branch

- `51e222c` `fix(pglite): export missing _invoke_ functions for AGE extension`
- `c7c530a` `feat: Add Apache AGE graph database extension`
- `bee4a36` backend `pgcrypto` work
- `3e61969` `pg_hashids` backend work
- `73abc36` `pg_uuidv7` backend work
- `f5f1005` backend `pg_dump` work
- `774390f` `pgTap` backend work
- `1a0bdab` C-code refactor for the builder branch
- `6c55ae1` skip system call during db init
- `c026976` switch to vanilla Emscripten SDK for builder artifacts

These are the main reason the builder branch is the right foundation for
extension packaging and `pg_dump` exploration.

## `pglite-build` Comparison

Current local comparison:

- `pglite-build`: branch `portable`, commit
  `c195113dbaf09488f8d5eeb2db91dacd123b74d0`
- `postgres-pglite`: branch `REL_17_5_WASM-pglite-builder`, commit
  `51e222cc5f799675b8dd098f5cb7bf46cbad75a2`

The important shared build scripts are identical between the two checkouts:

- `wasm-build/build-ext.sh`
- `wasm-build/build-pgcore.sh`
- `wasm-build/extension.sh`
- `wasm-build/getsyms.py`
- `wasm-build/linkimports.sh`
- `wasm-build/pack_extension.py`
- `wasm-build/reqsym.py`

The meaningful differences are:

- `pglite-build/wasm-build/build-with-docker.sh` exists only in `pglite-build`.
  Our `xtask` and `assets/wasix-build/docker_*.sh` scripts own the
  WASIX build orchestration instead.
- `postgres-pglite/wasm-build/include` exists only in the builder branch and is
  part of the source tree we should preserve.
- `linkexport.sh` differs only by a source-file marker comment in the builder
  branch.
- `sdk.sh` in `pglite-build` still contains Emscripten SDK patching for
  `getTempRet0`, stack-first behavior, and old `wasm-opt` handling. The builder
  branch has removed those patch blocks. They are not part of the WASIX/Wasmer
  production path.
- `sdk_port-wasi.c` in the builder branch has cleanup over `pglite-build`:
  a safer `sigfillset` implementation, removal of unused socket globals, and
  removal of dead socket prototype code.

The reusable pieces from `pglite-build` are therefore not a separate runtime
path. They are the source-spine ideas we should preserve in `xtask`:

- install-delta based extension packaging
- `wasm-objdump` based import/export extraction through `getsyms.py`
- link-export generation for symbols needed by side modules
- extension catalog flow that builds contrib and extra PGlite extensions from
  one configured tree

Do not call `pack_extension.py` directly for published assets. It writes plain
`.tar` archives, uses build-time mtimes/owners/modes, mutates `PGROOT` after
packing, and allows non-canonical `/lib/*` runtime dependency paths. Port its
install-delta and import-extraction model into `xtask`, then emit deterministic
`.tar.zst` archives under canonical PostgreSQL paths.

## Required Stable-Branch Fixes

These are required unless our WASIX-specific implementation makes the upstream
fix obsolete and that replacement is proven in tests.

- `a58ae72` stable PGlite protocol exports and startup HBA load.
  Relevance: current Rust host still expects `ProcessStartupPacket`,
  `pgl_getMyProcPort`, `pgl_sendConnData`, `pgl_pq_flush`,
  `PostgresMainLoopOnce`, and error-recovery exports. The builder branch instead
  exposes `pgl_initdb`, `pgl_backend`, `pgl_shutdown`, and `interactive_one`.
  The production decision is to either port the stable exports or update the Rust
  host to the builder-branch ABI. Current decision: **replaced** by the explicit
  WASIX protocol ABI in `src/pglite/postgres_mod.rs` plus SQLx,
  tokio-postgres, raw wire-protocol, SSLRequest, CancelRequest, and
  Parse/Bind/Execute recovery tests. The Rust host keeps the upstream
  `pgl_initdb`/`pgl_backend` lifecycle separate from WASIX transport exports.

- `01792c3` stable checkpointer disable.
  Relevance: embedded single-process PGlite must not rely on postmaster-managed
  background processes. Startup and persistence tests must prove this is handled
  on the builder branch. Current decision: **ported** into the maintained
  `wasix-dl` patch by guarding the XLOG-driven `RequestCheckpoint` path with
  `#ifndef __PGLITE__`.

- `0c98d7c` stable imported-memory build fix.
  Relevance: any final Wasmer AOT artifact must have a deterministic memory
  shape compatible with headless loading and side-module linking. Current
  decision: **replaced** by the WASIX dynamic-main / side-module build contract
  (`-sMODULE_KIND=dynamic-main`, `-sWASM_EXCEPTIONS=yes`, `-Wl,-shared`) and
  manifest/AOT hash validation. The Emscripten `-sIMPORTED_MEMORY=1` flag is not
  part of the WASIX production toolchain.

- `ac31093` stable default `postgres` user and `/home/postgres`.
  Relevance: end users should get the normal `postgres` identity without custom
  startup flags or client-side special cases. Current decision: **replaced** by
  the WASIX identity bridge (`postgres`, `/home/postgres`), runtime environment
  defaults, and runtime smoke tests that verify `current_user` and
  `session_user`.

## Optional Or Extension-Specific Pending Ports

- `6c76f5e` `IsTransactionBlock` export.
  Needed only if the Rust host exposes transaction-state APIs matching PGlite JS.

- `d0f2748` PostGIS backend proof.
  Required before PostGIS is promoted, but not before the initial `vector` and
  contrib-extension production path.

## Current Decision

Use `REL_17_5_WASM-pglite-builder` for Phase 1 because it contains the extension
and `pg_dump` source/build spine. The required stable-branch audit is now
decision-complete for the current WASIX architecture: one item is ported into
the maintained patch and three are replaced by explicit WASIX behavior plus
guards/tests. `xtask` validates these replacement markers in strict mode.

The generated asset manifest currently still records older source pins. That is
allowed only as WIP evidence; release builds must regenerate it from
`assets/sources.toml` and pass:

```sh
cargo run -p xtask -- assets check --strict-generated
```
