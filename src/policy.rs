//! Per-caller authorization for the object storage API.
//!
//! A global bucket allowlist only answers "may this instance touch that
//! bucket at all", which means every service that can reach rusti2 inherits
//! the union of everyone's access — the indexer could delete objects the API
//! owns, and a compromised service could reach into a bucket that has nothing
//! to do with it. Authorization here is a function of three things instead:
//! who is calling, what they are trying to do, and which bucket and key
//! prefix they are trying to do it to.
//!
//! Policy is configuration, not code, so adding a caller is an env change and
//! never a release. It is parsed once at startup and a bad policy stops the
//! process rather than silently degrading to "allow".

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::Deserialize;

/// One operation the service exposes. Named after the RPC minus the `Object`
/// suffix, which is what a policy author writes in configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    PresignPut,
    Stat,
    Download,
    Upload,
    Delete,
}

impl Method {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "PresignPut" => Some(Self::PresignPut),
            "Stat" => Some(Self::Stat),
            "Download" => Some(Self::Download),
            "Upload" => Some(Self::Upload),
            "Delete" => Some(Self::Delete),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PresignPut => "PresignPut",
            Self::Stat => "Stat",
            Self::Download => "Download",
            Self::Upload => "Upload",
            Self::Delete => "Delete",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A bucket plus a literal key prefix within it.
///
/// Written in configuration as `bucket/prefix`, where a trailing `*` is
/// cosmetic: `cotab-avatars/users/*` and `cotab-avatars/users/` are the same
/// scope. `bucket/*` and a bare `bucket` both mean the whole bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub bucket: String,
    pub key_prefix: String,
}

impl Scope {
    fn parse(raw: &str) -> Result<Self, PolicyError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(PolicyError::EmptyScope);
        }
        let (bucket, prefix) = match raw.split_once('/') {
            Some((bucket, prefix)) => (bucket, prefix),
            None => (raw, ""),
        };
        if bucket.is_empty() {
            return Err(PolicyError::EmptyScope);
        }
        // A trailing `*` reads as a wildcard to anyone writing the config, but
        // matching is prefix matching either way. Strip it so the stored
        // prefix is exactly what a key must start with.
        let key_prefix = prefix.strip_suffix('*').unwrap_or(prefix);
        Ok(Self {
            bucket: bucket.to_owned(),
            key_prefix: key_prefix.to_owned(),
        })
    }

    fn covers(&self, bucket: &str, key: &str) -> bool {
        self.bucket == bucket && key.starts_with(&self.key_prefix)
    }
}

/// An authenticated caller and everything it is allowed to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// Stable service name, used in authorization failures and access logs.
    /// Never the token.
    pub name: String,
    methods: Vec<Method>,
    scopes: Vec<Scope>,
}

impl Caller {
    /// Reports whether this caller may run `method` against `bucket`/`key`.
    ///
    /// Both halves must pass: a caller granted `Delete` on one bucket does not
    /// get `Delete` everywhere, and a caller scoped to a bucket does not get
    /// every method on it.
    pub fn allows(&self, method: Method, bucket: &str, key: &str) -> bool {
        self.methods.contains(&method) && self.scopes.iter().any(|s| s.covers(bucket, key))
    }
}

/// The parsed contents of `RUSTI2_CALLERS`, indexed by bearer token.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    by_token: HashMap<String, Arc<Caller>>,
}

impl Policy {
    /// Resolves a bearer token to the caller it identifies.
    pub fn caller_for_token(&self, token: &str) -> Option<Arc<Caller>> {
        self.by_token.get(token).cloned()
    }

