//! Live sessions under the pinned omp release, against a scripted stand-in for the model.

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context as _, Result};
use serde_json::{json, Value};

use crate::fixtures::{
    remove_scratch, repo, runner, scratch, write_executable, EXAMPLE_MODEL, EXAMPLE_MODEL_ID,
};
use crate::policy::maintenance_prompt;

/// The arguments `run-cycle.sh` starts omp with, read from the script itself so this test
/// cannot drift from it, with the container's paths replaced by this machine's.
fn cycle_omp_arguments(policy: &Path, session_dir: &Path, prompt: &str) -> Result<Vec<String>> {
    let script = runner("run-cycle.sh")?;
    let mut block = String::new();
    let mut started = false;
    for line in script.lines().map(str::trim) {
        if !started && line.starts_with("set -- ") {
            started = true;
        }
        if started {
            block.push(' ');
            block.push_str(line.trim_end_matches('\\'));
            if !line.ends_with('\\') {
                if block.contains(" omp -p") {
                    break;
                }
                block.clear();
                started = false;
            }
        }
    }
    let words: Vec<&str> = block.split_whitespace().collect();
    let start = words
        .iter()
        .position(|word| *word == "omp")
        .context("run-cycle.sh starts no omp")?;
    words
        .get(start + 1..)
        .unwrap_or_default()
        .iter()
        .map(|word| match *word {
            "/opt/ctx-memory/policy.yml" => Ok(policy.display().to_string()),
            "/opt/ctx-memory/ctx-record.ts" => {
                Ok(repo().join("runner/ctx-record.ts").display().to_string())
            }
            "\"$session_dir\"" => Ok(session_dir.display().to_string()),
            "\"$prompt\"" => Ok(prompt.to_owned()),
            "\"$CTX_MODEL\"" => Ok(EXAMPLE_MODEL.to_owned()),
            "\"${SESSION_MINUTES}m\"" => script
                .lines()
                .find_map(|line| line.strip_prefix("SESSION_MINUTES="))
                .map(|minutes| format!("{minutes}m"))
                .context("run-cycle.sh passes omp ${SESSION_MINUTES} but never sets it"),
            other if other.contains('$') || other.contains('"') => {
                bail!("run-cycle.sh passes omp `{other}`, which this test cannot stand in for")
            }
            other => Ok(other.to_owned()),
        })
        .collect()
}

/// The key the probe's session holds, in the file the example models.yml names; the fake model
/// checks that every call carries it, and nothing else, as the model API's bearer token.
const MODEL_KEY: &str = "live-probe-model-key";

/// A scripted stand-in for the model API, an Anthropic Messages API. The answer to a request is
/// the step at the index of the number of tool results the request carries, so a retry or a side
/// request (omp asks for skill summaries) never advances the script.
struct FakeModel {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    /// The path, model, `Authorization` and `X-Api-Key` of every request, in arrival order.
    routes: Arc<Mutex<Vec<Value>>>,
    sightings: Arc<Mutex<Vec<String>>>,
}

impl FakeModel {
    fn start(steps: Vec<Value>) -> Result<Self> {
        Self::serve(steps, None)
    }

    /// Also looks, each time the session asks the model, for a running process whose command
    /// line holds `token`, and keeps every one it sees.
    fn start_watching(steps: Vec<Value>, token: String) -> Result<Self> {
        Self::serve(steps, Some(token))
    }

