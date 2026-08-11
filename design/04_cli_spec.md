# CLI Specification

## Design principles

- No `add`, `stage`, `commit`, or `init`. The working directory is the truth; `push` commits and uploads in one step; `clone` handles repository initialisation.
- No untracked files. Every file under the working tree is managed.
- No commit messages. State capture is lightweight.
- Path arguments are relative to the current working directory.

## Global options

`omemfs --version` prints the program name followed by its Cargo package
version, then exits successfully. It does not require a repository or contact
a remote.

## Repository discovery

Every command except `clone` locates the repository by walking up from the current working directory: the first ancestor directory that contains a `.omemfs/` subdirectory is the **repository root**, and `.omemfs/` lives directly under it. The search continues to the filesystem root; if no `.omemfs/` is found in the current directory or any ancestor, the command fails with `not a omemfs repository (no .omemfs/ found in <cwd> or any parent)`. There is no environment-variable override.

Because of this, all commands may be run from any subdirectory of the working tree, exactly like `git`. Two locations are distinguished internally:

- **repository root** — the parent of `.omemfs/`. All tree-entry relative paths (the paths stored in objects and shown when the root is the cwd) are anchored here.
- **current working directory (cwd)** — where the command was invoked. Relative `<path>` arguments are resolved against the cwd, then re-expressed relative to the repository root before matching against tree entries. An absolute `<path>` is used as-is. A `<path>` that resolves outside the repository root is rejected with `path '<p>' is outside the repository '<root>'`.

When the cwd is the repository root, path resolution is identical to expressing every argument relative to the root, so behaviour is unchanged from running at the root.

`clone` is the sole exception: it creates a new repository at the destination directory and therefore performs no upward discovery.

## Implementation status

All four remote types are implemented: `local` (plain directory on the filesystem), `s3` (AWS S3 and S3-compatible stores), `gcs` (Google Cloud Storage), and `azure` (Azure Blob Storage). Every command (`clone`, `push`, `pull`, `cat`, `stats`, `expand`, `pack`) works against any of the four backends. The four backends share one synchronous `ObjectStore` trait and one `RootPointer` CAS abstraction; the cloud adapters confine their async SDK calls behind a shared Tokio runtime so async never leaks past the backend module. See `design/13_cloud_backends.md` for the per-backend architecture, SDK versions, authentication models, and testing strategy.

A command still errors if the configured remote is misconfigured or refers to an unknown backend type (see the error doc under `omemfs pack` below).

### Remote configuration blocks

Each remote is configured as one object under `remotes.origin` / `remotes.backup` in `.omemfs/config` (see `02_storage_format.md` for the full schema). The fields per backend:

**`local`** — `path` (directory on the filesystem).

**`s3`** — `bucket`, `region`, `prefix`, and optional `access_key_id` / `secret_access_key`. When the keys are omitted, the AWS default credential chain is used (environment, profile, IMDS, etc.). Two optional fields support S3-compatible stores and test harnesses: `endpoint` (a custom service endpoint URL, e.g. a MinIO server) and `force_path_style` (boolean; use path-style addressing `https://endpoint/bucket/key` rather than virtual-hosted-style — required by MinIO).

**`gcs`** — `bucket`, `prefix`, and optional `project_id`. Authentication uses one of: `credentials_json_path` (path to a service-account JSON key file), `credentials_json` (the service-account JSON inline), Application Default Credentials (ADC) when neither is set, or anonymous access against a test emulator. An optional `endpoint` field points the client at a non-Google endpoint (the Google storage-testbench emulator for tests).

**`azure`** — `account`, `container`, `prefix`, and the Entra ID (Azure AD) credential fields `tenant_id`, `client_id`, `client_secret`. **Azure authenticates with Entra ID (`TokenCredential`) only — account keys and SAS tokens are not supported.** An optional `endpoint` field overrides the default blob service URL (`https://<account>.blob.core.windows.net`).

All four blocks accept an optional `encryption` object (DEK). All new fields are additive and serde-optional, so existing configs keep working.

### Parallel transfer concurrency

The environment variable `OMEMFS_TRANSFER_CONCURRENCY` controls how many objects are transferred in parallel during the object-graph transfer phase of `push` and `clone`. The default is `1` (fully serial) for the local backend; cloud backends default to `8`. Set it to override either default, e.g. `OMEMFS_TRANSFER_CONCURRENCY=16 omemfs push`. Parallelism never affects correctness or the single index-root CAS: object PUTs land first, then a single CAS in `finish()` (see `03_sync_model.md` and `12_locking.md`).

Concurrency (request count) and peak transfer memory are separate knobs. The environment variable `OMEMFS_TRANSFER_MEMORY_BUDGET` caps the total bytes of transfer buffers held resident across all workers at once, defaulting to `67108864` (64 MiB); `0` or unset selects the default. Because omemfs objects are variable-sized (a chunk can be ≈16 MiB), the worker count alone does not bound memory — the budget does, independently of concurrency. A single object larger than the whole budget is admitted alone (it runs rather than deadlocks). See `02_storage_format.md`, "Two independent knobs: concurrency and memory".

Scope: the knob parallelises the two object-store breadth-first transfer loops — `push`'s upload of missing objects and the shared `transfer_objects`/`transfer_objects_many` copy (which also serves `clone`'s per-file content fetch, `cat`'s single-object fetch, and `expand`'s/`pull`'s blob fetches). These are pure content-addressed object copies with no inter-object ordering. The working-tree materialisation phases of `clone` and `pull` (directory creation, stub writing, file writes, depth-ordered cleanup) remain serial because they carry parent-before-child filesystem ordering; their *remote object fetches* still run through the parallel transfer path.

`expand` and `pull` use a Plan → Fetch → Apply sequence. Planning tests only
whether an object is in the local cache; it never treats remote readability as
a local hit. The de-duplicated plan is fetched through one concurrent
`transfer_objects_many` walk into that cache before working-tree materialisation
starts. A planned tree root recursively covers its child trees, manifests, and
chunks. `expand` discovers tree structure with parallel sibling traversal, but
leaves entries whose Git stub-visibility decision depends on apply-time state
to the on-demand fallback. This preserves stub-threshold semantics while
providing concurrency for the common many-small-files case. See
`02_storage_format.md`, "Multi-root batching (Improvement B)".

## Commands

### omemfs clone

Initialise a local repository and populate the working directory from a remote.

```
omemfs clone [<directory>]
```

**Arguments**
- `<directory>`: destination directory (default: current directory). Created if absent; must be empty unless `--force` is given.

**Options**
- `--stub-threshold <size>`: entries at or above this size are stubbed instead of downloaded (default: `1M`). Accepted forms: plain bytes (`1024`), or a number followed by `K`, `M`, or `G` (e.g. `100K`, `1M`, `2G`). Use `0` to expand everything. Invalid size strings (e.g. `1GB`, non-numeric input) are a hard CLI error.
- `--force`: allow cloning into a non-empty directory (existing paths are skipped)
- `--url <url>`: remote URL to clone from. When provided, skips the URL prompt and uses the given URL directly.
- `--new`: declare that the remote is a new (empty) repository. Mutually exclusive with `--existing`. Implies the repository is being created; the remote prefix must be empty.
- `--existing`: declare that the remote is an existing repository. Mutually exclusive with `--new`. The index root must be present on the remote.
- `--encrypt`: generate a new DEK and enable encryption for the remote. Only valid when creating a new repository (implied by `--new`, or by the interactive new/existing answer). Combining `--encrypt` with `--existing` is a hard error.

**Interactive setup**

`omemfs clone` prompts for remote configuration interactively. All remote options (URL, credentials, encryption) are entered at the terminal. The prompts are shared with `omemfs config add-backup`.

The first prompt accepts either a plain remote URL, an `omemfs_repo_` connection string exported from another machine, or an empty line. Entering an empty line (on a TTY) starts the guided menu described below, which collects the same settings field by field without requiring the user to assemble a URL by hand.

```
Hint: if you have already cloned this repository on another machine, run
  omemfs config export
to get an omemfs_repo_... connection string that fills in all parameters at once.

Remote URL or connection string (leave blank to choose a remote type): 
```

Accepted URL forms:
- `s3://<bucket>/<prefix>`
- `gs://<bucket>/<prefix>`
- `azure://<container>/<prefix>`
- `/<absolute/path>` (local directory)
- `omemfs_repo_<base32>` (connection string from `omemfs config export`)

When a plain URL is entered, the credentials appropriate to the backend are prompted first. Secrets are entered without echo. The prompts depend on the URL scheme:

- `s3://` — `Region`, `Access Key ID`, `Secret Access Key`. Leaving the access key and secret blank uses the AWS default credential chain. The `endpoint` / `force_path_style` options for S3-compatible stores are not prompted on the URL path; use the guided menu (below) or set them in config / a connection string.
- `gs://` — service-account credentials: a path to a service-account JSON key file (or ADC when left blank). An optional project id may be set in config.
- `azure://` — Entra ID credentials only: `Tenant ID`, `Client ID`, `Client Secret`. No account-key or SAS prompt exists.

**Guided menu (empty first prompt)**

When the first prompt is left blank, a remote-type menu is shown instead of parsing a URL. If the input stream ends (EOF) before a menu choice is entered — for example when stdin is redirected from `/dev/null` in an automation that supplied no `--url` — clone exits with an error rather than looping.

```
Select a remote type:
  1) Local directory
  2) Amazon S3 (or S3-compatible such as MinIO)
  3) Google Cloud Storage
  4) Azure Blob Storage
Choice [1-4]: 
```

An out-of-range or non-numeric choice is re-prompted. After the type is chosen, the backend's settings are collected field by field. Secrets are entered without echo. Optional fields show their default in brackets and accept an empty line. The collected fields build the same `RemoteConfig` the URL path produces, then the flow rejoins the shared new/existing declaration and encryption prompts below.

- **Local directory** — `Path`.
- **Amazon S3** — `Bucket`, `Prefix` (optional), `Region` (default `us-east-1`), `Access Key ID` (blank uses the AWS default credential chain), `Secret Access Key` (prompted only when an access key id was entered), `S3-compatible endpoint URL` (optional; blank for AWS S3), and — only when an endpoint was entered — `Use path-style addressing? [Y/n]` (defaults to yes; required by MinIO and most S3-compatible stores).
- **Google Cloud Storage** — `Bucket`, `Prefix` (optional), `Service-account JSON key path` (blank uses Application Default Credentials), `Project ID` (optional), `Endpoint` (optional; for emulators).
- **Azure Blob Storage** — `Account`, `Container`, `Prefix` (optional), `Tenant ID`, `Client ID`, `Client Secret` (no echo), `Endpoint` (optional; defaults to `https://<account>.blob.core.windows.net`).

The guided menu is also offered by `omemfs config add-backup` and by the `Add backup remote?` follow-up, wherever an empty remote URL is entered on a TTY.

**New/existing declaration**

After credentials, the user must declare whether the remote is a new (empty) repository or an existing one. The declaration is resolved as follows (in priority order):

1. **Connection string** (`omemfs_repo_…`): implies **existing**. Connection strings are produced only by existing repos; no new/existing prompt is shown.
2. **`--new` flag**: declares new. Skip the new/existing prompt.
3. **`--existing` flag**: declares existing. Skip the new/existing prompt.
4. **`--encrypt` flag** (alone, without `--new`/`--existing`): implies **new**. `--encrypt` is only valid when creating a repo; the encryption prompt is skipped (DEK is generated immediately).
5. **TTY with none of the above**: ask the user:
   ```
   Is this a new (empty) remote or an existing one? [new/existing]: 
   ```
