#!/usr/bin/env python3
"""Read one pixel out of a capture, in WINDOW POINTS.

    d1-probe.py shot.png 100 400

`screencapture` writes a 2x file on this display, so a caller thinking in
gpui's coordinates has to divide — and getting that wrong is silent: you
sample a plausible pixel from the wrong place and the assertion passes.
The scale is read from the image against a stated window width rather than
assumed, so a non-retina display or a resized window fails loudly.

Exists because **`cmp -s` is not a strong enough guard for a frame capture.**
Slice D1's first capture pair differed by a hover wash on one row — the
toggle click had missed entirely and the rail was still on screen in the
"shut" shot — and a byte comparison called that a pass. A frame test has to
assert about the frame: what is at x=100 when the menu is open is the rail,
and what is there when it is shut is the content.
"""

import sys

from PIL import Image

WINDOW_W = 1080.0


def main() -> None:
    path, x, y = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
    im = Image.open(path).convert("RGB")
    scale = im.width / WINDOW_W
    if abs(scale - round(scale)) > 0.02:
        raise SystemExit(
            f"{path} is {im.width}px for a {WINDOW_W:.0f}pt window — scale "
            f"{scale:.3f} is not integral, so a point coordinate cannot be "
            f"mapped without guessing"
        )
    px = im.getpixel((int(x * scale), int(y * scale)))
    print(f"{px[0]} {px[1]} {px[2]}")


if __name__ == "__main__":
    main()
