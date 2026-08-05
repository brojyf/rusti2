use std::collections::{HashMap, HashSet};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub cloudflare_account_id: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    /// Buckets this instance is allowed to serve. Requests naming any
    /// other bucket are rejected with PERMISSION_DENIED.
    pub allowed_buckets: HashSet<String>,
    /// Cloudflare API token with Queues read/write; required when any
    /// queue is configured.
    pub cloudflare_api_token: String,
    /// Logical queue name -> Cloudflare queue ID.
    pub queues: HashMap<String, String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let allowed_buckets: HashSet<String> = required("RUSTI2_ALLOWED_BUCKETS")?
            .split(',')
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect();
        if allowed_buckets.is_empty() {
            return Err("RUSTI2_ALLOWED_BUCKETS must list at least one bucket".into());
        }

        // RUSTI2_QUEUES maps logical names to queue IDs:
        //   avatar-moderation=<queue-id>,other-queue=<queue-id>
        let mut queues = HashMap::new();
        for entry in env::var("RUSTI2_QUEUES").unwrap_or_default().split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (name, id) = entry
                .split_once('=')
                .ok_or_else(|| format!("RUSTI2_QUEUES entry {entry:?} must be name=queue-id"))?;
            queues.insert(name.trim().to_string(), id.trim().to_string());
        }

        let cloudflare_api_token = if queues.is_empty() {
            env::var("CLOUDFLARE_API_TOKEN").unwrap_or_default()
        } else {
            required("CLOUDFLARE_API_TOKEN")?
        };

        Ok(Self {
            bind_addr: env::var("RUSTI2_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:50051".into()),
            cloudflare_account_id: required("CLOUDFLARE_ACCOUNT_ID")?,
            r2_access_key_id: required("R2_ACCESS_KEY_ID")?,
            r2_secret_access_key: required("R2_SECRET_ACCESS_KEY")?,
            allowed_buckets,
            cloudflare_api_token,
            queues,
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
