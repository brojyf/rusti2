use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info};

use crate::config::Config;
use crate::pb::object_storage_server::ObjectStorage;
use crate::pb::upload_object_request::Payload;
use crate::pb::{
    DeleteObjectRequest, DeleteObjectResponse, DownloadObjectChunk, DownloadObjectRequest,
    PresignPutObjectRequest, PresignPutObjectResponse, StatObjectRequest, StatObjectResponse,
    UploadObjectRequest, UploadObjectResponse,
};

const CHUNK_BYTES: usize = 1024 * 1024;
/// Uploads are buffered in memory before the R2 put; refuse anything larger.
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_PRESIGN_SECS: u64 = 900;
const MIN_PRESIGN_SECS: u64 = 60;
const MAX_PRESIGN_SECS: u64 = 3600;

pub struct ObjectStorageService {
    s3: aws_sdk_s3::Client,
    config: Config,
}

impl ObjectStorageService {
    pub fn new(s3: aws_sdk_s3::Client, config: Config) -> Self {
        Self { s3, config }
    }

    fn check_bucket(&self, bucket: &str) -> Result<(), Status> {
        if bucket.is_empty() {
            return Err(Status::invalid_argument("bucket is required"));
        }
        if !self.config.allowed_buckets.contains(bucket) {
            return Err(Status::permission_denied(format!(
                "bucket {bucket:?} is not served by this instance"
            )));
        }
        Ok(())
    }
}

fn check_key(key: &str) -> Result<(), Status> {
    if key.is_empty() {
        return Err(Status::invalid_argument("key is required"));
    }
    Ok(())
}

fn internal(op: &str, err: impl std::fmt::Debug) -> Status {
    error!(op, ?err, "r2 operation failed");
    Status::internal(format!("{op} failed"))
}

#[tonic::async_trait]
impl ObjectStorage for ObjectStorageService {
    async fn presign_put_object(
        &self,
        request: Request<PresignPutObjectRequest>,
    ) -> Result<Response<PresignPutObjectResponse>, Status> {
        let req = request.into_inner();
        self.check_bucket(&req.bucket)?;
        check_key(&req.key)?;
        if req.content_type.is_empty() {
            return Err(Status::invalid_argument("content_type is required"));
        }

        let expires_secs = match u64::from(req.expires_in_seconds) {
            0 => DEFAULT_PRESIGN_SECS,
            s => s.clamp(MIN_PRESIGN_SECS, MAX_PRESIGN_SECS),
        };
        let expires_in = Duration::from_secs(expires_secs);
        let presigning = PresigningConfig::expires_in(expires_in)
            .map_err(|e| internal("presign_put_object", e))?;

        let presigned = self
            .s3
            .put_object()
            .bucket(&req.bucket)
            .key(&req.key)
            .content_type(&req.content_type)
            .presigned(presigning)
            .await
            .map_err(|e| internal("presign_put_object", e))?;

        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|now| (now + expires_in).as_secs() as i64)
            .unwrap_or_default();

