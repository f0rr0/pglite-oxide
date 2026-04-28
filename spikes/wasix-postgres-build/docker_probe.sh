#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"

IMAGE="${IMAGE:-pglite-oxide-wasix-build:local}"
JOBS="${JOBS:-4}"
CONTAINER_ROOT="${CONTAINER_ROOT:-/work/spikes/wasix-postgres-build}"
CONTAINER_BUILD_DIR="${CONTAINER_BUILD_DIR:-$CONTAINER_ROOT/work/docker-configure}"
CONTAINER_MIN_EXT_BUILD="${CONTAINER_MIN_EXT_BUILD:-$CONTAINER_ROOT/build/docker-min-ext}"
CONTAINER_PGSRC="${CONTAINER_PGSRC:-$CONTAINER_ROOT/work/postgres-pglite-wasix-src}"
DOCKER="${DOCKER:-$(command -v docker 2>/dev/null || true)}"
if [ -z "$DOCKER" ] && [ -x /usr/local/bin/docker ]; then
  DOCKER=/usr/local/bin/docker
fi
if [ -z "$DOCKER" ] && [ -x /opt/homebrew/bin/docker ]; then
  DOCKER=/opt/homebrew/bin/docker
fi
if [ -z "$DOCKER" ]; then
  echo "docker CLI not found; set DOCKER=/path/to/docker" >&2
  exit 127
fi
export PATH="$(dirname "$DOCKER"):$PATH"

"$ROOT/prepare_patched_source.sh"

if [ "${FORCE_IMAGE_BUILD:-0}" = "1" ] || ! "$DOCKER" image inspect "$IMAGE" >/dev/null 2>&1; then
  "$DOCKER" build \
    -t "$IMAGE" \
    -f "$ROOT/docker/Dockerfile" \
    "$ROOT/docker"
else
  echo "reusing Docker image $IMAGE"
fi

"$DOCKER" run --rm \
  --cpus="$JOBS" \
  -e JOBS="$JOBS" \
  -e BUILD_DIR="$CONTAINER_BUILD_DIR" \
  -e PGSRC="$CONTAINER_PGSRC" \
  -e FORCE_RECONFIGURE="${FORCE_RECONFIGURE:-0}" \
  -e MIN_EXT_BUILD="$CONTAINER_MIN_EXT_BUILD" \
  -e WASIX_HOME=/opt/wasixcc-home/.wasixcc \
  -v "$REPO_ROOT:/work" \
  -w /work \
  "$IMAGE" \
  bash -lc '
    set -euo pipefail
    export PATH="$WASIX_HOME/bin:$PATH"
    needs_configure=0
    if [ "${FORCE_RECONFIGURE:-0}" = "1" ] || [ ! -f "$BUILD_DIR/config.status" ]; then
      needs_configure=1
    elif ! cmp -s "$PGSRC/.pglite-oxide-source-head" "$BUILD_DIR/.pglite-oxide-source-head"; then
      needs_configure=1
    elif ! cmp -s "$PGSRC/.pglite-oxide-patch-sha256" "$BUILD_DIR/.pglite-oxide-patch-sha256"; then
      needs_configure=1
    fi

    if [ "$needs_configure" = "1" ]; then
      rm -rf "$BUILD_DIR"
      ./spikes/wasix-postgres-build/configure_wasix_dl.sh
      cp "$PGSRC/.pglite-oxide-source-head" "$BUILD_DIR/.pglite-oxide-source-head"
      cp "$PGSRC/.pglite-oxide-patch-sha256" "$BUILD_DIR/.pglite-oxide-patch-sha256"
    else
      echo "reusing configured build at $BUILD_DIR"
    fi
    make -s -C "$BUILD_DIR/src/backend/utils" generated-header-symlinks
    make -s -C "$BUILD_DIR/src/backend/utils/fmgr" dfmgr.o
    CONFIGURE_BUILD="$BUILD_DIR" MIN_EXT_BUILD="$MIN_EXT_BUILD" \
      ./spikes/wasix-postgres-build/build_min_ext.sh
  '
