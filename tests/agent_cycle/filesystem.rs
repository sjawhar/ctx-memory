//! The files and directories `run-cycle.sh` checks before it hands the session to its user.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{ensure, Context as _, Result};

use crate::fixtures::{
    remove_scratch, repo, runner, scratch, script_function, write_executable, EXAMPLE_MODEL,
};

/// Stand-ins for what `run-cycle.sh` calls, so the script runs on a machine without the
/// image: `ctx` reports one pending window and `omp` records that it was started. The model
/// provider file is the runner's example, in a directory closed to the session's user, as an
/// image installs its own.
struct CycleRig {
    root: PathBuf,
    bin: PathBuf,
    data: PathBuf,
    models: PathBuf,
    omp_log: PathBuf,
}

impl CycleRig {
    fn new(label: &str) -> Result<Self> {
        let root = scratch(label)?;
        let bin = root.join("bin");
        let data = root.join("data");
        // The provider file's directories, this one among them, must be closed to group and
        // others, whatever the umask this test runs under made them.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
        std::fs::create_dir_all(&bin)?;
        std::fs::create_dir_all(data.join("windows/w1"))?;
        std::fs::create_dir_all(root.join("home/.omp/agent"))?;
        std::fs::create_dir_all(root.join("workspace"))?;
        std::fs::create_dir_all(root.join("tmp"))?;
        for dir in [
            "xdg/data/omp/plugins",
            "xdg/data/omp/natives",
            "xdg/state/omp",
            "xdg/cache/omp",
        ] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        // The shell's startup files, as the image keeps them: there, and closed to the
        // session's user.
        for file in [".bashrc", ".profile", ".bash_profile", ".bash_logout"] {
            let path = root.join("home").join(file);
            std::fs::write(&path, "")?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))?;
        }
        let models = root.join("opt/models.yml");
        std::fs::create_dir_all(root.join("opt"))?;
        std::fs::write(&models, runner("models.example.yml")?)?;
        std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o444))?;
        std::fs::set_permissions(root.join("opt"), std::fs::Permissions::from_mode(0o555))?;
        let omp_log = root.join("omp-started");
        write_executable(
            &bin.join("ctx"),
            "#!/bin/sh\nprintf '%s\\n' '{\"ok\":true,\"data\":{\"windows\":[{\"id\":\"w1\"}]}}'\n",
        )?;
        write_executable(
            &bin.join("omp"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
                omp_log.display()
            ),
        )?;
        Ok(Self {
            root,
            bin,
            data,
            models,
            omp_log,
        })
    }

    /// Makes the script believe it started as root and that `ctx-agent` is this test's own
    /// user: `id` answers as root would, `setpriv` runs the command it is handed as this
    /// user, and `chown` changes nothing. `id -u ctx-agent` answers what `agent-uid` holds,
    /// 65534 unless [`Self::set_agent_uid`] changed it: a uid that owns nothing here, so the
    /// files this test's user made are not the session's own.
    fn pretend_root(&self) -> Result<()> {
        let gid = String::from_utf8(Command::new("id").arg("-g").output()?.stdout)?;
        self.set_agent_uid("65534")?;
        write_executable(
            &self.bin.join("id"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n  -u) if [ -n \"${{2-}}\" ]; then cat '{}'; else echo 0; fi ;;\n  -g) echo {} ;;\n  *) exit 0 ;;\nesac\n",
                self.root.join("agent-uid").display(),
                gid.trim()
            ),
        )?;
        write_executable(
            &self.bin.join("setpriv"),
            "#!/bin/sh\nwhile [ \"${1#--}\" != \"$1\" ]; do shift; done\nexec \"$@\"\n",
        )?;
        write_executable(&self.bin.join("chown"), "#!/bin/sh\nexit 0\n")
    }

    /// What `id -u ctx-agent` answers from now on. A data file, not a rewritten stub, so no
    /// program is rewritten while another test may be starting one.
    fn set_agent_uid(&self, uid: &str) -> Result<()> {
        std::fs::write(self.root.join("agent-uid"), format!("{uid}\n"))?;
        Ok(())
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn xdg(&self, category: &str) -> PathBuf {
        self.root.join("xdg").join(category)
    }

    /// Closes the share to writes the way the deployment does, leaving only this cycle's
    /// transcript directory open.
    fn lock_share(&self) -> Result<()> {
        std::fs::create_dir_all(self.data.join("agent-sessions/ctx-agent-test-1"))?;
        for dir in ["windows/w1", "windows", "agent-sessions", ""] {
            std::fs::set_permissions(self.data.join(dir), std::fs::Permissions::from_mode(0o555))?;
        }
        Ok(())
    }

    /// Closes the home directory, omp's configuration in it and the working directory to
    /// writes, as the image does. models.yml is not there: `run-cycle.sh` writes it after its
    /// checks.
    fn lock_home(&self) -> Result<()> {
        let agent = self.home().join(".omp/agent");
        std::fs::write(agent.join("config.yml"), "{}\n")?;
        std::fs::set_permissions(
            agent.join("config.yml"),
            std::fs::Permissions::from_mode(0o444),
        )?;
        for dir in [
            agent,
            self.home().join(".omp"),
            self.home(),
            self.workspace(),
        ] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))?;
        }
        Ok(())
    }

    /// Closes omp's run-time directories and the plugin and native module directories in them.
    /// The test does not run as root, so it cannot give them to root with the sticky bit set;
    /// a directory closed to the session's user stands in for that.
    fn lock_xdg(&self) -> Result<()> {
        for dir in [
            self.xdg("data").join("omp/plugins"),
            self.xdg("data").join("omp/natives"),
            self.xdg("data").join("omp"),
            self.xdg("state").join("omp"),
            self.xdg("cache").join("omp"),
        ] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))?;
        }
        Ok(())
    }

    fn command(&self) -> Result<Command> {
        let path = std::env::var("PATH").context("PATH is not set")?;
        let mut command = Command::new("sh");
        command
            .arg(repo().join("runner/run-cycle.sh"))
            .current_dir(self.workspace())
            .env("PWD", self.workspace())
            .env("PATH", format!("{}:{path}", self.bin.display()))
            .env("HOME", self.home())
            .env("TMPDIR", self.root.join("tmp"))
            .env("CTX_DATA_DIR", &self.data)
            .env("CTX_SERVER", "http://127.0.0.1:9")
            .env("CTX_MODEL_BASE_URL", "http://127.0.0.1:9")
            .env("CTX_MODELS_FILE", &self.models)
            .env("CTX_MODEL", EXAMPLE_MODEL)
            .env("ANTHROPIC_API_KEY", "not-a-key")
            .env("CLOUD_RUN_EXECUTION", "ctx-agent-test-1")
            .env("XDG_DATA_HOME", self.xdg("data"))
            .env("XDG_STATE_HOME", self.xdg("state"))
            .env("XDG_CACHE_HOME", self.xdg("cache"));
        Ok(command)
    }

    /// Puts `text` in the model provider file and leaves it as the image keeps it: closed to
    /// writes, in a directory closed to writes.
    fn replace_models(&self, text: &str) -> Result<()> {
        let dir = self.root.join("opt");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        std::fs::remove_file(&self.models)?;
        std::fs::write(&self.models, text)?;
        std::fs::set_permissions(&self.models, std::fs::Permissions::from_mode(0o444))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555))?;
        Ok(())
    }

    fn run(&self) -> Result<std::process::Output> {
        self.command()?.output().context("running run-cycle.sh")
    }

    /// Makes `ctx` report no pending window and keep each call's arguments, one line per call,
    /// in the file this returns.
    fn nothing_pending(&self) -> Result<PathBuf> {
        let calls = self.root.join("ctx-calls");
        write_executable(
            &self.bin.join("ctx"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{{\"ok\":true,\"data\":{{\"windows\":[]}}}}'\n",
                calls.display()
            ),
        )?;
        Ok(calls)
    }

    fn omp_started(&self) -> bool {
        self.omp_log.exists()
    }

    fn remove(self) -> Result<()> {
        remove_scratch(&self.root)
    }
}

fn assert_refused(rig: &CycleRig, output: &std::process::Output, reason: &str) -> Result<()> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        !output.status.success(),
        "the cycle exited 0 although {reason}: {stderr}"
    );
    ensure!(
        !rig.omp_started(),
        "the cycle started its session although {reason}: {stderr}"
    );
    ensure!(
        stderr.contains(reason),
        "the cycle failed without saying that {reason}: {stderr}"
    );
    // The refusal is the check's own, before anything is prepared for a session: a cycle
    // that went on and failed later (writing the key, starting omp) merely got lucky.
    ensure!(
        !stderr.contains("/run/ctx-agent") && !stderr.contains("runs as root"),
        "the cycle went past its checks before it stopped: {stderr}"
    );
    Ok(())
}

