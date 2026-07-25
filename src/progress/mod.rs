//! Progress output: log file + optional TTY phase-tree view.
//!
//! All instrumentation is always active: every `dlog_lN!` / `dtimer_lN!` call
//! writes to a log file under `.omemfs/logs/`, and — when stdout is a TTY —
//! also updates a phase-tree view using cursor-up + erase.
//!
//! # IO architecture
//!
//! Both IO paths are fully asynchronous to avoid blocking the hot path:
//!
//! - **Log file**: `write_line()` queues lines on an `mpsc` channel; a
//!   dedicated background thread flushes them to disk.
//! - **Phase view**: `PhaseHandle` sends updates over an `mpsc` channel;
//!   a `RefreshThread` drains the channel and redraws the terminal at a
//!   fixed 100 ms interval.
//!
//! # Initialisation
//!
//! Before running a command, call [`ProgressContext::new`] with the command
//! name and whether stdout is a TTY. As soon as the repository directory is
//! known, call [`ProgressContext::set_repo_dir`] so the log file is moved to
//! its final location. At command exit, call [`ProgressContext::finish`].
//!
//! The macros in `src/debug.rs` reach the active context via the
//! `PROGRESS` thread-local.
//!
//! # Command output ordering (TTY mode)
//!
//! When stdout is a TTY, commands do not write their results directly to
//! stdout.  Instead, [`Output`] accumulates them in a buffer and calls
//! [`deposit_output`] at `Output::finish()` time.  The buffers are held here
//! in `pending_output` and are only flushed to stdout inside
//! [`ProgressContext::finish`], which is called by `main.rs` *after* the
//! command function returns and all [`crate::debug::DebugTimer`] guards have
//! dropped.  This guarantees that the last phase rows appear on screen before
//! the command's results, without any pin/clear trickery.
//!
//! Multiple [`Output`] instances may call [`deposit_output`] in a single
//! command (e.g. early-return paths that print "Already up to date").  All
//! deposited buffers are concatenated and flushed in order at `finish()`.
//!
//! [`Output`]: crate::term::Output

pub mod log_file;
pub mod phase_view;

use std::cell::RefCell;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log_file::LogFile;
use phase_view::{PhaseHandle, PhaseUpdate, PhaseView};

use crate::debug::Layer;

/// Interval between TTY refreshes.
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// Process-global log file handle shared across all threads.
static GLOBAL_LOG: OnceLock<LogFile> = OnceLock::new();

/// Process start time, used by worker threads to compute elapsed seconds.
static GLOBAL_START: OnceLock<std::time::Instant> = OnceLock::new();

/// Process-global progress context, set by [`set_context`].
static GLOBAL_PROGRESS: OnceLock<Arc<ProgressContext>> = OnceLock::new();

/// The per-process (main-thread) progress context.
pub struct ProgressContext {
    log: LogFile,
    /// When true, emit progress lines to stdout via the phase-tree TTY view.
    tty: bool,
    /// When true, use ANSI color in phase detail lines (respects NO_COLOR).
    color: bool,
    start: Instant,
    /// Phase-tree TTY view (shared with the RefreshThread).
    phase_view: Arc<Mutex<PhaseView>>,
    /// Sender half of the PhaseUpdate channel (shared with PhaseHandles).
    phase_tx: std::sync::mpsc::SyncSender<(usize, PhaseUpdate)>,
    /// Stop-signal sender for the RefreshThread.  Dropped in `finish()` to
    /// signal the thread to exit.
    refresh_stop_tx: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    /// Background refresh thread handle.
    refresh_thread: Mutex<Option<JoinHandle<()>>>,
    /// Buffered command output deposited by `Output::finish()` (TTY mode).
    pending_output: Mutex<Vec<Vec<u8>>>,
    /// The most recently started phase id. `dlog` / `dtimer` detail lines are
    /// routed to this phase so they appear in the TTY view as the phase runs.
    ///
    /// Shared with the `PhaseHandle` returned by `begin_phase` so the handle can
    /// clear it when the phase completes / fails / is dropped. Without this,
    /// detail lines emitted between a phase finishing and the next phase
    /// starting would be attributed to the already-finished phase.
    active_phase_id: Arc<Mutex<Option<usize>>>,
}

