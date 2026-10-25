//! External executable implementation of the source protocol.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};

use crate::proc;

use super::{Message, Request, Source};

/// A source executable found on `PATH`.
pub struct External {
    program: PathBuf,
    timeout_secs: Option<u64>,
}

impl External {
    /// Finds `ctx-source-<kind>` on `PATH`.
    ///
    /// # Errors
    ///
    /// Returns an error when no executable with that name is present on `PATH`.
    pub fn find(kind: &str) -> Result<Self> {
        let name = format!("ctx-source-{kind}");
        let paths = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
        let program = std::env::split_paths(&paths)
            .map(|directory| {
                if directory.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    directory
                }
            })
            .map(|directory| directory.join(&name))
            .find(|candidate| executable_file(candidate))
            .ok_or_else(|| anyhow!("{name} not found on PATH"))?;
        Ok(Self {
            program,
            timeout_secs: None,
        })
    }

    /// Configures the source-specific timeout when one is present in `ctx.toml`.
    #[must_use]
    pub fn with_timeout_secs(mut self, timeout_secs: Option<u64>) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    fn timeout(&self, request: &Request) -> Duration {
        let default = match request {
            Request::Fetch(_) => 3_600,
            Request::Resolve(_) => 120,
        };
        Duration::from_secs(self.timeout_secs.unwrap_or(default))
    }
}

impl Source for External {
    fn run(&self, request: &Request, sink: &mut dyn FnMut(Message) -> Result<()>) -> Result<()> {
        let input = serde_json::to_vec(request).context("serializing source request")?;
        let program = self
            .program
            .to_str()
            .ok_or_else(|| anyhow!("source executable path is not valid UTF-8"))?;
        let output = proc::run_with_stdin(program, &[], &input, self.timeout(request))?;
        if !output.success() {
            let status = output
                .code
                .map_or_else(|| "signal".to_owned(), |code| format!("exit {code}"));
            let stderr = stderr_tail(&output.stderr);
            if stderr.is_empty() {
                bail!("{program} {status}");
            }
            bail!("{program} {status}: {stderr}");
        }
        for (line_number, line) in output.stdout.lines().enumerate() {
            if line.trim().is_empty() {
                bail!("{program}: empty protocol line {}", line_number + 1);
            }
            let message = serde_json::from_str(line)
                .with_context(|| format!("{program}: parsing protocol line {}", line_number + 1))?;
            sink(message).with_context(|| {
                format!("{program}: accepting protocol line {}", line_number + 1)
            })?;
        }
        Ok(())
    }
}

fn executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn stderr_tail(stderr: &str) -> String {
    let start = stderr.len().saturating_sub(4_096);
    stderr
        .as_bytes()
        .get(start..)
        .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
        .unwrap_or_default()
}