6. **Non-TTY with none of the above**: hard error (exit code 1):
   ```
   error: cannot determine remote intent in non-interactive mode
   Pass --new or --existing to specify whether this is a new or existing repository.
   ```

When the answer is **new**, the user is asked whether to enable encryption (unless `--encrypt` already implies it):
```
Enable encryption? [Y/n]: 
```
If yes (or `--encrypt` was given), a new DEK is generated automatically:
```
(Generating new DEK...)
```

When the answer is **existing**, the user is asked whether the repository is encrypted and, if so, to enter the existing DEK. Secrets are entered without echo:
```
Is it encrypted? [y/N]: y
DEK (base64): 
```
If encryption is not enabled on the existing repository, no further prompts are shown for encryption.

After the origin remote is configured, the user is asked whether to add a backup remote:

```
Add backup remote? [y/N]: 
```

If yes, the same URL (or guided-menu), credential, and new/existing prompts are repeated for the `backup` remote: an empty backup URL on a TTY opens the same remote-type menu. A new DEK is generated independently for backup.

If an `omemfs_repo_` connection string is entered, it encodes the full config for both remotes (origin and backup). No further prompts are needed.

**Validation**

After the declaration is resolved and credentials are collected, the remote is contacted to validate consistency before any local state is written:

- **new**: the remote prefix must be completely empty — no `objects/` content and no fixed `INDEX_ROOT`. If the prefix is non-empty, exit with code 1:
  ```
  error: remote prefix is not empty
  Use --existing if this is an existing repository, or choose an empty prefix for a new one.
  ```
- **existing + encrypted**: the derived index root object (`objects/<HMAC-SHA256(DEK, "omemfs:index-root:v1") hex>`) must exist on the remote. If absent, exit with code 1:
  ```
  error: index root not found on remote
  The encryption key may be wrong, or the URL/prefix may point to a different repository.
  ```
- **existing + unencrypted**: the fixed `<prefix>/INDEX_ROOT` must exist on the remote. If absent, exit with code 1:
  ```
  error: INDEX_ROOT not found on remote
  Check the URL/prefix. If this is a new repository, use --new instead.
  ```

**Connection-string exemption**: a clone from an `omemfs_repo_…` connection string skips the index-root presence validation. The connection string embeds the URL and DEK from a working config, so the wrong-key/wrong-prefix confusion the validation guards against cannot arise through normal use; an absent index root is treated as a repo that has not been pushed yet (this supports cloning a freshly created repo on a second machine before its first push). Divergence after the clone is still covered by the post-clone sync guard (see `03_sync_model.md`).

**Behaviour**

1. Create `<directory>` and initialise `.omemfs/` (config, empty `clone_root`).
2. Collect URL and credentials interactively (or from the connection string) and resolve the new/existing declaration.
3. Validate the remote against the declaration (see Validation above).
4. Determine encryption configuration:
   - **Connection string**: DEK is already embedded — skip encryption prompts.
   - **New repository**: prompt `Enable encryption? [Y/n]` (or skip if `--encrypt`). If yes, generate a new DEK.
   - **Existing repository**: prompt `Is it encrypted? [y/N]`. If yes, prompt `DEK (base64):` without echo and store the entered DEK.
5. Write the remote settings (including DEK if applicable) to config with permissions `0600`.
6. If **new repository**: print `Remote is empty. Initialised empty repository.`, write the `.omemfs-filter` template (the same template written in step 11), and exit. The first `omemfs push` will create the index root on the remote.
7. Fetch the root tree object and set `clone_root` to the remote root hash. **Clone is lazy and stub-aware: it does not download the entire repository.** Only the objects actually needed to materialise sub-threshold entries are fetched.
8. Walk the root tree, deciding per entry against the stub threshold:
   - An entry (file **or** directory) whose size is at or above the threshold is **stubbed** from the parent tree entry's metadata alone (hash, size, mtime, mode, blob_count). Nothing is downloaded for a stubbed entry — not its blob/chunks, not its subtree, not even its own tree object.
   - An entry below the threshold is **materialised**: a blob's content and a directory's tree object are fetched on demand through the pack reader (local cache → packcache → remote), recursing into below-threshold directories.
   Consequently, after a clone that stubs large entries the local object cache (`.omemfs/objects/`) is largely empty — this is by design. Subsequent `ls`/`push`/`pull`/`expand` read missing objects from the remote via the pack reader, and large stubbed content is materialised later on demand by `omemfs expand`.
9. Restore filesystem `mtime` and the executable-bit `mode` for materialised files from tree entry metadata.
10. If `.omemfs-filter` does not exist at the repository root, write the default template (see `05_ignore_and_aggregate.md`). The check is performed after working tree expansion (step 8), so a `.omemfs-filter` that already exists on the remote is never overwritten.

**Output example (plain URL — new repository, interactive)**

```
$ omemfs clone ./mydata

Hint: if you have already cloned this repository on another machine, run
  omemfs config export
to get an omemfs_repo_... connection string that fills in all parameters at once.

Remote URL or connection string (leave blank to choose a remote type): s3://my-primary-bucket/omemfs-repo
Region: ap-northeast-1
Access Key ID: AKIA...
Secret Access Key: 
Is this a new (empty) remote or an existing one? [new/existing]: new
Enable encryption? [Y/n]: Y
(Generating new DEK...)

Add backup remote? [y/N]: y
Remote URL: s3://my-backup-bucket/omemfs-repo
Region: ap-northeast-1
Access Key ID: AKIA...
Secret Access Key: 
Is this a new (empty) remote or an existing one? [new/existing]: new
Enable encryption? [Y/n]: Y
(Generating new DEK...)

Remote is empty. Initialised empty repository.
```

**Output example (guided menu — new S3-compatible repository, interactive)**

```
$ omemfs clone ./mydata

Hint: if you have already cloned this repository on another machine, run
  omemfs config export
to get an omemfs_repo_... connection string that fills in all parameters at once.

Remote URL or connection string (leave blank to choose a remote type): 
Select a remote type:
  1) Local directory
  2) Amazon S3 (or S3-compatible such as MinIO)
  3) Google Cloud Storage
  4) Azure Blob Storage
Choice [1-4]: 2
Bucket: omemfs-test
Prefix (optional): omemfs-repo
Region [us-east-1]: 
Access Key ID (blank to use the AWS default credential chain): minioadmin
Secret Access Key: 
S3-compatible endpoint URL (blank for AWS S3): http://localhost:9000
Use path-style addressing? [Y/n]: Y
Is this a new (empty) remote or an existing one? [new/existing]: new
Enable encryption? [Y/n]: n

Add backup remote? [y/N]: N

Remote is empty. Initialised empty repository.
```

**Output example (plain URL — new repository, non-interactive flags)**

```
$ omemfs clone --url s3://my-primary-bucket/omemfs-repo --new --encrypt ./mydata
Region: ap-northeast-1
Access Key ID: AKIA...
Secret Access Key: 
(Generating new DEK...)

Remote is empty. Initialised empty repository.
```

**Output example (plain URL — existing repository, interactive)**

```
$ omemfs clone ./mydata

Hint: if you have already cloned this repository on another machine, run
  omemfs config export
to get an omemfs_repo_... connection string that fills in all parameters at once.

Remote URL or connection string (leave blank to choose a remote type): s3://my-primary-bucket/omemfs-repo
Region: ap-northeast-1
Access Key ID: AKIA...
Secret Access Key: 
Is this a new (empty) remote or an existing one? [new/existing]: existing
Is it encrypted? [y/N]: y
DEK (base64): 

Add backup remote? [y/N]: N

Cloning from s3://my-primary-bucket/omemfs-repo ...
Remote root: a3f89b2c
28 file(s) expanded, 3 file(s) stubbed (>= 1048576 bytes).
Cloned into ./mydata
```

**Output example (connection string)**

```
$ omemfs clone ./mydata

Hint: ...

Remote URL or connection string (leave blank to choose a remote type): omemfs_repo_MFRA2YLN...
Importing config from connection string...
  origin: s3://my-primary-bucket/omemfs-repo
  backup: s3://my-backup-bucket/omemfs-repo

Cloning from s3://my-primary-bucket/omemfs-repo ...
Remote root: a3f89b2c
28 file(s) expanded, 3 file(s) stubbed (>= 1048576 bytes).
Cloned into ./mydata
```

**Disaster recovery**

To recover from a lost origin remote using the backup:

```
omemfs clone ./recovered
Remote URL or connection string (leave blank to choose a remote type): s3://my-backup-bucket/omemfs-repo
...
Is this a new (empty) remote or an existing one? [new/existing]: existing
...
```

The cloned repository will have origin pointing at the backup URL and the backup's DEK as the origin DEK. The `backup` remote is absent from config after recovery. A new backup can be added afterwards with `omemfs config add-backup`.

**Errors**

- Destination not empty (without `--force`): `error: directory '<dir>' is not empty`
- Non-TTY with no `--new`/`--existing` flag (and no connection string / `--encrypt`):
  ```
  error: cannot determine remote intent in non-interactive mode
  Pass --new or --existing to specify whether this is a new or existing repository.
  ```
- `--new` and `--existing` both given: `error: --new and --existing are mutually exclusive`
- `--encrypt` combined with `--existing`: `error: --encrypt is only valid for a new repository; use interactive mode to provide the existing DEK`
- **new** but remote prefix is non-empty (exit code 1):
  ```
  error: remote prefix is not empty
  Use --existing if this is an existing repository, or choose an empty prefix for a new one.
  ```
- **existing + encrypted** but index root absent (exit code 1):
  ```
  error: index root not found on remote
  The encryption key may be wrong, or the URL/prefix may point to a different repository.
  ```
- **existing + unencrypted** but `INDEX_ROOT` absent (exit code 1):
  ```
  error: INDEX_ROOT not found on remote
  Check the URL/prefix. If this is a new repository, use --new instead.
  ```
- Invalid DEK (wrong length or not valid base64): `error: invalid DEK — expected 32 bytes encoded as base64`
- DEK mismatch detected (decryption fails on first object): `error: decryption failed — DEK may be incorrect`

---

### omemfs push

Scan the working tree, build tree and blob objects, upload missing objects to the remote, and update the index root and `clone_root`.

```
omemfs push [<path>...]
```

**Arguments**
- `<path>...`: optional path scope(s), resolved relative to the cwd. If given, only the specified paths and their contents are pushed; the rest of the tree is unchanged. If omitted, the scope defaults to the cwd: from the repository root this is a full-tree push, and from a subdirectory it is a scoped push of that subtree. When multiple paths are given, all are pushed in a single remote operation (one CAS write). If one path is a descendant of another, the ancestor path subsumes the descendant (automatic deduplication).

**Options**
- `--with-backup`: after pushing to origin, also push the current state to the `backup` remote (if configured). See backup push behaviour below.
- `--dry-run`: show what would be pushed without making changes

**Behaviour (full push)**

1. Scan the working tree and build all tree and blob objects. Store new objects in `.omemfs/objects/`. Conflict helper files are excluded from the scan; their base paths are recorded as a side-effect during scan traversal.
2. If any conflict helpers were found during the scan → error (see below). Note: paths excluded by `.omemfs-filter` are not scanned, so conflict helper files inside ignored directories do not block the push.
3. If the new root hash equals the current clone root → `nothing to push`, exit.
4. Read the index root from origin (derived key for encrypted remotes, fixed `INDEX_ROOT` for unencrypted remotes). If the clone root is not the empty-tree hash and the index root is absent, fail with a hard error (see post-clone sync guard in `03_sync_model.md`).
5. Upload objects missing from origin.
6. Update origin's index root with a CAS write conditioned on the raw bytes read in step 4. If the CAS fails → error (see below).
7. Write the new root hash to `clone_root`.
8. If `--with-backup` is given and `backup` is configured: perform a backup push (see below).

