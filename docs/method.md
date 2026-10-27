# The ctx-memory method

`ctx-memory` keeps durable team knowledge as typed Markdown records connected by wikilinks. The graph, not a hand-written briefing, is the memory. The root `rules.md` is one short page of operating rules: it has `type: rules`, stays below 8,192 bytes, and contains no time-ordered account of current state. Use `ctx open`, `ctx search`, and `ctx dossier` to learn current state from records.

## Authority and provenance

Every durable claim carries a source, a verbatim quote, and an authority tier. The quote is a distinctive 2–20 word excerpt that a reader can verify at the source. A source pointer should remain resolvable through `ctx resolve` and `ctx cite verify`.

Use numeric tiers as the canonical representation:

| Tier | Accepted name | Output label | Meaning |
| --- | --- | --- | --- |
| 1 | `external` or legacy `counterparty` | `E1` | An external source's own words, verbatim. |
| 2 | `internal` or legacy `principal` | `I2` | An internal source's own words, verbatim. |
| 3 | `summary` | `s3` | A generated or editorial summary. |
| 4 | `recollection` | `r4` | A secondhand paraphrase. |

Quoted numerals `"1"` through `"4"` are accepted. Tiers 1 and 2 are direct sources. They outrank summaries and recollections when records conflict. A summary is useful navigation, but it does not replace its source.

Keep superseded values and their citations. Mark the old value as superseded, add the replacement and its evidence, and link the records. Deleting the old value discards the explanation for the current one.

## Open work and honest absence

Open decisions, commitments, and questions receive a `revisit_by` or `due` date. `ctx sweep` reports two distinct classes of result:

- Structural findings, including broken links, orphan records, missing required provenance, rule-page defects, and schema violations, make the command exit nonzero and block a commit.
- Stale open items are a review queue. They remain visible without making the structural gate fail.

A missing answer needs a `not_found` record, not a claim that the fact does not exist. Record `searched:` with the exact queries, paths, and source surfaces checked; identify where the fact would normally live; and log the query according to the tree's rules. Search the relevant raw material before making an absence claim. Raw source material is retained rather than pruned, and citations must continue to resolve to it.

## Ingest loop

An ingest cycle has one writer. Read-only helpers may inspect source material and draft findings, but the writer alone creates records, updates links, supersedes values, closes items, and advances cursors.

Run the cycle in this order:

1. Fetch sources into a window.
2. Read the delta and source material.
3. Connect each fact to an existing record before minting a new record.
4. Verify every selected quote against its source.
5. Run `ctx sweep` and fix structural findings.
6. Commit the tree changes.
7. Run `ctx cursor advance <window>`.

A fetch proposes a cursor; it does not advance it. Advance only after the tree commit that durably handles the window, so an interrupted cycle remains recoverable. The ingest loop records knowledge and follow-up work; it does not send outbound messages. The step-by-step procedure an agent follows is [skills/ctx-memory-ingest/SKILL.md](../skills/ctx-memory-ingest/SKILL.md).
