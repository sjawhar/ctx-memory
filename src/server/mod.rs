//! The ctx server: one process that answers read commands over HTTP and is the only writer
//! of records.
//!
//! The tree is loaded once and kept behind a lock, so a query costs a search rather than a
//! tree load, and a write cannot race another write.

pub mod command;
pub mod identity;
pub mod request_log;
pub mod write;

use std::future::IntoFuture as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::Router;
use chrono::{DateTime, Utc};
use tokio::net::TcpListener;

use crate::config::{DataDir, StateDir};
use crate::context::Context;
use crate::git::{Access, Repo};
use crate::lease::{self, Lease};
use crate::output::{Outcome, Sink};
use crate::protocol;
use crate::redact::one_line;
use crate::store::Store;
use crate::windows;

pub use identity::{Caller, Via};

/// Arguments of `ctx serve`.
#[derive(clap::Args, Debug)]
pub struct ServeArgs {
    /// Address for Cloud Run ingress; callers present a Google identity token.
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<SocketAddr>,

    /// Loopback address for the Tailscale sidecar, which sets the caller's login header.
    #[arg(long, value_name = "ADDR")]
    pub tailnet_listen: Option<SocketAddr>,

    /// The audience every Google identity token must carry.
    #[arg(long, env = "CTX_SERVER_AUDIENCE")]
    pub audience: Option<String>,

    /// Who may call the ingress listener: an account email, or `@domain` for every account in
    /// one domain. Repeat the flag, or separate the entries with commas.
    #[arg(
        long = "allow-principal",
        env = "CTX_SERVER_PRINCIPALS",
        value_name = "EMAIL|@DOMAIN",
        value_delimiter = ','
    )]
    pub allow_principal: Vec<String>,

    /// Where Google publishes the keys that sign identity tokens.
    #[arg(long, default_value = identity::GOOGLE_KEYS_URL)]
    pub google_keys: String,

    /// Seconds between one pull of `main` and the next.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    pub pull_interval: u64,

    /// Seconds the listeners wait for this copy's record index before they bind.
    #[arg(long, value_name = "SECONDS", default_value_t = 90)]
    pub index_wait: u64,
}

/// Everything the request handlers share: one loaded tree behind one lock, the write lease
/// this copy holds, the last health check, which a probe answers from while the tree is
/// busy, how the follow loop is doing, whether the disk holds what a failed rollback left,
/// where the share's windows and this copy's index are, and the server's own directories,
/// which no answer names.
pub struct Shared {
    context: Mutex<Context>,
    lease: Mutex<WriteLease>,
    health: Mutex<command::Health>,
    following: Mutex<Following>,
    /// A write's rollback failed, so the tree on disk may hold what that write left, which
    /// was never published and may have been refused. While this is set nothing reads the
    /// tree back from disk: the tree served stays the one from before the write, until the
    /// clone holds nothing the remote lacks.
    tainted: AtomicBool,
    /// The share's directory and this copy's own, read once when the server starts, as
    /// [`Places`] reads the same paths. The index is built and caught up from them with no
    /// hold on the tree, so a read is answered while that runs.
    data: DataDir,
    state: StateDir,
    places: Places,
}

