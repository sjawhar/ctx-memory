//! The content net: items a deployment never stores because their text names a project it keeps
//! out, by the project's key or by a term of it, or because a person outside it takes part in
//! them, by address.
//!
//! A project's own items carry no pointer a pattern could name: a mail, a document or a meeting
//! about it lives in a mailbox, a drive or a calendar shared with everything else. What marks it
//! is its key in the text: an issue of the project, `<KEY>-<number>`, or a link into it,
//! `dispatch://<KEY>` or a Dispatch web link `/projects/<KEY>`; a term of it, such as one of its
//! repositories as `owner/name` or its name; or an address of a person only that project shares
//! with. The Gmail, Drive and Calendar sources each drop an item that the net names, before it
//! is a record, and log the drop to standard error by reason and pointer ([`withheld`]). An item
//! about the project that names no key, term or link, and that no excluded address takes part
//! in, is not caught.
//!
//! The net, and every other exclusion setting of a source (List-IDs, repository ids), is
//! refused unless `--content-net` turned them on ([`enable`]): only a ctx that applies them
//! takes that argument, so a deployment that sets them never runs a ctx that would ignore them.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context as _, Result};
use regex::Regex;
use serde_json::Value;

use crate::record::Record;

mod normalize;
#[cfg(test)]
pub(super) mod tests;

pub(super) use normalize::decode_hex_escapes;
use normalize::normalized;

/// The environment variable that adds keys, one per comma, to a source's `exclude_keys`.
pub(super) const EXCLUDE_KEYS_ENV: &str = "CTX_EXCLUDE_KEYS";
/// The environment variable that adds addresses, one per comma, to a source's `exclude_emails`.
pub(super) const EXCLUDE_EMAILS_ENV: &str = "CTX_EXCLUDE_EMAILS";
/// The environment variable that adds terms, one per comma, to a source's `exclude_terms`.
pub(super) const EXCLUDE_TERMS_ENV: &str = "CTX_EXCLUDE_TERMS";
/// The environment variable that adds repositories, `owner/name`, one per comma, to a source's
/// `exclude_repositories`.
pub(super) const EXCLUDE_REPOSITORIES_ENV: &str = "CTX_EXCLUDE_REPOSITORIES";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns the exclusion settings on, as `--content-net` does: until then a source refuses them.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// Refuses `setting`, which is set, unless `--content-net` turned the exclusion settings on.
///
/// # Errors
///
/// Returns an error when the settings are off.
pub(super) fn require(setting: &str) -> Result<()> {
    if ENABLED.load(Ordering::SeqCst) {
        return Ok(());
    }
    bail!(
        "{setting} is set, but ctx was started without --content-net; give --content-net to apply the exclusion settings (a ctx too old to apply them refuses that argument, so it never runs with them unapplied)"
    )
}

/// Refuses each of `variables` that is set to a value naming anything, unless `--content-net`
/// turned the exclusion settings on, so `fetch` and `serve` stop before they read anything.
///
/// # Errors
///
/// Returns an error when one is set while the settings are off, or cannot be read.
pub(super) fn refuse_unless_enabled(variables: &[&str]) -> Result<()> {
    for variable in variables {
        let named = super::environment(variable)?
            .is_some_and(|value| value.split(',').any(|entry| !entry.trim().is_empty()));
        if named {
            require(variable)?;
        }
    }
    Ok(())
}

/// Why a source withheld an item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Hit {
    /// Its text names this key of the content net.
    Key(String),
    /// Its text names a term of the content net: a repository or a phrase, which the log does
    /// not name, since a term may be confidential.
    Term,
    /// An address of the content net takes part in it.
    Address,
    /// Who holds a permission on it could not be read, so no address could be checked.
    PermissionsUnread,
    /// It came from a mailing list the deployment excludes.
    ListId,
    /// It is a repository the deployment excludes by id.
    RepositoryId,
}

