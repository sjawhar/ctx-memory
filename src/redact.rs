//! What a credential looks like, and the one-line form in which a log line quotes text from
//! outside.
//!
//! The server refuses a write that carries a credential, and every line the server, the fetch
//! job or a lease writes to standard error passes outside text — git's error, a caller's path,
//! Cloud Storage's answer — through [`one_line`]: on Cloud Run each line of standard error is
//! its own log entry, and README promises an alert three fixed forms, each at the start of a
//! line.

use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use regex::Regex;

/// How many characters of outside text a log line carries.
const LINE_LIMIT: usize = 300;

/// The credentials a record may never carry and a log line never shows: what each is called,
/// and the shape it has.
///
/// The agent cycle's session can read its own model key, and an instruction it obeys could
/// put that key, or any credential it can reach, into a record. That key may be a model
/// gateway's key rather than Anthropic's: `sk-` and lower-case letters and digits, 64 of them
/// from the gateway this shape was written for, so the pattern asks for 32 or more. Each shape
/// asks for the prefix and enough of a body after it that prose naming a prefix is not refused.
const CREDENTIALS: &[(&str, &str)] = &[
    (
        "an Anthropic API key (sk-ant-…)",
        r"sk-ant-[A-Za-z0-9_\-]{16,}",
    ),
    (
        "a model gateway key (sk-…)",
        r"(?P<before>^|[^A-Za-z0-9])sk-[a-z0-9]{32,}",
    ),
    (
        "a GitHub token (ghp_, gho_, ghs_, ghu_, ghr_…)",
        r"(?P<before>^|[^A-Za-z0-9])gh[oprsu]_[A-Za-z0-9]{20,}",
    ),
    (
        "a GitHub fine-grained token (github_pat_…)",
        r"(?P<before>^|[^A-Za-z0-9])github_pat_[A-Za-z0-9_]{20,}",
    ),
    (
        "a Slack token (xoxb-, xoxp-, xoxe-, xoxe.…)",
        r"(?P<before>^|[^A-Za-z0-9])xox[a-z][\-.][A-Za-z0-9.\-]{10,}",
    ),
    (
        "a Google OAuth access token (ya29.…)",
        r"(?P<before>^|[^A-Za-z0-9])ya29\.[A-Za-z0-9_\-]{20,}",
    ),
    (
        "a Google OAuth refresh token (1//…)",
        r"(?P<before>^|[^A-Za-z0-9])1//[A-Za-z0-9_\-]{20,}",
    ),
    (
        "a private key (-----BEGIN … PRIVATE KEY-----)",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----",
    ),
];

/// [`CREDENTIALS`], compiled once.
static CREDENTIAL_SHAPES: LazyLock<Result<Vec<(&'static str, Regex)>, String>> =
    LazyLock::new(|| {
        CREDENTIALS
            .iter()
            .map(|(name, shape)| Ok((*name, Regex::new(shape).map_err(|error| error.to_string())?)))
            .collect()
    });

/// [`CREDENTIAL_SHAPES`], or why they did not compile.
fn credential_shapes() -> Result<&'static [(&'static str, Regex)]> {
    CREDENTIAL_SHAPES
        .as_ref()
        .map(Vec::as_slice)
        .map_err(|error| anyhow!("compiling the credential shapes: {error}"))
}

/// What each kind of credential `text` holds is called, each kind once.
///
/// # Errors
///
/// Returns an error when the credential shapes do not compile.
pub fn credentials_in(text: &str) -> Result<Vec<&'static str>> {
    Ok(credential_shapes()?
        .iter()
        .filter(|(_, shape)| shape.is_match(text))
        .map(|(name, _)| *name)
        .collect())
}

