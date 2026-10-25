//! Authenticated JSON GETs run through `curl`.
//!
//! Tokens travel in the `Authorization` header, never in an argument or URL. Callers
//! provide a hint that explains how to set a missing environment variable.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use serde_json::Value;

use crate::proc;

/// One HTTP round trip may take this long before curl gives up.
const HTTP_TIMEOUT_SECS: u64 = 90;

/// Read `var` from the environment. Missing values name the caller-provided hint.
///
/// # Errors
///
/// Returns an error when `var` is not set.
pub fn env_token(var: &str, hint: &str) -> Result<String> {
    std::env::var(var).with_context(|| format!("{var} not set — invoke via `{hint}`"))
}

/// Percent-encode a query value: everything but unreserved characters.
#[must_use]
pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// `GET url` with `Authorization: Bearer <token>`, parsed as JSON.
///
/// # Errors
///
/// Returns an error when curl cannot run, exits unsuccessfully, times out, or returns invalid
/// JSON.
pub fn get_json(url: &str, bearer: &str) -> Result<Value> {
    get_json_with(url, bearer, proc::run_with_stdin)
}

fn get_json_with<F>(url: &str, bearer: &str, mut run: F) -> Result<Value>
where
    F: FnMut(&str, &[&str], &[u8], Duration) -> Result<proc::Output>,
{
    let authorization = format!("Authorization: Bearer {bearer}\n");
    let max_time = HTTP_TIMEOUT_SECS.to_string();
    let output = run(
        "curl",
        &["-sS", "--max-time", max_time.as_str(), "-H", "@-", url],
        authorization.as_bytes(),
        Duration::from_secs(HTTP_TIMEOUT_SECS + 5),
    )?;
    if !output.success() {
        bail!(
            "curl exited with {:?}: {}",
            output.code,
            output.stderr.trim()
        );
    }
    serde_json::from_str(&output.stdout).with_context(|| format!("parsing JSON from GET {url}"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    #[test]
    fn percent_encode_escapes_reserved_characters_only() {
        assert_eq!(percent_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(percent_encode("a b+c=d"), "a%20b%2Bc%3Dd");
    }

    #[test]
    fn missing_token_names_the_caller_hint() {
        let error = env_token("CTX_TEST_UNSET_KEY_4D2", "ctx example command").err();
        let rendered = error.map_or_else(String::new, |value| format!("{value:#}"));
        assert!(rendered.contains("CTX_TEST_UNSET_KEY_4D2 not set"));
        assert!(rendered.contains("invoke via `ctx example command`"));
    }

    #[test]
    fn present_token_is_returned_verbatim() -> Result<()> {
        std::env::set_var("CTX_TEST_SET_KEY_9A1", "test-token");
        assert_eq!(env_token("CTX_TEST_SET_KEY_9A1", "unused")?, "test-token");
        Ok(())
    }

    #[test]
    fn authorization_is_passed_on_stdin_not_in_curl_arguments() -> Result<()> {
        let captured = RefCell::new(None);
        let response = get_json_with(
            "https://api.example.com/resource",
            "test-bearer-token",
            |program, args, stdin, _timeout| {
                let arguments: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
                *captured.borrow_mut() = Some((program.to_owned(), arguments, stdin.to_vec()));
                Ok(proc::Output {
                    code: Some(0),
                    stdout: json!({"ok": true}).to_string(),
                    stderr: String::new(),
                })
            },
        )?;
        let (program, arguments, stdin) = captured
            .into_inner()
            .ok_or_else(|| anyhow::anyhow!("runner was not called"))?;

        assert_eq!(response, json!({"ok": true}));
        assert_eq!(program, "curl");
        assert!(arguments.iter().any(|argument| argument == "@-"));
        assert!(arguments
            .iter()
            .all(|argument| !argument.contains("test-bearer-token")));
        assert_eq!(stdin, b"Authorization: Bearer test-bearer-token\n");
        Ok(())
    }
}
