//! Which rooms a source reads: the `rooms` allowlist and the `exclude_rooms` list, each entry a
//! room id, a room alias, or a pattern of room names.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::exclude::glob;

/// What the source knows of a room besides its id: its name (`m.room.name`) and its canonical
/// alias (`m.room.canonical_alias`), either of which it may lack.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Facts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) alias: Option<String>,
}

impl Facts {
    /// What a record calls the room: its name, else its canonical alias, else its id.
    pub(super) fn label<'a>(&'a self, room: &'a str) -> &'a str {
        self.name
            .as_deref()
            .or(self.alias.as_deref())
            .unwrap_or(room)
    }
}

/// One entry of `rooms` or `exclude_rooms`.
#[derive(Debug)]
enum Entry {
    /// `!…`: the room with this id.
    Id(String),
    /// `#…`: the room whose canonical alias this is, in any case.
    Alias(String),
    /// Anything else: the rooms whose name this pattern matches, in any case, `*` standing for
    /// any run of characters.
    Name(String),
}

impl Entry {
    fn parse(entry: &str) -> Result<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            bail!("a matrix rooms or exclude_rooms entry is empty");
        }
        Ok(if entry.starts_with('!') {
            Self::Id(entry.to_owned())
        } else if entry.starts_with('#') {
            Self::Alias(entry.to_lowercase())
        } else {
            Self::Name(entry.to_lowercase())
        })
    }

    /// Whether the entry names `room`: `Some` when that is known, `None` when it depends on a
    /// name or an alias the source has not learned.
    fn names(&self, room: &str, facts: Option<&Facts>) -> Option<bool> {
        match self {
            Self::Id(id) => Some(id == room),
            Self::Alias(alias) => facts.map(|facts| {
                facts
                    .alias
                    .as_deref()
                    .is_some_and(|known| known.to_lowercase() == *alias)
            }),
            Self::Name(pattern) => facts.map(|facts| {
                facts
                    .name
                    .as_deref()
                    .is_some_and(|name| glob(pattern.as_bytes(), name.to_lowercase().as_bytes()))
            }),
        }
    }
}

/// Whether a run reads a room.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Decision {
    Read,
    Skip,
    /// It depends on the room's name or alias, which the source has not learned yet: the run
    /// plans the room and decides once it has read them, before it reads any message.
    Unknown,
}

/// The configured choice of rooms.
#[derive(Debug, Default)]
pub(super) struct Selection {
    allow: Vec<Entry>,
    exclude: Vec<Entry>,
}

impl Selection {
    /// The selection `rooms` and `exclude_rooms` configure.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is empty.
    pub(super) fn new(rooms: &[String], exclude_rooms: &[String]) -> Result<Self> {
        Ok(Self {
            allow: rooms
                .iter()
                .map(|entry| Entry::parse(entry))
                .collect::<Result<_>>()?,
            exclude: exclude_rooms
                .iter()
                .map(|entry| Entry::parse(entry))
                .collect::<Result<_>>()?,
        })
    }

    /// Whether a run reads `room`, given what it knows of the room. An exclusion wins over the
    /// allowlist; an empty allowlist allows every room.
    pub(super) fn decide(&self, room: &str, facts: Option<&Facts>) -> Decision {
        let mut exclusion_unknown = false;
        for entry in &self.exclude {
            match entry.names(room, facts) {
                Some(true) => return Decision::Skip,
                Some(false) => {}
                None => exclusion_unknown = true,
            }
        }
        let allowed = if self.allow.is_empty() {
            Some(true)
        } else {
            let answers: Vec<Option<bool>> = self
                .allow
                .iter()
                .map(|entry| entry.names(room, facts))
                .collect();
            if answers.contains(&Some(true)) {
                Some(true)
            } else if answers.contains(&None) {
                None
            } else {
                Some(false)
            }
        };
        match allowed {
            Some(false) => Decision::Skip,
            None => Decision::Unknown,
            Some(true) if exclusion_unknown => Decision::Unknown,
            Some(true) => Decision::Read,
        }
    }

    /// The room ids the allowlist names, which a run warns about when the account has not
    /// joined them.
    pub(super) fn allowed_ids(&self) -> impl Iterator<Item = &str> {
        self.allow.iter().filter_map(|entry| match entry {
            Entry::Id(id) => Some(id.as_str()),
            Entry::Alias(_) | Entry::Name(_) => None,
        })
    }

    /// Whether an entry names a room by its alias, so a run reads each room's alias.
    pub(super) fn names_aliases(&self) -> bool {
        self.allow
            .iter()
            .chain(&self.exclude)
            .any(|entry| matches!(entry, Entry::Alias(_)))
    }
}
