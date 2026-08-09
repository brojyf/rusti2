use proptest::prelude::*;

/// Simulates the bearer token extraction logic from `auth.rs`.
fn extract_bearer_token(header_value: &str) -> Option<String> {
    let stripped = header_value
        .strip_prefix("Bearer ")
        .or_else(|| header_value.strip_prefix("bearer "))?;
    let token = stripped.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

/// Simulates scope parsing from `policy.rs`.
fn parse_scope(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (bucket, prefix) = match raw.split_once('/') {
        Some((bucket, prefix)) => (bucket, prefix),
        None => (raw, ""),
    };
    if bucket.is_empty() {
        return None;
    }
    let key_prefix = prefix.strip_suffix('*').unwrap_or(prefix);
    Some((bucket.to_owned(), key_prefix.to_owned()))
}

/// Simulates scope covering check.
fn scope_covers(scope: &(String, String), bucket: &str, key: &str) -> bool {
    scope.0 == bucket && key.starts_with(&scope.1)
}

proptest! {
    /// Bearer token extraction must handle arbitrary header values
    /// without panicking.
    #[test]
    fn fuzz_extract_bearer_token(header in "\\PC*") {
        let result = extract_bearer_token(&header);

        if let Some(token) = result {
            assert!(!token.is_empty());

            // Must have come from a Bearer-prefixed header.
            let lower = header.to_lowercase();
            assert!(lower.starts_with("bearer "));
        }
    }

    /// Scope parsing must never panic and must produce a non-empty bucket.
    #[test]
    fn fuzz_scope_parse(raw in "\\PC*") {
        let result = parse_scope(&raw);
        if let Some((bucket, key_prefix)) = result {
            assert!(!bucket.is_empty());
            // key_prefix must not end with * (already stripped)
            assert!(!key_prefix.ends_with('*'));
        }
    }

    /// scope_covers must be consistent: a scope parsed from a string must
    /// cover a key starting with its own prefix within the same bucket.
    #[test]
    fn fuzz_scope_covers_consistency(
        bucket in "[a-z][a-z0-9-]{0,20}",
        prefix in "[a-z0-9/]{0,30}",
        suffix in "[a-z0-9.]{0,30}"
    ) {
        let scope_raw = format!("{bucket}/{prefix}*");
        let scope = parse_scope(&scope_raw).expect("valid scope must parse");

        let key = format!("{prefix}{suffix}");

        // Same bucket + key with prefix must be covered.
        assert!(scope_covers(&scope, &bucket, &key));

        // Different bucket must not be covered.
        let other_bucket = format!("other-{bucket}");
        assert!(!scope_covers(&scope, &other_bucket, &key));

        // Same bucket but key without the prefix must not be covered
        // (unless prefix is empty). Prepending "x" only guarantees a
        // non-matching key when the result doesn't coincidentally start
        // with `prefix` again (e.g. prefix "x" turns "x{prefix}" back
        // into a prefix match), so skip when that guarantee doesn't hold.
        if !prefix.is_empty() && !suffix.is_empty() {
            let foreign_key = format!("x{prefix}{suffix}");
            if !foreign_key.starts_with(&prefix) {
                assert!(!scope_covers(&scope, &bucket, &foreign_key));
            }
        }
    }
}

#[cfg(test)]
mod fuzz_policy_tests {
    use super::*;

    #[test]
    fn bearer_token_known_cases() {
        assert_eq!(
            extract_bearer_token("Bearer my-token"),
            Some("my-token".to_owned())
        );
        assert_eq!(
            extract_bearer_token("bearer my-token"),
            Some("my-token".to_owned())
        );
        assert_eq!(extract_bearer_token("Basic my-token"), None);
        assert_eq!(extract_bearer_token(""), None);
        assert_eq!(extract_bearer_token("Bearer "), None);
    }

    #[test]
    fn scope_parse_known_cases() {
        assert_eq!(
            parse_scope("bucket/key"),
            Some(("bucket".to_owned(), "key".to_owned()))
        );
        assert_eq!(
            parse_scope("bucket/*"),
            Some(("bucket".to_owned(), "".to_owned()))
        );
        assert_eq!(
            parse_scope("bucket"),
            Some(("bucket".to_owned(), "".to_owned()))
        );
        assert_eq!(parse_scope(""), None);
        assert_eq!(parse_scope("/prefix"), None);
    }
}
