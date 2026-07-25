use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::codec::encrypt::EncryptKey;
use crate::error::Error;
use crate::lock::RepoLock;
use crate::object::Hash;
use crate::store::local::{LocalStore, atomic_write, sync_local_objects_fs};

const OMEMFS_DIR: &str = ".omemfs";
const CONFIG_FILE: &str = "config";
const CLONE_ROOT_FILE: &str = "clone_root";
const OBJECTS_DIR: &str = "objects";
const TMP_DIR: &str = "tmp";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: String,
    pub remotes: std::collections::HashMap<String, RemoteConfig>,
}

/// Per-remote encryption settings. The DEK is a randomly generated 32-byte
/// value stored as base64. It is kept in the local config only; the remote
/// backend never receives the plaintext DEK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    pub algorithm: String,
    pub dek: String, // base64-encoded 32 bytes
}

impl EncryptionConfig {
    /// Generate a new random DEK and return an EncryptionConfig.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut dek_bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut dek_bytes);
        let dek = base64::engine::general_purpose::STANDARD.encode(dek_bytes);
        EncryptionConfig {
            algorithm: "aes-256-gcm".to_string(),
            dek,
        }
    }

    /// Decode and return the raw 32-byte DEK.
    pub fn decode_key(&self) -> Result<EncryptKey, Error> {
        let dek_bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.dek)
            .map_err(|e| Error::Other(format!("invalid DEK in config: {e}")))?;
        if dek_bytes.len() != 32 {
            return Err(Error::Other(format!(
                "DEK must be 32 bytes, got {}",
                dek_bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&dek_bytes);
        Ok(EncryptKey::new(arr))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RemoteConfig {
    Local {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        encryption: Option<EncryptionConfig>,
    },
    S3 {
        bucket: String,
        region: String,
        prefix: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        access_key_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        secret_access_key: Option<String>,
        /// Custom S3 service URL for S3-compatible stores such as MinIO.
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        /// Use path-style addressing (`host/bucket/key`) instead of
        /// virtual-hosted style. Required by MinIO and most S3-compatible stores.
        #[serde(skip_serializing_if = "Option::is_none")]
        force_path_style: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encryption: Option<EncryptionConfig>,
    },
    /// Azure Blob Storage. Authentication is Entra ID (Azure AD) only —
    /// `ClientSecretCredential` from `tenant_id` / `client_id` /
    /// `client_secret`. No account key, no SAS (see design/13_cloud_backends.md).
    Azure {
        account: String,
        container: String,
        prefix: String,
        tenant_id: String,
        client_id: String,
        client_secret: String,
        /// Custom blob service URL (defaults to
        /// `https://<account>.blob.core.windows.net`).
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encryption: Option<EncryptionConfig>,
    },
    /// Google Cloud Storage. Auth: service-account JSON (file path or inline) /
    /// Application Default Credentials / anonymous (emulator).
    Gcs {
        bucket: String,
        prefix: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        credentials_json_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        credentials_json: Option<String>,
        /// Custom storage endpoint (e.g. the storage-testbench emulator).
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encryption: Option<EncryptionConfig>,
    },
}

impl RemoteConfig {
    pub fn encryption(&self) -> Option<&EncryptionConfig> {
        match self {
            RemoteConfig::Local { encryption, .. } => encryption.as_ref(),
            RemoteConfig::S3 { encryption, .. } => encryption.as_ref(),
            RemoteConfig::Azure { encryption, .. } => encryption.as_ref(),
            RemoteConfig::Gcs { encryption, .. } => encryption.as_ref(),
        }
    }
}

impl Config {
    pub fn new(remote_name: &str, remote: RemoteConfig) -> Self {
        let mut remotes = std::collections::HashMap::new();
        remotes.insert(remote_name.to_string(), remote);
        Config {
            version: "2.0".to_string(),
            remotes,
        }
    }
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

/// A local omemfs repository rooted at a working-tree directory.
pub struct Repo {
    /// Root of the working tree (parent of `.omemfs/`).
    pub work_dir: PathBuf,
}

impl Repo {
    /// Open an existing repository. Returns an error if `.omemfs/` is absent.
    pub fn open(work_dir: impl Into<PathBuf>) -> Result<Self, Error> {
        let work_dir = work_dir.into();
        let omemfs_dir = work_dir.join(OMEMFS_DIR);
        if !omemfs_dir.is_dir() {
            return Err(Error::Other(format!(
                "not a omemfs repository (no .omemfs/ found in {})",
                work_dir.display()
            )));
        }
        Ok(Repo { work_dir })
    }

    /// Discover the repository by walking up from `start` until a directory
    /// containing `.omemfs/` is found; that directory becomes `work_dir`. The
    /// search proceeds to the filesystem root. Mirrors how `git` locates `.git`,
    /// so any command may run from a subdirectory of the working tree.
    ///
    /// `start` should be an absolute, canonicalised path (e.g. the canonicalised
    /// cwd) so the returned `work_dir` is canonical and `strip_prefix` against it
    /// in `normalize_path` is reliable across symlinks.
    pub fn discover(start: impl Into<PathBuf>) -> Result<Self, Error> {
        let start = start.into();
        let mut dir = start.as_path();
        loop {
            if dir.join(OMEMFS_DIR).is_dir() {
                return Ok(Repo {
                    work_dir: dir.to_path_buf(),
                });
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => {
                    return Err(Error::Other(format!(
                        "not a omemfs repository (no .omemfs/ found in {} or any parent)",
                        start.display()
                    )));
                }
            }
        }
    }

    /// Initialise a new repository in `work_dir`. Creates `.omemfs/` and
    /// writes `config`. `clone_root` is not written here; it is created later
    /// by the first successful `clone`/`pull`/`push`.
    pub fn init(work_dir: impl Into<PathBuf>, config: Config) -> Result<Self, Error> {
        let work_dir = work_dir.into();
        let omemfs_dir = work_dir.join(OMEMFS_DIR);
        fs::create_dir_all(&omemfs_dir)?;
        fs::create_dir_all(omemfs_dir.join(OBJECTS_DIR))?;
        fs::create_dir_all(omemfs_dir.join(TMP_DIR))?;

        // Write config durably (fsync + atomic rename): the config holds the DEK,
        // whose loss is unrecoverable. Use atomic_write rather than fs::write.
        let config_path = omemfs_dir.join(CONFIG_FILE);
        let config_json = serde_json::to_string_pretty(&config).map_err(Error::Json)?;
        atomic_write(&config_path, config_json.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))?;
        }

        Ok(Repo { work_dir })
    }

    fn omemfs_dir(&self) -> PathBuf {
        self.work_dir.join(OMEMFS_DIR)
    }

    fn clone_root_path(&self) -> PathBuf {
        self.omemfs_dir().join(CLONE_ROOT_FILE)
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.omemfs_dir().join(OBJECTS_DIR)
    }

    /// Read the current clone root. Returns `None` if no sync has happened yet.
    pub fn read_clone_root(&self) -> Result<Option<Hash>, Error> {
        let path = self.clone_root_path();
        match fs::read_to_string(&path) {
            Ok(s) => {
                let t = s.trim();
                if t.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Hash::from_hex(t)?))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Write `hash` to `clone_root` atomically and durably.
    /// Issues a durability barrier first to ensure all local cache objects
    /// referenced by `hash` have reached stable storage before the pointer is
    /// persisted (they are written without per-object fsync).
    pub fn write_clone_root(&self, hash: &Hash) -> Result<(), Error> {
        sync_local_objects_fs(&self.objects_dir())?;
        let content = hash.as_str().to_string() + "\n";
        atomic_write(&self.clone_root_path(), content.as_bytes())?;
        Ok(())
    }

    /// Acquire the exclusive repository lock (`.omemfs/clone_root.lock`).
    ///
    /// The returned `RepoLock` releases the lock on drop. Hold it for the
    /// duration of any operation that reads or writes `clone_root`.
    pub fn acquire_lock(&self) -> Result<RepoLock, Error> {
        RepoLock::acquire(&self.omemfs_dir())
    }

    /// Return the local object cache store (`.omemfs/objects/`).
    pub fn local_store(&self) -> LocalStore {
        LocalStore::for_cache(self.objects_dir())
    }

    pub fn packcache_dir(&self) -> PathBuf {
        self.omemfs_dir().join("packcache")
    }

    /// Return the local pack cache store (`.omemfs/packcache/`).
    ///
    /// This cache holds raw (still-encrypted) remote pack files, content-addressed
    /// by pack_hash. It is populated on demand by `PackReader::fetch_pack_slice`.
    /// Unencrypted: pack files are stored as-is from the remote; decryption happens
    /// downstream in the codec layer, not here.
    pub fn packcache_store(&self) -> LocalStore {
        LocalStore::for_cache(self.packcache_dir())
    }

    pub fn objcache_dir(&self) -> PathBuf {
        self.omemfs_dir().join("objcache")
    }

    /// Return the local index-file cache store (`.omemfs/objcache/`).
    ///
    /// This cache holds remote index files (delta / hot / cold shards) as
    /// PLAINTEXT, decrypted once on first fetch. It is the plaintext sibling of
    /// `packcache_store()` (which holds raw encrypted pack files). Index files are
    /// content-addressed and immutable, so cache entries are never stale.
    pub fn objcache_store(&self) -> LocalStore {
        LocalStore::for_cache(self.objcache_dir())
    }

    /// Return the remote store for the named remote. Encryption is determined by
    /// the remote's own `encryption` field.
    ///
    /// Return type: `LocalStore` (NOT `Box<dyn ObjectStore>`). Per the
    /// encryption-layering decision (private/s3-backend-plan.md section B item
    /// 7), the storage-key HMAC stays in the `LocalStore` wrapper and the cloud
    /// backends are added as `ObjectsBackend::S3/Azure/Gcs` variants INSIDE
    /// `LocalStore`, rather than wrapping a `Box<dyn ObjectStore>` in an
    /// `EncryptingStore`. Keeping the concrete `LocalStore` return type means
    /// the ~24 consumer call sites that use `LocalStore`-specific methods
    /// (`.encrypt_key`, `.storage_key_of`, `.open_read_by_storage_key`,
    /// `.clone()`) stay byte-for-byte unchanged when the cloud arms are wired in
    /// Phase 3 — zero consumer churn.
    ///
    /// The S3 / Azure / GCS arms are implemented: each constructs its cloud
    /// client, wraps it in the corresponding `ObjectsBackend` cloud variant
    /// inside `LocalStore`, and returns that `LocalStore` here (so the HMAC
    /// and the encryption layering are reused unchanged for cloud remotes).
    pub fn remote_store(&self, remote_name: &str) -> Result<LocalStore, Error> {
        let config = self.read_config()?;
        let remote = config
            .remotes
            .get(remote_name)
            .ok_or_else(|| Error::Other(format!("remote '{}' not configured", remote_name)))?;
        store_for_config(remote)
    }

    /// Return the backend-pluggable index-root pointer for the named remote.
    ///
    /// This is the single place that maps a remote's type to its `RootPointer`
    /// implementation: local-directory remotes get a `LocalRootPointer`, and
    /// each cloud backend (S3 / Azure / GCS) has its own `RootPointer` arm,
    /// mirroring `remote_store`.
    pub fn remote_root_pointer(
        &self,
        remote_name: &str,
    ) -> Result<Box<dyn crate::codec::pack::root_pointer::RootPointer>, Error> {
        let config = self.read_config()?;
        let remote = config
            .remotes
            .get(remote_name)
            .ok_or_else(|| Error::Other(format!("remote '{}' not configured", remote_name)))?;
        root_pointer_for_config(remote)
    }

    /// Build a fully-wired `PackReader` for `remote_name`, returning it
    /// alongside the raw remote `LocalStore` and the remote's encryption key
    /// -- callers typically need at least one of these independently
    /// afterward (e.g. `LazyTreeStore::new`, `apply_diff`, a direct
    /// `codec::store_read` against the same remote, or `commands::cat`'s
    /// `print_pack_object`/`resolve_prefix_on_remote`, which take the raw
    /// store directly rather than going through the `PackReader` wrapper).
    ///
    /// When `io_record` is `Some`, the remote is wrapped in a `StatsStore` so
    /// every GET/HEAD/byte through the `ObjectStore` trait is counted onto
    /// it; pass `None` for a diagnostic read that should not appear in
    /// `omemfs stats`' I/O history (e.g. `commands::cat`'s lookups).
    ///
    /// Consolidates the 6-argument `PackReader::new` call plus its preceding
    /// raw_remote/remote_key/StatsStore wiring, which was copy-pasted at
    /// around a dozen call sites (refactor-instructions.md E1). Two call
    /// sites are NOT expressible here and stay direct `PackReader::new`
    /// calls: `commands::stats`'s "throwaway local cache" variant (its
    /// local_cache is the remote itself, since `read_index_root` never
    /// consults it) and `commands::pack`'s `collect_hot_hashes` (which needs
    /// `if let (Ok(_), Ok(_)) = (...)` tolerance rather than `?`, since a
    /// missing remote there is skipped, not fatal).
    pub fn pack_reader(
        &self,
        remote_name: &str,
        io_record: Option<&std::sync::Arc<crate::store::stats::IoRecord>>,
    ) -> Result<
        (
            crate::codec::pack::reader::PackReader,
            LocalStore,
            Option<EncryptKey>,
        ),
        Error,
    > {
        let raw_remote = self.remote_store(remote_name)?;
        let remote_key = raw_remote.encrypt_key.clone();
        let remote: Box<dyn crate::store::ObjectStore> = match io_record {
            Some(rec) => Box::new(crate::store::stats::StatsStore::new(
                Box::new(raw_remote.clone()),
                std::sync::Arc::clone(rec),
            )),
            None => Box::new(raw_remote.clone()),
        };
        let reader = crate::codec::pack::reader::PackReader::new(
            remote,
            self.local_store(),
            self.packcache_store(),
            self.objcache_store(),
            self.remote_root_pointer(remote_name)?,
            remote_key.clone(),
        );
        Ok((reader, raw_remote, remote_key))
    }

    /// Build a fully-wired `PackWriter` for `remote_name`, returning it
    /// alongside the remote's encryption key. Always wraps the remote in a
    /// `StatsStore` -- every push path that uses this factory tracks I/O
    /// stats.
    ///
    /// Consolidates the 4-argument `PackWriter::new` call plus its preceding
    /// wiring, copy-pasted at push's 5 remote-writing paths (refactor-
    /// instructions.md E1). `push_to_backup` is NOT covered (no stats
    /// tracking, a separate "backup" remote, warn-and-return error handling
    /// instead of `?`) and stays a direct `PackWriter::new` call.
    pub fn pack_writer(
        &self,
        remote_name: &str,
        io_record: &std::sync::Arc<crate::store::stats::IoRecord>,
    ) -> Result<(crate::codec::pack::writer::PackWriter, Option<EncryptKey>), Error> {
        let raw_remote = self.remote_store(remote_name)?;
        let remote_key = raw_remote.encrypt_key.clone();
        let stats_remote = crate::store::stats::StatsStore::new(
            Box::new(raw_remote),
            std::sync::Arc::clone(io_record),
        );
        let writer = crate::codec::pack::writer::PackWriter::new(
            Box::new(stats_remote),
            self.remote_root_pointer(remote_name)?,
            self.objcache_store(),
            remote_key.clone(),
        )?;
        Ok((writer, remote_key))
    }

    pub fn read_config(&self) -> Result<Config, Error> {
        let path = self.omemfs_dir().join(CONFIG_FILE);
        let s = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&s)?)
    }

    /// Write config back to disk atomically (preserves 0600 permissions).
    pub fn write_config(&self, config: &Config) -> Result<(), Error> {
        let config_path = self.omemfs_dir().join(CONFIG_FILE);
        let config_json = serde_json::to_string_pretty(config).map_err(Error::Json)?;
        atomic_write(&config_path, config_json.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// Build the `ObjectStore` (as a `LocalStore`) for a `RemoteConfig`.
///
/// This is the single mapping point from a remote's config to its object store,
/// independent of any `Repo` so it can be used before a repository exists (e.g.
/// the clone-time remote validation in `commands::clone`). For local remotes it
/// returns a directory-backed store; each cloud arm builds the per-backend
/// `CloudObjectIo`, wraps it in a `CloudObjects` adapter (carrying the prefix),
/// and returns a `LocalStore` over the cloud variant — so the storage-key HMAC
/// and the encryption layering stay in one place (`LocalStore`).
pub(crate) fn store_for_config(remote: &RemoteConfig) -> Result<LocalStore, Error> {
    let key = remote
        .encryption()
        .map(|enc| enc.decode_key())
        .transpose()?;
    match remote {
        RemoteConfig::Local { path, .. } => {
            if let Some(k) = key {
                Ok(LocalStore::for_remote_encrypted(path, k))
            } else {
                Ok(LocalStore::for_remote(path))
            }
        }
        RemoteConfig::S3 { .. } | RemoteConfig::Azure { .. } | RemoteConfig::Gcs { .. } => {
            let (io, prefix) = build_cloud_io(remote)?;
            let objects = crate::store::cloud::CloudObjects::new(io, prefix);
            Ok(LocalStore::for_cloud(objects, key))
        }
    }
}

/// Build the backend-pluggable index-root pointer for a `RemoteConfig`.
///
/// This is the single mapping point from a remote's config to its `RootPointer`
/// implementation, independent of any `Repo` (see [`store_for_config`]). Each
/// cloud arm builds its backend and returns the matching cloud `RootPointer`
/// over the index-root key (derived directly via `cloud::index_root_cloud_key`
/// — NOT through the storage-key HMAC).
pub(crate) fn root_pointer_for_config(
    remote: &RemoteConfig,
) -> Result<Box<dyn crate::codec::pack::root_pointer::RootPointer>, Error> {
    let key = remote
        .encryption()
        .map(|enc| enc.decode_key())
        .transpose()?;
    match remote {
        RemoteConfig::Local { path, .. } => Ok(Box::new(
            crate::codec::pack::root_pointer::LocalRootPointer::new(PathBuf::from(path), key),
        )),
        RemoteConfig::S3 {
            bucket,
            region,
            prefix,
            access_key_id,
            secret_access_key,
            endpoint,
            force_path_style,
            ..
        } => {
            let backend = crate::store::cloud::s3::S3Backend::new(
                bucket.clone(),
                region.clone(),
                access_key_id.clone(),
                secret_access_key.clone(),
                endpoint.clone(),
                *force_path_style,
                cloud_runtime()?,
            )?;
            Ok(Box::new(crate::store::cloud::CloudRootPointer::new(
                backend,
                prefix,
                key.as_ref(),
            )))
        }
        RemoteConfig::Azure {
            account,
            container,
            prefix,
            tenant_id,
            client_id,
            client_secret,
            endpoint,
            ..
        } => {
            let backend = crate::store::cloud::azure::AzureBackend::new(
                account.clone(),
                container.clone(),
                tenant_id.clone(),
                client_id.clone(),
                client_secret.clone(),
                endpoint.clone(),
                cloud_runtime()?,
            )?;
            Ok(Box::new(crate::store::cloud::CloudRootPointer::new(
                backend,
                prefix,
                key.as_ref(),
            )))
        }
        RemoteConfig::Gcs {
            bucket,
            prefix,
            credentials_json,
            credentials_json_path,
            endpoint,
            ..
        } => {
            let backend = crate::store::cloud::gcs::GcsBackend::new(
                bucket.clone(),
                credentials_json.clone(),
                credentials_json_path.clone(),
                endpoint.clone(),
                cloud_runtime()?,
            )?;
            Ok(Box::new(crate::store::cloud::CloudRootPointer::new(
                backend,
                prefix,
                key.as_ref(),
            )))
        }
    }
}

/// Process-wide shared cloud runtime, built once on first use.
///
/// `store_for_config` and `root_pointer_for_config` both call [`cloud_runtime`]
/// to obtain the runtime they hand to a backend constructor; a single command
/// that needs both a remote's object store and its root pointer (e.g. `push`,
/// `pull`, via `Repo::pack_reader` / `Repo::pack_writer`) previously built two
/// independent tokio runtimes (design/13 "One runtime per process, not one
/// per call").
static CLOUD_RUNTIME: std::sync::OnceLock<std::sync::Arc<crate::store::cloud::CloudRuntime>> =
    std::sync::OnceLock::new();

/// Return the shared process-wide cloud runtime, building it on first call.
fn cloud_runtime() -> Result<std::sync::Arc<crate::store::cloud::CloudRuntime>, Error> {
    if let Some(rt) = CLOUD_RUNTIME.get() {
        return Ok(std::sync::Arc::clone(rt));
    }
    let rt = std::sync::Arc::new(crate::store::cloud::CloudRuntime::new()?);
    // `OnceLock::get_or_init` cannot propagate `CloudRuntime::new`'s
    // `Result`, so build the runtime above and race `set` instead: if another
    // thread won the race, drop this one and use the winner's (dropping an
    // unused tokio runtime is safe -- it never had a task on it, and Phase 9
    // (G2) does not change the type's `Drop` impl).
    let _ = CLOUD_RUNTIME.set(rt);
    Ok(std::sync::Arc::clone(
        CLOUD_RUNTIME.get().expect("just set above"),
    ))
}

/// Build the per-backend `CloudObjectIo` for a cloud `RemoteConfig`, plus the
/// configured prefix. The returned IO is shared (`Arc`) so the `CloudObjects`
/// adapter can clone it cheaply.
fn build_cloud_io(
    remote: &RemoteConfig,
) -> Result<
    (
        std::sync::Arc<dyn crate::store::cloud::CloudObjectIo>,
        String,
    ),
    Error,
> {
    use crate::store::cloud::{azure::AzureBackend, gcs::GcsBackend, s3::S3Backend};
    let rt = cloud_runtime()?;
    match remote {
        RemoteConfig::S3 {
            bucket,
            region,
            prefix,
            access_key_id,
            secret_access_key,
            endpoint,
            force_path_style,
            ..
        } => {
            let backend = S3Backend::new(
                bucket.clone(),
                region.clone(),
                access_key_id.clone(),
                secret_access_key.clone(),
                endpoint.clone(),
                *force_path_style,
                rt,
            )?;
            Ok((std::sync::Arc::new(backend), prefix.clone()))
        }
        RemoteConfig::Azure {
            account,
            container,
            prefix,
            tenant_id,
            client_id,
            client_secret,
            endpoint,
            ..
        } => {
            let backend = AzureBackend::new(
                account.clone(),
                container.clone(),
                tenant_id.clone(),
                client_id.clone(),
                client_secret.clone(),
                endpoint.clone(),
                rt,
            )?;
            Ok((std::sync::Arc::new(backend), prefix.clone()))
        }
        RemoteConfig::Gcs {
            bucket,
            prefix,
            credentials_json,
            credentials_json_path,
            endpoint,
            ..
        } => {
            let backend = GcsBackend::new(
                bucket.clone(),
                credentials_json.clone(),
                credentials_json_path.clone(),
                endpoint.clone(),
                rt,
            )?;
            Ok((std::sync::Arc::new(backend), prefix.clone()))
        }
        RemoteConfig::Local { .. } => Err(Error::Other(
            "build_cloud_io called on a local remote".to_string(),
        )),
    }
}

/// Lexically resolve `.` and `..` components of `path` without touching the
/// filesystem. A leading `..` that would escape the path's root is dropped (it
/// cannot go above the root). Used to fold a cwd-joined argument before it is
/// stripped against the repository root, so `cd sub; omemfs ls ../other` maps to
/// the correct repo-relative path even when `other` does not yet exist on disk.
fn lexically_normalize(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a normal segment if present; keep prefixes/root anchors.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Convert a `<path>` argument to a repository-root-relative, forward-slash
/// string. Relative arguments are resolved against `cwd`; absolute arguments are
/// used as-is. The result is then re-expressed relative to `repo_root`, so all
/// downstream scope matching (which compares against root-anchored tree-entry
/// paths) works identically regardless of which subdirectory the command ran in.
///
/// A path that resolves outside `repo_root` is rejected. When `cwd == repo_root`
/// the result is identical to the historical "relative to work_dir" behaviour.
pub fn normalize_path(
    path: &std::path::Path,
    repo_root: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<String, Error> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let abs = lexically_normalize(&abs);
    let rel = abs.strip_prefix(repo_root).map_err(|_| {
        Error::Other(format!(
            "path '{}' is outside the repository '{}'",
            path.display(),
            repo_root.display()
        ))
    })?;
    // Normalise separators and strip any trailing slash. Shell tab-completion
    // appends a trailing slash to directory arguments (e.g. `expand foo/`), but
    // tree-entry relative paths never carry one. Without this, scope matching
    // that compares for equality (`rel == scope`) silently fails to match a
    // directory passed with a trailing slash, turning the command into a no-op.
    // A path that is only slashes normalises to the empty string.
    let normalised = rel.to_string_lossy().replace('\\', "/");
    Ok(normalised.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn normalize_path_strips_trailing_slash() {
        let work = std::path::Path::new("/work");
        // cwd == repo root: behaviour identical to the historical
        // "relative to work_dir" semantics.
        // A trailing slash (as shell tab-completion appends) is stripped so the
        // result matches a tree-entry relative path, which never carries one.
        assert_eq!(
            normalize_path(std::path::Path::new("foo/"), work, work).unwrap(),
            "foo"
        );
        assert_eq!(
            normalize_path(std::path::Path::new("foo/bar/"), work, work).unwrap(),
            "foo/bar"
        );
        // No trailing slash is unchanged.
        assert_eq!(
            normalize_path(std::path::Path::new("foo/bar"), work, work).unwrap(),
            "foo/bar"
        );
        // An absolute path inside the repo root is made relative and stripped.
        assert_eq!(
            normalize_path(std::path::Path::new("/work/foo/"), work, work).unwrap(),
            "foo"
        );
        // A path that is only slashes normalises to the empty string.
        assert_eq!(
            normalize_path(std::path::Path::new("/work/"), work, work).unwrap(),
            ""
        );
    }

    #[test]
    fn normalize_path_resolves_relative_against_cwd() {
        let root = std::path::Path::new("/work");
        let cwd = std::path::Path::new("/work/sub");
        // A bare relative arg from a subdirectory becomes root-anchored.
        assert_eq!(
            normalize_path(std::path::Path::new("foo"), root, cwd).unwrap(),
            "sub/foo"
        );
        // `.` from a subdirectory resolves to that subdirectory.
        assert_eq!(
            normalize_path(std::path::Path::new("."), root, cwd).unwrap(),
            "sub"
        );
        // `..` climbs out of the subdirectory.
        assert_eq!(
            normalize_path(std::path::Path::new("../other"), root, cwd).unwrap(),
            "other"
        );
        assert_eq!(
            normalize_path(std::path::Path::new(".."), root, cwd).unwrap(),
            ""
        );
        // An absolute arg is used as-is, independent of cwd.
        assert_eq!(
            normalize_path(std::path::Path::new("/work/abs/x"), root, cwd).unwrap(),
            "abs/x"
        );
    }

    #[test]
    fn normalize_path_rejects_outside_repo() {
        let root = std::path::Path::new("/work");
        let cwd = std::path::Path::new("/work/sub");
        // Climbing above the repository root is rejected.
        assert!(normalize_path(std::path::Path::new("../../escape"), root, cwd).is_err());
        // An absolute path outside the repo is rejected.
        assert!(normalize_path(std::path::Path::new("/elsewhere"), root, cwd).is_err());
    }

    #[test]
    fn discover_walks_up_to_repo_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let remote = RemoteConfig::Local {
            path: "/tmp/remote".to_string(),
            encryption: None,
        };
        Repo::init(&root, Config::new("origin", remote)).unwrap();

        // A nested subdirectory discovers the repo root above it.
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let repo = Repo::discover(&nested).unwrap();
        assert_eq!(repo.work_dir, root);

        // Discovery from the root itself also works.
        let repo_at_root = Repo::discover(&root).unwrap();
        assert_eq!(repo_at_root.work_dir, root);
    }

    #[test]
    fn discover_errors_when_no_repo() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        // No .omemfs/ anywhere up to the filesystem root → error.
        assert!(Repo::discover(&dir).is_err());
    }

    #[test]
    fn init_writes_parseable_config() {
        let tmp = TempDir::new().unwrap();
        let remote = RemoteConfig::Local {
            path: "/tmp/remote".to_string(),
            encryption: None,
        };
        let config = Config::new("origin", remote);
        let repo = Repo::init(tmp.path(), config).unwrap();

        // Config file exists and parses back into a Config.
        let parsed = repo.read_config().unwrap();
        assert_eq!(parsed.version, "2.0");
        assert!(parsed.remotes.contains_key("origin"));

        // 0600 permissions on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(tmp.path().join(".omemfs/config"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn cloud_runtime_is_a_process_wide_singleton() {
        // refactor-instructions.md Phase 9 (G2): a command that needs both a
        // remote's object store and its root pointer must not build two
        // independent tokio runtimes. `cloud_runtime()` returns the exact
        // same `Arc` (pointer-equal, not just equal-by-value) on every call.
        let a = cloud_runtime().unwrap();
        let b = cloud_runtime().unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
    }
}
