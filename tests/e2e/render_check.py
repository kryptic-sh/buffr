#!/usr/bin/env python3
"""Assert buffr's chrome (tab strip + statusline) is actually painted in a
grim capture of a headless-sway run.

Regression coverage for the "browser tabs are not rendering" bug: a paint
closure whose `Some(...)` guard on optional overlays (pinned-close confirm,
permissions prompt, omnibar, context menu) made the whole chrome paint a no-op
when none were open — the tab strip and statusline rendered as the swapchain's
clear color (uniform, no text, no pills).

Checks per capture:
  - tab strip rows [0, TAB_STRIP_HEIGHT): more than 3 distinct quantized
    colors AND at least 20 light pixels (title text / pill fill / active
    accent stripe present).
  - statusline rows [h - STATUSLINE_HEIGHT, h): same.

`--diff-ref <ppm>` additionally asserts the tab strip of this capture differs
from the reference capture's strip (used to prove opening a second tab / a new
tab changes the strip instead of leaving it identical).

Exit 0 on pass, 1 on failure with a message.
"""
import sys


TAB_STRIP_HEIGHT = 34
STATUSLINE_HEIGHT = 30
LIGHT = 100.0  # luminance threshold for text / pill / accent pixels
MIN_DISTINCT_COLORS = 4
MIN_LIGHT_PIXELS = 20


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    assert data[:2] == b"P6", f"{path}: not a P6 PPM"
    parts = data.split(b"\n", 3)
    w, h = map(int, parts[1].split())
    px = parts[3]
    img = [
        [tuple(px[(y * w + x) * 3:(y * w + x) * 3 + 3]) for x in range(w)]
        for y in range(h)
    ]
    return img, w, h


def strip_region(img, w, y0, y1):
    """(distinct quantized colors, light pixel count) for rows [y0, y1)."""
    colors = set()
    light = 0
    for y in range(y0, y1):
        for x in range(w):
            r, g, b = img[y][x]
            colors.add((r // 16, g // 16, b // 16))
            if 0.30 * r + 0.59 * g + 0.11 * b > LIGHT:
                light += 1
    return len(colors), light


def strip_bytes(img, w, y0, y1):
    out = bytearray()
    for y in range(y0, y1):
        for x in range(w):
            out += bytes(img[y][x])
    return bytes(out)


def main():
    args = sys.argv[1:]
    diff_ref = None
    if "--diff-ref" in args:
        i = args.index("--diff-ref")
        diff_ref = args[i + 1]
        del args[i:i + 2]
    shot = args[0]

    img, w, h = read_ppm(shot)
    if h < TAB_STRIP_HEIGHT + STATUSLINE_HEIGHT:
        print(f"RENDER FAIL: capture too small ({w}x{h})")
        return 1

    fails = []
    for name, y0, y1 in [
        ("tab strip", 0, TAB_STRIP_HEIGHT),
        ("statusline", h - STATUSLINE_HEIGHT, h),
    ]:
        ncolors, light = strip_region(img, w, y0, y1)
        if ncolors <= MIN_DISTINCT_COLORS:
            fails.append(
                f"{name}: only {ncolors} distinct colors (<= {MIN_DISTINCT_COLORS}; "
                "chrome not painted — uniform clear color?)"
            )
        if light < MIN_LIGHT_PIXELS:
            fails.append(
                f"{name}: only {light} light pixels (< {MIN_LIGHT_PIXELS}; "
                "no title text / pill / accent rendered)"
            )

    if diff_ref:
        ref, rw, rh = read_ppm(diff_ref)
        mine = strip_bytes(img, w, 0, TAB_STRIP_HEIGHT)
        theirs = strip_bytes(ref, rw, 0, TAB_STRIP_HEIGHT)
        if mine == theirs:
            fails.append(
                "tab strip identical to the reference capture — opening "
                "another tab / new tab did not change the strip"
            )

    if fails:
        print("RENDER FAIL: " + "; ".join(fails))
        return 1
    note = f" (differs from {diff_ref})" if diff_ref else ""
    print(f"RENDER OK: tab strip + statusline painted, {w}x{h}{note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