/// A session that cannot be handed to `ctx-agent` does not run: not as root, and not as
/// whoever started the script.
#[test]
fn the_cycle_refuses_to_start_its_session_without_root_to_drop() -> Result<()> {
    let rig = CycleRig::new("not-root")?;
    let output = rig.run()?;
    let result = assert_refused(&rig, &output, "must start as root");
    rig.remove()?;
    result
}

/// Without the model API's address the session would have nowhere its calls may go; the cycle
/// stops before anything else, rather than start one that calls a model somewhere else.
#[test]
fn the_cycle_refuses_to_start_without_the_model_gateway_address() -> Result<()> {
    let rig = CycleRig::new("gateway-unset")?;
    rig.pretend_root()?;
    let unset = rig
        .command()?
        .env_remove("CTX_MODEL_BASE_URL")
        .output()
        .context("running run-cycle.sh")?;
    let empty = rig
        .command()?
        .env("CTX_MODEL_BASE_URL", "")
        .output()
        .context("running run-cycle.sh")?;
    let result = assert_refused(&rig, &unset, "CTX_MODEL_BASE_URL must")
        .and_then(|()| assert_refused(&rig, &empty, "CTX_MODEL_BASE_URL must"));
    rig.remove()?;
    result
}

/// The deployment names the model provider file and the model the session selects; the runner
/// has no default for either, so a cycle without them stops before anything else.
#[test]
fn the_cycle_refuses_to_start_without_its_model_provider_and_model() -> Result<()> {
    let rig = CycleRig::new("model-unset")?;
    rig.pretend_root()?;
    let mut result = Ok(());
    for variable in ["CTX_MODELS_FILE", "CTX_MODEL"] {
        let unset = rig
            .command()?
            .env_remove(variable)
            .output()
            .context("running run-cycle.sh")?;
        let empty = rig
            .command()?
            .env(variable, "")
            .output()
            .context("running run-cycle.sh")?;
        let reason = format!("{variable} must");
        result = result
            .and_then(|()| assert_refused(&rig, &unset, &reason))
            .and_then(|()| assert_refused(&rig, &empty, &reason))
            .with_context(|| format!("{variable} unset or empty"));
    }
    rig.remove()?;
    result
}

