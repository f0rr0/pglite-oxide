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

is_release_pr=false
if [[ "${subject}" =~ ${release_pr_pattern} && "${head_branch}" == release-plz-* ]]; then
  is_release_pr=true
fi

package_versions_from_ref() {
  local ref="${1:?package_versions_from_ref requires a git ref}"
  local files

  files="$(
    git ls-tree -r --name-only "${ref}" |
      grep -E '(^Cargo.toml$|^crates/.*/Cargo.toml$)' || true
  )"

  while IFS= read -r file; do
    [[ -z "${file}" ]] && continue
    git show "${ref}:${file}" | awk -v file="${file}" '
    /^\[package\][[:space:]]*$/ {
      in_package = 1
      next
    }
    /^\[/ && in_package {
      exit
    }
    in_package && $0 ~ /^[[:space:]]*name[[:space:]]*=/ {
      name = $0
      sub(/^[^=]*=[[:space:]]*"/, "", name)
      sub(/".*$/, "", name)
    }
    in_package && $0 ~ /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^=]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      if (name == "") {
        name = file
      }
      print name "=" line
      exit
    }
  '
  done <<< "${files}" | sort
}

base_versions="$(package_versions_from_ref "${base_ref}")"
head_versions="$(package_versions_from_ref "${head_ref}")"

if [[ -z "${base_versions}" || -z "${head_versions}" ]]; then
  echo "could not read package versions from Cargo.toml files" >&2
  exit 1
fi

if [[ "${base_versions}" != "${head_versions}" && "${is_release_pr}" != true ]]; then
  cat >&2 <<EOF
This PR changes one or more workspace package versions.

Package version bumps are release-plz owned. Run the Release workflow with
prepare-release-pr and merge the generated release-plz PR instead of changing
the version in a feature/fix PR.

release-plz PRs are allowed only when their branch starts with release-plz- and
their title starts with chore(release):.

Received:
  ${subject}

Base package versions:
${base_versions}

Head package versions:
${head_versions}
EOF
  exit 1
fi

while IFS= read -r file; do
  [[ -z "${file}" ]] && continue

  case "${file}" in
    Cargo.toml | Cargo.lock | build.rs | src/* | assets/* | crates/* | xtask/* | examples/* | benches/*)
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

if [[ "${is_release_pr}" == true ]]; then
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
