# Storage Format and Codec

## Local repository layout

```
<working tree>/
└── .omemfs/
    ├── config          # repository configuration (JSON)
    ├── clone_root      # tree hash of the last successfully synced state (text)
    ├── objects/        # local object cache (adaptive depth — see objects/ section)
    ├── packcache/      # local cache of whole remote pack files (raw/encrypted, content-addressed by pack_hash)
    ├── objcache/       # local cache of decrypted remote index files (plaintext, content-addressed by index hash)
    └── stubs/          # stub records for deferred files (mirrors working tree path layout)
```

The `packcache/` directory holds raw remote pack files (encrypted, exactly as stored on the remote backend). It is populated on demand: the first slice read of a pack fetches the whole pack once and streams it into `packcache/<pack_hash>`. Subsequent slice reads in the same run, and in all future runs, are served from this local copy without any remote GET. Because packs are content-addressed by their `pack_hash` (= SHA-256 of `ED E1 || body`), the cache is immutable and requires no invalidation. An orphaned pack left after remote consolidation is simply never referenced again and causes no staleness. The directory is safe to delete at any time; it will be repopulated on the next read. Growth is unbounded in the current implementation; pruning is future work.

The `objcache/` directory holds remote index files (delta / hot / cold shards, remote magic `ED E2`) cached as **plaintext** after a single decryption on first fetch. It is the plaintext counterpart of `packcache/`: both cache content-addressed, immutable L6 remote objects, but `packcache/` stores raw encrypted bytes (pack slices are decrypted downstream per-object) while `objcache/` stores the whole-file-decrypted index bytes so that the frequent `exists()` dedup checks on push — and the index lookups on read — never re-decrypt. Index files are content-addressed by their logical hash, so a cache entry is never stale; an index superseded by `pack` consolidation simply becomes unreferenced and is never read again. The directory is safe to delete at any time and is repopulated on the next read/push. Growth is unbounded in the current implementation; pruning is future work.

Index files are deliberately kept **out of** `objects/`: that cache holds only logical objects (blobs, trees, chunk manifests/bodies). Keeping the L6 index bytes in a separate `objcache/` lets `omemfs stats` treat any `ED E1..EF` object found under `objects/` as genuinely anomalous (corruption / misplacement), and keeps the "Local cache composition" breakdown free of pack-layer noise. (Repositories created by an earlier build that cached index files under `objects/` will still show those as `unknown` in stats; they are unreferenced and safe to delete, or disappear on a fresh clone. Pre-production, no migration is performed — see STATUS.md.)

## config

Repository settings in JSON.

```json
{
  "version": "2.0",
  "remotes": {
    "origin": {
      "type": "s3",
      "bucket": "my-primary-bucket",
      "region": "ap-northeast-1",
      "prefix": "omemfs-repo",
      "access_key_id": "AKIA...",
      "secret_access_key": "...",
      "encryption": {
        "algorithm": "aes-256-gcm",
        "dek": "<base64-encoded 32 bytes>"
      }
    }
  }
}
```

Supported remote types: `local`, `s3`, `gcs`, `azure`.

The `remotes` object recognises exactly two fixed keys:

- `"origin"` — the primary remote, used by all push and pull operations.
- `"backup"` — the secondary remote, used only for backup pushes (`omemfs push --with-backup`). Pull from backup is not supported.

Both keys are optional. A repository with no remotes configured is valid (useful for local-only operation or before `clone` completes setup).

### Encryption configuration

When encryption is enabled for a remote, an `encryption` field is added **inside that remote's config object**. The DEK (data encryption key) is a randomly generated 32-byte value stored as base64.

```json
{
  "version": "2.0",
  "remotes": {
    "origin": {
      "type": "s3",
      ...
      "encryption": {
        "algorithm": "aes-256-gcm",
        "dek": "<base64-encoded 32 bytes>"
      }
    },
    "backup": {
      "type": "s3",
      ...
      "encryption": {
        "algorithm": "aes-256-gcm",
        "dek": "<base64-encoded 32 bytes — different from origin's DEK>"
      }
    }
  }
}
```

Each remote has its own independent DEK. The DEK is generated at clone time and stored in config because the local filesystem is assumed to be under the user's control — only objects in the remote backend need protection.

A remote without an `encryption` field stores objects unencrypted.

To keep DEKs out of version control, the `.omemfs/` directory must never be committed or shared. The config file must have permissions `0600`.

### Remote type: local

```json
{
  "type": "local",
  "path": "/path/to/remote/dir"
}
```

### Remote type: s3

```json
{
  "type": "s3",
  "bucket": "my-bucket",
  "region": "ap-northeast-1",
  "prefix": "omemfs-repo",
  "access_key_id": "AKIA...",
  "secret_access_key": "...",
  "endpoint": "https://minio.local:9000",
  "force_path_style": true
}
```

`access_key_id` / `secret_access_key` are optional; when both are omitted the AWS default credential chain is used. `endpoint` and `force_path_style` are optional and used only for S3-compatible stores (e.g. MinIO) — `endpoint` overrides the service URL and `force_path_style: true` selects path-style addressing.

### Remote type: gcs

```json
{
  "type": "gcs",
  "bucket": "my-gcs-bucket",
  "prefix": "omemfs-repo",
  "project_id": "my-project",
  "credentials_json_path": "/path/to/service-account.json",
  "endpoint": "http://localhost:9000"
}
```

Authentication uses a service-account JSON key, supplied either as a file path (`credentials_json_path`) or inline (`credentials_json`). When neither is set, Application Default Credentials (ADC) are used; against the storage-testbench emulator, anonymous access is used. `project_id` and `endpoint` are optional (`endpoint` points at a non-Google endpoint such as the test emulator).

### Remote type: azure

```json
{
  "type": "azure",
  "container": "my-container",
  "prefix": "omemfs-repo",
  "account": "myaccount",
  "tenant_id": "...",
  "client_id": "...",
  "client_secret": "...",
  "endpoint": "https://myaccount.blob.core.windows.net"
}
```

Azure authenticates with **Entra ID (Azure AD) only** — via a `TokenCredential` built from `tenant_id` / `client_id` / `client_secret` (a `ClientSecretCredential`). Account keys and SAS tokens are **not** supported. `endpoint` is optional and overrides the default blob service URL (`https://<account>.blob.core.windows.net`).

### Credential management

All credentials (access keys, DEKs) are stored directly in `config`. The file must have permissions `0600`. Do not commit `.omemfs/` to version control.

Types that hold credentials (`EncryptionConfig`, `RemoteConfig`, and therefore
`Config`) implement `Debug` by hand and print `<redacted>` in place of every
secret field (`dek`, `access_key_id`, `secret_access_key`, `client_secret`,
`credentials_json`). Non-secret fields (bucket, region, prefix, endpoint,
`credentials_json_path`, ...) are printed normally, so `{:?}` stays useful for
diagnostics without ever writing a credential to a log, panic message or
terminal. Serialisation to `config` is unaffected.

### Local directory permissions

`.omemfs/` holds the DEK, the object cache and the logs, so it is private to
the owner:

- `.omemfs/` and `.omemfs/logs/` are created with mode `0700`. Because
  `.omemfs/` itself is `0700`, everything beneath it (`objects/`, `tmp/`, ...)
  is unreachable by other users whatever the modes of the inner directories;
  those inner directories are deliberately left to the umask, since the same
  object-store code also manages shared local-directory remotes, which must
  stay group-accessible.
- Files omemfs creates directly under `.omemfs/` (`config`, `clone_root`,
  `clone_root.lock`, `io_stats.jsonl`, `stat_cache`) and log files are created
  with mode `0600`.

The mode is applied at creation, independent of the umask, and is not
re-applied to an already existing directory or file (a repository created by an
older version keeps its existing modes; the owner may `chmod` it).

Files written through the atomic temp-file-and-rename helpers (`config`,
`clone_root`, `stat_cache`, cached objects) get `0600` from `tempfile`; the lock
file, `io_stats.jsonl` and log files, which are opened directly, request `0600`
explicitly. A **local-directory remote** belongs to the remote, not to
`.omemfs/`, and is not covered by this section. Files materialised into the
**working tree** are likewise not covered; their permissions come from the
recorded mode (see `03_sync_model.md`).

To share a repository config with another machine, use `omemfs config export` to produce an `omemfs_repo_` connection string, and provide that string when prompted during `omemfs clone` on the target machine.

## clone_root

A single line containing the tree hash (64 hex characters) of the last successfully synced state.

```
a3f89b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a
```

This is the local side of the 3-way comparison: working tree vs. clone root vs. remote root.

When the repository has never been synced (immediately after `clone` from an empty remote), this file contains the hash of an empty tree.

## Stub files

When a file is deferred during `clone` or `pull` (its size meets or exceeds the stub threshold), the working tree file is **not** written to disk. Instead, a stub record is written alongside the original path in the working tree:

- **File stub**: `<path>.omemfs-stub` next to where the file would be.
- **Directory stub**: `<dir>/.omemfs-stub` inside the directory (which is created as an empty directory to hold the stub marker).

A stub record is a JSON object containing enough metadata for the scan step to reconstruct the correct tree entry and for `omemfs expand` to download and materialise the file:

```json
{"target_type":"blob","hash":"a1b2c3...","size":5242880,"mtime":"2026-03-07T10:30:45Z"}
```

Fields:
- `target_type`: `"blob"` (file) or `"tree"` (directory)
- `hash`: object hash (blob or tree)
- `size`: file size in bytes for blobs; total descendant blob size for trees
- `mtime`: last-modified time (ISO 8601 / UTC)
- `mode`: `"755"` if the owner execute bit is set; omitted otherwise
- `blob_count`: number of descendant blobs (tree stubs only; omitted when 0)
- `chunked`: `true` when the blob was stored as chunked objects; omitted otherwise

When `omemfs expand` materialises the file, the stub record is deleted.

For a full description of the stub system (threshold rules, stub invariant, partial expansion, reconcile, Git repository constraints), see [`08_stub_system.md`](08_stub_system.md).

## objects/ directory

Objects are stored by their SHA-256 hash under `objects/` using an **adaptive depth** sharding scheme.

### Depth and layout

Each directory manages its own depth independently, stored in a `.depth` file inside that directory. The depth starts at 0 (flat layout) and increments when the file count in a directory exceeds 1 000.

| Depth | Layout                                     | Example                                      |
|-------|--------------------------------------------|----------------------------------------------|
| 0     | `objects/<64-char-hash>`                   | `objects/178b5fbed164...`                    |
| 1     | `objects/<2>/<62-char-hash>`               | `objects/17/8b5fbed164...`                   |
| 2     | `objects/<2>/<2>/<60-char-hash>`           | `objects/17/8b/5fbed164...`                  |

### Migration

When a directory's file count crosses 1 000, a `.migrating` marker is written first and existing flat files are moved atomically into two-char subdirectories. If the process crashes mid-migration, the next write call resumes from where it was interrupted. After all files are moved, `.depth` is incremented and `.migrating` is removed.

Each two-char subdirectory is an independent shard that tracks its own depth. So `objects/17/` may be at depth 1 while `objects/ab/` is still at depth 0.

The `objects/` directory is a **local cache** of remote objects. Any object present in the remote backend can be safely deleted locally and re-fetched on demand.

**Future work**: each `exists()`/`find_in` call re-reads the target directory's `.depth` marker and probes `.migrating` freshly (`src/store/objects_dir.rs`), which is several filesystem syscalls per call rather than one. An in-process per-directory memoisation of these two markers (invalidated when a migration is observed) would reduce this further, independently of the STAT_CACHE seal work above — not designed or implemented as part of this pass; see STATUS.md.

## Remote backend layout

Remote backends (local directory, S3, GCS, Azure) use a common key structure under `<prefix>/`. The exact layout depends on whether encryption is configured.

**Encrypted remote**

```
<prefix>/
└── objects/
    └── <2>/<2>/<2>/<58-char-hash>   # fixed 3-level sharding; includes content objects and the index root
```