/// root copies the provider file into the session's HOME before every session, so a file the
/// session's user could change is one an earlier session could have pointed at another address,
/// with the next session's key. Each way to change it is refused: the file the session's own (its
/// owner can give itself the write it lacks), the file or a directory above it writable by group
/// or others, the path running through a symbolic link, whose target and its directories are not
/// the ones checked, and a path that is not absolute. A file that is not there is refused too.
#[test]
fn the_cycle_refuses_a_model_provider_file_the_session_user_can_change() -> Result<()> {
    let rig = CycleRig::new("models-writable")?;
    let dir = rig.root.join("opt");
    let set_mode = |path: &std::path::Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    };
    rig.pretend_root()?;
    let own_uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?;
    rig.set_agent_uid(own_uid.trim())?;
    let owned = rig.run()?;
    rig.set_agent_uid("65534")?;
    set_mode(&rig.models, 0o464)?;
    let group_writable = rig.run()?;
    set_mode(&rig.models, 0o444)?;
    set_mode(&dir, 0o557)?;
    let directory = rig.run()?;
    set_mode(&dir, 0o755)?;
    let elsewhere = rig.root.join("elsewhere.yml");
    std::fs::write(&elsewhere, runner("models.example.yml")?)?;
    set_mode(&elsewhere, 0o444)?;
    let link = dir.join("link.yml");
    std::os::unix::fs::symlink(&elsewhere, &link)?;
    set_mode(&dir, 0o555)?;
    let linked = rig
        .command()?
        .env("CTX_MODELS_FILE", &link)
        .output()
        .context("running run-cycle.sh")?;
    let relative = rig
        .command()?
        .env("CTX_MODELS_FILE", "opt/models.yml")
        .current_dir(&rig.root)
        .output()
        .context("running run-cycle.sh")?;
    let missing = rig
        .command()?
        .env("CTX_MODELS_FILE", dir.join("absent.yml"))
        .output()
        .context("running run-cycle.sh")?;
    let unchanged = rig.run()?;
    let result = assert_refused(&rig, &owned, &format!("owns {}", rig.models.display()))
        .context("a file the session's user owns")
        .and_then(|()| {
            assert_refused(
                &rig,
                &group_writable,
                &format!("can rewrite {}, on the way", rig.models.display()),
            )
            .context("a group-writable file")
        })
        .and_then(|()| {
            assert_refused(
                &rig,
                &directory,
                &format!("can rewrite {}, on the way", dir.display()),
            )
            .context("a directory others can write")
        })
        .and_then(|()| assert_refused(&rig, &linked, "no symbolic link").context("a symbolic link"))
        .and_then(|()| assert_refused(&rig, &relative, "absolute path").context("a relative path"))
        .and_then(|()| {
            assert_refused(&rig, &missing, "CTX_MODELS_FILE must name a file")
                .context("a missing file")
        })
        .and_then(|()| {
            // With the file closed again, the cycle goes on to its next check, so each
            // refusal above was the provider file's.
            assert_refused(&rig, &unchanged, "can write").context("the closed file")
        });
    rig.remove()?;
    result
}

