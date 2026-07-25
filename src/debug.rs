//! Debug instrumentation for performance investigation.
//!
//! Provides per-layer `dlog_lN!` macros and `DebugTimer` (with `dtimer_lN!`
//! helpers) for RAII-based scope timing.
//!
//! All output is routed through the active `ProgressContext` (see
//! `src/progress/mod.rs`): log lines are written to `.omemfs/logs/` and,
//! when stdout is a TTY, the terminal hierarchy view is updated.
//!
//! See `design/06_progress_view.md` for the full design.

use std::time::{Duration, Instant};

/// Architectural layer the log line originated from.
///
/// Seven layers correspond to the seven pipeline stages:
///   L1 cmd  — command logic, working-tree scan, tree_ops
///   L2 ser  — serialize / deserialize (object.rs)
///   L3 chk  — chunk / assemble (codec/chunk.rs)
///   L4 cmp  — compress / decompress (codec/compress.rs)
///   L5 enc  — encrypt / decrypt (codec/encrypt.rs)
///   L6 pak  — pack routing + index (codec/pack/)
///   L7 sto  — store I/O (store/)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Layer {
    L1,
    L2,
    L3,
    L4,
    L5,
    L6,
    L7,
}

impl Layer {
    /// 7-character fixed-width tag rendered inside the log prefix.
    pub fn tag(self) -> &'static str {
        match self {
            Layer::L1 => "L1 cmd ",
            Layer::L2 => "L2 ser ",
            Layer::L3 => "L3 chk ",
            Layer::L4 => "L4 cmp ",
            Layer::L5 => "L5 enc ",
            Layer::L6 => "L6 pak ",
            Layer::L7 => "L7 sto ",
        }
    }

    pub fn idx(self) -> usize {
        match self {
            Layer::L1 => 0,
            Layer::L2 => 1,
            Layer::L3 => 2,
            Layer::L4 => 3,
            Layer::L5 => 4,
            Layer::L6 => 5,
            Layer::L7 => 6,
        }
    }

    /// ANSI SGR color code for this layer's prefix.
    pub fn color_code(self) -> u8 {
        match self {
            Layer::L1 => 32, // green
            Layer::L2 => 36, // cyan
            Layer::L3 => 33, // yellow
            Layer::L4 => 35, // magenta
            Layer::L5 => 31, // red
            Layer::L6 => 34, // blue
            Layer::L7 => 37, // white
        }
    }
}

// ---------------------------------------------------------------------------
// Internal emit helpers (called by the macros and DebugTimer)
// ---------------------------------------------------------------------------

/// Emit a single debug line for `layer` at the current `DEPTH`.
pub fn _emit_layer(layer: Layer, depth: usize, text: String) {
    crate::progress::emit(depth, layer, &text);
}

// ---------------------------------------------------------------------------
// Thread-local depth counter (for DebugTimer nesting)
// ---------------------------------------------------------------------------

use std::cell::Cell;

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

pub fn depth() -> usize {
    DEPTH.with(|d| d.get())
}

fn push_depth() {
    DEPTH.with(|d| d.set(d.get() + 1));
}

fn pop_depth() {
    DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
}

fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{:.3}s", secs)
    } else if d.as_micros() >= 1000 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{}us", d.as_micros())
    }
}

// ---------------------------------------------------------------------------
// DebugTimer
// ---------------------------------------------------------------------------

pub struct DebugTimer {
    layer: Layer,
    label: String,
    start: Instant,
    depth: usize,
}

impl DebugTimer {
    pub fn new_with_layer(layer: Layer, label: String) -> Self {
        let d = depth();
        _emit_layer(layer, d, format!("{}: start", label));
        push_depth();
        Self {
            layer,
            label,
            start: Instant::now(),
            depth: d,
        }
    }
}

impl Drop for DebugTimer {
    fn drop(&mut self) {
        pop_depth();
        let elapsed = self.start.elapsed();
        _emit_layer(
            self.layer,
            self.depth,
            format!("{}: end ({})", self.label, fmt_elapsed(elapsed)),
        );
    }
}

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

/// Emit a Layer 1 (cmd) debug message.
#[macro_export]
macro_rules! dlog_l1 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L1,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 1 (cmd) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l1 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L1,
            format!($($arg)*),
        )
    };
}

/// Emit a Layer 2 (serialize) debug message.
#[macro_export]
macro_rules! dlog_l2 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L2,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 2 (serialize) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l2 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L2,
            format!($($arg)*),
        )
    };
}

/// Emit a Layer 3 (chunk) debug message.
#[macro_export]
macro_rules! dlog_l3 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L3,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 3 (chunk) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l3 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L3,
            format!($($arg)*),
        )
    };
}

/// Emit a Layer 4 (compress) debug message.
#[macro_export]
macro_rules! dlog_l4 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L4,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 4 (compress) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l4 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L4,
            format!($($arg)*),
        )
    };
}

/// Emit a Layer 5 (encrypt) debug message.
#[macro_export]
macro_rules! dlog_l5 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L5,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 5 (encrypt) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l5 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L5,
            format!($($arg)*),
        )
    };
}

/// Emit a Layer 6 (pack) debug message.
#[macro_export]
macro_rules! dlog_l6 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L6,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 6 (pack) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l6 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L6,
            format!($($arg)*),
        )
    };
}

/// Emit a Layer 7 (store) debug message.
#[macro_export]
macro_rules! dlog_l7 {
    ($($arg:tt)*) => {
        $crate::debug::_emit_layer(
            $crate::debug::Layer::L7,
            $crate::debug::depth(),
            format!($($arg)*),
        );
    };
}

/// Construct a Layer 7 (store) `DebugTimer`.
#[macro_export]
macro_rules! dtimer_l7 {
    ($($arg:tt)*) => {
        $crate::debug::DebugTimer::new_with_layer(
            $crate::debug::Layer::L7,
            format!($($arg)*),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_tag_widths_match() {
        assert_eq!(Layer::L1.tag().len(), 7);
        assert_eq!(Layer::L2.tag().len(), 7);
        assert_eq!(Layer::L3.tag().len(), 7);
        assert_eq!(Layer::L4.tag().len(), 7);
        assert_eq!(Layer::L5.tag().len(), 7);
        assert_eq!(Layer::L6.tag().len(), 7);
        assert_eq!(Layer::L7.tag().len(), 7);
    }

    #[test]
    fn layer_idx_unique() {
        let layers = [
            Layer::L1,
            Layer::L2,
            Layer::L3,
            Layer::L4,
            Layer::L5,
            Layer::L6,
            Layer::L7,
        ];
        for i in 0..layers.len() {
            for j in (i + 1)..layers.len() {
                assert_ne!(layers[i].idx(), layers[j].idx());
            }
        }
    }
}
