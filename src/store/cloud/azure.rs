//! Azure Blob Storage cloud backend (Entra ID auth only).
//!
//! Implements the synchronous [`CloudObjectIo`] boundary on top of the async
//! `azure_storage_blob` client by driving every call to completion on a shared
//! [`CloudRuntime`] via `block_on`. Async never leaks past this file.
//!
//! Per `design/13_cloud_backends.md`:
//! - Auth is Entra ID only (`ClientSecretCredential` from tenant/client/secret).
//! - 404 detection: `err.http_status() == Some(NotFound)`.
//! - 412 detection (CAS): `err.http_status() == Some(PreconditionFailed)` ->
//!   `Error::CasFailed`.
//! - CAS token: the blob ETag string.

use std::sync::Arc;

use azure_core::credentials::TokenCredential;
use azure_core::http::{NoFormat, RequestContent, StatusCode, Url};
use azure_identity::ClientSecretCredential;
use azure_storage_blob::models::{
    BlobClientGetPropertiesResultHeaders as _, BlobClientUploadOptions,
    BlobContainerClientListBlobsOptions,
};
use azure_storage_blob::{BlobClient, BlobServiceClient};
use bytes::Bytes;
use futures::StreamExt as _;

use super::{CloudObjectIo, CloudRuntime};
use crate::error::Error;

/// Azure blob service client + shared runtime. `BlobServiceClient` is not
/// `Clone`, so this backend is never cloned; it is shared as
/// `Arc<dyn CloudObjectIo>` (and the root-pointer builds its own instance).
pub struct AzureBackend {
    service: BlobServiceClient,
    rt: Arc<CloudRuntime>,
    container: String,
}

impl AzureBackend {
    /// Build an Azure backend from the configured fields. Auth is Entra ID
    /// (`ClientSecretCredential`). `endpoint` overrides the default service URL
    /// `https://<account>.blob.core.windows.net`.
    pub fn new(
        account: String,
        container: String,
        tenant_id: String,
        client_id: String,
        client_secret: String,
        endpoint: Option<String>,
        rt: Arc<CloudRuntime>,
    ) -> Result<Self, Error> {
        let service_url =
            endpoint.unwrap_or_else(|| format!("https://{account}.blob.core.windows.net"));
        let url = Url::parse(&service_url)
            .map_err(|e| Error::Other(format!("invalid Azure service URL {service_url}: {e}")))?;

        let service = rt.block_on(async {
            let credential =
                ClientSecretCredential::new(&tenant_id, client_id, client_secret.into(), None)
                    .map_err(|e| {
                        Error::Other(format!("Azure ClientSecretCredential failed: {e:?}"))
                    })?;
            let credential: Arc<dyn TokenCredential> = credential;
            BlobServiceClient::new(url, Some(credential), None)
                .map_err(|e| Error::Other(format!("Azure BlobServiceClient failed: {e:?}")))
        })?;

        Ok(AzureBackend {
            service,
            rt,
            container,
        })
    }

    fn blob_client(&self, key: &str) -> BlobClient {
        self.service
            .blob_container_client(&self.container)
            .blob_client(key)
    }
}

/// `true` if an Azure error carries the given HTTP status code.
fn has_status(e: &azure_core::Error, status: StatusCode) -> bool {
    e.http_status() == Some(status)
}

/// Build the upload body (an unformatted byte payload) from owned bytes.
fn upload_body(body: Vec<u8>) -> RequestContent<Bytes, NoFormat> {
    RequestContent::from(body)
}

/// Default upload options.
fn upload_options() -> BlobClientUploadOptions<'static> {
    BlobClientUploadOptions::default()
}

impl CloudObjectIo for AzureBackend {
    fn head_exists(&self, key: &str) -> Result<bool, Error> {
        self.rt.block_on(async {
            self.blob_client(key)
                .exists()
                .await
                .map_err(|e| Error::Other(format!("Azure exists failed: {e:?}")))
        })
    }

    fn head_size(&self, key: &str) -> Result<u64, Error> {
        self.rt.block_on(async {
            match self.blob_client(key).get_properties(None).await {
                Ok(props) => {
                    let len = props
                        .content_length()
                        .map_err(|e| Error::Other(format!("Azure content_length: {e:?}")))?
                        .ok_or_else(|| {
                            Error::Other("Azure get_properties: no content_length".into())
                        })?;
                    Ok(len)
                }
                Err(e) if has_status(&e, StatusCode::NotFound) => {
                    Err(Error::ObjectNotFound(key.to_string()))
                }
                Err(e) => Err(Error::Other(format!("Azure get_properties failed: {e:?}"))),
            }
        })
    }

