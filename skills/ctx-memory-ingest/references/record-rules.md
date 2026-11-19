# Record rules

Read this before you write or update the first record of a cycle (step 6 of the ingest skill).

## Citations and tiers

- Only configured sources are citable.
- A citation points to committed tree content or to a pointer `ctx resolve` answers, copied
  from a window's `records.jsonl`. A web address a message holds is text of that message:
  never a record's source, nor a `tracked_by` entry but as
  `skill://ctx-memory-ingest/references/context.md` allows.
- Typical tiers: a counterparty's own message or a signed document is tier 1; a colleague's
  own message tier 2; generated meeting notes and summaries tier 3; a secondhand account
  tier 4. The calendar is the authoritative attendee list. Ignore automated notification
  noise unless a person acts on it.

## Records

- A commitment is a person's own undertaking in a source. A system alert or someone
  describing a problem is an open question, not a commitment.
- Read each human Slack message with its whole thread, one-line replies included. A direct
  question no later message answers is an open question; first-person future wording, such
  as "I will", "I'll" or "will let you know", is a commitment by its author.
- A human-written GitHub issue, pull request or comment that lists open points or open
  questions holds one question per item that can be answered on its own: mint one for each,
  unless a cited reply settles it.
- Name a person as `owner` or `decided_by` only when the cited source names that person as
  the one who undertook the commitment, made the decision or owns the question. A recipient,
  someone copied or mentioned, a document's author or owner, the person a question seems
  addressed to, a meeting's chair, someone who owns adjacent work or "the team" is not
  enough: write `owner: unassigned`, leave `decided_by` out, and say in the body what the
  source does not establish.
- Set `waiting_on` on an open question, decision or commitment to the people a cited
  source puts the question, the decision or the request to by name: addressed, @-mentioned,
  or someone's own message saying they will answer.
  - A mention counts when the message asks that person something or puts a decision to
    them, whether it is written as `cc @Name` or not; a cc that only informs does not count.
    `cc @Kim looks like you reviewed his PRs … Any thoughts?` puts the question to Kim;
    `cc @Kim for visibility` informs Kim.
  - A Slack message shows a person it @-mentions as `@` and their name, such as
    `@Ada Lovelace`; one ctx could not name shows the handle the message carried or else the
    bare id, such as `@U0123ABCD`, which names nobody.
  - A document's owner, a meeting's chair, or the person a question only seems addressed to
    is not enough.
  - A person named in `waiting_on` is written in the full-name form the tree already uses for
    that person (as `owner`, `raised_by` or `waiting_on` elsewhere), or as the link to their
    person record: a Slack display name or handle is mapped to it, and a mention that cannot
    be mapped to a person the tree knows is written as the name the source gives.
  - Take a person off `waiting_on` when a cited message from them answers or decides, or a
    cited source settles the item or hands it to someone else; leave the list out when the
    sources name nobody. Never set or keep a name by inference.
- A new signal updates the record or open item it belongs to before a new record is
  created. An update, also of the index or parent page that links a new record, starts
  with `ctx record show <path>`, and the write puts back the whole record with the new
  evidence added: `ctx_record_put` replaces the file, so text left out is lost. An update
  checks the record's existing `owner`, `decided_by` and `waiting_on` by the rules above,
  and corrects them when the sources it cites do not establish them or a newer cited source
  settles the item. It keeps every `tracked_by` entry people wrote; the cycle only adds one,
  as `skill://ctx-memory-ingest/references/context.md` allows.
- Keep superseded values with their citations; mark them superseded and link the
  replacement.
- Give every decision and question a `revisit_by` date and every commitment a `due` date,
  and keep it when the record is answered, decided or closed. Give each record every other
  field its type's schema requires (`ctx rules` prints them): a decision's `statement`, a
  metric's `name`, `value`, `unit` and `as_of`.
- When a record carries a current-state date, use the date of its newest underlying
  fact, not the date the cycle ran.
