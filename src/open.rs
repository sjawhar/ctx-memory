//! Open-item retrieval.

use chrono::NaiveDate;
use serde::Serialize;

use crate::schema::{today_utc, OPEN_KINDS, OPEN_STATES};
use crate::tree::Tree;

#[derive(Debug, Serialize)]
pub struct OpenItem {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub owner: Option<String>,
    pub deadline: Option<NaiveDate>,
    pub days_overdue: Option<i64>,
    pub title: String,
    pub opened: Option<NaiveDate>,
}

/// Returns the currently open items using today's UTC date.
#[must_use]
pub fn open_items(tree: &Tree) -> Vec<OpenItem> {
    open_items_at(tree, today_utc())
}

/// Returns open items and overdue durations relative to `today`.
#[must_use]
pub fn open_items_at(tree: &Tree, today: NaiveDate) -> Vec<OpenItem> {
    let mut items: Vec<OpenItem> = tree
        .docs
        .iter()
        .filter(|doc| {
            doc.front
                .kind
                .as_deref()
                .is_some_and(|kind| OPEN_KINDS.contains(&kind))
                && doc
                    .front
                    .status
                    .as_deref()
                    .is_some_and(|status| OPEN_STATES.contains(&status))
        })
        .map(|doc| {
            let deadline = doc.front.deadline();
            OpenItem {
                path: doc.rel.to_string_lossy().into_owned(),
                kind: doc.front.kind.clone().unwrap_or_default(),
                status: doc.front.status.clone().unwrap_or_default(),
                owner: doc.front.owner.clone(),
                deadline,
                days_overdue: deadline
                    .filter(|deadline| *deadline < today)
                    .map(|deadline| (today - deadline).num_days()),
                opened: doc.front.opened.or(doc.front.date),
                title: doc.label(),
            }
        })
        .collect();
    items.sort_by_key(|item| {
        (
            std::cmp::Reverse(item.days_overdue),
            item.deadline.is_none(),
            item.deadline,
        )
    });
    items
}
