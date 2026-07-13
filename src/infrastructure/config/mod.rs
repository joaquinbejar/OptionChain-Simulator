pub mod clickhouse;
pub mod mongo;
pub mod redis;

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
