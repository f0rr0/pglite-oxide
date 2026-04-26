# Release Process

`pglite-oxide` publishes source crates to crates.io with release-plz. The CLI
binaries in this repository are maintenance helpers, so the release path
deliberately avoids binary artifact tooling such as cargo-dist until there is a
user-facing binary to distribute.

## One-time setup

- Ensure the crate owner has crates.io publish rights for `pglite-oxide`.
- `pglite-oxide@0.1.0` is already on crates.io, so future releases use crates.io
  Trusted Publishing. Configure `f0rr0/pglite-oxide`, workflow
  `.github/workflows/release.yml`, and environment `crates-io` in the crates.io
  trusted publisher settings.
- Repository Actions settings must allow GitHub Actions to create pull requests.
- The `Release` workflow needs `contents: write`, `pull-requests: write`, and
  `id-token: write`; these are already declared in the workflow.
- Do not set `package.publish = ["crates-io"]`; crates.io is Cargo's default
  registry, and release-plz treats `package.publish` entries as named alternate
  registries.

## Release intent

release-plz uses Conventional Commits as the release changeset. PRs that touch
release-affecting package files must use one of these PR title types:

- `feat:` for user-facing additions
- `fix:` for behavior fixes
- `perf:` for performance improvements
- `refactor:` for behavior-preserving package changes that still need a release
- `revert:` for reverted release-affecting changes
- any type with `!` for breaking changes

Docs, CI, issue-template, and other repository-only changes may use non-release
types such as `docs:`, `ci:`, `chore:`, `style:`, or `test:`. The CI release
intent check treats these paths as release-affecting: `Cargo.toml`, `Cargo.lock`,
`build.rs`, `src/**`, `assets/**`, `examples/**`, and `benches/**`.

## Releasing from main

1. Merge release-worthy work to `main`.
2. Open GitHub Actions, run `Release` from `main`, and choose
   `prepare-release-pr`.
3. Review and merge the release-plz PR. It updates `Cargo.toml`, `Cargo.lock`,
   and `CHANGELOG.md`.
4. Run `Release` from `main` with `publish-dry-run`.
5. If the dry run passes, run `Release` again with `publish`.

release-plz publishes unpublished package versions to crates.io, creates the bare
SemVer tag such as `0.2.0`, and creates the GitHub release from the generated
changelog. Bare SemVer tags intentionally match the existing `0.1.0` tag.
