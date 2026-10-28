//! Writer admission, path, rules and credential refusal tests.

use serde_json::Value;

use crate::support;

mod citation_and_sweep;
mod credentials;
mod paths;
mod rules;

/// The kinds of the findings one refusal carried, in order.
pub(super) fn finding_kinds(reply: &support::Reply) -> Vec<String> {
    reply
        .field("findings")
        .ok()
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(|finding| finding.get("kind").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
