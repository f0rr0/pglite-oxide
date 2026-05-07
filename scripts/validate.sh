#!/usr/bin/env sh
set -eu

mode="${1:-pre-push}"
shift || true

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
if [ -d "$cargo_bin" ]; then
  PATH="$cargo_bin:$PATH"
  export PATH
fi

allow_dirty=0
for arg in "$@"; do
  case "$arg" in
    --allow-dirty)
      allow_dirty=1
      ;;
    --*)
      echo "unknown flag for $mode: $arg" >&2
      exit 2
      ;;
    *)
      ;;
  esac
done

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    echo "run scripts/bootstrap-tools.sh to install the pinned local toolchain" >&2
    exit 1
  fi
}

run_xtask() {
  if [ -n "${PGLITE_OXIDE_XTASK:-}" ]; then
    xtask="$PGLITE_OXIDE_XTASK"
    if command -v cygpath >/dev/null 2>&1; then
      xtask="$(cygpath -u "$xtask" 2>/dev/null || printf '%s\n' "$xtask")"
    fi
    run "$xtask" "$@"
  else
    require cargo
    run cargo run -p xtask -- "$@"
  fi
}

xtask_output() {
  if [ -n "${PGLITE_OXIDE_XTASK:-}" ]; then
    xtask="$PGLITE_OXIDE_XTASK"
    if command -v cygpath >/dev/null 2>&1; then
      xtask="$(cygpath -u "$xtask" 2>/dev/null || printf '%s\n' "$xtask")"
    fi
    "$xtask" "$@"
  else
    require cargo
    cargo run --quiet -p xtask -- "$@"
  fi
}

run_prek() {
  require prek
  stage="${1:?run_prek requires a stage}"
  shift
  run prek run --all-files --stage "$stage" "$@"
}

run_prek_tracked_files() {
  require prek
  stage="${1:?run_prek_tracked_files requires a stage}"
  printf '\n==> prek run --tracked-files --stage %s\n' "$stage"
  git ls-files |
    while IFS= read -r file; do
      [ -e "$file" ] && printf '%s\0' "$file"
    done |
    xargs -0 prek run --stage "$stage" --files
}

cargo_publish_args() {
  if [ "$allow_dirty" -eq 1 ]; then
    printf '%s\n' --allow-dirty
  fi
}

cargo_package_args() {
  if [ "$allow_dirty" -eq 1 ]; then
    printf '%s\n' --allow-dirty
  fi
}

