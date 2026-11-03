//! Bounded subprocesses.
//!
//! A command that is missing or hangs returns an error rather than blocking its caller. A
//! process that calls [`stop_runs_with_this_process`] also stops the runs under way when it is
//! itself stopped or killed.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

/// How many runs a stop reaches at once. A fetch runs one source at a time; a run past this many
/// is not tracked, and only its parent-death signal stops it.
const TRACKED_RUNS: usize = 32;

/// The process group of each run under way, 0 for a free slot. The signal handler reads them, so
/// they are atomics, which a handler may read, not a lock, which it may not take.
static RUNNING: [AtomicI32; TRACKED_RUNS] = [const { AtomicI32::new(0) }; TRACKED_RUNS];

/// Whether runs end with this process: set once by [`stop_runs_with_this_process`].
static BOUND: AtomicBool = AtomicBool::new(false);

/// A run's slot in [`RUNNING`], freed once the run is over.
///
/// A run is tracked just after it is spawned, so a stop that lands in between misses it. On
/// Linux its parent-death signal still stops it as this process ends; elsewhere it runs on.
struct Tracked(Option<&'static AtomicI32>);

impl Tracked {
    fn start(group: i32) -> Self {
        Self(RUNNING.iter().find(|slot| {
            slot.compare_exchange(0, group, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }))
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(slot) = self.0 {
            slot.store(0, Ordering::SeqCst);
        }
    }
}

/// Kills the process group of every run under way, then ends this process by `signal`, as it
/// would have ended without a handler.
extern "C" fn stop_runs(signal: libc::c_int) {
    for slot in &RUNNING {
        let group = slot.load(Ordering::SeqCst);
        if group > 0 {
            // `kill` is async-signal-safe. A negative PID names the whole group, so what the
            // program started stops with it; a group already gone answers ESRCH.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
    // SA_RESETHAND gave `signal` its default action back and SA_NODEFER left it unblocked, so
    // `raise` ends the process at once. The first process of a PID namespace, as `ctx` is in a
    // container whose entry point it is, is not ended by a default action it asks for itself, so
    // it exits as a shell reports a process that signal ended. `raise` and `_exit` are
    // async-signal-safe.
    unsafe {
        libc::raise(signal);
        libc::_exit(128 + signal);
    }
}

/// Makes the runs of this process stop when it does, for a command such as `ctx fetch` whose
/// runs (its sources) are of no use once it is gone.
///
/// SIGTERM, SIGINT and SIGHUP kill the process group of every run under way and then end this
/// process by the same signal, or, as the first process of a PID namespace, exit with 128 plus
/// its number. A run gets its own process group, so a terminal's interrupt does not reach it, and
/// without this a stopped fetch would leave its sources running. A signal this process started
/// out ignoring, as a background job ignores SIGINT, stays ignored. On Linux each later run is
/// also killed by the kernel when this process ends any other way, SIGKILL included; that
/// reaches only the program itself, not what it started.
///
/// # Errors
///
/// Returns an error when a signal's action cannot be read or set.
pub fn stop_runs_with_this_process() -> Result<()> {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // A zeroed `sigaction` is a valid value: no handler, an empty mask, no flags.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // `current` is a valid `sigaction` this call fills.
        if unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut current) } == -1 {
            return Err(anyhow!(
                "reading the action of signal {signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if current.sa_sigaction == libc::SIG_IGN {
            continue;
        }
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = stop_runs as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
        // `action` is a valid `sigaction` that outlives both calls.
        unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        if unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) } == -1 {
            return Err(anyhow!(
                "handling signal {signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    BOUND.store(true, Ordering::SeqCst);
    Ok(())
}

/// Asks the kernel to kill the child when this process ends, which reaches it however this
/// process ends, SIGKILL included. Linux sends the signal when the thread that started the child
/// ends; a run's thread waits for its child, so that is when the process does.
#[cfg(target_os = "linux")]
fn die_with_this_process(command: &mut Command) -> Result<()> {
    let parent = libc::pid_t::try_from(std::process::id())
        .map_err(|error| anyhow!("converting this process id: {error}"))?;
    let signal = libc::c_ulong::try_from(libc::SIGKILL)
        .map_err(|error| anyhow!("converting SIGKILL: {error}"))?;
    // The closure runs in the child between fork and exec, so it may only call what is
    // async-signal-safe: `prctl` and `getppid` are, and an error from a raw code allocates nothing.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, signal) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // This process may have ended before the request was made; the child then has
            // another parent already, and nothing would ever stop it.
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    Ok(())
}

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

fn deadline_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "deadline exceeded")
}

/// The instant a run that began at `start` with `timeout` must finish by. `None` when `timeout`
/// is so large the sum overflows. This is the one place a run's deadline is computed, so a test
/// can hold it to `start + timeout` exactly, without waiting.
fn run_deadline(start: Instant, timeout: Duration) -> Option<Instant> {
    start.checked_add(timeout)
}

fn poll_until_ready(
    descriptor: std::os::fd::RawFd,
    events: libc::c_short,
    deadline: Instant,
) -> std::io::Result<()> {
    loop {
        if Instant::now() >= deadline {
            return Err(deadline_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_millis = i32::try_from(
            remaining
                .as_millis()
                .saturating_add(1)
                .min(u128::from(i32::MAX.unsigned_abs())),
        )
        .map_err(std::io::Error::other)?;
        let mut poll_fd = libc::pollfd {
            fd: descriptor,
            events,
            revents: 0,
        };
        // `poll_fd` points to a valid descriptor owned by its caller for this system call.
        let poll_result = unsafe { libc::poll(&raw mut poll_fd, 1, timeout_millis) };
        if poll_result > 0 {
            return Ok(());
        }
        if poll_result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }
}

/// Reads `pipe` to its end, or fails once the deadline passes or it has held more than `limit`
/// bytes. A failure drops the pipe, so a writer that goes on writing gets `EPIPE` rather than
/// filling this process's memory.
fn read_pipe_until<R>(mut pipe: R, deadline: Instant, limit: usize) -> std::io::Result<Vec<u8>>
where
    R: Read + AsRawFd,
{
    let descriptor = pipe.as_raw_fd();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8_192];
    loop {
        poll_until_ready(descriptor, libc::POLLIN | libc::POLLHUP, deadline)?;
        if Instant::now() >= deadline {
            return Err(deadline_error());
        }
        let bytes_read = pipe.read(&mut chunk)?;
        if bytes_read == 0 {
            return Ok(buffer);
        }
        let bytes = chunk.get(..bytes_read).ok_or_else(|| {
            std::io::Error::other("pipe returned more bytes than the read buffer can hold")
        })?;
        if buffer.len().saturating_add(bytes.len()) > limit {
            return Err(std::io::Error::other(format!(
                "output exceeds {limit} bytes"
            )));
        }
        buffer.extend_from_slice(bytes);
    }
}

/// Writes `input` to `pipe` by the deadline. The pipe is made nonblocking first: a write of more
/// than the pipe holds to a child that never reads would otherwise block past the deadline,
/// since `POLLOUT` promises room for one byte, not for the whole write.
fn write_pipe_until<W>(mut pipe: W, input: &[u8], deadline: Instant) -> std::io::Result<()>
where
    W: Write + AsRawFd,
{
    let descriptor = pipe.as_raw_fd();
    // `descriptor` is the open pipe `pipe` owns for this whole call.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut written = 0;
    while written < input.len() {
        poll_until_ready(descriptor, libc::POLLOUT, deadline)?;
        if Instant::now() >= deadline {
            return Err(deadline_error());
        }
        let remaining = input
            .get(written..)
            .ok_or_else(|| std::io::Error::other("stdin write offset exceeds input length"))?;
        let just_written = match pipe.write(remaining) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if just_written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "stdin write made no progress",
            ));
        }
        written = written
            .checked_add(just_written)
            .ok_or_else(|| std::io::Error::other("stdin write offset overflowed"))?;
    }
    Ok(())
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

/// A run stopped because it went past its wall-clock limit.
///
/// Typed, so a caller can tell "the program was stopped part-way through" — after which what
/// it did is unknown — from "the program said no".
#[derive(Debug)]
pub struct TimedOut {
    program: String,
    timeout: Duration,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: timed out after {}s",
            self.program,
            self.timeout.as_secs()
        )
    }
}

