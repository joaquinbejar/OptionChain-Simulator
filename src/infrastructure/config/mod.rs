pub mod clickhouse;
pub mod mongo;
pub mod redis;

/// Redacts URL userinfo (credentials) from every URL-like substring.
///
/// For each occurrence of a `scheme://user:password@` or `scheme://user@`
/// section, the credentials are replaced with `***`, yielding `scheme://***@`.
/// URLs without userinfo (`scheme://host...`) are left untouched, and so is any
/// `@` that appears after the authority (for example inside a path).
///
/// This is intentionally implemented with plain string scanning (no regex, no
/// extra dependencies): for every `://`, the authority section runs up to the
/// next `/`, whitespace, or end of string; if it contains an `@`, everything
/// between `://` and that `@` is treated as credentials and redacted.
pub(crate) fn redact_userinfo(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut i = 0;

    while i < s.len() {
        match s[i..].find("://") {
            Some(pos) => {
                // Byte index just past the "://" scheme separator.
                let sep = i + pos + 3;
                result.push_str(&s[i..sep]);

                // The authority section ends at the first '/', whitespace, or
                // the end of the string.
                let rest = &s[sep..];
                let authority_end = rest
                    .find(|c: char| c == '/' || c.is_whitespace())
                    .map(|p| sep + p)
                    .unwrap_or_else(|| s.len());

                // If the authority contains an '@', everything before it is
                // userinfo (credentials) and must be redacted.
                match s[sep..authority_end].find('@') {
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
    fn test_redact_userinfo_at_in_path_not_redacted() {
        // The '@' appears after the authority (inside the path); it must not be
        // treated as userinfo.
        let input = "redis://localhost:6379/some@path";
        assert_eq!(redact_userinfo(input), input);
    }

    #[test]
    fn test_redact_userinfo_plain_text_unchanged() {
        let input = "no url here";
        assert_eq!(redact_userinfo(input), input);
    }
}
