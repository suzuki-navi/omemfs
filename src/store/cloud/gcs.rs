//! Google Cloud Storage cloud backend.
//!
//! Implements the synchronous [`CloudObjectIo`] boundary on top of the async
//! official `google-cloud-storage` client (the `Storage` data-plane client and
//! the `StorageControl` metadata client) by driving every call to completion on
//! a shared [`CloudRuntime`] via `block_on`. Async never leaks past this file.
//!
//! Per `design/13_cloud_backends.md`:
//! - bucket resource name is `projects/_/buckets/<bucket>` for the control-plane
//!   list/get; the data-plane `write_object`/`read_object` take the same
//!   resource name as the bucket argument.
//! - status detection is transport-aware (`has_status`): the data-plane client
//!   speaks gRPC, so a condition is matched against EITHER the REST HTTP status
//!   code OR the equivalent gRPC `Code` (404<->NotFound, 412<->FailedPrecondition
//!   for CAS, 409<->Aborted).
//! - CAS token: the object generation number (`i64`), stored as its decimal
//!   ASCII bytes for round-trip.

use std::sync::Arc;

use bytes::Bytes;
use google_cloud_gax::paginator::ItemPaginator as _;
use google_cloud_storage::client::{Storage, StorageControl};

use super::{CloudObjectIo, CloudRuntime};
use crate::error::Error;

/// GCS client pair + shared runtime.
#[derive(Clone)]
pub struct GcsBackend {
    storage: Storage,
    control: StorageControl,
    rt: Arc<CloudRuntime>,
    /// Bucket resource name: `projects/_/buckets/<bucket>`.
    bucket_resource: String,
}

impl GcsBackend {
    /// Build a GCS backend. Credentials come from inline service-account JSON,
    /// a service-account JSON file path, anonymous (for the storage-testbench
    /// emulator, selected when `endpoint` is set and no credentials are given),
    /// or Application Default Credentials.
    pub fn new(
        bucket: String,
        credentials_json: Option<String>,
        credentials_json_path: Option<String>,
        endpoint: Option<String>,
        rt: Arc<CloudRuntime>,
    ) -> Result<Self, Error> {
        let creds = build_credentials(credentials_json, credentials_json_path, endpoint.is_some())?;

        let (storage, control) = rt.block_on(async {
            let mut sb = Storage::builder();
            let mut cb = StorageControl::builder();
            if let Some(c) = &creds {
                sb = sb.with_credentials(c.clone());
                cb = cb.with_credentials(c.clone());
            }
            if let Some(ep) = &endpoint {
                sb = sb.with_endpoint(ep.clone());
                cb = cb.with_endpoint(ep.clone());
            }
            let storage = sb
                .build()
                .await
                .map_err(|e| Error::Other(format!("GCS Storage client build failed: {e:?}")))?;
            let control = cb.build().await.map_err(|e| {
                Error::Other(format!("GCS StorageControl client build failed: {e:?}"))
            })?;
            Ok::<_, Error>((storage, control))
        })?;

        Ok(GcsBackend {
            storage,
            control,
            rt,
            bucket_resource: format!("projects/_/buckets/{bucket}"),
        })
    }
}

/// Build the GCS credentials. Returns `None` for ADC (the client builds its own
/// default credentials when none are supplied).
fn build_credentials(
    inline: Option<String>,
    path: Option<String>,
    endpoint_set: bool,
) -> Result<Option<google_cloud_auth::credentials::Credentials>, Error> {
    use google_cloud_auth::credentials::anonymous::Builder as AnonBuilder;
    use google_cloud_auth::credentials::service_account::Builder as SaBuilder;

    let json = match (inline, path) {
        (Some(j), _) => Some(j),
        (None, Some(p)) => Some(
            std::fs::read_to_string(&p)
                .map_err(|e| Error::Other(format!("reading GCS credentials file {p}: {e}")))?,
        ),
        (None, None) => None,
    };

    if let Some(json) = json {
        let value: serde_json::Value = serde_json::from_str(&json)
            .map_err(|e| Error::Other(format!("invalid GCS service-account JSON: {e}")))?;
        let creds = SaBuilder::new(value)
            .build()
            .map_err(|e| Error::Other(format!("building GCS service-account creds: {e:?}")))?;
        return Ok(Some(creds));
    }

    // No explicit credentials: use anonymous for an emulator endpoint, else ADC.
    if endpoint_set {
        Ok(Some(AnonBuilder::new().build()))
    } else {
        Ok(None)
    }
}