/// The provider file is the deployment's, so the runner checks what its guarantees rest on: one
/// address, the placeholder, so every call goes to `CTX_MODEL_BASE_URL`, even one to a model
/// that names its own; and one key, read from the file only the session's user can read, never a
/// key written into the provider file, which the session could read, nor that file's key handed
/// to a second provider or header.
#[test]
fn the_cycle_refuses_a_model_provider_file_that_routes_a_call_or_a_key_elsewhere() -> Result<()> {
    let rig = CycleRig::new("models-shape")?;
    rig.pretend_root()?;
    let example = runner("models.example.yml")?;
    let placeholder = "    baseUrl: \"@CTX_MODEL_BASE_URL@\"\n";
    let key = "    apiKey: \"!cat /run/ctx-agent/model-key\"\n";
    let model = "      - {id: example-model,";
    ensure!(
        example.matches(placeholder).count() == 1
            && example.matches(key).count() == 1
            && example.matches(model).count() == 1,
        "runner/models.example.yml does not hold the lines this test changes"
    );
    let mut result = Ok(());
    for (label, text, reason) in [
        (
            "no placeholder",
            example.replace(placeholder, "    baseUrl: \"https://elsewhere.example\"\n"),
            "CTX_MODELS_FILE must hold one address",
        ),
        (
            "two placeholders",
            example.replace(placeholder, &format!("{placeholder}{placeholder}")),
            "CTX_MODELS_FILE must hold one address",
        ),
        (
            "a model's own address",
            example.replace(
                model,
                "      - {baseUrl: \"https://elsewhere.example\", id: example-model,",
            ),
            "CTX_MODELS_FILE must hold one address",
        ),
        (
            "a key in the file",
            example.replace(key, "    apiKey: \"sk-written-into-the-file\"\n"),
            "CTX_MODELS_FILE must take its one apiKey",
        ),
        (
            "the key file in a header too",
            example.replace(
                key,
                &format!("{key}    headers: {{x-key: \"!cat /run/ctx-agent/model-key\"}}\n"),
            ),
            "CTX_MODELS_FILE must take its one apiKey",
        ),
    ] {
        rig.replace_models(&text)?;
        let output = rig.run()?;
        result = result.and_then(|()| {
            assert_refused(&rig, &output, reason).with_context(|| format!("a file with {label}"))
        });
    }
    rig.remove()?;
    result
}

/// The address is written into the models.yml the session reads, so only an http or https origin
/// is taken: a value with no scheme, another scheme, a user, a path, a query, a port that is not
/// one run of digits, or a character that would end the YAML string or the substitution that
/// writes it is refused before anything else. An origin, with or without a port or a final
/// slash, goes on to the next check.
#[test]
fn the_cycle_takes_only_an_http_origin_as_the_model_gateway_address() -> Result<()> {
    let rig = CycleRig::new("gateway-malformed")?;
    let mut result = Ok(());
    for wrong in [
        "gateway.example.internal:4000",
        "ftp://gateway.example.internal:4000",
        "http://",
        "http://:4000",
        "http://user@gateway.example.internal:4000",
        "http://gateway.example.internal:4000/v1",
        "http://gateway.example.internal:4000?x=1",
        "http://gateway.example.internal:4000\"",
        "http://gateway.example.internal|x",
        "http://gateway.example.internal:4000\nhttp://elsewhere",
        "http://gateway.example.internal:",
        "http://gateway.example.internal::4000",
        "http://gateway.example.internal:4000:4001",
        "http://gateway.example.internal:port",
    ] {
        let output = rig
            .command()?
            .env("CTX_MODEL_BASE_URL", wrong)
            .output()
            .context("running run-cycle.sh")?;
        result = result.and_then(|()| {
            assert_refused(&rig, &output, "CTX_MODEL_BASE_URL must be")
                .with_context(|| format!("CTX_MODEL_BASE_URL {wrong:?}"))
        });
    }
    for origin in [
        "http://gateway.example.internal:4000",
        "https://gateway.example.com",
        "http://127.0.0.1:4000/",
    ] {
        let output = rig
            .command()?
            .env("CTX_MODEL_BASE_URL", origin)
            .output()
            .context("running run-cycle.sh")?;
        result = result.and_then(|()| {
            assert_refused(&rig, &output, "must start as root")
                .with_context(|| format!("CTX_MODEL_BASE_URL {origin:?}"))
        });
    }
    rig.remove()?;
    result
}

#[test]
fn the_cycle_refuses_a_session_user_that_cannot_read_the_windows() -> Result<()> {
    let rig = CycleRig::new("unreadable")?;
    rig.pretend_root()?;
    std::fs::set_permissions(
        rig.data.join("windows"),
        std::fs::Permissions::from_mode(0o000),
    )?;
    let output = rig.run()?;
    let result = assert_refused(&rig, &output, "cannot read");
    rig.remove()?;
    result
}

/// The session reads windows that outside parties wrote; if it could also write the share,
/// one obeyed instruction could move `cursors.json` past material never stored or rewrite
/// a window.
#[test]
fn the_cycle_refuses_a_session_user_that_can_write_the_share() -> Result<()> {
    let rig = CycleRig::new("writable")?;
    rig.pretend_root()?;
    let output = rig.run()?;
    let result = assert_refused(&rig, &output, "can write");
    rig.remove()?;
    result
}

