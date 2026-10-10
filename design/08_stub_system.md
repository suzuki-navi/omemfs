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

The stub's recorded tree object may not be in the local object cache: after a
lazy, stub-aware clone (see "Stub threshold" below) a stubbed directory's tree
object is never downloaded in the first place. Reading it to perform the merge
therefore goes through the same read-through, remote-fetching store `pull`
already uses for its own clone-root/remote-root reads (`LazyTreeStore` --
`03_sync_model.md`, "Lazy tree reads") rather than the raw local cache: a local
miss is fetched from the remote on demand and cached locally, so only
partial-expansion directories actually present in the scanned scope ever incur
a fetch. This remote-fetching store is used ONLY for that one read -- every
other local-presence decision the same scan makes (most importantly a
blob-writing scan's sealed-blob verification) stays strictly local-only; see
`03_sync_model.md`, "Path-scoped push", for why the two must never be the same
store value.

After push, the stub's hash field is updated to reflect the merged tree. `push`
does this by reusing `pull`'s own stub-reconcile step (`reconcile_stubs`/
`reconcile_leaf_stub`, described in "Stub reconcile (pull and restore)" below)
once the new `clone_root` has been durably written: a full push reconciles
every stub under the working tree, a scoped push reconciles only the stubs
under each pushed path. This is the same mechanism and the same "blob | file
stub" / "tree | dir stub" reconcile rules pull already applies -- push does not
duplicate the decision logic, only triggers it at a different point (after its
own `clone_root` advancement instead of pull's).

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
real file takes priority and its content is hashed normally. After `clone_root`
has been durably advanced to the new pushed root, every stub touched by the
push is reconciled against it (a full push reconciles the whole working tree;
a scoped push reconciles only the stubs under each pushed path) using the same
mechanism `pull` uses -- see "Stub reconcile" below.

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

#### Interaction with pull's ordinary diff/apply

`pull`'s remote-side diff (`clone_root` vs `remote_root`) is computed purely
from tree objects; it has no notion of which `clone_root` paths are currently
directory-stubbed in the working tree. If the content underneath a
directory-stubbed path changes on the remote, the diff still reports ordinary
per-entry `Modified`/`Added`/`Deleted` changes for the files inside it (the
diff never stops early there — only reconcile's own decision to skip a
dir-stubbed path, described above, is stub-aware). Applying those per-entry
changes literally would write real files underneath a directory that is
supposed to be a whole-unit stub, corrupting the partial/full-stub
distinction. `pull`'s apply step must therefore skip any diff entry whose
path is inside a locally directory-stubbed path (i.e. some proper ancestor
directory currently holds a `.omemfs-stub` marker) — the same way stub
reconcile itself only ever updates that one directory stub's record and
never descends into it. Reconcile (this section) is what brings the
directory stub's own record up to date afterward; the ordinary apply step
must not race it by writing individual files inside the stubbed subtree
first.

This also applies to a `Deleted` entry for a single file inside a
directory-stubbed path (the remote deleted just one of the stubbed
directory's files, not the whole subtree): a naive "the directory now
contains only its `.omemfs-stub` marker, so the whole stub must have been
deleted on the remote" cleanup cannot tell this case apart from a genuine
whole-subtree deletion, because the working tree looks identical either
way (only the marker is ever on disk for a fully-stubbed directory,
regardless of how much of the *logical* subtree the remote still has).
Whether the directory stub itself should be removed is therefore
decidable only by consulting the new `clone_root` — exactly reconcile's
`absent (deleted on remote) → remove stub` row above, keyed on whether
`clone_root` still has *any* entry at the stubbed path, not on the
apply step's per-file `Deleted` entries for paths underneath it. Apply
must not remove a directory stub marker itself as a side effect of
processing `Deleted` entries under it; only reconcile removes it, and
only when the whole path is gone from the new `clone_root`.

The same deferral applies when the directory-stubbed path **itself** is the
one whose entry changed kind (`tree → blob`/`tree → symlink` on the
remote) -- e.g. `Added(bigdir)` for a dir-stubbed `bigdir` whose remote
`clone_root` entry became a blob. Apply's ordinary `Added`/`Symlink`
handling assumes it is writing into an *absent or plain-file* path; it does
not (and, for a locally directory-stubbed path, must not) know how to
replace a whole directory-stub directory with new content, because doing
so correctly is exactly reconcile's `blob | dir stub` /
`symlink | any stub` table rows (`SwitchToFileStub`/`MaterialiseSymlink`)
above -- remove the marker, remove the directory, write the new stub or
symlink in its place. Apply must therefore skip a diff entry whose path
**is itself** a locally directory-stubbed path (not only entries under
one), the same way it already skips entries under one, and leave the
whole transition to reconcile. Concretely: attempting the naive
"write/stub the new content, then delete whatever was there" sequence
apply otherwise uses for a plain-file `Added` fails outright for a
directory-stubbed path either way round -- deleting a non-empty directory
via the same tolerant single-file remove apply uses for a stale plain file
raises `EISDIR`, and writing new content in its place before the directory
is gone raises `ENOTEMPTY`/`EISDIR` from the rename.

The mirror-image case must be deferred to reconcile the same way: a **file**
stub whose remote `clone_root` entry changed kind from blob to tree (`Added`
or `AddedTree` for a path that is currently a file stub -- e.g. `f.txt`
stubbed locally, replaced by a directory `f.txt/` on the remote). The diff
represents this as `Deleted(f.txt)` for the old blob plus `Added(f.txt/...)`
entries for the new tree's contents (or a single `AddedTree(f.txt)` if the
new tree meets the stub threshold). Apply's `Deleted` handling (the
delete-first phase) must not unconditionally remove the file stub marker
here: doing so before the type change is recognised loses the one signal
("this path was a stub") reconcile would otherwise use, and lets the
subsequent `Added` entries for `f.txt/...` materialise as ordinary real
files under `f.txt/` -- exactly the `tree | file stub` reconcile table row
(`SwitchToDirStub`: remove the file stub marker, create `f.txt/`, write a
directory stub inside), silently bypassed. Apply must therefore recognise
a `Deleted` entry for a path that is currently a file stub as potentially
the old side of such a transition, leave the stub marker in place instead of
removing it, and skip every entry (`Added`/`AddedTree`/`Symlink`) whose path
is at or under that same path in the SAME way the dir-stub-ancestor skip
above already does -- reconcile alone decides and applies the kind switch
once the new `clone_root` is current.

