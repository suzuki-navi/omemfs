//! Phase-tree TTY view for progress output.
//!
//! Each command registers named phases; the view renders them as a tree with
//! per-phase detail lines (up to MAX_DETAIL_LINES, oldest replaced by `...`).
//!
//! # Threading model
//!
//! `PhaseHandle` communicates with `PhaseView` over an `mpsc` channel so
//! `detail()` and `complete()` are non-blocking and safe to call from any
//! thread — including worker threads running concurrently with other phases.
//!
//! # TTY-disabled mode
//!
//! When `PhaseView::new(false)` is used (non-TTY), `begin_phase` returns a
//! no-op `PhaseHandle` that discards all updates.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------

/// Query the terminal width via `TIOCGWINSZ`. Returns 80 on failure.
fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if ret == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

/// Return the visible (display) width of `s`, in terminal cells, after stripping
/// ANSI escape sequences.
///
/// The width is the number of cells the text occupies when rendered, not the
/// number of `char`s: East Asian Wide / Fullwidth characters (e.g. CJK
/// ideographs) occupy 2 cells, and so do East Asian Ambiguous characters — such
/// as the phase status symbols `○` and `→` — under `width_cjk`, matching how
/// CJK/Japanese terminal locales render them. (`✓`/`✗` are Neutral and count as
/// 1; the crate's per-character classification is trusted.) See design/06.
fn visible_len(s: &str) -> usize {
    // Strip ANSI escape sequences first; they contribute 0 cells.
    let mut stripped = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for inner in chars.by_ref() {
                    if inner.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            stripped.push(c);
        }
    }
    UnicodeWidthStr::width_cjk(stripped.as_str())
}

/// Return the number of physical terminal rows a line occupies when printed.
fn physical_lines(s: &str, term_width: usize) -> usize {
    if term_width == 0 {
        return 1;
    }
    let vlen = visible_len(s);
    vlen.div_ceil(term_width).max(1)
}

/// Maximum detail lines shown per running phase (older lines become `...`).
const MAX_DETAIL_LINES: usize = 8;

// ---------------------------------------------------------------------------
// PhaseUpdate
// ---------------------------------------------------------------------------

pub enum PhaseUpdate {
    Detail(String),
    Complete(String),
    Fail(String),
}

// ---------------------------------------------------------------------------
// PhaseState
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone)]
enum PhaseState {
    Pending,
    Running,
    Done,
    Failed,
}

// ---------------------------------------------------------------------------
// Phase (internal)
// ---------------------------------------------------------------------------

struct Phase {
    label: String,
    state: PhaseState,
    summary: String,
    /// Detail lines for this phase (capped at MAX_DETAIL_LINES).
    detail: VecDeque<String>,
    /// True when some detail lines were dropped (show `...`).
    detail_truncated: bool,
}

impl Phase {
    fn new(label: String) -> Self {
        Self {
            label,
            state: PhaseState::Pending,
            summary: String::new(),
            detail: VecDeque::new(),
            detail_truncated: false,
        }
    }

    fn push_detail(&mut self, line: String) {
        if self.detail.len() >= MAX_DETAIL_LINES {
            self.detail.pop_front();
            self.detail_truncated = true;
        }
        self.detail.push_back(line);
    }

    fn complete(&mut self, summary: String) {
        self.state = PhaseState::Done;
        self.summary = summary;
        self.detail.clear();
        self.detail_truncated = false;
    }

    fn fail(&mut self, summary: String) {
        self.state = PhaseState::Failed;
        self.summary = summary;
        self.detail.clear();
        self.detail_truncated = false;
    }

