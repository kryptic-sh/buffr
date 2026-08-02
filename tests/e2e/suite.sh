#!/usr/bin/env bash
# Run every case in expectations.tsv. Exit non-zero if any fails.
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pass=0; fail=0; failed=()
while IFS=$'\t' read -r page expect _; do
  case "$page" in ''|\#*) continue;; esac
  if "$DIR/run.sh" "$page" "$expect"; then
    pass=$((pass+1))
  else
    fail=$((fail+1)); failed+=("$page")
  fi
done < "$DIR/expectations.tsv"
echo
echo "e2e: $pass passed, $fail failed"
if [ $fail -gt 0 ]; then
  printf 'failed: %s\n' "${failed[*]}"
  exit 1
fi
