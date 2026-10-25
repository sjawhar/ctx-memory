//! Google Workspace CLI (`gws`) adapter for JSON responses.

use std::collections::HashSet;

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::Value;

use crate::proc;

/// One `gws` invocation is a single API page.
#[cfg(not(test))]
const GWS_TIMEOUT: Duration = Duration::from_secs(90);
#[cfg(test)]
const GWS_TIMEOUT: Duration = Duration::from_secs(1);

/// The JSON object bounded by the first `{` and the last `}` in `stdout`.
///
/// # Errors
///
/// Returns an error when `stdout` has no complete JSON object or that object is invalid JSON.
pub fn json_object_in(stdout: &str, what: &str) -> Result<Value> {
    let start = stdout.find('{');
    let end = stdout.rfind('}');
    let (start, end) = match (start, end) {
        (Some(start), Some(end)) if end >= start => (start, end),
        _ => {
            let preview: String = stdout.chars().take(500).collect();
            bail!("{what}: no JSON object found in output: {preview:?}");
        }
    };
    let slice = stdout
        .get(start..=end)
        .ok_or_else(|| anyhow!("{what}: JSON bounds not on a character boundary"))?;
    serde_json::from_str(slice).with_context(|| format!("{what}: parsing JSON"))
}

/// Run `gws <args...>` and parse the JSON object in its stdout.
///
/// # Errors
///
/// Returns an error when `gws` cannot run, exits unsuccessfully, or emits invalid JSON.
pub fn gws(args: &[&str]) -> Result<Value> {
    let joined = args.join(" ");
    let output =
        proc::run("gws", args, GWS_TIMEOUT).with_context(|| format!("spawning gws {joined}"))?;
    if !output.success() {
        let message = if output.stderr.trim().is_empty() {
            output.stdout.trim().to_owned()
        } else {
            output.stderr.trim().to_owned()
        };
        bail!("gws {joined}: failed (exit {:?}): {message}", output.code);
    }
    json_object_in(&output.stdout, &format!("gws {joined}"))
}

/// Call `gws <service_call> --params '<json>'`, following `nextPageToken`.
///
/// # Errors
///
/// Returns an error when a page cannot run, parameters cannot serialize, or a page token repeats.
pub fn gws_paginate(service_call: &[&str], params: &Value, items_key: &str) -> Result<Vec<Value>> {
    gws_paginate_with(service_call, params, items_key, gws)
}

fn gws_paginate_with<F>(
    service_call: &[&str],
    params: &Value,
    items_key: &str,
    mut run_page: F,
) -> Result<Vec<Value>>
where
    F: FnMut(&[&str]) -> Result<Value>,
{
    let mut items = Vec::new();
    let mut page_token: Option<String> = None;
    let mut seen_page_tokens = HashSet::new();
    loop {
        let mut call_params = params.clone();
        if let Some(token) = &page_token {
            if let Value::Object(map) = &mut call_params {
                map.insert("pageToken".to_owned(), Value::String(token.clone()));
            }
        }
        let params_json = serde_json::to_string(&call_params)?;
        let mut args: Vec<&str> = service_call.to_vec();
        args.push("--params");
        args.push(&params_json);
        let response = run_page(&args)?;
        if let Some(Value::Array(values)) = response.get(items_key) {
            items.extend(values.iter().cloned());
        }
        page_token = response
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        if let Some(token) = &page_token {
            if !seen_page_tokens.insert(token.clone()) {
                bail!("gws: repeated page token {token:?}");
            }
        } else {
            break;
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;

    #[test]
    fn json_object_is_extracted_from_between_log_lines() -> Result<()> {
        let output = "INFO request\n{\"messages\": [{\"id\": \"EXAMPLE\"}]}\nINFO done\n";
        let value = json_object_in(output, "gws test")?;
        assert_eq!(
            value
                .get("messages")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn output_without_an_object_is_an_error_naming_the_call() {
        let error = json_object_in("nothing here", "gws calendar get").err();
        let rendered = error.map_or_else(String::new, |value| value.to_string());
        assert!(rendered.starts_with("gws calendar get: no JSON object found"));
    }

    #[test]
    fn a_repeated_page_token_stops_pagination_with_an_error() -> Result<()> {
        let mut pages = VecDeque::from([
            json!({"items": [], "nextPageToken": "t"}),
            json!({"items": [], "nextPageToken": "t"}),
        ]);
        let error = gws_paginate_with(&["calendar", "events", "list"], &json!({}), "items", |_| {
            pages
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("fake runner exhausted"))
        })
        .err()
        .ok_or_else(|| anyhow::anyhow!("pagination unexpectedly succeeded"))?;

        assert!(format!("{error:#}").contains("repeated page token \"t\""));
        Ok(())
    }
}