impl ProgressContext {
    /// Initialise a new context for the given command.
    ///
    /// `tty` controls whether progress lines are printed to stdout. Pass
    /// `false` when stdout is not a TTY (pipe / redirect / CI).
    pub fn new(command: &str, tty: bool) -> std::io::Result<Self> {
        let log = LogFile::new(command)?;
        let color = tty && crate::term::color_enabled(crate::term::ColorChoice::Auto, true);
        let (phase_view_inner, phase_tx) = PhaseView::new(tty);
        let phase_view = Arc::new(Mutex::new(phase_view_inner));

        let (stop_tx, refresh_thread) = if tty {
            let view = Arc::clone(&phase_view);
            let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
            let handle = std::thread::spawn(move || {
                run_refresh_thread(view, rx);
            });
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        Ok(Self {
            log,
            tty,
            color,
            start: Instant::now(),
            phase_view,
            phase_tx,
            refresh_stop_tx: Mutex::new(stop_tx),
            refresh_thread: Mutex::new(refresh_thread),
            pending_output: Mutex::new(Vec::new()),
            active_phase_id: Arc::new(Mutex::new(None)),
        })
    }

    /// Register a new phase and return a [`PhaseHandle`] for it.
    ///
    /// When TTY is disabled, returns a no-op handle.
    pub fn begin_phase(&self, label: impl Into<String>) -> PhaseHandle {
        if !self.tty {
            return PhaseHandle::noop();
        }
        let id = self.phase_view.lock().unwrap().add_phase(label.into());
        *self.active_phase_id.lock().unwrap() = Some(id);
        PhaseHandle::new(id, self.phase_tx.clone(), Arc::clone(&self.active_phase_id))
    }

    /// Returns a clone of the `LogFile` handle so worker threads can write
    /// log lines without going through the main thread.
    pub fn log_handle(&self) -> LogFile {
        self.log.clone()
    }

    /// Move the log file to its final location under `repo_dir/.omemfs/logs/`.
    pub fn set_repo_dir(&self, repo_dir: &Path) {
        self.log.set_repo_dir(repo_dir);
    }

    /// Emit a single log line. Always writes to the log file; in TTY mode also
    /// routes the line as a detail entry to the currently active phase.
    pub fn emit(&self, depth: usize, layer: Layer, text: &str) {
        let elapsed = self.start.elapsed().as_secs_f64();
        let plain = format_line(layer, elapsed, depth, text);
        self.log.write_line(&plain);

        if self.tty
            && let Some(id) = *self.active_phase_id.lock().unwrap()
        {
            let detail = if self.color {
                format_colored_line(layer, elapsed, depth, text)
            } else {
                plain.clone()
            };
            // Best-effort: dropped when channel is full. Log file always receives.
            let _ = self.phase_tx.try_send((id, PhaseUpdate::Detail(detail)));
        }
    }

    /// Write a plain output line immediately, interleaved with the phase view.
    ///
    /// In TTY mode the phase area is temporarily cleared, the line is printed,
    /// and the view is redrawn immediately.
    /// In non-TTY mode the line is written directly to stdout.
    pub fn emit_output_line(&self, line: &str) {
        if self.tty {
            self.phase_view.lock().unwrap().emit_inline(line);
        } else {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{}", line);
            let _ = out.flush();
        }
    }

    /// Accept command output bytes to be held until `finish()`.
    pub fn deposit_output(&self, buf: Vec<u8>) {
        self.pending_output.lock().unwrap().push(buf);
    }

    /// Stop the refresh thread and seal the phase view, leaving the completed
    /// phase list on screen and guaranteeing nothing redraws over it afterwards.
    ///
    /// Call this just before streaming raw bytes (e.g. `cat` blob content)
    /// directly to stdout. The subsequent `finish()` is a no-op for the phase
    /// view (it is already sealed) but still flushes the log file and any
    /// deposited command output.
    pub fn freeze_phase_view(&self) {
        // Stop the RefreshThread first so it cannot redraw concurrently with the
        // final seal redraw. Dropping the stop sender + join is idempotent: a
        // later `finish()` finds both slots already taken.
        {
            let mut guard = self.refresh_stop_tx.lock().unwrap();
            drop(guard.take());
        }
        if let Some(handle) = self.refresh_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        if self.tty {
            self.phase_view.lock().unwrap().seal();
        }
    }

    /// Finish: stop background threads, flush log file and any deposited
    /// command output.
    ///
    /// When `errored == true`, the progress area is erased; when `false`, the
    /// phase list is left on screen and command output is printed below it.
    ///
    /// Shutdown order:
    ///   1. Stop RefreshThread (drop stop sender, join).
    ///   2. Perform a final phase-view redraw or erase.
    ///   3. Flush log writer thread (send shutdown, join).
    ///   4. Write buffered command output to stdout.
    pub fn finish(&self, errored: bool) {
        // 1. Signal RefreshThread to stop and wait for it.
        {
            let mut guard = self.refresh_stop_tx.lock().unwrap();
            drop(guard.take());
        }
        if let Some(handle) = self.refresh_thread.lock().unwrap().take() {
            let _ = handle.join();
        }

        // 2. Final phase-view flush.
        if self.tty {
            self.phase_view.lock().unwrap().finish(errored);
        }

        // 3. Flush log writer thread.
        self.log.finish();

        // 4. Write buffered command output below the progress lines.
        let pending: Vec<Vec<u8>> = std::mem::take(&mut *self.pending_output.lock().unwrap());
        if !pending.is_empty() {
            let mut stdout = std::io::stdout();
            for buf in pending {
                let _ = stdout.write_all(&buf);
            }
            let _ = stdout.flush();
        }
    }

    /// Elapsed seconds since this context was created.
    pub fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

// ---------------------------------------------------------------------------
// RefreshThread
// ---------------------------------------------------------------------------

fn run_refresh_thread(view: Arc<Mutex<PhaseView>>, stop: std::sync::mpsc::Receiver<()>) {
    loop {
        match stop.recv_timeout(REFRESH_INTERVAL) {
            Ok(()) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut v) = view.lock() {
                    v.refresh();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Line formatting helpers
// ---------------------------------------------------------------------------

/// Format a single log line without ANSI color (used for the log file).
pub fn format_line(layer: Layer, elapsed_secs: f64, depth: usize, text: &str) -> String {
    let indent = "  ".repeat(depth);
    format!(
        "[omemfs {}] {:+9.3}s {}{}",
        layer.tag(),
        elapsed_secs,
        indent,
        text
    )
}

/// Format a single log line with ANSI color (used for TTY phase detail display).
fn format_colored_line(layer: Layer, elapsed_secs: f64, depth: usize, text: &str) -> String {
    let indent = "  ".repeat(depth);
    let code = layer.color_code();
    format!(
        "\x1b[{}m[omemfs {}]\x1b[0m {:+9.3}s {}{}",
        code,
        layer.tag(),
        elapsed_secs,
        indent,
        text
    )
}

// ---------------------------------------------------------------------------
// Thread-local storage
// ---------------------------------------------------------------------------

thread_local! {
    /// The active `ProgressContext` for the current thread.
    static PROGRESS: RefCell<Option<Arc<ProgressContext>>> = const { RefCell::new(None) };
}

/// Install `ctx` as the active context for this thread.
/// Also registers the log file, start time, and context in process-global
/// slots so worker threads can write.
pub fn set_context(ctx: Arc<ProgressContext>) {
    let _ = GLOBAL_LOG.set(ctx.log.clone());
    let _ = GLOBAL_START.set(ctx.start);
    let _ = GLOBAL_PROGRESS.set(ctx.clone());
    PROGRESS.with(|p| {
        *p.borrow_mut() = Some(ctx);
    });
}

/// Clear the active context.
pub fn clear_context() {
    PROGRESS.with(|p| {
        *p.borrow_mut() = None;
    });
}

/// Emit a log line through the active context.
///
/// Used by the `dlog_lN!` macros in `debug.rs`.
/// On worker threads the thread-local `PROGRESS` is not set, so we fall
/// back to writing directly to the process-global `GLOBAL_LOG`.
pub fn emit(depth: usize, layer: Layer, text: &str) {
    let handled = PROGRESS.with(|p| {
        if let Some(ctx) = p.borrow().as_ref() {
            ctx.emit(depth, layer, text);
            true
        } else {
            false
        }
    });
    if !handled {
        if let Some(ctx) = GLOBAL_PROGRESS.get() {
            ctx.emit(depth, layer, text);
        } else if let Some(log) = GLOBAL_LOG.get() {
            let elapsed = GLOBAL_START
                .get()
                .map(|s| s.elapsed().as_secs_f64())
                .unwrap_or(0.0);
            let plain = format_line(layer, elapsed, depth, text);
            log.write_line(&plain);
        }
    }
}

/// Return elapsed seconds from the active context, or `0.0` if none.
pub fn elapsed_secs() -> f64 {
    PROGRESS.with(|p| {
        p.borrow()
            .as_ref()
            .map(|ctx| ctx.elapsed_secs())
            .unwrap_or(0.0)
    })
}

/// Notify the active context that the repository root is now known,
/// so the log file can be moved to its final location.
pub fn notify_repo_dir(repo_dir: &std::path::Path) {
    PROGRESS.with(|p| {
        if let Some(ctx) = p.borrow().as_ref() {
            ctx.set_repo_dir(repo_dir);
        }
    });
}

/// Emit a plain output line immediately, interleaved with the phase view.
///
/// Use this for streaming per-item results (e.g. "restored: foo.txt") that
/// should appear in real time rather than being buffered until command exit.
/// Falls back to a direct `println!` when no context is active.
pub fn emit_output_line(line: &str) {
    let handled = PROGRESS.with(|p| {
        if let Some(ctx) = p.borrow().as_ref() {
            ctx.emit_output_line(line);
            true
        } else {
            false
        }
    });
    if !handled {
        println!("{}", line);
    }
}

/// Deposit buffered command output into the active context to be flushed
/// after all pending DebugTimers have dropped.
///
/// Called by `Output::finish()` in TTY mode.  Returns the buffer unchanged
/// if there is no active context (caller should flush it directly instead).
pub fn deposit_output(buf: Vec<u8>) -> Option<Vec<u8>> {
    PROGRESS.with(|p| {
        if let Some(ctx) = p.borrow().as_ref() {
            ctx.deposit_output(buf);
            None
        } else {
            Some(buf)
        }
    })
}

/// Register a new phase in the active context and return a [`PhaseHandle`].
///
/// When no context is active (e.g. in tests or non-TTY environments without
/// a context), returns a no-op handle.
pub fn begin_phase(label: impl Into<String>) -> PhaseHandle {
    PROGRESS.with(|p| {
        if let Some(ctx) = p.borrow().as_ref() {
            ctx.begin_phase(label)
        } else {
            PhaseHandle::noop()
        }
    })
}

/// Stop the refresh thread and seal the phase view via the active context.
///
/// No-op when no context is active. Call this before streaming raw bytes
/// directly to stdout so the periodic redraw cannot erase that output.
/// See [`ProgressContext::freeze_phase_view`].
pub fn freeze_phase_view() {
    PROGRESS.with(|p| {
        if let Some(ctx) = p.borrow().as_ref() {
            ctx.freeze_phase_view();
        }
    });
}