The table above covers every path that **is** a stub. It says nothing about a
third state: a path present in `clone_root` but currently neither a stub nor a
real file on disk (`ls`'s `D` status — see `04_cli_spec.md`). This state is
reached whenever a materialised file or a stub is deleted directly (`rm`)
instead of through `omemfs stub`/an `omemfs restore` that reconciles it, and it
is deliberately **not** in the reconcile table above: reconcile only ever
updates or removes an *existing* stub record, it never creates a new one for a
path that has none. A `D` path is `restore`'s job, not reconcile's — it is
exactly the "plan to restore a blob" case in `04_cli_spec.md`'s "omemfs
restore", and if the blob (or one of its chunks) named by `clone_root` is not
in the local cache, it is what `restore`'s fetch phase fetches (see "Missing
objects" in that same section). Before `restore` gained a fetch phase, this was
also the state that produced the `error: object not found: <hash>` failure
this document's stub reconcile table was never responsible for guarding
against, because the failing path was never a stub in the first place.

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
2. For each target, **write the stub record before deleting anything**:
   a. Read the `clone_root` entry to obtain `hash`, `size`, `mtime`, `mode`.
   b. Write the stub file first: `<name>.omemfs-stub` for a file target, or
      `.omemfs-stub` inside the directory for a directory target. This makes
      the stub record durable while the original content is still fully
      present on disk.
   c. Only then delete the original content: for a file target, delete
      `<name>` itself; for a directory target, delete every other entry
      already inside the directory (recursively), leaving only the
      just-written `.omemfs-stub` marker behind.

   This ordering is deliberate: if step (c) fails partway (a permission
   error, a symlink refused by `fsguard`, the process being killed), the
   directory is left with the `.omemfs-stub` marker coexisting with whatever
   real files step (c) had not yet removed. That is exactly the "Partial
   expansion" state described above — a state `push`'s scan/merge logic
   already understands and handles correctly — rather than a state where
   both the original content and the stub record are gone. Writing the stub
   record *after* deleting (the reverse order) would instead risk losing
   both the data and the stub record if the process died in between: the
   directory would be empty with no stub, silently discarding the target's
   logical content until a user manually intervenes.
3. `clone_root` is not modified. The next `push` will see the stub as unchanged.

Deletion in step (c) goes through the same `fsguard`-guarded boundary as any
other working-tree mutation (`design/03_sync_model.md`, "Write safety: the
working-tree write boundary") — including the directory-type check ahead of
it, so a directory target that has been swapped for a symlink pointing
outside `work_dir` is refused rather than having its "contents" deleted
through the symlink.

### `omemfs expand`

Materialise one or more stubbed files or directories.

**Behaviour**:

0. If a requested `<path>` does not match any on-disk stub record, but a proper
   ancestor directory does have an on-disk directory stub, lower the stub
   frontier one tree level at a time until the requested path itself is an
   on-disk stub record (or is found to already be materialised, or an error is
   raised). See "Lowering the stub frontier past a `.git` entry" below and
   `design/04_cli_spec.md`, "Paths below the stub frontier", for the full
   algorithm.
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

### Malformed stub records are fail-closed

A stub marker's content is read and JSON-parsed while scanning (both the file
stub and the directory stub case above). If that read or parse fails — the
marker file is truncated, holds invalid JSON, or otherwise fails to decode as
a `StubRecord` — the scan aborts with a hard error instead of silently
proceeding.

This matters because the real file/directory the stub replaces is, by
definition, absent from the working tree (that is what "stubbed" means): if
an unreadable or corrupt stub marker were instead treated as "no stub here,"
the scan would see nothing at all at that path and record it as **absent**,
which `push` then faithfully uploads as a deletion — silently destroying the
remote's copy of content that in truth still exists (only its local marker
is damaged). Failing the whole scan closed, rather than treating the
corrupted marker as an ordinary miss, is the only response that cannot lose
data: it forces the operator to notice and repair the marker (or restore it,
e.g. from `omemfs pull`/a backup) before any push can proceed.

