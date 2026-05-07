#!/usr/bin/env bash
set -euo pipefail

: "${GITHUB_TOKEN:?GITHUB_TOKEN is required}"

cargo run -p xtask -- assets download --latest-compatible --all-targets
