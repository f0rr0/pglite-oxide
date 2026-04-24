#!/usr/bin/env bash
set -euo pipefail

subject="${1:-}"
base_ref="${2:-origin/main}"
head_ref="${3:-HEAD}"
head_branch="${4:-}"

if [[ -z "${subject}" ]]; then
  echo "expected a non-empty PR title or commit subject" >&2
  exit 1
fi

release_pattern='^((feat|fix|perf|refactor|revert)(\([a-z0-9][a-z0-9._/-]*\))?(!)?|[a-z]+(\([a-z0-9][a-z0-9._/-]*\))?!): .+'
release_pr_pattern='^chore\(release\): .+'

affected_files=()

while IFS= read -r file; do
  [[ -z "${file}" ]] && continue

  case "${file}" in
    Cargo.toml | Cargo.lock | build.rs | src/* | assets/* | examples/* | benches/*)
      affected_files+=("${file}")
      ;;
  esac
done < <(git diff --name-only "${base_ref}...${head_ref}" --)

if (( ${#affected_files[@]} == 0 )); then
  exit 0
fi

if [[ "${subject}" =~ ${release_pattern} ]]; then
  exit 0
fi

if [[ "${subject}" =~ ${release_pr_pattern} && "${head_branch}" == release-plz-* ]]; then
  exit 0
fi

cat >&2 <<EOF
This PR changes release-affecting package files, but its title does not carry
release intent for release-plz.

Use one of these Conventional Commit types in the PR title:
  feat, fix, perf, refactor, revert

Breaking changes may use any type with !, for example:
  chore!: remove a deprecated API

release-plz PRs are exempt only when their branch starts with release-plz- and
their title starts with chore(release):.

Docs, CI, issue-template, and repository-only changes can keep non-release types
such as docs:, ci:, chore:, style:, or test: when they do not touch package code.

Received:
  ${subject}

Release-affecting files:
EOF

printf '  %s\n' "${affected_files[@]}" >&2
exit 1
