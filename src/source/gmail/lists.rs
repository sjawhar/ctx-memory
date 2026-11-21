//! Mailing lists whose mail the Gmail source never stores, by the list's identifier.
//!
//! A mailing list's messages carry its identifier in their `List-Id` header (RFC 2919), such
//! as GitHub's notification mail about one repository, `List-Id: example-org/busy
//! <busy.example-org.github.com>`. Nothing in the message's pointer, `gmail:<message id>`, names
//! the list, so no pointer pattern can exclude it: the source reads the header itself.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use serde_json::Value;

/// The environment variable that adds List-IDs, one per comma, to the source's
/// `exclude_list_ids`.
pub(super) const EXCLUDE_LIST_IDS_ENV: &str = "CTX_GMAIL_EXCLUDE_LIST_IDS";

/// The List-IDs whose mail is never stored, lowercased: a List-ID is a domain-like name, so
/// `Busy.Example-Org.github.com` and `busy.example-org.github.com` are one list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ExcludedLists(BTreeSet<String>);

impl ExcludedLists {
    /// The configuration's List-IDs and those `environment`, the value of
    /// [`EXCLUDE_LIST_IDS_ENV`], names, separated by commas. Both lists apply, so neither can take
    /// a list off the other. An empty entry between two commas of the environment is skipped.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is not a List-ID of RFC 2919's form: at most 255 octets of
    /// two or more labels joined by dots, each of one or more of the characters a dot-atom allows
    /// (letters, digits and ``!#$%&'*+-/=?^_`{|}~``), such as `busy.example-org.github.com`. An
    /// empty entry in the configuration, a whole header value, `busy..example-org.github.com` or
    /// `busy.example-org.github.com;` would match no message, so the mail it was meant to exclude
    /// would be stored.
    pub(super) fn from_parts(configured: &[String], environment: Option<&str>) -> Result<Self> {
        let listed = environment
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let mut ids = BTreeSet::new();
        for id in configured.iter().map(|id| id.trim()).chain(listed) {
            if !is_list_id(id) {
                bail!(
                    "{id:?} is not a List-ID: name the identifier between the angle brackets of the header alone, labels joined by dots, such as busy.example-org.github.com"
                );
            }
            ids.insert(id.to_ascii_lowercase());
        }
        Ok(Self(ids))
    }

    /// Whether `message`, a Gmail API message in `full` format, came from a list this names: one
    /// of its `List-Id` headers, the name read whatever its case, carries one of the identifiers.
    pub(super) fn names(&self, message: &Value) -> bool {
        if self.0.is_empty() {
            return false;
        }
        message
            .pointer("/payload/headers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|header| {
                header
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case("list-id"))
            })
            .filter_map(|header| header.get("value").and_then(Value::as_str))
            .any(|value| {
                let mut id = list_id(value);
                id.make_ascii_lowercase();
                self.0.contains(&id)
            })
    }
}

/// The identifier a `List-Id` header value carries: what its last `<…>` holds, after the
/// optional description (`example-org/busy <busy.example-org.github.com>`), or the whole value
/// when it has no angle brackets, with every whitespace character taken out. RFC 2919 §3 has a
/// reader ignore whitespace inside the brackets, which a header folded across lines, or a
/// transport that breaks one, puts there (`<busy.\r\n\texample-org.github.com>`).
fn list_id(value: &str) -> String {
    value
        .rsplit_once('<')
        .map_or(value, |(_, bracketed)| {
            bracketed.split_once('>').map_or(bracketed, |(id, _)| id)
        })
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Whether `id` is a List-ID of RFC 2919's form: at most 255 octets (§2) of
/// `list-label "." list-id-namespace`, two or more dot-atom labels, each one or more `atext`
/// characters (RFC 5322 §3.2.3).
fn is_list_id(id: &str) -> bool {
    let atext = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c);
    id.len() <= 255
        && id.contains('.')
        && id
            .split('.')
            .all(|label| !label.is_empty() && label.chars().all(atext))
}
