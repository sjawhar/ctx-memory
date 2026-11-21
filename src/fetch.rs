//! Fetch execution, staging, and durable window completion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use fs2::FileExt as _;

use crate::atomic::write_replace;
use crate::backfill;
use crate::config::{DataDir, SourceConfig, StateDir};
use crate::context::Context;
use crate::cursor::{self, AdvanceOutcome, Cursors};
use crate::delta::{self, Delta};
use crate::exclude::Exclusion;
use crate::lease::{self, Lease};
use crate::record::Record;
use crate::redact::one_line;
use crate::render;
use crate::replay::{self, Taken};
use crate::source::{
    self, FetchRequest, Request as SourceRequest, Session, SourceOutcome, SourceRef, StreamResult,
    StreamStatus,
};
use crate::store::Store;
use crate::windows::{
    self, Manifest, SourceManifest, SourcedRecord, Status, StreamManifest, Window,
};

/// Parameters controlling one fetch execution.
#[derive(Clone, Debug)]
pub struct FetchOptions {
    pub window_end: DateTime<Utc>,
    pub only: Vec<String>,
}

/// The published window, source results, and cursor movement from a fetch execution.
#[derive(Debug)]
pub struct FetchOutcome {
    pub published: Published,
    pub errored: Vec<(String, String)>,
    pub cursors: AdvanceOutcome,
}

/// Runs every selected configured source into one complete staged window, then moves the cursor
/// of every stream that stored its messages.
///
/// # Errors
///
/// Returns an error when the fetch runs as a Cloud Run job with no lease bucket, another fetch
/// holds the state lock, state artifacts cannot be persisted, or an invalid selected source is
/// requested. Source failures are retained in the returned outcome and its manifest.
pub fn fetch(context: &Context, options: &FetchOptions) -> Result<FetchOutcome> {
    let (_lock, lease) = become_the_writer(context)?;
    let state = context.state()?;
    let data = context.data()?;
    validate_selection(&context.config.sources, &options.only)?;
    let exclusion = Exclusion::configured(&context.config)?;

    let cursors = cursor::load(&data.cursors())?;
    // Read before the window is staged, so a file this build cannot read stops the fetch with
    // nothing staged.
    let mut backfills = backfill::load(&data.backfill())?;
    let staged = windows::stage(&data.windows(), options.window_end)?;
    let store = Store::load(data, state)?;
    // What the windows this store has not indexed yet delivered — another machine's fetches, or
    // a window whose indexing failed after it was published — counts as delivered too.
    store.catch_up(data)?;
    let writer = Publisher {
        context,
        data,
        store: &store,
        lease: lease.as_ref(),
        exclusion: &exclusion,
        schemes: source::configured_schemes(&context.config.sources)?,
        owners: delta::Owners::discover(&context.tree, &context.config.sources),
    };
    let generated_at = Utc::now();
    let mut collected = Collected::default();
    let reads = begin_backfills(context, data, options, &mut backfills)?;

    for (spec, span) in reads {
        check_lease(lease.as_ref(), &format!("fetching {}", spec.id))?;
        let result = run_source(
            context,
            data,
            spec,
            &span,
            &cursors,
            &writer.schemes,
            &exclusion,
        )?;
        collected.warnings.extend(span.warning(&spec.id, &result));
        collect_source_result(spec, result, &mut collected, &store, &exclusion)?;
    }

    let Collected {
        records,
        sources,
        warnings,
        errored,
    } = collected;
    let exit_status = if errored.is_empty() {
        Status::Ok
    } else {
        Status::Error
    };
    let manifest = Manifest {
        version: 1,
        window: staged.id.to_string(),
        generated_at,
        window_end: options.window_end,
        sources,
        warnings,
        exit_status,
        replay_of: None,
        replay_cohort: None,
    };
    let published = writer.publish(staged, &records, manifest)?;
    // The streams that stored their messages move their own cursors; a failed stream holds only
    // its own, so the next fetch re-reads only what this one did not store.
    check_lease(lease.as_ref(), "moving the cursors")?;
    let cursors = cursor::advance(&data.cursors(), &published.window.id, &published.manifest)?;
    // A source whose backfill reads are all stored reads the configured backfill next time.
    if backfills.finish(&published.manifest) {
        backfill::write(&data.backfill(), &backfills)?;
    }
    release_the_lease(lease);
    Ok(FetchOutcome {
        published,
        errored,
        cursors,
    })
}

