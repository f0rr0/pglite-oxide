# pglite-oxide Product Plan

This is the current plan after auditing the abandoned extensions/assets implementation and checking upstream PGlite, py-pglite, pglite-build, pglite-bindings, pglite-go, and pdo-pglite.

## Product Story

`pglite-oxide` should be positioned as embedded Postgres for Rust tests and local apps, with a local Postgres server mode for any language or Postgres client.

Recommended GitHub description:

> Embedded Postgres for Rust tests and local apps. No Docker, works with SQLx and any Postgres client.

Do not lead with "PGlite WASI runtime". Average developers should first see:

- fast Rust test databases without Docker
- local app storage using real Postgres semantics
- SQLx, tokio-postgres, Diesel, SeaORM, and Tauri examples
- `pglite-proxy` for Python, Go, Node, and other clients
- pgvector/local RAG once the runtime proof passes

## Corrected Extension Finding

The abandoned implementation proved that bundling extension archives is not enough.

`pgvector` works in upstream JS PGlite and py-pglite because those projects use the JS/Emscripten runtime path. JS PGlite preloads `.so` extension side modules through Emscripten before Postgres calls `dlopen`. py-pglite shells out to Node and uses `@electric-sql/pglite`, so it inherits that path.

Our current Rust path embeds `pglite.wasi` through Wasmtime. The published WASI artifact currently has `dlopen`/`dlsym` stubs for built-in symbols such as `plpgsql` and `dict_snowball`, but not `pgvector`.

Observed evidence:

- `CREATE EXTENSION IF NOT EXISTS "vector"` fails in `fetch_finfo_record` / `fmgr_c_validator`.
- `vector.so` contains `pg_finfo_vector_*` and `vector_*` symbols.
- the packaged `pglite.wasi` contains no `pg_finfo_vector*`, `vector_in`, `vector_out`, `pgvector`, or `/vector.so` strings.
- `pglite-build` has notes for a static vector WASI rebuild, but its WASI extension linking path is still marked TODO.

Conclusion: do not ship public `extensions::VECTOR`, pgvector docs, or a default `extensions` feature until a real runtime-level pgvector test passes in Rust.

## Extension Strategy

Use all-or-nothing behavior for extensions. We should not monkey patch a partial asset install path and imply general extension support.

Pick one implementation model before public API:

1. Static-linked WASI runtime
   - Build `pglite.wasi` with the supported extensions statically linked.
   - Generate the WASI `dlsym` table from the linked extension symbols.
   - Package the matching SQL/control files in the runtime tree.
   - This is likely the fastest path for first-class pgvector.

2. Real dynamic side-module loading under Wasmtime
   - Support loading extension `.so`/WASM modules at runtime.
   - This is closer to JS PGlite behavior, but it is a larger runtime project.

Until one path is proven:

- keep extension APIs out of public releases
- keep pgvector examples clearly marked blocked or out of README
- keep any archived extension assets as internal research artifacts only

Acceptance tests required before public extension API:

- direct `Pglite` can `CREATE EXTENSION vector`
- direct `Pglite` can create a `vector(3)` column and query distance
- `PgliteServer` plus SQLx can use `vector`
- runtime artifact inspection confirms expected vector symbols or dynamic loader support
- extension installation rejects unsafe archive paths if external archives are used

## Package And Asset Model

Only convert to a workspace when there is a real second package to publish.

If extensions or tools need published assets:

- workspace crates:
  - `pglite-oxide`: public user-facing crate
  - `pglite-oxide-assets`: internal asset crate, published only because crates.io requires dependencies to be published
  - optional unpublished `xtask` for asset sync and verification
- core dependency:
  ```toml
  pglite-oxide-assets = { version = "=0.4.0", path = "crates/assets", optional = true }
  ```
- release-plz should update exact internal versions.

If external extension archives are still needed:

- repack upstream `.tar.gz` archives into deterministic `.tar.zst`
- validate SHA256 and sizes in tests
- include generated metadata:
  - upstream commit
  - archive SHA256
  - compressed size
  - extension SQL names
  - asset filename

If static-linked runtime is chosen, prefer packaging the matching runtime tree rather than shipping separate extension archives that users think are dynamically loaded.

## Feature Model

Once runtime support is proven, keep the end-user install simple:

```toml
pglite-oxide = "0.4"
```

Target feature model:

```toml
[features]
default = ["runtime-cache", "extensions"]
runtime-cache = ["wasmtime/cache"]
extensions = ["dep:pglite-oxide-assets"]
```

If `extensions` cannot be made true by default with working tests and crates.io package limits, do not expose the feature yet.

## Public API Target

After runtime proof:

```rust
use pglite_oxide::{extensions, Pglite};

let mut db = Pglite::builder()
    .temporary()
    .extension(extensions::VECTOR)
    .open()?;
```

Add:

