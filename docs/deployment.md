# Deploying ctx-memory

A team deployment runs three programs against one tree, kept in a git repository, and one data directory on a share every program mounts:

- **the server**, `ctx serve`, the only writer of records, which every agent and person reads and writes through ([README](../README.md#remote-mode));
- **the fetch job**, `ctx fetch` on a schedule, which stores each source's new messages as a window in the data directory ([sources.md](sources.md));
- **the ingest cycle**, [`runner/run-cycle.sh`](../runner/run-cycle.sh) on a schedule, one headless Oh My Pi (omp) session that turns the pending windows into records through the server ([the ingest skill](../skills/ctx-memory-ingest/SKILL.md)).

This page is the contract between those programs and the deployment that runs them: what to install, the arguments and environment each takes, and what each checks. It names no platform account, project or address; a deployment supplies its own. The programs were built to run as Cloud Run services and jobs with a Filestore share, and the names below that are Cloud Run's or Google Cloud's say so.

## What a release holds

Each `v*` tag publishes, through [`.github/workflows/release.yml`](../.github/workflows/release.yml), once the tagged commit passes CI:

- `ctx-x86_64-unknown-linux-musl.tar.gz` and `ctx-aarch64-unknown-linux-musl.tar.gz`, static Linux builds, and `ctx-aarch64-apple-darwin.tar.gz`, each holding the `ctx` binary alone;
- `ctx-memory-<version>-share.tar.gz`, the tag's `skills/` and `runner/` trees, which an image installs beside the binary;
- `SHA256SUMS`, and a GitHub build attestation of each archive (`gh attestation verify <archive> --repo sjawhar/ctx-memory`).

An image builds from a release, never from a checkout: download the archives by URL, check each against a SHA-256 the image's own build pins, and unpack them. The share archive's `runner/omp.lock` names the omp release the runner is tested against and that archive's SHA-256, so an image that installs omp from it runs the build the runner's tests drove.

## The server and the fetch job

Both run the `ctx` binary in an image that also holds `git` and the CA certificates (`ca-certificates` on Debian). Neither keeps a tree on the share: each clones its own into its container's disk.

| Variable | Server | Fetch job | What it is |
| --- | --- | --- | --- |
| `CTX_MEMORY` | yes | yes | Where the container keeps its clone of the tree, on its own disk. |
| `CTX_MEMORY_REMOTE` | yes | yes | The tree's git URL. At start `ctx` clones it into an empty `CTX_MEMORY`, puts a clone it made itself back the way the remote has it, and fast-forwards any other clone. |
| `CTX_SECRET_GITHUB_APP` | yes | yes | The Secret Manager secret holding the GitHub App's `{app_id, private_key}`. `ctx` mints installation tokens from it, keeps each until it is near expiry, and sends one on every git call; the GitHub source reads with it too. |
| `CTX_DATA_DIR` | yes | yes | The data directory on the share. |
| `CTX_STATE_DIR` | yes | yes | The container's own state directory, never on the share. |
| `CTX_LEASE_BUCKET` | yes | yes | The Cloud Storage bucket the leases live in. Required on Cloud Run: two copies can run at once there, and the lease keeps one the only writer. |
| `CTX_SERVER_AUDIENCE`, `CTX_SERVER_PRINCIPALS` | yes | | The audience every identity token at `--listen` must carry, and who may call it (`--audience`, `--allow-principal`). |
| `CTX_EMBED_MODEL`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION` | optional | optional | Meaning search with a Vertex AI embedding model. |
| `CTX_SECRET_SLACK`, `CTX_SECRET_SLACK_APP`, `CTX_SECRET_GOOGLE`, `CTX_GITHUB_INSTALLATIONS` | | as configured | Each source's credential and settings ([sources.md](sources.md)). |
| `CTX_EXCLUDE` | yes | yes | Pointer patterns of messages the deployment never ingests, comma-separated, added to the tree's `ingest.exclude` ([sources.md](sources.md#excluded-and-repeated-messages)). Set it on both: the fetch job skips and drops what it names, and the server leaves it out of every delta it answers with, including windows fetched before the pattern existed. |

The server's arguments are `serve --listen <addr> --allow-principal <email|@domain>…` for callers that bring a Google identity token, and `--tailnet-listen 127.0.0.1:<port>` for a Tailscale sidecar in the same instance, which reaches it on loopback and names the caller with Tailscale's login header. Run one instance at a time: a server holds the `server` lease while it runs, and a copy that cannot cover a write with the lease answers it `503` and commits nothing. A startup probe can connect to the `--listen` port, which the server opens only after its first health check passed; `/healthz` is the health endpoint ([README](../README.md#remote-mode)).

The fetch job's argument is `fetch`. Its timeout must not be longer than 3,600 seconds: at fetch start it renews a Slack token that expires within that time (`FETCH_JOB_MAX_RUNTIME_SECONDS` in `src/api/slack.rs`), so a longer run could outlive the token it renewed. The Slack app it signs in with is created from [`slack/app-manifest.yaml`](../slack/app-manifest.yaml) ([sources.md](sources.md#the-ctx-memory-slack-app)).

Where the share lets only root write, both containers run as root.

## The ingest cycle

The cycle reads text that people outside the deployment wrote, so its session runs with the narrowest rights that still do the work, and `run-cycle.sh` refuses to start one when its image or its job does not hold what follows. Each refusal is one `run-cycle: refusing to start the session: …` line on standard error and exit status 1. Its header comment says why each check is there.

### The image

- `ctx` and `omp` on `PATH`. omp is the release `runner/omp.lock` pins, its archive checked against the SHA-256 there before it is unpacked. `jq` and `setpriv` (util-linux) are installed; `gh` is not, because omp's `issue://` and `pr://` reads run it at the read tier, which the policy approves.
- `runner/run-cycle.sh` on `PATH` as the entrypoint, which starts as root (no `USER`), and `runner/policy.yml` and `runner/ctx-record.ts` at `/opt/ctx-memory/policy.yml` and `/opt/ctx-memory/ctx-record.ts`, owned by root.
- The two skills of the release, `skills/ctx-memory` and `skills/ctx-memory-ingest`, in a directory root owns, and `$HOME/.omp/agent/config.yml` naming that directory and no other under `skills.customDirectories`. `omp skill list --json` run as `ctx-agent` lists exactly those two `custom:user` skills.
- A user `ctx-agent`, the session's user. `HOME` is its home, and that home, everything in it and the working directory are root's and closed to writes, with a `.bashrc`, `.profile`, `.bash_profile` and `.bash_logout` of root's in it.
- `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` set, each with an `omp` directory owned by root, group `ctx-agent`, mode `1775`; `$XDG_DATA_HOME/omp/plugins` and `$XDG_DATA_HOME/omp/natives` root's and closed to writes. omp extracts its native module on its first run, so a build step that runs omp once as `ctx-agent`, then gives `natives` to root, leaves the job nothing to write there.
- The model provider file, at a path root owns in a directory root owns (see `CTX_MODELS_FILE` below).

### The job's environment

| Variable | What it is |
| --- | --- |
| `CTX_SERVER`, `CTX_SERVER_AUDIENCE` | The server's URL and the audience of the identity token `ctx` mints for it. Every command of the cycle runs on the server, so the job holds no tree, no store and no source credential. |
| `CTX_DATA_DIR` | The data directory on the share. The session reads the windows there and writes its JSONL transcript under `agent-sessions/<execution>/`, which goes back to root when the session ends. `ctx-agent` may read the windows and write nothing else on the share. |
| `CTX_MODEL_BASE_URL` | The model API's `http` or `https` origin, with no user, path or query. Every model call goes there. |
| `CTX_MODELS_FILE` | The model provider file, by absolute path with no symbolic link: an omp `models.yml` naming the one provider the session may call, as in [`runner/models.example.yml`](../runner/models.example.yml). Past its comment lines, one line names `baseUrl`, `    baseUrl: "@CTX_MODEL_BASE_URL@"`, and one names `apiKey` or the key file, `    apiKey: "!cat /run/ctx-agent/model-key"`, each four spaces in; so no model or second provider names an address of its own, and the key goes nowhere else. `run-cycle.sh` writes the file, with the address in place of the placeholder, to `$HOME/.omp/agent/models.yml` as root before each session. It refuses a file that breaks those rules, and one that `ctx-agent` could change: the file or any directory above it owned by `ctx-agent` or writable by group or others, other than a directory of root's with the sticky bit, such as `/tmp`. Those line checks keep the file from routing a call or the key elsewhere by mistake; what the file may otherwise say is the deployment's to keep, as is the rest of the image. |
| `CTX_MODEL` | The model the session selects, `<provider>/<model id>` of that file, as omp's `--model` takes it. |
| `ANTHROPIC_API_KEY` | The key the model calls carry, as a secret environment variable. `run-cycle.sh` writes it to `/run/ctx-agent/model-key`, readable only by `ctx-agent`, and unsets it before the session starts. |
| `CLOUD_RUN_EXECUTION` | The execution's name. It names the transcript directory and, as `CTX_SESSION`, the `Session:` trailer of every commit the session makes. Cloud Run sets it; another platform sets it to a name unique to each run. A cycle whose session is refused by the model runs one fresh session, in `agent-sessions/<execution>-retry/`, and that one's records carry `<execution>-retry`. |
| `CTX_CYCLE_MAX_WINDOWS` | Optional: the most pending windows one session handles, oldest first. `run-cycle.sh` passes it to `ctx pending` as `--limit`, and names it in the session's prompt, so the session reads only that many and leaves the rest for the next cycle. A refused cycle's second session is capped the same, so a retried execution can touch up to twice this many across its two sessions. Unset, a session handles every pending window. A whole number greater than zero, or the job is refused. Use it to keep one session from reading a whole backlog at once when a source has fallen far behind. |

`CTX_MODELS_FILE` and `CTX_MODEL` belong with the provider file, so an image that carries the file usually sets both. The runner has no default for either.

### Maintenance passes

`run-cycle.sh maintain <pass>` runs one maintenance pass instead of a cycle: a one-time job over the whole tree, whose instructions are the ingest skill's `maintenance/<pass>.md`, in the same session a cycle runs, with the same image, environment, checks and policy. It handles no window and moves no marker, so it runs whether or not a window is pending. The passes are the ones `MAINTENANCE_PASSES` in `run-cycle.sh` names; any other argument is refused before anything else. On Cloud Run, run one with the job's own image and environment and the arguments overridden, for example `gcloud run jobs execute <job> --args=maintain,waiting-on`.

- `waiting-on` re-checks `waiting_on` on every open record against the sources it cites, by the ingest skill's rule, for a tree whose records were written before that rule. Run it once after upgrading to a release that has it.

### What the exit status means

An execution that exits 0 finished its work: a cycle found no pending window, or its session ended on the model's final answer and omp reported success. A maintenance pass exits 0 only when, beyond that, its last message ends with the pass's completion line, `<pass> pass complete`, which its prompt tells the session to end its report with, and the marker is where the pass found it; a deploy step that waits on a pass can take the execution's success as the pass done. Every other end exits non-zero, with a line that says why:

- `run-cycle: refusing to start the session: …` on standard error, status 1: a check before a session failed, so that session did not run. Most checks come before the first session; sealing a refused cycle's first session and checking the share again come before its fresh session, so there the first session has run.
- `run-cycle: ctx pending failed (exit N)`, or `run-cycle: ctx marker show failed before the <pass> pass (exit N)` (or `after`), on standard error, status N: a read around the session failed.
- `run-cycle: the session stopped at its 50-minute time limit before it finished` on standard error, status 1: omp's time limit stopped the session. When the limit passes while a tool runs, the pinned omp ends the session and exits 0, so the runner reads how the session ended from omp's own events. The records the session wrote and the windows it moved the marker past stand, so the next cycle starts where it stopped; run a maintenance pass stopped this way again.
- `run-cycle: the session exited 0 but did not finish: …` on standard error, status 1: omp reported success, but the model's last message is not a final answer: there is none, it is empty (the pinned omp also exits 0 when the model keeps answering with nothing past its retries), or it stopped for another reason, such as its length limit.
- `run-cycle: the <pass> maintenance pass ended without its completion report: …` on standard error, status 1.
- `run-cycle: the <pass> maintenance pass moved the marker, which it must not do` on standard error, status 1 unless omp already failed.
- Any other status is omp's own, after `run-cycle: the session exited with status N; …` on standard output: the session failed in omp, with a model error or a signal among the causes. A cycle whose session the model refused first runs one fresh session, and exits with that session's outcome, judged as above.

### Checking an image

The runner's own tests (`tests/agent_cycle/`) check the policy against every `ctx` command the skills name, run `run-cycle.sh` against stand-ins for each refusal, run what it does after a session against omp's events, and drive live sessions of the pinned omp. What holds only in a built image — that the session runs as `ctx-agent` with no new privileges, that its model calls reach `CTX_MODEL_BASE_URL` with the job's key as the bearer token, that a `SIGTERM` reaches the session and the transcript is sealed, that no file the session tries to write lands — is the deployment's to check, by running the image as its job runs it, as root with a share mounted, against a stand-in for the server and the model API.
