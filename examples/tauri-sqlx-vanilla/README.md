# pglite SQLx Tauri profile

This is a vanilla TypeScript Tauri v2 app that exercises `pglite-oxide` through a real `sqlx::PgPool`. It uses the crate defaults: bundled PGDATA template, compiled Wasmtime module cache, quiet WASI stdio, and the preferred local proxy.

## Run the desktop app

```sh
npm install
npm run tauri dev
```

The window paints first. The pglite runtime, preferred local proxy, SQLx pool, schema setup, and query profile run only when the profile command is invoked.

## Run the headless profiler

```sh
cd src-tauri
cargo run --release --bin profile_queries -- --fresh --rows 10000 --json-out /tmp/pglite-profile-release.json
```

Use `--fresh` to remove the profile data directory before the run. Omit it to measure a warm start with an existing cluster.

The profiler uses the optimized default path. Flags:

- `--rows <n>`: control seed size.
- `--json-out <path>`: write the full report as JSON.

## What is measured

- Runtime archive install/reuse.
- Wasmtime module load, compile, or compiled-cache reuse.
- PostgreSQL cluster creation, bundled template install, or reuse.
- Preferred proxy startup: Unix socket on macOS/Linux when possible, TCP fallback otherwise.
- SQLx pool connection, including the first backend wire-protocol handshake.
- Schema creation, seeding, indexing, and real SQLx query timings.

The SQLx pool intentionally uses `max_connections(1)` because the embedded pglite runtime is single-process and proxy access is serialized.
