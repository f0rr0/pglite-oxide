#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"
PGSRC="${PGSRC:-$REPO_ROOT/spikes/upstream/postgres-pglite}"
CONFIGURE_BUILD="${CONFIGURE_BUILD:-$ROOT/work/configure-smoke}"
BUILD="${MIN_EXT_BUILD:-$ROOT/build/min_ext}"

WASIX_HOME="${WASIX_HOME:-/tmp/wasixcc-home/.wasixcc}"
export HOME="${WASIX_HOME%/.wasixcc}"
export PATH="$WASIX_HOME/bin:$PATH"

mkdir -p "$BUILD"

if [ ! -f "$CONFIGURE_BUILD/src/include/pg_config_os.h" ]; then
  echo "missing configure smoke build at $CONFIGURE_BUILD" >&2
  echo "run the wasix-dl configure smoke first" >&2
  exit 2
fi

make -C "$CONFIGURE_BUILD/src/backend/utils" generated-header-symlinks >/dev/null

wasixcc -sWASM_EXCEPTIONS=yes -sPIC=yes \
  -I"$CONFIGURE_BUILD/src/include" \
  -I"$PGSRC/src/include" \
  -c "$ROOT/min_ext/min_ext.c" \
  -o "$BUILD/min_ext.o"

wasixcc -sWASM_EXCEPTIONS=yes -sPIC=yes -Wl,-shared \
  "$BUILD/min_ext.o" \
  -o "$BUILD/min_ext.so"

echo "built $BUILD/min_ext.so"
wasixnm --undefined-only "$BUILD/min_ext.so" | sed -n '1,80p'
