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
| `CLOUD_RUN_EXECUTION` | The execution's name. It names the transcript directory and, as `CTX_SESSION`, the `Session:` trailer of every commit the session makes. Cloud Run sets it; another platform sets it to a name unique to each run. |

`CTX_MODELS_FILE` and `CTX_MODEL` belong with the provider file, so an image that carries the file usually sets both. The runner has no default for either.

### Checking an image

The runner's own tests (`tests/agent_cycle/`) check the policy against every `ctx` command the skills name, run `run-cycle.sh` against stand-ins for each refusal, and drive live sessions of the pinned omp. What holds only in a built image — that the session runs as `ctx-agent` with no new privileges, that its model calls reach `CTX_MODEL_BASE_URL` with the job's key as the bearer token, that a `SIGTERM` reaches the session and the transcript is sealed, that no file the session tries to write lands — is the deployment's to check, by running the image as its job runs it, as root with a share mounted, against a stand-in for the server and the model API.
