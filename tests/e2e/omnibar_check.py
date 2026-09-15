#!/usr/bin/env python3
"""Pixel checks for the omnibar e2e suite (tests/e2e/omnibar.sh).

Modes:

  typed <ppm>
      The omnibar is open with ONE typed character. Locate the popup by its
      border, then assert the input row shows that character, not just the
      cursor. Regression coverage for "the first letter typed into the
      omnibar is cut off": the scroll window over-scrolled by one, so the
      first char was never drawn and only the 2-px cursor bar remained.

  closed <ppm>
      Enter was pressed on a typed URL. Assert no popup border is left over
      the page and the navigation target's flat colour fills the browser
      region. Regression coverage for "the omnibar stays stuck on screen
      after Enter until a tab switch": the chrome frame after the popup
      closed uploaded only the tab-strip / statusline bands, so the GPU
      chrome texture kept the popup pixels over the page.

Exit 0 on pass, 1 on failure with a message.
"""
import sys

# Mirrors apps/buffr-app/src/chrome_paint.rs.
POPUP_BORDER_COLOR = (0x7A, 0xA2, 0xF7)
POPUP_BORDER = 2
# Mirrors buffr_ui::{TAB_STRIP_HEIGHT, STATUSLINE_HEIGHT, INPUT_HEIGHT}.
TAB_STRIP_HEIGHT = 34
STATUSLINE_HEIGHT = 30
INPUT_HEIGHT = 28

# A popup border row is one unbroken run of the border colour at least this
# wide (comfortably below chrome_paint.rs's OMNIBAR_POPUP_MIN_WIDTH).
MIN_BORDER_RUN = 150
# The input row with the cursor alone lights at most the 2-px cursor bar;
# a drawn glyph adds several more columns.
MIN_TEXT_COLUMNS = 5
# tests/e2e/pages/omnibar_target.html background.
TARGET_COLOR = (0x3C, 0x8C, 0x50)
COLOR_TOLERANCE = 24
MIN_TARGET_FRACTION = 0.90


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    assert data[:2] == b"P6", f"{path}: not a P6 PPM"
    parts = data.split(b"\n", 3)
    w, h = map(int, parts[1].split())
    return parts[3], w, h


def pixel(px, w, x, y):
    i = (y * w + x) * 3
    return px[i], px[i + 1], px[i + 2]


def longest_border_run(px, w, y):
    """(start_x, length) of the longest border-colour run on row y."""
    best = (0, 0)
    start = None
    for x in range(w + 1):
        on = x < w and pixel(px, w, x, y) == POPUP_BORDER_COLOR
        if on and start is None:
            start = x
        elif not on and start is not None:
            if x - start > best[1]:
                best = (start, x - start)
            start = None
    return best


def popup_top(px, w, h):
    """(x, y) of the popup's top-left border corner, or None."""
    for y in range(TAB_STRIP_HEIGHT, h - STATUSLINE_HEIGHT):
        x, run = longest_border_run(px, w, y)
        if run >= MIN_BORDER_RUN:
            return x, y, run
    return None


def is_text_pixel(rgb):
    # The input text and cursor are near-white; the `> ` prefix is the blue
    # accent, which this rejects by its channel spread.
    return min(rgb) > 150 and max(rgb) - min(rgb) < 40


def check_typed(path):
    px, w, h = read_ppm(path)
    top = popup_top(px, w, h)
    if top is None:
        return "no omnibar popup border found — the omnibar never opened"
    x0, y0, run = top
    inner_x = x0 + POPUP_BORDER
    inner_y = y0 + POPUP_BORDER
    inner_w = run - 2 * POPUP_BORDER
    columns = set()
    for y in range(inner_y, min(inner_y + INPUT_HEIGHT, h)):
        for x in range(inner_x, inner_x + inner_w):
            if is_text_pixel(pixel(px, w, x, y)):
                columns.add(x)
    if len(columns) < MIN_TEXT_COLUMNS:
        return (
            f"input row lights only {len(columns)} text column(s) "
            f"(< {MIN_TEXT_COLUMNS}): the typed character was not drawn"
        )
    return None


def check_closed(path):
    px, w, h = read_ppm(path)
    top = popup_top(px, w, h)
    if top is not None:
        x0, y0, run = top
        return (
            f"omnibar popup border still on screen at ({x0},{y0}), "
            f"{run}px wide, after the omnibar closed"
        )
    y_range = range(TAB_STRIP_HEIGHT, h - STATUSLINE_HEIGHT)
    total = 0
    matching = 0
    for y in y_range:
        for x in range(w):
            total += 1
            rgb = pixel(px, w, x, y)
            if all(abs(c - t) <= COLOR_TOLERANCE for c, t in zip(rgb, TARGET_COLOR)):
                matching += 1
    fraction = matching / total if total else 0.0
    if fraction < MIN_TARGET_FRACTION:
        return (
            f"only {fraction:.0%} of the browser region shows the target page "
            f"colour (< {MIN_TARGET_FRACTION:.0%}): navigation or page paint "
            "did not land, or chrome is covering it"
        )
    return None


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("typed", "closed"):
        print("usage: omnibar_check.py <typed|closed> <shot.ppm>")
        return 2
    mode, path = sys.argv[1], sys.argv[2]
    err = check_typed(path) if mode == "typed" else check_closed(path)
    if err:
        print(f"OMNIBAR FAIL ({mode}): {err}")
        return 1
    print(f"OMNIBAR OK ({mode})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
