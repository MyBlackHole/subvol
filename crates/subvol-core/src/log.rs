use std::sync::atomic::{AtomicBool, Ordering};

static LOG_ENABLED: AtomicBool = AtomicBool::new(false);

/// 开启/关闭日志输出（仅影响 info/verbose 级别，error/warn 不受影响）
pub fn set_enabled(enabled: bool) {
    LOG_ENABLED.store(enabled, Ordering::Release);
}

/// 查询日志是否开启
pub fn is_enabled() -> bool {
    LOG_ENABLED.load(Ordering::Acquire)
}

/// 从环境变量 SUBVOL_LOG 初始化日志开关
pub fn init_from_env() {
    if let Ok(v) = std::env::var("SUBVOL_LOG") {
        if v == "1" || v == "true" || v == "yes" {
            set_enabled(true);
        }
    }
}

/// 错误日志 — 始终输出到 stderr
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        eprintln!("[ERROR] {}", format_args!($($arg)*));
    };
}

/// 警告日志 — 始终输出到 stderr
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        eprintln!("[WARN] {}", format_args!($($arg)*));
    };
}

/// 信息日志 — 仅在日志开启时输出到 stdout
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::log::is_enabled() {
            println!("[INFO] {}", format_args!($($arg)*));
        }
    };
}

/// 详细日志 — 仅在日志开启时输出到 stdout
#[macro_export]
macro_rules! log_verbose {
    ($($arg:tt)*) => {
        if $crate::log::is_enabled() {
            println!("[VERBOSE] {}", format_args!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_enable_toggle() {
        set_enabled(false);
        assert!(!is_enabled());
        set_enabled(true);
        assert!(is_enabled());
        set_enabled(false);
        assert!(!is_enabled());
    }

    /// 验证宏可以正常编译
    #[test]
    fn test_log_macros_compile() {
        set_enabled(true);
        crate::log_error!("error test {}", 1);
        crate::log_warn!("warn test {}", 2);
        crate::log_info!("info test {}", 3);
        crate::log_verbose!("verbose test {}", 4);
        set_enabled(false);
        crate::log_info!("should not appear {}", 5);
        crate::log_verbose!("should not appear {}", 6);
        crate::log_error!("error always {}", 7);
        crate::log_warn!("warn always {}", 8);
    }

    /// 验证环境变量初始化
    #[test]
    fn test_init_from_env() {
        set_enabled(false);
        assert!(!is_enabled());
        std::env::set_var("SUBVOL_LOG", "1");
        init_from_env();
        assert!(is_enabled());
        std::env::remove_var("SUBVOL_LOG");
        set_enabled(false);
    }
}