**Behaviour (path-scoped push)**

Same as full push, but:
- Conflict-helper detection (step 2) is a side effect of scanning only the specified paths' subtrees, exactly as in full push — it is never a separate filesystem walk of the whole `work_dir`. Consequently, conflict helpers outside the specified paths, or inside a scoped path but excluded by `.omemfs-filter`, do not block the push; a symlink loop elsewhere in the working tree (outside the scanned subtrees) cannot break a scoped push either, since it is never visited.
- Only objects reachable from the specified paths' new subtrees are uploaded.
- The index root is updated by splicing each new subtree into the current remote root tree; all splices are applied before the single CAS write.
- `clone_root` is updated by applying the same splices to the current clone root tree.
- If a specified path was deleted locally **and** is already absent on the remote (another client already deleted it), this is a no-op for that path — no error is raised. An informational note is printed: `note: '<path>' is already absent on remote`. This applies to both single-path and multi-path push.

**Backup push behaviour (`--with-backup`)**

A backup push runs after a successful origin push. It uploads the state identified by the current `clone_root` to the `backup` remote:

1. Enumerate all objects reachable from `clone_root`.
2. Upload objects missing from backup.
3. Read backup's current index root and update it with a CAS write.
4. `clone_root` is **not** updated (backup is write-only; the sync baseline remains the origin state).

If the backup push fails, a warning is printed but the exit code reflects only the origin push result. The origin push is not rolled back.

```
--with-backup requires a 'backup' remote to be configured.
Use 'omemfs config add-backup' to set one up.
```

**Output example**

```
Pushing to origin...
  3 objects uploaded
Remote root: b4c1d2e3
Pushing to backup...
  3 objects uploaded
Backup remote root: c5d6e7f8
```

**Errors**

- Unresolved conflict helper files found:
  ```
  error: unresolved conflicts — resolve or restore before pushing
  The following conflict helper files were found:
    src/main.rs.omemfs-conflict-base
    src/main.rs.omemfs-conflict-local
    src/main.rs.omemfs-conflict-remote
  ```
- Concurrent push detected on origin:
  ```
  error: remote has been updated since last sync
  Run 'omemfs pull' and retry 'omemfs push'.
  ```
- `--with-backup` specified but no backup remote configured:
  ```
  error: --with-backup requires a 'backup' remote; run 'omemfs config add-backup' first
  ```

---

### omemfs pull

Fetch the remote root, compare it against the clone root and working tree, apply the remote changes, and update `clone_root`. If any conflict is detected, nothing is applied (see Conflict handling below).

```
omemfs pull [<path>...]
```

**Arguments**
- `<path>...`: optional path scope(s), resolved relative to the cwd. If given, only the specified paths are pulled; the rest of the working tree and clone root are unchanged. If omitted, the scope defaults to the cwd: a full pull from the repository root, a scoped pull of that subtree from a subdirectory. When multiple paths are given, all diffs are collected first and conflict-checked together — if any path conflicts, the entire pull is aborted (no partial application). Ancestor deduplication applies as with `push`.

**Options**
- `--dry-run`: show what would change without applying anything
- `--stub-threshold <size>`: NEW remote entries (Added entries only; existing materialised files are never auto-converted to stubs) at or above this size are stubbed (default: `1M`). Accepted forms: plain bytes, or a number followed by `K`, `M`, or `G`. Invalid size strings are a hard CLI error.

**Behaviour (full pull)**

1. Read the index root and extract the remote root hash. If the clone root is not the empty-tree hash and the index root is absent, fail with a hard error (see post-clone sync guard in `03_sync_model.md`). If equal to clone root → `Already up to date.`, exit.
2. Compute the diff between clone root and remote root (remote changes).
3. If the working tree differs from clone root (local changes): compute the diff between clone root and working tree (local changes). Check for path-level conflicts (same path in both diffs). If any → abort (see Conflict handling below).
4. Collect the blob hashes referenced by the remote diff that are missing from the local cache and download all of them in a single batched, concurrent transfer (see `02_storage_format.md`, "Multi-root batching"), rather than one blob at a time. Tree objects needed to enumerate directory children along the way are still fetched individually as the diff is walked; only the leaf-blob fetches are batched.
5. Apply remote changes to the working tree, restoring `mtime` and the executable-bit `mode` for each materialised file from the remote tree entry. A remote change that only flips the executable bit is applied as a modification. Local-only changes are left untouched.
6. Write the remote root hash to `clone_root`.

**Behaviour (path-scoped pull)**

Same as full pull, but:
- Only remote changes within the specified paths are considered.
- Only the working tree under those paths is modified.
- All diffs across all specified paths are collected and conflict-checked together before any changes are applied. If any path has a conflict, the entire pull is aborted.
- `clone_root` is updated by splicing each remote root subtree into the current clone root tree (all splices applied, then written once).

**Output example**

```
Pulling from origin...
  modified: docs/guide.md
  added:    src/new_module.rs
2 paths updated.
```

**Conflict output**

```
error: pull would overwrite local uncommitted changes
The following paths conflict with incoming remote changes:
  modified: src/main.rs

Save or discard your local changes first:
  omemfs push           (commit local changes, then pull)
  omemfs restore <path> (discard local changes)
```

---

### omemfs ls

List entries with their sync status and stub state.

Use `omemfs ls --dirty` to see changes not yet pushed (equivalent of `git status`).

```
omemfs ls [-r] [--full-hash] [--dirty] [--no-remote] [--remote | --clone | --working] [<path>...]
```

**Arguments**
- `<path>...`: paths to list, resolved relative to the cwd (default: the cwd itself). From the repository root the default lists the whole tree root; from a subdirectory it lists that subdirectory. A directory argument first shows the directory itself as a self-entry row, then lists its immediate children; a file argument prints that file's single entry.

**Options**
- `-r`, `--recursive`: list all descendants recursively. Recursion stops at a stubbed directory boundary: a directory whose tree object is not present in the local store (only its `.omemfs-stub` marker exists) is listed as a single `S` row rather than descended into, since its subtree objects are not held locally. This matches the non-recursive listing, which never reads child tree objects.
- `--full-hash`: show the full 64-character hash instead of the default 8-character prefix
- `--dirty`: show only entries that differ from the clone root. Implies `-r`. When the working tree matches the clone root, prints nothing. Like a plain `ls`, `--dirty` honours the cwd / `<path>` scope: from a subdirectory (or with a `<path>` argument) only changes under that scope are listed, not the whole tree. The scoped working-tree scan (below) applies, so only the in-scope subtrees are walked.
- `--no-remote`: skip the remote check entirely. The `R` column always shows ` ` (space).
- `--remote`: show hash, size, blob_count, and mtime from the **remote root** (reads the index root). Mutually exclusive with `--clone` and `--working`.
- `--clone`: show hash, size, blob_count, and mtime from the **clone root**. If a path is absent from the clone root but exists in the working tree (e.g. a locally-created path not yet pushed), `ls` lists it from the working tree with status `A` instead of erroring. This fallback allows locally-added paths to be listed without `--working`. Mutually exclusive with `--remote` and `--working`.
- `--working`: show hash, size, blob_count, and mtime from the **working tree** scan (default when none of `--remote`, `--clone`, `--working` is given). All rows are sourced from the working tree. Mutually exclusive with `--remote` and `--clone`. The substitution is resolved per displayed row (navigating the working tree only at the paths already selected for output), not by pre-flattening the entire working tree into a lookup table; a non-recursive `ls` therefore costs the same whether or not `--working` is in effect, and `-r` / `<path>` scoping bound its cost exactly like `--clone` (see "Scoped working-tree scan" below). This mirrors how `--remote` resolves its substitution from the clone-root/remote-root diff rather than a full remote-tree walk. Being the default, this is what a plain `ls` shows.

**Remote check behaviour**

By default, `omemfs ls` attempts to read the index root from the origin remote and compare the stored remote root hash with `clone_root` to populate the `R` column. The index root lookup is bounded by a 5-second timeout: the blocking read runs on a worker thread, and if it does not return within 5 seconds the lookup is abandoned. A timeout is treated exactly like the error path — the `R` column silently shows ` ` (space) for all entries and `ls` still succeeds. If the remote is not configured or any other error occurs, the `R` column likewise silently shows ` ` (space) for all entries.

The remote check reads only tree objects reachable from the displayed paths (short-circuit: subtrees whose hash matches between `clone_root` and `remote_root` are skipped). Blob content is never downloaded.

**Scoped working-tree scan**