/// The server's own directories — its clone, its data directory and its state directory —
/// each with the placeholder a remote caller reads in its place, the longest first, so a
/// directory inside another is named for itself.
///
/// An error carries the paths it failed at: `reading /srv/…/data/windows: No such file or
/// directory`. Those say where this copy keeps its files, which is nothing a caller needs and
/// something an attacker maps, so every error body and every captured command output passes
/// through [`Places::hide`] before it leaves the server. The directories are read once, when
/// the server starts, which is also when their configuration is read.
struct Places(Vec<(String, &'static str)>);

impl Places {
    fn of(context: &Context) -> Self {
        let mut places: Vec<(String, &'static str)> = Vec::new();
        let mut add = |path: &Path, placeholder: &'static str| {
            for form in [Some(path.to_path_buf()), path.canonicalize().ok()]
                .into_iter()
                .flatten()
            {
                let text = form.to_string_lossy().trim_end_matches('/').to_owned();
                if text.len() > 1 && !places.iter().any(|(known, _)| *known == text) {
                    places.push((text, placeholder));
                }
            }
        };
        add(&context.tree.root, "<tree>");
        if let Ok(data) = context.data() {
            add(data.root(), "<data>");
        }
        if let Ok(state) = context.state() {
            add(state.root(), "<state>");
        }
        places.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        Self(places)
    }

    /// `text` with each of the server's directories replaced by its placeholder, but only
    /// where it is a complete path prefix. A caller's draft can contain a server directory's
    /// bytes in the middle of its own path (`/client/srv/tree-notes.md`): that is the
    /// caller's label and must not become the invented `/client<tree>-notes.md`.
    fn hide(&self, text: &str) -> String {
        self.0
            .iter()
            .fold(text.to_owned(), |text, (path, placeholder)| {
                hide_path(&text, path, placeholder)
            })
    }
}

/// `path` in `text`, replaced with `placeholder` only when it is a complete path prefix.
fn hide_path(text: &str, path: &str, placeholder: &str) -> String {
    let mut hidden = String::with_capacity(text.len());
    let mut after = 0;
    for (start, _) in text.match_indices(path) {
        let end = start + path.len();
        if path_starts_here(text, start) && path_ends_here(text, end) {
            hidden.push_str(&text[after..start]);
            hidden.push_str(placeholder);
            after = end;
        }
    }
    if after == 0 {
        return text.to_owned();
    }
    hidden.push_str(&text[after..]);
    hidden
}

/// Whether `start` is the start of a path token: it is at text's start, or follows the forms
/// error messages and JSON use before a path. A slash, letter or other path character before it
/// means the server directory's bytes are part of the caller's own path, not the server's.
fn path_starts_here(text: &str, start: usize) -> bool {
    text[..start].chars().next_back().is_none_or(|before| {
        before.is_whitespace() || matches!(before, '"' | '\'' | '(' | '=' | ':')
    })
}

/// Whether `end` is the end of a path token. A child of the server's directory is still hidden;
/// a name such as `<server tree>-notes.md` is not.
fn path_ends_here(text: &str, end: usize) -> bool {
    text[end..].chars().next().is_none_or(|after| {
        after == '/'
            || after.is_whitespace()
            || matches!(
                after,
                '"' | '\'' | ')' | ']' | '}' | ',' | ';' | ':' | '?' | '#' | '='
            )
    })
}

/// The write lease, as this copy has it.
struct WriteLease {
    /// The lease, once a write has taken it. A check that failed gave it up.
    held: Option<Lease>,
    /// This copy gave the lease up because it is stopping. No write takes it again: one that
    /// did would write beside the copy that replaces this one.
    released: bool,
}

/// How the follow loop's passes are going: nothing while its last pass succeeded, and
/// otherwise since when its passes have failed. Why they fail is in the server's log, which
/// only the operator reads.
#[derive(Clone, Debug, Default)]
pub struct Following {
    pub failing_since: Option<DateTime<Utc>>,
}

impl Shared {
    /// Holds the tree a server answers from, with no lease taken yet, makes the first health
    /// check, and reads the directories this copy indexes from and keeps its index in.
    ///
    /// A probe that finds the tree busy answers from the last check, and the follow loop can
    /// take the tree before any probe gets in.
    ///
    /// # Errors
    ///
    /// Returns an error when the clone cannot be checked or those directories cannot be
    /// resolved.
    pub fn new(context: Context) -> Result<Self> {
        let health = command::check_health(&context)?;
        let places = Places::of(&context);
        let data = DataDir::new(
            context
                .data()
                .context("resolving the directory this copy indexes the share from")?
                .root()
                .to_path_buf(),
        );
        let state = StateDir::new(
            context
                .state()
                .context("resolving the directory this copy keeps its index in")?
                .root()
                .to_path_buf(),
        );
        Ok(Self {
            context: Mutex::new(context),
            lease: Mutex::new(WriteLease {
                held: None,
                released: false,
            }),
            health: Mutex::new(health),
            following: Mutex::new(Following::default()),
            tainted: AtomicBool::new(false),
            data,
            state,
            places,
        })
    }

    /// Where the share keeps its windows, as this copy read it when it started.
    #[must_use]
    pub fn data(&self) -> &DataDir {
        &self.data
    }

    /// Where this copy keeps its own index, as it read it when it started.
    #[must_use]
    pub fn state(&self) -> &StateDir {
        &self.state
    }

    /// `text`, as a remote caller may read it: the server's own directories are named
    /// `<tree>`, `<data>` and `<state>`, never by their paths.
    #[must_use]
    pub fn hide_paths(&self, text: &str) -> String {
        self.places.hide(text)
    }

    /// Says the disk may hold what a write whose rollback failed left.
    pub fn taint(&self) {
        self.tainted.store(true, Ordering::SeqCst);
    }

    /// Says the disk holds nothing a failed rollback left, as after a reset that succeeded.
    pub fn untaint(&self) {
        self.tainted.store(false, Ordering::SeqCst);
    }

    /// Whether the disk may hold what a write whose rollback failed left.
    pub fn tainted(&self) -> bool {
        self.tainted.load(Ordering::SeqCst)
    }

    /// Locks the loaded tree.
    ///
    /// A request handler that panicked while it held the lock poisoned it. A poisoned lock
    /// is recovered rather than refused, or one panic would stop every later request for the
    /// life of the process: the tree is put back the way somebody chose before it is handed
    /// on, because the handler may have stopped part-way through a write. Until that
    /// succeeds the lock stays poisoned, so the next caller tries again.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree cannot be put back after a holder panicked.
    pub fn tree(&self) -> Result<MutexGuard<'_, Context>> {
        self.context
            .lock()
            .or_else(|poisoned| self.recovered(poisoned))
    }

    /// Locks the loaded tree if nobody holds it, without waiting; `None` when it is held.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree cannot be put back after a holder panicked.
    pub fn try_tree(&self) -> Result<Option<MutexGuard<'_, Context>>> {
        match self.context.try_lock() {
            Ok(tree) => Ok(Some(tree)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Poisoned(poisoned)) => self.recovered(poisoned).map(Some),
        }
    }

    /// The tree a panicked holder left, put back before anyone else reads it.
    fn recovered<'lock>(
        &self,
        poisoned: PoisonError<MutexGuard<'lock, Context>>,
    ) -> Result<MutexGuard<'lock, Context>> {
        let mut tree = poisoned.into_inner();
        recover(&mut tree, self).context("recovering the tree after a request handler panicked")?;
        self.context.clear_poison();
        Ok(tree)
    }

    /// The last health check.
    pub fn last_health(&self) -> command::Health {
        unpoisoned(&self.health).clone()
    }

    /// Keeps a health check for the probes that arrive while the tree is busy.
    pub fn remember_health(&self, health: command::Health) {
        *unpoisoned(&self.health) = health;
    }

    /// How the follow loop is doing.
    pub fn following(&self) -> Following {
        unpoisoned(&self.following).clone()
    }

    /// Records whether one follow pass failed.
    fn followed(&self, failed: bool) {
        let mut following = unpoisoned(&self.following);
        let since = following.failing_since.unwrap_or_else(Utc::now);
        following.failing_since = failed.then_some(since);
    }

    /// Makes sure this copy holds the write lease, taking it on the first write.
    ///
    /// The lease is held for the life of the copy and renewed in the background, so a write
    /// costs one check rather than a round of taking and giving back. A check that fails
    /// gives this copy's handle up without freeing the lease object: the next write tries to
    /// take the lease again, and takes it once the takeover window has passed.
    ///
    /// # Errors
    ///
    /// Returns an error when another copy holds the lease, Cloud Storage cannot be reached,
    /// or this copy gave the lease up to stop. A caller that sees an error must write nothing.
    pub fn hold_lease(&self) -> Result<()> {
        let mut lease = unpoisoned(&self.lease);
        if lease.released {
            bail!(
                "this copy is stopping and has given its write lease up; the copy that replaces \
                 it takes writes"
            );
        }
        if let Some(held) = lease.held.as_ref() {
            if let Err(error) = held.check() {
                if let Some(held) = lease.held.take() {
                    held.forsake();
                }
                return Err(error);
            }
            return Ok(());
        }
        lease.held = lease::for_role("server")?;
        Ok(())
    }

    /// Gives the write lease up, which this copy does when it stops.
    ///
    /// A write whose caller hung up runs on after its request is gone, and holds the tree
    /// until it has published or put the clone back. So the tree is taken first, and the
    /// lease freed only once no write is under way: freed earlier, the copy that replaces this
    /// one could write while this one's push was still landing. A write that reaches the tree
    /// after this finds the lease given up and writes nothing.
    pub fn release_lease(&self) {
        // A tree that cannot be put back after a panic has no write under way either: the
        // holder that panicked has stopped.
        let tree = self.tree();
        if let Err(error) = &tree {
            eprintln!(
                "stopping with a tree that could not be recovered: {}",
                one_line(&format!("{error:#}"))
            );
        }
        let mut lease = unpoisoned(&self.lease);
        lease.released = true;
        if let Some(held) = lease.held.take() {
            if let Err(error) = held.release() {
                eprintln!(
                    "releasing the write lease: {}",
                    one_line(&format!("{error:#}"))
                );
            }
        }
        drop(lease);
        drop(tree);
    }
}

