#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

base_ref="${ASSET_INPUT_BASE_REF:-}"
if [[ -z "$base_ref" ]]; then
  if git rev-parse --verify -q '@{upstream}' >/dev/null; then
    base_ref='@{upstream}'
  else
    base_ref='origin/main'
  fi
fi

if ! git rev-parse --verify -q "${base_ref}^{commit}" >/dev/null; then
  echo "asset input fingerprint check skipped: ${base_ref} is not available" >&2
  exit 0
fi

changed="$(
  git diff --name-only "${base_ref}...HEAD" -- \
    assets/sources.toml \
    assets/extensions.promoted.toml \
    assets/extensions.smoke.toml \
    assets/wasix-build \
    crates/assets/Cargo.toml \
    crates/assets/build.rs \
    crates/assets/src \
    crates/aot \
    xtask/src/main.rs \
    xtask/src/extension_catalog.rs \
    assets/generated/asset-inputs.sha256
)"

if [[ -z "$changed" ]]; then
  echo "asset input fingerprint check skipped: no asset input changes"
  exit 0
fi

cargo run -p xtask -- assets verify-committed
