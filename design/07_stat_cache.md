# STAT_CACHE

## Purpose

`STAT_CACHE` is a persistent local cache that maps working-tree file paths to their SHA-256 content hashes, keyed by filesystem `(mtime, size)`. When a file's stat fields match a cache entry, its content hash can be reused without reading or hashing the file.

This optimisation matters for commands that scan the entire working tree (`omemfs ls`, `omemfs push`) on large repositories where most files are unchanged since the last run.

`STAT_CACHE` is a **pure acceleration cache**: it is never authoritative. Any corruption, truncation, or missing entry causes the affected file to be re-hashed normally — correctness is never compromised.

---

## File location

```
<working tree>/.omemfs/STAT_CACHE
```

---

## Entry structure

Each entry records the filesystem stat values that were observed when the hash was computed, together with the resulting hash and an `unsafe` flag for racily clean detection.

| Field         | Type   | Description |
|---------------|--------|-------------|
| `mtime_secs`  | `i64`  | Seconds since Unix epoch (may be negative for pre-epoch times) |
| `mtime_nanos` | `u32`  | Subsecond nanoseconds (0–999 999 999) |
| `fs_size`     | `u64`  | File size in bytes |
| `hash`        | `[u8; 32]` | SHA-256 content hash of the file |
| `is_unsafe`   | `bool` | Racily clean flag — see [Racily clean detection](#racily-clean-detection) |

---

## Binary file format (v1)

All multi-byte integers are **big-endian**.

```
HEADER     32 bytes
INDEX      entry_count × 8 bytes   (sorted by path bytes, ascending)
PATHS      concatenated path bytes + padding to 8-byte boundary
DATA       entry_count × 56 bytes
TRAILER    32 bytes                 (reserved, zero-filled)
```

### HEADER (32 bytes)

| Offset | Size | Field          | Value |
|--------|------|----------------|-------|
| 0      | 4    | magic          | `OSTC` (0x4F 0x53 0x54 0x43) |
| 4      | 4    | version        | `1` (u32) |
| 8      | 4    | entry_count    | number of entries (u32) |
| 12     | 4    | header_flags   | reserved, `0` |
| 16     | 4    | index_offset   | always `32` (u32) |
| 20     | 4    | paths_offset   | byte offset of PATHS section (u32) |
| 24     | 4    | data_offset    | byte offset of DATA section (u32) |
| 28     | 4    | reserved       | `0` |

### INDEX (entry_count × 8 bytes)

Sorted ascending by raw path bytes. Each entry:

| Offset | Size | Field       |
|--------|------|-------------|
| 0      | 4    | path_offset — byte offset within the PATHS section (u32) |
| 4      | 4    | path_len    — byte length of the path (u32) |

The sort order enables binary search during scope-limited reads.

### PATHS

The raw UTF-8 path bytes of all entries, concatenated in the same order as INDEX. Followed by 0–7 zero-padding bytes to align the DATA section to an 8-byte boundary.

Paths are repo-relative forward-slash separated strings (no leading slash, no trailing slash). Example: `src/main.rs`.

### DATA (entry_count × 56 bytes)

One record per entry, in the same order as INDEX:

| Offset | Size | Field         | Type |
|--------|------|---------------|------|
| 0      | 8    | mtime_secs    | i64  |
| 8      | 4    | mtime_nanos   | u32  |
| 12     | 8    | fs_size       | u64  |
| 20     | 32   | hash          | [u8; 32] |
| 52     | 4    | flags         | u32 |

`flags` bit assignments:

| Bit  | Name      | Meaning |
|------|-----------|---------|
| 0    | `UNSAFE`  | Entry is racily clean; must not be used as a cache hit |
| 1–31 | reserved  | Must be zero on write; ignored on read |

### TRAILER (32 bytes)

Reserved for future use. Written as zero bytes. Readers must not validate this field.

---

## Lookup

`lookup_current(path, mtime, fs_size) -> Option<Hash>`:

Returns the cached hash if all of the following conditions hold:

1. An entry for `path` exists in the cache.
2. `entry.is_unsafe` is `false`.
3. `entry.fs_size == fs_size`.
4. `entry.mtime_secs == mtime_secs` and `entry.mtime_nanos == mtime_nanos`.

On a hit, the stored hash is returned directly — the file is not read. The caller is responsible for comparing the returned hash against the clone root to determine whether the file is modified.

On a miss (any condition fails), the caller must hash the file normally.

---

## Racily clean detection

The "racily clean" problem arises when a file is written and then immediately scanned within the same clock second. The cached `mtime_secs` matches the file's current `mtime_secs`, so a later write within the same second — which would update the file content — would not change `mtime_secs` and would produce a false cache hit.

**Detection rule**: when inserting an entry, if `now - mtime < 3s`
(i.e. `mtime_secs >= floor(now_as_unix_secs) - 2`), set `is_unsafe = true`.
This 3-second window (`RACY_THRESHOLD_SECS`, defined once in `src/stat_cache.rs`)
is the same constant used by the clone-root fallback in `scan_and_store`
(see [Usage in scan_and_store](#usage-in-scan_and_store)).
The entry is stored (so the hash is available for reference) but
`lookup_current` will never return it as a hit.

On the next scan — after at least 3 seconds have elapsed since the file's
mtime — the file is re-hashed. If the hash is stable at that point, the entry
is overwritten with `is_unsafe = false` and becomes eligible for future cache
hits.

```
is_racily_clean(mtime, now) = (now - mtime < 3s)
```

---

## Update

`update(path, mtime, fs_size, hash)`:

Inserts or replaces the entry for `path`. Computes `is_unsafe` from the current clock at insertion time.

The entry is written even when `is_unsafe = true` so that the hash value is available for future reference (e.g., for debugging or for updating to a safe entry after the racy window passes).

---

## Usage in `scan_and_store`

During a working-tree scan:

1. For each regular file encountered:
   a. Call `fs::metadata` to obtain `(mtime, size)`.
   b. Call `stat_cache.lookup_current(rel_path, mtime, size)`.
   c. **Cache hit**: use the returned hash directly; do not read the file.
   d. **Cache miss**: read and hash the file normally.

2. After the scan completes:
   - For each file that was **re-hashed** (cache miss): call `stat_cache.update(path, mtime, size, hash)`.
   - Write the updated cache back to disk atomically.

Cache hits do not trigger a writeback unless something changed. The cache is always written atomically via a temp file + rename.

### Blob-write mode and the STAT_CACHE invariant

`scan_and_store` runs in one of two blob-write modes (see design/03 "Scan
blob-write mode"):

- `write_blobs = true` (`push`, `stub`): a cache-miss file is hashed **and** its
  blob object is written to `.omemfs/objects/`.
- `write_blobs = false` (`ls`, `pull`): a cache-miss file is hashed but its blob
  object is **not** written.

In **both** modes the file's `(mtime, size, hash)` is recorded in the STAT_CACHE
on a cache miss (and tree objects are always written). This means a STAT_CACHE
entry **no longer implies that the corresponding blob exists in the local object
store**. The same divergence could already occur if a blob were deleted after a
`push` populated the cache; `write_blobs = false` simply makes it routine.

Consumers must therefore not assume "STAT_CACHE hit ⇒ blob present". A push
scan checks the local object before accepting a cache or clone-root metadata
hit. If it is missing, push captures and stages the blob during the scan, before
the new root is sealed. Upload reads only this sealed local object graph and
never regenerates from the live working tree. The "Durability ordering"
guarantee below still holds for the objects written in a given scan.

### Clone-root fallback

When `lookup_current` misses (no STAT_CACHE entry for a path), `scan_and_store`
falls back to comparing the file's `(mtime, size)` against the corresponding
entry in the clone root tree.  The fallback is only accepted when the file's
mtime is outside the 3-second racy window (`age >= RACY_THRESHOLD_SECS`):

```
can_skip_hash(clone_root_entry, fs_size, fs_mtime, now):
  1. Entry must be a Blob entry with matching size.
  2. fs_mtime must equal the clone-root entry's mtime.
  3. (now - fs_mtime) >= 3s   — racy window check.
```

When the fallback accepts a file, its `(mtime, size, hash)` triple is inserted
into STAT_CACHE so subsequent scans hit the cache directly, avoiding the
clone-root lookup.

The `stub` command also uses `lookup_current` to verify a file against
`clone_root` before replacing it with a stub record (see `src/commands/stub.rs`
`lookup_current` call).  On a STAT_CACHE miss the file is read and hashed
directly.

### Durability ordering

`STAT_CACHE` entries reference object hashes that are stored in `.omemfs/objects/`.
Local cache objects are written with `atomic_write_no_fsync` (no per-object fsync).
A durability barrier (`sync_local_objects_fs`) is therefore issued **inside
`StatCache::write`**, immediately before the atomic rename, to ensure that every
object referenced by the new cache entries has reached durable storage before the
cache pointer is persisted.

Without this ordering a power failure between the cache write and a subsequent
`syncfs` could produce a STAT_CACHE that references object files that were lost,
causing `scan` to return stale hashes without detecting the loss.

---

## Read optimisation: scope-limited load

When a scan covers only a subdirectory (e.g. `omemfs push src/` or `omemfs ls src/`), only in-scope entries need to be loaded. `StatCache::read_scoped(omemfs_dir, scope_prefix)` parses the header and INDEX, then searches the sorted INDEX with a lower-bound (binary search) algorithm to find the contiguous range of entries whose path equals the scope prefix or starts with `scope_prefix + "/"`, and decodes only that slice into entries.

Because the INDEX is sorted by raw path bytes, the in-scope entries form a single contiguous run. The range is `[lower_bound(prefix), lower_bound(prefix_after))`, where `prefix_after` is the prefix with its final byte incremented — this bounds the run on the right without catching siblings such as `foo.txt` for scope `foo` (only `foo` itself and `foo/...` descendants are included, never `foo.txt` or `foobar`). A scope equal to the repository root (empty prefix) delegates to the full `read`.

Scope-limited reads skip the full-file decode and load only the relevant slice, which is important for large caches on repositories with many files.

After a scope-limited scan, the cache is reloaded in full before writeback so that the new in-scope entries are merged with the out-of-scope entries; out-of-scope entries survive byte-for-byte. The dirty-flag optimisation is preserved: a scoped scan that changes nothing in scope leaves the merged cache clean and skips the write entirely. Removals are not currently produced by the scoped scan paths, so the merge only overlays insertions/updates.

For a multi-path scoped command (multiple scopes given at once, for either `push` or `ls`), scope-limited loading is **not** used: the command falls back to a single full `read`. This keeps the union-of-scopes logic simple and correct (a full read already covers every scope). A single-path scoped `push` or `ls` uses `read_scoped` with that path's prefix, and writes back via `write_scoped_merge` with the same prefix. An unscoped command (no `<path>`) uses the full `read`/`write_if_dirty` path.

The sorted INDEX section is always written to enable scope-limited reads.

---

## Corruption handling

If the file is absent, truncated, has an unrecognised magic, or has a version mismatch:

- Return an empty cache. The scan proceeds normally and rebuilds the cache.

Per-entry structural corruption (e.g. `path_offset + path_len > paths_section_size`)
causes the affected entry to be silently dropped; the remainder of the cache is
still parsed and used.

A non-UTF-8 path byte sequence causes `parse_v1` to return `None` immediately,
treating the entire cache as empty.  This is safe but coarse: all files will be
re-hashed on the next scan and the cache will be rebuilt.

`STAT_CACHE` is never read by the remote backend and is never pushed or pulled. It is a local-only file.
