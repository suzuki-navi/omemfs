#![allow(clippy::too_many_arguments, clippy::type_complexity)]

mod base32;
mod codec;
mod commands;
pub mod debug;
mod error;
mod filter;
mod fsmeta;
mod io_stats;
mod lock;
mod log_parser;
mod object;
pub mod progress;
mod repo;
mod scan;
mod stat_cache;
mod store;
mod stub;
mod term;
mod tree_ops;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

use commands::stub as stub_cmd;
use commands::{
    cat, clone, conflict as conflict_cmd, expand, log as log_cmd, ls, pack, pull, push, restore,
    stats,
};

#[derive(Parser)]
#[command(name = "omemfs", about = "Object-memory filesystem sync tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialise a local repository interactively and populate from a remote
    Clone {
        /// Destination directory (default: current directory)
        directory: Option<PathBuf>,
        /// Remote URL to clone from without interactive prompts
        #[arg(long)]
        url: Option<String>,
        /// Generate a DEK and enable encryption (only valid for a new repository)
        #[arg(long)]
        encrypt: bool,
        /// Declare the remote is a new (empty) repository
        #[arg(long)]
        new: bool,
        /// Declare the remote is an existing repository
        #[arg(long)]
        existing: bool,
        /// Stub files at or above this size (e.g. 1M, 100K, 0 to expand all)
        #[arg(long, default_value = "1M")]
        stub_threshold: String,
        /// Allow cloning into a non-empty directory
        #[arg(long)]
        force: bool,
    },

    /// Scan working tree, upload objects, and update INDEX_ROOT
    Push {
        /// Optional path scope(s) (push only these paths)
        paths: Vec<PathBuf>,
        /// After pushing to origin, also push the current state to the backup remote
        #[arg(long)]
        with_backup: bool,
        /// Show what would be pushed without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Fetch remote root and apply non-conflicting changes
    Pull {
        /// Optional path scope(s) (pull only these paths)
        paths: Vec<PathBuf>,
        /// Show what would change without applying anything
        #[arg(long)]
        dry_run: bool,
        /// Stub new remote entries at or above this size
        #[arg(long, default_value = "1M")]
        stub_threshold: String,
    },

    /// List entries with sync status
    Ls {
        /// Paths to list (default: current directory)
        paths: Vec<PathBuf>,
        /// List all descendants recursively
        #[arg(short = 'r', long)]
        recursive: bool,
        /// Show full 64-character hash
        #[arg(long)]
        full_hash: bool,
        /// Show only entries that differ from clone root (implies -r)
        #[arg(long)]
        dirty: bool,
        /// Skip remote check; R column always shows space
        #[arg(long)]
        no_remote: bool,
        /// Show hash/size/blob_count/mtime from remote root (conflicts with --no-remote)
        #[arg(long, conflicts_with_all = ["clone", "working"])]
        remote: bool,
        /// Show hash/size/blob_count/mtime from clone root
        #[arg(long, conflicts_with_all = ["remote", "working"])]
        clone: bool,
        /// Show hash/size/blob_count/mtime from working tree (default)
        #[arg(long, conflicts_with_all = ["remote", "clone"])]
        working: bool,
    },

    /// Discard local changes and restore paths to clone_root state
    Restore {
        /// Paths to restore (default: entire working tree)
        paths: Vec<PathBuf>,
        /// Show what would be restored without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Print content of an object to stdout
    Cat {
        /// hash, clone-root, remote-root, index-root, or ref[:path] (path may also be separated by /)
        target: String,
        /// Print only the resolved 64-character hash instead of the object content
        #[arg(long)]
        hash: bool,
        /// Remote name (default: origin)
        #[arg(long, default_value = "origin")]
        remote: String,
    },

    /// Convert working tree files into stubs
    Stub {
        /// Files or directories to stub
        paths: Vec<PathBuf>,
        /// Show what would be stubbed without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Materialise stubbed files into the working tree
    Expand {
        /// Paths to expand (default: expand all stubs)
        paths: Vec<std::path::PathBuf>,
        /// Remote to download from if blobs are not cached locally
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Show what would be expanded without making changes
        #[arg(long)]
        dry_run: bool,
        /// Only expand stubs below this size; stubs at or above stay stubbed (e.g. 1M, 100K)
        #[arg(long, default_value = "1M")]
        stub_threshold: String,
        /// Expand all stubs regardless of size (overrides --stub-threshold)
        #[arg(short = 'r', long)]
        recursive: bool,
    },

    /// Compact the remote pack layer and merge delta indexes
    Pack,

    /// Print object store statistics
    Stats {
        /// Also compute the remote-backed sections (Remote storage and Remote
        /// object sizes). Without this flag no remote I/O is performed.
        #[arg(long)]
        remote: bool,
        /// Output in JSON format
        #[arg(long)]
        json: bool,
    },

    /// Manage conflict helper files left by pull
    Conflict {
        #[command(subcommand)]
        subcommand: ConflictCommand,
    },

    /// Repository configuration commands
    Config {
        #[command(subcommand)]
        subcommand: ConfigCommand,
    },

    /// Analyse log files in .omemfs/logs/
    Log {
        #[command(subcommand)]
        subcommand: LogCommand,
    },
}

#[derive(Subcommand)]
enum ConflictCommand {
    /// List all paths with unresolved conflict helper files
    List,
    /// Delete conflict helper files without modifying the originals
    Clean {
        /// Paths to scope the operation (default: entire working tree)
        paths: Vec<PathBuf>,
        /// Show what would be deleted without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Adopt the remote version and remove helper files
    AcceptRemote {
        /// Paths to resolve (default: entire working tree)
        paths: Vec<PathBuf>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Adopt the local version and remove helper files
    AcceptLocal {
        /// Paths to resolve (default: entire working tree)
        paths: Vec<PathBuf>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Adopt the base (clone root) version and remove helper files
    AcceptBase {
        /// Paths to resolve (default: entire working tree)
        paths: Vec<PathBuf>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// List log files in the repository, newest first
    Ls {
        /// Show only logs whose name contains this command (e.g. push)
        #[arg(long)]
        cmd: Option<String>,
        /// Show at most N entries (default: 10)
        #[arg(short = 'n', default_value = "10")]
        count: usize,
    },
    /// Display log lines with optional layer/grep filtering
    Show {
        /// Log REF: omitted=latest, @N=nth entry, logical name, or file path
        file: Option<String>,
        /// Show only lines from this layer (repeatable; e.g. --layer L4)
        #[arg(long = "layer")]
        layers: Vec<String>,
        /// Show only lines whose message contains this pattern
        #[arg(long)]
        grep: Option<String>,
    },
    /// Aggregate timer spans and print statistics
    Timers {
        /// Log REF: omitted=latest, @N=nth entry, logical name, or file path
        file: Option<String>,
        /// Sort key: total (default), avg, count, max
        #[arg(long, default_value = "total")]
        sort: String,
        /// Restrict to lines from this layer (repeatable; e.g. --layer L7)
        #[arg(long = "layer")]
        layers: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Export the full repository config as an omemfs_repo_ connection string
    Export,
    /// Add or replace the backup remote interactively
    AddBackup {
        /// Overwrite an existing backup remote configuration
        #[arg(long)]
        force: bool,
        /// Remote URL for the backup (skips the URL prompt)
        #[arg(long)]
        url: Option<String>,
        /// Declare the backup remote is a new (empty) repository
        #[arg(long)]
        new: bool,
        /// Declare the backup remote is an existing repository
        #[arg(long)]
        existing: bool,
        /// Generate a DEK and enable encryption (only valid for a new repository)
        #[arg(long)]
        encrypt: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    // The current working directory where the command was invoked. Relative
    // `<path>` arguments are resolved against this; the repository root is then
    // discovered by walking up from here (see `Repo::discover`). Canonicalise so
    // `strip_prefix` against the (also canonical) repo root is reliable across
    // symlinks; fall back to the raw cwd if canonicalisation fails.
    let cwd = std::env::current_dir().expect("cannot determine current directory");
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // `log` subcommands analyse existing log files and must not create a new one.
    // They operate on `.omemfs/logs/` under the repository root, so they discover
    // the root by walking up from the cwd like every other non-clone command.
    if let Command::Log { subcommand } = cli.command {
        let work_dir = match repo::Repo::discover(&cwd) {
            Ok(repo) => repo.work_dir,
            Err(e) => {
                eprintln!("error: {e}");
                process::exit(1);
            }
        };
        let result = run_log_command(subcommand, work_dir);
        if let Err(e) = result {
            eprintln!("error: {e}");
            process::exit(1);
        }
        return;
    }

    // Resolve the repository root for every command except `clone` (which
    // creates a new repository and therefore performs no upward discovery).
    // `work_dir` always denotes the repository root from here on; the cwd is
    // carried separately as `current_dir` for cwd-relative path resolution.
    let is_clone = matches!(cli.command, Command::Clone { .. });
    let work_dir = if is_clone {
        cwd.clone()
    } else {
        match repo::Repo::discover(&cwd) {
            Ok(repo) => repo.work_dir,
            Err(e) => {
                eprintln!("error: {e}");
                process::exit(1);
            }
        }
    };

    let command_name = match &cli.command {
        Command::Clone { .. } => "clone",
        Command::Push { .. } => "push",
        Command::Pull { .. } => "pull",
        Command::Ls { .. } => "ls",
        Command::Restore { .. } => "restore",
        Command::Cat { .. } => "cat",
        Command::Stub { .. } => "stub",
        Command::Expand { .. } => "expand",
        Command::Pack => "pack",
        Command::Stats { .. } => "stats",
        Command::Conflict { .. } => "conflict",
        Command::Config { .. } => "config",
        Command::Log { .. } => unreachable!(),
    };
    let tty = atty::is(atty::Stream::Stdout);
    let progress_ctx = match progress::ProgressContext::new(command_name, tty) {
        Ok(ctx) => std::sync::Arc::new(ctx),
        Err(e) => {
            eprintln!("omemfs: could not initialise log file: {}", e);
            process::exit(1);
        }
    };
    progress::set_context(std::sync::Arc::clone(&progress_ctx));

    let result = match cli.command {
        Command::Clone {
            directory,
            url,
            encrypt,
            new,
            existing,
            stub_threshold,
            force,
        } => {
            let directory = directory.unwrap_or_else(|| work_dir.clone());
            match parse_size(&stub_threshold) {
                Ok(stub_threshold) => clone::run(clone::CloneOptions {
                    url,
                    encrypt,
                    new,
                    existing,
                    directory,
                    stub_threshold,
                    force,
                }),
                Err(e) => Err(error::Error::Other(e)),
            }
        }

        Command::Push {
            paths,
            with_backup,
            dry_run,
        } => push::run(push::PushOptions {
            current_dir: cwd.clone(),
            work_dir,
            paths,
            dry_run,
            with_backup,
        }),

        Command::Pull {
            paths,
            dry_run,
            stub_threshold,
        } => match parse_size(&stub_threshold) {
            Ok(stub_threshold) => pull::run(pull::PullOptions {
                current_dir: cwd.clone(),
                work_dir,
                paths,
                dry_run,
                stub_threshold,
            }),
            Err(e) => Err(error::Error::Other(e)),
        },

        Command::Ls {
            paths,
            recursive,
            full_hash,
            dirty,
            no_remote,
            remote,
            clone,
            working: _,
        } => ls::run(ls::LsOptions {
            work_dir,
            current_dir: cwd.clone(),
            paths,
            recursive,
            full_hash,
            dirty,
            no_remote,
            source: if remote {
                ls::LsSource::Remote
            } else if clone {
                ls::LsSource::Clone
            } else {
                ls::LsSource::Working
            },
        }),

        Command::Restore { paths, dry_run } => restore::run(restore::RestoreOptions {
            work_dir,
            current_dir: cwd.clone(),
            paths,
            dry_run,
        }),

        Command::Cat {
            target,
            hash,
            remote,
        } => cat::run(cat::CatOptions {
            work_dir,
            target,
            hash_only: hash,
            remote_name: remote,
        }),

        Command::Stub { paths, dry_run } => stub_cmd::run(stub_cmd::StubOptions {
            work_dir,
            current_dir: cwd.clone(),
            paths,
            dry_run,
        }),

        Command::Expand {
            paths,
            remote,
            dry_run,
            stub_threshold,
            recursive,
        } => {
            // -r/--recursive overrides --stub-threshold (expand everything), so the
            // size string is only parsed (and validated) when --recursive is absent.
            let threshold = if recursive {
                Ok(0)
            } else {
                parse_size(&stub_threshold)
            };
            match threshold {
                Ok(stub_threshold) => expand::run(expand::ExpandOptions {
                    work_dir,
                    current_dir: cwd.clone(),
                    paths,
                    remote_name: remote,
                    dry_run,
                    stub_threshold,
                }),
                Err(e) => Err(error::Error::Other(e)),
            }
        }

        Command::Pack => pack::run(pack::PackOptions { work_dir }),

        Command::Stats { remote, json } => stats::run(stats::StatsOptions {
            work_dir,
            remote,
            json,
        }),

        Command::Conflict { subcommand } => match subcommand {
            ConflictCommand::List => {
                conflict_cmd::run_list(conflict_cmd::ConflictListOptions { work_dir })
            }
            ConflictCommand::Clean { paths, dry_run } => {
                conflict_cmd::run_clean(conflict_cmd::ConflictCleanOptions {
                    work_dir,
                    current_dir: cwd.clone(),
                    paths,
                    dry_run,
                })
            }
            ConflictCommand::AcceptRemote { paths, dry_run } => {
                conflict_cmd::run_accept(conflict_cmd::ConflictAcceptOptions {
                    work_dir,
                    current_dir: cwd.clone(),
                    paths,
                    dry_run,
                    side: conflict_cmd::AcceptSide::Remote,
                })
            }
            ConflictCommand::AcceptLocal { paths, dry_run } => {
                conflict_cmd::run_accept(conflict_cmd::ConflictAcceptOptions {
                    work_dir,
                    current_dir: cwd.clone(),
                    paths,
                    dry_run,
                    side: conflict_cmd::AcceptSide::Local,
                })
            }
            ConflictCommand::AcceptBase { paths, dry_run } => {
                conflict_cmd::run_accept(conflict_cmd::ConflictAcceptOptions {
                    work_dir,
                    current_dir: cwd.clone(),
                    paths,
                    dry_run,
                    side: conflict_cmd::AcceptSide::Base,
                })
            }
        },

        Command::Config { subcommand } => match subcommand {
            ConfigCommand::Export => run_config_export(&work_dir),
            ConfigCommand::AddBackup {
                force,
                url,
                new,
                existing,
                encrypt,
            } => clone::run_add_backup(clone::AddBackupOptions {
                work_dir: work_dir.clone(),
                force,
                url,
                new,
                existing,
                encrypt,
            }),
        },

        Command::Log { .. } => unreachable!(),
    };

    let errored = result.is_err();
    progress_ctx.finish(errored);
    progress::clear_context();

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn run_log_command(
    subcommand: LogCommand,
    work_dir: std::path::PathBuf,
) -> Result<(), crate::error::Error> {
    match subcommand {
        LogCommand::Ls { cmd, count } => log_cmd::run_ls(log_cmd::LogLsOptions {
            work_dir,
            cmd,
            count,
        }),
        LogCommand::Show { file, layers, grep } => log_cmd::run_show(log_cmd::LogShowOptions {
            work_dir,
            file,
            layers,
            grep,
        }),
        LogCommand::Timers { file, sort, layers } => {
            log_cmd::run_timers(log_cmd::LogTimersOptions {
                work_dir,
                file,
                sort,
                layers,
            })
        }
    }
}

fn run_config_export(work_dir: &std::path::Path) -> Result<(), crate::error::Error> {
    let repo = repo::Repo::open(work_dir)?;
    let config = repo.read_config()?;
    let conn_str = clone::encode_connection_string(&config)?;
    eprintln!("Warning: the following string contains credentials. Handle with care.");
    println!("{}", conn_str);
    Ok(())
}

/// Parse human-readable size strings like "1M", "100K", "1G", or plain bytes.
///
/// Accepted forms (per design/04_cli_spec.md): plain decimal bytes (`1024`), or a
/// non-negative decimal number followed by a single `K`, `M`, or `G` suffix
/// (case-insensitive, 1024-based). Any other input (trailing garbage such as
/// `10MBx`, multi-letter suffixes like `1GB`, empty string, non-numeric input,
/// or a value that overflows `u64`) is rejected as a hard error.
fn parse_size(s: &str) -> Result<u64, String> {
    let invalid = || {
        format!(
            "invalid size '{s}': expected bytes (e.g. 1024) or a number with a K/M/G suffix (e.g. 100K, 1M, 2G)"
        )
    };

    if s.is_empty() {
        return Err(invalid());
    }

    let last = s.as_bytes()[s.len() - 1];
    let multiplier: u64 = match last.to_ascii_uppercase() {
        b'K' => 1_024,
        b'M' => 1_024 * 1_024,
        b'G' => 1_024 * 1_024 * 1_024,
        _ => 1,
    };

    // The numeric portion is the whole string for a plain byte count, or the
    // string minus its trailing suffix when a K/M/G suffix is present.
    let num_str = if multiplier == 1 {
        s
    } else {
        &s[..s.len() - 1]
    };

    // Reject empty numeric portions ("K") and any non-decimal-digit content
    // (this also rejects signs, whitespace, and embedded suffixes like "10MBx",
    // because the leftover "10MB" fails to parse as a plain integer).
    if num_str.is_empty() || !num_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }

    let base: u64 = num_str.parse().map_err(|_| invalid())?;
    base.checked_mul(multiplier).ok_or_else(invalid)
}

#[cfg(test)]
mod parse_size_tests {
    use super::parse_size;

    #[test]
    fn plain_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1").unwrap(), 1);
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn k_m_g_suffixes() {
        assert_eq!(parse_size("1K").unwrap(), 1_024);
        assert_eq!(parse_size("100K").unwrap(), 100 * 1_024);
        assert_eq!(parse_size("1M").unwrap(), 1_024 * 1_024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1_024 * 1_024 * 1_024);
    }

    #[test]
    fn suffix_is_case_insensitive() {
        assert_eq!(parse_size("1k").unwrap(), 1_024);
        assert_eq!(parse_size("1m").unwrap(), 1_024 * 1_024);
        assert_eq!(parse_size("1g").unwrap(), 1_024 * 1_024 * 1_024);
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        // "10MBx" must not silently parse as 10MB.
        assert!(parse_size("10MBx").is_err());
        assert!(parse_size("1GB").is_err());
        assert!(parse_size("1KB").is_err());
        assert!(parse_size("100MM").is_err());
    }

    #[test]
    fn non_numeric_is_rejected() {
        assert!(parse_size("").is_err());
        assert!(parse_size("K").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-1").is_err());
        assert!(parse_size("1.5M").is_err());
        assert!(parse_size(" 10").is_err());
    }

    #[test]
    fn overflow_is_rejected() {
        // A value that overflows u64 when multiplied by the suffix is an error,
        // not a silent wrap to a small number.
        assert!(parse_size("99999999999999999999G").is_err());
        assert!(parse_size("18446744073709551616").is_err()); // u64::MAX + 1
    }
}
