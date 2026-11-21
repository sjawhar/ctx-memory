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

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context as _, Result};
use regex::Regex;
use serde_json::Value;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization as _;

use crate::record::Record;

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
    /// The terms, as given, and the expression that matches each whole, in any case.
    terms: Vec<String>,
    term_net: Option<Regex>,
    /// Whole addresses, lowercased.
    addresses: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Keys {
    net: Regex,
    keys: Vec<String>,
}

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
        let net = Self::from_parts(lists.keys, super::environment(EXCLUDE_KEYS_ENV)?.as_deref())
            .context("reading the excluded keys")?
            .with_terms(
                lists.terms,
                super::environment(EXCLUDE_TERMS_ENV)?.as_deref(),
            )
            .context("reading the excluded terms")?
            .with_repositories(
                lists.repositories,
                super::environment(EXCLUDE_REPOSITORIES_ENV)?.as_deref(),
            )
            .context("reading the excluded repositories")?
            .with_addresses(
                lists.addresses,
                super::environment(EXCLUDE_EMAILS_ENV)?.as_deref(),
            )
            .context("reading the excluded addresses")?;
        if !net.is_empty() {
            require("the content net (exclude_keys, exclude_terms, exclude_repositories, exclude_emails or their variables)")?;
        }
        Ok(net)
    }

    /// The net `lists` make alone, with no environment.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is not a key, term, repository or address.
    #[cfg(test)]
    pub(super) fn from_lists(lists: &NetLists<'_>) -> Result<Self> {
        Self::from_parts(lists.keys, None)?
            .with_terms(lists.terms, None)?
            .with_repositories(lists.repositories, None)?
            .with_addresses(lists.addresses, None)
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
    /// matching any other.
    ///
    /// # Errors
    ///
    /// Returns an error when a term is shorter than three characters, which would match too much.
    pub(super) fn with_terms(
        self,
        configured: &[String],
        environment: Option<&str>,
    ) -> Result<Self> {
        let mut terms = Vec::new();
        for term in entries(configured, environment) {
            if term.chars().count() < 3 {
                bail!("{term:?} is not a term: a term has three characters or more, or it would drop most items");
            }
            terms.push(term.to_owned());
        }
        self.adding_terms(terms)
    }

    /// The net with the repositories of a configuration list and of an environment value,
    /// comma-separated, added as terms: each `owner/name`, so also `github.com/owner/name`.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is not `owner/name`. A bare name is refused: one such as
    /// `rocq` would drop every item that mentions the word.
    pub(super) fn with_repositories(
        self,
        configured: &[String],
        environment: Option<&str>,
    ) -> Result<Self> {
        let part = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let mut repositories = Vec::new();
        for repository in entries(configured, environment) {
            let well_formed = repository
                .split_once('/')
                .is_some_and(|(owner, name)| part(owner) && part(name));
            if !well_formed {
                bail!(
                    "{repository:?} is not a repository as owner/name, such as example-org/busy; a bare name would drop every item that mentions the word"
                );
            }
            repositories.push(repository.to_owned());
        }
        self.adding_terms(repositories)
    }

    fn adding_terms(mut self, added: Vec<String>) -> Result<Self> {
        if added.is_empty() {
            return Ok(self);
        }
        for term in added {
            if !self
                .terms
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&term))
            {
                self.terms.push(term);
            }
        }
        let alternatives = self
            .terms
            .iter()
            .map(|term| {
                term.split_whitespace()
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(r"\s+")
            })
            .collect::<Vec<_>>()
            .join("|");
        self.term_net = Some(
            Regex::new(&format!(
                r"(?i)(?:^|[^a-z0-9_-])(?:{alternatives})(?:[^a-z0-9_-]|$)"
            ))
            .context("building the content net's terms")?,
        );
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

    /// Whether the net names no key, term or address, so nothing matches it.
    pub(super) fn is_empty(&self) -> bool {
        self.keys.is_none() && self.term_net.is_none() && self.addresses.is_empty()
    }

    /// Whether the net names a key or a term, which text can name.
    pub(super) fn has_text_net(&self) -> bool {
        self.keys.is_some() || self.term_net.is_some()
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

    /// The excluded key or term `text` names as it reads.
    fn hit_as_read(&self, text: &str) -> Option<Hit> {
        self.keys
            .as_ref()
            .and_then(|keys| keys.key_in(text))
            .map(|key| Hit::Key(key.to_owned()))
            .or_else(|| {
                self.term_net
                    .as_ref()
                    .is_some_and(|terms| terms.is_match(text))
                    .then_some(Hit::Term)
            })
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

/// Whether `c` cannot be part of an address in a header, so it separates two of them or an
/// address from its display name.
fn is_address_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | ':' | '"' | '(' | ')' | '[' | ']')
}

