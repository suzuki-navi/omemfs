//! In-memory cloud fakes for tests.
//!
//! [`MemCloud`] implements the per-backend [`CloudObjectIo`] boundary entirely
//! in memory with a monotonic version token per key, and [`MemCloudRootPointer`]
//! implements [`RootPointer`] on top of it with the same opaque-token +
//! 412-on-precondition CAS model the real cloud backends use:
//!
//! - `expected = Absent`  => create-only (fail if the key already exists),
//! - `expected = Present(token)` => compare the stored token to `token`
//!   (mismatch => `Error::CasFailed`).
//!
//! This mirrors the S3/Azure ETag and the GCS generation-number CAS exactly, so
//! it carries the Azure CAS coverage (Azure has no local emulator) and lets the
//! shared CAS battery run against the cloud abstraction in the default unit
//! suite.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use super::CloudObjectIo;
use crate::error::Error;

struct Entry {
    body: Vec<u8>,
    /// Monotonic version identity for this key (its current "ETag/generation").
    token: u64,
}

/// In-memory implementation of [`CloudObjectIo`]. Each stored object carries a
/// monotonic `token` that changes on every write, modelling an ETag/generation.
///
/// Failure injection (refactor-instructions.md Phase 1 safety net, used by the
/// C2 / F4 fixes): `head_exists` can be made to return a transient error
/// instead of `Ok`, and every `head_exists` call is counted. Real cloud SDKs
/// can fail `head_exists` with a network/auth error distinct from "object
/// absent" (404); this lets tests distinguish "backend said no" from "backend
/// broke" without a live network dependency.
#[derive(Default)]
pub struct MemCloud {
    objects: Mutex<HashMap<String, Entry>>,
    next_token: Mutex<u64>,
    head_exists_calls: AtomicU64,
    fail_head_exists: std::sync::atomic::AtomicBool,
}

impl MemCloud {
    pub fn new() -> Self {
        MemCloud::default()
    }

    fn fresh_token(&self) -> u64 {
        let mut n = self.next_token.lock().unwrap();
        *n += 1;
        *n
    }

    /// Total number of `head_exists` calls made so far. Used to assert on
    /// existence-check counts (e.g. F4's "one HEAD per new upload" fix).
    pub fn head_exists_call_count(&self) -> u64 {
        self.head_exists_calls.load(Ordering::SeqCst)
    }

    /// When `true`, every subsequent `head_exists` call returns
    /// `Err(Error::Other(..))` (simulating a transient network/auth failure)
    /// instead of a real existence check. Used to verify C2: callers must not
    /// silently treat a `head_exists` error as "object absent".
    pub fn set_fail_head_exists(&self, fail: bool) {
        self.fail_head_exists.store(fail, Ordering::SeqCst);
    }
}

fn token_bytes(t: u64) -> Vec<u8> {
    t.to_be_bytes().to_vec()
}

fn token_from_bytes(b: &[u8]) -> Option<u64> {
    let arr: [u8; 8] = b.try_into().ok()?;
    Some(u64::from_be_bytes(arr))
}

impl CloudObjectIo for MemCloud {
    fn head_exists(&self, key: &str) -> Result<bool, Error> {
        self.head_exists_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_head_exists.load(Ordering::SeqCst) {
            return Err(Error::Other(
                "simulated head_exists network failure".to_string(),
            ));
        }
        Ok(self.objects.lock().unwrap().contains_key(key))
    }

    fn head_size(&self, key: &str) -> Result<u64, Error> {
        match self.objects.lock().unwrap().get(key) {
            Some(e) => Ok(e.body.len() as u64),
            None => Err(Error::ObjectNotFound(key.to_string())),
        }
    }

    fn list(&self, objects_prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        let map = self.objects.lock().unwrap();
        Ok(map
            .iter()
            .filter(|(k, _)| k.starts_with(objects_prefix))
            .map(|(k, e)| (k.clone(), e.body.len() as u64))
            .collect())
    }

    fn get(&self, key: &str) -> Result<Bytes, Error> {
        match self.objects.lock().unwrap().get(key) {
            Some(e) => Ok(Bytes::from(e.body.clone())),
            None => Err(Error::ObjectNotFound(key.to_string())),
        }
    }

