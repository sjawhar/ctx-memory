//! Fetch execution, staging, and durable window completion.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt as _;

use crate::atomic::write_replace;
use crate::config::{DataDir, SourceConfig, StateDir};
use crate::context::Context;
use crate::cursor::{self, AdvanceOutcome, Cursors};
use crate::delta::{self, Delta};
use crate::exclude::Exclusion;
use crate::lease::{self, Lease};
use crate::record::Record;
use crate::redact::one_line;
use crate::render;
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

/// The completed window, source results, and cursor movement from a fetch execution.
#[derive(Debug)]
pub struct FetchOutcome {
    pub window: Window,
    pub manifest: Manifest,
    pub delta: Delta,
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
    let state = context.state()?;
    let data = context.data()?;
    let _lock = acquire_lock(state)?;
    // The flock keeps one machine single; the share has no locks, so the lease is what keeps
    // the fetch job single across machines. It is taken before anything is staged, so a run
    // that may not write writes nothing at all.
    let lease = lease::for_role("fetch")?;
    validate_selection(&context.config.sources, &options.only)?;
    let exclusion = Exclusion::configured(&context.config)?;

    let cursors = cursor::load(&data.cursors())?;
    let staged = windows::stage(&data.windows(), options.window_end)?;
    let store = Store::load(data, state)?;
    // What the windows this store has not indexed yet delivered — another machine's fetches, or
    // a window whose indexing failed after it was published — counts as delivered too.
    store.catch_up(data)?;
    let schemes = source::configured_schemes(&context.config.sources)?;
    let generated_at = Utc::now();
    let mut collected = Collected::default();

    for spec in context
        .config
        .sources
        .iter()
        .filter(|spec| options.only.is_empty() || options.only.contains(&spec.id))
    {
        lease
            .as_ref()
            .map_or(Ok(()), Lease::check)
            .with_context(|| format!("before fetching {}", spec.id))?;
        let result = run_source(context, data, spec, options, &cursors, &schemes, &exclusion)?;
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
    };
    lease
        .as_ref()
        .map_or(Ok(()), Lease::check)
        .context("before writing the window")?;
    write_window(&staged, &records, &manifest)?;
    let previous = windows::previous(&data.windows(), &staged.id)?;
    let owners = delta::Owners::discover(&context.tree, &context.config.sources);
    let snapshot = delta::open_snapshot(&context.tree, generated_at);
    let delta = delta::compute(
        &context.tree,
        &schemes,
        &owners,
        &exclusion,
        &staged,
        previous.as_ref(),
        generated_at,
        context.embedding()?.embedder(),
    )?;
    // Write delta.json, delta.md, and open-snapshot.json before completing the staged window.
    delta::write(&staged, &snapshot, &delta)?;
    // The compute can wait on Vertex AI for a long time, long enough for this copy to lose the
    // lease; a copy that lost it publishes nothing, so the window stays staged.
    lease
        .as_ref()
        .map_or(Ok(()), Lease::check)
        .context("before publishing the window")?;
    let window = windows::complete(staged, &data.windows())?;
    // The store learns what this window delivered only once the window is published: a message
    // whose delivered version the store holds is one no later fetch delivers as new, so a run
    // that stopped before publishing must leave nothing there. A store write that fails here
    // leaves the window unindexed, and the next fetch indexes it first.
    store.index_window(&window)?;
    // The streams that stored their messages move their own cursors; a failed stream holds only
    // its own, so the next fetch re-reads only what this one did not store.
    lease
        .as_ref()
        .map_or(Ok(()), Lease::check)
        .context("before moving the cursors")?;
    let cursors = cursor::advance(&data.cursors(), &window.id, &manifest)?;
    if let Some(lease) = lease {
        // The next run starts at once rather than waiting out the takeover window; a failure
        // here costs that run the wait and nothing else.
        if let Err(error) = lease.release() {
            eprintln!(
                "releasing the fetch lease: {}",
                one_line(&format!("{error:#}"))
            );
        }
    }
    Ok(FetchOutcome {
        window,
        manifest,
        delta,
        errored,
        cursors,
    })
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

fn run_source(
    context: &Context,
    data: &DataDir,
    spec: &SourceConfig,
    options: &FetchOptions,
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
        window_end: options.window_end,
        backfill_hours: context.config.ingest.backfill_hours,
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
            stream_manifest(stream, outcome, &cursors, &discovered, &records),
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

fn stream_manifest(
    stream: &str,
    outcome: &StreamResult,
    cursors: &BTreeMap<String, String>,
    discovered: &[source::DiscoveredStream],
    records: &[Record],
) -> StreamManifest {
    let matching = records.iter().filter(|record| record.stream == stream);
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
