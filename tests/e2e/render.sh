#!/usr/bin/env bash
# Render e2e runner: launch buffr under headless sway (no window
# decorations so the client area starts at y=0), grim-capture the output,
# and assert the chrome (tab strip + statusline) is painted.
#
# Usage: render.sh <out.ppm> <url1> [url2 ...]
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${BUFFR_E2E_BIN:-$REPO/target/debug/buffr-app}"
OUT="${1:?out.ppm}"; shift
URLS=("$@")

[ -x "$BIN" ] || { echo "MISSING BINARY $BIN (cargo build -p buffr-app)"; exit 90; }
[ ${#URLS[@]} -ge 1 ] || { echo "usage: render.sh <out.ppm> <url> [url ...]"; exit 90; }

WORK="$(mktemp -d)"
SHOT="$WORK/shot.png"
trap 'rm -rf "$WORK"' EXIT

export XDG_RUNTIME_DIR="$WORK/xdg"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman          # software rendering: no GPU on CI
export LIBGL_ALWAYS_SOFTWARE=1

cat > "$WORK/inner.sh" <<INNER
#!/usr/bin/env bash
export XDG_SESSION_TYPE=wayland
# BUFFR_LOG_CONSOLE not needed here — this harness judges pixels, not logs.
export RUST_LOG=info,buffr_core=debug
cd "$REPO"
"$BIN" --private ${URLS[*]} > "$WORK/app.log" 2>&1 &
APP=\$!
# Give CEF time to navigate + paint before capturing.
sleep 5
grim "$SHOT"
kill \$APP 2>/dev/null
sleep 0.5
swaymsg exit
INNER
chmod +x "$WORK/inner.sh"

cat > "$WORK/sway.conf" <<CONF
# No server-side decorations: the client area must start at y=0 so the
# tab strip rows land exactly at the top of the capture.
default_border none
output HEADLESS-1 mode 1280x800
exec $WORK/inner.sh
CONF

timeout 120 sway --unsupported-gpu -c "$WORK/sway.conf" >"$WORK/sway.log" 2>&1

if [ ! -f "$SHOT" ]; then
  echo "render: no screenshot captured (grim failed?)"
  exit 90
fi
convert "$SHOT" ppm:- > "$OUT" 2>/dev/null || { echo "render: convert to ppm failed"; exit 90; }
python3 "$REPO/tests/e2e/render_check.py" "$OUT"