/// Locks the write lease, whatever a holder that panicked left: the lease keeps its own state
/// behind its own lock, so a handler that stopped mid-check left nothing to repair here.
fn unpoisoned<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

/// Puts the tree back the way somebody chose, after a holder of its lock panicked.
///
/// A clone `ctx` made is put back the way the remote has it, as a failed write's would be.
/// Anyone else's tree keeps what is in it: the next write refuses to run over uncommitted
/// work and names it. The loaded tree is then read again from disk, unless the disk may hold
/// what a failed rollback left and the tree still holds something the remote lacks
/// ([`clean_after_taint`]): then the tree served stays the one in memory.
fn recover(context: &mut Context, shared: &Shared) -> Result<()> {
    let root = context.tree.root.clone();
    let repo = Repo::open_optional(&root)?;
    if repo.as_ref().is_some_and(Repo::cloned_by_ctx) {
        Repo::open_with_credential(&root, &context.config.credentials, Access::Read)?
            .reset_to_remote()?;
        shared.untaint();
    } else if !clean_after_taint(shared, repo.as_ref())? {
        return Ok(());
    }
    *context = Context::open(&root)?;
    Ok(())
}

/// Whether the tree may be read back from disk: no failed rollback is known to have left
/// anything, or the tree, which somebody keeps, now holds nothing the remote lacks, so what
/// the rollback left is gone. The second clears the mark.
fn clean_after_taint(shared: &Shared, repo: Option<&Repo>) -> Result<bool> {
    if !shared.tainted() {
        return Ok(true);
    }
    let clean = match repo {
        Some(repo) => repo.unpublished()?.is_empty(),
        None => true,
    };
    if clean {
        shared.untaint();
    }
    Ok(clean)
}

/// Runs the server until the process is stopped.
///
/// A panic in a request handler is reported in one line too ([`panic_line`]): Rust's own
/// report runs to several, and on Cloud Run each is its own log entry.
///
/// # Errors
///
/// Returns an error when a listener cannot be bound or an HTTP server stops with one.
pub fn serve(context: Context, arguments: &ServeArgs, sink: &mut Sink<'_>) -> Result<Outcome> {
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(ToString::to_string);
        eprintln!("{}", panic_line(info.payload_as_str(), location.as_deref()));
    }));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the server runtime")?;
    runtime.block_on(run(context, arguments, sink))?;
    Ok(Outcome::Pass)
}

