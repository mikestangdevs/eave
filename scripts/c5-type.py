#!/usr/bin/env python3
"""Post keystrokes to the frontmost window through Quartz.

    c5-type.py cmd-l doc

sends ⌘L and then the three characters `d`, `o`, `c`.

**Synthetic typing DOES reach a gpui window.** That is worth stating
plainly because this repo's notes recorded the opposite for a while and a
slice was planned around the belief. What is true is narrower and is the
whole trap: a *raised* window is not a *key* window, and a key event posted
to a non-key window is delivered to whatever is key instead — which from a
screenshot is indistinguishable from the app ignoring the keystroke. So the
caller must click the window first. `c5-capture.sh` does.

`CGEventPost(kCGHIDEventTap, …)` rather than `CGEventPostToPid`: the HID tap
is what `as-click.py` already uses for the same reason, and posting to a pid
skips the window server's key-window routing, which is precisely the part
that has to work here.

Run with the PATH `python3` — `/usr/bin/python3` has no Quartz module.
"""

import subprocess
import sys
import time

import Quartz

# Virtual keycodes are POSITIONAL, not character codes: `kVK_ANSI_A` is 0
# because of where the key sits, not because of what it prints. Only the
# characters the capture actually types are listed; adding one is a lookup
# in `HIToolbox/Events.h`, not a computation.
KEYCODES = {
    "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7,
    "c": 8, "v": 9, "b": 11, "q": 12, "w": 13, "e": 14, "r": 15,
    "y": 16, "t": 17, "1": 18, "2": 19, "3": 20, "4": 21, "6": 22,
    "5": 23, "9": 25, "7": 26, "8": 28, "0": 29, "o": 31, "u": 32,
    "i": 34, "p": 35, "l": 37, "j": 38, "k": 40, "n": 45, "m": 46,
    ".": 47, "/": 44, "-": 27, " ": 49, "=": 24,
    # Slice E2 — ⌘[ / ⌘]. Positional ANSI bracket keys (`kVK_ANSI_LeftBracket`
    # / `kVK_ANSI_RightBracket`), not the characters, same as every other
    # entry here.
    "[": 33, "]": 30,
    # Slice S1 — ⌘, opens Settings. `kVK_ANSI_Comma`.
    ",": 43,
}

# Named keys, which are neither characters nor chords. `send` taps these
# through Quartz for the same reason it taps chords that way: a key with no
# character to insert is matched on its keycode, and Quartz delivers those
# to this window reliably. (Slice E1 added them for escape and the zoom
# chords; `=` and `0` are in KEYCODES above and reachable as `cmd-=`.)
NAMED = {
    "escape": 53,
    "return": 36,
    "enter": 36,
    "tab": 48,
    "delete": 51,
    "left": 123,
    "right": 124,
    "down": 125,
    "up": 126,
}

MODIFIERS = {
    "cmd": Quartz.kCGEventFlagMaskCommand,
    "shift": Quartz.kCGEventFlagMaskShift,
    "ctrl": Quartz.kCGEventFlagMaskControl,
    "alt": Quartz.kCGEventFlagMaskAlternate,
}


def tap(keycode: int, flags: int = 0, char: str | None = None) -> None:
    """One key down/up pair.

    **`char` is not optional for anything that has to produce text**, and
    this is the trap that cost this script a run. A synthesized
    `CGEventCreateKeyboardEvent` carries a keycode and NO unicode string,
    and gpui's `Keystroke::key_char` comes from the NSEvent's characters —
    so the app sees a key press with nothing to insert. `⌘L` worked (a
    chord is matched on the keycode) and the three letters after it did
    not, which from a screenshot looks like an omnibox that ignores typing.

    It is the same shape as the CEF spike's `native_key_code: 0` finding:
    the event is well-formed, the field it needs is the one nobody set, and
    the failure is silent and plausible.
    """
    for down in (True, False):
        event = Quartz.CGEventCreateKeyboardEvent(None, keycode, down)
        if flags:
            Quartz.CGEventSetFlags(event, flags)
        if char is not None:
            Quartz.CGEventKeyboardSetUnicodeString(event, len(char), char)
        Quartz.CGEventPost(Quartz.kCGHIDEventTap, event)
        time.sleep(0.02)


def send(token: str) -> None:
    """A named key (`escape`), a chord (`cmd-shift-l`), or a run of plain
    characters (`doc`)."""
    if token in NAMED:
        tap(NAMED[token])
        return
    if "-" in token and len(token) > 1:
        *mods, key = token.split("-")
        flags = 0
        for m in mods:
            if m not in MODIFIERS:
                raise SystemExit(f"unknown modifier {m!r} in {token!r}")
            flags |= MODIFIERS[m]
        code = KEYCODES.get(key, NAMED.get(key))
        if code is None:
            raise SystemExit(f"unknown key {key!r} in {token!r}")
        tap(code, flags)
        return
    # **Plain text goes through System Events, not through Quartz**, and
    # that split is the finding rather than a preference.
    #
    # A Quartz `CGEventCreateKeyboardEvent` delivers CHORDS to this window
    # perfectly — ⌘L opened the omnibox on the first try — and delivers
    # CHARACTERS to it not at all. Setting the unicode string explicitly
    # (`CGEventKeyboardSetUnicodeString`, which is the documented fix for
    # exactly this) did not change it either: two capture runs came back
    # with the omnibox open on its seeded address and the three typed
    # letters nowhere.
    #
    # `System Events`'s `keystroke` does land. Note this is the OPPOSITE
    # split from the one `as-capture.sh` records for the mouse, where
    # System Events' `click at` does not reach this app's titlebar and
    # Quartz does — so "use Quartz for everything" is the wrong lesson from
    # that note, and this is the counterexample.
    #
    # Typing speed still matters: the omnibox asks the engine for typeahead
    # on every keystroke, and a burst with no gaps leaves the last query in
    # flight when the shutter opens.
    # ONE `osascript` call for the whole run of characters, not one per
    # character. Per-character calls delivered only the FIRST one: each
    # `osascript` is a process launch that briefly changes what is
    # frontmost, and the keystrokes issued while that is settling go
    # nowhere. The capture came back with `d` in a field that had been
    # asked for `doc`.
    script = f'tell application "System Events" to keystroke "{token}"'
    subprocess.run(["osascript", "-e", script], check=True)


def main() -> None:
    for token in sys.argv[1:]:
        send(token)
        time.sleep(0.25)


if __name__ == "__main__":
    main()
