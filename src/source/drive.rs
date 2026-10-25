//! Built-in Google Drive source.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::gws;
use crate::record::Record;

use super::{
    window_start, FetchRequest, Message, Op, Request, ResolveRequest, Source, StreamStatus,
    PROTOCOL_VERSION,
};

const FILE_FIELDS: &str = "files(id,name,mimeType,modifiedTime,sharedWithMeTime,webViewLink,owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress),parents),nextPageToken";

/// Built-in source for Google Drive files.
pub struct DriveSource;

impl DriveSource {
    /// Creates a Drive source using the `gws` command line client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DriveSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for DriveSource {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        match request {
            Request::Fetch(request) => {
                let config = parse_config(&request.config)?;
                emit_fetch_hello(request, &config, sink)?;
                fetch_with_api(request, &config, &GwsDriveApi, sink)
            }
            Request::Resolve(request) => {
                let _config = parse_config(&request.config)?;
                emit_resolve_hello(request, sink)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriveConfig {
    #[serde(default)]
    modified: bool,
    #[serde(default)]
    shared_with_me: bool,
    #[serde(default)]
    folders: Vec<FolderConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderConfig {
    id: String,
    initial: InitialListing,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum InitialListing {
    All,
    Window,
}

fn parse_config(value: &Value) -> Result<DriveConfig> {
    let config: DriveConfig =
        serde_json::from_value(value.clone()).context("parsing Drive source configuration")?;
    let mut folders = BTreeSet::new();
    for folder in &config.folders {
        if folder.id.is_empty() {
            bail!("Drive source folder id must not be empty");
        }
        if !folders.insert(&folder.id) {
            bail!("Drive source folder {} appears more than once", folder.id);
        }
    }
    Ok(config)
}

trait DriveApi {
    fn files(&self, params: &Value) -> Result<Vec<Value>>;
    fn files_page(&self, params: &Value) -> Result<Value>;
}

struct GwsDriveApi;

impl DriveApi for GwsDriveApi {
    fn files(&self, params: &Value) -> Result<Vec<Value>> {
        gws::gws_paginate(&["drive", "files", "list"], params, "files")
    }

    fn files_page(&self, params: &Value) -> Result<Value> {
        let params =
            serde_json::to_string(params).context("serializing Drive request parameters")?;
        gws::gws(&["drive", "files", "list", "--params", &params])
    }
}

fn fetch_streams(config: &DriveConfig) -> Vec<String> {
    let mut streams = Vec::new();
    if config.modified {
        streams.push("modified".to_owned());
    }
    if config.shared_with_me {
        streams.push("shared_with_me".to_owned());
    }
    streams.extend(
        config
            .folders
            .iter()
            .map(|folder| format!("folder:{}", folder.id)),
    );
    streams
}

fn emit_fetch_hello(
    request: &FetchRequest,
    config: &DriveConfig,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: false,
        streams: fetch_streams(config),
    })
}

fn emit_resolve_hello(
    request: &ResolveRequest,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    sink(Message::Hello {
        protocol: PROTOCOL_VERSION,
        scheme: request.source.scheme.clone(),
        ops: vec![Op::Fetch],
        verbatim: false,
        streams: Vec::new(),
    })
}

fn fetch_with_api<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    config: &DriveConfig,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    if config.modified {
        fetch_one_stream(request, "modified", api, sink, fetch_modified)?;
    }
    if config.shared_with_me {
        fetch_one_stream(request, "shared_with_me", api, sink, fetch_shared_with_me)?;
    }
    for folder in &config.folders {
        let stream = format!("folder:{}", folder.id);
        let has_cursor = request.cursors.contains_key(&stream);
        let result = window_start(
            request.cursors.get(&stream).map(String::as_str),
            request.window_end,
            request.backfill_hours,
        )
        .and_then(|start| fetch_folder(api, folder, has_cursor, start, request.window_end))
        .and_then(|files| {
            records_from_files(files, &stream, &request.source.scheme, "modifiedTime")
        });
        emit_stream_result(request, stream, result, sink)?;
    }
    Ok(())
}

type StreamFetcher<C> = fn(&C, DateTime<Utc>, DateTime<Utc>, &str) -> Result<Vec<Record>>;

fn fetch_one_stream<C: DriveApi + ?Sized>(
    request: &FetchRequest,
    stream: &str,
    api: &C,
    sink: &mut dyn FnMut(Message) -> Result<()>,
    fetcher: StreamFetcher<C>,
) -> Result<()> {
    let result = window_start(
        request.cursors.get(stream).map(String::as_str),
        request.window_end,
        request.backfill_hours,
    )
    .and_then(|start| fetcher(api, start, request.window_end, &request.source.scheme));
    emit_stream_result(request, stream.to_owned(), result, sink)
}

fn emit_stream_result(
    request: &FetchRequest,
    stream: String,
    result: Result<Vec<Record>>,
    sink: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    match result {
        Ok(records) => {
            for record in records {
                sink(Message::Record(Box::new(record)))?;
            }
            sink(Message::StreamEnd {
                stream,
                status: StreamStatus::Ok,
                cursor: Some(
                    request
                        .window_end
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
                error: None,
            })
        }
        Err(error) => sink(Message::StreamEnd {
            stream,
            status: StreamStatus::Error,
            cursor: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

fn fetch_modified<C: DriveApi + ?Sized>(
    api: &C,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    scheme: &str,
) -> Result<Vec<Record>> {
    let files = api.files(&file_list_params(&format!(
        "modifiedTime > '{}' and modifiedTime <= '{}' and trashed = false",
        start.to_rfc3339_opts(SecondsFormat::Secs, true),
        end.to_rfc3339_opts(SecondsFormat::Secs, true)
    )))?;
    records_from_files(files, "modified", scheme, "modifiedTime")
}

fn fetch_folder<C: DriveApi + ?Sized>(
    api: &C,
    folder: &FolderConfig,
    has_cursor: bool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Value>> {
    let query = if folder.initial == InitialListing::All && !has_cursor {
        format!("'{}' in parents and trashed = false", folder.id)
    } else {
        format!(
            "'{}' in parents and modifiedTime > '{}' and modifiedTime <= '{}' and trashed = false",
            folder.id,
            start.to_rfc3339_opts(SecondsFormat::Secs, true),
            end.to_rfc3339_opts(SecondsFormat::Secs, true)
        )
    };
    api.files(&file_list_params(&query))
}

fn fetch_shared_with_me<C: DriveApi + ?Sized>(
    api: &C,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    scheme: &str,
) -> Result<Vec<Record>> {
    let mut files = Vec::new();
    let mut token: Option<String> = None;
    let mut seen_tokens = BTreeSet::new();
    loop {
        let mut params = file_list_params("sharedWithMe = true");
        let fields = params
            .as_object_mut()
            .ok_or_else(|| anyhow!("Drive request parameters are not an object"))?;
        fields.insert(
            "orderBy".to_owned(),
            Value::String("sharedWithMeTime desc".to_owned()),
        );
        if let Some(token) = &token {
            fields.insert("pageToken".to_owned(), Value::String(token.clone()));
        }
        let page = api.files_page(&params)?;
        let page_files = page
            .get("files")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, Clone::clone);
        let mut reached_window_start = false;
        for file in page_files {
            let shared_at = file_timestamp(&file, "sharedWithMeTime")?;
            if shared_at <= start {
                reached_window_start = true;
                break;
            }
            if shared_at <= end {
                files.push(file);
            }
        }
        token = page
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|next| !next.is_empty())
            .map(str::to_owned);
        match token.as_ref() {
            Some(next) if !seen_tokens.insert(next.clone()) => {
                bail!("Drive shared-with-me list repeated page token {next:?}");
            }
            Some(_) if !reached_window_start => {}
            _ => break,
        }
    }
    records_from_files(files, "shared_with_me", scheme, "sharedWithMeTime")
}

fn file_list_params(query: &str) -> Value {
    json!({
        "q": query,
        "pageSize": 100,
        "fields": FILE_FIELDS,
        "supportsAllDrives": true,
        "includeItemsFromAllDrives": true,
        "corpora": "user",
    })
}

fn records_from_files(
    files: Vec<Value>,
    stream: &str,
    scheme: &str,
    timestamp_field: &str,
) -> Result<Vec<Record>> {
    files
        .into_iter()
        .map(|file| record_from_file(&file, stream, scheme, timestamp_field))
        .collect()
}

fn record_from_file(
    file: &Value,
    stream: &str,
    scheme: &str,
    timestamp_field: &str,
) -> Result<Record> {
    let id = required_file_string(file, "id")?;
    let title = file.get("name").and_then(Value::as_str).map(str::to_owned);
    let author = display_name(file.get("lastModifyingUser")).or_else(|| {
        file.get("owners")
            .and_then(Value::as_array)
            .and_then(|owners| owners.first())
            .and_then(|owner| display_name(Some(owner)))
    });
    let text = title
        .as_deref()
        .map_or_else(|| "Drive file".to_owned(), str::to_owned);
    Ok(Record {
        pointer: format!("{scheme}:{id}"),
        stream: stream.to_owned(),
        ts: file_timestamp(file, timestamp_field)?,
        label: Some("drive".to_owned()),
        author,
        author_id: None,
        thread: None,
        title,
        url: file
            .get("webViewLink")
            .and_then(Value::as_str)
            .map(str::to_owned),
        text,
        aliases: vec![id.to_owned()],
        context: false,
        raw: file.clone(),
    })
}

fn required_file_string<'a>(file: &'a Value, field: &str) -> Result<&'a str> {
    file.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Drive file is missing {field}"))
}

fn file_timestamp(file: &Value, field: &str) -> Result<DateTime<Utc>> {
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct FakeDriveApi {
        calls: RefCell<Vec<Value>>,
    }

    impl DriveApi for FakeDriveApi {
        fn files(&self, params: &Value) -> Result<Vec<Value>> {
            self.calls.borrow_mut().push(params.clone());
            Ok(Vec::new())
        }

        fn files_page(&self, params: &Value) -> Result<Value> {
            self.calls.borrow_mut().push(params.clone());
            Ok(json!({"files": []}))
        }
    }

    fn request(cursor: Option<&str>) -> Result<FetchRequest> {
        let mut cursors = std::collections::BTreeMap::new();
        if let Some(cursor) = cursor {
            cursors.insert("folder:FOLDER-EXAMPLE".to_owned(), cursor.to_owned());
        }
        let window_end = DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z")?.with_timezone(&Utc);
        Ok(FetchRequest {
            protocol: PROTOCOL_VERSION,
            source: super::super::SourceRef {
                id: "drive".to_owned(),
                kind: "drive".to_owned(),
                scheme: "drive".to_owned(),
            },
            config: json!({"folders": [{"id": "FOLDER-EXAMPLE", "initial": "all"}]}),
            window_end,
            backfill_hours: 24,
            cursors,
            discovered: Vec::new(),
            state_dir: std::env::temp_dir(),
        })
    }

    #[test]
    fn all_initial_folder_becomes_windowed_after_its_first_cursor() -> Result<()> {
        let config = parse_config(&json!({
            "folders": [{"id": "FOLDER-EXAMPLE", "initial": "all"}]
        }))?;
        let api = FakeDriveApi::default();
        let mut output = Vec::new();
        let initial = request(None)?;
        fetch_with_api(&initial, &config, &api, &mut |message| {
            output.push(message);
            Ok(())
        })?;
        let initial_query = api
            .calls
            .borrow()
            .first()
            .and_then(|params| params.get("q"))
            .and_then(Value::as_str)
            .context("initial folder query")?
            .to_owned();

        let windowed = request(Some("2026-10-25T17:00:00Z"))?;
        fetch_with_api(&windowed, &config, &api, &mut |_| Ok(()))?;
        let windowed_query = api
            .calls
            .borrow()
            .get(1)
            .and_then(|params| params.get("q"))
            .and_then(Value::as_str)
            .context("windowed folder query")?
            .to_owned();

        assert_eq!(
            initial_query,
            "'FOLDER-EXAMPLE' in parents and trashed = false"
        );
        assert!(windowed_query.contains("modifiedTime > '2026-10-25T17:00:00Z'"));
        Ok(())
    }

    #[test]
    fn shared_with_me_requests_descending_shared_time() -> Result<()> {
        let config = parse_config(&json!({"shared_with_me": true}))?;
        let request = request(None)?;
        let api = FakeDriveApi::default();
        fetch_with_api(&request, &config, &api, &mut |_| Ok(()))?;

        let order = api
            .calls
            .borrow()
            .first()
            .and_then(|params| params.get("orderBy"))
            .and_then(Value::as_str)
            .context("shared-with-me request contains orderBy")?
            .to_owned();
        assert_eq!(order, "sharedWithMeTime desc");
        Ok(())
    }
}
