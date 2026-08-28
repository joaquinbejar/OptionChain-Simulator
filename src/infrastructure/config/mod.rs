//! Environment-driven configuration for every external service.
//!
//! One rule holds across all of it: **a blank value is an unset value**. See
//! [`read_var`], which every module here reads through.

pub mod clickhouse;
pub mod mongo;
pub mod redis;
/// Operational configuration for v2 rolling simulations: retention, the
/// cleanup cadence, and the domain-cache capacities.
pub mod simulation_v2;
/// Operational configuration for persisting v2 snapshots: whether it happens,
/// how large a batch may be, how long an insert may take, and how long the rows
/// are kept.
pub mod snapshot;

/// Reads an environment variable, treating a blank value as unset.
///
/// **The rule, once, for every configuration knob in this service: a blank
/// value is an unset value.** `KNOB=` and `KNOB="   "` mean the same thing as
/// not writing `KNOB` at all, and the documented default applies.
///
/// A blank value in a `.env` file is how a knob gets commented out in practice,
/// so honouring it as a real empty string produces failures that point
/// somewhere else entirely. The case that motivated stating this rule:
/// `REDIS_PASSWORD=` used to yield `Some("")`, which built `redis://:@host` —
/// an AUTH attempt with an empty password against a server that has none. The
/// connection failed with an authentication error, and nothing in that message
/// pointed at an empty variable.
///
/// The returned value is trimmed, so surrounding whitespace never reaches a
/// host name, a credential or a parser.
#[must_use]
pub(crate) fn read_var(variable: &str) -> Option<String> {
    let raw = std::env::var(variable).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Redacts URL userinfo (credentials) from every URL-like substring inside a
/// larger text (log lines, driver error messages).
///
/// For each occurrence of `scheme://`, the candidate section runs up to the
/// next whitespace (or end of string); if it contains an `@`, everything
/// between `://` and its LAST `@` is treated as credentials and replaced with
/// `***`. Bounding by the last `@` is deliberately CONSERVATIVE: a raw
/// credential may contain `/`, `:`, or `@` (RedisConfig::url and env-provided
/// URIs embed them verbatim), so the scan prefers over-redacting a legitimate
/// path `@` (`scheme://host/some@path` → `scheme://***@path`) to ever leaking
/// a password fragment. Credentials containing whitespace cannot be recovered
/// from mid-text scanning; whole-string values should use [`redact_uri`].
pub(crate) fn redact_userinfo(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut i = 0;

    while i < s.len() {
        match s[i..].find("://") {
            Some(pos) => {
                // Byte index just past the "://" scheme separator.
                let sep = i + pos + 3;
                result.push_str(&s[i..sep]);

                // The candidate section runs to the next whitespace (or end).
                // NOT stopping at '/' is intentional — see the doc comment.
                let rest = &s[sep..];
                let section_end = rest
                    .find(|c: char| c.is_whitespace())
                    .map(|p| sep + p)
                    .unwrap_or_else(|| s.len());

                // Everything before the LAST '@' is treated as credentials.
                match s[sep..section_end].rfind('@') {
                    Some(at_rel) => {
                        let at = sep + at_rel;
                        result.push_str("***@");
                        i = at + 1;
                    }
                    None => {
                        i = sep;
                    }
                }
            }
            None => {
                result.push_str(&s[i..]);
                break;
            }
        }
    }

    result
}

/// Redacts the userinfo of a value that IS a single URI (not text containing
/// one), e.g. `MongoDBConfig::uri` taken verbatim from the environment.
///
/// Scans the WHOLE string after `://` for the last `@`, so credentials
/// containing `/`, `:`, `@`, or even whitespace are fully covered — anything
/// before the last `@` is replaced with `***`. URIs without an `@` are
/// returned unchanged.
pub(crate) fn redact_uri(uri: &str) -> String {
    match uri.find("://") {
        Some(pos) => {
            let sep = pos + 3;
            match uri[sep..].rfind('@') {
                Some(at_rel) => {
                    let at = sep + at_rel;
                    format!("{}***@{}", &uri[..sep], &uri[at + 1..])
                }
                None => uri.to_string(),
            }
        }
        None => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_userinfo_user_and_password() {
        assert_eq!(
            redact_userinfo("mongodb://admin:s3cret@localhost:27017"),
            "mongodb://***@localhost:27017"
        );
    }

    #[test]
    fn test_redact_userinfo_username_only() {
        assert_eq!(
            redact_userinfo("redis://user@localhost:6379"),
            "redis://***@localhost:6379"
        );
    }

    #[test]
    fn test_redact_userinfo_password_only() {
        assert_eq!(
            redact_userinfo("redis://:s3cret@localhost:6379"),
            "redis://***@localhost:6379"
        );
    }

    #[test]
    fn test_redact_userinfo_no_userinfo_unchanged() {
        let input = "redis://localhost:6379/3";
        assert_eq!(redact_userinfo(input), input);
    }

    #[test]
    fn test_redact_userinfo_multiple_urls() {
        assert_eq!(
            redact_userinfo("redis://u:p@h1:6379 and mongodb://a:b@h2:27017"),
            "redis://***@h1:6379 and mongodb://***@h2:27017"
        );
    }

    #[test]
    fn test_redact_userinfo_at_in_path_over_redacts_conservatively() {
        // A path '@' cannot be distinguished from an unencoded credential
        // containing '/' without parsing knowledge we don't have; the scan
        // deliberately over-redacts rather than risk leaking a password.
        assert_eq!(
            redact_userinfo("redis://localhost:6379/some@path"),
            "redis://***@path"
        );
    }

    #[test]
    fn test_redact_userinfo_slash_in_password() {
        // A raw '/' inside a credential must not defeat the redaction.
        let out = redact_userinfo("connect failed: redis://user:p/secret@localhost:6379 timeout");
        assert!(!out.contains("p/secret"));
        assert_eq!(out, "connect failed: redis://***@localhost:6379 timeout");
    }

    #[test]
    fn test_redact_uri_slash_in_password() {
        assert_eq!(
            redact_uri("mongodb://admin:p/secret@localhost:27017"),
            "mongodb://***@localhost:27017"
        );
    }

    #[test]
    fn test_redact_uri_whitespace_in_password() {
        // Whole-string scanning covers credentials containing whitespace too.
        assert_eq!(
            redact_uri("mongodb://admin:p secret@localhost:27017"),
            "mongodb://***@localhost:27017"
        );
    }

    #[test]
    fn test_redact_uri_no_userinfo_unchanged() {
        let input = "mongodb://localhost:27017";
        assert_eq!(redact_uri(input), input);
    }

    #[test]
    fn test_redact_uri_plain_text_unchanged() {
        let input = "no uri here";
        assert_eq!(redact_uri(input), input);
    }

    #[test]
    fn test_redact_userinfo_unencoded_at_in_password() {
        // An unencoded '@' inside the password must not leak a fragment: the
        // LAST '@' in the authority bounds the userinfo.
        assert_eq!(
            redact_userinfo("redis://user:p@ss@localhost:6379"),
            "redis://***@localhost:6379"
        );
    }

    #[test]
    fn test_redact_userinfo_plain_text_unchanged() {
        let input = "no url here";
        assert_eq!(redact_userinfo(input), input);
    }
}

#[cfg(test)]
mod blank_rule_tests {
    use super::read_var;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    /// Environment mutation is process-wide, so these serialise.
    static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn set_var(name: &str, value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(name, value);
        }
    }

    fn remove_var(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// An unset variable reads as absent.
    #[test]
    fn test_an_unset_variable_is_none() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_var("OCS_TEST_BLANK_RULE");

        assert_eq!(read_var("OCS_TEST_BLANK_RULE"), None);
    }

    /// A blank value reads as absent, which is the rule this module states.
    ///
    /// Every form of blank: empty, spaces, and a tab. `KNOB=` is how a knob
    /// gets commented out in practice, and reading it as an empty string is
    /// what made `REDIS_PASSWORD=` an AUTH attempt with no password.
    #[test]
    fn test_a_blank_variable_is_none() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        for blank in ["", "   ", "\t", " \t "] {
            set_var("OCS_TEST_BLANK_RULE", blank);
            assert_eq!(
                read_var("OCS_TEST_BLANK_RULE"),
                None,
                "{blank:?} must read as unset"
            );
        }
        remove_var("OCS_TEST_BLANK_RULE");
    }

    /// A real value survives, trimmed.
    ///
    /// Trimming matters as much as the blank rule: surrounding whitespace in a
    /// `.env` file must never reach a host name, a credential or a parser.
    #[test]
    fn test_a_real_value_survives_and_is_trimmed() {
        let _guard = match ENV_MUTEX.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        set_var("OCS_TEST_BLANK_RULE", "  s3cret  ");
        assert_eq!(
            read_var("OCS_TEST_BLANK_RULE"),
            Some("s3cret".to_string()),
            "a real value must survive, without its padding"
        );
        remove_var("OCS_TEST_BLANK_RULE");
    }
}
