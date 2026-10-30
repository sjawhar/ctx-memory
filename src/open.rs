//! Open-item retrieval.

use std::collections::{hash_map::Entry, BTreeMap, HashMap};
use std::io::Write;

use anyhow::{anyhow, bail, Result};
use chrono::NaiveDate;
use owo_colors::OwoColorize as _;
use serde::Serialize;
use serde_json::Value;

use crate::delta::Staleness;
use crate::output::printable;
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

/// What `ctx open` narrows its list to. The default keeps every open item.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenFilter<'a> {
    /// Each filter must match a record the item links to.
    pub linked: &'a [LinkedFilter],
    /// The item's owner must be one of these people; `linked` names a record `linked` matched.
    pub owners: &'a [String],
    /// The item must wait on one of these people, matched as an owner is.
    pub waiting_on: &'a [String],
    /// The item's deadline must have passed.
    pub overdue: bool,
}

#[derive(Debug, Serialize)]
pub struct OpenItemView {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub owner: Option<String>,
    pub waiting_on: Vec<String>,
    pub tracked_by: Vec<String>,
    pub deadline: Option<NaiveDate>,
    pub days_overdue: Option<i64>,
    pub title: String,
    pub opened: Option<NaiveDate>,
    pub unreviewed: Option<Vec<Staleness>>,
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

/// Returns the open items `filter` keeps, with overdue durations relative to `today`.
///
/// # Errors
///
/// Returns an error when a linked target's frontmatter cannot be read or `linked` is used
/// as an owner or a person waited on without at least one linked-record filter.
pub fn open_items_with_filters(
    tree: &Tree,
    filter: &OpenFilter<'_>,
    today: NaiveDate,
) -> Result<Vec<OpenItemView>> {
    for (flag, names) in [
        ("--owner", filter.owners),
        ("--waiting-on", filter.waiting_on),
    ] {
        if names.iter().any(|name| name == "linked") && filter.linked.is_empty() {
            bail!("{flag} linked requires --linked");
        }
    }

    let mut frontmatter = HashMap::new();
    let mut items = Vec::new();
    for document in tree.docs.iter().filter(|document| is_open(document)) {
        if filter.overdue && days_overdue(document, today).is_none() {
            continue;
        }
        let links = resolved_links(tree, document);
        let mut linked_positions = Vec::new();
        let mut all_filters_match = true;
        for linked in filter.linked {
            let mut this_filter_matches = false;
            for position in &links {
                if linked_target_matches(tree, &mut frontmatter, *position, linked)? {
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
        if !filter.owners.is_empty()
            && !any_person_matches(
                tree,
                document,
                document.front.owner.as_slice(),
                filter.owners,
                &linked_positions,
            )?
        {
            continue;
        }
        if !filter.waiting_on.is_empty()
            && !any_person_matches(
                tree,
                document,
                &document.front.waiting_on,
                filter.waiting_on,
                &linked_positions,
            )?
        {
            continue;
        }
        items.push(open_item(document, today));
    }
    sort_items(&mut items);
    Ok(items)
}

/// Adds current unreviewed-reply flags to open-item views.
pub fn apply_staleness(items: &mut [OpenItemView], stale: &BTreeMap<String, Vec<Staleness>>) {
    for item in items {
        item.unreviewed = stale.get(&item.path).cloned();
    }
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
    OpenItemView {
        path: document.rel.to_string_lossy().into_owned(),
        kind: document.front.kind.clone().unwrap_or_default(),
        status: document.front.status.clone().unwrap_or_default(),
        owner: document.front.owner.clone(),
        waiting_on: document.front.waiting_on.clone(),
        tracked_by: document.front.tracked_by.clone(),
        deadline: document.front.deadline(),
        days_overdue: days_overdue(document, today),
        opened: document.front.opened.or(document.front.date),
        title: document.label(),
        unreviewed: None,
    }
}

/// How many days past its deadline an item is on `today`; none on the deadline itself.
fn days_overdue(document: &Doc, today: NaiveDate) -> Option<i64> {
    document
        .front
        .deadline()
        .filter(|deadline| *deadline < today)
        .map(|deadline| (today - deadline).num_days())
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

/// Whether any of `people`, a record's owner or the people it waits on, is one of `names`.
fn any_person_matches(
    tree: &Tree,
    document: &Doc,
    people: &[String],
    names: &[String],
    linked_positions: &[usize],
) -> Result<bool> {
    for person in people {
        if person_matches(tree, document, person, names, linked_positions)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `person`, as a record names someone, is one of `names`: the whole name in any
/// case, or, when `person` links a record, that record's stem or link target. The name
/// `linked` stands for a record a `--linked` filter matched.
fn person_matches(
    tree: &Tree,
    document: &Doc,
    person: &str,
    names: &[String],
    linked_positions: &[usize],
) -> Result<bool> {
    if names
        .iter()
        .any(|name| name != "linked" && person.eq_ignore_ascii_case(name))
    {
        return Ok(true);
    }
    for link in wikilinks(person) {
        let Resolution::Found(person_position) = tree.resolve(document, &link.target) else {
            continue;
        };
        for name in names {
            if name == "linked" {
                if linked_positions.contains(&person_position) {
                    return Ok(true);
                }
                continue;
            }
            if person_name_matches(tree, document, person_position, &link.target, name)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn person_name_matches(
    tree: &Tree,
    document: &Doc,
    person_position: usize,
    target: &str,
    name: &str,
) -> Result<bool> {
    let person = tree
        .docs
        .get(person_position)
        .ok_or_else(|| anyhow!("person link points outside the tree"))?;
    Ok(person.stem().eq_ignore_ascii_case(name)
        || target.eq_ignore_ascii_case(name)
        || matches!(tree.resolve(document, name), Resolution::Found(position) if position == person_position))
}

/// Prints open items the way a person reads them: kind, when, owner, title, path, who the item
/// waits on and what tracks it.
///
/// # Errors
///
/// Returns an error when the output cannot be written.
pub fn print(items: &[OpenItemView], out: &mut dyn Write) -> std::io::Result<()> {
    if items.is_empty() {
        return writeln!(out, "nothing open");
    }
    let today = today_utc();
    for item in items {
        let when = item.days_overdue.map_or_else(
            || {
                item.deadline.map_or_else(
                    || match item.opened.map(|opened| (today - opened).num_days()) {
                        Some(age) => format!("no date (opened {age}d ago)"),
                        None => "no date".to_owned(),
                    },
                    |deadline| deadline.format("%a %Y-%m-%d").to_string(),
                )
            },
            |days| format!("overdue {days}d"),
        );
        // Titles, owners, paths and pointers are the tree's text, and so the sources' text:
        // each is printed without its control characters.
        writeln!(
            out,
            "[{}] {}  {}",
            printable(&item.kind).bold(),
            when.red(),
            printable(item.owner.as_deref().unwrap_or("unassigned")).dimmed()
        )?;
        writeln!(out, "     {}", printable(&item.title))?;
        writeln!(out, "     {}", printable(&item.path).dimmed())?;
        for (label, values) in [
            ("waiting on", &item.waiting_on),
            ("tracked by", &item.tracked_by),
        ] {
            if !values.is_empty() {
                writeln!(out, "     {label} {}", printable(&values.join(", ")))?;
            }
        }
        if let Some(unreviewed) = item.unreviewed.as_deref() {
            for reply in unreviewed {
                let same_day = if reply.same_day { " (same day)" } else { "" };
                writeln!(
                    out,
                    "     unreviewed {} {} ({}){}",
                    reply.ts,
                    printable(&reply.pointer),
                    reply.how,
                    same_day
                )?;
            }
        }
        writeln!(out)?;
    }
    Ok(())
}