    pub fn caller_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_token.values().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Parses the JSON policy document.
    ///
    /// ```json
    /// [
    ///   {
    ///     "name": "cotab-api",
    ///     "token": "…",
    ///     "methods": ["PresignPut", "Stat", "Delete"],
    ///     "scopes": ["cotab-avatars-pending/*", "cotab-avatars/*"]
    ///   }
    /// ]
    /// ```
    pub fn parse(raw: &str) -> Result<Self, PolicyError> {
        let entries: Vec<CallerEntry> =
            serde_json::from_str(raw).map_err(|e| PolicyError::Malformed(e.to_string()))?;
        if entries.is_empty() {
            return Err(PolicyError::NoCallers);
        }

        let mut by_token: HashMap<String, Arc<Caller>> = HashMap::new();
        let mut seen_names: Vec<String> = Vec::new();

        for entry in entries {
            let name = entry.name.trim().to_owned();
            if name.is_empty() {
                return Err(PolicyError::EmptyName);
            }
            if seen_names.contains(&name) {
                return Err(PolicyError::DuplicateName(name));
            }

            // A short token is almost certainly a placeholder that made it to
            // production. Refusing to start is the only moment anyone will
            // notice.
            let token = entry.token.trim().to_owned();
            if token.len() < MIN_TOKEN_LEN {
                return Err(PolicyError::WeakToken {
                    caller: name,
                    len: token.len(),
                });
            }

            let mut methods = Vec::new();
            for raw_method in &entry.methods {
                let method =
                    Method::parse(raw_method).ok_or_else(|| PolicyError::UnknownMethod {
                        caller: name.clone(),
                        method: raw_method.clone(),
                    })?;
                if !methods.contains(&method) {
                    methods.push(method);
                }
            }
            if methods.is_empty() {
                return Err(PolicyError::NoMethods(name));
            }

            let mut scopes = Vec::new();
            for raw_scope in &entry.scopes {
                scopes.push(Scope::parse(raw_scope)?);
            }
            if scopes.is_empty() {
                return Err(PolicyError::NoScopes(name));
            }

            // Two callers sharing a token cannot be told apart, so the access
            // log and every authorization decision would be a coin flip.
            if by_token.contains_key(&token) {
                return Err(PolicyError::DuplicateToken(name));
            }

            seen_names.push(name.clone());
            by_token.insert(
                token,
                Arc::new(Caller {
                    name,
                    methods,
                    scopes,
                }),
            );
        }

        Ok(Self { by_token })
    }
}

/// Tokens are expected to be 32 random bytes rendered as hex or base64. The
/// floor is deliberately below that so a shorter-but-still-real token is not
/// rejected, and far above anything a human would type by hand.
const MIN_TOKEN_LEN: usize = 24;

#[derive(Debug, Deserialize)]
struct CallerEntry {
    name: String,
    token: String,
    methods: Vec<String>,
    scopes: Vec<String>,
}

#[derive(Debug)]
pub enum PolicyError {
    Malformed(String),
    NoCallers,
    EmptyName,
    DuplicateName(String),
    DuplicateToken(String),
    WeakToken { caller: String, len: usize },
    UnknownMethod { caller: String, method: String },
    NoMethods(String),
    NoScopes(String),
    EmptyScope,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "RUSTI2_CALLERS is not a valid policy document: {e}"),
            Self::NoCallers => f.write_str("RUSTI2_CALLERS must define at least one caller"),
            Self::EmptyName => f.write_str("every caller needs a non-empty name"),
            Self::DuplicateName(n) => write!(f, "caller {n:?} is defined more than once"),
            Self::DuplicateToken(n) => {
                write!(
                    f,
                    "caller {n:?} reuses a token already assigned to another caller"
                )
            }
            Self::WeakToken { caller, len } => write!(
                f,
                "caller {caller:?} has a {len}-character token; at least {MIN_TOKEN_LEN} required"
            ),
            Self::UnknownMethod { caller, method } => write!(
                f,
                "caller {caller:?} grants unknown method {method:?}; expected one of \
                 PresignPut, Stat, Download, Upload, Delete"
            ),
            Self::NoMethods(n) => write!(f, "caller {n:?} grants no methods"),
            Self::NoScopes(n) => write!(f, "caller {n:?} grants no scopes"),
            Self::EmptyScope => f.write_str("a scope must name a bucket"),
        }
    }
}

