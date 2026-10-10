//! Resolving the process log level from `LOGLEVEL`.
//!
//! The service used to log at `DEBUG` unconditionally: `main` passed the
//! literal `"DEBUG"`, and `LOGLEVEL` was read nowhere in `src/` at all.
//! `.env.example` documented the variable, so an operator set it, saw no
//! change, and reasonably concluded the logging configuration was broken rather
//! than absent.
//!
//! The cost was not cosmetic. At `DEBUG` the service emits hyper connection
//! traces on every request, which for a batch consumer materialising hundreds of
//! tapes buries anything worth reading and costs real IO.
//!
//! Resolution is a pure function so it can be asserted directly, rather than by
//! scraping log output for what did or did not appear.
//!
//! It lives beside the other configuration knobs rather than in `utils`: it has
//! one caller, and `rules/global_rules.md` puts a new knob with its config
//! module. `utils::env` is the exception, and only because `utils::admission`
//! reads a knob of its own and must not import a layer above it.

use super::read_var;
use std::fmt;

/// The environment variable that sets the process log level.
pub const LOG_LEVEL_VAR: &str = "LOGLEVEL";

/// The level used when `LOGLEVEL` is unset, blank, or unrecognised.
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;

/// A log level this service accepts.
///
/// The five `tracing` levels and nothing else, `#[repr(u8)]` as
/// `rules/global_rules.md` asks of a small closed enum.
///
/// `Ord` is severity-ASCENDING — `Trace < Error` — which is the REVERSE of
/// `tracing::Level`, where `TRACE > ERROR`. Nothing here compares the two, but
/// anyone who starts to should know they disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    /// Per-row, per-leg detail. Development only.
    Trace,
    /// Internal state transitions.
    Debug,
    /// Business events: started, resolved, finished.
    Info,
    /// Recoverable problems and constraint relaxations.
    Warn,
    /// Unrecoverable failures and invariant violations.
    Error,
}

impl LogLevel {
    /// The upper-case name, as `LOGLEVEL` spells it.
    #[must_use]
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    /// Parses a level name, case-insensitively.
    ///
    /// Returns `None` for anything else, which the caller reports rather than
    /// silently swallowing.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "TRACE" => Some(LogLevel::Trace),
            "DEBUG" => Some(LogLevel::Debug),
            "INFO" => Some(LogLevel::Info),
            "WARN" => Some(LogLevel::Warn),
            "ERROR" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

impl LogLevel {
    /// The `tracing` level this one installs as the subscriber's maximum.
    #[must_use]
    #[inline]
    pub fn as_tracing_level(self) -> tracing::Level {
        match self {
            LogLevel::Trace => tracing::Level::TRACE,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Error => tracing::Level::ERROR,
        }
    }
}

/// Installs the process-wide `tracing` subscriber at `level`.
///
/// optionstratlib 0.22 removed `setup_logger_with_level` and no longer depends
/// on `tracing-subscriber`, so the subscriber is installed here: the same
/// formatted output with `level` as the maximum. Idempotent: if a global
/// subscriber already exists (a second call, or a test harness that installed
/// one), the existing one is kept and this is a no-op.
pub fn init_logging(level: LogLevel) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(level.as_tracing_level())
        .try_init();
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `LOGLEVEL` resolved to, and whether anything was wrong with it.
///
/// The two are separate because they are consumed at different moments: the
/// level has to be known BEFORE a subscriber exists, and the complaint about a
/// bad value can only be logged AFTER one does. Returning both lets `main`
/// install the subscriber first and then say what it ignored, instead of
/// swallowing the mistake or aborting over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLogLevel {
    /// The level to configure.
    pub level: LogLevel,
    /// The value that was rejected, if one was. `None` when `LOGLEVEL` was
    /// unset, blank, or valid.
    pub rejected: Option<String>,
}

/// Resolves a raw `LOGLEVEL` value.
///
/// Unset or blank resolves to [`DEFAULT_LOG_LEVEL`] with nothing rejected: a
/// blank value is an unset value everywhere in this service
/// ([`crate::utils::env`]), and a knob nobody set is not a mistake.
///
/// An unrecognised value resolves to the same default but reports itself, so it
/// can be warned about once. It never aborts startup: refusing to boot because
/// a log level is misspelled trades a small problem for a total one.
#[must_use]
pub fn resolve_log_level(raw: Option<&str>) -> ResolvedLogLevel {
    match raw {
        None => ResolvedLogLevel {
            level: DEFAULT_LOG_LEVEL,
            rejected: None,
        },
        // Blank is unset, not a mistake. `read_var` filters it before
        // `resolve_log_level_from_env` gets here, but this function is public
        // and a direct caller must not be told its empty string was rejected.
        Some(raw) if raw.trim().is_empty() => ResolvedLogLevel {
            level: DEFAULT_LOG_LEVEL,
            rejected: None,
        },
        Some(raw) => match LogLevel::parse(raw) {
            Some(level) => ResolvedLogLevel {
                level,
                rejected: None,
            },
            None => ResolvedLogLevel {
                level: DEFAULT_LOG_LEVEL,
                rejected: Some(raw.to_string()),
            },
        },
    }
}

