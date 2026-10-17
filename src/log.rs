//! Dependency-free process logging.
//!
//! One environment variable and one CLI flag control the level; every record
//! goes to stderr as `[   12.345s] level target: message`. Normal playback is
//! silent at the default `warn` level, `WALLBREAK_LOG=info|debug|trace` (or
//! `-v`/`-vv`/`-vvv` on the CLI) adds progressively more detail for users and
//! developers. Never log credentials or ambient environment values.

use std::{
    fmt,
    sync::{
        atomic::{AtomicU8, Ordering},
        OnceLock,
    },
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Warn as u8);
static START: OnceLock<Instant> = OnceLock::new();

/// Apply `WALLBREAK_LOG` if it names a valid level; ignore absent or invalid
/// values so a typo cannot break playback. Returns the applied level.
pub fn init_from_env() -> Level {
    let level = std::env::var("WALLBREAK_LOG")
        .ok()
        .and_then(|value| Level::parse(&value))
        .unwrap_or(Level::Warn);
    set_level(level);
    level
}

pub fn set_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
    // Anchor the recorded timeline at logging setup, not at the first record.
    let _ = START.get_or_init(Instant::now);
}

pub fn level() -> Level {
    match MAX_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

pub fn enabled(level: Level) -> bool {
    level <= self::level()
}

fn elapsed() -> f32 {
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

pub fn record(level: Level, target: &str, message: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    eprintln!(
        "[{:10.3}s] {:5} {target}: {message}",
        elapsed(),
        level.label()
    );
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Error, module_path!(), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Warn, module_path!(), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Info, module_path!(), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Debug, module_path!(), format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Trace, module_path!(), format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_level_names_case_insensitively() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse(" warn "), Some(Level::Warn));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("loud"), None);
    }

    #[test]
    fn ordering_lets_higher_levels_include_lower_ones() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
        set_level(Level::Info);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Info));
        assert!(!enabled(Level::Debug));
        set_level(Level::Warn);
    }
}
