//! Stable JSON output envelopes for command results.

use std::borrow::Cow;
use std::io::Write;

use anyhow::Result;
use serde::Serialize;

pub const JSON_VERSION: u32 = 1;

/// Source text as a terminal may show it: every control character removed but tab and newline,
/// and carriage return, vertical tab, form feed and U+0085 (next line) shown as a space.
///
/// Window and record text comes from outside — a Slack message, an email, an issue anyone can
/// open — and a C0 control (`ESC`, `BEL`, carriage return, backspace) or a C1 control (the
/// single-character `CSI` and `OSC`, U+0080 to U+009F) in it would drive the reader's terminal:
/// rewrite what is on screen, set its title, or hide text. The human printers pass every piece
/// of source text through this; their own styling is added afterwards. JSON output keeps the
/// text whole, escaped ([`emit`]). The four controls that break a line are spaces rather than
/// nothing because the quote check reads them as spaces: `foo\u{85}bar` shown as `foobar`
/// would make a quote copied from the output one its source does not hold.
#[must_use]
pub fn printable(text: &str) -> Cow<'_, str> {
    let shown = |character: char| !character.is_control() || matches!(character, '\t' | '\n');
    if text.chars().all(shown) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .filter_map(|character| match character {
                '\r' | '\u{b}' | '\u{c}' | '\u{85}' => Some(' '),
                character if shown(character) => Some(character),
                _ => None,
            })
            .collect(),
    )
}

/// Where one command's output goes.
///
/// The command line passes the process streams; `ctx serve` passes buffers, so the same
/// command can answer a remote caller with the bytes a local run would have printed.
///
/// The human printers style their text for a terminal. A sink writes that styling only when
/// it was told its standard output is a terminal a person reads ([`Sink::coloured`]); every
/// other sink — a pipe, a file, `NO_COLOR`, and the buffers `ctx serve` returns to agents —
/// gets the same text with no escape code in it.
pub struct Sink<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    colour: bool,
}

impl<'a> Sink<'a> {
    /// A sink that writes plain text, as captured output always is.
    pub fn new(out: &'a mut dyn Write, err: &'a mut dyn Write) -> Self {
        Self {
            out,
            err,
            colour: false,
        }
    }

    /// Lets the human printers' styling through, for standard output that is a terminal.
    #[must_use]
    pub const fn coloured(mut self, colour: bool) -> Self {
        self.colour = colour;
        self
    }
}

/// Whether this process's standard output is a terminal a person reads with colour: it is a
/// terminal, and `NO_COLOR` is unset or empty (no-color.org).
#[must_use]
pub fn colour_wanted(stdout: &impl std::io::IsTerminal) -> bool {
    stdout.is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// A writer that drops the terminal escape sequences a styled printer writes.
///
/// A sequence is `ESC [`, any parameter bytes, and one final byte from `@` to `~`. A printer
/// may write a sequence in several pieces, so the writer remembers where it stopped.
struct Plain<'a> {
    out: &'a mut dyn Write,
    escape: Escape,
}

/// Where a [`Plain`] writer is in the bytes it is handed.
#[derive(Clone, Copy)]
enum Escape {
    /// Text, which goes through.
    Text,
    /// An `ESC`, which may start a sequence.
    Started,
    /// Inside a sequence, until its final byte.
    Sequence,
}

impl Write for Plain<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut text = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            match self.escape {
                Escape::Text if byte == 0x1b => {
                    self.out
                        .write_all(bytes.get(text..index).unwrap_or_default())?;
                    self.escape = Escape::Started;
                }
                Escape::Started if byte == b'[' => self.escape = Escape::Sequence,
                // An escape that starts no sequence is dropped, and the byte after it is text.
                Escape::Started => {
                    self.escape = Escape::Text;
                    text = index;
                }
                Escape::Sequence if (0x40..=0x7e).contains(&byte) => {
                    self.escape = Escape::Text;
                    text = index + 1;
                }
                // Text goes through, and a sequence is dropped until its final byte.
                Escape::Text | Escape::Sequence => {}
            }
        }
        if matches!(self.escape, Escape::Text) {
            self.out.write_all(bytes.get(text..).unwrap_or_default())?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[derive(Serialize)]
pub struct Envelope<'a, T: Serialize> {
    pub version: u32,
    pub command: &'a str,
    pub data: T,
}

#[derive(Serialize)]
pub struct ErrorEnvelope<'a> {
    pub version: u32,
    pub command: &'a str,
    pub error: ErrorBody,
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub message: String,
}

pub enum Outcome {
    Pass,
    StructuralDefects,
    SourceErrors,
    /// The exit code a remote command reported, which the client repeats exactly.
    Code(u8),
}

impl Outcome {
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::StructuralDefects | Self::SourceErrors => 1,
            Self::Code(code) => *code,
        }
    }
}