/// Resolves `LOGLEVEL` from the environment.
///
/// See [`resolve_log_level`]; this only supplies the value, through the same
/// blank-is-unset reader every other knob uses.
#[must_use]
pub fn resolve_log_level_from_env() -> ResolvedLogLevel {
    resolve_log_level(read_var(LOG_LEVEL_VAR).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unset `LOGLEVEL` resolves to `INFO`, not to `DEBUG`.
    ///
    /// The bug this closes: the service passed a literal `"DEBUG"` and never
    /// read the variable at all.
    #[test]
    fn test_an_unset_level_resolves_to_info() {
        let resolved = resolve_log_level(None);

        assert_eq!(resolved.level, LogLevel::Info);
        assert_eq!(
            resolved.rejected, None,
            "nothing was set, so nothing is wrong"
        );
    }

    /// A blank value is an unset value, like every other knob.
    #[test]
    fn test_a_blank_level_resolves_to_the_default() {
        // `read_var` filters blanks before this function sees them, so the
        // reachable blank case is `None`. The trimmed forms are covered here
        // too, in case a caller passes a raw value directly.
        for blank in ["", "   "] {
            let resolved = resolve_log_level(Some(blank));
            assert_eq!(resolved.level, DEFAULT_LOG_LEVEL, "{blank:?}");
            assert_eq!(
                resolved.rejected, None,
                "{blank:?} is a knob nobody set, not a value to complain about"
            );
        }
    }

    /// Every accepted name, in any case.
    ///
    /// Asserted on the RESOLVED level rather than by scraping output, which is
    /// what the issue asks for: a test that reads log lines proves what a
    /// formatter did, not what the level was.
    #[test]
    fn test_every_level_name_is_accepted_case_insensitively() {
        for (raw, expected) in [
            ("trace", LogLevel::Trace),
            ("TRACE", LogLevel::Trace),
            ("Debug", LogLevel::Debug),
            ("info", LogLevel::Info),
            ("warn", LogLevel::Warn),
            ("  Error  ", LogLevel::Error),
        ] {
            let resolved = resolve_log_level(Some(raw));
            assert_eq!(resolved.level, expected, "for {raw:?}");
            assert_eq!(resolved.rejected, None, "for {raw:?}");
        }
    }

    /// `LOGLEVEL=WARN` resolves to `WARN`, which is what the subscriber's
    /// `with_max_level` is built from and therefore what excludes `INFO` and
    /// `DEBUG`.
    #[test]
    fn test_warn_admits_nothing_below_it() {
        let resolved = resolve_log_level(Some("WARN"));

        assert_eq!(resolved.level, LogLevel::Warn);
    }

    /// An unrecognised value falls back AND reports itself.
    ///
    /// Both halves matter: falling back keeps a misspelling from aborting
    /// startup, and reporting it is what lets `main` warn once instead of
    /// leaving the operator to wonder why nothing changed.
    #[test]
    fn test_an_unrecognised_level_falls_back_and_is_reported() {
        let resolved = resolve_log_level(Some("verbose"));

        assert_eq!(resolved.level, DEFAULT_LOG_LEVEL);
        assert_eq!(resolved.rejected, Some("verbose".to_string()));
    }

    /// Every variant installs the `tracing` level of the same name.
    ///
    /// A swapped arm would not fail anywhere else: the service would just log
    /// at the wrong verbosity. Comparing the names pins the mapping.
    #[test]
    fn test_every_level_maps_to_the_tracing_level_of_the_same_name() {
        for level in [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            assert_eq!(level.as_tracing_level().as_str(), level.as_str());
        }
    }

    /// The variable name is the one the docs promise.
    ///
    /// `resolve_log_level_from_env` is the only function `main` calls, and a
    /// typo in the constant would pass every other test in this module while
    /// reading nothing.
    #[test]
    fn test_the_env_variable_is_read_by_its_documented_name() {
        use once_cell::sync::Lazy;
        use std::sync::Mutex;

        static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        assert_eq!(LOG_LEVEL_VAR, "LOGLEVEL");

        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(LOG_LEVEL_VAR, "error");
        }
        assert_eq!(resolve_log_level_from_env().level, LogLevel::Error);

        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(LOG_LEVEL_VAR);
        }
        assert_eq!(resolve_log_level_from_env().level, DEFAULT_LOG_LEVEL);
    }
}
