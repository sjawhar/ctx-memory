//! How the content net reads text as a reader of it would see it.

use std::borrow::Cow;

use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization as _;

/// `text` as a reader of it would see it, when that differs from how it reads: folded
/// iCalendar lines unfolded, HTML tags taken out, entities and percent-escapes decoded, folded
/// to NFKC, every format character (`\p{Cf}`, soft hyphens and zero-width characters among them)
/// dropped, Unicode dashes read as `-`, and a line break after a hyphen joined. `None` when
/// `text` holds nothing any of those would change.
pub(super) fn normalized(text: &str) -> Option<String> {
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
    Cow::Owned(String::from_utf8_lossy(&decode_hex_escapes(text.as_bytes(), b'%')).into_owned())
}

/// `bytes` with each escape, `escape` followed by two hexadecimal digits (`%2D`, or `=2D` in
/// quoted-printable), replaced by the byte it names. An `escape` that two hexadecimal digits do
/// not follow is kept.
pub(in crate::source) fn decode_hex_escapes(bytes: &[u8], escape: u8) -> Vec<u8> {
    let hex = |byte: Option<&u8>| byte.and_then(|byte| char::from(*byte).to_digit(16));
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == escape {
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
    decoded
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

/// The tags a reader sees as a line break: HTML's block elements (a paragraph, a section, a
/// list or list item, a table or its cells, a definition, a figure, a form), and a break. Any
/// other tag, such as `b` or `a`, joins the text on either side of it.
const BREAKING_TAGS: [&str; 38] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "caption",
    "center",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "td",
    "th",
    "tr",
    "ul",
];

/// `text` with its HTML tags taken out: each of the [`BREAKING_TAGS`], opening or closing, read
/// as a line break, so `Notes<br>SYNTH-12` names `SYNTH`, and any other tag as nothing, so
/// `SYNTH<b>-12</b>` does too. A `<` starts a tag only before a letter, `/`, `!` or `?`, so
/// `a < b` stays; a comment ends at `-->`, and a `>` inside a quoted attribute value does not
/// end its tag. A tag or comment that never ends is no tag: its `<` and the text after it are
/// kept.
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
            if breaks_a_line(tag) {
                untagged.push('\n');
            }
            rest = &tag[end..];
        } else {
            untagged.push('<');
            rest = &tag[1..];
        }
    }
    untagged.push_str(rest);
    Cow::Owned(untagged)
}

fn breaks_a_line(tag: &str) -> bool {
    let name = tag
        .trim_start_matches('<')
        .trim_start_matches('/')
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    BREAKING_TAGS
        .iter()
        .any(|breaking| breaking.eq_ignore_ascii_case(name))
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