    /// Render this phase as a single header line.
    fn render_header(&self, color: bool) -> String {
        let (symbol, color_code): (&str, u8) = match self.state {
            PhaseState::Pending => ("○", 37),
            PhaseState::Running => ("→", 33),
            PhaseState::Done => ("✓", 32),
            PhaseState::Failed => ("✗", 31),
        };
        let symbol_str = if color {
            format!("\x1b[{}m{}\x1b[0m", color_code, symbol)
        } else {
            symbol.to_string()
        };
        if self.summary.is_empty() {
            format!("  {} {}", symbol_str, self.label)
        } else {
            format!("  {} {}  ({})", symbol_str, self.label, self.summary)
        }
    }

    /// Render all lines for this phase: header + optional detail block.
    fn render_lines(&self, color: bool) -> Vec<String> {
        let mut out = vec![self.render_header(color)];
        if self.state == PhaseState::Running && !self.detail.is_empty() {
            if self.detail_truncated {
                out.push("      ...".to_string());
            }
            for line in &self.detail {
                out.push(format!("      {}", line));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// PhaseView
// ---------------------------------------------------------------------------

/// Build the ANSI sequence that erases `prev_lines` physical rows of the
/// progress area, or None when there is nothing to erase.
fn clear_sequence(prev_lines: usize) -> Option<String> {
    if prev_lines > 0 {
        Some(format!("\x1b[{}A\x1b[J", prev_lines))
    } else {
        None
    }
}

/// Maintains the phase-tree TTY display.
pub struct PhaseView {
    phases: Vec<Phase>,
    update_rx: mpsc::Receiver<(usize, PhaseUpdate)>,
    prev_lines: usize,
    color: bool,
    tty: bool,
    /// Once sealed, the view performs no further screen writes. Used before a
    /// command streams raw bytes (e.g. `cat` blob content) directly to stdout,
    /// so the periodic redraw cannot erase that output. See `seal`.
    sealed: bool,
}

impl PhaseView {
    pub fn new(tty: bool) -> (Self, mpsc::SyncSender<(usize, PhaseUpdate)>) {
        let (tx, rx) = mpsc::sync_channel(256);
        let color = tty && crate::term::color_enabled(crate::term::ColorChoice::Auto, true);
        let view = Self {
            phases: Vec::new(),
            update_rx: rx,
            prev_lines: 0,
            color,
            tty,
            sealed: false,
        };
        (view, tx)
    }

    /// Register a new phase in `Running` state and return its id.
    ///
    /// The phase starts as `Running` immediately, and redraws right away so
    /// it appears on screen without waiting for the next 100 ms refresh tick.
    pub fn add_phase(&mut self, label: String) -> usize {
        let id = self.phases.len();
        let mut phase = Phase::new(label);
        phase.state = PhaseState::Running;
        self.phases.push(phase);
        if self.tty && !self.sealed {
            self.redraw();
        }
        id
    }

    /// Drain pending updates from the channel and redraw if anything changed.
    pub fn refresh(&mut self) {
        // Once sealed, the view is frozen: do not drain, mutate, or redraw.
        if self.sealed {
            return;
        }
        let mut dirty = false;
        while let Ok((id, update)) = self.update_rx.try_recv() {
            if let Some(phase) = self.phases.get_mut(id) {
                match update {
                    PhaseUpdate::Detail(line) => phase.push_detail(line),
                    PhaseUpdate::Complete(s) => phase.complete(s),
                    PhaseUpdate::Fail(s) => phase.fail(s),
                }
                dirty = true;
            }
        }
        if dirty && self.tty {
            self.redraw();
        }
    }

    fn build_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for phase in &self.phases {
            out.extend(phase.render_lines(self.color));
        }
        out
    }

    fn redraw(&mut self) {
        if self.sealed {
            return;
        }
        let lines = self.build_lines();
        let mut stdout = std::io::stdout();
        if self.prev_lines > 0 {
            let _ = write!(stdout, "\x1b[{}A\x1b[J", self.prev_lines);
        }
        let term_width = terminal_width();
        let mut phys_count = 0usize;
        for line in &lines {
            let _ = writeln!(stdout, "{}", line);
            phys_count += physical_lines(line, term_width);
        }
        let _ = stdout.flush();
        self.prev_lines = phys_count;
    }

    /// Drain pending updates and finalize the progress area.
    /// On error (`errored == true`), erase the whole progress area; on success,
    /// leave the phase list on screen. Command output (deposited via
    /// `Output::finish`) is printed below the phase list by
    /// `ProgressContext::finish` when the command succeeds.
    pub fn finish(&mut self, errored: bool) {
        // If already sealed, the phase list has been drawn for the last time and
        // raw output may already be streaming below it; do nothing.
        if self.sealed {
            return;
        }
        // Drain all pending updates unconditionally (do not gate on dirty flag).
        while let Ok((id, update)) = self.update_rx.try_recv() {
            if let Some(phase) = self.phases.get_mut(id) {
                match update {
                    PhaseUpdate::Detail(line) => phase.push_detail(line),
                    PhaseUpdate::Complete(s) => phase.complete(s),
                    PhaseUpdate::Fail(s) => phase.fail(s),
                }
            }
        }
        if self.tty {
            if errored {
                // Erase the progress area so no stale rows appear above the error.
                if let Some(seq) = clear_sequence(self.prev_lines) {
                    let mut stdout = std::io::stdout();
                    let _ = write!(stdout, "{}", seq);
                    let _ = stdout.flush();
                }
            } else {
                // Always do a final redraw so the completed phase list is visible.
                self.redraw();
            }
        }
        self.prev_lines = 0;
    }

    /// Drain pending updates, perform one final redraw, then seal the view.
    ///
    /// After sealing, every other method becomes a no-op so nothing touches the
    /// screen again. This is used right before a command streams raw bytes
    /// (e.g. `cat` blob content) directly to stdout: the completed phase list
    /// stays on screen and the raw output appears below it, with no risk of the
    /// periodic redraw erasing it. `prev_lines` is reset to 0 so any later
    /// (no-op) call would not emit cursor-up escapes.
    pub fn seal(&mut self) {
        if self.sealed {
            return;
        }
        while let Ok((id, update)) = self.update_rx.try_recv() {
            if let Some(phase) = self.phases.get_mut(id) {
                match update {
                    PhaseUpdate::Detail(line) => phase.push_detail(line),
                    PhaseUpdate::Complete(s) => phase.complete(s),
                    PhaseUpdate::Fail(s) => phase.fail(s),
                }
            }
        }
        if self.tty {
            self.redraw();
        }
        self.prev_lines = 0;
        self.sealed = true;
    }

    /// Clear the progress area, print one line inline, then mark dirty for
    /// the next refresh.
    pub fn emit_inline(&mut self, line: &str) {
        if !self.tty || self.sealed {
            return;
        }
        let mut stdout = std::io::stdout();
        if self.prev_lines > 0 {
            let _ = write!(stdout, "\x1b[{}A\x1b[J", self.prev_lines);
            self.prev_lines = 0;
        }
        let _ = writeln!(stdout, "{}", line);
        let _ = stdout.flush();
        // Redraw immediately so the phase list reappears below the line.
        self.redraw();
    }
}

// ---------------------------------------------------------------------------
// PhaseHandle
// ---------------------------------------------------------------------------

/// A handle to a single registered phase.
///
/// Obtained from `progress::begin_phase`.  Safe to clone and move to other
/// threads.  Updates are sent over an `mpsc` channel and applied by the
/// `RefreshThread` on its 100 ms interval.
#[derive(Clone)]
pub struct PhaseHandle {
    id: usize,
    tx: Option<mpsc::SyncSender<(usize, PhaseUpdate)>>,
    /// Shared "currently active phase" slot owned by the `ProgressContext`.
    /// Cleared (set to `None`) when this handle finishes its phase via
    /// `complete`/`fail`/`Drop`, but only if it still points at this phase id,
    /// so a newer phase that has since started is not clobbered.
    active_phase_id: Option<Arc<Mutex<Option<usize>>>>,
}

impl PhaseHandle {
    pub(crate) fn new(
        id: usize,
        tx: mpsc::SyncSender<(usize, PhaseUpdate)>,
        active_phase_id: Arc<Mutex<Option<usize>>>,
    ) -> Self {
        Self {
            id,
            tx: Some(tx),
            active_phase_id: Some(active_phase_id),
        }
    }

    /// No-op handle used when TTY is disabled.
    pub(crate) fn noop() -> Self {
        Self {
            id: 0,
            tx: None,
            active_phase_id: None,
        }
    }

    /// Clear the shared active-phase slot if it still references this phase.
    fn clear_active(&self) {
        if let Some(slot) = &self.active_phase_id
            && let Ok(mut guard) = slot.lock()
            && *guard == Some(self.id)
        {
            *guard = None;
        }
    }

    /// Append a detail line.  Redrawn on the next 100 ms refresh tick.
    ///
    /// Uses `try_send` (best-effort): if the channel is full, the update is
    /// silently dropped. The log file always receives every event regardless.
    pub fn detail(&self, line: impl Into<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send((self.id, PhaseUpdate::Detail(line.into())));
        }
    }

    /// Mark the phase as completed with a summary line.
    pub fn complete(self, summary: impl Into<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((self.id, PhaseUpdate::Complete(summary.into())));
        }
        self.clear_active();
    }

    /// Mark the phase as failed with a summary line.
    pub fn fail(self, summary: impl Into<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((self.id, PhaseUpdate::Fail(summary.into())));
        }
        self.clear_active();
    }
}

