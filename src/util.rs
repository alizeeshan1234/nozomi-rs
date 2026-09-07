//! Small helpers shared across transports.

/// Placeholder printed in `Debug` output wherever a secret would be.
pub(crate) const REDACTED: &str = "<redacted>";

/// Percent-encode a query-string value. Everything outside the RFC 3986
/// unreserved set (`A-Z a-z 0-9 - _ . ~`) becomes `%XX`, so a key with a
/// stray newline, space, `#`, or `&` cannot truncate or corrupt the URL.
pub(crate) fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `Retry-After` in whole seconds, if present and numeric.
pub(crate) fn retry_after(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Normalize a base URL: trim trailing slashes and require an http(s) scheme.
pub(crate) fn normalize_base_url(what: &str, base: &str) -> crate::Result<String> {
    let base = base.trim().trim_end_matches('/').to_string();
    if !base.starts_with("http://") && !base.starts_with("https://") {
        return Err(crate::Error::Config(format!(
            "{what} must start with http:// or https://, got {base}"
        )));
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_everything_but_unreserved() {
        assert_eq!(encode_query_value("abc-XYZ_0.9~"), "abc-XYZ_0.9~");
        assert_eq!(encode_query_value("a b#c&d=e\n"), "a%20b%23c%26d%3De%0A");
        assert_eq!(encode_query_value(""), "");
    }

    #[test]
    fn retry_after_parses_seconds_only() {
        let mut h = http::HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert(http::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after(&h), Some(7));
        h.insert(
            http::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn normalizes_base_urls() {
        assert_eq!(
            normalize_base_url("x", "https://a.b/// ").unwrap(),
            "https://a.b"
        );
        assert!(normalize_base_url("x", "a.b").is_err());
    }
}