/// A window this process published: its manifest and its delta.
#[derive(Debug)]
pub struct Published {
    pub window: Window,
    pub manifest: Manifest,
    pub delta: Delta,
}

/// What a fetch and a replay both publish a staged window with.
struct Publisher<'a> {
    context: &'a Context,
    data: &'a DataDir,
    store: &'a Store,
    lease: Option<&'a Lease>,
    exclusion: &'a Exclusion,
    schemes: crate::pointer::Schemes,
    owners: delta::Owners,
}

impl Publisher<'_> {
    /// Writes `records` and `manifest` into `staged`, computes and writes its delta against the
    /// window before it, publishes the window and indexes it in the store.
    fn publish(
        &self,
        staged: Window,
        records: &[SourcedRecord],
        manifest: Manifest,
    ) -> Result<Published> {
        check_lease(self.lease, "writing the window")?;
        write_window(&staged, records, &manifest)?;
        let previous = windows::previous(&self.data.windows(), &staged.id)?;
        let snapshot = delta::open_snapshot(&self.context.tree, manifest.generated_at);
        let delta = delta::compute(
            &self.context.tree,
            &self.schemes,
            &self.owners,
            self.exclusion,
            &staged,
            previous.as_ref(),
            manifest.generated_at,
            self.context.embedding()?.embedder(),
        )?;
        // Write delta.json, delta.md, and open-snapshot.json before completing the staged window.
        delta::write(&staged, &snapshot, &delta)?;
        // The compute can wait on Vertex AI for a long time, long enough for this copy to lose
        // the lease; a copy that lost it publishes nothing, so the window stays staged.
        check_lease(self.lease, "publishing the window")?;
        let window = windows::complete(staged, &self.data.windows())?;
        // The store learns what this window delivered only once the window is published: a
        // message whose delivered version the store holds is one no later fetch delivers as new,
        // so a run that stopped before publishing must leave nothing there. A store write that
        // fails here leaves the window unindexed, and the next fetch indexes it first.
        self.store.index_window(&window)?;
        Ok(Published {
            window,
            manifest,
            delta,
        })
    }
}

/// Fails unless this copy still holds the fetch lease, when it took one, naming what it was about
/// to do.
fn check_lease(lease: Option<&Lease>, before: &str) -> Result<()> {
    lease
        .map_or(Ok(()), Lease::check)
        .with_context(|| format!("before {before}"))
}

/// Makes this process the one writer of the data directory a fetch writes: the state
/// directory's lock, which keeps one machine single, and, where a lease bucket is set, the
/// fetch lease, which keeps the fetch job single across machines. Both are taken before anything
/// is staged, so a run that may not write writes nothing at all.
fn become_the_writer(context: &Context) -> Result<(File, Option<Lease>)> {
    // Cloud Scheduler can start the fetch job while its last execution still runs, and the two
    // executions share the data directory but not the `flock` below, which is per machine. With
    // no lease bucket neither would take a lease, so a fetch that runs as a Cloud Run job does
    // not start, and writes nothing. Cloud Run names the job it runs in `CLOUD_RUN_JOB`.
    if lease::bucket().is_none() && std::env::var_os("CLOUD_RUN_JOB").is_some() {
        bail!(
            "ctx fetch as a Cloud Run job needs CTX_LEASE_BUCKET: two executions can run at \
             once, and the fetch lease is what keeps one of them the only writer"
        );
    }
    let lock = acquire_lock(context.state()?)?;
    let lease = lease::for_role("fetch")?;
    Ok((lock, lease))
}

