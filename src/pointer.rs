//! Citation pointer parsing for configured source schemes.

use std::collections::BTreeSet;
use std::fmt;

/// A canonical source pointer consisting of its scheme and locator.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Pointer {
    pub scheme: String,
    pub locator: String,
}

impl fmt::Display for Pointer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.scheme, self.locator)
    }
}

/// A pointer found in prose, with its byte range and an optional thread pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Found {
    pub pointer: Pointer,
    pub thread: Option<Pointer>,
    pub start: usize,
    pub end: usize,
}

/// Registered pointer schemes with strict parsers for built-in source schemes.
pub struct Schemes {
    schemes: BTreeSet<String>,
}

impl Schemes {
    /// Creates a parser registry containing every built-in scheme and valid configured schemes.
    #[must_use]
    pub fn new(configured: impl IntoIterator<Item = String>) -> Self {
        let mut schemes = ["slack", "gmail", "drive", "gcal", "github"]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for scheme in configured {
            if is_scheme(&scheme) {
                schemes.insert(scheme);
            }
        }
        Self { schemes }
    }

    /// Returns every registered pointer found in `text`, in source order.
    #[must_use]
    pub fn find_all(&self, text: &str) -> Vec<Found> {
        let mut found = Vec::new();
        let mut offset = 0;
        while offset < text.len() {
            if let Some(permalink) = slack_permalink_at(text, offset) {
                offset = permalink.end;
                found.push(permalink);
                continue;
            }
            if let Some((pointer, end)) = self.pointer_at(text, offset) {
                found.push(Found {
                    pointer,
                    thread: None,
                    start: offset,
                    end,
                });
                offset = end;
                continue;
            }
            let Some(character) = text.get(offset..).and_then(|rest| rest.chars().next()) else {
                break;
            };
            offset += character.len_utf8();
        }
        found
    }

    /// Parses exactly one pointer, permitting surrounding whitespace only.
    #[must_use]
    pub fn parse(&self, argument: &str) -> Option<Pointer> {
        let trimmed = argument.trim();
        let mut found = self.find_all(trimmed);
        match found.as_slice() {
            [only] if only.start == 0 && only.end == trimmed.len() => {
                found.pop().map(|entry| entry.pointer)
            }
            _ => None,
        }
    }

    fn pointer_at(&self, text: &str, start: usize) -> Option<(Pointer, usize)> {
        self.schemes.iter().find_map(|scheme| {
            text.get(start..)
                .filter(|remainder| {
                    remainder
                        .strip_prefix(scheme)
                        .is_some_and(|after_scheme| after_scheme.starts_with(':'))
                })
                .and_then(|_| match scheme.as_str() {
                    "slack" => slack_pointer_at(text, start),
                    "gmail" => gmail_pointer_at(text, start),
                    "drive" => drive_pointer_at(text, start),
                    "gcal" => gcal_pointer_at(text, start),
                    "github" => github_pointer_at(text, start),
                    _ => generic_pointer_at(text, start, scheme),
                })
        })
    }
}

/// Whether `text` claims a pointer: a scheme, a colon, and a locator with no space in it.
///
/// A text of this shape claims provenance at a source, whether or not any source is
/// configured for its scheme. Free text ("overheard in standup") claims none.
#[must_use]
pub fn is_pointer_shaped(text: &str) -> bool {
    text.trim()
        .split_once(':')
        .is_some_and(|(scheme, locator)| {
            is_scheme(scheme) && !locator.is_empty() && !locator.contains(char::is_whitespace)
        })
}

/// Whether `text` is one web address: an `http://` or `https://` URL, the scheme in any case,
/// with a host, no user name or password, and no whitespace or control character.
///
/// A web address names where a reader can open a summary, such as an answered question on a
/// tracker or a pull request. No configured source resolves it and nothing fetches it, so it
/// claims no checked provenance: it is a location, like free text.
#[must_use]
pub fn is_web_address(text: &str) -> bool {
    let text = text.trim();
    let Some(rest) = after_web_scheme(text) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && !text.chars().any(|c| c.is_whitespace() || c.is_control())
        && http_url(text).is_some()
}

