# Sources

A source imports and resolves material outside the Markdown tree. Built-in source kinds include `slack`, `gmail`, `drive`, `calendar`, and `github`. A third-party source is an executable named `ctx-source-<kind>` on `PATH`.

## Protocol

`ctx` starts an external source with no arguments. It sends one JSON request on standard input and closes standard input. The source writes one UTF-8 JSON message per line to standard output and may write diagnostics to standard error. Credentials arrive through environment variables named by the source configuration; they do not belong in a request, window, or manifest.

`state_dir` in the request is the directory the source may keep files in between runs. A fetch names `<data_dir>/sources/<source id>`, which every machine fetching this tree shares, so material the source can reach only through that file — a Slack thread whose parent is older than the window — is not lost when the next fetch runs on another machine. `ctx source run` and a live `ctx resolve` name `<state_dir>/sources/<source id>` instead: they store no record and move no cursor, so a diagnostic or a read cannot change what the next fetch reads.

The first output line is a `hello` message with protocol version `1`, the configured scheme, supported operations, and a `verbatim` flag. For `fetch`, it also names the complete stream list. A fetch emits records followed by exactly one `stream_end` for every planned stream. A successful `stream_end` proposes the cursor to use after a durable commit. A resolve request emits records for found pointers; a missing requested pointer is not found.

For example, configure a third-party Linear source with `id = "linear"`, `kind = "linear"`, and `scheme = "linear"`. A fetch request can look like this:

```json
{"op":"fetch","protocol":1,"source":{"id":"linear","kind":"linear","scheme":"linear"},"config":{"team":"ENG","token_env":"LINEAR_TOKEN"},"window_end":"2026-01-02T18:00:00Z","backfill_hours":24,"cursors":{"ENG":"2026-01-02T16:00:00Z"},"discovered":[],"state_dir":"/data/example/sources/linear"}
```

Its protocol stream can look like this:

```jsonl
{"type":"hello","protocol":1,"scheme":"linear","ops":["fetch","resolve"],"verbatim":true,"streams":["ENG"]}
{"type":"record","pointer":"linear:ENG-42/c-1","stream":"ENG","ts":"2026-01-02T17:12:09Z","label":"ENG","author":"Dana","author_id":"U0EXAMPLE9","thread":"linear:ENG-42","title":"Example issue","url":"https://linear.app/example/issue/ENG-42","text":"Dana confirmed the example issue is ready for review.","aliases":["ENG-42"],"context":false,"raw":{"id":"c-1","issue":"ENG-42"}}
{"type":"stream_end","stream":"ENG","status":"ok","cursor":"2026-01-02T18:00:00Z"}
```

`ctx` treats a nonzero source exit, timeout, invalid JSON, a mismatched pointer scheme, an unplanned stream, a non-context record outside the window, or a missing `stream_end` as an error for every unfinished stream. Context records may fall outside the fetch window and never advance a cursor. Error streams do not propose a usable cursor.

## Cursors