    fn serve(steps: Vec<Value>, watch: Option<String>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(Mutex::new(Vec::new()));
        let sightings = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let route_log = Arc::clone(&routes);
        let sighted = Arc::clone(&sightings);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if let (Some(token), Ok(mut sighted)) = (&watch, sighted.lock()) {
                    sighted.extend(processes_holding(token));
                }
                let _ = serve_one(stream, &steps, &seen, &route_log);
            }
        });
        Ok(Self {
            port,
            requests,
            routes,
            sightings,
        })
    }

    /// Every request reached the model API as `runner/models.example.yml` routes it: the model
    /// is the example's model id, the one `run-cycle.sh` selects, sent as it is, and the one
    /// credential is the key from the file models.yml names, as a bearer token.
    fn check_gateway_route(&self) -> Result<()> {
        let routes = self
            .routes
            .lock()
            .map_err(|error| anyhow::anyhow!("reading the recorded routes: {error}"))?;
        ensure!(!routes.is_empty(), "the session never called the model");
        let bearer = format!("Bearer {MODEL_KEY}");
        for route in routes.iter() {
            ensure!(
                route.get("path").and_then(Value::as_str) == Some("/v1/messages")
                    && route.get("model").and_then(Value::as_str) == Some(EXAMPLE_MODEL_ID)
                    && route.get("authorization").and_then(Value::as_str) == Some(&bearer)
                    && route.get("x-api-key").is_some_and(Value::is_null),
                "a model call did not reach the model API as models.yml routes it: {route}"
            );
        }
        Ok(())
    }

    fn sightings(&self) -> Result<Vec<String>> {
        Ok(self
            .sightings
            .lock()
            .map_err(|error| anyhow::anyhow!("reading the recorded processes: {error}"))?
            .clone())
    }

    /// Every tool result the session sent back, by the id of the step that asked for it.
    fn tool_results(&self) -> Result<Vec<(String, bool, String)>> {
        let requests = self
            .requests
            .lock()
            .map_err(|error| anyhow::anyhow!("reading the recorded requests: {error}"))?;
        let mut results = Vec::new();
        for request in requests.iter() {
            for message in request
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for block in message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if results.iter().any(|(seen, _, _)| *seen == id) {
                        continue;
                    }
                    let error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    results.push((
                        id,
                        error,
                        block
                            .get("content")
                            .map_or_else(String::new, Value::to_string),
                    ));
                }
            }
        }
        Ok(results)
    }

    /// The tools the session first offered the model.
    fn offered_tools(&self) -> Result<Vec<Value>> {
        let requests = self
            .requests
            .lock()
            .map_err(|error| anyhow::anyhow!("reading the recorded requests: {error}"))?;
        requests
            .iter()
            .find_map(|request| {
                request
                    .get("tools")
                    .and_then(Value::as_array)
                    .filter(|tools| !tools.is_empty())
                    .cloned()
            })
            .context("the session never offered the model a tool")
    }

    fn tool_names(&self) -> Result<Vec<String>> {
        Ok(self
            .offered_tools()?
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }

    /// The argument names the session offered the model for the tool `name`.
    fn tool_fields(&self, name: &str) -> Result<Vec<String>> {
        let tools = self.offered_tools()?;
        let tool = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .with_context(|| format!("the session never offered the model `{name}`"))?;
        Ok(tool
            .pointer("/input_schema/properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default())
    }
}

/// Reads one request from `stream`: its request line and its body. Its path, model,
/// `Authorization` and `X-Api-Key` go to `routes`.
fn read_request(stream: &TcpStream, routes: &Mutex<Vec<Value>>) -> Result<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut length = 0;
    let mut authorization = Value::Null;
    let mut api_key = Value::Null;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse()?;
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Value::from(value.trim());
            } else if name.eq_ignore_ascii_case("x-api-key") {
                api_key = Value::from(value.trim());
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|request| request.get("model").cloned());
    routes
        .lock()
        .map_err(|error| anyhow::anyhow!("recording a route: {error}"))?
        .push(json!({
            "path": request_line.split_whitespace().nth(1),
            "model": model,
            "authorization": authorization,
            "x-api-key": api_key,
        }));
    Ok((request_line, body))
}