This applies uniformly to both call sites in `src/scan.rs`: the file-stub
branch in `scan_dir` and the directory-stub branch in `classify_dir_stub`
(used both for a directory encountered as a child, and for a scan rooted
directly on a stubbed directory). See `tests/stub.bats` for the regression
covering a corrupted stub marker.

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

### Path-scoped pull's local-change check must resolve a stubbed leaf

Path-scoped `pull <path>`'s local-change check (design/03_sync_model.md,
"Path-scoped pull" step 3) computes the working tree's `<path>` hash to diff
against `clone_root`. When `<path>` is a leaf scope (a file or symlink) that
is currently a **file stub** (`<path>.omemfs-stub` present, the real file
absent), this must resolve to the stub record's own `hash` — exactly the way
`push`'s scan (`scan_and_store_with_cache`, "Interaction with `push`" above)
treats a file stub transparently as its recorded content, never as an
absence. Treating the stub the same way `push_scoped`'s
`resolve_scoped_stub` already does for the scoped-push case (splice the
stub's recorded `TreeEntry` in directly) is the correct fix, applied on the
pull side to the working-tree side of the local-change check: a stubbed leaf
is "content exists, not materialised," so an on-disk-absence check alone
(without also consulting the stub) would misreport it as a local deletion of
`<path>`, producing a spurious conflict against any remote change to
`<path>` that the local side never actually made (contrast with a genuine
local deletion, which correctly IS a local change and must still be
detected as one — "Whole-root absence" in design/03).

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

**Lowering the stub frontier past a `.git` entry.** When `omemfs expand
<path>` targets a path nested inside an unexpanded directory stub, the
frontier is lowered one level at a time (see `design/04_cli_spec.md`, "Paths
below the stub frontier"), and a level may contain a `.git` Tree entry as an
ordinary sibling of the path being resolved. Lowering such a level is
permitted: `.git` becomes a single atomic directory stub, never split and
never partially expanded — the same all-or-nothing treatment already allowed
above for a whole Git repo root, applied to `.git` as a sibling encountered
mid-lowering.

Two constraints apply:

- Within one level's lowering, the `.git` child (if present) is written
  **last**, after every other sibling's stub-or-materialise decision has
  already been made and written. `stub_would_be_visible_to_git`'s enclosing-
  worktree lookup detects a Git working tree by the physical existence of a
  `.git` entry alone, and writing a directory stub for `.git` makes it
  physically exist (as a directory containing only `.omemfs-stub`, with no
  `HEAD`). Writing `.git` first would make every remaining sibling's
  Git-visibility check invoke `git check-ignore` inside that non-functional
  repository, fail (exit 128), and hit the fail-safe "visible" answer —
  silently forcing every remaining sibling to be fully materialised instead of
  stubbed. Writing `.git` last avoids this deterministically.
- Resolving a requested path that lies on or below a `.git` entry (the target
  requires descending into `.git`, or the target *is* `.git`) is refused, even
  though `.git` itself would just become an atomic stub like any other
  sibling. The reason is what `expand` does next: the whole point of the
  command is to materialise the resolved target, and materialising `.git`
  while its sibling project files remain individual stub markers is exactly
  the half-stubbed, half-real Git working tree state this section forbids —
  once `.git` is real and functional, `git status` in that directory would
  really report the sibling `.omemfs-stub` files as untracked or dirty. The
  error directs the user to expand the enclosing directory as a whole instead,
  which correctly expands `.git` together with every sibling in one atomic
  step.

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