/// The session's transcript is the record of what it did; the script stays to give it back
/// to root once omp exits, rather than handing its process to omp, and seals it however it exits.
#[test]
fn the_cycle_seals_its_transcript_after_the_session() -> Result<()> {
    let script = runner("run-cycle.sh")?;
    let code: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    ensure!(
        !code.iter().any(|line| line.starts_with("exec ")),
        "run-cycle.sh still execs, so nothing runs after the session to seal its transcript"
    );
    ensure!(
        script_function("seal_session")?.contains("chown -R 0:0 \"$1\""),
        "run-cycle.sh never gives the session directory back to root"
    );
    ensure!(
        code.iter().any(|line| line.starts_with("trap ")
            && line.contains("EXIT")
            && line.contains("seal_session \"$session_dir\"")),
        "run-cycle.sh seals the transcript only on its happy path"
    );
    Ok(())
}

/// Sealing a transcript directory moves the capture of omp's stdout beside it into it, so the
/// whole event stream is kept with the transcript, root's, and only there. A capture an earlier
/// attempt of the same execution left in the directory is kept beside the new one, and a name the
/// session left there, a link among them, is never written through: the capture takes the next
/// free name. A capture that cannot be moved fails the seal, says so, and stays where it was.
#[test]
fn sealing_moves_the_event_capture_into_the_transcript() -> Result<()> {
    let root = scratch("seal-capture")?;
    let dir = root.join("agent-sessions/ctx-agent-test-1");
    std::fs::create_dir_all(&dir)?;
    let capture = root.join("agent-sessions/ctx-agent-test-1.events.json");
    let elsewhere = root.join("elsewhere");
    let seal = |events: &str, stubs: &str| -> Result<std::process::Output> {
        std::fs::write(&capture, events)?;
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                "set -eu\nchown() {{ :; }}\n{stubs}\n{}\nseal_session \"$1\"",
                script_function("seal_session")?
            ))
            .arg("seal")
            .arg(&dir)
            .output()
            .context("running seal_session")
    };
    let first = seal("{\"type\":\"agent_start\"}\n", "")?;
    let second = seal("{\"type\":\"agent_end\"}\n", "")?;
    std::os::unix::fs::symlink(&elsewhere, dir.join("events.2.jsonl"))?;
    let third = seal("{\"type\":\"session\"}\n", "")?;
    let capture_left = capture.exists();
    let unmoved = seal("{\"type\":\"turn_end\"}\n", "mv() { return 1; }")?;
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    let kept = (
        read("events.jsonl"),
        read("events.1.jsonl"),
        read("events.3.jsonl"),
    );
    let stayed = std::fs::read_to_string(&capture).unwrap_or_default();
    let elsewhere_written = elsewhere.exists();
    remove_scratch(&root)?;

    for (label, output) in [("first", &first), ("second", &second), ("third", &third)] {
        ensure!(
            output.status.success(),
            "sealing failed the {label} time: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    ensure!(
        kept == (
            "{\"type\":\"agent_start\"}\n".to_owned(),
            "{\"type\":\"agent_end\"}\n".to_owned(),
            "{\"type\":\"session\"}\n".to_owned()
        ),
        "the transcript does not keep each capture under its own name: {kept:?}"
    );
    ensure!(!capture_left, "the capture stayed beside the transcript");
    ensure!(
        !elsewhere_written,
        "sealing wrote through a link the session left in its directory"
    );
    ensure!(
        !unmoved.status.success()
            && String::from_utf8_lossy(&unmoved.stderr).contains("run-cycle: could not move")
            && stayed == "{\"type\":\"turn_end\"}\n",
        "a capture that could not be moved did not fail the seal, say so and stay: {}",
        String::from_utf8_lossy(&unmoved.stderr)
    );
    Ok(())
}

/// A refused ingest cycle or maintenance pass gets one fresh session. `run-cycle.sh` runs as its
/// container's PID 1 and cannot hand its own process to omp, so the retry's structure is read from
/// the script, the way the transcript sealing is: a run that reaches omp needs a locked share this
/// pretend-root harness cannot build. What the retry decides is run in `outcome.rs`. The
/// invariants the retry rests on: omp is started from a function, so the same launch serves both
/// attempts; the retry fires only on a non-zero exit, only when the session's own transcript shows
/// a model refusal, and never for a pass that moved the marker; it starts over in its own
/// directory; and it is one retry, not a loop, so a second refusal is the outcome.
#[test]
fn the_cycle_retries_once_after_a_model_refusal() -> Result<()> {
    let script = runner("run-cycle.sh")?;
    let code: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    let has = |needle: &str| code.iter().any(|line| line.contains(needle));

    ensure!(
        has("run_session() {"),
        "run-cycle.sh starts omp inline, so the retry has no launch to run a second time"
    );
    ensure!(
        code.iter()
            .filter(|line| line.contains("run_session"))
            .count()
            >= 3,
        "run-cycle.sh does not call run_session for both attempts"
    );
    ensure!(
        code.iter()
            .filter(|line| line.contains("run_session || status="))
            .count()
            == 2,
        "run-cycle.sh must run exactly two sessions at most: the cycle and its one retry"
    );
    ensure!(
        has("session_refused() {") && has("stopDetails") && has("refusal"),
        "run-cycle.sh does not read the refusal by its stopDetails"
    );
    ensure!(
        has("\"$@\" >\"$event_log\" &") && has("event_log=\"$session_dir.events.json\""),
        "run-cycle.sh does not capture omp's stdout, where the runtime emits a refusal, to a file \
         beside the sealed transcript"
    );
    ensure!(
        !has("cat \"$event_log\""),
        "run-cycle.sh writes the whole captured stdout to the job log, where Cloud Logging drops \
         events over its line limit and in bursts, rather than its summary"
    );
    let retry = code
        .iter()
        .find(|line| {
            line.contains("session_refused \"$session_dir.events.json\"")
                && line.contains("[ -z \"$marker_moved\" ]")
        })
        .context("the retry is not guarded by a refused session that left the marker alone")?;
    ensure!(
        retry.contains("\"$status\" -ne 0"),
        "the retry runs even when the first session succeeded"
    );
    ensure!(
        has("-retry\"")
            && has("session_dir=\"$CTX_DATA_DIR/agent-sessions/$CLOUD_RUN_EXECUTION-retry\""),
        "the retry does not open its own transcript directory"
    );
    Ok(())
}

