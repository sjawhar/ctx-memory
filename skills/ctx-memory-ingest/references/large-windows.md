# More than one answer holds

Read this when `ctx pending --json` answers with `later_windows` above zero, or lists a window
with a `page`.

When `later_windows` is above zero, more windows wait than the answer lists; handle the ones it
lists, and the next cycle gets the rest.

A window listed with a `page` holds more new messages than one answer lists: more than the
tree's page size (`page_messages` in its `ctx.toml`, 500 unless set), or more than fits in one
reply. It is worked one page a cycle, oldest messages first. The answer's delta lists only this
page: the window's `page.worked` oldest messages are done already, this page ends at message
`page.through` of its `new_messages`, and `page.ends_at` is the pointer of that last message.

1. Work the page as you would a window: read the records of the messages the delta lists, by
   their pointers, a few at a time, then write and verify their records.
2. When `page.last` is false, record the page with `ctx marker page <window id> '<ends_at>'`,
   giving the page's `ends_at` exactly as the answer lists it, in single quotes (a Matrix
   pointer holds a `$`), and end the cycle. The next cycle's `ctx pending` lists the next page.
   Do not read the next page in this cycle.
3. When `page.last` is true, this is the window's last page: move the marker past the window
   with `ctx marker advance <window id>`, as for any window. `ctx marker page` refuses a last
   page.

Record a page only once its records are written and verified. A cycle that stops before then
leaves the page unrecorded, and the next cycle reads that page again and no page before it; the
records already written for it are updated, not minted twice. The marker refuses to move past a
window whose last page no answer has listed yet, and `ctx marker page` refuses any page but the
one the answer lists for the oldest window after the marker.
