# Remote Root History

> **Status: Not yet implemented.** This document specifies a planned feature.
> No part of it exists in the current implementation. It is recorded here so the
> design is settled before any code, test, or storage-format change is made.
> Until it is implemented, the index root carries a single `remote_root` and no
> history (see `02_storage_format.md`).

## Goal

Let the user browse and restore recent past states of the remote, similar to
"Time Machine", without turning omemfs into a version-control system. The remote
keeps a bounded window of recent root snapshots (e.g. the last 30 days). Older
snapshots fall out of the window and their exclusively-referenced objects become
reclaimable.

Two properties are explicit non-goals, because they conflict with the bounded
window:

- **No history graph.** Entries are a flat list of independent root snapshots.
  They are **not** chained by parent pointers.
- **No unbounded retention.** Anything older than the retention window is pruned.

### Why a flat list, not a parent chain

A parent pointer (as in a Git commit) makes every ancestor transitively
reachable from the newest entry. Under omemfs' reclaim model — where
unreferenced objects are dropped by the backup-reclone cycle and there is no
generational GC (see `02_storage_format.md`, "Reclaiming unreferenced objects")
— a parent chain would pin **every object from the beginning of history**
forever and make old objects impossible to drop. That directly defeats the
bounded-window goal.

A flat list avoids this. Each entry is a **self-contained snapshot**: a complete
root tree hash. Shared subtrees are deduplicated automatically because objects
are content-addressed, so listing many snapshots side by side does not duplicate
storage. Dropping one entry does not affect the reachability of any other entry,
because there are no inter-entry links. Pruning an entry simply returns the
objects that **only** that entry referenced to the orphan set.

## Storage format

The history list lives **inside the index root** plaintext. It is not a separate
object: enumerating the snapshots must read a list of hashes from somewhere, and
the index root is the one mutable pointer that is already read and CAS-written on
every push. Embedding the list there avoids an extra object fetch and reuses the
existing CAS for atomic append-and-prune.

The current index root plaintext (`02_storage_format.md`, "INDEX_ROOT (index
root object)") is extended as follows. The change bumps the plaintext `version`
byte to `0x02` and repurposes the existing 2-byte `padding` field as
`history_count`:

```
magic              : 2 bytes   (ED E3)
version            : 1 byte    (0x02)   ← bumped from 0x01 for history support
reserved           : 1 byte    (0x00)
remote_root        : 32 bytes  (tree hash; all-zero if never pushed)
hot_hash           : 32 bytes
bloom_hash         : 32 bytes
cold_prefix_bits   : 1 byte
reserved2          : 3 bytes
delta_count        : 2 bytes   (big-endian)
history_count      : 2 bytes   (big-endian)   ← new; was `padding`
delta_hash[0..N]   : 32 × delta_count bytes        (newest first)
history[0..H]      : 40 × history_count bytes       (newest first)
cold_shard[...]    : 32 × 2^cold_prefix_bits bytes
```

Each history entry is 40 bytes:

```
entry = {
    timestamp_be : 8 bytes   (Unix seconds, big-endian, UTC)
    root         : 32 bytes  (root tree hash of that snapshot)
}
```

The entries are ordered **newest first**, matching the existing `delta_hash[]`
convention.

Entries carry only a timestamp and a root hash. No author, message, or label is
stored: the purpose is browse-and-restore, not an audit log. Keeping the entry
fixed at 40 bytes keeps the list compact.

### Relationship between `remote_root` and `history[0]`

`remote_root` is retained as a dedicated field and is kept **identical to
`history[0].root`** by construction (when any history exists). This is
redundant, but deliberate:

- `pull` and `ls` read only `remote_root` today (`PackReader::read_root`).
  Keeping the field means those paths are unchanged.
- A reader that does not understand history (an older binary reading a `0x02`
  index root, or any code that only needs the current state) can still obtain
  the current remote root from the fixed-offset field without parsing the
  variable-length history list.

When `history_count == 0` (a brand-new remote that has never been pushed),
`remote_root` is the all-zero hash and there is no `history[0]`.

### Size

