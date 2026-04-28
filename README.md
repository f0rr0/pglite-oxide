# pglite-oxide

[![CI](https://github.com/f0rr0/pglite-oxide/actions/workflows/ci.yml/badge.svg)](https://github.com/f0rr0/pglite-oxide/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/pglite-oxide.svg)](https://crates.io/crates/pglite-oxide)
[![docs.rs](https://docs.rs/pglite-oxide/badge.svg)](https://docs.rs/pglite-oxide)
[![MSRV](https://img.shields.io/badge/msrv-1.92-blue)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20AND%20Apache--2.0%20AND%20PostgreSQL-blue)](https://github.com/f0rr0/pglite-oxide#license)

Embedded Postgres for Rust tests and local apps. No Docker, works with SQLx and
any Postgres client.

Use it when you want:

- local Postgres semantics in a Rust, Tauri, or desktop AI app
- fast Postgres-backed tests without Docker or testcontainers
- a PostgreSQL connection URI for SQLx, `tokio-postgres`, Python, Go, Node, or any other client

The crate targets PostgreSQL 17.x PGlite builds and Rust 1.92+.

## Install

```sh
cargo add pglite-oxide serde_json
```

Default features include bundled runtime assets and cache warming hooks.
Size-sensitive builds can opt out with `default-features = false` and provide
runtime assets explicitly.

## Direct Embedded API

Use `Pglite` when your Rust code owns the database calls.

```rust,no_run
use pglite_oxide::Pglite;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Pglite::open("./.pglite")?;

    db.exec("CREATE TABLE IF NOT EXISTS items(value TEXT)", None)?;
    db.query("INSERT INTO items(value) VALUES ($1)", &[json!("alpha")], None)?;

    let result = db.query("SELECT value FROM items", &[], None)?;
    println!("{:?}", result.rows);

    db.close()?;
    Ok(())
}
```

For tests, use `Pglite::temporary()?`. Temporary databases clone a process-local
template cluster, so repeated tests avoid fresh `initdb` work.

## PostgreSQL Client URI

Use `PgliteServer` when an existing library expects a PostgreSQL URL. Configure
client pools with one connection because the embedded runtime owns one backend.

For SQLx:

```sh
cargo add sqlx --features postgres,runtime-tokio
cargo add tokio --features macros,rt-multi-thread
```

```rust,no_run
use pglite_oxide::PgliteServer;
use sqlx::{Connection, Row};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.database_url()).await?;

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

For non-Rust tests:

```sh
pglite-proxy --temporary --tcp 127.0.0.1:0 --print-uri
```

Use the printed URL with your normal Postgres client library.

## How It Works

`pglite-oxide` is moving to a WASIX dynamic-linking asset pipeline. CI owns the
expensive work: building PGlite, extension side modules, `pg_dump`, and
target-specific Wasmer LLVM AOT artifacts. Application code gets a small API,
preload hooks, and package-managed assets.

The pgvector path is staged in that pipeline and will become public only after
the Rust runtime passes `CREATE EXTENSION vector`, vector insert, and distance
query smoke tests.

## Docs

- [Usage guide](https://github.com/f0rr0/pglite-oxide/blob/main/docs/USAGE.md)
- [Extensions](https://github.com/f0rr0/pglite-oxide/blob/main/docs/EXTENSIONS.md)
- [Runtime and performance notes](https://github.com/f0rr0/pglite-oxide/blob/main/docs/RUNTIME.md)
- [Performance plan](https://github.com/f0rr0/pglite-oxide/blob/main/docs/PERFORMANCE.md)
- [pg_dump status](https://github.com/f0rr0/pglite-oxide/blob/main/docs/PG_DUMP.md)
- [Tauri usage](https://github.com/f0rr0/pglite-oxide/blob/main/docs/TAURI.md)
- [Tauri SQLx profiler example](https://github.com/f0rr0/pglite-oxide/blob/main/examples/tauri-sqlx-vanilla)
- [Development guide](https://github.com/f0rr0/pglite-oxide/blob/main/docs/DEVELOPMENT.md)
- [Runtime asset provenance](https://github.com/f0rr0/pglite-oxide/blob/main/docs/ASSETS.md)
- [Release process](https://github.com/f0rr0/pglite-oxide/blob/main/docs/RELEASE.md)
