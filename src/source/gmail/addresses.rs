//! The addresses a mail header names, read as RFC 5322 reads a mailbox list (§3.4): display
//! names, quoted strings, comments, groups and folding, with RFC 2047's encoded words decoded.

use base64::Engine as _;

use super::super::content_net::decode_hex_escapes;

/// What an address header names.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct Mailboxes {
    /// Each mailbox's address, its local part unquoted and the comments and folding around and
    /// inside it taken out, lowercased.
    pub(super) addresses: Vec<String>,
    /// The header's display names, group names and comments, encoded words decoded, which the
    /// content net also scans for an address written there.
    pub(super) display: String,
}

#[derive(Debug)]
enum Token {
    Word(String),
    Quoted(String),
    Special(char),
}

/// The mailboxes `value`, the value of an address header, names.
pub(super) fn mailboxes(value: &str) -> Mailboxes {
    let mut found = Mailboxes::default();
    let mut comments = Vec::new();
    let tokens = lex(value, &mut comments);
    let mut segment: Vec<&Token> = Vec::new();
    let mut in_angle = false;
    for token in &tokens {
        match token {
            Token::Special('<') => {
                in_angle = true;
                segment.push(token);
            }
            Token::Special('>') => {
                in_angle = false;
                segment.push(token);
            }
            Token::Special(',' | ';') if !in_angle => {
                read_mailbox(&segment, &mut found);
                segment.clear();
            }
            // A group's name ends at its colon: what came before it is a display name.
            Token::Special(':') if !in_angle => {
                display(&segment, &mut found);
                segment.clear();
            }
            _ => segment.push(token),
        }
    }
    read_mailbox(&segment, &mut found);
    for comment in comments {
        push_display(&mut found, &comment);
    }
    found
}

/// Reads one mailbox, `name-addr` or `addr-spec`, into `found`.
fn read_mailbox(tokens: &[&Token], found: &mut Mailboxes) {
    let Some(open) = tokens
        .iter()
        .position(|token| matches!(token, Token::Special('<')))
    else {
        match addr_spec(tokens) {
            Some(address) => found.addresses.push(address),
            None => display(tokens, found),
        }
        return;
    };
    let (before, rest) = tokens.split_at(open);
    display(before, found);
    let inside = rest.get(1..).unwrap_or_default();
    let close = inside
        .iter()
        .position(|token| matches!(token, Token::Special('>')))
        .unwrap_or(inside.len());
    let (angle, after) = inside.split_at(close);
    // An obsolete route (`<@relay:name@domain>`) ends at its colon.
    let start = angle
        .iter()
        .rposition(|token| matches!(token, Token::Special(':')))
        .map_or(0, |colon| colon + 1);
    if let Some(address) = addr_spec(angle.get(start..).unwrap_or_default()) {
        found.addresses.push(address);
    }
    display(after, found);
}

/// The address the tokens of an `addr-spec` write, `local@domain` lowercased, when they write
/// one.
fn addr_spec(tokens: &[&Token]) -> Option<String> {
    let mut address = String::new();
    for token in tokens {
        match token {
            Token::Word(word) => address.push_str(word),
            Token::Quoted(text) => address.push_str(text),
            Token::Special('@') => address.push('@'),
            Token::Special(_) => return None,
        }
    }
    let (local, domain) = address.split_once('@')?;
    (!local.is_empty() && !domain.is_empty() && !domain.contains('@'))
        .then(|| address.to_ascii_lowercase())
}

fn display(tokens: &[&Token], found: &mut Mailboxes) {
    for token in tokens {
        match token {
            Token::Word(text) | Token::Quoted(text) => push_display(found, text),
            Token::Special(c) => found.display.push(*c),
        }
    }
}

fn push_display(found: &mut Mailboxes, text: &str) {
    found.display.push(' ');
    found.display.push_str(&decode_encoded_words(text));
}

/// The tokens of `value`: words (runs of anything but space, specials, quotes and comments,
/// dots among them), quoted strings unquoted, and the specials `< > , ; : @`. Folding and other
/// space separate tokens and are dropped; each comment, nested ones read whole, is dropped from
/// the tokens and added to `comments`. A domain literal (`[…]`) is one word.
fn lex(value: &str, comments: &mut Vec<String>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut characters = value.chars();
    let end_word = |word: &mut String, tokens: &mut Vec<Token>| {
        if !word.is_empty() {
            tokens.push(Token::Word(std::mem::take(word)));
        }
    };
    while let Some(c) = characters.next() {
        match c {
            '"' => {
                end_word(&mut word, &mut tokens);
                let mut quoted = String::new();
                while let Some(c) = characters.next() {
                    match c {
                        '"' => break,
                        '\\' => quoted.extend(characters.next()),
                        '\r' | '\n' => {}
                        c => quoted.push(c),
                    }
                }
                tokens.push(Token::Quoted(quoted));
            }
            '(' => {
                end_word(&mut word, &mut tokens);
                let mut comment = String::new();
                let mut depth = 1_u32;
                while let Some(c) = characters.next() {
                    match c {
                        '\\' => comment.extend(characters.next()),
                        '(' => {
                            depth += 1;
                            comment.push(c);
                        }
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            comment.push(c);
                        }
                        c => comment.push(c),
                    }
                }
                comments.push(comment);
            }
            '[' => {
                end_word(&mut word, &mut tokens);
                let mut literal = String::from('[');
                for c in characters.by_ref() {
                    if !c.is_whitespace() {
                        literal.push(c);
                    }
                    if c == ']' {
                        break;
                    }
                }
                tokens.push(Token::Word(literal));
            }
            '<' | '>' | ',' | ';' | ':' | '@' => {
                end_word(&mut word, &mut tokens);
                tokens.push(Token::Special(c));
            }
            c if c.is_whitespace() => end_word(&mut word, &mut tokens),
            c => word.push(c),
        }
    }
    end_word(&mut word, &mut tokens);
    tokens
}