Each entry is 40 bytes. With a 30-day window and, say, 30 pushes per day, the
list holds roughly 900 entries ≈ 36 KiB. The index root is GET/PUT as a single
small object on every push and is **not** part of the pack cache, so a list of
this size is acceptable. If a deployment pushes far more frequently, see
"Retention policy" for how the window bounds growth, and "Open question:
externalising the list" for an escape hatch.

## Push: append and prune

History maintenance happens entirely within the existing push flow
(`03_sync_model.md`, "Push") and within the same CAS write, so append and prune
are atomic with respect to concurrent pushers.

1. A push that does not change the root exits early (`03_sync_model.md`, Push
   step 3: working tree == clone root → nothing to push). Therefore a new
   history entry is appended **only when the root actually changes**. No-op
   pushes never grow the list.
2. When the new root tree hash is known (before the CAS write of the index
   root), prepend `(now, new_root)` to `history[]` and set `remote_root =
   new_root`.
3. **Prune** in the same step: drop every trailing entry whose `timestamp` is
   older than the retention window (see "Retention policy"). Because the list is
   newest-first and entries are independent, this is a tail truncation; removing
   an entry never invalidates a newer one.
4. The amended index root (new `history[]`, new `remote_root`, updated
   `history_count`) is written with the existing CAS conditioned on the bytes
   read at push start. Append and prune are therefore committed atomically; a
   concurrent pusher either sees the whole update or none of it.

Path-scoped push (`03_sync_model.md`, "Path-scoped push") also produces a new
root tree and appends an entry the same way.

### CAS failure and retry

On a CAS failure the user must `pull` and retry the push (`03_sync_model.md`,
Push step 6). Because the list has no parent links, retry is a plain re-read of
the (now newer) `history[]` followed by another prepend — there is no rebase or
chain-rewrite complexity. The objects built locally are reused as today.

## Interaction with `pack` and reclaim (critical)

This is the load-bearing part of the design. Pruning a history entry must **not**
by itself delete any object — object deletion remains the job of the
backup-reclone cycle, and there is no generational GC.