/// Logs to standard error that the item at `pointer` was withheld, and why: one JSON line that
/// names the key, when a key withheld it, and the pointer, and never the item's title, file
/// name, address, term or text. It is no warning of the window, which the shared store keeps.
pub(super) fn withheld(hit: &Hit, pointer: &str) {
    let (reason, key) = match hit {
        Hit::Key(key) => ("private-content-net", Some(key.as_str())),
        Hit::Term => ("private-term", None),
        Hit::Address => ("private-address", None),
        Hit::PermissionsUnread => ("permissions-unread", None),
        Hit::ListId => ("private-list-id", None),
        Hit::RepositoryId => ("private-repository-id", None),
    };
    let key = key.map_or_else(String::new, |key| format!(r#","key":{}"#, Value::from(key)));
    let line = format!(
        r#"{{"event":"withheld","reason":"{reason}"{key},"pointer":{}}}"#,
        Value::from(pointer)
    );
    eprintln!("{line}");
    #[cfg(test)]
    tests::LOGGED.with(|logged| logged.borrow_mut().push(line));
}

/// The content net's lists as one source's configuration names them.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NetLists<'a> {
    /// `exclude_keys`: project keys.
    pub(super) keys: &'a [String],
    /// `exclude_terms`: whole terms, such as a project's name.
    pub(super) terms: &'a [String],
    /// `exclude_repositories`: repositories, `owner/name`.
    pub(super) repositories: &'a [String],
    /// `exclude_emails`: whole addresses.
    pub(super) addresses: &'a [String],
}

/// The keys, terms and addresses a deployment excludes.
#[derive(Clone, Debug, Default)]
pub(super) struct ContentNet {
    keys: Option<Keys>,
    /// The terms, as given, and the expression that matches each whole, in any case, bounded as
    /// a key is.
    terms: Vec<String>,
    term_net: Option<Regex>,
    /// The repositories, as given, and the expression that matches each whole, in any case,
    /// where `_` and `-` continue a name, so `owner/name-tools` is another repository.
    repositories: Vec<String>,
    repository_net: Option<Regex>,
    /// Whole addresses, lowercased.
    addresses: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Keys {
    net: Regex,
    keys: Vec<String>,
}

/// Reads one environment variable: its value, or `None` when it is not set.
pub(super) type Environment<'a> = &'a dyn Fn(&str) -> Result<Option<String>>;

impl ContentNet {
    /// The net `lists` make with what [`EXCLUDE_KEYS_ENV`], [`EXCLUDE_TERMS_ENV`],
    /// [`EXCLUDE_REPOSITORIES_ENV`] and [`EXCLUDE_EMAILS_ENV`] add, in the environment of the
    /// process that runs the source, never a caller's.
    ///
    /// # Errors
    ///
    /// Returns an error when a variable cannot be read, an entry is not a key, term, repository
    /// or address, or the net names anything while `--content-net` did not turn the settings on.
    pub(super) fn configured(lists: &NetLists<'_>) -> Result<Self> {
        let net = Self::build(lists, &super::environment)?;
        if !net.is_empty() {
            require("the content net (exclude_keys, exclude_terms, exclude_repositories, exclude_emails or their variables)")?;
        }
        Ok(net)
    }

    /// The net `lists` make with what `environment` reads of the net's variables added.
    ///
    /// # Errors
    ///
    /// Returns an error when a variable cannot be read, or an entry is not a key, term,
    /// repository or address.
    pub(super) fn build(lists: &NetLists<'_>, environment: Environment<'_>) -> Result<Self> {
        Self::from_parts(lists.keys, environment(EXCLUDE_KEYS_ENV)?.as_deref())
            .context("reading the excluded keys")?
            .with_terms(lists.terms, environment(EXCLUDE_TERMS_ENV)?.as_deref())
            .context("reading the excluded terms")?
            .with_repositories(
                lists.repositories,
                environment(EXCLUDE_REPOSITORIES_ENV)?.as_deref(),
            )
            .context("reading the excluded repositories")?
            .with_addresses(lists.addresses, environment(EXCLUDE_EMAILS_ENV)?.as_deref())
            .context("reading the excluded addresses")
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
        let mut keys: Vec<String> = Vec::new();
        for key in entries(configured, environment) {
            let mut characters = key.chars();
            let well_formed = characters.next().is_some_and(|c| c.is_ascii_uppercase())
                && characters.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !well_formed {
                bail!(
                    "{key:?} is not a project key: a key is an upper-case letter followed by upper-case letters, digits or underscores, such as SYNTH"
                );
            }
            if !keys.iter().any(|known| known == key) {
                keys.push(key.to_owned());
            }
        }
        if keys.is_empty() {
            return Ok(Self::default());
        }
        let alternatives = keys
            .iter()
            .map(|key| regex::escape(key))
            .collect::<Vec<_>>()
            .join("|");
        let net = Regex::new(&format!(
            r"(?i)(?:^|[^a-z0-9])({alternatives})-[0-9]+(?:[^a-z0-9]|$)|dispatch://({alternatives})(?:[^a-z0-9]|$)|/projects/({alternatives})(?:[^a-z0-9]|$)"
        ))
        .context("building the content net")?;
        Ok(Self {
            keys: Some(Keys { net, keys }),
            ..Self::default()
        })
    }

    /// The net with the terms of a configuration list and of an environment value,
    /// comma-separated, added: each matched whole, in any case, any run of whitespace inside it
    /// matching any other, and bounded as a key is, by anything but an ASCII letter or digit, so
    /// `Falcon_plan_v2.docx` and `Falcon-team` name `Falcon`.
    ///
    /// # Errors
    ///
    /// Returns an error when a term is shorter than three characters, which would match too much.
    pub(super) fn with_terms(
        mut self,
        configured: &[String],
        environment: Option<&str>,
    ) -> Result<Self> {
        for term in entries(configured, environment) {
            if term.chars().count() < 3 {
                bail!("{term:?} is not a term: a term has three characters or more, or it would drop most items");
            }
            adding(&mut self.terms, term);
        }
        self.term_net = whole(&self.terms, "a-z0-9").context("building the content net's terms")?;
        Ok(self)
    }

    /// The net with the repositories of a configuration list and of an environment value,
    /// comma-separated, added: each `owner/name`, matched whole, in any case, so also in
    /// `github.com/owner/name`. `_` and `-` continue a repository's name, so `owner/name-tools`
    /// is another repository.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is not `owner/name`. A bare name is refused: one such as
    /// `rocq` would drop every item that mentions the word.
    pub(super) fn with_repositories(
        mut self,
        configured: &[String],
        environment: Option<&str>,
    ) -> Result<Self> {
        let part = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        for repository in entries(configured, environment) {
            let well_formed = repository
                .split_once('/')
                .is_some_and(|(owner, name)| part(owner) && part(name));
            if !well_formed {
                bail!(
                    "{repository:?} is not a repository as owner/name, such as example-org/busy; a bare name would drop every item that mentions the word"
                );
            }
            adding(&mut self.repositories, repository);
        }
        self.repository_net = whole(&self.repositories, "a-z0-9_-")
            .context("building the content net's repositories")?;
        Ok(self)
    }

    /// The net with the addresses of a configuration list and of an environment value,
    /// comma-separated, added: whole addresses, matched in any case.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is not one address alone (`name@domain.tld`, with no
    /// display name, angle brackets or space), which would match no one.
    pub(super) fn with_addresses(
        mut self,
        configured: &[String],
        environment: Option<&str>,
    ) -> Result<Self> {
        for address in entries(configured, environment) {
            let well_formed = address.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty()
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
                    && !domain.contains('@')
            }) && !address.contains(is_address_separator);
            if !well_formed {
                bail!("{address:?} is not an email address alone, such as alice@example.com");
            }
            self.addresses.insert(address.to_ascii_lowercase());
        }
        Ok(self)
    }

    /// Whether the net names no key, term, repository or address, so nothing matches it.
    pub(super) fn is_empty(&self) -> bool {
        !self.has_text_net() && self.addresses.is_empty()
    }

    /// Whether the net names a key, a term or a repository, which text can name.
    pub(super) fn has_text_net(&self) -> bool {
        self.keys.is_some() || self.term_net.is_some() || self.repository_net.is_some()
    }

    /// Whether the net names an address.
    pub(super) fn has_addresses(&self) -> bool {
        !self.addresses.is_empty()
    }

    /// Whether `text` names an excluded key or term.
    #[cfg(test)]
    pub(super) fn matches(&self, text: &str) -> bool {
        self.hit_in(text).is_some()
    }

    /// Why the net withholds an item whose text is `text`: an excluded key or term it names, as
    /// it reads or as a reader of it would see it ([`normalized`]).
    pub(super) fn hit_in(&self, text: &str) -> Option<Hit> {
        if !self.has_text_net() {
            return None;
        }
        self.hit_as_read(text)
            .or_else(|| normalized(text).and_then(|seen| self.hit_as_read(&seen)))
    }

    /// The excluded key, term or repository `text` names as it reads.
    fn hit_as_read(&self, text: &str) -> Option<Hit> {
        let names = |net: &Option<Regex>| net.as_ref().is_some_and(|net| net.is_match(text));
        self.keys
            .as_ref()
            .and_then(|keys| keys.key_in(text))
            .map(|key| Hit::Key(key.to_owned()))
            .or_else(|| (names(&self.term_net) || names(&self.repository_net)).then_some(Hit::Term))
    }

    /// Why the net withholds an item for any string `value` holds, at any depth: a value, or the
    /// name of a field, since a source's raw data can carry text there too (a Calendar event's
    /// `extendedProperties`).
    pub(super) fn hit_in_value(&self, value: &Value) -> Option<Hit> {
        if !self.has_text_net() {
            return None;
        }
        match value {
            Value::String(text) => self.hit_in(text),
            Value::Array(items) => items.iter().find_map(|item| self.hit_in_value(item)),
            Value::Object(fields) => fields
                .iter()
                .find_map(|(name, field)| self.hit_in(name).or_else(|| self.hit_in_value(field))),
            Value::Null | Value::Bool(_) | Value::Number(_) => None,
        }
    }

    /// Why the net withholds `record`, by a key or term in what it would store: its title, text,
    /// author, label or link, or any string of its raw data.
    pub(super) fn hit_in_record(&self, record: &Record) -> Option<Hit> {
        if !self.has_text_net() {
            return None;
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
        .find_map(|text| self.hit_in(text))
        .or_else(|| self.hit_in_value(&record.raw))
    }

    /// Whether `text`, an address or a header that lists some (`"Ada" <ada@example.com>, …`),
    /// names an excluded address.
    pub(super) fn names_address(&self, text: &str) -> bool {
        !self.addresses.is_empty()
            && text
                .split(is_address_separator)
                .filter(|token| token.contains('@'))
                .any(|token| self.is_excluded_address(token))
    }

    /// Whether `address`, one address alone, is an excluded one, in any case.
    pub(super) fn is_excluded_address(&self, address: &str) -> bool {
        self.addresses.contains(&address.to_ascii_lowercase())
    }
}

impl Keys {
    /// The configured key `text` names as it reads.
    fn key_in(&self, text: &str) -> Option<&str> {
        let captures = self.net.captures(text)?;
        let named = (1..=3).find_map(|group| captures.get(group))?.as_str();
        self.keys
            .iter()
            .find(|key| key.eq_ignore_ascii_case(named))
            .or_else(|| {
                self.keys
                    .iter()
                    .find(|key| key.to_uppercase() == named.to_uppercase())
            })
            .or_else(|| self.keys.first())
            .map(String::as_str)
    }
}

/// The entries of a configuration list and of an environment value, comma-separated, trimmed.
/// An empty entry between two commas of the environment is skipped; an empty one in the
/// configuration is kept, so the caller refuses it.
fn entries<'a>(
    configured: &'a [String],
    environment: Option<&'a str>,
) -> impl Iterator<Item = &'a str> {
    let listed = environment
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty());
    configured.iter().map(|entry| entry.trim()).chain(listed)
}

fn adding(known: &mut Vec<String>, entry: &str) {
    if !known.iter().any(|it| it.eq_ignore_ascii_case(entry)) {
        known.push(entry.to_owned());
    }
}

/// The expression that matches each of `entries` whole, in any case, any run of whitespace
/// inside one matching any other, where the characters of the class `inner` continue a word, so
/// one of them next to an entry makes it another word. `None` when there are no entries.
fn whole(entries: &[String], inner: &str) -> Result<Option<Regex>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let alternatives = entries
        .iter()
        .map(|entry| {
            entry
                .split_whitespace()
                .map(regex::escape)
                .collect::<Vec<_>>()
                .join(r"\s+")
        })
        .collect::<Vec<_>>()
        .join("|");
    let net = Regex::new(&format!(
        r"(?i)(?:^|[^{inner}])(?:{alternatives})(?:[^{inner}]|$)"
    ))?;
    Ok(Some(net))
}

/// Whether `c` cannot be part of an address in a header, so it separates two of them or an
/// address from its display name.
fn is_address_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | ':' | '"' | '(' | ')' | '[' | ']')
}
