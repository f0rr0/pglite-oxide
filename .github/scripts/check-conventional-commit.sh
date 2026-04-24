#!/usr/bin/env bash
set -euo pipefail

subject="${1:-}"

if [[ -z "${subject}" ]]; then
  echo "expected a non-empty commit subject or PR title" >&2
  exit 1
fi

pattern='^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\([a-z0-9][a-z0-9._/-]*\))?(!)?: .+'

if [[ ! "${subject}" =~ ${pattern} ]]; then
  cat >&2 <<EOF
Expected Conventional Commits format:
  <type>(optional-scope)!: <description>

Allowed types:
  build, chore, ci, docs, feat, fix, perf, refactor, revert, style, test

Received:
  ${subject}
EOF
  exit 1
fi
