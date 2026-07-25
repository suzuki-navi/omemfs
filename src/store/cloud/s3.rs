//! AWS S3 (and S3-compatible, e.g. MinIO) cloud backend.
//!
//! Implements the synchronous [`CloudObjectIo`] boundary on top of the async
//! `aws-sdk-s3` client by driving every call to completion on a shared
//! [`CloudRuntime`] via `block_on`. Async never leaks past this file. The
//! storage-key HMAC stays in `LocalStore`; this adapter only ever sees
//! already-formed cloud keys.
//!
//! Per `design/13_cloud_backends.md`:
//! - 404 detection: the typed `is_not_found()` on the head/get service error.
//! - 412 detection (CAS): the HTTP status of the put error == 412 ->
//!   `Error::CasFailed`.
//! - CAS token: the object ETag string (quotes preserved for round-trip).

use std::sync::Arc;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;

use super::{CloudObjectIo, CloudRuntime, MULTIPART_THRESHOLD};
use crate::error::Error;

/// Part size for multipart uploads. S3 requires every part except the last to
/// be at least 5 MiB; [`MULTIPART_THRESHOLD`] (16 MiB) safely exceeds that.
const MULTIPART_PART_SIZE: usize = MULTIPART_THRESHOLD;

/// S3 client + shared runtime. Cheaply cloneable (both fields are `Arc`-like).
#[derive(Clone)]
pub struct S3Backend {
    client: Client,
    rt: Arc<CloudRuntime>,
    bucket: String,
}

impl S3Backend {
    /// Build an S3 backend from the configured fields. Static credentials are
    /// used when both `access_key_id` and `secret_access_key` are present;
    /// otherwise the AWS default credential chain is used. `endpoint` +
    /// `force_path_style` target an S3-compatible store such as MinIO.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bucket: String,
        region: String,
        access_key_id: Option<String>,
        secret_access_key: Option<String>,
        endpoint: Option<String>,
        force_path_style: Option<bool>,
        rt: Arc<CloudRuntime>,
    ) -> Result<Self, Error> {
        let client = rt.block_on(async {
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(Region::new(region));
            if let (Some(ak), Some(sk)) = (access_key_id, secret_access_key) {
                let creds = Credentials::new(ak, sk, None, None, "omemfs-static");
                loader = loader.credentials_provider(creds);
            }
            if let Some(ep) = &endpoint {
                loader = loader.endpoint_url(ep.clone());
            }
            let sdk_config = loader.load().await;
            let mut builder = aws_sdk_s3::config::Builder::from(&sdk_config);
            if force_path_style.unwrap_or(false) {
                builder = builder.force_path_style(true);
            }
            Client::from_conf(builder.build())
        });
        Ok(S3Backend { client, rt, bucket })
    }

    /// Single-PUT upload of `body`.
    async fn put_single(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| s3_err("put_object", e))?;
        Ok(())
    }

    /// Multipart upload of `body`, splitting into [`MULTIPART_PART_SIZE`] parts.
    /// Aborts the upload on any error so no orphaned parts accrue charges.
    async fn put_multipart(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        let create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| s3_err("create_multipart_upload", e))?;
        let upload_id = create
            .upload_id()
            .ok_or_else(|| Error::Other("S3 create_multipart_upload returned no upload id".into()))?
            .to_string();

        // Convert to a refcounted buffer once; parts are zero-copy slices of it.
        let body = Bytes::from(body);
        let result = self.upload_parts(key, &upload_id, &body).await;
        match result {
            Ok(parts) => {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .map_err(|e| s3_err("complete_multipart_upload", e))?;
                Ok(())
            }
            Err(e) => {
                // Best-effort abort; report the original error.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        body: &Bytes,
    ) -> Result<Vec<CompletedPart>, Error> {
        let mut parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut offset = 0usize;
        while offset < body.len() {
            let end = (offset + MULTIPART_PART_SIZE).min(body.len());
            // Refcounted slice of the shared buffer — no per-part copy (the old
            // `chunk.to_vec()` transiently doubled a large object's buffer).
            let part = body.slice(offset..end);
            offset = end;
            let resp = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(part))
                .send()
                .await
                .map_err(|e| s3_err("upload_part", e))?;
            let mut cp = CompletedPart::builder().part_number(part_number);
            if let Some(etag) = resp.e_tag() {
                cp = cp.e_tag(etag);
            }
            parts.push(cp.build());
            part_number += 1;
        }
        Ok(parts)
    }
}

