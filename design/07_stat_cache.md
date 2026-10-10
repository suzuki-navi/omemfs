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

Each entry records the filesystem stat values that were observed when the hash was computed, together with the resulting hash, an `unsafe` flag for racily clean detection, and a `sealed` flag that lets a blob-writing scan skip a redundant local-existence check.

| Field         | Type   | Description |
|---------------|--------|-------------|
| `mtime_secs`  | `i64`  | Seconds since Unix epoch (may be negative for pre-epoch times) |
| `mtime_nanos` | `u32`  | Subsecond nanoseconds (0–999 999 999) |
| `fs_size`     | `u64`  | File size in bytes |
| `hash`        | `[u8; 32]` | SHA-256 content hash of the file |
| `is_unsafe`   | `bool` | Racily clean flag — see [Racily clean detection](#racily-clean-detection) |
| `sealed`      | `bool` | Set once a blob-writing scan has written or verified the local object for `hash` — see [Seal invalidation](#seal-invalidation) |

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
| 1    | `SEALED`  | A blob-writing scan has written or verified the local object for `hash`; a later blob-writing scan may skip its own local-existence check for this entry when [seal trust](#seal-invalidation) holds — see [Seal invalidation](#seal-invalidation) |
| 2–31 | reserved  | Must be zero on write; ignored on read |

`SEALED` and `UNSAFE` are independent bits computed from unrelated inputs
(`SEALED` from a local-existence check result; `UNSAFE` from the write-time
clock). `lookup_current`/`lookup_current_entry` never return an entry with
`UNSAFE` set, so the `SEALED` bit of an unsafe entry is never read — the two
flags cannot interact.

### TRAILER (32 bytes)

| Offset | Size | Field        | Description |
|--------|------|--------------|--------------|
| 0      | 16   | `seal_epoch` | Opaque epoch ID, copied from `.omemfs/objects/.seal_epoch` at the time this cache was last written by a blob-writing scan. All-zero means "no epoch recorded" — an older cache written before this field existed, or a cache last written by a read-only scan (`write_blobs = false`), which never sets it. See [Seal invalidation](#seal-invalidation). |
| 16     | 16   | reserved     | Must be zero on write; ignored on read. |

Introducing `seal_epoch` does not require a version bump: the field lives in
bytes that were always written as zero and never validated by readers (see
"Corruption handling" below), so a v1 cache produced before this field existed
already has `seal_epoch = 0`, which correctly decodes as "no epoch recorded".

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

`update(path, mtime, fs_size, hash, sealed)`:

Inserts or replaces the entry for `path`. Computes `is_unsafe` from the current clock at insertion time. `sealed` is set directly from the caller (see [Seal invalidation](#seal-invalidation) for who calls this with `sealed = true`).

The entry is written even when `is_unsafe = true` so that the hash value is available for future reference (e.g., for debugging or for updating to a safe entry after the racy window passes).

`mark_sealed(path)`:

Sets the `SEALED` bit on an existing entry, leaving `mtime_secs`, `mtime_nanos`, `fs_size`, `hash`, and `is_unsafe` untouched. A cache hit has already proven those fields match the current file, so `mark_sealed` never needs to recompute them. Returns `true` (and marks the cache dirty) only when the bit was not already set; a no-op on an already-sealed entry or a missing path returns `false` and leaves the cache clean. This asymmetry — `update` always writes a fresh entry, `mark_sealed` only ever flips one bit and only when it changes something — is what keeps "no writeback on an unchanged scan" true for a fully-sealed cache (see [Usage in scan_and_store](#usage-in-scan_and_store)).

---

## Usage in `scan_and_store`

During a working-tree scan:

1. For each regular file encountered:
   a. Call `fs::metadata` to obtain `(mtime, size)`.
   b. Call `stat_cache.lookup_current(rel_path, mtime, size)`.
   c. **Cache hit**: use the returned hash directly; do not read the file.
   d. **Cache miss**: read and hash the file normally.

2. After the scan completes:
   - For each file that was **re-hashed** (cache miss): call `stat_cache.update(path, mtime, size, hash, sealed)`, where `sealed = write_blobs` (a blob-writing scan just wrote or verified the object it references; a read-only scan has not).
   - For each file that was a **cache hit** and whose local blob was written or verified during this scan (see [Seal invalidation](#seal-invalidation)): call `stat_cache.mark_sealed(path)`.
   - Write the updated cache back to disk atomically.

Cache hits do not trigger a writeback unless something changed — including a `mark_sealed` call that was a no-op (already-sealed entry). The cache is always written atomically via a temp file + rename.

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

Consumers must therefore not assume "STAT_CACHE hit ⇒ blob present" **unless the
entry is `sealed` and seal trust holds** (see [Seal invalidation](#seal-invalidation)
below). Absent that, a push scan checks the local object before accepting a
cache or clone-root metadata hit. If it is missing, push captures and stages the
blob during the scan, before the new root is sealed. Upload reads only this
sealed local object graph and never regenerates from the live working tree. The
"Durability ordering" guarantee below still holds for the objects written in a
given scan.

`process_file_job`'s exact skip condition on a STAT_CACHE hit is:

```
write_blobs == false                                   -- read-only scan never needed the blob
  OR (entry.sealed AND seals_trusted)                   -- verified-present, and the verification is still trustworthy
  OR store.exists(entry.hash)                           -- fall back to the check this replaces
```

Only the middle branch is new. It never changes what a scan accepts — it only
skips a `store.exists()` call whose answer is already known to be `true`. The
clone-root fallback branch (a STAT_CACHE *miss*) always calls `store.exists()`
unchanged: there is no seal bit to consult for an entry that was never cached.

### Seal invalidation

`.omemfs/objects/` is documented (design/02, "objects/ directory") as a **local
cache**: "any object present in the remote backend can be safely deleted
locally and re-fetched on demand." A `sealed` entry's promise — "this blob has
already been written to or verified in the local object store" — is only true
until someone acts on that documented permission and deletes it. Seal
invalidation is the mechanism that keeps that promise honest across three
independent failure shapes, from cheapest/broadest to narrowest.

#### Seal epoch (whole-directory loss)

`.omemfs/objects/.seal_epoch` holds a 16-byte random ID, created the first time
any blob-writing scan needs it. Every blob-writing scan compares this marker's
current ID against the `seal_epoch` recorded in the TRAILER of the STAT_CACHE it
loaded:

```
seals_trusted = (cache.seal_epoch != 0) AND (cache.seal_epoch == marker.seal_epoch)
```

- On a match, `seals_trusted = true` for the whole scan: every `sealed` entry's
  local-existence check may be skipped.
- On a mismatch (including "no marker file" — a fresh or manually emptied
  `objects/` directory — or "no epoch recorded" — an older cache): the scan
  proceeds exactly as it does today (every entry re-verified via
  `store.exists()`), and at the end of the scan a **new** marker ID is generated
  and the freshly-written cache records that new ID as its `seal_epoch`.

Deleting `.omemfs/objects/` wholesale (`rm -rf`, a restored snapshot, a lost
disk) deletes the marker along with every object it referenced, so the very
next blob-writing scan observes a mismatch and re-verifies everything — the
STAT_CACHE never needs to be touched by hand.

A read-only scan (`write_blobs = false`) does not create or overwrite the
marker (it writes no blobs, so it has nothing to seal), but it **must** carry
the `seal_epoch` it loaded forward into the cache it writes back — dropping it
to zero on every `ls`/`pull` would invalidate every seal each time a read-only
command runs. See "Implementation notes" below for where this applies.

`.seal_epoch` is a dotfile directly inside `.omemfs/objects/`, at the same level
as the existing `.depth` and `.migrating` markers; every existing directory
walk in that layer already skips dotfiles, so no enumeration path needs to
change to accommodate it.

#### Sampling verification (partial loss)

The seal epoch cannot detect a *partial* loss — e.g. one shard subdirectory
deleted, or a filesystem fault that drops some files but leaves
`.seal_epoch` untouched. When `seals_trusted` is otherwise `true`, a
blob-writing scan additionally samples up to `OMEMFS_SEAL_SAMPLE` (default
256; `0` disables sampling) `sealed` entries at a fixed stride across the
sorted entry list (deterministic — no RNG, so the check is reproducible in
tests) and calls `store.exists()` on each. If every sampled entry's object is
present, the scan proceeds as above. If **any** sampled object is missing,
`seals_trusted` is downgraded to `false` for the rest of that scan — every
entry, sealed or not, falls back to a real `store.exists()` call, exactly as
it does today.

Sampling only ever narrows the trust window for the *current* scan; it never
writes anything back by itself (a scan that ends up re-verifying everything
also ends up re-sealing everything, via the normal `update`/`mark_sealed` path
above).

#### Upload-failure recovery (final backstop)

If, despite the above, an upload step still finds a referenced local blob
missing (`ObjectNotFound` while reading from `.omemfs/objects/` to transfer it),
push does not fail silently or upload a tree that references a missing object.
Instead it:

1. Clears the `SEALED` bit on every entry in the STAT_CACHE and writes the
   cache back (hashes are untouched — nothing needs to be re-hashed).
2. Aborts the push with an error directing the user to run `push` again.

The retry then behaves exactly like today's `push` (every entry re-verified via
`store.exists()`, missing blobs recaptured from the working tree) and is
guaranteed to succeed absent a concurrent second loss. This is deliberately a
full seal-clear rather than a full cache-clear: the STAT_CACHE's `(mtime, size,
hash)` data is still correct and reusable, so a retry costs one full
`store.exists()` pass, not a full re-hash of the working tree.

This path is the only one of the three that can still be reached if a future
bug undercounts a loss that epoch/sampling should have caught — it is the
guarantee that a seal-trust mistake degrades to "push fails, retry succeeds,"
never to silent data loss.

#### Implementation notes

- `StatCache` exposes `seals_trusted()` (epoch + sampling, computed once when
  the cache is loaded for a blob-writing scan) rather than threading the result
  through `FileJobConfig` from every caller; `process_file_job` (`src/scan.rs`)
  reads it directly off the cache.
- `stub` (`src/commands/stub.rs`) scans with `write_blobs = true` but does not
  currently write the STAT_CACHE back at all, so it neither sets nor consumes
  seal state. This is an existing asymmetry (unchanged by this design), not a
  new gap: `stub`'s local-existence checks are unaffected either way.
- `ls`/`pull` writeback call sites must carry `seal_epoch` forward unchanged, as
  noted above.

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