/// Lets the next run start at once rather than wait out the takeover window; a failure here
/// costs that run the wait and nothing else.
fn release_the_lease(lease: Option<Lease>) {
    if let Some(lease) = lease {
        if let Err(error) = lease.release() {
            eprintln!(
                "releasing the fetch lease: {}",
                one_line(&format!("{error:#}"))
            );
        }
    }
}

/// Parameters of one `ctx fetch --replay` run.
#[derive(Clone, Debug)]
pub struct ReplayOptions {
    /// The cohort file ([`crate::replay`]).
    pub cohort: PathBuf,
    /// The most messages a run keeps, past the first origin window that keeps one, which it
    /// takes whole however many it keeps.
    pub max_messages: usize,
    /// The most origin windows that keep a message a run takes.
    pub max_windows: usize,
}

/// What one replay run did.
#[derive(Debug)]
pub struct ReplayOutcome {
    /// The windows it wrote, oldest first: one for each origin window that kept a message.
    pub windows: Vec<Published>,
    /// The origin windows it took, in the cohort's order, those that kept no message among them.
    pub replayed: Vec<String>,
    /// How many cohort lines of those origin windows the deployment excludes.
    pub excluded: usize,
    /// The origin windows of the cohort that no run has replayed yet.
    pub remaining: usize,
}

/// Delivers again, as new messages in new windows, the messages of the next whole origin windows
/// of a cohort file that no run has replayed ([`crate::replay`]), and notes each origin window
/// it took in the cohort's progress file.
///
/// A message the deployment excludes is left out, and the store is not asked for it. Each other
/// message is written as the store holds it now ([`Store::get`]), not as its origin window held
/// it: a message that changed since is delivered as it is now, and indexing the new window does
/// not make an older version current again. It is new even when the store already delivered it
/// unchanged. One window is written for each origin window that keeps a message, as a fetch
/// writes one, with its delta, and it sorts after every window and after the marker. No source
/// is contacted and no cursor moves. A window a run published counts as replayed in every later
/// run of the same cohort, even when that run stopped before it could note it.
///
/// # Errors
///
/// Returns an error, and writes nothing, when another fetch holds the lock or the lease, the
/// cohort or its progress file cannot be read or belong to different cohorts, or a kept message
/// of the run is one the store does not hold. Returns an error when a window cannot be written.
pub fn replay(context: &Context, options: &ReplayOptions) -> Result<ReplayOutcome> {
    let (_lock, lease) = become_the_writer(context)?;
    let state = context.state()?;
    let data = context.data()?;
    let cohort = replay::Cohort::read(&options.cohort)?;
    let progress_path = data.replay(replay::cohort_name(&options.cohort)?);
    let mut progress = replay::load(&progress_path, &cohort.sha256)?;
    let exclusion = Exclusion::configured(&context.config)?;
    let store = Store::load(data, state)?;
    // What the windows this store has not indexed yet delivered is what the store holds now too.
    store.catch_up(data)?;
    let completed = windows::after(&data.windows(), None)?;
    if note_published(&completed, &cohort.sha256, &mut progress)? {
        check_lease(lease.as_ref(), "noting the replayed windows")?;
        replay::write(&progress_path, &progress)?;
    }
    // Every kept message of the run is looked up before any window is staged, so a message the
    // store does not hold stops the run with nothing written.
    let planned = cohort
        .batch(
            &progress,
            options.max_messages,
            options.max_windows,
            |pointer| !exclusion.matches(pointer),
        )
        .into_iter()
        .map(|taken| Ok((replay_records(&store, &taken)?, taken)))
        .collect::<Result<Vec<_>>>()?;
    let window_end = replay_window_end(data, completed.last())?;
    let writer = Publisher {
        context,
        data,
        store: &store,
        lease: lease.as_ref(),
        exclusion: &exclusion,
        schemes: source::configured_schemes(&context.config.sources)?,
        owners: delta::Owners::discover(&context.tree, &context.config.sources),
    };
    let mut written = Vec::new();
    let mut replayed = Vec::new();
    let mut excluded = 0;
    for (records, taken) in planned {
        let origin = taken.origin.window.to_string();
        if !records.is_empty() {
            let staged = windows::stage(&data.windows(), window_end)?;
            let manifest = Manifest {
                version: 1,
                window: staged.id.to_string(),
                generated_at: Utc::now(),
                window_end,
                sources: replay_sources(context, &records),
                warnings: Vec::new(),
                exit_status: Status::Ok,
                replay_of: Some(origin.clone()),
                replay_cohort: Some(cohort.sha256.clone()),
            };
            written.push(writer.publish(staged, &records, manifest)?);
        }
        check_lease(lease.as_ref(), "noting the replayed window")?;
        progress.replayed.insert(origin.clone());
        replay::write(&progress_path, &progress)?;
        excluded += taken.excluded;
        replayed.push(origin);
    }
    release_the_lease(lease);
    Ok(ReplayOutcome {
        windows: written,
        replayed,
        excluded,
        remaining: cohort.remaining(&progress),
    })
}

