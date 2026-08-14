#!/usr/bin/env bash
# Render e2e suite: proves buffr's chrome actually paints under a live
# compositor. Guards the "browser tabs are not rendering" regression (the
# whole chrome paint was a no-op, leaving the tab strip and statusline as
# the swapchain clear color) and that tab count / new-tab state changes
# reach the strip.
#
# Each case launches buffr under headless sway, grim-captures, and checks
# pixels. Exit non-zero if any case fails.
set -uo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PAGE1="file://$DIR/pages/render.html"
PAGE2="file://$DIR/pages/render2.html"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass=0; fail=0; failed=()

case_run() { # <name> <expected-result> <out.ppm> [--diff-ref ref] <urls...>
  local name="$1" want="$2" out="$3"; shift 3
  local diffargs=()
  if [ "${1:-}" = "--diff-ref" ]; then
    diffargs=("--diff-ref" "$2"); shift 2
  fi
  if "$DIR/render.sh" "$out" "$@" >/dev/null 2>&1 \
    && python3 "$DIR/render_check.py" "$out" "${diffargs[@]}" >/dev/null 2>&1; then
    echo "PASS  $name"; pass=$((pass+1))
  else
    echo "FAIL  $name (want: $want)"; fail=$((fail+1)); failed+=("$name")
  fi
}

# One tab: strip + statusline must be painted (not the clear color).
case_run "one-tab chrome painted" "painted" "$WORK/one.ppm" "$PAGE1"
# Two tabs: strip painted AND differs from the one-tab strip (both pills).
case_run "two-tab strip reflects tab count" "painted+differs" "$WORK/two.ppm" \
  --diff-ref "$WORK/one.ppm" "$PAGE1" "$PAGE2"
# New tab page: strip painted AND differs from the one-tab strip.
case_run "new-tab strip reflects new tab" "painted+differs" "$WORK/nt.ppm" \
  --diff-ref "$WORK/one.ppm" "$PAGE1" "buffr://new"

echo
echo "render e2e: $pass passed, $fail failed"
if [ $fail -gt 0 ]; then
  printf 'failed: %s\n' "${failed[*]}"
  exit 1
fi
