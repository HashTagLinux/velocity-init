//! Minimal console logger replacing `env_logger`.
//!
//! PID 1 needs exactly three things from its console sink: a maximum
//! level (CLI `--log-level`, overridden by `RUST_LOG`), one line per
//! record on stderr, and no dependency that pulls in regex/humantime
//! parsing for directive grammars we never use. Per-module `RUST_LOG`
//! directives (`debug,velocity=info`) are therefore *not* supported:
//! when `RUST_LOG` names a plain level it wins, otherwise the
//! configured default applies (which matches `env_logger`'s
//! `default_filter_or` behaviour for the global level).

use std::io::Write;
use std::time::Instant;

use log::{LevelFilter, Metadata, Record};

/// stderr sink with a fixed maximum level.
pub struct ConsoleLogger {
    filter: LevelFilter,
    started: Instant,
}

impl ConsoleLogger {
    /// Build a console sink passing through `filter` and below.
    pub fn new(filter: LevelFilter) -> Self {
        Self {
            filter,
            started: Instant::now(),
        }
    }

    /// Resolve the effective filter: `RUST_LOG` wins when it names a
    /// plain level (case-insensitive, surrounding whitespace ignored);
    /// otherwise `default` applies.
    pub fn resolve(default: LevelFilter) -> LevelFilter {
        Self::resolve_with(std::env::var("RUST_LOG").ok().as_deref(), default)
    }

    /// Pure core of [`ConsoleLogger::resolve`]: unit tests pass values
    /// directly instead of mutating the process environment.
    pub fn resolve_with(raw: Option<&str>, default: LevelFilter) -> LevelFilter {
        raw.map(str::trim).and_then(parse_level).unwrap_or(default)
    }
}

/// Parse `error | warn | info | debug | trace` (case-insensitive).
/// Anything else — empty, `off`, module directives — is `None`, so the
/// caller keeps its default instead of disabling logging by accident.
fn parse_level(s: &str) -> Option<LevelFilter> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Some(LevelFilter::Error),
        "warn" | "warning" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

impl log::Log for ConsoleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.filter
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Seconds since process start: monotonic, no date formatting
        // dependencies, and stable across timezones/locales.
        let up = self.started.elapsed();
        let _ = writeln!(
            std::io::stderr(),
            "[{:>8.3} {:<5} {}] {}",
            up.as_secs_f64(),
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {
        // stderr is unbuffered, so there is normally nothing to flush —
        // but the `Log` contract asks flush to push buffered records out,
        // and a lost final line during a crash is the worst place to
        // discover we skipped it.
        let _ = std::io::stderr().flush();
    }
}

// ──────────────────────────── unit tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn plain_levels_parse_case_insensitively() {
        assert_eq!(parse_level("info"), Some(LevelFilter::Info));
        assert_eq!(parse_level("DEBUG"), Some(LevelFilter::Debug));
        assert_eq!(parse_level("Warn"), Some(LevelFilter::Warn));
        assert_eq!(parse_level("error"), Some(LevelFilter::Error));
        assert_eq!(parse_level("trace"), Some(LevelFilter::Trace));
    }

    #[test]
    fn directives_and_garbage_keep_the_default() {
        for bad in ["", "off", "velocity=info", "debug,velocity=info", "verbose"] {
            assert_eq!(parse_level(bad), None, "{bad:?}");
            assert_eq!(
                ConsoleLogger::resolve_with(Some(bad), LevelFilter::Warn),
                LevelFilter::Warn,
                "{bad:?} must not change the default"
            );
        }
        assert_eq!(ConsoleLogger::resolve_with(None, LevelFilter::Info), LevelFilter::Info);
        assert_eq!(
            ConsoleLogger::resolve_with(Some("  debug "), LevelFilter::Info),
            LevelFilter::Debug
        );
    }

    #[test]
    fn enabled_honours_the_max_level() {
        let log = ConsoleLogger::new(LevelFilter::Warn);
        for level in [log::Level::Error, log::Level::Warn] {
            assert!(log.enabled(&log::Metadata::builder().level(level).target("t").build()));
        }
        for level in [log::Level::Info, log::Level::Debug, log::Level::Trace] {
            assert!(!log.enabled(&log::Metadata::builder().level(level).target("t").build()));
        }
    }
}