/// `text` with each RFC 2047 encoded word (`=?charset?B?…?=` or `=?charset?Q?…?=`) decoded with
/// the character set it names, or as UTF-8, lossily. A word that does not decode is kept.
fn decode_encoded_words(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("=?") {
        decoded.push_str(&rest[..start]);
        let word = &rest[start + 2..];
        if let Some((text, length)) = encoded_word(word) {
            decoded.push_str(&text);
            rest = word.get(length..).unwrap_or_default();
        } else {
            decoded.push_str("=?");
            rest = word;
        }
    }
    decoded.push_str(rest);
    decoded
}

/// The text the encoded word `word` (what follows its `=?`) holds, and how many bytes of `word`
/// it takes, up to and with its closing `?=`.
fn encoded_word(word: &str) -> Option<(String, usize)> {
    let (charset, rest) = word.split_once('?')?;
    let (encoding, rest) = rest.split_once('?')?;
    let (payload, _) = rest.split_once("?=")?;
    let length = charset.len() + 1 + encoding.len() + 1 + payload.len() + 2;
    let bytes = match encoding {
        "B" | "b" => base64::engine::general_purpose::STANDARD
            .decode(payload.trim_end_matches('='))
            .or_else(|_| {
                base64::engine::general_purpose::STANDARD_NO_PAD
                    .decode(payload.trim_end_matches('='))
            })
            .ok()?,
        "Q" | "q" => quoted_printable(payload),
        _ => return None,
    };
    // RFC 2231 lets a character set carry a language after a `*`.
    let label = charset.split('*').next().unwrap_or(charset);
    let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    Some((encoding.decode(&bytes).0.into_owned(), length))
}

/// The bytes a `Q` encoded word's payload writes: `_` a space, `=XX` a byte.
fn quoted_printable(payload: &str) -> Vec<u8> {
    let spaced = payload
        .bytes()
        .map(|byte| if byte == b'_' { b' ' } else { byte })
        .collect::<Vec<_>>();
    decode_hex_escapes(&spaced, b'=')
}

#[cfg(test)]
mod tests {
    use super::mailboxes;

    /// Each mailbox's address is read past display names, quoted strings, comments, groups,
    /// folding and routes; display names and comments are kept, encoded words decoded, for the
    /// net to scan.
    #[test]
    fn a_mailbox_list_is_read_as_rfc_5322_reads_it() {
        for (header, addresses) in [
            ("Ada <Ada@Theorem.example>", vec!["ada@theorem.example"]),
            (
                "\"Side, Out\" <out@partner.example>, b@c.example",
                vec!["out@partner.example", "b@c.example"],
            ),
            ("\"out\"@partner.example", vec!["out@partner.example"]),
            ("\"o\\\"ut\"@partner.example", vec!["o\"ut@partner.example"]),
            (
                "out(comment (nested))@partner.example",
                vec!["out@partner.example"],
            ),
            ("out @\r\n partner.example", vec!["out@partner.example"]),
            (
                "Team: a@b.example, c@d.example;, e@f.example",
                vec!["a@b.example", "c@d.example", "e@f.example"],
            ),
            (
                "<@relay.example:out@partner.example>",
                vec!["out@partner.example"],
            ),
            ("out@[192.0.2.1]", vec!["out@[192.0.2.1]"]),
            ("undisclosed-recipients:;", vec![]),
        ] {
            assert_eq!(mailboxes(header).addresses, addresses, "{header}");
        }
        let read = mailboxes("=?UTF-8?B?b3V0QHBhcnRuZXIuZXhhbXBsZQ==?= (via =?utf-8?Q?x=40y.example?=) <l@lists.example>");
        assert_eq!(read.addresses, ["l@lists.example"]);
        assert!(
            read.display.contains("out@partner.example"),
            "{}",
            read.display
        );
        assert!(read.display.contains("x@y.example"), "{}", read.display);
    }
}
