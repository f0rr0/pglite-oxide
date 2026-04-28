#!/usr/bin/env sh
set -eu

mode="${1:-pre-push}"
root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

run_prek() {
  require prek
  stage="${1:?run_prek requires a stage}"
  shift
  run prek run --all-files --stage "$stage" "$@"
}

clean_package_artifacts() {
  rm -f target/package/*.crate
}

published_packages() {
  internal_packages
  printf '%s\n' pglite-oxide
}

internal_packages() {
  printf '%s\n' \
    pglite-oxide-assets \
    pglite-oxide-aot-aarch64-apple-darwin \
    pglite-oxide-aot-x86_64-apple-darwin \
    pglite-oxide-aot-x86_64-unknown-linux-gnu \
    pglite-oxide-aot-aarch64-unknown-linux-gnu \
    pglite-oxide-aot-x86_64-pc-windows-msvc
}

run_root_release_check() {
  printf '\n==> %s\n' "$*"
  tmp="$(mktemp)"
  if "$@" >"$tmp" 2>&1; then
    cat "$tmp"
    rm -f "$tmp"
    return 0
  fi
  status=$?
  cat "$tmp" >&2
  if grep -q 'no matching package named `pglite-oxide-assets` found' "$tmp"; then
    echo "warning: skipping root crate release check until internal crates exist in crates.io" >&2
    rm -f "$tmp"
    return 0
  fi
  rm -f "$tmp"
  return "$status"
}

case "$mode" in
  commit-msg)
    require prek
    run prek run --stage commit-msg --commit-msg-filename "${2:?commit-msg mode requires a message file}"
    ;;

  pre-commit)
    run_prek pre-commit
    ;;

  pre-push)
    run_prek pre-push
    ;;

  ci)
    require cargo
    require npm
    require prek
    run prek validate-config prek.toml
    run scripts/check-no-legacy-runtime.sh
    run scripts/validate.sh pre-commit
    run scripts/validate.sh pre-push
    run cargo check --workspace --all-targets --locked
    run cargo check --workspace --no-default-features --all-targets --locked
    run cargo test --doc --workspace --locked
    if command -v cargo-nextest >/dev/null 2>&1; then
      run cargo nextest run --workspace --all-targets --locked
    else
      run cargo test --workspace --all-targets --locked
    fi
    run cargo check --manifest-path examples/tauri-sqlx-vanilla/src-tauri/Cargo.toml --locked
    run npm --prefix examples/tauri-sqlx-vanilla ci
    run npm --prefix examples/tauri-sqlx-vanilla run build
    ;;

  release)
    require cargo
    clean_package_artifacts
    for package in $(internal_packages); do
      run cargo package -p "$package" --locked --no-verify --allow-dirty
    done
    run_root_release_check cargo package -p pglite-oxide --locked --no-verify --allow-dirty
    run scripts/check-crate-size.sh --enforce
    for package in $(internal_packages); do
      run cargo publish -p "$package" --dry-run --locked --allow-dirty
    done
    run_root_release_check cargo publish -p pglite-oxide --dry-run --locked --allow-dirty
    ;;

  *)
    cat >&2 <<'MSG'
usage: scripts/validate.sh <mode>

modes:
  commit-msg <file>  validate a Conventional Commit message with prek
  pre-commit         run all pre-commit prek hooks
  pre-push           run all pre-push prek hooks
  ci                 full source, test, lint, docs, and example checks
  release            crates.io publish dry-run and strict package size
MSG
    exit 2
    ;;
esac
