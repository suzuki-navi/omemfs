# Stub System

## Overview

The stub system allows omemfs to keep the working tree physically smaller than the
full logical tree. Files and directories that are not needed locally are replaced
with small JSON marker files (stubs). The logical tree — as seen by `push`, `pull`,
and `ls` — is unaffected.

A stub is not a deletion. It records enough metadata (`hash`, `size`, `mtime`,
`mode`) for any command to treat the entry as if the content were present, and for
`omemfs expand` to materialise the file from the local object cache or the remote
backend.

---

## Stub file format

### File stub

A file stub replaces the original file with a sidecar file that carries the
`.omemfs-stub` suffix:

```
work_dir/
└── docs/
    └── large-report.pdf.omemfs-stub   ← stub; original file absent
```

The stub file contains a JSON object:

```json
{"target_type":"blob","hash":"a1b2c3...","size":5242880,"mtime":"2026-03-07T10:30:45Z"}
```

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `target_type` | `"blob"` or `"tree"` | Type of the original entry |
| `hash` | hex string (64 chars) | SHA-256 hash of the blob or tree object |
| `size` | integer | File size in bytes (blob) or total descendant blob size (tree) |
| `mtime` | ISO 8601 string (UTC) | Last-modified time of the original entry |
| `mode` | `"755"` or absent | Present only when the owner execute bit was set |
| `blob_count` | integer or absent | Number of descendant blobs (tree stubs only; 0 or absent means 0) |

JSON is stored minimised (no whitespace).

There is no `chunked` field. An earlier revision of this format carried a
`chunked` boolean recording whether the blob was stored as chunked objects,
but it had no reader: `expand` determines chunked-ness from the object's own
magic bytes when it actually materialises the blob (`codec::chunk::is_chunked`
on the real object, not on the stub record), so the stub never needs to know.
Removed (refactor-instructions.md C5) rather than kept unread, since keeping
it required `omemfs stub` and `expand`'s re-stub path to pay for an extra
object-store probe purely to compute a value nothing consumes. Serde ignores
unknown JSON fields by default (`StubRecord` does not set
`deny_unknown_fields`), so stub files written by an older binary that still
contain `"chunked":true` remain readable; the field is silently ignored.

### Directory stub

A directory stub keeps the directory entry on disk but places a `.omemfs-stub`
file inside it. All original contents are absent.

```
work_dir/
└── archive-2024/
    └── .omemfs-stub   ← stub for archive-2024/ tree; no other files inside
```

The directory stub file uses the same JSON format as a file stub, with
`target_type: "tree"`.

### Partial expansion

A directory may contain both a `.omemfs-stub` file and real files. This occurs
when a user creates new files inside a stubbed directory, or when `omemfs expand`
materialises only part of the tree.

```
work_dir/
└── archive-2024/
    ├── .omemfs-stub         ← stub recording the tree hash
    └── new-report.txt       ← real file added by the user
```

In this state, `new-report.txt` is a local addition to the stub's logical tree.
The next `push` merges the stub's recorded entries with the real files (real files
take priority over same-name stub entries), and writes the combined tree to the
remote.

After push, the stub's hash field is updated to reflect the merged tree.

---

## Stub threshold

`omemfs clone`, `omemfs pull`, and `omemfs expand` share a common threshold rule.
An entry whose `size` is at or above the threshold is placed as a stub instead of
being materialised.

Default threshold: **1 MiB** (1 048 576 bytes).

Override with `--stub-threshold <size>` on any of those commands. Size formats:
`1024` (bytes), `100K`, `100M`, `100G` (1024-based). `0` means expand everything.

### Threshold rules (clone / pull for new paths)

Applied entry-by-entry when placing entries from a tree object into the working tree:

| Entry kind | size vs threshold | Result |
|------------|-------------------|--------|
| blob | `< threshold` | Materialise: download object, write file |
| blob | `>= threshold` | Stub: write `<name>.omemfs-stub`, skip download |
| tree | empty, or `< threshold` | Materialise: create directory, recurse and apply rules to children |
| tree | `>= threshold` | Stub: create directory, write `.omemfs-stub`, skip children |
| symlink | any | Always materialise (symlinks are size 0) |

The threshold applies only to **new paths** — entries not yet present in the
working tree. Existing materialised files are never converted to stubs
automatically. Existing stubs are only updated via stub reconcile (see below).

---

## Stub invariant

A stub file must always reflect the same logical content as the same path in the
current `clone_root` tree (same `hash`, `size`, `mtime`, `mode`).

This invariant allows `ls`, `push`, and `pull` to treat a stub as an unchanged
entry without fetching the actual object:

