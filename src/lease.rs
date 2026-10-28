//! One writer per role, across machines.
//!
//! Two roles write what every machine shares: the fetch job writes windows, cursors and
//! per-source state on the share, and the server writes the marker on the share and pushes the
//! graph repository. Two local commands write the same files, and take the same leases: `ctx
//! delta --write` replaces a window's delta files, which the fetch writes, so it takes the
//! fetch lease; `ctx marker advance` run without a server moves the marker, which the server
//! moves, so it takes the server lease. The share is NFS without file locks, and both roles can
//! be started twice at once: a Cloud Run rollout starts the new server copy before the old one
//! stops, and Cloud Scheduler can start a fetch while the last one still runs. A small Cloud
//! Storage object is the arbiter.
//!
//! `embeddings/` on the share is the one thing several writers share without a lease: the
//! fetch job, the server and any machine that searches or reads a delta each add the vectors
//! they compute. Each file holds the vector one model gives one text, is named by that text's
//! hash and is replaced atomically, so two writers that use one model write the same thing.
//!
//! The object holds its holder and the time that holder last renewed it. Every write of it
//! carries a precondition, which is what makes the rule safe rather than hopeful: a copy
//! creates the object with `ifGenerationMatch=0`, and takes a lease over with
//! `ifGenerationMatch=<the generation it read>`, so of two copies that both decide to take
//! over, exactly one wins (cloud.google.com/storage/docs/request-preconditions).
//!
//! The holder renews every 30 seconds and another copy may take over only after 120 seconds
//! without a renewal. A holder stops writing 30 seconds before that, so the copy that cannot
//! renew has stopped before any other copy is allowed to start. `check()` runs before every
//! write batch and also asks Cloud Storage whether the generation this copy holds is still the
//! object's, so a holder that lost the lease to a takeover stops at its next batch.
//!
//! A tree with no `CTX_LEASE_BUCKET` takes no lease, for a role or a local command: a dev box
//! shares its data directory with nobody, and its fetch is single by its own `flock`. A server
//! or a fetch job on Cloud Run is never such a tree, and refuses to start without one.

use std::hash::{BuildHasher as _, Hasher as _};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration as Wait;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::credentials;
use crate::redact::one_line;

/// Google's Cloud Storage JSON API.
const STORAGE_API: &str = "https://storage.googleapis.com";

/// Seconds between one renewal and the next.
const RENEW: i64 = 30;

/// Seconds without a renewal after which another copy may take the lease over.
const TAKEOVER: i64 = 120;

/// What one copy reads as "now".
///
/// Production reads the system clock. A test hands in a clock it moves itself, so the two
/// minutes a takeover waits for cost a test nothing.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>);

impl Clock {
    /// The system clock, which every running copy uses.
    #[must_use]
    pub fn system() -> Self {
        Self(Arc::new(Utc::now))
    }

    /// A clock a caller drives.
    #[must_use]
    pub fn from_fn(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    fn now(&self) -> DateTime<Utc> {
        (self.0)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Clock")
    }
}

/// A held lease. It is renewed in the background until it is released or dropped.
#[derive(Debug)]
pub struct Lease {
    held: Arc<Held>,
    renewal: Option<JoinHandle<()>>,
}

/// Everything the holder and its renewal thread share.
#[derive(Debug)]
struct Held {
    bucket: String,
    object: String,
    holder: String,
    base: String,
    clock: Clock,
    client: reqwest::blocking::Client,
    state: Mutex<State>,
    stopping: Mutex<bool>,
    wake: Condvar,
}

/// What this copy holds right now.
#[derive(Debug)]
struct State {
    /// The generation this copy wrote last; the precondition of its next write.
    generation: i64,
    /// When this copy last renewed, by its own clock.
    renewed_at: DateTime<Utc>,
    /// Why this copy no longer holds the lease, once it does not.
    lost: Option<String>,
    /// Whether this copy has already given the lease up.
    released: bool,
}

/// The lease object's content.
#[derive(Debug, Deserialize)]
struct Written {
    holder: String,
    renewed_at: DateTime<Utc>,
}

/// The part of a Cloud Storage object resource a lease reads: Cloud Storage reports a
/// generation as a string, because it does not fit a JSON number everywhere.
#[derive(Deserialize)]
struct Metadata {
    generation: String,
}

/// What one precondition-carrying write did.
enum Wrote {
    /// The write landed, and the object is now at this generation.
    Generation(i64),
    /// The precondition did not hold: another copy owns the object.
    Refused(String),
}

impl Lease {
    /// Takes the lease named `object` in `bucket`, or reports who holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when another copy holds a lease it still renews, when Cloud Storage
    /// cannot be reached, or when another copy wins the same takeover.
    pub fn acquire(bucket: &str, object: &str, holder: &str) -> Result<Self> {
        Self::acquire_with(bucket, object, holder, Clock::system())
    }

