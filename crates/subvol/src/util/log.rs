use core::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

pub const LOG_OFF: u8 = 0;
pub const LOG_ERROR: u8 = 1;
pub const LOG_WARN: u8 = 2;
pub const LOG_INFO: u8 = 3;
pub const LOG_DEBUG: u8 = 4;

static LOG_LEVEL: AtomicU8 = AtomicU8::new(LOG_OFF);

pub fn set_level(level: u8) {
    LOG_LEVEL.store(level.min(LOG_DEBUG), Ordering::Release);
}

pub fn level() -> u8 {
    LOG_LEVEL.load(Ordering::Acquire)
}

pub fn enabled(required: u8) -> bool {
    required != LOG_OFF && level() >= required
}

pub fn init_from_env() {
    let value = std::env::var("SUBVOL_REWRITE_LOG")
        .ok()
        .or_else(|| std::env::var("SUBVOL_LOG").ok());
    let level = match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("1") | Some("true") | Some("yes") | Some("info") => LOG_INFO,
        Some("error") => LOG_ERROR,
        Some("warn") | Some("warning") => LOG_WARN,
        Some("debug") | Some("verbose") => LOG_DEBUG,
        _ => LOG_OFF,
    };
    set_level(level);
}

pub fn emit(level: u8, label: &str, args: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    eprintln!("[subvol][{label}] {args}");
}

#[macro_export]
macro_rules! rewrite_log_error {
    ($($arg:tt)*) => {
        $crate::util::log::emit($crate::util::log::LOG_ERROR, "ERROR", format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! rewrite_log_warn {
    ($($arg:tt)*) => {
        $crate::util::log::emit($crate::util::log::LOG_WARN, "WARN", format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! rewrite_log_info {
    ($($arg:tt)*) => {
        $crate::util::log::emit($crate::util::log::LOG_INFO, "INFO", format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! rewrite_log_debug {
    ($($arg:tt)*) => {
        $crate::util::log::emit($crate::util::log::LOG_DEBUG, "DEBUG", format_args!($($arg)*));
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_can_be_disabled_and_enabled() {
        set_level(LOG_OFF);
        assert!(!enabled(LOG_ERROR));
        set_level(LOG_DEBUG);
        assert!(enabled(LOG_ERROR));
        assert!(enabled(LOG_DEBUG));
        set_level(LOG_OFF);
    }
}
