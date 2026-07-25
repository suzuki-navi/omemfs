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
