use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{retry::RetryConfig, Region};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use rusti2::pb::object_storage_server::ObjectStorage;
use rusti2::pb::DownloadObjectRequest;
use rusti2::policy::Policy;
use rusti2::service::ObjectStorageService;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tracing::Instrument;
use tracing_subscriber::{layer::Context, prelude::*, registry::LookupSpan, Layer};

struct ErrorTraceCapture(mpsc::UnboundedSender<String>);

impl<S> Layer<S> for ErrorTraceCapture
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "rusti2::service"
            && *event.metadata().level() == tracing::Level::ERROR
        {
            let _ = self.0.send(rusti2::telemetry::trace_id());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_download_body_logs_the_request_trace() {
    let provider = SdkTracerProvider::builder().build();
    let (logs_tx, mut logs_rx) = mpsc::unbounded_channel();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("download-test")))
        .with(ErrorTraceCapture(logs_tx));
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (close_tx, close_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        let mut byte = [0];
        while !headers.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.unwrap();
            headers.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: image/png\r\n\r\nx")
            .await
            .unwrap();
        // Close only after GetObject returned its streaming response, so the
        // error occurs in the background body reader rather than the RPC setup.
        close_rx.await.unwrap();
    });
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("auto"))
        .endpoint_url(format!("http://{address}"))
        .credentials_provider(Credentials::new(
            "test",
            "test",
            None,
            None,
            "download-test",
        ))
        .retry_config(RetryConfig::disabled())
        .force_path_style(true)
        .build();
    let service = ObjectStorageService::new(aws_sdk_s3::Client::from_conf(config));
    let policy = Policy::parse(
        r#"[{"name":"reader","token":"reader-test-token-000000000000","methods":["Download"],"scopes":["pending/*"]}]"#,
    )
    .unwrap();
    let mut request = tonic::Request::new(DownloadObjectRequest {
        bucket: "pending".into(),
        key: "upload".into(),
    });
    request.extensions_mut().insert(
        policy
            .caller_for_token("reader-test-token-000000000000")
            .unwrap(),
    );
    let http_request = http::Request::builder()
        .uri("/rusti2.v1.ObjectStorage/DownloadObject")
        .header(
            "traceparent",
            "00-11111111111111111111111111111111-2222222222222222-01",
        )
        .body(())
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        service
            .download_object(request)
            .instrument(rusti2::telemetry::grpc_span(&http_request)),
    )
    .await
    .unwrap()
    .unwrap();
    close_tx.send(()).unwrap();
    let mut stream = response.into_inner();
    let error = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    let trace_id = tokio::time::timeout(Duration::from_secs(5), logs_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(trace_id, "11111111111111111111111111111111");
    server.await.unwrap();
    provider.shutdown().unwrap();
}
