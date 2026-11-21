//! The content net: items a deployment never stores because their text names a project it keeps
//! out, by the project's key.
//!
//! A project's own items carry no pointer a pattern could name: a mail, a document or a meeting
//! about it lives in a mailbox, a drive or a calendar shared with everything else. What marks it
//! is its key in the text: an issue of the project, `<KEY>-<number>`, or a link into it,
//! `dispatch://<KEY>`. The Gmail, Drive and Calendar sources each drop an item whose text
//! matches, before it is a record. An item about the project that names no key, issue or link
//! is not caught.

use anyhow::{bail, Context as _, Result};
use regex::Regex;
use serde_json::Value;

use crate::record::Record;

/// The environment variable that adds keys, one per comma, to a source's `exclude_keys`.
pub(super) const EXCLUDE_KEYS_ENV: &str = "CTX_EXCLUDE_KEYS";

/// The keys a deployment excludes, as one expression: each key as `\b<KEY>-[0-9]+\b` and as
/// `dispatch://<KEY>`, matched with their case. `None` when there are no keys.
#[derive(Clone, Debug, Default)]
pub(super) struct ContentNet(Option<Regex>);

impl ContentNet {
    /// The source configuration's `exclude_keys` and the keys [`EXCLUDE_KEYS_ENV`] names, in the
    /// environment of the process that runs the source, never a caller's.
    ///
    /// # Errors
    ///
    /// Returns an error when the variable cannot be read or a key is not one (see
    /// [`ContentNet::from_parts`]).
    pub(super) fn configured(configured: &[String]) -> Result<Self> {
        Self::from_parts(configured, super::environment(EXCLUDE_KEYS_ENV)?.as_deref())
            .context("reading the excluded keys")
    }