/// Notes in `progress` the origin window of each of `completed` that replays the cohort whose
/// SHA-256 is `cohort`, and answers whether that added one. The published windows are what the
/// runs did, whatever the progress file says: a run that stopped between publishing a window and
/// noting it must not deliver that origin window again.
fn note_published(
    completed: &[Window],
    cohort: &str,
    progress: &mut replay::Progress,
) -> Result<bool> {
    let mut added = false;
    for window in completed {
        let manifest = window.manifest()?;
        if manifest.replay_cohort.as_deref() == Some(cohort) {
            if let Some(origin) = manifest.replay_of {
                added |= progress.replayed.insert(origin);
            }
        }
    }
    Ok(added)
}

/// The window end a replay run stages its windows at: now, or a second after the newest window
/// or the window the marker holds when either ends later, so every replay window sorts after
/// both and `ctx pending` lists it.
fn replay_window_end(data: &DataDir, newest: Option<&Window>) -> Result<DateTime<Utc>> {
    let (_, held) = crate::marker::held(data)?;
    let mut end = Utc::now();
    for id in newest
        .map(|window| &window.id)
        .into_iter()
        .chain(held.as_ref())
    {
        end = end.max(id.time()? + chrono::Duration::seconds(1));
    }
    Ok(end)
}

/// The records the kept messages of one origin window are replayed as: each as the store holds
/// it now, from the source that delivered that version, and as a new message, ordered as a
/// fetch's window reads, by source, then time, then pointer.
fn replay_records(store: &Store, taken: &Taken<'_>) -> Result<Vec<SourcedRecord>> {
    let mut records = taken
        .kept
        .iter()
        .map(|pointer| {
            let (source, mut record) = store.get_sourced(pointer)?.ok_or_else(|| {
                anyhow!(
                    "replay of {}: the store holds no message {pointer}",
                    taken.origin.window
                )
            })?;
            // The store keeps a version as the first window that delivered it held it, which
            // may have been as context for a reply; replayed, it is a new message.
            record.context = false;
            Ok(SourcedRecord { source, record })
        })
        .collect::<Result<Vec<_>>>()?;
    records.sort_by(|left, right| {
        (
            left.source.as_str(),
            left.record.ts,
            left.record.pointer.as_str(),
        )
            .cmp(&(
                right.source.as_str(),
                right.record.ts,
                right.record.pointer.as_str(),
            ))
    });
    Ok(records)
}

