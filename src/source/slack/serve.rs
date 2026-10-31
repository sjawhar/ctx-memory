//! The text ctx serves for a stored Slack message: the message as Slack sent it, rendered as a
//! fetch renders it now, with each person it mentions named from what the store already holds.
//!
//! A fetch names a mention by asking Slack, but only the fetch job holds a Slack credential:
//! whatever serves a record to a reader has none, so nothing here asks Slack. A person's name
//! comes from the messages they wrote: the full name Slack sends with a message as its
//! sender's profile, else the name the fetch gave them as an author. Nothing here changes a
//! stored record.

use std::cell::RefCell;
use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

use crate::config::SourceConfig;
use crate::pointer::Pointer;
use crate::record::Record;
use crate::store::Store;

use super::records::{mentions, served_text, Mention};

/// Where a stored Slack message keeps its sender's full name: the profile Slack sends with it.
const FULL_NAME: &str = "$.raw.user_profile.real_name";

/// Where a stored record keeps the name the fetch gave its author, which a Slack fetch takes
/// from the same lookup that names the people a message mentions.
const AUTHOR_NAME: &str = "$.author";

/// The pointer schemes of the configured Slack sources: the schemes of the records that hold
/// Slack messages.
///
/// # Errors
///
/// Returns an error when a Slack source has no pointer scheme.
pub fn slack_schemes(sources: &[SourceConfig]) -> Result<Vec<String>> {
    sources
        .iter()
        .filter(|source| source.kind == "slack")
        .map(crate::source::scheme)
        .collect()
}

/// Serves stored Slack messages with the people they mention named, for one command: each
/// person is looked up in the store once.
///
/// A mention reads as the name the store holds for the person: the full name a message of
/// theirs carries, else the name their messages' author line gives, else the name the stored
/// text already shows, else the label Slack wrote into the mention, else the id. The served
/// text is never worse than the stored one ([`served_text`]).
pub struct Mentions<'s> {
    store: &'s Store,
    schemes: Vec<String>,
    names: RefCell<BTreeMap<String, Option<String>>>,
}

impl<'s> Mentions<'s> {
    /// Serves the records of `schemes`, the Slack sources' ([`slack_schemes`]), from `store`.
    #[must_use]
    pub fn new(store: &'s Store, schemes: Vec<String>) -> Self {
        Self {
            store,
            schemes,
            names: RefCell::new(BTreeMap::new()),
        }
    }

    /// The text to serve for `record`, a record read from the store, when it is not the stored
    /// text. Only a Slack message's text changes.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn text(&self, record: &Record) -> Result<Option<String>> {
        if !self.holds(&record.pointer) {
            return Ok(None);
        }
        let Some(text) = record.raw.get("text").and_then(Value::as_str) else {
            return Ok(None);
        };
        let mut people = BTreeMap::new();
        for mention in mentions(text) {
            if let Mention::User { id, .. } = mention {
                if let Some(name) = self.name(id)? {
                    people.insert(id.to_owned(), name);
                }
            }
        }
        Ok(served_text(text, &record.text, &people))
    }

    /// Replaces the text of `record`, a copy read from the store, with the text to serve.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn serve(&self, record: &mut Record) -> Result<()> {
        if let Some(text) = self.text(record)? {
            record.text = text;
        }
        Ok(())
    }

    /// The text to serve for the newest stored version of `pointer` whose stored text
    /// `written` accepts, when it is not that stored text: the version a window's delta was
    /// written from, which a later edit may have replaced.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn version_text(
        &self,
        pointer: &str,
        written: impl Fn(&str) -> bool,
    ) -> Result<Option<String>> {
        let Some((scheme, locator)) = pointer.split_once(':') else {
            return Ok(None);
        };
        if !self.holds(pointer) {
            return Ok(None);
        }
        let pointer = Pointer {
            scheme: scheme.to_owned(),
            locator: locator.to_owned(),
        };
        for (_, version) in self.store.versions(&pointer)?.into_iter().rev() {
            if written(&version.text) {
                return self.text(&version);
            }
        }
        Ok(None)
    }

    /// Whether `pointer` names a Slack message.
    fn holds(&self, pointer: &str) -> bool {
        pointer
            .split_once(':')
            .is_some_and(|(scheme, _)| self.schemes.iter().any(|slack| slack == scheme))
    }

    /// The name the store holds for the person `id` names: the full name the newest message of
    /// theirs that carries one gives, else the name the newest one gives its author.
    fn name(&self, id: &str) -> Result<Option<String>> {
        if let Some(known) = self.names.borrow().get(id) {
            return Ok(known.clone());
        }
        let name = match self.store.authored(&self.schemes, id, FULL_NAME)? {
            Some(full) => Some(full),
            None => self.store.authored(&self.schemes, id, AUTHOR_NAME)?,
        };
        self.names.borrow_mut().insert(id.to_owned(), name.clone());
        Ok(name)
    }
}
