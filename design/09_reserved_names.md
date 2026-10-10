# Reserved Names (`.omemfs-` prefix)

## Overview

omemfs places special files and directories in the working tree alongside user
files. These are identified by the `.omemfs-` prefix and form a reserved
namespace. User files should not use names that start with `.omemfs-`.

The `.omemfs/` directory (note: no hyphen) at the repository root is a separate
concept — it is the metadata store and is not covered by this document.

---

## Naming rules

### File-adjacent names

`<original-name>.omemfs-<kind>[-<subtype>]`

A file placed next to an original file and associated with it. Examples:
- `report.pdf.omemfs-stub` — stub marker for `report.pdf`
- `main.rs.omemfs-conflict-base` — conflict helper for `main.rs`

### Directory-interior names

`<dir>/.omemfs-<kind>`

A file placed inside a directory and associated with the directory as a whole.
Example:
- `archive/.omemfs-stub` — stub marker for the `archive/` directory

### Standalone configuration files

`.omemfs-<kind>`

A file placed at any directory level that controls omemfs behaviour for the
subtree below. Example:
- `.omemfs-filter` — filter configuration (ignore and aggregate rules)

---

## Reserved kinds

### `.omemfs-stub` — stub system

Marks a file or directory as physically absent from the working tree while
remaining logically present.

- **File stub**: `<name>.omemfs-stub` alongside where `<name>` would be.
- **Directory stub**: `<dir>/.omemfs-stub` inside the (otherwise empty) directory.

Stubs record `hash`, `size`, `mtime`, `mode`, and (for tree stubs) `blob_count`.
`omemfs expand` uses this information to materialise the original content from
the local object cache or the remote backend.

**Not tracked**: stub files are never uploaded to the remote. They are local
working-tree markers only.

For the full specification, see [`08_stub_system.md`](08_stub_system.md).

---

### `.omemfs-conflict-{base,local,remote}` — conflict helpers

Written by `omemfs pull` when a path conflicts between local and remote changes.
Three files are placed alongside the conflicting path:

| File | Contents |
|------|----------|
| `<path>.omemfs-conflict-base` | Content at `clone_root` (the last common synced state) |
| `<path>.omemfs-conflict-local` | Content from the current working tree |
| `<path>.omemfs-conflict-remote` | Content from the remote root |

The original file (`<path>`) is left unchanged (local content preserved).

The user inspects the three versions and edits `<path>` to the desired merged
result. `omemfs push` **refuses to run** if any unresolved conflict helper files
are present (it reports an error listing the paths with unresolved conflicts).
Users resolve conflicts via `omemfs conflict accept-local`, `omemfs conflict
accept-remote`, or `omemfs conflict clean`, which remove the helper files before
`push` can proceed. Exception: conflict helpers inside directories excluded by
`.omemfs-filter` do not block `push`.
`omemfs restore <path>` removes the helpers for the restored path.

**Not tracked**: conflict helpers are excluded from working tree scans. They are
never uploaded to the remote and are never included in tree objects.

For the full specification, see [`03_sync_model.md`](03_sync_model.md).

---

### `.omemfs-filter` — filter configuration

A tracked configuration file that controls two aspects of working tree handling
for the subtree below the file:

- `[ignore]` section: paths excluded from push/scan.
- `[aggregate]` section: directories collapsed to a single line in `omemfs ls`.

**Tracked**: `.omemfs-filter` files are uploaded to the remote as ordinary
tracked files and are included in tree objects.

Can be placed at any level in the working tree; patterns in each file apply
only within the directory containing the file.

`omemfs clone` creates a default `.omemfs-filter` at the repository root if
none exists on the remote.

For the full specification, see [`05_ignore_and_aggregate.md`](05_ignore_and_aggregate.md).

---

## Behaviour by command

| Kind | `push` | `pull` | `ls` | `restore` |
|------|--------|--------|------|-----------|
| `.omemfs-stub` | Reads hash from stub; does not re-upload object | Skips download for stubbed paths; updates stub record via reconcile | Shows as entry with `Z=S` or `Z=s` | Updates stub record to match `clone_root` |
| `.omemfs-conflict-*` | Excluded from scan; **push refuses** if any are present (unless in an ignored directory); user resolves via `omemfs conflict accept-*` / `conflict clean` | Written on conflict | — | Deleted for the restored path |
| `.omemfs-filter` | Tracked as normal file; `[ignore]` section affects what else is scanned | Tracked; pulled as normal file | `[ignore]` section affects what is shown | Tracked; restored as normal file |

