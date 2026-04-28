#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$ROOT/build"

WASIX_HOME="${WASIX_HOME:-/tmp/wasixcc-home/.wasixcc}"
WASMER_BIN="${WASMER_BIN:-/tmp/wasmer-7.1/bin/wasmer}"

export HOME="${WASIX_HOME%/.wasixcc}"
export PATH="$WASIX_HOME/bin:$PATH"

mkdir -p "$BUILD"

wasixcc -sWASM_EXCEPTIONS=yes -sPIC=yes -Wl,-shared \
  "$ROOT/libneeded.c" \
  -o "$BUILD/libneeded.so"

wasixcc -sWASM_EXCEPTIONS=yes -sPIC=yes -Wl,-shared \
  "$ROOT/libdlopened.c" \
  -o "$BUILD/libdlopened.so"

wasixcc -sWASM_EXCEPTIONS=yes -sPIC=yes -sMODULE_KIND=dynamic-main \
  -Wl,-rpath,'$ORIGIN' \
  "$ROOT/main.c" "$BUILD/libneeded.so" \
  -o "$BUILD/main.wasm"

"$WASMER_BIN" run "$BUILD/main.wasm" --volume "$BUILD:/lib" --cwd /lib