/// Emits a command result in either the stable JSON envelope or human-readable form.
///
/// # Errors
///
/// Returns an error when JSON serialization or output fails.
pub fn emit<T: Serialize>(
    sink: &mut Sink<'_>,
    json: bool,
    command: &str,
    data: &T,
    human: impl FnOnce(&T, &mut dyn Write) -> std::io::Result<()>,
) -> Result<()> {
    if json {
        let envelope = Envelope {
            version: JSON_VERSION,
            command,
            data,
        };
        write_json(&mut *sink.out, &envelope)?;
        writeln!(sink.out)?;
    } else if sink.colour {
        human(data, &mut *sink.out)?;
    } else {
        human(
            data,
            &mut Plain {
                out: &mut *sink.out,
                escape: Escape::Text,
            },
        )?;
    }
    Ok(())
}

/// Emits a command failure using the stable JSON error envelope.
///
/// # Errors
///
/// Returns an error when JSON serialization or output fails.
pub fn emit_error(out: &mut dyn Write, command: &str, message: String) -> Result<()> {
    let envelope = ErrorEnvelope {
        version: JSON_VERSION,
        command,
        error: ErrorBody { message },
    };
    write_json(&mut *out, &envelope)?;
    writeln!(out)?;
    Ok(())
}

/// Writes one value as compact JSON in which no string carries a raw control character.
///
/// `serde_json` escapes the C0 controls already, and writes `DEL` and the C1 controls
/// (U+007F to U+009F) as they are; a terminal can read a C1 control as `CSI` or `OSC`, so
/// [`Escaping`] writes those as `\u` escapes too. The decoded text is unchanged.
///
/// # Errors
///
/// Returns an error when the value cannot be serialized or written.
pub fn write_json(out: &mut dyn Write, value: &impl Serialize) -> serde_json::Result<()> {
    value.serialize(&mut serde_json::Serializer::with_formatter(out, Escaping))
}

/// The compact JSON formatter, with `DEL` and the C1 controls written as `\u` escapes.
struct Escaping;

impl serde_json::ser::Formatter for Escaping {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        let mut start = 0;
        for (index, character) in fragment.char_indices() {
            if ('\u{7f}'..='\u{9f}').contains(&character) {
                writer.write_all(fragment.get(start..index).unwrap_or_default().as_bytes())?;
                write!(writer, "\\u{:04x}", u32::from(character))?;
                start = index + character.len_utf8();
            }
        }
        writer.write_all(fragment.get(start..).unwrap_or_default().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::{printable, Envelope, ErrorBody, ErrorEnvelope, JSON_VERSION};

    #[derive(Serialize)]
    struct Payload {
        value: u8,
    }

    /// A line breaker becomes a space, so a quote copied from the output still matches its
    /// source; any other control is dropped; tab and newline stay.
    #[test]
    fn printable_spaces_line_breakers_and_drops_other_controls() {
        assert_eq!(printable("foo\u{85}bar"), "foo bar");
        assert_eq!(printable("a\rb\u{b}c\u{c}d"), "a b c d");
        assert_eq!(printable("x\u{1b}]0;title\u{7}y\u{9b}z"), "x]0;titleyz");
        assert_eq!(printable("kept\tand\nkept"), "kept\tand\nkept");
    }

    #[test]
    fn envelope_has_the_stable_version_command_and_data_shape() -> anyhow::Result<()> {
        let envelope = Envelope {
            version: JSON_VERSION,
            command: "test",
            data: Payload { value: 7 },
        };
        let value = serde_json::to_value(envelope)?;
        assert_eq!(value.get("version"), Some(&serde_json::json!(1)));
        assert_eq!(value.get("command"), Some(&serde_json::json!("test")));
        assert_eq!(
            value.get("data").and_then(|data| data.get("value")),
            Some(&serde_json::json!(7))
        );
        Ok(())
    }

    #[test]
    fn error_envelope_nests_the_message() -> anyhow::Result<()> {
        let envelope = ErrorEnvelope {
            version: JSON_VERSION,
            command: "test",
            error: ErrorBody {
                message: "failed".to_owned(),
            },
        };
        let value = serde_json::to_value(envelope)?;
        assert_eq!(
            value.get("error").and_then(|error| error.get("message")),
            Some(&serde_json::json!("failed"))
        );
        Ok(())
    }

    /// Writes one styled line through a sink, the escape sequences split across writes as a
    /// styled value writes them, and returns what reached standard output.
    fn styled_through(colour: bool) -> anyhow::Result<String> {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut sink = super::Sink::new(&mut out, &mut err).coloured(colour);
        super::emit(&mut sink, false, "test", &(), |(), out| {
            out.write_all(b"[\x1b")?;
            out.write_all(b"[1mI2\x1b[0")?;
            out.write_all(b"m] title \x1b[2mpath\x1b[0m\n")
        })?;
        Ok(String::from_utf8(out)?)
    }

    /// `ctx serve` returns a command's output to agents that parse it, and a pipe is read the
    /// same way: a sink that is not a terminal's writes the text and none of the styling.
    #[test]
    fn a_plain_sink_drops_every_escape_sequence() -> anyhow::Result<()> {
        assert_eq!(styled_through(false)?, "[I2] title path\n");
        assert_eq!(
            styled_through(true)?,
            "[\x1b[1mI2\x1b[0m] title \x1b[2mpath\x1b[0m\n"
        );
        Ok(())
    }
}