The index root is stored as a regular object under `objects/` at a derived key (see [Index root name derivation](#index-root-name-derivation) below). It is indistinguishable from content objects by key alone.

**Unencrypted remote**

```
<prefix>/
├── objects/
│   └── <2>/<2>/<2>/<58-char-hash>   # fixed 3-level sharding; content objects only
└── INDEX_ROOT                        # fixed-name root pointer
```

The local-directory backend also writes a lock file `<prefix>/INDEX_ROOT.lock` for CAS serialisation; the lock file is never present on S3/GCS/Azure backends (they use conditional writes instead — see `12_locking.md`).

### Parallel transfer and memory

The object-graph transfer loops (`push`'s upload-of-missing and the shared `transfer_objects` copy, which also serves `clone`'s per-file fetch and `expand`) may run in parallel: `OMEMFS_TRANSFER_CONCURRENCY` worker threads share the single `&dyn ObjectStore` (the trait is `Send + Sync`). The default is `1` (serial) for the local backend and `8` for cloud backends; set the variable to override. Parallelism is confined to these per-object transfer loops — it never changes the object format and never affects the single index-root CAS, which still runs once in `finish()` after all object writes have joined (see `03_sync_model.md` and `12_locking.md`). The working-tree materialisation phases of `clone`/`pull` stay serial (filesystem ordering), but their remote object fetches still go through the parallel `transfer_objects` path.

**Multi-root batching (Improvement B)**

`parallel_bfs` walks the object graph reachable from a **set** of root hashes, not just one: the shared work queue is seeded with all roots up front and the initial outstanding-work count is `roots.len()`. Everything else — the shared `visited` set for dedup, the outstanding-count termination signal, first-error-wins — is unchanged and already handles roots that share subtrees correctly (a hash claimed via one root is not re-walked via another). `transfer_objects_many` in `src/commands/push.rs` exposes this as a multi-root counterpart to `transfer_objects`, taking a slice of root hashes instead of one.

Read-side commands use an explicit **Plan → Fetch → Apply** split. Planning
consults only the bare local cache: a hash that is resolvable through a
read-through remote store is still missing locally and must remain a fetch
root. Fetch transfers the de-duplicated roots from the snapshot reader into
that bare local cache in one graph walk. Apply then materialises working-tree
paths from the local cache, with a read-through fallback only for entries whose
stub-visibility decision could not safely be made during planning. This keeps
"cached locally" distinct from "readable remotely" and prevents remote
membership checks from accidentally emptying a fetch batch.

`pull` plans materialised blobs and eligible added trees as roots. A tree root
lets the transfer walker discover its child trees, manifests, and chunks.
`expand` uses a parallel tree-only planner: sibling tree objects are fetched
and decoded concurrently, while the planner adds only blobs below the configured
stub threshold to the subsequent multi-root fetch. Entries whose Git visibility
is only knowable while applying remain on the existing on-demand fallback path.

This matters because per-root granularity, not just per-object granularity, determines how much parallelism is actually available to exploit. A BFS seeded with a single root has no parallel work until that root's own children are discovered — for a root that is itself one leaf blob (any file below the CDC `min_size` threshold of 1 MiB — see "Object routing (write path)" above), the BFS is a walk of exactly one node, so `workers` threads have nothing to divide between them regardless of how high `OMEMFS_TRANSFER_CONCURRENCY` is set. `expand` and `pull` were calling `transfer_objects` once per blob in a sequential loop across sibling files, so a tree of many small files paid for a full worker pool that never had more than one node of real work in flight at a time — the *outer* loop across files, not the *inner* per-object BFS, was the actual bottleneck, and the outer loop had no concurrency at all. Seeding one `transfer_objects_many` call with every blob hash that needs fetching turns that outer loop into work the shared queue can distribute across all `workers` threads from the start, so the existing cloud-backend concurrency default (`8`) is finally exploited across files, not just within one file's chunk graph.

#### Two independent knobs: concurrency and memory

The worker count alone is the wrong knob for peak memory. A single number `N` conflates two distinct resources: the number of **in-flight requests** (which hides network round-trip latency, and wants to be large) and the number of **object buffers resident at once** (`N × object-size`, which wants to be small). omemfs objects are variable-sized — a single chunk can be up to ≈16 MiB — so a concurrency that is healthy for many small objects can balloon peak RSS when many large chunks happen to be transferred at once. Measured: a fresh 400 MiB push on a 2-core machine at concurrency 16 peaked at ≈646 MiB resident, almost entirely transient upload buffers, while an incremental (mostly-cached) push of the same tree stayed near 64 MiB. A fully-dynamic search that probes concurrency upward until a resource saturates was rejected: the feedback signal is too noisy to converge — the *same* fresh-400 MiB / concurrency-8 push was measured at 82 MiB peak RSS on one run and 393 MiB on another, so an exploration loop would chase noise.

The two knobs are therefore separated:

- **`OMEMFS_TRANSFER_CONCURRENCY`** — the worker count (request parallelism). Default `1` local, `8` cloud. Tuned for latency hiding.
- **`OMEMFS_TRANSFER_MEMORY_BUDGET`** — a ceiling, in bytes, on the total size of transfer buffers held resident across all workers at once. Default **64 MiB** (`0`/unset → the default; an explicit value overrides). Tuned for memory safety, independent of the worker count.

The memory-budget size hint is advisory and must never cause a network metadata
request. A pack or inline entry supplies its stored size from the already-loaded
index. An entry without a local size hint reserves a conservative bounded
amount; it does not issue a remote `HEAD` merely to improve scheduling.

The memory budget is a counting semaphore over bytes (a `Mutex<u64>` of remaining budget + a `Condvar`), held in `src/commands/transfer.rs` and shared by all workers of one transfer run. Before a worker reads/buffers an object it acquires `min(size_hint, budget_capacity)` bytes from the semaphore, blocking until that many are free; it releases them (RAII guard) once the object's bytes are no longer resident (after the PUT completes, or after the local write on a download). Acquiring `min(size_hint, capacity)` rather than `size_hint` guarantees forward progress even for a single object larger than the whole budget — that object runs alone rather than deadlocking. With the budget in place, peak transfer-buffer memory is bounded by `budget + (one in-flight oversized object per worker that was admitted under the clamp)`, regardless of the worker count, so raising concurrency for latency no longer raises the memory ceiling.

A second-order copy still applies when transfers run through the `StatsStore` wrapper (used by `omemfs pack` to count GET/PUT/HEAD/byte totals): `StatsStore::write_from` reads the entire object into a `Vec` to count its byte length before forwarding it to the inner store (`src/store/stats.rs:126-127`). This buffer is charged against the same memory budget at the transfer layer, so it does not multiply the ceiling. Because individual stored objects are bounded at ≤ ~16 MiB by L3 chunking, the per-object buffer is always small; the budget bounds how many are resident at once.

**Fixed 3-level sharding**

All remote backends use **fixed 3-level sharding**: the 64-char hex hash is split into three 2-char directory components followed by the remaining 58 characters as the file/key name.

Example — hash `abcdef1234567890...` (64 chars):

```
objects/ab/cd/ef/1234567890...   (remaining 58 chars)
```

This layout is fixed from the first write; there is no migration or adaptive depth on the remote side. A local-directory remote uses the same fixed 3-level layout via `RemoteObjectsDir`.

The layout is **byte-for-byte identical across all four backends**. The local directory uses `/`-separated path components on the filesystem; S3, GCS, and Azure store the same string `<prefix>/objects/<2>/<2>/<2>/<58-char-hash>` as a single flat object key / object name / blob name, with `/` treated as an ordinary character (cloud object stores have no real directories — the slashes are only a naming convention). So a given object resolves to the same key string on every backend, and an unencrypted index root lives at the same `<prefix>/INDEX_ROOT` flat key. The cloud adapters share the same hex⇄key layout helpers as `RemoteObjectsDir`.

All remote backends — including the backup remote — use the identical format: a `objects/` pack layer plus an index root (derived path on encrypted remotes, fixed `INDEX_ROOT` on unencrypted remotes). There is no plain-objects fallback format.

### Remote enumeration (`list_with_sizes`)

`omemfs stats` (and any caller needing the full key set) enumerates the remote through `ObjectStore::list_with_sizes()`, which returns `(storage_key_hex, byte_size)` pairs for every stored object **without reading any object's contents**. Each cloud backend performs this as **one paginated LIST** over `<prefix>/objects/` (S3 `ListObjectsV2`, Azure `List Blobs`, GCS `Objects.list`), following the continuation token / next-page marker until exhausted; the size of each object is already carried in the LIST response, so no per-object HEAD/GET is needed. Keys whose final path component is not a 64-hex name (after re-joining the sharded components) are skipped, so stray non-object keys under the prefix are ignored. The local-directory backend implements the same contract with a single directory walk plus `fs::metadata` per file. On unencrypted remotes the fixed `INDEX_ROOT` lives outside `objects/` and is therefore not returned by `list_with_sizes`; `omemfs stats` accounts for it separately (see `04_cli_spec.md`, Section 1).

### Index root name derivation

For **encrypted repos**, the index root is stored at a key derived from the DEK:

```
index_root_name = HMAC-SHA256(DEK, "omemfs:index-root:v1")
```

The context string `"omemfs:index-root:v1"` is encoded as ASCII bytes. The result is hex-encoded to produce a 64-character string `n`, and the object is stored at:

```
objects/<n[0..2]>/<n[2..4]>/<n[4..6]>/<n[6..64]>
```

This is the same path format used for every other stored object, so the index root blends in among content objects. An observer with access to the remote storage cannot identify which object is the index root without knowledge of the DEK.

**Domain separation**: content objects use `storage_key = HMAC-SHA256(DEK, logical_hash)`, where `logical_hash` is a 32-byte SHA-256 output. The context string `"omemfs:index-root:v1"` has a different length and structure, and a SHA-256 preimage of that ASCII string is computationally unavailable. Collision between the index root key and any content object storage key is cryptographically negligible.

**Unencrypted repos** keep the fixed key `<prefix>/INDEX_ROOT`. No DEK exists to derive from, and hiding the index root is meaningless without encryption.

**CAS lock file**: the local-directory backend always uses the fixed name `<prefix>/INDEX_ROOT.lock`, regardless of encryption mode. The lock file reveals only that the prefix is an omemfs repo, not which object is the root. S3/GCS/Azure backends use conditional writes and need no lock file.

**Accepted residual risk**: the index root is the only mutable key in the remote — its storage key never changes between pushes (it is derived from the DEK, not from content). An observer who records per-object access timestamps or write timestamps over time can still identify the index root by its mutation pattern, even though its name is not fixed. This risk is accepted; the derived name removes trivial fixed-name identification but does not prevent traffic-analysis attacks.

## Timestamps

All timestamps are stored in UTC using ISO 8601 format:

```
2026-03-07T10:30:45Z
```

Command output converts to the user's local timezone (via `TZ` environment variable):

```
2026-03-07 19:30:45 JST
```

## Hash format

SHA-256, represented as a 64-character lowercase hex string. The hash is always computed from the serialised bytes including the 2-byte type tag prefix — see [L2 (ser): serialize / deserialize](#l2-ser-serialize--deserialize) for the full hash anchor rules.

No Git-style `<type> <size>\0` header is prepended before hashing.

---

## Codec pipeline

Object bytes pass through six layers (L2–L7) on the write path, and the reverse on read. L1 is the command layer (working-tree scan, CLI logic) and sits outside the codec pipeline.

```
Write path:  L2 serialize → L3 chunk → L4 compress → L5 encrypt → L6 pack → L7 store
Read path:   L7 load → L6 unpack → L5 decrypt → L4 decompress → L3 assemble → L2 deserialize
```

Each layer has a single, well-defined responsibility. Layers are independent and can be replaced or bypassed without affecting the others.

### Layer boundary summary

| Layer | Tag       | Input                | Output                             | Hash computed? |
|-------|-----------|----------------------|------------------------------------|----------------|
| L2    | `L2 ser`  | logical object       | tagged bytes (`ED F0`/`F1`)        | ✓ (from here)  |
| L3    | `L3 chk`  | tagged bytes         | chunk objects × N + manifest, or tagged bytes as-is | — |
| L4    | `L4 cmp`  | bytes                | compressed bytes                   | —              |
| L5    | `L5 enc`  | compressed bytes     | encrypted bytes                    | —              |
| L6    | `L6 pak`  | encrypted bytes      | routed to pack file, index, or objects/ | — |
| L7    | `L7 sto`  | pack files / index files / raw objects | persisted in backend | — |

### `ED` prefix map

The byte `ED` is not a valid UTF-8 sequence start byte and never appears at the start of JSON, plain-text, or ordinary binary content. Each layer uses a distinct `ED xx` range so that any stored object can be identified by its first two bytes after decompress.

```
ED D0        escaped raw (L4 cmp — raw content whose first byte was ED D0..DF)
ED D1–DD     reserved for future L4 compress extensions
ED DE        zstd with tree dictionary v1 (L4 cmp)
ED DF        plain zstd (L4 cmp)
ED E0        standalone escape (L6 pak — standalone object whose first 2 bytes were ED E0..EF)
ED E1        pack file (L6 pak)
ED E2        index file — hot / cold shard / delta (L6 pak)
ED E3        INDEX_ROOT (L6 pak)
ED E4        Bloom filter (L6 pak)
ED E5–EF     reserved for future L6 pack extensions
ED F0        blob escape (L2 ser — blob whose first byte was ED F0..FF)
ED F1        tree (L2 ser)
ED F2        chunked manifest (L3 chk)
ED F3        chunk body (L3 chk)
ED F4–FF     reserved for future L2 serialize extensions
```

### Streaming design

#### Chunk-bounded memory model

Because L3 chunking sits above L4 (compress) and L5 (encrypt) in the pipeline, every physical stored object is bounded at approximately CDC_MAX (≤ ~16 MiB) plus small codec overhead. This means L4, L5, and L7 may process whole buffers in memory by design — no streaming is needed through those layers. (Exception: tree objects bypass L3 chunking and are always stored whole — their serialised size is bounded in practice by per-directory entry counts, and tree JSON deserialisation needs the full buffer anyway.) The layers that require streaming care are the two **L3 ↔ local-file boundaries**:

- **Write side**: reading a large source file whole before passing it to `store_chunked` would require peak memory proportional to file size.
- **Read side**: concatenating all chunk payloads into one buffer before writing to disk would likewise require peak memory proportional to file size.

The design below keeps peak memory at ≤ ~64 MiB independent of file size.

#### STREAMING_THRESHOLD

`STREAMING_THRESHOLD = 64 MiB`

Source files smaller than this threshold use the in-memory write path: read the
file whole from one open handle, hash it, and pass the buffer to
`store_chunked`. Files at or above the threshold use the one-pass streaming
write below. The threshold is invisible in the stored format because
`StreamCDC` produces identical cut points to in-memory `FastCDC`.

Peak memory is bounded by `max(STREAMING_THRESHOLD, CDC_MAX + codec overhead)` ≈ 64 MiB.

#### One-pass streaming write (files ≥ STREAMING_THRESHOLD)

Open the source once and record handle metadata `(mtime, size, mode)`. Read the
first two bytes to decide whether the serialised stream needs the `ED F0`
escape, seek the same handle back to zero, then run `StreamCDC` over
`[0xED 0xF0 (if needed)] || file content`. The logical SHA-256 and the chunks
are computed from this same emitted byte stream. For each chunk:

```
chunk_bytes → tag as 0xED 0xF3 | chunk_bytes
            → compute chunk_hash = SHA256(tagged)
            → L4 compress (in-memory candidate comparison)
            → L5 encrypt  (in-memory AES-256-GCM)
            → L7 store_at(chunk_hash)
```

Collect all `chunk_hash` values in order. Chunk objects may be written before
the logical blob hash is final because each has its own content hash.

**Single-chunk special case**: if `StreamCDC` emits only one chunk, the object is stored whole at the logical hash (no manifest); same rule as the in-memory path.

**Manifest-last ordering**: after all chunks are stored, the `ED F2` manifest is stored at the logical hash. This ordering means chunks are always written before the manifest that references them. A manifest in the store implies all its chunks are present.

#### TOCTOU policy

After EOF, read metadata again from the same open handle and compare
`(mtime, size)` with the initial values. Also compare the number of bytes read
with `size`. If the file changed:

1. Discard all pass results. Do **not** write the manifest. (The individual chunk objects already stored are content-addressed and remain valid — they will simply be unreferenced until a future successful write.)
2. Retry the file once with a new handle.
3. If the retry is also unstable, return `SourceChanged`. Push treats this as
   a best-effort skip and preserves the previous tree entry.

This protects the invariant that the bytes stored under the logical hash actually hash to it.

#### Compatibility invariant

`StreamCDC` (streaming) and `FastCDC` (in-memory) from the fastcdc v2020 crate produce identical cut points over the same input bytes with the same parameters (`min=1 MiB / avg=4 MiB / max=16 MiB`). Therefore, whether the streaming or in-memory path was taken, the logical hash, chunk hashes, and chunk boundaries are always identical. The threshold switch is invisible in the stored format.

#### Streaming read / materialisation

After L5 decrypt + L4 decompress of the object at the logical hash, inspect the first 2 bytes:

- **Not `ED F2`**: the object is unchunked; pass bytes directly to L2 deserialize.
- **`ED F2` (manifest)**: read the chunk hash list (bytes[2..] split into 32-byte groups). For each chunk hash in order:
  1. L7 load → L5 decrypt → L4 decompress.
  2. Verify first 2 bytes are `ED F3`; strip the `ED F3` prefix.
  3. On the first chunk only: if the assembled stream begins with the `ED F0` blob-escape tag, strip it. Most blobs carry no tag (their first byte is not in `ED F0`–`ED FF`), in which case nothing is stripped. (Tree objects are never materialised this way; they go through in-memory assembly.)
  4. Write the chunk bytes sequentially to a `NamedTempFile` created in the destination directory.
  After all chunks are written, atomically rename the temp file to its final path. Apply `mtime` and `mode` after the rename, per the existing materialisation rules.

`omemfs cat` writes chunks sequentially to stdout instead of a temp file. Pull's conflict helper files (`.omemfs-conflict-{base,local,remote}`) are produced with the same sequential chunk write-through, so a GB-class conflicted file never buffers a whole side in memory.

To inspect an object's ordered chunk-hash list itself — without materialising or streaming the reassembled content — use `omemfs cat --manifest`; see design/04_cli_spec.md, `omemfs cat`, "Chunk manifest output".

**In-memory assembly** (`load_assembled`) is retained for small objects and internal consumers (e.g. tree JSON deserialisation), where the caller needs the complete byte buffer anyway.

---

## L2 (ser): serialize / deserialize

**Responsibility**: convert between a logical object and raw bytes, and compute the object hash.

### Magic byte prefix (type tag)

The byte `ED` is not a valid UTF-8 sequence start byte and never appears at the start of JSON or plain-text content. L2 (ser) uses `ED Fx` bytes to tag the two logical object types it knows about:

| Prefix      | Object type | Produced by |
|-------------|-------------|-------------|
| `ED F0 ...` | blob escape — first byte of file content was `ED Fx` | L2 ser |
| `ED F1 ...` | tree (normal JSON) | L2 ser |
| *(none)*    | blob — file content stored as-is (first byte is not `ED Fx`) | L2 ser |

The `ED F2` and `ED F3` tags are produced by **L3 (chk)**, not by L2 (ser):

| Prefix      | Object type | Produced by |
|-------------|-------------|-------------|
| `ED F2 ...` | chunked manifest | L3 chk |
| `ED F3 ...` | chunk body | L3 chk |

The `ED Dx` range is used by L4 (cmp). The `ED F4`–`ED FF` range is reserved for future L2 serialize extensions.

### blob

A blob's serialised form is the file content itself, with an escape applied when necessary.

```
serialised(blob) =
  file content bytes                   (if first byte is not in ED F0..FF)
  0xED 0xF0 | file content bytes       (if first byte is in ED F0..FF)
```

The escape ensures the compress stage can reliably identify `ED Dx` as its own prefix and will not misinterpret blob content.

### tree

A tree's serialised form is a 2-byte type tag followed by the minimised JSON representation (no whitespace, no indentation, entries sorted alphabetically by `name`).

```
serialised(tree) = 0xED 0xF1 | UTF-8 bytes of minimised JSON
```

Example:

```json
{"kind":"normal","entries":[{"kind":"blob","name":"file.txt","hash":"a1b2...","mtime":"2026-05-16T10:00:00.000000000Z","size":1234}]}
```

(The `ED F1` prefix precedes this JSON in the actual stored bytes.)

### Hash anchor

The object hash is computed from the **tagged serialised bytes** — always including the type tag, regardless of whether the escape was applied:

```
hash(blob) = SHA256(0xED 0xF0 | file content bytes)
hash(tree) = SHA256(0xED 0xF1 | tree JSON bytes)
```

This ensures blob and tree objects with identical content always have different hashes.

L3 (chk) adds two further hash rules using its own tags (see [L3 (chk): chunk / assemble](#l3-chk-chunk--assemble) for details):

```
hash(chunk body)    = SHA256(0xED 0xF3 | segment bytes)
hash(manifest)      = the logical hash of the object being chunked
                      (same hash as if the object were stored whole)
```

The manifest hash is **not** computed from the manifest bytes — it reuses the logical object hash so that callers always address objects by the same hash regardless of whether chunking was applied.

### Write path

**Idempotency and encode-skip optimisation**

Object storage is content-addressed: the object's hash is its identity. Writing
the same hash twice produces identical bytes, so it is always safe to skip the
second write. The `codec::store_write` (and `build_and_store`) call checks
`store.exists(hash)` before running the encode pipeline (compress → encrypt).
If the object is already present, the write returns immediately without encoding.

This is safe because:
- Objects are immutable: a hash never maps to different content.
- `write_from` itself is idempotent at the L7 layer (skips the file write if the
  path already exists), but the encode pipeline runs before that check.
- Skipping encode is therefore the only way to avoid the CPU cost of compress and
  encrypt for objects that are already stored.

Blob objects are captured from one open file description per attempt. Files
below STREAMING_THRESHOLD are buffered in memory. Larger files feed the same
stream into the logical hasher and `StreamCDC`; after EOF the final logical hash
addresses the manifest. The manifest is written last, after the handle metadata
stability check. Tree objects are small enough to fit in memory.

### Snapshot read path

A `PackReader` is a **snapshot reader**. Its first index-root access reads and
decrypts the index root exactly once; that decoded immutable root defines the
reader's view for its lifetime. It retains the root, parsed delta and hot
indexes, loaded cold shards, and Bloom filter in memory. A command that needs a
newer remote state constructs a new reader; an existing reader is never
refreshed implicitly.

Normal `pull`, `expand`, `restore`, clone materialisation, and lazy-tree reads
use **SnapshotOnly** lookup semantics. They resolve an object only through
this snapshot's delta/hot/cold indexes and do not issue a per-object remote
`HEAD` for a standalone fallback. A successful push uploads every referenced
object and publishes its index entry before the index-root CAS, so all
objects reachable from the snapshot remote root are resolvable from that
snapshot. An unindexed standalone object is an orphan from an interrupted or
obsolete write and is outside normal command semantics.

`restore` resolves objects reachable from `clone_root` rather than from the
remote root, but the same guarantee covers it: the hot index's reachability
set is defined over the working tree, `clone_root`, **and** `remote_root` of
the local clone (see "Three-tier index" below), and objects that age out of
the hot index are recorded in a cold shard rather than deleted (see
"Reclaiming unreferenced objects" below) — so a `clone_root`-reachable object
is always resolvable through SnapshotOnly, and never needs an unindexed
standalone fallback. `restore` must not use LiveFallback: unlike `cat`'s
single diagnostic hash lookup, restore's fetch phase can resolve thousands of
objects in one batched transfer, so LiveFallback's extra per-miss
`remote.exists` probe (see "Implementation" below) would multiply into a
per-object cost restore cannot amortise the way `cat`'s "single extra round
trip" does; and unlike `cat`, restore is a destructive bulk operation, so
silently materialising an unindexed orphan into the working tree would
contradict "restore reproduces exactly what `clone_root` records", not merely
display a diagnostic warning about it.

An explicit diagnostic/raw-hash operation may opt into **LiveFallback** lookup.
Only that mode may probe the remote standalone key after a SnapshotOnly miss,
and it must identify such a result as outside the snapshot. This preserves
repair and diagnosis without adding an S3 round trip to every normal graph
lookup.

**Implementation.** LiveFallback is implemented as `PackReader::resolve_diagnostic`
(`src/codec/pack/reader.rs`), a method distinct from — and never called by —
`resolve()`/`open_read()`/`exists()`. It first runs the same index-only
`locate()` used by `resolve()`; on `Located::Entry` or `Located::NoIndexRoot`
it returns the object's bytes with `outside_snapshot = false`, identical to
what `resolve()` would return. Only on `Located::NotFound` does it take the
extra step: a single `self.remote.exists(hash)` probe, and if that reports
presence, a single `self.remote.open_read(hash)` fetch, returning the bytes
with `outside_snapshot = true`. If the probe reports absence, it returns the
same `Error::ObjectNotFound` a plain SnapshotOnly miss would. This is a
bounded, single extra remote round trip that only happens on a genuine
SnapshotOnly miss.

The only caller of `resolve_diagnostic` is `omemfs cat` (`src/commands/cat.rs`),
and only for a **full 64-character hash** target that is not already in the
local cache: `cat` first attempts ordinary SnapshotOnly resolution (the same
`PackReader` + `transfer_objects` graph walk used for a resolved hash prefix);
only when that misses does it fall to `resolve_diagnostic`. A hash **prefix**
(4..63 chars) never uses LiveFallback — there is no exact remote key to probe
for a prefix, so an unresolved prefix is reported as not found exactly as
before. `ls`, `pull`, `expand`, `restore`, and clone materialisation are
unaffected and remain strictly SnapshotOnly, as described above.

When `cat` displays an object obtained via LiveFallback (`outside_snapshot ==
true`), it prints a warning to **stderr** (stdout carries only the decoded
object content, so scripts consuming `cat`'s stdout are not affected) along
the lines of:

```
warning: <hash> was found outside the current snapshot (no delta/hot/cold
index entry references it; likely an orphan from an interrupted or obsolete
write) -- it is not reachable from any recorded root
```

Pack-layer artifacts (pack files, index files, Bloom filters) are a separate
concept: they are never referenced by the logical delta/hot/cold index by
design (they ARE that index), so a full hash naming one of them also misses
SnapshotOnly resolution and is also fetched through the same raw probe — but
`cat` recognises their leading magic bytes and renders the existing pack-layer
diagnostic view (design/04_cli_spec.md, "Pack-layer output") for them
unconditionally, without the "outside the snapshot" warning, since that
warning is about logical objects missing from the index, not about the index
files themselves.

```
load(target_hash):
  1. Check local objects/ cache — return immediately on hit.
  2. locate_snapshot(target_hash) — described below.
  3. Resolve the classification to bytes per "On index hit" below.

locate_snapshot(target_hash):
  1. Use the index root retained at reader construction.
  2. Search in-memory delta indexes newest-first, then the hot index.
     → found: Entry.
  3. If the snapshot Bloom filter definitely excludes target_hash: NotFound.
  4. Compute the covering cold shard. Load it from memory, objcache/, or a
     single-flight remote GET, then binary-search it.
     → found: Entry.
     → not found: NotFound.

On index hit:
  inline entry     → return data bytes directly (already encrypted; caller decrypts).
  pack entry       → obtain the whole pack into the pack cache once, then slice locally.
  standalone entry → fetch objects/<storage_key> directly from remote.
```

**Pack cache fetch.** The first read of a pack downloads the whole pack and
atomically installs it as `.omemfs/packcache/<pack_hash>`; later slices of that
pack are served from that one local file. A pack is never fetched twice by
concurrent readers: a cache miss takes a single reader-wide fetch lock, checks
the cache again under the lock, and only then downloads. A waiter that finds the
pack installed by the time it gets the lock skips the download. If a download
fails, the error is returned to the caller that ran it and no completed cache
entry is left; a waiter then takes the lock and attempts the download itself.

Two known performance limits of this implementation (correctness is not
affected):

- **The fetch lock is one lock per reader, not per `pack_hash`.** First-time
  downloads of *different* packs are serialised, so concurrent readers cannot
  overlap remote round trips for distinct packs. Index-file and Bloom-filter
  loads use the same kind of reader-wide lock.
- **A slice read opens the cached pack and discards the bytes before the
  indexed offset** (`PACK_BODY_OFFSET` + entry offset) instead of seeking to it.
  Packs target 4 MiB (16 MiB maximum), so an object costs on average about half
  a pack of local reads. The `ObjectStore` trait has no seek or range-read
  operation, and the remote is only ever read as a whole pack.

All normal read-side classifications are derived from one published index root.
This eliminates the per-object `HEAD` round trip and makes concurrent planning
deterministic. A Bloom definite miss can skip cold-shard loading because the
filter and cold shards share one snapshot. A false positive only costs an index
lookup; it never changes classification. Remote changes after reader
construction are intentionally invisible until a new reader is opened.

### Legacy read path (superseded)

```
stored bytes (after decompress) → inspect first 2 bytes:
  ED F0 → blob (strip 2-byte prefix; remainder is file content)
  ED F1 → tree (strip 2-byte prefix; deserialise JSON)
  anything else → blob (entire bytes are file content)
```

For chunked objects (manifest at logical hash), the read path assembles chunk payloads using either in-memory concatenation or sequential write-through; see the [Streaming design](#streaming-design) section and the [L3 read algorithm](#read-algorithm-assemble) for the full details.

---

## L3 (chk): chunk / assemble

**Responsibility**: split large serialised objects into fixed-size chunk objects and store a manifest, or pass small objects through unchanged.

### Write algorithm

Apply FastCDC to the serialised bytes with the following parameters:

| Parameter | Value  |
|-----------|--------|
| min_size  | 1 MiB  |
| avg_size  | 4 MiB  |
| max_size  | 16 MiB |

```
If FastCDC produces only one chunk:
  pass the serialised bytes through to L4 (cmp) unchanged
  → stored at logical_hash

If FastCDC produces ≥ 2 chunks:
  for each chunk_bytes:
    tagged = 0xED 0xF3 | chunk_bytes
    chunk_hash = SHA256(tagged)
    tagged → L4 compress → L5 encrypt → L7 store_at(chunk_hash)

  manifest = 0xED 0xF2 | chunk_hash[0] (32 bytes) | … | chunk_hash[N-1] (32 bytes)
  manifest → L4 compress → L5 encrypt → L7 store_at(logical_hash)
```

The `logical_hash` is the hash computed by L2 (ser) and is unchanged by chunking. Callers always address objects by their logical hash.

**Hash-only variant** (read-only commands)

The working-tree scan can compute a file's `logical_hash` without storing any
blob object, by running only the L2 hash anchor over the file's content (a
single streaming SHA-256 pass — the same digest the streaming write computes
alongside chunking). Read-only commands (`ls`, `pull`) use this variant: they need the
blob hashes to assemble tree objects and diff, but never read blob *bodies*
back, so L3 chunking, L4 compression, L5 encryption, and the L7 write are
skipped for blobs. Tree objects are still written normally. See design/03 "Scan
blob-write mode".

**Streaming write variant** (source files ≥ STREAMING_THRESHOLD)

For source files at or above STREAMING_THRESHOLD, FastCDC runs in streaming mode using `StreamCDC` (fastcdc v2020) over the prefixed source stream instead of over a full in-memory buffer. Each chunk emitted by `StreamCDC` is buffered in memory (≤ CDC_MAX ≈ 16 MiB), tagged `ED F3`, hashed, and stored through the normal in-memory L4 → L5 → L7 pipeline — identical to the algorithm above. Cut points are identical to the in-memory `FastCDC` variant by construction (same parameters, same crate version), so chunk hashes, chunk boundaries, and the logical hash are always the same regardless of which path was taken.

### Read algorithm (assemble)

After L4 (cmp) decompress, inspect the first 2 bytes of the result:

```
ED F2 → manifest detected
         read chunk_hash list: bytes[2..] split into 32-byte groups
         for each chunk_hash:
           L7 load → L5 decrypt → L4 decompress
           verify first 2 bytes are ED F3
           strip ED F3 prefix → chunk_bytes
         concatenate all chunk_bytes in order → serialised bytes → L2 deserialize

anything else → pass bytes directly to L2 deserialize (no chunking)
```

**Two consumption modes**

- **In-memory concatenation** (`load_assembled`): used for small objects and internal consumers (e.g. tree JSON deserialisation) that need the complete byte buffer. All chunk payloads are concatenated into a single `Vec<u8>` and returned to the caller.
- **Sequential write-through**: used during file materialisation (`pull`, `expand`) and `omemfs cat`. Each decoded chunk is written sequentially to a `NamedTempFile` in the destination directory (or to stdout for `cat`) without accumulating a full in-memory buffer. After the last chunk is written, the temp file is atomically renamed to its final path. This keeps peak memory bounded at approximately one chunk (≤ CDC_MAX ≈ 16 MiB) regardless of file size. See the Streaming design section for the full materialisation flow.

### Chunk objects

Chunk objects carry the `ED F3` type tag so that any object in the store can be identified by its first two bytes. The tag also prevents a chunk object from being silently misinterpreted as a blob or tree during debugging.

```
chunk_hash = SHA256(0xED 0xF3 | chunk_bytes)
```

Chunks are not addressed by logical hashes and do not appear in tree entries. They are an internal detail of the chunk stage.

---

## L4 (cmp): compress / decompress

**Responsibility**: reduce the stored size of serialised bytes.

The compression format is identified by a 2-byte magic prefix on the stored bytes. The byte `ED` was chosen as the leading byte because it is not a valid UTF-8 sequence start byte and therefore never appears at the start of any JSON or plain-text serialised content.

### Format table

| Stored leading bytes | Format            | Description                                    |
|----------------------|-------------------|------------------------------------------------|
| `ED DE ...`          | tree-dict zstd v1 | zstd with built-in tree dictionary version 1   |
| `ED DF ...`          | plain zstd        | zstd without a dictionary                      |
| `ED D0 ...`          | escaped raw       | raw content whose first byte is `ED D0`–`ED DF` |
| anything else        | raw               | content stored as-is, no prefix                |

The `ED F0`–`ED FF` range is used by L2 (ser). L4 (cmp) does not emit or interpret these values, so there is no collision between the two layers.

Future tree dictionary versions will use other `ED`-prefixed magic bytes (reserved range: `ED D1` – `ED DD`).

### Write algorithm

Three candidates are tried for every object type, and the smallest total byte count (including the 2-byte prefix where present) wins:

1. `ED DE` + zstd with embedded tree dictionary v1
2. `ED DF` + plain zstd
3. raw: prepend `ED D0` only if the serialised content starts with `ED D0`–`ED DF`; otherwise store as-is with no prefix

Selection is size-based and applied uniformly to all object types. The tree dictionary mainly benefits small tree objects, but because the winner is always the smallest result it is also considered for blob and chunk objects.

L4 (cmp) does not know whether the object is a tree or a blob — it simply tries all three candidates and picks the smallest result.

Note: the already-compressed check (JPEG, PNG, ZIP, etc.) has been removed. zstd compressing already-compressed data will produce a result larger than the raw candidate, so the size comparison naturally selects raw in those cases.

Because L3 chunking sits above L4 in the pipeline, inputs to L4 are always bounded by CDC_MAX (≤ ~16 MiB). The three-candidate comparison therefore operates on whole in-memory buffers by design — this is the intended final behaviour, not a temporary simplification.

### Read algorithm

1. Read the first 2 bytes of the stored stream.
2. Dispatch on the leading bytes:
   - `ED DE` → decompress with tree dictionary v1 (strip 2-byte prefix first)
   - `ED DF` → decompress with plain zstd via `zstd::stream::Decoder` (strip 2-byte prefix first)
   - `ED D0` → strip 2-byte prefix; the remainder is the raw serialised content
   - anything else → the entire file content is the raw serialised content
3. Pass resulting stream to L3 (chk) assemble (which detects `ED F2` manifests) and then L2 (ser) deserialize (which interprets `ED F0`/`ED F1` type tags).

### Tree dictionary

Tree objects are small JSON blobs with highly repetitive field names (`"entries"`, `"name"`, `"kind"`, `"hash"`, `"mtime"`, `"size"`, etc.). A zstd dictionary trained on typical tree JSON significantly improves the compression ratio for these objects compared with plain zstd.

- The v1 dictionary is embedded in the omemfs binary — it is not stored in the remote backend.
- The magic bytes `ED DE` identify the v1 dictionary. The zstd frame header's `dict_id` field provides a secondary check that the correct dictionary is loaded.

### Implementation note

Always open object files in binary mode. Text-mode line-ending conversion on Windows would corrupt the content and invalidate the hash.

### Untrusted-remote validation: decompression output size limit

On an unencrypted remote, the compressed bytes read by `decompress` have no
GCM tag protecting them, so a hostile or corrupted remote can supply a small
`ED DE`/`ED DF` payload engineered to zstd-decompress into an arbitrarily
large output — a decompression bomb. Because L3 chunking sits above L4 in
the pipeline (see "Write algorithm" above), every legitimate decompressed
result is bounded by `CDC_MAX` (≤ ~16 MiB; `src/codec/chunk.rs`) plus small
codec overhead; a tree object (which bypasses L3 chunking, see "Streaming
design") is bounded in practice by per-directory entry counts and is smaller
still in every observed case. `decompress_zstd_bytes` therefore reads through
a size-limited adapter (`Read::take`, requesting one byte more than the limit
so that "still more data available past the limit" can be distinguished from
"the stream ended exactly at the limit") instead of `read_to_end` with no
bound, and returns `Error::InvalidObject` if the limit is exceeded. The limit
is `DECOMPRESS_MAX = 64 MiB` — four times `CDC_MAX` — chosen generously above
the ~16 MiB legitimate ceiling to leave headroom for codec/dictionary
overhead and any future chunk-size tuning, while still keeping a hostile
payload's worst case bounded to a two-order-of-magnitude blowup rather than
unbounded.

---

## L5 (enc): encrypt / decrypt

**Responsibility**: protect object bytes at rest in the remote backend.

The remote backend stores only the encrypted bytes; it never sees plaintext content.

### MVP: passthrough (no encryption)

In the initial implementation, this stage is a no-op passthrough. The API is defined but the encryption key is absent, so `encrypt(bytes) = bytes` and `decrypt(bytes) = bytes`.

### Client-side encryption

The goal of encryption is to protect objects stored in the remote backend (S3, GCS, etc.) from being read by third parties who gain access to the remote storage. The local filesystem is assumed to be under the user's control and is not encrypted.

Encrypted objects carry **no magic prefix**. Whether an object is encrypted is determined solely by the repository configuration in `.omemfs/config`.

**Key management**

A single DEK (data encryption key) is shared by all objects in the repository. The DEK is a randomly generated 32-byte value stored in `.omemfs/config` (see the [Encryption configuration](#encryption-configuration) section). There is no passphrase derivation step; the DEK is read directly from config.

There is no `omemfs init` command. When `omemfs clone` is run and the user answers `Y` to the `Enable encryption?` prompt, a random DEK is generated and written to the remote's `encryption.dek` field in `.omemfs/config`.

**Nonce derivation from object hash**

The nonce is derived deterministically from the object hash:

```
nonce = object_hash[0..12]   (first 12 bytes of the 32-byte SHA-256 hash)
```

This eliminates per-object nonce storage overhead. It is safe because:
- Objects are immutable: the same hash always corresponds to the same plaintext.
- GCM requires that (key, nonce) is never reused for different plaintexts. Since each object has a unique hash, each (key, nonce) pair is used for exactly one plaintext.

**Write algorithm when encryption is configured**

```
serialised bytes → compress → AES-256-GCM encrypt (key=DEK, nonce=object_hash[0..12]) → store
```

Encryption is a single one-shot call to the `aes-gcm` crate's `Aes256Gcm` (AAD is empty), which appends the 16-byte GCM authentication tag to the ciphertext itself. Because inputs are bounded by CDC_MAX, both the compressed input and the ciphertext output fit comfortably in memory, so there is no streaming variant.

**Write algorithm when encryption is not configured**

```
serialised bytes → compress → store
```

**Read algorithm when encryption is configured**

Because inputs to L5 are bounded by CDC_MAX (≤ ~16 MiB plus the 16-byte GCM tag) — a consequence of L3 chunking sitting above L5 — decryption is performed entirely in memory:

```
1. Read all stored bytes into a buffer (ciphertext || 16-byte GCM tag).
2. Call `Aes256Gcm::decrypt` (key=DEK, nonce=object_hash[0..12], AAD empty),
   which verifies the tag using its own constant-time comparison before
   returning anything.
3a. If verification succeeds: hand the plaintext buffer to L4 (cmp) decompress.
3b. If verification fails: no plaintext is ever produced; return an error.
    No decrypted bytes are passed to any downstream layer.
```

This in-memory approach ensures that no unverified plaintext ever reaches L4 (cmp) decompress or L2 (ser) deserialize, preventing exposure to tampered data (including decompression bombs). The security property is identical to a tempfile-based approach: the plaintext is withheld from all downstream layers until the GCM tag is confirmed valid.

**Read algorithm when encryption is not configured**

All objects are treated as unencrypted. No decryption is attempted. The stored stream is passed directly to L4 (cmp) decompress.

**Stored format (encrypted object)**

```
AES-256-GCM ciphertext || GCM auth tag (16 bytes)
```

No magic prefix is stored. The ciphertext is the same length as the compressed object bytes.

- `GCM auth tag (16 bytes)`: a 16-byte value computed during encryption that authenticates both the ciphertext and the nonce. Verifying the tag on decryption proves the data has not been tampered with.

**Scope of encryption**

Only objects written to the remote backend are encrypted. Local cached objects (`.omemfs/objects/`) are stored as compressed but unencrypted bytes, since the local filesystem is assumed to be under the user's control.

**Decryption failures**

If GCM tag verification fails, the read is aborted with an error and the in-memory plaintext buffer is dropped. The object must be re-fetched or restored from a known-good backup.

**Storage key derivation**

When encryption is configured, object bytes are stored in the remote backend under a _storage key_ derived from the DEK and the logical hash rather than the logical hash itself:

```
storage_key = HMAC-SHA256(DEK, logical_hash)
```

Here `logical_hash` is the 32-byte raw SHA-256 digest (not hex). The result is hex-encoded to produce the 64-character storage key used as the object path.

This prevents a _confirmation-of-file attack_: an adversary who knows the SHA-256 hash of a plaintext file (e.g. a known OS binary) could otherwise issue a `HEAD objects/<hash>` request against the remote backend to determine whether that file is stored there — without being able to read its contents. Using a keyed storage key means an adversary without the DEK cannot derive the storage path for any given file.

When encryption is not configured (`key = None`), `storage_key = logical_hash` (identity mapping; no HMAC is applied).

**Index root key derivation**

The index root is not a content object and does not have a logical hash. On encrypted remotes, its storage key is derived independently using a domain-separated context string:

```
index_root_name = HMAC-SHA256(DEK, "omemfs:index-root:v1")
```

The context string is encoded as ASCII bytes. The result is hex-encoded to the same 64-character form and sharded identically to content objects (`objects/<n[0..2]>/<n[2..4]>/<n[4..6]>/<n[6..64]>`). See [Index root name derivation](#index-root-name-derivation) for the full rationale, domain-separation argument, and accepted residual risks.

On unencrypted remotes, the index root is stored at the fixed key `<prefix>/INDEX_ROOT`.

The storage key derivation is encapsulated entirely within the L7 (sto) layer. All higher layers (L6 pak, L5 enc, L2 ser, L1 cmd) address objects by their logical hash; the mapping to the physical storage path happens transparently inside `LocalStore::exists`, `LocalStore::open_read`, and `LocalStore::write_from`.

The `omemfs cat pack:<hash>` command accepts a 64-character logical hash. The corresponding storage key is shown in its output as a reference field.

---

## L6 (pak): pack / unpack

**Responsibility**: reduce the number of S3 API calls by batching small encrypted objects into pack files and routing them through a three-tier index (delta / hot / cold).

### Object routing (write path)

Each encrypted object is routed based on its byte size after encryption:

| Size | Destination | Index entry |
|------|-------------|-------------|
| < 256 B | embedded directly in the index file as an inline entry | inline entry |
| 256 B – 1 MiB | appended to a pack file buffer, flushed on push | pack entry |
| ≥ 1 MiB | written directly to `objects/<storage_key>` in the remote backend | none (standalone) |

The 1 MiB boundary aligns with the CDC `min_size` parameter used in the chunk stage. Objects at or above this threshold are already chunked; routing them as standalone avoids double-batching overhead.

### Standalone escape

Standalone objects (≥ 1 MiB) are written directly to `objects/<storage_key>` in the remote backend. Because encryption produces arbitrary bytes, the stored content could begin with `ED E0`–`ED EF`, which would be indistinguishable from L6 internal objects (pack files, index files, etc.) if `objects/` were scanned directly.

To prevent this ambiguity, L6 wraps the encrypted bytes with an escape prefix before writing standalone objects:

```
if encrypted_bytes[0..2] is in ED E0..EF:
  stored = [ED E0] | encrypted_bytes
else:
  stored = encrypted_bytes
```

On read, L6 strips the `ED E0` prefix if present before returning the encrypted bytes to the caller.

`ED E0` was chosen as the escape magic to match the convention that each layer's escape uses the `00` suffix (L4 uses `ED D0`; L6 uses `ED E0`).

### Pack file format

```
ED E1 | encrypted_bytes[0] | encrypted_bytes[1] | ...
```

- The `ED E1` magic prefix identifies the file as a pack.
- Each entry is the raw encrypted bytes of one object. Boundaries (offset, length) are stored in the index, not in the pack file itself.
- The pack file is neither compressed nor encrypted (its contents are already encrypted per-object). Encrypting the pack file itself would provide no additional confidentiality benefit, because metadata about object boundaries is held in the index file, which is separately encrypted.

| Parameter | Value |
|-----------|-------|
| Target size | 4 MiB |
| Maximum size | 16 MiB |

### Index file format

All index files (delta, hot, and cold shards) share the same binary format. However, standalone entries (tag `0x03`) are only written into delta and hot index files — cold shards never contain standalone entries.

Index files are encrypted with AES-256-GCM using the same DEK as regular objects. The nonce is derived from the index file's own SHA-256 hash using the same derivation rule as for regular objects (`nonce = hash[0..12]`). The plaintext binary content is what defines the hash; the stored file in the remote backend is the ciphertext.

```
magic       : 2 bytes  (ED E2)
version     : 1 byte   (0x01)
reserved    : 1 byte   (0x00)
entry_count : 4 bytes  (big-endian)
entries     : entry × entry_count  (sorted ascending by hash)
```

**Untrusted-remote validation of `entry_count`**: `entry_count` is attacker-
or corruption-controlled on an unencrypted or compromised remote (there is no
GCM tag to reject a tampered plaintext before this field is read). Before
allocating a `Vec` sized by `entry_count`, the deserialiser caps it at the
maximum number of entries that could possibly fit in the remaining bytes,
computed from the smallest possible entry encoding (tag `0x03` standalone:
1-byte tag + 32-byte hash = 33 bytes). Concretely: `entry_count =
entry_count.min((data.len() - pos) / 33)` before it is used as a `Vec`
capacity. A value exceeding this bound cannot be satisfied by the actual byte
stream regardless of its truncation-check outcome, so capping first prevents
an attacker from forcing a huge allocation (e.g. `entry_count = 0xFFFFFFFF`
attempting a ~515 GB allocation and aborting the process) purely from a
32-bit field in a few-KB file. The existing per-entry truncation checks
during the parse loop are unaffected — they still reject a file whose actual
entry count/sizes disagree with the (now-capped) count as before.

Entries are sorted by hash so binary search (O(log N)) can be used. Ordering is
**strictly** ascending: no two entries may share a hash. This is enforced on
deserialise (`entries[i].hash <= entries[i-1].hash` is rejected as an unsorted
index) and on the write side by `IndexFile::push`, which is idempotent by
hash — pushing an entry whose hash is already present replaces the existing
entry in place (last write wins) rather than inserting a second, equal-hash
entry.

Entry format — tag byte selects the variant:

```
tag  : 1 byte
hash : 32 bytes  (logical hash of the object)

[tag = 0x01: inline entry]
  data_length : 1 byte   (0–255; inline threshold is < 256 B)
  data        : data_length bytes  (encrypted bytes)

[tag = 0x02: pack entry]
  pack_hash : 32 bytes
  offset    : 4 bytes  (big-endian, byte offset within the pack file after the 2-byte magic)
  length    : 4 bytes  (big-endian, length of the encrypted bytes)

[tag = 0x03: standalone entry]  — delta and hot index only
  (no additional fields; signals that the object is stored directly at objects/<storage_key>)
```

### INDEX_ROOT (index root object)

The index root is the **single root pointer** for the remote: it embeds the remote root tree hash plus pack index metadata. It is updated atomically using compare-and-swap (CAS) on every push.

**Storage location**

- **Encrypted remote**: stored at `objects/<index_root_name[0..2]>/<index_root_name[2..4]>/<index_root_name[4..6]>/<index_root_name[6..64]>`, where `index_root_name = HMAC-SHA256(DEK, "omemfs:index-root:v1")` hex-encoded. See [Index root name derivation](#index-root-name-derivation).
- **Unencrypted remote**: stored at the fixed key `<prefix>/INDEX_ROOT`.

**Push safety (CAS specification)**: at the start of a push, the client reads and stores the raw index root bytes. After all new objects have been uploaded, the client writes the new index root using a CAS operation whose expected value is those raw bytes read at push start (not re-read at finish time). If the CAS fails — meaning another client pushed concurrently — the push errors and the user must `pull` first before retrying. For the local-directory backend, the read-compare-write sequence is serialized by an exclusive `flock(2)` on the lock file at the fixed path `<prefix>/INDEX_ROOT.lock`, closing the TOCTOU window. The cloud backends use server-side conditional writes instead of a lock file: S3 and Azure use ETag preconditions (`If-Match` / `If-None-Match: *`, `.if_not_exists()`), GCS uses `ifGenerationMatch`. A `412 Precondition Failed` maps to a CAS failure. See `03_sync_model.md` and `design/13_cloud_backends.md`.

**Rationale for fixed lock file name**: the lock file reveals only that the prefix is an omemfs repo, not which object is the root. Deriving the lock file name from the DEK would provide no additional privacy benefit because the lock file is a transient coordination artifact used only by the local-directory backend; S3 backends use conditional writes and have no lock file at all.

When encryption is configured, the index root object is encrypted with AES-256-GCM using the repository DEK. Because the index root is not content-addressed, the nonce cannot be derived from the content hash. Instead, a fresh random 12-byte nonce is generated on every write and prepended to the stored bytes:

```
Stored index root object (encrypted form):
  nonce      : 12 bytes  (random, freshly generated on each write)
  ciphertext : AES-256-GCM(DEK, nonce, INDEX_ROOT_plaintext)
  GCM tag    : 16 bytes  (appended after ciphertext)
```

On read, the first 12 bytes are extracted as the nonce and the remainder is decrypted and verified before use. Generating a new random nonce on every push ensures nonce uniqueness without coordination.

The plaintext structure of the index root object is:

```
magic              : 2 bytes   (ED E3)
version            : 1 byte    (0x01)
reserved           : 1 byte    (0x00)
remote_root        : 32 bytes  (tree hash; all-zero bytes if never pushed)
hot_hash           : 32 bytes  (hash of the hot index file)
bloom_hash         : 32 bytes  (hash of the Bloom filter file; all-zero if not yet generated)
cold_prefix_bits   : 1 byte    (number of hash prefix bits used to address cold shards; 0 on init)
reserved2          : 3 bytes
delta_count        : 2 bytes   (big-endian, number of pending delta index files)
padding            : 2 bytes
delta_hash[0..N]   : 32 × delta_count bytes  (newest first)
cold_shard[0..2^cold_prefix_bits] : 32 × 2^cold_prefix_bits bytes
```

Index root plaintext size at various `cold_prefix_bits` values (with `delta_count = 0`). The writer only produces 0 (fresh repository) and 8 (after the first cold shard split); the other rows are for size reference only (the reader accepts any value):
The stored size in the remote backend adds 28 bytes (12-byte nonce + 16-byte GCM tag) when encryption is enabled.

| `cold_prefix_bits` | cold_shard entries | Index root size |
|--------------------|--------------------|-----------------|
| 0 | 1 | ~144 bytes |
| 4 | 16 | ~656 bytes |
| 8 | 256 | ~8.5 KiB |
| 12 | 4 096 | ~128 KiB |

**Untrusted-remote validation of `cold_prefix_bits`**: on an unencrypted
remote, INDEX_ROOT is stored as plaintext with no GCM tag at all (see
"Storage location" above), so `cold_prefix_bits` is entirely
attacker-or-corruption-controlled. The largest value ever legitimately
written by this client is far below the 12 shown in the table above — `pack`
only increments it in steps of 4 bits, and each increment requires every
`2^N` existing prefix slot to already have a dedicated shard, so reaching
even `cold_prefix_bits = 16` (65 536 shard slots) would require the remote to
already hold tens of thousands of split cold shards. The deserialiser rejects
any `cold_prefix_bits > 32` (`Error::InvalidObject`) before computing
`1u64 << cold_prefix_bits` or the downstream `delta_count * 32 + cold_shard_count
* 32` byte-length arithmetic. 32 is chosen generously above any plausible
operational value (comfortably above the table's largest entry, 12) while
still being small enough that `1u64 << 32` (the shard count implied by the
bound) times 32 bytes stays a bounded, non-overflowing `u64` computation on
every platform — unlike `1usize << 64` and above, which either panics (debug)
or silently wraps to 0 (release), corrupting the truncation check that
follows. A rejected `cold_prefix_bits` never reaches the shift or the
allocation.

### Three-tier index

**delta index** — one file per push

- Contains only the entries added in that push.
- Listed in the index root's `delta_hash[]` field, newest entry first.
- Merged into the hot index when `omemfs pack` is run.

**hot index** — single file, regenerated by `omemfs pack`

- Contains all objects reachable from the working tree, `clone_root`, and `remote_root` of the local clone.
- Objects under stub paths are excluded (the local clone has not materialised them).
- Rebuilt by `omemfs pack` from the current delta files plus the existing hot index.

**cold shards** — one file per address prefix, managed by `omemfs pack`

- Contains pack and inline entries for objects no longer reachable from the hot index.
- Standalone objects are **not** recorded in cold shards; they are always reachable via `objects/<storage_key>` directly.
- The index root's `cold_shard[p]` field holds the hash of the shard file for hash-prefix `p`.
- Multiple prefix slots may point to the same shard file (shared shard). A shared shard holds all entries whose prefix has not yet been split out into a dedicated file.

### Appending new cold entries

When `omemfs pack` moves newly-unreachable entries into the cold tier, it
appends them to the shared shard (the shard referenced by slot 0) and
rewrites that shard under a new hash. Because a rewrite changes the shard's
hash, every index root slot that pointed at the *old* shared shard hash must
be repointed at the *new* one.

The update MUST be scoped to exactly the slots whose value equals the old
shared shard hash (the value slot 0 held **before** this rewrite):

1. Record the old shared shard hash (`cold_shard[0]` as read at the start of
   this operation, or the all-zero placeholder if no shared shard exists
   yet) before writing the new shard.
2. Write the new shared shard (old entries + newly appended entries) and
   obtain its hash.
3. For every slot `p` in `cold_shard[]`: update `cold_shard[p]` to the new
   shared shard hash **only if** `cold_shard[p]` currently equals the old
   shared shard hash recorded in step 1.

Slots that already point to a **dedicated shard** (i.e. a shard produced by
"Cold shard splitting" below, whose hash therefore differs from the old
shared shard hash) MUST NOT be touched by this step — a dedicated shard's
entries are not part of the shared shard being rewritten, and overwriting
its slot with the new shared shard hash would make those entries
unreachable from the index root (the shard file itself would remain on the
remote, but nothing would reference it any more).

### Cold shard splitting

On each `omemfs pack` run, if any cold shard file exceeds 4 MiB:

1. Load the largest cold shard file.
2. Select the most populous hash prefix `p` within it. The prefix is the **first byte** (8 bits) of the hash, regardless of the current `cold_prefix_bits`.
3. Extract all entries with prefix `p` into a new dedicated shard file (2 files written).
4. Write a new shared shard file containing the remaining entries.
5. Update the index root: `cold_shard[p]` → new dedicated shard hash; all other slots pointing to the old shared shard → new shared shard hash.

This produces exactly **2 new shard files** per `omemfs pack` invocation, regardless of the total number of entries.

**Raising `cold_prefix_bits` to 8**:

Splitting addresses slots by the first hash byte, so `cold_prefix_bits` must be at least 8 before the first split. If it is lower (it is 0 on a fresh repository), it is raised **directly to 8** at that point rather than in steps: each existing slot is replicated into `2^(8 - old_bits)` slots pointing to the same shard hash, so no new shard files are needed and only the index root is rewritten.

8 is the maximum the writer produces. Once it is reached, further splits only reassign slots within the 256-slot array. A shared shard that is still over 4 MiB when every slot already has a dedicated shard cannot be split further and simply grows. (The reader handles any `cold_prefix_bits`; only the writer is limited to 8.)

### Pack file consolidation (`omemfs pack`)

On each `omemfs pack` run, small pack files referenced by the hot index are consolidated into larger files to reduce the number of S3 GET requests required on the read path.

**Consolidation threshold**: pack files smaller than **2 MiB** are candidates for consolidation.

**Consolidation procedure**:

1. Collect all pack file hashes referenced by hot index pack entries.
2. Identify candidate pack files whose stored size is less than 2 MiB.
3. Fetch the encrypted bytes for each candidate pack entry from the candidate files.
4. Write the bytes into new pack files, targeting **4 MiB** per file (same target as push). When the buffer exceeds 4 MiB, flush the current file and start a new one.
5. Update pack entry fields (`pack_hash`, `offset`, `length`) in the hot index to point to the new pack files.
6. The new hot index and new pack files are written before the index root is updated.

Pack files referenced by cold shards are **not** consolidated (cold objects are infrequently accessed; the cost savings are small and cold shard index files would need to be rewritten).

A pack file that remains after consolidation with a size between 2–4 MiB is left as-is. It will not be re-consolidated in future `omemfs pack` runs (since it is at or above the 2 MiB threshold), and this is acceptable — the file is large enough that per-object GET cost is reasonable.

**Reclaiming unreferenced objects**:

Unreferenced objects (left by consolidation, cold-shard splitting, and Bloom-filter regeneration) are **not** deleted in place. Storage is reclaimed instead via the backup-reclone cycle: add a backup remote, push to it (a backup push copies only objects reachable from `clone_root`, so orphans are naturally dropped), then adopt the backup as the new origin. This flow is already described in the disaster-recovery section of the CLI spec.

### Index file local caching (read and push paths)

Index files (delta, hot, cold shards) are content-addressed and immutable: a
given hash always maps to exactly the same plaintext bytes. Both `PackReader`
(pull / expand) and `PackWriter` (push) therefore cache every index file they
load in `.omemfs/objcache/` as plaintext. The cache check happens before the
remote fetch; on a cache hit the object is deserialized directly from local
disk.

**`PackReader::load_index_file`** — used on the read path: check local cache
first; on miss, fetch from remote → decrypt → write plaintext to local cache →
deserialize. Within one reader snapshot it also retains the parsed index in
memory. Concurrent first loads of the same index hash are single-flight: one
caller performs the fetch/decode and all other callers wait for, then reuse,
that result. This prevents duplicate GETs or repeated deserialization when a
parallel planner reaches the same immutable index file.

**`PackWriter::load_index_file`** — used during the push dedup check
(`exists()` → `index_contains()`): identical local-first + cache-on-miss
logic. Without this cache, repeated pushes re-fetch all delta index files from
the remote on every push (reads grow linearly with the accumulated delta count,
e.g. 24→79 reads over 6 pushes). With the cache, subsequent pushes find the
same delta indexes in `.omemfs/objects/` and issue zero remote GETs for them.
This disk cache avoids a remote fetch, but it does not by itself avoid
re-deserialising the cached bytes on every call — see "Index lookup cost on
the push path" below.

On the **push** path, the Bloom filter itself is not cached this way: it is
loaded once per `PackWriter` construction (`load_remote_bloom`, a single
fetch-and-decrypt) and held resident in memory (`Mutex<BloomFilter>`) for the
whole push run, since every object-existence check during that run consults
the same in-memory instance. Its hash also changes on every push (`finish()`
inserts the run's new hashes and writes the result as a new CAS object — see
"Push flow with Bloom filter" below), so a disk cache entry keyed by the old
hash would never be looked up again; a single in-memory load per push is
already the cheapest possible access pattern, and adding an `objects/` cache
entry on top of it would only add cache-directory churn for no benefit.

On the **read** path, the access pattern is different, which is why the read
path *does* cache the Bloom filter this way (see "Read path" below,
Improvement C): `PackReader::locate` may consult the Bloom filter once per
resolved hash, and a single `pull` or `expand` run resolves many hashes over
its lifetime — there is no equivalent single long-lived in-memory instance to
reuse across those calls the way `PackWriter` has. Re-fetching and
re-decrypting the same Bloom filter bytes from the remote for every hash
lookup would be far more expensive than the push path's one-time load, so the
read path fetches the Bloom filter once, decrypts it once, and caches the
plaintext in `objects/` under its own hash — identical to how delta, hot, and
cold index files are already cached. This also means a Bloom filter fetched
during one command invocation stays cached for a later invocation (e.g. a
second `expand` run), as long as no intervening `omemfs pack` has replaced it
with a new one under a new hash.

### Read path

This section describes `PackReader::locate` in detail. It is the same algorithm
as "Snapshot read path" above, with the order in which the cold-layer steps run.

```
load(target_hash):
  1. Check local objects/ cache — return immediately on hit.
  2. locate(target_hash) — described below — to classify the hash as
     Entry / NotFound.
  3. Resolve the classification to bytes per "On index hit" below.

locate(target_hash):
  1. Use the index root retained at reader construction (if there is none,
     the remote has never been pushed to: fall back to a direct fetch of
     objects/<storage_key>).
  2. Search delta files newest-first (binary search within each).
     → found: Entry (inline or pack).
  3. Binary search the hot index.
     → found: Entry.
  4. [Improvement A] Compute p = first cold_prefix_bits bits of target_hash;
     let shard = cold_shard[p]. If shard is already present in the local
     objcache/ (a purely local existence check, no network call):
       Search it now.
       → found: Entry. Done.
       → not found: NotFound. Done (step 6 would search this same shard).
     Else (shard not yet cached locally): continue to step 5.
  5. [Improvement C] If a Bloom filter is recorded for this snapshot
     (index root's bloom_hash is set), it is structurally usable, and, once
     loaded (objcache/, or fetch + decrypt + cache on first use — see above),
     it reports target_hash as "definitely absent":
       → NotFound. Step 6 (the cold-shard fetch) is skipped.
     Otherwise ("maybe present", no Bloom filter recorded, or a structurally
     defective filter that is ignored): continue to step 6.
  6. Fetch cold_shard[p] from remote, cache it in objcache/, and binary
     search it.
     → found: Entry.
     → not found: NotFound.
```

There is **no per-object remote `HEAD`** in `locate`. A hash that is in none of
the snapshot's indexes is `NotFound`, even if an unindexed standalone object
with that key happens to sit on the remote. That is by design: a successful
push publishes an index entry for every object reachable from its root before
the index-root CAS (see "Snapshot read path" and "Membership test semantics
(push path)" below), so an object that matters is always in the snapshot. The
only remote `HEAD` on the read side is the diagnostic lookup that `omemfs cat`
runs for a full hash after `locate` reports `NotFound`.

```
On index hit:
  inline entry     → return data bytes directly (already encrypted; caller decrypts).
  pack entry       → if .omemfs/packcache/<pack_hash> absent, fetch the whole pack from remote once
                     and stream it into packcache/; subsequent slices for the same pack are served
                     locally → slice at [offset, offset+length) → return bytes.
                     (Reduces remote GET for packed-object reads to one fetch per pack.)
  standalone entry → fetch objects/<storage_key> directly from remote.
```

**Untrusted-remote validation of pack entry `length`**: `offset` and `length`
come from an index-file `PackEntry` (see "Index file format" above), which on
an unencrypted or compromised remote is not authenticated before this slice
read runs. Before allocating the slice buffer, `fetch_pack_slice` caps
`length` at the size of the already-locally-cached pack file (`packcache`
holds the whole pack body once fetched, so its stat'd byte length is known
without any extra remote call). A `length` that would read past the end of
the cached pack — whether because it is individually absurd or because
`offset + length` overruns the file — is rejected with
`Error::InvalidObject` before the `vec![0u8; length as usize]` allocation, so
a hostile or corrupted index entry cannot force a multi-gigabyte allocation
(up to 4 GiB, the `u32` maximum) merely by writing a large `length` field.
This bound is exact (the real pack file size) rather than a fixed constant,
because the pack is always fetched and cached in full before any slice is
served (see the `pack entry` row above), so its true size is always known by
the time a slice length needs checking.

**Why the covering cold shard is searched before the Bloom filter (Improvement A)**

Once a shard has been fetched at least once it lives in `objcache/` forever, because index files are content-addressed and immutable (see "Index file local caching" above). A later search of it is then a local disk read, cheaper than loading and consulting the Bloom filter, for any hash the shard covers, whether the search hits or misses. Step 4 therefore checks whether the covering shard is already cached, which is a purely local existence check, and searches it immediately. A shard that has never been fetched is left alone, so the first lookup for it goes on to the Bloom check and, only if that cannot rule the hash out, the shard download.

**Why the Bloom filter can skip the cold-shard fetch (Improvement C)**

The Bloom filter and the cold shards are both built from the same `omemfs pack` snapshot, by the same run, so a "definitely absent" answer is exact for "not in any cold shard". False negatives are impossible by construction, so skipping the shard download on a definite miss never loses an object. A "maybe present" answer (a false positive, or a hash in a category the filter cannot narrow down), no filter recorded at all (`bloom_hash_opt()` returns `None`: a remote that has never run `omemfs pack`), or a structurally defective filter (see "Structural validation (push and read paths)") falls through to the shard fetch.

**Cost only, never classification**

Improvements A and C change *whether and when* network calls happen while resolving a hash, never *what* `locate` returns: the answer always comes from the delta, hot, and cold indexes of one snapshot.


### Bloom filter

The pack stage maintains a Bloom filter over **all** known remote object hashes — inline, pack, and standalone. It is used during push to quickly determine whether an object with the same hash has already been pushed before, avoiding redundant writes; it is also consulted on the read path (see "Read path" above, Improvement C) to recognise a genuinely-absent hash without a cold-shard fetch.

**Membership test semantics (push path)**

- `"definitely absent"` → the object has never been pushed; include it in the push batch without further checks.
- `"maybe present"` → the object may already exist. Consult the **locally cached index first** (delta → hot → cold shard via `PackWriter::load_index_file`, which serves immutable index files from `.omemfs/objects/` without remote I/O on a cache hit).
  - Found in the index → present; skip re-uploading it.
  - **Not found in the index → treated as absent**, unconditionally, even though a standalone object with this exact hash might already sit on the remote under its storage key (e.g. left behind by an earlier push that was interrupted after uploading the bytes but before publishing the index entry — the two are not atomic within a single push; only the final CAS-write of `INDEX_ROOT` in `finish()` is). Reporting "absent" here forces `route()` to run, which unconditionally appends an index entry for the hash. This is what guarantees the push-completion invariant in "Snapshot read path" above (every object reachable from a pushed root is resolvable through the snapshot index): an object's bytes reaching the remote is not sufficient on its own — only a published index entry is.

    This does not cost a redundant upload: `route()`'s underlying write (`ObjectsBackend::write_stream` / `CloudObjects::write_stream`) is itself content-addressed and idempotent — it checks for the storage key's existence and skips the actual PUT/file-write when the bytes are already there. So the only added cost of always treating an index-miss as absent is that one idempotent write call (which resolves to a single existence check when the bytes already exist), never a second full upload.

  (An earlier version of this membership test issued a `HEAD objects/<storage_key>` request on an index miss and reported "present" on a hit, precisely to avoid that idempotent-write call for the common case of an object already pushed but not yet indexed. This was a bug, not an optimisation: it let a standalone object that is physically on the remote but missing from every index tier — the orphan case above — be reported "present" forever, since `route()` (the only place that appends an index entry) was then never invoked for it. Once orphaned this way, the object stayed permanently unindexed and unresolvable through SnapshotOnly reads, surviving indefinitely across every later push that happened to touch it, because each one repeated the same wrong "present" answer.)

False negatives are impossible by design. False positives trigger an unnecessary index lookup but never cause data loss or a missing index entry.

**Membership test semantics (read path, Improvement C)** — see "Read path"
above for the full step ordering. `"definitely absent"` short-circuits `locate`
straight to `NotFound` and skips the cold-shard fetch. This is safe because the
Bloom filter and the cold shards share one `omemfs pack` snapshot with no
staleness gap between them, and because a read never looks outside that
snapshot (there is no per-object remote `HEAD`). `"maybe present"`, or no Bloom
filter recorded, falls through to the cold-shard search exactly as `locate`
would without a Bloom filter at all. The read path's use is strictly a fast
path for skipping an unnecessary shard download; unlike the push path, it never
substitutes for the index lookup when the filter cannot rule an object out.

**Bloom filter file format** (stored as a CAS object, magic `ED E4`):

The Bloom filter is encrypted with AES-256-GCM using the same DEK as regular objects. The nonce is derived from the file's own SHA-256 hash (`nonce = hash[0..12]`), consistent with the rule used for regular objects and index files. The hash is computed over the plaintext content below; the remote backend stores only the ciphertext.

Plaintext format:

```
magic             : 2 bytes   (ED E4)
version           : 1 byte    (0x01)
num_hash_functions: 1 byte    (recommended: 7)
num_bits          : 8 bytes   (big-endian, u64)
element_count     : 8 bytes   (big-endian, u64 — for fill-rate monitoring and capacity estimation; an upper bound on the distinct element count, since re-inserting an already-present hash still increments it)
bits              : ceil(num_bits / 8) bytes
```

The index root's `bloom_hash` field holds the hash of the current Bloom filter file. An all-zero `bloom_hash` indicates that no Bloom filter has been generated yet; in that case the full index lookup is used unconditionally.

Because `num_hash_functions` and `num_bits` travel with the filter itself, a
Bloom filter is **self-describing**: `insert`/`may_contain` only ever read
`self`'s own fields, never an external "expected size" setting. This is what
makes the acceptance rule in "Filter sizing" below possible — a filter's
membership answers are correct regardless of what capacity it happens to have
been built for.

**Size at p = 1% false-positive rate**

| Element count | Bloom filter size |
|---------------|-------------------|
| 100 K | ~120 KiB |
| 1 M | ~1.2 MiB |
| 10 M | ~12 MiB |
| 100 M | ~114 MiB |

The final row is not just the largest table entry — it is the hard ceiling
`capacity_for` will ever target. See `MAX_CAPACITY` in "Bounding the capacity
(hostile and corrupt filters)" below.

**Push flow with Bloom filter**

```
push:
  1. Fetch the index root → load Bloom filter (local cache or S3 GET). Any
     structurally valid filter is accepted regardless of its size relative to
     the current design target — see "Filter sizing" below. A genuine
     absence (no bloom_hash, or an "object not found" fetch result) falls
     back to a fresh empty filter; any other fetch/decrypt/deserialise
     failure aborts the push instead of silently substituting one — see
     "Filter sizing" below ("Genuine absence vs. an unreadable filter").
  2. For each object to push:
       Bloom filter → "definitely absent" → include in push batch.
       Bloom filter → "maybe present"     → verify via index / HEAD; skip if already present.
  3. Write pack files and delta index for the new objects.
  4. Add new object hashes to the Bloom filter → write new Bloom filter as a CAS object.
  5. CAS-update the index root (delta_hash[] and bloom_hash updated atomically).
```

**Maintenance**

`omemfs pack` regenerates the Bloom filter from scratch (all entries in hot index, cold shards, and standalone objects) to eliminate accumulated false positives. `omemfs pack` is also the **only** operation that resizes the filter (see "Filter sizing" below); push only ever appends to whatever filter it loaded. The old Bloom filter file becomes unreferenced and is reclaimed only via the backup-reclone cycle described above.

### Filter sizing

**Push never rebuilds an empty filter merely because the loaded filter's size
does not match the current design target.** A structurally valid filter —
`num_bits >= 1` and `num_hash_functions >= 1`, successfully fetched, decrypted,
and deserialised — is accepted and used as-is, whatever its `(num_bits,
num_hash_functions)` happen to be. A fresh (empty) filter is only constructed
when there is nothing valid to load because the filter **genuinely does not
exist yet**: no snapshot at all, an all-zero `bloom_hash`, or the Bloom
object fetch failing with "object not found" — or when what was fetched is
structurally unusable (`num_bits == 0`, which would divide by zero inside
`may_contain`, or `num_hash_functions == 0`, which is well-defined but
useless).

This is a deliberate narrowing of the older rule ("rebuild on any size
mismatch"), which had a sharp edge: rebuilding an empty filter for a remote
that already holds a large number of objects makes every one of them look
"definitely absent" until the next `pack`, so it gets re-uploaded. Accepting
any structurally valid filter regardless of size removes that edge entirely —
there is no longer a "resize the target" step that can trigger it — while
changing nothing about correctness, since membership answers never depended on
matching the local target in the first place.

**Genuine absence vs. an unreadable filter — these are not the same failure
and must not be handled the same way.** A missing `bloom_hash` (or a fetch
that fails with "object not found") means the remote has no Bloom filter yet
— a fresh empty filter is the correct, harmless answer, and this is expected
on a brand-new repository or one whose only `pack` predates Bloom filter
support. But once `bloom_hash` names an object that is *supposed* to exist,
any other failure while obtaining it — a transient I/O or network error, an
authentication failure, a GCM decrypt failure (wrong key or corrupted
ciphertext), or a deserialisation failure (corrupted or truncated plaintext)
— means the remote's actual Bloom filter state could not be determined at
all. Silently substituting a fresh empty filter in that case is unsafe: for
the rest of that push, every cold-tier object the real filter would have
reported "maybe present" instead reports "definitely absent" and gets
re-uploaded and re-inserted into the *published* filter, corrupting it for
every subsequent reader until the next `omemfs pack` rebuilds it from
scratch — a false negative disguised as a routine cache miss, breaking the
no-false-negatives invariant the rest of this design section relies on (see
"Bloom filter" above). Such a push must instead fail outright, surfacing the
underlying error to the caller, so a transient or persistent problem is
visible and never silently masked by a filter reset.

`load_remote_bloom` therefore distinguishes exactly these two outcomes:

- **`bloom_hash` unset, or the fetch fails with `Error::ObjectNotFound`**
  (the object genuinely does not exist on the remote) → fall back to a
  fresh empty filter, sized from whatever capacity hint is available (see
  below).
- **Any other failure** while reading the object that `bloom_hash` names —
  a non-`ObjectNotFound` error from the fetch (I/O, auth, or any other
  backend error), a `read_to_end` failure, a `decrypt` failure, or a
  `BloomFilter::deserialise` failure — → propagate the error; `PackWriter`
  construction fails and the push does not proceed.
- A **structurally unusable but successfully decoded** filter (`num_bits ==
  0` or `num_hash_functions == 0`) is a separate, distinct case: the object
  was read, decrypted, and deserialised without error, so this is not
  treated as "unreadable" — it falls back to a fresh filter sized from the
  decoded filter's `element_count`, per the existing rule above.

**Choosing a capacity (when a fresh filter must be built).** The designed
capacity for `n` known elements is:

```
capacity_for(n) = clamp(ladder_ceil(clamp(n, 1, MAX_CAPACITY) * 3 / 2), MIN_CAPACITY, MAX_CAPACITY)
ladder = {1, 2, 5} × 10^k   (100_000, 200_000, 500_000, 1_000_000, 2_000_000, ...)
```

at a fixed false-positive target `FP_RATE = 0.01` (unchanged — see "Rejected
alternatives" below for why `p` itself is not made a knob). The ×1.5 margin
means a filter stays at its designed false-positive rate until the element
count it was built for grows by 50% — since only `omemfs pack` resizes,
that's "until the next `pack` after a 50% growth in objects," not "until the
next push." The 1-2-5 ladder keeps `pack` idempotent (re-running it against an
unchanged element count reproduces the same `num_bits`) and bounds the
over-provisioning from quantisation to at most ~1.67×.

`capacity_for(n) >= n` holds only for `n <= MAX_CAPACITY`. Above that bound the
result is intentionally under-provisioned: `n` is clamped down before sizing,
so the filter is deliberately built smaller than its true element count would
call for. This degrades the filter's false-positive rate gradually as
`element_count` grows past `MAX_CAPACITY` — exactly like any other
over-capacity filter (see "Fill-rate reporting" below) — and never produces a
false negative, since membership answers stay exact by construction
regardless of how the filter was sized. See "Bounding the capacity (hostile
and corrupt filters)" below for why the clamp exists and where it lives.

- **`omemfs pack`** (`regenerate_bloom`) knows the exact element count it is
  about to insert (hot index entries + cold shard entries) and calls
  `capacity_for` directly.
- **Push's fallback fresh filter** (reached only in the narrow cases above) has
  no exact count to hand. When a filter *was* read but rejected for being
  structurally unusable, its `element_count` field is used as the estimate
  (`capacity_for(loaded.element_count)`); the fill-rate/capacity field described
  above is repurposed for this. When no filter could be read at all, the
  fallback is `MIN_CAPACITY` — the same size this constant has always produced.
  `loaded.element_count` here is an 8-byte field inside an object that whoever
  wrote the remote Bloom filter fully controls, and it survives
  fetch/decrypt/deserialise with no range check of its own (see "Bounding the
  capacity" below) — a corrupt or hostile filter can set it to any `u64`, up to
  and including values that would demand terabytes of memory if used
  unclamped as a capacity hint. `load_remote_bloom` passes it through to
  `capacity_for` unclamped; the bound lives inside `capacity_for` itself, not
  at this call site.

**Bounding the capacity (hostile and corrupt filters).** `element_count`
travels in the Bloom filter's plaintext header as a plain 8-byte big-endian
`u64` (see "Bloom filter file format" above) with no range check at
deserialise time — `deserialise` only checks the fixed 20-byte header and that
the bit array length matches `num_bits`. A 20-byte object with `num_bits = 0`
and `element_count = 10^12` therefore deserialises without error, and handing
that `element_count` straight to `capacity_for` as a capacity hint would try
to allocate a bit array sized for roughly 1.6 TiB — aborting the process on
allocation failure well before `omemfs` gets a chance to report an error.

`capacity_for` clamps its input to `MAX_CAPACITY = 100_000_000` — the element
count of the largest row in the "Size at p = 1% false-positive rate" table
above (~114 MiB) — before applying the ×1.5 margin, and clamps its *output* to
the same bound after `ladder_ceil`, per the formula above. Both clamps matter:
the input clamp keeps the ×1.5 multiplication (and the internal `u128 ->
u64` truncation inside `ladder_ceil`) from ever seeing a value anywhere near
`u64::MAX`; the output clamp guarantees `capacity_for` never returns more than
`MAX_CAPACITY` even though `ladder_ceil` alone can round up past it (e.g.
`ladder_ceil(150_000_000) = 200_000_000`). Clipping to `MAX_CAPACITY` — rather
than erroring, or falling back to `MIN_CAPACITY` — preserves `capacity_for`'s
monotonic-non-decreasing property across its entire `u64` domain, which both
the acceptance rule above and `omemfs pack`'s idempotency depend on: an error
would turn a corrupt Bloom filter into a push-blocking failure for no
correctness benefit, and falling back to `MIN_CAPACITY` would make
`capacity_for` non-monotonic (a huge but bogus element count would size the
filter *smaller* than a merely large one).

Both clamps live inside `capacity_for` itself, not in its callers
(`load_remote_bloom` in `writer.rs`, `regenerate_bloom` in `pack.rs`) — the
same "one place owns the sizing formula" rule this section already establishes
for the rest of the formula. Neither caller needs to know its input might be
hostile.

This sizing logic lives in one place — `codec::pack::bloom` (constants
`FP_RATE`, `MIN_CAPACITY`, and the `capacity_for`/`sized_for` functions) —
rather than being duplicated between the push path and `omemfs pack`, since the
Bloom file format is owned by that module and both callers already depend on
it. Unlike `PACK_TARGET`/`PACK_MAX` (a pack-shape convention that happens to be
shared by two call sites), a divergence here would matter even after the
size-mismatch rejection is removed: `capacity_for` is also what a diagnostic
tool would use to report how far a filter is from its designed capacity (see
"Fill-rate reporting" below), so it must mean the same thing everywhere it is
read as well as everywhere it is written.

**Safe migration.** The size-mismatch rejection removal (this section) and the
dynamic sizing (the formula above) must ship together, or in that order — never
sizing first without the rejection-removal, since a fluctuating per-push target
computed from a smoothly growing element count would otherwise trigger the
same empty-filter rebuild on every push that crosses a threshold. With the
rejection removed first (or simultaneously), the very first push after
upgrading simply accepts whatever filter is already on the remote — including
one sized by the old fixed `100_000` constant — with no rebuild and no
re-upload. The next `omemfs pack` then resizes it, using the formula above,
from the actual element count observed at that time.

**Structural validation (push and read paths).** A single predicate,
`BloomFilter::structural_defect()`, decides whether a successfully
fetched/decrypted/deserialised Bloom filter is safe to use as-is: it returns
`Some("num_bits == 0")` when the bit array would make `may_contain`/`insert`
divide by zero, `Some("num_hash_functions == 0")` when hashing would be a
no-op (well-defined but useless), and `None` otherwise. Both the push path
(`load_remote_bloom` in `writer.rs`) and the read path (`locate`'s Bloom step
in `reader.rs`) call this one predicate instead of each repeating the
`num_bits == 0 || num_hash_functions == 0` condition inline.

This draws a deliberate line between two different kinds of failure:

- *"Could not be interpreted at all"* — fetch failure, decrypt failure, or
  `BloomFilter::deserialise` returning `Err` (bad magic, wrong version,
  truncated, bit-array length mismatch). Unchanged by this section: `writer.rs`
  falls back to a fresh filter as it always did, and `reader.rs`'s `?` still
  propagates the error to the caller, because the object itself cannot be
  trusted to mean anything.
- *"Interpreted successfully but structurally degenerate"* — `deserialise`
  returns `Ok`, but `structural_defect()` is `Some(..)`. Only this case is new
  fail-open behaviour, and the two paths respond to it differently because
  they have different failure-safe defaults:
  - **Push** (`load_remote_bloom`) replaces the filter with a fresh empty one
    sized from `capacity_for(loaded.element_count)` (bounded per the previous
    section), exactly as it did before `structural_defect()` existed. The push
    proceeds as if the remote had no Bloom filter yet, at the cost of a
    possible spurious re-upload of already-present objects on this one push.
  - **Read** (`locate`) treats a structurally defective filter the same as "no
    Bloom filter recorded at all" (`bloom_hash_opt() == None`): it skips the
    Bloom step entirely and falls through to the cold-shard fetch. "Maybe
    present" is always a safe answer for a Bloom filter to give, so falling
    through is the correct degenerate behaviour — there is nothing to replace
    the filter with on the read path (`PackReader` never writes), and skipping
    the check costs one shard fetch, not correctness.

As a last-resort backstop against a defect predicate that is ever wrong, or a
filter that reaches `insert`/`may_contain` some other way without going
through either loader, `may_contain` itself returns `true` (fail-open: "maybe
present", never a false negative) when `num_bits == 0`, instead of dividing by
zero; `insert` returns immediately without touching the bit array or
`element_count` under the same condition. These are defensive backstops, not
the primary fix — the primary fix is that `structural_defect()` keeps a
defective filter from reaching `insert`/`may_contain` on the push path in the
first place, and keeps `locate` from calling `may_contain` on one at all on
the read path.

**Fill-rate reporting.** Because `element_count` is already recorded and
`capacity_for`'s bit computation can be inverted to recover the capacity a
given `num_bits` was designed for, a loaded filter's fill state can be reported
without any format change: `omemfs cat <bloom-hash>` and `omemfs stats` can
show `element_count` against the filter's designed capacity and an estimated
current false-positive rate, and `push` can emit a one-line warning when a
filter is over capacity, suggesting `omemfs pack`. Reaching `MAX_CAPACITY` is a
ceiling `omemfs pack` cannot resize past: once a repository's live element
count exceeds `MAX_CAPACITY`, `pack`'s `capacity_for` call clips to
`MAX_CAPACITY` the same way the push fallback does, so the filter's
false-positive rate keeps degrading gently with further growth rather than
being fixed by any operation available today — see "Rejected alternatives"
below for the layered/scalable filter design that would lift this ceiling.

`omemfs stats`'s "Pack effectiveness" section reports this via the
`bloom_num_bits` field on the `pack_detail` record in `io_stats.jsonl` (see
`io_stats.rs`). `io_stats.jsonl` records written before that field existed lack
the key and deserialise it to `0` via serde's default. Since `sized_for`
guarantees a real filter always has `num_bits >= 1`, `bloom_num_bits == 0`
unambiguously means "not recorded by this run," not "an actual zero-bit
filter." `omemfs stats` (both the text and `--json` output) must therefore
treat `bloom_num_bits == 0` as "unknown" and omit the designed-capacity and
estimated-false-positive-rate figures entirely, rather than computing and
displaying `designed_capacity(0) = 0` and the hardcoded worst-case
`estimated_fp_rate(.., 0, ..) = 1.0`, which would misreport a healthy filter as
zero-capacity and 100% false-positive.

**Rejected alternatives**:

- *Raise the fixed constant (e.g. 100 000 → 500 000) without changing the
  rejection rule.* Only pushes the same cliff further out and leaves it
  reachable by any repository that keeps growing, or by an older client
  pushing against a filter a newer client resized upward.
- *Resize opportunistically inside push's `finish()`* by re-reading the full
  snapshot index and re-inserting every hash. Technically possible (`finish()`
  already has snapshot access) but loads an unbounded amount of index/cold-shard
  data into every push and duplicates `omemfs pack`'s own responsibility; also
  lengthens the time the CAS token from push-start remains valid against.
- *Layered/scalable Bloom filters* (stack a new filter and OR the answers) would
  let push grow capacity without a format version bump in the *logic*, but
  does require storing a list of filters instead of one, which is a format
  change. Left as a future option if resizing only at `pack` time proves
  insufficient in practice.
- *Lower `FP_RATE` instead of sizing dynamically.* `DEFAULT_NUM_HASH_FUNCTIONS =
  7` is the optimal hash count for `p = 0.01`; moving `p` alone without
  recomputing `k` stops being optimal, and the cost of the current 1%-target
  false positive (one extra index lookup, never data loss) does not justify
  the larger filters a materially lower `p` would need.
- *Reject an oversized `element_count` inside `deserialise` itself.* Would
  conflate "the bytes are well-formed" with "the hint they carry is
  plausible," turning a merely-large-but-legitimate repository's Bloom filter
  into a hard deserialise error instead of a degraded-but-working filter, and
  would need its own bound (duplicating `MAX_CAPACITY`) at a layer that has no
  other opinion about filter sizing today.
- *Treat over-`MAX_CAPACITY` as an error, or clamp down to `MIN_CAPACITY`,
  instead of clipping to `MAX_CAPACITY`.* An error blocks a push for a
  condition that never risks data loss — only a wider false-positive margin —
  and clamping to `MIN_CAPACITY` breaks `capacity_for`'s monotonicity (a
  corrupt-but-huge `element_count` would produce a *smaller* filter than a
  legitimately large one), which "Filter sizing" above and `omemfs pack`'s
  idempotency both rely on.
- *Drop the `element_count` hint entirely and always fall back to
  `MIN_CAPACITY` when a filter must be rebuilt.* Simpler, but throws away a
  genuinely useful signal for the common case (a filter rejected for
  `num_hash_functions == 0` on an otherwise-healthy, large repository) purely
  to defend against the rare hostile/corrupt case that `MAX_CAPACITY` clamping
  already handles without losing that signal.

**Known incompatibility.** After `omemfs pack` has resized a filter upward, a
push from an older binary that still checks against the fixed `100_000`
target would see a size mismatch and (under the old rejection rule) rebuild an
empty filter, re-uploading everything reachable. This is an accepted,
documented incompatibility between old and new binaries against the same
remote — consistent with this project's "pre-production, compatibility breaks
are acceptable" stance (see STATUS.md) — not a runtime check.

### Index lookup cost on the push path (`PackWriter`)

`PackReader` (the read path) keeps every index file it has parsed resident in
memory for the lifetime of one snapshot (`indexes: Mutex<HashMap<Hash,
Arc<IndexFile>>>` — see "Snapshot read path" above), so a given index is
deserialised at most once per reader, and single-flights concurrent
cache-miss callers through an `index_load: Mutex<()>` guard so that a burst of
parallel transfer workers racing to look up the same not-yet-cached hash
fetches and deserialises it once, not once per racing thread.
`PackWriter::load_index_file`, used by `index_contains` for the "maybe
present" branch above, carries the same two properties for the same reason:
`route()` drives it from multiple parallel transfer worker threads (design/02
"Parallel transfer and memory"), exactly like `PackReader`.

- **Resident cache** (`indexes: Mutex<HashMap<Hash, Arc<IndexFile>>>`).
  Without it, every "maybe present" check against the hot index (true
  positives included, not just false positives) would pay a full
  deserialisation of the already-decrypted-and-disk-cached bytes
  (`.omemfs/objcache/`, per "Index file local caching" below) on every call,
  independent of the Bloom filter's false-positive rate — fixing the Bloom
  filter's capacity narrows how often "maybe present" is reached at all, but
  does not remove this per-call cost once it is.
- **Single-flight guard** (`index_load: Mutex<()>`, mirroring
  `PackReader::index_load`). Without it, the resident cache alone does not
  prevent a race: several threads can all observe a cache miss on
  `self.indexes` for the same hash before any of them has finished
  fetching+deserialising, and each independently repeats the fetch and the
  deserialisation. The guard serialises the fetch+deserialise path (checked
  again under the lock in case another caller just finished), so at most one
  caller per genuine cache miss pays that cost; every other caller — for that
  hash or a different one racing at the same time — blocks briefly and then
  either reuses the now-cached result or proceeds with its own miss.

Both are purely in-memory additions on top of the existing disk cache — they
change no on-disk or on-wire format.

---

## `omemfs stats` — object store statistics

`omemfs stats` reports a cloud-cost-oriented capacity breakdown of the remote object store (when the configured remote is a local directory) and a deep composition scan of the local object cache (`.omemfs/objects/`).

### Scope

| Remote type | Local cache (deep scan) | Remote objects (cheap LIST) | L6 pack layer |
|-------------|-------------------------|-----------------------------|---------------|
| `local`     | ✓ scanned + classified  | ✓ enumerated (sizes only)   | ✓ classified  |
| `s3`, `gcs`, `azure` | ✓ scanned + classified | — not shown | — not shown |

The **local cache** is read object-by-object and classified by type / compression / blob content type (the "Local cache composition" section).

The **remote** is NOT read object-by-object. Instead, `omemfs stats` performs a single cheap enumeration via `ObjectStore::list_with_sizes()` — which yields `(storage_key_hex, byte_size)` pairs with no per-object GET (a single paginated LIST on cloud backends, one directory walk + `fs::metadata` locally) — and classifies each storage key by comparing it against the known storage-key sets derived from the index root. No remote data-object contents are read. Reading the small index files (hot / delta / cold) and the Bloom filter to enumerate the known keys is expected and bounded.

### Remote classification (cheap LIST + index metadata)

The known storage-key sets are built from the index root:

- **pack-files** — storage keys of every `PackEntry.pack_hash` across the hot index, all delta indexes, and all cold shards.
- **index-files** — storage keys of the hot index hash, every `delta_hashes[]`, and every distinct cold shard hash.
- **bloom** — storage key of `bloom_hash`.
- **standalone-objects** — storage keys of every `StandaloneEntry.hash` across hot / delta / cold indexes.
- **index-root** — the index-root object itself. On encrypted remotes it lives under `objects/` (at the derived name) and so appears in the LIST; on unencrypted remotes it is the fixed `INDEX_ROOT` file at the prefix root, which is NOT under `objects/` and therefore does NOT appear in `list_with_sizes` — it is accounted for separately by stat-ing `index_root_path(remote_base, key)`.
- **orphans** — any listed key not in any known set. These are unreferenced objects (old pack files left behind by consolidation, superseded index files, old Bloom filters) reclaimable via the backup-reclone cycle.

The storage key for a hash is derived as `HMAC-SHA256(DEK, hash)` when encryption is configured, otherwise the hash itself (`ObjectStore::storage_key_of`).

Pack files begin with `ED E1` and are never encrypted, but the remote classification above does not rely on inspecting their bytes — it matches storage keys against the index-derived sets, so it works identically for encrypted index/bloom files whose ciphertext does not start with the expected magic.

### Local cache composition (deep scan)

Local cache stored bytes are classified by inspecting their leading two bytes before decompression (L4 header), and after decompression (L2/L3 type tag).

**Before decompression (L4 header)**

| Leading bytes | Classification |
|---------------|---------------|
| `ED E0`       | standalone escape — strip 2-byte prefix and classify inner bytes |
| `ED E1`       | L6 pack file (identified here; not decompressed) |
| `ED DE`       | L4 dict-zstd compressed — decompress for L2/L3 classification |
| `ED DF`       | L4 plain-zstd compressed — decompress for L2/L3 classification |
| `ED D0`       | L4 escaped raw — strip prefix for L2/L3 classification |
| anything else | raw — use as-is for L2/L3 classification |

**After decompression (L2/L3 type tag)**

| Leading bytes | Classification |
|---------------|---------------|
| `ED F1`       | L2 tree |
| `ED F2`       | L3 chunk-manifest |
| `ED F3`       | L3 chunk-body |
| `ED F0`       | L2 blob (escaped) |
| anything else | L2 blob (raw) |

---

## L7 (sto): store / load

**Responsibility**: write bytes to and read bytes from a physical storage backend.

The storage backend is addressed by object hash. It does not interpret the bytes it holds.

### ObjectStore trait

```rust
pub trait ObjectStore: Send + Sync {
    fn exists(&self, hash: &Hash) -> Result<bool, Error>;

    /// Return the stored byte length of the object addressed by `hash`.
    /// Errors if the object does not exist.
    fn size(&self, hash: &Hash) -> Result<u64, Error>;

    /// List every stored object as (storage_key_hex, byte_size) pairs.
    ///
    /// This is the primary interface for remote storage capacity reporting.
    /// Cloud backends (S3/Azure/GCS) return both the object key and its size in a
    /// single paginated LIST response (ListObjectsV2 / List Blobs / Objects.list),
    /// so capacity can be computed with no per-object GET or HEAD request.
    /// `LocalStore` walks the objects directory and stats each file.
    /// Entries whose file is missing or unreadable are silently skipped (matching
    /// the tolerance of existing enumeration).
    /// The current return type is an eager `Vec`; a streaming/iterator variant may
    /// be introduced later for very large buckets.
    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error>;

    /// Open the stored (encoded) bytes for `hash` as a stream.
    /// Returns raw stored bytes (encrypted + compressed); decoding is the caller's responsibility.
    fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error>;

    /// Write bytes from `reader` into the store at address `hash`. Idempotent.
    /// `reader` provides the already-encoded (compressed + encrypted) bytes.
    fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error>;
}
```

- `exists` returns `true` if the object is present in the store.
- `size` returns the stored byte length of an object by hash. `LocalStore` reads the filesystem metadata of the storage-key path; the cloud backends use the byte length carried in a HEAD / get-properties / get-object response (S3 / Azure / GCS).
- `list_with_sizes` returns all stored objects as `(storage_key_hex, byte_size)` pairs. On cloud backends (S3/Azure/GCS) a single paginated LIST call already includes each object's size in the response, so capacity reporting requires no per-object HEAD. `LocalStore` enumerates the objects directory and calls `fs::metadata` for each entry.
- `open_read` returns a `Box<dyn Read>`. On the local backend the reader is backed by the storage file, so no bytes are loaded into memory until the caller reads from the stream. The cloud backends collect the object body into memory (a `Cursor<Bytes>`) via the async→sync bridge before returning the reader; this is bounded because stored objects are chunk-limited (≤16 MiB), and a future streaming adapter can remove the buffering for large packs (see `design/13_cloud_backends.md`).
- `write_from` (local backend) pipes `reader` directly into a `NamedTempFile` created in the same directory as the target object path (via `ObjectsDir::write_stream` / `RemoteObjectsDir::write_stream`), then atomically renames it to the final path. No intermediate copy through `.omemfs/tmp/` is performed. The cloud backends buffer the source into memory and issue a single PUT (multipart / staged upload above ~16 MiB); see `design/13_cloud_backends.md`.
- Pack-file consolidation reads whole candidate pack files via `open_read` rather than range-reading slices. This is possible because consolidation candidates are bounded below the 2 MiB consolidation threshold, so no range-read primitive is needed in the trait.
- The trait has **no** root-pointer methods. The only root pointer is the index root (stored at the derived key on encrypted remotes, at the fixed key `INDEX_ROOT` on unencrypted remotes), which is read through `PackReader::read_root` and written (CAS) through `PackWriter::finish`. There is no separate `REMOTE_ROOT` file in any backend, including backup. The index-root read and CAS-write go through a separate backend-pluggable `RootPointer` abstraction shared by `push` and `pack`; see the CAS-safety section in `design/03_sync_model.md` for the local-directory, S3, Azure, and GCS backend mappings, and `design/13_cloud_backends.md` for the per-backend SDK details.

### Local backend

Stores bytes under `.omemfs/objects/` using the adaptive-depth sharding scheme described in the [objects/ directory](#objects-directory) section above.

`LocalStore::open_read` opens the object file with `fs::File::open` and returns the file handle as a stream. `LocalStore::write_from` streams the reader directly into a `NamedTempFile` created inside the target shard subdirectory and renames it atomically. Local cache objects use `atomic_write_no_fsync` (no per-object fsync); a durability barrier is issued later, inside `write_clone_root` and `StatCache::write`, before the pointer files are persisted.

`LocalStore::open_read_by_storage_key(hex)` is a lower-level variant that bypasses the logical→storage-key derivation and opens the file directly by the provided storage-key hex. It is used by `omemfs stats` to read objects from encrypted remote stores where only the storage key (the filename) is known — not the original logical hash.

### Remote backend

Uses the fixed 3-level sharding layout under `<prefix>/objects/` as described in the [Remote backend layout](#remote-backend-layout) section above.

When encryption is configured, the physical path is derived from the _storage key_ (see [L5 (enc): encrypt / decrypt](#l5-enc-encrypt--decrypt) — Storage key derivation), not the logical hash:

```
path = objects/<storage_key[0..2]>/<storage_key[2..4]>/<storage_key[4..6]>/<storage_key[6..64]>
```

When encryption is not configured, the path uses the logical hash directly (identical to the storage key in that case).

`LocalStore` performs this mapping internally. Callers pass a logical hash to `exists`, `open_read`, and `write_from`; the storage key derivation is invisible to them.