/// The line a panic in the server writes to standard error: where it happened and what it
/// said, as [`one_line`] gives any text it quotes.
fn panic_line(payload: Option<&str>, location: Option<&str>) -> String {
    let payload = payload.unwrap_or("a payload that is not text");
    let line = match location {
        Some(location) => format!("the server panicked at {location}: {payload}"),
        None => format!("the server panicked: {payload}"),
    };
    one_line(&line)
}

async fn run(context: Context, arguments: &ServeArgs, sink: &mut Sink<'_>) -> Result<()> {
    if arguments.listen.is_none() && arguments.tailnet_listen.is_none() {
        bail!("ctx serve needs --listen, --tailnet-listen, or both");
    }
    // A Cloud Run rollout runs the old copy and the new one at once, and the write lease is
    // what keeps one of them the only writer. A copy on Cloud Run with no lease bucket would
    // take no lease and write beside the other, so it does not start. Cloud Run reaches a
    // copy through `--listen`, and names the service it runs in `K_SERVICE`.
    if lease::bucket().is_none()
        && (arguments.listen.is_some() || std::env::var_os("K_SERVICE").is_some())
    {
        bail!(
            "ctx serve on Cloud Run needs CTX_LEASE_BUCKET: a rollout runs two copies at once, \
             and the write lease is what keeps one of them the only writer"
        );
    }
    // A copy that stopped mid-write leaves files behind; the remote is the record.
    let credentials = context.config.credentials.clone();
    let context = reset_clone(context, &credentials)?;
    // The first health check is made here, before any port is bound: a probe that finds the
    // tree busy answers from the last check, and the first follow pass may hold the tree
    // before any probe gets in. A clone that cannot be checked is one no write can start from.
    let shared = Arc::new(Shared::new(context).context("checking the clone before serving it")?);
    // The record index is replayed from every window on the share whenever the state
    // directory holds none, which a container's does at every start, and the first caller to
    // need one used to wait that replay out. It is built here instead, before any port is
    // bound, so the deployment's startup probe holds traffic while it runs, and off the tree
    // lock, so a read that needs no index is answered while it goes. The listeners wait no
    // longer than `--index-wait` for it, because a startup probe gives a container a bounded
    // time to accept a connection and a share whose replay outlasts that still needs serving.
    let warming = warm_index(Arc::clone(&shared));
    if arguments.index_wait > 0 {
        let mut warmed = warming.clone();
        let wait = Duration::from_secs(arguments.index_wait);
        if tokio::time::timeout(wait, index_built(&mut warmed))
            .await
            .is_err()
        {
            eprintln!(
                "the record index is still being built after {}s: binding the listeners now, \
                 and a read that needs the index waits for the rest of it",
                arguments.index_wait
            );
        }
    }
    let mut servers = Vec::new();
    // A container runtime stops this copy with SIGTERM before the copy that replaces it
    // writes. Both listeners stop on that signal, and the lease is given up afterwards.
    let (stop, _) = tokio::sync::watch::channel(false);

    // The deployment's startup probe connects to the ingress port, so it is bound first.
    if let Some(address) = arguments.listen {
        let audience = arguments
            .audience
            .clone()
            .ok_or_else(|| anyhow!("--listen needs --audience (CTX_SERVER_AUDIENCE)"))?;
        // Anyone with a Google account can mint a token for any audience, so the audience
        // says which server a caller meant, not who the caller is. Cloud Run's invoker check
        // is declared in another repository and cannot be read from here, so this copy keeps
        // its own list and refuses to start without one.
        let allowed = identity::Allowed::parse(&arguments.allow_principal).context(
            "--listen needs --allow-principal (CTX_SERVER_PRINCIPALS): who may call this server",
        )?;
        let google = Arc::new(identity::Google::new(
            arguments.google_keys.clone(),
            audience,
            allowed,
        )?);
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("binding {address}"))?;
        announce(sink, "listen", listener.local_addr()?)?;
        let router = router(Arc::clone(&shared), Identity::Google(google));
        servers.push(tokio::spawn(
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped(stop.subscribe()))
                .into_future(),
        ));
    }

    if let Some(address) = arguments.tailnet_listen {
        // The caller's name on this port is a header the Tailscale sidecar sets, which only
        // the sidecar beside this process can be trusted to set. An address off the loopback
        // interface would let anything that can reach the port name itself.
        if !address.ip().is_loopback() {
            bail!(
                "--tailnet-listen takes a loopback address: the sidecar runs beside this process"
            );
        }
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("binding {address}"))?;
        announce(sink, "tailnet-listen", listener.local_addr()?)?;
        let router = router(Arc::clone(&shared), Identity::Tailscale);
        servers.push(tokio::spawn(
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped(stop.subscribe()))
                .into_future(),
        ));
    }
    let following = follow(Arc::clone(&shared), arguments.pull_interval, warming);
    let signalled = tokio::spawn(async move {
        match terminated().await {
            Ok(()) => drop(stop.send(true)),
            Err(error) => eprintln!(
                "the server cannot watch for SIGTERM: {}",
                one_line(&format!("{error:#}"))
            ),
        }
    });

    for server in servers {
        server.await.context("the HTTP server stopped")??;
    }
    signalled.abort();
    following.abort();
    // Every connection is closed, so the lease this copy holds is given up here: the copy that
    // replaces it writes at once instead of waiting out the takeover window. A write whose
    // caller hung up may still be running, and the release waits for it. The release talks to
    // Cloud Storage with a blocking client, so it runs off the runtime. The request log's last
    // lines are written before the process exits.
    let releasing = Arc::clone(&shared);
    tokio::task::spawn_blocking(move || {
        releasing.release_lease();
        request_log::flush(LAST_LINES_WAIT);
    })
    .await
    .context("releasing the write lease")?;
    Ok(())
}

