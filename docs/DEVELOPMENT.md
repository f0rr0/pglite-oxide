# Development

Run the local gates before opening a PR:

```sh
scripts/validate.sh ci
scripts/validate.sh release
cargo deny check
```

The hook split is intentionally small:

- pre-commit: file hygiene and formatting
- pre-push: whitespace diff check, `cargo clippy --all-targets`, and
  `cargo test --all-targets`
- CI/release: the hook checks plus no-default build, doctests, Tauri example,
  frontend build, workflow linting, feature powerset, public API compatibility,
  crate packaging, publish dry-run, and supply-chain policy

Install local hooks and the supply-chain gate when needed:

```sh
scripts/install-hooks.sh
cargo install cargo-deny --locked
```

`tests/runtime_smoke.rs` starts the real WASM backend and is intentionally
slower than the protocol unit tests.

## Maintenance Utilities

The repository includes maintenance commands:

- `pglite-dump` expands the bundled runtime archive for inspection.
- `pglite-proxy` exposes a local PostgreSQL socket backed by the embedded
  runtime.
- `cargo run --example build_pgdata_template` regenerates the bundled
  prepopulated PGDATA template.

Release process details are tracked in [RELEASE.md](RELEASE.md).
