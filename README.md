# ctx-memory

`ctx-memory` is a graph database in markdown: typed YAML-frontmatter records linked by wikilinks. The files are the source of truth, git provides history and collaboration, and the `ctx` command reads, checks, and fetches the tree.

## Install

Each `v*` tag publishes `ctx` as a GitHub release: a static build for x86_64 and aarch64 Linux (`ctx-x86_64-unknown-linux-musl.tar.gz`, `ctx-aarch64-unknown-linux-musl.tar.gz`) and a build for Apple silicon macOS (`ctx-aarch64-apple-darwin.tar.gz`), each holding the `ctx` binary alone, and `ctx-memory-<version>-share.tar.gz`, the tag's `skills/` and `runner/` trees for an image or a machine that installs them beside the binary ([docs/deployment.md](docs/deployment.md)). The release carries `SHA256SUMS` and a GitHub build attestation of each archive. [`.github/workflows/release.yml`](.github/workflows/release.yml) builds them from the tagged commit once it passes CI.

There is no Intel macOS build: on an Intel Mac, skip mise and the download below and install from source.

With [mise](https://mise.jdx.dev): install mise first and activate it in your shell ([mise's getting started guide](https://mise.jdx.dev/getting-started.html)), then install the latest release:

```bash
mise use -g github:sjawhar/ctx-memory@latest
ctx --version
```

mise skips a release newer than its `minimum_release_age` setting, so for a day or so after a release it installs the one before; to take the newest at once, name its version from the [releases page](https://github.com/sjawhar/ctx-memory/releases/latest), for example `@0.2.9`.

Without mise, download the latest release, check it against `SHA256SUMS`, and put `ctx` on your `PATH`. This needs `curl` and `tar`. On Linux:

```bash
archive=ctx-$(uname -m)-unknown-linux-musl.tar.gz
base=https://github.com/sjawhar/ctx-memory/releases/latest/download
curl -fsSLO "$base/$archive" &&
  curl -fsSLO "$base/SHA256SUMS" &&
  grep " $archive$" SHA256SUMS | sha256sum -c - &&
  mkdir -p "$HOME/.local/bin" &&
  tar -xzf "$archive" -C "$HOME/.local/bin" ctx
export PATH="$HOME/.local/bin:$PATH"   # add this line to your shell profile too
ctx --version
```

On Apple silicon macOS:

```bash
archive=ctx-aarch64-apple-darwin.tar.gz
base=https://github.com/sjawhar/ctx-memory/releases/latest/download
curl -fsSLO "$base/$archive" &&
  curl -fsSLO "$base/SHA256SUMS" &&
  grep " $archive$" SHA256SUMS | shasum -a 256 -c - &&
  mkdir -p "$HOME/.local/bin" &&
  tar -xzf "$archive" -C "$HOME/.local/bin" ctx
export PATH="$HOME/.local/bin:$PATH"   # add this line to your shell profile too
ctx --version
```

From source (needs Rust, a C compiler and linker, and [CMake](https://cmake.org/download/): install Rust with [rustup](https://rustup.rs); on macOS run `xcode-select --install` and install CMake; on Debian or Ubuntu run `sudo apt install build-essential cmake`):

```bash
cargo install --locked --git https://github.com/sjawhar/ctx-memory
```

The skills under [`skills/`](skills/) — `ctx-memory`, how an agent reads and writes a tree, and `ctx-memory-ingest`, the ingest cycle's procedure — install as a plugin. The repository is an Oh My Pi plugin package (`package.json`): a plugin tree that installs it, for example as `github:sjawhar/ctx-memory#v<version>` (the latest release's tag), gives every session both skills. It is also a Claude Code plugin marketplace (`.claude-plugin/`):

```
/plugin install ctx-memory --marketplace sjawhar/ctx-memory
```

### Join a team server

A team that runs a ctx server ([Remote mode](#remote-mode)) on its Tailscale network reaches it by name over that network. To use it:

1. Install Tailscale: follow [Tailscale's install guide](https://tailscale.com/download) for your system (on Linux, `curl -fsSL https://tailscale.com/install.sh | sh`).
2. Join your team's tailnet and sign in with your team account. On Linux, run `sudo tailscale up`, then open the URL it prints. On macOS, complete the onboarding flow in the Tailscale app.
3. Point `ctx` at the server. Replace `<server>` and `<tailnet>` with your team's server name and tailnet name; ask a teammate if you do not know them:

   ```bash
   export CTX_SERVER='http://<server>.<tailnet>.ts.net'   # add this line to your shell profile too
   ```

4. Check that it works. Both commands print results from the team's tree:

   ```bash
   ctx search "a word from your team's tree"
   ctx rules
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
| `ctx rules` | Print the tree's root `rules.md` page, then each schema under `schemas/`: the fields a record of that type must carry, which every write is checked against. |
| `ctx sweep` | Report broken links, orphans, missing provenance, unparsable frontmatter, schema violations, and stale open records. Structural findings exit nonzero; stale records remain visible without failing the command. |
| `ctx search <query>` | Search every frontmatter value a record declares — title, quotes, statement, aliases, owner, status, dates, source pointers, a tree's own fields — and the markdown body. Hits are ranked by text relevance, then by authority tier. When `CTX_EMBED_MODEL` (with `GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`) is set, a record is also found by meaning, so a query matches a record it paraphrases even when they share no word; with no Google credential the search falls back to text alone and says so. Use `--verbatim` for direct-source records only and `--limit <n>` to change the result limit (`0` returns every match). |
| `ctx dossier <entity>` | Show the entity page, configured vital fields, and related records. |
| `ctx open` | List open decisions, commitments, and questions, each with who it waits on (`waiting_on`) and what tracks it (`tracked_by`). Filter with `--owner <name>`, `--waiting-on <name>` (whole name, any case, comma list), `--overdue` (past its `revisit_by` or `due` date) or `--linked FIELD=VALUE[,VALUE…]`; every filter given must match. |
| `ctx fetch` | Fetch configured sources into a durable window and move the cursor of every stream that stored its messages. Use `--window-end <RFC3339>` or one or more `--source <id>` values to narrow a run. As a Cloud Run job — wherever `CLOUD_RUN_JOB` is set — it requires `CTX_LEASE_BUCKET`. |
| `ctx delta` | Print the record delta the fetch wrote for a window, less excluded messages and cut to one answer's size; it never computes one. `--write` computes it again against the current tree and writes it for the selected `--window`, under the `fetch` lease when `CTX_LEASE_BUCKET` is set. |
| `ctx cursor show` | Print stored source-stream cursors. |
| `ctx pending` | List the completed windows after the agent marker, with the delta they combine into. |
| `ctx marker show` | Print the window the agent has handled up to. |
| `ctx context <window>` | Print what the tree's context source (`[context_source]` in `ctx.toml`) says about the subjects one completed window names: `{window, subjects, items[{ref, title, status, updated_at, links}], dropped?, unavailable?}`. With `CTX_SERVER` set it reads `GET /v1/windows/{id}/context`, which only the callers `context_source.callers` names may read (403 for anyone else). A tree with no context source answers an empty list; a source that fails, times out or prints malformed output answers `unavailable`, never an error status. |
| `ctx marker advance <window>` | Move the marker to a completed window, after the commit that handled it. Run locally with `CTX_LEASE_BUCKET` set, it takes the `server` lease and exits `2` while a server holds it; with `CTX_SERVER` set, the server moves it. |
| `ctx resolve <pointer>...` | Resolve citation pointers from the local store. Use `--thread` to include a pointer's thread. |
| `ctx cite verify [files...]` | Verify citation pointers and every quote a record makes, including each entry of a `quotes` list. Exits `1` when a quote is broken, when a claim names a pointer nothing can check, when a tier 1 or 2 claim names no pointer, when a tier 1 or 2 record has no tier 1 or 2 quote that held, or when a record's frontmatter does not parse, so that none of its quotes could be read; a tier 3 or 4 claim that names no pointer is only counted among the quotes nothing checked. A named file may be written relative to the working directory or to the tree root, or as an absolute path; a path that names no record of the tree is an error, never `checked 0`. In remote mode a named file that exists on the caller's machine is sent to the server and checked as written there. Use `--live` to permit configured sources to resolve missing local records. |
| `ctx source run <id>` | Run one configured source. Without `--json`, print its raw protocol stream; with `--json`, emit one envelope containing validated messages and the source outcome. Use `--op fetch` or `--op resolve`. A Slack fetch is refused because only `ctx fetch` may rotate its credential. |
| `ctx state path` | Print the active state directory, SQLite store path, and data directory. |
| `ctx slack login` | Sign in to Slack in a browser and save the `slack` credential the fetch job reads: Slack's OAuth v2 flow for a user token of the ctx-memory Slack app, an app only ctx signs in with (`slack/app-manifest.yaml`), with its callback on `127.0.0.1:3118` or the redirected URL pasted on standard input. It reads the app's client id and secret from the secret `CTX_SECRET_SLACK_APP` names. Before it reports success it saves the token, its refresh token and its expiry as a new version of the Secret Manager secret `CTX_SECRET_SLACK` names, as the identity the Google metadata server (or `GCE_METADATA_HOST`) answers for. The token expires in 12 hours; the fetch job, holding the fetch lease, refreshes it. See [docs/sources.md](docs/sources.md#the-ctx-memory-slack-app). |
| `ctx serve` | Serve read commands and record writes over HTTP: `--listen <addr>` for callers with a Google identity token, `--tailnet-listen <addr>` for a Tailscale sidecar on loopback. `--listen` requires `--audience <aud>` (`CTX_SERVER_AUDIENCE`), the audience every identity token must carry, and `--allow-principal <email\|@domain>` (`CTX_SERVER_PRINCIPALS`), who may call it. `--tailnet-allow-tag <tag:name>` (`CTX_SERVER_TAILNET_TAGS`; repeat it or separate tags with commas) lets the tailnet listener answer tagged nodes carrying one of those tags, and requires `--tailnet-listen` and `--tailscale-socket <path>` (`CTX_TAILSCALE_SOCKET`), the unix socket of tailscaled's local API. On Cloud Run — with `--listen`, or wherever `K_SERVICE` is set — `CTX_LEASE_BUCKET` is required too. `--pull-interval <seconds>` sets how often it follows `main`. `--index-wait <seconds>` (90 by default) is how long it waits, at most, for its record index and for the window deltas `ctx open` reads before it binds its listeners. |
| `ctx record show <path>` | Print a record's whole text, frontmatter and body, as the tree holds it; with `CTX_SERVER` set, as the server's clone of `main` holds it. `--json` carries the text unchanged in `data.text`; the text form drops control characters, as every printer does. A path that is not a record path, or that names no record the tree loaded (the loader follows no symbolic link), exits `2`. It writes nothing and moves no cursor, marker or lease. |
| `ctx record put <path> [-\|<file>]` | Write a record through the server, reading it from standard input unless a file is named. |
| `ctx record put --batch <-\|file>` | Write several records through the server in one commit, or none of them, reading a JSON list of `{"path": …, "content": …}` from standard input (`-`) or a file. Each record is checked against the tree with all of them in it, so new records that link each other, which no order of single writes can land, land together. A refusal prints each finding with the record it is about. |
| `ctx record rm <path>` | Remove a record through the server. |

## Remote mode

One `ctx serve` process can be the only writer of a tree. It keeps the tree and its index in memory, answers read commands, and writes records itself: it pulls `main`, writes the file, checks it the way `ctx sweep` and `ctx cite verify` check it, commits it under the caller's own name and pushes it. A write that would add a finding to its own path is refused and nothing is committed; reachability is not checked per write, because a record and the page that links it are often two separate writes, so `ctx sweep` reports orphans over the whole tree instead. A batch (`/v1/records/put-batch`) is one write: its records are written together, each is checked against the tree with all of them in it, and one commit holds them all, or a refusal names each finding's record and nothing is committed. A write that carries something with the shape of a credential — an Anthropic API key, a model gateway key, a GitHub, Slack or Google OAuth token, a private key — in a put's content, or in the record path or session of a put or a delete, is refused with `422` before anything is written, with a `credential` finding that names the kind and never repeats the credential (one in the path shows as `[redacted]`); a marker advance whose session holds one is refused with `400`.

A read costs no tree load, and no delta the copy has already read. The unreviewed replies `ctx open` reports come from the durable delta of every completed window on the share, and a window's delta is written once, when the fetch completes the window: a call lists the share's windows and looks at each `delta.json`, and reads and parses only the ones this copy has not read or that were written since. So a warm copy answers `ctx open` without parsing a delta at all, however many the share holds. A window whose delta `ctx delta --window <id> --write` computes again is read again, and a window that is gone from the share is dropped. A `ctx open` run without a server reads the windows it needs once and exits.

Each finding says what to do about it, because the caller is usually a session that cannot read this code: a `gap` names the link and says to write that record first or in the same call, a `schema` names the field and says that `ctx rules` prints each type's schema, and a `quote` names the line of the record the quote is on, so a record that quotes one source many times says which quote failed, and where to read the words that do hold.

A record path names a file this tree loads as a record: a `.md` file under the tree root, named by its components, none of them hidden and none of them a symbolic link. It is plain text: no `*`, `?`, `[` or `]`, no component that starts with `-` or `:`, and no control character, because git and the commit message both read it. `ctx record show` takes the same paths. The rules page, and a file named `AGENTS.md`, `CLAUDE.md`, `GEMINI.md` or `SKILL.md` at any depth, each in any letter case, are refused with `400`, for a put and a delete alike: the rules page changes only through a reviewed pull request, and an agent that works in a clone of the tree reads those files as its instructions. A batch that names no record, or one path twice, is refused with `400`. A path no file can have — a name longer than 255 bytes, the most a Linux file system holds, or a path longer than 1,024 bytes — is refused with `422` (`not-a-record`) before anything is written, for a put and a delete alike, and so is a put whose path runs through an existing file, or through another record of the same batch, or ends at a directory; the answer names the caller's path and none of the server's. A put to a path git ignores is refused with `422` (`not-a-record`) before anything is written. A put whose file the loaded tree does not carry for any other reason — an `.ignore` file or a global exclude, which the tree's loader reads and git does not — is written, found unloaded and rolled back, and refused with the same `422`; either way it is never committed with no check behind it, because every check matches findings by the record's path, so a path nothing loads would otherwise pass them all by finding nothing. A delete of a path git ignores is refused with `400`.

Nothing a write leaves behind outlives the answer. A refused write, a failed push, a failed token mint and a rebase that conflicts all end with the clone back on the commit the write started from, so no later write can publish a commit whose own caller was told it failed, and no copy is left unable to fast-forward. A caller's own mistake is answered `400`, a check it failed `422`, a race it lost — a rebase onto a person's merge that conflicts — `409`, and the server's own trouble `500`. A push that ended badly is not called failed until the remote has been asked: one the remote took is answered with its commit, and when the remote cannot be asked either, the answer is `503` with the outcome unknown. A write asked for again after its commit landed changes nothing, and is answered with the commit that holds the record. The caller's session goes into the commit message as one line, whatever it holds: `ctx` sends the first of `CTX_SESSION`, `OMP_SESSION_ID`, `PI_SESSION_ID` and `CLAUDE_CODE_SESSION_ID` that is set, as the session header of its requests does (below), or, when none is, the user it runs as (`USER`).

A clone `ctx` made from `CTX_MEMORY_REMOTE` holds nothing of its own outside a write. A commit or a file left there — by a rollback that itself failed — is one whose caller was told the write failed, so the next write and the next follow pass put the clone back the way the remote has it before anything else, and a clone that cannot be put back takes no write. A tree somebody keeps is theirs: the server refuses to start in it, and refuses each write, while it holds uncommitted changes or commits the remote does not have, and names them, because a push from it would publish them with none of the checks a write gets.

`/healthz` answers `503` while a write could not start from the clone: a rebase left in progress, a clone and a remote that have each moved on from the other, or a clone `ctx` made that holds a commit the remote does not have. `/healthz` also answers `503` when git cannot open the clone for any reason other than its not being a git repository (an unreadable configuration, a directory git does not trust), and when the health check itself fails; the body is then `{"ok": false, "error": "the health check failed; the server's log says why", "following": {…}}`, and the reason is only in the server's log. A probe never waits for a write or a follow pass that holds the tree: it answers from the last check, with `"busy": true`. The server makes its first check before it opens a port, so every answer carries a check, and it does not start when that check fails. It starts building its record index there too: the index is in the state directory, which a container's image does not carry, so it is replayed from every window on the share at every start, and a caller that arrived first used to wait that replay out holding the tree, with every other caller behind it. Beside that build it reads every window's `delta.json` that `ctx open` reads, which a process reads once and keeps, so the first `ctx open` after a start no longer reads the whole share itself (12.8 s for the first call against a deployment's share, 29 ms for the next). The listeners wait at most `--index-wait` seconds, counted once for both, before they bind, because a startup probe gives a container a bounded time to accept a connection; `--index-wait 0` binds at once and waits for nothing. A build or a read that is still running when a wait of `--index-wait` seconds gives up says so on standard error, one line each, and a build or a read that failed says why, one line; the next follow pass builds the index again, and the next `ctx open` reads what the warm-up did not. `--index-wait` bounds when the listeners bind, not when a command is answered: until the build is over every request waits for it, cheap ones included, and each waits before it takes the tree, so no request holds the tree while it waits. A `ctx open` that arrives while the deltas are still being read does not wait for that read: it reads what it needs, as the first one did before. Its `following` field says how the follow loop is doing: `ok`, and while its passes fail, `since` and `error`. `/healthz` answers any caller, so every `error` it carries is a fixed text, such as `the follow loop is failing; the server's log says why`, and the reason goes only to the server's log. A failing loop — the remote or GitHub's token mint unreachable — keeps the answer `200`, because a restart cannot reach them either, and a copy that cannot reach the remote at start cannot clone at all.

The server's standard error carries three fixed-form lines that a deployment's alerts can match, each at the start of a line; every line the server writes there is one line, with control characters shown as spaces and any credential or identity token replaced by `[redacted]`: a lease's own lines (`renewing the server lease: …` when Cloud Storage refuses a renewal) and the error that stops the server are one line too, though they quote Cloud Storage's or git's answers, which run over several, and so is a panic, `the server panicked at <where>: <what>`. The fetch job writes its standard error the same way. Every answer the write handlers give other than `200` from `/v1/records/put`, `/v1/records/put-batch`, `/v1/records/delete` or `/v1/marker/advance` prints `write failed: <route> <status> <reason>`, a write whose caller hung up before its answer included: the reason is one line of at most 300 characters with any credential replaced by `[redacted]`, a `422` gives `refused: <finding kinds>`, and a `401` gives the fixed `the caller was not identified`. An answer axum gives before a handler runs — `413` for a body over its 2 MiB limit, `405` for a wrong method — prints no line there. Each failed follow pass prints `following main: <why>`, and a pass that succeeds prints `follow heartbeat: ok` when 5 minutes have passed since the previous heartbeat. So if neither line appears for longer than 5 minutes plus one pull interval (`--pull-interval`, 60 seconds by default), the follow loop has stopped. Match each form at the start of the line: a refused path is quoted inside its `write failed:` line.

The server's standard output carries its request log: one line of JSON for each request either listener takes — axum's own `404`, `405` and `413` answers, and a request whose caller hung up before its answer, included — that says when, at which listener, who called, the session the caller says it is, which command, the record path of a `record show`, `record put` or `record rm` and the record paths of a batch put, how it ended, its status, how long it took and how many bytes the answer held. A search is logged by the length of its query, never its words, and no line carries a record's text, a quote or an identity token. A request never waits for its line: while nothing reads standard output, lines are dropped and the server answers on. [docs/deployment.md](docs/deployment.md#the-servers-request-log) lists the fields.

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

Who the caller is depends on where it runs. On a dev box the request goes over the tailnet and Tailscale's own login header names the caller, so `--tailnet-listen` takes a loopback address only: the sidecar runs beside the process, and anything else that could reach the port could name itself. Tailscale Serve sends no login header for a tagged node (a job or service that joined the tailnet with a tag rather than a person's login). With `--tailnet-allow-tag`, a request at `--tailnet-listen` without the login header is identified by the one address in its `X-Forwarded-For` header, which Tailscale Serve overwrites with the tailnet address the request came from: the server asks tailscaled's local API at `--tailscale-socket` who that address is (`/localapi/v0/whois`), and answers only a node carrying one of the allowed tags, named by that tag as both its name and its email (`tag:ctx-ingest`), which the request log's `caller` and the author of each commit it writes then show. A missing or repeated `X-Forwarded-For`, an address tailscaled does not know, an untagged node, a node whose tags are not allowed and a local API that does not answer are each refused with `401`; the login header, when present, always wins and the local API is not asked. Without `--tailnet-allow-tag` a request without the login header is refused, as before. Inside Google Cloud the client sends a Google identity token for `CTX_SERVER_AUDIENCE`, taken from the metadata server; where the workload has no Google service account of its own, `CTX_IMPERSONATE_SA` names one to mint the token as. `CTX_SERVER_AUDIENCE` must equal the origin of `CTX_SERVER` (`scheme://host[:port]`, no path), or `ctx` refuses and mints no token, so a token is only ever sent to the server it was minted for. The server accepts nothing else: a request without an identity is refused, and the ingress port ignores every `Tailscale-*` header. An identity token says which account minted it and which server it was meant for, and any Google account anywhere can mint one, so the ingress port also answers only the principals `--allow-principal` names — an account email, or `@domain` for a whole domain — and refuses everything else, whatever a deployment's own invoker policy allows.

Every request also names the caller's session, in an `X-Ctx-Session` header: `CTX_SESSION` when it is set, else `OMP_SESSION_ID`, which an Oh My Pi setup may export for its session, else `PI_SESSION_ID`, which Oh My Pi's bash tool sets for every command its agent runs from Oh My Pi 18.12.4 on, else `CLAUDE_CODE_SESSION_ID`, which Claude Code sets, else none; an empty variable counts as unset. An agent on an older Oh My Pi names its session with `CTX_SESSION`, as the ingest runner does. Many sessions reach the server under one identity — every dev box on a tailnet can sign in as the same person — and the request log tells them apart by it. It is the caller's word, not an identity: the server logs it and decides nothing by it. A session id that is not 1 to 128 ASCII letters, digits, `.`, `_`, `:` and `-` (one of white space, for example) is not sent, no other variable is sent in its place, and `ctx` says so on standard error.

`/v1/command` is the read door: it runs the allowlisted read commands only, `record show` among them, and refuses any argv that writes, such as `delta --write`, which replaces the newest window's files, or `record put` and `record rm`. Records are written through `/v1/records/put`, `/v1/records/put-batch` and `/v1/records/delete`, and the marker through `/v1/marker/advance`; each holds the write lease. A marker moved through the server names the signed-in caller and the session it names, and a window the caller names wrongly is answered by its id. No answer names the server's own directories: an error from any route, a finding, and what a command run through `/v1/command` prints show the clone as `<tree>`, the data directory as `<data>` and the state directory as `<state>` (`reading <data>/windows: No such file or directory`). The one exception is the record a `record show` that succeeds prints: it goes out as the tree holds it, because a cycle writes it back whole. The server's own log keeps the paths.

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

`version` is currently `1`. `id` names the tree's state subdirectory and is required when sources are configured. `data_dir` holds the files that machines share, described below. `folder_notes` gives folder-level records an additional wikilink name. `dossier.vitals` selects frontmatter fields displayed by `ctx dossier`. `ingest.backfill_hours` changes the default initial source window, and `ingest.exclude` names, by pointer pattern, messages the tree never ingests ([docs/sources.md](docs/sources.md#excluded-and-repeated-messages)). Source configuration is documented in [docs/sources.md](docs/sources.md).

`[context_source]` names an optional program that says what a system the graph does not ingest, such as an issue tracker, knows about one window's subjects: `command` (the program and its arguments, no shell), `callers` (who may read it: account emails or `@domain`, as `--allow-principal` takes them, or a tailnet tag `tag:<name>` for a tagged node, which the server must also allow with `--tailnet-allow-tag`; the ingest cycle's identity), `key_pattern` (a regular expression whose matches in the window's messages are subjects, such as `\b[A-Z][A-Z0-9]+-[0-9]+\b`), `link_prefixes` (a web address in a message, or in an open record's `tracked_by`, that starts with one of these is a subject) and `timeout_secs` (at most and by default 60). The program reads `{"window": <id>, "subjects": [<subject>…]}` on standard input, at most 30 subjects, and prints `{"items": [{"ref", "title", "status", "updated_at", "links"}…]}`, or `{"unavailable": "<reason>"}` when it cannot answer. Whatever it prints, the server reads at most 256 KB of it, strips control characters, cuts a title to 200 characters, keeps only whole `https://` links, keeps each item under 1.5 KB and the items under 48 KB, and counts what it left out in `dropped`. A program that is missing, exits non-zero, outlives its time or prints anything else makes the answer `unavailable`. Each run is one `context source: window …` line on standard error, with counts, bytes and milliseconds and never an item's text.

Two directories hold what `ctx` keeps outside the tree. The **data directory** holds the files every machine reading this tree needs: `windows/`, `cursors.json`, `marker.json`, `sources.json` (each source's verbatim declaration), `sources/<source id>/`, what a source must keep between fetches, such as the Slack threads whose parent is older than the window, and `embeddings/`, a cache of record and message vectors. `ctx fetch` writes everything except `marker.json`, `ctx marker advance` writes `marker.json`, and `ctx delta --write` rewrites the delta files of the window it selects. `embeddings/` has three writers: the fetch job's delta, the server, and any machine that runs `ctx search` or `ctx delta --write` with meaning search configured each add the vectors they compute. Each file there holds the vector one model gives one text, is named by that text's hash and is replaced atomically, so two writers that use one model write the same thing, a vector another model made is computed again rather than used, and no lease covers it. Every other command only reads the data directory. It is `CTX_DATA_DIR`, then `data_dir` in `ctx.toml`, and defaults to the state directory, so a single-machine tree needs no configuration. The **state directory** holds what each machine keeps for itself: `resolved/` (the records a live `ctx resolve` or `ctx cite verify --live` obtained here), `store.sqlite3` (an index rebuilt from the windows and from `resolved/`), the fetch lock, and the scratch space a `ctx source run` or a live resolve gives a source. It is `CTX_STATE_DIR`, then `state_dir` in the user configuration, then `$XDG_STATE_HOME/ctx`, then `~/.local/state/ctx`; each tree uses a subdirectory named for its `id`. One data directory holds one tree.

When more than one machine shares that data directory, `CTX_LEASE_BUCKET` names a Cloud Storage bucket and each writing role takes a lease object in it: `fetch` for the fetch job, `server` for the server. A local command that writes what one of those roles writes takes that role's lease: `ctx delta --write` replaces files the fetch writes, so it takes the `fetch` lease, and `ctx marker advance` moves the marker the server moves, so it takes the `server` lease. The holder renews its lease every 30 seconds and checks it before every write batch, once it holds what the batch writes, and judges the time since its last renewal after Cloud Storage has answered; another copy may take it over only after 120 seconds without a renewal, and a holder that cannot renew stops writing 30 seconds before that, so the copy that stops is never still writing when the next one starts. A holder whose check fails stops renewing and leaves the object in place, so the next copy waits out the window rather than start while a push may still be in flight. A copy that is asked to stop — `SIGTERM`, as a Cloud Run rollout sends — gives its lease up as it goes, so the copy that replaces it writes at once; the 120 seconds are for the copy that is killed outright. A fetch that cannot take the lease names the copy that holds it, exits non-zero and writes nothing; a write the server cannot cover with its lease is answered `503` and nothing is committed. A local `ctx delta --write` or `ctx marker advance` holds its lease for its one write and frees it afterwards, and `ctx delta --write` checks it again just before it writes; when another copy holds the lease, the command names the holder, exits `2` and writes nothing. A running server holds its lease the whole time, so where a server runs, move the marker through it, with `CTX_SERVER` set. Without `CTX_LEASE_BUCKET` nothing is leased, and one machine stays single through the lock `ctx fetch` takes on its state directory; a server or a fetch job on Cloud Run is never that machine, and refuses to start without the bucket.

`CTX_MEMORY_REMOTE` names a git URL to clone into `CTX_MEMORY` when that path holds no clone yet, and to fast-forward when it does. A container that starts with an empty filesystem — the server or the fetch job — reaches the tree with that and the app key below, and needs no credential helper and no ssh key: a private GitHub remote refuses an anonymous clone, so `ctx` mints a GitHub App installation token and sends it as an `Authorization` header on every git call, clone, fetch, pull and push alike. The token is never written into the remote URL, so it never reaches `.git/config`, a reflog or an error message, and never appears in a command line. `CTX_MEMORY` is on the container's own disk, never on the share: each copy keeps a clone of its own, and a starting copy puts a clone it made back the way the remote has it, which on a shared clone would throw away the write of the copy still running.

A credential a machine holds outside Secret Manager is named under `[credentials]`: `github_app = { file = "~/.config/ctx/github-app.json" }`. In Google Cloud, `CTX_SECRET_<NAME>` names a Secret Manager secret instead and the running identity reads it; nothing writes a credential into the tree. A source reads its own credential from the environment, as an external source does, so the Slack, Google and Matrix sources take `CTX_SECRET_<NAME>`, `CTX_CREDENTIAL_<NAME>_FILE` or `CTX_CREDENTIAL_<NAME>` (for example `CTX_SECRET_SLACK`, `CTX_CREDENTIAL_GOOGLE` or `CTX_CREDENTIAL_MATRIX`) and read no `[credentials]` entry. `ctx slack login` reads `slack_app` through `CTX_SECRET_SLACK_APP` and writes the `slack` credential through `CTX_SECRET_SLACK`; the fetch job reads both when it refreshes the Slack token, and writes the renewed `slack` credential.

## The Slack app

ctx reads Slack through a Slack app that nobody else signs in with: the ctx-memory app, an internal app of the workspace with five read scopes, no bot user and token rotation on, created from [`slack/app-manifest.yaml`](slack/app-manifest.yaml). Its tokens expire every 12 hours, and the fetch job refreshes them with the app's client secret while it holds the fetch lease, saving each new credential before it uses it. It used to sign in with the Claude Code Slack app, whose tokens rotate and which Oh My Pi's Slack connection also uses. Slack's [Using token rotation](https://docs.slack.dev/authentication/using-token-rotation) says: "If you refresh your credentials repeatedly before expiration (e.g., by calling the `oauth.v2.access` API method multiple times for the same token within a 12 hour period), we will enforce a limit of 2 active tokens. If more than 2 tokens exist after the refresh, the oldest additional token will be revoked." Oh My Pi refreshes its token about 20 times a day, and the token ctx stored answered `token_revoked` at its first fetch; that both count against one such limit is the diagnosis's best reading, supported but not proven. With its own app, ctx is the only holder of the app's tokens, so no other holder's refreshes count against that limit. A credential the earlier sign-in wrote is still read and still refreshed.

Setting the app up takes three steps: create it from the manifest at [api.slack.com/apps](https://api.slack.com/apps), store its client id and secret as the JSON `{client_id, client_secret}` in the secret `CTX_SECRET_SLACK_APP` names, and run `ctx slack login` once. [docs/sources.md](docs/sources.md#the-ctx-memory-slack-app) gives each step's commands.

## Deploying

A team runs three programs against one tree and one shared data directory: the server, the scheduled fetch job, and the scheduled ingest cycle, a headless Oh My Pi session that turns each fetch's windows into records through the server. The ingest cycle's runner — [`runner/run-cycle.sh`](runner/run-cycle.sh), the shell policy its session runs under, the record tools it writes through, and the Oh My Pi release it is tested against — ships in this repository and in each release, beside the skills. [docs/deployment.md](docs/deployment.md) is the contract: what an image installs, and the arguments and environment each program takes. Each release publishes an image for the server and the fetch job and one for the ingest cycle; a deployment supplies its model provider, accounts, share and schedules.

### The images

Each `v*` release publishes two images for `linux/amd64`, tagged with the release's version and, for the newest release, `latest`; [`.github/workflows/images.yml`](.github/workflows/images.yml) builds them from that release's own archives, checked against its `SHA256SUMS` and its build attestations:

- `ghcr.io/sjawhar/ctx-memory/server` ([`images/server/Dockerfile`](images/server/Dockerfile)): `ctx`, `git` and the CA certificates. Its entrypoint is `ctx` with no default command, so the server runs `serve …` and the fetch job `fetch`.
- `ghcr.io/sjawhar/ctx-memory/agent` ([`images/agent/Dockerfile`](images/agent/Dockerfile)): `ctx`, the release's runner and skills, and the Oh My Pi release `runner/omp.lock` pins. Its entrypoint is `run-cycle.sh`: no arguments run one ingest cycle, `maintain <pass>` one maintenance pass.

No arm64 image is published: the agent runs the pinned Oh My Pi build, which has no `linux-arm64` archive. Each image carries a GitHub build attestation: `gh attestation verify oci://ghcr.io/sjawhar/ctx-memory/server:<version> --repo sjawhar/ctx-memory`.

The server and the fetch job take the environment in [docs/deployment.md](docs/deployment.md#the-server-and-the-fetch-job). Mount the share and name it in `CTX_DATA_DIR`; keep the clone (`CTX_MEMORY`) and the state directory (`CTX_STATE_DIR`, `/var/lib/ctx` in the image) on the container's own disk. Outside Google Cloud, give the GitHub App key as a file through `CTX_CREDENTIAL_GITHUB_APP_FILE` rather than `CTX_SECRET_GITHUB_APP`. The image runs as root, because a share often lets only root write. For example, a server that a Tailscale sidecar in the same network namespace reaches on loopback:

```bash
docker run -d --name ctx-server --network container:tailscale \
  -v /srv/ctx-data:/data -e CTX_DATA_DIR=/data \
  -e CTX_MEMORY=/var/lib/ctx/tree -e CTX_MEMORY_REMOTE=https://github.com/<owner>/<tree>.git \
  -v /srv/ctx-secrets/github-app.json:/run/secrets/github-app.json:ro \
  -e CTX_CREDENTIAL_GITHUB_APP_FILE=/run/secrets/github-app.json \
  ghcr.io/sjawhar/ctx-memory/server:<version> serve --tailnet-listen 127.0.0.1:8080
```

The fetch job runs the same image with `fetch` and the sources' credentials ([docs/sources.md](docs/sources.md)).

The ingest cycle takes the environment in [docs/deployment.md](docs/deployment.md#the-jobs-environment): `CTX_SERVER` (and `CTX_SERVER_AUDIENCE` for a server that takes Google identity tokens), `CTX_DATA_DIR`, `CTX_MODEL_BASE_URL`, `CTX_MODELS_FILE`, `CTX_MODEL`, `ANTHROPIC_API_KEY` and `CLOUD_RUN_EXECUTION`, a name unique to each run. Start it as root, as the image does; it runs the session as `ctx-agent`, uid and gid 10001, which must be able to read the share's windows and write nothing else on it. The image carries no model provider, and `run-cycle.sh` refuses to start without one. The provider file is the deployment's own:

- Copy [`runner/models.example.yml`](runner/models.example.yml) and change the provider's name, `api` and models. Keep its `baseUrl` line, `    baseUrl: "@CTX_MODEL_BASE_URL@"`, and its `apiKey` line, `    apiKey: "!cat /run/ctx-agent/model-key"`, exactly as they are, four spaces in: the runner puts `CTX_MODEL_BASE_URL` in place of the placeholder, writes `ANTHROPIC_API_KEY` to `/run/ctx-agent/model-key` and removes it from the environment, and refuses a file with any other address or key line.
- Mount it read-only at a path with no symbolic link, such as `/etc/ctx-memory/models.yml` (`/etc/ctx-memory` is root's in the image), and name that path in `CTX_MODELS_FILE`. Neither the file nor any directory above it may be owned by uid 10001 or writable by group or others. On Kubernetes, mount a ConfigMap key with `subPath`, because a ConfigMap volume's files are symbolic links.
- Set `CTX_MODEL` to `<provider>/<model id>` of that file.

```bash
docker run --rm \
  -v /srv/ctx-data:/data -e CTX_DATA_DIR=/data \
  -v /srv/ctx-config/models.yml:/etc/ctx-memory/models.yml:ro \
  -e CTX_MODELS_FILE=/etc/ctx-memory/models.yml -e CTX_MODEL=<provider>/<model id> \
  -e CTX_MODEL_BASE_URL=https://<model API host> -e ANTHROPIC_API_KEY \
  -e CTX_SERVER=https://<ctx server> \
  -e CLOUD_RUN_EXECUTION="cycle-$(date -u +%Y%m%dT%H%M%SZ)" \
  ghcr.io/sjawhar/ctx-memory/agent:<version>
```

Where the server is reached at `--listen`, add `-e CTX_SERVER_AUDIENCE=https://<ctx server>`: `ctx` then sends a Google identity token, which it takes from the Google metadata server (or mints as `CTX_IMPERSONATE_SA`), so the container needs a Google identity ([Remote mode](#remote-mode)). Run the cycle on a schedule, such as hourly; [docs/deployment.md](docs/deployment.md#what-the-exit-status-means) says what each exit status means.

## Extend

A source is declared in `ctx.toml` and may be built in or external. The built-in kinds are `slack`, `gmail`, `drive`, `calendar`, `github` and `matrix`. An external source named `kind = "linear"` runs `ctx-source-linear` from `PATH`; it receives one JSON request on standard input and emits a JSON Lines protocol on standard output. See [docs/sources.md](docs/sources.md) for each built-in source, the protocol, cursor rules, and conformance command.

Unknown commands use the same convention: `ctx report ...` runs `ctx-report` from `PATH` when `report` is not a built-in command. The name may hold only ASCII letters, digits, `-` and `_`; `ctx` refuses any other name before it searches `PATH`, so a name cannot be a path to another program. Like git and cargo plugins, external commands own their stdout and exit status; `ctx` passes the resolved tree and state paths, JSON flag, and `ctx` version through `CTX_*` environment variables. The v1 JSON envelope and `0`/`1`/`2` exit-code contract apply only to built-in commands.

Two agent skills ship with the package: [skills/ctx-memory/SKILL.md](skills/ctx-memory/SKILL.md) is the reading and writing protocol, and [skills/ctx-memory-ingest/SKILL.md](skills/ctx-memory-ingest/SKILL.md) is the ingest cycle. The reasoning behind their rules is in [docs/method.md](docs/method.md).

## License

MIT. See [LICENSE](LICENSE).