/// The longest a stopping copy waits for the request log's last lines to reach standard output.
const LAST_LINES_WAIT: Duration = Duration::from_secs(2);

/// Resolves when the server is asked to stop.
async fn stopped(mut stop: tokio::sync::watch::Receiver<bool>) {
    let _ = stop.changed().await;
}

/// Resolves when this process is asked to stop, the way a container runtime asks.
async fn terminated() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("watching for SIGTERM")?;
    tokio::select! {
        _ = terminate.recv() => Ok(()),
        interrupted = tokio::signal::ctrl_c() => interrupted.context("watching for an interrupt"),
    }
}

/// Starts the server from `main`: throws away whatever a stopped copy left in a clone `ctx`
/// made itself, and refuses to start in anyone else's tree while it holds work the remote does
/// not have.
///
/// A copy can stop mid-write and leave uncommitted files, a rebase git stopped part-way
/// through, or a commit the remote does not have. In a clone made from `CTX_MEMORY_REMOTE`, as
/// a container's is, none of that is anybody's work: the remote is the record, and a commit
/// left behind is one whose caller was told the write failed. So all of it goes.
///
/// In a tree somebody keeps and works in, the same things are theirs. Resetting it once cost
/// a person every uncommitted change in their checkout, and a commit they had not pushed would
/// reach `main` with the server's first push, with none of the checks a write gets. So the
/// server names what is there and stops, and leaves it for its owner to push, stash or remove.
fn reset_clone(context: Context, credentials: &crate::credentials::Config) -> Result<Context> {
    let root = context.tree.root.clone();
    let Some(repo) = Repo::open_optional(&root)? else {
        return Ok(context);
    };
    if !repo.cloned_by_ctx() {
        let work = repo.unpublished()?;
        if !work.is_empty() {
            bail!(
                "{} holds work the remote does not have, and ctx serve throws away only what is \
                 in a clone it made itself from {}, and publishes nothing it did not check; \
                 commit and push it, stash it or remove it first:\n  {}",
                root.display(),
                crate::git::REMOTE_ENV,
                work.join("\n  ")
            );
        }
        return Ok(context);
    }
    if repo.wedged()?.is_none() && repo.uncommitted()?.is_empty() {
        return Ok(context);
    }
    repo.with_credential(credentials, Access::Read)?
        .discard_strays()?;
    Context::open(&root)
}

/// What the follow loop prints, at most once every [`HEARTBEAT_EVERY`], while its passes
/// succeed. An alert on its absence catches a pass that never ends and a copy that is down.
/// A failed pass prints `following main: <why>` instead, as one line ([`one_line`]):
/// git's error runs to several lines, and on Cloud Run each line is its own log entry. The
/// heartbeat starts with other words, so a match on that failure line never matches it.
const HEARTBEAT: &str = "follow heartbeat: ok";

/// The longest the follow loop goes between two heartbeats while its passes succeed.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(300);

/// Follows `main` and the windows the fetch job writes.
///
/// Every interval: fast-forward the clone and reload the tree when the commit moved, under
/// the one writer lock, and index the windows that appeared on the share, which takes no
/// lock. A record a person merges on GitHub is searchable within one interval. How each pass
/// went is kept for `/healthz`, and said on standard error: each failed pass, and a heartbeat
/// while they succeed.
///
/// The first pass waits for the index this copy built as it started ([`warm_index`]): a pass
/// beside that build would replay the same windows a second time.
fn follow(
    shared: Arc<Shared>,
    interval: u64,
    mut warming: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let period = Duration::from_secs(interval.max(1));
        let mut heard: Option<std::time::Instant> = None;
        index_built(&mut warming).await;
        loop {
            tokio::time::sleep(period).await;
            let following = Arc::clone(&shared);
            let followed = tokio::task::spawn_blocking(move || follow_once(&following)).await;
            let failure = match followed {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(error) => Some(format!("the follow pass stopped: {error}")),
            };
            match &failure {
                Some(why) => eprintln!("following main: {}", one_line(why)),
                None if heard.is_none_or(|at| at.elapsed() >= HEARTBEAT_EVERY) => {
                    eprintln!("{HEARTBEAT}");
                    heard = Some(std::time::Instant::now());
                }
                None => {}
            }
            shared.followed(failure.is_some());
        }
    })
}

