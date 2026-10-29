//! The lease object: acquisition, takeover, renewal, checks and release against Cloud Storage's
//! generations and preconditions.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use ctx_memory::lease::{self, Clock, Lease};
use serde_json::{json, Value};

use crate::fixtures::{frozen, reads, Cloud, BUCKET};
use crate::support::fake_gcs::Recorded;

fn at(text: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(text)
        .with_context(|| format!("parsing {text}"))?
        .with_timezone(&Utc))
}

/// A clock a test moves forward, so a holder can see time pass without the test waiting.
fn movable(start: DateTime<Utc>) -> (Clock, Arc<Mutex<DateTime<Utc>>>) {
    let now = Arc::new(Mutex::new(start));
    let reading = Arc::clone(&now);
    let clock = Clock::from_fn(move || *reading.lock().unwrap_or_else(PoisonError::into_inner));
    (clock, now)
}

fn message(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

/// The `ifGenerationMatch` of the last write the fake storage received.
fn last_precondition(writes: &[Recorded]) -> Result<String> {
    writes
        .last()
        .and_then(|write| write.parameter("ifGenerationMatch"))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no write carried a precondition"))
}

#[test]
fn second_holder_cannot_acquire_live_lease() -> Result<()> {
    let cloud = Cloud::start()?;

    let first = Lease::acquire(BUCKET, "fetch", "fetch@one")?;
    let second = Lease::acquire(BUCKET, "fetch", "fetch@two");

    let refused = message(
        &second
            .err()
            .context("a second holder acquired a live lease")?,
    );
    let (_, held) = cloud.held_by("fetch")?;
    first.check()?;
    assert!(
        refused.contains("fetch@one"),
        "the refusal does not name the holder: {refused}"
    );
    assert_eq!(held.get("holder"), Some(&json!("fetch@one")));
    assert_eq!(
        cloud.storage.writes()?.len(),
        1,
        "the refused holder wrote the lease object"
    );
    Ok(())
}

#[test]
fn takeover_after_two_minutes() -> Result<()> {
    let cloud = Cloud::start()?;
    let start = at("2026-10-27T12:00:00Z")?;

    let first = Lease::acquire_with(BUCKET, "fetch", "fetch@one", frozen(start))?;
    let (generation, _) = cloud.held_by("fetch")?;
    let early = Lease::acquire_with(
        BUCKET,
        "fetch",
        "fetch@two",
        frozen(start + Duration::seconds(119)),
    );
    let late = Lease::acquire_with(
        BUCKET,
        "fetch",
        "fetch@two",
        frozen(start + Duration::seconds(121)),
    )?;

    let refused = message(
        &early
            .err()
            .context("a lease was taken over at 119 seconds")?,
    );
    let (_, held) = cloud.held_by("fetch")?;
    drop(first);
    drop(late);
    assert!(
        refused.contains("fetch@one"),
        "the refusal does not name the holder: {refused}"
    );
    assert_eq!(held.get("holder"), Some(&json!("fetch@two")));
    assert_eq!(
        last_precondition(&cloud.storage.writes()?)?,
        generation.to_string(),
        "the takeover did not replace the generation it saw"
    );
    Ok(())
}

#[test]
fn old_holder_check_fails_after_takeover() -> Result<()> {
    let cloud = Cloud::start()?;
    let start = at("2026-10-27T12:00:00Z")?;

    let old = Lease::acquire_with(BUCKET, "fetch", "fetch@one", frozen(start))?;
    old.check()?;
    let new = Lease::acquire_with(
        BUCKET,
        "fetch",
        "fetch@two",
        frozen(start + Duration::seconds(121)),
    )?;
    let checked = old.check();

    let refused = message(
        &checked
            .err()
            .context("the old holder still holds the lease")?,
    );
    let (_, held) = cloud.held_by("fetch")?;
    new.check()?;
    assert!(
        refused.contains("taken over"),
        "the old holder's check does not say it lost the lease: {refused}"
    );
    assert_eq!(held.get("holder"), Some(&json!("fetch@two")));
    Ok(())
}

#[test]
fn a_renewal_moves_the_generation_and_keeps_the_lease() -> Result<()> {
    let cloud = Cloud::start()?;
    let start = at("2026-10-27T12:00:00Z")?;
    let (clock, now) = movable(start);

    let lease = Lease::acquire_with(BUCKET, "fetch", "fetch@one", clock)?;
    let (first, _) = cloud.held_by("fetch")?;
    *now.lock().unwrap_or_else(PoisonError::into_inner) = start + Duration::seconds(95);
    lease.renew()?;
    // Without the renewal this check refuses: 95 seconds is past the point where a holder
    // stops writing.
    lease.check()?;

    let (second, held) = cloud.held_by("fetch")?;
    assert!(
        second > first,
        "the renewal left the object at generation {second}"
    );
    assert_eq!(held.get("renewed_at"), Some(&json!("2026-10-27T12:01:35Z")));
    assert_eq!(
        held.get("holder").and_then(Value::as_str),
        Some(lease.holder())
    );
    Ok(())
}

#[test]
fn a_holder_that_cannot_renew_stops_before_the_takeover_window() -> Result<()> {
    let _cloud = Cloud::start()?;
    let start = at("2026-10-27T12:00:00Z")?;
    let (clock, now) = movable(start);

    let lease = Lease::acquire_with(BUCKET, "server", "server@one", clock)?;
    lease.check()?;
    *now.lock().unwrap_or_else(PoisonError::into_inner) = start + Duration::seconds(95);
    let stale = lease.check();

    let refused = message(
        &stale
            .err()
            .context("a holder kept writing past its renewal")?,
    );
    assert!(
        refused.contains("renew"),
        "the holder does not say why it stopped: {refused}"
    );
    Ok(())
}

/// The age a check judges is the age when the check ends. Reading the generation takes as
/// long as Cloud Storage takes to answer, and an age sampled before that read let a holder
/// that crossed the line while it waited pass the check, with a push still to follow.
#[test]
fn a_check_judges_the_age_after_it_reads_the_object() -> Result<()> {
    let cloud = Cloud::start()?;
    let start = at("2026-10-27T12:00:00Z")?;
    let storage = Arc::clone(&cloud.storage);
    let taken = Arc::new(AtomicUsize::new(usize::MAX));
    let seen = Arc::clone(&taken);
    // Ninety-five seconds pass while Cloud Storage answers the first read after the lease is
    // taken.
    let clock = Clock::from_fn(move || {
        let answered = reads(&storage).unwrap_or_default();
        if answered > seen.load(Ordering::SeqCst) {
            start + Duration::seconds(95)
        } else {
            start
        }
    });

    let lease = Lease::acquire_with(BUCKET, "server", "server@one", clock)?;
    taken.store(reads(&cloud.storage)?, Ordering::SeqCst);
    let checked = lease.check();

    let refused = message(
        &checked
            .err()
            .context("a check passed for a holder that crossed the line while it read")?,
    );
    assert!(
        refused.contains("renew"),
        "the holder does not say why it stopped: {refused}"
    );
    Ok(())
}

#[test]
fn a_released_lease_is_free_at_once() -> Result<()> {
    let cloud = Cloud::start()?;

    let first = Lease::acquire(BUCKET, "fetch", "fetch@one")?;
    first.release()?;
    let second = Lease::acquire(BUCKET, "fetch", "fetch@two")?;

    let (_, held) = cloud.held_by("fetch")?;
    second.check()?;
    assert_eq!(held.get("holder"), Some(&json!("fetch@two")));
    assert_eq!(
        cloud.storage.deletes()?.len(),
        1,
        "the release did not delete the lease object"
    );
    Ok(())
}

/// What the fetch's Slack source asks before it rotates a credential: whether this process
/// holds the fetch lease now. It does from the moment the lease is taken until it is released
/// or dropped, and holding one role's lease is not holding another's.
#[test]
fn this_process_holds_a_lease_only_while_it_has_it() -> Result<()> {
    let _cloud = Cloud::start()?;

    assert!(!lease::holds("fetch"), "no lease is taken yet");
    let first = Lease::acquire(BUCKET, "fetch", "fetch@one")?;
    assert!(lease::holds("fetch"));
    assert!(
        !lease::holds("server"),
        "the fetch lease is not the server's"
    );
    first.release()?;
    assert!(!lease::holds("fetch"), "a released lease is not held");
    let second = Lease::acquire(BUCKET, "fetch", "fetch@two")?;
    assert!(lease::holds("fetch"));
    drop(second);
    assert!(!lease::holds("fetch"), "a dropped lease is not held");

    // A lease not renewed for as long as `check` refuses is not held either, though nothing
    // has marked it lost: another copy may take it over soon.
    let start = at("2026-10-27T12:00:00Z")?;
    let (clock, now) = movable(start);
    let stale = Lease::acquire_with(BUCKET, "fetch", "fetch@three", clock)?;
    assert!(lease::holds("fetch"));
    *now.lock().unwrap_or_else(PoisonError::into_inner) = start + Duration::seconds(95);
    assert!(
        !lease::holds("fetch"),
        "a lease 95 seconds unrenewed is held"
    );
    drop(stale);
    Ok(())
}
