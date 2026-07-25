# Locking

## Overview

When multiple omemfs processes run concurrently against the same working tree,
they may corrupt the `clone_root` state file or observe an inconsistent working
tree mid-scan. omemfs uses a single lock file to prevent this.

```
.omemfs/clone_root.lock
```

This is the only local lock file. Multi-machine exclusion for `INDEX_ROOT` is
handled separately by CAS writes at the remote backend (see `03_sync_model.md`).

---

## Lock acquisition

`.omemfs/clone_root.lock` is a **persistent** lock file.  Commands acquire an
exclusive `flock(2)` on it:

```
open(".omemfs/clone_root.lock", O_WRONLY | O_CREAT)
flock(fd, LOCK_EX | LOCK_NB)
```

`LOCK_NB` (non-blocking) makes acquisition fail immediately if another process
holds the lock.  The winning process writes its PID and command name into the
file for diagnostics.

---

## Lock release

The lock is released automatically when the process exits (or when the file
descriptor is closed), so there is no explicit unlink step.  `clone_root`
updates use `atomic_write` as before (temp-file → rename), independently of
the lock release.

There is no stale-lock problem: when a process is killed, the kernel releases
the flock automatically, so any waiting process can acquire the lock
immediately on the next attempt.

---

## Commands and their lock requirements

| Command | Lock required | Reason |
|---|---|---|
| `omemfs push` | yes | reads and writes `clone_root` |
| `omemfs pull` | yes | reads `clone_root`, updates working tree, writes `clone_root` |
| `omemfs restore` | yes | reads `clone_root`, modifies working tree |
| `omemfs stub` | yes | verifies working tree against `clone_root` before modifying files |
| `omemfs pack` | yes | modifies pack index under `.omemfs/`; must not race with push/pull |
| `omemfs expand` | yes | writes working-tree files; lock prevents races with push scanning a partially-written file |
| `omemfs conflict clean` | yes | removes conflict helpers; must not race with pull |
| `omemfs conflict accept-*` | yes | replaces working-tree files and removes conflict helpers; must not race with push/pull |
| `omemfs ls` | no | read-only; stale reads are acceptable |
| `omemfs cat` | no | read-only |
| `omemfs stats` | no | read-only |
| `omemfs clone` | no | no existing `clone_root` to race against |
| `omemfs config` | no | writes `.omemfs/config` only; does not touch `clone_root` or the working tree |
| `omemfs log` | no | read-only log analysis |

### Read-only commands

`omemfs ls`, `omemfs cat`, and `omemfs stats` do not acquire the lock. A
concurrent write may cause them to observe a momentarily stale `clone_root`,
but the rename-based write guarantee means they never read a partially-written
file.

### `omemfs expand`

`expand` writes working-tree files but does not modify `clone_root`.  It
acquires the lock to prevent races with a concurrent `push` that might scan
while `expand` is writing files.  Each file is written through `atomic_write`
(temp-file → rename) so a concurrent observer always sees either the old
content or the fully-written new content — never a partial write.

---

## Deadlock prevention

There is only one local lock (`clone_root.lock`), so deadlock between local
locks cannot occur. The remote CAS is a network call and carries no local lock
dependency.

---

## Lock contention

When `flock(LOCK_EX | LOCK_NB)` fails (the lock is held by another process),
the command fails immediately with:

```
error: Unable to acquire lock '.omemfs/clone_root.lock': file exists.

Another omemfs process (PID: 12345) holds this lock. Wait for it to finish,
or terminate that process if it is stuck.
```

The PID and command name written to the file at acquisition time are shown for
diagnostics. The message deliberately does NOT suggest deleting the lock
file: because locking is `flock(2)`-based (see "No stale-lock problem"
below), deleting `.omemfs/clone_root.lock` while the holder is still alive
does not free anything the holder is using -- it just makes the next `omemfs`
invocation create a brand-new inode and take its own, independent flock on
it, so the original holder and the new process would then mutate
`clone_root` concurrently with no lock between them. If you believe the
holder is genuinely stuck, terminate that process (e.g. `kill <PID>`);
the kernel releases the flock the moment its file descriptor closes.

### No stale-lock problem

`flock(2)` is held by the kernel on behalf of the file descriptor.  When the
owning process exits (normally, via SIGKILL, OOM, or power loss), the kernel
releases the flock automatically.  There is no stale-lock condition and no
PID-based cleanup is required.

### Limitations

`flock(2)` may not be reliable on some network filesystems (NFS in particular
may silently ignore `flock` or treat it as advisory only).  Using omemfs with
a `.omemfs/` directory on an NFS mount is not supported.

---

## Object store locking

Objects under `.omemfs/objects/` do not require a lock. Objects are
content-addressed: the same content always produces the same hash, so two
processes writing the same object concurrently produce identical results.
`atomic_write` (see `11_crash_safety.md`) ensures no partial objects are ever
visible at the final path.

---

## Relationship with crash safety

The lock file pattern and the `atomic_write` helper serve complementary roles:

- `atomic_write` guarantees that `clone_root` is never half-written (crash
  safety for a single writer).
- `clone_root.lock` guarantees that only one process writes `clone_root` at a
  time (concurrency safety between multiple processes).

Both are required for full correctness.

---

## INDEX_ROOT CAS on the local-directory backend

On the local-directory backend, the `INDEX_ROOT` read-compare-write (CAS) is
serialized by an exclusive `flock` on a lock file placed next to `INDEX_ROOT`
**on the remote side** (e.g. `.omemfs-remote/INDEX_ROOT.lock`).  This is
separate from the clone-side `clone_root.lock` and is held only for the
duration of the CAS check-and-write.

## INDEX_ROOT CAS on cloud backends (no lock file)

The cloud backends (S3, Azure, GCS) need **no lock file at all** for the
`INDEX_ROOT` CAS. Each provides a server-side conditional write that is atomic
across all clients: S3 and Azure condition the index-root `PutObject` /
upload on the object's ETag (`If-Match: <etag>` for an update,
`If-None-Match: *` / `.if_not_exists()` for the create case), and GCS conditions
the upload on the object's generation (`ifGenerationMatch=<generation>`, or
`=0` for the create case). The server atomically rejects a write whose
precondition no longer holds with `412 Precondition Failed`, which the backend
maps to `Error::CasFailed`. Because the compare-and-write happens atomically on
the server, there is no TOCTOU window to close with a lock and no
`INDEX_ROOT.lock` object is ever written on a cloud backend. See the CAS-safety
section of `03_sync_model.md` and `design/13_cloud_backends.md` for the
per-backend mappings.

Parallel object transfers (`OMEMFS_TRANSFER_CONCURRENCY > 1`) do **not** affect
this: object writes are content-addressed and idempotent, so they require no
locking, and the single mutable pointer — the index root — is still written by
exactly one CAS in `finish()` after all parallel object PUTs have joined. The
clone-side `clone_root.lock` continues to serialize the local `clone_root`
update for the whole command regardless of backend.

---

## Summary

| Mechanism | Scope | Purpose |
|---|---|---|
| `clone_root.lock` (`flock`) | single machine | prevent concurrent updates to `clone_root` |
| `atomic_write` (`rename`) | single machine | prevent partial writes to `clone_root` and objects |
| Remote CAS (cloud: ETag `If-Match` / `ifGenerationMatch`; local: remote `flock`) | multi-machine | prevent concurrent updates to `INDEX_ROOT` |