/// Builds this copy's record index, and catches it up to the windows on the share.
///
/// The index is in the state directory, which a container's image does not carry, so it is
/// replayed from every window on the share at every start. That replay is started here, as
/// the server starts, rather than left to the first caller that needs an index or to the
/// first follow pass. It holds nothing a read waits on.
///
/// The receiver says when the build is over, however it went: a failure is one line on
/// standard error, and the next follow pass tries again.
fn warm_index(shared: Arc<Shared>) -> tokio::sync::watch::Receiver<bool> {
    let (built, building) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let indexed =
            tokio::task::spawn_blocking(move || index_windows(shared.data(), shared.state())).await;
        match indexed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!(
                "building the record index: {}",
                one_line(&format!("{error:#}"))
            ),
            Err(error) => eprintln!("building the record index: the build stopped: {error}"),
        }
        drop(built.send(true));
    });
    building
}

/// Waits until the index build [`warm_index`] started is over, however it went.
///
/// The value is read out of the receiver rather than held, so nothing of the channel's own is
/// alive while this waits.
async fn index_built(building: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        let built = *building.borrow_and_update();
        if built {
            return;
        }
        if building.changed().await.is_err() {
            return;
        }
    }
}

/// One pass of the follow loop.
///
/// The pull takes the tree, because it rewrites the files the tree was read from. The windows
/// are indexed after it, with the tree free: that reads the share and writes this copy's own
/// index, and touches nothing a read is answered from. A pass used to hold the tree for all of
/// it, so a copy whose state directory was empty — a container's is at every start — held it
/// for as long as replaying every window on the share took, and every read waited that out.
///
/// A failed pull is not the end of the pass: the windows the fetch job wrote are on the
/// share, not in the clone, so they are still indexed while the remote is unreachable.
///
/// A tree that is not a git repository has no `main` to follow. Every other failure — git
/// unable to open the clone, GitHub refusing the token a pull needs — is the loop failing,
/// and is reported.
fn follow_once(shared: &Shared) -> Result<()> {
    let pulled = {
        let mut tree = shared.tree()?;
        let root = tree.tree.root.clone();
        match Repo::open_optional(&root) {
            Ok(Some(repo)) => follow_main(repo, &mut tree, &root, shared),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        }
    };
    let indexed = index_windows(shared.data(), shared.state());
    pulled.and(indexed)
}

/// Brings the clone up to the remote: puts a clone `ctx` made back the way the remote has it
/// when it holds anything of its own, fast-forwards it, and reloads the tree when either
/// changed it.
///
/// A copy that writes nothing still serves its tree, so what a failed rollback left in a clone
/// `ctx` made goes here, within one interval, rather than at the next write. Until it goes,
/// the tree served stays the one read before: what is on disk is what a failed write left,
/// which was never published and may have been refused. In a tree somebody keeps nothing of
/// theirs is thrown away, so after a failed rollback the tree is not read back from disk,
/// whatever the pull moved, until it holds nothing the remote lacks ([`clean_after_taint`]).
fn follow_main(repo: Repo, tree: &mut Context, root: &Path, shared: &Shared) -> Result<()> {
    let repo = repo.with_credential(&tree.config.credentials, Access::Read)?;
    let before = repo.head().ok();
    let discarded = repo.discard_strays()?;
    if repo.cloned_by_ctx() {
        // The clone is the way the remote has it now, whatever a failed rollback left.
        shared.untaint();
    }
    let pulled = repo.pull_ff_only();
    let tainted = shared.tainted();
    if !clean_after_taint(shared, Some(&repo))? {
        return pulled;
    }
    if tainted || discarded || repo.head().ok() != before {
        *tree = Context::open(root)?;
    }
    pulled
}

/// Indexes every completed window this copy's index has not indexed yet, oldest first.
///
/// The index keeps that watermark itself ([`Store::catch_up`]), so a copy replays the share
/// once, when it builds its index, and reads only what appeared since on every pass after
/// that. The follow loop used to keep the watermark in memory, where every copy starts with
/// nothing: a copy's first pass then re-read every window on the share, however many there
/// were, and the deployment's took 157 s of a restart on 2026-11-01 with the tree held.
fn index_windows(data: &DataDir, state: &StateDir) -> Result<()> {
    let store = Store::load(data, state).context("opening this copy's record index")?;
    store.catch_up(data)
}

/// Prints one bound address, so a caller that asked for port 0 can find it.
fn announce(sink: &mut Sink<'_>, label: &str, address: SocketAddr) -> Result<()> {
    writeln!(sink.out, "{label} {address}")?;
    sink.out.flush()?;
    Ok(())
}

/// The routes one listener answers. The listener decides how a caller is identified, and each
/// request it answers is one line of the request log ([`request_log`]).
fn router(shared: Arc<Shared>, identity: Identity) -> Router {
    let door = identity.door();
    Router::new()
        .route("/healthz", get(command::healthz))
        .route(protocol::COMMAND_ROUTE, post(command::command))
        .route(protocol::PUT_ROUTE, post(write::put))
        .route(protocol::PUT_BATCH_ROUTE, post(write::put_batch))
        .route(protocol::DELETE_ROUTE, post(write::delete))
        .route(protocol::MARKER_ROUTE, post(write::marker_advance))
        .with_state(Listener { shared, identity })
        .layer(axum::middleware::from_fn_with_state(door, request_log::log))
}