    fn put(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        let token = self.fresh_token();
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), Entry { body, token });
        Ok(())
    }

    fn conditional_put(
        &self,
        key: &str,
        body: &[u8],
        expected_token: Option<&[u8]>,
    ) -> Result<Vec<u8>, Error> {
        let mut map = self.objects.lock().unwrap();
        match (expected_token, map.get(key)) {
            // Create-only: must not already exist.
            (None, Some(_)) => return Err(Error::CasFailed),
            (None, None) => {}
            // Update: the stored token must match the expected one.
            (Some(_), None) => return Err(Error::CasFailed),
            (Some(exp), Some(entry)) => {
                let exp = token_from_bytes(exp).ok_or(Error::CasFailed)?;
                if exp != entry.token {
                    return Err(Error::CasFailed);
                }
            }
        }
        let token = {
            let mut n = self.next_token.lock().unwrap();
            *n += 1;
            *n
        };
        map.insert(
            key.to_string(),
            Entry {
                body: body.to_vec(),
                token,
            },
        );
        Ok(token_bytes(token))
    }

    fn get_with_token(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        match self.objects.lock().unwrap().get(key) {
            Some(e) => Ok(Some((e.body.clone(), token_bytes(e.token)))),
            None => Ok(None),
        }
    }
}

/// [`RootPointer`] over a [`MemCloud`] at a fixed key, using the cloud
/// conditional-put CAS (version token + 412). This is the in-memory analogue
/// of the S3/Azure/GCS root pointers and proves the CAS abstraction holds
/// without a network backend. `Arc<MemCloud>` implements `CloudObjectIo` via
/// the blanket `Arc<T>` impl in `store/cloud/mod.rs`, so this is just
/// `CloudRootPointer` specialised to a shared `MemCloud` (refactor-
/// instructions.md E10 -- previously its own byte-identical struct+impl).
pub type MemCloudRootPointer = super::CloudRootPointer<std::sync::Arc<MemCloud>>;

// ---------------------------------------------------------------------------
// Shared RootPointer CAS battery (parameterized over Local + MemCloud)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cas_battery {
    use super::*;
    use crate::codec::pack::root_pointer::{LocalRootPointer, RootPointer, RootToken};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build the set of `RootPointer` implementations under test. Each closure
    /// returns a FRESH, empty pointer plus a guard whose lifetime keeps any
    /// backing storage alive for the duration of the test.
    fn pointers() -> Vec<(
        &'static str,
        Box<dyn Fn() -> (Box<dyn RootPointer>, Box<dyn std::any::Any>)>,
    )> {
        vec![
            (
                "local",
                Box::new(|| {
                    let tmp = TempDir::new().unwrap();
                    let rp = LocalRootPointer::new(tmp.path().to_path_buf(), None);
                    (
                        Box::new(rp) as Box<dyn RootPointer>,
                        Box::new(tmp) as Box<dyn std::any::Any>,
                    )
                }),
            ),
            (
                "memcloud",
                Box::new(|| {
                    let cloud = Arc::new(MemCloud::new());
                    let rp = MemCloudRootPointer::for_key(cloud.clone(), "INDEX_ROOT");
                    (
                        Box::new(rp) as Box<dyn RootPointer>,
                        Box::new(cloud) as Box<dyn std::any::Any>,
                    )
                }),
            ),
        ]
    }

    #[test]
    fn read_absent_returns_none_and_absent_token() {
        for (name, make) in pointers() {
            let (rp, _guard) = make();
            let (bytes, token) = rp.read().unwrap();
            assert!(bytes.is_none(), "[{name}] pointer must start absent");
            assert_eq!(token, RootToken::Absent, "[{name}]");
        }
    }

    #[test]
    fn cas_create_then_read_back() {
        for (name, make) in pointers() {
            let (rp, _guard) = make();
            let v1 = b"root-v1".to_vec();
            rp.cas_write(&RootToken::Absent, &v1).unwrap();
            assert_eq!(rp.read().unwrap().0, Some(v1), "[{name}]");
        }
    }

    #[test]
    fn cas_absent_fails_when_already_present() {
        for (name, make) in pointers() {
            let (rp, _guard) = make();
            let v1 = b"root-v1".to_vec();
            rp.cas_write(&RootToken::Absent, &v1).unwrap();
            let err = rp.cas_write(&RootToken::Absent, b"root-v2").unwrap_err();
            assert!(
                matches!(err, Error::CasFailed),
                "[{name}] expected CasFailed, got {err:?}"
            );
            assert_eq!(rp.read().unwrap().0, Some(v1), "[{name}] bytes preserved");
        }
    }

    #[test]
    fn cas_update_with_token_from_read() {
        for (name, make) in pointers() {
            let (rp, _guard) = make();
            rp.cas_write(&RootToken::Absent, b"root-v1").unwrap();
            let (_, token) = rp.read().unwrap();
            let v2 = b"root-v2".to_vec();
            rp.cas_write(&token, &v2).unwrap();
            assert_eq!(rp.read().unwrap().0, Some(v2), "[{name}]");
        }
    }

    #[test]
    fn cas_rejects_stale_token_and_preserves_bytes() {
        for (name, make) in pointers() {
            let (rp, _guard) = make();
            rp.cas_write(&RootToken::Absent, b"root-v1").unwrap();
            let (_, stale) = rp.read().unwrap();
            let v2 = b"root-v2".to_vec();
            rp.cas_write(&stale, &v2).unwrap();
            // The stale token no longer matches the current version.
            let err = rp.cas_write(&stale, b"root-v3").unwrap_err();
            assert!(
                matches!(err, Error::CasFailed),
                "[{name}] expected CasFailed, got {err:?}"
            );
            assert_eq!(rp.read().unwrap().0, Some(v2), "[{name}] bytes unchanged");
        }
    }
}

