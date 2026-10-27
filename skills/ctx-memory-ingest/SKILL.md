---
name: ctx-memory-ingest
description: "Use when running an ingest cycle into a ctx-memory tree: fetching configured sources, converting meetings, chat, email and documents into records, or backfilling history. The tree is found via CTX_MEMORY or ctx.toml."
---

# ctx-memory ingest

One ingest cycle reads a fetched window of source material, turns what is new into
records, verifies the tree, commits its own changes, and only then advances the source
cursors. The cycle records knowledge and follow-up work; it never sends messages to
people.

Read the tree's own rules first with `ctx rules`, and follow the `ctx-memory` skill for
quotes, tiers and the not-found protocol. A tree's rules page and its ingest notes can
add local rules; they bind this procedure.

## Invocation and state

A cycle runs either from a scheduler (a timer or CI job that calls `ctx fetch` and then
starts the agent) or by hand. Two cycles never run at once on one tree: `ctx fetch`
refuses while another fetch holds the tree's lock, and a manual cycle waits for a
scheduled one to finish.

`ctx --json state path` prints the tree's state directory (`state_dir`). Each window is
`<state_dir>/windows/<window id>/`; the local message store is `store.sqlite3`; and
`cursors.json` appears after the first `ctx cursor advance`. Never edit cursor state by
hand: `ctx --json cursor show` reads it (plain `ctx cursor show` prints nothing while no
cursor exists) and `ctx cursor advance <window>` moves it.

If a scheduler started the cycle and owns cursor advancement, the agent leaves cursors
alone and the scheduler advances them after it confirms the cycle's commit. Otherwise the
agent runs `ctx cursor advance <window>` as the last step.

## Boundaries

- The cycle writes the tree and its own local state. It does not post chat messages,
  send email, comment on code review, or write anything else a person reads.
- Only configured sources are citable. A citation points to committed tree content or to
  a pointer `ctx resolve` can answer.
- A source error leaves that source's cursors unchanged. Record the error; do not skip
  the source silently or advance past it.
- A missing artifact is a gap to record, not a fact to fill in.
- Never write credentials or secrets into the tree. Apply the rules page's audience
  rules before repeating tree content anywhere outward.

## One writer, read-only helpers

The cycle owner is the only tree writer. It mints, appends, supersedes, closes and
commits. Helpers (subagents or lanes) may read the window and the tree and return draft
records or evidence; they never edit the tree or the state directory.

## Sources

The tree's `ctx.toml` is the source list of record (`docs/sources.md` describes the
format). Built-in sources cover Slack, Gmail, Google Drive and Google Calendar; other
sources plug in as `ctx-source-<kind>` executables. A source can also discover streams
from a frontmatter field on records (`discover_field`), for example a per-person channel.

Typical tiers: a counterparty's own message or a signed document is tier 1; a colleague's
own message is tier 2; generated meeting notes and summaries are tier 3; a secondhand
account is tier 4. Calendar is the authoritative attendee list for a meeting. Ignore
automated notification noise unless a person acts on it.

## Per-cycle procedure

1. Run `ctx rules`, then `ctx --json cursor show`.
2. Get the window. A scheduler names the window it fetched; use it and do not fetch
   again. By hand, run `ctx --json fetch` and note the window id it prints. If any
   stream reports an error, stop: leave cursors unchanged and report the error to whoever
   runs the cycle (and in the cycle log, if the rules page names one). If every stream's
   `records` count in the manifest is zero, report that the cycle found nothing new and
   stop.
3. Read the window directory: `manifest.json` (per-stream status, record counts and
   proposed cursors), `records.jsonl` (one normalized record per message, with its
   `pointer`, `thread`, `ts`, `author` and `text`), `rendered.md` (the same material for
   reading) and `delta.md` (which existing records each new message touches). `delta.json`
   and `open-snapshot.json` are the machine-readable forms `ctx` uses for later deltas;
   leave them alone. For a message the delta matches to a record, update, supersede or
   close that record instead of minting a new one. A message re-delivered under a pointer
   the tree already cites still counts as a new message, and `delta.md` shows it matched
   to the record that cites it.
4. Convert meetings into one record per meeting. Keep notes and transcript as separate
   sources, check notes against the transcript, and turn every action item into a
   commitment or an explicit disposition on the meeting record. For a meeting with only a
   transcript, read the whole transcript for assigned actions. Speaker labels from
   diarization are provisional: establish who said what from the attendee list and the
   transcript's own content, and mark attribution uncertain when the evidence does not
   settle it.
5. Cite each message by the `pointer` field of its line in `records.jsonl`, copied
   exactly (for example `slack:<channel>/<ts>`, `gmail:<id>`, or a third-party source's
   own scheme). For a record about a whole thread, cite the line's `thread` pointer. Do
   not copy raw messages into the tree.
6. Extract decisions, commitments, questions, metrics and entities, each with its owner,
   dates, status, a 2-20 word verbatim quote, the source pointer and a tier. Before
   minting, search for the entity, event or open item (`ctx search`, `ctx dossier`,
   `ctx open`) and update it when it exists. Link every new record from a parent or
   index so it is reachable.
7. Record the cycle in the tree's cycle log, if the rules page names one: the window id,
   counts, and what was minted, updated and closed, as wikilinks. Facts go in records,
   not in the log.
8. Verify. Check each new quote against its source with `ctx resolve <pointer>`, adding
   `--thread` when the record cites a thread pointer, then run
   `ctx cite verify <changed paths>` and `ctx sweep`. A structural sweep finding blocks
   the commit; stale items do not.
9. Commit only the files this cycle changed, by explicit path. Put a line
   `window: <id>` in the commit message, with the window id exactly as `ctx fetch`
   printed it, so the cycle that handled a window can always be found. If the tree has a
   remote, push the commit where its workflow says (a branch or a pull request) and
   never force-push.
10. Advance cursors. When no scheduler owns this step, run `ctx cursor advance <id>`
    once the commit exists (and is pushed, when the tree has a remote). `ctx` does not
    check for the commit, so this order is the cycle's job. It moves only the streams
    that succeeded in that window, repeating it for the same window changes nothing, and
    it refuses a window older than the one that last moved a stream.

## Record rules

- A commitment is a person's own undertaking in a source. A system alert or someone
  describing a problem is an open question, not a commitment.
- The owner of a commitment is the person who undertook it, never a recipient, someone
  copied, or someone mentioned.
- A new signal updates the record or open item it belongs to before a new record is
  created.
- Keep superseded values with their citations; mark them superseded and link the
  replacement.
- Give open decisions, commitments and questions a `due` or `revisit_by` date.
- When a record carries a current-state date, use the date of its newest underlying
  fact, not the date the cycle ran.

## When a cycle fails

- An interrupted cycle has not advanced cursors, so the next cycle re-reads the same
  window. Do not advance cursors by hand to get past a failure.
- A scheduler that runs cycles should log each run and treat a failed run, or a log that
  stops growing while the schedule is enabled, as an outage to fix rather than a quiet
  period.