    /// The keys of a configuration list and of an environment value, comma-separated. Both lists
    /// apply, so neither can take a key off the other. An empty entry between two commas of the
    /// environment is skipped.
    ///
    /// # Errors
    ///
    /// Returns an error when a key is not an upper-case letter followed by upper-case letters,
    /// digits or underscores. A key in another form, such as `synth` or `SYNTH-1`, would match
    /// no item's key, so the items it was meant to exclude would be stored.
    pub(super) fn from_parts(configured: &[String], environment: Option<&str>) -> Result<Self> {
        let listed = environment
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|key| !key.is_empty());
        let mut keys: Vec<&str> = Vec::new();
        for key in configured.iter().map(|key| key.trim()).chain(listed) {
            let mut characters = key.chars();
            let well_formed = characters.next().is_some_and(|c| c.is_ascii_uppercase())
                && characters.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !well_formed {
                bail!(
                    "{key:?} is not a project key: a key is an upper-case letter followed by upper-case letters, digits or underscores, such as SYNTH"
                );
            }
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Ok(Self(None));
        }
        let alternatives = keys
            .iter()
            .map(|key| regex::escape(key))
            .collect::<Vec<_>>()
            .join("|");
        let net = Regex::new(&format!(
            r"\b(?:{alternatives})-[0-9]+\b|dispatch://(?:{alternatives})"
        ))
        .context("building the content net")?;
        Ok(Self(Some(net)))
    }

    /// Whether the net names no key, so nothing matches it.
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Whether `text` names an excluded key.
    pub(super) fn matches(&self, text: &str) -> bool {
        self.0.as_ref().is_some_and(|net| net.is_match(text))
    }

    /// Whether any string `value` holds, at any depth, names an excluded key: a value, or the
    /// name of a field, since a source's raw data can carry text there too (a Calendar event's
    /// `extendedProperties`).
    pub(super) fn matches_value(&self, value: &Value) -> bool {
        if self.is_empty() {
            return false;
        }
        match value {
            Value::String(text) => self.matches(text),
            Value::Array(items) => items.iter().any(|item| self.matches_value(item)),
            Value::Object(fields) => fields
                .iter()
                .any(|(name, field)| self.matches(name) || self.matches_value(field)),
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }

    /// Whether anything `record` would store names an excluded key: its title, text, author,
    /// label or link, or any string of its raw data.
    pub(super) fn matches_record(&self, record: &Record) -> bool {
        if self.is_empty() {
            return false;
        }
        [
            record.title.as_deref(),
            Some(record.text.as_str()),
            record.author.as_deref(),
            record.label.as_deref(),
            record.url.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| self.matches(text))
            || self.matches_value(&record.raw)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::ContentNet;

    fn net(keys: &[&str]) -> Result<ContentNet> {
        let keys = keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
        ContentNet::from_parts(&keys, None)
    }

    /// Each key matches as an issue, `<KEY>-<number>` between word boundaries, and as a link,
    /// `dispatch://<KEY>`, with its case. An issue of another key that starts or ends like it,
    /// a number that runs into a word, and the key alone do not match. Control: with no key,
    /// nothing does.
    #[test]
    fn a_key_matches_as_an_issue_or_a_link_and_nothing_else() -> Result<()> {
        let synth = net(&["SYNTH"])?;
        for text in [
            "see SYNTH-12.",
            "SYNTH-1",
            "(SYNTH-3040)",
            "branch INFRA-1-SYNTH-7-fix",
            "open dispatch://SYNTH/artifact/plan",
            "dispatch://SYNTHB",
        ] {
            assert!(synth.matches(text), "{text}");
            assert!(!ContentNet::default().matches(text), "{text}");
        }
        for text in [
            "SYNTHB-12",
            "XSYNTH-12",
            "_SYNTH-12",
            "SYNTH-12a",
            "SYNTH-",
            "SYNTH",
            "synth-12",
            "dispatch://synth",
            "the synth project",
        ] {
            assert!(!synth.matches(text), "{text}");
        }
        Ok(())
    }

    /// A value matches when any string it holds, at any depth, does; a record when its title,
    /// text, author, label, link or raw data does.
    #[test]
    fn a_value_or_a_record_matches_through_any_of_its_strings() -> Result<()> {
        let synth = net(&["SYNTH"])?;
        assert!(synth.matches_value(&json!({"a": [1, {"b": "fix SYNTH-4"}]})));
        assert!(synth.matches_value(&json!({"a": [1, {"b": "fix INFRA-4"}], "SYNTH-4": 1})));
        assert!(!synth.matches_value(&json!({"a": [1, {"b": "fix INFRA-4"}], "INFRA-4": 1})));
        let record = |title: &str, raw: serde_json::Value| crate::record::Record {
            pointer: "gmail:a".to_owned(),
            stream: "me".to_owned(),
            ts: chrono::DateTime::UNIX_EPOCH,
            label: None,
            author: None,
            author_id: None,
            thread: None,
            title: Some(title.to_owned()),
            url: None,
            text: "nothing here".to_owned(),
            aliases: Vec::new(),
            context: false,
            raw,
        };
        assert!(synth.matches_record(&record("SYNTH-9 plan", json!({}))));
        assert!(synth.matches_record(&record("plan", json!({"description": "dispatch://SYNTH"}))));
        assert!(!synth.matches_record(&record("plan", json!({"description": "INFRA-9"}))));
        Ok(())
    }

    /// `CTX_EXCLUDE_KEYS` adds keys to the configuration's, and a key in another form is an
    /// error rather than one that matches nothing.
    #[test]
    fn the_environment_adds_keys_to_the_configurations() -> Result<()> {
        let both = ContentNet::from_parts(&["SYNTH".to_owned()], Some(" SYNTHB ,, SYNTH"))?;
        assert!(both.matches("SYNTH-1") && both.matches("SYNTHB-1"));
        let configured = ContentNet::from_parts(&["SYNTH".to_owned()], None)?;
        assert!(!configured.matches("SYNTHB-1"));
        assert!(!ContentNet::from_parts(&[], Some(""))?.matches("SYNTH-1"));
        for refused in [
            ContentNet::from_parts(&[" ".to_owned()], None),
            ContentNet::from_parts(&["synth".to_owned()], None),
            ContentNet::from_parts(&[], Some("SYNTH-1")),
            ContentNet::from_parts(&[], Some("SYN TH")),
            ContentNet::from_parts(&[], Some("1SYNTH")),
        ] {
            assert!(refused.is_err(), "{refused:?}");
        }
        Ok(())
    }
}