/// The shape of an identity token, the signed JSON Web Token a caller of the ingress listener
/// sends with every request: three base64url parts, the first `eyJ`, the encoding of `{"`. A
/// log line never shows one, or the start of one, though a write is not refused for it: the
/// shapes above are the ones a record may never carry.
const IDENTITY_TOKEN: &str =
    r"(?P<before>^|[^A-Za-z0-9_\-])eyJ[A-Za-z0-9_\-]{8,}(?:\.[A-Za-z0-9_\-]*){0,2}";

/// [`IDENTITY_TOKEN`], compiled once.
static IDENTITY_TOKEN_SHAPE: LazyLock<Result<Regex, String>> =
    LazyLock::new(|| Regex::new(IDENTITY_TOKEN).map_err(|error| error.to_string()));

/// `text` with every credential and identity token in it replaced by `[redacted]`. A shape
/// that asks for what stands before a credential, so a prefix inside a longer word is not one,
/// names it `before`, and that stays.
///
/// # Errors
///
/// Returns an error when the credential shapes do not compile.
pub fn redacted(text: &str) -> Result<String> {
    let identity_token = IDENTITY_TOKEN_SHAPE
        .as_ref()
        .map_err(|error| anyhow!("compiling the identity token shape: {error}"))?;
    Ok(credential_shapes()?
        .iter()
        .map(|(_, shape)| shape)
        .chain(std::iter::once(identity_token))
        .fold(text.to_owned(), |text, shape| {
            shape.replace_all(&text, "${before}[redacted]").into_owned()
        }))
}

/// `text` on one line, whole: every credential taken out, every control character (a
/// newline, an escape, U+0085) a space, and each run of white space one space. Text quoted in
/// it can neither start a line of its own nor act on whoever reads the log.
#[must_use]
pub fn flattened(text: &str) -> String {
    let text = redacted(text).unwrap_or_else(|_| {
        "(withheld: the credential shapes that would redact it did not compile)".to_owned()
    });
    let mut flattened = String::with_capacity(text.len());
    let mut space_pending = false;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            space_pending = !flattened.is_empty();
        } else {
            if space_pending {
                flattened.push(' ');
            }
            flattened.push(character);
            space_pending = false;
        }
    }
    flattened
}

/// `text` as one log line's worth: [`flattened`], and cut to [`LINE_LIMIT`] characters.
#[must_use]
pub fn one_line(text: &str) -> String {
    let mut line = flattened(text);
    if let Some((cut, _)) = line.char_indices().nth(LINE_LIMIT) {
        line.truncate(cut);
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential goes, and what stood before it stays: a path that holds a token keeps the
    /// slash in front of it.
    #[test]
    fn redaction_keeps_what_stands_before_a_credential() -> Result<()> {
        let token = format!("ghs_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let key = format!("sk-ant-{}", "api03-A1b2C3d4E5f6G7h8");
        assert_eq!(
            redacted(&format!("decisions/{token}.md"))?,
            "decisions/[redacted].md"
        );
        assert_eq!(redacted(&format!("{token} leads"))?, "[redacted] leads");
        assert_eq!(redacted(&format!("key={key}"))?, "key=[redacted]");
        Ok(())
    }

    /// An identity token, or the start of one, never reaches a log line, though the write
    /// checks, which only refuse what a record may never carry, do not look for one.
    #[test]
    fn a_log_line_shows_no_identity_token() -> Result<()> {
        use base64::Engine as _;
        // Built from parts, so the source holds nothing secret scanning reads as a token.
        let part = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        let header = part(r#"{"alg":"RS256","kid":"k1"}"#);
        let claims = part(r#"{"email":"ada@example.com","aud":"https://ctx.example"}"#);
        let token = format!("{header}.{claims}.{}", part("signature"));
        let start = format!("{header}.{}", claims.get(..12).unwrap_or_default());
        assert_eq!(
            redacted(&format!("decisions/{token}.md"))?,
            "decisions/[redacted].md"
        );
        assert_eq!(one_line(&format!("x/{start}")), "x/[redacted]");
        assert!(
            credentials_in(&token)?.is_empty(),
            "a write is refused for it"
        );
        Ok(())
    }
}