/// The manifest entry of each source of a replay window's `records`: read, with every stream
/// that holds a record, and no cursor, because a replay reads no source. A source `ctx.toml` no
/// longer configures still delivered its messages; its kind is then unknown, and the entry names
/// the messages' pointer scheme in its place.
fn replay_sources(
    context: &Context,
    records: &[SourcedRecord],
) -> BTreeMap<String, SourceManifest> {
    let read = StreamResult {
        status: StreamStatus::Ok,
        cursor: None,
        error: None,
    };
    let mut by_source = BTreeMap::<&str, Vec<&Record>>::new();
    for sourced in records {
        by_source
            .entry(sourced.source.as_str())
            .or_default()
            .push(&sourced.record);
    }
    by_source
        .into_iter()
        .map(|(id, own)| {
            let spec = context.config.sources.iter().find(|spec| spec.id == id);
            let discovered = spec
                .and_then(|spec| spec.discover_field.as_deref())
                .map_or_else(Vec::new, |field| {
                    source::discover_streams(&context.tree, field)
                });
            let streams = own
                .iter()
                .map(|record| record.stream.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|stream| {
                    let manifest = stream_manifest(
                        stream,
                        &read,
                        &BTreeMap::new(),
                        &discovered,
                        own.iter().copied(),
                    );
                    (stream.to_owned(), manifest)
                })
                .collect();
            let kind = spec.map_or_else(
                || {
                    own.first()
                        .and_then(|record| record.pointer.split_once(':'))
                        .map_or_else(String::new, |(scheme, _)| scheme.to_owned())
                },
                |spec| spec.kind.clone(),
            );
            (
                id.to_owned(),
                SourceManifest {
                    kind,
                    status: Status::Ok,
                    error: None,
                    streams,
                },
            )
        })
        .collect()
}

pub(crate) fn acquire_lock(state: &StateDir) -> Result<File> {
    fs::create_dir_all(state.root())
        .with_context(|| format!("creating {}", state.root().display()))?;
    let path = state.fetch_lock();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            bail!("fetch already running")
        }
        Err(error) => Err(error).with_context(|| format!("locking {}", path.display())),
    }
}

fn validate_selection(sources: &[SourceConfig], only: &[String]) -> Result<()> {
    for source in only {
        if !sources.iter().any(|spec| spec.id == *source) {
            bail!("source {source} is not configured");
        }
    }
    Ok(())
}

/// Where one source's read in this fetch ends, and how many hours back its backfill reaches.
struct Span {
    window_end: DateTime<Utc>,
    backfill_hours: u32,
    /// Where a backfill read an earlier fetch did not store began, when the backfill reaches
    /// further back than configured to read it.
    unstored_since: Option<DateTime<Utc>>,
}

impl Span {
    /// The warning that names a backfill reaching further than configured. Only a stream with
    /// no cursor reads the backfill, so there is one only when such a stream ran.
    fn warning(&self, source: &str, run: &SourceRun) -> Option<String> {
        let start = self.unstored_since.filter(|_| run.read_backfill())?;
        Some(format!(
            "source {source}: its backfill reaches back to {}, where a read an earlier fetch did not store began",
            start.to_rfc3339_opts(SecondsFormat::AutoSi, true)
        ))
    }
}

/// Notes where each selected source's backfill starts, and writes it before any source runs, so
/// a fetch that stops before it publishes its window leaves the next one reading from there.
/// A source no longer configured keeps its start, as it keeps its cursors, for when it returns.
fn begin_backfills<'a>(
    context: &'a Context,
    data: &DataDir,
    options: &FetchOptions,
    backfills: &mut backfill::Backfills,
) -> Result<Vec<(&'a SourceConfig, Span)>> {
    let reads = context
        .config
        .sources
        .iter()
        .filter(|spec| options.only.is_empty() || options.only.contains(&spec.id))
        .map(|spec| {
            let reach = backfills.begin(
                &spec.id,
                options.window_end,
                context.config.ingest.backfill_hours,
            );
            let span = Span {
                window_end: options.window_end,
                backfill_hours: reach.hours,
                unstored_since: reach.unstored_since,
            };
            (spec, span)
        })
        .collect();
    backfill::write(&data.backfill(), backfills)?;
    Ok(reads)
}