    /// Takes the lease, reading "now" from `clock`.
    ///
    /// # Errors
    ///
    /// As [`Lease::acquire`].
    pub fn acquire_with(bucket: &str, object: &str, holder: &str, clock: Clock) -> Result<Self> {
        let held = Arc::new(Held::new(bucket, object, holder, clock)?);
        let now = held.clock.now();
        // The generation is read before the content, never after: a content read that is newer
        // than the generation already read makes this copy's precondition fail, which refuses
        // a takeover rather than racing one. The other order would take over a live lease.
        let (expected, previous) = match held.generation()? {
            None => (0, None),
            Some(generation) => {
                let written = held.written()?.ok_or_else(|| {
                    anyhow!("the {object} lease was released while it was read; try again")
                })?;
                (generation, Some(written))
            }
        };
        if let Some(written) = &previous {
            let age = now - written.renewed_at;
            if age < Duration::seconds(TAKEOVER) {
                bail!(
                    "the {object} lease is held by {} (renewed {} seconds ago); another copy may take it over only after {TAKEOVER} seconds without a renewal",
                    written.holder,
                    age.num_seconds()
                );
            }
        }
        let generation = match held.write(expected, now)? {
            Wrote::Generation(generation) => generation,
            Wrote::Refused(detail) => bail!("another copy took the {object} lease first: {detail}"),
        };
        held.set(State {
            generation,
            renewed_at: now,
            lost: None,
            released: false,
        })?;
        let renewing = Arc::clone(&held);
        let renewal = std::thread::Builder::new()
            .name(format!("ctx-lease-{object}"))
            .spawn(move || renew_until_stopped(&renewing))
            .with_context(|| format!("starting the renewal of the {object} lease"))?;
        Ok(Self {
            held,
            renewal: Some(renewal),
        })
    }

    /// The name this copy holds the lease under.
    #[must_use]
    pub fn holder(&self) -> &str {
        &self.held.holder
    }

    /// Renews the lease once, which the renewal thread also does every 30 seconds.
    ///
    /// # Errors
    ///
    /// Returns an error when the lease was taken over or Cloud Storage cannot be reached.
    pub fn renew(&self) -> Result<()> {
        self.held.renew_once()
    }

