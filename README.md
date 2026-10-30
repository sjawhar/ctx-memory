# ctx-memory

`ctx-memory` is a graph database in markdown: typed YAML-frontmatter records linked by wikilinks. The files are the source of truth, git provides history and collaboration, and the `ctx` command reads, checks, and fetches the tree.

## Install

Each `v*` tag publishes `ctx` as a GitHub release: a static build for x86_64 and aarch64 Linux (`ctx-x86_64-unknown-linux-musl.tar.gz`, `ctx-aarch64-unknown-linux-musl.tar.gz`) and a build for Apple silicon macOS (`ctx-aarch64-apple-darwin.tar.gz`), each holding the `ctx` binary alone, and `ctx-memory-<version>-share.tar.gz`, the tag's `skills/` tree for an image or a machine that installs it beside the binary. The release carries `SHA256SUMS` and a GitHub build attestation of each archive. [`.github/workflows/release.yml`](.github/workflows/release.yml) builds them from the tagged commit once it passes CI. With [mise](https://mise.jdx.dev):

```bash
mise use -g github:sjawhar/ctx-memory@0.2.0
```

From source:

```bash
cargo install --locked --git https://github.com/sjawhar/ctx-memory
```

The skills under [`skills/`](skills/) — `ctx-memory`, how an agent reads and writes a tree, and `ctx-memory-ingest`, the ingest cycle's procedure — install as a plugin. The repository is an Oh My Pi plugin package (`package.json`): a plugin tree that installs it, for example as `github:sjawhar/ctx-memory#v0.2.0`, gives every session both skills. It is also a Claude Code plugin marketplace (`.claude-plugin/`):

