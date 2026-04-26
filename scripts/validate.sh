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
  rm -f target/package/pglite-oxide-*.crate
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
    run scripts/validate.sh pre-commit
    run scripts/validate.sh pre-push
    run cargo check --no-default-features --all-targets --locked
    run cargo test --doc --locked
    run cargo check --manifest-path examples/tauri-sqlx-vanilla/src-tauri/Cargo.toml --locked
    run npm --prefix examples/tauri-sqlx-vanilla ci
    run npm --prefix examples/tauri-sqlx-vanilla run build
    ;;

  release)
    require cargo
    clean_package_artifacts
    run cargo package --locked --no-verify
    run scripts/check-crate-size.sh --enforce
    run cargo publish --dry-run --locked
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