/// Whether an `http:` or `https:` scheme stands anywhere in `text`, in any case, once the tabs
/// and line breaks a URL parser drops are taken out.
///
/// A source that holds one names a web address, and a web address is accepted as a source only
/// when it is the whole source: where a URL in prose ends is a reader's guess, so prose around
/// one could hide where it leads.
#[must_use]
pub fn holds_web_scheme(text: &str) -> bool {
    let folded: String = text
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .flat_map(char::to_lowercase)
        .collect();
    folded.contains("http:") || folded.contains("https:")
}

/// What follows the `http://` or `https://` that `text` starts with, in any case.
fn after_web_scheme(text: &str) -> Option<&str> {
    ["https://", "http://"].into_iter().find_map(|scheme| {
        text.get(..scheme.len())
            .filter(|start| start.eq_ignore_ascii_case(scheme))
            .and_then(|_| text.get(scheme.len()..))
    })
}

/// Whether `text`, read as a URL, has a Slack workspace's host.
///
/// The scanner reads a Slack permalink as a Slack pointer, so such a link names a message a
/// source checks. Every other form the URL parser reads as a Slack host, whether with a port,
/// capitals, a trailing dot, a user name, percent-encoding or full-width letters, names that
/// message too: it is no web address a summary may cite unchecked. Only the host of the address
/// given is read: nothing follows a redirect or reads an address inside another site's URL.
#[must_use]
pub fn is_source_link(text: &str) -> bool {
    http_url(text.trim()).is_some_and(|url| url.host_str().is_some_and(is_slack_host))
}

/// `text` parsed as an `http` or `https` URL with a host.
fn http_url(text: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(text).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|h| !h.is_empty())
    })
}

/// Whether `host` is Slack's or one of its workspaces', in any case and with a trailing dot.
fn is_slack_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == SLACK_HOST_SUFFIX.trim_start_matches('.') || host.ends_with(SLACK_HOST_SUFFIX)
}

/// The end of the host of a Slack workspace, whose message permalinks the scanner reads.
const SLACK_HOST_SUFFIX: &str = ".slack.com";

fn is_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (2..=32).contains(&scheme.len())
        && first.is_ascii_lowercase()
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn byte_at(text: &str, offset: usize) -> Option<u8> {
    text.as_bytes().get(offset).copied()
}

fn scan_while(text: &str, start: usize, allowed: impl Fn(u8) -> bool) -> usize {
    let mut end = start;
    while byte_at(text, end).is_some_and(&allowed) {
        end += 1;
    }
    end
}

fn is_generic_locator_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'.' | b'_' | b'~' | b'/' | b'@' | b':' | b'+' | b'=' | b'-'
        )
}

fn has_strict_continuation(text: &str, end: usize) -> bool {
    byte_at(text, end).is_some_and(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'@' | b'+' | b'=' | b'-')
    })
}