/// `text` as a reader of it would see it, when that differs from how it reads: folded
/// iCalendar lines unfolded, HTML tags taken out, entities and percent-escapes decoded, folded
/// to NFKC, every format character (`\p{Cf}`, soft hyphens and zero-width characters among them)
/// dropped, Unicode dashes read as `-`, and a line break after a hyphen joined. `None` when
/// `text` holds nothing any of those would change.
fn normalized(text: &str) -> Option<String> {
    if !text
        .bytes()
        .any(|byte| matches!(byte, b'<' | b'&' | b'%' | b'\n') || !byte.is_ascii())
    {
        return None;
    }
    let unfolded = unfold(text);
    let untagged = strip_tags(&unfolded);
    let decoded = decode_entities(&untagged);
    let unescaped = decode_percents(&decoded);
    let read = unescaped
        .nfkc()
        .filter(|c| get_general_category(*c) != GeneralCategory::Format)
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}' => '-',
            c => c,
        })
        .collect::<String>();
    Some(join_hyphenated_lines(read))
}

/// `text` with each line break that follows a hyphen taken out, with the spaces and tabs that
/// begin the next line, as a key wrapped at its hyphen (`SYNTH-` and `12`) reads joined.
fn join_hyphenated_lines(text: String) -> String {
    if !text.contains("-\n") && !text.contains("-\r\n") {
        return text;
    }
    let mut joined = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find('-') {
        joined.push_str(&rest[..=at]);
        let after = &rest[at + 1..];
        rest = match after
            .strip_prefix("\r\n")
            .or_else(|| after.strip_prefix('\n'))
        {
            Some(next) => next.trim_start_matches([' ', '\t']),
            None => after,
        };
    }
    joined.push_str(rest);
    joined
}

/// `text` with its percent-escapes (`%2D`) decoded, the bytes they name read as UTF-8, lossily.
/// A `%` that two hexadecimal digits do not follow is kept.
fn decode_percents(text: &str) -> Cow<'_, str> {
    if !text.contains('%') {
        return Cow::Borrowed(text);
    }
    let hex = |byte: Option<&u8>| byte.and_then(|byte| char::from(*byte).to_digit(16));
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            if let (Some(high), Some(low)) = (hex(bytes.get(index + 1)), hex(bytes.get(index + 2)))
            {
                decoded.extend(u8::try_from(high * 16 + low).ok());
                index += 3;
                continue;
            }
        }
        decoded.push(byte);
        index += 1;
    }
    Cow::Owned(String::from_utf8_lossy(&decoded).into_owned())
}

/// `text` with each folded line unfolded: a line break followed by one space or tab, as
/// iCalendar folds a long line (RFC 5545 §3.1), is taken out.
fn unfold(text: &str) -> Cow<'_, str> {
    if !text.contains("\n ") && !text.contains("\n\t") {
        return Cow::Borrowed(text);
    }
    let mut unfolded = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(c) = characters.next() {
        let breaks = match c {
            '\r' if characters.peek() == Some(&'\n') => {
                let mut ahead = characters.clone();
                ahead.next();
                matches!(ahead.peek(), Some(' ' | '\t')).then_some(2)
            }
            '\n' => matches!(characters.peek(), Some(' ' | '\t')).then_some(1),
            _ => None,
        };
        match breaks {
            Some(skip) => {
                for _ in 0..skip {
                    characters.next();
                }
            }
            None => unfolded.push(c),
        }
    }
    Cow::Owned(unfolded)
}

