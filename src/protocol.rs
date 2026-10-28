//! The routes and JSON bodies the ctx server and its remote client share.
//!
//! Most of these bodies were built as `serde_json::Value` maps, which `serde_json` writes with
//! their keys sorted. Their fields are declared in that order, so each type writes the bytes the
//! route has always carried; `tests/protocol.rs` pins every one.

use serde::{Deserialize, Serialize, Serializer};

use crate::marker;

/// The route that runs one allowlisted read command.
pub const COMMAND_ROUTE: &str = "/v1/command";
/// The route that writes one record.
pub const PUT_ROUTE: &str = "/v1/records/put";
/// The route that removes one record.
pub const DELETE_ROUTE: &str = "/v1/records/delete";
/// The route that moves the agent marker.
pub const MARKER_ROUTE: &str = "/v1/marker/advance";

/// The body of [`COMMAND_ROUTE`].
#[derive(Debug, Deserialize, Serialize)]
pub struct CommandRequest {
    pub argv: Vec<String>,
    /// Files on the caller's machine that `cite verify` names, sent as text, so a draft the
    /// server's tree does not hold is checked rather than matched against nothing.
    #[serde(default)]
    pub drafts: Vec<Draft>,
}

/// One file a remote caller's `cite verify` names, as the caller has it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Draft {
    pub content: String,
    /// The path exactly as the caller named it on the command line.
    pub path: String,
}

/// What one captured command run produced, as [`COMMAND_ROUTE`] answers it.
#[derive(Debug, Deserialize, Serialize)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
}

/// The body of [`PUT_ROUTE`].
#[derive(Debug, Deserialize, Serialize)]
pub struct PutRequest {
    pub content: String,
    pub path: String,
    #[serde(default)]
    pub session: Option<String>,
}

/// The body of [`DELETE_ROUTE`].
#[derive(Debug, Deserialize, Serialize)]
pub struct DeleteRequest {
    pub path: String,
    #[serde(default)]
    pub session: Option<String>,
}

/// The body of [`MARKER_ROUTE`].
#[derive(Debug, Deserialize, Serialize)]
pub struct MarkerRequest {
    #[serde(default)]
    pub session: Option<String>,
    pub window: String,
}

/// One reason a write was refused, in the shape [`PUT_ROUTE`] answers with.
#[derive(Debug, Deserialize, Serialize)]
pub struct Finding {
    pub detail: String,
    pub kind: String,
    pub path: String,
}

/// The answer to a write that was committed and published.
#[derive(Debug, Deserialize, Serialize)]
pub struct Committed {
    pub commit: String,
}

/// The answer to a write a check refused.
#[derive(Debug, Deserialize, Serialize)]
pub struct Refused {
    pub findings: Vec<Finding>,
}

/// The answer to a marker advance.
#[derive(Debug, Deserialize, Serialize)]
pub struct MarkerResponse {
    /// Written as a JSON value: the marker's own field order is the marker file's, and this
    /// route has always answered it with its keys sorted.
    #[serde(serialize_with = "as_json_value")]
    pub marker: marker::Marker,
}

/// Writes `value` as a `serde_json::Value` writes it, with every map's keys sorted.
fn as_json_value<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    serde_json::to_value(value)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}