impl Drop for PhaseHandle {
    /// Dropping a handle without calling `complete`/`fail` still clears the
    /// shared active-phase slot (if it points at this phase), so later detail
    /// lines are not attributed to a finished phase. The phase row itself stays
    /// in the `Running` state per design/06 until the display is torn down.
    fn drop(&mut self) {
        self.clear_active();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_len_strips_ansi_escapes() {
        assert_eq!(visible_len("hello"), 5);
        assert_eq!(visible_len("\x1b[32mhello\x1b[0m"), 5);
        assert_eq!(visible_len("\x1b[1;33mABC\x1b[0m"), 3);
        assert_eq!(visible_len(""), 0);
    }

    #[test]
    fn visible_len_counts_wide_chars_as_two_cells() {
        // CJK ideographs are East Asian Wide → 2 cells each.
        assert_eq!(visible_len("日本語"), 6);
        // Mixed ASCII + CJK in a file path.
        assert_eq!(visible_len("src/設計.rs"), 4 + 4 + 3);
        // Wide chars survive ANSI stripping.
        assert_eq!(visible_len("\x1b[32m日本\x1b[0m"), 4);
    }

    #[test]
    fn visible_len_status_symbol_widths_match_unicode_width_cjk() {
        // Phase status symbols, measured with width_cjk (the values the redraw
        // logic relies on). Per Unicode EAW data in unicode-width 0.2:
        //   ○ U+25CB, → U+2192  → Ambiguous → 2 cells under width_cjk
        //   ✓ U+2713, ✗ U+2717  → Neutral   → 1 cell
        // We assert the library's actual values rather than an assumed uniform
        // width, so this stays the single source of truth for wrap prediction.
        assert_eq!(visible_len("○"), 2);
        assert_eq!(visible_len("→"), 2);
        assert_eq!(visible_len("✓"), 1);
        assert_eq!(visible_len("✗"), 1);
    }

    #[test]
    fn physical_lines_counts_wraps() {
        assert_eq!(physical_lines("hello", 80), 1);
        // 80 cells → exactly 1 physical line
        let eighty = "a".repeat(80);
        assert_eq!(physical_lines(&eighty, 80), 1);
        // 81 cells → wraps to 2 physical lines
        let eighty_one = "a".repeat(81);
        assert_eq!(physical_lines(&eighty_one, 80), 2);
        // empty string → 1 physical line (for the newline itself)
        assert_eq!(physical_lines("", 80), 1);
    }

    #[test]
    fn physical_lines_counts_wraps_with_wide_chars() {
        // 40 CJK chars = 80 cells → exactly 1 physical line at width 80.
        let forty_wide = "あ".repeat(40);
        assert_eq!(physical_lines(&forty_wide, 80), 1);
        // 41 CJK chars = 82 cells → wraps to 2 physical lines.
        // (A char-counting implementation would wrongly report 1 row here.)
        let forty_one_wide = "あ".repeat(41);
        assert_eq!(physical_lines(&forty_one_wide, 80), 2);
    }

    #[test]
    fn new_phase_is_running() {
        let (mut view, _tx) = PhaseView::new(false);
        let id = view.add_phase("Scan files".to_string());
        assert_eq!(view.phases[id].state, PhaseState::Running);
    }

    #[test]
    fn seal_drains_pending_updates() {
        // seal() must apply any queued updates before sealing so the final
        // on-screen state reflects the last completion.
        let (mut view, tx) = PhaseView::new(false);
        let id = view.add_phase("Cat".to_string());
        tx.send((id, PhaseUpdate::Complete("blob".to_string())))
            .unwrap();
        view.seal();
        assert_eq!(view.phases[id].state, PhaseState::Done);
        assert_eq!(view.phases[id].summary, "blob");
    }

    #[test]
    fn seal_is_idempotent_and_resets_prev_lines() {
        let (mut view, _tx) = PhaseView::new(false);
        view.add_phase("Cat".to_string());
        view.seal();
        assert!(view.sealed);
        assert_eq!(view.prev_lines, 0);
        // Calling seal again must not panic or change state.
        view.seal();
        assert!(view.sealed);
        assert_eq!(view.prev_lines, 0);
    }

    #[test]
    fn updates_after_seal_are_not_applied_to_screen_state() {
        // After sealing, draining is no longer performed, so a late update is
        // left unconsumed on the channel rather than mutating a finished phase.
        let (mut view, tx) = PhaseView::new(false);
        let id = view.add_phase("Cat".to_string());
        view.seal();
        tx.send((id, PhaseUpdate::Complete("late".to_string())))
            .unwrap();
        view.refresh(); // no-op while sealed: must not drain or apply
        assert_eq!(view.phases[id].state, PhaseState::Running);
        assert_eq!(view.phases[id].summary, "");
    }

    #[test]
    fn finish_after_seal_is_noop() {
        // finish() must not redraw or touch prev_lines once sealed.
        let (mut view, _tx) = PhaseView::new(false);
        view.add_phase("Cat".to_string());
        view.seal();
        view.finish(false);
        assert!(view.sealed);
        assert_eq!(view.prev_lines, 0);
    }

    #[test]
    fn detail_lines_capped_at_max() {
        let mut phase = Phase::new("Upload".to_string());
        phase.state = PhaseState::Running;
        for i in 0..(MAX_DETAIL_LINES + 3) {
            phase.push_detail(format!("line {}", i));
        }
        assert_eq!(phase.detail.len(), MAX_DETAIL_LINES);
        assert!(phase.detail_truncated);
        // Oldest lines dropped; last line is the most recently pushed.
        assert_eq!(
            phase.detail.back().unwrap(),
            &format!("line {}", MAX_DETAIL_LINES + 2)
        );
    }

    #[test]
    fn detail_truncated_false_when_under_limit() {
        let mut phase = Phase::new("Upload".to_string());
        phase.state = PhaseState::Running;
        for i in 0..MAX_DETAIL_LINES {
            phase.push_detail(format!("line {}", i));
        }
        assert!(!phase.detail_truncated);
    }

    #[test]
    fn complete_clears_detail() {
        let mut phase = Phase::new("Upload".to_string());
        phase.state = PhaseState::Running;
        phase.push_detail("line 1".to_string());
        phase.complete("ok".to_string());
        assert_eq!(phase.state, PhaseState::Done);
        assert!(phase.detail.is_empty());
        assert!(!phase.detail_truncated);
    }

    #[test]
    fn fail_clears_detail() {
        let mut phase = Phase::new("Upload".to_string());
        phase.state = PhaseState::Running;
        phase.push_detail("line 1".to_string());
        phase.fail("error".to_string());
        assert_eq!(phase.state, PhaseState::Failed);
        assert!(phase.detail.is_empty());
    }

    #[test]
    fn render_pending_has_circle_symbol() {
        let phase = Phase::new("Test phase".to_string());
        let line = phase.render_header(false);
        assert!(line.contains("○"), "expected ○ in: {}", line);
        assert!(line.contains("Test phase"));
    }

    #[test]
    fn render_done_shows_summary() {
        let mut phase = Phase::new("Test phase".to_string());
        phase.complete("42 files".to_string());
        let line = phase.render_header(false);
        assert!(line.contains("✓"), "expected ✓ in: {}", line);
        assert!(line.contains("42 files"), "expected summary in: {}", line);
    }

    #[test]
    fn render_running_with_detail_includes_ellipsis_when_truncated() {
        let mut phase = Phase::new("Upload".to_string());
        phase.state = PhaseState::Running;
        for i in 0..(MAX_DETAIL_LINES + 1) {
            phase.push_detail(format!("line {}", i));
        }
        let lines = phase.render_lines(false);
        // Header + "..." + MAX_DETAIL_LINES detail lines.
        assert_eq!(lines.len(), 1 + 1 + MAX_DETAIL_LINES);
        assert!(lines[1].contains("..."));
    }

    #[test]
    fn render_running_no_ellipsis_when_not_truncated() {
        let mut phase = Phase::new("Upload".to_string());
        phase.state = PhaseState::Running;
        phase.push_detail("line 1".to_string());
        phase.push_detail("line 2".to_string());
        let lines = phase.render_lines(false);
        assert_eq!(lines.len(), 3); // header + 2 detail lines, no "..."
        assert!(!lines.iter().any(|l| l.contains("...")));
    }

    #[test]
    fn completed_phase_renders_one_line_only() {
        let mut phase = Phase::new("Scan".to_string());
        phase.state = PhaseState::Running;
        phase.push_detail("scanning...".to_string());
        phase.complete("234 files".to_string());
        let lines = phase.render_lines(false);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn phase_handle_via_channel() {
        let (mut view, tx) = PhaseView::new(false);
        let id = view.add_phase("Upload".to_string());
        view.phases[id].state = PhaseState::Running;

        let slot = Arc::new(Mutex::new(Some(id)));
        let handle = PhaseHandle::new(id, tx, slot);
        handle.detail("uploading blob a1b2");
        handle.complete("8 blobs");

        // Drain the channel manually.
        while let Ok((rid, update)) = view.update_rx.try_recv() {
            match update {
                PhaseUpdate::Detail(line) => view.phases[rid].push_detail(line),
                PhaseUpdate::Complete(s) => view.phases[rid].complete(s),
                PhaseUpdate::Fail(s) => view.phases[rid].fail(s),
            }
        }

        assert_eq!(view.phases[id].state, PhaseState::Done);
        assert_eq!(view.phases[id].summary, "8 blobs");
        assert!(view.phases[id].detail.is_empty());
    }

    #[test]
    fn complete_clears_active_phase_id() {
        let (_view, tx) = PhaseView::new(false);
        let id = 3usize;
        let slot = Arc::new(Mutex::new(Some(id)));
        let handle = PhaseHandle::new(id, tx, Arc::clone(&slot));
        handle.complete("done");
        // After completion the active-phase slot must be cleared so later log
        // lines are not attributed to a finished phase.
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[test]
    fn fail_clears_active_phase_id() {
        let (_view, tx) = PhaseView::new(false);
        let id = 1usize;
        let slot = Arc::new(Mutex::new(Some(id)));
        let handle = PhaseHandle::new(id, tx, Arc::clone(&slot));
        handle.fail("boom");
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[test]
    fn drop_clears_active_phase_id() {
        let (_view, tx) = PhaseView::new(false);
        let id = 2usize;
        let slot = Arc::new(Mutex::new(Some(id)));
        {
            let _handle = PhaseHandle::new(id, tx, Arc::clone(&slot));
            // Drop without complete/fail.
        }
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[test]
    fn complete_does_not_clear_newer_active_phase() {
        // If a newer phase has started (slot points elsewhere), completing an
        // older phase must not clobber the newer phase's active id.
        let (_view, tx) = PhaseView::new(false);
        let old_id = 0usize;
        let new_id = 1usize;
        let slot = Arc::new(Mutex::new(Some(new_id)));
        let old_handle = PhaseHandle::new(old_id, tx, Arc::clone(&slot));
        old_handle.complete("old done");
        assert_eq!(*slot.lock().unwrap(), Some(new_id));
    }

    #[test]
    fn noop_handle_does_not_panic() {
        let handle = PhaseHandle::noop();
        handle.detail("should be dropped silently");
        let handle2 = PhaseHandle::noop();
        handle2.complete("ok");
        let handle3 = PhaseHandle::noop();
        handle3.fail("err");
    }

    // detail() must never block even when the channel is full.
    // With the old blocking send, calling detail() ~10_000 times without a
    // receiver draining the channel would deadlock this test.
    #[test]
    fn detail_does_not_block_when_channel_full() {
        // Channel capacity is 256 per PhaseView::new. Intentionally do not drain.
        let (_view, tx) = PhaseView::new(false);
        let id = 0;
        let handle = PhaseHandle::new(id, tx, Arc::new(Mutex::new(Some(id))));
        for i in 0..10_000 {
            handle.detail(format!("line {}", i)); // must return immediately
        }
        // If we reach here, detail() did not block.
    }

    // complete() must still be delivered even after many detail() drops.
    // The channel consumer (RefreshThread) drains the channel in production;
    // here we simulate that by draining before calling complete().
    #[test]
    fn complete_delivered_after_full_channel() {
        let (mut view, tx) = PhaseView::new(false);
        let id = view.add_phase("Phase".to_string());
        view.phases[id].state = PhaseState::Running;

        let handle = PhaseHandle::new(id, tx, Arc::new(Mutex::new(Some(id))));
        // Flood with detail lines — most are dropped because channel is full.
        for i in 0..10_000 {
            handle.detail(format!("line {}", i));
        }
        // Simulate what RefreshThread does: drain the channel before complete().
        while view.update_rx.try_recv().is_ok() {}
        // Now complete() has room and must be delivered.
        handle.complete("done");

        // Drain again to apply the Complete update.
        while let Ok((rid, update)) = view.update_rx.try_recv() {
            if let Some(phase) = view.phases.get_mut(rid) {
                match update {
                    PhaseUpdate::Detail(line) => phase.push_detail(line),
                    PhaseUpdate::Complete(s) => phase.complete(s),
                    PhaseUpdate::Fail(s) => phase.fail(s),
                }
            }
        }

        assert_eq!(view.phases[id].state, PhaseState::Done);
        assert_eq!(view.phases[id].summary, "done");
    }

    #[test]
    fn clear_sequence_returns_none_when_no_lines() {
        assert_eq!(clear_sequence(0), None);
    }

    #[test]
    fn clear_sequence_returns_ansi_escape_when_lines_present() {
        assert_eq!(clear_sequence(5), Some("\x1b[5A\x1b[J".to_string()));
        assert_eq!(clear_sequence(1), Some("\x1b[1A\x1b[J".to_string()));
        assert_eq!(clear_sequence(100), Some("\x1b[100A\x1b[J".to_string()));
    }
}
