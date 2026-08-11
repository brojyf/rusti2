use std::sync::Arc;
use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use rusti2::auth::ServiceTokenAuth;
use rusti2::pb::object_storage_client::ObjectStorageClient;
use rusti2::pb::object_storage_server::ObjectStorageServer;
use rusti2::pb::upload_object_request::Payload;
use rusti2::pb::{upload_object_request::Metadata, PresignPutObjectRequest, UploadObjectRequest};
use rusti2::policy::Policy;
use rusti2::service::ObjectStorageService;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Server};
use tonic::{Code, Request};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;

const WRITER_TOKEN: &str = "writer-token-0000000000000000";
const READER_TOKEN: &str = "reader-token-0000000000000000";

fn policy() -> Arc<Policy> {
    Arc::new(
        Policy::parse(&format!(
            r#"[
              {{
                "name": "writer",
                "token": "{WRITER_TOKEN}",
                "methods": ["PresignPut", "Upload"],
                "scopes": ["shared/tenant-a/"]
              }},
              {{
                "name": "reader",
                "token": "{READER_TOKEN}",
                "methods": ["Stat"],
                "scopes": ["shared/tenant-a/"]
              }}
            ]"#
        ))
        .expect("parse test policy"),
    )
}

fn s3_client() -> aws_sdk_s3::Client {
    let credentials = Credentials::new("test", "test", None, None, "authz-test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("auto"))
        .endpoint_url("https://example.invalid")
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

fn request<T>(message: T, authorization: Option<&str>) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(authorization) = authorization {
        request.metadata_mut().insert(
            "authorization",
            authorization.parse().expect("valid authorization metadata"),
        );
    }
    request
}

fn presign(bucket: &str, key: &str) -> PresignPutObjectRequest {
    PresignPutObjectRequest {
        bucket: bucket.into(),
        key: key.into(),
        content_type: "image/jpeg".into(),
        expires_in_seconds: 60,
    }
}

async fn start_server() -> (
    Channel,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server address");
    let incoming = TcpListenerStream::new(listener);

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ObjectStorageServer<ObjectStorageService>>()
        .await;

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(health_service)
            .add_service(InterceptedService::new(
                ObjectStorageServer::new(ObjectStorageService::new(s3_client())),
                ServiceTokenAuth::new(policy()),
            ))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("valid server URI")
        .connect()
        .await
        .expect("connect test client");
    (channel, shutdown_tx, server)
}

#[tokio::test]
async fn caller_authentication_and_authorization_are_enforced_end_to_end() {
    let (channel, shutdown, server) = start_server().await;
    let mut storage = ObjectStorageClient::new(channel.clone());

    let missing = storage
        .presign_put_object(request(presign("shared", "tenant-a/x"), None))
        .await
        .expect_err("missing token must fail");
    assert_eq!(missing.code(), Code::Unauthenticated);

    let malformed = storage
        .presign_put_object(request(presign("shared", "tenant-a/x"), Some(WRITER_TOKEN)))
        .await
        .expect_err("non-bearer token must fail");
    assert_eq!(malformed.code(), Code::Unauthenticated);

    let unknown = storage
        .presign_put_object(request(
            presign("shared", "tenant-a/x"),
            Some("Bearer unknown-token-000000000000"),
        ))
        .await
        .expect_err("unknown token must fail");
    assert_eq!(unknown.code(), Code::Unauthenticated);
    assert_eq!(missing.message(), malformed.message());
    assert_eq!(malformed.message(), unknown.message());

    storage
        .presign_put_object(request(
            presign("shared", "tenant-a/x"),
            Some(&format!("Bearer {WRITER_TOKEN}")),
        ))
        .await
        .expect("granted method and scope must succeed");

    let wrong_bucket = storage
        .presign_put_object(request(
            presign("other", "tenant-a/x"),
            Some(&format!("Bearer {WRITER_TOKEN}")),
        ))
        .await
        .expect_err("ungranted bucket must fail");
    assert_eq!(wrong_bucket.code(), Code::PermissionDenied);

    let wrong_method = storage
        .presign_put_object(request(
            presign("shared", "tenant-a/x"),
            Some(&format!("Bearer {READER_TOKEN}")),
        ))
        .await
        .expect_err("ungranted method must fail");
    assert_eq!(wrong_method.code(), Code::PermissionDenied);

    let wrong_prefix = storage
        .presign_put_object(request(
            presign("shared", "tenant-b/x"),
            Some(&format!("Bearer {WRITER_TOKEN}")),
        ))
        .await
        .expect_err("ungranted key prefix must fail");
    assert_eq!(wrong_prefix.code(), Code::PermissionDenied);

    // Keep the stream open after its metadata. A handler that tries to read a
    // body chunk before denying this prefix will hang until the timeout.
    let (body_tx, body_rx) = mpsc::channel(1);
    body_tx
        .send(UploadObjectRequest {
            payload: Some(Payload::Metadata(Metadata {
                bucket: "shared".into(),
                key: "tenant-b/x".into(),
                content_type: "image/jpeg".into(),
                cache_control: String::new(),
            })),
        })
        .await
        .expect("queue upload metadata");
    let denied_upload = timeout(
        Duration::from_secs(1),
        storage.upload_object(request(
            ReceiverStream::new(body_rx),
            Some(&format!("Bearer {WRITER_TOKEN}")),
        )),
    )
    .await
    .expect("server tried to read the upload body before authorizing")
    .expect_err("ungranted upload must fail");
    assert_eq!(denied_upload.code(), Code::PermissionDenied);
    drop(body_tx);

    let mut health = HealthClient::new(channel.clone());
    let health_response = health
        .check(HealthCheckRequest {
            service: "rusti2.v1.ObjectStorage".into(),
        })
        .await
        .expect("health must be callable without a token")
        .into_inner();
    assert_eq!(health_response.status, ServingStatus::Serving as i32);

    drop(health);
    drop(storage);
    drop(channel);
    shutdown.send(()).expect("shut down test server");
    timeout(Duration::from_secs(1), server)
        .await
        .expect("test server shutdown timed out")
        .expect("test server task panicked")
        .expect("test server failed");
}