/// How one listener identifies its callers.
#[derive(Clone)]
pub enum Identity {
    /// Cloud Run ingress: a Google identity token for this server's audience.
    Google(Arc<identity::Google>),
    /// The Tailscale sidecar on loopback: the login header it sets for a user device.
    Tailscale,
}

impl Identity {
    /// The door of the request log this listener is.
    const fn door(&self) -> request_log::Door {
        match self {
            Self::Google(_) => request_log::Door::IdentityToken,
            Self::Tailscale => request_log::Door::Tailnet,
        }
    }
}

/// One listener's state: the shared tree and the identity rule of its port.
#[derive(Clone)]
pub struct Listener {
    pub shared: Arc<Shared>,
    pub identity: Identity,
}

impl Listener {
    /// Identifies the caller of one request, by the rule of the port it arrived on.
    ///
    /// # Errors
    ///
    /// Returns an error when the request carries no identity this listener accepts.
    pub async fn caller(&self, headers: &HeaderMap) -> Result<Caller> {
        match &self.identity {
            Identity::Google(google) => google.caller(headers).await,
            Identity::Tailscale => identity::tailscale(headers),
        }
    }
}

/// How many completed fetch windows the tree's data directory holds.
///
/// # Errors
///
/// Returns an error when the data directory cannot be resolved or read.
pub fn window_count(context: &Context) -> Result<usize> {
    let data = context.data()?;
    Ok(windows::after(&data.windows(), None)?.len())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;

    use super::{index_windows, panic_line, Places, Shared};
    use crate::context::Context;
    use crate::pointer::Pointer;
    use crate::store::Store;

    /// A panic's report is one line whatever its message holds, names where the panic
    /// happened, and carries no credential.
    #[test]
    fn a_panic_is_reported_in_one_line() {
        // The token is built from parts so the source holds no literal that secret scanning
        // reads as a live token.
        let message = format!(
            "failed\nat line two\u{1b}[2J with ghp_{}",
            "A1b2C3d4E5f6G7h8I9j0K1l2"
        );
        let line = panic_line(Some(message.as_str()), Some("src/server/write.rs:42:7"));
        assert!(
            line.starts_with("the server panicked at src/server/write.rs:42:7: failed at line two"),
            "{line}"
        );
        assert!(!line.chars().any(char::is_control), "{line}");
        assert!(
            line.contains("[redacted]") && !line.contains("ghp_"),
            "{line}"
        );
        assert_eq!(
            panic_line(None, None),
            "the server panicked: a payload that is not text"
        );
    }

    /// A server directory is replaced only when it is a path of the server's, not when its
    /// bytes stand inside a caller's own path.
    #[test]
    fn a_server_path_is_hidden_only_as_a_complete_path_prefix() {
        let places = Places(vec![("/srv/ctx/tree".to_owned(), "<tree>")]);
        assert_eq!(
            places.hide("reading /srv/ctx/tree/decisions/note.md: denied"),
            "reading <tree>/decisions/note.md: denied"
        );
        assert_eq!(places.hide(r#"path="/srv/ctx/tree""#), r#"path="<tree>""#);
        assert_eq!(
            places.hide("/client/srv/ctx/tree-notes.md"),
            "/client/srv/ctx/tree-notes.md"
        );
        assert_eq!(
            places.hide("/srv/ctx/tree-notes.md"),
            "/srv/ctx/tree-notes.md"
        );
    }

    /// A handler that panics while it holds the tree lock poisons the lock. Every later
    /// request used to be refused for the life of the process; the next holder recovers the
    /// tree instead.
    #[test]
    #[allow(
        clippy::panic,
        reason = "the test is a handler that panics while it holds the lock"
    )]
    fn a_panic_under_the_tree_lock_does_not_stop_the_server() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ctx-poisoned-tree-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("ctx.toml"), "version = 1\nid = \"example\"\n")?;
        std::fs::write(root.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
        let shared = Arc::new(Shared::new(Context::open(&root)?)?);
        let holder = Arc::clone(&shared);
        let panicked = std::thread::spawn(move || {
            let _tree = holder.tree();
            panic!("a handler failed while it held the tree");
        })
        .join();
        let after = shared.tree().map(|tree| tree.tree.root.clone());
        let again = shared.tree().map(|tree| tree.tree.root.clone());
        std::fs::remove_dir_all(&root)?;

        assert!(panicked.is_err(), "the handler did not panic");
        assert_eq!(after.ok().as_deref(), Some(root.as_path()));
        assert_eq!(again.ok().as_deref(), Some(root.as_path()));
        Ok(())
    }

    /// A copy gives its lease up as it stops. A write that reaches the tree after that — one
    /// whose caller hung up, still queued behind the write before it — would take the lease
    /// again and write beside the copy that replaced this one, so it is refused.
    #[test]
    fn a_copy_that_gave_its_lease_up_writes_nothing_more() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ctx-released-lease-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("ctx.toml"), "version = 1\nid = \"example\"\n")?;
        std::fs::write(root.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
        let shared = Shared::new(Context::open(&root)?)?;
        let before = shared.hold_lease();
        shared.release_lease();
        let after = shared.hold_lease();
        std::fs::remove_dir_all(&root)?;

        assert!(before.is_ok(), "a running copy may write: {before:?}");
        assert!(
            after.is_err(),
            "a copy that gave its lease up took it again"
        );
        Ok(())
    }

    /// After a failed rollback in a tree a person keeps, the refused record stays on disk.
    /// Recovering the tree after a later handler panicked read the tree back from that disk
    /// and served the record (round-4 review of d5655fc, N4-3); it keeps the tree it had until
    /// the tree holds nothing the remote lacks.
    #[test]
    #[allow(
        clippy::panic,
        reason = "the test is a handler that panics while it holds the lock"
    )]
    fn panic_recovery_after_a_failed_rollback_never_serves_what_it_left() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ctx-tainted-tree-{}", std::process::id()));
        std::fs::create_dir_all(root.join("decisions"))?;
        std::fs::write(root.join("ctx.toml"), "version = 1\nid = \"example\"\n")?;
        std::fs::write(root.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
        let git = |arguments: &[&str]| -> Result<()> {
            let status = std::process::Command::new("git")
                .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com"])
                .args(arguments)
                .current_dir(&root)
                .status()?;
            anyhow::ensure!(status.success(), "git {arguments:?} failed");
            Ok(())
        };
        git(&["init", "--quiet", "--initial-branch=main"])?;
        git(&["add", "ctx.toml", "rules.md"])?;
        git(&["commit", "--quiet", "-m", "seed"])?;
        let shared = Arc::new(Shared::new(Context::open(&root)?)?);
        let stray = root.join("decisions/zeppelin.md");
        std::fs::write(&stray, "---\ntype: decision\nstatus: open\n---\nRefused.\n")?;
        shared.taint();
        let panic_under_the_lock = || {
            let holder = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _tree = holder.tree();
                panic!("a handler failed while it held the tree");
            })
            .join()
            .is_err()
        };
        let serves = |shared: &Shared| -> Result<bool> {
            Ok(shared
                .tree()?
                .tree
                .docs
                .iter()
                .any(|document| document.rel.ends_with("zeppelin.md")))
        };
        let first_panicked = panic_under_the_lock();
        let served_while_left = serves(&shared);
        let tainted_while_left = shared.tainted();
        std::fs::remove_file(&stray)?;
        let second_panicked = panic_under_the_lock();
        let tainted_after = {
            let _tree = shared.tree()?;
            shared.tainted()
        };
        std::fs::remove_dir_all(&root)?;

        assert!(first_panicked && second_panicked, "a handler did not panic");
        assert!(
            !served_while_left?,
            "recovery read back the record a failed rollback left"
        );
        assert!(
            tainted_while_left,
            "the mark went while the record was there"
        );
        assert!(!tainted_after, "the mark stayed once the tree was clean");
        Ok(())
    }

    /// The share is indexed with the tree free. A read takes the tree, and a pass that held
    /// it while it replayed every window on the share made every read wait behind it: on
    /// 2026-11-01 a restarted copy of the deployment held it for 157 s, and the two reads a
    /// scheduled session opened with waited 163 s.
    #[test]
    fn the_share_is_indexed_while_something_else_holds_the_tree() -> Result<()> {
        let id = format!("ctx-index-off-the-lock-{}", std::process::id());
        let root = std::env::temp_dir().join(&id);
        let data = root.join("data");
        let window = data.join("windows/20260102T000000Z-0000");
        std::fs::create_dir_all(&window)?;
        std::fs::write(
            root.join("ctx.toml"),
            format!(
                "version = 1\nid = \"{id}\"\ndata_dir = \"{}\"\n",
                data.display()
            ),
        )?;
        std::fs::write(root.join("rules.md"), "---\ntype: rules\n---\n# Rules\n")?;
        std::fs::write(
            window.join("manifest.json"),
            "{\"version\":1,\"window\":\"20260102T000000Z-0000\",\"generated_at\":\"2026-01-02T00:00:00Z\",\"window_end\":\"2026-01-02T00:00:00Z\",\"sources\":{},\"warnings\":[],\"exit_status\":\"ok\"}\n",
        )?;
        std::fs::write(
            window.join("records.jsonl"),
            "{\"source\":\"echo\",\"record\":{\"pointer\":\"echo:C0EXAMPLE01/held\",\"stream\":\"C0EXAMPLE01\",\"ts\":\"2026-01-01T23:00:00Z\",\"text\":\"indexed while the tree was held\"}}\n",
        )?;
        std::fs::write(window.join("rendered.md"), "# window\n")?;

        let shared = Shared::new(Context::open(&root)?)?;
        let state = shared.state().root().to_path_buf();
        let held = shared.tree()?;
        let indexed = index_windows(shared.data(), shared.state());
        drop(held);
        let found = Store::load(shared.data(), shared.state()).and_then(|store| {
            store.get(&Pointer {
                scheme: "echo".to_owned(),
                locator: "C0EXAMPLE01/held".to_owned(),
            })
        });
        std::fs::remove_dir_all(&root)?;
        std::fs::remove_dir_all(&state)?;

        indexed?;
        assert!(
            found?.is_some(),
            "the window was not indexed while the tree was held"
        );
        Ok(())
    }
}
