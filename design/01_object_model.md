# Object Model

## Principles

All data is stored as an **object**. Objects are **immutable** and identified by their SHA-256 hash.

The hash is computed from the serialised bytes, which always include a 2-byte type tag prefix:

```
hash(blob) = SHA256(0xED 0xF0 | file content bytes)
hash(tree) = SHA256(0xED 0xF1 | tree JSON bytes)
```

Identical content always produces an identical hash, giving automatic deduplication. Because each object type uses a distinct prefix, a blob and a tree with identical payload bytes will always have different hashes.

## Object types

Four types are defined:

```
blob      file content (binary or text)
tree      directory structure
manifest  chunk list for a large object (produced by the chunk stage)
chunk     a fixed segment of a large serialised object
```

The `kind` of a child object is recorded by its parent tree entry — not embedded in the object itself.

`manifest` and `chunk` objects are produced transparently by the codec pipeline's chunk stage when a serialised object exceeds the split threshold. They are an implementation detail of the storage layer — the logical object model (blob / tree) is unchanged.

The **clone root** and **remote root** are not objects — they are tree hashes stored as plain text files (clone root in `.omemfs/clone_root`, remote root in the remote backend's designated entry). See the sync model documentation for details.

---

## blob

A blob object holds the raw bytes of a file — no header, no encoding.

```
<raw file bytes>
```

### Example

A file containing `Hello, world!` produces a blob whose hash is `SHA256(0xED 0xF0 | b"Hello, world!")`.

Binary files are stored as-is (no Base64 encoding).

---

## tree

A tree object represents a directory. Its content is minimised JSON (no whitespace, no indentation), with entries sorted alphabetically by `name`.

```json
{"kind":"normal","entries":[{"kind":"blob","name":"file.txt","hash":"a1b2...","mtime":"2026-03-07T10:30:45.000000000Z","size":1234},{"kind":"tree","name":"subdir","hash":"b2c3...","mtime":"2026-03-07T09:00:00.000000000Z","size":567890,"blob_count":12},{"kind":"symlink","name":"link","target":"../other","mtime":"2026-03-07T08:00:00.000000000Z"}]}
```

### Entry kinds

#### blob (regular file)

```json
{
  "kind": "blob",
  "name": "file.txt",
  "hash": "a1b2c3...",
  "mtime": "2026-03-07T10:30:45.000000000Z",
  "size": 1234
}
```

- `hash`: points to the blob object
- `size`: file size in bytes
- `mtime`: last-modified time (ISO 8601 / UTC, nanosecond precision, e.g. `2026-06-12T09:49:09.901738969Z`)
- `mode`: omitted when the file is not executable; set to `"755"` when the owner execute bit is set

Executable file example:

```json
{
  "kind": "blob",
  "name": "run.sh",
  "hash": "f0e1d2...",
  "mtime": "2026-03-07T10:30:45.000000000Z",
  "size": 128,
  "mode": "755"
}
```

`mode` rules:
- Recorded only for blob entries (not for tree or symlink entries).
- Set to `"755"` when `mode & 0o100 != 0`; otherwise the field is absent.
- Directories are always restored with `0o755`; omemfs does not record directory modes.
- On filesystems that do not support Unix permissions (e.g. DrvFs on WSL), `chmod` may return `EPERM` during restore — this is non-fatal.

#### tree (subdirectory)

```json
{
  "kind": "tree",
  "name": "subdir",
  "hash": "b2c3d4...",
  "mtime": "2026-03-07T09:00:00.000000000Z",
  "size": 567890,
  "blob_count": 12
}
```

- `hash`: points to the child tree object
- `mtime`: maximum mtime among all descendant entries (computed deterministically; `null` if the directory is empty)
- `size`: total size of all descendant blob entries in bytes
- `blob_count`: total number of blob and symlink leaf entries reachable from this tree (used for the count column in `omemfs ls`). Always emitted, including when `0`; readers must default missing values to `0` for robustness.

#### symlink

```json
{
  "kind": "symlink",
  "name": "link",
  "target": "../other",
  "mtime": "2026-03-07T08:00:00.000000000Z"
}
```

- `target`: the link destination path (stored as-is)
- `mtime`: last-modified time of the symlink itself (ISO 8601 / UTC, nanosecond precision). Read from `lstat` (the link, not its target) and restored with `lutimes` on materialise so the link's mtime survives a pull/expand round-trip.
- No `hash` field; symlinks do not create a separate object
- No `size` field; symlinks have no byte size in the object model

### Invariants

1. **Minimised JSON**: no whitespace, no indentation.
2. **Entries sorted by name**: alphabetical order, ensuring identical directory content always hashes to the same value.
3. **mtime propagation**: a tree's `mtime` is the maximum mtime of its entries, excluding `null` values. An empty tree has `mtime: null`.
4. **size propagation**: a tree's `size` is the sum of its entries' sizes. An empty tree has `size: 0`.
5. **Entry name validation**: `name` must be non-empty, must not be exactly `.` or `..`, must not contain `/` or a NUL byte, and must not fall in the reserved `.omemfs` / `.omemfs-*` namespace (see below). `Tree::deserialise` (`src/object.rs`) enforces this on every entry of every tree it parses and returns `Error::InvalidObject` otherwise, rather than silently accepting a name that could act as a path-traversal vector once a caller joins it onto a base directory (e.g. `base_dir.join(entry.name())`). Because deserialisation is the one point every tree-reading command goes through — `pull`, `expand`, `clone`, `restore`, `ls`, `stats`, `pack`, `cat`, and the shared helpers in `tree_ops.rs` all call `Tree::deserialise` rather than parsing tree JSON themselves — this single check protects all of them without any command-specific validation. This project targets Linux (see `CLAUDE.md`); the rule intentionally covers only the universally unsafe characters and does not attempt to enforce Windows' broader reserved-character or reserved-name set.

   **Reserved-name rejection**: an entry named exactly `.omemfs`, or matching
   the reserved `.omemfs-` namespace (containing the substring `.omemfs-`,
   in either the standalone/dir-interior form `.omemfs-<kind>` or the
   file-adjacent form `<name>.omemfs-<kind>`), is rejected the same way as
   the other invalid-name cases above — **except** for the exact standalone
   name `.omemfs-filter`, which is accepted. `.omemfs-filter` is a tracked
   configuration file (design/09_reserved_names.md, "`.omemfs-filter` —
   filter configuration"): `scan_and_store` includes it in every tree it
   builds, so a well-formed tree routinely contains a `.omemfs-filter` entry,
   unlike `.omemfs-stub`/`.omemfs-conflict-*`, which `scan_and_store` always
   excludes and which therefore never legitimately appear in a tree. The
   general rejection protects `pull`/`clone`/`restore`/`expand` from a remote
   tree that names an entry `.omemfs` (which would overwrite the
   repository's own metadata directory once materialised) or names an entry
   as a fake stub marker / conflict helper (which would be misinterpreted by
   a later local scan); `.omemfs-filter` carries neither risk, since its
   *contents* are already ordinary tracked (and therefore
   remote-controlled) data. See `design/09_reserved_names.md`, "Reserved
   names in remote tree entries (untrusted input)" and "The `.omemfs-filter`
   exception", for the full rationale and how this differs from the local
   working-tree scan's own (separate) handling of the same name patterns.

   `\` (backslash) is deliberately **allowed** and is not a path separator on Linux — it is an ordinary byte that can legally appear anywhere in a filename (e.g. `a\bslash.txt`, `we\ird\name`). It is stored and restored verbatim, exactly like any other non-separator byte. Correspondingly, **no path-string normalisation anywhere in the codebase may rewrite `\` into `/`**: doing so would silently turn a single legitimate entry name into a multi-component path and write/read the wrong location. Earlier revisions of several path-handling helpers called `.replace('\\', "/")` on relative-path strings as an apparent (and, on this Linux-only target, dead) attempt at Windows separator normalisation; those calls have been removed for this reason.

   Known limitation: `Tree::deserialise` is also used in a few places (`src/commands/cat.rs`, `src/commands/stats.rs`, `src/commands/pull.rs`, `src/commands/transfer.rs`) purely as an "is this object a tree?" discriminator — a failed `deserialise` is treated as "not a tree, therefore a blob" rather than "a malformed tree." Now that `\` is no longer rejected, this ambiguity is reachable only for genuinely malformed tree objects, not for ordinary filenames. A future improvement would be a dedicated `Tree::is_tree_bytes()` check (testing for the `ED F1` tag alone, without full JSON validation) so the two cases — "not a tree" vs. "a corrupt tree" — can be told apart; this is not implemented yet.

### Empty directories

An empty directory is represented as a tree with an empty entries array:

```json
{"kind":"normal","entries":[]}
```

Its `mtime` is `null` and its `size` is `0`.

### mtime stability on push

During `push`, if a file's content hash matches the same path in the current clone root tree, the clone root's mtime is reused instead of the filesystem mtime. This prevents spurious tree hash changes caused by tools like `rsync -a` or `cp -p` that preserve timestamps.

---

## manifest and chunk

`manifest` and `chunk` objects are implementation details of the codec pipeline's L3 (chk) layer. Their byte formats, hash rules, and FastCDC parameters are defined in [Storage Format — L3 (chk): chunk / assemble](02_storage_format.md#l3-chk-chunk--assemble).

