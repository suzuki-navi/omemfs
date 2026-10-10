# Sync Model

## Three-state model

omemfs tracks three states to determine what changed and where:

```
working tree    the actual files on disk — the user's truth
clone root      tree hash of the last successfully synced state (stored in .omemfs/clone_root)
remote root     tree hash of the current remote state (stored inside the index root on the backend)
```

The remote is the authoritative record. The local `objects/` directory is a cache.

```
                   push
working tree  ──────────►  remote root
     ▲                          │
     │  restore                 │  pull
     │                          │
     └────  expand  ──  stub  ◄─┘

clone root: the last successfully synced state, held locally
```

These arrows describe **state** relationships only — which of the three states
a command reads from and writes to. They say nothing about where a command is
allowed to resolve an *object* from once it knows which state it needs. Object
resolution is a separate, lower layer (the local cache, the lazy tree store,
and — for a diagnostic-only command — LiveFallback; see `02_storage_format.md`,
"Snapshot read path"). In particular, `restore`'s arrow points only at the
working tree and `clone_root`: it never reads or advances the remote root. That
does not mean `restore` is barred from talking to the remote at all — it may
still need to fetch, from the remote, objects that `clone_root` already
records but the local cache does not yet hold (see `04_cli_spec.md`, "omemfs
restore" → "Missing objects"). Fetching a `clone_root`-reachable object is
resolving state `restore` already owns by this diagram, not advancing to a
different state, so it adds no new arrow here.

## Change detection

Push and pull determine what changed by comparing all three states:

| Comparison                   | Meaning                              |
|------------------------------|--------------------------------------|
| working tree ≠ clone root    | local uncommitted changes            |
| remote root ≠ clone root     | remote has new changes since last sync |
| both differ from clone root  | potential conflict                   |

The diff between two trees is computed by recursively comparing tree hashes. Two paths are compared only when their tree hashes differ, so large unchanged subtrees are skipped in constant time.

---

## Working tree scan optimization

When scanning the working tree for `push`, omemfs avoids computing the content
hash for files that are provably unchanged using a two-stage check.

### Stage 1: mtime pre-filter

For each file, the scan checks whether the filesystem mtime and size both match
the values stored in the clone root tree entry for that path:

```
file.mtime == clone_root.mtime  AND  file.size == clone_root.size
  → candidate for skipping hash computation
```

If both match, the clone root hash is reused without reading the file content.

### Stage 2: racy check

The mtime comparison alone is not sufficient. A file written very recently may
have its mtime rounded to a coarser value by the filesystem (e.g. FAT32 rounds
mtime to 2-second intervals), making it appear to match even after being modified.

To guard against this, files within the "racy window" always have their hash
computed regardless of the mtime comparison:

```
now - file.mtime < RACY_THRESHOLD  →  racy: force hash computation
```

`RACY_THRESHOLD` is **3 seconds** — enough margin to cover FAT32's 2-second mtime
granularity plus a safety margin. This is the same strategy used by Git
("racy git" detection).

### mtime stability

When a file's hash is computed (because mtime differed or the file was racy) and
the result matches the clone root hash, the **clone root mtime is reused** in the
new tree entry instead of the filesystem mtime.

This prevents spurious tree hash changes caused by tools like `rsync -a` or
`cp -p` that preserve file content but alter filesystem timestamps.

### Parallel walk and hash computation

The scan is the dominant cost of `ls` / `push` / `pull` on a large working tree.
Two kinds of work are parallelised against a **single persistent global thread
pool** (rayon): the **directory recursion** itself and the **per-file hashing**
within each directory level. The pool is created once per process and reused for
every `scan_dir` call, so the scan no longer spawns and joins OS threads per
directory (the previous design spawned two threads for every directory level
that held two or more files — tens of thousands of `clone3`/`join` pairs on a
large tree, which dominated the wall-clock time).

`scan_dir` is structured as a pure function: each call returns its own local
result (the level's `TreeEntry` list, a partial `files` map, and a partial
`ScanSideData`) and the caller merges the children's results into its own. There
is no shared mutable `files_out`/`side` threaded through the recursion, so two
sibling subdirectories can be scanned concurrently with no locking. The
subdirectories of one level are scanned with `rayon::scope` (one task per child
directory); the regular files of one level are hashed with `par_iter`. rayon's
work-stealing flattens both axes onto the same fixed-size pool, so the live
thread count is bounded by the pool size regardless of tree depth or fan-out —
unlike per-directory `thread::scope`, recursion does not multiply the thread
count.

The pool size is `OMEMFS_SCAN_THREADS` when set (and parseable as a positive
integer), otherwise `min(available_parallelism, 4)`. The cap of 4 is deliberately
conservative: the scan's bottleneck on a cold tree is per-file read + SHA-256 and
(for `push`) blob compression, and a small amount of parallelism relieves that
without oversubscribing disk I/O; the local object store's transfer concurrency
also defaults to 1 (`OMEMFS_TRANSFER_CONCURRENCY`, see `04_cli_spec.md`). On a
2-core machine the effective default is 2.

**Parallelism does not affect the result.** The tree hash is order-independent of
how the entries are produced: `build_and_store` sorts entries by name before
serialising (see `01_object_model.md`), so child results may be merged in any
order. The parent-after-child ordering that a directory's tree hash requires (a
parent tree entry embeds each child's hash) is preserved naturally: a `rayon::scope`
join point waits for every child task to finish before the parent builds its own
tree. The object store is `Send + Sync` and content-addressed writes are
idempotent (`NamedTempFile` + atomic rename / `persist`), so concurrent blob and
tree writes are safe and need no ordering. STAT_CACHE and clone-root lookups are
read-only and safe to share by shared reference. If any task returns an error,
the scan fails with that error (first error wins — rayon's `par_iter` /
collecting into `Result` short-circuits on the first `Err`), matching the
previous serial behaviour. Every regular file is still `stat`-ed and, on a
STAT_CACHE / clone-root miss, fully read and hashed: the parallelisation never
skips a file based on mtime alone (the mtime pre-filter above is the only skip,
and it is unchanged).

**Unreadable entries are an error; live-tree races preserve previous state.**
Listing a directory (`read_dir`) or stat-ing an entry found by a
parent's listing (`file_type`/`symlink_metadata`) can fail for two different
reasons: (a) the entry no longer exists — a benign race where something else
(the same user, another process) removed it between being listed and being
visited, which is expected and harmless; or (b) a genuine I/O problem such as
a permission error (`EACCES`) or a failing filesystem. Conflating the two by
treating every stat/listing failure as "nothing here" is a correctness bug:
case (b) would silently produce a smaller tree than what is actually on disk,
and a subsequent `push` would record that smaller tree as the new remote
state -- indistinguishable, from the remote's point of view, from the user
having deleted the unreadable files. A push scan therefore distinguishes them:
an entry that vanishes after its parent was listed, or remains active through
the bounded per-file retries, is marked unstable. Its clone-root entry is
preserved when one exists; a transient new entry is omitted for this push.
Every other `io::Error` (permission denied, stale NFS handle, etc.) is
propagated and fails the scan. A prior entry absent from both the initial and
final directory observations remains a real deletion.

### Scan blob-write mode (read-only vs. write commands)

The working-tree scan computes, for every regular file that misses both the
STAT_CACHE and the clone-root fallback, its content hash. Computing the hash
requires reading the whole file. **Whether the blob is also chunked, compressed,
encrypted, and written to the local object store is a separate decision** that
depends on the command:

- **`push` / `stub`** need the blob objects in `.omemfs/objects/` so that the
  upload step can transfer them to the remote. They scan with blob writing
  **enabled** (`write_blobs = true`).
- **`ls` / `pull`** only need the working-tree **root hash** (to diff against the
  clone root and the remote). They never read blob *bodies* back — only tree
  objects are read during diffing. For these commands the scan computes hashes
  but **skips** chunking, compression, encryption, and the blob object write
  (`write_blobs = false`). Tree objects are **always** written, because the diff
  walk (`diff_trees` / `load_all_entries`) reads them back.

Skipping the blob write removes the dominant cost of a cold (cache-miss) `ls`:
on a ~1.5 GB tree, the per-file compress + object-store-write accounted for the
large majority of the command's wall-clock time, while the hash itself is a
single streaming SHA-256 pass.

#### In-process tree-entry cache

A read-only command such as `ls` touches every directory tree object **twice**:
the scan phase builds each tree (via `build_and_store`, which serialises the
entries and stores the object), and the listing/diff phase then re-reads the same
tree objects (`diff_trees` and `collect_tree_rows`, both via `load_all_entries`)
to produce output. The second pass re-reads each tree object from the local store
and decompresses it, even though the scan held the identical entries in memory
moments earlier.

To eliminate the second pass, omemfs keeps an **in-process tree-entry cache**
keyed by tree object hash for the lifetime of a single command:

- `build_and_store` inserts the entries into the cache once the tree object's
  hash is known (and the object is durable on disk).
- `load_all_entries` consults the cache before reading the store; a hit returns
  the cached entries with no disk read and no decompress.

A cache hit is always correct because tree objects are **content-addressed and
immutable**: a matching hash guarantees identical entries. The cache is bounded
to one command — it is enabled by an RAII guard at the start of `ls` and cleared
when the guard drops — so memory does not accumulate across invocations, and the
cache is **disabled by default**, leaving `push`, `pull`, and every other command
unaffected.

This cache addresses only the listing phase's redundant reads. The scan phase
still builds and serialises every tree, and on a very large working tree the
dominant remaining cost is the filesystem walk itself. On a ~370k-file working
tree the cache cut a full `ls -r` from roughly 17.9 s to 12.2 s, with
`read_object` and decompress calls dropping by about 96% (from ~18,800 to ~840).

#### Minimising `statx` during the walk

The walk's residual cost is the per-entry `statx` count. The scan reduces it on
two fronts:

- **One metadata call per entry.** `read_dir` already returns a `DirEntry` whose
  `file_type()` is satisfied on Linux from the `getdents64` records (no extra
  `statx`) in the common case. The walk uses that `file_type()` to classify each
  entry (symlink / dir / regular file) and only calls `symlink_metadata` once,
  for the single entry being turned into a tree entry, where `mode` / `size` /
  `mtime` are genuinely needed. The earlier code paid an extra `symlink_metadata`
  per ignored entry and re-`stat`-ed paths it had already classified.
- **Existence checks via the already-listed name set.** A file-adjacent stub
  (`<name>.omemfs-stub`) and a real file (`<name>`) live in the *same* directory,
  whose full name set (`raw_names`) was already materialised by the single
  `read_dir`. Their presence is therefore decided by a set lookup rather than an
  `is_file()` / `exists()` `statx`. A *directory* stub marker
  (`.omemfs-stub` inside a child directory) is not in the current level's name
  set, so that one existence check still costs a `statx` — kept to a single call.

On a stub-free repository (the common case) these changes drop the per-entry
`statx` count from 2–3 to 1.

#### Partial-expansion merge is independent of scan root

The two stub-detection paths above — the file-adjacent stub name-set lookup
and the directory-stub `statx` — are both run by a directory's *parent* while
enumerating that directory as a child. When the child is a directory stub
whose target is a tree and the directory also has real children (a
*partially expanded* directory; see `08_stub_system.md`, "Partial
expansion"), the parent recurses into it and then merges the stub's recorded
tree with the tree just scanned from disk (`merge_partial_expansion`):
on-disk entries win over same-name stub entries, and the merged tree replaces
the disk-only tree as that child's entry.

This merge runs identically when the partially expanded directory is not a
child at all, but is itself the top-level `dir` a `scan_dir` call was invoked
on — which happens for every scoped scan (`ls <path>`, `push <path>`; see
`04_cli_spec.md`, "Scoped working-tree scan") whose `<path>` lands exactly on
a partially expanded directory. A scan root has no parent to perform the
lookup on its behalf, so `scan_dir` detects its own `.omemfs-stub` marker and
applies the merge itself before returning, rather than assuming the merge is
always "handled by the caller." Skipping the merge at the root would build a
tree containing only the on-disk entries and silently drop every stub-only
entry the directory still logically contains — entries that a whole-tree
(unscoped) scan reaching the same directory as a descendant would have merged
in. The two scans would then disagree about that directory's hash, and a
subsequent diff against `clone_root` would report the missing entries as
spurious deletions, propagated up to every ancestor row shown by `ls`.

The merge is therefore scan-root-independent: a given partially expanded
directory produces the same merged tree hash whether `scan_dir` reaches it as
a child of an enumerating parent or is invoked on it directly as the scan
root. This is what allows a scoped command's result for a path to always
agree with an unscoped command's result for the same path (see
`04_cli_spec.md`, "Scoped working-tree scan").

#### STAT_CACHE invariant and push-time staging

Historically a STAT_CACHE hit implied "this blob is already in the local object
store" — because the only way an entry entered the cache was a scan that had
just written the blob. Allowing `ls` / `pull` to populate the STAT_CACHE
**without** writing the blob breaks that implication: a later `push` may find a
file that is a STAT_CACHE hit (so the scan does not re-write it) yet whose blob
is absent locally and absent on the remote.

Note this window already existed in a narrower form: a STAT_CACHE entry written
by a previous `push` survives even if the local blob is later deleted (the cache
lives in `.omemfs/`, the blobs in `.omemfs/objects/`, and they can diverge).

To make `push` correct regardless of how the STAT_CACHE was populated, its scan
accepts a metadata/cache hit only when the referenced local blob is present.
"Present" always means `codec::chunk::blob_fully_cached` — the manifest and
every chunk it lists, for a chunked blob — never a bare `store.exists()` on
the top-level hash alone (see `07_stat_cache.md`, "Depth of the
local-existence check"). When it is absent, the file is captured and staged
during the scan. The tree root is not sealed until every newly referenced
blob is local (or already a preserved remote entry). Upload never re-reads
the working tree. Consequently, edits or deletes after the scan cannot
invalidate the root being uploaded.

### Diff output

Each path falls into one of three categories:

- **added**: present in target, absent in base
- **deleted**: absent in target, present in base
- **modified**: present in both, but hash differs — or, for blobs, the
  executable-bit `mode` differs (an executable-bit-only change is a
  modification, matching push's tree-hash-based dirty detection, since
  `mode` is part of the serialised tree entry)

Rename detection is not performed. A rename is treated as a delete + add.

---

## Push

```
omemfs push
```

### Steps

1. Check for unresolved conflict helper files (`.omemfs-conflict-*`) in the working tree.
   - If any are found → error. The user must resolve all conflicts before pushing.

2. Scan the working tree and build tree and blob objects with blob writing enabled (`write_blobs = true`). Each file is opened once per attempt; metadata and bytes come from the same open handle. Retry active files once, then preserve their previous entry (or omit an unstable new file) and report a warning. Store every newly referenced blob in `.omemfs/objects/` before sealing the tree root.

3. Compare the working tree hash to the clone root hash.
   - If they are equal → nothing to push. If `--with-backup` was given and this is not a `--dry-run`, still attempt a backup push (design/04_cli_spec.md, "Backup push behaviour") before exiting, since origin having nothing new does not mean backup has caught up. Otherwise exit here.

4. Read the index root from the remote backend and record the raw bytes (including the remote root hash and pack index metadata). The index root location is the derived key for encrypted remotes, or the fixed `<prefix>/INDEX_ROOT` for unencrypted remotes.

4b. Compare the remote root read in step 4 against `clone_root`. If they differ, the remote has advanced since this clone last synced: fail immediately with the same error as a CAS failure (`remote has been updated since last sync`), without uploading any objects or touching the index root. Normalize both sides before comparing: a missing index root, an explicitly empty/all-zero remote root, an absent `clone_root`, and the canonical empty-tree hash must all be treated as equivalent to "nothing has been pushed yet" — so the very first push into a fresh remote, and a push immediately after `clone --new`/`clone --existing`, are both allowed to proceed. There is one more exception, checked first: if the working tree just scanned in step 1 already hashes to exactly the remote root just read (i.e. this push's own new content already matches what the remote currently holds), the push proceeds regardless of how stale `clone_root` is — pushing it changes nothing on the remote, so no update can be lost. This is what makes a push right after `omemfs conflict accept-remote <path>` succeed as a safe no-op: a conflicting `pull` never advances `clone_root` (see "Conflict handling" below), but `accept-remote` restores the remote side's exact content *and* its tracked mtime/mode, so the resolved working tree already hashes identically to the remote root.

5. Upload objects that are missing from the remote (BFS order: blobs before trees), reading only the sealed local object store. A missing local object is an invariant violation and fails the push; the live working tree is never consulted during upload.

6. Update the index root on the remote using a CAS (compare-and-swap) write conditioned on the version token observed in step 4. On the local-directory backend the read-compare-write is serialized with an exclusive `flock(2)` on `<prefix>/INDEX_ROOT.lock`; the cloud backends use server-side conditional writes (S3/Azure ETag, GCS generation). All object uploads in step 5 complete (and, when parallel, join) before this single CAS write runs.
   - If the CAS fails (another client pushed concurrently) → error. The locally built objects are already in `.omemfs/objects/` and can be reused after a `pull`.

7. Write the new working tree hash to `.omemfs/clone_root`.

### Push error: unresolved conflicts

```
error: unresolved conflicts — resolve or restore before pushing
The following conflict helper files were found:
  src/main.rs.omemfs-conflict-base
  src/main.rs.omemfs-conflict-local
  src/main.rs.omemfs-conflict-remote
```

Conflict helper files are excluded from all working tree scans. They are never treated as user-managed files and are never included in tree or blob objects.

### Push error: concurrent remote update

```
error: remote has been updated since last sync
Run 'omemfs pull' and retry 'omemfs push'.
```

The locally built tree is preserved; only the index root was not updated. After `pull` succeeds, `push` can proceed.

This single error message now covers two distinct causes:

1. **Divergence detected up front (step 4b / the per-path check under "Path-scoped push" below).** This is the common case: someone else already pushed since this clone's last sync, observed at the very start of this push, before any object was built or uploaded. This is the primary defense against a lost update — without it, a push whose remote had already advanced before the push started would sail through the CAS unaffected, because the CAS's own precondition (the token read at push start still matching at finish) is trivially satisfied in that case: nothing changed *during* the push's own window, so the CAS has nothing to catch.
2. **A genuine CAS failure at finish time**, if another push's CAS-write lands in the narrow window between this push's own step 4b check and its own finish. The CAS narrows this residual window to just this push's own duration; it does not, by itself, guarantee fast-forward-only semantics across the whole operation — that guarantee comes from step 4b (and its per-path analogue for a scoped push).

Both outcomes are correct and indistinguishable to the user: either way, someone else's change would otherwise have been silently overwritten, and this push must be retried after a `pull`.

---

## Pull

```
omemfs pull
```

### Steps

1. Read the index root from the remote backend and extract the remote root hash. The index root location is determined by encryption mode (see CAS safety section).
   - If remote root == clone root → already up to date, exit.

2. Determine the relationship between the working tree and the remote root using 3-way comparison:
   - **working tree == clone root** (clean): proceed to fast-forward.
   - **working tree ≠ clone root** (dirty): check for conflicts between local changes and remote changes.

3. Compute the diff between clone root and remote root (remote changes).

4. If the working tree is dirty, compute the diff between clone root and working tree (local changes).
   - Check whether the local change set and remote change set intersect (same path appears in both).
   - If they intersect → conflict, abort (see Conflict handling below).
   - If they do not intersect → safe to merge.

5. There is **no** eager object pre-download. The diff (step 3) reads both the
   clone root and the remote root through the lazy tree store, so it fetches only
   the tree objects of the subtrees that actually differ. Each changed/added
   blob's content is fetched on demand when it is applied (step 6).

6. Apply remote changes to the working tree:
   - For each path changed by the remote: write the new content to the working tree, then restore metadata (`mtime` and the executable-bit `mode`) from the remote tree entry. For a symlink, the link itself is recreated and its `mtime` is restored with `lutimes` (acting on the link, not its target); symlinks have no `mode`.
   - A remote change that only flips the executable bit is still a modification and is applied like any other (the content is rewritten and the new mode is set).
   - Local-only changes (paths not touched by the remote) are left untouched.
   - Paths deleted by the remote are deleted from the working tree, unless the local working tree has them modified (that would have been caught as a conflict in step 4).

7. Write the remote root hash to `.omemfs/clone_root`.

### Lazy tree reads (both clone root and remote root)

After a lazy, stub-aware clone (see `04_cli_spec.md`), the local `objects/`
cache does **not** contain the clone root tree objects of stubbed paths, nor the
remote root skeleton — only the objects needed to materialise sub-threshold
content were fetched. The pull diff, navigation, and conflict resolution read
tree objects on **both** sides, so a naive implementation would fault on
`ObjectNotFound` for a stubbed subtree.

Pull does **not** pre-download either tree skeleton. Pre-downloading the remote
root (or the clone root) would BFS-walk the entire tree and fetch essentially
the whole pack set on the first pull after a lazy clone, negating the lazy-clone
benefit. Instead, **both** the clone-root and remote-root tree reads are routed
through a **lazy tree store**: a read-through wrapper over the local cache that,
on a local miss, fetches the single missing tree object from the remote (via the
pack reader, decrypting it), writes it to the local cache as plaintext, and
returns it. Subsequent reads of the same object are served locally.

Because the tree diff compares two subtree hashes **before** reading either tree
object and returns immediately when they are equal (see the Diff algorithm
below), routing both sides through the lazy tree store fetches only the subtrees
that actually differ between the clone root and the remote root — i.e. exactly
the subtrees the diff descends into. A first pull of a small change therefore
reads on the order of the index root, the changed tree spine, and the changed
blob's pack (kilobytes), not the whole pack set.

Changed and added **blob content** is likewise fetched on demand: `apply_diff`
materialises each blob by calling the read-through fetch, which downloads the
blob (and any chunk manifest children) from the remote only when it is actually
written to the working tree. Sub-threshold blobs of a stubbed subtree the diff
never descends into are never fetched.

The same lazy tree store backs:

- the clone-root vs remote-root diff (remote changes),
- `navigate` along a scoped pull's path components into **both** the clone root
  and the remote root,
- `mark_deleted_tree` over a clone-root subtree deleted on the remote,
- the conflict-helper "base" side, which resolves a single clone-root blob hash,
- **both `push`'s and `pull`'s own working-tree scan** (`scan_and_store_with_cache`
  and, for a scoped pull's leaf path, `scan_file_as_tree`): a partial-expansion
  directory (`08_stub_system.md`, "Partial expansion") merges its `.omemfs-stub`
  record's recorded tree object with the real files found on disk, and that
  recorded tree object is a clone-root-rooted object subject to the exact same
  lazy-clone gap as any other clone-root read — it may never have been
  downloaded if the directory was stubbed as a whole. Routing the scan itself
  through the lazy tree store (not just the diff that runs after it) is what
  lets `push`/`pull` succeed the first time a user adds a file directly inside
  a stubbed directory, without requiring an intervening `expand`. `push_full`
  constructs a `PackReader`/lazy tree store for this purpose before scanning
  (previously it had no reader at all, only acquiring one later to upload);
  `push_scoped` moves its existing construction earlier, from just before the
  remote-divergence check to before the per-path scan loop. Constructing a
  `PackReader` for this scan and constructing the `PackWriter` used later for
  the actual upload/CAS are independent: the reader never reads the writer's
  CAS snapshot and vice versa (see "Push → Steps" below), so building the
  reader earlier introduces no ordering dependency on the writer,
- `ls`'s local (clone-root vs working-tree) diff (see `04_cli_spec.md`, "Local
  diff self-healing") — the one other command that reads clone-root tree
  objects the same way pull does. (`ls`'s own working-tree *scan* does not yet
  route the equivalent partial-expansion read through this store, unlike
  push/pull now do — see `04_cli_spec.md`, "Local diff self-healing", for why
  this is left as an open item rather than folded into this fix: `ls`'s design
  intent is to never block on the remote, and a scan's side effects are harder
  to cleanly abandon mid-way than a pure diff read's.)
- `restore`'s plan-phase walk of `clone_root` against the working tree (see
  `04_cli_spec.md`, "omemfs restore"), when that walk hits a clone-root tree
  object the local cache does not hold.

This is a shared abstraction (`tree_ops::LazyTreeStore`, `pub(crate)`), not
pull-exclusive: pull's own uses above are unbounded (pull's entire purpose is
to talk to the remote, so it always waits out a fetch), and push's scan use is
unbounded for the same reason (push already unconditionally needs a working
remote connection to complete at all), while `ls` wraps its use in an overall
timeout and falls back to a local-only, per-subtree-tolerant diff on timeout or
an unrecoverable remote error, since `ls` is meant to stay responsive rather
than block on, or abort because of, a slow or unreachable remote.

The bounded memory guarantee is preserved: the lazy store never holds a whole
skeleton in memory — each fetched object is cached to the local store as
plaintext and the in-memory footprint is a single object at a time.

#### Working tree scan optimization with a partially-stubbed clone root

The working-tree scan reuses stored clone-root tree-entry mtime/size to skip
re-hashing unchanged files (the mtime pre-filter above). This needs a flattened
map of clone-root blob entries (`flatten_tree_entries`), which walks the entire
clone root. Building it through the lazy tree store would re-introduce the full
clone-root fetch.

The scan only visits files that exist as **real files** in the working tree;
stubbed paths are not scanned. So the flattened map only needs entries for paths
that are materialised in the working tree. The flatten is therefore built lazily
through the lazy tree store but **bounded to the subtrees that exist on disk**:
at each clone-root tree node it recurses into a child subtree only when the
corresponding path is present in the working tree (a real file, a real
directory, or covered by a directory stub it is about to expand). A child that is
absent on disk, or is itself a stub, is skipped — its clone-root tree object is
never fetched. For a heavily stubbed clone this reads only the thin spine of
materialised directories, which is small. Correctness is unaffected: a path with
no flattened entry is simply hashed by the scan (the pre-filter is an
optimization, never a correctness requirement).

### Dirty pull (no conflict)

If the working tree has local changes that do not overlap with remote changes, pull proceeds and local changes are preserved:

```
Pulled remote changes. Your local modifications to the following paths were preserved:
  modified: notes/todo.md
```

---

## Conflict handling

A path matching `[line_merge]` (see `design/15_line_history_merge.md`) that is
modified on both sides is merged automatically instead of following the
conflict handling below: `pull` runs a three-way line merge and, on success,
writes the merged content directly to the working tree. The conflict handling
below is the fallback for such a path, used only when the merge detects a
genuine overlapping edit (design/15, "Conflict-fallback trigger") or a
precondition is not met (e.g. the path is not a plain-file modification on
both sides) — in either case the path is treated exactly like any other
conflicting path.

A conflict occurs when the same path is modified both locally (working tree vs. clone root) and remotely (remote root vs. clone root).

An executable-bit-only (chmod) change counts as a modification on either side. For example, a local `chmod +x` overlapping with a remote content change to the same path is a conflict. The one exception: when both sides hold the **same content hash**, the overlap is resolved without conflict by applying the remote metadata (mtime/mode).

A remote `AddedTree` (an auto-stubbed new directory, reported at the directory's own path — see "Diff algorithm" below) is also a conflict when the working tree has a local change at any path beneath it, even though the two sides' diff maps have no key in common: if the remote adds a large directory `d/` as a single `AddedTree("d")` while the working tree already has an untracked file `d/untracked.txt` (a local `Added("d/untracked.txt")`), pull detects this as conflicting on `d` rather than silently letting the `AddedTree` through. There is no meaningful per-content comparison to fall back on across a directory/file granularity mismatch.

The one exception is a local change beneath a `.git` entry (`d/.git/...`): a pre-existing local Git worktree at the `AddedTree`'s path is the case design/08 ("Stubs and Git repositories") already handles by materialising into it instead of stubbing, so it is not treated as a conflict. A local change beside the `.git` entry (`d/notes.txt`) is still a conflict.

### Conflict helper files

For each conflicting path `<path>`, pull writes three helper files alongside the original:

```
<path>.omemfs-conflict-base    — content at clone_root (last synced state)
<path>.omemfs-conflict-local   — content in the working tree (current local state)
<path>.omemfs-conflict-remote  — content at remote_root (latest remote state)
```

Example: if `src/main.rs` conflicts, the following files are created:

```
src/main.rs                          (unchanged — local content preserved)
src/main.rs.omemfs-conflict-base
src/main.rs.omemfs-conflict-local
src/main.rs.omemfs-conflict-remote
```

The original file (`src/main.rs`) is **not modified** — the local working tree content is preserved as-is.

When a conflict is detected, pull is **fully aborted**: no changes are applied to the working tree, and `clone_root` is **not** updated. Even non-conflicting remote changes are held back until all conflicts are resolved. This atomic-abort policy applies to all pull variants (full, single-path, multi-path).

`<path>` above is always the repo-root-relative path (`src/main.rs`), not a path relative to some scoped subtree. This matters for a multi-path scoped pull: each specified path's own diff maps are keyed relative to that path's own scoped subtree root, so pulling `a` and `b` together, a conflict on `f` under each is the key `f` in both `a`'s and `b`'s diff — but the conflict set pull tracks across all specified paths, and the helper files it writes, are keyed on the repo-relative path (`a/f` and `b/f` as two distinct conflicts), never on the bare scoped-relative key. See `04_cli_spec.md`, "Behaviour (path-scoped pull)", for the full detail.

### Advancing clone_root on resolution

Resolving a conflict with `omemfs conflict accept-local`, `accept-remote`, or `accept-base` advances `clone_root`'s entry for each resolved path to the value `pull` observed on the remote when it detected the conflict (spliced in per-path, the same way `pull` and a scoped `push` already splice individual paths into `clone_root` without touching unrelated entries — see "Path-scoped push" below). This applies uniformly whether the resolution targets the whole working tree or is scoped to a single file or subtree via `conflict accept-*`'s `<path>` argument, matching the granularity of the splice itself.

The advancement records that the resolution is a considered response to a specific, now-acknowledged remote state, not an unaware overwrite. It is what lets a subsequent `push` accept `accept-local`'s or `accept-base`'s content even though it deliberately differs from the remote's conflicting version: `push`'s CAS precheck (step 4b below) compares the remote's current value against `clone_root`, and after this advancement the two agree, so the push is recognized as an intentional supersession rather than a lost update. `accept-remote` needs no such help on its own — its content already matches the remote exactly, making the following push a safe no-op regardless (see step 4b) — but the same advancement is applied for consistency and so that a later `pull` also stops re-detecting the same conflict.

If the remote advances again before the next push or pull (e.g. someone else pushes while this conflict is being resolved), the newly-advanced remote no longer matches the value that was spliced in, and both `pull` and `push` correctly detect this as a *new* divergence, exactly as they would for any other stale `clone_root`. Protection against unaware, accidental overwrites is not weakened by this — only the specific paths a human has just looked at and resolved are advanced.

**Ordering guarantee.** `accept-*` computes the advanced `clone_root` and writes it durably *before* removing the conflict helper files and the `.omemfs-conflict-meta` sidecar for any path. Only once `clone_root` has been persisted does it delete the helpers/sidecar for the resolved paths. This means an interruption or failure partway through a multi-path `accept-*` — a process crash, a `write_clone_root` I/O error, or a tree object needed to splice a later path being unreadable — never leaves a path in the worst state: either `clone_root` was not advanced yet, in which case the helpers and sidecar are still present and `conflict list` still reports the path (a plain re-run of the same `accept-*` retries from scratch, which is safe: re-splicing the same recorded `remote_resolution` and re-writing the same accepted content are both idempotent); or `clone_root` was advanced and durably written, in which case deleting that path's helpers/sidecar afterward cannot un-advance it even if the deletion itself is interrupted (a leftover helper file with no matching entry in `conflict list`'s scan is harmless residue, cleanable by `conflict clean`). What never happens is the pre-fix ordering, where the helpers/sidecar (and with them the recorded `remote_resolution`) could be deleted while `clone_root` was still unadvanced — permanently losing the information needed to retry, and leaving a subsequent `push` failing CAS with no path listed as conflicting to explain why.

**Lazy tree reads.** Splicing or removing a path's entry in `clone_root` reads the intermediate clone-root tree objects along that path (`tree_ops::splice_entry` / `tree_ops::remove_entry`). After a lazy, stub-aware clone (`04_cli_spec.md`), those objects may not yet be in the local cache. `accept-*` routes these reads through the same lazy tree store described above ("Lazy tree reads (both clone root and remote root)") — a local miss is fetched transparently from the `origin` remote and cached, exactly as `pull` and `ls` already do for their own clone-root reads. A dry run makes no remote contact: it reports what would happen without touching `clone_root` at all, so no tree object needs to be read.

After writing conflict helper files, pull exits with a non-zero status and reports the conflicting paths. The next pull (after the conflicts are resolved) applies all pending remote changes at once.

```
Conflict: helper files written for the following paths:
  conflict: src/main.rs

Resolve conflicts and push:
  omemfs push           (after resolving conflicts)
  omemfs restore <path> (discard local changes)
```

### Missing sides

If a conflicting path was added by one side (absent at `clone_root`) or deleted by one side, the corresponding helper file for the absent side is not written. Only the sides that have content produce a helper file.

**Clearing stale residue before writing.** Before writing any of the (up to three) helper files and the metadata sidecar for a path, `pull` first removes every existing `.omemfs-conflict-*` artifact (all three helpers plus the sidecar, i.e. `conflict::CONFLICT_ARTIFACT_SUFFIXES`) for that exact path, then writes only the sides that currently have content. This matters because a conflict on the same path can recur: the user leaves an earlier conflict unresolved, the remote advances again in the meantime (e.g. the remote side that used to be a blob is now deleted, or vice versa), and the next `pull` re-detects the same path as conflicting with a different shape on one or more sides. Without clearing first, a side that is a "missing side" (no helper written) in the *new* conflict would silently keep the helper file left over from the *previous* one — stale content with no connection to the diff `pull` just computed. `conflict accept-remote`/`accept-local`/`accept-base` decide what to apply purely from "does the helper file for this side exist" (see "Advancing clone_root on resolution" above), so a stale helper is indistinguishable from a genuine one and gets applied as if it were current, silently reintroducing content the remote (or local, or base) side no longer holds. Clearing first closes this gap: a "missing side" in the new conflict is guaranteed to have no helper file, exactly as if this were the first time the path conflicted.

### Remote side is a symlink

A conflicting path's remote side is not always a blob or an absence: the remote can also have turned the path into a symlink (`DiffEntry::Symlink` in the diff `pull` computed against `clone_root`) while the local working tree independently modified the same path's content. This is a real, reachable conflict shape, not merely theoretical — a local edit to a plain file racing a remote `ln -s` (or `mv`-onto-symlink) at the same path reaches it directly.

`pull` writes no `<path>.omemfs-conflict-remote` helper file for this case (design/08_stub_system.md: a symlink's "content" is just its target string, not a blob suited to a diff-able helper file), matching the "Missing sides" rule above for a remote side with nothing blob-shaped to show. The metadata sidecar's `remote_resolution`, however, does record this shape explicitly (`RemoteResolution::Symlink { target, mtime }`, alongside the existing `Blob`/`Deleted`/`Unrecorded` variants) rather than falling back to `Unrecorded`, so `accept-remote` (and, for consistency, `accept-local`/`accept-base`) can still resolve the path correctly:

- `accept-remote` creates the symlink itself (`fsguard::write_symlink`, replacing whatever is at the path — file, dangling symlink, or a leftover empty directory — exactly as `pull`'s own `DiffEntry::Symlink` application does), restores its own `mtime` via `lutimes` (symlinks have no executable-bit `mode` to restore), and advances `clone_root`'s entry for the path to a `TreeEntry::Symlink` built from the recorded `target`/`mtime` — the same "splice the remote's exact recorded value" treatment `Blob` and `Deleted` already get (see "Advancing clone_root on resolution" above).
- `accept-local`/`accept-base` advance `clone_root` the same way (a `TreeEntry::Symlink` splice), since that advancement only needs to agree with what the remote actually holds, independent of which side's *content* ends up in the working tree.

Before this was recorded, `accept-remote` on this conflict shape deleted the file (the generic "no helper file for this side" branch) without ever creating the symlink, and left `clone_root` unadvanced (the `Unrecorded` fallback) — so the resolved working tree matched neither the remote (missing symlink) nor `clone_root` (still pointing at the pre-conflict state), and every subsequent `push`/`pull` kept failing: `push` saw a local delete that did not match the remote's symlink and was rejected by the CAS precheck (`clone_root` still disagreeing with the actual remote state), while `pull` re-diffed the same unresolved symlink change every time.

### Pruning directories emptied by conflict resolution

`omemfs conflict accept-remote`/`accept-local`/`accept-base` resolve one conflicting *file* path at a time. When the accepted side has no content for that path (e.g. `accept-remote` on a local-edit-vs-remote-delete conflict), the working-tree file is deleted (see "Missing sides" above). If that deletion leaves the file's parent directory (and, transitively, any now-empty ancestor directory up to but not including the repository root) with no remaining entries, `accept-*` removes those now-empty directories from the working tree as part of resolving the path.

This is deliberately different from `tree_ops::remove_entry`'s own policy (`design/01_object_model.md`, "Empty directories"): `remove_entry` keeps an intermediate tree that becomes empty as a genuine empty-tree object, because a directory a user created and pushed on purpose (with no files in it) must survive being spliced through unrelated deletions elsewhere in the tree — `remove_entry` has no way to tell that case apart from "this delete happened to empty out this directory". `accept-*`'s per-path resolution is a narrower context: it is unwinding the working-tree side of exactly one path that the remote has deleted (whole subtree or otherwise), so a directory left empty by that specific deletion is never one the user is deliberately keeping — it is scaffolding for the file that is being removed. Pruning it here keeps the working tree consistent with a directory that no longer exists at `clone_root`/`remote_root`, and prevents a later `push` from re-adding an empty directory that neither side actually has (see "Advancing clone_root on resolution" above: `clone_root`'s own entry for the path is already advanced/removed by the same `accept-*` call, so an unpruned empty directory on disk is the only remaining place the deletion could otherwise resurface from).

A directory that already existed as an intentionally-empty directory in the working tree *before* this resolution (i.e. it was empty for a reason unrelated to the path being resolved) is not a case `accept-*` can produce: the directory this pruning walk considers is always an ancestor of the one file path just deleted, and the walk stops pruning as soon as it reaches a directory that still has a remaining entry (its own `.omemfs-conflict-*` helper files for the same path are removed first, in the same `accept-*` call, before this walk runs — see "Advancing clone_root on resolution", "Ordering guarantee" — so they never keep a would-be-empty directory alive).

### Resolving a conflict

Option 1 — inspect helper files, edit the original, then push:

```bash
diff src/main.rs.omemfs-conflict-local src/main.rs.omemfs-conflict-remote
# Edit src/main.rs to the desired merged content
omemfs push     # push the resolved result
```

Option 2 — commit local changes first, then pull:

```bash
omemfs push     # commit local state
omemfs pull     # apply remote changes on top
omemfs push     # push the merged result
```

Option 3 — discard local changes, accept remote:

```bash
omemfs restore <path>   # discard local changes and remove helper files for the path
omemfs pull
```

---

## Diff algorithm

### Tree comparison

Two trees are compared recursively by comparing their hashes. The equal-hash
check happens **before** either tree object is loaded, so an unchanged subtree is
skipped without fetching it. This is what makes a clone-root-side read served by
the lazy tree store fetch only the differing subtrees:

```
diff(base_hash, target_hash):
    if base_hash == target_hash: return []   # subtree unchanged — no tree read
    base_entries  = load_tree(base_hash)
    target_entries = load_tree(target_hash)

    for each path in union(base_entries, target_entries):
        if path only in target: emit added(path)
        if path only in base:   emit deleted(path)
        if in both:
            if both are blobs and (hash differs or mode differs): emit modified(path)
            if both are trees and hash differs: recurse into diff(base[path].hash, target[path].hash)
            if type changed:   emit deleted(path) + added(path)
```

`emit deleted(path)` for a type change is not a single entry when the old
side at `path` was a tree: it expands to a `deleted` for every blob/symlink
reachable under that old tree (recursively, the same expansion `diff` already
performs for a path removed outright — "if path only in base: emit
deleted(path)" above, when `base[path]` is itself a tree). This matters
because a type change can go either direction:

- Old side is a tree, new side is a blob or symlink: every entry that used to
  live under the old tree must be reported `deleted` (not silently dropped),
  in addition to the single `added(path)` for the new blob/symlink. Otherwise
  the receiving side is left with an on-disk directory it can no longer make
  sense of, and applying the `added` blob on top of that directory fails.
- Old side is a blob or symlink, new side is a tree: the old blob/symlink
  itself must be reported `deleted(path)`, in addition to `added` for every
  entry of the new tree (recursively). Otherwise the receiving side keeps the
  stale file in place instead of removing it before the new directory is
  materialised.
- This holds identically when the new tree is large enough to be auto-stubbed
  (design/08_stub_system.md, "Threshold rules") instead of recursed into: the
  new side is then reported as a single `AddedTree(path)` rather than being
  recursed into. `diff` is keyed one entry per `path`, so this `AddedTree`
  necessarily lands at the very same key the old blob/symlink's
  `deleted(path)` was just reported at, and a second write to that key
  overwrites the first -- there is no way for both to survive as separate
  map entries. The `deleted(path)` emission is therefore for this one case
  informational only (it never reaches `apply_diff`); `apply_diff`'s
  `AddedTree` handling must be unconditionally self-contained about removing
  whatever currently occupies `path` (file, symlink, or stub) before
  creating the directory and writing the directory stub, the same way its
  `Symlink` handling already removes any stale stub/entry before
  materialising the link, rather than relying on a separate `Deleted` entry
  at the same path to have cleared it first.

#### Whole-root absence

The pseudo-code above assumes `diff` is called with two tree hashes already
known to exist, and only ever recurses into an entry missing on *one* side
*within* an otherwise-existing tree (`base[path]` or `target[path]`). A
caller may also need to diff two sides where the **entire root** being
compared is absent — not a path missing inside a tree, but there being no
tree at all on one side. `diff_trees` (`src/commands/pull.rs`) models this by
taking `base`/`target` as `Option<&Hash>` rather than `&Hash`:

- `target` absent, `base` present: every entry reachable from `base`
  (recursively) is `deleted`. This is the same expansion as "path only in
  base: emit deleted(path)" above, applied to the whole root instead of to
  one entry within a tree — `mark_deleted_tree` performs exactly this walk
  and is reused for it.
- `target` absent, `base` also absent: the diff is empty. Nothing existed on
  either side, so there is nothing to report.
- `target` present: proceed with the algorithm above (`base` absent is
  already handled by `load_tree(base_hash) = {}` when recursing).

This matters for path-scoped `pull`'s local-diff computation (see
"Path-scoped pull" below): when `<path>` still exists at `clone_root` but has
been removed from the working tree entirely, the working-tree side of the
scoped diff has no tree to navigate to (`target` is absent), and the correct
local diff is "every path under `clone_root`'s `<path>` was deleted locally"
— exactly the first case above — not an empty diff. Treating a wholly-absent
target as "nothing to compare, so no local changes" would hide a local
deletion from conflict detection: a full pull always has a working-tree root
hash to diff against (the working-tree scan always produces some hash, even
the empty-tree hash, for the top-level scan), so this whole-root-absence case
is specific to a scoped `<path>` that a full pull's own local diff never
encounters.

### Change application

```
apply_changes(base_tree, remote_tree, working_tree):
    diff = diff(base_tree, remote_tree)
    for each change in diff:
        if change is added:
            if path exists in working_tree and differs from remote → conflict
            else: write remote content to working tree
        if change is deleted:
            if path in working_tree has same hash as in base_tree → delete
            if path in working_tree has different hash → conflict
            if path already absent in working_tree → no-op
        if change is modified:
            if path in working_tree has same hash as in base_tree → overwrite with remote
            if path in working_tree has same hash as in remote_tree → no-op (already applied)
            if path in working_tree has different hash → conflict
```

### Write safety: the working-tree write boundary

No code may write, create, or unlink a working-tree path via a lexical `Path`. Every such operation goes through `src/fsguard.rs`, which resolves the path component-by-component from a `work_dir` directory fd using `openat(O_DIRECTORY|O_NOFOLLOW)` and then performs the operation `*at()`-relative to the resolved fd. A component strictly below `work_dir` that is a symlink is refused with `Error::SymlinkEscape`; `work_dir` itself and its ancestors are deliberately exempt (a legitimately symlinked or mounted `work_dir` must keep working). An unresolvable boundary is always an error, never a pass. Because the resolved fd *is* the fd the write uses, there is no check-then-use window.

This replaces an earlier lexical guard (`fsmeta::reject_symlink_escape`, since removed) that re-derived the boundary via `Repo::discover` and then walked the path a second time with `symlink_metadata` before the real write happened. That shape had three independent fail-open holes: `Repo::discover` itself calls `.is_dir()` (which follows symlinks), so a symlinked directory whose target happened to contain its own `.omemfs/` marker was mistaken for the repository root itself, collapsing the walk to nothing to check; a `Repo::discover` or `strip_prefix` failure returned `Ok(())` (permissive) rather than an error; and the check-then-write shape was a TOCTOU regardless of the first two holes. `fsguard` closes all three by construction: there is no separate boundary-discovery step to spoof, no permissive fallback branch anywhere in the module, and the fd that passed the check is the fd the write uses, so there is nothing to race.

Deletion and metadata mutation (chmod / mtime) go through the same primitive as content writes — `fsguard::remove_file_tolerant` and `GuardedDir::set_mode`/`set_mtime` — for the same reason: a lexically-planned delete or a path-based `chmod`/`utimes` follows a symlink exactly like a naive write would.

The boundary covers *deciding* whether to delete, not just the delete call itself. A plain `Path::is_dir()`/`Path::exists()` follows a symlink exactly like a naive write would, so any code that branches on "is this a directory I should recurse into and clear out" must ask that question through `fsguard` too, not through `std::fs`/`Path` directly — otherwise the write-side guarantee above is trivially bypassed by a caller that only ever *reads* through the symlink to decide what to delete, then deletes through a separately-resolved (and by then possibly raced) path. `fsguard::is_real_dir` (lstat-based, symlink-refusing at every component) answers that question; `fsguard::remove_dir_all` performs the matching guarded recursive delete, refusing if any component — including one discovered partway through the recursion — turns out to be a symlink.

The following call sites are all inside this boundary; each is either walking through `fsguard::GuardedDir`/`fsguard::remove_file_tolerant`/`fsguard::remove_dir_all`, or (scoped `push`) deciding whether to walk at all with `fsguard::is_real_dir` before reading any content into an upload:

- `omemfs stub`'s directory-stub deletion (`src/commands/stub.rs`): after writing the `.omemfs-stub` marker (see `design/08_stub_system.md`, "omemfs stub"), the directory's other contents are removed with `fsguard::remove_dir_all`, and the directory-vs-file decision at the top of `stub run()` uses `fsguard::is_real_dir` rather than `Path::is_dir()`.
- `omemfs restore`'s conflict-helper cleanup (`src/commands/restore.rs`, `remove_conflict_helpers`): removes the three conflict helper files and the metadata sidecar via `fsguard::remove_file_tolerant`, tolerating a helper that is already gone, instead of a bare `fs::remove_file` that both follows a symlinked ancestor and silently swallows every error.
- `omemfs conflict clean`'s helper-file and metadata-sidecar deletion (`src/commands/conflict.rs`, `run_clean`): removes each collected helper path, and each base path's `.omemfs-conflict-meta` sidecar, via `fsguard::remove_file_tolerant`.
- Path-scoped `push` (`src/commands/push.rs`, `push_scoped`): before scanning or reading a scoped path's content for upload, every path component from `work_dir` down to (and including) the scoped path itself is verified to be symlink-free via `fsguard::is_real_dir`/`fsguard`'s component walk, so a symlinked ancestor of the scoped path can never be traversed to read — and upload — content from outside `work_dir`.

Enumerating call sites is deliberately avoided as a *substitute* for the guarantee above — the guarantee comes from `fsguard` being the only available primitive for working-tree mutation and boundary-sensitive reads, not from an exhaustive list. The list above documents where each of the read/delete decisions is anchored, for reviewers checking this specific boundary; it is not a claim that no other call site matters. See `src/fsguard.rs`'s module doc comment for the primitive's exact contract (error policy, temp-file naming, and the `GuardedDir` API).

---

## Checking local changes

There is no separate `status` command. Use `omemfs ls --dirty` to see the diff between the working tree and the clone root (equivalent to `git status`):

```
$ omemfs ls --dirty
M  b2c3d4e5      512 2026-05-16 src/lib.rs
A  a1b2c3d4     1024 -          docs/new.md
D  -               - -          old/unused.rs
```

When the working tree matches the clone root exactly, `ls --dirty` produces no output.

---

## CAS safety for push

When two clients push concurrently, only one can win. omemfs uses a compare-and-swap write for the index root.

Both `push` and `pack` read the index root and CAS-write it through a single backend-pluggable **root-pointer abstraction** (the `RootPointer` trait). Neither command talks to the backend directly; they share the same primitive, so the read and CAS semantics are guaranteed identical between them.

The contract is built around an **opaque version token**. `read` returns the raw index-root bytes (absent → `None`) *together with* a `RootToken` capturing the version observed at read time (`RootToken::Absent` when the root did not exist). `cas_write` takes that token as its `expected` argument and only writes if the pointer's current version still equals it, mapping a mismatch to a CAS failure. The token is intentionally opaque: each backend decides what identifies a version. This is what lets a cloud backend (whose servers offer no byte-compare-and-swap) slot in unchanged — the condition is expressed in the provider's own terms (ETag, generation number) rather than as a byte comparison. The backend mappings are:

- Local directory: `read` is a plain file read (absent → `(None, RootToken::Absent)`), with the token being the stored bytes themselves (`RootToken::Present(bytes)`); this keeps behaviour byte-identical to a direct byte comparison. `cas_write` serializes the read-compare-write with an exclusive `flock(2)` on the fixed-name lock file `<prefix>/INDEX_ROOT.lock`, re-reads the current bytes under the lock, recomputes the token the same way as `read`, compares it to the expected token, and on a match writes via atomic rename. Only the client that holds the lock may update the index root. The lock file name is always `INDEX_ROOT.lock`, regardless of whether the repo is encrypted, because it is a transient coordination artifact and does not reveal which object is the root.
- S3 (implemented): `read` is a `GetObject` (404 → `RootToken::Absent`) that captures the object **ETag** as the token; `cas_write` is a conditional `PutObject` using `If-Match: <etag>` when `expected = Present(etag)`, or `If-None-Match: *` when `expected = Absent` (the root must not yet exist). A `412 Precondition Failed` maps to `Error::CasFailed`. No lock file is needed because the conditional write is atomic on the server side.
- Azure (implemented): `read` is a download / get-properties (404 → `RootToken::Absent`) that captures the blob **ETag** as the token; `cas_write` is a conditional upload using `.if_not_exists()` when `expected = Absent` and `if_match: <etag>` when `expected = Present(etag)`. A `412 Precondition Failed` (`err.http_status() == 412`) maps to `Error::CasFailed`. No lock file is needed.
- GCS (implemented): `read` captures the object **generation number** as the token; `cas_write` uses `set_if_generation_match(<generation>)` when `expected = Present(generation)`, or `set_if_generation_match(0)` when `expected = Absent`. A `412 Precondition Failed` (`err.http_status_code() == Some(412)`) maps to `Error::CasFailed`. No lock file is needed.

ETag and generation number are different concepts that differ between providers; the token is opaque precisely so each backend supplies its own version identity without changing the call sites.

The CAS ensures that the index root is only updated if its version still equals the token observed before uploading (the expected token is captured at the start of the operation, never re-derived at finish time). If the condition fails, the push is rejected and the user must pull first.

**What the CAS does and does not guarantee.** The CAS by itself guarantees only *atomicity* of the index-root update relative to the token observed at push start: it prevents two pushes from both succeeding when their upload windows overlap, and it prevents this push from silently clobbering a change that landed *during* its own window. It does **not**, by itself, provide fast-forward-only / lost-update protection across the whole operation: if the remote had already advanced *before* this push started, the token read at push start already reflects that advanced state, so the CAS's precondition (token unchanged between start and finish) is trivially satisfied and the CAS has nothing to catch — this push would sail through and silently overwrite the intervening change. That guarantee is provided by a separate, up-front check: the whole-root divergence check (step 4b under "Push → Steps" above) for a full push, and the per-path divergence check below for a scoped push. The CAS's job is to close the much narrower residual window — a concurrent push whose own start-to-finish window overlaps this one — that the up-front check, being a single point-in-time comparison, cannot cover.

The index root location is determined by the encryption mode:

- **Encrypted remote**: the derived key `objects/<HMAC-SHA256(DEK, "omemfs:index-root:v1") hex>` under the same fixed 3-level sharding as content objects.
- **Unencrypted remote**: the fixed key `<prefix>/INDEX_ROOT`.

All CAS semantics are identical in both cases; only the storage path differs.

### Parallel object PUTs and the single CAS

When `OMEMFS_TRANSFER_CONCURRENCY > 1`, the object uploads in push step 5 run on multiple worker threads against the shared `&dyn ObjectStore`. This does not weaken the CAS guarantee: object writes are content-addressed and idempotent (writing the same hash twice produces identical bytes), so they need no ordering or mutual exclusion between workers. Only the **index root** is mutable, and it is written exactly once — by a single `cas_write` in `finish()`, after every parallel object PUT has completed and the worker threads have joined. The expected version token is the one captured at the start of the push (step 4), never re-derived at finish time. Concurrency therefore affects only the speed of the object-transfer phase; the "parallel object PUTs land first, then a single CAS in `finish()`" structure is what keeps the one-winner concurrent-push semantics identical across all four backends and all concurrency levels.

---

## Path-scoped push and pull

```
omemfs push <path>
omemfs pull <path>
```

Path-scoped operations apply only to the specified path (file or directory subtree). Changes outside `<path>` are neither read nor written. The principle is symmetric for push and pull:

> **For both push and pull: `<path>` is updated to the new state; everything outside `<path>` retains its current value in clone root.**

### Path-scoped push

1. Scan `<path>` in the working tree and build a new subtree. Before reading any content, every path component from `work_dir` down to (and including) `<path>` is verified to be symlink-free (see "Write safety: the working-tree write boundary" above) — a symlinked ancestor is refused rather than followed, so a scoped push can never read (and upload) content that actually lives outside `work_dir` by walking through a symlink planted along `<path>`.

    **Deletion detection.** A scoped push must tell "`<path>` was intentionally removed from the working tree" apart from "`<path>` merely could not be observed", since only the former is safe to splice into the remote as a deletion. `<path>` is treated as a deletion intent **only** when a direct `symlink_metadata` lookup on it reports `io::ErrorKind::NotFound` (nothing exists there at all — not even a symlink or a stub marker). Every other case that keeps the working tree from being read is a hard error that aborts the whole push before any object is uploaded or the index root is touched — the remote is left exactly as it was:
    - `<path>` itself is a stub (file or directory): this is not a deletion at all — handled separately by splicing the stub's recorded `TreeEntry`, never treated as absence (design/08_stub_system.md, "Interaction with push").
    - `<path>` sits below a directory that is itself dir-stubbed (an ancestor of `<path>`, not `<path>` itself, carries a `.omemfs-stub` marker): the path is unreachable, not confirmed absent — the ancestor's real content has been deferred, and `<path>` may or may not exist inside it. Push aborts with an error rather than guessing.
    - `symlink_metadata` fails with any `io::ErrorKind` other than `NotFound` — most importantly `PermissionDenied` (an unreadable ancestor or the path itself, e.g. `EACCES`) — leaves existence genuinely unknown; push aborts rather than assume deletion.
    - A symlink at `<path>` whose target does not exist ("dangling symlink") is not a deletion either: `symlink_metadata` (which does not follow the final symlink) reports the link itself as present, so this case is naturally excluded from the `NotFound` test above and is scanned like any other symlink entry.

1c. **Unchanged-path skip.** After scanning `<path>` (step 1), a path whose freshly-scanned entry already matches `clone_root`'s recorded entry at that same path is skipped entirely — it needs no splice, no upload, and does not count toward "something to push" (mirroring step 3's whole-tree "nothing to push" check, scoped to this one path). For a blob leaf, "matches" means both the content hash **and** the executable-bit `mode` are equal — same rule as "Diff output" above (a mode-only change, e.g. `chmod +x`, is a real modification even when the hash is unchanged) and as `ls --dirty`'s dirty detection. Comparing only the hash here would silently treat a mode-only change as "nothing to push" and, if a remote-side mode-only change to the same path exists, would let this push's splice overwrite it without ever reading or preserving it. For a directory subtree, "matches" is a single tree-hash comparison, since `mode` is not a field of `TreeEntry::Tree` and any mode change to a file inside the subtree already changes that subtree's hash.

2. Read the index root and extract the current remote root hash; record the raw index root bytes for the CAS check.
2b. Per-path divergence check (the scoped analogue of step 4b above): for each pushed `<path>`, compare the remote's subtree at `<path>` — as observed in the snapshot read in step 2 — against `clone_root`'s subtree at the same `<path>`. If any pushed path's subtree differs, fail immediately with the same "remote has been updated since last sync" error, scoped to that path, without uploading any objects or touching the index root. Remote changes **outside** every pushed `<path>` must **not** block the push — they are preserved by the splice logic below, which only ever touches the pushed paths and leaves the rest of the remote root tree untouched.

    The same "no real divergence" exceptions as step 4b apply per path, checked first:
    - If `<path>`'s freshly-scanned new subtree already matches the remote's current subtree at `<path>`, the push proceeds for that path regardless of `clone_root`'s staleness there — same rationale as step 4b (a push right after `omemfs conflict accept-remote <path>` for a scoped path).
    - Deleting a `<path>` that is already absent on the remote is always allowed, matching the existing "already absent on remote" no-op handling below — there being nothing there means there is nothing to lose, regardless of what `clone_root` still records for it.
    - The empty-root normalization from step 4b (a missing path on either side, an absent `clone_root`, and the empty-tree hash all equivalent to "nothing here yet") applies wherever a whole root is being compared.

    When `<path>` names a blob leaf directly (not a directory), "the remote's subtree at `<path>`" and "`clone_root`'s subtree at `<path>`" must each be compared by content hash **and** `mode` together, for the same reason as step 1c: a `Blob` entry's own `hash` field is its content hash alone and does not change when only its `mode` changes, so a hash-only comparison here would miss a remote-side or clone-root-side mode-only change to the exact leaf being pushed. This does not apply when `<path>` names a directory: a `Tree` entry's hash already covers every descendant's `mode`, so a tree-hash comparison alone is sufficient there.
3. Splice the new `<path>` subtree into a copy of the **remote root tree** (use the remote root as the base for the rest of the tree). Compute the new root tree hash.
4. Upload objects missing from the remote (only those reachable from the new `<path>` subtree).
5. Update the index root with a CAS conditioned on the raw bytes read in step 2.
6. Update `clone_root` by splicing the new `<path>` subtree into a copy of the **current clone root tree** (use the clone root as the base for the rest).

Step 1's working-tree scan of `<path>` uses the same mtime pre-filter described
under "Working tree scan optimization" above, which needs a flattened map of
clone-root blob entries. For a scoped push this map is built **only from the
clone root's `<path>` subtree**: the clone root is navigated along `<path>`'s
path components — reading only that spine of tree objects — and the subtree at
the end of the spine is then flattened, with map keys carrying the full
repo-relative prefix (e.g. `blog/post.md`, not `post.md`) so they match the
scan's `rel_path` lookups. Clone-root tree objects outside `<path>`, other than
the navigated spine, are never read. If `<path>` does not exist in the clone
root, the map is empty; if `<path>` resolves to a single blob or symlink, the
map has exactly one entry. As with the partially-stubbed-clone-root case above,
this bound is purely an optimization, not a correctness requirement: a path
missing from the map is simply re-hashed by the scan. Scoped push variants
that never scan a directory — deleting a path, splicing in a stub, or pushing
a single file — build no flatten map at all.

This flatten never crosses a stub boundary: if `<path>`'s own clone-root entry
is a `Tree` whose object is not present in the local cache (`<path>` itself is
a whole-stubbed directory a lazy clone never downloaded — `08_stub_system.md`,
"Stub threshold"), it is not descended into, matching the same guard the
whole-tree scoped flatten's recursive step already applies to every deeper
subtree it walks. Before this guard existed (D15), scoping a push directly at
a stubbed directory (`omemfs push <stubbed-dir>`) unconditionally tried to
read that directory's tree object for the pre-filter map and failed with
`ObjectNotFound`, even though pushing the same content via an ancestor scope
or a whole-tree push never hit this failure — the map miss this guard produces
instead is always safe, exactly as it is for every other case above.

The scan itself (step 1) reads a partial-expansion directory's `.omemfs-stub`
record (`08_stub_system.md`, "Partial expansion") through the same
`LazyTreeStore` `pull`'s working-tree scan uses (see "Lazy tree reads" above)
rather than the local-only store: that record's `hash` field may name a tree
object never downloaded by a lazy clone. This is deliberately a SEPARATE store
argument from the plain local store used for everything else in the scan
(writing new objects, and every `.exists()`-based local-presence decision,
most importantly the sealed-blob verification a blob-writing scan performs) —
routing those through the same remote-fetching store would silently launder a
genuine local miss (e.g. a blob deleted from the local cache that must be
recaptured) into a false "still present".

A single-path scoped push also loads STAT_CACHE in scope-limited form
(`read_scoped` / `write_scoped_merge`, restricted to `<path>`'s prefix); a
multi-path scoped push falls back to a full STAT_CACHE read. See
`07_stat_cache.md`, "Read optimisation: scope-limited load", for details.

After the operation:
- The remote root (inside the index root) reflects the new `<path>` on top of whatever was already on the remote.
- `clone_root` reflects the new `<path>`; paths outside `<path>` remain at their previous clone root state.
- Any remote changes outside `<path>` that existed before the push are still visible as a diff between the remote root and `clone_root`, and will be processed by the next full or path-scoped pull.

### Path-scoped pull

1. Read the index root and extract the remote root hash.
2. Compare the remote root's `<path>` subtree against the clone root's `<path>` subtree. If they are equal, exit (already up to date for `<path>`).
3. Compare the working tree's `<path>` against the clone root's `<path>` to detect local changes. If `<path>` is entirely absent from the working tree (removed since the last sync) while the clone root's `<path>` still has content, this is a local deletion of everything under `<path>` — see "Whole-root absence" above — and must be diffed as such, not as an empty local diff, so that a remote change to a path local deleted is still detected as a conflict in step 4 (matching a full pull's own handling of a locally-deleted path).
4. Check for conflicts within `<path>` (same logic as full pull). If any conflict is found, abort.
5. Apply remote changes within `<path>` to the working tree. Paths outside `<path>` in the working tree are untouched.
6. Update `clone_root` by splicing the remote root's `<path>` subtree into a copy of the **current clone root tree**. Paths outside `<path>` in clone root are unchanged.

When `<path>` names a regular file or symlink, it is a leaf entry rather than a
subtree. Its add, modification, or deletion is applied only to that entry, and
the corresponding clone-root leaf is replaced or removed. If that leaf has a
whole-file conflict, its conflict helper files are written alongside the leaf
itself, exactly as for a full pull; the file-scope path is not prefixed again.

**Leaf-scope type changes.** Whether `<path>` is treated as a leaf entry
depends on the entry *kind* on each side (remote root's `<path>` and clone
root's `<path>`), not on which side happens to be present. `<path>` is a leaf
scope whenever the remote-side entry, the clone-root-side entry, or both, is
a blob or symlink (i.e. whenever at least one existing side is *not* a tree).
This includes the case where the two sides disagree in kind — e.g. `<path>`
was a regular file at the last sync and is now a directory on the remote (or
vice versa). Such a type change is diffed the same way an unchanged-kind leaf
is: both sides are wrapped in a synthetic one-entry tree (so `<path>`'s own
name is the sole entry) and compared with the ordinary tree-diff algorithm,
which already has a rule for a same-path kind change ("Diff algorithm",
above) — the old side's full content is reported `deleted` (expanded
recursively when it is a tree) and the new side's content is reported
`added` (recursed into when it is a tree, subject to the same stub
threshold). Only when the entries present on *both* sides that exist are
trees is `<path>` treated as an ordinary subtree, compared by navigating each
side to `<path>`'s tree hash directly (no wrapper needed). Treating any
non-tree side as sufficient to select the leaf/wrapper path (rather than
requiring agreement between the two sides) is what lets a type change be
diffed at all: navigating both sides to `<path>` and diffing the two hashes
directly, as if both were tree hashes, fails once one of them is actually a
blob hash, since the tree-diff algorithm reads its `base`/`target` hashes as
tree objects unconditionally.

The working tree's on-disk entity at `<path>` (used for the local-change
check in step 3) is wrapped the same way, by its OWN actual on-disk kind --
which need not match the leaf/subtree classification above, since that
classification is driven only by the remote and clone-root sides. When
`<path>` is a leaf scope (by the rule above) but the working tree still has a
real directory on disk at `<path>` (e.g. the type change has not yet been
pulled locally either), that directory is scanned as an ordinary subtree and
the resulting tree hash is wrapped in the same one-entry synthetic tree,
rather than being read as file content.

After the operation:
- The working tree's `<path>` matches the remote root.
- `clone_root` reflects the remote state for `<path>`; everything outside `<path>` retains its previous clone root state.
- Any remote changes outside `<path>` remain as a diff between the remote root (from the index root) and `clone_root`, visible to the next full or path-scoped pull.

### Invariant

Both operations maintain the following invariant:

> `clone_root` records the per-path sync boundary. A path that has been synced (pushed or pulled) is reflected in clone root at the remote version. A path that has not yet been synced retains its previous clone root value, and the diff between clone root and remote root correctly describes the pending remote changes for that path.

Step 2b above is what enforces this invariant on the push side: it refuses to advance a path's remote state unless this clone's `clone_root` for that exact path is still the value the remote agrees with, so a scoped push can never silently overwrite a remote change to a path it did not itself observe.

---

## Post-clone sync guard

`pull` and `push` enforce the following invariant when accessing the remote index root:

> If the local clone root is **not** the empty-tree hash and the remote index root object is absent, the operation fails with a hard error. An absent index root is interpreted as "empty remote" only when the clone root equals the empty-tree hash (i.e. the local clone has never successfully synced any content).

The guard applies to the **origin** remote only. A backup push always treats an absent index root on the backup remote as "create on first push": the backup remote has no sync baseline (`clone_root` tracks origin, and backup is write-only), and its new/existing status is validated once at `config add-backup` time instead.

**Error messages**

For push:
```
error: index root not found on remote
The remote appears empty, but this clone has sync history.
Possible causes: wrong encryption key, wrong URL/prefix, or the remote was reset.
```

For pull:
```
error: index root not found on remote
The remote appears empty, but this clone has sync history.
Possible causes: wrong encryption key, wrong URL/prefix, or the remote was reset.
```

**Rationale**: with a derived index root key (encrypted remotes), a wrong DEK and a genuinely empty remote are indistinguishable — both result in a key-not-found response. Silently treating a missing index root as an empty remote after content has been synced would corrupt the clone (overwriting `clone_root` with the empty-tree hash on pull, or pushing into what appears to be a brand-new repo on push). The guard detects wrong-key and divergence scenarios without requiring a marker file.
