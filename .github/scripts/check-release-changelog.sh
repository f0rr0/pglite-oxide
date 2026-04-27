#!/usr/bin/env bash
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

package_version="$(
  awk '
    /^\[package\][[:space:]]*$/ {
      in_package = 1
      next
    }
    /^\[/ && in_package {
      exit
    }
    in_package && $0 ~ /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^=]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' Cargo.toml
)"

if [[ -z "${package_version}" ]]; then
  echo "could not read package version from Cargo.toml" >&2
  exit 1
fi

is_version_heading_awk='
  function is_version_heading(line) {
    return index(line, "## [" version "] - ") == 1 ||
      (index(line, "## [" version "](") == 1 &&
       line ~ /\) - [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/)
  }
'

top_release_heading="$(
  awk '
    /^## \[Unreleased\]/ {
      seen_unreleased = 1
      next
    }
    seen_unreleased && /^## \[/ {
      print
      exit
    }
  ' CHANGELOG.md
)"

if [[ -z "${top_release_heading}" ]]; then
  echo "CHANGELOG.md does not contain a release section after [Unreleased]" >&2
  exit 1
fi

if ! awk -v version="${package_version}" -v heading="${top_release_heading}" "${is_version_heading_awk}"'
  BEGIN {
    exit is_version_heading(heading) ? 0 : 1
  }
'; then
  cat >&2 <<EOF
CHANGELOG.md top release section does not match Cargo.toml version.

Cargo.toml version:
  ${package_version}

Top changelog release section:
  ${top_release_heading}

Run the Release workflow with prepare-release-pr and merge the generated
release-plz PR before running publish or publish-dry-run.
EOF
  exit 1
fi

if ! awk -v version="${package_version}" "${is_version_heading_awk}"'
  is_version_heading($0) {
    in_section = 1
    next
  }
  in_section && /^## \[/ {
    exit
  }
  in_section && $0 ~ /[^[:space:]]/ && $0 !~ /^### / {
    found_body = 1
  }
  END {
    exit found_body ? 0 : 1
  }
' CHANGELOG.md; then
  cat >&2 <<EOF
CHANGELOG.md has a ${package_version} release section, but it has no release
note body.

Run release-plz release-pr to regenerate the changelog section before
publishing.
EOF
  exit 1
fi

expected_unreleased="[Unreleased]: https://github.com/f0rr0/pglite-oxide/compare/${package_version}...HEAD"

if ! grep -Fxq "${expected_unreleased}" CHANGELOG.md; then
  cat >&2 <<EOF
CHANGELOG.md [Unreleased] compare link does not start from ${package_version}.

Expected:
  ${expected_unreleased}

This usually means the changelog was not updated for the package version that
is about to be published.
EOF
  exit 1
fi

echo "release changelog matches package version ${package_version}"
