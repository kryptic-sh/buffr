#!/usr/bin/env bash
# Edit-mode e2e runner.
#
# Launches buffr under headless sway with software rendering, aims a REAL
# cursor at a target element via sway's seat commands, and decides from
# buffr's own log whether the click entered Insert mode.
#
# Coordinates are self-calibrating: the runner clicks a known screen point,
# the page reports the client coords it saw (tests/e2e/pages/e2e.js), and the
# difference is the viewport origin. Nothing hard-codes the chrome height, so
# a tab-strip change cannot silently skew every case.
#
# Usage: run.sh <page.html> <target-id> <insert|normal>
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PAGE="${1:?page}"; TARGET="${2:?target id}"; EXPECT="${3:?insert|normal}"
BIN="$REPO/target/debug/buffr-app"
PAGE_PATH="$REPO/tests/e2e/pages/$PAGE"

[ -x "$BIN" ] || { echo "MISSING BINARY $BIN (cargo build -p buffr-app)"; exit 90; }
[ -f "$PAGE_PATH" ] || { echo "MISSING PAGE $PAGE_PATH"; exit 90; }

WORK="$(mktemp -d)"
LOG="$WORK/buffr.log"
VERDICT="$WORK/verdict"

export XDG_RUNTIME_DIR="$WORK/xdg"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless
# no WLR_LIBINPUT_NO_DEVICES: it suppresses the seat pointer capability,
# and a client that never sees wl_pointer cannot receive synthetic clicks
export WLR_RENDERER=pixman          # software rendering: no GPU on CI
export LIBGL_ALWAYS_SOFTWARE=1

cat > "$WORK/inner.sh" <<INNER
#!/usr/bin/env bash
# Runs inside the sway session, so WAYLAND_DISPLAY is set for buffr.
export XDG_SESSION_TYPE=wayland
export BUFFR_DISABLE_ZYGOTE=1
export RUST_LOG=info,buffr_core=debug,buffr_app=debug,buffr=debug
export BUFFR_LOG_CONSOLE=1
cd "$REPO"

"$BIN" --private "file://$PAGE_PATH" > "$LOG" 2>&1 &
APP=\$!

# Wait for the page to load and report its geometry.
for i in \$(seq 1 200); do
  grep -q 'E2E-RECTS-DONE' "$LOG" && break
  sleep 0.25
done
if ! grep -q 'E2E-RECTS-DONE' "$LOG"; then
  echo "no-page-load" > "$VERDICT"; kill \$APP 2>/dev/null; swaymsg exit; exit 0
fi

# --- calibrate: click a known screen point, read back the client coords ---
CAL_X=500; CAL_Y=500
swaymsg seat - cursor set \$CAL_X \$CAL_Y >/dev/null
swaymsg seat - cursor press button1 >/dev/null
sleep 0.1
swaymsg seat - cursor release button1 >/dev/null
sleep 0.6
CLICK=\$(grep -o 'E2E-CLICK [0-9-]* [0-9-]*' "$LOG" | tail -1)
if [ -z "\$CLICK" ]; then
  echo "no-calibration" > "$VERDICT"; kill \$APP 2>/dev/null; swaymsg exit; exit 0
fi
CX=\$(echo "\$CLICK" | awk '{print \$2}')
CY=\$(echo "\$CLICK" | awk '{print \$3}')
OFF_X=\$(( CAL_X - CX ))
OFF_Y=\$(( CAL_Y - CY ))
echo "calibration: viewport origin = \$OFF_X,\$OFF_Y" >> "$LOG"

# --- aim at the target ---
RECT=\$(grep -o "E2E-RECT $TARGET [0-9-]* [0-9-]* [0-9-]* [0-9-]*" "$LOG" | tail -1)
if [ -z "\$RECT" ]; then
  echo "no-target-rect" > "$VERDICT"; kill \$APP 2>/dev/null; swaymsg exit; exit 0
fi
RX=\$(echo "\$RECT" | awk '{print \$3}'); RY=\$(echo "\$RECT" | awk '{print \$4}')
RW=\$(echo "\$RECT" | awk '{print \$5}'); RH=\$(echo "\$RECT" | awk '{print \$6}')
TX=\$(( OFF_X + RX + RW / 2 ))
TY=\$(( OFF_Y + RY + RH / 2 ))
echo "clicking $TARGET at screen \$TX,\$TY (rect \$RX,\$RY \${RW}x\${RH})" >> "$LOG"

# Mark the log so we only consider mode changes caused by THIS click, not
# the calibration click that preceded it.
MARK="E2E-MARK-\$RANDOM"
echo "\$MARK" >> "$LOG"

swaymsg seat - cursor set \$TX \$TY >/dev/null
swaymsg seat - cursor press button1 >/dev/null
sleep 0.1
swaymsg seat - cursor release button1 >/dev/null

# Generous: async pages focus well after the click.
sleep 3

if sed -n "/\$MARK/,\\\$p" "$LOG" | grep -q 'edit-mode entered'; then
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
  echo "PASS  $PAGE#$TARGET expected=$EXPECT"
  rm -rf "$WORK"
  exit 0
fi
echo "FAIL  $PAGE#$TARGET expected=$EXPECT got=$GOT"
echo "      log: $LOG"
grep -E 'E2E-FOCUS|E2E-CLICK|calibration:|clicking |edit-mode entered|focus ignored' "$LOG" | tail -12 | sed 's/^/      /'
exit 1
