# Runtime Assets

`pglite-oxide` ships a pinned WASIX runtime tree and target-specific Wasmer LLVM
AOT artifacts. End users depend only on `pglite-oxide`; the published asset and
AOT crates are internal packaging units required by crates.io.

Source/build-spine work is Phase 1 of
[WASIX_WASMER_ROADMAP.md](WASIX_WASMER_ROADMAP.md). Asset generation should be
implemented in that order before broader extension or release work.
The no-compromise completion gates live in
[PHASE_1_2_COMPLETENESS.md](PHASE_1_2_COMPLETENESS.md).

## Package Model

- `pglite-oxide-assets` contains the portable WASIX runtime archive,
  deterministic extension archives, `pg_dump`, a prepopulated PGDATA template,
  and generated metadata.
- `pglite-oxide-aot-*` crates contain target-specific serialized Wasmer LLVM
  artifacts. Normal user builds load these with headless Wasmer and do not build
  LLVM locally.
- `assets/sources.toml` pins upstream repositories, commits, toolchain versions,
  Docker image digests, and expected hashes used by asset CI.
- The WASIX build Dockerfile pins the Ubuntu base image by digest. `xtask
  assets check` verifies that the Dockerfile and `assets/sources.toml` use the
  same digest so local and CI builds do not silently drift.

The runtime installs only requested extensions into each database root. Asset
packs may contain many supported extensions, but per-instance extraction remains
demand-driven.

## Current Metadata

This section records the intended source baseline. The checked-in generated
asset manifest must be regenerated from `assets/sources.toml` before any release;
CI should fail if the manifest branch/commit, hashes, or Wasmer engine identity
do not match the source pins.

- PostgreSQL runtime: `17.5`
- Upstream Postgres/PGlite foundation: `electric-sql/postgres-pglite`
  `REL_17_5_WASM-pglite-builder`
- Comparison branch: `electric-sql/postgres-pglite` `REL_17_5-pglite`
- Build evidence branch: `electric-sql/pglite-build` `portable`
- Build evidence commit: `c195113dbaf09488f8d5eeb2db91dacd123b74d0`
- Runtime archive SHA-256:
  `acb15508381101bcb5a25ef58d7150b2aa4fc6c4f79da2329e6876a7f1db2265`
- Runtime executable SHA-256:
  `8043b5b330722c02ef4f36b491cceec8d12e524b800f291594f44cc9e27e264e`
- PGDATA template archive SHA-256:
  `a0a91f4fbd0428787ce78b351ee84f0c33f9ce8578448701b0f6080f7d8b052e`
- `vector` extension archive SHA-256:
  `b1f18a593229a2ada8e45809a86b8db3054ec90059556f0de6eb55de7c0e2adb`
- `pg_trgm` extension archive SHA-256:
  `b3da404d2c4d662b5995c8af1c1a17c69dacf97a2f5dacaf7a6b906d46f0dd89`
- `pg_dump` module SHA-256:
  `30ff71f1dd82b164ce2a4f595018217f3de7f946c297f0deba0f66f51127193e`

## Update Checklist

1. Update `assets/sources.toml` with exact upstream pins and expected hashes.
2. Build the WASIX runtime, extensions, and `pg_dump` from the pinned source set.
3. Generate deterministic `.tar.zst` extension archives and asset manifests.
4. Generate target-specific Wasmer LLVM AOT artifacts on native CI runners.
5. Run extension smoke tests; expose constants only for passing extensions.
6. Package compiled timezone data produced inside the pinned Docker build from
   PostgreSQL's pinned `src/timezone/data/tzdata.zi`. Do not run a host-local
   `zic` during packaging; the cross-built tree's `zic` is a WASIX binary and
   is not executable on the maintainer host.
7. Verify the generated manifest records Wasmer version, WASIX toolchain,
   engine identity, target triple, `dylink.0`, import/export sets, source pins,
   tzdata/zic provenance, and extension startup requirements.
8. Run package-size gates for every published asset and AOT crate.
9. Run `scripts/validate.sh ci` and `scripts/validate.sh release`.

## Maintainer Commands

Validate source pins, canonical path settings, `.gitmodules`, and the local
source checkout:

```sh
cargo run -p xtask -- assets check --strict-local
```

Check whether the preserved WASIX patch applies to the active
`postgres-pglite` checkout, and whether the builder branch still contains the
expected `pglite-build` build-script spine:

```sh
cargo run -p xtask -- assets source-spine --check-patch-applies
```

Audit which upstream `postgres-pglite` fixes are included in the active builder
branch and which required stable-branch fixes are ported or explicitly replaced
by the WASIX architecture:

```sh
cargo run -p xtask -- assets audit-upstream --strict
```

The audit details live in [UPSTREAM_AUDIT.md](UPSTREAM_AUDIT.md).

Check generated asset metadata against `assets/sources.toml` before release:

```sh
cargo run -p xtask -- assets check --strict-generated
```

Run the production source-spine build through `xtask`:

```sh
cargo run -p xtask -- assets build --profile release --target-triple x86_64-unknown-linux-gnu --execute
```

Do not run the production build wrappers directly for release assets. They live
under `assets/wasix-build` so `xtask` can orchestrate and guard them. A
successful build writes `assets/wasix-build/build/outputs.json` with the
produced module paths, hashes, imports, exports, memories, and `dylink.0`
metadata. Follow the build command with `assets package` and `assets aot` to
refresh deterministic archives, manifests, and target-specific AOT artifacts.

For release CI, use the orchestration command so source validation, build,
dynamic-link closure checks, packaging, AOT generation, manifest validation, and
package-size gates stay coupled:

```sh
cargo run -p xtask -- assets release-build --profile release --target-triple x86_64-unknown-linux-gnu --fetch
```

The root crate excludes `assets/checkouts/**` from published packages. The
checkouts are maintainer/CI build inputs, not end-user assets.
