//! `GET /v1/windows/{id}/context`: what the tree's context source says about one window.
//!
//! The caller is identified by the rule of the port, as every other request is, and must then
//! be one of the `callers` the tree's `[context_source]` names: the context of a window is
//! for the cycle that handles it. A tree with no context source answers an empty list. A
//! context source that fails answers 200 with `unavailable`, never an error status, because
//! the cycle goes on without context ([`crate::context_source`]).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::command::error_response;
use super::identity::Allowed;
use super::request_log::Note;
use super::Listener;
use crate::context_source::{self, Refused, WindowContext};
use crate::redact::one_line;

/// How a request for a window's context ends, once the caller is known.
enum Ended {
    Answered(WindowContext),
    Forbidden(String),
    Refused(Refused),
}

/// Answers `GET /v1/windows/{id}/context`.
pub async fn window_context(
    State(listener): State<Listener>,
    Extension(note): Extension<Note>,
    Path(window): Path<String>,
    headers: HeaderMap,
) -> Response {
    let shared = &listener.shared;
    note.command("context".to_owned());
    let caller = match listener.caller(&headers).await {
        Ok(caller) => caller,
        Err(error) => {
            return error_response(shared, StatusCode::UNAUTHORIZED, &format!("{error:#}"))
        }
    };
    note.caller(&caller);
    let running = Arc::clone(shared);
    let asked = window.clone();
    let ended = tokio::task::spawn_blocking(move || -> anyhow::Result<Ended> {
        // The tree is held only to read the configuration and the window: the program runs
        // without it, so a slow context source holds up no other request.
        let prepared = {
            let context = running.tree()?;
            if let Some(source) = &context.config.context_source {
                if !Allowed::parse(&source.callers)?.admits(&caller.email) {
                    return Ok(Ended::Forbidden(caller.email));
                }
            }
            match context_source::prepare(&context, &asked) {
                Ok(prepared) => prepared,
                Err(error) => match error.downcast::<Refused>() {
                    Ok(refused) => return Ok(Ended::Refused(refused)),
                    Err(error) => return Err(error),
                },
            }
        };
        Ok(Ended::Answered(prepared.map_or_else(
            || WindowContext::empty(&asked),
            context_source::read,
        )))
    })
    .await;
    match ended {
        Ok(Ok(Ended::Answered(answer))) => (StatusCode::OK, Json(answer)).into_response(),
        Ok(Ok(Ended::Forbidden(email))) => error_response(
            shared,
            StatusCode::FORBIDDEN,
            &format!("{email} is not among the callers context_source.callers names"),
        ),
        Ok(Ok(Ended::Refused(refused @ Refused::Malformed(_)))) => {
            error_response(shared, StatusCode::BAD_REQUEST, &refused.to_string())
        }
        Ok(Ok(Ended::Refused(refused @ Refused::Missing(_)))) => {
            error_response(shared, StatusCode::NOT_FOUND, &refused.to_string())
        }
        // The server could not read what the request needs. Context is optional, so this is
        // an answer the cycle goes on from, said once in the log, not an error status.
        Ok(Err(error)) => unavailable(shared, &window, &format!("{error:#}")),
        Err(error) => unavailable(
            shared,
            &window,
            &format!("the context read stopped: {error}"),
        ),
    }
}

fn unavailable(shared: &super::Shared, window: &str, detail: &str) -> Response {
    let window: String = one_line(window).chars().take(64).collect();
    eprintln!(
        "context source: window {window}: unavailable: {}",
        one_line(&shared.hide_paths(detail))
    );
    let mut answer = WindowContext::empty(&window);
    answer.unavailable = Some("the server could not read this window's context".to_owned());
    (StatusCode::OK, Json(answer)).into_response()
}
