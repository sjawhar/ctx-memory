//! Shared vocabulary for records and sweep checks.

use chrono::{NaiveDate, Utc};

pub const OPEN_KINDS: [&str; 3] = ["decision", "commitment", "question"];
pub const OPEN_STATES: [&str; 3] = ["open", "deferred", "proposed"];
pub const NEEDS_TIER: [&str; 3] = ["meeting", "spec", "contract"];
pub const NEEDS_QUOTE: [&str; 4] = ["decision", "commitment", "metric", "claim"];

/// Returns the current calendar date in UTC.
#[must_use]
pub fn today_utc() -> NaiveDate {
    Utc::now().date_naive()
}
