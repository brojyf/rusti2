mod config;
mod queues;
mod service;

pub mod pb {
    tonic::include_proto!("rusti2.v1");
}

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use tonic::transport::Server;
use tracing::info;

use crate::config::Config;
use crate::pb::object_storage_server::ObjectStorageServer;
use crate::pb::queues_server::QueuesServer;
use crate::queues::QueuesService;
use crate::service::ObjectStorageService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env().map_err(std::io::Error::other)?;
    let addr = config.bind_addr.parse()?;

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

    let queues_service = QueuesService::new(
        &config.cloudflare_account_id,
        config.cloudflare_api_token.clone(),
        config.queues.clone(),
    );

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ObjectStorageServer<ObjectStorageService>>()
        .await;
    health_reporter
        .set_serving::<QueuesServer<QueuesService>>()
        .await;

    info!(%addr, buckets = ?config.allowed_buckets, queues = ?config.queues.keys(), "rusti2 listening");
    Server::builder()
        .add_service(health_service)
        .add_service(ObjectStorageServer::new(ObjectStorageService::new(
            s3,
            config.clone(),
        )))
        .add_service(QueuesServer::new(queues_service))
        .serve(addr)
        .await?;

    Ok(())
}