fn generic_pointer_at(text: &str, start: usize, scheme: &str) -> Option<(Pointer, usize)> {
    let locator_start = start.checked_add(scheme.len() + 1)?;
    if !byte_at(text, locator_start).is_some_and(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    let mut cursor = locator_start;
    let mut end = None;
    while byte_at(text, cursor).is_some_and(is_generic_locator_byte) {
        if byte_at(text, cursor).is_some_and(|byte| byte.is_ascii_alphanumeric()) {
            end = cursor.checked_add(1);
        }
        cursor += 1;
    }
    let end = end?;
    let locator = text.get(locator_start..end)?.to_owned();
    Some((
        Pointer {
            scheme: scheme.to_owned(),
            locator,
        },
        end,
    ))
}

fn slack_channel_at(text: &str, start: usize) -> Option<(String, usize)> {
    let end = scan_while(text, start, |byte| {
        byte.is_ascii_uppercase() || byte.is_ascii_digit()
    });
    let channel = text.get(start..end)?;
    let first = channel.bytes().next()?;
    if channel.len() < 9 || !matches!(first, b'C' | b'D' | b'G') {
        return None;
    }
    Some((channel.to_owned(), end))
}

fn is_slack_timestamp(timestamp: &str) -> bool {
    timestamp.len() == 17
        && timestamp.bytes().enumerate().all(|(index, byte)| {
            (index == 10 && byte == b'.') || (index != 10 && byte.is_ascii_digit())
        })
}

fn slack_pointer_at(text: &str, start: usize) -> Option<(Pointer, usize)> {
    let locator_start = start.checked_add("slack:".len())?;
    let (channel, channel_end) = slack_channel_at(text, locator_start)?;
    if byte_at(text, channel_end) != Some(b'/') {
        return None;
    }
    let timestamp_start = channel_end.checked_add(1)?;
    let end = timestamp_start.checked_add(17)?;
    let timestamp = text.get(timestamp_start..end)?;
    if !is_slack_timestamp(timestamp) || has_strict_continuation(text, end) {
        return None;
    }
    Some((
        Pointer {
            scheme: "slack".to_owned(),
            locator: format!("{channel}/{timestamp}"),
        },
        end,
    ))
}

fn lower_hex_at(text: &str, start: usize) -> Option<(String, usize)> {
    let end = start.checked_add(16)?;
    let id = text.get(start..end)?;
    id.bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        .then(|| (id.to_owned(), end))
}

fn gmail_pointer_at(text: &str, start: usize) -> Option<(Pointer, usize)> {
    let locator_start = start.checked_add("gmail:".len())?;
    let (locator, end) = text
        .get(locator_start..)
        .filter(|locator| locator.starts_with("thread/"))
        .and_then(|_| {
            let id_start = locator_start.checked_add("thread/".len())?;
            lower_hex_at(text, id_start).map(|(id, end)| (format!("thread/{id}"), end))
        })
        .or_else(|| lower_hex_at(text, locator_start))?;
    if has_strict_continuation(text, end) {
        return None;
    }
    Some((
        Pointer {
            scheme: "gmail".to_owned(),
            locator,
        },
        end,
    ))
}

fn drive_pointer_at(text: &str, start: usize) -> Option<(Pointer, usize)> {
    let locator_start = start.checked_add("drive:".len())?;
    let id_end = scan_while(text, locator_start, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
    });
    let id = text.get(locator_start..id_end)?;
    if id.len() < 20 {
        return None;
    }
    let mut end = id_end;
    if byte_at(text, id_end) == Some(b'@') {
        let revision_start = id_end.checked_add(1)?;
        let revision_end = scan_while(text, revision_start, |byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-')
        });
        let revision = text.get(revision_start..revision_end)?;
        let revision_end = revision
            .bytes()
            .rposition(|byte| byte.is_ascii_alphanumeric())
            .and_then(|position| revision_start.checked_add(position + 1))?;
        end = revision_end;
    }
    if has_strict_continuation(text, end) {
        return None;
    }
    Some((
        Pointer {
            scheme: "drive".to_owned(),
            locator: text.get(locator_start..end)?.to_owned(),
        },
        end,
    ))
}

fn gcal_component_end(text: &str, start: usize, separator: u8) -> usize {
    scan_while(text, start, |byte| {
        byte != separator
            && (byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'@' | b'#' | b'-'))
    })
}

fn gcal_pointer_at(text: &str, start: usize) -> Option<(Pointer, usize)> {
    let locator_start = start.checked_add("gcal:".len())?;
    let calendar_end = gcal_component_end(text, locator_start, b'/');
    if byte_at(text, calendar_end) != Some(b'/')
        || !byte_at(text, calendar_end.checked_sub(1)?)
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    let event_start = calendar_end.checked_add(1)?;
    let event_end = scan_while(text, event_start, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
    });
    if event_start == event_end
        || !byte_at(text, event_end.checked_sub(1)?)
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || has_strict_continuation(text, event_end)
    {
        return None;
    }
    Some((
        Pointer {
            scheme: "gcal".to_owned(),
            locator: text.get(locator_start..event_end)?.to_owned(),
        },
        event_end,
    ))
}

