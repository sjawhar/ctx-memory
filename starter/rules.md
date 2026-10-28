---
type: rules
title: Tree rules
---

# Tree rules

- Treat this tree as a graph of typed records. Link each new record from a parent or index.
- Record every durable claim with a verbatim quote of at most about 20 words, its source, and an authority tier. Each part of a quote between `...` needs at least 3 words or 12 letters and digits.
- A tier 1 or 2 claim names a pointer that a configured source storing its records verbatim resolves, so `ctx cite verify` can check it. A claim with free-text provenance is a tier 3 summary or a tier 4 recollection.
- Prefer tier 1 or 2 verbatim sources over summaries. Keep superseded values with their history.
- For an absence, record what was searched and where the fact would normally live; absence from the tree is not evidence of absence elsewhere.
- Give open decisions, commitments, and questions a `revisit_by` or `due` date.
- Keep raw source material available so citations continue to resolve.
- Run `ctx sweep` and `ctx cite verify` before committing tree changes.

Use `ctx open`, `ctx search`, and `ctx dossier` to inspect current state. [[index]] contains sample records only.
