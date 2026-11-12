//! The routes and JSON bodies the ctx server and its remote client share.
//!
//! Most of these bodies were built as `serde_json::Value` maps, which `serde_json` writes with
//! their keys sorted. Their fields are declared in that order, so each type writes the bytes the
//! route has always carried; `tests/protocol.rs` pins every one.

use serde::{Deserialize, Serialize, Serializer};

use crate::marker;

/// The version of the protocol below: these routes, their bodies and answers, and what each
/// means. The server names it on every answer and the client on every request, both in
/// [`VERSION_HEADER`], and [`PROTOCOL_ROUTE`] and `/healthz` say it as `protocol`.
/// `ctx protocol` prints the client's and its server's, which the ingest runner compares before
/// it starts a session.
///
/// Bump it by one, by hand, in the release that adds or removes a route or changes what a
/// route takes, answers or means in a way a client of the version before would misread; never
/// for a release that changes none of that. It is not the crate's version: most releases change
/// no route, and the two would only drift. A bump also sets the `*_SINCE` constant of each
/// route whose meaning it changed, so the server refuses that route to an older client, and
/// says so in `docs/deployment.md`.
///
/// - 0: a client or a server from before this number existed, which names none.
/// - 1: the v0.3 protocol. A window larger than one page is worked a page at a time:
///   [`PAGE_ROUTE`] records a page, and [`MARKER_ROUTE`] refuses to pass a window whose last
///   page no answer has listed.
pub const VERSION: u32 = 1;

/// The header that carries the protocol version, in a request the client's and in an answer
/// the server's. A request without it is from a client of protocol 0.
pub const VERSION_HEADER: &str = "X-Ctx-Protocol";

/// The oldest client protocol that works a window a page at a time. An older client is answered
/// `409` on [`PAGE_ROUTE`], and on a move of the marker ([`MARKER_ROUTE`]) that would pass a
/// window with pages left: by that move it means to pass the whole window.
pub const PAGED_SINCE: u32 = 1;

/// The route that answers how the server is and which protocol version it speaks.
pub const HEALTH_ROUTE: &str = "/healthz";

/// The route that answers which protocol version the server speaks, and nothing else, to any
/// caller. `ctx protocol` asks it before [`HEALTH_ROUTE`]: on a `run.app` URL Cloud Run's own
/// front end answers `/healthz` with a `404` of its own, since it reserves paths that end in
/// `z`, so no answer from the server reaches the caller there. A server older than this route
/// answers it `404`, and from v0.3.2 on that `404` names the server's version in
/// [`VERSION_HEADER`], as every answer does; the client asks `/healthz` only after a `404` that
/// names none.
pub const PROTOCOL_ROUTE: &str = "/v1/protocol";

/// The route that runs one allowlisted read command.
pub const COMMAND_ROUTE: &str = "/v1/command";
/// The route that writes one record.
pub const PUT_ROUTE: &str = "/v1/records/put";
/// The route that writes several records in one commit.
pub const PUT_BATCH_ROUTE: &str = "/v1/records/put-batch";
/// The route that removes one record.
pub const DELETE_ROUTE: &str = "/v1/records/delete";
/// The route that moves the agent marker.
pub const MARKER_ROUTE: &str = "/v1/marker/advance";
/// The route that records a page of the window after the marker.
pub const PAGE_ROUTE: &str = "/v1/marker/page";
/// The route that answers what the tree's context source says about one window, with the
/// window id in place of `{id}`.
pub const CONTEXT_ROUTE: &str = "/v1/windows/{id}/context";

/// [`CONTEXT_ROUTE`] for one window.
#[must_use]
pub fn context_path(window: &str) -> String {
    CONTEXT_ROUTE.replace("{id}", window)
}

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

/// The body of [`PUT_BATCH_ROUTE`]: records written together, each checked against the tree
/// with every one of them in it, and committed in one commit or not at all.
#[derive(Debug, Deserialize, Serialize)]
pub struct PutBatchRequest {
    pub records: Vec<RecordFile>,
    #[serde(default)]
    pub session: Option<String>,
}

/// One record a batch writes: its tree path and its whole text, frontmatter and body.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordFile {
    pub content: String,
    pub path: String,
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

/// The body of [`PAGE_ROUTE`]: the window after the marker, and the pointer of the message the
/// page the agent has worked ends with, the `page.ends_at` that `ctx pending` listed.
#[derive(Debug, Deserialize, Serialize)]
pub struct PageRequest {
    pub ends_at: String,
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

/// The answer to a page record.
#[derive(Debug, Deserialize, Serialize)]
pub struct PageResponse {
    pub page: marker::Page,
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