/// The pattern `run-cycle.sh` reads a refusal by, run against the real shape of the event. The
/// pinned runtime writes the refusal to its stdout as a `stopDetails` object whose `type` is
/// `"refusal"`, after its `category` and `explanation`; an earlier pattern that expected `type`
/// first matched none. The pattern must also not fire on a window's own copy of such an event,
/// which reaches a tool result JSON-escaped. The pattern is read from the script so the test
/// cannot drift from it.
#[test]
fn the_refusal_pattern_matches_a_real_refusal_and_not_an_escaped_copy() -> Result<()> {
    let script = runner("run-cycle.sh")?;
    let line = script
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("grep -qE '") && line.ends_with("\"$1\""))
        .context("session_refused does not grep the captured stdout with one pattern")?;
    let pattern = line
        .strip_prefix("grep -qE '")
        .and_then(|rest| rest.rsplit_once("' "))
        .map(|(pattern, _)| pattern)
        .context("could not read the refusal pattern from run-cycle.sh")?;

    // A real refusal event: category and explanation precede type, and the explanation even holds
    // a brace, so a pattern that stops at the first `}` would miss it.
    let refusal = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","stopDetails":{"category":"reasoning_extraction","explanation":"blocked (see terms) }","type":"refusal"}}}"#;
    // The same text as a window quoted it: a tool result, JSON-escaped, so the real quotes and
    // brace the pattern needs are not there.
    let escaped = r#"{"type":"message_end","message":{"role":"toolResult","content":[{"text":"...\"stopDetails\":{\"category\":\"x\",\"type\":\"refusal\"}..."}]}}"#;
    let benign = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop","stopDetails":null}}"#;

    let dir = scratch("refusal-pattern")?;
    let file = dir.join("events.json");
    let matches = |content: &str| -> Result<bool> {
        std::fs::write(&file, content)?;
        Ok(std::process::Command::new("grep")
            .args(["-qE", pattern])
            .arg(&file)
            .status()?
            .success())
    };
    let refusal_matched = matches(refusal)?;
    let escaped_matched = matches(escaped)?;
    let benign_matched = matches(benign)?;
    remove_scratch(&dir)?;

    ensure!(
        refusal_matched,
        "the refusal pattern misses a real refusal event: {pattern}"
    );
    ensure!(
        !escaped_matched,
        "the refusal pattern fires on a window's escaped copy of a refusal: {pattern}"
    );
    ensure!(
        !benign_matched,
        "the refusal pattern fires on a non-refusal stop: {pattern}"
    );
    Ok(())
}

/// A maintenance pass handles no window and moves no marker; `run-cycle.sh` enforces that rather
/// than trusting the pass's instructions. Read from the script, like the retry: the enforcement
/// runs after the session, which this pretend-root harness cannot reach (it writes the model key
/// to `/run/ctx-agent`, root's). The invariants: the marker is read before and after a pass, the
/// two are compared, a move fails the job, and all of it is a pass alone, so a cycle (which moves
/// the marker per window) is untouched.
#[test]
fn the_maintenance_pass_fails_if_it_moved_the_marker() -> Result<()> {
    let script = runner("run-cycle.sh")?;
    let code: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    let has = |needle: &str| code.iter().any(|line| line.contains(needle));

    ensure!(
        code.iter()
            .filter(|line| line.contains("ctx marker show --json"))
            .count()
            == 2,
        "run-cycle.sh must read the marker before and after the pass to tell whether it moved"
    );
    ensure!(
        has("marker_before=\"$(ctx marker show --json)\"")
            && has("marker_after=\"$(ctx marker show --json)\""),
        "run-cycle.sh does not capture the marker on each side of the pass"
    );
    let compared = code
        .iter()
        .find(|line| line.contains("\"$marker_before\" != \"$marker_after\""))
        .context("run-cycle.sh never compares the marker before and after the pass")?;
    let moved = code
        .iter()
        .position(|line| line == compared)
        .context("locating the marker comparison")?;
    ensure!(
        code.get(moved + 1..moved + 3)
            .is_some_and(|lines| lines.iter().any(|line| line.contains("status=1"))),
        "a pass that moved the marker does not fail the job"
    );
    ensure!(
        code.iter()
            .filter(|line| *line == &"if [ -n \"$pass\" ]; then")
            .count()
            >= 2,
        "the marker reads are not both guarded by a maintenance pass"
    );
    Ok(())
}

