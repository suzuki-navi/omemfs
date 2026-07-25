# Filter Configuration (`.omemfs-filter`)

## Overview

`.omemfs-filter` is a single configuration file that controls two aspects of how omemfs handles the working tree:

- **ignore** — exclude paths from working tree scans. Excluded paths are never uploaded to the remote.
- **aggregate** — collapse directories into a single display entry in `omemfs ls`. Display-only; has no effect on sync behaviour.

The file uses `.gitignore`-compatible pattern syntax in two named sections: `[ignore]` and `[aggregate]`.

---

## File placement

- **Filename**: `.omemfs-filter`
- **Location**: any directory; patterns in each section apply only within the directory containing the file
- **Tracked**: the file itself is included in push/pull like any other file (it is synced to the remote)
- **Missing file**: no exclusion or aggregation; all paths are scanned and expanded normally

---

## File format

```
# Lines before any section header belong to the [ignore] section (default).

[ignore]
# Patterns here are excluded from working tree scans.
# Full .gitignore subset syntax, including ! for negation.
target/
node_modules/

[aggregate]
# Patterns here are collapsed to a single line in `omemfs ls`.
# Same syntax as [ignore].
.git
target/
node_modules/
```

### Section rules

- A line of the form `[ignore]` or `[aggregate]` (case-sensitive, no leading/trailing space) begins a new section.
- Lines before the first section header belong to `[ignore]` (the default section). This allows the file to look like a plain `.gitignore` when only ignore patterns are needed.
- A file may contain multiple `[ignore]` sections and multiple `[aggregate]` sections, in any order. Patterns from all sections of the same kind are concatenated (merged).
- A path that matches patterns in **both** sections gets both behaviours: excluded from push **and** collapsed in `ls`.

### Pattern syntax

Each section uses the same `.gitignore` subset:

| Pattern | Meaning |
|---------|---------|
| blank line | ignored |
| `# comment` | rest of line ignored |
| `/pattern` | matches `pattern` only directly inside the directory containing `.omemfs-filter` |
| `**/pattern` | matches `pattern` at any depth inside that directory |
| `pattern` | equivalent to `**/pattern` |
| `*` | any string not containing `/` |
| trailing `/` | optional; has no effect on matching |
| `!pattern` | negates a previously matched pattern (standard `.gitignore` negation) |
| `\#`, `\!` | literal `#` or `!` at line start |

Unsupported:

- `?` (single-character wildcard)
- `[abc]` (character class)

Lines using unsupported syntax are silently skipped (fail-safe).

---

## Hierarchical application

`.omemfs-filter` files can be placed in multiple directories within the working tree. Patterns in each file apply only to the subtree rooted at the directory containing that file.

The `/` anchor in each file is relative to the directory containing that file:

```
project/
├── .omemfs-filter      # [ignore] /target  → matches project/target only
└── sub/
    └── .omemfs-filter  # [ignore] /dist    → matches project/sub/dist only
```

**Ignore**: a path is excluded if it matches the `[ignore]` section of **any** `.omemfs-filter` in its ancestor chain (OR evaluation). Negation (`!`) applies only within the file where it appears; it does not override a match from a different ancestor file.

**Aggregate**: a directory is aggregated if it matches the `[aggregate]` section of **any** `.omemfs-filter` in its ancestor chain (OR evaluation).

When a directory is excluded by `[ignore]`, the scan does not descend into it, so `.omemfs-filter` files inside it are never read.

### Scope-limited filter load

Loading the filter set normally walks the whole working tree to discover every `.omemfs-filter` file. For a scoped command (`omemfs ls <path>` or scoped `push`), that whole-tree walk dominates the cost on a large repository even though only one subtree is listed.

`FilterSet::load_scoped(work_dir, scope_prefix)` loads only the files that can affect paths under `scope_prefix`:

- the `.omemfs-filter` in `work_dir` and in each ancestor directory along `scope_prefix` (these apply to the subtree by the ancestor-chain rule above), read by direct stat — no sibling directories are visited; and
- the `.omemfs-filter` files **inside** the scoped subtree, found by walking only that subtree (with the same `[ignore]`-pruning as the full walk).

This yields exactly the filter files whose subtree contains `scope_prefix` or is contained by it — the only files that can match an in-scope path — so `is_ignored` / `is_aggregated` decisions for in-scope paths are identical to a full load. An empty `scope_prefix` (root) is identical to the full `load`. The walk never descends into out-of-scope siblings such as other top-level project directories.

---

## Effect on commands

### `[ignore]` section

| Command | Behaviour |
|---------|-----------|
| `push` | Excluded paths are skipped during the working tree scan; they are never included in the new root tree |
| `ls` | Excluded paths are shown with `I` or `i` in the Z column; hash/size/mtime shown as `-` |
| `pull` | Remote entries are **still applied** even if they match an ignore pattern (the remote state takes precedence) |
| `restore` | Not affected; clone root content is restored regardless of ignore patterns |
| `cat` | Not affected |

**Z column values for ignored paths in `omemfs ls`**