fn serve_one(
    stream: TcpStream,
    steps: &[Value],
    seen: &Mutex<Vec<Value>>,
    routes: &Mutex<Vec<Value>>,
) -> Result<()> {
    let (request_line, body) = read_request(&stream, routes)?;
    let mut stream = stream;
    if !request_line.starts_with("POST /v1/messages") {
        stream.write_all(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    let request: Value = serde_json::from_slice(&body)?;
    let done = request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .count();
    let offers_tools = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let answer = if offers_tools {
        steps
            .get(done)
            .cloned()
            .unwrap_or_else(|| json!({"text": "The script is finished."}))
    } else {
        json!({"text": "ok"})
    };
    let model = request.get("model").cloned().unwrap_or(Value::Null);
    seen.lock()
        .map_err(|error| anyhow::anyhow!("recording a request: {error}"))?
        .push(request);

    let mut events = vec![json!({
        "type": "message_start",
        "message": {
            "id": format!("msg_{done}"), "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    })];
    let stop_reason = if let (Some(tool), Some(input)) = (answer.get("tool"), answer.get("input")) {
        events.push(json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": format!("toolu_{done}"), "name": tool, "input": {}}
        }));
        events.push(json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}
        }));
        "tool_use"
    } else {
        events.push(json!({
            "type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}
        }));
        events.push(json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": answer.get("text").cloned().unwrap_or(Value::Null)}
        }));
        "end_turn"
    };
    events.push(json!({"type": "content_block_stop", "index": 0}));
    events.push(json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
        "usage": {"output_tokens": 1}
    }));
    events.push(json!({"type": "message_stop"}));

    stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    )?;
    for event in events {
        let name = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        stream.write_all(format!("event: {name}\ndata: {event}\n\n").as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

/// The command line of every running process that holds `token`, read from `/proc`.
fn processes_holding(token: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        })
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .map(|cmdline| String::from_utf8_lossy(&cmdline).replace('\0', " "))
        .filter(|cmdline| cmdline.contains(token))
        .collect()
}

/// The omp binary `CTX_TEST_OMP` names, checked to run and to be the release `runner/omp.lock`
/// pins.
fn pinned_omp() -> Result<std::ffi::OsString> {
    let omp = std::env::var_os("CTX_TEST_OMP").context(
        "CTX_TEST_OMP must name the pinned omp binary; this test drives it and never skips",
    )?;
    let version = Command::new(&omp)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", omp.to_string_lossy()))?;
    ensure!(
        version.status.success(),
        "{} --version failed: {}",
        omp.to_string_lossy(),
        String::from_utf8_lossy(&version.stderr)
    );
    let lock = runner("omp.lock")?;
    let pinned = lock
        .lines()
        .find_map(|line| line.strip_prefix("OMP_RELEASE=v"))
        .and_then(|release| release.split('-').next())
        .context("runner/omp.lock pins no omp release")?;
    let reported = String::from_utf8_lossy(&version.stdout);
    ensure!(
        reported.trim() == format!("omp/{pinned}"),
        "CTX_TEST_OMP is {}, not the omp {pinned} runner/omp.lock pins",
        reported.trim()
    );
    Ok(omp)
}