/// A byte an owner name may hold: GitHub owners are alphanumeric and hyphen.
fn is_github_owner_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

/// A byte a repository name may hold: alphanumeric, hyphen, underscore, or dot.
fn is_github_repo_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// Parses a GitHub pointer (contract C7):
///
/// `github:<owner>/<repo>#<n>` and its `/comment/<id>`, `/review/<id>` and
/// `/review-comment/<id>` forms, or `github:<owner>/<repo>@<sha>` for a commit message.
fn github_pointer_at(text: &str, start: usize) -> Option<(Pointer, usize)> {
    let locator_start = start.checked_add("github:".len())?;
    let owner_end = scan_while(text, locator_start, is_github_owner_byte);
    if owner_end == locator_start || byte_at(text, owner_end) != Some(b'/') {
        return None;
    }
    let repo_start = owner_end.checked_add(1)?;
    let repo_end = scan_while(text, repo_start, is_github_repo_byte);
    if repo_end == repo_start {
        return None;
    }
    let end = match byte_at(text, repo_end) {
        Some(b'#') => github_item_end(text, repo_end.checked_add(1)?)?,
        Some(b'@') => github_commit_end(text, repo_end.checked_add(1)?)?,
        _ => return None,
    };
    if has_strict_continuation(text, end) {
        return None;
    }
    Some((
        Pointer {
            scheme: "github".to_owned(),
            locator: text.get(locator_start..end)?.to_owned(),
        },
        end,
    ))
}

/// The end of an issue or pull-request item: `<n>` and an optional sub-thread form.
fn github_item_end(text: &str, number_start: usize) -> Option<usize> {
    let number_end = scan_while(text, number_start, |byte| byte.is_ascii_digit());
    if number_end == number_start {
        return None;
    }
    if byte_at(text, number_end) != Some(b'/') {
        return Some(number_end);
    }
    let kind_start = number_end.checked_add(1)?;
    let kind_end = scan_while(text, kind_start, |byte| {
        byte.is_ascii_lowercase() || byte == b'-'
    });
    let kind = text.get(kind_start..kind_end)?;
    if !matches!(kind, "comment" | "review" | "review-comment")
        || byte_at(text, kind_end) != Some(b'/')
    {
        return None;
    }
    let id_start = kind_end.checked_add(1)?;
    let id_end = scan_while(text, id_start, |byte| byte.is_ascii_digit());
    (id_end != id_start).then_some(id_end)
}

/// The end of a commit locator: a lower-hex SHA of seven to forty digits.
fn github_commit_end(text: &str, sha_start: usize) -> Option<usize> {
    let sha_end = scan_while(text, sha_start, |byte| {
        byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
    });
    let length = sha_end.checked_sub(sha_start)?;
    (7..=40).contains(&length).then_some(sha_end)
}

