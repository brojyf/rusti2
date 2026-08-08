//! Caller authentication.
//!
//! Every request to `ObjectStorage` must carry a service token. The token
//! identifies which service is calling; [`crate::policy`] decides what that
//! service may do. Authentication happens once, in an interceptor, so no RPC
//! can be added later that forgets to do it — the handler receives a caller or
//! it is never invoked.
//!
//! The token travels in the standard `authorization: Bearer …` header. When
//! rusti2 moves behind mTLS the caller identity comes from the peer
//! certificate instead and only [`authenticate`] changes; the policy layer and
//! every handler stay as they are.

use std::sync::Arc;

use tonic::{Request, Status};

use crate::policy::{Caller, Policy};

const INVALID_SERVICE_TOKEN: &str = "invalid service token";

/// Rejects unauthenticated requests and attaches the resolved [`Caller`] to
/// everything else.
#[derive(Clone)]
pub struct ServiceTokenAuth {
    policy: Arc<Policy>,
}

impl ServiceTokenAuth {
    pub fn new(policy: Arc<Policy>) -> Self {
        Self { policy }
    }
}

impl tonic::service::Interceptor for ServiceTokenAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let caller = authenticate(&self.policy, &request)?;
        request.extensions_mut().insert(caller);
        Ok(request)
    }
}

fn authenticate<T>(policy: &Policy, request: &Request<T>) -> Result<Arc<Caller>, Status> {
    let header = request
        .metadata()
        .get("authorization")
        .ok_or_else(|| Status::unauthenticated(INVALID_SERVICE_TOKEN))?
        .to_str()
        .map_err(|_| Status::unauthenticated(INVALID_SERVICE_TOKEN))?;

    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or_else(|| Status::unauthenticated(INVALID_SERVICE_TOKEN))?
        .trim();

    // The failure is deliberately not specific about whether the token was
    // absent, malformed or simply unknown: a caller with a bad token has
    // nothing to learn from the difference, and an attacker does.
    policy
        .caller_for_token(token)
        .ok_or_else(|| Status::unauthenticated(INVALID_SERVICE_TOKEN))
}

/// Reads back the caller the interceptor attached.
///
/// A missing caller means the service was mounted without the interceptor,
/// which would silently serve every request unauthenticated. That is a wiring
/// bug rather than a client error, so it fails closed and loudly.
pub fn caller_of<T>(request: &Request<T>) -> Result<Arc<Caller>, Status> {
    request
        .extensions()
        .get::<Arc<Caller>>()
        .cloned()
        .ok_or_else(|| {
            tracing::error!("request reached a handler without an authenticated caller");
            Status::internal("authentication is not configured")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "api-token-000000000000000000";

    fn policy() -> Policy {
        Policy::parse(&format!(
            r#"[{{
              "name": "cotab-api",
              "token": "{TOKEN}",
              "methods": ["Stat"],
              "scopes": ["cotab-avatars/*"]
            }}]"#
        ))
        .expect("parse policy")
    }

    fn request_with(header: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(value) = header {
            request
                .metadata_mut()
                .insert("authorization", value.parse().expect("metadata value"));
        }
        request
    }

    #[test]
    fn accepts_a_known_bearer_token() {
        let caller = authenticate(&policy(), &request_with(Some(&format!("Bearer {TOKEN}"))))
            .expect("authenticate");
        assert_eq!(caller.name, "cotab-api");
    }

    #[test]
    fn accepts_a_lowercase_bearer_scheme() {
        assert!(authenticate(&policy(), &request_with(Some(&format!("bearer {TOKEN}")))).is_ok());
    }

    #[test]
    fn rejects_a_missing_token() {
        let status = authenticate(&policy(), &request_with(None)).expect_err("must reject");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_a_token_without_the_bearer_scheme() {
        let status = authenticate(&policy(), &request_with(Some(TOKEN))).expect_err("must reject");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_an_unknown_token() {
        let status = authenticate(&policy(), &request_with(Some("Bearer someone-elses-token")))
            .expect_err("must reject");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn authentication_failures_do_not_reveal_token_state() {
        let policy = policy();
        let statuses = [
            authenticate(&policy, &request_with(None)).expect_err("missing must reject"),
            authenticate(&policy, &request_with(Some(TOKEN))).expect_err("malformed must reject"),
            authenticate(&policy, &request_with(Some("Bearer unknown-token")))
                .expect_err("unknown must reject"),
        ];

        assert!(statuses
            .iter()
            .all(|status| status.message() == INVALID_SERVICE_TOKEN));
    }

    #[test]
    fn fails_closed_when_the_interceptor_is_not_mounted() {
        let status = caller_of(&Request::new(())).expect_err("must fail closed");
        assert_eq!(status.code(), tonic::Code::Internal);
    }
}
