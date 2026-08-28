#!/usr/bin/env python3
"""Find the Slice E1 probe squares in a capture.

    e1-probe.py shot.png
    e1-probe.py shot.png --json
    e1-probe.py shot.png --centre link --window-w 1080

`scripts/e1-page.html` pins a 40x40 lime square to the viewport's top-left,
a 40x40 magenta square to its bottom-right, and sits the link on a flat
navy chip. Between them they answer the two questions a resize capture has
to answer and a screenshot alone cannot:

* **Was the page SCALED to fit?** A scaled frame scales the squares, so
  their size stops being 40 logical pixels. That is a direct measurement of
  the stretch, not an inference from how the text looks.
* **Did the page actually RE-LAY OUT at the new size?** The magenta square
  is pinned to the viewport, so it only reaches the new bottom-right corner
  if Chromium ran layout there. A frame that was merely scaled has it in
  the old proportional place, which is a different number.

⚠️ **`screencapture` COLOUR-MANAGES, and the first version of this script
failed on exactly that.** The page's `#00ff00` comes back as
`(117, 251, 76)` on this display and its `#ff00ff` as `(234, 51, 247)`: the
shot is written in the display's profile, not the page's. Near-neutral
colours survive — the fixture's `#12131a` background reads back exactly —
which is why the effect is easy to miss until a saturated probe is used,
and why "the page did not paint" was the first, wrong, conclusion.

So a probe is identified by a **family plus a vote**: filter the image's
colours to those in the right hue family, take the most common one, and
measure THAT exact colour's bounding box. The vote is what keeps the macOS
traffic-light green — 500 pixels of `(101, 196, 102)` in the titlebar,
above the page — from being counted as the top-left probe, which is what a
predicate on its own did.
"""

import json
import sys
from collections import Counter

from PIL import Image

# Each probe is a family, not a value. See the colour-management note.
FAMILIES = {
    # Green-dominant by a wide margin.
    "tl": lambda p: p[1] > p[0] + 60 and p[1] > p[2] + 60,
    # Red and blue both high, green well below both.
    "br": lambda p: p[0] > 140 and p[2] > 140 and p[1] < p[0] - 50 and p[1] < p[2] - 50,
    # A mid navy: blue clearly ahead, and not a near-black.
    "link": lambda p: 60 < p[2] < 160 and p[2] > p[0] + 20 and p[2] > p[1] + 15,
}

# A probe square is 40x40 logical: 1600 pixels at 1x, 6400 at 2x. A blob
# smaller than this is chrome that happens to be the right hue.
MIN_PIXELS = 600


def find(im, which):
    """The probe's bounding box as `(x, y, w, h)` in device pixels, or None."""
    counts = Counter(im.getdata())
    pred = FAMILIES[which]
    candidates = [(n, c) for c, n in counts.items() if pred(c) and n >= MIN_PIXELS]
    if not candidates:
        return None
    _, colour = max(candidates)
    w, h = im.size
    px = im.load()
    x0 = y0 = None
    x1 = y1 = -1
    for y in range(h):
        for x in range(w):
            if px[x, y] == colour:
                if x0 is None or x < x0:
                    x0 = x
                if y0 is None or y < y0:
                    y0 = y
                x1 = max(x1, x)
                y1 = max(y1, y)
    if x0 is None:
        return None
    return (x0, y0, x1 - x0 + 1, y1 - y0 + 1)


def main() -> None:
    path = sys.argv[1]
    args = sys.argv[2:]
    im = Image.open(path).convert("RGB")

    # `--centre <tl|br|link> --window-w <points>` prints one point in WINDOW
    # POINTS, for a caller about to click it. The scale is derived from the
    # stated window width rather than assumed, the way `d1-probe.py` derives
    # it — an assumed 2x on a 1x display puts the click in the wrong quarter
    # of the window, and the shot then looks like the feature is broken.
    if "--centre" in args:
        which = args[args.index("--centre") + 1]
        window_w = float(args[args.index("--window-w") + 1])
        scale = im.width / window_w
        if abs(scale - round(scale)) > 0.02:
            raise SystemExit(
                f"{path} is {im.width}px for a {window_w:.0f}pt window — "
                f"scale {scale:.3f} is not integral, so a point coordinate "
                f"cannot be mapped without guessing"
            )
        scale = round(scale)
        b = find(im, which)
        if b is None:
            raise SystemExit(f"the {which} probe is not in {path}")
        x, y, w, h = b
        print(f"{(x + w // 2) // scale} {(y + h // 2) // scale}")
        return

    out = {"image": [im.width, im.height]}
    for which in ("tl", "br", "link"):
        out[which] = find(im, which)
    if "--json" in args:
        print(json.dumps(out))
        return

    def fmt(b):
        return "absent" if b is None else " ".join(str(v) for v in b)

    print(
        f"image {im.width}x{im.height} | tl {fmt(out['tl'])} "
        f"| br {fmt(out['br'])} | link {fmt(out['link'])}"
    )


if __name__ == "__main__":
    main()
