//! Which rooms a source reads.

use anyhow::Result;

use super::super::rooms::{Decision, Facts, Selection};

fn named(name: &str) -> Facts {
    Facts {
        name: Some(name.to_owned()),
        alias: None,
    }
}

fn selection(rooms: &[&str], exclude: &[&str]) -> Result<Selection> {
    let owned = |list: &[&str]| {
        list.iter()
            .map(|entry| (*entry).to_owned())
            .collect::<Vec<_>>()
    };
    Selection::new(&owned(rooms), &owned(exclude))
}

#[test]
fn every_joined_room_is_read_when_nothing_is_configured() -> Result<()> {
    let selection = selection(&[], &[])?;
    assert_eq!(selection.decide("!a:example.org", None), Decision::Read);
    Ok(())
}

#[test]
fn an_exclusion_names_a_room_by_id_alias_or_name_pattern() -> Result<()> {
    let selection = selection(
        &[],
        &[
            "!bank:example.org",
            "#codes:example.org",
            "*verif*",
            "Delivery Bot",
        ],
    )?;
    assert_eq!(selection.decide("!bank:example.org", None), Decision::Skip);
    let aliased = Facts {
        name: None,
        alias: Some("#Codes:example.org".to_owned()),
    };
    assert_eq!(
        selection.decide("!c:example.org", Some(&aliased)),
        Decision::Skip
    );
    assert_eq!(
        selection.decide("!v:example.org", Some(&named("Login Verification"))),
        Decision::Skip
    );
    assert_eq!(
        selection.decide("!d:example.org", Some(&named("delivery bot"))),
        Decision::Skip
    );
    assert_eq!(
        selection.decide("!f:example.org", Some(&named("Family"))),
        Decision::Read
    );
    // A room whose name is not known yet is planned and decided once its name is read.
    assert_eq!(selection.decide("!f:example.org", None), Decision::Unknown);
    Ok(())
}

#[test]
fn an_allowlist_reads_only_the_rooms_it_names_and_an_exclusion_wins() -> Result<()> {
    let mixed = selection(&["!a:example.org", "Book club"], &["!a:example.org"])?;
    assert_eq!(mixed.decide("!a:example.org", None), Decision::Skip);
    assert_eq!(
        mixed.decide("!b:example.org", Some(&named("Book Club"))),
        Decision::Read
    );
    assert_eq!(
        mixed.decide("!c:example.org", Some(&named("Work"))),
        Decision::Skip
    );
    assert_eq!(mixed.decide("!c:example.org", None), Decision::Unknown);
    let ids = selection(&["!a:example.org"], &[])?;
    assert_eq!(ids.decide("!a:example.org", None), Decision::Read);
    assert_eq!(ids.decide("!b:example.org", None), Decision::Skip);
    assert_eq!(ids.allowed_ids().collect::<Vec<_>>(), ["!a:example.org"]);
    Ok(())
}

#[test]
fn an_empty_entry_is_refused() {
    assert!(selection(&[""], &[]).is_err());
    assert!(selection(&[], &["  "]).is_err());
}