impl std::error::Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    const API_TOKEN: &str = "api-token-000000000000000000";
    const INDEXER_TOKEN: &str = "indexer-token-00000000000000";

    fn policy() -> Policy {
        Policy::parse(&format!(
            r#"[
              {{
                "name": "cotab-api",
                "token": "{API_TOKEN}",
                "methods": ["PresignPut", "Stat", "Delete"],
                "scopes": ["cotab-avatars-pending/*", "cotab-avatars/*"]
              }},
              {{
                "name": "cogate-indexer",
                "token": "{INDEXER_TOKEN}",
                "methods": ["Download", "Upload", "Delete"],
                "scopes": ["cotab-avatars-pending/*", "cotab-avatars/*"]
              }}
            ]"#
        ))
        .expect("parse policy")
    }

    #[test]
    fn resolves_caller_by_token() {
        let policy = policy();
        assert_eq!(
            policy.caller_for_token(API_TOKEN).expect("caller").name,
            "cotab-api"
        );
        assert!(policy.caller_for_token("nope").is_none());
    }

    #[test]
    fn grants_only_listed_methods() {
        let caller = policy().caller_for_token(API_TOKEN).expect("caller");
        assert!(caller.allows(Method::PresignPut, "cotab-avatars-pending", "users/a/b"));
        // The API never streams object bodies; the indexer does.
        assert!(!caller.allows(Method::Download, "cotab-avatars-pending", "users/a/b"));
    }

    #[test]
    fn refuses_buckets_outside_the_caller_scopes() {
        let caller = policy().caller_for_token(INDEXER_TOKEN).expect("caller");
        assert!(caller.allows(Method::Delete, "cotab-avatars", "users/a/b"));
        assert!(!caller.allows(Method::Delete, "some-other-service-bucket", "users/a/b"));
    }

    #[test]
    fn enforces_key_prefixes_within_a_bucket() {
        let policy = Policy::parse(&format!(
            r#"[{{
              "name": "scoped",
              "token": "{API_TOKEN}",
              "methods": ["Delete"],
              "scopes": ["shared-bucket/tenant-a/"]
            }}]"#
        ))
        .expect("parse policy");
        let caller = policy.caller_for_token(API_TOKEN).expect("caller");

        assert!(caller.allows(Method::Delete, "shared-bucket", "tenant-a/avatar.jpg"));
        assert!(!caller.allows(Method::Delete, "shared-bucket", "tenant-b/avatar.jpg"));
        // No path normalization happens in S3 keys, so a key that merely
        // mentions the prefix later does not match it.
        assert!(!caller.allows(Method::Delete, "shared-bucket", "tenant-b/../tenant-a/x"));
    }

    #[test]
    fn treats_bare_bucket_and_star_alike() {
        assert_eq!(
            Scope::parse("bucket").expect("parse"),
            Scope {
                bucket: "bucket".into(),
                key_prefix: String::new(),
            }
        );
        assert_eq!(
            Scope::parse("bucket/*").expect("parse"),
            Scope {
                bucket: "bucket".into(),
                key_prefix: String::new(),
            }
        );
    }

    #[test]
    fn rejects_placeholder_tokens() {
        let err = Policy::parse(
            r#"[{"name":"x","token":"changeme","methods":["Stat"],"scopes":["b/*"]}]"#,
        )
        .expect_err("short token must fail");
        assert!(matches!(err, PolicyError::WeakToken { .. }));
    }

    #[test]
    fn rejects_shared_tokens() {
        let err = Policy::parse(&format!(
            r#"[
              {{"name":"a","token":"{API_TOKEN}","methods":["Stat"],"scopes":["b/*"]}},
              {{"name":"b","token":"{API_TOKEN}","methods":["Stat"],"scopes":["b/*"]}}
            ]"#
        ))
        .expect_err("shared token must fail");
        assert!(matches!(err, PolicyError::DuplicateToken(_)));
    }

    #[test]
    fn rejects_unknown_methods() {
        let err = Policy::parse(&format!(
            r#"[{{"name":"x","token":"{API_TOKEN}","methods":["Copy"],"scopes":["b/*"]}}]"#
        ))
        .expect_err("unknown method must fail");
        assert!(matches!(err, PolicyError::UnknownMethod { .. }));
    }

    #[test]
    fn rejects_a_caller_with_no_grants() {
        assert!(matches!(
            Policy::parse(&format!(
                r#"[{{"name":"x","token":"{API_TOKEN}","methods":[],"scopes":["b/*"]}}]"#
            ))
            .expect_err("no methods must fail"),
            PolicyError::NoMethods(_)
        ));
        assert!(matches!(
            Policy::parse(&format!(
                r#"[{{"name":"x","token":"{API_TOKEN}","methods":["Stat"],"scopes":[]}}]"#
            ))
            .expect_err("no scopes must fail"),
            PolicyError::NoScopes(_)
        ));
    }

    #[test]
    fn rejects_an_empty_policy() {
        assert!(matches!(
            Policy::parse("[]").expect_err("empty policy must fail"),
            PolicyError::NoCallers
        ));
    }
}
