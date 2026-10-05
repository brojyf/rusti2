//! Best-effort OTLP logs and traces for the object storage service.

use opentelemetry::trace::{TraceContextExt, TraceId, TracerProvider};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{logs::SdkLoggerProvider, trace::SdkTracerProvider, Resource};
use tracing_opentelemetry::{OpenTelemetryLayer, OpenTelemetrySpanExt};
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

struct RequestHeaders<'a>(&'a http::HeaderMap);

impl opentelemetry::propagation::Extractor for RequestHeaders<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.to_str().ok()
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect()
    }
}

/// Only the RPC route is recorded; headers, tokens and object payloads are not.
pub fn grpc_span<B>(request: &http::Request<B>) -> tracing::Span {
    use opentelemetry::propagation::TextMapPropagator;
    let parent = opentelemetry_sdk::propagation::TraceContextPropagator::new()
        .extract(&RequestHeaders(request.headers()));
    let span = tracing::info_span!(
        "grpc.request",
        otel.name = %request.uri().path(),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        rpc.system = "grpc",
        rpc.method = %request.uri().path(),
    );
    let _ = span.set_parent(parent);
    span
}

/// Config is what `setup` needs from the environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// `OTEL_SERVICE_NAME`, the resource `service.name` every span carries.
    pub service_name: String,
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` — the Collector's HTTP receiver.
    /// Empty disables export entirely (the right default for tests and local
    /// runs: no spans are sent anywhere, but the tracing layer still creates
    /// span contexts so trace_id is available for log correlation).
    pub endpoint: String,
}

/// Installs the global subscriber with a `tracing-opentelemetry` layer when
/// an endpoint is configured, or a plain fmt subscriber when it is not.
///
/// Returns a shutdown handle. Callers must call `shutdown()` before the
/// process exits so the batch exporter flushes remaining spans.
///
/// `setup` does not dial. The OTLP HTTP client connects lazily, so a
/// Collector that is down at boot delays nothing and fails nothing.
pub fn setup(cfg: Config) -> Shutdown {
    let endpoint = cfg.endpoint.trim().trim_end_matches('/').to_owned();
    if endpoint.is_empty() {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_timer(UtcTime::rfc_3339())
            .init();
        tracing::info!("OTLP telemetry export disabled; OTEL_EXPORTER_OTLP_ENDPOINT is unset");
        return Shutdown::none();
    }

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", endpoint))
        .with_timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(e) => e,
        Err(err) => {
            // A telemetry init failure must not prevent the process from
            // starting. Log and continue with only the fmt subscriber.
            tracing_subscriber::fmt()
                .with_target(false)
                .with_timer(UtcTime::rfc_3339())
                .init();
            tracing::warn!(
                "failed to create OTLP exporter, continuing without trace export: {err}"
            );
            return Shutdown::none();
        }
    };

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(cfg.service_name.clone())
                .build(),
        )
        .build();

    let tracer = tracer_provider.tracer(cfg.service_name.clone());
    let telemetry_layer = OpenTelemetryLayer::new(tracer);
    let logger_provider = match opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/logs"))
        .with_timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(exporter) => Some(
            SdkLoggerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(
                    Resource::builder()
                        .with_service_name(cfg.service_name.clone())
                        .build(),
                )
                .build(),
        ),
        Err(err) => {
            eprintln!("failed to create OTLP log exporter: {err}");
            None
        }
    };
    // Exporter diagnostics stay local: exporting them would create a feedback
    // loop whenever the Collector is unavailable.
    let logs_layer = logger_provider.as_ref().map(|provider| {
        OpenTelemetryTracingBridge::new(provider).with_filter(
            tracing_subscriber::filter::filter_fn(|metadata| {
                !["opentelemetry", "hyper", "reqwest", "h2"]
                    .iter()
                    .any(|prefix| metadata.target().starts_with(prefix))
            }),
        )
    });

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_timer(UtcTime::rfc_3339()),
        )
        .with(telemetry_layer)
        .with(logs_layer)
        .init();

    tracing::info!(
        endpoint = %endpoint,
        service_name = %cfg.service_name,
        "OTLP telemetry export enabled",
    );

    Shutdown {
        inner: Some(tracer_provider),
        logger_provider,
    }
}

/// Owns the tracer and logger providers. Dropping it is not guaranteed to
/// flush; call [`Shutdown::shutdown`] before exiting.
pub struct Shutdown {
    inner: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl Shutdown {
    fn none() -> Self {
        Self {
            inner: None,
            logger_provider: None,
        }
    }

    /// Flush and shut down the tracer provider. Safe to call when telemetry
    /// was never enabled.
    pub async fn shutdown(self) {
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(provider) = self.inner {
                let _ = provider.shutdown();
            }
            if let Some(provider) = self.logger_provider {
                let _ = provider.shutdown();
            }
        })
        .await;
    }
}

/// Returns the current trace id as a hex string, or `""` when there is no
/// valid span context.
///
/// This is what gets logged alongside `request_id` so the two identifiers
/// appear on the same line and a log platform can join them.
pub fn trace_id() -> String {
    let span = tracing::Span::current();
    if span.is_disabled() {
        return String::new();
    }
    let cx = span.context();
    let span_ref = cx.span();
    let trace = span_ref.span_context().trace_id();
    if trace == TraceId::INVALID {
        return String::new();
    }
    trace.to_string()
}
