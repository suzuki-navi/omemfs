# Crash Safety

## Purpose

omemfs must survive any interruption — Ctrl+C (SIGINT), SIGKILL, OOM kill, power
loss, or filesystem errors — and remain in a self-consistent state after restart.

omemfs installs no signal handlers, so the process can be killed at any point
without running `Drop` cleanup. Safety is achieved by ordering file writes so
that **any prefix of operations leaves the repository in a valid state**.

---

## State files and safety requirements

All state lives under `.omemfs/` in the working tree root.

| File | Kind | Requirement |
|---|---|---|
| `config` | Configuration | Must never contain partial content (DEK loss is unrecoverable). Written with `atomic_write`. |
| `clone_root` | Required state | Must never contain partial content. Written with `atomic_write`, preceded by a durability barrier. |
| `objects/**` (local cache) | Immutable CAS cache | The final path must never contain a partial object. Written with `atomic_write_no_fsync` (rename-atomic, no per-object fsync). A durability barrier is issued before each `clone_root` and `STAT_CACHE` write to ensure all referenced objects reach durable storage first. |
| temp files (`*.tmp*`) | Staging area | Created by `tempfile::NamedTempFile` (`.tmp` prefix) in the same directory as their destination. Leftover stale temp files (older than 24 hours) are opportunistically deleted on the next write to that directory. They impose no correctness risk. |
| `STAT_CACHE` | Acceleration cache | Corruption or truncation only degrades performance — files are re-hashed. Written with `atomic_write_no_fsync`, preceded by a durability barrier so STAT_CACHE never references objects that have not yet survived a power failure. |

The remote store adds one more file:

| File | Kind | Requirement |
|---|---|---|
| `INDEX_ROOT` (remote) | Remote root pointer | Written with `atomic_write`; additionally protected by a read-then-write CAS check to detect concurrent writers. |
| `objects/**` (remote) | Immutable CAS objects | Written with `atomic_write` (full fsync). Remote objects are shared across clones and must be individually durable. |

Working-tree `.omemfs-stub` files (file and directory stubs) live outside
`.omemfs/` and are recreated by `pull` if lost. They should be written with
`atomic_write` to avoid leaving behind half-written JSON that would cause
`expand` to fail with a parse error.

---

## `atomic_write` helper

All writes to `config`, `clone_root`, `objects/**`, `INDEX_ROOT`, and stub
files go through one of the two helpers in `src/store/local.rs`:

### `atomic_write(path, data)` — durable write

Used for state that must survive a power failure.

```
1. Create NamedTempFile in the same directory as `path`
2. Write data to the temp file
3. f.sync_all()                     — flush data to durable storage
4. NamedTempFile::persist(path)     — atomic rename
5. dir_fd.sync_all()                — flush directory entry to durable storage
```

Step 5 (directory fsync after rename) is required by POSIX to guarantee that
the new directory entry survives a power failure. Without it the rename can
disappear on recovery even though the file data was safely written.

Temp files are created with `tempfile::NamedTempFile` (`.tmp` prefix) in the
**same directory** as `path` — required for the rename to be atomic on most
filesystems.  There is no dedicated `.omemfs/tmp/` directory.  On interruption,
a stale temp file may remain in the directory; it will be opportunistically
removed on the next write to that directory (any file older than 24 hours with
a `.tmp` prefix is deleted before a new temp file is created).  The final path
is always either the previous content or the new content — never partial.

### `atomic_write_no_fsync(path, data)` — crash-safe but not power-loss-safe

Used for pure caches where OS-crash safety is enough and the cost of fsync is
not warranted.

```
1. Create NamedTempFile in the same directory as `path`
2. Write data to the temp file
3. NamedTempFile::persist(path)     — atomic rename (no fsync)
```

The rename is atomic at the OS level, so the file is never partially written.
Power loss before the page cache flushes may revert the file to its previous
state, which is acceptable for a cache.

---

## Durability barrier

A **durability barrier** is a call to `sync_local_objects_fs(objects_dir)` that
flushes all dirty pages in the local `objects/` directory to durable storage.

### Implementation

On Linux: `libc::syncfs(fd)` where `fd` is an open file descriptor to the
`objects/` directory.  This flushes the entire filesystem containing that
directory, which is equivalent to calling `sync_all` on every file — without
holding file handles open for each one.