    /// Confirms this copy may write, which it must do before every write batch.
    ///
    /// The age of the last renewal is judged when the check ends, after Cloud Storage has
    /// answered: that read takes as long as it takes, and an age sampled before it could pass
    /// a holder that crossed the line while it waited.
    ///
    /// # Errors
    ///
    /// Returns an error when the lease was taken over, when it has not been renewed for long
    /// enough that another copy may take it over, or when Cloud Storage cannot be reached. A
    /// caller that sees an error must write nothing.
    pub fn check(&self) -> Result<()> {
        let held_before = self.lost_or_generation()?;
        let current = self.held.generation()?;
        let held_after = self.lost_or_generation()?;
        let renewed_at = self.held.state()?.renewed_at;
        let age = self.held.clock.now() - renewed_at;
        if age >= Duration::seconds(TAKEOVER - RENEW) {
            bail!(
                "the {} lease has not been renewed for {} seconds; another copy may take it over after {TAKEOVER} seconds, so this copy stops writing",
                self.held.object,
                age.num_seconds()
            );
        }
        // The renewal thread may have moved the generation while Cloud Storage answered; both
        // the generation before the read and the one after it are this copy's.
        match current {
            Some(current) if current == held_before || current == held_after => Ok(()),
            Some(current) => bail!(
                "the {} lease was taken over: it is at generation {current}, not the {held_after} this copy holds",
                self.held.object
            ),
            None => bail!(
                "the {} lease object is gone, so this copy no longer holds it",
                self.held.object
            ),
        }
    }

    /// The generation this copy holds, or why it holds none.
    fn lost_or_generation(&self) -> Result<i64> {
        let state = self.held.state()?;
        if let Some(reason) = &state.lost {
            bail!(
                "this copy no longer holds the {} lease: {reason}",
                self.held.object
            );
        }
        Ok(state.generation)
    }

    /// Stops renewing the lease without giving it up, which a copy whose check failed does.
    ///
    /// When the check failed by age, the object is still this copy's, and freeing it would let
    /// the next copy start at once while a push of this one may still be in flight. Left
    /// alone, it lets another copy in after the takeover window, as a killed copy's does.
    pub fn forsake(self) {
        self.held
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .released = true;
        // Dropping stops the renewal thread, and frees nothing of a lease marked released.
    }

    /// Gives the lease up, so the next copy takes it without waiting out the takeover window.
    ///
    /// A dropped lease is given up the same way; this is the form that reports a failure.
    ///
    /// # Errors
    ///
    /// Returns an error when Cloud Storage cannot be reached. A lease this copy no longer
    /// holds is left alone.
    pub fn release(mut self) -> Result<()> {
        self.stop();
        self.held.free()
    }

