//! The git clone the server reads and writes.
//!
//! Records live in a git repository, so `ctx` drives the `git` command itself rather than
//! holding a second idea of what a commit is. Every call names the repository explicitly, so
//! no command depends on the process working directory.
//!
//! The graph repository is private, so every call to the remote — clone, fetch, pull and push
//! alike — carries a GitHub App installation token. It travels in git's environment as an
//! `http.extraheader`, never in the remote URL and never in a command line, so no other
//! process on the box reads it out of `/proc`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use crate::api::github;
use crate::credentials;
use crate::proc;

/// Names the remote a server or fetch container clones its tree from.
pub const REMOTE_ENV: &str = "CTX_MEMORY_REMOTE";

/// The file, in a clone's git directory, that says `ctx` made the clone itself from
/// [`REMOTE_ENV`]. It holds the remote's URL. git never commits or shows it.
const CLONE_MARK: &str = "ctx-memory-clone";

/// How long one call to the remote — a fetch, a pull or a push — may take before git is
/// stopped.
///
/// The write lease lets another copy take over 30 seconds after the last check this copy
/// passed, and the writer checks the lease again right before each push. A push bounded to
/// less than that lands, if it lands at all, while this copy is still the one writer.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(25);

/// How long a first clone may take. It carries the whole history, and no lease waits on it.
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);

/// What a credential has to let git do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Clone, fetch and pull.
    Read,
    /// Everything a read does, and push.
    Write,
}

/// What git sends to prove it may read or write a remote.
///
/// The value is an `Authorization` header git adds to every HTTP request. It is kept out of
/// the remote URL, so it never reaches `.git/config`, a reflog or an error message, and out
/// of argv, so it is not readable from `/proc/<pid>/cmdline`.
#[derive(Clone)]
pub struct Credential {
    minted: github::InstallationToken,
    header: String,
}

impl Credential {
    /// Wraps one GitHub installation token as the header git sends.
    #[must_use]
    fn github(minted: github::InstallationToken) -> Self {
        let header = format!(
            "Authorization: Basic {}",
            STANDARD.encode(format!("x-access-token:{}", minted.token))
        );
        Self { minted, header }
    }

    /// Puts the credential in git's environment for one command.
    fn apply(&self, command: &mut Command) {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraheader")
            .env("GIT_CONFIG_VALUE_0", &self.header);
    }

    /// Text git printed, with the token taken out.
    ///
    /// git quotes a URL it was given back in its errors, and those errors reach a caller of
    /// the server as the body of an answer. Nothing a caller reads carries the token.
    fn redact(&self, text: &str) -> String {
        text.replace(&self.minted.token, "[redacted]")
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Credential(github installation token)")
    }
}

/// The credentials minted so far, one per remote and access level, kept until they are nearly
/// expired. A copy of the server pulls every minute, and a token lives an hour: minting one
/// per git call would cost two GitHub API calls a minute for nothing.
static MINTED: LazyLock<Mutex<BTreeMap<(String, Access), Credential>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// The credential a remote needs, or `None` when it needs none.
///
/// A remote that is not GitHub, and a machine with no app key, keep whatever git itself is
/// configured with. A token is only ever sent over HTTPS, or to a loopback address, which a
/// test's own GitHub runs on and which no network carries.
///
/// # Errors
///
/// Returns an error when GitHub refuses the installation lookup or the mint.
pub fn credential_for(
    url: &str,
    config: &credentials::Config,
    access: Access,
) -> Result<Option<Credential>> {
    if !carries_a_token(url) {
        return Ok(None);
    }
    let Some((owner, repository)) = github::repository_of(url) else {
        return Ok(None);
    };
    let key = (url.to_owned(), access);
    {
        let held = MINTED
            .lock()
            .map_err(|_| anyhow!("the git credential cache was poisoned"))?;
        if let Some(credential) = held.get(&key).filter(|held| held.minted.usable()) {
            return Ok(Some(credential.clone()));
        }
    }
    let Ok(app) = github::App::load(config) else {
        return Ok(None);
    };
    let installation = app.installation_for(&owner, &repository)?;
    let request = match access {
        Access::Read => github::TokenRequest::contents_read(&repository),
        Access::Write => github::TokenRequest::contents_write(&repository),
    };
    let credential = Credential::github(app.installation_token(installation, &request)?);
    MINTED
        .lock()
        .map_err(|_| anyhow!("the git credential cache was poisoned"))?
        .insert(key, credential.clone());
    Ok(Some(credential))
}