/// `true` if a GCS error denotes the given HTTP status condition, recognising
/// BOTH transports the client may use: the REST/HTTP status code AND the
/// equivalent gRPC `Code` (the `google-cloud-storage` data-plane client speaks
/// gRPC, where `http_status_code()` is `None` and the condition is carried by
/// `status().code`). The mapping covers the codes this adapter checks:
///   404 Not Found            <-> `Code::NotFound`
///   409 Conflict             <-> `Code::Aborted`
///   412 Precondition Failed  <-> `Code::FailedPrecondition`
fn has_status(e: &google_cloud_storage::Error, status: u16) -> bool {
    if e.http_status_code() == Some(status) {
        return true;
    }
    use google_cloud_gax::error::rpc::Code;
    let want = match status {
        404 => Code::NotFound,
        409 => Code::Aborted,
        412 => Code::FailedPrecondition,
        _ => return false,
    };
    e.status().map(|s| s.code) == Some(want)
}

/// Encode a generation number as its decimal ASCII bytes (the CAS token).
fn gen_to_bytes(g: i64) -> Vec<u8> {
    g.to_string().into_bytes()
}

/// Decode a CAS token back to a generation number.
fn gen_from_bytes(b: &[u8]) -> Result<i64, Error> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| Error::Other("invalid GCS generation token".into()))
}

impl GcsBackend {
    /// Fetch object metadata via the control plane. `Ok(None)` on 404.
    fn get_object_meta(
        &self,
        key: &str,
    ) -> Result<Option<google_cloud_storage::model::Object>, Error> {
        self.rt.block_on(async {
            match self
                .control
                .get_object()
                .set_bucket(&self.bucket_resource)
                .set_object(key)
                .send()
                .await
            {
                Ok(obj) => Ok(Some(obj)),
                Err(e) if has_status(&e, 404) => Ok(None),
                Err(e) => Err(Error::Other(format!("GCS get_object failed: {e:?}"))),
            }
        })
    }
}

impl CloudObjectIo for GcsBackend {
    fn head_exists(&self, key: &str) -> Result<bool, Error> {
        Ok(self.get_object_meta(key)?.is_some())
    }

    fn head_size(&self, key: &str) -> Result<u64, Error> {
        match self.get_object_meta(key)? {
            Some(obj) => Ok(obj.size.max(0) as u64),
            None => Err(Error::ObjectNotFound(key.to_string())),
        }
    }