fn run_source(
    context: &Context,
    data: &DataDir,
    spec: &SourceConfig,
    span: &Span,
    cursors: &Cursors,
    schemes: &crate::pointer::Schemes,
    exclusion: &Exclusion,
) -> Result<SourceRun> {
    let scheme = source::scheme(spec)?;
    let discovered = spec
        .discover_field
        .as_deref()
        .map_or_else(Vec::new, |field| {
            source::discover_streams(&context.tree, field)
        });
    let requested_cursors = cursors
        .sources
        .get(&spec.id)
        .map_or_else(BTreeMap::new, |streams| {
            streams
                .iter()
                .map(|(stream, entry)| (stream.clone(), entry.cursor.clone()))
                .collect()
        });
    let state_dir = data.source_state(&spec.id);
    fs::create_dir_all(&state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let request = SourceRequest::Fetch(FetchRequest {
        protocol: source::PROTOCOL_VERSION,
        source: SourceRef {
            id: spec.id.clone(),
            kind: spec.kind.clone(),
            scheme,
        },
        config: source::config_json(spec)?,
        window_end: span.window_end,
        backfill_hours: span.backfill_hours,
        cursors: requested_cursors.clone(),
        discovered: discovered.clone(),
        state_dir,
        exclude: source_exclude(spec, exclusion),
    });
    let mut session = Session::new(&request, schemes);
    let outcome = match source::open(spec, request.source().scheme.as_str()) {
        Ok(source) => {
            let run = source.run(&request, &mut |message| session.accept(message).map(|_| ()));
            session.finish(run)
        }
        Err(error) => session.finish(Err(error)),
    };
    Ok(SourceRun {
        cursors: requested_cursors,
        discovered,
        outcome,
    })
}

/// What one fetch execution accumulates across its sources.
#[derive(Default)]
struct Collected {
    records: Vec<SourcedRecord>,
    sources: BTreeMap<String, SourceManifest>,
    warnings: Vec<String>,
    errored: Vec<(String, String)>,
}

/// The exclusion patterns a source's request carries: a built-in source's reading skips what
/// they name. An external source gets none, because one written for protocol 1 may refuse a
/// request field it does not know; the fetch drops what the patterns name all the same.
pub(crate) fn source_exclude(spec: &SourceConfig, exclusion: &Exclusion) -> Vec<String> {
    if crate::config::default_scheme(&spec.kind).is_some() {
        exclusion.patterns().to_vec()
    } else {
        Vec::new()
    }
}

fn collect_source_result(
    spec: &SourceConfig,
    result: SourceRun,
    collected: &mut Collected,
    store: &Store,
    exclusion: &Exclusion,
) -> Result<()> {
    let SourceRun {
        cursors,
        discovered,
        outcome,
    } = result;
    if let Some(verbatim) = outcome.verbatim {
        store.set_source_verbatim(&spec.id, verbatim)?;
    }
    let failed = outcome.has_errors();
    let SourceOutcome {
        records: fetched,
        streams: results,
        warnings,
        error,
        ..
    } = outcome;
    for warning in &warnings {
        let stream = warning
            .stream
            .as_deref()
            .map_or_else(String::new, |stream| format!(" stream {stream}"));
        collected
            .warnings
            .push(format!("source {}{stream}: {}", spec.id, warning.message));
    }
    let fetched = fetched
        .into_iter()
        .filter(|record| !exclusion.matches(&record.pointer))
        .collect::<Vec<_>>();
    let held = store.unchanged(&fetched)?;
    let (records, repeated) = window_records(&fetched, &held);
    for (stream, count) in repeated {
        collected.warnings.push(format!(
            "source {} stream {stream}: {count} message(s) were delivered again unchanged since an earlier window and are not new",
            spec.id
        ));
    }

    let mut streams = BTreeMap::new();
    for (stream, outcome) in &results {
        if outcome.status == StreamStatus::Error {
            collected.errored.push((
                spec.id.clone(),
                outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("stream {stream} failed")),
            ));
        }
        streams.insert(
            stream.clone(),
            stream_manifest(stream, outcome, &cursors, &discovered, records.iter()),
        );
    }
    if let Some(error) = error.as_deref() {
        collected.errored.push((spec.id.clone(), error.to_owned()));
    }
    let status = if failed { Status::Error } else { Status::Ok };
    collected.sources.insert(
        spec.id.clone(),
        SourceManifest {
            kind: spec.kind.clone(),
            status,
            error,
            streams,
        },
    );
    collected
        .records
        .extend(records.into_iter().map(|record| SourcedRecord {
            source: spec.id.clone(),
            record,
        }));
    Ok(())
}