/// `text` with its HTML tags taken out. A `<` starts a tag only before a letter, `/`, `!` or
/// `?`, so `a < b` stays; a comment ends at `-->`, and a `>` inside a quoted attribute value does
/// not end its tag. A tag or comment that never ends is no tag: its `<` and the text after it
/// are kept.
fn strip_tags(text: &str) -> Cow<'_, str> {
    if !text.contains('<') {
        return Cow::Borrowed(text);
    }
    let mut untagged = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        untagged.push_str(&rest[..start]);
        let tag = &rest[start..];
        let opens = tag[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
        let end = if !opens {
            None
        } else if let Some(comment) = tag.strip_prefix("<!--") {
            comment
                .find("-->")
                .map(|end| "<!--".len() + end + "-->".len())
        } else {
            tag_end(tag)
        };
        if let Some(end) = end {
            rest = &tag[end..];
        } else {
            untagged.push('<');
            rest = &tag[1..];
        }
    }
    untagged.push_str(rest);
    Cow::Owned(untagged)
}

/// The byte just past the `>` that ends the tag `tag` starts with; a `>` inside an attribute
/// value quoted after its `=` does not count.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    let mut after_equals = false;
    for (index, c) in tag.char_indices().skip(1) {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None => match c {
                '>' => return Some(index + 1),
                '=' => after_equals = true,
                '"' | '\'' if after_equals => {
                    quote = Some(c);
                    after_equals = false;
                }
                c if c.is_whitespace() => {}
                _ => after_equals = false,
            },
        }
    }
    None
}

const NAMED_ENTITIES: [(&str, char); 17] = [
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", '\u{00A0}'),
    ("shy", '\u{00AD}'),
    ("hyphen", '\u{2010}'),
    ("dash", '\u{2010}'),
    ("ndash", '\u{2013}'),
    ("mdash", '\u{2014}'),
    ("horbar", '\u{2015}'),
    ("minus", '\u{2212}'),
    ("zwnj", '\u{200C}'),
    ("zwj", '\u{200D}'),
    ("ZeroWidthSpace", '\u{200B}'),
    ("NoBreak", '\u{2060}'),
];

/// `text` with its character references decoded: `&#45;`, `&#x2d;` (the `;` optional) and the
/// [`NAMED_ENTITIES`]. A `&` that starts none is kept, and reading goes on after it.
fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some((c, length)) = entity(after) {
            decoded.push(c);
            rest = &after[length..];
        } else {
            decoded.push('&');
            rest = after;
        }
    }
    decoded.push_str(rest);
    Cow::Owned(decoded)
}

/// The character the reference that `after` (the text after an `&`) starts with names, and how
/// many bytes of `after` it takes.
fn entity(after: &str) -> Option<(char, usize)> {
    if let Some(number) = after.strip_prefix('#') {
        let (digits, radix, prefix) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16, 2),
            None => (number, 10, 1),
        };
        let length = digits
            .bytes()
            .take_while(|byte| {
                if radix == 16 {
                    byte.is_ascii_hexdigit()
                } else {
                    byte.is_ascii_digit()
                }
            })
            .count();
        // Leading zeros name nothing, however many there are: `&#0000000045;` is `-`.
        let significant = digits[..length].trim_start_matches('0');
        if length == 0 || significant.len() > 8 {
            return None;
        }
        let code = if significant.is_empty() {
            0
        } else {
            u32::from_str_radix(significant, radix).ok()?
        };
        let c = char::from_u32(code)?;
        let semicolon = usize::from(digits[length..].starts_with(';'));
        return Some((c, prefix + length + semicolon));
    }
    let length = after.bytes().take_while(u8::is_ascii_alphanumeric).count();
    if !after[length..].starts_with(';') {
        return None;
    }
    let name = &after[..length];
    let (_, c) = NAMED_ENTITIES.iter().find(|(known, _)| *known == name)?;
    Some((*c, length + 1))
}

#[cfg(test)]
pub(super) mod tests {
    use std::cell::RefCell;

    use anyhow::Result;
    use serde_json::json;

    use super::{withheld, ContentNet, Hit, NetLists};

