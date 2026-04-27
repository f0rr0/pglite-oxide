# Release Process

`pglite-oxide` publishes source crates to crates.io with release-plz. The CLI
binaries in this repository are maintenance helpers, so the release path
deliberately avoids binary artifact tooling such as cargo-dist until there is a
user-facing binary to distribute.

## One-time setup

- Ensure the crate owner has crates.io publish rights for `pglite-oxide`.
- The crate already exists on crates.io. Future releases use crates.io Trusted
  Publishing. Configure `f0rr0/pglite-oxide`, workflow
  `.github/workflows/release.yml`, and environment `crates-io` in the crates.io
  trusted publisher settings.
- Do not configure `CARGO_REGISTRY_TOKEN`; the release workflow relies on the
  GitHub OIDC token granted by `id-token: write`.
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

Package version bumps are release-plz owned. Feature and fix PRs may change
package code, dependencies, and assets, but they must not change the root
`Cargo.toml` package version. The version bump and matching `CHANGELOG.md`
section must come from a `release-plz-*` PR titled `chore(release): ...`.

## Releasing from main

1. Merge release-worthy work to `main`.
2. Open GitHub Actions, run `Release` from `main`, and choose
   `prepare-release-pr`.
3. Review and merge the release-plz PR. It updates `Cargo.toml`, `Cargo.lock`,
   and `CHANGELOG.md`.
4. Run `Release` from `main` with `publish-dry-run`.
5. If the dry run passes, run `Release` again with `publish`.

The manual publish job uses `release_always = true` because the workflow is not
triggered on every merge; it only runs when a maintainer explicitly selects a
publish operation. The job fails if release-plz reports that it created no
release, so a green publish run means a crate/GitHub release was actually
produced. The dry-run and publish action steps are intentionally separate so the
real publish step omits the `dry_run` input entirely.

The publish job also validates release-note readiness before running expensive
package checks. The current root package version must be the first release
section in `CHANGELOG.md`, that section must contain release-note body content,
and the `[Unreleased]` compare link must start at that version. If this check
fails, run `prepare-release-pr` and merge the generated release-plz PR before
publishing.

release-plz publishes unpublished package versions to crates.io, creates the bare
SemVer tag such as `0.3.0`, and creates the GitHub release from the generated
changelog. Bare SemVer tags intentionally match the existing `0.1.0` and
`0.2.0` tags.
