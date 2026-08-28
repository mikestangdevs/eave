# This branch is a healed copy of eave's history

`archive/full-history-healed` carries all 324 commits of eave's history up to
2026-08-28. **Its commit ids are not the original ones**, and two files carry
substituted content at many of those commits. This file explains exactly what
was changed and why, so nothing here is mistaken for the record it replaces.

## What was lost

Two blobs were deleted from the local object store and could not be recovered
from this machine — not from a pack (the store had none, everything was loose),
not from another clone, and not from iCloud (no `.icloud` placeholders existed;
the files were gone, not evicted):

| blob | file | first appeared |
| --- | --- | --- |
| `8279261e2626a80a0602a228d4ad0e95ab6128c9` | `crates/eave-app/src/loaders.rs` | `dc0f873a`, 2026-08-18 |
| `b10a6adce15bb85de88b04e4ef607b4d3c09a54d` | `crates/eave-core/src/thresholds.rs` | `35db9572`, 2026-08-16 |

Neither file is missing from the working tree. What was lost is one *version*
of each.

Because a blob's id is baked into every tree and commit above it, git could not
enumerate the object graph, and no push of the real history could succeed:

    $ git push origin main
    fatal: unable to read 8279261e2626a80a0602a228d4ad0e95ab6128c9
    fatal: the remote end hung up unexpectedly

That is why `main` on this remote is a fresh root commit rather than this
history, and why this branch exists beside it.

## What was substituted

Each lost blob was replaced with the nearest surviving version of the same file.
`git replace` was tried first and rejected: `git fast-export` and `pack-objects`
deliberately ignore replace refs, so the rewrite was done with
`git filter-branch --index-filter`, which rewrites index entries and never reads
blob content — the only reason it could run at all.

| file | substituted with | direction | commits touched |
| --- | --- | --- | --- |
| `crates/eave-app/src/loaders.rs` | `6427b859`, the 2026-08-24 version | forward — the file was *created* with the lost blob, so no earlier version exists | 229 |
| `crates/eave-core/src/thresholds.rs` | `fb8d1ce3`, the 2026-08-15 version | backward — the version immediately before the lost one | 202 |

So: between 2026-08-18 and 2026-08-24, `loaders.rs` reads as its later self, and
between 2026-08-16 and 2026-08-22, `thresholds.rs` reads as its earlier self.
Every other file at every commit is untouched and exact.

## What this branch is good for, and what it is not

It is a faithful record of the *shape* of the work — every message, author,
date, parent and every other file — and a real off-machine backup, which the
history did not have before.

It is not byte-exact for those two files in those windows, and its commit ids do
not match the ones written throughout `CLAUDE.md`, `EAVE_CURRENT`,
`docs/progress.md` and `docs/reports/*`. Those documents refer to the ORIGINAL
ids, which remain valid in the local repository on `archive/full-history-pre-github`.
Resolve any SHA quoted in project documentation against that ref, not this one.

`main` is unrelated to this branch by design: it is a new root holding the same
tree, chosen so that every SHA already written down stays valid.