```
/plugin install ctx-memory --marketplace sjawhar/ctx-memory
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

Built-in commands emit one versioned JSON envelope with `--json`; `ctx source run` otherwise prints its raw protocol stream. Text output is styled only when standard output is a terminal and `NO_COLOR` is unset, so a pipe, a file and the output the server returns carry no escape codes. Record, window and source text is written by other people, so every text-mode printer removes its control characters before it prints — C0 controls other than tab and newline, DEL, and C1 controls — and no message can move the cursor, retitle the terminal or hide a line; that holds for every built-in command's output, a source's standard error under `ctx source run`, and error messages. `--json` keeps the text whole, with every control character written as a `\u` escape. Built-in commands exit `0` on success, `1` for findings or source errors, and `2` for operational or usage errors. `--memory` overrides tree discovery; `CTX_MEMORY` supplies the same option. Without either, `ctx` looks for the nearest ancestor containing `ctx.toml`, then for `memory` in the user configuration file at `$XDG_CONFIG_HOME/ctx/config.toml` or `~/.config/ctx/config.toml`.

## What is distinctive

Markdown records and structural linting are established tools. `ctx-memory` adds five rules that keep a team memory useful under change:

1. **Authority tiers drive retrieval.** Numeric tiers 1–4 distinguish external verbatim, internal verbatim, summaries, and recollections. Search matches every frontmatter value a record declares and the markdown body, ranks by text relevance, and breaks a tie by authority tier, so a direct source outranks an equally relevant summary. With embeddings configured it also matches by meaning, fusing the two rankings, so a paraphrase is found even when no word is shared. `ctx search --verbatim` keeps only records whose strongest tier is 1 or 2; the tier belongs to the whole record, so a tier 3 quote such a record carries shows beside it (see [docs/method.md](docs/method.md#authority-and-provenance)).
2. **Absence is documented work.** A `not_found` result names what was searched and is logged as demand for future conversion.
3. **Open work has a revisit date.** Decisions, commitments, and questions remain visible through `ctx open`; `ctx sweep` separates structural defects that fail CI from stale records that require review.
4. **Universal negatives need evidence.** The sweep flags claims such as “never told” when they do not identify a direct source or searched surface.
5. **Citations can resolve to their source message.** `ctx resolve` and `ctx cite verify` inspect records held by configured sources instead of treating a pointer as an opaque note. A record that makes several claims lists them under `quotes`, and each entry is verified against the source it names.

## Commands

Run `ctx --help` for the current interface.

| Command | Purpose |
| --- | --- |
| `ctx rules` | Print the tree's root `rules.md` page. |
| `ctx sweep` | Report broken links, orphans, missing provenance, unparsable frontmatter, schema violations, and stale open records. Structural findings exit nonzero; stale records remain visible without failing the command. |
| `ctx search <query>` | Search every frontmatter value a record declares — title, quotes, statement, aliases, owner, status, dates, source pointers, a tree's own fields — and the markdown body. Hits are ranked by text relevance, then by authority tier. When `CTX_EMBED_MODEL` (with `GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`) is set, a record is also found by meaning, so a query matches a record it paraphrases even when they share no word; with no Google credential the search falls back to text alone and says so. Use `--verbatim` for direct-source records only and `--limit <n>` to change the result limit (`0` returns every match). |
| `ctx dossier <entity>` | Show the entity page, configured vital fields, and related records. |
| `ctx open` | List open decisions, commitments, and questions. Filter with `--owner <name>` or `--linked FIELD=VALUE[,VALUE…]`. |
| `ctx fetch` | Fetch configured sources into a durable window and move the cursor of every stream that stored its messages. Use `--window-end <RFC3339>` or one or more `--source <id>` values to narrow a run. As a Cloud Run job — wherever `CLOUD_RUN_JOB` is set — it requires `CTX_LEASE_BUCKET`. |
| `ctx delta` | Inspect a window's derived record delta; `--write` writes it for the selected `--window`, under the `fetch` lease when `CTX_LEASE_BUCKET` is set. |
| `ctx cursor show` | Print stored source-stream cursors. |
| `ctx pending` | List the completed windows after the agent marker, with the delta they combine into. |
| `ctx marker show` | Print the window the agent has handled up to. |
| `ctx marker advance <window>` | Move the marker to a completed window, after the commit that handled it. Run locally with `CTX_LEASE_BUCKET` set, it takes the `server` lease and exits `2` while a server holds it; with `CTX_SERVER` set, the server moves it. |
| `ctx resolve <pointer>...` | Resolve citation pointers from the local store. Use `--thread` to include a pointer's thread. |
| `ctx cite verify [files...]` | Verify citation pointers and every quote a record makes, including each entry of a `quotes` list. Exits `1` when a quote is broken, when a claim names a pointer nothing can check, when a tier 1 or 2 claim names no pointer, when a tier 1 or 2 record has no tier 1 or 2 quote that held, or when a record's frontmatter does not parse, so that none of its quotes could be read; a tier 3 or 4 claim that names no pointer is only counted among the quotes nothing checked. A named file may be written relative to the working directory or to the tree root, or as an absolute path; a path that names no record of the tree is an error, never `checked 0`. In remote mode a named file that exists on the caller's machine is sent to the server and checked as written there. Use `--live` to permit configured sources to resolve missing local records. |
| `ctx source run <id>` | Run one configured source. Without `--json`, print its raw protocol stream; with `--json`, emit one envelope containing validated messages and the source outcome. Use `--op fetch` or `--op resolve`. A Slack fetch is refused because only `ctx fetch` may rotate its credential. |
| `ctx state path` | Print the active state directory, SQLite store path, and data directory. |
| `ctx slack login` | Sign in to Slack in a browser and save the `slack` credential the fetch job reads: Slack's OAuth v2 flow for a user token of the ctx-memory Slack app, an app only ctx signs in with (`deploy/slack/ctx-memory-manifest.yaml`), with its callback on `127.0.0.1:3118` or the redirected URL pasted on standard input. It reads the app's client id and secret from the secret `CTX_SECRET_SLACK_APP` names. Before it reports success it saves the token, its refresh token and its expiry as a new version of the Secret Manager secret `CTX_SECRET_SLACK` names, as the identity the Google metadata server (or `GCE_METADATA_HOST`) answers for. The token expires in 12 hours; the fetch job, holding the fetch lease, refreshes it. See [docs/sources.md](docs/sources.md#the-ctx-memory-slack-app). |
| `ctx serve` | Serve read commands and record writes over HTTP: `--listen <addr>` for callers with a Google identity token, `--tailnet-listen <addr>` for a Tailscale sidecar on loopback. `--listen` requires `--audience <aud>` (`CTX_SERVER_AUDIENCE`), the audience every identity token must carry, and `--allow-principal <email\|@domain>` (`CTX_SERVER_PRINCIPALS`), who may call it. On Cloud Run — with `--listen`, or wherever `K_SERVICE` is set — `CTX_LEASE_BUCKET` is required too. `--pull-interval <seconds>` sets how often it follows `main`. |
| `ctx record show <path>` | Print a record's whole text, frontmatter and body, as the tree holds it; with `CTX_SERVER` set, as the server's clone of `main` holds it. `--json` carries the text unchanged in `data.text`; the text form drops control characters, as every printer does. A path that is not a record path, or that names no record the tree loaded (the loader follows no symbolic link), exits `2`. It writes nothing and moves no cursor, marker or lease. |
| `ctx record put <path> [-\|<file>]` | Write a record through the server, reading it from standard input unless a file is named. |
| `ctx record rm <path>` | Remove a record through the server. |

## Remote mode

One `ctx serve` process can be the only writer of a tree. It keeps the tree and its index in memory, answers read commands, and writes records itself: it pulls `main`, writes the file, checks it the way `ctx sweep` and `ctx cite verify` check it, commits it under the caller's own name and pushes it. A write that would add a finding to its own path is refused and nothing is committed; reachability is not checked per write, because a record and the page that links it are two separate writes, so `ctx sweep` reports orphans over the whole tree instead. A write that carries something with the shape of a credential — an Anthropic API key, a model gateway key, a GitHub, Slack or Google OAuth token, a private key — in a put's content, or in the record path or session of a put or a delete, is refused with `422` before anything is written, with a `credential` finding that names the kind and never repeats the credential (one in the path shows as `[redacted]`); a marker advance whose session holds one is refused with `400`.

A record path names a file this tree loads as a record: a `.md` file under the tree root, named by its components, none of them hidden and none of them a symbolic link. It is plain text: no `*`, `?`, `[` or `]`, no component that starts with `-` or `:`, and no control character, because git and the commit message both read it. `ctx record show` takes the same paths. The rules page, and a file named `AGENTS.md`, `CLAUDE.md`, `GEMINI.md` or `SKILL.md` at any depth, each in any letter case, are refused with `400`, for a put and a delete alike: the rules page changes only through a reviewed pull request, and an agent that works in a clone of the tree reads those files as its instructions. A path no file can have — a name longer than 255 bytes, the most a Linux file system holds, or a path longer than 1,024 bytes — is refused with `422` (`not-a-record`) before anything is written, for a put and a delete alike, and so is a put whose path runs through an existing file or ends at a directory; the answer names the caller's path and none of the server's. A put to a path git ignores is refused with `422` (`not-a-record`) before anything is written. A put whose file the loaded tree does not carry for any other reason — an `.ignore` file or a global exclude, which the tree's loader reads and git does not — is written, found unloaded and rolled back, and refused with the same `422`; either way it is never committed with no check behind it, because every check matches findings by the record's path, so a path nothing loads would otherwise pass them all by finding nothing. A delete of a path git ignores is refused with `400`.

Nothing a write leaves behind outlives the answer. A refused write, a failed push, a failed token mint and a rebase that conflicts all end with the clone back on the commit the write started from, so no later write can publish a commit whose own caller was told it failed, and no copy is left unable to fast-forward. A caller's own mistake is answered `400`, a check it failed `422`, a race it lost — a rebase onto a person's merge that conflicts — `409`, and the server's own trouble `500`. A push that ended badly is not called failed until the remote has been asked: one the remote took is answered with its commit, and when the remote cannot be asked either, the answer is `503` with the outcome unknown. A write asked for again after its commit landed changes nothing, and is answered with the commit that holds the record. The caller's session goes into the commit message as one line, whatever it holds.

A clone `ctx` made from `CTX_MEMORY_REMOTE` holds nothing of its own outside a write. A commit or a file left there — by a rollback that itself failed — is one whose caller was told the write failed, so the next write and the next follow pass put the clone back the way the remote has it before anything else, and a clone that cannot be put back takes no write. A tree somebody keeps is theirs: the server refuses to start in it, and refuses each write, while it holds uncommitted changes or commits the remote does not have, and names them, because a push from it would publish them with none of the checks a write gets.

`/healthz` answers `503` while a write could not start from the clone: a rebase left in progress, a clone and a remote that have each moved on from the other, or a clone `ctx` made that holds a commit the remote does not have. `/healthz` also answers `503` when git cannot open the clone for any reason other than its not being a git repository (an unreadable configuration, a directory git does not trust), and when the health check itself fails; the body is then `{"ok": false, "error": "the health check failed; the server's log says why", "following": {…}}`, and the reason is only in the server's log. A probe never waits for a write or a follow pass that holds the tree: it answers from the last check, with `"busy": true`. The server makes its first check before it opens a port, so every answer carries a check, and it does not start when that check fails. Its `following` field says how the follow loop is doing: `ok`, and while its passes fail, `since` and `error`. `/healthz` answers any caller, so every `error` it carries is a fixed text, such as `the follow loop is failing; the server's log says why`, and the reason goes only to the server's log. A failing loop — the remote or GitHub's token mint unreachable — keeps the answer `200`, because a restart cannot reach them either, and a copy that cannot reach the remote at start cannot clone at all.