When one or more `<path>` arguments are given, `ls` scans **only the working-tree subtrees under those paths**, not the whole working tree. This mirrors the scoped scan that `push <path>` already performs: the per-path subtree is scanned (its files are lstat'd and, on a cache miss, hashed) and the resulting subtree hash is spliced onto the clone root at the path's position to reconstruct a full working-tree root hash. The diff against the clone root then operates on this reconstructed root exactly as in the unscoped case, so all downstream behaviour (`X`/`R`/`Z` columns, `--working`, `-r`) is unchanged.

The splice uses the clone root as the base (or an empty tree when no clone root exists yet, e.g. immediately after a push-only init), so out-of-scope paths reuse the clone-root tree objects unchanged and incur no working-tree I/O. This is the dominant cost saving: in a repository with hundreds of thousands of files, `omemfs ls daily` only walks the `daily/` subtree instead of every file. When no `<path>` is given the scan covers the full working tree as before.

This scoping also bounds `--working`'s row substitution: since that substitution is resolved per displayed row (see the `--working` option above), an out-of-scope subtree is never visited for it either, whether or not `--working` is given.

To keep the scope from leaking back into a whole-tree walk, the `.omemfs-filter` set for a single `<path>` is loaded scope-limited (`FilterSet::load_scoped`): only the ancestors of the path and the files inside its subtree are read, never out-of-scope siblings (see `05_ignore_and_aggregate.md` "Scope-limited filter load"). Without this, discovering filter files alone would re-walk the entire tree.

If a `<path>` is itself a file or a stub, no subtree scan runs: the file is hashed directly (or the stub's recorded entry is spliced), matching `push`'s single-path handling. A `<path>` excluded by an `.omemfs-filter` ignore pattern is not scanned.

For a single `<path>`, the STAT_CACHE is loaded scope-limited (`read_scoped`) and written back with `write_scoped_merge`, so only the in-scope cache entries are parsed rather than the whole file (see `07_stat_cache.md` "Read optimisation: scope-limited load"). This matters on large repositories whose STAT_CACHE has grown to many megabytes: `omemfs ls daily` then parses only the `daily` slice. Multiple `<path>` arguments fall back to a full STAT_CACHE read, as scoped `push` does.

The mtime pre-filter map passed into the scan is likewise bounded for a single `<path>`: it is built with `flatten_tree_entries_scoped`, the same bounded-flatten mechanism as scoped push (see `03_sync_model.md` "Path-scoped push"), from only the clone root's `<path>` subtree, instead of flattening the whole clone root as before. Multiple `<path>` arguments or an unscoped `ls` fall back to the full `flatten_tree_entries`. As with scoped push, a path missing from the map is not an error: the scan simply re-hashes the corresponding file instead of skipping it via the pre-filter, so this bound is a pure optimisation with no correctness dependency.

**Output format**

One line per entry: `RXZ <hash> <size> <blob_count> <mtime> <wt_mtime> <path>`. Each
path appears at most once: a path resolved from the working tree (e.g. a directory
absent from the clone root, listed with status `A`) is never emitted a second time
from the working-tree-vs-clone-root diff. This holds for empty directories too,
which are diffed as added-empty-dir entries.

Remote column `R` (clone root vs remote root):
- `M`: entry modified on the remote (present in both clone root and remote root, but with a different hash, OR — for a blob — the same hash with a different executable-bit `mode`). A mode-only change is reported as `M` for consistency with the `X` column, which reports a working-tree `chmod`-only change as modified via the same tree-hash-based comparison push uses for its dirty detection.
- `A`: entry added on the remote (present in remote root but absent in clone root)
- `D`: entry deleted on the remote (present in clone root but absent in remote root)
- ` ` (space): up to date with remote, or remote not configured / unreachable

Status column `X` (working tree vs clone root):
- `A`: added in working tree (not in clone root)
- `M`: modified in working tree. A file is modified when its content hash differs
  from the clone root entry, **or** when its executable-bit `mode` differs (e.g.
  `chmod +x` / `chmod -x` with unchanged content). This matches push's notion of
  a dirty tree: the tree hash includes `mode`, so a mode-only change is pushed.
- `D`: deleted in working tree (in clone root but absent locally)
- ` ` (space): unchanged

Stub/conflict column `Z`:
- `!`: unresolved conflict helper files exist for this path, or (for a directory) exist for any descendant path. Conflict takes precedence over all other Z values.
- `?`: this path is a reserved `.omemfs-` file whose kind is not recognised by this version of omemfs (produced by a newer version). Hash/size/mtime are shown as `-`. The file is skipped by the scan and never modified. See `09_reserved_names.md`.
- `I`: this path is excluded by an `.omemfs-filter` ignore pattern and is **not** present in clone root. X is ` `. Hash/size/mtime are shown as `-`. Takes precedence over `S`/`s`.
- `i`: this path is excluded by an `.omemfs-filter` ignore pattern and **is** present in clone root, so it will be deleted from the remote on the next push. The X column shows `D`. Hash/size/mtime are shown as `-`. Takes precedence over `S`/`s`.
- `S`: this entry itself is a stub (file stub or fully-stubbed directory — only `.omemfs-stub` inside, no real files)
- `s`: this directory has stub-related indirect state — either partially expanded (`.omemfs-stub` coexists with real files) or a descendant somewhere in its subtree is stubbed
- ` ` (space): no conflict, not ignored, fully materialised

`<hash>`: hash of the entry (8 chars by default; 64 with `--full-hash`). `-` for symlinks.
`<size>`: size in bytes (clone root value for known entries; working tree value for added entries). `-` for symlinks.
`<blob_count>`: number of blobs contained (1 for files; total descendant blobs for directories).
`<mtime>`: last-modified time from the clone root tree entry. `-` if absent from clone root.
`<wt_mtime>`: last-modified time of the working tree file (filesystem mtime). `-` for deleted files, directories, and symlinks. `-` is also shown for unchanged files to avoid noise from filesystem mtime drift.
`<path>`: path relative to the **repository root** (the parent of `.omemfs/`). When `ls` is run from the repository root this is identical to a cwd-relative path; from a subdirectory the displayed paths remain root-anchored so they match the paths stored in tree objects. The directory itself is always shown as the first row: `.` for the working tree root (no path argument or explicit `.` argument run at the root), or `<dir>/` (root-relative) when a directory path is given or when the default cwd scope is a subdirectory.

**Output example**

```
  M  a1b2c3d4    12288 8 2026-05-10  .
     a3f89b2c     4096 3 2026-05-10  src/
 M   b2c3d4e5      512 1 05-15 09:30 src/lib.rs
 A   c3d4e5f6      128 1          -  src/new.rs
 D   d4e5f6a7      256 1 05-10 08:00 src/old.rs
M    e5f6a7b8     1024 1 05-12 10:00 src/config.rs
A    f6a7b8c9      256 1          -  src/remote_new.rs
D    a7b8c9d0      128 1 05-11 07:00 src/remote_del.rs
DM!  8c9852d4       12 1 05-22 22:06 timer_test.txt
  S  abcd1234      512 1 05-23 09:00 large.bin
  S  efgh5678     4096 3 05-23 09:00 archive/
  s  01234567     4096 5 05-23 09:00 projects/
```

The first row (`.`) is the working tree root self-entry. Its X column follows the same rules as other directories: `M` when the working tree differs from the clone root anywhere, ` ` when in sync. For `omemfs ls src/`, the first row would be `src/`.

**Self-entry row details**

- **Path:** `.` for root, `<dir>/` for a named directory argument.
- **Hash / size / blob_count / mtime:** aggregate values for the directory (from the clone root tree entry, or computed from root tree entries for the root self-row).
- **X column:** `M` if any descendant differs from clone root; `A` if the directory is new (not in clone root); ` ` if in sync.
- **R column:** `M` if any remote change exists under this directory; ` ` otherwise.
- **Z column:** `!` if any descendant has conflict helper files; `s` if any descendant is stubbed; ` ` otherwise.
- **`--dirty` mode:** self-entry rows are not shown (only changed entries are listed).

The `Z` column (3rd character) shows stub state, ignore state, and conflicts:
- `timer_test.txt` has `!` — conflict helper files exist for this path
- `large.bin` has `S` — it is a file stub (`.omemfs-stub` exists, original file absent)
- `archive/` has `S` — it is a fully-stubbed directory (only `.omemfs-stub` inside)
- `projects/` has `s` — it contains stubs somewhere in its subtree, or is partially expanded
- An ignored path (matched by `.omemfs-filter`) shows `I` when absent from clone root, or `i` when present in clone root (in which case the X column shows `D`, meaning it will be removed from the remote on the next push)
- An unknown reserved `.omemfs-` file (from a newer omemfs version) shows `?`

For a directory, `!` is shown when any descendant has unresolved conflict helper files:

```
  ! a3f89b2c     4096 3 2026-05-10          - src/
 M  b2c3d4e5      512 1 05-15 09:30 05-17 14:22 src/lib.rs
 M! 8c9852d4       12 1 05-22 22:06 05-23 01:07 src/main.rs
```

Here `src/` shows `!` because `src/main.rs.omemfs-conflict-{base,local,remote}` files exist beneath it.

---

### omemfs config export

Export the full repository config (both remotes, credentials, and DEKs) as a single `omemfs_repo_` connection string.

```
omemfs config export
```

The connection string encodes the `remotes` section of `.omemfs/config` as a base32-encoded JSON blob with the prefix `omemfs_repo_`. It can be pasted into the `Remote URL or connection string` prompt during `omemfs clone` on another machine to reproduce the full config without entering credentials manually.

**Output**

The warning is printed to stderr; the connection string is printed to stdout so it can be piped or captured.

```
$ omemfs config export
Warning: the following string contains credentials. Handle with care.
omemfs_repo_MFRA2YLNMFRA2YLNMFRA2YLN...
```

---

### omemfs config add-backup

Add or replace the `backup` remote in an existing repository.

```
omemfs config add-backup
```

Prompts interactively for the backup remote URL, credentials, and the new/existing declaration. The prompts, intent-resolution rules, validation, and error messages are identical to those of `omemfs clone` (see the **New/existing declaration** and **Validation** sections above). Leaving the URL prompt blank on a TTY opens the same guided remote-type menu (see **Guided menu** above).

A new DEK is generated and stored under `remotes.backup.encryption.dek` in config when the backup is a new encrypted repository.

If a `backup` remote already exists, the command errors unless `--force` is given.

**Options**
- `--force`: overwrite an existing backup remote configuration
- `--new`: declare that the backup remote is a new (empty) repository. Mutually exclusive with `--existing`.
- `--existing`: declare that the backup remote is an existing repository. Mutually exclusive with `--new`.
- `--encrypt`: generate a new DEK and enable encryption. Only valid with `--new` (or when the interactive declaration resolves to new). Combining `--encrypt` with `--existing` is a hard error.

**Output example**

```
$ omemfs config add-backup
Remote URL: s3://my-backup-bucket/omemfs-repo
Region: ap-northeast-1
Access Key ID: AKIA...
Secret Access Key: 
Is this a new (empty) remote or an existing one? [new/existing]: new
Enable encryption? [Y/n]: Y
(Generating new DEK...)
Backup remote configured.
```

---

### omemfs restore

Discard local working tree changes for one or more paths by restoring the content recorded in `clone_root`.

```
omemfs restore [<path>...]
```

**Arguments**
- `<path>...`: paths to restore, resolved relative to the cwd. If omitted, restore the entire working tree — unlike `push`/`pull`/`ls`, the no-argument default is always the whole tree, not the cwd subtree, because restoring is a deliberate destructive action that should not silently narrow to the current directory.

**Options**
- `--dry-run`: show what would be restored without making changes

**Behaviour**

1. Read `clone_root`. If absent (repository has never been synced), error out.
2. For each path specified (or the entire tree if none):
   - If the path is present in `clone_root` as a blob: overwrite the working tree file with the clone root content. Restore `mtime` and the executable-bit `mode` from the tree entry.
   - If the working tree content already matches the clone root blob but the executable bit differs, only the mode is fixed (chmod, no content rewrite). The path is still reported and counted as restored.
   - If the path is present in `clone_root` as a tree (directory): recursively restore all descendant blobs.
   - If the path is absent from `clone_root`: delete the file from the working tree (it was locally added and is being discarded).
   - If the path is not present in the working tree and not in `clone_root`: no-op.
   - For each restored or deleted path, also delete any corresponding conflict helper files (`<path>.omemfs-conflict-{base,local,remote}`) if they exist.
3. `clone_root` is **not** modified. The remote is not accessed.

This operation is the inverse of local edits: after `restore`, the working tree at `<path>` matches the clone root exactly. Any conflict helper files for restored paths are removed as part of the restore.

**Output example**

```
$ omemfs restore src/lib.rs docs/
  restored: src/lib.rs
  deleted:  docs/new.md
  restored: docs/guide.md
3 path(s) restored.
```

**Errors**

- No `clone_root` (repository never synced): `error: no clone_root — repository has never been synced`
- Path not inside work_dir: `error: path '<path>' is not inside the working tree`

---

### omemfs cat

Print the content of an object to stdout.

```
omemfs cat [--hash] [--remote <name>] <target>
```

**Arguments**

`<target>` accepts the following forms:

- SHA-256 hash (64 hex characters) or a unique prefix (4+ characters) of any stored object.
- `clone-root`: alias for the current clone root. Reads `.omemfs/clone_root`.
- `remote-root`: alias for the current remote root. Reads the remote root hash from the index root on the origin remote (derived key for encrypted remotes, fixed `INDEX_ROOT` for unencrypted remotes).
- `index-root`: print the pack-layer index root from the remote as JSON. See **Pack-layer output** below.
- `<ref>:<path>` or `<ref>/<path>`: traverse a tree object along `<path>`. `<ref>` may be any of the above forms. The ref and path may be separated by either `:` or `/`; the earliest of the two characters is the separator (so a `/` inside the path is preserved when `:` is used as the separator).

**Options**
- `--hash`: print only the resolved 64-character hash to stdout, instead of the object content. Useful for scripting (e.g. `HASH=$(omemfs cat --hash clone-root)`). Not supported for pack-layer objects.
- `--remote <name>`: remote to use when reading from the remote store (default: `origin`).

**Resolution order for hash targets**

When a hash (or prefix) is given, the command tries in order:

1. **Local cache** — if the object exists locally (resolving a 4+ character prefix against the local store), decode it (decrypt → decompress → deserialise) and display the logical object content.
2. **Remote store** — if not found locally, resolve the target against the remote and display the pack-layer object. A full 64-character hash is fetched directly; a 4+ character **prefix** is resolved against the remote pack index (enumerating the delta, hot, and cold-shard index files) so the abbreviated hash that `ls` prints can be pasted directly. A prefix that matches more than one stored object is reported as ambiguous with a sample of the matches. See **Pack-layer output** below.

If neither source has the object, an error is returned.

**Behaviour**
- blob: print raw bytes to stdout (no trailing newline added).
- tree: pretty-print the JSON content with 2-space indentation and a trailing newline.
- When stdout is a TTY, JSON output (tree objects and pack-layer objects) is syntax-highlighted.
- Symlinks have no associated object; `<ref>/<path>` targeting a symlink is an error.
- With `--hash`, `/<path>` traversal still occurs; the hash of the resolved leaf object is printed.

**Output examples**

```
$ omemfs cat a3f89b2c/README.md
# My Project
...
```

```
$ omemfs cat a3f89b2c:README.md   # `:` and `/` separators are equivalent
# My Project
...
```

```
$ omemfs cat b2c3d4e5
{
  "entries": [
    {
      "hash": "a1b2c3...",
      "kind": "blob",
      "mtime": "2026-05-16T10:00:00Z",
      "name": "README.md",
      "size": 1234
    }
  ]
}
```

```
$ omemfs cat clone-root
{
  "entries": [...]
}
```

```
$ omemfs cat --hash clone-root
a3f89b2c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2
```

```
$ omemfs cat --hash clone-root/docs/guide.md
b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3
```

**Errors**

- Hash not found (local and remote): `error: object not found: <hash>`
- Ambiguous prefix: `error: ambiguous hash prefix '<prefix>' — matches multiple objects`
- Path on a non-tree: `error: '<hash>' is a blob, not a tree`
- Path targeting a symlink: `error: path '<path>' is a symlink and has no object`
- No clone root: `error: no clone_root — repository has never been synced`
- No remote root (index root absent): `error: no index root on origin`
- `--hash` used with a pack-layer object: `error: --hash is not supported for pack-layer objects`

---

#### Pack-layer output

When a hash resolves to a physical pack-layer object on the remote (i.e. not found in the local cache), or when `index-root` is specified, the command inspects the object and prints its internal structure as JSON.

**`index-root`**

Read the index root from the remote (derived key for encrypted remotes, fixed `INDEX_ROOT` for unencrypted remotes) and print it as JSON.

```
$ omemfs cat index-root
{
  "remote_root": "a3f89b2c...",
  "hot_hash": "b4c5d6e7...",
  "bloom_hash": "c5d6e7f8...",
  "cold_prefix_bits": 4,
  "delta_hashes": [
    "d6e7f8a9..."
  ],
  "cold_shards": [
    "f8a9b0c1...",
    ...
  ]
}
```

All hashes are shown as 64-character hex strings. An all-zero hash (unset field) is shown as `null`.

**Pack-layer hash**

When a hash is not in the local cache, read the physical object from the remote, inspect the first two bytes after decryption, and print it in the appropriate format:

| Leading bytes (after decrypt) | Interpretation | Output |
|-------------------------------|----------------|--------|
| `ED E0` | pack file | `{ "type": "pack-file", "logical_hash": "...", "storage_key": "...", "stored_bytes": N }` |
| `ED E2` | index file (hot / delta / cold) | JSON with `entry_count` and `entries` array |
| `ED E4` | Bloom filter | JSON with statistics |

Pack files (`ED E0`) have no parseable entry structure without an index, so only the stored byte count is shown.

Index file JSON format:
```json
{
  "type": "index-file",
  "logical_hash": "...",
  "storage_key": "...",
  "entry_count": 3,
  "entries": [
    { "type": "inline",     "hash": "1a2b...", "data_length": 42 },
    { "type": "pack",       "hash": "2b3c...", "pack_hash": "aaaa...", "offset": 0, "length": 512 },
    { "type": "standalone", "hash": "3c4d..." }
  ]
}
```

Bloom filter JSON format:
```json
{
  "type": "bloom-filter",
  "logical_hash": "...",
  "storage_key": "...",
  "num_hash_functions": 7,
  "num_bits": 8000000,
  "element_count": 50000,
  "fill_rate": 0.4375
}
```

`fill_rate` is `(number of set bits) / num_bits`.

**Pack-layer errors**

- Index root not found: `error: no index root on <remote>`

---

### omemfs stub

Convert working tree files into stubs, freeing disk space while preserving the logical tree.

```
omemfs stub <path>... [--dry-run]
```

**Arguments**
- `<path>...`: one or more files or directories to stub (required), resolved relative to the cwd. Directories are processed recursively — all regular files under the directory are stubbed.

**Options**
- `--dry-run`: show what would be stubbed without making any changes

**Preconditions**

Stubbing is only allowed when the working tree is clean with respect to the remote:

1. Each target path must exist in `clone_root` (i.e. the file has been pushed at least once). Paths not present in `clone_root` are rejected with an error directing the user to run `omemfs push` first.
2. The working tree content of each target must match `clone_root` (same hash). Unsaved local edits are rejected with an error directing the user to run `omemfs push` first.

These preconditions ensure that the object is present on the remote, so `omemfs expand` can always recover the file.

All precondition checks are performed up front before any files are modified. If any check fails, nothing is changed.

`stub` is stub-boundary-aware: it reads `clone_root` through the **local** object store only, and never descends into an already-stubbed subtree whose tree object is absent locally. Resolving a target path therefore does not fetch any remote object, and stubbing a file (or directory) that lives next to — or under — a pre-existing directory stub succeeds rather than failing with `object not found`. A target that is itself already fully stubbed is reported as "already stubbed, skipping" rather than erroring on its missing tree object.

Files that are already stubbed (`<file>.omemfs-stub` exists) are skipped with a warning.

**Behaviour**

1. Resolve each `<path>` to a repo-relative path; enumerate all regular files (directories are traversed recursively, `.omemfs/` is always excluded).
2. For each target file, verify the two preconditions above.
3. If `--dry-run`: print the list of files that would be stubbed and exit.
4. For each target file:
   a. Read the hash / size / mtime from the `clone_root` entry.
   b. Write `<file>.omemfs-stub` alongside the original file.
   c. Delete the original file.

`clone_root` is not updated. The stub is transparent to the next `omemfs push` — the scan reads the stub record and includes the same hash in the tree, so the remote sees no change.

**Output example**

```
$ omemfs stub assets/video.mp4 data/dump.csv
2 file(s) stubbed.
```

```
$ omemfs stub --dry-run assets/
  would stub: assets/video.mp4
  would stub: assets/banner.png
2 file(s) would be stubbed.
```

**Error examples**

```
error: cannot stub 'new_file.txt': not found in clone_root (run 'omemfs push' to sync first)
```

```
error: cannot stub 'notes.md': working tree differs from clone_root (run 'omemfs push' to save changes first)
```

---

### omemfs expand

Materialise one or more stubbed files into the working tree.

```
omemfs expand [<path>...] [-r | --recursive] [--stub-threshold <size>] [--remote <name>] [--dry-run]
```

**Arguments**
- `<path>...`: paths to expand (relative to the current working directory). If omitted, all `*.omemfs-stub` files found in the working tree are considered. Explicitly named paths are always expanded regardless of `--stub-threshold`.

**Options**
- `-r`, `--recursive`: expand all stubs regardless of size. Equivalent to `--stub-threshold 0`; overrides a simultaneously-given `--stub-threshold`.
- `--stub-threshold <size>`: only expand stubs whose size is strictly below this value; stubs at or above remain stubbed (default: `1M`). Format: `1024` / `100K` / `100M` / `100G`. Use `0` to expand everything (equivalent to `-r`). Invalid size strings (e.g. `1GB`, non-numeric input) are a hard CLI error.
- `--remote <name>`: remote to download blobs from when they are not in the local cache (default: `origin`)
- `--dry-run`: show what would be expanded without writing files or removing stub records

**Behaviour**

1. Collect all stub records matching the specified paths (or all stubs when no path is given). Stubs selected by explicit path argument are always included, regardless of size.
2. Apply the size filter to any stubs not selected by explicit path: stubs at or above `--stub-threshold` are skipped unless `-r`/`--recursive` is set.
3. Collect the blob hashes across all stubs to be expanded that are not yet present in the local cache, and fetch all of them from the specified remote in a single batched, concurrent transfer (via the pack index — supports inline entries, pack-file slices, and standalone objects; see `02_storage_format.md`, "Multi-root batching"), instead of downloading one blob at a time. For each fetched object, decrypt and decompress the bytes, then store them unencrypted in `.omemfs/objects/`.
4. Write the file content to the working tree path and restore `mtime` and the executable-bit `mode` from the stub record.
5. Delete the stub record **after** the file (or directory) has been written successfully. If writing fails mid-way, the stub record is left in place so the expansion can be retried.

The reported `N file(s) expanded` count is the number of **files (blobs and symlinks) actually materialised** into the working tree, not the number of top-level stub records processed. Expanding a directory stub materialises its whole subtree (down to any nested stubs left in place by `--stub-threshold`), so the count reflects every file written, which may be far greater than one.

Path arguments are normalised before matching: a trailing slash (as appended by shell tab-completion, e.g. `expand foo/`) is stripped so a directory passed with a trailing slash matches its stub record rather than silently expanding nothing.

**Output example**

```
$ omemfs expand large_dataset.zip
1 file(s) expanded.
```

```
$ omemfs expand --dry-run
  would expand: large_dataset.zip
  would expand: assets/video.mp4
2 stub(s) would be expanded.
```

```
$ omemfs expand
2 file(s) expanded, 1 stub(s) at or above threshold kept.
```

---

### omemfs stats

Print cloud-cost-oriented statistics about the remote object store, recent remote I/O history, pack-layer effectiveness, the local object cache composition, and size-distribution histograms for both remote objects and working-tree files.

This is an internal diagnostic command intended for development and operations, primarily for tuning the pack-layer size thresholds (`PACK_TARGET_SIZE`, `PACK_MAX_SIZE`, `CONSOLIDATION_THRESHOLD`, `COLD_SHARD_SPLIT_THRESHOLD`). The output opens with a one-glance **Summary** panel and is then split into a **`REMOTE`** group and a **`LOCAL`** group, each introduced by a banner line, so the reader can immediately tell which side of the sync each section describes. Within those groups the sections appear in this order:

0. **Summary** — a 2–3 line overview panel printed first: remote total (split into live + reclaimable), local cache total, and the most recent recorded I/O.
1. **Remote storage (origin)** — a cheap LIST-based capacity breakdown of the remote (only when `--remote` is given; works against any remote backend, local or cloud). *(`REMOTE` group)*
2. **Remote object sizes** — size-distribution histogram of the stored remote objects (only when `--remote` is given; works against any remote backend, local or cloud). *(`REMOTE` group)*
3. **Recent I/O (last 20 commands)** — a summary of the last 20 recorded command runs, including `pack`. *(`REMOTE` group)*
4. **Pack effectiveness** — pack-layer tuning metrics from the most recent `pack` run. *(`REMOTE` group)*
5. **Local cache composition** — the deep compression/blob-type classification of the LOCAL cache only. *(`LOCAL` group)*
6. **Working-tree file sizes** — size-distribution histogram of the working-tree files tracked by omemfs (after applying `.omemfs-filter` `[ignore]` rules). *(`LOCAL` group)*

By default `omemfs stats` performs NO remote I/O at all: only the local sections (3–6) are computed. The two remote-backed sections (1 and 2) are gated behind the explicit `--remote` flag, because they enumerate the remote (`list_with_sizes`) and read its small index files and Bloom filter. This keeps the default invocation cheap and offline-safe (e.g. when the remote is unreachable or its access would incur cost).

**Units.** All human-readable byte sizes are formatted with binary (IEC) units — `B`, `KiB` (1024 B), `MiB` (1024 KiB), `GiB` (1024 MiB) — at one decimal place (e.g. `12.4 MiB`). The compact form (`fmt_size_compact`, used inside the Recent I/O table) is identical but drops the space (`12.4MiB`). JSON output is unaffected: it always reports raw integer byte counts. Integer object/operation counts are printed with thousands separators (e.g. `1,773`).

**Section 0 — Summary panel and group banners.** Before any section, a Summary panel is printed: a title line (`omemfs stats — origin` when `--remote` resolves a remote, else `omemfs stats`) followed by a heavy rule and these lines:
- `Remote` — present only with `--remote`: `<total count> objects   <total bytes>   (live <bytes> + reclaimable <bytes>)`, where *reclaimable* is the orphan bytes and *live* is `total − orphans`. The parenthetical is omitted when there are no orphans.
- `Local` — always: `<count> objects   <stored bytes> stored` (the local cache totals).
- `Last I/O` — present only when `.omemfs/io_stats.jsonl` has records: the most recent record as `<MM-DD HH:MM:SS>  <cmd>   (<writes> writes <bytes>, <reads> reads <bytes>)`.

Each group is introduced by a banner line of the form `━━━ REMOTE ━━…` / `━━━ LOCAL ━━…` (the heavy rule visually distinguishes a group banner from the lighter per-section rule). The `REMOTE` banner is printed only when there is remote content to show (i.e. `--remote` was given, or `io_stats.jsonl` has records). The `LOCAL` banner is always printed (the local sections are unconditional).

```
omemfs stats [--remote] [--json]
```

**Options**
- `--remote`: also compute the remote-backed sections (Remote storage and Remote object sizes). Without this flag, no remote I/O is performed and those two sections are omitted. Works against any remote backend (local or cloud).
- `--json`: output statistics in JSON format instead of human-readable text

**Section 1 — Remote storage (origin)**

Shown only when `--remote` is given. Without `--remote` no remote enumeration is performed and this section is omitted. The breakdown works for any remote backend and is computed from a single cheap enumeration of the remote — `remote.list_with_sizes()` returns `(storage_key_hex, byte_size)` pairs without reading any object's contents (on cloud backends this is one paginated LIST; on a local directory it is a single directory walk + `fs::metadata`).

Each listed `(key, size)` is classified into one of the following classes by comparing the key against known storage-key sets derived from the index root:

- **pack-files** — keys of every `PackEntry.pack_hash` across the hot index, all delta indexes, and all cold shards.
- **index-files** — the hot index, every delta index, and every cold shard (storage keys of those hashes).
- **bloom** — the current Bloom filter (`bloom_hash`).
- **standalone-objects** — keys of every `StandaloneEntry.hash` across hot / delta / cold indexes.
- **index-root** — the index-root object itself. On UNENCRYPTED remotes the fixed `INDEX_ROOT` lives at the prefix root (NOT under `objects/`), so it does not appear in `list_with_sizes` (which walks `objects/`); it is accounted for separately via a `size()`/HEAD on the resolved index-root key (a filesystem stat on the local backend; a HEAD / get-properties / get-object on cloud backends).
- **orphans** — anything in the LIST not in any known set. These are unreferenced objects (e.g. old pack files left behind by consolidation, superseded index files, old Bloom filters) that are reclaimable via the backup-reclone cycle.

The per-class count and total bytes are printed, plus a grand total. The orphans line is marked as "reclaimable via backup-reclone".

This section does NOT read the contents of any data object (no `open_read` of packs / standalones). Reading the small index files (hot / delta / cold) and the Bloom filter via the pack reader / load helpers is expected and required to enumerate the known storage keys; their cost is bounded and small.

**Section 2 — Remote object sizes**

Shown only when `--remote` is given (same guard as Section 1), for any remote backend. The histogram is built from the same `(key, size)` pairs returned by `remote.list_with_sizes()` — no additional remote I/O is performed. On UNENCRYPTED remotes the fixed `INDEX_ROOT` lives outside `objects/` (not in `list_with_sizes`); its size (obtained from the `size()`/HEAD done by Section 1) is included so the histogram total matches the Remote storage total.

Fixed buckets (lower-bound inclusive):

| Bucket label | Range |
|---|---|
| `<256B` | 0 – 255 bytes |
| `256B-1KB` | 256 – 1023 bytes |
| `1-4KB` | 1 024 – 4 095 bytes |
| `4-16KB` | 4 096 – 16 383 bytes |
| `16-64KB` | 16 384 – 65 535 bytes |
| `64-256KB` | 65 536 – 262 143 bytes |
| `256KB-1MB` | 262 144 – 1 048 575 bytes |
| `1-4MB` | 1 048 576 – 4 194 303 bytes |
| `4-16MB` | 4 194 304 – 16 777 215 bytes |
| `>16MB` | ≥ 16 777 216 bytes |

Only non-empty buckets are printed. Each row shows: bucket label, count (with thousands separators), percentage of count, a `distribution` bar (a fixed-width unicode block bar whose filled fraction equals the percentage-of-count, so the visual length is proportional to how many objects fall in that bucket), total bytes (human-readable via `fmt_size`), and percentage of total bytes. A header line shows the total object count and total bytes. The same histogram renderer is shared by Section 6 (Working-tree file sizes).

**Section 3 — Recent I/O (last 20 commands)**

If `.omemfs/io_stats.jsonl` exists and is non-empty, read it, take the most recent 20 records, and print a "Recent I/O" section as a compact table (one header row plus one row per record). If the file is absent or empty, omit the section entirely. `pack` records now carry real GET/PUT/HEAD/byte counts (see Notes).

The section displays a fixed set of columns: `time`, `cmd`, optional `remote`, `writes`, `reads`, `HEAD`, `pack`. The `remote` column appears ONLY when the displayed records contain more than one distinct remote value; when all records share the same remote, the column is omitted entirely.

Column formatting rules:
- **time**: Strip the ISO-8601 timestamp down to `MM-DD HH:MM:SS` (remove the year and the `T`/`Z` separators). Example: `2026-06-13T02:05:11Z` → `06-13 02:05:11`.
- **cmd**: The command string, left-aligned.
- **remote** (conditional): The remote name, left-aligned. Shown only when more than one distinct remote is present in the displayed records.
- **writes**: `<write_ops> <compact_size>` where compact_size = write_bytes formatted without spaces (e.g. `15.0MiB`). When write_bytes == 0, show `—` for the size portion (keep the ops count: `0 —`).
- **reads**: `<read_ops> <compact_size>`, same dash rule for zero read_bytes.
- **HEAD**: `<exists_found>/<exists_miss>`. When BOTH are 0, show `—`.
- **pack**: `<pack_files_written>×<compact_total_size>` where compact_total_size is the sum of pack_sizes_bytes formatted without spaces. Example: `3×13.0MiB`. When pack_files_written == 0, show `—`. Individual pack sizes are intentionally omitted (they appear in the Pack effectiveness section instead).

Compact size format: Like `fmt_size` but without the space between number and unit (e.g. `15.0MiB`, `12.0KiB`, `512B`, `1.4GiB`). Use the same thresholds and precision as `fmt_size`.

Column widths: auto-size each column to fit the widest cell (including the header label). Use a 2-space gap between columns. Left-align text columns (time, cmd, remote). The writes/reads/HEAD/pack cells are compound strings; left-align them under their headers (matching the left edge).

Because the compound cells use terse notation, a one-line legend is printed immediately under the header row to make them self-documenting: `legend: HEAD = found/miss (bloom)   pack = files × total`.

After the table, print a blank line, then the "Totals (all recorded commands)" block. The `HEAD` total line additionally annotates the bloom miss rate with a short explanation that a miss falls through to a remote HEAD, e.g. `HEAD  8,412 ops  bloom miss rate: 3.1%  (misses fall through to a remote HEAD)`. A 100% miss rate is a performance signal (the Bloom pre-filter is not short-circuiting existence checks), not a corruption signal.

**Section 4 — Pack effectiveness**

Read `.omemfs/io_stats.jsonl`, find the most recent record whose `pack_detail` is present, and display it. If `io_stats.jsonl` has records but none carries a `pack_detail`, the header is still printed with a `(no recent consolidation recorded)` placeholder line — so the reader can tell "a pack has not been run recently" apart from "this section is missing". The whole section is omitted only when there is no I/O history at all (no `io_stats.jsonl`, same guard as the Recent I/O section). Fields are sourced from the `pack_detail` object of that record (see `omemfs pack` and `io_stats.jsonl` below):

- header line with that record's timestamp
- `delta indexes merged` — `pack_detail.deltas_merged`
- `pack files <packs_before> → <packs_after> (consolidated <bytes_in> → <bytes_out>)`
- `pack sizes <pack_sizes_after formatted>`
- `cold splits <cold_splits>`
- `hot index <hot_index_entries> entries   bloom <bloom_elements> elements`

**Section 5 — Local cache composition**

The deep scan of the LOCAL cache only (`.omemfs/objects/`): object type counts, compression methods, storage sizes, compression ratios, and blob content types. Each cache object is read and decompressed to classify it. The remote is NOT read-and-classified for this section (that is what Section 1's cheap LIST replaces).

Two presentation details improve readability:
- The **Compression (L4)** table adds a `stored` column reporting the total stored bytes attributable to each compression method (`dict_zstd`, `plain_zstd`, `escaped_raw`, `raw`) across all objects, so the size impact of each method is visible alongside its count. The existing `total` / `tree` / `blob` columns remain object counts.
- The compression-effectiveness block is framed as **`Space saved (1 − stored / logical)`** rather than the raw stored/logical ratio. Each bar's filled fraction equals the space saved, so a *longer* bar means *better* compression (more intuitive than the previous ratio bar, where a shorter bar meant better). The printed percentage is the saved percentage (`100 − stored/logical%`). The JSON `compression_ratio` field is unchanged (it remains the `stored / logical` fraction).

This scan covers `.omemfs/objects/` only. The sibling caches `.omemfs/packcache/` (raw remote pack files) and `.omemfs/objcache/` (decrypted remote index files) are NOT scanned: they hold L6 remote artifacts, not logical objects, and are reported via the remote sections instead. Because index files are cached under `objcache/`, an object carrying a pack-layer magic (`ED E1..EF`) encountered under `objects/` is genuinely anomalous and is counted as `unknown` — the `unknown` count is therefore a meaningful corruption/misplacement signal. (Exception: a repository created by an earlier build that cached index files under `objects/` will show those as `unknown`. They are unreferenced and may be deleted, or disappear on a fresh clone; pre-production, no migration is performed.)

**Section 6 — Working-tree file sizes**

Always shown (no remote required). A read-only walk of `repo.work_dir` collects the sizes of all files that omemfs tracks (i.e. after applying `.omemfs-filter` `[ignore]` rules). The same fixed buckets as Section 2 are used; only non-empty buckets are printed.

The walk mirrors exactly the ignore and exclusion semantics of `scan.rs`:
- The `.omemfs/` directory at every level is skipped.
- Files matching `.omemfs-stub` / `*.omemfs-stub`, `.omemfs-conflict-*` patterns are skipped.
- `.omemfs-filter` itself IS tracked (it is included in the push scan).
- Directories that are ignored by a `[ignore]` pattern are pruned immediately (not descended into). This is essential for performance — `node_modules/` and similar large ignored trees must not be scanned at all.
- Symlinks are excluded (they have no file-size blob in the tracked object model).
- The walk only calls `fs::symlink_metadata` / `len()` — no hashing or object storage occurs.

**Output example (text)**

The example below is for `omemfs stats --remote`; a default `omemfs stats` (no `--remote`) starts at the "Recent I/O" section and omits the two leading remote sections.

```
omemfs stats — origin
══════════════════════════════════════════════════════════════════════
  Remote      13 objects   19.1 MiB   (live 17.1 MiB + reclaimable 2.0 MiB)
  Local    1,234 objects   48.2 MiB stored
  Last I/O   06-13 02:05:11  push   (251 writes 15.0 MiB, 3 reads 12.0 KiB)

━━━ REMOTE ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Remote storage  (origin)
──────────────────────────────────────────────────────────────────────
  pack-files            3   13.0 MiB
  index-files           4    8.0 KiB
  bloom                 1    1.2 KiB
  standalone-objects    2    4.1 MiB
  index-root            1      512 B
  orphans               2    2.0 MiB   (reclaimable via backup-reclone)
  ────────────────────────────────────
  total                13   19.1 MiB

Remote object sizes  (origin)   (n=13, total=19.1 MiB)
──────────────────────────────────────────────────────────────────────
  bucket        count    %cnt   distribution          bytes    %byt
  1-4KB             4   30.8%   ██████░░░░░░░░░░░░░   8.7 KiB    0.0%
  256KB-1MB         2   15.4%   ███░░░░░░░░░░░░░░░░   8.2 MiB   42.9%
  1-4MB             7   53.8%   ██████████░░░░░░░░  10.9 MiB   57.1%

Recent I/O  (last 20 commands)
──────────────────────────────────────────────────────────────────────
time            cmd   writes        reads        HEAD    pack
legend: HEAD = found/miss (bloom)   pack = files × total
06-13 02:05:11  push  251 15.0MiB  3 12.0KiB    2/248   3×13.0MiB
06-13 01:45:12  pack  12 2.1MiB    6 48.0KiB    —       —
06-13 01:30:02  pull  0 —          8 96.0KiB    1/40    —

Totals (all recorded commands)
  writes  1,043 ops  123.4 MiB
  reads     312 ops   18.2 MiB
  HEAD    8,412 ops  bloom miss rate: 3.1%  (misses fall through to a remote HEAD)

Pack effectiveness  (2026-06-13 01:45:12)
──────────────────────────────────────────────────────────────────────
  delta indexes merged  3
  pack files  5 → 2  (consolidated 1.8 MiB → 1.8 MiB)
  pack sizes  1.1 MiB  700.0 KiB
  cold splits  0
  hot index  128 entries     bloom  140 elements

━━━ LOCAL ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Local cache composition
──────────────────────────────────────────────────────────────────────
Objects:  1,234 total
  blob             1,089  ( 88.3%)
  tree               123  (  9.9%)
  ...

Compression (L4):       total    tree    blob      stored
  dict_zstd                98      98       0   148.0 KiB
  plain_zstd               32       0      32     2.1 MiB
  raw                   1,099       0   1,099    45.9 MiB

Storage:
  total   48.2 MiB
  tree    avg  1.2 KiB, min 64 B, max 12.4 KiB
  blob    avg  44.0 KiB, min 1 B, max 2.1 MiB

Space saved (1 − stored / logical):
  tree  ████████░░░░░░░░░░░░  41.7%
  blob  ██░░░░░░░░░░░░░░░░░░   8.8%

Working-tree file sizes   (n=3,975, total=1.4 GiB)
──────────────────────────────────────────────────────────────────────
  bucket        count    %cnt   distribution          bytes    %byt
  <256B           512   12.9%   ██░░░░░░░░░░░░░░░░░  64.0 KiB    0.0%
  256B-1KB        362    9.1%   ██░░░░░░░░░░░░░░░░░ 317.1 KiB    0.0%
  1-4KB           612   15.4%   ███░░░░░░░░░░░░░░░░   1.4 MiB    0.1%
  4-16KB          843   21.2%   ████░░░░░░░░░░░░░░░   7.2 MiB    0.5%
  16-64KB         563   14.2%   ███░░░░░░░░░░░░░░░░  18.2 MiB    1.3%
  64-256KB        418   10.5%   ██░░░░░░░░░░░░░░░░░  60.2 MiB    4.3%
  256KB-1MB       247    6.2%   █░░░░░░░░░░░░░░░░░░ 128.4 MiB    9.1%
  1-4MB           331    8.3%   ██░░░░░░░░░░░░░░░░░ 545.0 MiB   38.5%
  4-16MB           82    2.1%   ░░░░░░░░░░░░░░░░░░ 648.7 MiB   45.8%
  >16MB             5    0.1%   ░░░░░░░░░░░░░░░░░░  21.2 MiB    1.5%
```

**Output example (JSON)**

```json
{
  "total": 1234,
  "by_type": { "tree": 123, "blob": 1089, "chunk_manifest": 0, "chunk_body": 0, "unknown": 4 },
  "by_compression": {
    "total": { "dict_zstd": 98, "plain_zstd": 32, "escaped_raw": 5, "raw": 1099 },
    "tree":  { "dict_zstd": 98, "plain_zstd": 0,  "escaped_raw": 0, "raw": 0 },
    "blob":  { "dict_zstd": 0,  "plain_zstd": 32, "escaped_raw": 5, "raw": 1099 }
  },
  "storage_bytes": { "total": 50544640, "tree_total": 153600, "tree_min": 64, "tree_max": 12698, "blob_total": 50391040, "blob_min": 1, "blob_max": 2202009 },
  "compression_ratio": { "tree": 0.583, "blob": 0.912 },
  "remote_storage": {
    "pack_files":         { "count": 3,  "bytes": 13631488 },
    "index_files":        { "count": 4,  "bytes": 8192 },
    "bloom":              { "count": 1,  "bytes": 1229 },
    "standalone_objects": { "count": 2,  "bytes": 4300800 },
    "index_root":         { "count": 1,  "bytes": 512 },
    "orphans":            { "count": 2,  "bytes": 2097152 },
    "total":              { "count": 13, "bytes": 20039173 }
  },
  "remote_object_histogram": [
    { "bucket": "1-4KB",    "count": 4, "bytes": 8909 },
    { "bucket": "256KB-1MB","count": 2, "bytes": 8601600 },
    { "bucket": "1-4MB",    "count": 7, "bytes": 11428864 }
  ],
  "pack_effectiveness": {
    "ts": "2026-06-13T01:45:12Z",
    "deltas_merged": 3,
    "packs_before": 5,
    "packs_after": 2,
    "consolidated_bytes_in": 1887436,
    "consolidated_bytes_out": 1887436,
    "cold_splits": 0,
    "hot_index_entries": 128,
    "bloom_elements": 140,
    "pack_sizes_after": [1153433, 716800]
  },
  "io_history": [
    {
      "ts": "2026-06-13T02:05:11Z", "cmd": "push", "remote": "origin",
      "exists_found": 2, "exists_miss": 248,
      "writes": 251, "write_bytes": 15728640, "reads": 3, "read_bytes": 12288,
      "pack_files_written": 3, "pack_sizes_bytes": [5242880, 5242880, 3145728]
    }
  ],
  "io_totals": {
    "writes": 1043, "write_bytes": 129477632, "reads": 312, "read_bytes": 19083264,
    "exists_found": 18, "exists_miss": 8394
  },
  "worktree_file_histogram": [
    { "bucket": "<256B",    "count": 512, "bytes": 65536 },
    { "bucket": "256B-1KB", "count": 362, "bytes": 324710 },
    { "bucket": "1-4KB",    "count": 612, "bytes": 1468416 },
    { "bucket": "4-16KB",   "count": 843, "bytes": 7549952 }
  ]
}
```

`remote_storage` and `remote_object_histogram` are present only when `--remote` is given AND origin is a local-type remote (omitted otherwise — in particular, a default `omemfs stats --json` without `--remote` never contains these keys). `remote_object_histogram` is an array of `{"bucket": "...", "count": N, "bytes": N}` for non-empty buckets in ascending order; it is an empty array `[]` when no remote listing is available. `worktree_file_histogram` is always present and follows the same format. `pack_effectiveness` is present only when a pack record with `pack_detail` exists (omitted, or rendered as `null`, otherwise). `io_history` contains the most recent 20 records from `.omemfs/io_stats.jsonl`; `io_totals` contains aggregate sums across all recorded commands. `io_history`/`io_totals` are omitted when the file is absent or empty.

**Notes**

- Objects that cannot be parsed (corrupted or unknown format) are counted as `unknown` and excluded from size statistics (local cache composition section).
- `.omemfs/objects` stores objects without encryption; the compression layer operates directly on the serialised object bytes.
- The Remote storage section uses a cheap LIST + index-metadata classification: it never reads the contents of data objects. The deep compression/blob-type classification (Local cache composition) is now LOCAL-CACHE ONLY.
- `.omemfs/io_stats.jsonl` records remote I/O (operations against origin or backup remote stores) for each successful run of push, pull, pack, clone, or expand. `pack` now records **real** GET/PUT/HEAD/byte counts (the remote it wraps is a `StatsStore`), so its record is no longer a zero-count placeholder. Pack records additionally carry a `pack_detail` object with pack-layer tuning metrics (`deltas_merged`, `packs_before`, `packs_after`, `consolidated_bytes_in`, `consolidated_bytes_out`, `cold_splits`, `hot_index_entries`, `bloom_elements`, `pack_sizes_after`, `orphaned_bytes`). Non-pack records omit `pack_detail`.
- Every record carries `duration_ms`, the wall-clock time (in milliseconds) from when the command opened its remote connection to when it finished writing the record — collected so `omemfs pack`'s scheduling can be reconsidered later from real data (how long a pack run holds the repo lock, whether push/pull durations grow as unconsolidated deltas accumulate). Records written before this field existed lack the key and deserialise with `duration_ms: 0`.
- `push` records additionally carry `deltas_after`: the number of delta index files listed in `INDEX_ROOT.delta_hashes` immediately after this push's CAS write (i.e. including the delta this push itself just added). This is the direct input for a delta-count-based pack scheduling policy (e.g. "run `omemfs pack` once `deltas_after` crosses N"). Non-push records omit the field.
- `pack_detail.orphaned_bytes` is a **lower-bound estimate** of the bytes this pack run made unreachable from `INDEX_ROOT`: the size of the previous hot index, the combined size of all delta index files just merged away, and the previous Bloom filter's size are counted in full (each is unconditionally replaced by a new object with a new hash); the size of consolidated small pack files is approximated by `consolidated_bytes_in` (the payload bytes actually copied out of them), which understates the true figure whenever a consolidated pack file still has other entries referenced from a cold shard (that pack file is not fully orphaned, but its consolidated entries' bytes are still counted here as if it were). Used together with `orphans` in `omemfs stats --remote`'s Remote storage section to judge whether a given pack cadence is worth its storage cost.
- Pack's index-root read and CAS-write go through the `LocalRootPointer` file-ops path (not through `ObjectStore`), so those operations are NOT counted in the pack I/O record — an accepted minor undercount.
- Local cache I/O is not counted. Commands that fail before touching the remote do not append a record. `omemfs stats` displays only the most recent 20 entries.
- Retention: before appending a new record, if `io_stats.jsonl` exceeds **10 MiB**, it is rewritten to keep only the newest (line-count) half of its records, then the new record is appended. This bounds the file's growth (refactor-instructions.md G4a) without needing a separate maintenance command. Rotation is best-effort: any I/O error during it is silently ignored, and the append is attempted regardless.

---

### omemfs log

Analyse structured log files written to `.omemfs/logs/` by previous command runs. The `log` subcommands are read-only and do not open a new log file.

```
omemfs log <subcommand>
```

For full details on the log format and available fields, see `design/10_log_analysis.md`.

**Subcommands**

- `ls` — list log files in the repository, newest first
- `show` — display log lines with optional layer/grep filtering
- `timers` — aggregate timer spans and print statistics

#### omemfs log ls

```
omemfs log ls [-n <count>] [--cmd <command>]
```

**Options**
- `-n <count>`: show at most N entries (default: 10)
- `--cmd <command>`: show only logs whose filename contains this command name (e.g. `push`)

#### omemfs log show

```
omemfs log show [<ref>] [--layer <layer>]... [--grep <pattern>]
```

**Arguments**
- `<ref>`: log to display. Omitted = latest log. `@N` selects the Nth most recent entry (1-indexed). A logical name (e.g. `push`) selects the latest log for that command. A file path is used directly.

**Options**
- `--layer <layer>`: show only lines from this layer (repeatable; e.g. `--layer L4`)
- `--grep <pattern>`: show only lines whose message contains this pattern

#### omemfs log timers

```
omemfs log timers [<ref>] [--sort <key>] [--layer <layer>]...
```

**Arguments**
- `<ref>`: same reference form as `log show`

**Options**
- `--sort <key>`: sort key — `total` (default), `avg`, `count`, or `max`
- `--layer <layer>`: restrict to lines from this layer (repeatable)

---

### omemfs conflict

Manage and resolve conflict helper files created by `pull` when the same path was modified both locally and remotely.

```
omemfs conflict <subcommand> [--dry-run] [<path>...]
```

`<path>` scopes the operation to a specific file or directory subtree. When omitted, the entire working tree is scanned.

**Subcommands**

- `list` — list all paths that have unresolved conflict helper files
- `clean` — delete conflict helper files without touching the originals
- `accept-remote` — adopt the remote version and remove helper files
- `accept-local` — adopt the local version and remove helper files
- `accept-base` — adopt the base (clone root) version and remove helper files

**Conflict metadata sidecar.** Alongside the three `<path>.omemfs-conflict-{base,local,remote}` helpers, `pull` writes one `<path>.omemfs-conflict-meta` sidecar (JSON) recording the tracked metadata (modification time and the executable-bit mode) of the **base** and **remote** sides. omemfs tracks `mtime` and `mode` in each tree entry, so they are part of the entry's hash. Without restoring them, `accept-remote` / `accept-base` would leave the resolved file with a fresh `mtime`, so its tree-entry hash would differ from the remote / clone root and the next `push` would needlessly re-upload it (and mint a new remote root) even though the content is identical. The sidecar lets `accept` restore the accepted side's metadata so the resolution is idempotent against that root. The **local** side is intentionally not recorded: `accept-local` keeps the working-tree file as-is, so its metadata needs no restoration. The sidecar is excluded from scan/push exactly like the conflict helpers, and is removed by `accept` and `clean`.

---

#### omemfs conflict list

List all file paths that currently have unresolved conflict helper files. No path argument; no `--dry-run`.

```
omemfs conflict list
```

Output: one path per line (the base file path, not the helper file name), followed by a summary count.

```
$ omemfs conflict list
  src/main.rs
  docs/api.md
2 paths with unresolved conflicts.
```

If there are no conflicts, prints nothing and exits 0.

---

#### omemfs conflict clean

Delete conflict helper files (`<path>.omemfs-conflict-{base,local,remote}`). The original file is **not** modified.

```
omemfs conflict clean [--dry-run] [<path>...]
```

**Behaviour**

1. Scan the target scope for all `*.omemfs-conflict-{base,local,remote}` files.
2. Delete each helper file, plus the `.omemfs-conflict-meta` sidecar for each affected base path.

**Output example**

```
$ omemfs conflict clean
  deleted: src/main.rs.omemfs-conflict-base
  deleted: src/main.rs.omemfs-conflict-local
  deleted: src/main.rs.omemfs-conflict-remote
1 conflict cleaned.
```

With `--dry-run`:

```
$ omemfs conflict clean --dry-run
  would delete: src/main.rs.omemfs-conflict-base
  would delete: src/main.rs.omemfs-conflict-local
  would delete: src/main.rs.omemfs-conflict-remote
1 conflict would be cleaned (dry run).
```

**Use case**: discard all resolution aids and leave the working tree as-is, intending to edit the original manually before pushing.

---

#### omemfs conflict accept-remote

Adopt the remote version for each conflicting path and remove the helper files.

```
omemfs conflict accept-remote [--dry-run] [<path>...]
```

**Behaviour** for each conflicting path `<p>`:

1. If `<p>.omemfs-conflict-remote` exists: overwrite `<p>` with its contents, then restore the remote side's tracked `mtime` and `mode` from the conflict metadata sidecar (if present) so the result is idempotent — a subsequent `push` does not re-upload an unchanged file.
2. If `<p>.omemfs-conflict-remote` does not exist (local-add vs remote-delete conflict): delete `<p>`.
3. Delete all three helper files for `<p>` (whichever exist), plus the `<p>.omemfs-conflict-meta` sidecar.

**Output example**

```
$ omemfs conflict accept-remote src/main.rs
  accepted remote: src/main.rs
1 conflict resolved.
```

**Errors**

- No conflict helper files found for the given path: `error: no conflict files found for 'src/lib.rs'`

---

#### omemfs conflict accept-local

Adopt the local working tree version for each conflicting path and remove the helper files.

```
omemfs conflict accept-local [--dry-run] [<path>...]
```

**Behaviour** for each conflicting path `<p>`:

1. If `<p>.omemfs-conflict-local` exists: overwrite `<p>` with its contents.
2. If `<p>.omemfs-conflict-local` does not exist (local-delete vs remote-modify conflict): delete `<p>`.
3. Delete all three helper files for `<p>` (whichever exist).

**Output example**

```
$ omemfs conflict accept-local config/settings.toml
  accepted local: config/settings.toml
1 conflict resolved.
```

---

#### omemfs conflict accept-base

Adopt the base (clone root) version for each conflicting path and remove the helper files. This discards both the local and remote changes, restoring the last-synced state.

```
omemfs conflict accept-base [--dry-run] [<path>...]
```

**Behaviour** for each conflicting path `<p>`:

1. If `<p>.omemfs-conflict-base` exists: overwrite `<p>` with its contents, then restore the base side's tracked `mtime` and `mode` from the conflict metadata sidecar (if present) so the result matches the clone-root state.
2. If `<p>.omemfs-conflict-base` does not exist (both sides added a new file): delete `<p>` — the base state is absence of the file.
3. Delete all three helper files for `<p>` (whichever exist), plus the `<p>.omemfs-conflict-meta` sidecar.

**Output example**

```
$ omemfs conflict accept-base docs/readme.md
  accepted base: docs/readme.md
1 conflict resolved.
```

---

#### Relationship to other commands

- **`push`**: errors out if any conflict helper files exist in the working tree. Resolve all conflicts before pushing.
- **`restore <path>`**: discards local changes and also removes any conflict helper files for the restored paths.
- **`ls`**: the `Z` column shows `!` for files (and their ancestor directories) that have conflict helper files.

After resolving all conflicts with `omemfs conflict accept-*` or by editing the originals manually, run `omemfs push` to commit the resolved state.

---

### omemfs pack

Compact the remote pack layer and merge delta indexes.

```
omemfs pack
```

`omemfs pack` is a maintenance command. It operates on the `origin` remote and requires no path arguments. No options are supported.

**Operations performed (in order)**

1. **Merge delta indexes** — all delta index files accumulated by previous `push` runs are merged into the hot index. The `delta_hash[]` list in the index root is cleared.
2. **Split hot vs cold** — entries no longer reachable from this clone's `working tree`, `clone_root`, and `remote_root` are moved from the hot index to cold shards. Standalone entries are never moved to cold shards; they remain reachable via `objects/<storage_key>` directly.
3. **Consolidate small pack files** — pack files smaller than **2 MiB** that are referenced by the hot index are merged into new files targeting **4 MiB** each (up to **16 MiB** max). This reduces the number of GET requests required during reads.
4. **Split oversized cold shards** — if any cold shard file exceeds **4 MiB**, it is split: the most-populous hash-prefix is extracted into a new dedicated shard, and the remainder is written to a new shared shard. Exactly 2 new shard files are produced per `omemfs pack` invocation.
5. **Regenerate Bloom filter** — the Bloom filter is rebuilt from scratch using all entries in the hot index, cold shards, and standalone objects. This eliminates accumulated false positives.
6. **CAS-update index root** — the updated index root (new hot index hash, cleared delta list, new cold shard hashes, new Bloom filter hash) is written with a compare-and-swap to detect concurrent modifications.

Unreferenced objects produced as a side effect of consolidation, cold-shard splitting, and Bloom-filter regeneration are **not** deleted by `omemfs pack`. Storage is reclaimed via the backup-reclone cycle: push to a backup remote (which copies only objects reachable from `clone_root`, dropping orphans naturally), then adopt the backup as the new origin. Each run's approximate contribution to this reclaimable total is recorded as `pack_detail.orphaned_bytes` in `.omemfs/io_stats.jsonl` (see "omemfs stats" → io_stats.jsonl Notes).

**When to run**

`omemfs pack` is not required for correctness. The system functions correctly with an unbounded list of delta indexes. However, pack performance degrades as deltas accumulate, because each read must search all delta files sequentially. Running `omemfs pack` periodically (e.g., after a large batch of pushes) keeps read latency low.

To reconsider the pack cadence later from real data rather than guesswork, `.omemfs/io_stats.jsonl` records, for every command, `duration_ms` (how long the run took — for `pack`, this includes holding the repo lock that blocks concurrent push/pull) and, for every `push`, `deltas_after` (the resulting count of unconsolidated delta index files). Comparing the growth of `deltas_after` and of push/pull `duration_ms` against a pack run's own `duration_ms` and `pack_detail.orphaned_bytes` answers whether the current cadence is too frequent (paying pack's fixed cost and storage churn for little benefit) or too infrequent (deltas piling up, slower lookups) for the actual push volume and repository size.

**Output**

On success:

```
omemfs pack: done.
```

If no index root exists on the remote (i.e. no objects have been pushed yet):

```
omemfs pack: no index root found; nothing to do.
```

**Errors**

- The configured `origin` remote refers to an unknown or misconfigured backend (for example an unrecognised `type`, or a cloud remote missing required credential fields): the command reports the configuration error and exits with a non-zero status before touching the remote.
- Any storage I/O error is reported and the command exits with a non-zero status. If the error occurs before the index root CAS write, the remote state is unchanged.
- The index root was updated by another client (concurrent `push` or `pack`) between this `pack`'s snapshot read and its CAS write. The CAS write fails, the remote state is left untouched, and the command reports:

  ```
  error: remote has been updated since last sync
  Re-run 'omemfs pack'.
  ```

  Unlike the `push` CAS error, no `pull` is needed: `pack` only rewrites the pack layer and index root and does not touch the working tree. Simply re-running `omemfs pack` re-reads the current index root and retries.

**Locking**

`omemfs pack` acquires the clone root lock (`.omemfs/clone_root.lock`) for the duration of the run. Other commands that require the lock (push, pull, restore, stub, expand, pack, conflict clean) will fail to acquire the lock until `omemfs pack` completes.
