---
name: ctx-memory
description: "Use when answering a question the team memory may hold or recording a durable fact; the tree is found via CTX_MEMORY or ctx.toml."
---

# ctx-memory

A memory tree is a graph of markdown records with YAML frontmatter and wikilinks. It is not injected into an agent session. Consult this skill and the `ctx` CLI when a durable fact or prior decision may matter.

## Read the tree

1. Run `ctx rules` first. The root `rules.md` defines the tree's local rules.
2. Search before answering:
   - `ctx search "<terms>"` ranks direct sources above summaries. Use `--verbatim` when only direct sources can answer the question.
   - `ctx dossier "<entity>"` follows the graph around one entity.
   - `ctx open` lists open decisions, commitments, and questions.
3. Cite each answer with the record path and a verbatim quote. Use `ctx resolve <pointer>` when a citation needs its source text.

Treat the tree as a durable record, not a live-system dashboard. Use the record's named source system when the question requires live state.

## Authority tiers

Use numeric tiers as the canonical form:

| Tier | Name | Label | Meaning |
| --- | --- | --- | --- |
| 1 | `external` (`counterparty` accepted) | `E1` | An external source's own words, verbatim. |
| 2 | `internal` (`principal` accepted) | `I2` | An internal source's own words, verbatim. |
| 3 | `summary` | `s3` | A generated or editorial summary. |
| 4 | `recollection` | `r4` | A secondhand paraphrase. |

Quoted strings `"1"` through `"4"` are also accepted. A tier 1 or 2 source outranks a conflicting summary or recollection.

## When a fact is not found

Absence from the tree is not evidence of absence elsewhere. Report `not found in the tree` and include:

- `searched:` the exact `ctx search` queries and relevant record or raw-source locations checked;
- where the fact would normally live, such as a record type or source system; and
- an open query-log entry when the tree's rules require one.

Do not convert an incomplete search into a negative claim.

## Write the tree

- Give every durable claim a distinctive verbatim quote of 2–20 words, its source pointer or location, and an authority tier.
- Keep superseded values. Mark the old value as superseded and link it to the record that replaced it.
- Give open decisions, commitments, and questions a `revisit_by` or `due` date.
- Link each new record from a parent or index so the graph remains reachable.
- Preserve raw sources and keep citations resolvable.
- Run `ctx sweep` before committing. Structural findings must be fixed; stale records are a review queue.

## Ingest safely

One writer owns a cycle. Read-only helpers may inspect material and draft recommendations, but only the writer changes records, creates links, closes items, or advances cursors.

Use this order: fetch, inspect the delta, connect a new fact to an existing record before minting a duplicate, verify quotes, run `ctx sweep`, commit, then run `ctx cursor advance <window>`. A fetch proposes cursors; it does not move them. Do not send messages or other outbound communication from an ingest cycle.
