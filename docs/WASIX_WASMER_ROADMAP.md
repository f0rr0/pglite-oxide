# WASIX/Wasmer Roadmap

This is the implementation roadmap for the production `pglite-oxide` runtime.
It is intentionally ordered by dependency, not by convenience. Work that affects
the runtime architecture, including experiments, is part of the main execution
path and must end with an explicit decision.

## Product Goal

`pglite-oxide` should be embedded Postgres for Rust tests and local apps:

- no Docker for end users
- no local LLVM, Cranelift, Wasmer compiler backend, or Postgres build step for
  end users
- direct Rust API for tests and local app storage
- local Postgres server mode consumable by any Postgres client, library, or
  language
- first-class SQLx, tokio-postgres, Diesel, SeaORM, and Tauri use cases
- first-class pgvector/local RAG use case
- bundled common Postgres extensions, installed into an instance only when the
  developer requests them

The product story is not "a PGlite WASI runtime". The product story is
"embedded Postgres for tests and local apps, with pgvector and server mode".

## Architecture Goal

The production runtime path is:

1. build PGlite/Postgres from pinned `postgres-pglite` sources as WASIX
   dynamic-linking modules
2. build SQL extensions as matching WASIX side modules from the same configured
   source tree and sysroot
3. build `pg_dump.wasix.wasm` from the same tree
4. generate deterministic runtime, extension, template, and manifest assets
5. precompile target-specific Wasmer LLVM AOT artifacts in CI
6. load verified AOT artifacts through headless Wasmer in user applications

Normal user builds must not compile Wasm locally. Unsupported targets fail with
a clear missing-AOT-artifact error.

## Non-Goals

- no Wasmtime/static WASI production path
- no Emscripten/JavaScript glue production path
- no user-side Docker, LLVM, Cranelift, or Postgres compilation
- no public extension constant before the extension passes Rust runtime smoke
  tests
- no public `pg_dump` API before dump/restore round-trip tests pass
- no duplicated runtime layouts or host-side path shims in the release path

Historical references and failed experiments belong under `spikes/` or in a
clearly marked research document.

## Current WIP State

The repository already has useful scaffold and evidence, but it is not yet the
production architecture:

- workspace, asset-crate, AOT-crate, and runtime docs exist
- runtime install now uses canonical PostgreSQL paths without host-side
  path-mirroring or timezone rewrite shims
- this branch wires `xtask assets build` and `xtask assets package` to the
  pinned WASIX source/build spine for the local macOS arm64 asset set
- `xtask assets check --strict-generated` validates generated source pins
  against `assets/sources.toml` and guards canonical runtime/extension layout
- `wasmer-wasix` currently pulls a broad default dependency surface
- `Pglite::preload()` does not yet keep a process-hot module for the next open
- current local performance is not acceptable for per-test setup
- the WASIX source patch has been cleaned of the old `#pragma TEST`, debug
  `popen`, `return stderr`, and timezone-stub logging additions, but still
  contains a deliberate C portability layer for initdb boot/single pipes, process
  identity, socket-buffer transport, and shared memory
- direct Rust, SQLx, tokio-postgres, raw wire-protocol, and vector-loaded paths
  now preserve PostgreSQL `ErrorResponse` SQLSTATE fields through the explicit
  `PostgresRecoverProtocolError` WASIX ABI for the currently covered
  Parse/Bind/Execute cases; broader generated extension and dependency/load
  order error coverage still belongs in the release gate
- the local `postgres-pglite` checkout has WASIX patch work from the spike that
  must be preserved and rebased onto the builder branch, not lost

Current WIP measurement from local macOS arm64:

```text
Pglite::preload()  ~= 6.5s
temporary().open() ~= 4.7s
first query        ~= dominated by open time
```

The measurement proves the direction enough to continue; it is not the desired
developer experience.

## Phase 1: Source And Build Spine

Use `electric-sql/postgres-pglite` branch `REL_17_5_WASM-pglite-builder` as the
active source baseline. Keep `REL_17_5-pglite` only as a comparison/reference
branch.

The live upstream-fix audit is tracked in [UPSTREAM_AUDIT.md](UPSTREAM_AUDIT.md).
Phase 1 is not complete while required audit items are still pending.
The no-compromise Phase 1/2 completion checklist is tracked in
[PHASE_1_2_COMPLETENESS.md](PHASE_1_2_COMPLETENESS.md).

Implementation:

- align `.gitmodules`, `assets/sources.toml`, and local upstream checkouts on
  `REL_17_5_WASM-pglite-builder`
