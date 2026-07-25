use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atty;

use filetime::FileTime;

use crate::codec;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Hash, Tree, TreeEntry};
use crate::repo::{Config, EncryptionConfig, RemoteConfig, Repo};
use crate::store::ObjectStore;
use crate::store::stats::IoRecord;
use crate::stub::{self, StubRecord};
use crate::term::Output;

/// Default AWS region applied when an S3 remote's region is left blank,
/// either via `s3://` URL shorthand (which carries no region) or an empty
/// answer to the interactive/non-interactive region prompt. Previously
/// duplicated as four independent literals (refactor-instructions.md D4).
const DEFAULT_S3_REGION: &str = "us-east-1";

pub struct CloneOptions {
    /// If set, skip interactive prompts and use this URL directly (local path or s3://).
    pub url: Option<String>,
    /// Generate a DEK and enable encryption (only valid for a new repository).
    pub encrypt: bool,
    /// `--new`: declare the remote is a new (empty) repository.
    pub new: bool,
    /// `--existing`: declare the remote is an existing repository.
    pub existing: bool,
    pub directory: PathBuf,
    pub stub_threshold: u64,
    pub force: bool,
}

/// Declared intent for a remote: is it new (to be created) or existing?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteIntent {
    New,
    Existing,
}

pub fn run(opts: CloneOptions) -> Result<(), Error> {
    if opts.directory.exists() {
        let is_empty = opts
            .directory
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !is_empty && !opts.force {
            return Err(Error::Other(format!(
                "directory '{}' is not empty; use --force to clone into a non-empty directory",
                opts.directory.display()
            )));
        }
    }

    // Flag-level validation common to clone and add-backup.
    validate_intent_flags(opts.new, opts.existing, opts.encrypt)?;

    let config = if let Some(ref url) = opts.url {
        // If the URL is a connection string, parse it directly (includes DEK).
        if url.starts_with("omemfs_repo_") {
            if opts.encrypt {
                return Err(Error::Other(
                    "--encrypt cannot be used with a connection string".to_string(),
                ));
            }
            parse_connection_string(url)?
        } else {
            // Non-interactive mode: build config directly from URL.
            let intent = resolve_intent_noninteractive(opts.new, opts.existing, opts.encrypt)?;
            let mut remote_config = build_remote_config_from_url(url)?;
            if intent == RemoteIntent::New && opts.encrypt {
                println!("(Generating new DEK...)");
                let enc = EncryptionConfig::generate();
                remote_config = set_remote_encryption(remote_config, Some(enc));
            }
            validate_remote_against_intent(&remote_config, intent)?;
            Config::new("origin", remote_config)
        }
    } else {
        if !atty::is(atty::Stream::Stdin) {
            return Err(Error::Other(
                "interactive setup required; run omemfs clone in a terminal".to_string(),
            ));
        }
        prompt_config(opts.new, opts.existing, opts.encrypt)?
    };
    println!();

    let origin_name = "origin";
    let origin_remote = config
        .remotes
        .get(origin_name)
        .ok_or_else(|| Error::Other("origin remote not configured".to_string()))?;

    println!("Cloning from {} ...", remote_display_url(origin_remote));

    fs::create_dir_all(&opts.directory)?;
    let repo = Repo::init(&opts.directory, config)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let _t = dtimer_l1!("clone");
    let local = repo.local_store();

    // Measured from here (not from `run`'s entry) so interactive prompt wait
    // time (URL entry, DEK confirmation) does not pollute the duration used
    // for pack-scheduling analysis -- this marks where remote I/O actually
    // starts.
    let started = std::time::Instant::now();
    let io_record = Arc::new(IoRecord::default());
    let (pack_reader, _remote, remote_key) = repo.pack_reader(origin_name, Some(&io_record))?;

    let result: Result<(), Error> = (|| {
        match pack_reader.read_root()? {
            None => {
                println!("Remote is empty. Initialised empty repository.");
                // Write default .omemfs-filter template for a brand-new repository.
                write_default_filter(&opts.directory)?;
            }
            Some(root_hash) => {
                println!("Remote root: {}", &root_hash.as_str()[..8]);

                // Lazy, stub-aware clone: do NOT download the whole repository.
                // expand_or_stub walks the root tree through the PackReader,
                // fetching only the tree objects and blobs needed to materialise
                // sub-threshold entries. Entries at or above the threshold are
                // stubbed from their parent tree-entry metadata alone, downloading
                // nothing. See design/04 "Behaviour" and design/08.
                let mut expanded = 0usize;
                let mut stubbed = 0usize;
                let mut existing_skipped = 0usize;
                {
                    let phase = crate::progress::begin_phase("Expand files");
                    expand_or_stub(
                        &root_hash,
                        &opts.directory,
                        &pack_reader,
                        &local,
                        remote_key.as_ref(),
                        &opts.directory,
                        "",
                        opts.stub_threshold,
                        false,
                        opts.force,
                        &mut expanded,
                        &mut stubbed,
                        &mut existing_skipped,
                    )?;
                    phase.complete(format!("{} files", expanded));
                }

                repo.write_clone_root(&root_hash)?;

                // Write the default .omemfs-filter template only if the remote did
                // not already contain one (i.e. it was not expanded in the step above).
                write_default_filter(&opts.directory)?;

                // The "Expand files" phase has completed but its row stays on
                // screen, so these summary lines must be buffered through
                // `Output` rather than printed directly — a direct write would
                // race with the periodic redraw and could be erased. They are
                // deposited and flushed below the phase list at command exit.
                let mut out = Output::for_stdout();
                if opts.stub_threshold > 0 && stubbed > 0 {
                    out.writeln(&format!(
                        "{} file(s) expanded, {} file(s) stubbed (>= {} bytes).\nCloned into {}",
                        expanded,
                        stubbed,
                        opts.stub_threshold,
                        opts.directory.display()
                    ))?;
                } else {
                    out.writeln(&format!(
                        "{} file(s) expanded.\nCloned into {}",
                        expanded,
                        opts.directory.display()
                    ))?;
                }
                if existing_skipped > 0 {
                    out.writeln(&format!("{} existing path(s) skipped.", existing_skipped))?;
                }
                out.finish()?;
            }
        }
        Ok(())
    })();

    if result.is_ok() {
        let omemfs_dir = opts.directory.join(".omemfs");
        let duration_ms = started.elapsed().as_millis() as u64;
        io_stats::append_record(&omemfs_dir, "clone", origin_name, &io_record, duration_ms);
    }
    result
}

