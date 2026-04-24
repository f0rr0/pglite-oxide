## Summary

## Release Intent

- [ ] Package/API/runtime change: PR title uses `feat:`, `fix:`, `perf:`, `refactor:`, `revert:`, or a breaking `!`.
- [ ] Docs/CI/repository-only change: no release intended.

## Verification

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --doc`
- [ ] `cargo test --test runtime_smoke -- --nocapture`
