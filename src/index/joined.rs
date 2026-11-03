//! Join variants of compound names.
//!
//! A name is written with a hyphen or a dot in one place and joined or spaced in another:
//! `Blendo-Matic`, `blendomatic`, `blendo matic`. The index's tokenizer splits a compound into
//! its parts, so a search by the joined form found nothing. The index keeps, for each compound a
//! tree holds, its joined form beside its parts; a query word that is such a joined form also
//! searches for those parts as a phrase, and a query's own compound, or two neighbouring query
//! words, also searches for its joined form. Every variant is one more alternative of the query,
//! so a record holding none of them ranks as it did. A joined form is looked up by its spelling
//! with diacritics folded, so `cafematic` meets `Café-Matic`; every variant is searched for as
//! the tree or the query spells it, which the tokenizer folds as it folds any word.

use std::collections::{HashMap, HashSet};

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization as _;

/// The characters that join the parts of a compound: a hyphen (also the Unicode hyphen and
/// non-breaking hyphen) or a dot.
const JOINERS: [char; 4] = ['-', '\u{2010}', '\u{2011}', '.'];

/// The joined form of each compound a tree holds, keyed by its folded spelling (`folded_key`),
/// with the phrases of its parts as the index tokenizes them: `cafematic` → `café matic`.
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
            let phrases = self.0.entry(folded_key(&joined)).or_default();
            let key = phrase_key(&phrase);
            if !phrases.iter().any(|kept| phrase_key(kept) == key) {
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
            let joined = compounds(word)
                .into_iter()
                .filter_map(|parts| joined_form(&parts))
                .chain(single_token(word));
            for joined in joined {
                for phrase in self.0.get(&folded_key(&joined)).into_iter().flatten() {
                    add(phrase.clone());
                }
                add(joined);
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

/// `text` as the hit count compares a joined name: by its folded spelling (`folded_key`), without
/// the characters that join a compound's parts, so `Café-Matic` reads `cafematic`.
pub(crate) fn joined_spelling(text: &str) -> String {
    folded_lowercase(text)
        .filter(|character| !JOINERS.contains(character))
        .collect()
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
/// holds a letter, since a number written with a dot or a hyphen (`1.5`, `2026-10-25`) is not the
/// number its digits run together into, and three characters or more, since a two-character
/// abbreviation joins into a common word (`U.S.`, `a.m.`, `I.T.`). It is searched for as it is:
/// the tokenizer folds what it folds in a query's words as in the words it stores.
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

/// The words `text` holds as the index's tokenizer reads them, so two literals it reads as the
/// same phrase compare equal (`Café-Matic` and `cafe matic`): lowercased, with the one diacritic
/// on an ASCII letter dropped as SQLite's unicode61 tokenizer drops it by default, and spaced. A
/// letter with two diacritics (`ǖ`), one on another base (`ǿ`, `ǣ`) and a letter of another
/// script (`ё`, a Hangul syllable) stay as they are, as the tokenizer keeps them.
fn phrase_key(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut folded = String::with_capacity(lowered.len());
    let mut marks = Vec::new();
    let mut decomposed = lowered.nfd().peekable();
    while let Some(base) = decomposed.next() {
        marks.clear();
        while let Some(mark) = decomposed.next_if(|next| is_combining_mark(*next)) {
            marks.push(mark);
        }
        folded.push(base);
        if !(base.is_ascii_alphabetic() && marks.len() == 1) {
            folded.extend(marks.iter());
        }
    }
    let recomposed: String = folded.nfc().collect();
    recomposed
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The spelling a joined name is looked up and compared by: lowercased, decomposed by Unicode
/// NFKD with every combining mark dropped, so `Café-Matic`'s joined form `cafématic` and the
/// query word `cafematic` meet. It is never searched for: the tokenizer folds only Latin
/// letters, so a folded word of another script (`елка` for `ёлка`, Hangul letters for a
/// syllable) would match nothing.
fn folded_key(text: &str) -> String {
    folded_lowercase(text).collect()
}

/// The characters of `text` lowercased, decomposed by Unicode NFKD, every combining mark dropped.
fn folded_lowercase(text: &str) -> impl Iterator<Item = char> + '_ {
    text.nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::{compounds, joinable, joined_spelling, phrase_key, Joins};

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
    fn a_joined_form_is_looked_up_with_diacritics_folded_and_searched_as_spelled() {
        let joins = joins(&["A Café-Matic grinder", "A Cafe-Matic stand", "Ёжик-Туман"]);
        // The tree's two spellings are one phrase to the tokenizer, so only one is searched for.
        assert_eq!(joins.variants(&["cafematic"]), ["café matic"]);
        assert_eq!(joins.variants(&["Cafématic"]), ["café matic"]);
        assert_eq!(Joins::default().variants(&["Crème-Line"]), ["crèmeline"]);
        // The tokenizer folds no letter outside Latin, so neither is a variant folded there.
        assert_eq!(joins.variants(&["ёжиктуман"]), ["ёжик туман"]);
        assert_eq!(Joins::default().variants(&["Ёжик-Туман"]), ["ёжиктуман"]);
        assert_eq!(Joins::default().variants(&["가나", "다라"]), ["가나다라"]);
        // The tokenizer drops one mark on an ASCII letter, and keeps it on `ø` (`ǿ`) and `æ` (`ǣ`).
        assert_eq!(phrase_key("Café-Ǿre ǣ"), "cafe ǿre ǣ");
    }

    #[test]
    fn joiners_are_dropped_and_nothing_else() {
        assert_eq!(
            joined_spelling("fizz-brook 1.5 a‐b c_d"),
            "fizzbrook 15 ab c_d"
        );
    }
}
