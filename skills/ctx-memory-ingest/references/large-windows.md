# More than one answer holds

Read this when `ctx pending --json` answers with `later_windows` above zero or with a
`next_offset`.

When `later_windows` is above zero, more windows wait than the answer lists; handle the ones it
lists, and the next cycle gets the rest. When the answer has a `next_offset`, its delta lists
only the oldest part of a large window (`listed_messages` below `new_messages`): handle that
part, then run `ctx pending --json --offset <next_offset>`, with the same `--limit` if you gave
one, for the next, and move the marker past that window only after its last part.
