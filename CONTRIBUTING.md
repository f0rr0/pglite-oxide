# Contributing

## Local Checks

Run the same gates as CI before opening a PR:

```sh
scripts/validate.sh ci
scripts/validate.sh release
cargo deny check
```

The runtime smoke starts embedded Postgres and is intentionally slower than unit tests.

Install local hooks with:

```sh
scripts/install-hooks.sh
```

Hooks stay deliberately smaller than CI: pre-commit handles file hygiene and
formatting, while pre-push runs whitespace diff checking,
`cargo clippy --all-targets`, and `cargo test --all-targets`. CI repeats those
hook checks and remains the source of truth for no-default builds, docs,
packaging, Tauri, frontend, feature combinations, public API compatibility, and
supply-chain checks.

In GitHub branch protection, require the aggregate `Required checks` status and
the Conventional Commit status before merging. Local hooks are convenience
checks and can be skipped; CI is authoritative.

## Assets

Bundled runtime assets must stay aligned with `docs/ASSETS.md`. If the WASI runtime
changes, update the asset metadata in `Cargo.toml` and run the full local checks.

## Releases

Releases are manual and must be dispatched from `main` through the GitHub
Actions `Release` workflow. release-plz owns version bumps, changelog updates,
tags, GitHub releases, and crates.io publishing. See `docs/RELEASE.md` for the
release-intent, Trusted Publishing, and manual workflow details.
