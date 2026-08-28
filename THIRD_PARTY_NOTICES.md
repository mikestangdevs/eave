# Third-party notices

eave embeds the following third-party assets in its binary. This file is the
attribution those licences require; it is not a courtesy list.

## Icons

| Component | What | Licence | Source |
| --- | --- | --- | --- |
| **Solar Icons** (Linear weight) | The 52 line glyphs in `crates/eave-app/assets/icons/` | **CC BY 4.0** | 480 Design — https://www.figma.com/community/file/1166831539721848736 |

> **Solar Icons by 480 Design**, licensed under
> [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

CC BY 4.0 requires attribution wherever the work is redistributed, and these
files travel inside the binary via `include_bytes!`. Vendored out of
`comet/crates/ui/assets/icons/` in the same workspace, which carries the same
set under the same terms.

The five `status-*.svg` glyphs are **not** Solar — they are eave's own, drawn
for Slice P3, because the Solar set has no VCS status family. They carry no
upstream licence.

## Provider marks

`claude-mark.svg`, `openai-mark.svg`, `cursor-mark.svg`, `grok-mark.svg`,
`hermes-mark.svg` and `pi-mark.svg` are the respective vendors' own logos and
remain their trademarks. eave uses them **nominatively** — to identify which
agent a session is running — in the same way a browser shows a site's own
favicon. They are not eave's marks, they do not imply endorsement by or
affiliation with those vendors, and they are not licensed to anyone who
redistributes eave.

Each vendor's trademark policy governs its own mark. If a vendor objects to a
particular use, the fix is to drop that mark, not to redraw it: a "close
enough" imitation of someone's logo is a worse position than not using one.

## Fonts

None embedded. eave asks the platform for `theme::SANS` / `theme::MONO` by
name and takes what the system resolves.

## Build tooling

`scripts/package-macos.sh` is adapted from **comet**'s script of the same name
(Copyright © 2026 Wing, **MIT**), in the same workspace. MIT requires the
copyright notice and permission notice to travel with substantial portions of
the work, and the bundling, signing and notarisation sequence here is
substantially theirs — the codesign flags, the staple-before-tar ordering, and
the `dmgbuild` settings block in particular.

The MIT text is reproduced with the upstream source; the notice it requires is
this entry. Nothing from comet is embedded in eave's binary: this is build
tooling, and it is listed for the same reason the icons are — because the
licence asks, not as a courtesy.

Where this script diverges from theirs the divergence is marked in a comment:
the icon step is optional (eave has no app mark yet), the dmg carries no
background artwork, and there is no Linux target.
