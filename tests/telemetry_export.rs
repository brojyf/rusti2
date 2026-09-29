use std::sync::{Arc, Mutex};

use axum::{body::Bytes, extract::State, routing::post, Router};
use tracing::Instrument;

type ReceivedExports = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_structured_logs_and_traces_on_shutdown() {
    let received = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let router = Router::new()
        .route(
            "/v1/{signal}",
            post(
                |axum::extract::Path(signal): axum::extract::Path<String>,
                 State(received): State<ReceivedExports>,
                 body: Bytes| async move {
                    received.lock().unwrap().push((signal, body.to_vec()));
                    (
                        [("content-type", "application/x-protobuf")],
                        Vec::<u8>::new(),
                    )
                },
            ),
        )
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let shutdown = rusti2::telemetry::setup(rusti2::telemetry::Config {
        service_name: "rusti2-export-test".into(),
        endpoint: format!("http://{address}"),
    });
    let request = http::Request::builder()
        .uri("/rusti2.v1.ObjectStorage/StatObject")
        .header(
            "traceparent",
            "00-11111111111111111111111111111111-2222222222222222-01",
        )
        .header("authorization", "Bearer must-not-be-exported")
        .body(())
        .unwrap();
    async {
        assert_eq!(
            rusti2::telemetry::trace_id(),
            "11111111111111111111111111111111"
        );
        tracing::info!(request_id = "grafana-test-request", "request completed");
    }
    .instrument(rusti2::telemetry::grpc_span(&request))
    .await;
    tracing::warn!(target: "opentelemetry_sdk", "exporter-diagnostic-must-stay-local");
    shutdown.shutdown().await;

    let received = received.lock().unwrap();
    for signal in ["logs", "traces"] {
        let payload = received
            .iter()
            .find(|(name, _)| name == signal)
            .unwrap_or_else(|| panic!("missing OTLP {signal} export"));
        assert!(payload
            .1
            .windows(b"rusti2-export-test".len())
            .any(|w| w == b"rusti2-export-test"));
        assert!(!payload
            .1
            .windows(b"must-not-be-exported".len())
            .any(|w| w == b"must-not-be-exported"));
        if signal == "traces" {
            assert!(payload.1.windows(16).any(|w| w == [0x11; 16]));
            assert!(payload.1.windows(8).any(|w| w == [0x22; 8]));
        }
        if signal == "logs" {
            assert!(payload
                .1
                .windows(b"grafana-test-request".len())
                .any(|w| w == b"grafana-test-request"));
            assert!(!payload
                .1
                .windows(b"exporter-diagnostic-must-stay-local".len())
                .any(|w| w == b"exporter-diagnostic-must-stay-local"));
        }
    }
    server.abort();
}
