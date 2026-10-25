//! Stable JSON output envelopes for command results.

use std::io::Write as _;

use anyhow::Result;
use serde::Serialize;

pub const JSON_VERSION: u32 = 1;

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
}

impl Outcome {
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::StructuralDefects | Self::SourceErrors => 1,
        }
    }
}

/// Emits a command result in either the stable JSON envelope or human-readable form.
///
/// # Errors
///
/// Returns an error when JSON serialization or stdout output fails.
pub fn emit<T: Serialize>(
    json: bool,
    command: &str,
    data: &T,
    human: impl FnOnce(&T),
) -> Result<()> {
    if json {
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        let envelope = Envelope {
            version: JSON_VERSION,
            command,
            data,
        };
        serde_json::to_writer(&mut output, &envelope)?;
        writeln!(&mut output)?;
    } else {
        human(data);
    }
    Ok(())
}

/// Emits a command failure using the stable JSON error envelope.
///
/// # Errors
///
/// Returns an error when JSON serialization or stdout output fails.
pub fn emit_error(command: &str, message: String) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let envelope = ErrorEnvelope {
        version: JSON_VERSION,
        command,
        error: ErrorBody { message },
    };
    serde_json::to_writer(&mut output, &envelope)?;
    writeln!(&mut output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::{Envelope, ErrorBody, ErrorEnvelope, JSON_VERSION};

    #[derive(Serialize)]
    struct Payload {
        value: u8,
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
}