/// Write the default `.omemfs-filter` template to the repository root if the
/// file does not already exist there (either from a remote or from a prior run).
fn write_default_filter(dir: &Path) -> Result<(), Error> {
    let filter_path = dir.join(".omemfs-filter");
    if !filter_path.exists() {
        fs::write(&filter_path, crate::filter::DEFAULT_FILTER_TEMPLATE)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Non-interactive URL parsing
// ---------------------------------------------------------------------------

fn expand_tilde(url: &str) -> String {
    if url == "~" {
        std::env::var("HOME").unwrap_or_else(|_| url.to_string())
    } else if let Some(rest) = url.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
        format!("{}/{}", home, rest)
    } else {
        url.to_string()
    }
}

fn build_remote_config_from_url(url: &str) -> Result<RemoteConfig, Error> {
    let url = &expand_tilde(url);
    if url.starts_with('/') || url.starts_with('.') || url.starts_with("local://") {
        let path = url.strip_prefix("local://").unwrap_or(url).to_string();
        Ok(RemoteConfig::Local {
            path,
            encryption: None,
        })
    } else if url.starts_with("s3://") {
        let rest = url.strip_prefix("s3://").unwrap_or(url);
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        Ok(RemoteConfig::S3 {
            bucket: bucket.to_string(),
            region: DEFAULT_S3_REGION.to_string(),
            prefix: prefix.to_string(),
            access_key_id: None,
            secret_access_key: None,
            endpoint: None,
            force_path_style: None,
            encryption: None,
        })
    } else if url.starts_with("gs://") {
        // gs://<bucket>/<prefix>
        let rest = url.strip_prefix("gs://").unwrap_or(url);
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        Ok(RemoteConfig::Gcs {
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            project_id: None,
            credentials_json_path: None,
            credentials_json: None,
            endpoint: None,
            encryption: None,
        })
    } else if url.starts_with("azure://") {
        // azure://<account>/<container>/<prefix...>
        let rest = url.strip_prefix("azure://").unwrap_or(url);
        let mut parts = rest.splitn(3, '/');
        let account = parts.next().unwrap_or("").to_string();
        let container = parts.next().unwrap_or("").to_string();
        let prefix = parts.next().unwrap_or("").to_string();
        if account.is_empty() || container.is_empty() {
            return Err(Error::Other(format!(
                "azure:// URL must be azure://<account>/<container>[/<prefix>]: {}",
                url
            )));
        }
        Ok(RemoteConfig::Azure {
            account,
            container,
            prefix,
            tenant_id: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            endpoint: None,
            encryption: None,
        })
    } else {
        Err(Error::Other(format!("unsupported remote URL: {}", url)))
    }
}

// ---------------------------------------------------------------------------
// Remote state helpers
// ---------------------------------------------------------------------------

/// Flag-level validation shared by clone and add-backup. Catches mutually
/// exclusive / incompatible flag combinations before any remote contact.
pub fn validate_intent_flags(new: bool, existing: bool, encrypt: bool) -> Result<(), Error> {
    if new && existing {
        return Err(Error::Other(
            "--new and --existing are mutually exclusive".to_string(),
        ));
    }
    if encrypt && existing {
        return Err(Error::Other(
            "--encrypt is only valid for a new repository; \
             use interactive mode to provide the existing DEK"
                .to_string(),
        ));
    }
    Ok(())
}

/// Resolve new/existing intent in non-interactive (flag-driven) mode, following
/// the priority order in design/04 (connection strings are handled by the
/// caller before this is reached):
///   --new            → New
///   --existing       → Existing
///   --encrypt alone  → New
///   none of the above → hard error (no TTY to prompt)
pub fn resolve_intent_noninteractive(
    new: bool,
    existing: bool,
    encrypt: bool,
) -> Result<RemoteIntent, Error> {
    if new {
        Ok(RemoteIntent::New)
    } else if existing {
        Ok(RemoteIntent::Existing)
    } else if encrypt {
        Ok(RemoteIntent::New)
    } else {
        Err(Error::Other(
            "cannot determine remote intent in non-interactive mode\n\
             Pass --new or --existing to specify whether this is a new or existing repository."
                .to_string(),
        ))
    }
}

/// Validate the remote backend against the declared intent. Performed before any
/// local state is written. See design/04 "Validation".
fn validate_remote_against_intent(
    config: &RemoteConfig,
    intent: RemoteIntent,
) -> Result<(), Error> {
    let encrypted = config.encryption().is_some();
    // Map the config to its backend-pluggable root pointer and object store
    // (Local / S3 / Azure / GCS), so the new/existing checks key on the same
    // presence probes every backend uses — no direct filesystem access. This
    // runs before the repository exists, so it goes through the Repo-independent
    // mapping in `repo`, not `Repo::remote_root_pointer`.
    let root_pointer = crate::repo::root_pointer_for_config(config)?;
    let store = crate::repo::store_for_config(config)?;
    validate_remote_state(root_pointer.as_ref(), &store, encrypted, intent)
}

/// Validate the probed remote state (index-root presence + object emptiness)
/// against the declared intent. Backend-agnostic: takes the already-built root
/// pointer and store so it can be exercised over any backend (incl. in-memory
/// fakes) without a live endpoint. See design/04 "Validation".
fn validate_remote_state(
    root_pointer: &dyn crate::codec::pack::root_pointer::RootPointer,
    store: &crate::store::local::LocalStore,
    encrypted: bool,
    intent: RemoteIntent,
) -> Result<(), Error> {
    let index_root_present = root_pointer.read()?.0.is_some();
    match intent {
        RemoteIntent::New => {
            // New requires a completely empty prefix: no index root AND no stored
            // objects. `iter_hashes` lists object content for both local and
            // cloud backends (an encrypted index root is stored under a derived
            // sharded object key, so it is caught here too).
            let has_objects = !store.iter_hashes().is_empty();
            if index_root_present || has_objects {
                return Err(Error::Other(
                    "remote prefix is not empty\n\
                     Use --existing if this is an existing repository, or choose an empty prefix for a new one."
                        .to_string(),
                ));
            }
        }
        RemoteIntent::Existing => {
            if !index_root_present {
                if encrypted {
                    return Err(Error::Other(
                        "index root not found on remote\n\
                         The encryption key may be wrong, or the URL/prefix may point to a different repository."
                            .to_string(),
                    ));
                } else {
                    return Err(Error::Other(
                        "INDEX_ROOT not found on remote\n\
                         Check the URL/prefix. If this is a new repository, use --new instead."
                            .to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Replace the encryption field on any RemoteConfig variant.
fn set_remote_encryption(config: RemoteConfig, enc: Option<EncryptionConfig>) -> RemoteConfig {
    match config {
        RemoteConfig::Local { path, .. } => RemoteConfig::Local {
            path,
            encryption: enc,
        },
        RemoteConfig::S3 {
            bucket,
            region,
            prefix,
            access_key_id,
            secret_access_key,
            endpoint,
            force_path_style,
            ..
        } => RemoteConfig::S3 {
            bucket,
            region,
            prefix,
            access_key_id,
            secret_access_key,
            endpoint,
            force_path_style,
            encryption: enc,
        },
        RemoteConfig::Azure {
            account,
            container,
            prefix,
            tenant_id,
            client_id,
            client_secret,
            endpoint,
            ..
        } => RemoteConfig::Azure {
            account,
            container,
            prefix,
            tenant_id,
            client_id,
            client_secret,
            endpoint,
            encryption: enc,
        },
        RemoteConfig::Gcs {
            bucket,
            prefix,
            project_id,
            credentials_json_path,
            credentials_json,
            endpoint,
            ..
        } => RemoteConfig::Gcs {
            bucket,
            prefix,
            project_id,
            credentials_json_path,
            credentials_json,
            endpoint,
            encryption: enc,
        },
    }
}

// ---------------------------------------------------------------------------
// Interactive config prompts
// ---------------------------------------------------------------------------

/// Interactively collect the full repository config (origin + optional backup).
/// Accepts a plain URL or an `omemfs_repo_` connection string at the first prompt.
///
/// `new`/`existing`/`encrypt` are the command-line flags; when present they
/// pre-resolve the new/existing declaration and skip the corresponding prompt
/// (design/04 "New/existing declaration").
fn prompt_config(new: bool, existing: bool, encrypt: bool) -> Result<Config, Error> {
    println!("Hint: if you have already cloned this repository on another machine, run");
    println!("  omemfs config export");
    println!("to get an omemfs_repo_... connection string that fills in all parameters at once.");
    println!();

    let input =
        prompt_visible("Remote URL or connection string (leave blank to choose a remote type): ")?;

    // An empty line opens the guided remote-type menu (design/04 "Guided menu").
    // A connection string fills in everything at once and implies existing.
    let origin_creds = if input.is_empty() {
        let mut prompter = StdinPrompt;
        collect_remote_from_menu(&mut prompter)?
    } else if input.starts_with("omemfs_repo_") {
        return parse_connection_string(&input);
    } else {
        // Step 1: Collect URL + credentials without prompting for encryption yet.
        prompt_remote_credentials(&input)?
    };

    // Step 2: Resolve intent + encryption, then validate the remote.
    let (origin, origin_intent) =
        configure_remote_interactively(origin_creds, new, existing, encrypt)?;
    validate_remote_against_intent(&origin, origin_intent)?;

    let mut config = Config::new("origin", origin);

    // Ask about backup remote. The backup uses its own interactive intent
    // resolution (the clone-level flags apply only to origin).
    let add_backup = prompt_visible("Add backup remote? [y/N]: ")?;
    if add_backup.trim().eq_ignore_ascii_case("y") {
        let backup_url = prompt_visible("Remote URL (leave blank to choose a remote type): ")?;
        let backup_creds = if backup_url.is_empty() {
            let mut prompter = StdinPrompt;
            collect_remote_from_menu(&mut prompter)?
        } else {
            prompt_remote_credentials(&backup_url)?
        };
        let (backup, backup_intent) =
            configure_remote_interactively(backup_creds, false, false, false)?;
        validate_remote_against_intent(&backup, backup_intent)?;
        config.remotes.insert("backup".to_string(), backup);
    }

    Ok(config)
}

/// Prompt for credentials (URL type, region, access keys) without asking about
/// encryption. Returns a RemoteConfig with encryption: None.
fn prompt_remote_credentials(url: &str) -> Result<RemoteConfig, Error> {
    match detect_remote_type(url)?.as_str() {
        "local" => {
            let expanded = expand_tilde(url);
            let path = expanded
                .strip_prefix("local://")
                .unwrap_or(&expanded)
                .to_string();
            Ok(RemoteConfig::Local {
                path,
                encryption: None,
            })
        }
        "s3" => {
            let rest = url.strip_prefix("s3://").unwrap_or(url);
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let region = prompt_visible("Region: ")?;
            let access_key_id = prompt_visible("Access Key ID (leave blank to use environment): ")?;
            let secret_access_key = if access_key_id.is_empty() {
                String::new()
            } else {
                prompt_hidden("Secret Access Key: ")?
            };
            Ok(RemoteConfig::S3 {
                bucket: bucket.to_string(),
                region: if region.is_empty() {
                    DEFAULT_S3_REGION.to_string()
                } else {
                    region
                },
                prefix: prefix.to_string(),
                access_key_id: if access_key_id.is_empty() {
                    None
                } else {
                    Some(access_key_id)
                },
                secret_access_key: if secret_access_key.is_empty() {
                    None
                } else {
                    Some(secret_access_key)
                },
                endpoint: None,
                force_path_style: None,
                encryption: None,
            })
        }
        _ => Err(Error::Other(format!("unsupported remote type: {}", url))),
    }
}

// ---------------------------------------------------------------------------
// Guided remote-type menu
// ---------------------------------------------------------------------------

/// Input source for the guided menu. Abstracting the two line readers behind a
/// trait lets the field-by-field collection be unit-tested with scripted input
/// instead of the real terminal. `visible` echoes; `hidden` does not (secrets).
trait Prompter {
    fn visible(&mut self, prompt: &str) -> Result<String, Error>;
    fn hidden(&mut self, prompt: &str) -> Result<String, Error>;
    /// Read a menu choice. Returns `None` on end-of-input (EOF) so the menu
    /// loop can error instead of spinning forever on an exhausted stream.
    fn choice(&mut self, prompt: &str) -> Result<Option<String>, Error>;
}

/// The real terminal prompter used in production: delegates to the existing
/// `prompt_visible` / `prompt_hidden` helpers.
struct StdinPrompt;

impl Prompter for StdinPrompt {
    fn visible(&mut self, prompt: &str) -> Result<String, Error> {
        prompt_visible(prompt)
    }
    fn hidden(&mut self, prompt: &str) -> Result<String, Error> {
        prompt_hidden(prompt)
    }
    fn choice(&mut self, prompt: &str) -> Result<Option<String>, Error> {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(prompt.as_bytes())?;
        stdout.flush()?;
        let stdin = io::stdin();
        let mut line = String::new();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 {
            // EOF with no newline read.
            return Ok(None);
        }
        Ok(Some(line.trim().to_string()))
    }
}

/// Show the remote-type menu and collect the chosen backend's settings field by
/// field, returning a `RemoteConfig` with `encryption: None` (encryption is
/// resolved later by `configure_remote_interactively`, exactly as on the URL
/// path). See design/04 "Guided menu (empty first prompt)".
fn collect_remote_from_menu(p: &mut dyn Prompter) -> Result<RemoteConfig, Error> {
    println!("Select a remote type:");
    println!("  1) Local directory");
    println!("  2) Amazon S3 (or S3-compatible such as MinIO)");
    println!("  3) Google Cloud Storage");
    println!("  4) Azure Blob Storage");

    loop {
        match p.choice("Choice [1-4]: ")? {
            None => {
                // EOF: no terminal to answer the menu.
                return Err(Error::Other(
                    "no remote type selected (end of input)".to_string(),
                ));
            }
            Some(choice) => match choice.trim() {
                "1" => return collect_local_fields(p),
                "2" => return collect_s3_fields(p),
                "3" => return collect_gcs_fields(p),
                "4" => return collect_azure_fields(p),
                "" => println!("Please enter a number from 1 to 4."),
                other => println!(
                    "Invalid choice '{}': please enter a number from 1 to 4.",
                    other
                ),
            },
        }
    }
}

fn collect_local_fields(p: &mut dyn Prompter) -> Result<RemoteConfig, Error> {
    let path = p.visible("Path: ")?;
    if path.is_empty() {
        return Err(Error::Other(
            "path is required for a local remote".to_string(),
        ));
    }
    let expanded = expand_tilde(&path);
    let path = expanded
        .strip_prefix("local://")
        .unwrap_or(&expanded)
        .to_string();
    Ok(RemoteConfig::Local {
        path,
        encryption: None,
    })
}

fn collect_s3_fields(p: &mut dyn Prompter) -> Result<RemoteConfig, Error> {
    let bucket = p.visible("Bucket: ")?;
    if bucket.is_empty() {
        return Err(Error::Other(
            "bucket is required for an S3 remote".to_string(),
        ));
    }
    let prefix = p.visible("Prefix (optional): ")?;
    let region = p.visible(&format!("Region [{}]: ", DEFAULT_S3_REGION))?;
    let access_key_id =
        p.visible("Access Key ID (blank to use the AWS default credential chain): ")?;
    let secret_access_key = if access_key_id.is_empty() {
        String::new()
    } else {
        p.hidden("Secret Access Key: ")?
    };
    let endpoint = p.visible("S3-compatible endpoint URL (blank for AWS S3): ")?;
    // force_path_style is only meaningful for a custom endpoint; default yes
    // there (MinIO and most S3-compatible stores require it).
    let force_path_style = if endpoint.is_empty() {
        None
    } else {
        let answer = p.visible("Use path-style addressing? [Y/n]: ")?;
        Some(!answer.trim().eq_ignore_ascii_case("n"))
    };
    Ok(RemoteConfig::S3 {
        bucket,
        region: if region.is_empty() {
            DEFAULT_S3_REGION.to_string()
        } else {
            region
        },
        prefix,
        access_key_id: if access_key_id.is_empty() {
            None
        } else {
            Some(access_key_id)
        },
        secret_access_key: if secret_access_key.is_empty() {
            None
        } else {
            Some(secret_access_key)
        },
        endpoint: if endpoint.is_empty() {
            None
        } else {
            Some(endpoint)
        },
        force_path_style,
        encryption: None,
    })
}

fn collect_gcs_fields(p: &mut dyn Prompter) -> Result<RemoteConfig, Error> {
    let bucket = p.visible("Bucket: ")?;
    if bucket.is_empty() {
        return Err(Error::Other(
            "bucket is required for a GCS remote".to_string(),
        ));
    }
    let prefix = p.visible("Prefix (optional): ")?;
    let key_path = p.visible("Service-account JSON key path (blank to use ADC): ")?;
    let project_id = p.visible("Project ID (optional): ")?;
    let endpoint = p.visible("Endpoint (optional): ")?;
    Ok(RemoteConfig::Gcs {
        bucket,
        prefix,
        project_id: if project_id.is_empty() {
            None
        } else {
            Some(project_id)
        },
        credentials_json_path: if key_path.is_empty() {
            None
        } else {
            Some(key_path)
        },
        credentials_json: None,
        endpoint: if endpoint.is_empty() {
            None
        } else {
            Some(endpoint)
        },
        encryption: None,
    })
}

fn collect_azure_fields(p: &mut dyn Prompter) -> Result<RemoteConfig, Error> {
    let account = p.visible("Account: ")?;
    if account.is_empty() {
        return Err(Error::Other(
            "account is required for an Azure remote".to_string(),
        ));
    }
    let container = p.visible("Container: ")?;
    if container.is_empty() {
        return Err(Error::Other(
            "container is required for an Azure remote".to_string(),
        ));
    }
    let prefix = p.visible("Prefix (optional): ")?;
    let tenant_id = p.visible("Tenant ID: ")?;
    let client_id = p.visible("Client ID: ")?;
    let client_secret = p.hidden("Client Secret: ")?;
    let endpoint = p.visible("Endpoint (optional): ")?;
    Ok(RemoteConfig::Azure {
        account,
        container,
        prefix,
        tenant_id,
        client_id,
        client_secret,
        endpoint: if endpoint.is_empty() {
            None
        } else {
            Some(endpoint)
        },
        encryption: None,
    })
}

/// Resolve the new/existing declaration (honouring flags, else prompting on a
/// TTY) and configure encryption accordingly, returning the configured remote
/// and the resolved intent:
///   - New repository: ask "Enable encryption?" (skipped if `--encrypt`) and
///     optionally generate a DEK.
///   - Existing repository: ask "Is it encrypted?" and if yes, prompt for DEK.
///
/// See design/04 "New/existing declaration" and the `Enable encryption?` /
/// `Is it encrypted?` prompts.
fn configure_remote_interactively(
    config: RemoteConfig,
    new: bool,
    existing: bool,
    encrypt: bool,
) -> Result<(RemoteConfig, RemoteIntent), Error> {
    let intent = if new {
        RemoteIntent::New
    } else if existing {
        RemoteIntent::Existing
    } else if encrypt {
        // --encrypt alone implies new.
        RemoteIntent::New
    } else {
        let answer =
            prompt_visible("Is this a new (empty) remote or an existing one? [new/existing]: ")?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "new" => RemoteIntent::New,
            "existing" => RemoteIntent::Existing,
            other => {
                return Err(Error::Other(format!(
                    "invalid answer '{}': expected 'new' or 'existing'",
                    other
                )));
            }
        }
    };

    let enc = match intent {
        RemoteIntent::New => {
            if encrypt {
                println!("(Generating new DEK...)");
                Some(EncryptionConfig::generate())
            } else {
                let answer = prompt_visible("Enable encryption? [Y/n]: ")?;
                if answer.trim().eq_ignore_ascii_case("n") {
                    None
                } else {
                    println!("(Generating new DEK...)");
                    Some(EncryptionConfig::generate())
                }
            }
        }
        RemoteIntent::Existing => {
            let answer = prompt_visible("Is it encrypted? [y/N]: ")?;
            if answer.trim().eq_ignore_ascii_case("y") {
                Some(prompt_dek()?)
            } else {
                None
            }
        }
    };
    Ok((set_remote_encryption(config, enc), intent))
}

/// Prompt for a base64-encoded DEK without echo. Validates that the value
/// decodes to exactly 32 bytes.
fn prompt_dek() -> Result<EncryptionConfig, Error> {
    let dek_b64 = prompt_hidden("DEK (base64): ")?;
    let enc = EncryptionConfig {
        algorithm: "aes-256-gcm".to_string(),
        dek: dek_b64,
    };
    enc.decode_key()?; // validates base64 encoding and 32-byte length
    Ok(enc)
}

fn detect_remote_type(url: &str) -> Result<String, Error> {
    let url = expand_tilde(url);
    if url.starts_with('/') || url.starts_with('.') || url.starts_with("local://") {
        Ok("local".to_string())
    } else if url.starts_with("s3://") {
        Ok("s3".to_string())
    } else if url.starts_with("gs://") {
        Ok("gcs".to_string())
    } else if url.starts_with("azure://") {
        Ok("azure".to_string())
    } else {
        Err(Error::Other(format!("unsupported remote URL: {}", url)))
    }
}

fn remote_display_url(remote: &RemoteConfig) -> String {
    match remote {
        RemoteConfig::Local { path, .. } => path.clone(),
        RemoteConfig::S3 { bucket, prefix, .. } => format!("s3://{}/{}", bucket, prefix),
        RemoteConfig::Azure {
            account,
            container,
            prefix,
            ..
        } => {
            format!(
                "https://{}.blob.core.windows.net/{}/{}",
                account, container, prefix
            )
        }
        RemoteConfig::Gcs { bucket, prefix, .. } => format!("gs://{}/{}", bucket, prefix),
    }
}

// ---------------------------------------------------------------------------
// Connection string (omemfs_repo_...) encode / decode
// ---------------------------------------------------------------------------

/// Encode the full config as an `omemfs_repo_` connection string.
pub fn encode_connection_string(config: &Config) -> Result<String, Error> {
    let json = serde_json::to_string(config).map_err(Error::Json)?;
    let encoded = crate::base32::encode(json.as_bytes());
    Ok(format!("omemfs_repo_{}", encoded))
}

/// Decode an `omemfs_repo_` connection string back into a Config.
pub fn parse_connection_string(s: &str) -> Result<Config, Error> {
    let encoded = s.strip_prefix("omemfs_repo_").ok_or_else(|| {
        Error::Other("invalid connection string: missing omemfs_repo_ prefix".to_string())
    })?;
    let bytes = crate::base32::decode(encoded)
        .map_err(|e| Error::Other(format!("invalid connection string: {}", e)))?;
    let config: Config = serde_json::from_slice(&bytes).map_err(|e| {
        Error::Other(format!(
            "invalid connection string (JSON parse error): {}",
            e
        ))
    })?;

    if let Some(origin) = config.remotes.get("origin") {
        println!("Importing config from connection string...");
        println!("  origin: {}", remote_display_url(origin));
    }
    if let Some(backup) = config.remotes.get("backup") {
        println!("  backup: {}", remote_display_url(backup));
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// config add-backup
// ---------------------------------------------------------------------------

pub struct AddBackupOptions {
    pub work_dir: PathBuf,
    pub force: bool,
    pub url: Option<String>,
    pub new: bool,
    pub existing: bool,
    pub encrypt: bool,
}

/// `omemfs config add-backup`: add or replace the `backup` remote. Intent
/// resolution, validation, and error messages are identical to `clone`
/// (design/04). For add-backup, validation is applied at config time.
pub fn run_add_backup(opts: AddBackupOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    let mut config = repo.read_config()?;

    if config.remotes.contains_key("backup") && !opts.force {
        return Err(Error::Other(
            "backup remote already configured; use --force to overwrite".to_string(),
        ));
    }

    validate_intent_flags(opts.new, opts.existing, opts.encrypt)?;

    let backup_config = if let Some(ref url) = opts.url {
        // Non-interactive path.
        if url.starts_with("omemfs_repo_") {
            let imported = parse_connection_string(url)?;
            imported.remotes.get("backup").cloned().ok_or_else(|| {
                Error::Other("connection string has no 'backup' remote".to_string())
            })?
        } else {
            let intent = resolve_intent_noninteractive(opts.new, opts.existing, opts.encrypt)?;
            let mut remote_config = build_remote_config_from_url(url)?;
            if intent == RemoteIntent::New && opts.encrypt {
                println!("(Generating new DEK...)");
                remote_config =
                    set_remote_encryption(remote_config, Some(EncryptionConfig::generate()));
            }
            validate_remote_against_intent(&remote_config, intent)?;
            remote_config
        }
    } else {
        // Interactive path.
        if !atty::is(atty::Stream::Stdin) {
            return Err(Error::Other(
                "interactive setup required; run omemfs config add-backup in a terminal"
                    .to_string(),
            ));
        }
        let url = prompt_visible("Remote URL (leave blank to choose a remote type): ")?;
        if url.starts_with("omemfs_repo_") {
            let imported = parse_connection_string(&url)?;
            imported.remotes.get("backup").cloned().ok_or_else(|| {
                Error::Other("connection string has no 'backup' remote".to_string())
            })?
        } else {
            let creds = if url.is_empty() {
                let mut prompter = StdinPrompt;
                collect_remote_from_menu(&mut prompter)?
            } else {
                prompt_remote_credentials(&url)?
            };
            let (cfg, intent) =
                configure_remote_interactively(creds, opts.new, opts.existing, opts.encrypt)?;
            validate_remote_against_intent(&cfg, intent)?;
            cfg
        }
    };

    config.remotes.insert("backup".to_string(), backup_config);
    repo.write_config(&config)?;
    println!("Backup remote configured.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt helpers
// ---------------------------------------------------------------------------

fn prompt_visible(prompt: &str) -> Result<String, Error> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(prompt.as_bytes())?;
    stdout.flush()?;

    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_hidden(prompt: &str) -> Result<String, Error> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(prompt.as_bytes())?;
    stdout.flush()?;

    // When stdin is not a TTY (e.g. piped input in tests), fall back to
    // plain line reading so automated workflows can supply the DEK.
    if !atty::is(atty::Stream::Stdin) {
        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }

    let password = rpassword::read_password()
        .map_err(|e| Error::Other(format!("failed to read password: {}", e)))?;
    Ok(password.trim().to_string())
}

// ---------------------------------------------------------------------------
// Expand / stub
// ---------------------------------------------------------------------------

/// Fetch a SINGLE object from `src` (the PackReader, which resolves local cache →
/// remote) into the plaintext `local` cache if it is not already there, then return
/// its deserialised bytes. This is the lazy single-object fetch used to read tree
/// objects while walking: it does NOT recurse into the object's children, so reading
/// a tree object here does not pull its subtree (only the tree objects we actually
/// descend, and the blobs we actually materialise, are downloaded).
///
/// `transfer_objects` is deliberately NOT used here: it walks the whole object graph
/// (it always recurses into a freshly-fetched tree's children), which would defeat
/// the lazy download. Instead we read the one object's decoded (decrypted +
/// decompressed) bytes via `src` and re-encode them into `local` with no key, since
/// the local cache stores plaintext.
fn ensure_tree_in_local(
    src: &dyn ObjectStore,
    local: &dyn ObjectStore,
    hash: &Hash,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<Vec<u8>, Error> {
    if let Ok(data) = codec::store_read(local, hash, None) {
        return Ok(data);
    }
    let data = codec::store_read(src, hash, remote_key)?;
    codec::store_write(local, hash, &data, None)?;
    Ok(data)
}

#[allow(clippy::too_many_arguments)]
fn expand_or_stub(
    tree_hash: &Hash,
    dir: &Path,
    src: &dyn ObjectStore,
    local: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    work_dir: &Path,
    rel_prefix: &str,
    threshold: u64,
    in_git_worktree: bool,
    force: bool,
    expanded: &mut usize,
    stubbed: &mut usize,
    existing_skipped: &mut usize,
) -> Result<(), Error> {
    // Fetch this tree object on demand (single object, no subtree) and read it
    // from the plaintext local cache.
    let data = ensure_tree_in_local(src, local, tree_hash, remote_key)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;

    fs::create_dir_all(dir)?;

    // If this tree is being materialised and is itself a git worktree root (it
    // has a direct ".git" child), its own children must not be partial-stubbed.
    // The descend branch already propagates this for nested directories, but the
    // top-level call (the clone root) has no parent to detect it — handle it here.
    let in_git_worktree = in_git_worktree
        || entries
            .iter()
            .any(|e| matches!(e, TreeEntry::Tree { name, .. } if name == ".git"));

    for entry in entries {
        let rel_path = if rel_prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", rel_prefix, entry.name())
        };

        match entry {
            TreeEntry::Blob {
                name,
                hash,
                size,
                mtime,
                mode,
            } => {
                let abs_path = dir.join(&name);
                // clone --force: if the destination path already exists, do not
                // overwrite it — count it as skipped and move on.
                if force && abs_path.exists() {
                    *existing_skipped += 1;
                    continue;
                }
                let should_stub = threshold > 0 && size >= threshold && !in_git_worktree;
                if should_stub {
                    // Stub purely from the parent tree entry's metadata: download
                    // nothing for a stubbed blob (not even its chunk manifest).
                    stub::write(
                        work_dir,
                        &rel_path,
                        &StubRecord {
                            target_type: crate::stub::StubTargetType::Blob,
                            hash,
                            size,
                            mtime,
                            mode,
                            blob_count: 0,
                        },
                    )?;
                    *stubbed += 1;
                } else {
                    // Materialise on demand: fetch this blob (and its chunk bodies
                    // if it is a chunked manifest) from the remote into the local
                    // plaintext cache, then stream it to the working tree. Only the
                    // blobs we actually materialise are downloaded.
                    if !local.exists(&hash)? {
                        if !src.exists(&hash)? {
                            return Err(Error::Other(format!(
                                "blob not found in cache or remote: {}",
                                &hash.as_str()[..8]
                            )));
                        }
                        crate::commands::push::transfer_objects(
                            src, local, &hash, remote_key, false,
                        )?;
                    }
                    crate::fsmeta::materialise_blob_at(local, &hash, &abs_path, &mtime, &mode)?;
                    *expanded += 1;
                }
            }
            TreeEntry::Tree {
                name,
                hash,
                size,
                mtime,
                blob_count,
            } => {
                let sub_dir = dir.join(&name);
                // clone --force: if a non-directory already exists at the tree's
                // destination path, skip it entirely (do not overwrite). An
                // existing directory is fine — recurse into it and let its
                // children be skipped individually.
                if force && sub_dir.exists() && !sub_dir.is_dir() {
                    *existing_skipped += 1;
                    continue;
                }
                // Whole-directory stubbing is allowed for ANY directory at or
                // above the threshold, including a git repo root: the directory
                // is stubbed as a unit (download nothing — not even its tree
                // object) and `expand` restores it intact. The git rule only
                // forbids PARTIAL stubbing inside a git worktree that is being
                // materialised; that is enforced via `in_git_worktree` below.
                // See design/08 "Stubs and Git repositories".
                let should_stub = threshold > 0 && size >= threshold && !in_git_worktree;
                if should_stub {
                    fs::create_dir_all(&sub_dir)?;
                    stub::write_dir_stub(
                        work_dir,
                        &rel_path,
                        &StubRecord {
                            target_type: crate::stub::StubTargetType::Tree,
                            hash,
                            size,
                            mtime,
                            mode: None,
                            blob_count,
                        },
                    )?;
                    *stubbed += 1;
                } else {
                    // We are descending (materialising) this below-threshold
                    // directory. Its children must not be partial-stubbed if we
                    // are inside a git worktree, or if this entry is ".git", or if
                    // this directory is itself a git repo root. The "repo root"
                    // case is detected at the top of the recursive call from the
                    // tree entries it reads anyway, so it does not need a separate
                    // read here; we only need to propagate the inherited / ".git"
                    // cases.
                    let child_in_git = in_git_worktree || name == ".git";
                    expand_or_stub(
                        &hash,
                        &sub_dir,
                        src,
                        local,
                        remote_key,
                        work_dir,
                        &rel_path,
                        threshold,
                        child_in_git,
                        force,
                        expanded,
                        stubbed,
                        existing_skipped,
                    )?;
                    if let Some(mt) = mtime {
                        let ft =
                            FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                        filetime::set_file_mtime(&sub_dir, ft).ok();
                    }
                }
            }
            TreeEntry::Symlink {
                name,
                target,
                mtime,
            } => {
                #[cfg(unix)]
                {
                    let link_path = dir.join(&name);
                    crate::fsmeta::write_symlink_atomic(&link_path, &target)?;
                    crate::fsmeta::restore_symlink_mtime(&link_path, &mtime);
                }
                let _ = target;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::pack::root_pointer::{LocalRootPointer, RootPointer, RootToken};
    use crate::store::ObjectStore;
    use crate::store::cloud::{CloudObjects, MemCloud, MemCloudRootPointer};
    use crate::store::local::LocalStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A fresh local root pointer + store rooted at the same empty temp dir.
    /// The TempDir guard is returned to keep the backing directory alive.
    fn local_backend() -> (Box<dyn RootPointer>, LocalStore, TempDir) {
        let tmp = TempDir::new().unwrap();
        let rp = LocalRootPointer::new(tmp.path().to_path_buf(), None);
        let store = LocalStore::for_remote(tmp.path());
        (Box::new(rp), store, tmp)
    }

    /// A fresh MemCloud-backed root pointer + store sharing one in-memory cloud,
    /// modelling a cloud remote (S3/Azure/GCS) without a live endpoint. The root
    /// pointer key matches the unencrypted index-root cloud key layout, but the
    /// validation logic only depends on presence, so any fixed key works here.
    fn cloud_backend() -> (Box<dyn RootPointer>, LocalStore) {
        let cloud = Arc::new(MemCloud::new());
        let rp = MemCloudRootPointer::for_key(cloud.clone(), "repo/INDEX_ROOT");
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let store = LocalStore::for_cloud(objects, None);
        (Box::new(rp), store)
    }

    /// Put one object into the store so the New-emptiness check sees content.
    fn write_one_object(store: &LocalStore) {
        let data = b"obj";
        let hash = crate::object::Hash::compute(data);
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();
    }

    // --- New intent ---------------------------------------------------------

    #[test]
    fn new_on_empty_local_ok() {
        let (rp, store, _g) = local_backend();
        validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::New).unwrap();
    }

    #[test]
    fn new_on_empty_cloud_ok() {
        let (rp, store) = cloud_backend();
        validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::New).unwrap();
    }

    #[test]
    fn new_rejects_existing_index_root_local() {
        let (rp, store, _g) = local_backend();
        rp.cas_write(&RootToken::Absent, b"root").unwrap();
        let err = validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::New).unwrap_err();
        assert!(err.to_string().contains("not empty"), "got: {err}");
    }

    #[test]
    fn new_rejects_existing_index_root_cloud() {
        let (rp, store) = cloud_backend();
        rp.cas_write(&RootToken::Absent, b"root").unwrap();
        let err = validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::New).unwrap_err();
        assert!(err.to_string().contains("not empty"), "got: {err}");
    }

    #[test]
    fn new_rejects_stored_objects_with_no_index_root_cloud() {
        // No index root, but objects/ has content: still non-empty (this is the
        // encrypted-index-root case, where the root lives under an object key).
        let (rp, store) = cloud_backend();
        write_one_object(&store);
        assert!(rp.read().unwrap().0.is_none());
        let err = validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::New).unwrap_err();
        assert!(err.to_string().contains("not empty"), "got: {err}");
    }

    // --- Existing intent ----------------------------------------------------

    #[test]
    fn existing_with_index_root_ok_cloud() {
        let (rp, store) = cloud_backend();
        rp.cas_write(&RootToken::Absent, b"root").unwrap();
        validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::Existing).unwrap();
    }

    #[test]
    fn existing_without_index_root_errors_unencrypted() {
        let (rp, store) = cloud_backend();
        let err =
            validate_remote_state(rp.as_ref(), &store, false, RemoteIntent::Existing).unwrap_err();
        assert!(
            err.to_string().contains("INDEX_ROOT not found"),
            "got: {err}"
        );
    }

    #[test]
    fn existing_without_index_root_errors_encrypted() {
        // Encrypted remotes surface the key-may-be-wrong hint instead.
        let (rp, store) = cloud_backend();
        let err =
            validate_remote_state(rp.as_ref(), &store, true, RemoteIntent::Existing).unwrap_err();
        assert!(
            err.to_string().contains("index root not found"),
            "got: {err}"
        );
    }

    // --- Guided menu --------------------------------------------------------

    /// A scripted prompter that returns queued lines in order, so the menu's
    /// field collection can be tested without a terminal. `visible`/`hidden`
    /// draw from the same queue; `choice` returns `None` once the queue is
    /// exhausted (modelling EOF).
    struct ScriptedPrompt {
        lines: std::collections::VecDeque<String>,
    }

    impl ScriptedPrompt {
        fn new(lines: &[&str]) -> Self {
            ScriptedPrompt {
                lines: lines.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Prompter for ScriptedPrompt {
        fn visible(&mut self, _prompt: &str) -> Result<String, Error> {
            Ok(self.lines.pop_front().unwrap_or_default())
        }
        fn hidden(&mut self, _prompt: &str) -> Result<String, Error> {
            Ok(self.lines.pop_front().unwrap_or_default())
        }
        fn choice(&mut self, _prompt: &str) -> Result<Option<String>, Error> {
            Ok(self.lines.pop_front())
        }
    }

    #[test]
    fn menu_local_collects_path() {
        let mut p = ScriptedPrompt::new(&["1", "/srv/repo"]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        match cfg {
            RemoteConfig::Local { path, encryption } => {
                assert_eq!(path, "/srv/repo");
                assert!(encryption.is_none());
            }
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn menu_s3_plain_aws_no_keys() {
        // Choice 2, bucket, prefix, region (default), no access key, no endpoint.
        let mut p = ScriptedPrompt::new(&["2", "my-bucket", "repo", "", "", ""]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        match cfg {
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
                assert_eq!(bucket, "my-bucket");
                assert_eq!(prefix, "repo");
                assert_eq!(region, "us-east-1"); // default applied
                assert!(access_key_id.is_none());
                assert!(secret_access_key.is_none());
                assert!(endpoint.is_none());
                assert!(force_path_style.is_none());
            }
            other => panic!("expected S3, got {:?}", other),
        }
    }

    #[test]
    fn menu_s3_compatible_with_endpoint_and_keys() {
        // bucket, prefix, region, access key, secret, endpoint, path-style answer.
        let mut p = ScriptedPrompt::new(&[
            "2",
            "omemfs-test",
            "",
            "ap-northeast-1",
            "minioadmin",
            "minioadmin",
            "http://localhost:9000",
            "", // empty answer -> defaults to yes (not "n")
        ]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        match cfg {
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
                assert_eq!(bucket, "omemfs-test");
                assert_eq!(prefix, "");
                assert_eq!(region, "ap-northeast-1");
                assert_eq!(access_key_id.as_deref(), Some("minioadmin"));
                assert_eq!(secret_access_key.as_deref(), Some("minioadmin"));
                assert_eq!(endpoint.as_deref(), Some("http://localhost:9000"));
                assert_eq!(force_path_style, Some(true));
            }
            other => panic!("expected S3, got {:?}", other),
        }
    }

    #[test]
    fn menu_s3_path_style_declined() {
        let mut p = ScriptedPrompt::new(&["2", "b", "", "", "", "http://localhost:9000", "n"]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        match cfg {
            RemoteConfig::S3 {
                force_path_style, ..
            } => {
                assert_eq!(force_path_style, Some(false));
            }
            other => panic!("expected S3, got {:?}", other),
        }
    }

    #[test]
    fn menu_gcs_collects_fields() {
        let mut p =
            ScriptedPrompt::new(&["3", "gcs-bucket", "repo", "/keys/sa.json", "my-project", ""]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        match cfg {
            RemoteConfig::Gcs {
                bucket,
                prefix,
                project_id,
                credentials_json_path,
                credentials_json,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "gcs-bucket");
                assert_eq!(prefix, "repo");
                assert_eq!(project_id.as_deref(), Some("my-project"));
                assert_eq!(credentials_json_path.as_deref(), Some("/keys/sa.json"));
                assert!(credentials_json.is_none());
                assert!(endpoint.is_none());
            }
            other => panic!("expected Gcs, got {:?}", other),
        }
    }

    #[test]
    fn menu_azure_collects_fields() {
        let mut p = ScriptedPrompt::new(&[
            "4", "acct", "cont", "repo", "tenant", "client", "secret", "",
        ]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        match cfg {
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
                assert_eq!(account, "acct");
                assert_eq!(container, "cont");
                assert_eq!(prefix, "repo");
                assert_eq!(tenant_id, "tenant");
                assert_eq!(client_id, "client");
                assert_eq!(client_secret, "secret");
                assert!(endpoint.is_none());
            }
            other => panic!("expected Azure, got {:?}", other),
        }
    }

    #[test]
    fn menu_reprompts_on_invalid_choice() {
        // "9" and "" are rejected, then "1" selects local.
        let mut p = ScriptedPrompt::new(&["9", "", "1", "/srv/repo"]);
        let cfg = collect_remote_from_menu(&mut p).unwrap();
        assert!(matches!(cfg, RemoteConfig::Local { .. }));
    }

    #[test]
    fn menu_eof_without_choice_errors() {
        // Empty queue: choice() returns None on the first call (EOF).
        let mut p = ScriptedPrompt::new(&[]);
        let err = collect_remote_from_menu(&mut p).unwrap_err();
        assert!(err.to_string().contains("end of input"), "got: {err}");
    }

    #[test]
    fn menu_s3_empty_bucket_errors() {
        let mut p = ScriptedPrompt::new(&["2", ""]);
        let err = collect_remote_from_menu(&mut p).unwrap_err();
        assert!(err.to_string().contains("bucket is required"), "got: {err}");
    }
}