    fn list(&self, objects_prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut items = self
                .control
                .list_objects()
                .set_parent(&self.bucket_resource)
                .set_prefix(objects_prefix)
                .by_item();
            while let Some(item) = items.next().await {
                let obj =
                    item.map_err(|e| Error::Other(format!("GCS list_objects failed: {e:?}")))?;
                out.push((obj.name, obj.size.max(0) as u64));
            }
            Ok(out)
        })
    }

    fn get(&self, key: &str) -> Result<Bytes, Error> {
        self.rt.block_on(async {
            let mut resp = self
                .storage
                .read_object(&self.bucket_resource, key)
                .send()
                .await
                .map_err(|e| {
                    if has_status(&e, 404) {
                        Error::ObjectNotFound(key.to_string())
                    } else {
                        Error::Other(format!("GCS read_object failed: {e:?}"))
                    }
                })?;
            let mut buf = Vec::new();
            while let Some(chunk) = resp.next().await {
                let chunk = chunk
                    .map_err(|e| Error::Other(format!("GCS read_object stream failed: {e:?}")))?;
                buf.extend_from_slice(&chunk);
            }
            Ok(Bytes::from(buf))
        })
    }

    fn put(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        // Unconditional: the caller (CloudObjects::write_stream) already
        // decided this key is absent. See the CloudObjectIo::put doc comment.
        self.rt.block_on(async {
            self.storage
                .write_object(&self.bucket_resource, key, Bytes::from(body))
                .send_unbuffered()
                .await
                .map_err(|e| Error::Other(format!("GCS write_object failed: {e:?}")))?;
            Ok(())
        })
    }

    fn conditional_put(
        &self,
        key: &str,
        body: &[u8],
        expected_token: Option<&[u8]>,
    ) -> Result<Vec<u8>, Error> {
        // Absent -> if_generation_match(0) (create-if-missing);
        // Present(gen) -> if_generation_match(gen).
        let generation = match expected_token {
            None => 0,
            Some(t) => gen_from_bytes(t)?,
        };
        self.rt.block_on(async {
            match self
                .storage
                .write_object(&self.bucket_resource, key, Bytes::copy_from_slice(body))
                .set_if_generation_match(generation)
                .send_unbuffered()
                .await
            {
                Ok(obj) => Ok(gen_to_bytes(obj.generation)),
                Err(e) if has_status(&e, 412) => Err(Error::CasFailed),
                // GCS also returns 409 for "already exists" when generation==0.
                Err(e) if generation == 0 && has_status(&e, 409) => Err(Error::CasFailed),
                Err(e) => Err(Error::Other(format!(
                    "GCS conditional write_object failed: {e:?}"
                ))),
            }
        })
    }

    fn get_with_token(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        // Metadata first to capture the generation; then the bytes.
        let meta = match self.get_object_meta(key)? {
            Some(m) => m,
            None => return Ok(None),
        };
        let bytes = self.get(key)?;
        Ok(Some((bytes.to_vec(), gen_to_bytes(meta.generation))))
    }
}

// GCS's RootPointer is `super::CloudRootPointer<GcsBackend>` (refactor-
// instructions.md E10) -- see store/cloud/mod.rs for the shared implementation.

#[cfg(test)]
mod tests {
    use super::*;
    use google_cloud_gax::error::rpc::{Code, Status};

    /// Build a gRPC-style service error carrying the given status code (the
    /// shape the data-plane `StorageControl` client produces — `http_status_code`
    /// is `None`, the condition lives in `status().code`).
    fn grpc_err(code: Code) -> google_cloud_storage::Error {
        google_cloud_storage::Error::service(Status::default().set_code(code))
    }

    /// Build a REST/HTTP error carrying the given HTTP status code.
    fn http_err(status: u16) -> google_cloud_storage::Error {
        google_cloud_storage::Error::http(status, Default::default(), Default::default())
    }

    #[test]
    fn has_status_matches_http_status_codes() {
        assert!(has_status(&http_err(404), 404));
        assert!(has_status(&http_err(412), 412));
        assert!(has_status(&http_err(409), 409));
        assert!(!has_status(&http_err(500), 404));
    }

    #[test]
    fn has_status_matches_equivalent_grpc_codes() {
        // The gRPC control-plane error has no HTTP status, so detection must key
        // on the gRPC Code instead. This is the regression guard for the bug
        // where 404/412 went undetected over gRPC (NotFound treated as a hard
        // error instead of "absent"; FailedPrecondition not mapped to CasFailed).
        assert!(has_status(&grpc_err(Code::NotFound), 404));
        assert!(has_status(&grpc_err(Code::FailedPrecondition), 412));
        assert!(has_status(&grpc_err(Code::Aborted), 409));
    }

    #[test]
    fn has_status_rejects_mismatched_grpc_codes() {
        assert!(!has_status(&grpc_err(Code::NotFound), 412));
        assert!(!has_status(&grpc_err(Code::FailedPrecondition), 404));
        assert!(!has_status(&grpc_err(Code::Internal), 404));
        // Codes this adapter does not map always return false.
        assert!(!has_status(&grpc_err(Code::NotFound), 500));
    }
}