/// Whether a token may be sent to this URL at all.
fn carries_a_token(url: &str) -> bool {
    url.starts_with("https://")
        || url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://[::1]:")
        || url.starts_with("http://localhost:")
}

/// One git repository holding a context tree.
///
/// Every call to git is a process, and under load a process costs far more than the question
/// it answers: one rejected push used to ask git six times who the remote was and twice which
/// branch it was on. Neither can change while one write is in flight, so each is read once.
#[derive(Debug, Clone, Default)]
pub struct Repo {
    root: PathBuf,
    credential: Option<Credential>,
    /// `origin`'s URL, or `None` when this repository has no `origin`.
    remote: std::cell::OnceCell<Option<String>>,
    /// The branch the clone is on. A write stays on the branch it started on, and a rebase
    /// keeps it.
    branch: std::cell::OnceCell<String>,
    /// Where this repository's `.git` is.
    git_dir: std::cell::OnceCell<PathBuf>,
    /// How long one call to the remote may take; [`NETWORK_TIMEOUT`] unless a test says.
    network_timeout: Option<Duration>,
}

/// Who a commit is attributed to.
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub email: String,
}

/// What [`Repo::open`] fails with when `root` is in no git repository at all. Every other
/// failure to open one is git failing on a repository that is there.
#[derive(Debug)]
struct NotARepository(PathBuf);

impl std::fmt::Display for NotARepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} is not a git repository", self.0.display())
    }
}

impl std::error::Error for NotARepository {}

impl Repo {
    /// Opens the repository rooted at `root`, with no credential for its remote.
    ///
    /// Only a call that reaches the remote needs one; a caller that pulls, fetches or pushes
    /// opens the repository with [`Repo::open_with_credential`] instead.
    ///
    /// # Errors
    ///
    /// Returns an error when `root` is not inside a git repository, or when git cannot open
    /// the one it is in.
    pub fn open(root: &Path) -> Result<Self> {
        let repo = Self {
            root: root.to_path_buf(),
            ..Self::default()
        };
        let arguments = ["rev-parse", "--absolute-git-dir"];
        let mut command = repo.command(&arguments);
        // Only git's own words tell "there is no repository here" from a repository it cannot
        // read, and it words them in the reader's language unless told otherwise.
        command.env("LC_ALL", "C");
        let output = command.output().with_context(|| {
            format!("running git {} in {}", arguments.join(" "), root.display())
        })?;
        if !output.status.success() {
            let reason = repo.reason(&output.stdout, &output.stderr);
            if output.status.code() == Some(128) && reason.contains("not a git repository") {
                return Err(NotARepository(root.to_path_buf()).into());
            }
            bail!(
                "git cannot open the repository at {} ({}): {reason}",
                root.display(),
                output.status
            );
        }
        let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let _ = repo.git_dir.set(PathBuf::from(git_dir));
        Ok(repo)
    }

    /// Opens the repository and mints the credential its remote needs.
    ///
    /// # Errors
    ///
    /// Returns an error when `root` is not a git repository, when git cannot read its remote,
    /// or when GitHub refuses to mint the token the remote needs.
    pub fn open_with_credential(
        root: &Path,
        config: &credentials::Config,
        access: Access,
    ) -> Result<Self> {
        Self::open(root)?.with_credential(config, access)
    }

    /// Mints the credential this repository's remote needs, for a repository already open.
    ///
    /// Kept apart from [`Repo::open`] so a caller can tell "this is not a git repository" from
    /// "GitHub would not mint the token", which are different troubles.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot read the remote, or GitHub refuses to mint the token
    /// the remote needs.
    pub fn with_credential(mut self, config: &credentials::Config, access: Access) -> Result<Self> {
        if let Some(url) = self.remote_url()? {
            self.credential = credential_for(&url, config, access)?;
        }
        Ok(self)
    }