#[cfg(test)]
mod memcloud_io_tests {
    use super::*;

    #[test]
    fn put_get_head_roundtrip() {
        let c = MemCloud::new();
        assert!(!c.head_exists("k").unwrap());
        c.put("k", b"hello".to_vec()).unwrap();
        assert!(c.head_exists("k").unwrap());
        assert_eq!(c.head_size("k").unwrap(), 5);
        assert_eq!(&c.get("k").unwrap()[..], b"hello");
    }

    #[test]
    fn conditional_put_create_only_semantics() {
        let c = MemCloud::new();
        // First create-only succeeds.
        c.conditional_put("k", b"v1", None).unwrap();
        // Second create-only fails (already present).
        assert!(matches!(
            c.conditional_put("k", b"v2", None),
            Err(Error::CasFailed)
        ));
    }

    #[test]
    fn conditional_put_token_match_required() {
        let c = MemCloud::new();
        let t1 = c.conditional_put("k", b"v1", None).unwrap();
        // Stale (wrong) token fails.
        assert!(matches!(
            c.conditional_put("k", b"v2", Some(b"\0\0\0\0\0\0\0\0")),
            Err(Error::CasFailed)
        ));
        // Correct token succeeds and yields a new token.
        let t2 = c.conditional_put("k", b"v2", Some(&t1)).unwrap();
        assert_ne!(t1, t2);
        assert_eq!(&c.get("k").unwrap()[..], b"v2");
    }

    #[test]
    fn list_filters_by_prefix() {
        let c = MemCloud::new();
        c.put("repo/objects/aa/bb/cc/x", b"1".to_vec()).unwrap();
        c.put("repo/INDEX_ROOT", b"22".to_vec()).unwrap();
        let listed = c.list("repo/objects").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1, 1);
    }

    // --- Failure-injection seam (refactor-instructions.md Phase 1) ---------

    #[test]
    fn head_exists_call_count_tracks_every_call() {
        let c = MemCloud::new();
        assert_eq!(c.head_exists_call_count(), 0);
        c.head_exists("k").unwrap();
        c.head_exists("k").unwrap();
        c.head_exists("other").unwrap();
        assert_eq!(c.head_exists_call_count(), 3);
    }

    #[test]
    fn fail_head_exists_returns_error_instead_of_false() {
        let c = MemCloud::new();
        // Absent key with injection off: a real "not found" answer (Ok(false)).
        assert!(!c.head_exists("k").unwrap());
        c.set_fail_head_exists(true);
        // With injection on, the call must surface an error, not Ok(false) --
        // this is the distinction C2 requires callers to preserve.
        assert!(c.head_exists("k").is_err());
        c.set_fail_head_exists(false);
        assert!(!c.head_exists("k").unwrap());
    }
}