/// A cycle told a window cap passes it to its own `ctx pending`, so the runner's read is the
/// size the session's will be and a long backlog is never read whole. With nothing pending the
/// cycle stops before any session, so this reaches the pending read and no further.
#[test]
fn the_cycle_passes_its_window_cap_to_ctx_pending() -> Result<()> {
    let rig = CycleRig::new("window-cap")?;
    rig.pretend_root()?;
    let calls = rig.nothing_pending()?;
    let output = rig
        .command()?
        .env("CTX_CYCLE_MAX_WINDOWS", "3")
        .output()
        .context("running run-cycle.sh with a window cap")?;
    let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
    rig.remove()?;
    ensure!(
        output.status.success(),
        "a capped cycle with nothing pending did not stop cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        recorded
            .lines()
            .any(|line| line.starts_with("pending") && line.contains("--limit 3")),
        "the cycle did not pass its cap to ctx pending: {recorded:?}"
    );
    Ok(())
}

/// The cap is optional and a positive whole number of windows: anything else is refused before
/// anything is prepared for a session, the way a missing variable is.
#[test]
fn the_runner_refuses_a_window_cap_that_is_not_a_positive_number() -> Result<()> {
    let rig = CycleRig::new("window-cap-bad")?;
    rig.pretend_root()?;
    rig.nothing_pending()?;
    let mut result = Ok(());
    for value in ["0", "00", "000", "08", "-1", "3.5", "lots", "3 4"] {
        let output = rig
            .command()?
            .env("CTX_CYCLE_MAX_WINDOWS", value)
            .output()
            .context("running run-cycle.sh with a bad window cap")?;
        result = assert_refused(&rig, &output, "CTX_CYCLE_MAX_WINDOWS")
            .with_context(|| format!("CTX_CYCLE_MAX_WINDOWS={value:?}"));
        if result.is_err() {
            break;
        }
    }
    rig.remove()?;
    result
}

#[test]
fn the_cycle_refuses_a_session_user_that_can_rewrite_omps_configuration() -> Result<()> {
    let rig = CycleRig::new("config-writable")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    std::fs::write(rig.home().join(".omp/agent/config.yml"), "{}\n")?;
    let output = rig.run()?;
    let result = assert_refused(&rig, &output, "can rewrite");
    rig.remove()?;
    result
}

/// omp discovers custom tools (`~/.omp/agent/tools`) and MCP servers (`~/.omp/agent/mcp.json`)
/// beside its configuration, so a session's user that can add a file there can name a program
/// omp starts, even when it cannot rewrite `config.yml`.
#[test]
fn the_cycle_refuses_a_session_user_that_can_add_files_beside_omps_configuration() -> Result<()> {
    let rig = CycleRig::new("config-replaceable")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    let path = rig.home().join(".omp/agent/config.yml");
    std::fs::write(&path, "{}\n")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))?;
    let output = rig.run()?;
    let agent = rig.home().join(".omp/agent");
    let result = assert_refused(&rig, &output, &format!("{} ", agent.display()))
        .and_then(|()| assert_refused(&rig, &output, "can write"));
    rig.remove()?;
    result
}

/// omp discovers custom tools (`.omp/tools`) and MCP servers (`.mcp.json`) in its working
/// directory too.
#[test]
fn the_cycle_refuses_a_session_user_that_can_write_its_working_directory() -> Result<()> {
    let rig = CycleRig::new("workspace-writable")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    rig.lock_home()?;
    std::fs::set_permissions(rig.workspace(), std::fs::Permissions::from_mode(0o755))?;
    rig.lock_xdg()?;
    let output = rig.run()?;
    let result = assert_refused(
        &rig,
        &output,
        &format!("can write {} where omp", rig.workspace().display()),
    );
    rig.remove()?;
    result
}

/// omp loads tools from `node_modules` in its plugin directory, which lives in its run-time data
/// directory, the one place the session's user may add files.
#[test]
fn the_cycle_refuses_a_session_user_that_can_write_omps_plugin_directory() -> Result<()> {
    let rig = CycleRig::new("plugins-writable")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    rig.lock_home()?;
    rig.lock_xdg()?;
    let plugins = rig.xdg("data").join("omp/plugins");
    std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o755))?;
    let output = rig.run()?;
    let result = assert_refused(
        &rig,
        &output,
        &format!("can write {} from which omp loads code", plugins.display()),
    );
    rig.remove()?;
    result
}

