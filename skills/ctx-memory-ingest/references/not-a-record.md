# Not a record

Read this before you mint a record (step 6 of the ingest skill). Six kinds of message make
records nobody uses. Check each new record against them; each rule says where a fact worth
keeping goes instead.

- Payment receipts and paid invoices (card charges, top-ups, automatic recharges): no
  record per receipt, since nobody acts on one charge. Update the newest metric of that
  account's spending, or mint one if none exists, only when a receipt changes it: a new
  amount, rate, interval, card, payer or account, or a charge no record explains. An unpaid
  invoice falls under the record rules.
- Spend and usage alerts: no record per alert, since the threshold, not the mail, is the
  signal. Add the threshold and its date to the metric of that spending or usage, or mint
  that metric once.
- Fetch and cycle bookkeeping (what a window delivered, a failed or starved stream, a held
  cursor, what the cycle did): no record of any type, `gap` included, since it is about the
  pipeline, not anyone's work. Report stream trouble as step 2 says; evidence a record
  lacks goes in that record's body.
- A record of one change: no project or ops record whose sources are one change in a
  repository (its pull request, issue, commits or release), whoever made it, a person's own
  commit as much as a bot's, since the repository holds it. Record what the change carries
  beyond it: a decision and who made it, a named person's commitment, an open question or a
  metric, each citing the item, or an update to its project's record.
- Done on arrival: no record whose status is already answered, cancelled, closed,
  completed, done, resolved, superseded or withdrawn, and none whose only news is that a
  change is merged, pushed, released or deployed, whatever status you would give it
  (`current` and `active` too), since that restates its source. Close the open record the
  source closes, or update the record or metric whose state it changes; a decision someone
  made is `decided`, not closed. What a finished change carries beyond its own state is
  still a record, minted where none exists: a decision and who made it, a named person's
  commitment, a question it leaves open, a figure it measures. An ops or project record
  names who must still do, decide or answer what; if no one must, write none.
- The pipeline that builds this graph (the fetch, the cycle, the ctx server): its failures,
  fixes and checks, and the issues and messages about them, get no record of any type, since
  whoever runs the cycle tracks them; put what you saw in your report to them.