/// The records of `fetched` that go into the window, and how many of each stream's were left
/// out because the store already held them (`held`, in `fetched`'s order).
///
/// A message whose newest delivered version is this one was handed to the agent by an earlier
/// window, so it is not new: a source that reads a stretch of its feed again — a cursor held
/// after a stopped run, a listing restarted from its top — must not hand the agent the same
/// messages as new in every window. Such a message stays in the window, as context and not as
/// a new message, only when a new message of this fetch replies to it, so the reply keeps the
/// text it answers; otherwise it is left out. A changed message has a new version and is new
/// again. A record the source itself sent as context is kept as it is.
fn window_records(fetched: &[Record], held: &[bool]) -> (Vec<Record>, BTreeMap<String, usize>) {
    let answered = fetched
        .iter()
        .zip(held)
        .filter(|(record, held)| !**held && !record.context)
        .filter_map(|(record, _)| record.thread.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    let mut repeated = BTreeMap::<String, usize>::new();
    let mut records = Vec::with_capacity(fetched.len());
    for (record, held) in fetched.iter().zip(held) {
        if !*held || record.context {
            records.push(record.clone());
        } else if answered.contains(record.pointer.as_str()) {
            let mut context = record.clone();
            context.context = true;
            records.push(context);
        } else {
            *repeated.entry(record.stream.clone()).or_default() += 1;
        }
    }
    (records, repeated)
}

fn stream_manifest<'r>(
    stream: &str,
    outcome: &StreamResult,
    cursors: &BTreeMap<String, String>,
    discovered: &[source::DiscoveredStream],
    records: impl Iterator<Item = &'r Record> + Clone,
) -> StreamManifest {
    let matching = records.filter(|record| record.stream == stream);
    let label = matching.clone().find_map(|record| record.label.clone());
    let records = matching.count();
    let owner = discovered
        .iter()
        .find(|candidate| candidate.stream == stream)
        .map(|candidate| candidate.owner.clone());
    StreamManifest {
        status: match outcome.status {
            StreamStatus::Ok => Status::Ok,
            StreamStatus::Error => Status::Error,
        },
        error: outcome.error.clone(),
        cursor_used: cursors.get(stream).cloned(),
        cursor_next: outcome.cursor.clone(),
        records,
        label,
        owner,
    }
}

fn write_window(window: &Window, records: &[SourcedRecord], manifest: &Manifest) -> Result<()> {
    let mut jsonl = Vec::new();
    for record in records {
        serde_json::to_writer(&mut jsonl, record)
            .with_context(|| format!("serializing record {}", record.record.pointer))?;
        jsonl.push(b'\n');
    }
    write_replace(&window.dir.join("records.jsonl"), &jsonl)?;
    let manifest = serde_json::to_vec_pretty(manifest).context("serializing fetch manifest")?;
    write_replace(&window.dir.join("manifest.json"), &manifest)?;
    let rendered = render::window(records);
    write_replace(&window.dir.join("rendered.md"), rendered.as_bytes())
}

struct SourceRun {
    cursors: BTreeMap<String, String>,
    discovered: Vec<source::DiscoveredStream>,
    outcome: SourceOutcome,
}

impl SourceRun {
    /// Whether a stream of this run had no cursor, and so read the backfill.
    fn read_backfill(&self) -> bool {
        self.outcome
            .streams
            .keys()
            .any(|stream| !self.cursors.contains_key(stream))
    }
}
