#!/usr/bin/env sh
set -eu

root="$(git rev-parse --show-toplevel)"
cd "$root"

if ! command -v prek >/dev/null 2>&1; then
  cat >&2 <<'MSG'
missing required command: prek

Install prek first, then rerun this script:
  brew install prek

Other installation methods are documented at https://prek.j178.dev/installation/
MSG
  exit 1
fi

hooks_path="$(git config --local --get core.hooksPath || true)"
if [ "$hooks_path" = ".githooks" ]; then
  git config --local --unset core.hooksPath
fi

prek install --prepare-hooks --overwrite
echo "Installed prek hooks from prek.toml"