fn slack_permalink_at(text: &str, start: usize) -> Option<Found> {
    let protocol_length = if text.get(start..)?.starts_with("https://") {
        "https://".len()
    } else if text.get(start..)?.starts_with("http://") {
        "http://".len()
    } else {
        return None;
    };
    let host_start = start.checked_add(protocol_length)?;
    let host_end = scan_while(text, host_start, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
    });
    let host = text.get(host_start..host_end)?;
    if !host.ends_with(SLACK_HOST_SUFFIX) || byte_at(text, host_end) != Some(b'/') {
        return None;
    }
    let archives_start = host_end.checked_add(1)?;
    if !text.get(archives_start..)?.starts_with("archives/") {
        return None;
    }
    let channel_start = archives_start.checked_add("archives/".len())?;
    let (channel, channel_end) = slack_channel_at(text, channel_start)?;
    let permalink_start = channel_end.checked_add(2)?;
    if text.get(channel_end..)?.starts_with("/p") {
        let digits_end = permalink_start.checked_add(16)?;
        let digits = text.get(permalink_start..digits_end)?;
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let url_end = url_end(text, digits_end);
        let query = text
            .get(digits_end..url_end)?
            .strip_prefix('?')
            .unwrap_or_default();
        let timestamp = permalink_timestamp(digits)?;
        let thread = query
            .split('&')
            .filter_map(|parameter| parameter.strip_prefix("thread_ts="))
            .find(|value| is_slack_timestamp(value))
            .map(|value| Pointer {
                scheme: "slack".to_owned(),
                locator: format!("{channel}/{value}"),
            });
        return Some(Found {
            pointer: Pointer {
                scheme: "slack".to_owned(),
                locator: format!("{channel}/{timestamp}"),
            },
            thread,
            start,
            end: url_end,
        });
    }
    None
}

fn url_end(text: &str, start: usize) -> usize {
    let mut end = start;
    while byte_at(text, end).is_some_and(|byte| {
        !byte.is_ascii_whitespace() && !matches!(byte, b')' | b'>' | b']' | b'`')
    }) {
        end += 1;
    }
    while end > start {
        let Some(previous) = end.checked_sub(1) else {
            break;
        };
        if !byte_at(text, previous)
            .is_some_and(|byte| matches!(byte, b'.' | b',' | b';' | b':' | b'!' | b'?'))
        {
            break;
        }
        end = previous;
    }
    end
}

fn permalink_timestamp(digits: &str) -> Option<String> {
    if digits.len() != 16 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds = digits.get(..10)?;
    let micros = digits.get(10..)?;
    Some(format!("{seconds}.{micros}"))
}

#[cfg(test)]
mod tests {
    use super::{holds_web_scheme, is_source_link, is_web_address, Schemes};

    fn parsed(text: &str) -> Option<String> {
        Schemes::new([])
            .parse(text)
            .map(|pointer| pointer.to_string())
    }

    #[test]
    fn parses_every_github_pointer_form() {
        for text in [
            "github:example-org/example-repo#12",
            "github:example-org/example-repo#12/comment/3894201101",
            "github:example-org/infra#7/review/2210047",
            "github:example-org/infra#7/review-comment/998877",
            "github:example-org/example-repo@d56570d",
            "github:example-org/example-repo@41481dbb1f0f0ad1f6cf9e83d3a6ad1d0ec4a0f1",
            "github:other-org/memory#1",
            "github:example-org/example.repo#3",
        ] {
            assert_eq!(parsed(text).as_deref(), Some(text), "{text}");
        }
    }

    #[test]
    fn a_github_pointer_without_an_item_or_a_commit_is_not_a_pointer() {
        for text in [
            "github:example-org",
            "github:example-org/example-repo",
            "github:example-org/example-repo#",
            "github:example-org/example-repo#abc",
            "github:/example-repo#1",
            "github:example-org/example-repo#12/comment/",
            "github:example-org/example-repo#12/comment/xyz",
            "github:example-org/example-repo#12/reaction/4",
            "github:example-org/example-repo@zzzzzzz",
            "github:example-org/example-repo@d565",
        ] {
            assert_eq!(parsed(text), None, "{text}");
        }
    }

    #[test]
    fn a_github_pointer_ends_before_the_prose_around_it() {
        let schemes = Schemes::new([]);
        let found = schemes.find_all(
            "see (github:example-org/example-repo#12), and github:example-org/example-repo#12/review/88. Done.",
        );
        let pointers: Vec<String> = found
            .iter()
            .map(|entry| entry.pointer.to_string())
            .collect();
        assert_eq!(
            pointers,
            [
                "github:example-org/example-repo#12",
                "github:example-org/example-repo#12/review/88"
            ]
        );
    }