    fn list(&self, objects_prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        self.rt.block_on(async {
            let container = self.service.blob_container_client(&self.container);
            let mut out = Vec::new();
            let options = BlobContainerClientListBlobsOptions {
                prefix: Some(objects_prefix.to_string()),
                ..Default::default()
            };
            let mut pager = container
                .list_blobs(Some(options))
                .map_err(|e| Error::Other(format!("Azure list_blobs failed: {e:?}")))?;
            while let Some(blob) = pager.next().await {
                let blob =
                    blob.map_err(|e| Error::Other(format!("Azure list_blobs item failed: {e:?}")))?;
                let name = match blob.name {
                    Some(n) => n,
                    None => continue,
                };
                let size = blob.properties.and_then(|p| p.content_length).unwrap_or(0);
                out.push((name, size));
            }
            Ok(out)
        })
    }

    fn get(&self, key: &str) -> Result<Bytes, Error> {
        self.rt.block_on(async {
            let resp = self.blob_client(key).download(None).await.map_err(|e| {
                if has_status(&e, StatusCode::NotFound) {
                    Error::ObjectNotFound(key.to_string())
                } else {
                    Error::Other(format!("Azure download failed: {e:?}"))
                }
            })?;
            let bytes =
                resp.body.collect().await.map_err(|e| {
                    Error::Other(format!("Azure download body collect failed: {e:?}"))
                })?;
            Ok(bytes)
        })
    }

    fn put(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        // Unconditional: the caller (CloudObjects::write_stream) already
        // decided this key is absent. See the CloudObjectIo::put doc comment.
        self.rt.block_on(async {
            let options = upload_options();
            self.blob_client(key)
                .upload(upload_body(body), Some(options))
                .await
                .map_err(|e| Error::Other(format!("Azure upload failed: {e:?}")))?;
            Ok(())
        })
    }

    fn conditional_put(
        &self,
        key: &str,
        body: &[u8],
        expected_token: Option<&[u8]>,
    ) -> Result<Vec<u8>, Error> {
        let body = body.to_vec();
        self.rt.block_on(async {
            // Build upload options carrying the precondition.
            let mut options = upload_options();
            match expected_token {
                // Create-only: If-None-Match: * (must not already exist).
                None => {
                    options.if_none_match = Some(azure_core::http::Etag::from("*"));
                }
                // Update: If-Match: <etag> (current version must equal token).
                Some(etag) => {
                    let etag = String::from_utf8_lossy(etag).to_string();
                    options.if_match = Some(azure_core::http::Etag::from(etag));
                }
            }
            let resp = self
                .blob_client(key)
                .upload(upload_body(body), Some(options))
                .await
                .map_err(|e| {
                    if has_status(&e, StatusCode::PreconditionFailed)
                        || has_status(&e, StatusCode::Conflict)
                    {
                        Error::CasFailed
                    } else {
                        Error::Other(format!("Azure conditional upload failed: {e:?}"))
                    }
                })?;
            // The new ETag identifies the written version. It is carried as the
            // standard ETag response header.
            let etag = etag_from_headers(resp.raw_response.headers())
                .ok_or_else(|| Error::Other("Azure upload returned no ETag for CAS".into()))?;
            Ok(etag.into_bytes())
        })
    }

    fn get_with_token(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        self.rt.block_on(async {
            // Capture the ETag via get_properties (typed header accessor), then
            // download the bytes.
            let etag = match self.blob_client(key).get_properties(None).await {
                Ok(props) => props
                    .etag()
                    .map_err(|e| Error::Other(format!("Azure get_properties etag: {e:?}")))?
                    .ok_or_else(|| Error::Other("Azure get_properties returned no ETag".into()))?
                    .to_string()
                    .into_bytes(),
                Err(e) if has_status(&e, StatusCode::NotFound) => return Ok(None),
                Err(e) => return Err(Error::Other(format!("Azure get_properties failed: {e:?}"))),
            };
            let resp = self
                .blob_client(key)
                .download(None)
                .await
                .map_err(|e| Error::Other(format!("Azure download failed: {e:?}")))?;
            let bytes =
                resp.body.collect().await.map_err(|e| {
                    Error::Other(format!("Azure download body collect failed: {e:?}"))
                })?;
            Ok(Some((bytes.to_vec(), etag)))
        })
    }
}

/// Extract the ETag response header from an Azure response's headers.
fn etag_from_headers(headers: &azure_core::http::headers::Headers) -> Option<String> {
    headers
        .get_optional_str(&azure_core::http::headers::ETAG)
        .map(|s| s.to_string())
}

// Azure's RootPointer is `super::CloudRootPointer<AzureBackend>` (refactor-
// instructions.md E10) -- see store/cloud/mod.rs for the shared implementation.