- `ls`: a stub whose hash matches `clone_root` is shown as unchanged (X = space).
- `push`: the stub's hash is used directly as the tree entry hash — the object is
  not re-uploaded.
- `pull`: a stub path is skipped for download; only the stub record itself may be
  updated.

### How the invariant is maintained

**`push`**: The working tree scan reads stub records and contributes their recorded
hash to the tree. If a real file exists alongside a stub (partial expansion), the
real file takes priority and its content is hashed normally.

**`pull` (fast-forward)**: After applying remote changes to the working tree, all
stubs are reconciled against the new `clone_root` (stub reconcile). See below.

**`restore`**: Restoring a path that is currently a stub updates the stub record to
match `clone_root` rather than materialising the file.

### Stub reconcile (pull and restore)

After updating `clone_root` (pull) or during `restore`, every stub in the working
tree is checked against the new `clone_root`:

| `clone_root` entry at path | Stub kind | Action |
|----------------------------|-----------|--------|
| blob | file stub | Update stub fields (`hash`, `size`, `mtime`, `mode`) to match `clone_root` |
| tree | dir stub | Update stub fields to match `clone_root` |
| blob | dir stub | Remove `.omemfs-stub`, remove directory, write `<name>.omemfs-stub` |
| tree | file stub | Remove `<name>.omemfs-stub`, create `<name>/`, write `.omemfs-stub` inside |
| symlink | any stub | Remove stub, create symlink (symlinks are always materialised) |
| absent (deleted on remote) | any stub | Remove stub; remove directory if empty |

For partial-expansion directories (`.omemfs-stub` and real files coexist), the
reconcile updates only the `.omemfs-stub` file and leaves the real files untouched.
It does **not** recurse into the directory to restore or remove real files — the
real files are assumed to still match the user's intent (they were intentionally
expanded). Recursing into a fully-stubbed directory (no real files) is also
incorrect because blob objects are not guaranteed to be in the local cache for
stubbed paths; the reconcile simply updates the stub record.

---

## Commands

### `omemfs stub`

Convert materialised files or directories into stubs to free disk space.

**Preconditions** (checked before any changes are made):

1. Each target path must exist in `clone_root`. A path that has never been pushed
   cannot be stubbed — run `omemfs push` first.
2. The working tree content of each target must match `clone_root` (same hash
   **and** same mode, i.e. executable-bit). A differing executable bit counts as
   an unsaved local modification and blocks stubbing for that path, both for
   individual files and for directory contents. Unsaved local edits would be
   silently discarded if stubbing were allowed — run `omemfs push` first.

Both conditions ensure the object is present on the remote, so a later
`omemfs expand` can always succeed.

All preconditions are verified before any file is modified. If any check fails,
nothing is changed.

**Behaviour**:

1. Verify preconditions for all targets.
2. For each target:
   a. Read the `clone_root` entry to obtain `hash`, `size`, `mtime`, `mode`.
   b. Write the stub file (`<name>.omemfs-stub` or `<dir>/.omemfs-stub`).
   c. Delete the original file (or all files inside the directory, then the
      directory contents, leaving the stub in place).
3. `clone_root` is not modified. The next `push` will see the stub as unchanged.

### `omemfs expand`

Materialise one or more stubbed files or directories.

**Behaviour**:

1. Collect stub records for the specified paths (or all stubs if no path is given).
2. Apply the size filter: stubs at or above `--stub-threshold` are kept stubbed
   unless `-r` / `--recursive` is set.
3. For each stub to be expanded:
   a. Ensure the blob (or tree and its children) is present in the local object
      cache (`.omemfs/objects/`). If absent, download from the remote backend via
      the pack index (supports inline entries in delta/hot/cold index files,
      pack-file slices, and standalone objects).
   b. Write the file content and restore `mtime` and the executable-bit `mode`
      from the stub record.
   c. Delete the stub file **after** the content has been written successfully.
      If writing fails mid-way, the stub record is left in place so the expansion
      can be retried safely.

`clone_root` is not modified. A subsequent `push` will see the materialised file
as unchanged (hash matches `clone_root`).

---

## Interaction with `push`

During `push`, the working tree scan (`scan_and_store_with_cache`) handles stubs
transparently:

- **File stub** (`<name>.omemfs-stub` exists, `<name>` absent): the stub record's
  `hash` is used directly as the tree entry for `<name>`. The object is not
  re-read or re-uploaded.
- **Directory stub** (`.omemfs-stub` inside `<dir>`, no other real files): the
  stub record's `hash` (a tree object hash) is used as the `<dir>` tree entry.