    /// Stops the renewal thread and waits for it.
    fn stop(&mut self) {
        {
            let Ok(mut stopping) = self.held.stopping.lock() else {
                return;
            };
            *stopping = true;
        }
        self.held.wake.notify_all();
        if let Some(renewal) = self.renewal.take() {
            let _ = renewal.join();
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stop();
        // A copy that stops of its own accord frees the lease, so the next one starts at
        // once instead of waiting out the takeover window. A copy that is killed cannot,
        // which is what the window is for.
        if let Err(error) = self.held.free() {
            eprintln!(
                "releasing the {} lease: {}",
                self.held.object,
                one_line(&format!("{error:#}"))
            );
        }
    }
}

/// Takes the lease of one role when this deployment leases at all.
///
/// Returns `None` when `CTX_LEASE_BUCKET` is unset, which is how a local tree runs: it is
/// single by the `flock` the fetch takes, and it shares its files with nobody.
///
/// # Errors
///
/// Returns an error when another copy holds the role's lease or Cloud Storage cannot be
/// reached.
pub fn for_role(role: &str) -> Result<Option<Lease>> {
    let Some(bucket) = bucket() else {
        return Ok(None);
    };
    Lease::acquire(&bucket, role, &holder_name(role)).map(Some)
}

/// The bucket this deployment keeps its leases in, when it leases at all.
#[must_use]
pub fn bucket() -> Option<String> {
    std::env::var("CTX_LEASE_BUCKET")
        .ok()
        .filter(|bucket| !bucket.is_empty())
}

/// A name that is unique to this process and tells an operator which copy holds the lease.
fn holder_name(role: &str) -> String {
    let place = std::env::var("K_REVISION")
        .or_else(|_| std::env::var("CLOUD_RUN_EXECUTION"))
        .ok()
        .filter(|place| !place.is_empty())
        .unwrap_or_else(host);
    format!("{role}@{place}/{}.{}", std::process::id(), *NONCE)
}

/// This machine's name, for a copy that runs outside Cloud Run.
fn host() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Two copies of one role can share a machine name and a process id (Cloud Run gives every
/// container the same ones), so the holder name carries a value drawn once per process.
static NONCE: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
});

/// Renews every 30 seconds until the lease is released, dropped, or lost.
fn renew_until_stopped(held: &Held) {
    loop {
        match held.sleep_until_renewal() {
            Ok(true) | Err(_) => return,
            Ok(false) => {}
        }
        if let Err(error) = held.renew_once() {
            // A renewal that fails is not fatal by itself: the next one may land well inside
            // the takeover window. What stops this copy writing is `check()`, which refuses
            // once the lease is lost or too old.
            // Cloud Storage's answer is pretty-printed JSON over many lines, and on Cloud Run
            // each line of standard error is its own log entry: the line is one line.
            eprintln!(
                "renewing the {} lease: {}",
                held.object,
                one_line(&format!("{error:#}"))
            );
            if held.state().is_ok_and(|state| state.lost.is_some()) {
                return;
            }
        }
    }
}

impl Held {
    fn new(bucket: &str, object: &str, holder: &str, clock: Clock) -> Result<Self> {
        if bucket.is_empty() {
            bail!("a lease needs a bucket");
        }
        if object.is_empty()
            || !object
                .chars()
                .all(|letter| letter.is_ascii_alphanumeric() || "-_.".contains(letter))
        {
            bail!("a lease object is named with letters, digits, '-', '_' and '.': got {object:?}");
        }
        Ok(Self {
            bucket: bucket.to_owned(),
            object: object.to_owned(),
            holder: holder.to_owned(),
            base: storage_base(),
            clock,
            client: credentials::client()?,
            state: Mutex::new(State {
                generation: 0,
                renewed_at: DateTime::<Utc>::MIN_UTC,
                lost: None,
                released: false,
            }),
            stopping: Mutex::new(false),
            wake: Condvar::new(),
        })
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("the {} lease state was poisoned", self.object))
    }

    fn set(&self, state: State) -> Result<()> {
        *self.state()? = state;
        Ok(())
    }

    /// Waits for the next renewal, and reports whether the lease is stopping instead.
    fn sleep_until_renewal(&self) -> Result<bool> {
        let stopping = self
            .stopping
            .lock()
            .map_err(|_| anyhow!("the {} lease state was poisoned", self.object))?;
        let (stopping, _) = self
            .wake
            .wait_timeout_while(
                stopping,
                Wait::from_secs(RENEW.unsigned_abs()),
                |stopping| !*stopping,
            )
            .map_err(|_| anyhow!("the {} lease state was poisoned", self.object))?;
        Ok(*stopping)
    }

    /// Writes the lease object again, keeping it this copy's.
    fn renew_once(&self) -> Result<()> {
        let (generation, lost) = {
            let state = self.state()?;
            (state.generation, state.lost.clone())
        };
        if let Some(reason) = lost {
            bail!(
                "this copy no longer holds the {} lease: {reason}",
                self.object
            );
        }
        let now = self.clock.now();
        match self.write(generation, now)? {
            Wrote::Generation(generation) => {
                let mut state = self.state()?;
                state.generation = generation;
                state.renewed_at = now;
                Ok(())
            }
            Wrote::Refused(detail) => {
                let reason = format!("it was taken over ({detail})");
                self.state()?.lost = Some(reason.clone());
                bail!(
                    "the {} lease is no longer this copy's: {reason}",
                    self.object
                );
            }
        }
    }