---

## Scan exclusions

The working tree scan (`scan_and_store`) always excludes:

- `.omemfs/` — the metadata directory
- `*.omemfs-stub` / `.omemfs-stub` — stub markers (handled separately)
- `*.omemfs-conflict-base` / `*.omemfs-conflict-local` / `*.omemfs-conflict-remote` — conflict helpers

These exclusions are hard-coded and apply regardless of `.omemfs-filter` settings.

### `.omemfs` exclusion scope

The name `.omemfs` (directory, no hyphen) is excluded at **every directory level**,
not only at the repository root. This applies to:

- Working tree scan (content under any `.omemfs/` subdirectory is never uploaded)
- `restore` (`.omemfs/` entries are never written or deleted by restore)
- Stub enumeration (stub discovery never descends into `.omemfs/`)
- Filter collection (`.omemfs-filter` files inside `.omemfs/` are never read)

Rationale: a working tree may contain nested omemfs checkouts or tool-generated
`.omemfs/` metadata directories. Excluding `.omemfs` at every level prevents
accidental upload or mutation of these metadata stores.

---

## Forward compatibility

Names matching `*.omemfs-*` or `.omemfs-*` that do not correspond to a known
kind should be treated as unknown reserved files:

- `push` (and the working-tree scan): warn once and skip (do not upload as a
  blob; do not delete). The file is never treated as regular content.
- `ls`: show as an entry with a warning annotation — the `Z` column shows `?`
  and the metadata columns (hash/size/mtime) show `-`.
- `pull` / `restore`: do not create or modify unknown reserved files.

This ensures that a working tree containing reserved files produced by a newer
version of omemfs can be read by an older version without data loss.

---

## Reserved names in remote tree entries (untrusted input)

Everything above in this document governs how the **local working tree** is
scanned: which local file/directory names `push`/`scan_and_store` treat as
reserved and skip rather than upload. That is a statement about local
filesystem content that omemfs itself only ever writes in the shapes
described above (a stub marker, a conflict helper, a filter file, etc.).

A **tree object read from the remote** is a different trust boundary. Its
entry names come from whatever bytes the remote happens to hold — written by
any client (or, if the remote has been tampered with, by an attacker) that
can produce arbitrary JSON and call it a tree object. Nothing on the read
path guarantees that a remote tree's entry names were ever produced by an
omemfs `scan_and_store`, so the local-scan exclusions above do not, by
themselves, protect a `pull` / `clone` / `restore` / `expand` that walks a
remote tree.

**Rule**: `Tree::deserialise` (`src/object.rs`) rejects, with
`Error::InvalidObject`, any entry whose `name`:

- is exactly `.omemfs` (the metadata directory name, no hyphen), or
- matches the reserved `.omemfs-` namespace (contains the substring
  `.omemfs-` — the same predicate as `is_reserved_name` below), covering both
  the standalone/dir-interior form `.omemfs-<kind>` and the file-adjacent form
  `<name>.omemfs-<kind>`,

**except** for the name `.omemfs-filter` exactly, which is accepted. See
"The `.omemfs-filter` exception" below for why this one name is carved out.

This is enforced regardless of whether the name corresponds to a *known*
reserved kind that `scan_and_store` excludes (`.omemfs-stub`,
`.omemfs-conflict-*`) or an *unknown* one — unlike the local-scan side (see
"Forward compatibility" above), a remote tree entry never gets the "unknown
reserved name, skip and warn" treatment. A tree is untrusted structured data
before it has been validated; there is no forward-compatible reserved kind
that a future omemfs version could legitimately want to smuggle in as a
**tree entry name**, because (`.omemfs-filter` aside) reserved names are, by
definition, local working-tree markers that `scan_and_store` excludes from
every tree it builds in the first place (see "Scan exclusions" above) — a
well-formed tree built by any version of omemfs, past or future, never
contains one of these *excluded* names as an entry. So an entry name in this
namespace other than `.omemfs-filter`, wherever it came from, is treated as
evidence of a hand-crafted or corrupted tree object, not as a
forward-compatibility case.