A cursor is an opaque value per source and stream, and it names only a point whose material a window already carries. `ctx` stores it and returns it to the source unchanged. Slack uses RFC 3339 timestamps, and GitHub the time its issues feed has reached and the default branch head it stored, written `feed=<time>[,issue=<number>][,head=<sha>]@<stored>`, or `<time>` / `<time>@<stored>` when it names neither an issue nor a head (see [GitHub source](#github-source)). Gmail, Drive's changes feed and Calendar use the token their API issued, with the time its stream is stored through beside it, written `<token>@<rfc3339>`, so a run whose token the API refuses lists again from that time; Drive's `meet` stream uses an RFC 3339 timestamp. A Gmail or Drive first listing read in slices names the time it has listed through with the token it took before the first slice: `<rfc3339>#<token>` for Drive, `<began>/<rfc3339>#<historyId>` for Gmail (see [Google sources](#google-sources)). A stream without a cursor receives the configured backfill window, which defaults to 24 hours.

Every built-in stream bounds what one run reads, so a long gap — after an outage, or a first fetch with a long backfill — takes several runs and every run moves its cursor. A stream that lists by time (Slack channels, a Gmail listing, Drive's first listing and its `meet` stream) reads at most seven days past its cursor and names the end of that slice as its cursor, with the token a Gmail or Drive listing carries. A stream that follows a feed stops after a fixed amount of it and names where it stopped: a Gmail history walk after 2,000 messages, the Drive changes feed after the page that brings it to 500 files, and a GitHub repository when the run's request budget runs out. Calendar is the exception: a sync token comes only at the end of a listing that covers everything from its start, so a resync is one listing, at one request per 100 events.

`ctx fetch` writes each stream's `cursor_used` and proposed `cursor_next` to the window manifest, and moves the cursor of every stream whose messages it stored, while it still holds the fetch lock. A stream that errored or proposed no cursor keeps its own cursor, so the next fetch re-reads that stream and no other. A fetch given an older `--window-end` than the window that last moved a stream leaves that stream's cursor alone and reports it as skipped.

Cursors live in `<data_dir>/cursors.json`. Read them with:

```bash
ctx cursor show
```

What the agent has turned into records is a separate position, the marker in `<data_dir>/marker.json`: `ctx pending` lists the completed windows after it, and `ctx marker advance <window>` moves it after the commit that handled the window.

## What each directory holds

The data directory is shared between machines and holds `windows/` (one directory per fetch), `cursors.json`, `marker.json`, `sources.json`, which records whether each source declared its records verbatim in its protocol hello, `sources/<source id>/`, what each source keeps between fetches, and `embeddings/`. `ctx fetch` writes everything in the data directory except `marker.json`, `ctx marker advance` writes `marker.json`, and `ctx delta --write` rewrites the delta files (`delta.json`, `delta.md` and `open-snapshot.json`) of the window it selects. With `CTX_LEASE_BUCKET` set, each of them writes under a lease: a fetch and `ctx delta --write` under the `fetch` lease, and `ctx marker advance` under the `server` lease the server moves the marker with (see the README's "Tree configuration"). `embeddings/` is the exception: a cache of record and message vectors that the fetch job's delta, the server, and any machine that runs `ctx search` or `ctx delta` with meaning search configured all add to. Each file holds the vector one model gives one text, is named by that text's hash and is replaced atomically, so a reader never sees half a file, two writers that use one model write the same thing, a vector another model made is computed again rather than used, and no lease covers it. No other read command writes the data directory. `ctx cite verify` needs the verbatim declaration, so a store rebuilt from the files keeps checking the same quotes.

The state directory belongs to one machine. `store.sqlite3` is an index and is replaced whenever it is missing or was written by another schema; it is replayed from the windows and from `resolved/`, the records a live `ctx resolve` or `ctx cite verify --live` obtained on this machine. `sources/<source id>/` here holds only the scratch space of a `ctx source run` or a live resolve.

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
workspace = "example"
channels = ["G0PRIVATE01"]
thread_poll_days = 30

[[sources]]
id = "gmail"
kind = "gmail"

[[sources]]
id = "linear"
kind = "linear"
scheme = "linear"
token_env = "LINEAR_TOKEN"
team = "ENG"
```

Every source needs a unique `id` matching `[a-z0-9][a-z0-9_-]{0,62}` and a unique pointer scheme. Built-in kinds choose their own default schemes; an external kind needs `scheme`. Source-specific keys are passed unchanged as the request's `config` object.

## Slack source

The `slack` kind calls Slack's Web API with one user OAuth credential, named `slack`. In Google Cloud `CTX_SECRET_SLACK` names its Secret Manager secret; on a machine outside, `CTX_CREDENTIAL_SLACK_FILE` names a file holding it and `CTX_CREDENTIAL_SLACK` carries the value. The value is the JSON `ctx slack login` writes, `{"kind": "app", "access", "refresh", "expires", "client_id", "token_url", "team_id", "user_id"}`: a rotating user token of the ctx-memory Slack app. A credential an earlier build wrote with the Claude Code Slack app, `{access, refresh, expires, client_id, token_url, resource}` with no `kind`, is still read, and still refreshed as below.

### The ctx-memory Slack app

ctx signs in with a Slack app of its own, which nobody else signs in with: an internal app of the workspace, created from [`slack/app-manifest.yaml`](../slack/app-manifest.yaml). It asks for five user scopes (`channels:history`, `channels:read`, `groups:history`, `groups:read`, `users:read`), has no bot user and no event subscriptions, and has token rotation on (`settings.token_rotation_enabled: true`). Slack's [Using token rotation](https://docs.slack.dev/authentication/using-token-rotation) says what that means: "With token rotation, it expires every 12 hours." Each refresh hands back a new refresh token, and "The previous token stops working as soon as it is redeemed"; "Refresh tokens are designed to be used once. After calling the `oauth.v2.access` API method, the refresh token you used is revoked after a short grace period." The access token in hand stays usable for a while: "Calling the `oauth.v2.access` API method will not immediately invalidate your existing access token." Rotation cannot be turned off once it is on.

ctx used to sign in with the Claude Code Slack app, whose tokens rotate and which Oh My Pi's Slack connection also uses. The same page says: "If you refresh your credentials repeatedly before expiration (e.g., by calling the `oauth.v2.access` API method multiple times for the same token within a 12 hour period), we will enforce a limit of 2 active tokens. If more than 2 tokens exist after the refresh, the oldest additional token will be revoked." In one observed case Oh My Pi refreshed its token of that app 19 times in a day on one machine, and the token `ctx slack login` stored there answered `token_revoked` at its first fetch, with more than 5 hours of its life left. That ctx's sign-in and Oh My Pi's refreshes count against one such limit is the diagnosis's best reading, supported but not proven: the documentation does not say so in words, Slack keeps no record of it, and no other documented cause fits. With an app of its own, ctx is the only holder of the app's tokens, and only the fetch job refreshes them, once each time a token comes within an hour of its expiry, so no other holder's refreshes can push ctx's token out.

Setting the app up takes three steps, by a person who may create apps in the workspace:

1. **Create the app.** At [api.slack.com/apps](https://api.slack.com/apps), choose **Create New App**, then **From a manifest**, pick the workspace, paste `slack/app-manifest.yaml`, and create it.
2. **Store its client id and secret.** On the app's **Basic Information** page, under **App Credentials**, copy the **Client ID** and the **Client Secret**, and add them as a version of the Secret Manager secret `CTX_SECRET_SLACK_APP` names, as the JSON `{"client_id": "…", "client_secret": "…"}`. The secret must exist first, with the fetch job's identity allowed to read it, or `gcloud` answers `NOT_FOUND`. With the secret `ctx-slack-app` in the project `example-project`:

   ```bash
   printf 'Client ID: '; read -r client_id
   printf 'Client Secret: '; stty -echo; read -r client_secret; stty echo; echo
   printf '{"client_id":"%s","client_secret":"%s"}' "$client_id" "$client_secret" \
     | gcloud secrets versions add ctx-slack-app --project example-project --data-file=-
   unset client_secret
   ```

3. **Sign in once.** Run `ctx slack login` with `CTX_SECRET_SLACK_APP` and `CTX_SECRET_SLACK` naming the two secrets, open the URL it prints, and choose **Allow**. That authorization installs the app for the person signing in, and the fetch job reads as that person from then on. It reads the app's secret and writes the sign-in as the identity the Google metadata server (or `GCE_METADATA_HOST`) answers for, which must hold Secret Accessor on the first and Secret Version Adder on the second. The fetch job's own identity needs the same two grants, because it refreshes the token with the app's client secret. With the secrets of step 2:

   ```bash
   CTX_SECRET_SLACK_APP=projects/example-project/secrets/ctx-slack-app \
   CTX_SECRET_SLACK=projects/example-project/secrets/ctx-slack \
   ctx slack login
   ```

`ctx slack login` reads the app's client id and secret from the `slack_app` credential: `CTX_SECRET_SLACK_APP` names its Secret Manager secret, and on a machine outside Google Cloud `CTX_CREDENTIAL_SLACK_APP_FILE` names a file holding it and `CTX_CREDENTIAL_SLACK_APP` carries it. It sends the browser to Slack's OAuth v2 authorize page with the app's `user_scope` and no bot scope, binds `127.0.0.1:3118` for the `http://localhost:3118/callback` redirect, and also accepts a pasted redirect URL on standard input, for a box whose port is not forwarded. It exchanges the code at `oauth.v2.access`, presenting the client id and secret with HTTP Basic, as that method's reference recommends. A token without an expiry and a refresh token means the app has token rotation off, which would leave the fetch nothing to renew; the login refuses it and saves nothing. Otherwise it saves the credential as a new version of the secret `CTX_SECRET_SLACK` names, as the identity the Google metadata server (or `GCE_METADATA_HOST`) answers for, before it reports which user and workspace the token reads as. Nothing it prints or stores carries the client secret. Slack's OAuth guide asks for HTTPS redirect URLs, but its PKCE guide describes how it treats an `http://localhost` redirect of an app that has never enabled PKCE ([Using PKCE](https://docs.slack.dev/authentication/using-pkce)), and the Claude Code app registers this same redirect.

### Refreshing and revoked tokens

Only a `ctx fetch` that holds the fetch lease refreshes the token, so two processes never redeem one refresh token. That is the fetch job, which runs with `CTX_LEASE_BUCKET` and takes the lease before it reads any source; it asks again just before it redeems the refresh token, and a lease not renewed for 90 seconds, the age at which the fetch stops writing, counts as not held. A fetch without a lease bucket (a dev box's), a live `ctx resolve` and the server use the stored token as it is and never refresh it; when it expires within the hour, such a fetch fails the Slack streams and says so. `ctx source run <slack-id> --op fetch` is refused, so a diagnostic source run cannot rotate the credential; use `ctx fetch --source <slack-id>` instead.

At fetch start, the lease holder renews an access token that expires within the maximum fetch runtime of 3,600 seconds. The deployment's fetch job timeout must not be longer than that ([deployment.md](deployment.md#the-server-and-the-fetch-job)). It posts `grant_type=refresh_token` and the refresh token to the credential's `token_url`: the ctx-memory app's with the app's client id and secret in HTTP Basic (from `CTX_SECRET_SLACK_APP`, read only then), the Claude Code app's as that app's public client. The new credential is saved as a new version of `CTX_SECRET_SLACK` **before** any call uses it, so a crash after the refresh cannot leave a stale refresh token stored while the new one is used. A save that fails is tried 3 more times, after 2, 5 and 15 seconds, each failure a line on standard error. If every try fails, the renewed token is not used: the Slack streams end with an error that gives the save's reason, says that Slack retires the stored refresh token after a short grace period, and says to run `ctx slack login` if the next fetch reports `invalid_refresh_token`.

A `token_revoked` or `invalid_auth` answer fails the Slack fetch with an error that says to run `ctx slack login` and then names Slack's cause, and the error never carries the token. Slack gives `token_revoked` when the token's user or workspace was deleted or the app was removed ([`conversations.list`](https://docs.slack.dev/reference/methods/conversations.list)), and it gave `token_revoked` for the token the Claude Code sign-in stored, which the diagnosis attributes to the 2-token limit (supported, not proven); it gives `invalid_auth` when the token is not valid or the request comes from an address the app does not allow ([`oauth.v2.access`](https://docs.slack.dev/reference/methods/oauth.v2.access)). An errored stream keeps its cursor, so the first fetch after the new sign-in reads the Slack streams again.

### What a fetch reads

A fetch reads the public channels the signed-in user belongs to, discovered from `conversations.list`, plus the private channels listed in `channels`. Direct messages and group direct messages are never listed, so they are never fetched; a configured id that is not a public or private channel the user can read raises a warning and is skipped. A live `ctx resolve` reads those channels and no others: before it reads any message it runs the same discovery, one `conversations.list` call per 1,000 channels, and it refuses a direct or group message, a private channel that `channels` does not list, and a public channel the user is not in. Unlike a fetch, it keeps archived channels, because what a fetch read there before is still in scope.

Each channel's cursor is an RFC 3339 timestamp. A thread reply is not in a channel's history unless it was also sent to the channel, so a fetch follows threads two ways. Each run reads every channel's history back 48 hours (or `thread_poll_days`, when that is shorter), not only since the cursor, and emits only the messages since the cursor: a message nobody had answered when it was first read shows up again carrying the `latest_reply` that its first reply gave it. A thread with replies is kept in a thread index under `<data_dir>/sources/slack` and polled each run until it has been idle for `thread_poll_days` (30 by default). A message is therefore looked at for a first reply for 48 hours after it is posted, and a thread is followed for `thread_poll_days` after its last reply. A first reply more than 48 hours after its message is not read, and neither is the rest of that thread, and nothing warns of it; a reply to a thread idle longer than `thread_poll_days` is not read either, and each run warns how many tracked threads it no longer polls. The read-back costs one `conversations.history` call per 200 messages posted in those 48 hours, per channel per run, and polling costs one `conversations.replies` call per followed thread per run. No stream ends until the index is saved, so a save that fails leaves every cursor where it was.

## Google sources

The `gmail`, `drive` and `calendar` kinds call Google's APIs directly with one OAuth credential, named `google`. In Google Cloud `CTX_SECRET_GOOGLE` names its Secret Manager secret, which the running identity reads; on a machine outside, `CTX_CREDENTIAL_GOOGLE_FILE` names a file holding it and `CTX_CREDENTIAL_GOOGLE` carries the value. The value is either Google's `authorized_user` JSON (`client_id`, `client_secret`, `refresh_token`), which `ctx` exchanges for an access token, or an access token itself, which is what a dev-box session receives for this credential. `CTX_GOOGLE_API_BASE` points the three sources and the token exchange at another host, which a test or a staging run uses; production leaves it unset.

A reply Google refuses with a rate limit is paused and sent again, up to eight times with the pause capped at a minute, because a window of mail or files costs more requests a minute than one user may spend.

```toml
[[sources]]
id = "gmail"
kind = "gmail"
query = "-category:promotions -category:social"

[[sources]]
id = "drive"
kind = "drive"
changes = true
meet = true
meet_folder = "Google Meet"
max_export_bytes = 5242880

[[sources]]
id = "calendar"
kind = "calendar"
calendars = ["alice@example.com"]
```

### Gmail

One stream, `me`, whose cursor is the mailbox history ID with the time the mailbox is stored through beside it, written `<historyId>@<rfc3339>`. A fetch with one asks `users.history.list` for the messages added since the ID and fetches at most 2,000 of them; a longer history is walked over several runs, the cursor naming the last change a run finished and keeping the time it carried. A history ID Gmail no longer holds (404 `notFound`) raises a warning and lists from the time the cursor carries; a cursor that carries no time stops the stream instead. A listing (a first fetch, or a relisting) reads the mailbox's history ID before it lists. When it starts more than seven days before the window end it lists seven days and names `<began>/<rfc3339>#<historyId>`: the time the listing began, the time it has listed through, and the ID it read first. Each later run first asks Gmail whether it still holds that ID; if not, it warns, reads a new ID and lists again from `<began>`, so mail imported during the catch-up with an old date is not lost. The run whose listing reaches the window end names `<historyId>@<rfc3339>`. A message listed twice is stored once; the window that lists it again shows it again among its new messages. A message the history or a listing names and Gmail answers 404 `notFound` for is skipped with a warning; any other 404 ends the stream with an error. A bare RFC 3339 time, which an earlier build wrote, is listed from in the same way.

`query` is a Gmail search query and defaults to `-category:promotions -category:social`. Its `-category:` terms exclude the same labels on the history path, where no query applies, and `SPAM` and `TRASH` are never fetched. A message that arrives after the window ends is left for the next fetch, which the held cursor brings back; a listing that sees one names `<began>/<window end>#<historyId>`, so the next run lists from the window's end and names the ID once no mail follows the window. The listing that reaches the window end asks Gmail with no upper date, so it also sees mail dated in the future. A message dated more than 10 minutes after both the window end and the moment the run read the history or listing that names it cannot have arrived yet, so its date is taken for a wrong clock: the window stores it at its end, its raw data keeps the date Gmail gave, a warning names it, and the cursor moves past it instead of waiting for the clock. A message dated between the window end and that bound waits for the next window. A record's text is the message headers with the first `text/plain` part, decoded with the character set that part declares, or the `text/html` part with its tags removed.

### Drive

Two streams. `drive` follows the Drive changes feed, so its cursor is a changes page token with the time Drive is stored through beside it, written `<token>@<rfc3339>`; a run reads pages until it has read 500 files and names the page after the last one it read, keeping the time; a run that reads the feed to its end names the window's end. A file the feed names after the window ended holds the cursor at the page that names it. A file dated more than 10 minutes after both the window end and the moment the run read the page or listing that names it cannot have changed yet, so its date is taken for a wrong clock: the window stores it at its end, its raw data keeps the date Drive gave, a warning names it, and the cursor moves past it instead of waiting for the clock. The same holds in every listing. A token Drive refuses (400, reason `invalid` at `pageToken`) raises a warning; the stream takes a new start page token and lists from the time the cursor carries, and a cursor that carries no time stops the stream instead. A first fetch takes a start page token, then lists every file modified since the backfill window: when the window end is more than seven days away it lists seven days and names `<rfc3339>#<token>`, the time it listed through and the token it took first; the run whose listing reaches the window end names `<token>@<window end>` and lists with no upper bound. The token was taken before that listing, so its feed never names a file changed between the window end and the listing; a listing that sees such a file leaves it out of the window and names `<window end>#<token>`, so the next run lists from the window's end, stores the file in its own window, and names the token once no file follows the window. The relisting after a refused token works the same way. The token is taken before the first slice because a file uploaded during a catch-up keeps its old modified time and so falls behind the slices: a token taken only at the last slice would open a feed that never names it, and the token taken first does. A bare token or a bare RFC 3339 time, which an earlier build wrote, still parses: the token gains a time on its first full read, and the time is listed from. `meet` reads the folder named by `meet_folder` ("Google Meet"), where Meet keeps one folder per meeting holding that meeting's notes and transcript; its cursor is an RFC 3339 timestamp, at most seven days past the last, held where it is when no folder answers that name. A file the `meet` stream read is not repeated by `drive`. `changes = false` or `meet = false` turns a stream off.

A file dated in the future that sits directly in the Meet folder, outside any meeting's folder, is stored only once its date has passed: the `meet` stream lists by a time window, and its cursor never waits for that file.

A Doc, Sheet or Slides file is stored as its exported text, stopping at `max_export_bytes` (5 MiB by default) with a warning that names the file. Any other file, such as a PDF or a Meet recording, is stored as its name and type with a warning, so one large file cannot stall a fetch. So is a document Drive will not export for a reason that is the file itself — 403 `cannotExportFile` (download restricted), `exportSizeLimitExceeded` (too large to export), `fileNotExportable` (a kind Drive does not export), `insufficientFilePermissions`, or 404 `notFound` (gone since the feed named it) — with a warning that names the file and Drive's answer. Any other refusal — `dailyLimitExceeded`, `insufficientPermissions` (a token without the Drive scope), 400 `badRequest`, a 401, or a 403 or 404 with no Drive reason, such as a proxy's page — ends the stream with an error, so its cursor stays where it was and a later run exports the file. The reasons are Drive's own ([handle errors](https://developers.google.com/workspace/drive/api/guides/handle-errors)); `exportSizeLimitExceeded` is not on that page and is from Google's [issue tracker](https://issuetracker.google.com/issues/36761333). Drive answered `fileNotExportable` and `notFound` live on 2026-10-28. No `cannotExportFile` answer has been seen, because the Drive checked holds no document with download turned off. A folder and a shortcut hold no text of their own and are not recorded. The source declares itself verbatim, so `ctx cite verify` checks a quote against the exported text.

### Calendar

Each configured calendar is one stream whose cursor is a Calendar sync token with the time the calendar is stored through beside it, written `<syncToken>@<rfc3339>`. A fetch without a cursor lists every event from the backfill window onwards and keeps the sync token that listing ends with; a listing that ends without one names the bare window-end time, with a warning, and the next fetch lists from it. A token Google refuses (410 Gone) raises a warning and lists again from the time the cursor carries, and a cursor that carries no time, as an earlier build wrote, stops the stream instead. A cancelled event is not recorded.

A record's timestamp is the event's start, and its text carries the start and end, the location, the attendees with their responses, and the description. An event that starts after the window ends is a context record: it is searchable and citable, and it does not appear in that window's delta.

## GitHub source

The `github` kind reads every repository the deployment's GitHub App is installed on, through the installations its configuration's `installations` list names, or `CTX_GITHUB_INSTALLATIONS` (comma-separated ids) when the list is empty; no other installation is called. It mints a read-only installation token per installation from the `github_app` credential, the JSON `{app_id, private_key}`, which it reads from `CTX_SECRET_GITHUB_APP`, `CTX_CREDENTIAL_GITHUB_APP_FILE` or `CTX_CREDENTIAL_GITHUB_APP`. `CTX_GITHUB_API` points it at another host, which a test uses.

```toml
[[sources]]
id = "github"
kind = "github"
request_budget = 600
rate_limit_floor = 2000
```

One repository is one stream, `github:<owner>/<repo>`. A run lists the repository's issues feed (issues and pull requests, oldest change first) from the cursor, then each listed issue's comments, and each pull request's reviews and review comments, one issue after another, then the default branch's commits. Pointers are `github:<owner>/<repo>#<number>`, with `/comment/<id>`, `/review/<id>` or `/review-comment/<id>` for an issue's follow-ups, and `github:<owner>/<repo>@<sha>` for a commit. An item changed after the window ended is left for the next fetch. GitHub pages the issues feed by key: its next link names the last item read, so an issue changed during the run moves to the end and hides no other. A run reads the default branch's head first and names that commit in every later commits request, so a push during the run moves nothing under it. When the cursor names the head the last run stored, a different head is read as a comparison of the two (`compare/<stored head>...<head>`): every commit the branch has now that the stored history lacks, whatever its date. So a force push is read, and so are commits that arrive with an older date, such as a merged pull request's own commits. With no stored head, or when GitHub cannot compare the two, the run lists the head's history since the stored time. An issue's comments and a pull request's review comments are listed whole, in id order, and paged by page number, so the source reads page 1, then the last page down to page 2, then page 1 again, and keeps the comments changed since the time the stream is stored through. The listings name no `since`: GitHub's `since` would bring an older comment into the listing when someone edits it, which moves every later comment one place on, onto a page the run may have read already. Listed whole, an edit during the run moves no comment, and a deletion hides no other. A thread that changed costs one request per 100 of all its comments.

GitHub may list fewer commits in a comparison than the answer's `total_commits` says it holds. The run then also lists the head's history since the stored time. A commit dated after the stored time cannot be in the stored history, so when the comparison's commits and those dated after the stored time add up to `total_commits`, every commit was read and the run stores the head. Otherwise some commit the comparison left out is older than the stored time: the run stores what it read but keeps the stored head, with a warning, and the next run compares from that head again.

A run spends at most `request_budget` paid REST requests (600 by default), and stops reading an installation's repositories once GitHub reports that installation's remaining `core` rate limit under `rate_limit_floor` (2,000). The pre-pass's GraphQL queries are not charged to the budget. The issues feed stops early when the budget left would not cover the follow-ups of the issues it may finish. It never stops while those issues all changed in one second, because that second may go on in the next page. A run finishes issues in order of their `updated_at`, then their number. A stream the run finished names `feed=<window end>,head=<sha>@<window end>` as its cursor, or the window end alone when its default branch has no commit. A stream the run stopped names `feed=<time>,issue=<number>[,head=<sha>]@<stored>`. The next run's issues feed starts at that time and skips that issue and the issues before it in the same second. So a second shared by more issues than one run's budget covers is still finished across runs. A run that finished every issue but not the commits names `feed=<window end>,head=<sha>@<stored>`, or `<window end>@<stored>` when it names no head. `stored` is the time the stream was stored through when the run started; the next run keeps every comment changed since it. A stopped stream that finished no issue keeps its cursor. Cursors an earlier build wrote (a bare time, or `<resume>@<stored>`) still work. The next run starts at the first repository the last run's request budget stopped and goes round, so every repository is read in turn. A repository that an installation's rate floor or GitHub's throttle held does not move that start: its installation's next answer decides when it is read, and a start pinned to it would read every other installation from its first repository on each run, so the budget could hold the same late repository every time.

GitHub keeps a rate limit for each installation, and within it one for each resource its answers name in `X-RateLimit-Resource`: `core` for the REST listings, `graphql` for the pre-pass. So a low count on one installation holds only its repositories, and the floor applies to `core` alone. GitHub does not always answer from one counter for an installation: in one observed run, within one minute and with one token, one REST answer said 14,407 left with a reset at 03:15:34Z and the next said 13,790 left with a reset at 03:47:48Z. A count near the end of its window can therefore read about 2,000 while `/rate_limit` reads 14,000 or more. The source cannot tell which counter answers its next request, so a count under the floor holds that installation for the rest of the run.

Before any REST request, a GraphQL pre-pass asks GitHub, 50 repositories a query, for each one's newest issue and pull request `updatedAt`, its `pushedAt`, and its default branch head's id and commit time. GitHub moves an issue's or pull request's `updatedAt` for each new comment, review, review comment and pushed commit, so a repository whose answers are all older than its cursor, and whose head is the one its cursor names, has nothing a REST read would list: its stream ends at the window end and costs no REST request. Only a settled stream is asked about: one whose cursor is the one a run proposed after it read the stream to the end and held nothing back. Every other stream is read through REST: one that held an item past the window end, stopped, failed, has no cursor or names a stopped run's cursor. So is every repository of a batch that GitHub answers with an error or without every repository, with a warning. The source keeps the settled streams, and the repository the next run starts at, in `<data_dir>/sources/<source id>/state.json`. A file this build cannot read costs one REST read of every repository. The source sends no conditional requests, because GitHub ties an `ETag` to the token that read the page and each fetch mints its own token. It deletes the `etags.json` that an earlier build kept.

## Conformance

Run a configured source through the same protocol validator used by fetch:

```bash
ctx source run linear
ctx source run linear --op resolve
```

Without `--json`, the command prints the raw protocol stream for inspection. With `--json`, it emits one v1 envelope containing the validated messages and source outcome. Use it to develop a source before including it in a full fetch, and keep source output free of credentials.
