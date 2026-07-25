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