The change required is in the reachability set used by `omemfs pack` when it
rebuilds the hot index. Today the hot index contains objects reachable from the
working tree, `clone_root`, and `remote_root` (`02_storage_format.md`, "hot
index"). With history:

```
hot index reachability  =  working tree
                         ∪  clone_root
                         ∪  every root in history[]      ← new
```

(`remote_root` is `history[0]`, so it is covered by the union.)

Consequences:

- While a snapshot is in the window, all objects it references are reachable
  from the hot index and are therefore preserved on the remote and copied by a
  backup-reclone. Browsing or restoring that snapshot never hits
  `ObjectNotFound`.
- When a snapshot is pruned out of the window, the objects it **exclusively**
  referenced are no longer in the reachability union. The next `omemfs pack`
  rebuild drops them from the hot index, which returns them to the orphan set,
  and the next backup-reclone cycle reclaims them. Objects still referenced by a
  newer in-window snapshot remain reachable and are untouched.

If `pack` did **not** include in-window history roots in its reachability set,
it would drop objects that an in-window snapshot still needs, and
`history --at <ref>` would fault. Including them is mandatory.

## Interaction with lazy clone

After a lazy, stub-aware clone (`03_sync_model.md`, "Lazy tree reads"), the local
`objects/` cache holds only what was needed to materialise sub-threshold
content. The history feature works with this as follows:

- **Listing** (`omemfs history`) reads the `history[]` array straight out of the
  index root. It requires **no** remote object fetch — timestamps and root
  hashes are already present in the single index-root read.
- **Browsing and restoring** a past snapshot (`history --at`, `ls --at`,
  `restore --at`) read the past root tree through the existing **lazy tree
  store** (`03_sync_model.md`, "Lazy tree reads"), so only the subtrees actually
  visited are fetched from the remote, exactly as for the current root. No new
  download mechanism is needed.

The reachability rule above guarantees the remote still holds the objects of any
in-window snapshot, even one whose objects were never cached locally.

## Retention policy

The window is **purely time-based: entries older than the retention period are
pruned.** The default retention is **30 days** and is configurable (the exact
config key is deferred to implementation; it belongs in the repo config schema,
`04_cli_spec.md`).

Pruning is evaluated at push time (see "Push: append and prune" step 3) against
`now`. A purely time-based policy is the simplest rule that meets the goal; its
known cost is that a very high push frequency grows the list within the window.
If that becomes a problem in practice, a thinning policy (keep all recent
entries, decimate older ones to daily) can be layered on later **without**
changing the storage format or the reclaim interaction, because every entry is
independent — thinning is just a different choice of which trailing entries to
drop.

## Commands

The history feature is browse-and-restore only. There is no `checkout` that
moves a HEAD to a past state — omemfs is a sync tool, not a VCS, and `clone_root`
remains the single local sync boundary.

A reference `<ref>` to a snapshot may be one of:

- a (possibly abbreviated) root tree hash present in `history[]`;
- `@N` — the Nth most recent entry, 1-indexed (`@1` = newest). This matches the
  `@N` form already used by `omemfs log show` (`04_cli_spec.md`, "omemfs log").
- `@{<when>}` — the newest entry at or before a time, where `<when>` is an
  absolute date (`@{2026-06-01}`) or a relative age (`@{3d}`). Resolved by a
  binary search over the newest-first list.

### omemfs history

Note: the existing `omemfs log` command is reserved for structured **log-file**
analysis (`design/10_log_analysis.md`). The remote-root history uses the
distinct verb `history` to avoid the collision.

```
omemfs history [-n <count>]
```

List remote root snapshots, newest first: index (`@N`), timestamp, and
abbreviated root hash. Reads only the index root; no remote object fetch.

```
$ omemfs history -n 3
@1  2026-06-15 09:20:11Z  a1b2c3d4
@2  2026-06-15 08:05:42Z  e5f6a7b8
@3  2026-06-14 22:44:03Z  c9d0e1f2
```

### omemfs ls --at <ref>

```
omemfs ls --at <ref> [-r] [--full-hash] [<path>...]
```

List a path as it existed in snapshot `<ref>`, reading the past root tree
through the lazy tree store. Behaves like `omemfs ls --remote` but against the
historical root instead of the current remote root. The `R`/dirty columns that
compare against the working tree are not meaningful with `--at` and are omitted.

### omemfs diff <ref1> <ref2>

```
omemfs diff <ref1> <ref2> [<path>...]
```

Show the difference between two snapshots, reusing the existing tree diff
algorithm (`03_sync_model.md`, "Diff algorithm"). Both roots are read through the
lazy tree store, so only differing subtrees are fetched. `<path>...` restricts
the diff to the given subtrees. (`diff` is a new top-level command; today the
only diff surface is `ls --dirty`, which compares the working tree to
`clone_root`.)

### omemfs restore --at <ref> <path>

```
omemfs restore --at <ref> <path>...
```

Restore the given path(s) to their content in snapshot `<ref>`, writing into the
working tree. This extends the existing `omemfs restore` (`04_cli_spec.md`,
"omemfs restore"), which today restores to the `clone_root` state. Content is
fetched on demand through the lazy tree store. As with the current `restore`,
this changes the working tree only; `clone_root` is updated per the normal push
flow when the restored state is subsequently pushed.

## Open question: externalising the list

If a deployment's push frequency makes the embedded list large enough to bloat
the per-push index-root GET/PUT, the list can be moved into a **single dedicated
object** whose hash the index root holds, instead of being embedded inline. This
keeps the index root small at the cost of one extra fetch when the list is
needed. It does **not** reintroduce the parent-chain problem: the external object
is a flat array, read only for enumeration, never a link target that affects
reachability. This is left as a future option; the embedded form is the baseline
because it is simpler and needs no extra round trip.

## Summary of changes required when implemented

- `02_storage_format.md`: bump index-root plaintext `version` to `0x02`, add
  `history_count` (repurposing `padding`) and the `history[]` array; document
  the `remote_root == history[0].root` invariant; extend the hot-index
  reachability definition to include all in-window history roots.
- `03_sync_model.md`: in Push (and path-scoped push), append-and-prune the
  history list within the index-root CAS write; note that lazy tree reads back
  `--at` browsing/restore.
- `04_cli_spec.md`: add `omemfs history`, `ls --at`, `diff`, and `restore --at`;
  add the retention-period config key; document the `<ref>` grammar.