        Ok(Response::new(PresignPutObjectResponse {
            url: presigned.uri().to_string(),
            expires_at,
        }))
    }

    async fn stat_object(
        &self,
        request: Request<StatObjectRequest>,
    ) -> Result<Response<StatObjectResponse>, Status> {
        let req = request.into_inner();
        self.check_bucket(&req.bucket)?;
        check_key(&req.key)?;

        let head = self
            .s3
            .head_object()
            .bucket(&req.bucket)
            .key(&req.key)
            .send()
            .await
            .map_err(|e| match e.as_service_error() {
                Some(svc) if svc.is_not_found() => {
                    Status::not_found(format!("object {:?} not found", req.key))
                }
                _ => internal("stat_object", e),
            })?;

        Ok(Response::new(StatObjectResponse {
            size_bytes: head.content_length().unwrap_or_default(),
            content_type: head.content_type().unwrap_or_default().to_string(),
            etag: head.e_tag().unwrap_or_default().to_string(),
        }))
    }

    type DownloadObjectStream = ReceiverStream<Result<DownloadObjectChunk, Status>>;

    async fn download_object(
        &self,
        request: Request<DownloadObjectRequest>,
    ) -> Result<Response<Self::DownloadObjectStream>, Status> {
        let req = request.into_inner();
        self.check_bucket(&req.bucket)?;
        check_key(&req.key)?;

        let object = self
            .s3
            .get_object()
            .bucket(&req.bucket)
            .key(&req.key)
            .send()
            .await
            .map_err(|e| match e.as_service_error() {
                Some(svc) if svc.is_no_such_key() => {
                    Status::not_found(format!("object {:?} not found", req.key))
                }
                _ => internal("download_object", e),
            })?;

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            let mut body = object.body;
            let mut pending: Vec<u8> = Vec::new();
            loop {
                match body.try_next().await {
                    Ok(Some(bytes)) => {
                        pending.extend_from_slice(&bytes);
                        while pending.len() >= CHUNK_BYTES {
                            let rest = pending.split_off(CHUNK_BYTES);
                            let chunk = std::mem::replace(&mut pending, rest);
                            if tx
                                .send(Ok(DownloadObjectChunk { body: chunk }))
                                .await
                                .is_err()
                            {
                                return; // client went away
                            }
                        }
                    }
                    Ok(None) => {
                        if !pending.is_empty() {
                            let _ = tx.send(Ok(DownloadObjectChunk { body: pending })).await;
                        }
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(internal("download_object", e))).await;
                        return;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn upload_object(
        &self,
        request: Request<Streaming<UploadObjectRequest>>,
    ) -> Result<Response<UploadObjectResponse>, Status> {
        let mut stream = request.into_inner();

        let metadata = match stream.message().await? {
            Some(UploadObjectRequest {
                payload: Some(Payload::Metadata(m)),
            }) => m,
            _ => {
                return Err(Status::invalid_argument(
                    "first message must carry metadata",
                ))
            }
        };
        self.check_bucket(&metadata.bucket)?;
        check_key(&metadata.key)?;

        let mut buf: Vec<u8> = Vec::new();
        while let Some(msg) = stream.message().await? {
            match msg.payload {
                Some(Payload::Body(bytes)) => {
                    if buf.len() + bytes.len() > MAX_UPLOAD_BYTES {
                        return Err(Status::invalid_argument(format!(
                            "upload exceeds {MAX_UPLOAD_BYTES} byte limit"
                        )));
                    }
                    buf.extend_from_slice(&bytes);
                }
                _ => return Err(Status::invalid_argument("expected body chunk")),
            }
        }

        let size_bytes = buf.len() as i64;
        let put = self
            .s3
            .put_object()
            .bucket(&metadata.bucket)
            .key(&metadata.key)
            .content_type(&metadata.content_type)
            .body(ByteStream::from(buf))
            .send()
            .await
            .map_err(|e| internal("upload_object", e))?;

        info!(bucket = %metadata.bucket, key = %metadata.key, size_bytes, "object uploaded");
        Ok(Response::new(UploadObjectResponse {
            etag: put.e_tag().unwrap_or_default().to_string(),
            size_bytes,
        }))
    }

    async fn delete_object(
        &self,
        request: Request<DeleteObjectRequest>,
    ) -> Result<Response<DeleteObjectResponse>, Status> {
        let req = request.into_inner();
        self.check_bucket(&req.bucket)?;
        check_key(&req.key)?;

        // S3/R2 DeleteObject succeeds even when the key does not exist,
        // which matches the idempotent contract in the proto.
        self.s3
            .delete_object()
            .bucket(&req.bucket)
            .key(&req.key)
            .send()
            .await
            .map_err(|e| internal("delete_object", e))?;

        Ok(Response::new(DeleteObjectResponse {}))
    }
}
