//! The text ctx serves for a stored Slack message, and the ways a quote of one may spell its
//! mentions.
//!
//! A message stored before mentions were named, or one whose mention Slack could not name,
//! shows a person as their bare id. ctx serves it with that person named from what the store
//! already holds. A fetch names a mention by asking Slack, but only the fetch job holds a Slack
//! credential: whatever serves a record to a reader has none, so nothing here asks Slack. A
//! person's names come from the messages they wrote: the full name Slack sends with a message
//! as its sender's profile, and the name the fetch gave them as an author. Nothing here changes
//! a stored record.

use std::cell::RefCell;
use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

use crate::config::SourceConfig;
use crate::pointer::Pointer;
use crate::record::Record;
use crate::store::Store;

use super::records::{served_text, spellings, Spelling};

/// Where a stored Slack message keeps its sender's full name: the profile Slack sends with it.
const FULL_NAME: &str = "$.raw.user_profile.real_name";

/// Where a stored Slack message keeps its sender's display name, in the same profile.
const DISPLAY_NAME: &str = "$.raw.user_profile.display_name";

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

/// One part of a stored Slack message as a quote may have read it: text every reading shares,
/// or a mention, which a quote may spell any one of several ways.
pub enum Spelled {
    Text(String),
    OneOf(Vec<String>),
}

/// Serves stored Slack messages with the people they mention named, for one command: each
/// person is looked up in the store once.
///
/// A person a stored message still shows as their bare id reads as the name the store holds
/// for them: the full name a message of theirs carries, else the name their messages' author
/// line gives, else the label Slack wrote into the mention, else the id. Every other mention
/// reads as stored ([`served_text`]).
pub struct Mentions<'s> {
    store: &'s Store,
    schemes: Vec<String>,
    names: RefCell<BTreeMap<String, Option<String>>>,
    every_name: RefCell<BTreeMap<String, Vec<String>>>,
}

impl<'s> Mentions<'s> {
    /// Serves the records of `schemes`, the Slack sources' ([`slack_schemes`]), from `store`.
    #[must_use]
    pub fn new(store: &'s Store, schemes: Vec<String>) -> Self {
        Self {
            store,
            schemes,
            names: RefCell::new(BTreeMap::new()),
            every_name: RefCell::new(BTreeMap::new()),
        }
    }

    /// The text to serve for `record`, a record read from the store, when it is not the stored
    /// text. Only a Slack message whose stored text shows a person as their bare id changes.
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
        served_text(text, &record.text, &mut |id| self.name(id))
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

    /// `record`, a Slack message read from the store, in the parts a quote of it is checked
    /// against: each mention spelled every way a reader may have seen it, and the rest as a
    /// fetch renders it. A person may be spelled by their id, the label Slack wrote, what the
    /// stored text shows, or any name the store holds for that id: every full name and author
    /// name their stored messages carry, so a quote taken before a name changed still holds.
    /// No other person's name stands for them. `None` when the record is no Slack message or
    /// mentions nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn spelled(&self, record: &Record) -> Result<Option<Vec<Spelled>>> {
        if !self.holds(&record.pointer) {
            return Ok(None);
        }
        let (text, stored) = match record.raw.get("text").and_then(Value::as_str) {
            Some(raw) => (raw, Some(record.text.as_str())),
            None => (record.text.as_str(), None),
        };
        let parts = spellings(text, stored);
        if !parts
            .iter()
            .any(|part| matches!(part, Spelling::Mention { .. }))
        {
            return Ok(None);
        }
        parts
            .into_iter()
            .map(|part| match part {
                Spelling::Text(text) => Ok(Spelled::Text(text)),
                Spelling::Mention {
                    person,
                    mut spellings,
                } => {
                    if let Some(id) = person {
                        spellings.extend(
                            self.every_name(id)?
                                .into_iter()
                                .map(|name| format!("@{name}")),
                        );
                    }
                    Ok(Spelled::OneOf(spellings))
                }
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
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

    /// Every name the store holds for the person `id` names: each full name, display name and
    /// author name their stored messages carry.
    fn every_name(&self, id: &str) -> Result<Vec<String>> {
        if let Some(known) = self.every_name.borrow().get(id) {
            return Ok(known.clone());
        }
        let mut names = Vec::new();
        for path in [FULL_NAME, DISPLAY_NAME, AUTHOR_NAME] {
            names.extend(self.store.authored_all(&self.schemes, id, path)?);
        }
        names.sort();
        names.dedup();
        self.every_name
            .borrow_mut()
            .insert(id.to_owned(), names.clone());
        Ok(names)
    }
}
