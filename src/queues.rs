use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use tonic::{Request, Response, Status};
use tracing::error;

use crate::pb::queues_server::Queues;
use crate::pb::{
    AckMessagesRequest, AckMessagesResponse, PublishMessageRequest, PublishMessageResponse,
    PullMessagesRequest, PullMessagesResponse, QueueMessage,
};

const DEFAULT_BATCH_SIZE: u32 = 10;
const MAX_BATCH_SIZE: u32 = 100;
const DEFAULT_VISIBILITY_MS: u32 = 300_000;
const MIN_VISIBILITY_MS: u32 = 1_000;
const MAX_VISIBILITY_MS: u32 = 43_200_000;

pub struct QueuesService {
    http: reqwest::Client,
    /// https://api.cloudflare.com/client/v4/accounts/{account}/queues
    api_base: String,
    api_token: String,
    /// Logical queue name -> Cloudflare queue ID.
    queues: HashMap<String, String>,
}

impl QueuesService {
    pub fn new(account_id: &str, api_token: String, queues: HashMap<String, String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_base: format!(
                "https://api.cloudflare.com/client/v4/accounts/{account_id}/queues"
            ),
            api_token,
            queues,
        }
    }

    fn queue_id(&self, name: &str) -> Result<&str, Status> {
        if name.is_empty() {
            return Err(Status::invalid_argument("queue is required"));
        }
        self.queues
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| {
                Status::permission_denied(format!("queue {name:?} is not served by this instance"))
            })
    }

    async fn call(
        &self,
        op: &str,
        url: String,
        payload: &impl Serialize,
    ) -> Result<serde_json::Value, Status> {
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_token)
            .json(payload)
            .send()
            .await
            .map_err(|e| internal(op, e))?;

        let status = response.status();
        let body: CfEnvelope = response.json().await.map_err(|e| internal(op, e))?;
        if !status.is_success() || !body.success {
            error!(op, %status, errors = ?body.errors, "cloudflare queues call failed");
            return Err(Status::internal(format!("{op} failed")));
        }
        Ok(body.result)
    }
}

fn internal(op: &str, err: impl std::fmt::Debug) -> Status {
    error!(op, ?err, "cloudflare queues call failed");
    Status::internal(format!("{op} failed"))
}

#[derive(Deserialize)]
struct CfEnvelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    #[serde(default)]
    result: serde_json::Value,
}

#[derive(Deserialize)]
struct CfPullResult {
    #[serde(default)]
    messages: Vec<CfMessage>,
}

#[derive(Deserialize)]
struct CfMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    lease_id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    attempts: u32,
}

#[tonic::async_trait]
impl Queues for QueuesService {
    async fn publish_message(
        &self,
        request: Request<PublishMessageRequest>,
    ) -> Result<Response<PublishMessageResponse>, Status> {
        let req = request.into_inner();
        let queue_id = self.queue_id(&req.queue)?;

        // The HTTP publish API only accepts "text" and "json" content
        // types, so arbitrary bytes travel as base64 inside a text
        // message; pull_messages reverses this.
        let payload = serde_json::json!({
            "body": BASE64.encode(&req.body),
            "content_type": "text",
            "delay_seconds": req.delay_seconds,
        });
        self.call(
            "publish_message",
            format!("{}/{queue_id}/messages", self.api_base),
            &payload,
        )
        .await?;

        Ok(Response::new(PublishMessageResponse {}))
    }

    async fn pull_messages(
        &self,
        request: Request<PullMessagesRequest>,
    ) -> Result<Response<PullMessagesResponse>, Status> {
        let req = request.into_inner();
        let queue_id = self.queue_id(&req.queue)?;

        let batch_size = match req.batch_size {
            0 => DEFAULT_BATCH_SIZE,
            n => n.min(MAX_BATCH_SIZE),
        };
        let visibility_timeout_ms = match req.visibility_timeout_ms {
            0 => DEFAULT_VISIBILITY_MS,
            n => n.clamp(MIN_VISIBILITY_MS, MAX_VISIBILITY_MS),
        };

        let payload = serde_json::json!({
            "batch_size": batch_size,
            "visibility_timeout_ms": visibility_timeout_ms,
        });
        let result = self
            .call(
                "pull_messages",
                format!("{}/{queue_id}/messages/pull", self.api_base),
                &payload,
            )
            .await?;

        let pulled: CfPullResult =
            serde_json::from_value(result).map_err(|e| internal("pull_messages", e))?;

        let messages = pulled
            .messages
            .into_iter()
            .map(|m| {
                // Messages published by rusti2 are base64 text; fall
                // back to the raw string for foreign producers.
                let body = BASE64
                    .decode(&m.body)
                    .unwrap_or_else(|_| m.body.into_bytes());
                QueueMessage {
                    id: m.id,
                    lease_id: m.lease_id,
                    body,
                    attempts: m.attempts.max(1),
                }
            })
            .collect();

        Ok(Response::new(PullMessagesResponse { messages }))
    }

    async fn ack_messages(
        &self,
        request: Request<AckMessagesRequest>,
    ) -> Result<Response<AckMessagesResponse>, Status> {
        let req = request.into_inner();
        let queue_id = self.queue_id(&req.queue)?;

        if req.ack_lease_ids.is_empty() && req.retries.is_empty() {
            return Ok(Response::new(AckMessagesResponse {}));
        }

        let payload = serde_json::json!({
            "acks": req.ack_lease_ids.iter()
                .map(|lease_id| serde_json::json!({ "lease_id": lease_id }))
                .collect::<Vec<_>>(),
            "retries": req.retries.iter()
                .map(|r| serde_json::json!({
                    "lease_id": r.lease_id,
                    "delay_seconds": r.delay_seconds,
                }))
                .collect::<Vec<_>>(),
        });
        self.call(
            "ack_messages",
            format!("{}/{queue_id}/messages/ack", self.api_base),
            &payload,
        )
        .await?;

        Ok(Response::new(AckMessagesResponse {}))
    }
}