    /// A web address is an http or https URL with a host and nothing around it. A pointer of
    /// another scheme, prose that holds a URL, a URL with no host, one that carries a user
    /// name, and one that holds a control character are not one.
    #[test]
    fn a_web_address_is_one_http_url_with_a_host() {
        for text in [
            "https://example.com/ask/1",
            "http://example.com",
            "  https://example.com/pull/12#issuecomment-3  ",
            "https://example.com:8443/a?b=c",
            "http://127.0.0.1:4000/ask/1",
            "HTTPS://example.com/ask/1",
        ] {
            assert!(is_web_address(text), "{text:?}");
        }
        for text in [
            "https:// example.com",
            "https://",
            "https:///path",
            "https://:443/path",
            "https://@/path",
            "https://user@/path",
            "https://user@example.com/path",
            "https://user:secret@example.com/path",
            "https://example.com/\u{1b}[2J",
            "https://example.com/\0x",
            "https://example.com/\u{9b}31m",
            "ftp://example.com/file",
            "see https://example.com",
            "https://example.com and more",
            "HTTPS://:443/path",
            "HTTPS://example.com:99999/path",
            "https://exa mple.com/path",
            "https:// /path",
            "foo:bar",
            "slack:C0EXAMPLE01/1700000000.000001",
            "overheard in standup",
            "",
        ] {
            assert!(!is_web_address(text), "{text:?}");
        }
    }

    /// The scanner reads a Slack permalink as a Slack pointer, so a web address on a Slack host
    /// names a message a source checks. Every form of it the URL parser reads as that host is a
    /// source link, whatever the scanner made of it; an address on any other host is not.
    #[test]
    fn a_web_address_on_a_slack_host_is_a_source_link() {
        for text in [
            "https://acme.slack.com:443/archives/C0EXAMPLE01/p1700000000000001",
            "https://ACME.Slack.COM/archives/C0EXAMPLE01/p1700000000000001",
            "https://acme.slack.com./archives/C0EXAMPLE01/p1700000000000001",
            "https://user@acme.slack.com/archives/C0EXAMPLE01/p1700000000000001",
            "https://a)b@acme.slack.com:443/archives/C0EXAMPLE01/p1700000000000001",
            "https://acme.slack.com/archives//C0EXAMPLE01/p1700000000000001",
            "https://%61cme.slack.com/archives/C0EXAMPLE01/p1700000000000001",
            "https://acme.\u{ff53}lack.com/archives/C0EXAMPLE01/p1700000000000001",
            "http://slack.com/archives/C0EXAMPLE01",
        ] {
            assert!(is_source_link(text), "{text:?}");
        }
        for text in [
            "https://example.com/ask/1",
            "https://slack.com.example.com/archives/C0EXAMPLE01",
            "https://notslack.com/archives/C0EXAMPLE01",
            "overheard in standup",
            "slack:C0EXAMPLE01/1700000000.000001",
        ] {
            assert!(!is_source_link(text), "{text:?}");
        }
    }

    /// A text holds a web address when an http or https scheme stands anywhere in it, in any
    /// case, even split by the tabs and line breaks a URL parser drops, or with no slashes
    /// after it, which the parser does not need.
    #[test]
    fn a_text_with_an_http_scheme_anywhere_holds_a_web_address() {
        for text in [
            "https://example.com/ask/1",
            "  HTTP://x y",
            "Https://example.com, said Ada",
            "[message](https://example.com/ask/1)",
            "source=https://a)b@example.com",
            "ht\ttps://example.com",
            "htt\nps://example.com",
            "see https:example.com",
        ] {
            assert!(holds_web_scheme(text), "{text:?}");
        }
        for text in [
            "overheard in standup",
            "ftp://example.com",
            "slack:C0EXAMPLE01/1700000000.000001",
            "the http protocol",
            "",
        ] {
            assert!(!holds_web_scheme(text), "{text:?}");
        }
    }
}
