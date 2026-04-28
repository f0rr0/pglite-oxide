#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"

IMAGE="${IMAGE:-pglite-oxide-wasix-build:local}"
JOBS="${JOBS:-4}"
CONTAINER_ROOT="${CONTAINER_ROOT:-/work/assets/wasix-build}"
CONTAINER_BUILD_DIR="${CONTAINER_BUILD_DIR:-$CONTAINER_ROOT/work/docker-pglite}"
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
  -e BUILD_DIR="$CONTAINER_BUILD_DIR" \
  -e PGSRC="$CONTAINER_PGSRC" \
  -e JOBS="$JOBS" \
  -e WASIX_HOME=/opt/wasixcc-home/.wasixcc \
  -v "$REPO_ROOT:/work" \
  -w /work \
  "$IMAGE" \
  bash -lc '
    set -euo pipefail
    export PATH="$WASIX_HOME/bin:$PATH"

    test -f "$BUILD_DIR/config.status"
    test -f "$BUILD_DIR/src/backend/pglite"
    cmp -s "$PGSRC/.pglite-oxide-source-head" "$BUILD_DIR/.pglite-oxide-source-head"
    cmp -s "$PGSRC/.pglite-oxide-patch-sha256" "$BUILD_DIR/.pglite-oxide-patch-sha256"
    sha256sum -c "$BUILD_DIR/.pglite-oxide-bridge-sha256" >/dev/null

    make -s -j"$JOBS" -C "$BUILD_DIR/contrib/pg_trgm" all
    test -f "$BUILD_DIR/contrib/pg_trgm/pg_trgm.so"
  '
