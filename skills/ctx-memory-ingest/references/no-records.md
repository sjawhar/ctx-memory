# A window that needed no record

Read this when a window or page you worked held new messages (`new_messages` above zero) and
you wrote no record for any of them.

Writing nothing is often right: a greeting, a question already answered, a receipt. But no
later cycle reads a window the marker has passed, nor a page already recorded, so a move that
passes new messages in silence is how material is lost for good, and a cycle that made one is
failed for skipping them. Say which it is in the move itself, with `--no-records` and the one
word that fits:

- `nothing-new`: the messages carry no decision, commitment, question, metric or fact about a
  person or project.
- `not-a-record`: each is of a kind the ingest skill's "Not a record" names, such as a payment
  receipt, a spend alert or pipeline bookkeeping.
- `already-recorded`: the records the tree holds already carry what they say, unchanged.

So, for a window, `ctx marker advance <that window id> --no-records nothing-new`, and for a
page, `ctx marker page <window id> '<page.ends_at>' --no-records nothing-new`. The flag
changes nothing else the command does, and the word is the only text it takes: no sentence of
your own, and nothing a message said.

A window with no new message takes no declaration, nor does one you wrote or updated a record
for. Declare a window one message of which deserves a record only after you have written that
record, never instead of writing it, and never to get past a failure: the declaration says you
read those messages and found nothing a record would keep. The command you ran, declaration and
all, stays in the cycle's transcript, and the cycle's log names each window you declared and the
word you declared it with.
