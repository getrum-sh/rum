//! Lightweight, dependency-free terminal styling and uv-style progress presentation.
//!
//! Automatically honors `NO_COLOR` and non-interactive TTYs (CI/piped stdout).

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static FORCE_NO_COLOR: AtomicBool = AtomicBool::new(false);

pub fn init_color() {
    let no_color = std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal();
    FORCE_NO_COLOR.store(no_color, Ordering::Relaxed);
}

pub fn is_color_enabled() -> bool {
    !FORCE_NO_COLOR.load(Ordering::Relaxed)
}

pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn green(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[32m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold_green(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[1;32m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn cyan(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[36m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold_cyan(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[1;36m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn yellow(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[33m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn red(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[31m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold_red(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[1;31m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn dim(text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[2m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn progress_bar(done_bytes: u64, total_bytes: u64, width: usize) -> String {
    if total_bytes == 0 {
        return "━".repeat(width);
    }
    let ratio = (done_bytes as f64 / total_bytes as f64).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let unfilled = width.saturating_sub(filled);

    if is_color_enabled() {
        format!(
            "\x1b[36m{}\x1b[2m{}\x1b[0m",
            "━".repeat(filled),
            "━".repeat(unfilled)
        )
    } else {
        format!("{}{}", "=".repeat(filled), "-".repeat(unfilled))
    }
}