The server's standard error carries three fixed-form lines that a deployment's alerts can match, each at the start of a line; every line the server writes there is one line, with control characters shown as spaces and any credential replaced by `[redacted]`: a lease's own lines (`renewing the server lease: …` when Cloud Storage refuses a renewal) and the error that stops the server are one line too, though they quote Cloud Storage's or git's answers, which run over several, and so is a panic, `the server panicked at <where>: <what>`. The fetch job writes its standard error the same way. Every answer the write handlers give other than `200` from `/v1/records/put`, `/v1/records/delete` or `/v1/marker/advance` prints `write failed: <route> <status> <reason>`: the reason is one line of at most 300 characters with any credential replaced by `[redacted]`, a `422` gives `refused: <finding kinds>`, and a `401` gives the fixed `the caller was not identified`. An answer axum gives before a handler runs — `413` for a body over its 2 MiB limit, `405` for a wrong method — prints nothing. Each failed follow pass prints `following main: <why>`, and a pass that succeeds prints `follow heartbeat: ok` when 5 minutes have passed since the previous heartbeat. So if neither line appears for longer than 5 minutes plus one pull interval (`--pull-interval`, 60 seconds by default), the follow loop has stopped. Match each form at the start of the line: a refused path is quoted inside its `write failed:` line.

