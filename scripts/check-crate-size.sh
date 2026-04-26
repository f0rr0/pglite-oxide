#!/usr/bin/env sh
set -eu

mode="${1:---warn}"
limit_bytes="${CRATES_IO_SIZE_LIMIT_BYTES:-10485760}"
crate_file="$(find target/package -maxdepth 1 -name 'pglite-oxide-*.crate' -type f 2>/dev/null | sort | tail -n 1 || true)"

if [ -z "$crate_file" ]; then
  echo "No packaged crate found under target/package; run cargo package first." >&2
  exit 1
fi

size_bytes="$(wc -c < "$crate_file" | tr -d ' ')"
size_mib="$(awk "BEGIN { printf \"%.2f\", $size_bytes / 1048576 }")"
limit_mib="$(awk "BEGIN { printf \"%.2f\", $limit_bytes / 1048576 }")"

if [ "$size_bytes" -le "$limit_bytes" ]; then
  echo "crate size ok: $crate_file is ${size_mib}MiB <= ${limit_mib}MiB"
  exit 0
fi

message="crate size warning: $crate_file is ${size_mib}MiB > ${limit_mib}MiB"
if [ "$mode" = "--enforce" ]; then
  echo "$message" >&2
  exit 1
fi

echo "$message" >&2
exit 0
