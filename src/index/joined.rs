//! Join variants of compound names.
//!
//! A name is written with a hyphen or a dot in one place and joined or spaced in another:
//! `Blendo-Matic`, `blendomatic`, `blendo matic`. The index's tokenizer splits a compound into
//! its parts, so a search by the joined form found nothing. The index keeps, for each compound a
//! tree holds, its joined form beside its parts; a query word that is such a joined form also
//! searches for those parts as a phrase, and a query's own compound, or two neighbouring query
//! words, also searches for its joined form. Every variant is one more alternative of the query,
//! so a record holding none of them ranks as it did.

use std::collections::{HashMap, HashSet};

/// The characters that join the parts of a compound: a hyphen (also the Unicode hyphen and
/// non-breaking hyphen) or a dot.
const JOINERS: [char; 4] = ['-', '\u{2010}', '\u{2011}', '.'];

/// The joined form of each compound a tree holds, with the phrases of its parts as the index
/// tokenizes them: `blendomatic` → `blendo matic`.
#[derive(Default)]
pub(super) struct Joins(HashMap<String, Vec<String>>);

impl Joins {
    /// Keeps every compound `text` holds.
    pub(super) fn add(&mut self, text: &str) {
        for parts in compounds(text) {
            let Some(joined) = joined_form(&parts) else {
                continue;
            };
            let phrase = parts.join(" ").to_lowercase();
            let phrases = self.0.entry(joined).or_default();
            if !phrases.contains(&phrase) {
                phrases.push(phrase);
            }
        }
    }

    /// What the query words `words` also search for, each one FTS5 string literal's text, none a
    /// repeat of a word or of another variant (an alternative counted twice would count its
    /// matches twice): for a word that is the joined form of a compound the tree holds, that
    /// compound's parts as a phrase; for a word that is itself a compound, its joined form and
    /// the other compounds of that joined form; for two neighbouring plain words, their joined
    /// form.
    pub(super) fn variants(&self, words: &[&str]) -> Vec<String> {
        let mut seen: HashSet<String> = words.iter().map(|word| phrase_key(word)).collect();
        let mut found = Vec::new();
        let mut add = |variant: String| {
            if seen.insert(phrase_key(&variant)) {
                found.push(variant);
            }
        };
        for word in words {
            let keys = compounds(word)
                .into_iter()
                .filter_map(|parts| joined_form(&parts))
                .chain(single_token(word));
            for key in keys {
                for phrase in self.0.get(&key).into_iter().flatten() {
                    add(phrase.clone());
                }
                add(key);
            }
        }
        for (first, second) in words.iter().zip(words.iter().skip(1)) {
            let (Some(first), Some(second)) = (single_token(first), single_token(second)) else {
                continue;
            };
            if let Some(joined) = joined_form(&[&first, &second]) {
                add(joined);
            }
        }
        found
    }
}

/// `text` without the characters that join a compound's parts, so `blendo-matic` reads
/// `blendomatic`.
pub(crate) fn without_joiners(text: &str) -> String {
    text.replace(JOINERS.as_slice(), "")
}

/// Whether the index joins `term`, by the rule of `joined_form`: it joins `Blendo-Matic` and
/// `blendomatic`, and not `1.5` or `U.S.`, so neither of those is found by `15` or `us`.
pub(crate) fn joinable(term: &str) -> bool {
    let parts: Vec<&str> = term
        .split(|character: char| !character.is_alphanumeric())
        .collect();
    joined_form(&parts).is_some()
}

/// Every compound in `text`, as its parts: each run of two or more alphanumeric parts with one
/// joiner between each part and the next.
fn compounds(text: &str) -> Vec<Vec<&str>> {
    // A compound is spelled with alphanumeric characters and joiners alone, so any other
    // character ends the span that may hold one.
    let outside = |character: char| !character.is_alphanumeric() && !JOINERS.contains(&character);
    let mut found = Vec::new();
    let mut parts: Vec<&str> = Vec::new();
    for span in text.split(outside) {
        parts.clear();
        parts.extend(span.split(JOINERS.as_slice()));
        // Two joiners in a row, or one at either end, leave an empty part, which ends a compound.
        found.extend(
            parts
                .split(|part| part.is_empty())
                .filter(|run| run.len() > 1)
                .map(<[&str]>::to_vec),
        );
    }
    found
}

