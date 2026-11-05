//! Terminal output shared by all messages: colors and formatting.

use std::fmt::Display;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use crate::audio::SAMPLE_RATE;

/// stderr is an interactive terminal (live status line, line clearing).
pub static TTY: LazyLock<bool> = LazyLock::new(|| std::io::stderr().is_terminal());
/// Colors only on a terminal, and never with `NO_COLOR` set.
pub static COLOR: LazyLock<bool> = LazyLock::new(|| *TTY && std::env::var_os("NO_COLOR").is_none());

/// Wrap `msg` in an SGR color code (e.g. "31" red, "32" green, "33" yellow, "1" bold).
pub fn paint(code: &str, msg: &str) -> String {
    if *COLOR {
        format!("\x1b[{code}m{msg}\x1b[0m")
    } else {
        msg.to_string()
    }
}

pub fn warning(msg: impl Display) {
    eprintln!("{} {msg}", paint("33", "warning:"));
}

pub fn error(msg: impl Display) {
    eprintln!("{} {msg}", paint("31", "error:"));
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Duration of `frames` at the pipeline rate.
pub fn fmt_frames(frames: u64) -> String {
    fmt_duration(Duration::from_secs_f64(frames as f64 / SAMPLE_RATE))
}

/// `  <path>  (<size><note>)`
pub fn print_file(path: &Path, note: &str) {
    let size = std::fs::metadata(path).map_or(0, |m| m.len()) as f64;
    let size = if size >= 1024.0 * 1024.0 {
        format!("{:.1} MB", size / 1024.0 / 1024.0)
    } else {
        format!("{:.1} KB", size / 1024.0)
    };
    eprintln!("  {}  ({size}{note})", path.display());
}
