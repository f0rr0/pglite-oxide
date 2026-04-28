# pglite-oxide Product Plan

`pglite-oxide` is embedded Postgres for Rust tests and local apps, with a local
server mode for any Postgres client. The product story should stay concrete:
no Docker, real Postgres semantics, SQLx/tokio-postgres/Diesel/SeaORM support,
Tauri/local-app storage, and pgvector for local RAG.

The ordered execution plan lives in
[WASIX_WASMER_ROADMAP.md](WASIX_WASMER_ROADMAP.md). Use that roadmap for
implementation order; this document records the product shape.
Runtime correctness coverage and upstream test-porting notes live in
[TESTING.md](TESTING.md).

## Architecture

The production runtime path is WASIX dynamic linking plus Wasmer LLVM AOT:

- build PGlite from pinned `postgres-pglite` builder sources
- build the main runtime as a dynamic WASIX module
- build SQL extensions as matching WASIX side modules
- build `pg_dump` from the same tree
- precompile target-specific Wasmer LLVM AOT artifacts in CI
- load artifacts through headless Wasmer in user applications

The source baseline should be `electric-sql/postgres-pglite`
`REL_17_5_WASM-pglite-builder`, with the plain `REL_17_5-pglite` branch kept as
a comparison reference. The builder branch already carries the upstream
extension catalog and symbol-export pipeline; our work is to make that path
WASIX-native and CI-owned.

Normal users do not install Docker, LLVM, or a Wasmer compiler backend. If the
host target has no published AOT pack, opening a database returns a clear
unsupported-target error.

## Workspace Model

The repo is a Rust workspace:

- `pglite-oxide`: public user-facing crate at the repository root
- `pglite-oxide-assets`: internal portable runtime and extension assets
- `pglite-oxide-aot-*`: internal target-specific AOT artifact packs
- `xtask`: unpublished build, manifest, size, smoke-test, and perf tooling
- `spikes/`: excluded historical research and upstream checkouts

Users should depend only on:

```toml
pglite-oxide = "0.4"
```

Default features remain small:

```toml
[features]
default = ["runtime-cache", "extensions"]
runtime-cache = []
extensions = ["asset and target AOT packs"]
```

If a pack exceeds crates.io's 10 MiB package limit, `xtask` should split it
deterministically while preserving the public `extensions` feature.

## Extensions

Extension support must be generated from smoke-tested assets. Public constants
exist only for extensions that pass Rust integration tests on the target runtime.

Target sources:

- PGlite's extension catalog and REPL exports
- `postgres-pglite/pglite/other_extensions`
- `postgres-pglite` `REL_17_5_WASM-pglite-builder` `pglite/Makefile`
- supported Postgres contrib directories
- pinned external extension repositories such as pgvector, pgtap, pg_uuidv7,
  pg_hashids, pg_ivm, age, postgis, and pg_textsearch

`live` is not a SQL extension and should not be exposed as an extension constant
until there is a Rust-native live-query API.

Public shape:

```rust
use pglite_oxide::{extensions, Pglite};

let mut db = Pglite::builder()
    .temporary()
    .extension(extensions::VECTOR)
    .open()?;

db.enable_extension(extensions::PG_TRGM)?;
Pglite::preload()?;
Pglite::preload_extensions([extensions::VECTOR])?;
```

Builder and server APIs should support single extensions and iterators, and
`PgliteServer::database_url()` should remain an alias for `connection_uri()`.

## pg_dump

`pglite-dump` should be a real logical dump tool driven by the WASIX
`pg_dump` module. Public API lands only after dump/restore passes in CI:

```rust
let sql = db.dump_sql(PgDumpOptions::default())?;
restored.exec(&sql, None)?;
```

CLI target:

```sh
pglite-dump --root ./.pglite > dump.sql
pglite-dump --root ./.pglite -- --schema-only > schema.sql
```

Use plain SQL, `--inserts`, `-j 1`, user `postgres`, and an internal
`/tmp/out.sql` output path unless upstream behavior changes.

## Cross-Language Tests

`pglite-proxy` is the entry point for Python, Go, Node, and other languages:

```sh
pglite-proxy --temporary --tcp 127.0.0.1:0 --print-uri
```

Required flags:

- `--temporary`
- `--root <path>`
- `--tcp <addr>`
- `--unix <path>`
- `--print-uri`
- `--extension <name>`

Language examples should consume the printed Postgres URL with standard client
libraries.

## Examples

First-class examples:

- SQLx test fixture
- tokio-postgres test fixture
- rstest fixture
- Diesel test
- SeaORM test
- Tauri local app
- pgvector local RAG with deterministic embeddings
- Python pytest plus psycopg through `pglite-proxy`
- Go test plus pgx through `pglite-proxy`
- Node plus `pg` through `pglite-proxy`

Testing is the near-term product wedge because it gives developers a fast,
observable win without asking them to redesign their application.

## CI and Release

CI must fail if the legacy runtime path returns outside `spikes/`.

Normal CI:

```sh
cargo check --workspace --all-targets --locked
cargo check --workspace --no-default-features --all-targets --locked
cargo test --doc --workspace --locked
cargo nextest run --workspace --all-targets --locked
cargo hack check --workspace --feature-powerset --no-dev-deps
```

Release checks:

```sh
cargo package -p pglite-oxide --locked --no-verify
cargo publish -p pglite-oxide --dry-run --locked
```

Run the same package and dry-run checks for every internal published asset and
AOT crate. Release-plz owns versions, tags, one root changelog, exact internal
dependency versions, and grouped releases. Trusted Publishing should be
configured for every published crate after first manual publish.
