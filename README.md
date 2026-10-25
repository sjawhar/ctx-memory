# ctx-memory

`ctx-memory` is a graph database in markdown: typed YAML-frontmatter records linked by wikilinks. The files are the source of truth, git provides history and collaboration, and the `ctx` command reads, checks, and fetches the tree.

## Install

```bash
cargo install --locked --git https://github.com/sjawhar/ctx-memory
```

## Quick start

Copy the starter tree from a checkout, then point `ctx` at the copy:

```bash
git clone https://github.com/sjawhar/ctx-memory
cp -R ctx-memory/starter ~/example-memory
ctx --memory ~/example-memory rules
ctx --memory ~/example-memory sweep
ctx --memory ~/example-memory search "durable claim"
ctx --memory ~/example-memory dossier Alice
ctx --memory ~/example-memory open
```

Use `--json` with any command to receive a versioned JSON envelope. `--memory` overrides tree discovery; `CTX_MEMORY` supplies the same option. Without either, `ctx` looks for the nearest ancestor containing `ctx.toml`, then for `memory` in the user configuration file at `$XDG_CONFIG_HOME/ctx/config.toml` or `~/.config/ctx/config.toml`.

## What is distinctive

Markdown records and structural linting are established tools. `ctx-memory` adds five rules that keep a team memory useful under change:

1. **Authority tiers drive retrieval.** Numeric tiers 1–4 distinguish external verbatim, internal verbatim, summaries, and recollections. Search ranks direct sources first; `ctx search --verbatim` excludes summaries.
2. **Absence is documented work.** A `not_found` result names what was searched and is logged as demand for future conversion.
3. **Open work has a revisit date.** Decisions, commitments, and questions remain visible through `ctx open`; `ctx sweep` separates structural defects that fail CI from stale records that require review.
4. **Universal negatives need evidence.** The sweep flags claims such as “never told” when they do not identify a direct source or searched surface.
5. **Citations can resolve to their source message.** `ctx resolve` and `ctx cite verify` inspect records held by configured sources instead of treating a pointer as an opaque note.

## Commands

Run `ctx --help` for the current interface.

| Command | Purpose |
| --- | --- |
| `ctx rules` | Print the tree's root `rules.md` page. |
| `ctx sweep` | Report broken links, orphans, missing provenance, schema violations, and stale open records. Structural findings exit nonzero; stale records remain visible without failing the command. |
| `ctx search <query>` | Search records, ranking stronger tiers first. Use `--verbatim` for direct-source records only and `--limit <n>` to change the result limit. |
| `ctx dossier <entity>` | Show the entity page, configured vital fields, and related records. |
| `ctx open` | List open decisions, commitments, and questions. Filter with `--owner <name>` or `--linked FIELD=VALUE[,VALUE…]`. |
| `ctx fetch` | Fetch configured sources into a durable window. Use `--window-end <RFC3339>` or one or more `--source <id>` values to narrow a run. |
| `ctx delta` | Inspect a window's derived record delta; `--write` writes it for the selected `--window`. |
| `ctx cursor show` | Print stored source-stream cursors. |
| `ctx cursor advance <window>` | Apply successful cursor proposals after the corresponding tree commit. |
| `ctx resolve <pointer>...` | Resolve citation pointers from the local store. Use `--thread` to include a pointer's thread. |
| `ctx cite verify [files...]` | Verify citation pointers and quotes. Use `--live` to permit configured sources to resolve missing local records. |
| `ctx source run <id>` | Run one configured source and print its protocol stream. Use `--op fetch` or `--op resolve`. |
| `ctx state path` | Print the active state directory and SQLite store path. |

## Tree configuration

A tree has a `ctx.toml` at its root. The starter tree shows the minimum configuration:

```toml
version = 1
id = "example-memory"
folder_notes = ["index.md"]

[dossier]
vitals = ["status", "last_reviewed"]
```

`version` is currently `1`. `id` names the tree's state subdirectory and is required when sources are configured. `folder_notes` gives folder-level records an additional wikilink name. `dossier.vitals` selects frontmatter fields displayed by `ctx dossier`. `ingest.backfill_hours` changes the default initial source window. Source configuration is documented in [docs/sources.md](docs/sources.md).

State is stored under `CTX_STATE_DIR`, then `state_dir` in the user configuration, then `$XDG_STATE_HOME/ctx`, then `~/.local/state/ctx`; each tree uses a subdirectory named for its `id`.

## Extend

A source is declared in `ctx.toml` and may be built in or external. An external source named `kind = "linear"` runs `ctx-source-linear` from `PATH`; it receives one JSON request on standard input and emits a JSON Lines protocol on standard output. See [docs/sources.md](docs/sources.md) for the protocol, cursor rules, and conformance command.

Unknown commands use the same convention: `ctx report ...` runs `ctx-report` from `PATH` when `report` is not a built-in command. The public command passes the resolved tree and state paths through environment variables so an extension can use the same context.

The agent-facing reading and writing protocol lives in [skills/ctx-memory/SKILL.md](skills/ctx-memory/SKILL.md). The reasoning behind its rules is in [docs/method.md](docs/method.md).

## License

MIT. See [LICENSE](LICENSE).