**The `.omemfs-filter` exception**: `.omemfs-filter` is not a local-scan
exclusion at all — it is the one reserved name that belongs to a different
category entirely. As specified above ("`.omemfs-filter` — filter
configuration") and in
[`05_ignore_and_aggregate.md`](05_ignore_and_aggregate.md), `.omemfs-filter`
is a **tracked** file: `scan_and_store` includes it in the tree it builds
like any other ordinary file, `push` uploads it, and `clone`/`pull` download
it as a normal tracked file. `omemfs clone --new` writes a default
`.omemfs-filter` at the repository root, so it is present in essentially
every repository's root tree. A well-formed tree built by `scan_and_store`
therefore *routinely* contains a `.omemfs-filter` entry — the opposite of
`.omemfs-stub` / `.omemfs-conflict-*`, which `scan_and_store` always excludes
and which must never appear in a well-formed tree. Consequently
`Tree::deserialise` must accept `.omemfs-filter` as an ordinary tree entry
name, or every `push`/`pull`/`clone`/`ls` on a repository with a filter file
(i.e. nearly every repository) would fail. The exception applies only to the
exact standalone name `.omemfs-filter` — `.omemfs-filter` is always placed
directly inside a directory (one filter file governs the directory it lives
in and the subtree below), never in the file-adjacent form
`<name>.omemfs-filter`, so no file-adjacent variant needs (or gets) this
exception; a name like `report.pdf.omemfs-filter` is not a real
`.omemfs-filter` file under this design and remains rejected. Because
`.omemfs-filter` is not a name `scan_and_store` ever excludes, this exception
does not weaken the "no forward-compatible reserved kind" argument above:
`.omemfs-filter` was never one of the excluded names that argument is about.

**Why this matters**: `Tree::deserialise` is the single point every
tree-reading command goes through (design/01_object_model.md, "Invariants"
#5). Without the general check, a tree entry literally named `.omemfs`
would, once joined onto a working-tree directory by
`pull`/`clone`/`restore`/`expand`, overwrite the repository's own metadata
directory (config, DEK, clone_root, local object cache, stat cache, lock
files) with attacker-controlled content. Likewise, an entry named
`<victim>.omemfs-stub` or `<path>.omemfs-conflict-remote` would, once
materialised, be picked up by a later local scan as a *bona fide* stub
marker or conflict helper — letting a malicious remote inject an arbitrary
stub (pointing `<victim>` at any hash) or splice attacker-chosen content
into a conflict resolution. `.omemfs-filter` carries no equivalent risk: a
malicious remote can already fully control the *contents* of a tracked
`.omemfs-filter` file (it is ordinary tracked content, just like any other
blob a hostile remote could serve), so accepting the entry *name* grants no
additional capability beyond what accepting any other tracked file's
content already grants.

**Relationship to the local-scan rules**: this check answers a different
question than the rest of this document. The local-scan rules (`push` and
the sections above) answer "how should omemfs handle a name it finds while
walking the local working tree?" — the answer is "recognise it as
infrastructure and treat it specially (tracked, for `.omemfs-filter`;
excluded, for `.omemfs-stub`/`.omemfs-conflict-*`), or as an unknown reserved
name and skip it with a warning." This check answers "should a name coming
from a remote tree ever be accepted as an ordinary tree-entry name at all?"
— the answer is unconditionally no, *except* for `.omemfs-filter`, which the
local-scan side already treats as ordinary tracked content rather than
something to exclude, so this check accepts it too for consistency. The two
checks share the same name predicate (`is_reserved_name`, defined once in
`src/object.rs` and reused by `src/scan.rs`) but apply it for opposite
purposes: the local-scan side uses it (together with the `.omemfs-filter`
carve-out already present in `is_known_reserved_name`) to *exclude*
non-tracked matching names from what gets scanned into a tree in the first
place; `Tree::deserialise` uses it (with its own, symmetrical
`.omemfs-filter` carve-out) to *reject* a tree that somehow contains one of
the *excluded* names anyway, because a well-formed tree built by
`scan_and_store` can never contain one of those — though it can, and
routinely does, contain a `.omemfs-filter` entry.

design/01_object_model.md's "Invariants" #5 has been updated to reflect that
`validate_entry_name` also enforces this rule, including the `.omemfs-filter`
exception.
