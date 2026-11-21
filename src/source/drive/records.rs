//! Drive file record rendering and exports.

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::api::google::Text;
use crate::record::Record;
use crate::source::{latest_believable, CLOCK_SKEW_MARGIN};

use super::super::content_net::ContentNet;
use super::api::DriveApi;

/// Google document kinds and the text export each one is stored as.
const EXPORTS: [(&str, &str); 3] = [
    ("application/vnd.google-apps.document", "text/plain"),
    ("application/vnd.google-apps.spreadsheet", "text/csv"),
    ("application/vnd.google-apps.presentation", "text/plain"),
];

/// The most text Drive exports of one document: "exported content is limited to 10MB" (Drive
/// API v3 `files.export`), and a larger document is refused. While a content net is set, an
/// export is read this far, so the net checks all of it, and the record keeps the first
/// `max_export_bytes`.
const DRIVE_EXPORT_CEILING: usize = 10 * 1024 * 1024;

/// Where a file's modified time puts it against the window, as of the moment a run read the
/// listing or page that names it.
#[derive(Clone, Copy)]
pub(super) struct Bounds {
    window_end: DateTime<Utc>,
    /// [`latest_believable`], taken when the run read that listing or page.
    believable: DateTime<Utc>,
}

/// What a file's modified time says about the window that stores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Place {
    /// Changed by the window end.
    Inside,
    /// Changed after the window end: the next window stores it.
    After,
    /// Dated later than [`latest_believable`]: no clock has reached that date, so the
    /// date is wrong and the file did not change after the window. Holding the cursor for it
    /// would hold every later change until the clock passed the date, so this window stores it
    /// at its end, and the record's raw data keeps the date Drive gave.
    Skewed,
}

impl Bounds {
    pub(super) fn read_now(window_end: DateTime<Utc>) -> Self {
        Self {
            window_end,
            believable: latest_believable(window_end),
        }
    }

    pub(super) fn place(self, modified: DateTime<Utc>) -> Place {
        if modified <= self.window_end {
            Place::Inside
        } else if modified <= self.believable {
            Place::After
        } else {
            Place::Skewed
        }
    }
}

/// What reading one file gave: its record, or the key of the content net its export names.
pub(super) enum Read {
    Kept(Box<Record>),
    Withheld(String),
}

const NET_ONLY_FIELDS: [&str; 2] = ["description", "permissions"];

/// Builds one record: a Google document's exported text, or metadata with a warning when the
/// file is not a Google document or Drive will not export this file. The record stores none of
/// the [`NET_ONLY_FIELDS`]. [`Read::Withheld`] when the export, all of it Drive gives, names a
/// key of `net`: the file is not stored, and no warning names it.
///
/// A file changed after the window ends is a context record: the window must not carry a
/// record after its end, and the file is still searchable and citable. A file dated later than
/// any clock has reached is stored at the window end, with a warning that names its date.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_from_file<C: DriveApi + ?Sized>(
    file: &Value,
    stream: &str,
    scheme: &str,
    api: &C,
    limit: usize,
    net: &ContentNet,
    bounds: Bounds,
    warnings: &mut Vec<String>,
) -> Result<Read> {
    let modified = file_timestamp(file, "modifiedTime")?;
    let id = required_file_string(file, "id")?;
    let name = file.get("name").and_then(Value::as_str);
    let mime = file
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let named = name.unwrap_or("(unnamed)");
    let read = if net.has_keys() {
        limit.max(DRIVE_EXPORT_CEILING)
    } else {
        limit
    };
    let exported = export_mime(mime)
        .map(|export| api.export(id, export, read))
        .transpose()
        .with_context(|| format!("exporting {scheme}:{id} {named}"))?;
    let text = match exported {
        Some(Text::Body {
            mut text,
            mut truncated,
        }) => {
            if let Some(key) = net.key_in(&text) {
                return Ok(Read::Withheld(key.to_owned()));
            }
            // Read past the cap for the net alone: the record keeps what the cap allows.
            if read > limit && text.len() > limit {
                let mut end = limit;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                truncated = true;
            }
            if truncated {
                warnings.push(format!(
                    "{scheme}:{id} {named} is longer than {limit} bytes; its export stops there"
                ));
            }
            text
        }
        // Drive will not export this file (download restricted, too large, of a kind Drive does
        // not export, not readable by the user) or no longer has it. Waiting for it would stop
        // the stream on every run, so its name is stored.
        Some(Text::Refused { status, message }) => {
            warnings.push(format!(
                "{scheme}:{id} {named} could not be exported (Drive answered {status}: {message}); stored as metadata only"
            ));
            format!("{named} ({mime})")
        }
        None => {
            warnings.push(format!(
                "{scheme}:{id} {named} is {mime}, not a Google document; stored as metadata only"
            ));
            format!("{named} ({mime})")
        }
    };
    let (ts, context) = match bounds.place(modified) {
        Place::Inside => (modified, false),
        Place::After => (modified, true),
        Place::Skewed => {
            warnings.push(format!(
                "{scheme}:{id} {named} is dated {}, more than {} minutes after both the window end and the time this fetch read it; the date is taken for a wrong clock, so this window stores the file at its end ({}) and its raw data keeps the date Drive gave",
                modified.to_rfc3339_opts(SecondsFormat::Secs, true),
                CLOCK_SKEW_MARGIN.num_minutes(),
                bounds.window_end.to_rfc3339_opts(SecondsFormat::Secs, true)
            ));
            (bounds.window_end, false)
        }
    };
    let author = display_name(file.get("lastModifyingUser")).or_else(|| {
        file.get("owners")
            .and_then(Value::as_array)
            .and_then(|owners| owners.first())
            .and_then(|owner| display_name(Some(owner)))
    });
    let mut raw = file.clone();
    if let Some(fields) = raw.as_object_mut() {
        for field in NET_ONLY_FIELDS {
            fields.remove(field);
        }
    }
    Ok(Read::Kept(Box::new(Record {
        pointer: format!("{scheme}:{id}"),
        stream: stream.to_owned(),
        ts,
        label: Some("drive".to_owned()),
        author,
        author_id: None,
        thread: None,
        title: name.map(str::to_owned),
        url: file
            .get("webViewLink")
            .and_then(Value::as_str)
            .map(str::to_owned),
        text,
        aliases: vec![id.to_owned()],
        context,
        raw,
    })))
}

/// The text export a Google document kind is stored as.
fn export_mime(mime: &str) -> Option<&'static str> {
    EXPORTS
        .iter()
        .find(|(kind, _)| *kind == mime)
        .map(|(_, export)| *export)
}

pub(super) fn required_file_string<'a>(file: &'a Value, field: &str) -> Result<&'a str> {
    file.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Drive file is missing {field}"))
}

pub(super) fn file_timestamp(file: &Value, field: &str) -> Result<DateTime<Utc>> {
    let value = required_file_string(file, field)?;
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .with_context(|| format!("parsing Drive file {field} {value:?}"))
}

fn display_name(user: Option<&Value>) -> Option<String> {
    user.and_then(|user| {
        user.get("displayName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                user.get("emailAddress")
                    .and_then(Value::as_str)
                    .filter(|email| !email.is_empty())
            })
    })
    .map(str::to_owned)
}
