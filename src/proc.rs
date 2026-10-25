//! Bounded subprocesses.
//!
//! A command that is missing or hangs returns an error rather than blocking its caller.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

/// What a bounded run produced. A non-zero exit is data for the caller to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}
impl Output {
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

fn checked_reader_output(
    program: &str,
    stream: &str,
    output: std::io::Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    output.map_err(|error| anyhow!("{program}: reading {stream}: {error}"))
}

fn read_pipe_until<R>(mut pipe: R, deadline: Instant) -> std::io::Result<Vec<u8>>
where
    R: Read + AsRawFd,
{
    let descriptor = pipe.as_raw_fd();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8_192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_millis = i32::try_from(
            remaining
                .as_millis()
                .min(u128::from(i32::MAX.unsigned_abs())),
        )
        .map_err(std::io::Error::other)?;
        let mut poll_fd = libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // `poll_fd` points to a valid descriptor owned by `pipe` for the duration of this call.
        let poll_result = unsafe { libc::poll(&raw mut poll_fd, 1, timeout_millis) };
        if poll_result == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "reader deadline exceeded",
            ));
        }
        if poll_result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let bytes_read = pipe.read(&mut chunk)?;
        if bytes_read == 0 {
            return Ok(buffer);
        }
        let bytes = chunk.get(..bytes_read).ok_or_else(|| {
            std::io::Error::other("pipe returned more bytes than the read buffer can hold")
        })?;
        buffer.extend_from_slice(bytes);
    }
}

fn join_reader(
    program: &str,
    stream: &str,
    reader: JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>> {
    let output = reader
        .join()
        .map_err(|_| anyhow!("{program}: {stream} reader failed"))?;
    checked_reader_output(program, stream, output)
}

fn join_readers(
    program: &str,
    stdout_reader: JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_reader: JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let stdout = join_reader(program, "stdout", stdout_reader)?;
    let stderr = join_reader(program, "stderr", stderr_reader)?;
    Ok((stdout, stderr))
}

fn kill_process_group(child: &std::process::Child, program: &str, timeout: Duration) -> Result<()> {
    let process_group = i32::try_from(child.id())
        .map_err(|error| anyhow!("{program}: converting process group: {error}"))?;
    // `process_group(0)` assigns the child PID as its process-group ID. A negative PID tells
    // `kill` to signal every process in that group, including descendants that inherited pipes.
    let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(anyhow!(
                "{program}: timed out after {}s: killing process group: {error}",
                timeout.as_secs()
            ));
        }
    }
    Ok(())
}

fn timeout_error(program: &str, timeout: Duration) -> anyhow::Error {
    anyhow!("{program}: timed out after {}s", timeout.as_secs())
}

/// Run `program args…` with stdin closed and a wall-clock limit.
///
/// A missing binary errors as `<program>: not found`; a run past `timeout` terminates its
/// process group and errors as `<program>: timed out after <N>s`.
///
/// # Errors
///
/// Returns an error when the program cannot start, a pipe cannot be read, or either the process
/// group or reader threads exceed the deadline.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => anyhow!("{program}: not found"),
            _ => anyhow!("{program}: {error}"),
        })?;
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{program}: stdout not captured"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("{program}: stderr not captured"))?;
    let started = Instant::now();
    let deadline = started
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("{program}: timeout is too large"))?;
    let reader_deadline = deadline
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("{program}: timeout is too large"))?;
    let stdout_reader = thread::spawn(move || read_pipe_until(stdout_pipe, reader_deadline));
    let stderr_reader = thread::spawn(move || read_pipe_until(stderr_pipe, reader_deadline));
    let mut status = None;

    loop {
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| anyhow!("{program}: waiting: {error}"))?;
        }
        if let Some(exit_status) = status.as_ref() {
            if stdout_reader.is_finished() && stderr_reader.is_finished() {
                let (stdout, stderr) = join_readers(program, stdout_reader, stderr_reader)?;
                return Ok(Output {
                    code: exit_status.code(),
                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                });
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    kill_process_group(&child, program, timeout)?;
    if status.is_none() {
        child
            .wait()
            .map_err(|error| anyhow!("{program}: waiting after timeout: {error}"))?;
    }
    join_readers(program, stdout_reader, stderr_reader)
        .map_err(|_| timeout_error(program, timeout))?;
    Err(timeout_error(program, timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_and_exit_code() -> Result<()> {
        let output = run(
            "sh",
            &["-c", "printf hello; exit 0"],
            Duration::from_secs(5),
        )?;
        assert_eq!(output.stdout, "hello");
        assert!(output.success());
        Ok(())
    }

    #[test]
    fn non_zero_exit_is_data_not_an_error() -> Result<()> {
        let output = run(
            "sh",
            &["-c", "echo nope >&2; exit 3"],
            Duration::from_secs(5),
        )?;
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stderr.trim(), "nope");
        assert!(!output.success());
        Ok(())
    }

    #[test]
    fn missing_binary_is_named() {
        let error = run("ctx-no-such-binary-7f3a", &[], Duration::from_secs(5)).err();
        assert_eq!(
            error.map(|value| value.to_string()),
            Some("ctx-no-such-binary-7f3a: not found".to_owned())
        );
    }

    #[test]
    fn hung_process_is_killed_and_reported() {
        let started = Instant::now();
        let error = run("sleep", &["30"], Duration::from_secs(1)).err();
        assert_eq!(
            error.map(|value| value.to_string()),
            Some("sleep: timed out after 1s".to_owned())
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn injected_reader_error_rejects_partial_stdout() -> Result<()> {
        let error = checked_reader_output(
            "gws",
            "stdout",
            Err(std::io::Error::other("injected pipe failure")),
        )
        .err()
        .ok_or_else(|| anyhow!("injected reader error unexpectedly succeeded"))?;
        assert!(format!("{error:#}").contains("gws: reading stdout: injected pipe failure"));
        Ok(())
    }

    #[test]
    fn timeout_is_enforced_when_a_descendant_holds_the_pipes() {
        let started = Instant::now();
        let output = run("sh", &["-c", "sleep 30 & wait"], Duration::from_secs(1));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        assert!(output.is_err() || output.is_ok_and(|value| !value.success()));
    }
}
