//! Color decisions and reusable `anstyle` styles for omemfs output.
//!
//! Rules (checked in order):
//! 1. If `NO_COLOR` is set (any value), disable color.
//! 2. If `CLICOLOR_FORCE=1` is set, force color on.
//! 3. Otherwise color is enabled only when the target stream is a TTY.

use anstyle::{Ansi256Color, Color, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ColorChoice {
    Always,
    Never,
    Auto,
}

pub fn color_enabled(choice: ColorChoice, is_tty: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("CLICOLOR_FORCE").as_deref(), Ok("1")) {
        return true;
    }
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => is_tty,
    }
}

/// Palette shared by all omemfs commands.
#[derive(Debug, Clone, Copy)]
pub struct Styles {
    pub added: Style,
    pub modified: Style,
    pub deleted: Style,
    pub hash: Style,
    pub directory: Style,
    pub meta: Style,
    /// Size digit grades: teal ramp for B/K/M, magenta for G/T.
    /// Index 0 = 1-3 digits (B), ..., index 4 = 13+ (T, clamped).
    pub size_digit_grades: [Style; 5],
    /// mtime stages:
    ///   just_now / min_ago: vivid sky blue (75)
    ///   recent (today, same-day ≥ 60 min): split — date part pale steel (110), time part sky (75)
    ///   older (≤ 14 days excl. today): pale steel blue (110)
    ///   oldest (> 14 days): grey (242)
    pub mtime_just_now: Style,
    pub mtime_min_ago: Style,
    pub mtime_today_date: Style,
    pub mtime_today_time: Style,
    pub mtime_recent: Style,
    pub mtime_older: Style,
    /// Conflict flag `!`: orange.
    pub conflict: Style,
    /// Stub flag `S`/`s`: cyan.
    pub stub: Style,
}

impl Styles {
    pub const fn new() -> Self {
        Self {
            // 114  #87D787  green
            added: Style::new()
                .fg_color(Some(Color::Ansi256(Ansi256Color(114))))
                .bold(),
            // 179  #D7AF5F  amber
            modified: Style::new()
                .fg_color(Some(Color::Ansi256(Ansi256Color(179))))
                .bold(),
            // 167  #D75F5F  red
            deleted: Style::new()
                .fg_color(Some(Color::Ansi256(Ansi256Color(167))))
                .bold(),
            // 242  #6C6C6C  grey
            hash: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(242)))),
            //  75  #5FAFFF  sky blue
            directory: Style::new()
                .fg_color(Some(Color::Ansi256(Ansi256Color(75))))
                .bold(),
            // 242  #6C6C6C  grey
            meta: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(242)))),
            // Teal ramp B→K→M, then magenta G→T.
            //   23  #005F5F  B  (dark teal)
            //   30  #008787  K  (mid teal)
            //   43  #00D7AF  M  (teal-green)
            //  135  #AF5FFF  G  (light magenta-violet)
            //  201  #FF00FF  T  (pure magenta)
            size_digit_grades: [
                Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(23)))),
                Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(30)))),
                Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(43)))),
                Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(135)))),
                Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(201)))),
            ],
            //  75  #5FAFFF  sky blue
            mtime_just_now: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(75)))),
            mtime_min_ago: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(75)))),
            // 110  #87AFD7  steel blue (date part of today stage)
            mtime_today_date: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(110)))),
            //  75  #5FAFFF  sky blue (time part of today stage)
            mtime_today_time: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(75)))),
            // 110  #87AFD7  steel blue (≤ 14 days excl. today)
            mtime_recent: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(110)))),
            // 242  #6C6C6C  grey (> 14 days)
            mtime_older: Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(242)))),
            // 208  #FF8700  orange
            conflict: Style::new()
                .fg_color(Some(Color::Ansi256(Ansi256Color(208))))
                .bold(),
            //  37  #00AFAF  cyan
            stub: Style::new()
                .fg_color(Some(Color::Ansi256(Ansi256Color(37))))
                .bold(),
        }
    }
}

impl Default for Styles {
    fn default() -> Self {
        Self::new()
    }
}