impl std::error::Error for TimedOut {}

fn timeout_error(program: &str, timeout: Duration) -> anyhow::Error {
    anyhow::Error::new(TimedOut {
        program: program.to_owned(),
        timeout,
    })
}

/// Run `program args…` with `stdin` and a wall-clock limit.
///
/// An empty `stdin` closes the child's standard input. A missing binary errors as
/// `<program>: not found`; a run past `timeout` terminates its process group and errors as
/// `<program>: timed out after <N>s`.
///
/// # Errors
///
/// Returns an error when the program cannot start, stdin cannot be written, a pipe cannot be
/// read, or the process group exceeds the deadline.
pub fn run_with_stdin(
    program: &str,
    args: &[&str],
    stdin: &[u8],
    timeout: Duration,
) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    run_command(command, program, stdin, timeout)
}

/// Run a prepared command with `stdin` and a wall-clock limit, as [`run_with_stdin`] does.
///
/// The caller sets the program, arguments, environment and directory; this sets the three
/// standard streams and puts the child in a process group of its own, so a run past
/// `timeout` stops every process it started. `program` names it in errors.
///
/// # Errors
///
/// Returns an error when the program cannot start, stdin cannot be written, a pipe cannot be
/// read, or the process group exceeds the deadline.
pub fn run_command(
    command: Command,
    program: &str,
    stdin: &[u8],
    timeout: Duration,
) -> Result<Output> {
    run_command_limited(command, program, stdin, timeout, usize::MAX, usize::MAX)
}

