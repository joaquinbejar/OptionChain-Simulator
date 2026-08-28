//! Environment variables, under one rule.
//!
//! **A blank value is an unset value.** `KNOB=` and `KNOB="   "` mean the same
//! thing as not writing `KNOB` at all, and the documented default applies.
//!
//! A blank value in a `.env` file is how a knob gets commented out in practice,
//! so honouring it as a real empty string produces failures that point
//! somewhere else entirely. The case that motivated writing the rule down:
//! `REDIS_PASSWORD=` used to yield `Some("")`, which built `redis://:@host` —
//! an AUTH attempt with an empty password against a server that has none. The
//! connection failed with an authentication error, and nothing in that message
//! pointed at an empty variable.
//!
//! This module sits in `utils` rather than in `infrastructure::config` so that
//! every layer can hold the rule, including `utils::admission`, which reads a
//! knob of its own and must not import a layer above it.

/// Reads an environment variable, treating a blank value as unset.
///
/// The returned value is trimmed, so surrounding whitespace never reaches a
/// host name, a credential or a parser.
#[must_use]
pub fn read_var(variable: &str) -> Option<String> {
    let raw = std::env::var(variable).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::read_var;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    /// Environment mutation is process-wide, so these serialise. The variable
    /// name is private to this module, so nothing else can collide with it.
    static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    const TEST_VAR: &str = "OCS_TEST_BLANK_RULE";

    fn set_var(value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(TEST_VAR, value);
        }
    }

    fn remove_var() {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(TEST_VAR);
        }
    }

    /// An unset variable reads as absent.
    #[test]
    fn test_an_unset_variable_is_none() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_var();

        assert_eq!(read_var(TEST_VAR), None);
    }

    /// Every form of blank reads as absent, which is the rule.
    #[test]
    fn test_a_blank_variable_is_none() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        for blank in ["", "   ", "\t", " \t "] {
            set_var(blank);
            assert_eq!(read_var(TEST_VAR), None, "{blank:?} must read as unset");
        }
        remove_var();
    }

    /// A real value survives, trimmed.
    #[test]
    fn test_a_real_value_survives_and_is_trimmed() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        set_var("  s3cret  ");
        assert_eq!(read_var(TEST_VAR), Some("s3cret".to_string()));
        remove_var();
    }
}
