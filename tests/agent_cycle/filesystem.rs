//! The files and directories `run-cycle.sh` checks before it hands the session to its user.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{ensure, Context as _, Result};

use crate::fixtures::{deploy, remove_scratch, repo, scratch, write_executable};

/// Stand-ins for what `run-cycle.sh` calls, so the script runs on a machine without the
/// image: `ctx` reports one pending window and `omp` records that it was started.
struct CycleRig {
    root: PathBuf,
    bin: PathBuf,
    data: PathBuf,
    omp_log: PathBuf,
}

impl CycleRig {
    fn new(label: &str) -> Result<Self> {
        let root = scratch(label)?;
        let bin = root.join("bin");
        let data = root.join("data");
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
            omp_log,
        })
    }

    /// Makes the script believe it started as root and that `ctx-agent` is this test's own
    /// user: `id` answers as root would, `setpriv` runs the command it is handed as this
    /// user, and `chown` changes nothing.
    fn pretend_root(&self) -> Result<()> {
        let gid = String::from_utf8(Command::new("id").arg("-g").output()?.stdout)?;
        write_executable(
            &self.bin.join("id"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n  -u) echo 0 ;;\n  -g) echo {} ;;\n  *) exit 0 ;;\nesac\n",
                gid.trim()
            ),
        )?;
        write_executable(
            &self.bin.join("setpriv"),
            "#!/bin/sh\nwhile [ \"${1#--}\" != \"$1\" ]; do shift; done\nexec \"$@\"\n",
        )?;
        write_executable(&self.bin.join("chown"), "#!/bin/sh\nexit 0\n")
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
            .arg(repo().join("deploy/agent/run-cycle.sh"))
            .current_dir(self.workspace())
            .env("PWD", self.workspace())
            .env("PATH", format!("{}:{path}", self.bin.display()))
            .env("HOME", self.home())
            .env("TMPDIR", self.root.join("tmp"))
            .env("CTX_DATA_DIR", &self.data)
            .env("CTX_SERVER", "http://127.0.0.1:9")
            .env("CTX_MODEL_BASE_URL", "http://127.0.0.1:9")
            .env("ANTHROPIC_API_KEY", "not-a-key")
            .env("CLOUD_RUN_EXECUTION", "ctx-agent-test-1")
            .env("XDG_DATA_HOME", self.xdg("data"))
            .env("XDG_STATE_HOME", self.xdg("state"))
            .env("XDG_CACHE_HOME", self.xdg("cache"));
        Ok(command)
    }

    fn run(&self) -> Result<std::process::Output> {
        self.command()?.output().context("running run-cycle.sh")
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

/// Without the model gateway's address the session would have nowhere its calls may go; the
/// cycle stops before anything else, rather than start one that calls Anthropic directly.
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
/// to root once omp exits, rather than handing its process to omp.
#[test]
fn the_cycle_seals_its_transcript_after_the_session() -> Result<()> {
    let script = deploy("run-cycle.sh")?;
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
        code.iter()
            .any(|line| line.contains("chown -R 0:0 \"$session_dir\"")),
        "run-cycle.sh never gives the session directory back to root"
    );
    ensure!(
        code.iter()
            .any(|line| line.starts_with("trap ") && line.contains("EXIT")),
        "run-cycle.sh seals the transcript only on its happy path"
    );
    Ok(())
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