- **Partial expansion** (`.omemfs-stub` and real files coexist in `<dir>`): the
  stub's tree object is read and merged with the real files. Real files take
  priority over same-name stub entries. The merged tree is stored as a new object
  and the stub record is updated to the new hash.
- **Real file present, stub also present**: the real file takes priority; the
  `.omemfs-stub` sidecar is treated as stale and ignored.

---

## Interaction with `pull`

Pull downloads only the objects that are needed for the working tree. Stubs are
excluded from the download set:

- Paths that are currently stubbed in the working tree: their objects are not
  downloaded. The stub record is updated via stub reconcile if the remote has
  changed the entry.
- New paths from the remote that meet the stub threshold: stub records are created;
  the objects are not downloaded.
- Paths materialised by pull (below the threshold): the file content is written
  and `mtime` and the executable-bit `mode` are restored from the remote tree
  entry.

This ensures that a large stubbed directory does not trigger a mass download on
every `pull`. Concretely, the bulk download walks the remote tree and skips any
blob leaf whose `size` is at or above the threshold, so its content never leaves
the remote.

A few paths still need a skipped blob's content, in which case it is fetched
**on demand** (a single-object download) rather than as part of the bulk set:

- A large file that lands inside a git worktree that is being materialised is
  materialised rather than stubbed (see the git-worktree rule — no partial stubbing
  inside a materialised worktree), so its blob is fetched when written.
- A conflicting path's remote content is needed to write the
  `.omemfs-conflict-remote` helper file.

---

## Interaction with `omemfs ls`

`omemfs ls` shows stub entries as if they were present. The status column `X`
compares the stub's recorded hash against `clone_root`:

- Stub hash matches `clone_root` → X = space (unchanged)
- Stub hash differs from `clone_root` → X = `M` (the stub file was hand-edited)

The stub/conflict column `Z`:

- `!`: unresolved conflict helper files exist for this path (or a descendant). Conflict takes precedence over stub state.
- `S`: the entry is directly stubbed (file or fully-stubbed directory — only `.omemfs-stub` inside, no real files).
- `s`: the directory has stub-related indirect state — partially expanded (`.omemfs-stub` coexists with real files), or a descendant is stubbed.
- ` ` (space): no conflict, fully materialised.

---

## Stubs and Git repositories

Stub files use the `.omemfs-stub` suffix or the `.omemfs-stub` directory entry
name. Inside a Git working tree, creating or renaming files to these names would
appear as untracked or modified changes to Git, leaving the repo in a half-stubbed,
half-real state that Git would report as a dirty tree.

The rule therefore forbids only **partial** stubbing inside a Git working tree that
is being **materialised**. Whole-directory stubbing — including a Git repository
**root** whose cumulative size is at or above the threshold — is always allowed:
the entire repo (its `.git` directory included) is replaced by a single directory
stub, and `omemfs expand` restores it intact. Because this is all-or-nothing, the
working tree is never left in a mixed state, and the repo simply ceases to be a Git
working tree on disk until expanded.

Concretely, when `omemfs clone` and `omemfs expand` walk a tree:

- An entry (file or directory) at or above the threshold is stubbed regardless of
  whether it is, or contains, a Git working tree. A Git repo root that meets the
  threshold is whole-stubbed as a directory stub.
- A directory below the threshold is **descended and materialised**. While
  descending, if the directory is (or is inside) a Git working tree, the
  "inside-a-git-worktree" flag is propagated to its children so that **no child is
  partial-stubbed**: every file under a materialised Git working tree is itself
  materialised, even if an individual file would otherwise meet the threshold.

This guarantees a materialised Git working tree is fully real on disk (never
partially stubbed), while still permitting a large Git repo to be stubbed as a unit.

`omemfs stub` (the explicit, user-invoked command) additionally refuses to place a
stub that would be visible to Git when stubbing inside — but not as the root of — a
Git working tree:

- Is inside a Git working tree (`<path>` is under a directory containing `.git`),
  unless the path is itself the Git working tree root (stubbing the entire repo as
  a unit is permitted), **and**
- The resulting `.omemfs-stub` file would be visible to Git (i.e. not matched by
  `.gitignore`).

**Visibility determination**: whether the resulting stub file would be visible to
Git is determined by invoking `git check-ignore` in the containing Git working tree.
If `git` is not installed or the check fails for any reason (non-zero exit, error
output), the path is treated as **visible** — i.e. stubbing is refused. This
fail-safe avoids silently polluting a Git working tree.

As a result, explicitly stubbing a path that is tracked by Git (via `omemfs stub`)
requires one of:
- Stubbing the entire Git repo root (so the `.git` directory is also removed and
  the repo ceases to be a Git working tree from that point).
- Adding the path to `.gitignore` before running `omemfs stub`.