    /// The generation of the lease object, or `None` when no copy holds it.
    fn generation(&self) -> Result<Option<i64>> {
        let url = self.object_url();
        let response = self
            .client
            .get(&url)
            .bearer_auth(token()?)
            .send()
            .with_context(|| format!("reading {url}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("reading {url} failed: {status} {}", detail(response));
        }
        let metadata: Metadata = response
            .json()
            .with_context(|| format!("decoding the metadata of {url}"))?;
        Ok(Some(metadata.generation.parse().with_context(|| {
            format!("{url} has an unreadable generation")
        })?))
    }

    /// The holder and renewal time the lease object carries.
    fn written(&self) -> Result<Option<Written>> {
        let url = self.object_url();
        let response = self
            .client
            .get(&url)
            .query(&[("alt", "media")])
            .bearer_auth(token()?)
            .send()
            .with_context(|| format!("reading {url}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("reading {url} failed: {status} {}", detail(response));
        }
        Ok(Some(response.json().with_context(|| {
            format!("decoding the lease object {}", self.object)
        })?))
    }

    /// Writes the lease object, but only while it is at `expected`.
    fn write(&self, expected: i64, now: DateTime<Utc>) -> Result<Wrote> {
        let url = format!("{}/upload/storage/v1/b/{}/o", self.base, self.bucket);
        let body = json!({
            "version": 1,
            "holder": self.holder,
            "renewed_at": now.to_rfc3339_opts(SecondsFormat::Secs, true),
        });
        let response = self
            .client
            .post(&url)
            .query(&[
                ("uploadType", "media"),
                ("name", self.object.as_str()),
                ("ifGenerationMatch", expected.to_string().as_str()),
            ])
            .bearer_auth(token()?)
            .json(&body)
            .send()
            .with_context(|| format!("writing {url}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Ok(Wrote::Refused(detail(response)));
        }
        if !status.is_success() {
            bail!("writing {url} failed: {status} {}", detail(response));
        }
        let metadata: Metadata = response
            .json()
            .with_context(|| format!("decoding the answer of {url}"))?;
        Ok(Wrote::Generation(
            metadata
                .generation
                .parse()
                .with_context(|| format!("{url} answered with an unreadable generation"))?,
        ))
    }

    /// Gives the lease up once, if this copy still holds it.
    fn free(&self) -> Result<()> {
        let generation = {
            let mut state = self.state()?;
            if state.released || state.lost.is_some() {
                return Ok(());
            }
            state.released = true;
            state.generation
        };
        self.delete(generation)
    }

    /// Removes the lease object, but only while it is this copy's.
    fn delete(&self, expected: i64) -> Result<()> {
        let url = self.object_url();
        let response = self
            .client
            .delete(&url)
            .query(&[("ifGenerationMatch", expected.to_string())])
            .bearer_auth(token()?)
            .send()
            .with_context(|| format!("releasing {url}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED
            || status == reqwest::StatusCode::NOT_FOUND
        {
            // Another copy already holds it, so there is nothing of this copy's to remove.
            return Ok(());
        }
        if !status.is_success() {
            bail!("releasing {url} failed: {status} {}", detail(response));
        }
        Ok(())
    }

    fn object_url(&self) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.base, self.bucket, self.object
        )
    }
}

/// The access token of the identity this copy runs as.
fn token() -> Result<String> {
    Ok(credentials::metadata_token()?.expose().to_owned())
}

/// The body of a failed answer, which carries Cloud Storage's own reason.
fn detail(response: reqwest::blocking::Response) -> String {
    response.text().unwrap_or_default()
}

/// Where the Cloud Storage JSON API is, which a test points at its own.
fn storage_base() -> String {
    std::env::var("CTX_STORAGE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| STORAGE_API.to_owned())
}

#[cfg(test)]
mod tests {
    use super::holder_name;

    #[test]
    fn a_holder_name_carries_the_role_and_the_process() {
        let name = holder_name("fetch");
        assert!(name.starts_with("fetch@"), "unexpected holder name: {name}");
        assert!(
            name.contains(&std::process::id().to_string()),
            "the holder name does not name the process: {name}"
        );
    }

    #[test]
    fn one_process_keeps_one_holder_name() {
        assert_eq!(holder_name("server"), holder_name("server"));
    }
}
