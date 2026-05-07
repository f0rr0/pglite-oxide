# Release Process (Maintainers)

This page is maintainer documentation for versioning, release CI, and crates.io
publishing. It is not part of the end-user documentation path.

Release automation is workspace-aware. `release-plz` owns version bumps for the
root crate, `pglite-oxide-assets`, and every `pglite-oxide-aot-*` crate.
Feature PRs should not edit package versions directly.

The root crate is the only user-facing release and changelog. Asset and AOT
crate changes are included in the root `CHANGELOG.md`, while those internal
crates do not create separate GitHub releases or tags. The public Git tag stays
the bare SemVer version, for example `0.4.0`, because the internal crates are
implementation details.

Before publishing, CI packages every published crate and enforces crates.io's
10 MB compressed `.crate` limit.

Releases use source-controlled inputs plus CI-generated portable WASIX and AOT
artifacts. The git repo intentionally does not commit portable runtime blobs or
native AOT binaries. The release workflow first verifies source pins, asset
input fingerprints, extension metadata, and crate templates; then it requires a
successful `Assets` workflow for the same SHA, downloads the generated portable
and AOT artifacts, stages them into a clean release workspace, package-checks
that workspace, and only then runs release-plz.

The release source of truth is the exact Assets workflow output for the release
SHA. Portable WASIX artifacts are built from pinned sources, native AOT
artifacts are generated from that portable asset set, and release validation
checks package contents, target triples, Wasmer versions, runtime hashes, and
crate sizes before publishing. The release workflow runs source/lint/example
checks before artifact download, then reruns the Rust test gate after artifact
installation so the release host executes against materialized native artifacts
instead of compile-only tests.

Normal CI and release CI split responsibilities deliberately. Rust-only changes
download the latest compatible Assets workflow bundle, verify its asset-input
fingerprint, and run native AOT runtime tests on the supported host matrix.
Asset-producing changes run the `Assets` workflow, which rebuilds portable
WASIX from pinned sources before generating and smoking the target AOT packs.
The release workflow refuses to publish unless the generated portable and AOT
artifacts for the exact release SHA are downloaded, staged, and package-checked.

`pglite-oxide` publishes source crates to crates.io with release-plz. The CLI
binaries in this repository are maintenance helpers, so the release path
deliberately avoids binary artifact tooling such as cargo-dist until there is a
user-facing binary to distribute.

## One-time setup

- Ensure the crate owner has crates.io publish rights for `pglite-oxide`,
  `pglite-oxide-assets`, and every `pglite-oxide-aot-*` crate.
- Configure crates.io Trusted Publishing for every published crate. Use
  repository `f0rr0/pglite-oxide`, workflow `.github/workflows/release.yml`,
  and environment `crates-io`.
- Do not configure `CARGO_REGISTRY_TOKEN`; the release workflow relies on the
  GitHub OIDC token granted by `id-token: write`.
- Repository Actions settings must allow GitHub Actions to create pull requests.
- The `Release` workflow uses job-scoped permissions. The release-PR job needs
  `contents: write` and `pull-requests: write`; the publish job needs
  `contents: write` and `id-token: write`.
- If release PRs should run normal PR CI automatically, configure a
  `RELEASE_PLZ_TOKEN` secret backed by a GitHub App or maintainer bot token.
  Without it, release-plz falls back to `GITHUB_TOKEN`; GitHub does not trigger
  normal PR workflows from PRs opened by that token.
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

Docs, CI, issue-template, tests, examples, xtask-only maintenance, source
checkout scripts, and other repository-only changes may use non-release types
such as `docs:`, `ci:`, `chore:`, `style:`, or `test:`. The CI release intent
check treats these paths as release-affecting: `Cargo.toml`, `build.rs`,
`src/**`, and `crates/**`.

Package version bumps are release-plz owned. Feature and fix PRs may change
package code, dependencies, and generated assets, but they must not change
workspace package versions. The version bump and matching `CHANGELOG.md`
section must come from a `release-plz-*` PR titled `chore(release): ...`.

## Maintainer paths

- Docs-only and repository-only PRs run the lightweight repository hygiene and
  workflow checks. They do not need a release title or changelog entry.
- Test-only PRs run Rust checks but do not need a release title unless they also
  change published package code.
- Runtime, API, generated asset, and AOT crate changes are release-affecting.
  Use a release-producing PR title such as `fix:`, `feat:`, `perf:`, or
  `refactor:`.
- Source-spine and asset-build script changes are not automatically
  release-affecting until they change generated package contents under
  `src/**` or `crates/**`, but CI treats them as asset-producing changes and
  requires committed artifact verification plus the `Assets` workflow when they
  affect release artifacts.

## Releasing from main

1. Merge release-worthy work to `main`.
2. Open GitHub Actions, run `Release` from `main`, and choose
   `prepare-release-pr`.
3. Review and merge the release-plz PR. It updates `Cargo.toml`, `Cargo.lock`,
   and `CHANGELOG.md`.
4. Wait for the `Assets` workflow on `main` to pass for the release commit.
5. Run `Release` from `main` with `publish-dry-run`.
6. If the dry run passes, run `Release` again with `publish`.

For portable asset-source changes, regenerate and verify the generated artifact
set before merging:

```sh
cargo run -p xtask -- assets fetch
cargo run -p xtask --features aot-serializer -- assets build-host
cargo run -p xtask -- assets verify-committed
```

Portable WASIX and native AOT artifacts are not committed. They are produced by
the `Assets` workflow matrix and downloaded by
`.github/scripts/download-aot-artifacts.sh` during dry-run and publish jobs. The
architecture-independent PGDATA template is also generated by that workflow
from the split WASIX `initdb` module and is not checked in.
`xtask release stage` materializes those generated payloads into crate skeletons
inside `target/pglite-oxide/release/workspace`; packaging and publish dry-runs
run from that staged workspace.

The manual publish job uses `release_always = true` because the workflow is not
triggered on every merge; it only runs when a maintainer explicitly selects a
publish operation. The job fails if release-plz reports that it created no
release, so a green publish run means a crate/GitHub release was actually
produced. The dry-run operation stops after staged package validation because
same-release internal crates are not present in crates.io until the real
release-plz publish step.

The publish job also validates release-note readiness before running expensive
package checks. The current root package version must be the first release
section in `CHANGELOG.md`, that section must contain release-note body content,
and the `[Unreleased]` compare link must start at that version. If this check
fails, run `prepare-release-pr` and merge the generated release-plz PR before
publishing.

release-plz publishes unpublished package versions to crates.io, creates the
bare SemVer tag such as `0.4.0`, and creates the GitHub release from the
generated changelog. The root crate depends on internal crates with exact
versions. Plain Cargo and release-plz dry-runs cannot fully dry-run the root
crate before those exact internal versions exist in the registry, so validation
dry-runs every internal crate, enforces package sizes, attempts the root checks,
and leaves final workspace publish ordering to the real release-plz publish.