With `CTX_SERVER` set — or `server` in the user configuration — `ctx` runs those commands on the server and prints its output and exit code. The server answers from its own tree, so `--memory` or `CTX_MEMORY` beside a configured server is refused for those commands rather than ignored. `CTX_SERVER` must be an `https://` URL, an `http://` URL of a loopback host (`localhost`, `127.0.0.0/8` or `::1`), or an `http://` URL of a tailnet name ending in `.ts.net`, whose WireGuard tunnel encrypts and authenticates the traffic; `ctx` refuses any other before it sends a request. Such a session needs no clone, no store and no source credential:

```bash
export CTX_SERVER='http://ctx-memory.<tailnet>.ts.net'   # your tailnet's name for the server
ctx search "renewal terms"
ctx pending
ctx record put decisions/renewal.md - <<'EOF'
---
type: decision
status: open
title: Renewal terms
quote: "we will renew at the current rate"
source: "slack:C0EXAMPLE01/1700000000.000001"
tier: 1
---
The vendor confirmed the rate.
EOF
```

Who the caller is depends on where it runs. On a dev box the request goes over the tailnet and Tailscale's own login header names the caller, so `--tailnet-listen` takes a loopback address only: the sidecar runs beside the process, and anything else that could reach the port could name itself. Inside Google Cloud the client sends a Google identity token for `CTX_SERVER_AUDIENCE`, taken from the metadata server; where the workload has no Google service account of its own, `CTX_IMPERSONATE_SA` names one to mint the token as. `CTX_SERVER_AUDIENCE` must equal the origin of `CTX_SERVER` (`scheme://host[:port]`, no path), or `ctx` refuses and mints no token, so a token is only ever sent to the server it was minted for. The server accepts nothing else: a request without an identity is refused, and the ingress port ignores every `Tailscale-*` header. An identity token says which account minted it and which server it was meant for, and any Google account anywhere can mint one, so the ingress port also answers only the principals `--allow-principal` names — an account email, or `@domain` for a whole domain — and refuses everything else, whatever a deployment's own invoker policy allows.

`/v1/command` is the read door: it runs the allowlisted read commands only, `record show` among them, and refuses any argv that writes, such as `delta --write`, which replaces the newest window's files, or `record put` and `record rm`. Records are written through `/v1/records/put` and `/v1/records/delete`, and the marker through `/v1/marker/advance`; each holds the write lease. A marker moved through the server names the signed-in caller and the session it names, and a window the caller names wrongly is answered by its id. No answer names the server's own directories: an error from any route, a finding, and what a command run through `/v1/command` prints show the clone as `<tree>`, the data directory as `<data>` and the state directory as `<state>` (`reading <data>/windows: No such file or directory`). The one exception is the record a `record show` that succeeds prints: it goes out as the tree holds it, because a cycle writes it back whole. The server's own log keeps the paths.

