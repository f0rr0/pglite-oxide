# pglite-oxide

[![CI](https://github.com/f0rr0/pglite-oxide/actions/workflows/ci.yml/badge.svg)](https://github.com/f0rr0/pglite-oxide/actions/workflows/ci.yml)

`pglite-oxide` embeds the [Electric SQL PGlite](https://github.com/electric-sql/pglite)
WASI PostgreSQL runtime in a Rust library. It installs the bundled runtime, starts
Postgres inside Wasmtime, and exposes a small synchronous API for executing SQL
without a separate database server. It can also expose the embedded backend over
a local PostgreSQL socket for Rust clients such as SQLx and `tokio-postgres`.

The crate currently targets PostgreSQL 17.x PGlite builds, Rust 1.92+, and
Wasmtime 44.

## Quick Start

```rust,no_run
use pglite_oxide::Pglite;
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut db = Pglite::builder().path("./.pglite").open()?;

    db.exec("CREATE TABLE IF NOT EXISTS items(value TEXT)", None)?;
    db.query(
        "INSERT INTO items(value) VALUES ($1)",
        &[json!("alpha")],
        None,
    )?;

    let result = db.query("SELECT value FROM items", &[], None)?;
    println!("{:?}", result.rows);

    db.close()?;
    Ok(())
}
```

Use `Pglite::temporary()?` for an ephemeral database in tests; it clones a
process-local template cluster so repeated tests do not rerun `initdb`.

## PostgreSQL Client Compatibility

Use `PgliteServer` when a library expects a PostgreSQL connection string. The
server owns one embedded backend, so configure downstream pools with a single
connection.

```rust,no_run
use pglite_oxide::PgliteServer;
use sqlx::{Connection, Row};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri()).await?;

    let row = sqlx::query("SELECT $1::int4 + 1 AS answer")
        .bind(41_i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<i32, _>("answer")?, 42);

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}
```

For app persistence, use `PgliteServer::builder().path("./.pglite").start()?`.
For Rust code that does not require a connection URI, prefer the direct
`Pglite` API because it avoids the socket/protocol compatibility layer.
For desktop app shape and state management notes, see
[`docs/TAURI.md`](docs/TAURI.md).

## Runtime API

`Pglite` is the main entry point.

- `Pglite::builder()` configures persistent, app-data, or temporary databases.
- `Pglite::open(path)` opens a persistent database rooted at `path`.
- `Pglite::temporary()` opens a cached ephemeral database for tests.
- `exec(sql, options)` runs simple SQL and returns zero or more result sets.
- `query(sql, params, options)` uses the extended protocol with JSON parameters.
- `describe_query(sql, options)` returns parameter and row metadata.
- `transaction(|tx| ...)` runs `BEGIN`/`COMMIT` with rollback on error.
- `listen`, `unlisten`, and `on_notification` support PostgreSQL notifications.
- `close()` shuts down the embedded backend.
- `PgliteServer` exposes a local PostgreSQL socket for existing client crates.

Values are passed as `serde_json::Value`. Default parsers and serializers cover
common Postgres types including integers, floats, booleans, JSON/JSONB, bytea,
dates/timestamps, UUIDs, and arrays discovered from `pg_type`.

## Query Options

`QueryOptions` controls result parsing and protocol behavior:

```rust,no_run
use pglite_oxide::{Pglite, QueryOptions, RowMode};

fn main() -> anyhow::Result<()> {
    let mut db = Pglite::open("./.pglite")?;

    let options = QueryOptions {
        row_mode: Some(RowMode::Array),
        ..QueryOptions::default()
    };

    let _rows = db.query("SELECT 1, 2", &[], Some(&options))?;

    Ok(())
}
```

For `COPY ... FROM '/dev/blob'`, set `QueryOptions::blob` to the bytes to expose
through the guest `/dev/blob`. For `COPY ... TO '/dev/blob'`, read the returned
`Results::blob`.

## SQL Templating Helpers

```rust,no_run
use pglite_oxide::{Pglite, QueryTemplate, format_query, quote_identifier};
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut db = Pglite::open("./.pglite")?;

    let sql = format_query(&mut db, "SELECT $1::int", &[json!(42)])?;
    assert_eq!(sql, "SELECT '42'::int");

    let mut template = QueryTemplate::new();
    template.push_sql("SELECT * FROM ");
    template.push_identifier("items");
    template.push_sql(" WHERE value = ");
    template.push_param(json!("alpha"));
    let built = template.build();

    assert_eq!(built.query, "SELECT * FROM \"items\" WHERE value = $1");
    assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");

    Ok(())
}
```

## Runtime Notes

The embedded backend uses the same shared-memory CMA protocol as upstream
PGlite. The host preopens:

- `/tmp` as the runtime root
- `/tmp/pglite/base` as the Postgres data directory
- `/home` for runtime home files
- `/dev` for small device shims such as `urandom`

The first instance in a process can take a while because Wasmtime compiles the
large PGlite WASM module and the first temporary cluster runs `initdb`. Compiled
modules are cached inside the process so additional `Pglite` instances avoid the
same compile cost. `Pglite::temporary()` also clones a process-local template
cluster, so later temporary databases in the same test process only copy the
prepared filesystem. Use `Pglite::builder().fresh_temporary().open()?` when a
test needs to exercise fresh cluster initialization.

Opening an existing cluster still invokes PGlite's `initdb` export because the
WASM runtime uses that entry point for in-memory backend setup too. Existing data
is preserved; the full cluster creation work is avoided once `PG_VERSION` exists.

The default `runtime-cache` feature also enables Wasmtime's persistent compiled
module cache, so later processes can reuse native code for the same PGlite WASM
module. Disable it with `default-features = false` if you need to avoid global
cache writes.

For fast local test loops in a downstream workspace, add the same profile
override used by this repository. Wasmtime's debug cache otherwise keys entries
by the rebuilt test binary mtime, which defeats reuse after ordinary edits:

```toml
[profile.dev.package.wasmtime-internal-cache]
debug-assertions = false
```

For larger downstream suites, prefer reusing one `Pglite` instance per test when
isolation allows it, and use `fresh_temporary` only for initialization-specific
coverage.

`PgliteServer` is deliberately blocking and handles one frontend connection at a
time against a single embedded backend. It refuses SSL/GSS negotiation requests
with the standard PostgreSQL `N` response; connection URIs generated by the
crate include `sslmode=disable`.

```sh
cargo run --bin pglite-proxy -- --root ./.pglite --tcp 127.0.0.1:5432
psql 'postgresql://postgres@127.0.0.1:5432/template1?sslmode=disable'
```

On Unix systems, the default proxy mode is `/tmp/.s.PGSQL.5432`:

```sh
cargo run --bin pglite-proxy
PGPASSWORD=postgres psql 'postgresql://postgres@/template1?host=/tmp'
```

Runtime asset provenance is tracked in `docs/ASSETS.md`.
Release process details are tracked in `docs/RELEASE.md`.

## Development

The required local gates are:

```sh
cargo fmt --all --check
cargo check --all-targets
cargo check --no-default-features --all-targets
cargo clippy --all-targets -- -D warnings
cargo deny check
cargo test --doc
cargo test --test runtime_smoke -- --nocapture
cargo test --test proxy_smoke -- --nocapture
cargo test --test client_compat -- --nocapture
cargo package --allow-dirty
```

Install the supply-chain gate with `cargo install cargo-deny --locked` if it is
not already available.

`tests/runtime_smoke.rs` starts the real WASM backend and is intentionally slower
than the protocol unit tests.

## Utilities

Two maintenance binaries are included:

- `pglite-dump` expands the bundled filesystem manifest/runtime assets.
- `pglite-manifest-sync` syncs `assets/pglite_fs_manifest.json` from the
  `pglite.js` bundle published on `electric-sql/pglite-build` `gh-pages`.
- `pglite-proxy` exposes a local PostgreSQL socket backed by the embedded runtime.
