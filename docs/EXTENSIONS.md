# Extensions

Bundled extensions are enabled explicitly once they pass the Rust SQL smoke
suite:

```rust
use pglite_oxide::{Pglite, extensions};

let mut db = Pglite::builder()
    .temporary()
    .extension(extensions::SOME_EXTENSION)
    .open()?;
```

`extensions::ALL` lists every extension that passed the CI smoke suite for the
current asset manifest. Public constants are generated from the manifest shape
so new extensions can be added without changing the builder API.

The runtime installs only requested archives into the database root. Archives
are unpacked through the same path-safety checks as the runtime archive and
support both the current `.tar.gz` bootstrap format and the planned
deterministic `.tar.zst` format.

## Current Public Extensions

None yet.

The staged asset manifest includes pgvector metadata and archive bytes, but the
current root runtime is still the legacy WASI bootstrap. Local validation shows
`CREATE EXTENSION vector` fails in `fmgr_c_validator` there. The public
`extensions::VECTOR` constant should only be generated after the WASIX/Wasmer
runtime path passes `CREATE EXTENSION vector`, vector insert, and vector
distance-query smoke tests from Rust.

## Asset Pipeline

The production pipeline builds portable WASIX modules and target-specific
Wasmer LLVM AOT artifacts from `assets/sources.toml`. Runtime assets live in
`pglite-oxide-assets`; target-specific AOT artifacts live in
`pglite-oxide-aot-*` crates. These crates are implementation details.
