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
//! Two things the rule deliberately does NOT do. It does not trim: whitespace
//! decides whether a value is blank and nothing else, because a credential
//! written with a leading space is a different credential and this module
//! cannot tell one variable's meaning from another's. And it does not cover an
//! opaque credential whose empty form is meaningful, which is what
//! [`read_secret`] is for.
//!
//! This module sits in `utils` rather than in `infrastructure::config` so that
//! every layer can hold the rule, including `utils::admission`, which reads a
//! knob of its own and must not import a layer above it.

/// Reads an environment variable, treating a blank value as unset.
///
/// Whitespace decides only whether the value is BLANK; what comes back is the
/// value as written. Trimming the result would quietly change bytes the caller
/// may have meant: a credential with a leading space is a different credential,
/// and this function cannot tell one variable's meaning from another's. A
/// caller that parses a number or a host trims at its own call site, where the
/// meaning is known.
#[must_use]
pub fn read_var(variable: &str) -> Option<String> {
    let raw = std::env::var(variable).ok()?;
    if raw.trim().is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// Reads an opaque credential, where an empty value is a REAL value.
///
/// The blank-is-unset rule exists because a blank knob is how an operator
/// comments one out, and because `REDIS_PASSWORD=` used to build an AUTH
/// attempt with an empty password against a server that has none. Neither
/// argument holds for a credential whose empty form is what a server actually
/// wants: a stock ClickHouse `default` user HAS no password, and `=` is the
/// only way to say so through the environment.
///
/// So this returns the value whenever the variable is PRESENT, empty included,
/// and never trims. Use it only where an empty credential is meaningful to the
/// server being configured; everything else reads through [`read_var`].
#[must_use]
pub fn read_secret(variable: &str) -> Option<String> {
    std::env::var(variable).ok()
}

#[cfg(test)]
mod tests {
    use super::{read_secret, read_var};
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

    /// A value that is not blank comes back exactly as written.
    ///
    /// Trimming it would change bytes this function cannot interpret: a
    /// credential with a leading space is a different credential, and only the
    /// caller knows whether its variable is a number, a host or a secret.
    #[test]
    fn test_a_value_is_returned_verbatim() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        for raw in [" padded ", "\ttabbed", "plain", "two words"] {
            set_var(raw);
            assert_eq!(
                read_var(TEST_VAR).as_deref(),
                Some(raw),
                "the value must survive unchanged"
            );
        }
        remove_var();
    }

    /// A credential reads as present even when it is empty.
    ///
    /// The blank-is-unset rule does not apply to an opaque credential whose
    /// empty form is a real configuration: a stock ClickHouse `default` user
    /// has no password, and `=` is the only way to say so.
    #[test]
    fn test_a_secret_may_be_empty() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        set_var("");
        assert_eq!(read_secret(TEST_VAR).as_deref(), Some(""));
        assert_eq!(
            read_var(TEST_VAR),
            None,
            "the two readers must disagree here"
        );

        set_var("  ");
        assert_eq!(
            read_secret(TEST_VAR).as_deref(),
            Some("  "),
            "a secret is never trimmed either"
        );

        remove_var();
        assert_eq!(read_secret(TEST_VAR), None, "unset is still unset");
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

    /// A padded value survives WITH its padding.
    ///
    /// Whitespace decides only whether the value is blank. Trimming a
    /// credential here would authenticate with different bytes than the
    /// operator wrote, and this function cannot tell a credential from a port.
    #[test]
    fn test_a_padded_value_keeps_its_padding() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        set_var("  s3cret  ");
        assert_eq!(read_var(TEST_VAR), Some("  s3cret  ".to_string()));
        remove_var();
    }
}
