//! The fake Cloud Storage, its bucket and the frozen clock that more than one lease check uses.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use ctx_memory::lease::Clock;
use serde_json::{json, Value};

use crate::support::fake_gcs::FakeGcs;
use crate::support::fake_http::FakeHttp;

/// The bucket every test leases from.
pub(super) const BUCKET: &str = "ctx-leases";

/// `GCE_METADATA_HOST` and `CTX_STORAGE_URL` are process-wide, so these tests take turns.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// A fake metadata server and a fake Cloud Storage, with this process pointed at both.
///
/// `CTX_LEASE_BUCKET` is deliberately not set in this process: a test hands it to the child
/// that should take a lease, so a child started for another reason takes none.
pub(super) struct Cloud {
    metadata: FakeHttp,
    pub(super) storage: Arc<FakeGcs>,
    _guard: MutexGuard<'static, ()>,
}

impl Cloud {
    pub(super) fn start() -> Result<Self> {
        let guard = ENVIRONMENT.lock().unwrap_or_else(PoisonError::into_inner);
        let metadata = FakeHttp::start()?;
        metadata.json(
            "/computeMetadata/v1/instance/service-accounts/default/token",
            200,
            &json!({ "access_token": "ya29.lease", "expires_in": 3599 }).to_string(),
        )?;
        let storage = Arc::new(FakeGcs::start()?);
        std::env::set_var("GCE_METADATA_HOST", metadata.address().to_string());
        std::env::set_var("CTX_STORAGE_URL", storage.url());
        Ok(Self {
            metadata,
            storage,
            _guard: guard,
        })
    }

    /// The environment a container running `ctx` would carry, lease bucket included.
    pub(super) fn environment(&self) -> Vec<(String, String)> {
        vec![
            (
                "GCE_METADATA_HOST".to_owned(),
                self.metadata.address().to_string(),
            ),
            ("CTX_STORAGE_URL".to_owned(), self.storage.url()),
            ("CTX_LEASE_BUCKET".to_owned(), BUCKET.to_owned()),
        ]
    }

    /// The holder and renewal time the lease object carries.
    pub(super) fn held_by(&self, name: &str) -> Result<(i64, Value)> {
        self.storage
            .object(name)?
            .ok_or_else(|| anyhow!("the lease object {name} does not exist"))
    }
}

impl Drop for Cloud {
    fn drop(&mut self) {
        std::env::remove_var("GCE_METADATA_HOST");
        std::env::remove_var("CTX_STORAGE_URL");
    }
}

/// A clock stopped at one moment, which is what a holder of a lease sees.
pub(super) fn frozen(moment: DateTime<Utc>) -> Clock {
    Clock::from_fn(move || moment)
}

/// How many reads of an object Cloud Storage has answered.
pub(super) fn reads(storage: &FakeGcs) -> Result<usize> {
    Ok(storage
        .requests()?
        .iter()
        .filter(|request| request.method == "GET")
        .count())
}
