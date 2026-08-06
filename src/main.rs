mod config;
mod service;

pub mod pb {
    tonic::include_proto!("rusti2.v1");
}

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use http::{Request, Response};
use tonic::transport::Server;
use tower::{Layer, Service};
use tracing::info;

use crate::config::Config;
use crate::pb::object_storage_server::ObjectStorageServer;
use crate::service::ObjectStorageService;

/// A [`Layer`] that intercepts `GET /api/health` and returns 200 OK.
#[derive(Clone)]
struct HealthLayer;

impl<S> Layer<S> for HealthLayer {
    type Service = HealthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HealthService { inner }
    }
}

#[derive(Clone)]
struct HealthService<S> {
    inner: S,
}

impl<S> Service<Request<tonic::body::Body>> for HealthService<S>
where
    S: Service<Request<tonic::body::Body>, Response = Response<tonic::body::Body>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<tonic::body::Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<tonic::body::Body>) -> Self::Future {
        if req.uri().path() == "/api/health" && req.method() == http::Method::GET {
            let resp = Response::builder()
                .status(200)
                .header("content-type", "text/plain")
                .body(tonic::body::Body::empty())
                .unwrap();
            return Box::pin(std::future::ready(Ok(resp)));
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env().map_err(std::io::Error::other)?;
    let addr = config.bind_addr.clone();

    let credentials = Credentials::new(
        config.r2_access_key_id.clone(),
        config.r2_secret_access_key.clone(),
        None,
        None,
        "rusti2-env",
    );
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("auto"))
        .endpoint_url(config.r2_endpoint())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    let s3 = aws_sdk_s3::Client::from_conf(s3_config);

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ObjectStorageServer<ObjectStorageService>>()
        .await;

    info!(%addr, buckets = ?config.allowed_buckets, "rusti2 listening");

    Server::builder()
        .layer(HealthLayer)
        .add_service(health_service)
        .add_service(ObjectStorageServer::new(ObjectStorageService::new(
            s3,
            config.clone(),
        )))
        .serve(addr.parse()?)
        .await?;

    Ok(())
}
