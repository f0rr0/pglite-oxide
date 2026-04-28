#!/usr/bin/env sh
set -eu

mode="${1:---warn}"
limit_bytes="${CRATES_IO_SIZE_LIMIT_BYTES:-10485760}"
crate_files="$(find target/package -maxdepth 1 -name '*.crate' -type f 2>/dev/null | sort || true)"

if [ -z "$crate_files" ]; then
  echo "No packaged crate found under target/package; run cargo package first." >&2
  exit 1
fi

limit_mib="$(awk "BEGIN { printf \"%.2f\", $limit_bytes / 1048576 }")"
failed=0

for crate_file in $crate_files; do
  size_bytes="$(wc -c < "$crate_file" | tr -d ' ')"
  size_mib="$(awk "BEGIN { printf \"%.2f\", $size_bytes / 1048576 }")"

  if [ "$size_bytes" -le "$limit_bytes" ]; then
    echo "crate size ok: $crate_file is ${size_mib}MiB <= ${limit_mib}MiB"
    continue
  fi

  message="crate size warning: $crate_file is ${size_mib}MiB > ${limit_mib}MiB"
  echo "$message" >&2
  failed=1
done

if [ "$mode" = "--enforce" ] && [ "$failed" -ne 0 ]; then
  exit 1
fi

exit 0
