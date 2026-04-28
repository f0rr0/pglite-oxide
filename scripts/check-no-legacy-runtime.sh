#!/usr/bin/env sh
set -eu

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

pattern='wasm''time|wasm''time-wasi|pglite\.wasi(\b|[^x[:alnum:]_-])|legacy-''wasi|pglite-''wasi(\b|[^x[:alnum:]_-])'

if rg -n "$pattern" \
  -g '!spikes/**' \
  -g '!target/**' \
  -g '!.git/**' \
  -g '!scripts/check-no-legacy-runtime.sh'
then
  cat >&2 <<'MSG'
legacy runtime reference found outside spikes/

The production runtime path is WASIX dynamic linking plus headless Wasmer AOT.
Keep historical references in spikes/ only.
MSG
  exit 1
fi