| Z | Meaning |
|---|---------|
| `I` | ignored; path is not in clone root |
| `i` | ignored; path exists in clone root and will be removed from remote on next push |
| `S` | stubbed (existing meaning, unrelated to ignore) |
| `s` | partially stubbed (existing meaning) |
| ` ` | fully materialised |

When Z is `I`, the X column is space (never tracked, so no push change).  
When Z is `i`, the X column is `D` (will be deleted from remote on next push).

Ignored directories are shown as a single line even with `-r`; their contents are not expanded unless the path is given as an explicit argument.

### `[aggregate]` section

*Not yet implemented.* The `[aggregate]` section is defined in the spec but `omemfs ls` currently has no aggregation logic. The table below documents the intended final behaviour.

| Command | Behaviour |
|---------|-----------|
| `push` | Not affected; aggregated directories are scanned and uploaded normally |
| `pull` | Not affected |
| `restore` | Not affected |
| `cat` | Not affected |
| `ls` (non-recursive) | Aggregated directory D shown as a single entry |
| `ls -r` | D shown as a single entry; contents **not** expanded |
| `ls <path>` where `<path>` is D | Aggregation **lifted for D only**; contents expanded normally |
| `ls -r <path>` where `<path>` is D | Aggregation lifted; full recursive expansion |

Nested aggregated directories are collapsed into the nearest aggregated ancestor.

---

## Aggregate metadata computation

`ls` shows `size`, `blob_count`, and `mtime` for each directory entry.  These
values are stored inside the tree object for each `Tree` entry and are computed
during the working-tree scan.

### Computation during scan

When `scan_dir` finishes scanning a directory, it has already built the full
list of `TreeEntry` values for that level.  The aggregate metadata is computed
directly from those in-memory entries using `Tree::aggregate_*` — no read from
the object store is required.

`tree_meta(hash, store)` is a convenience wrapper for callers that only have a
hash (e.g. `splice_entry`, `remove_entry` in `tree_ops.rs`).  It is **not**
called inside `scan_dir`; doing so would read back the tree object that was
just written, incurring an unnecessary read + decrypt + decompress cycle per
directory.

### Implementation rule

> During a working-tree scan, compute aggregate metadata from the live
> `Vec<TreeEntry>` that is already in memory.  Never call `tree_meta` inside
> the scan loop.

## Default template on `omemfs clone`

`omemfs clone` creates a default `.omemfs-filter` at the repository root **only when no `.omemfs-filter` already exists there**. An existing file is never overwritten. After creation the file is a normal tracked file; the user may freely edit or delete it.

If the remote already contains `.omemfs-filter` (cloned from a repository that was previously initialised), the file is downloaded as part of normal clone expansion and no template is created.

```
# omemfs filter configuration
# Generated by `omemfs clone`. You may freely edit, remove, or add patterns.
# This file is tracked in omemfs and synced alongside other files.

[ignore]
# Paths matching these patterns are excluded from push/scan.
# Syntax is the same as .gitignore (including ! for negation).

# Microsoft Office lock files
~$*

# LibreOffice lock files
.~lock.*#

# macOS metadata
.DS_Store

# Windows metadata
Thumbs.db
desktop.ini

# Build artifacts
target/
node_modules/
__pycache__/

[aggregate]
# Directories matching these patterns are shown as a single entry in `omemfs ls`.
# They are still synced to the remote normally.

# Version control metadata
.git

# Build artifacts (large trees; shown collapsed even though not ignored)
target/
node_modules/
__pycache__/
```

Patterns intentionally omitted from the default (add manually if needed):

- Editor swap/lock files (`*.swp`, `.#*`) — editor-specific
- Generic temporary files (`*.bak`, `*.tmp`, `*.log`) — may conflict with user data
- Additional macOS metadata (`._*`, `.Spotlight-V100`) — borderline cases

---

## Examples

### Ignore build artifacts, aggregate version control metadata

```
[ignore]
target/
node_modules/

[aggregate]
.git
```

### Ignore and aggregate the same directories (most common)

Listing a pattern in both sections gives both effects — excluded from push **and** shown collapsed in `ls`:

```
[ignore]
target/
node_modules/
__pycache__/

[aggregate]
.git
target/
node_modules/
__pycache__/
```

### Plain `.gitignore`-style file (ignore only, no section header needed)

```
# No section header — these belong to [ignore] by default.
target/
node_modules/
*.pyc
```

### Negation within a section

Negation (`!`) works for file-level patterns. However, once a **directory** is excluded, the scan does not descend into it, so files inside cannot be re-included by a negation pattern:

```
[ignore]
logs/
!logs/keep-this.log   ← has no effect; logs/ is already excluded as a directory
```

To keep `logs/keep-this.log`, exclude the directory contents instead of the directory itself:

```
[ignore]
logs/*
!logs/keep-this.log   ← works: logs/ is not excluded, only its contents are
```

Note: re-inclusion of a file inside an excluded directory is not supported. Use wildcard patterns on directory contents (`dir/*`) rather than the directory name (`dir/`) when negation is needed.