    thread_local! {
        /// The drop log lines this test thread wrote, beside standard error.
        pub(in crate::source) static LOGGED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// The drop log lines this test thread wrote since it last asked.
    pub(in crate::source) fn take_logged() -> Vec<String> {
        LOGGED.with(|logged| std::mem::take(&mut *logged.borrow_mut()))
    }

    fn net(keys: &[&str]) -> Result<ContentNet> {
        let keys = keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
        ContentNet::from_parts(&keys, None)
    }

    /// Each key matches as an issue, `<KEY>-<number>`, as a link, `dispatch://<KEY>`, and as a
    /// Dispatch web link, `/projects/<KEY>`, in any case: a key is bounded by anything but an
    /// ASCII letter or digit, so `_` and a letter outside ASCII separate it too. An issue of
    /// another key that starts or ends like it, a number that runs into a word, and the key
    /// alone do not match. Control: with no key, nothing does.
    #[test]
    fn a_key_matches_as_an_issue_or_a_link_and_nothing_else() -> Result<()> {
        let synth = net(&["SYNTH"])?;
        for text in [
            "see SYNTH-12.",
            "SYNTH-1",
            "(SYNTH-3040)",
            "branch INFRA-1-SYNTH-7-fix",
            "open dispatch://SYNTH/artifact/plan",
            "dispatch://SYNTH",
            "notes_SYNTH-12.pdf",
            "SYNTH-12_draft",
            "中SYNTH-12",
            "SYNTH-12é",
            "synth-12",
            "dispatch://synth/ask/1",
            "https://dispatch.example.com/projects/SYNTH/issues",
            "https://dispatch.example.com/projects/synth",
        ] {
            assert!(synth.matches(text), "{text}");
            assert!(!ContentNet::default().matches(text), "{text}");
        }
        for text in [
            "SYNTHB-12",
            "XSYNTH-12",
            "1SYNTH-12",
            "SYNTH-12a",
            "SYNTH-",
            "SYNTH",
            "dispatch://SYNTHB",
            "/projects/SYNTHB/issues",
            "projects/SYNTH",
            "the synth project",
        ] {
            assert!(!synth.matches(text), "{text}");
        }
        Ok(())
    }

    /// A key is matched in text as a reader of it would see it: HTML tags taken out (a comment,
    /// and a `>` inside a quoted attribute, do not end them early), entities decoded (a bare `&`
    /// that starts none is kept), Unicode dashes read as `-`, soft hyphens and zero-width
    /// characters dropped, and folded iCalendar lines unfolded. Control: the same text naming
    /// another key does not match.
    #[test]
    fn a_key_is_matched_through_markup_entities_dashes_and_folding() -> Result<()> {
        let synth = net(&["SYNTH"])?;
        for (text, other) in [
            ("SYNTH<b>-12</b>", "SYNTHB<b>-12</b>"),
            ("SYNTH&#45;12", "SYNTHB&#45;12"),
            ("SYNTH&#x2d;12", "SYNTHB&#x2d;12"),
            ("SYNTH&ndash;12", "SYNTHB&ndash;12"),
            (
                r#"<a href="x">SYNTH</a>-12"#,
                r#"<a href="x">SYNTHB</a>-12"#,
            ),
            (
                r#"<a title="a > b">SYNTH</a>-12"#,
                r#"<a title="a > b">SYNTHB</a>-12"#,
            ),
            ("SYNTH<!-- a > b -->-12", "SYNTHB<!-- a > b -->-12"),
            ("Q&A on SYNTH&#45;12", "Q&A on SYNTHB&#45;12"),
            ("SYNTH\u{2013}12", "SYNTHB\u{2013}12"),
            ("SYNTH\u{2011}12", "SYNTHB\u{2011}12"),
            ("SYN\u{00AD}TH-12", "SYN\u{00AD}THB-12"),
            ("SYNTH\u{200B}-12", "SYNTHB\u{200B}-12"),
            ("DESCRIPTION:SYN\r\n TH-12", "DESCRIPTION:SYN\r\n THB-12"),
            ("DESCRIPTION:SYNTH-\r\n\t12", "DESCRIPTION:SYNTHB-\r\n\t12"),
        ] {
            assert!(synth.matches(text), "{text:?}");
            assert!(!synth.matches(other), "{other:?}");
        }
        assert!(!synth.matches("a < b and SYNTHB > c, Q&A"));
        Ok(())
    }

    /// The reading is hardened past what markup alone hides: an unterminated `<` keeps the text
    /// after it; percent-escapes are decoded; every format character (`\p{Cf}`) is dropped; text
    /// is folded to NFKC, so fullwidth letters read as ASCII; a numeric entity may carry any
    /// number of leading zeros; and a line break after a hyphen inside a key is joined. Control:
    /// the same text naming another key does not match.
    #[test]
    fn a_key_is_matched_through_escapes_format_characters_and_compatibility_forms() -> Result<()> {
        let synth = net(&["SYNTH"])?;
        for (text, other) in [
            ("<b SYNTH&#45;12", "<b SYNTHB&#45;12"),
            ("SYNTH%2D12", "SYNTHB%2D12"),
            (
                "https://x.example/%53YNTH-12",
                "https://x.example/%53YNTHB-12",
            ),
            ("SYNTH\u{2066}-12\u{2069}", "SYNTHB\u{2066}-12\u{2069}"),
            ("SYNTH\u{200E}-12", "SYNTHB\u{200E}-12"),
            (
                "\u{FF33}\u{FF39}\u{FF2E}\u{FF34}\u{FF28}-12",
                "\u{FF33}\u{FF39}\u{FF2E}\u{FF34}\u{FF28}\u{FF22}-12",
            ),
            ("SYNTH&#0000000045;12", "SYNTHB&#0000000045;12"),
            ("SYNTH&#x000000002d;12", "SYNTHB&#x000000002d;12"),
            ("SYNTH-\n12", "SYNTHB-\n12"),
            ("SYNTH-\r\n12", "SYNTHB-\r\n12"),
        ] {
            assert!(synth.matches(text), "{text:?}");
            assert!(!synth.matches(other), "{other:?}");
        }
        Ok(())
    }

    /// A value names a key when any string it holds, at any depth, does; a record when its
    /// title, text, author, label, link or raw data does. The key named is the configured one.
    #[test]
    fn a_value_or_a_record_names_a_key_through_any_of_its_strings() -> Result<()> {
        let synth = net(&["SYNTH", "OTHER"])?;
        let key = |key: &str| Some(Hit::Key(key.to_owned()));
        assert_eq!(
            synth.hit_in_value(&json!({"a": [1, {"b": "fix synth-4"}]})),
            key("SYNTH")
        );
        assert_eq!(
            synth.hit_in_value(&json!({"a": [1, {"b": "fix INFRA-4"}], "SYNTH-4": 1})),
            key("SYNTH")
        );
        assert_eq!(
            synth.hit_in_value(&json!({"a": [1, {"b": "fix INFRA-4"}], "INFRA-4": 1})),
            None
        );
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
        assert_eq!(
            synth.hit_in_record(&record("OTHER-9 plan", json!({}))),
            Some(Hit::Key("OTHER".to_owned()))
        );
        assert!(synth
            .hit_in_record(&record("plan", json!({"description": "dispatch://SYNTH"})))
            .is_some());
        assert!(synth
            .hit_in_record(&record("plan", json!({"description": "INFRA-9"})))
            .is_none());
        Ok(())
    }

    /// An excluded address is named whole and in any case, alone or in a header beside a display
    /// name and other addresses; an address that only contains it, or shares its domain, is
    /// not. A malformed entry is an error. Control: with no address, nothing is named.
    #[test]
    fn an_address_is_named_whole_in_any_case() -> Result<()> {
        let addresses = ContentNet::default().with_addresses(
            &["Outside@Partner.example".to_owned()],
            Some(" ,b@x.example"),
        )?;
        for text in [
            "outside@partner.example",
            "\"Out Side\" <OUTSIDE@partner.example>, ada@theorem.example",
            "mailto:outside@partner.example",
            "B@X.example",
        ] {
            assert!(addresses.names_address(text), "{text}");
            assert!(!ContentNet::default().names_address(text), "{text}");
        }
        for text in [
            "someone@partner.example",
            "notoutside@partner.example",
            "outside@partner.example.org",
            "ada@theorem.example",
        ] {
            assert!(!addresses.names_address(text), "{text}");
        }
        for refused in [
            "outside",
            "Out <outside@partner.example>",
            "a@b@partner.example",
            "a@localhost",
            "@partner.example",
        ] {
            assert!(
                ContentNet::default()
                    .with_addresses(&[refused.to_owned()], None)
                    .is_err(),
                "{refused}"
            );
        }
        Ok(())
    }

    /// A drop is logged as one JSON line naming the reason, the key when a key withheld the item,
    /// and the pointer, in that order, and nothing else.
    #[test]
    fn a_drop_is_logged_by_reason_key_and_pointer_alone() {
        take_logged();
        withheld(&Hit::Key("SYNTH".to_owned()), "gmail:abc");
        withheld(&Hit::Address, "gcal:team@example.com/E1");
        assert_eq!(
            take_logged(),
            [
                r#"{"event":"withheld","reason":"private-content-net","key":"SYNTH","pointer":"gmail:abc"}"#,
                r#"{"event":"withheld","reason":"private-address","pointer":"gcal:team@example.com/E1"}"#,
            ]
        );
    }

    /// A term is matched whole, in any case, as a reader sees it: a repository as `owner/name`,
    /// so also inside `github.com/owner/name` and the paths after it, and a phrase across any
    /// run of whitespace or markup. A term is bounded by anything but an ASCII letter, digit, `_`
    /// or `-`, so `owner/name-proofs` and a longer word are other terms, and a bare repository
    /// name matches nothing ctx was not given. A drop by a term is logged without the term,
    /// which may be confidential. Control: a public repository and a public phrase are not
    /// matched, and with no term nothing is.
    #[test]
    fn a_term_is_matched_whole_in_any_case() -> Result<()> {
        let terms = ContentNet::from_lists(&NetLists {
            terms: &[" Project  Falcon ".to_owned()],
            repositories: &["theorem-labs/synth".to_owned()],
            ..NetLists::default()
        })?;
        for text in [
            "see theorem-labs/synth for the plan",
            "https://github.com/Theorem-Labs/Synth/pull/3",
            "git@github.com:theorem-labs/synth.git",
            "theorem-labs/<b>synth</b>",
            "the project falcon\nkickoff",
            "PROJECT FALCON",
            "<b>Project</b>&nbsp;Falcon",
        ] {
            assert_eq!(terms.hit_in(text), Some(Hit::Term), "{text:?}");
            assert_eq!(ContentNet::default().hit_in(text), None, "{text:?}");
        }
        for text in [
            "theorem-labs/synth-proofs",
            "theorem-labs/synthb",
            "xtheorem-labs/synth",
            "theorem-labs/public-repo",
            "the synth repository",
            "Project Falconer",
            "Quarterly plan",
        ] {
            assert_eq!(terms.hit_in(text), None, "{text:?}");
        }
        take_logged();
        withheld(&Hit::Term, "gmail:abc");
        assert_eq!(
            take_logged(),
            [r#"{"event":"withheld","reason":"private-term","pointer":"gmail:abc"}"#]
        );
        Ok(())
    }

    /// A repository entry is `owner/name`, and a bare name is refused: `rocq` alone would drop
    /// every item that mentions the word. A term shorter than three characters is refused too.
    /// `CTX_EXCLUDE_TERMS` and `CTX_EXCLUDE_REPOSITORIES` add to the configuration's lists.
    #[test]
    fn a_repository_is_owner_slash_name_and_a_term_is_three_characters_or_more() -> Result<()> {
        for refused in [
            "rocq",
            "theorem-labs/",
            "/synth",
            "a/b/c",
            "theorem labs/synth",
        ] {
            let lists = NetLists {
                repositories: &[refused.to_owned()],
                ..NetLists::default()
            };
            assert!(ContentNet::from_lists(&lists).is_err(), "{refused}");
        }
        for refused in ["", "ab", " x "] {
            let lists = NetLists {
                terms: &[refused.to_owned()],
                ..NetLists::default()
            };
            assert!(ContentNet::from_lists(&lists).is_err(), "{refused:?}");
        }
        let both = ContentNet::from_lists(&NetLists::default())?
            .with_terms(&[], Some(" Project Falcon ,, "))?
            .with_repositories(&[], Some("theorem-labs/synth"))?;
        assert_eq!(both.hit_in("Project Falcon"), Some(Hit::Term));
        assert_eq!(both.hit_in("theorem-labs/synth"), Some(Hit::Term));
        assert!(both.has_text_net());
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