- keep `pglite-build` `portable` pinned as build-script provenance, but treat
  the configured `postgres-pglite` builder branch as the single production source
  tree; shared `wasm-build` scripts must stay audited for drift
- preserve the current WASIX patch as
  `assets/wasix-build/patches/postgres-pglite-wasix-dl.patch`
- rebase that patch onto the builder branch as a maintained `wasix-dl` build
  personality
- preserve upstream PGlite lifecycle/protocol work where possible:
  `pgl_initdb`, `pgl_backend`, `pgl_shutdown`, `interactive_one`, and
  `pg_proto.c`
- keep the Rust protocol bridge as an explicit maintained ABI layer and document
  every required export
- build the main PGlite module, extension side modules, and
  `pg_dump.wasix.wasm` from one configured source tree
- normalize outputs to canonical Postgres paths only:
  `/bin/pglite`, `/bin/pg_dump`, `/lib/postgresql`,
  `/share/postgresql/extension`, and `/share/postgresql/timezonesets`
- package timezone data from a pinned build input, not from Rust-generated
  minimal files
- audit newer upstream fixes and mark each as included, rejected, or pending:
  checkpointer disable, background-worker disable, default `postgres`
  user/database/role, imported memory sizing, artifact cache fixes,
  data-directory locking, startup `postgresConfig`, and `pgoutput` symbol exports

`xtask` requirements:

- validate `assets/sources.toml`
- validate source pins and local checkout drift
- validate that the builder branch still carries the expected `pglite-build`
  symbol/import, extension-build, and package-delta machinery
- fetch or verify pinned sources
- run the pinned builder
- generate runtime, extension, and `pg_dump` outputs
- generate deterministic archives and manifests
- fail if generated manifest source pins do not match `assets/sources.toml`

Done when one local command can produce PGlite, `vector`, one contrib extension,
and `pg_dump` from the same pinned tree.

## Phase 2: End-To-End Correctness Proof

Wire the new artifacts into the Rust host without mixing old assets.

Runtime correctness:

- open/init a database
- run `SELECT 1`
- recover from SQL errors without corrupting backend state
- preserve original Postgres `ErrorResponse` fields for Parse/Bind/Execute
  failures through `PostgresRecoverProtocolError`; direct syntax, direct
  missing-table Parse, direct Bind, SQLx Parse/Bind/Execute, tokio-postgres
  Parse/Execute, raw wire-protocol Bind, and vector-loaded direct/server error
  recovery are covered; broader generated extension and dependency/load-order
  failures remain required before release
- close and reopen cleanly
- keep the canonical-layout guard passing so path mirroring and timezone rewrite
  shims cannot return
- lock persistent roots so two processes cannot use the same `PGDATA`

Server correctness:

- SQLx connection works
- tokio-postgres connection works
- SSLRequest receives the correct no-SSL response
- CancelRequest is handled safely
- extended-query errors do not emit premature `ReadyForQuery`
- raw wire-protocol Bind errors emit `ErrorResponse`, skip `BindComplete`, and
  recover only after `ReadyForQuery`

Extension correctness:

- load the `vector` side module
- run `CREATE EXTENSION vector`
- insert and query vector values
- exercise vector through `PgliteServer`

Private `pg_dump` correctness:

- run WASIX `pg_dump`
- restore into a fresh database
- verify schema and rows
- verify dumps include `CREATE EXTENSION vector`

Done when direct API, server API, vector, and private dump/restore pass from the
new build path.

## Phase 3: Startup And Runtime Architecture

This is core implementation work, not optional experimentation. Caches and
snapshots can preserve broken state, so this phase starts only after Phase 2
correctness passes.

Add phase timers for:

- manifest validation
- runtime cache/extraction
- AOT install/decompress/hash
- module deserialization
- WASIX runtime construction
- instance creation
- backend start
- startup packet
- first query
- extension load
- first vector query
- `pg_dump`

Implement the baseline fast path:

- Wasmer LLVM AOT artifacts
- headless Wasmer loading
- process-wide engine/module cache
- persistent raw AOT cache
- runtime asset cache
- extension asset cache
- PGDATA template cache
- vector-enabled template cache

Promoted experiments with required decisions:

- snapshot/journaling/Instaboot-style restore:
  - temporary databases first
  - restore to first `SELECT 1`
  - repeat create/drop cycles without corruption
  - repeat with `vector`
  - promote only if correctness is proven
- MountFS:
  - compare Wasmer 7.1 stable and 7.2 alpha
  - verify nested mounts for `/`, `/base`, `/tmp`, `/lib/postgresql`, and
    `/share/postgresql`
- hardlink/reflink/copy:
  - benchmark runtime files
  - benchmark PGDATA templates
  - choose per-platform default