/// Run a prepared command as [`run_command`] does, reading at most `stdout_limit` bytes of its
/// standard output and `stderr_limit` of its standard error. A stream past its limit is an
/// error naming the limit: the pipe is closed, so the program stops on its next write or
/// runs into the deadline.
///
/// # Errors
///
/// Returns an error when the program cannot start, stdin cannot be written, a pipe cannot be
/// read or holds more than its limit, or the process group exceeds the deadline.
pub fn run_command_limited(
    mut command: Command,
    program: &str,
    stdin: &[u8],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<Output> {
    let child_stdin = if stdin.is_empty() {
        Stdio::null()
    } else {
        Stdio::piped()
    };
    #[cfg(target_os = "linux")]
    if BOUND.load(Ordering::SeqCst) {
        die_with_this_process(&mut command)?;
    }
    let mut child = command
        .stdin(child_stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => anyhow!("{program}: not found"),
            _ => anyhow!("{program}: {error}"),
        })?;
    // The child leads its own process group, so its id names the group a stop kills.
    let _tracked = Tracked::start(
        i32::try_from(child.id())
            .map_err(|error| anyhow!("{program}: converting process group: {error}"))?,
    );
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{program}: stdout not captured"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("{program}: stderr not captured"))?;
    let started = Instant::now();
    let deadline =
        run_deadline(started, timeout).ok_or_else(|| anyhow!("{program}: timeout is too large"))?;
    let stdout_reader = thread::spawn(move || read_pipe_until(stdout_pipe, deadline, stdout_limit));
    let stderr_reader = thread::spawn(move || read_pipe_until(stderr_pipe, deadline, stderr_limit));
    if !stdin.is_empty() {
        let stdin_pipe = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{program}: stdin not captured"))?;
        if let Err(error) = write_pipe_until(stdin_pipe, stdin, deadline) {
            kill_process_group(&child, program, timeout)?;
            child.wait().map_err(|wait_error| {
                anyhow!("{program}: waiting after stdin error: {wait_error}")
            })?;
            return Err(anyhow!("{program}: writing stdin: {error}"));
        }
    }
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
    if stdout_reader.is_finished() && stderr_reader.is_finished() {
        join_readers(program, stdout_reader, stderr_reader)
            .map_err(|_| timeout_error(program, timeout))?;
    }
    Err(timeout_error(program, timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test runs with standard input closed.
    fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
        run_with_stdin(program, args, &[], timeout)
    }

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
            "ctx-source-example",
            "stdout",
            Err(std::io::Error::other("injected pipe failure")),
        )
        .err()
        .ok_or_else(|| anyhow!("injected reader error unexpectedly succeeded"))?;
        assert!(format!("{error:#}")
            .contains("ctx-source-example: reading stdout: injected pipe failure"));
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

    #[test]
    fn timeout_is_not_extended_by_a_pipe_holder_outside_the_process_group() -> Result<()> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let pid_file = std::env::temp_dir().join(format!(
            "ctx-memory-proc-{}-{unique}.pid",
            std::process::id()
        ));
        std::fs::File::create_new(&pid_file)?;
        // The holder sleeps far longer than the one-second timeout, so the two outcomes are
        // unmistakable: a timeout that fires returns in about a second, and one a pipe holder
        // extended would wait the whole sleep. The bound sits between them with wide margin on
        // each side, so neither a slow machine under load nor a longer kill path can confuse them.
        let holder = Duration::from_secs(120);
        let script = format!(
            "setsid sh -c 'sleep {}' & echo $! > {}",
            holder.as_secs(),
            pid_file.display()
        );

        let started = Instant::now();
        let output = run("sh", &["-c", &script], Duration::from_secs(1));
        let elapsed = started.elapsed();
        let pid = std::fs::read_to_string(&pid_file)?;
        // Deterministic, not timing: the holder is still sleeping when `run` returns. A run that
        // waited for the pipe to close would return only after the holder had exited, so finding
        // it alive proves the timeout fired without waiting for the holder, whatever the load.
        let holder_alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()?
            .success();
        let process_group = format!("-{}", pid.trim());
        let cleanup = std::process::Command::new("kill")
            .args(["-KILL", "--", process_group.as_str()])
            .status()?;
        std::fs::remove_file(&pid_file)?;

        assert!(cleanup.success());
        assert!(output.is_err());
        assert!(
            holder_alive,
            "the run returned only after the pipe holder had exited, so it waited for it"
        );
        assert!(
            elapsed < Duration::from_secs(1) + holder / 4,
            "a pipe holder outside the process group extended the one-second timeout to {elapsed:?}"
        );
        Ok(())
    }

    /// The wall-time bounds above are generous on purpose, so they prove only that a timeout is
    /// not extended, not the deadline itself. This reads the deadline straight from the poll the
    /// bounded runs are built on: a pipe with its write end open and no data never becomes
    /// readable, so the poll runs to its deadline and no further. The upper bound is close enough
    /// to the short deadline to catch a deadline computed seconds late, and far enough above it to
    /// absorb scheduling under load; the lower bound catches a poll that gives up early.
    #[test]
    fn poll_waits_for_its_deadline_and_no_longer() {
        let mut fds = [0 as std::os::fd::RawFd; 2];
        assert_eq!(
            unsafe { libc::pipe(fds.as_mut_ptr()) },
            0,
            "could not make a pipe"
        );
        let [read_end, write_end] = fds;
        let deadline_in = Duration::from_millis(300);
        let started = Instant::now();
        let result = poll_until_ready(read_end, libc::POLLIN, Instant::now() + deadline_in);
        let elapsed = started.elapsed();
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
        assert!(
            result.is_err(),
            "the poll reported ready on a pipe with no data"
        );
        assert!(
            elapsed >= deadline_in,
            "the poll gave up {elapsed:?} in, before its {deadline_in:?} deadline"
        );
        assert!(
            elapsed < deadline_in + Duration::from_secs(1),
            "the poll ran {elapsed:?}, well past its {deadline_in:?} deadline: a deadline computed \
             late would show here"
        );
    }

    /// The deadline the bounded runs enforce is exactly the start plus the timeout, computed in
    /// one place so this holds it without waiting: a deadline set even a little early or late is
    /// caught here, which a wall-time bound on a real run cannot do without flaking.
    #[test]
    fn the_run_deadline_is_the_start_plus_the_timeout() {
        let start = Instant::now();
        for seconds in [0, 1, 2, 7, 3600] {
            let timeout = Duration::from_secs(seconds);
            assert_eq!(
                run_deadline(start, timeout),
                Some(start + timeout),
                "the run deadline for a {timeout:?} timeout is not the start plus the timeout"
            );
        }
        assert_eq!(
            run_deadline(start, Duration::MAX),
            None,
            "a timeout too large to add to the start must have no deadline, not a wrapped one"
        );
    }
}
