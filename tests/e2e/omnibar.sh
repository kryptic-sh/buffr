#!/usr/bin/env bash
# Omnibar e2e suite. Drives real keystrokes through BUFFR_E2E_KEYS (the
# headless CI seat has no input devices, so buffr's key-injection hook posts
# synthetic keys through the exact dispatch path a physical keyboard takes),
# then judges buffr's log and a grim capture of the headless sway output.
#
# Cases:
#   - first typed char renders: `o` + one char; the input row must draw
#     the char, not only the cursor (omnibar_check.py typed).
#   - Enter navigates and closes: `o` + a typed file:// URL + Enter; the
#     log must show a navigate to exactly that URL (not an
#     "about:blank…" concatenation) and the capture must show the target
#     page with no popup left over it (omnibar_check.py closed).
#   - Esc closes: `e` + a char + Esc over a static page; the capture must
#     show the page with no popup left over it (omnibar_check.py closed).
#
# Exit non-zero if any case fails.
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$DIR/../.." && pwd)"
BIN="${BUFFR_E2E_BIN:-$REPO/target/debug/buffr-app}"
START="file://$DIR/pages/render.html"
TARGET="file://$DIR/pages/omnibar_target.html"
# The Wayland socket lives under XDG_RUNTIME_DIR, whose path must fit a
# sockaddr_un — so it gets its own short mktemp dir, never one under WORK.
XDG_BASE="$(mktemp -d)"
# BUFFR_E2E_WORK keeps the captures and logs for inspection.
if [ -n "${BUFFR_E2E_WORK:-}" ]; then
  WORK="$BUFFR_E2E_WORK"; mkdir -p "$WORK"
  trap 'rm -rf "$XDG_BASE"' EXIT
else
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK" "$XDG_BASE"' EXIT
fi

[ -x "$BIN" ] || { echo "MISSING BINARY $BIN (cargo build -p buffr-app)"; exit 90; }

export WLR_BACKENDS=headless
export WLR_RENDERER=pixman          # software rendering: no GPU on CI
export LIBGL_ALWAYS_SOFTWARE=1

# Seconds to let the last key's effects (navigation, page paint, chrome
# frame) land before capturing.
SETTLE_SECS=3

# BUFFR_E2E_KEYS for typing <text> one `char:` token per character. The
# token list is comma-separated, so <text> must not contain a comma.
keys_for() {
  local text="$1" out="" i
  for ((i = 0; i < ${#text}; i++)); do
    out+="char:${text:i:1},"
  done
  printf '%s' "${out%,}"
}

# launch <run-dir> <start-url> <keys>: run buffr under headless sway, type
# <keys>, wait until the driver reports the sequence dispatched, settle,
# capture <run-dir>/shot.ppm. buffr's log lands in <run-dir>/buffr.log with
# ANSI colour codes stripped.
launch() {
  local run="$1" url="$2" keys="$3"
  local xdg="$XDG_BASE/$(basename "$run")"
  mkdir -p "$run" "$xdg"; chmod 700 "$xdg"
  cat > "$run/inner.sh" <<INNER
#!/usr/bin/env bash
export XDG_SESSION_TYPE=wayland
export RUST_LOG=info,buffr_core=debug
export BUFFR_E2E_KEYS='$keys'
cd "$REPO"
"$BIN" --private "$url" > "$run/raw.log" 2>&1 &
APP=\$!
for i in \$(seq 1 160); do
  grep -q 'e2e: key sequence dispatched' "$run/raw.log" && break
  sleep 0.25
done
sleep $SETTLE_SECS
grim "$run/shot.png"
kill \$APP 2>/dev/null
sleep 0.5
swaymsg exit
INNER
  chmod +x "$run/inner.sh"
  cat > "$run/sway.conf" <<CONF
# No server-side decorations: the client area starts at y=0.
default_border none
output HEADLESS-1 mode 1280x800
exec $run/inner.sh
CONF
  XDG_RUNTIME_DIR="$xdg" timeout 120 sway --unsupported-gpu -c "$run/sway.conf" \
    > "$run/sway.log" 2>&1
  sed 's/\x1b\[[0-9;]*m//g' "$run/raw.log" > "$run/buffr.log" 2>/dev/null
  [ -f "$run/shot.png" ] && convert "$run/shot.png" ppm:- > "$run/shot.ppm" 2>/dev/null
}

pass=0; fail=0; failed=()
report() { # <name> <error-or-empty>
  if [ -z "$2" ]; then
    echo "PASS  $1"; pass=$((pass+1))
  else
    echo "FAIL  $1"; printf '%s\n' "$2" | sed 's/^/      /'
    fail=$((fail+1)); failed+=("$1")
  fi
}

# Verdict for a capture: empty on pass, the checker's message on failure.
shot_verdict() { # <run-dir> <typed|closed>
  if [ ! -s "$1/shot.ppm" ]; then
    echo "no screenshot captured (buffr or grim failed; see $1/sway.log)"
    return
  fi
  python3 "$DIR/omnibar_check.py" "$2" "$1/shot.ppm" | grep 'FAIL'
}

# ── first typed char renders ─────────────────────────────────────────────
RUN="$WORK/typed"
launch "$RUN" "$START" "char:o,char:w"
report "omnibar: first typed char renders" "$(shot_verdict "$RUN" typed)"

# ── Enter navigates and closes the omnibar ───────────────────────────────
RUN="$WORK/enter"
launch "$RUN" "$START" "char:o,$(keys_for "$TARGET"),named:Return"
err=""
if ! grep -qF "navigate url=$TARGET" "$RUN/buffr.log"; then
  err="expected 'navigate url=$TARGET' in buffr's log; saw:"$'\n'
  err+="$(grep -E 'navigate url=' "$RUN/buffr.log" | tail -3)"
elif grep -q "navigate url=about:blank[^ ]" "$RUN/buffr.log"; then
  err="navigated to a garbage about:blank-prefixed URL"
fi
report "omnibar: Enter navigates to the typed URL" "$err"
report "omnibar: Enter closes the popup" "$(shot_verdict "$RUN" closed)"

# ── Esc cancels and closes the omnibar over a static page ────────────────
# `e` opens the omnibar on the current tab. The target page is static, so
# no page paint arrives afterwards to repaint the chrome by accident.
RUN="$WORK/esc"
launch "$RUN" "$TARGET" "char:e,char:w,named:Escape"
report "omnibar: Esc closes the popup" "$(shot_verdict "$RUN" closed)"

echo
echo "omnibar e2e: $pass passed, $fail failed"
if [ $fail -gt 0 ]; then
  printf 'failed: %s\n' "${failed[*]}"
  exit 1
fi