clean_package_artifacts() {
  rm -f target/package/*.crate
}

internal_packages() {
  xtask_output assets internal-packages
}

aot_targets() {
  xtask_output assets aot-targets
}

host_aot_manifest() {
  host="$1"
  if [ -f "target/pglite-oxide/aot/$host/manifest.json" ]; then
    printf '%s\n' "target/pglite-oxide/aot/$host/manifest.json"
  elif [ -f "crates/aot/$host/artifacts/manifest.json" ]; then
    printf '%s\n' "crates/aot/$host/artifacts/manifest.json"
  else
    return 1
  fi
}

run_root_publish_dry_run() {
  tmp="$(mktemp)"
  if cargo publish -p pglite-oxide --dry-run --locked $(cargo_publish_args) >"$tmp" 2>&1; then
    cat "$tmp"
    rm -f "$tmp"
    return 0
  fi

  status=$?
  if grep -Eq 'no matching package named `pglite-oxide-(assets|aot-[^`]+)` found' "$tmp"; then
    cat >&2 <<'MSG'
warning: root crate publish dry-run could not resolve exact internal crate
versions from crates.io.

This is expected for same-release internal asset/AOT versions. release-plz owns
the actual publish order; this validation dry-runs every internal crate before
release-plz publish/dry-run is invoked.
MSG
    rm -f "$tmp"
    return 0
  fi

  cat "$tmp" >&2
  rm -f "$tmp"
  return "$status"
}

validate_repo() {
  require prek
  run prek validate-config prek.toml
  run_prek_tracked_files pre-commit
}

validate_artifacts() {
  run_xtask assets verify-committed
}

validate_workflows() {
  require actionlint
  require zizmor
  run actionlint
  run zizmor --config .github/zizmor.yml --min-severity medium --persona auditor .github/workflows .github/actions
}

validate_lint() {
  require cargo
  run scripts/check-dependency-invariants.sh
  run cargo clippy --workspace --all-targets --locked -- -D warnings
}

validate_tests() {
  require cargo
  run cargo check --workspace --locked
  run cargo check --workspace --no-default-features --all-targets --locked
  run cargo test --doc --workspace --locked
  run cargo test --workspace --all-targets --locked --no-run
}

validate_dev() {
  validate_repo
  validate_artifacts
  validate_lint
  validate_tests
}

require_host_runtime_artifacts() {
  require cargo
  host="$(rustc -vV | awk '/^host:/{print $2}')"
  if ! host_aot_manifest "$host" >/dev/null 2>&1; then
    cat >&2 <<MSG
missing host AOT artifacts for $host.

Build them locally:
  cargo run -p xtask -- assets fetch
  cargo run -p xtask --features aot-serializer -- assets build-host

Or install them from CI:
  cargo run -p xtask -- assets download --sha <sha> --target-triple $host
  cargo run -p xtask -- assets download --latest-compatible --target-triple $host
  cargo run -p xtask -- assets install-local --target-triple $host
MSG
    exit 1
  fi
  if [ ! -f "target/pglite-oxide/assets/manifest.json" ]; then
    cat >&2 <<MSG
missing generated portable assets at target/pglite-oxide/assets.

Build them locally:
  cargo run -p xtask -- assets fetch
  cargo run -p xtask --features aot-serializer -- assets build-host

Or install them from CI:
  cargo run -p xtask -- assets download --sha <sha> --target-triple $host
  cargo run -p xtask -- assets download --latest-compatible --target-triple $host
  cargo run -p xtask -- assets install-local --target-triple $host
MSG
    exit 1
  fi
  run_xtask assets install-local --target-triple "$host"
  export PGLITE_OXIDE_GENERATED_ASSETS_DIR="$root/target/pglite-oxide/assets"
  export PGLITE_OXIDE_GENERATED_AOT_DIR="$root/target/pglite-oxide/aot"
}

validate_runtime_smoke() {
  require_host_runtime_artifacts
  export RUST_BACKTRACE="${RUST_BACKTRACE:-full}"
  run cargo test -p pglite-oxide --locked \
    --test runtime_smoke \
    --test proxy_smoke \
    --test cli_smoke \
    --test performance_smoke \
    --test extensions_smoke \
    --test postgres_regression \
    -- --nocapture
  run cargo test -p pglite-oxide --locked --lib pg_dump -- --nocapture
}

validate_runtime() {
  require_host_runtime_artifacts
  run cargo test --workspace --all-targets --locked
}

validate_examples() {
  require cargo
  require npm
  run scripts/sync-example-lockfiles.py --check
  run cargo check --manifest-path examples/tauri-sqlx-vanilla/src-tauri/Cargo.toml --locked
  run npm --prefix examples/tauri-sqlx-vanilla ci
  run npm --prefix examples/tauri-sqlx-vanilla run build
}

validate_package() {
  require cargo
  clean_package_artifacts
  run cargo package --workspace --exclude xtask --locked --no-verify $(cargo_package_args)
  run scripts/check-crate-size.sh --enforce
}

validate_feature_powerset() {
  require cargo-hack
  run cargo hack check --workspace --feature-powerset --no-dev-deps --exclude-features aot-serializer,template-runner
}

validate_semver() {
  require cargo-semver-checks
  run cargo semver-checks check-release --package pglite-oxide --manifest-path Cargo.toml
}

validate_supply_chain() {
  require cargo-deny
  run cargo deny check
}

require_release_aot_artifacts() {
  for target in $(aot_targets); do
    manifest="$(host_aot_manifest "$target" 2>/dev/null || true)"
    if [ -z "$manifest" ]; then
      manifest="target/pglite-oxide/aot/$target/manifest.json"
    fi
    if [ ! -f "$manifest" ]; then
      echo "missing release AOT artifacts for $target; download them from the successful Assets workflow before release validation" >&2
      exit 1
    fi
    python3 - "$manifest" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    manifest = json.load(f)
if not manifest.get("artifacts"):
    raise SystemExit(f"{path} does not contain generated AOT artifacts")
PY
  done
}

require_release_portable_assets() {
  if [ ! -f "target/pglite-oxide/assets/manifest.json" ]; then
    echo "missing release portable assets; download or build Assets workflow outputs before release validation" >&2
    exit 1
  fi
}

validate_release_aot_artifacts() {
  for target in $(aot_targets); do
    run_xtask assets check-aot --target-triple "$target"
  done
}

validate_release() {
  require cargo
  if [ "${PGLITE_OXIDE_RELEASE_STAGED:-0}" != "1" ]; then
    require_release_portable_assets
    require_release_aot_artifacts
    run_xtask release stage
    (
      cd target/pglite-oxide/release/workspace
      PGLITE_OXIDE_RELEASE_STAGED=1 scripts/validate.sh release --allow-dirty
    )
    return 0
  fi
  require_release_portable_assets
  require_release_aot_artifacts
  run_xtask assets check --strict-generated
  validate_release_aot_artifacts
  validate_package
  for package in $(internal_packages); do
    run cargo publish -p "$package" --dry-run --locked $(cargo_publish_args)
  done
  printf '\n==> cargo publish -p pglite-oxide --dry-run --locked\n'
  run_root_publish_dry_run
}

case "$mode" in
  commit-msg)
    require prek
    run prek run --stage commit-msg --commit-msg-filename "${1:?commit-msg mode requires a message file}"
    ;;

  pre-commit)
    run_prek pre-commit
    ;;

  pre-push)
    run_prek pre-push
    ;;

  repo)
    validate_repo
    ;;

  artifacts)
    validate_artifacts
    ;;

  lint)
    validate_lint
    ;;

  test)
    validate_tests
    ;;

  workflows)
    validate_workflows
    ;;

  dev)
    validate_dev
    ;;

  runtime)
    validate_runtime
    ;;

  runtime-smoke)
    validate_runtime_smoke
    ;;

  examples)
    validate_examples
    ;;

  package)
    validate_package
    ;;

  feature-powerset)
    validate_feature_powerset
    ;;

  semver)
    validate_semver
    ;;

  supply-chain)
    validate_supply_chain
    ;;

  dev-ci)
    validate_dev
    validate_examples
    ;;

  ci)
    validate_dev
    validate_workflows
    validate_examples
    validate_package
    validate_feature_powerset
    validate_semver
    validate_supply_chain
    ;;

  release)
    validate_release
    ;;

  *)
    cat >&2 <<'MSG'
usage: scripts/validate.sh <mode> [--allow-dirty]

modes:
  commit-msg <file>  validate a Conventional Commit message with prek
  pre-commit         run all pre-commit prek hooks
  pre-push           run all pre-push prek hooks
  repo               repository hygiene and formatting
  workflows          actionlint and zizmor GitHub Actions checks
  lint               dependency invariants and clippy
  test               source-only checks, doctests, and test compilation
  dev                repo, source-only asset checks, lint, and tests/compile gate
  runtime            require host generated assets and run runtime tests
  runtime-smoke      require host generated assets and run runtime smoke tests only
  examples           Tauri/Rust/frontend example checks
  package            package all published crates and enforce size limits
  feature-powerset   cargo-hack feature combination checks
  semver             cargo-semver-checks public API compatibility
  supply-chain       cargo-deny dependency checks
  dev-ci             repo, artifacts, lint, test, and examples
  ci                 full local CI parity lane
  release            package generated release workspace and publish-dry-run internals
  artifacts          verify source-controlled asset inputs and AOT crate templates
MSG
    exit 2
    ;;
esac
