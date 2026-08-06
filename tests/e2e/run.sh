#!/usr/bin/env bash
# Edit-mode e2e runner.
#
# Loads a page in buffr under headless sway with software rendering and
# decides, from buffr's own log, whether focusing that page's field entered
# Insert mode.
#
# Focus is driven by the PAGE, not by synthetic input: every trigger is one a
# real site actually uses (autofocus, .focus() after a tick, a dialog grabbing
# its search box, a component focusing inside its shadow root). That is also
# what makes the suite runnable on CI, where the headless seat has no input
# devices at all and neither pointer nor keyboard events can be injected.
#
# Usage: run.sh <page.html> <insert|normal>
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PAGE="${1:?page}"; EXPECT="${2:?insert|normal}"
BIN="$REPO/target/debug/buffr-app"
PAGE_PATH="$REPO/tests/e2e/pages/$PAGE"

[ -x "$BIN" ] || { echo "MISSING BINARY $BIN (cargo build -p buffr-app)"; exit 90; }
[ -f "$PAGE_PATH" ] || { echo "MISSING PAGE $PAGE_PATH"; exit 90; }

WORK="$(mktemp -d)"
LOG="$WORK/buffr.log"
VERDICT="$WORK/verdict"

export XDG_RUNTIME_DIR="$WORK/xdg"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman          # software rendering: no GPU on CI
export LIBGL_ALWAYS_SOFTWARE=1

cat > "$WORK/inner.sh" <<INNER
#!/usr/bin/env bash
export XDG_SESSION_TYPE=wayland
# No BUFFR_DISABLE_ZYGOTE here: the renderer sandbox is enabled, and
# Chromium refuses --no-zygote together with the sandbox ("Zygote cannot
# be disabled if sandbox is enabled"). The flag was only ever a
# precaution for an ICU crash traced to LD_LIBRARY_PATH, not the zygote.
# BUFFR_LOG_CONSOLE is required: page console output is the harness's only
# feedback channel and is gated off by default for privacy.
export RUST_LOG=info,buffr_core=debug
export BUFFR_LOG_CONSOLE=1
cd "$REPO"

"$BIN" --private "file://$PAGE_PATH" > "$LOG" 2>&1 &
APP=\$!

for i in \$(seq 1 240); do
  grep -q 'E2E-RECTS-DONE' "$LOG" && break
  sleep 0.25
done
if ! grep -q 'E2E-RECTS-DONE' "$LOG"; then
  echo "no-page-load" > "$VERDICT"; kill \$APP 2>/dev/null; swaymsg exit; exit 0
fi

# The page fires its trigger 400 ms after load; async cases add up to ~600 ms
# on top. Wait well past both so a slow case fails for being wrong, not slow.
sleep 4

if grep -q 'edit-mode entered' "$LOG"; then
  echo "insert" > "$VERDICT"
else
  echo "normal" > "$VERDICT"
fi
kill \$APP 2>/dev/null
swaymsg exit
INNER
chmod +x "$WORK/inner.sh"

cat > "$WORK/sway.conf" <<CONF
output HEADLESS-1 mode 1280x800
exec $WORK/inner.sh
CONF

timeout 180 sway --unsupported-gpu -c "$WORK/sway.conf" >"$WORK/sway.log" 2>&1

GOT="$(cat "$VERDICT" 2>/dev/null || echo no-verdict)"
if [ "$GOT" = "$EXPECT" ]; then
  echo "PASS  $PAGE expected=$EXPECT"
  rm -rf "$WORK"
  exit 0
fi
echo "FAIL  $PAGE expected=$EXPECT got=$GOT"
echo "      log: $LOG"
grep -E 'E2E-FOCUS|E2E-TRIGGER-ERROR|edit-mode entered' "$LOG" | tail -8 | sed 's/^/      /'
exit 1