/// Without the XDG directories omp keeps what it writes in HOME, which is closed to the
/// session, so the session would fail; and a missing directory there is one the session's user
/// could make its own.
#[test]
fn the_cycle_refuses_to_start_without_omps_run_time_directories() -> Result<()> {
    let rig = CycleRig::new("xdg-missing")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    rig.lock_home()?;
    let output = rig
        .command()?
        .env_remove("XDG_STATE_HOME")
        .output()
        .context("running run-cycle.sh")?;
    let unset = assert_refused(&rig, &output, "XDG_STATE_HOME");
    let natives = rig.xdg("data").join("omp/natives");
    std::fs::remove_dir(&natives)?;
    let output = rig.run()?;
    let missing = assert_refused(&rig, &output, &format!("{} is missing", natives.display()));
    rig.remove()?;
    unset.and(missing)
}

/// omp sources `~/.bashrc` when it builds its shell snapshot, so a startup file the session's
/// user can rewrite is a program it can name.
#[test]
fn the_cycle_refuses_a_session_user_that_can_rewrite_a_shell_startup_file() -> Result<()> {
    let rig = CycleRig::new("bashrc-writable")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    let bashrc = rig.home().join(".bashrc");
    std::fs::set_permissions(&bashrc, std::fs::Permissions::from_mode(0o644))?;
    let output = rig.run()?;
    let result = assert_refused(&rig, &output, &format!("can rewrite {}", bashrc.display()));
    rig.remove()?;
    result
}

/// A startup file that is missing is one the session's user could write itself, were the home
/// directory ever open to it.
#[test]
fn the_cycle_refuses_a_home_without_its_shell_startup_files() -> Result<()> {
    let rig = CycleRig::new("profile-missing")?;
    rig.pretend_root()?;
    rig.lock_share()?;
    for file in [".profile", ".bash_profile"] {
        let profile = rig.home().join(file);
        std::fs::remove_file(&profile)?;
        let output = rig.run()?;
        let result = assert_refused(&rig, &output, &format!("{} is missing", profile.display()));
        if result.is_err() {
            rig.remove()?;
            return result;
        }
        std::fs::write(&profile, "")?;
        std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o444))?;
    }
    rig.remove()
}

/// A cycle with no pending window has nothing to do and exits 0 before any session. A
/// maintenance pass works over the whole tree, not over windows: it does not ask what is
/// pending and goes on to the same checks a cycle's session must pass, here the first one,
/// which refuses a share the session's user can write.
#[test]
fn a_maintenance_pass_runs_whether_or_not_a_window_is_pending() -> Result<()> {
    let rig = CycleRig::new("maintain-nothing-pending")?;
    rig.pretend_root()?;
    let calls = rig.nothing_pending()?;
    let cycle = rig.run()?;
    let cycle_calls = std::fs::read_to_string(&calls).unwrap_or_default();
    std::fs::remove_file(&calls)?;
    let pass = rig
        .command()?
        .args(["maintain", "waiting-on"])
        .output()
        .context("running run-cycle.sh maintain waiting-on")?;
    let pass_calls = std::fs::read_to_string(&calls).unwrap_or_default();
    let pass_refusal = assert_refused(&rig, &pass, "can write");
    rig.remove()?;

    let cycle_out = String::from_utf8_lossy(&cycle.stdout);
    ensure!(
        cycle.status.success() && cycle_out.contains("no pending windows"),
        "a cycle with nothing pending did not stop at once: {cycle_out}{}",
        String::from_utf8_lossy(&cycle.stderr)
    );
    ensure!(
        cycle_calls.lines().any(|line| line.starts_with("pending")),
        "the cycle did not ask what is pending: {cycle_calls:?}"
    );
    pass_refusal?;
    ensure!(
        pass_calls.is_empty(),
        "the maintenance pass ran ctx before its session: {pass_calls:?}"
    );
    Ok(())
}

/// A maintenance pass is named, and only a pass the runner knows runs: an argument never
/// names a file. Anything else is refused before anything is prepared for a session.
#[test]
fn the_runner_refuses_a_maintenance_pass_it_does_not_know() -> Result<()> {
    let rig = CycleRig::new("maintain-unknown")?;
    rig.pretend_root()?;
    let mut result = Ok(());
    for (arguments, reason) in [
        (&["maintain", "nonesuch"][..], "no maintenance pass named"),
        (
            &["maintain", "../../etc/passwd"][..],
            "no maintenance pass named",
        ),
        (&["maintain", ""][..], "no maintenance pass named"),
        (
            &["maintain", "waiting-on waiting-on"][..],
            "no maintenance pass named",
        ),
        (&["maintain"][..], "usage"),
        (&["maintain", "waiting-on", "again"][..], "usage"),
        (&["waiting-on"][..], "usage"),
    ] {
        let output = rig
            .command()?
            .args(arguments)
            .output()
            .context("running run-cycle.sh")?;
        result = assert_refused(&rig, &output, reason)
            .with_context(|| format!("run-cycle.sh {arguments:?}"));
        if result.is_err() {
            break;
        }
    }
    rig.remove()?;
    result
}
