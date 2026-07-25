//! `Output` wraps stdout with a color decision and a TTY-mode buffer.
//!
//! When stdout is a TTY, writes accumulate in an in-memory buffer and are
//! deposited into the active `ProgressContext` at `finish()` time, so that
//! command output appears *after* all progress lines have been cleared.
//! When stdout is a pipe or file, writes go directly to stdout without color.

use std::io::{self, Stdout, Write};

use crate::term::color::{ColorChoice, Styles, color_enabled};

pub struct Output {
    sink: Sink,
    pub styles: Styles,
    pub colored: bool,
}

enum Sink {
    /// stdout is a TTY: buffer everything and deposit at `finish()`.
    Buffered(Vec<u8>),
    /// stdout is not a TTY: write directly.
    Direct(Stdout),
}

impl Output {
    /// Build an `Output` for stdout. Enables color when stdout is a TTY.
    /// In TTY mode, writes are buffered until `finish()` so that the progress
    /// view is cleared before command results appear.
    pub fn for_stdout() -> Self {
        let is_tty = atty::is(atty::Stream::Stdout);
        let colored = color_enabled(ColorChoice::Auto, is_tty);
        let sink = if is_tty {
            Sink::Buffered(Vec::new())
        } else {
            Sink::Direct(io::stdout())
        };
        Self {
            sink,
            styles: Styles::new(),
            colored,
        }
    }

    /// Build a non-colored, non-buffered output — used for raw binary payloads
    /// that must stream directly to stdout (e.g. `cat` blob content).
    pub fn raw_stdout() -> Self {
        Self {
            sink: Sink::Direct(io::stdout()),
            styles: Styles::new(),
            colored: false,
        }
    }

    pub fn colored(&self) -> bool {
        self.colored
    }

    pub fn writeln(&mut self, line: &str) -> io::Result<()> {
        let w = self.writer_impl();
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer_impl().write_all(bytes)
    }

    /// Expose the underlying writer for streaming large payloads.
    pub fn writer(&mut self) -> &mut dyn Write {
        self.writer_impl()
    }

    /// Apply `style` to `text` if color is enabled.
    #[allow(dead_code)]
    pub fn paint(&self, style: anstyle::Style, text: &str) -> String {
        if self.colored {
            format!("{}{}{}", style.render(), text, style.render_reset())
        } else {
            text.to_string()
        }
    }

    /// Finish the output session.
    ///
    /// In TTY (buffered) mode, deposits the accumulated buffer into the active
    /// `ProgressContext` so it is flushed *after* all pending `DebugTimer`
    /// drops have fired.  If no context is active the buffer is written directly.
    pub fn finish(self) -> io::Result<()> {
        match self.sink {
            Sink::Buffered(buf) => {
                if let Some(buf) = crate::progress::deposit_output(buf) {
                    let mut stdout = io::stdout();
                    stdout.write_all(&buf)?;
                    stdout.flush()?;
                }
                Ok(())
            }
            Sink::Direct(mut s) => s.flush(),
        }
    }

    fn writer_impl(&mut self) -> &mut dyn Write {
        match &mut self.sink {
            Sink::Buffered(v) => v,
            Sink::Direct(s) => s,
        }
    }
}

/// Apply `style` to `text` if `colored` is true, otherwise return text unchanged.
pub fn paint(colored: bool, style: anstyle::Style, text: &str) -> String {
    if colored {
        format!("{}{}{}", style.render(), text, style.render_reset())
    } else {
        text.to_string()
    }
}