`scripts/remote-smoke.sh` drives a running server through that surface, and with `--write` also writes one record, reads it back and removes it again.

## Tree configuration

A tree has a `ctx.toml` at its root. The starter tree shows the minimum configuration:

```toml
version = 1
id = "example-memory"
folder_notes = ["index.md"]

[dossier]
vitals = ["status", "last_reviewed"]
```

`version` is currently `1`. `id` names the tree's state subdirectory and is required when sources are configured. `data_dir` holds the files that machines share, described below. `folder_notes` gives folder-level records an additional wikilink name. `dossier.vitals` selects frontmatter fields displayed by `ctx dossier`. `ingest.backfill_hours` changes the default initial source window. Source configuration is documented in [docs/sources.md](docs/sources.md).

Two directories hold what `ctx` keeps outside the tree. The **data directory** holds the files every machine reading this tree needs: `windows/`, `cursors.json`, `marker.json`, `sources.json` (each source's verbatim declaration), `sources/<source id>/`, what a source must keep between fetches, such as the Slack threads whose parent is older than the window, and `embeddings/`, a cache of record and message vectors. `ctx fetch` writes everything except `marker.json`, `ctx marker advance` writes `marker.json`, and `ctx delta --write` rewrites the delta files of the window it selects. `embeddings/` has three writers: the fetch job's delta, the server, and any machine that runs `ctx search` or `ctx delta` with meaning search configured each add the vectors they compute. Each file there holds the vector one model gives one text, is named by that text's hash and is replaced atomically, so two writers that use one model write the same thing, a vector another model made is computed again rather than used, and no lease covers it. Every other command only reads the data directory. It is `CTX_DATA_DIR`, then `data_dir` in `ctx.toml`, and defaults to the state directory, so a single-machine tree needs no configuration. The **state directory** holds what each machine keeps for itself: `resolved/` (the records a live `ctx resolve` or `ctx cite verify --live` obtained here), `store.sqlite3` (an index rebuilt from the windows and from `resolved/`), the fetch lock, and the scratch space a `ctx source run` or a live resolve gives a source. It is `CTX_STATE_DIR`, then `state_dir` in the user configuration, then `$XDG_STATE_HOME/ctx`, then `~/.local/state/ctx`; each tree uses a subdirectory named for its `id`. One data directory holds one tree.

When more than one machine shares that data directory, `CTX_LEASE_BUCKET` names a Cloud Storage bucket and each writing role takes a lease object in it: `fetch` for the fetch job, `server` for the server. A local command that writes what one of those roles writes takes that role's lease: `ctx delta --write` replaces files the fetch writes, so it takes the `fetch` lease, and `ctx marker advance` moves the marker the server moves, so it takes the `server` lease. The holder renews its lease every 30 seconds and checks it before every write batch, once it holds what the batch writes, and judges the time since its last renewal after Cloud Storage has answered; another copy may take it over only after 120 seconds without a renewal, and a holder that cannot renew stops writing 30 seconds before that, so the copy that stops is never still writing when the next one starts. A holder whose check fails stops renewing and leaves the object in place, so the next copy waits out the window rather than start while a push may still be in flight. A copy that is asked to stop — `SIGTERM`, as a Cloud Run rollout sends — gives its lease up as it goes, so the copy that replaces it writes at once; the 120 seconds are for the copy that is killed outright. A fetch that cannot take the lease names the copy that holds it, exits non-zero and writes nothing; a write the server cannot cover with its lease is answered `503` and nothing is committed. A local `ctx delta --write` or `ctx marker advance` holds its lease for its one write and frees it afterwards, and `ctx delta --write` checks it again just before it writes; when another copy holds the lease, the command names the holder, exits `2` and writes nothing. A running server holds its lease the whole time, so where a server runs, move the marker through it, with `CTX_SERVER` set. Without `CTX_LEASE_BUCKET` nothing is leased, and one machine stays single through the lock `ctx fetch` takes on its state directory; a server or a fetch job on Cloud Run is never that machine, and refuses to start without the bucket.

`CTX_MEMORY_REMOTE` names a git URL to clone into `CTX_MEMORY` when that path holds no clone yet, and to fast-forward when it does. A container that starts with an empty filesystem — the server or the fetch job — reaches the tree with that and the app key below, and needs no credential helper and no ssh key: a private GitHub remote refuses an anonymous clone, so `ctx` mints a GitHub App installation token and sends it as an `Authorization` header on every git call, clone, fetch, pull and push alike. The token is never written into the remote URL, so it never reaches `.git/config`, a reflog or an error message, and never appears in a command line. `CTX_MEMORY` is on the container's own disk, never on the share: each copy keeps a clone of its own, and a starting copy puts a clone it made back the way the remote has it, which on a shared clone would throw away the write of the copy still running.

A credential a machine holds outside Secret Manager is named under `[credentials]`: `github_app = { file = "~/.config/ctx/github-app.json" }`. In Google Cloud, `CTX_SECRET_<NAME>` names a Secret Manager secret instead and the running identity reads it; nothing writes a credential into the tree. A source reads its own credential from the environment, as an external source does, so the Slack and Google sources take `CTX_SECRET_<NAME>`, `CTX_CREDENTIAL_<NAME>_FILE` or `CTX_CREDENTIAL_<NAME>` (for example `CTX_SECRET_SLACK` or `CTX_CREDENTIAL_GOOGLE`) and read no `[credentials]` entry. `ctx slack login` reads `slack_app` through `CTX_SECRET_SLACK_APP` and writes the `slack` credential through `CTX_SECRET_SLACK`; the fetch job reads both when it refreshes the Slack token, and writes the renewed `slack` credential.

## The Slack app

ctx reads Slack through a Slack app that nobody else signs in with: the ctx-memory app, an internal app of the workspace with five read scopes, no bot user and token rotation on, created from [`deploy/slack/ctx-memory-manifest.yaml`](deploy/slack/ctx-memory-manifest.yaml). Its tokens expire every 12 hours, and the fetch job refreshes them with the app's client secret while it holds the fetch lease, saving each new credential before it uses it. It used to sign in with the Claude Code Slack app, whose tokens rotate and which Oh My Pi's Slack connection also uses. Slack's [Using token rotation](https://docs.slack.dev/authentication/using-token-rotation) says: "If you refresh your credentials repeatedly before expiration (e.g., by calling the `oauth.v2.access` API method multiple times for the same token within a 12 hour period), we will enforce a limit of 2 active tokens. If more than 2 tokens exist after the refresh, the oldest additional token will be revoked." Oh My Pi refreshes its token about 20 times a day, and the token ctx stored answered `token_revoked` at its first fetch; that both count against one such limit is the diagnosis's best reading, supported but not proven. With its own app, ctx is the only holder of the app's tokens, so no other holder's refreshes count against that limit. A credential the earlier sign-in wrote is still read and still refreshed.

Setting the app up takes three steps: create it from the manifest at [api.slack.com/apps](https://api.slack.com/apps), store its client id and secret as the JSON `{client_id, client_secret}` in the secret `CTX_SECRET_SLACK_APP` names, and run `ctx slack login` once. [docs/sources.md](docs/sources.md#the-ctx-memory-slack-app) gives each step's commands.

## Extend

A source is declared in `ctx.toml` and may be built in or external. An external source named `kind = "linear"` runs `ctx-source-linear` from `PATH`; it receives one JSON request on standard input and emits a JSON Lines protocol on standard output. See [docs/sources.md](docs/sources.md) for the protocol, cursor rules, and conformance command.

Unknown commands use the same convention: `ctx report ...` runs `ctx-report` from `PATH` when `report` is not a built-in command. The name may hold only ASCII letters, digits, `-` and `_`; `ctx` refuses any other name before it searches `PATH`, so a name cannot be a path to another program. Like git and cargo plugins, external commands own their stdout and exit status; `ctx` passes the resolved tree and state paths, JSON flag, and `ctx` version through `CTX_*` environment variables. The v1 JSON envelope and `0`/`1`/`2` exit-code contract apply only to built-in commands.

Two agent skills ship with the package: [skills/ctx-memory/SKILL.md](skills/ctx-memory/SKILL.md) is the reading and writing protocol, and [skills/ctx-memory-ingest/SKILL.md](skills/ctx-memory-ingest/SKILL.md) is the ingest cycle. The reasoning behind their rules is in [docs/method.md](docs/method.md).

## License

MIT. See [LICENSE](LICENSE).
