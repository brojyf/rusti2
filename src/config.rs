use std::env;
use std::sync::Arc;

use crate::policy::Policy;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub cloudflare_account_id: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    /// Who may call this instance and what each of them may do. There is no
    /// separate global bucket allowlist: a bucket is reachable exactly when
    /// some caller is granted a scope in it, which keeps the answer to "who
    /// can delete this object" in one place.
    pub policy: Arc<Policy>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let policy = Policy::parse(&required("RUSTI2_CALLERS")?).map_err(|e| e.to_string())?;

        Ok(Self {
            bind_addr: env::var("RUSTI2_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3002".into()),
            cloudflare_account_id: required("CLOUDFLARE_ACCOUNT_ID")?,
            r2_access_key_id: required("R2_ACCESS_KEY_ID")?,
            r2_secret_access_key: required("R2_SECRET_ACCESS_KEY")?,
            policy: Arc::new(policy),
        })
    }

    pub fn r2_endpoint(&self) -> String {
        format!(
            "https://{}.r2.cloudflarestorage.com",
            self.cloudflare_account_id
        )
    }
}

fn required(name: &str) -> Result<String, String> {
    match env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!("missing required env var {name}")),
    }
}