/// Runs omp in `root/workspace` with `root/home` as its `HOME`: the repository's skills, and
/// `runner/models.example.yml` as `run-cycle.sh` writes it, with `model`'s address for the
/// placeholder and [`MODEL_KEY`] in the file it names. `env` is added to the session's otherwise
/// fixed environment, and `extra` after the cycle's own omp arguments, where a repeated flag wins
/// (`cli/flag-tables.ts:209-219`), so a probe can widen what the cycle allows. Every call the
/// session made must have reached `model` as models.yml routes it.
fn run_session(
    omp: &std::ffi::OsStr,
    root: &Path,
    model: &FakeModel,
    prompt: &str,
    env: &[(&str, &str)],
    extra: &[&str],
) -> Result<()> {
    let home = root.join("home");
    let workspace = root.join("workspace");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(home.join(".omp/agent"))?;
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&sessions)?;
    std::fs::write(
        home.join(".omp/agent/config.yml"),
        format!(
            "skills:\n  customDirectories:\n    - {}\n",
            repo().join("skills").display()
        ),
    )?;
    let key_file = root.join("model-key");
    std::fs::write(&key_file, MODEL_KEY)?;
    let template = runner("models.example.yml")?;
    for placeholder in [
        "\"@CTX_MODEL_BASE_URL@\"",
        "\"!cat /run/ctx-agent/model-key\"",
    ] {
        ensure!(
            template.matches(placeholder).count() == 1,
            "runner/models.example.yml does not hold {placeholder} once, which this test stands in for"
        );
    }
    std::fs::write(
        home.join(".omp/agent/models.yml"),
        template
            .replace(
                "@CTX_MODEL_BASE_URL@",
                &format!("http://127.0.0.1:{}", model.port),
            )
            .replace("/run/ctx-agent/model-key", &key_file.display().to_string()),
    )?;
    let mut arguments = cycle_omp_arguments(&repo().join("runner/policy.yml"), &sessions, prompt)?;
    // The prompt is positional and stays last.
    let prompt_argument = arguments
        .pop()
        .context("run-cycle.sh passes omp no prompt")?;
    arguments.extend(extra.iter().map(|word| (*word).to_owned()));
    arguments.push(prompt_argument);
    let ctx_dir = Path::new(env!("CARGO_BIN_EXE_ctx"))
        .parent()
        .context("locating the ctx binary's directory")?;
    let stdout = std::fs::File::create(root.join("omp.stdout"))?;
    let stderr_path = root.join("omp.stderr");
    let stderr = std::fs::File::create(&stderr_path)?;
    let mut child = Command::new(omp)
        .args(&arguments)
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", ctx_dir.display()))
        .env("HOME", &home)
        .env("TMPDIR", root)
        .env("CTX_MEMORY", repo().join("starter"))
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("starting {}", omp.to_string_lossy()))?;
    let deadline = Instant::now() + Duration::from_secs(600);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            child.kill()?;
            bail!("the session ran past ten minutes");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    ensure!(
        status.success(),
        "omp exited with {status}: {}",
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );
    model.check_gateway_route()
}