- `PgliteBuilder::extension(extension)`
- `PgliteBuilder::extensions(iter)`
- `Pglite::enable_extension(extension)`
- `PgliteServerBuilder::extension(extension)`
- `PgliteServerBuilder::extensions(iter)`
- `PgliteServer::database_url()` as an alias for `connection_uri()`

Use generated constants rather than a large hand-written enum:

```rust
extensions::VECTOR
extensions::PG_TRGM
extensions::HSTORE
extensions::ALL
```

If the `extensions` feature is disabled, extension APIs should be absent and docs should show the lean install path.

## pg_dump

Do not expose public pg_dump APIs until the runner is proven with a dump/restore integration test.

Target API after proof:

```rust
let sql = db.dump_sql(PgDumpOptions::default())?;
restored.exec(&sql, None)?;
```

Add:

- `PgDumpOptions`
- `Pglite::dump_sql(options)`
- `Pglite::dump_bytes(options)`

Target CLI:

```sh
pglite-dump --root ./.pglite > dump.sql
pglite-dump --root ./.pglite -- --schema-only > schema.sql
```

Use upstream pg_dump defaults aligned with PGlite tools:

- plain SQL
- `--inserts`
- `-j 1`
- user `postgres`
- internal `/tmp/out.sql`

If `pg_dump.wasm` cannot be driven from Rust without JS glue, keep the asset hidden and do not expose public pg_dump APIs.

## pglite-proxy

Make `pglite-proxy` the cross-language test entry point:

```sh
pglite-proxy --temporary --tcp 127.0.0.1:0 --print-uri
```

Flags:

- `--temporary`
- `--root <path>`
- `--tcp <addr>`
- `--unix <path>`
- `--print-uri`
- `--extension <name>` only after extension runtime proof

Python, Go, Node, and other languages should use the printed Postgres URL with normal client libraries.

## Examples

First-class examples to add:

- SQLx test fixture
- tokio-postgres test fixture
- rstest fixture
- Diesel test
- SeaORM test
- Tauri local app
- Python pytest + psycopg through `pglite-proxy`
- Go test + pgx through `pglite-proxy`
- Node test through `pglite-proxy`
- pgvector local RAG with deterministic embeddings, after runtime proof
- Rig + pgvector only if dependency stability is acceptable

Testing PMF should be the near-term focus. py-pglite gained traction by making test databases easy; `pglite-oxide` can do the same for Rust and any language that can consume a Postgres URL.

## README Shape

Rewrite the first screen around concrete use cases:

1. tagline and one short install block
2. Rust direct API quick start
3. SQLx quick start through `PgliteServer`
4. `pglite-proxy` quick start for any Postgres client
5. testing framework examples
6. local apps and Tauri
7. pgvector/local RAG only after runtime proof
8. "How it works" later, where WASI/Wasmtime details belong

Avoid verbose descriptions. The README should let a developer understand the value in under 30 seconds.

## CI And Release

Keep release-plz as release owner.

Workspace-aware commands once the repo becomes a workspace:

```sh
cargo check --workspace --all-targets --locked
cargo check --workspace --no-default-features --all-targets --locked
cargo test --doc --workspace --locked
cargo nextest run --workspace --all-targets --locked
cargo hack check --workspace --feature-powerset --no-dev-deps
```

Package checks:

```sh
cargo package -p pglite-oxide --locked --no-verify
cargo package -p pglite-oxide-assets --locked --no-verify
cargo publish -p pglite-oxide-assets --dry-run --locked
cargo publish -p pglite-oxide --dry-run --locked
```

Also add:

- per-crate compressed package size checks under crates.io's 10 MB limit
- `cargo-nextest` in CI and pre-push
- conventional commit PR-title enforcement
- workspace-aware release intent checks via `cargo metadata`
- changelog validation that understands package-scoped tags
- trusted publishing for every published crate
- release-plz pinned to a full commit SHA

## Changelog

Use one root `CHANGELOG.md`.

If an asset crate is added:

```toml
[[package]]
name = "pglite-oxide"
version_group = "pglite-oxide"
changelog_path = "CHANGELOG.md"
changelog_include = ["pglite-oxide-assets"]
git_release_enable = true
git_tag_name = "{{ package }}-v{{ version }}"

[[package]]
name = "pglite-oxide-assets"
version_group = "pglite-oxide"
changelog_update = false
git_release_enable = false
git_tag_enable = false
semver_check = false
```

Set `dependencies_update = true`.

## Immediate Next Work

1. Build or obtain a WASI runtime with static pgvector support.
2. Add a minimal local runtime acceptance test:
   - `CREATE EXTENSION vector`
   - insert vectors
   - distance query
3. Inspect the runtime artifact for vector symbols or verified dynamic loader support.
4. Only after that, reintroduce the extension API, docs, and packaging changes.
5. Keep proxy/test examples moving independently because they do not depend on pgvector.