    /// Opens the repository rooted at `root`, or returns `None` when `root` is in none.
    ///
    /// # Errors
    ///
    /// Returns an error when there is a repository and git cannot open it: a configuration it
    /// cannot read, a directory it does not trust, a git that could not start or was killed.
    /// None of those means the tree has no `main` to follow, and a caller that took them for
    /// "no repository" would follow nothing and report nothing wrong.
    pub fn open_optional(root: &Path) -> Result<Option<Self>> {
        match Self::open(root) {
            Ok(repo) => Ok(Some(repo)),
            Err(error) if error.is::<NotARepository>() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The URL of `origin` — as git will contact it, after any `insteadOf` rewriting — or
    /// `None` when this repository has no `origin`.
    ///
    /// Read once and kept: a repository does not change its remote while `ctx` is using it. A
    /// failure is not kept, so the next call asks again.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot answer: it could not start, was killed, or could not
    /// read its configuration. None of those says the repository has no remote. Taken for
    /// "no remote", one such failure during a write skipped the pull and the push, and the
    /// write answered 200 for a commit nobody published.
    pub fn remote_url(&self) -> Result<Option<String>> {
        if let Some(remote) = self.remote.get() {
            return Ok(remote.clone());
        }
        // `git remote get-url` exits 2 when there is no such remote.
        let remote = self.query(&["remote", "get-url", "origin"], 2)?;
        let _ = self.remote.set(remote.clone());
        Ok(remote)
    }

    /// Whether git ignores `path`, a path relative to the root.
    ///
    /// A file at an ignored path is one no commit takes, no clean removes and the tree does
    /// not load. A tracked file is never ignored, whatever the ignore rules say.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot answer.
    pub fn ignores(&self, path: &str) -> Result<bool> {
        let arguments = ["check-ignore", "--quiet", "--", path];
        let mut command = self.command(&arguments);
        // `check-ignore` takes no pathspec magic, `literal` included, and refuses to run under
        // it. It reads its argument as a name, not a pattern.
        command.env_remove("GIT_LITERAL_PATHSPECS");
        Ok(self.answer(command, &arguments, 1)?.is_some())
    }

    /// The branch this clone is on, read once.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot name the branch.
    pub fn branch(&self) -> Result<String> {
        if let Some(branch) = self.branch.get() {
            return Ok(branch.clone());
        }
        let branch = self.run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        let _ = self.branch.set(branch.clone());
        Ok(branch)
    }

    /// The commit the working tree is on.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot resolve `HEAD`, as in a repository with no commit.
    pub fn head(&self) -> Result<String> {
        self.run(&["rev-parse", "HEAD"])
    }

    /// Every uncommitted change, as `git status --porcelain` names it, each file git does not
    /// track included. Empty when the working tree and index match `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot report the status.
    pub fn uncommitted(&self) -> Result<Vec<String>> {
        Ok(self
            .run(&["status", "--porcelain", "--untracked-files=all"])?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Everything this tree holds that the remote does not, each as a line a person reads: a
    /// rebase left in progress, every uncommitted change as `git status` names it, and every
    /// commit the upstream does not have, by its short hash and subject. Empty when the tree
    /// holds nothing the remote lacks.
    ///
    /// In a tree somebody keeps, each of these is their work. The server neither throws it
    /// away nor publishes it: a commit of theirs would reach `main` with the server's next
    /// push, with none of the checks a write through the server gets.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot report the status or the log.
    pub fn unpublished(&self) -> Result<Vec<String>> {
        let mut work = Vec::new();
        if self.rebase_in_progress() {
            work.push("a rebase in progress".to_owned());
        }
        work.extend(self.uncommitted()?);
        if self.has_remote()? && self.has_upstream()? {
            for commit in self
                .run(&["log", "--format=%h %s", "@{upstream}..HEAD"])?
                .lines()
            {
                work.push(format!(
                    "the commit {commit}, which the remote does not have"
                ));
            }
        }
        Ok(work)
    }

    /// Whether `ctx` made this clone itself from [`REMOTE_ENV`], so the remote is the record
    /// and nothing in the clone is anybody's work.
    ///
    /// Every other tree is somebody's: `ctx` reads it and writes records into it, but never
    /// throws away what it holds.
    #[must_use]
    pub fn cloned_by_ctx(&self) -> bool {
        self.git_dir
            .get()
            .is_some_and(|git_dir| git_dir.join(CLONE_MARK).is_file())
    }

    /// Whether git stopped part-way through a rebase and is waiting for a person.
    ///
    /// Until that state is cleared, every `pull` fails, so a clone left this way answers no
    /// write and follows no merge.
    #[must_use]
    pub fn rebase_in_progress(&self) -> bool {
        let Some(git_dir) = self.git_dir.get() else {
            return false;
        };
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
    }

    /// Abandons a rebase git stopped part-way through, if there is one.
    ///
    /// "There was none" is not a failure, and neither is a rebase git will not abandon: the
    /// caller is recovering from something that already went wrong and has its own answer to
    /// give either way.
    pub fn abort_rebase(&self) {
        if self.rebase_in_progress() {
            let _ = self.run(&["rebase", "--abort"]);
        }
    }

    /// Puts the clone back on `commit`, throwing away everything added since, including a
    /// rebase git stopped part-way through and files git does not track.
    ///
    /// A write that failed is undone by going back to the commit the clone was on when the
    /// write started. That is exact: it removes what this write added and nothing else, so a
    /// commit somebody else left in this clone is never thrown away with it, and it costs no
    /// call to the remote.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot reset or clean the tree.
    pub fn reset_to(&self, commit: &str) -> Result<()> {
        // A rebase still in progress owns the index, so it goes first.
        self.abort_rebase();
        self.run(&["reset", "--hard", commit])?;
        self.run(&["clean", "-fd"])?;
        Ok(())
    }

    /// Throws away everything this clone holds that the remote does not.
    ///
    /// For a clone this process made and owns — a container's, cloned from
    /// `CTX_MEMORY_REMOTE` — the remote is the record: a commit that is only here is one
    /// whose caller was told the write failed, and publishing it later would publish a record
    /// nobody was told about. A tree somebody keeps and works in is not reset this way; it is
    /// theirs, and so is every commit in it.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot fetch, reset or clean the tree.
    pub fn reset_to_remote(&self) -> Result<()> {
        self.abort_rebase();
        if self.has_remote()? {
            self.fetch()?;
            self.run(&["reset", "--hard", "@{upstream}"])?;
        } else {
            self.run(&["reset", "--hard", "HEAD"])?;
        }
        self.run(&["clean", "-fd"])?;
        Ok(())
    }

    /// Throws away what a clone `ctx` made holds outside a write — files nobody committed, a
    /// rebase left in progress, a commit the remote does not have — and says whether there was
    /// any.
    ///
    /// Outside a write none of that is anybody's work: it is what a write whose rollback
    /// failed left behind, and a commit among it is one whose caller was told the write
    /// failed. Left in place, the next push would publish it and the tree the server serves
    /// would carry it. A tree somebody keeps is theirs and is never touched.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot report the clone's state, or cannot put it back the
    /// way the remote has it.
    pub fn discard_strays(&self) -> Result<bool> {
        if !self.cloned_by_ctx() {
            return Ok(false);
        }
        if self.wedged()?.is_none() && self.uncommitted()?.is_empty() {
            return Ok(false);
        }
        self.reset_to_remote()?;
        Ok(true)
    }

    /// Updates the remote-tracking branches from `origin`.
    ///
    /// # Errors
    ///
    /// Returns an error when the fetch fails.
    pub fn fetch(&self) -> Result<()> {
        self.run_bounded(&["fetch", "--quiet", "origin"], &[])?;
        Ok(())
    }

    /// Why this clone cannot be the start of a write or a follow pass, or `None` when it can.
    ///
    /// - A rebase left in progress, or a clone and a remote that have each moved on from the
    ///   other: from either state `pull --ff-only` fails, so every write and every follow pass
    ///   fails until something clears it.
    /// - A clone `ctx` made that holds a commit the remote does not have. Outside a write that
    ///   commit is one whose caller was told the write failed, and the next push would publish
    ///   it. A tree somebody keeps may be ahead of the remote: those commits are theirs to push.
    ///
    /// A repository with no remote, or a branch with no upstream, is never wedged.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot report the status of the branch.
    pub fn wedged(&self) -> Result<Option<&'static str>> {
        if self.rebase_in_progress() {
            return Ok(Some(
                "a rebase was left in progress, so no pull can run and no write can be published",
            ));
        }
        if !self.has_remote()? || !self.has_upstream()? {
            return Ok(None);
        }
        if self.is_ancestor("HEAD", "@{upstream}") {
            return Ok(None);
        }
        if !self.is_ancestor("@{upstream}", "HEAD") {
            return Ok(Some(
                "the clone and the remote have each moved on from the other, so no write can be \
                 published",
            ));
        }
        if self.cloned_by_ctx() {
            return Ok(Some(
                "the clone holds a commit the remote does not have, which the next push would \
                 publish although its caller was told the write failed",
            ));
        }
        Ok(None)
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors.
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        self.run(&["merge-base", "--is-ancestor", ancestor, descendant])
            .is_ok()
    }

    /// Fast-forwards the current branch from `remote`, doing nothing when there is no remote.
    ///
    /// `--no-rebase` keeps a machine whose git config sets `pull.rebase` from turning this
    /// into a rebase.
    ///
    /// # Errors
    ///
    /// Returns an error when the pull fails or would need a merge.
    pub fn pull_ff_only(&self) -> Result<()> {
        if !self.has_remote()? {
            return Ok(());
        }
        self.run_bounded(&["pull", "--ff-only", "--no-rebase", "--quiet"], &[])?;
        Ok(())
    }

    /// Replays local commits on top of the remote branch, as `committer`.
    ///
    /// A rebase writes a new commit for every one it replays, and each one needs a committer.
    /// The caller of the write is that committer, as they are of the commit itself
    /// ([`Repo::commit`]): a container has no git identity of its own, and git stops a rebase
    /// part-way through rather than guess one. The replayed commits are not signed, for the
    /// reason the commit is not.
    ///
    /// # Errors
    ///
    /// Returns an error when the rebase fails, leaving the repository as git left it.
    pub fn pull_rebase(&self, committer: &Author) -> Result<()> {
        self.run_bounded(
            &["-c", "commit.gpgsign=false", "pull", "--rebase", "--quiet"],
            &[
                ("GIT_COMMITTER_NAME", committer.name.as_str()),
                ("GIT_COMMITTER_EMAIL", committer.email.as_str()),
            ],
        )?;
        Ok(())
    }

    /// Commits the given paths, attributing the commit to `author`.
    ///
    /// The commit is not signed. It is the caller's record, not this machine's: the server
    /// writes it on behalf of whoever asked, and a signature from the box's own key would
    /// assert that the key's owner wrote it. It also keeps a write working on a machine whose
    /// `commit.gpgsign` is on and whose signing key is absent or locked, which a container's
    /// is, and saves a signing subprocess on every write.
    ///
    /// # Errors
    ///
    /// Returns an error when staging or committing fails.
    pub fn commit(&self, paths: &[String], message: &str, author: &Author) -> Result<String> {
        let mut add = vec!["add", "--"];
        add.extend(paths.iter().map(String::as_str));
        self.run(&add)?;
        let identity = format!("{} <{}>", author.name, author.email);
        self.run_with_env(
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--author",
                &identity,
                "--message",
                message,
            ],
            &[
                ("GIT_COMMITTER_NAME", author.name.as_str()),
                ("GIT_COMMITTER_EMAIL", author.email.as_str()),
            ],
        )?;
        self.head()
    }

    /// Whether this repository has an `origin` to pull from and push to.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot be run at all.
    pub fn has_remote(&self) -> Result<bool> {
        Ok(self.remote_url()?.is_some())
    }

    /// Whether the branch the working tree is on has an upstream: `branch.<branch>.remote`
    /// is set, which git answers with status 1 when it is not. Any other failure is an
    /// error, never "no upstream": read as none, it would let a write publish commits the
    /// remote does not have.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot name the branch or read its configuration.
    pub fn has_upstream(&self) -> Result<bool> {
        let key = format!("branch.{}.remote", self.branch()?);
        Ok(self.query(&["config", "--get", &key], 1)?.is_some())
    }

    /// Pushes the current branch, reporting a remote that moved on rather than failing.
    ///
    /// A push that did not end cleanly is not read from git's exit code or its message,
    /// neither of which says whether the remote moved its branch: a push can land and still
    /// fail afterwards, when the connection breaks or the remote's own process fails, and a
    /// push stopped at its time limit may have landed too, because the remote moves its
    /// branch before it runs `post-receive` and git waits for that hook. So the remote is
    /// fetched, within the same bound, and asked:
    ///
    /// - a remote that holds `HEAD` took the push: [`Push::Pushed`];
    /// - a remote that carries a commit this branch does not moved on: [`Push::Rejected`], and
    ///   the caller rebases and pushes again;
    /// - a remote that could not be asked: [`Push::Unknown`];
    /// - anything else did not take the push, and is an error.
    ///
    /// A caller is never told a published write failed.
    ///
    /// The push names `origin`, so git updates the remote-tracking branch with it and the
    /// credential travels in the environment rather than in the URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the remote neither took the push nor moved on.
    pub fn push(&self) -> Result<Push> {
        if !self.has_remote()? {
            return Ok(Push::Pushed);
        }
        let branch = self.branch()?;
        let refspec = format!("HEAD:{branch}");
        let failed = match self.call_remote(&["push", "origin", &refspec], &[]) {
            Ok(output) if output.success() => return Ok(Push::Pushed),
            Ok(output) => anyhow!("git push failed: {}", self.said(output.stderr.as_bytes())),
            Err(stopped) => stopped,
        };
        self.settle(&branch, failed)
    }

    /// What a push that did not end cleanly did, as the remote now says.
    fn settle(&self, branch: &str, failed: anyhow::Error) -> Result<Push> {
        if let Err(unread) = self.fetch() {
            return Ok(Push::Unknown(format!(
                "{failed:#}, and the fetch that would show whether it landed failed too: \
                 {unread:#}"
            )));
        }
        let remote = format!("origin/{branch}");
        if self.is_ancestor("HEAD", &remote) {
            return Ok(Push::Pushed);
        }
        // A remote that moved on carries a commit this branch does not. A branch the remote
        // does not have at all did not move: the push just did not land.
        let carried = self
            .run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{remote}^{{commit}}"),
            ])
            .is_ok();
        if carried && !self.is_ancestor(&remote, "HEAD") {
            return Ok(Push::Rejected(format!("{failed:#}")));
        }
        Err(failed.context("the remote does not hold the commit"))
    }