- mmap/native deserialization:
  - compare against `Module::deserialize_from_file`
  - keep SHA verification before unsafe deserialization
- side-module AOT preloading:
  - prove extension load avoids local compilation
  - prove repeated extension enable reuses module cache
- SIMD/relaxed SIMD:
  - benchmark pgvector distance workloads
  - promote only with measurable gain and no portability regression
- Cranelift:
  - test exceptions, dynamic linking, vector, and server mode
  - keep as maintainer/dev backend only if correctness passes

Performance gates:

- first open under 5s on GitHub Ubuntu
- warm open under 1s
- vector enable plus first query under 2s
- after a stable baseline, block more than 25% regression

Done when the fastest correct startup architecture is chosen and implemented.

## Phase 4: Extensions And `pg_dump` Product Surface

Generate extension metadata from:

- PGlite catalogs
- builder branch `pglite/Makefile`
- `postgres-pglite/pglite/other_extensions`
- supported Postgres contrib directories
- pinned external repos

Classify every extension:

- normal `CREATE EXTENSION`
- preload-required
- startup-config-required
- native dependency/load-order-required
- not a SQL extension

Initial promotion order:

1. `vector`
2. `pg_trgm`
3. `hstore`
4. `pgcrypto`
5. one representative contrib extension
6. `pgtap`
7. `pg_uuidv7`
8. `pg_hashids`
9. `pg_ivm`
10. `age`
11. `pg_textsearch`
12. PostGIS after size, load-order, and dependency proof

Public APIs:

- `PgliteBuilder::extension`
- `PgliteBuilder::extensions`
- `Pglite::enable_extension`
- `Pglite::preload_extensions`
- server builder equivalents
- `PgliteServer::database_url`

Rules:

- generate constants only after smoke tests pass
- keep `live` out of SQL extension constants
- `extensions::ALL` includes only passing constants for the current asset set

`pg_dump` public API:

- expose only after round-trip tests pass
- add `PgDumpOptions`, `Pglite::dump_sql`, `Pglite::dump_bytes`, and the real
  `pglite-dump` CLI
- defaults are plain SQL, `--inserts`, `-j 1`, user `postgres`, and internal
  `/tmp/out.sql`

Done when extension constants and dump APIs are generated and test-gated.

## Phase 5: CI, Release, Docs, Examples

CI:

- workspace check
- no-default-features check
- doctests
- nextest
- feature powerset
- package size gates
- publish dry-runs for all published crates
- asset build matrix
- no legacy runtime gate
- dependency invariant gate
- macOS multi-module exception tests
- extension smoke tests
- protocol correctness tests
- Rust, Python, Go, and Node examples CI

Dependency hardening:

- minimize `wasmer` features
- minimize `wasmer-wasix` features
- block Wasmtime/static WASI regressions
- block backend compiler crates in the normal user path
- allow base Wasmer metadata crates only if required by headless/sys loading

Release:

- release-plz remains release owner
- one root `CHANGELOG.md`
- one version group
- exact internal dependency versions
- internal asset/AOT changes included in the root changelog
- Trusted Publishing for every published crate
- sensitive GitHub Actions pinned to full SHAs

Docs and examples:

- README first screen focuses on embedded Postgres, tests, local apps, pgvector,
  and any Postgres client
- update development, assets, runtime, performance, extensions, dump, and
  release docs as implementation lands
- add examples for SQLx, tokio-postgres, rstest, Diesel, SeaORM, Tauri,
  pgvector local RAG, Python pytest plus psycopg, Go pgx, and Node `pg`

Done when the product is buildable, fast, tested, publishable, and honestly
documented.

## Required Test Categories

Before release, the suite must cover:

- direct `SELECT 1`
- persistence and restart
- temporary template cache
- persistent root lock
- SQLx server connection
- tokio-postgres server connection
- SSLRequest
- CancelRequest
- extended-query error recovery
- SQLSTATE fidelity for direct, server, extension-loaded, Parse, Bind, and
  Execute error paths
- vector create/insert/query/distance
- generated extension smoke suite
- unsafe archive rejection
- canonical path validation
- manifest SHA validation
- AOT SHA verification
- unsupported target error
- macOS multi-module exception recovery
- private then public dump/restore
- Python, Go, and Node proxy tests
- package size checks
- publish dry-runs

## Decision Log Requirements

Every promoted experiment must end with one of these states in the repo:

- `promoted`: implementation is on the production path
- `blocked`: evidence and blocker are documented
- `rejected`: reason and alternative are documented

Do not leave runtime-affecting experiments as loose notes after Phase 3.