impl CloudObjectIo for S3Backend {
    fn head_exists(&self, key: &str) -> Result<bool, Error> {
        self.rt.block_on(async {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(e) => {
                    if e.as_service_error()
                        .map(|se| se.is_not_found())
                        .unwrap_or(false)
                    {
                        Ok(false)
                    } else {
                        Err(s3_err("head_object", e))
                    }
                }
            }
        })
    }

    fn head_size(&self, key: &str) -> Result<u64, Error> {
        self.rt.block_on(async {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(out) => Ok(out.content_length().unwrap_or(0).max(0) as u64),
                Err(e) => {
                    if e.as_service_error()
                        .map(|se| se.is_not_found())
                        .unwrap_or(false)
                    {
                        Err(Error::ObjectNotFound(key.to_string()))
                    } else {
                        Err(s3_err("head_object", e))
                    }
                }
            }
        })
    }

    fn list(&self, objects_prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut paginator = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(objects_prefix)
                .into_paginator()
                .send();
            while let Some(page) = paginator.next().await {
                let page = page.map_err(|e| s3_err("list_objects_v2", e))?;
                for obj in page.contents() {
                    if let Some(k) = obj.key() {
                        let size = obj.size().unwrap_or(0).max(0) as u64;
                        out.push((k.to_string(), size));
                    }
                }
            }
            Ok(out)
        })
    }

    fn get(&self, key: &str) -> Result<Bytes, Error> {
        self.rt.block_on(async {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| {
                    if e.as_service_error()
                        .map(|se| se.is_no_such_key())
                        .unwrap_or(false)
                    {
                        Error::ObjectNotFound(key.to_string())
                    } else {
                        s3_err("get_object", e)
                    }
                })?;
            let agg = resp
                .body
                .collect()
                .await
                .map_err(|e| Error::Other(format!("S3 get_object body collect failed: {e}")))?;
            Ok(agg.into_bytes())
        })
    }

    fn put(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        // Unconditional: the caller (CloudObjects::write_stream) already
        // decided this key is absent. See the CloudObjectIo::put doc comment.
        if body.len() <= MULTIPART_THRESHOLD {
            self.rt.block_on(self.put_single(key, body))
        } else {
            self.rt.block_on(self.put_multipart(key, body))
        }
    }

    fn conditional_put(
        &self,
        key: &str,
        body: &[u8],
        expected_token: Option<&[u8]>,
    ) -> Result<Vec<u8>, Error> {
        self.rt.block_on(async {
            let mut req = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from(body.to_vec()));
            match expected_token {
                // Create-only: must not already exist.
                None => req = req.if_none_match("*"),
                // Update: the current ETag must still equal `etag`.
                Some(etag) => {
                    let etag = String::from_utf8_lossy(etag).to_string();
                    req = req.if_match(etag);
                }
            }
            match req.send().await {
                Ok(out) => {
                    let etag = out
                        .e_tag()
                        .ok_or_else(|| {
                            Error::Other("S3 put_object returned no ETag for CAS".into())
                        })?
                        .to_string();
                    Ok(etag.into_bytes())
                }
                Err(e) => {
                    if put_is_precondition_failed(&e) {
                        Err(Error::CasFailed)
                    } else {
                        Err(s3_err("conditional put_object", e))
                    }
                }
            }
        })
    }

    fn get_with_token(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        self.rt.block_on(async {
            match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(resp) => {
                    let etag = resp
                        .e_tag()
                        .ok_or_else(|| Error::Other("S3 get_object returned no ETag".into()))?
                        .to_string();
                    let agg = resp.body.collect().await.map_err(|e| {
                        Error::Other(format!("S3 get_object body collect failed: {e}"))
                    })?;
                    Ok(Some((agg.into_bytes().to_vec(), etag.into_bytes())))
                }
                Err(e) => {
                    if e.as_service_error()
                        .map(|se| se.is_no_such_key())
                        .unwrap_or(false)
                    {
                        Ok(None)
                    } else {
                        Err(s3_err("get_object", e))
                    }
                }
            }
        })
    }
}

/// Map any S3 SDK error to a generic storage I/O error, tagged with the op name.
fn s3_err<E: std::fmt::Debug, R: std::fmt::Debug>(
    op: &str,
    e: aws_sdk_s3::error::SdkError<E, R>,
) -> Error {
    Error::Other(format!("S3 {op} failed: {e:?}"))
}

/// Detect a `412 Precondition Failed` on a `put_object` error (the ETag /
/// If-None-Match / If-Match CAS path). The HTTP status comes from the raw
/// response carried by the SDK error (present on a service error). S3 also
/// returns 409 Conflict for a concurrent create race; treat both as CAS.
fn put_is_precondition_failed<E>(
    e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>,
) -> bool {
    use aws_sdk_s3::error::SdkError;
    if let SdkError::ServiceError(ctx) = e {
        let code = ctx.raw().status().as_u16();
        return code == 412 || code == 409;
    }
    false
}

// S3's RootPointer is `super::CloudRootPointer<S3Backend>` (refactor-
// instructions.md E10) -- see store/cloud/mod.rs for the shared implementation.
