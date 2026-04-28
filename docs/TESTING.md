# Testing Groundwork

This document tracks the correctness suite for the WASIX/Wasmer runtime. The
goal is not only to prove `SELECT 1`, but to exercise the protocol and extension
paths developers will use through the direct Rust API, local server mode, and
normal Postgres client libraries.

## Upstream Suites Audited

- PGlite core tests:
  - `assets/checkouts/pglite/packages/pglite/tests/basic.test.ts`
  - `assets/checkouts/pglite/packages/pglite/tests/exec-protocol.test.ts`
  - `assets/checkouts/pglite/packages/pglite/tests/types.test.ts`
  - `assets/checkouts/pglite/packages/pglite/tests/notify.test.ts`
  - `assets/checkouts/pglite/packages/pglite/tests/describe-query.test.ts`
  - `assets/checkouts/pglite/packages/pglite/tests/pgvector.test.ts`
- PGlite socket tests:
  - `assets/checkouts/pglite/packages/pglite-socket/tests/server.test.ts`
  - `assets/checkouts/pglite/packages/pglite-socket/tests/query-with-node-pg.test.ts`
  - `assets/checkouts/pglite/packages/pglite-socket/tests/query-with-postgres-js.test.ts`
- pgvector regression tests:
  - `assets/checkouts/pgvector/test/sql/vector_type.sql`
  - `assets/checkouts/pgvector/test/expected/vector_type.out`
- PostgreSQL regression tests:
  - `assets/checkouts/postgres-pglite/src/test/regress/sql`
  - these are too broad to port wholesale now, but they are the source for
    future SQL behavior and datatype coverage.
- pglite-bindings:
  - no formal test suite is present in the local checkout; the useful reference
    material is startup/protocol gateway code under
    `assets/checkouts/pglite-bindings/17.x`.

## Current Coverage

### Direct Rust API

Covered by `tests/runtime_smoke.rs`:

- temporary open and `SELECT 1`;
- identity defaults: user, database, and timezone;
- timezone catalog data and invalid timezone recovery;
- `COPY FROM/TO '/dev/blob'`;
- `LISTEN` / `NOTIFY`;
- query formatting and templating;
- typed parameter serialization for ints, bools, JSON, arrays, and bytea;
- row mode;
- transaction commit and rollback;
- syntax and Parse-phase SQLSTATE preservation;
- missing relation SQLSTATE preservation;
- Bind-phase invalid typed parameter SQLSTATE preservation;
- Bind-phase wrong parameter count SQLSTATE preservation;
- recovery after protocol errors;
- close, restart, and persistent root reopen.
- fresh persistent initdb without the PGDATA template;
- stale `postmaster.pid` / `postmaster.opts` cleanup before restart;
- interrupted PGDATA cleanup when `PG_VERSION` is missing;
- interrupted PGDATA cleanup when `PG_VERSION` exists but `global/pg_control`
  is missing;
- persistent root lock rejection for concurrent direct opens and server/direct
  conflicts;
- persistent root lock rejection for concurrent server opens.

### Local Server Mode

Covered by `tests/client_compat.rs`:

- SQLx parameter queries and table reads;
- tokio-postgres parameter queries and table reads;
- SQLx Parse, Bind, and Execute error recovery;
- tokio-postgres Parse and Execute error recovery;
- pipelined extended queries;
- mixed success/error/success pipelined extended queries;
- explicit prepared-statement reuse;
- transaction error recovery through rollback;
- partial TCP reads and pipelined simple queries at raw wire level;
- raw wire-protocol Bind errors with exact `ErrorResponse -> ReadyForQuery`
  synchronization;
- client disconnect during an extended-query exchange without poisoning the
  backend;
- SSLRequest no-SSL response;
- CancelRequest safe close;
- `COPY FROM STDIN` rejection with SQLSTATE `0A000` and connection recovery;
- timezone recovery and reconnect behavior.

The raw wire-protocol test exists because normal clients often validate some
Bind mistakes before sending them to Postgres. It directly sends `Parse`,
`Bind`, `Describe`, `Execute`, and `Sync` messages and verifies SQLSTATE plus
connection recovery.

Streaming `COPY FROM STDIN` is intentionally fail-closed in server mode today.
The current WASIX ABI steps the backend through call/return protocol buffers;
true `COPY FROM STDIN` needs a continuation/threaded transport that can yield
after `CopyInResponse` and resume inside COPY state when `CopyData` arrives.
Until that architecture lands, server mode returns SQLSTATE `0A000` and keeps
the connection usable. Direct Rust API blob COPY through `/dev/blob` is covered
by `tests/runtime_smoke.rs`.

### Extensions

Covered by `tests/extensions_smoke.rs`:

- `vector` direct API load, create, insert, distance query, and deterministic
  `pg_catalog` extension schema;
- `vector` direct API extension-originated errors:
  - invalid vector literal,
  - dimension mismatch,
  - pgvector core type cases ported from `vector_type.sql`;
- `vector` through `PgliteServer` and SQLx;
- SQLx recovery after vector-originated errors;
- demand-driven extension install: the side module is absent until requested,
  `enable_extension` is idempotent, reopen after install works without a local
  compiler, and existing side modules are seeded into the headless Wasmer cache;
- `pg_trgm` direct API, deterministic `pg_catalog` extension schema, and SQLx
  server smoke coverage;
- extension archive SHA-256 mismatch rejection.

### pg_dump

Covered privately by `src/pglite/pg_dump.rs` tests:

- packaged WASIX `pg_dump` artifact loads through the AOT manifest;
- `pg_dump` connects to `PgliteServer` over local TCP via Wasmer host
  networking;
- plain SQL output includes table DDL and `INSERT` data;
- indexes, views, and sequences round-trip;
- custom pg_dump args are honored for `--schema-only` and
  `--quote-all-identifiers`;
- dump SQL restores into a fresh direct `Pglite`;
- restored data is queryable after pg_dump's `search_path` reset;
- the source server remains usable after `pg_dump`.
- vector extension dump includes extension DDL and restores vector data into a
  fresh vector-enabled database.

## Remaining Release Gates

These are not optional before a production release:

- broader generated extension smoke suite for every public extension constant;
- extension load-order and missing native dependency failures;
- public `pg_dump` CLI/API tests using the same private runner coverage;
- larger PostgreSQL regression subsets for datatypes, DDL, transactions, COPY,
  and planner/index behavior;
- Python, Go, and Node proxy examples in CI;
- performance gates from `docs/PERFORMANCE.md` and
  `docs/WASIX_WASMER_ROADMAP.md`.

## Porting Policy

Port upstream tests in layers:

1. Prefer a small direct Rust or server-client test that captures the exact
   contract.
2. Preserve SQLSTATE checks for error behavior.
3. Verify recovery with a successful query after every expected error.
4. Use raw wire-protocol tests when client libraries hide the backend behavior.
5. Add broad upstream regression ports only after the runtime path is stable, so
   failures identify real behavior gaps instead of build-spine churn.