/// The joined form of a compound's parts, lowercased, when it can stand for a name: one that
/// holds a letter, since a number written with a dot or a hyphen (`1.5`, `2026-10-25`) is not
/// the number its digits run together into, and three characters or more, since a two-character
/// abbreviation joins into a common word (`U.S.`, `a.m.`, `I.T.`).
fn joined_form(parts: &[&str]) -> Option<String> {
    let joined = parts.concat().to_lowercase();
    (joined.chars().count() >= 3 && joined.chars().any(char::is_alphabetic)).then_some(joined)
}

/// A query word that is one plain word once the punctuation around it is trimmed (`Blendomatic,`
/// is `blendomatic`), lowercased.
fn single_token(word: &str) -> Option<String> {
    let trimmed = word.trim_matches(|character: char| !character.is_alphanumeric());
    (!trimmed.is_empty() && trimmed.chars().all(char::is_alphanumeric))
        .then(|| trimmed.to_lowercase())
}

/// The words `text` holds as the tokenizer reads them, lowercased and spaced, so two literals
/// that search for the same phrase compare equal: `Blendo-Matic` and `blendo matic`.
fn phrase_key(text: &str) -> String {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{compounds, joinable, without_joiners, Joins};

    fn joins(texts: &[&str]) -> Joins {
        let mut joins = Joins::default();
        for text in texts {
            joins.add(text);
        }
        joins
    }

    #[test]
    fn a_compound_is_parts_joined_by_one_hyphen_or_dot() {
        assert_eq!(
            compounds("A Blendo-Matic from Brewly.Home, not -- this - one. Ab‑Cd-Ef."),
            vec![
                vec!["Blendo", "Matic"],
                vec!["Brewly", "Home"],
                vec!["Ab", "Cd", "Ef"]
            ]
        );
    }

    #[test]
    fn a_joined_word_also_searches_for_the_compound_the_tree_holds() {
        let joins = joins(&["Bought a Blendo-Matic 500."]);
        assert_eq!(joins.variants(&["Blendomatic"]), ["blendo matic"]);
        assert_eq!(joins.variants(&["blendomatic,"]), ["blendo matic"]);
    }

    #[test]
    fn a_compound_word_also_searches_for_its_joined_form() {
        assert_eq!(Joins::default().variants(&["Fizz-Brook"]), ["fizzbrook"]);
        let joins = joins(&["The Fizz.Brook kit"]);
        assert_eq!(joins.variants(&["Fizz-Brook"]), ["fizzbrook"]);
    }

    #[test]
    fn two_neighbouring_words_also_search_for_their_joined_form() {
        assert_eq!(
            Joins::default().variants(&["blendo", "matic", "blender"]),
            ["blendomatic", "maticblender"]
        );
    }

    #[test]
    fn numbers_and_two_letter_abbreviations_are_not_joined() {
        let none: [&str; 0] = [];
        let joins = joins(&["Mix 1.5 parts; ship to the U.S. by 9 a.m. on 2026-10-25."]);
        assert!(joins.0.is_empty(), "{:?}", joins.0);
        assert_eq!(Joins::default().variants(&["1.5"]), none);
        assert_eq!(Joins::default().variants(&["U.S."]), none);
        assert_eq!(Joins::default().variants(&["10", "25"]), none);
        assert!(!joinable("1.5") && !joinable("U.S.") && !joinable("2026-10-25"));
        assert!(joinable("Blendo-Matic") && joinable("blendomatic,"));
    }

    #[test]
    fn no_variant_repeats_a_word_or_another_variant() {
        let none: [&str; 0] = [];
        let joins = joins(&["Blendo-Matic", "blendo.matic"]);
        assert_eq!(joins.variants(&["Blendo-Matic", "blendomatic"]), none);
        assert_eq!(joins.variants(&["Blendomatic"]), ["blendo matic"]);
    }

    #[test]
    fn joiners_are_dropped_and_nothing_else() {
        assert_eq!(
            without_joiners("fizz-brook 1.5 a‐b c_d"),
            "fizzbrook 15 ab c_d"
        );
    }
}