/// One live session under `run-cycle.sh`'s own flags and `policy.yml`, against the omp
/// release `runner/omp.lock` pins, with a scripted model that does what an injected instruction
/// asks: read a URL, search one, start a program through `ctx`, and write a record from a file
/// at the shell. `ctx rules` and `ctx record show` still run. No model is called.
#[test]
#[ignore = "drives the omp release runner/omp.lock pins; CI runs it with CTX_TEST_OMP naming that binary"]
fn a_live_session_reads_no_url_and_starts_nothing_but_ctx() -> Result<()> {
    let omp = pinned_omp()?;
    // The program prints `escaped-ctx`, a string its own command line does not hold, so the
    // marker in a result can only come from the program having run.
    let escape = format!(
        "ctx a{}/bin/sh -c 'printf \"%s-%s\" escaped ctx'",
        "/..".repeat(48)
    );
    let model = FakeModel::start(vec![
        json!({"tool": "read", "input": {"path": "https://example.com"}}),
        json!({"tool": "grep", "input": {"pattern": "Example", "path": "https://example.com"}}),
        json!({"tool": "read", "input": {"path": "ssh://example.com/etc/hostname"}}),
        json!({"tool": "bash", "input": {"command": escape}}),
        json!({"tool": "bash", "input": {"command": "curl -sS https://example.com"}}),
        json!({"tool": "bash", "input": {"command": "ctx rules"}}),
        json!({"tool": "bash", "input": {"command": "ctx record show index.md"}}),
        json!({"tool": "bash", "input": {"command": "ctx record put index.md /etc/hostname"}}),
    ])?;
    let root = scratch("live")?;
    let outcome = run_session(&omp, &root, &model, "Run the scripted probe.", &[], &[]);
    remove_scratch(&root)?;
    outcome?;

    assert_eq!(
        model.tool_names()?.into_iter().collect::<BTreeSet<_>>(),
        [
            "bash",
            "read",
            "grep",
            "glob",
            "todo",
            "ctx_record_put",
            "ctx_record_rm"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>(),
        "the session holds a different set of tools than run-cycle.sh names"
    );
    // A service run hands its command to omp's launch broker, which keeps the program it runs in
    // records under omp's state directory, where the session can write. policy.yml turns the
    // mode off, so the bash tool takes no service name.
    let bash_fields = model.tool_fields("bash")?;
    ensure!(
        !bash_fields
            .iter()
            .any(|field| field == "name" || field == "ready"),
        "the session's bash tool can start a service: {bash_fields:?}"
    );
    let results = model.tool_results()?;
    let result = |step: usize| -> Result<(bool, String)> {
        results
            .iter()
            .find(|(id, _, _)| *id == format!("toolu_{step}"))
            .map(|(_, error, text)| (*error, text.clone()))
            .with_context(|| {
                format!("the session sent back no result for step {step}: {results:?}")
            })
    };
    for (step, what) in [
        (0, "read https://example.com"),
        (1, "grep https://example.com"),
    ] {
        let (error, text) = result(step)?;
        ensure!(
            error && text.contains("URL reads are disabled"),
            "`{what}` was not refused: {text}"
        );
    }
    for (step, what) in [
        (2, "read ssh://example.com/etc/hostname"),
        (3, "ctx a/../…/bin/sh"),
        (4, "curl"),
        (7, "ctx record put index.md /etc/hostname"),
    ] {
        let (error, text) = result(step)?;
        ensure!(
            error && text.contains("requires approval") && !text.contains("escaped-ctx"),
            "`{what}` was not stopped at the approval gate: {text}"
        );
    }
    let (_, text) = result(5)?;
    ensure!(
        text.contains("Tree rules") && !text.contains("requires approval"),
        "`ctx rules` did not run, so the probe above refuses everything and proves nothing: {text}"
    );
    let (_, text) = result(6)?;
    ensure!(
        text.contains("This index links every sample record")
            && !text.contains("requires approval"),
        "`ctx record show index.md` did not print the record: {text}"
    );
    Ok(())
}

/// The session writes no file, so a record reaches the server through `ctx_record_put`, the
/// tool runner/ctx-record.ts adds: the record goes to `ctx record put -- <path> -` on
/// standard input and no shell runs, so no text in it is refused or read as shell, nested
/// bullets, ` - ` and `reboot` among them. A stand-in `ctx` keeps the arguments and standard
/// input of each call; it refuses a record that holds `REFUSE`, as the server refuses one.
/// No model is called.
#[test]
#[ignore = "drives the omp release runner/omp.lock pins; CI runs it with CTX_TEST_OMP naming that binary"]
fn a_live_session_writes_records_through_the_ctx_record_tools() -> Result<()> {
    let omp = pinned_omp()?;
    let root = scratch("live-record")?;
    let calls = root.join("ctx-calls");
    let bin = root.join("bin");
    std::fs::create_dir_all(&calls)?;
    std::fs::create_dir_all(&bin)?;
    // One directory per call: one file per argument, and the standard input.
    write_executable(
        &bin.join("ctx"),
        &format!(
            "#!/bin/sh\ncall='{dir}'/$(ls '{dir}' | wc -l)\nmkdir \"$call\"\ni=0\nfor arg in \"$@\"; do\n  printf '%s' \"$arg\" > \"$call/$i\"\n  i=$((i + 1))\ndone\n[ \"$2\" = rm ] || cat > \"$call/stdin\"\nif grep -q REFUSE \"$call/stdin\" 2>/dev/null; then echo 'refused: the record holds REFUSE' >&2; exit 1; fi\necho stored\n",
            dir = calls.display()
        ),
    )?;
    let record = "---\ntype: decision\ntitle: The probe's record\n---\n# Heading\n\n\
        Line with `code`, $HOME, #hash, a | pipe, a > redirect, & and; (parens) \\ backslash.\n\
        - a top-level bullet\n  - one nested bullet\n* a starred bullet\n\
        Ada - CEO said we had to reboot the host; see -e and -c.\n\
        It's \"quoted\" and 'single-quoted'.\n";
    let model = FakeModel::start(vec![
        json!({"tool": "ctx_record_put", "input": {"path": "decisions/probe.md", "content": record}}),
        json!({"tool": "ctx_record_put", "input": {"path": "--help", "content": "REFUSE this one"}}),
        json!({"tool": "ctx_record_rm", "input": {"path": "decisions/probe.md"}}),
    ])?;
    let path = std::env::var("PATH").unwrap_or_default();
    let outcome = run_session(
        &omp,
        &root,
        &model,
        "Run the scripted probe.",
        &[("PATH", &format!("{}:{path}", bin.display()))],
        &[],
    );
    let mut received = Vec::new();
    for call in 0..std::fs::read_dir(&calls).map_or(0, Iterator::count) {
        let dir = calls.join(call.to_string());
        let mut arguments = Vec::new();
        while let Ok(argument) = std::fs::read_to_string(dir.join(arguments.len().to_string())) {
            arguments.push(argument);
        }
        let stdin = std::fs::read_to_string(dir.join("stdin")).ok();
        received.push((arguments, stdin));
    }
    remove_scratch(&root)?;
    outcome?;

    let results = model.tool_results()?;
    let result = |step: usize| -> Result<(bool, String)> {
        results
            .iter()
            .find(|(id, _, _)| *id == format!("toolu_{step}"))
            .map(|(_, error, text)| (*error, text.clone()))
            .with_context(|| {
                format!("the session sent back no result for step {step}: {results:?}")
            })
    };
    let words = |list: &[&str]| {
        list.iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let expected = vec![
        (
            words(&["record", "put", "--", "decisions/probe.md", "-"]),
            Some(record.to_owned()),
        ),
        (
            words(&["record", "put", "--", "--help", "-"]),
            Some("REFUSE this one".to_owned()),
        ),
        (words(&["record", "rm", "--", "decisions/probe.md"]), None),
    ];
    ensure!(
        received == expected,
        "ctx did not receive each call byte for byte: {received:?}"
    );
    let (error, text) = result(0)?;
    ensure!(!error && text.contains("stored"), "the put failed: {text}");
    let (error, text) = result(1)?;
    ensure!(
        error && text.contains("refused: the record holds REFUSE"),
        "a refused put did not fail with ctx's findings: {text}"
    );
    let (error, text) = result(2)?;
    ensure!(!error && text.contains("stored"), "the rm failed: {text}");
    Ok(())
}

/// The session writes no file: it holds no write or edit tool, and policy.yml denies every tool
/// that writes one. Each file that would make omp start a program is tried: an `lsp.json` in its
/// directory (by `edit` and by `write`), a stored model key that is a `!command` in omp's own
/// `agent.db` (the write tool's `file.db:table` form), `~/.bashrc`, and the shell snapshot
/// directory. Every call is refused, nothing lands, and nothing named runs; `ctx rules` still
/// runs, so the refusals are not the session failing everything. No model is called.
#[test]
#[ignore = "drives the omp release runner/omp.lock pins; CI runs it with CTX_TEST_OMP naming that binary"]
fn a_live_session_writes_no_file() -> Result<()> {
    let omp = pinned_omp()?;
    let root = scratch("live-nowrite")?;
    let token = format!("ctx-write-probe-{}", std::process::id());
    let marker = root.join(&token);
    let run = format!("touch '{}'", marker.display());
    let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?;
    let lsp = root.join("workspace/lsp.json");
    let agent_db = root.join("home/.omp/agent/agent.db");
    let bashrc = root.join("home/.bashrc");
    let snapshot = root.join(format!(
        "omp-shell-snapshots-{}/snapshot-bash-planted.sh",
        uid.trim()
    ));
    let config = json!({"servers": {"probe": {
        "command": "sh", "args": ["-c", &run], "fileTypes": [".md"], "rootMarkers": ["lsp.json"],
    }}})
    .to_string();
    let key = json!({"type": "api_key", "key": format!("!{run}")}).to_string();
    let row = json!({"provider": "openai", "credential_type": "api_key", "data": key}).to_string();
    let at = |path: &Path| path.display().to_string();
    let model = FakeModel::start_watching(
        vec![
            json!({"tool": "edit", "input": {"path": at(&lsp), "edits": [{"op": "create", "diff": config}]}}),
            json!({"tool": "write", "input": {"path": at(&lsp), "content": config}}),
            json!({"tool": "write", "input": {"path": format!("{}:auth_credentials", at(&agent_db)), "content": row}}),
            json!({"tool": "write", "input": {"path": at(&bashrc), "content": run}}),
            json!({"tool": "write", "input": {"path": at(&snapshot), "content": run}}),
            json!({"tool": "bash", "input": {"command": "ctx rules"}}),
        ],
        token.clone(),
    )?;
    let outcome = run_session(
        &omp,
        &root,
        &model,
        "Run the scripted probe.",
        &[("PI_EDIT_VARIANT", "patch")],
        // The write and edit tools, offered to the session although run-cycle.sh leaves them
        // out, so what refuses each call below is the policy's own deny and not their absence.
        &[
            "--tools",
            "bash,read,grep,glob,todo,write,edit,ctx_record_put,ctx_record_rm",
        ],
    );
    let landed: Vec<String> = [&lsp, &bashrc, &snapshot, &marker]
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| at(path))
        .collect();
    let mut stored = Vec::new();
    for suffix in ["", "-wal"] {
        stored.extend(std::fs::read(format!("{}{suffix}", at(&agent_db))).unwrap_or_default());
    }
    let key_stored = String::from_utf8_lossy(&stored).contains(&token);
    let sightings = model.sightings()?;
    remove_scratch(&root)?;
    outcome?;

    let results = model.tool_results()?;
    let result = |step: usize| -> Result<(bool, String)> {
        results
            .iter()
            .find(|(id, _, _)| *id == format!("toolu_{step}"))
            .map(|(_, error, text)| (*error, text.clone()))
            .with_context(|| {
                format!("the session sent back no result for step {step}: {results:?}")
            })
    };
    // Every write is checked before the test fails, so a failure names each one that went through.
    let mut open = Vec::new();
    for (step, what) in [
        (0, "edit-create lsp.json"),
        (1, "write lsp.json"),
        (2, "write a `!command` key into agent.db"),
        (3, "write ~/.bashrc"),
        (4, "write into the shell snapshot directory"),
    ] {
        let (error, text) = result(step)?;
        // The deny names itself, so a refusal for some other reason does not pass as the gate
        // holding (`tools/approval.ts:330-339`).
        if !error || !text.contains("blocked by user policy") {
            open.push(format!("`{what}` was not refused by the policy: {text}"));
        }
    }
    ensure!(
        open.is_empty() && landed.is_empty() && !key_stored && sightings.is_empty(),
        "the session wrote a file (landed: {landed:?}; key in agent.db: {key_stored}; \
         running: {sightings:?}): {open:#?}"
    );
    let (_, text) = result(5)?;
    ensure!(
        text.contains("Tree rules"),
        "`ctx rules` did not run, so the refusals above prove nothing: {text}"
    );
    Ok(())
}

/// omp starts every MCP server a configuration file in its working directory names, before the
/// session's first turn; policy.yml's `mcp.enableProjectConfig: false` drops those files. A
/// `.mcp.json` in the session's directory that names a program is not read: the program never
/// runs, while `ctx rules` still does. No model is called.
#[test]
#[ignore = "drives the omp release runner/omp.lock pins; CI runs it with CTX_TEST_OMP naming that binary"]
fn a_live_session_starts_no_mcp_server_its_directory_names() -> Result<()> {
    let omp = pinned_omp()?;
    let root = scratch("live-mcp")?;
    let token = format!("ctx-mcp-probe-{}", std::process::id());
    let marker = root.join(&token);
    std::fs::create_dir_all(root.join("workspace"))?;
    for name in [".mcp.json", "mcp.json"] {
        std::fs::write(
            root.join("workspace").join(name),
            json!({"mcpServers": {"probe": {
                "command": "sh", "args": ["-c", format!("touch '{}'; sleep 30", marker.display())],
            }}})
            .to_string(),
        )?;
    }
    let model = FakeModel::start_watching(
        vec![json!({"tool": "bash", "input": {"command": "ctx rules"}})],
        token.clone(),
    )?;
    let outcome = run_session(&omp, &root, &model, "Run the scripted probe.", &[], &[]);
    let started = marker.exists();
    let sightings = model.sightings()?;
    remove_scratch(&root)?;
    outcome?;

    ensure!(
        !started && sightings.is_empty(),
        "omp started the MCP server the session's directory names (running: {sightings:?})"
    );
    let results = model.tool_results()?;
    let text = results
        .iter()
        .find(|(id, _, _)| id == "toolu_0")
        .map(|(_, _, text)| text.clone())
        .with_context(|| format!("the session sent back no result: {results:?}"))?;
    ensure!(
        text.contains("Tree rules"),
        "`ctx rules` did not run, so the probe proves nothing: {text}"
    );
    Ok(())
}

/// A maintenance pass runs in the cycle's own session, under the prompt `run-cycle.sh maintain
/// waiting-on` gives it: the session reads the pass's instructions where the prompt sends it,
/// and every command those instructions name runs under the cycle's policy. The scripted model
/// reads the instructions, lists the open records, shows one and resolves a pointer with its
/// thread; none of them is stopped at the approval gate. No model is called.
#[test]
#[ignore = "drives the omp release runner/omp.lock pins; CI runs it with CTX_TEST_OMP naming that binary"]
fn a_live_maintenance_session_reads_its_pass_and_runs_its_commands() -> Result<()> {
    let omp = pinned_omp()?;
    let prompt = maintenance_prompt("waiting-on", "0123456789abcdef0123456789abcdef")?;
    let model = FakeModel::start(vec![
        json!({"tool": "read", "input": {"path": "skill://ctx-memory-ingest/maintenance/waiting-on.md"}}),
        json!({"tool": "bash", "input": {"command": "ctx open --json"}}),
        json!({"tool": "bash", "input": {"command": "ctx record show decisions/record-provenance.md"}}),
        json!({"tool": "bash", "input": {"command": "ctx resolve --thread slack:C0EXAMPLE01/1700000000.000001"}}),
    ])?;
    let root = scratch("live-maintain")?;
    let outcome = run_session(&omp, &root, &model, &prompt, &[], &[]);
    remove_scratch(&root)?;
    outcome?;

    let results = model.tool_results()?;
    let result = |step: usize| -> Result<(bool, String)> {
        results
            .iter()
            .find(|(id, _, _)| *id == format!("toolu_{step}"))
            .map(|(_, error, text)| (*error, text.clone()))
            .with_context(|| {
                format!("the session sent back no result for step {step}: {results:?}")
            })
    };
    let (error, text) = result(0)?;
    ensure!(
        !error && text.contains("Maintenance pass: waiting-on"),
        "the session could not read the pass's instructions where its prompt sends it: {text}"
    );
    let (error, text) = result(1)?;
    ensure!(
        !error && text.contains("waiting_on") && text.contains("record-provenance.md"),
        "`ctx open --json` did not list the open records with waiting_on: {text}"
    );
    let (error, text) = result(2)?;
    ensure!(
        !error && !text.contains("requires approval") && text.contains("type: decision"),
        "`ctx record show` did not print the record: {text}"
    );
    let (_, text) = result(3)?;
    ensure!(
        !text.contains("requires approval"),
        "`ctx resolve --thread` was stopped at the approval gate: {text}"
    );
    Ok(())
}
