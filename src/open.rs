//! Open-item retrieval.

use std::collections::{hash_map::Entry, HashMap};

use anyhow::{anyhow, bail, Result};
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use crate::schema::{today_utc, OPEN_KINDS, OPEN_STATES};
use crate::tree::{wikilinks, Doc, Resolution, Tree};

#[derive(Debug, Clone)]
pub struct LinkedFilter {
    pub field: String,
    pub values: Vec<String>,
}

/// Parses a `FIELD=VALUE[,VALUE…]` linked-record filter.
///
/// # Errors
///
/// Returns an error when the filter has no field, separator, or value.
pub fn parse_linked(input: &str) -> std::result::Result<LinkedFilter, String> {
    let Some((field, values)) = input.split_once('=') else {
        return Err("linked filters must use FIELD=VALUE[,VALUE…]".to_owned());
    };
    let field = field.trim();
    if field.is_empty() {
        return Err("linked filter field must not be empty".to_owned());
    }
    let values: Vec<String> = values
        .split(',')
        .map(str::trim)
        .map(str::to_owned)
        .collect();
    if values.is_empty() || values.iter().any(String::is_empty) {
        return Err("linked filter values must not be empty".to_owned());
    }
    Ok(LinkedFilter {
        field: field.to_owned(),
        values,
    })
}

#[derive(Debug, Serialize)]
pub struct OpenItemView {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub owner: Option<String>,
    pub deadline: Option<NaiveDate>,
    pub days_overdue: Option<i64>,
    pub title: String,
    pub opened: Option<NaiveDate>,
    pub unreviewed: Option<Vec<Value>>,
}

/// Returns the currently open items using today's UTC date.
#[must_use]
pub fn open_items(tree: &Tree) -> Vec<OpenItemView> {
    open_items_at(tree, today_utc())
}

/// Returns open items and overdue durations relative to `today`.
#[must_use]
pub fn open_items_at(tree: &Tree, today: NaiveDate) -> Vec<OpenItemView> {
    let mut items: Vec<OpenItemView> = tree
        .docs
        .iter()
        .filter(|document| is_open(document))
        .map(|document| open_item(document, today))
        .collect();
    sort_items(&mut items);
    items
}

/// Returns open items after applying linked-record and owner filters.
///
/// # Errors
///
/// Returns an error when a linked target's frontmatter cannot be read or `linked` is used
/// as an owner without at least one linked-record filter.
pub fn open_items_with_filters(
    tree: &Tree,
    linked: &[LinkedFilter],
    owners: &[String],
) -> Result<Vec<OpenItemView>> {
    if owners.iter().any(|owner| owner == "linked") && linked.is_empty() {
        bail!("--owner linked requires --linked");
    }

    let today = today_utc();
    let mut frontmatter = HashMap::new();
    let mut items = Vec::new();
    for document in tree.docs.iter().filter(|document| is_open(document)) {
        let links = resolved_links(tree, document);
        let mut linked_positions = Vec::new();
        let mut all_filters_match = true;
        for filter in linked {
            let mut this_filter_matches = false;
            for position in &links {
                if linked_target_matches(tree, &mut frontmatter, *position, filter)? {
                    this_filter_matches = true;
                    if !linked_positions.contains(position) {
                        linked_positions.push(*position);
                    }
                }
            }
            if !this_filter_matches {
                all_filters_match = false;
                break;
            }
        }
        if !all_filters_match {
            continue;
        }
        if !owners.is_empty() && !owner_matches(tree, document, owners, &linked_positions)? {
            continue;
        }
        items.push(open_item(document, today));
    }
    sort_items(&mut items);
    Ok(items)
}

fn is_open(document: &Doc) -> bool {
    document
        .front
        .kind
        .as_deref()
        .is_some_and(|kind| OPEN_KINDS.contains(&kind))
        && document
            .front
            .status
            .as_deref()
            .is_some_and(|status| OPEN_STATES.contains(&status))
}

fn open_item(document: &Doc, today: NaiveDate) -> OpenItemView {
    let deadline = document.front.deadline();
    OpenItemView {
        path: document.rel.to_string_lossy().into_owned(),
        kind: document.front.kind.clone().unwrap_or_default(),
        status: document.front.status.clone().unwrap_or_default(),
        owner: document.front.owner.clone(),
        deadline,
        days_overdue: deadline
            .filter(|deadline| *deadline < today)
            .map(|deadline| (today - deadline).num_days()),
        opened: document.front.opened.or(document.front.date),
        title: document.label(),
        unreviewed: None,
    }
}

fn sort_items(items: &mut [OpenItemView]) {
    items.sort_by_key(|item| {
        (
            std::cmp::Reverse(item.days_overdue),
            item.deadline.is_none(),
            item.deadline,
        )
    });
}

fn resolved_links(tree: &Tree, document: &Doc) -> Vec<usize> {
    wikilinks(&document.raw)
        .into_iter()
        .filter_map(|link| match tree.resolve(document, &link.target) {
            Resolution::Found(position) => Some(position),
            Resolution::Missing | Resolution::Ambiguous(_) => None,
        })
        .collect()
}

fn linked_target_matches(
    tree: &Tree,
    frontmatter: &mut HashMap<usize, Value>,
    position: usize,
    filter: &LinkedFilter,
) -> Result<bool> {
    let fields = cached_frontmatter(tree, frontmatter, position)?;
    Ok(fields
        .get(&filter.field)
        .is_some_and(|value| field_matches(value, &filter.values)))
}

fn cached_frontmatter<'a>(
    tree: &Tree,
    frontmatter: &'a mut HashMap<usize, Value>,
    position: usize,
) -> Result<&'a Value> {
    match frontmatter.entry(position) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            let document = tree
                .docs
                .get(position)
                .ok_or_else(|| anyhow!("linked document points outside the tree"))?;
            let fields = document.frontmatter_json()?;
            Ok(entry.insert(fields))
        }
    }
}

fn field_matches(value: &Value, expected: &[String]) -> bool {
    match value {
        Value::String(actual) => expected.iter().any(|value| value == actual),
        Value::Number(_) | Value::Bool(_) | Value::Null => {
            let actual = value.to_string();
            expected.iter().any(|value| value == &actual)
        }
        Value::Array(_) | Value::Object(_) => false,
    }
}

fn owner_matches(
    tree: &Tree,
    document: &Doc,
    owners: &[String],
    linked_positions: &[usize],
) -> Result<bool> {
    let Some(owner) = document.front.owner.as_deref() else {
        return Ok(false);
    };
    for link in wikilinks(owner) {
        let Resolution::Found(owner_position) = tree.resolve(document, &link.target) else {
            continue;
        };
        for name in owners {
            if name == "linked" {
                if linked_positions.contains(&owner_position) {
                    return Ok(true);
                }
                continue;
            }
            if owner_name_matches(tree, document, owner_position, &link.target, name)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn owner_name_matches(
    tree: &Tree,
    document: &Doc,
    owner_position: usize,
    target: &str,
    name: &str,
) -> Result<bool> {
    let owner = tree
        .docs
        .get(owner_position)
        .ok_or_else(|| anyhow!("owner link points outside the tree"))?;
    Ok(owner.stem().eq_ignore_ascii_case(name)
        || target.eq_ignore_ascii_case(name)
        || matches!(tree.resolve(document, name), Resolution::Found(position) if position == owner_position))
}
