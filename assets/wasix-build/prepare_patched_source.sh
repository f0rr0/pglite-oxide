#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"
UPSTREAM_PGSRC="${UPSTREAM_PGSRC:-$REPO_ROOT/assets/checkouts/postgres-pglite}"
PATCHED_PGSRC="${PATCHED_PGSRC:-$ROOT/work/postgres-pglite-wasix-src}"
PATCH_PATH="${PATCH_PATH:-$ROOT/patches/postgres-pglite-wasix-dl.patch}"
POSTGRES_PGLITE_COMMIT="${POSTGRES_PGLITE_COMMIT:-$(git -C "$UPSTREAM_PGSRC" rev-parse HEAD)}"

PATCH_SHA="$(shasum -a 256 "$PATCH_PATH" | awk '{print $1}')"
HEAD_FILE="$PATCHED_PGSRC/.pglite-oxide-source-head"
PATCH_FILE="$PATCHED_PGSRC/.pglite-oxide-patch-sha256"

if [ -e "$PATCHED_PGSRC/.git" ] \
  && [ -f "$HEAD_FILE" ] \
  && [ -f "$PATCH_FILE" ] \
  && [ "$(cat "$HEAD_FILE")" = "$POSTGRES_PGLITE_COMMIT" ] \
  && [ "$(cat "$PATCH_FILE")" = "$PATCH_SHA" ]; then
  echo "reusing patched postgres-pglite source at $PATCHED_PGSRC"
  exit 0
fi

git -C "$UPSTREAM_PGSRC" worktree remove --force "$PATCHED_PGSRC" >/dev/null 2>&1 || true
rm -rf "$PATCHED_PGSRC"
git -C "$UPSTREAM_PGSRC" worktree prune
git -C "$UPSTREAM_PGSRC" worktree add --detach "$PATCHED_PGSRC" "$POSTGRES_PGLITE_COMMIT"
git -C "$PATCHED_PGSRC" apply --whitespace=nowarn "$PATCH_PATH"

printf '%s' "$POSTGRES_PGLITE_COMMIT" > "$HEAD_FILE"
printf '%s' "$PATCH_SHA" > "$PATCH_FILE"
echo "prepared patched postgres-pglite source at $PATCHED_PGSRC"