On non-Linux platforms: `libc::sync()` (global sync, conservative fallback).

### When the barrier is issued

The barrier is called inside `Repo::write_clone_root` and `StatCache::write`,
immediately before the atomic rename that would persist the new pointer.  This
means:

- All object files that were written since the previous barrier are guaranteed
  to be on durable storage before the pointer that references them is persisted.
- If power fails after the barrier but before the pointer rename, the state is
  identical to a crash before any write: the pointer retains its old value.
- If power fails after the pointer rename, all referenced objects are already
  durable (the barrier ran first), so the new pointer is fully valid.

### Why one barrier per command is enough

Every `pull` command ends in exactly one `write_clone_root` call (or zero on
"already up to date"). The objects downloaded during the command are all in the
same filesystem as `clone_root`, so a single `syncfs` at the end covers all of
them atomically.

### Why the barrier is not needed for remote objects

Remote objects are written with the full `atomic_write` helper (fsync per
file). They are shared across clones and must be individually durable so that
any clone can read them at any point after the write returns.

### Power-failure analysis

| Event | Objects on disk? | Pointer updated? | Outcome |
|---|---|---|---|
| Crash before any object is written | No | No | Next run re-downloads everything |
| Crash after some objects, before barrier | Partial | No | Pointer still valid; next run fills gaps |
| Crash after barrier, before pointer rename | Yes | No | Next run re-downloads (idempotent; objects already present) |
| Crash after pointer rename | Yes | Yes | Repository consistent |

---

## `clone_root` write-ordering invariant

> **`clone_root` is written only after all working-tree changes that correspond
> to the new state have been fully applied.**

This invariant holds for every code path:

### pull — fast-forward (no local changes)

```
1. Scan working tree — refresh STAT_CACHE during scan
2. Download missing objects into objects/      (atomic_write_no_fsync per object)
3. apply_diff — update/delete working-tree files
4. write_clone_root(remote_root)               (durability barrier + atomic rename)  ← last
```

Files written by `apply_diff` in step 3 enter the STAT_CACHE on the next scan
(i.e., the next command invocation), not in the same pull run.

### pull — scoped (path argument given)

```
1. Scan target paths — refresh STAT_CACHE during scan
2. Download missing objects into objects/      (atomic_write_no_fsync per object)
3. apply_diff — update/delete target paths only
4. splice_into_clone_root — compute new clone_root hash
5. write_clone_root(new_clone_root)            (durability barrier + atomic rename)  ← last
```

### pull — conflict detected

```
1. Detect conflicts during diff computation (before any working-tree writes)
2. Write .omemfs-conflict-{base,local,remote} helper files
3. Return Err(Error::Conflict) non-zero exit — clone_root is NOT updated
```

If **any** conflict is detected, **nothing** is applied to the working tree —
no partial application occurs.  Conflict helper files are written so the user
can inspect and resolve them.  `clone_root` retains its old value.  The next
`pull` will re-fetch the remote root and retry.

### push

```
1. Scan working tree — compute working_hash
   (local cache objects written with atomic_write_no_fsync)
2. Upload missing objects to remote
3. writer.finish — update INDEX_ROOT (remote root pointer)
4. write_clone_root(new_root_hash)  (durability barrier + atomic rename)  ← last
```

Updating `clone_root` after the remote reflects "the local clone is now caught
up with the remote state it just created".

---

## Self-healing after interruption

| Interrupted operation | State after interruption | Next operation behaviour |
|---|---|---|
| Object download (local) | Objects in page cache or lost; pointer unchanged | Next download re-fetches missing objects (content-addressed: idempotent) |
| Object download (remote) | Stale temp file (`.tmp*`) in destination directory; final path absent | Next download fills in the gap; stale temp cleaned on next write |
| `apply_diff` (WD update) | `clone_root` is old; WD may be partially updated (each file written atomically, so no file is half-written) | `pull` restarts and applies the remainder; already-written files are idempotent |
| `STAT_CACHE` write (after barrier) | Cache absent or old; objects are durable | Next scan re-hashes files with missing cache entries |
| `write_clone_root` (after barrier) | atomic rename — old or new value only | Either value is consistent; objects for new value are already durable |
| Push upload | `clone_root` is old; remote is incomplete | Next push overwrites remote (idempotent) |
| Push `write_clone_root` | Remote updated; `clone_root` still old | Next push re-uploads same content (idempotent) |
