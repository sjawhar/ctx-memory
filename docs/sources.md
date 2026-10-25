# Sources

A source imports and resolves material outside the Markdown tree. Built-in source kinds include `slack`, `gmail`, `drive`, and `calendar`. A third-party source is an executable named `ctx-source-<kind>` on `PATH`.

## Protocol

`ctx` starts an external source with no arguments. It sends one JSON request on standard input and closes standard input. The source writes one UTF-8 JSON message per line to standard output and may write diagnostics to standard error. Credentials arrive through environment variables named by the source configuration; they do not belong in a request, window, or manifest.

The first output line is a `hello` message with protocol version `1`, the configured scheme, supported operations, and a `verbatim` flag. For `fetch`, it also names the complete stream list. A fetch emits records followed by exactly one `stream_end` for every planned stream. A successful `stream_end` proposes the cursor to use after a durable commit. A resolve request emits records for found pointers; a missing requested pointer is not found.

For example, configure a third-party Linear source with `id = "linear"`, `kind = "linear"`, and `scheme = "linear"`. A fetch request can look like this:

```json
{"op":"fetch","protocol":1,"source":{"id":"linear","kind":"linear","scheme":"linear"},"config":{"team":"ENG","token_env":"LINEAR_TOKEN"},"window_end":"2026-01-02T18:00:00Z","backfill_hours":24,"cursors":{"ENG":"2026-01-02T16:00:00Z"},"discovered":[],"state_dir":"/state/example/sources/linear"}
```

Its protocol stream can look like this:

```jsonl
{"type":"hello","protocol":1,"scheme":"linear","ops":["fetch","resolve"],"verbatim":true,"streams":["ENG"]}
{"type":"record","pointer":"linear:ENG-42/c-1","stream":"ENG","ts":"2026-01-02T17:12:09Z","label":"ENG","author":"Dana","author_id":"U0EXAMPLE9","thread":"linear:ENG-42","title":"Example issue","url":"https://linear.app/example/issue/ENG-42","text":"Dana confirmed the example issue is ready for review.","aliases":["ENG-42"],"context":false,"raw":{"id":"c-1","issue":"ENG-42"}}
{"type":"stream_end","stream":"ENG","status":"ok","cursor":"2026-01-02T18:00:00Z"}
```

`ctx` treats a nonzero source exit, timeout, invalid JSON, a mismatched pointer scheme, an unplanned stream, a record outside the window, or a missing `stream_end` as an error for every unfinished stream. Error streams do not propose a usable cursor.

## Cursors

A cursor is an opaque value per source and stream. `ctx` stores it and returns it to the source unchanged. Built-in sources use RFC 3339 timestamps; a stream without one receives the configured backfill window, which defaults to 24 hours.

`ctx fetch` writes each stream's `cursor_used` and proposed `cursor_next` to the window manifest but does not mutate cursor state. After committing the tree changes that use a completed window, run:

```bash
ctx cursor advance <window>
```

The command applies only successful stream proposals while holding the fetch lock. It skips errored streams and rejects a window older than the one that last advanced a stream, preserving material for a later retry.

## Configuration

Source configuration lives in the tree's root `ctx.toml`. Unknown tree configuration keys are errors.

```toml
version = 1
id = "example"
folder_notes = ["index.md"]

[dossier]
vitals = ["status", "last_reviewed"]

[ingest]
backfill_hours = 24

[[sources]]
id = "slack"
kind = "slack"
token_env = "SLACK_TOKEN"
workspace = "example"
channels = ["C0EXAMPLE01"]
discover_field = "ops_channel"
thread_poll_days = 30

[[sources]]
id = "gmail"
kind = "gmail"
query = ""

[[sources]]
id = "linear"
kind = "linear"
scheme = "linear"
token_env = "LINEAR_TOKEN"
team = "ENG"
```

Every source needs a unique `id` and pointer scheme. Built-in kinds choose their own default schemes; an external kind needs `scheme`. Source-specific keys are passed unchanged as the request's `config` object.

## Conformance

Run a configured source through the same protocol validator used by fetch:

```bash
ctx source run linear
ctx source run linear --op resolve
```

The command prints the protocol stream for inspection. Use it to develop a source before including it in a full fetch, and keep source output free of credentials.
