# Development

Run the local gates before opening a PR:

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

Install the supply-chain gate when needed:

```sh
cargo install cargo-deny --locked
```

`tests/runtime_smoke.rs` starts the real WASM backend and is intentionally
slower than the protocol unit tests.

## Maintenance Utilities

The repository includes maintenance binaries:

- `pglite-dump` expands the bundled filesystem manifest/runtime assets.
- `pglite-manifest-sync` syncs `assets/pglite_fs_manifest.json` from the
  `pglite.js` bundle published on `electric-sql/pglite-build` `gh-pages`.
- `pglite-proxy` exposes a local PostgreSQL socket backed by the embedded
  runtime.

Release process details are tracked in [RELEASE.md](RELEASE.md).
