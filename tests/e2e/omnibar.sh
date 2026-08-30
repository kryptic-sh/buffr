#!/usr/bin/env bash
# Omnibar e2e: `o` opens a new tab + omnibar; typing a URL and pressing
# Enter must navigate to that URL (not a garbage "about:blankhttps://…"
# concatenation) and close the omnibar.
#
# Drives real keystrokes through BUFFR_E2E_KEYS: the headless CI seat has
# no input devices, so buffr's key-injection hook posts synthetic keys
# through the exact dispatch path a physical keyboard takes.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO/target/debug/buffr-app"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

[ -x "$BIN" ] || { echo "MISSING BINARY $BIN (cargo build -p buffr-app)"; exit 90; }

export XDG_RUNTIME_DIR="$WORK/xdg"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman          # software rendering: no GPU on CI
export LIBGL_ALWAYS_SOFTWARE=1

# `o` opens a new tab + omnibar; type https://e2e.omnibar.test; Enter.
export BUFFR_E2E_KEYS="char:o,char:h,char:t,char:t,char:p,char:s,char::,char:/,char:/,char:e,char:2,char:e,char:.,char:o,char:m,char:n,char:i,char:b,char:a,char:r,char:.,char:t,char:e,char:s,char:t,named:Return"

cat > "$WORK/inner.sh" <<INNER
#!/usr/bin/env bash
export XDG_SESSION_TYPE=wayland
export RUST_LOG=info,buffr_core=debug
export BUFFR_E2E_KEYS="$BUFFR_E2E_KEYS"
cd "$REPO"
"$BIN" --private "file://$WORK/start.html" > "$WORK/buffr.log" 2>&1 &
APP=\$!
sleep 13
kill \$APP 2>/dev/null
swaymsg exit
INNER
chmod +x "$WORK/inner.sh"

cat > "$WORK/sway.conf" <<CONF
output HEADLESS-1 mode 1280x800
exec $WORK/inner.sh
CONF

# start.html is a real page (pre-fill should apply to it, but `o` opens a
# NEW tab whose URL is about:blank — that is the empty-omnibar case).
cat > "$WORK/start.html" <<'HTML'
<html><body>omnibar e2e start</body></html>
HTML

timeout 60 sway --unsupported-gpu -c "$WORK/sway.conf" >"$WORK/sway.log" 2>&1

LOG="$WORK/buffr.log"
# The omnibar must navigate to the typed URL, not a concatenated placeholder.
if grep -q "navigate url=https://e2e.omnibar.test/" "$LOG"; then
  # ...and not to a garbage about:blank-prefixed URL.
  if grep -q "navigate url=about:blankhttps" "$LOG"; then
    echo "FAIL  omnibar: navigated to a garbage about:blank-prefixed URL"
    grep -E "navigate url=" "$LOG" | tail -3 | sed 's/^/      /'
    exit 1
  fi
  echo "PASS  omnibar: typed URL navigates (no about:blank concatenation)"
  exit 0
else
  echo "FAIL  omnibar: expected navigate to https://e2e.omnibar.test/ in log"
  grep -iE "navigate|omnibar|e2e:" "$LOG" | tail -8 | sed 's/^/      /'
  exit 1
fi