    /// What git printed, trimmed, with any credential taken out of it.
    fn said(&self, output: &[u8]) -> String {
        let text = String::from_utf8_lossy(output);
        let text = text.trim();
        self.credential
            .as_ref()
            .map_or_else(|| text.to_owned(), |credential| credential.redact(text))
    }

    /// Runs a git command that talks to the remote, and stops it — with every process it
    /// started — once it has run for the network bound. A remote that stops answering would
    /// otherwise hold the call, and the tree lock above it, for as long as the connection
    /// stays open.
    fn call_remote(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> Result<proc::Output> {
        let timeout = self.network_timeout.unwrap_or(NETWORK_TIMEOUT);
        let mut command = self.command(arguments);
        command.envs(environment.iter().copied());
        proc::run_command(command, "git", &[], timeout)
            .with_context(|| format!("git {}", arguments.join(" ")))
    }

    /// Runs a git command that talks to the remote, within the network bound, and returns its
    /// trimmed standard output.
    fn run_bounded(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Result<String> {
        let output = self.call_remote(arguments, environment)?;
        if !output.success() {
            bail!(
                "git {} failed: {}",
                arguments.join(" "),
                self.reason(output.stdout.as_bytes(), output.stderr.as_bytes())
            );
        }
        Ok(output.stdout.trim().to_owned())
    }

    /// Runs a git command in this repository and returns its trimmed standard output.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot be started or exits non-zero.
    pub fn run(&self, arguments: &[&str]) -> Result<String> {
        self.run_with_env(arguments, &[])
    }

    fn run_with_env(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Result<String> {
        let mut command = self.command(arguments);
        command.envs(environment.iter().copied());
        let output = command
            .output()
            .with_context(|| format!("running git {}", arguments.join(" ")))?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                arguments.join(" "),
                self.reason(&output.stdout, &output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Runs a git command whose answer is its trimmed output, or `None` when it exits with
    /// `absent`, the status that command gives for "there is none".
    fn query(&self, arguments: &[&str], absent: i32) -> Result<Option<String>> {
        self.answer(self.command(arguments), arguments, absent)
    }

    /// [`Repo::query`] for a command already built. Any status but 0 and `absent` — and a git
    /// that could not start or was stopped by a signal, which has no status at all — is an
    /// error, never "there is none".
    fn answer(
        &self,
        mut command: Command,
        arguments: &[&str],
        absent: i32,
    ) -> Result<Option<String>> {
        let output = command
            .output()
            .with_context(|| format!("running git {}", arguments.join(" ")))?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            )),
            Some(code) if code == absent => Ok(None),
            _ => bail!(
                "git {} failed ({}): {}",
                arguments.join(" "),
                output.status,
                self.reason(&output.stdout, &output.stderr)
            ),
        }
    }

    /// Why a git command failed, as it said it. git prints some refusals — "nothing to
    /// commit" among them — on standard output, so a failure with nothing on standard error
    /// is told by what it printed there instead of with an empty reason.
    fn reason(&self, stdout: &[u8], stderr: &[u8]) -> String {
        if stderr.iter().all(u8::is_ascii_whitespace) {
            self.said(stdout)
        } else {
            self.said(stderr)
        }
    }

    /// One git command in this repository.
    ///
    /// Every path `ctx` hands git is a name, never a pattern: a record path comes from a
    /// caller, and with pathspec magic on, `decisions/*.md` would name every record in the
    /// directory and `:(top)…` would name something else again.
    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .args(arguments)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true");
        if let Some(credential) = &self.credential {
            credential.apply(&mut command);
        }
        command
    }
}

/// What a push did.
#[derive(Debug)]
pub enum Push {
    Pushed,
    /// The remote moved on; the message git printed.
    Rejected(String),
    /// The push did not end cleanly and the remote could not be asked whether it landed;
    /// why.
    Unknown(String),
}

/// Clones `CTX_MEMORY_REMOTE` into `root` when that path holds no tree yet, and brings it up
/// to the remote when it does.
///
/// The server and the fetch job start from an empty container filesystem, so both call this
/// before they load a tree. With no `CTX_MEMORY_REMOTE` set, a local tree is left alone.
///
/// A clone this function made is marked as `ctx`'s own, and brought up to date by being put
/// back the way the remote has it: whatever a stopped copy left in it is nobody's work, and a
/// pull would refuse to run over it. Any other tree is somebody's, and is only fast-forwarded.
///
/// The graph repository is private, so the clone and the pull carry a read credential, minted
/// from the app key the environment names. There is no tree yet, so no `[credentials]` table
/// is read here: a container names its app key with `CTX_SECRET_GITHUB_APP` or
/// `CTX_CREDENTIAL_GITHUB_APP`.
///
/// # Errors
///
/// Returns an error when the clone fails, or when `root` holds files that are not a clone of
/// the remote.
pub fn clone_when_empty(root: &Path) -> Result<()> {
    let Some(remote) = std::env::var_os(REMOTE_ENV) else {
        return Ok(());
    };
    let remote = remote
        .to_str()
        .ok_or_else(|| anyhow!("{REMOTE_ENV} is not valid UTF-8"))?
        .to_owned();
    let credentials = credentials::Config::new();
    if root.join(".git").exists() {
        let repo = Repo::open_with_credential(root, &credentials, Access::Read)?;
        if repo.cloned_by_ctx() {
            return repo.reset_to_remote();
        }
        return repo.pull_ff_only();
    }
    if root.exists() && root.read_dir()?.next().is_some() {
        bail!(
            "{} is not a clone of {remote} and is not empty",
            root.display()
        );
    }
    let parent = root
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", root.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let credential = credential_for(&remote, &credentials, Access::Read)?;
    clone_into(&remote, root, credential.as_ref(), CLONE_TIMEOUT)
}

/// Clones `remote` into `root`, marks the clone as `ctx`'s own, and gives up after `timeout`.
fn clone_into(
    remote: &str,
    root: &Path,
    credential: Option<&Credential>,
    timeout: Duration,
) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("clone")
        .arg("--quiet")
        .arg(remote)
        .arg(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true");
    if let Some(credential) = credential {
        credential.apply(&mut command);
    }
    let output = proc::run_command(command, "git", &[], timeout)
        .with_context(|| format!("cloning {remote}"))?;
    if !output.success() {
        let stderr = output.stderr.trim();
        let stderr =
            credential.map_or_else(|| stderr.to_owned(), |credential| credential.redact(stderr));
        bail!("cloning {remote} into {} failed: {stderr}", root.display());
    }
    let mark = root.join(".git").join(CLONE_MARK);
    std::fs::write(&mark, remote).with_context(|| format!("writing {}", mark.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests;
