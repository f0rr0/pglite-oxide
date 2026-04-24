# Contributing

## Local Checks

Run the same gates as CI before opening a PR:

```sh
cargo fmt --all --check
cargo check --all-targets
cargo check --no-default-features --all-targets
cargo clippy --all-targets -- -D warnings
cargo deny check
cargo test --lib --bins
cargo test --doc
cargo test --test runtime_smoke -- --nocapture
cargo test --test proxy_smoke -- --nocapture
cargo test --test client_compat -- --nocapture
cargo package --locked --allow-dirty
```

The runtime smoke starts embedded Postgres and is intentionally slower than unit tests.

## Assets

Bundled runtime assets must stay aligned with `docs/ASSETS.md`. If the WASI runtime
changes, update the asset metadata in `Cargo.toml` and run the full local checks.

## Releases

Releases are manual and must be dispatched from `main` through the GitHub
Actions `Release` workflow. release-plz owns version bumps, changelog updates,
tags, GitHub releases, and crates.io publishing. See `docs/RELEASE.md` for the
release-intent, Trusted Publishing, and manual workflow details.
