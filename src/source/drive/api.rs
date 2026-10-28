//! The Drive API seam: the calls a fetch makes, and the Google client that answers them.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use crate::api::client;
use crate::api::google::{self, Google, Text};

/// The file fields every listing asks for.
const FILE_FIELDS: &str = "files(id,name,mimeType,modifiedTime,webViewLink,owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress),parents),nextPageToken";
/// The change fields the changes feed asks for.
const CHANGE_FIELDS: &str = "changes(removed,fileId,file(id,name,mimeType,modifiedTime,trashed,webViewLink,owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress),parents)),nextPageToken,newStartPageToken";

/// One page of the changes feed: the files it carries and the token that follows it.
#[derive(Clone, Debug, Default)]
pub(super) struct Changes {
    pub(super) files: Vec<Value>,
    /// The next page when `more`, else the start of the changes after this page.
    pub(super) next_page_token: String,
    /// Drive named a page after this one.
    pub(super) more: bool,
}

pub(super) trait DriveApi {
    /// The changes token a first fetch starts from.
    fn start_page_token(&self) -> Result<String>;
    /// The page of changes `page_token` opens, and where the feed continues after it, or `None`
    /// when Drive refuses the token.
    fn changes(&self, page_token: &str) -> Result<Option<Changes>>;
    /// Files matching a Drive query.
    fn files(&self, query: &str) -> Result<Vec<Value>>;
    /// The text export of one file, or Drive's refusal to make it.
    fn export(&self, id: &str, mime: &str, limit: usize) -> Result<Text>;
}

pub(super) struct GoogleDriveApi {
    google: Google,
}

impl GoogleDriveApi {
    pub(super) fn load() -> Result<Self> {
        Ok(Self {
            google: Google::load()?,
        })
    }
}

impl DriveApi for GoogleDriveApi {
    fn start_page_token(&self) -> Result<String> {
        let response = self.google.get(
            "drive/v3/changes/startPageToken",
            &[
                ("supportsAllDrives", "true".to_owned()),
                ("includeItemsFromAllDrives", "true".to_owned()),
            ],
        )?;
        response
            .get("startPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("the Drive changes start has no startPageToken"))
    }

    fn changes(&self, page_token: &str) -> Result<Option<Changes>> {
        let (status, page) = self.google.reply(
            "drive/v3/changes",
            &[
                ("pageToken", page_token.to_owned()),
                ("pageSize", google::page_size().to_string()),
                ("fields", CHANGE_FIELDS.to_owned()),
                ("includeRemoved", "false".to_owned()),
                ("supportsAllDrives", "true".to_owned()),
                ("includeItemsFromAllDrives", "true".to_owned()),
            ],
        )?;
        // Drive answers a token it does not accept with 400 `invalid` at `pageToken`.
        if google::invalid_parameter(status, &page, "pageToken") {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            bail!("Google drive/v3/changes returned {status}: {page}");
        }
        let files = page
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|change| change.get("removed").and_then(Value::as_bool) != Some(true))
            .filter_map(|change| change.get("file"))
            .filter(|file| file.get("trashed").and_then(Value::as_bool) != Some(true))
            .cloned()
            .collect();
        let token = |field: &str| {
            page.get(field)
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_owned)
        };
        Ok(Some(match token("nextPageToken") {
            Some(next_page_token) => Changes {
                files,
                next_page_token,
                more: true,
            },
            None => Changes {
                files,
                next_page_token: token("newStartPageToken")
                    .unwrap_or_else(|| page_token.to_owned()),
                more: false,
            },
        }))
    }

    fn files(&self, query: &str) -> Result<Vec<Value>> {
        let pages = self.google.paginate(
            "drive/v3/files",
            &[
                ("q", query.to_owned()),
                ("pageSize", google::page_size().to_string()),
                ("fields", FILE_FIELDS.to_owned()),
                ("supportsAllDrives", "true".to_owned()),
                ("includeItemsFromAllDrives", "true".to_owned()),
                ("corpora", "user".to_owned()),
            ],
            "files",
        )?;
        Ok(pages.items)
    }

    fn export(&self, id: &str, mime: &str, limit: usize) -> Result<Text> {
        self.google.get_text(
            &format!("drive/v3/files/{}/export", client::percent_encode(id)),
            &[("mimeType", mime.to_owned())],
            limit,
        )
    }
}
