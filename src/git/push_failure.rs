//! Which failed pushes failed on the remote's side: the one list of what git prints for them.
//!
//! git words a push it could not make in a few fixed forms, and these are read from what it
//! printed on standard error, run with `LC_ALL=C` so its own words and the C library's are in
//! English. A push that failed on the remote's side — the hosting service's backend failed, it
//! answered with a server error, or the connection broke — may land when it is made again. A
//! refusal does not: git refused a push that is not a fast-forward, the remote's hook declined
//! it, or the remote refused the credential, and the same push is refused again.

/// What git prints before the remote's reason, in the line for a ref the remote refused
/// (`print_one_push_report` in git's `transport.c`, `REF_STATUS_REMOTE_REJECT`):
/// ` ! [remote rejected] HEAD -> main (<reason>)`.
const REMOTE_REJECTED: &str = "[remote rejected]";

/// The `[remote rejected]` reasons a remote gives for a failure of its own. Both are GitHub's:
/// `failure` came with `remote: fatal error in commit_refs` during an outage on 2026-11-04,
/// and `Internal Server Error` answered a push to this repository the same day. Any other
/// reason keeps the answer a failed push always had. Most are the remote refusing the push —
/// `pre-receive hook declined`, `protected branch hook declined`, `hook declined` (git's
/// `builtin/receive-pack.c`) — and are read as refusals. A stock receive-pack's own failures,
/// `failed to update ref` and `unable to migrate objects to permanent storage`, are failures
/// too, but no remote this server pushes to has been seen giving them, so they are left out.
const REMOTE_FAILURE_REASONS: &[&str] = &["failure", "Internal Server Error"];

/// The line for a ref the remote took no stand on: it ended the push without reporting what it
/// did (`transport.c`, `REF_STATUS_EXPECTING_REPORT`):
/// ` ! [remote failure] HEAD -> main (remote failed to report status)`.
const REMOTE_FAILURE: &str = "[remote failure]";

/// What git prints before the HTTP status of an answer it took for a failure:
/// `The requested URL returned error: 503` (`normalize_curl_result` in git's `http.c`, which
/// `fatal: unable to access '<url>': …` quotes) and `error: RPC failed; HTTP 502 curl 22 …`
/// (`run_slot` in git's `remote-curl.c`). A `5xx` is the remote's failure; any other status —
/// a `401` or `403` for a credential, a `404`, a `413` for a push too large — is a refusal.
const HTTP_STATUS: &[&str] = &["The requested URL returned error: ", "RPC failed; HTTP "];

/// A connection that broke, was refused or timed out, as the C library's `strerror` words it
/// for a socket and libcurl quotes it (`Recv failure: Connection reset by peer`, `Failed to
/// connect to <host> port 443 after 2 ms: Connection refused`), as libcurl words a connection
/// it could not make with no reason from the system (`Failed to connect to 127.0.0.1 port 1
/// after 0 ms: Couldn't connect to server`, from git 2.43 with its curl; newer curl says
/// `Could not connect to server`, `curl_easy_strerror` in curl's `lib/strerror.c`), or as
/// libcurl words its own limits (`Connection timed out after 300000 milliseconds`,
/// `Operation timed out after …`).
const CONNECTION_FAILURES: &[&str] = &[
    "Connection reset by peer",
    "Connection refused",
    "Couldn't connect to server",
    "Could not connect to server",
    "Connection timed out",
    "Operation timed out",
];

/// Refusals no retry changes, whichever of the failures above git printed beside them: git's
/// own refusal of a push that is not a fast-forward (`[rejected]`, with `non-fast-forward`,
/// `fetch first`, `needs force`, `stale info` and the rest, `transport.c`), a credential the
/// remote refused over HTTP (`fatal: Authentication failed for '<url>'`, `remote-curl.c`), and
/// a key or file the remote refused (`Permission denied`).
const REFUSALS: &[&str] = &[
    "[rejected]",
    "Authentication failed for ",
    "Permission denied",
];

/// Whether a push that git ended with `code` and that printed `said` failed on the remote's
/// side, so that making it again may land it.
///
/// Only a git that exited on its own with a failure status can be read this way: a git stopped
/// by a signal, or at the time limit, printed nothing that says what the remote did. One
/// refusal anywhere in what git printed makes the push a refusal, whatever failure it printed
/// beside it.
pub(super) fn on_the_remotes_side(code: Option<i32>, said: &str) -> bool {
    if matches!(code, None | Some(0)) {
        return false;
    }
    let mut remote = false;
    for line in said.lines() {
        if REFUSALS.iter().any(|refusal| line.contains(refusal)) {
            return false;
        }
        if let Some(reason) = remote_rejected_reason(line) {
            if !REMOTE_FAILURE_REASONS.contains(&reason) {
                return false;
            }
            remote = true;
        }
        match http_status(line) {
            Some(status) if (500..600).contains(&status) => remote = true,
            Some(_) => return false,
            None => {}
        }
        if line.contains(REMOTE_FAILURE)
            || CONNECTION_FAILURES
                .iter()
                .any(|failure| line.contains(failure))
        {
            remote = true;
        }
    }
    remote
}

/// The reason in a `[remote rejected]` line, the text in its last parentheses, or `None` for
/// any other line.
fn remote_rejected_reason(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(REMOTE_REJECTED)?;
    let (_, reason) = rest.trim_end().strip_suffix(')')?.rsplit_once(" (")?;
    Some(reason)
}

/// The HTTP status a line names after one of [`HTTP_STATUS`], or `None` when it names none.
fn http_status(line: &str) -> Option<u16> {
    HTTP_STATUS.iter().find_map(|prefix| {
        let (_, rest) = line.split_once(prefix)?;
        let digits = rest.get(..3)?;
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::on_the_remotes_side;

    /// What git printed for a push during the outage of 2026-11-04, exiting with status 1.
    const OUTAGE: &str = "remote: fatal error in commit_refs        \n\
                          To https://github.com/owner/graph.git\n \
                          ! [remote rejected] HEAD -> main (failure)\n\
                          error: failed to push some refs to 'https://github.com/owner/graph.git'";

    /// What git printed for pushes that failed on the remote's side, each with the status git
    /// exited with.
    const REMOTE_SIDE: &[(i32, &str)] = &[
        (1, OUTAGE),
        (
            1,
            "To https://github.com/owner/graph.git\n \
             ! [remote rejected] HEAD -> main (Internal Server Error)\n\
             error: failed to push some refs to 'https://github.com/owner/graph.git'",
        ),
        (
            1,
            " ! [remote failure]  HEAD -> main (remote failed to report status)\n\
             error: failed to push some refs to 'https://github.com/owner/graph.git'",
        ),
        (
            128,
            "fatal: unable to access 'https://github.com/owner/graph.git/': The requested URL \
             returned error: 503",
        ),
        (
            1,
            "error: RPC failed; HTTP 502 curl 22 The requested URL returned error: 502\n\
             send-pack: unexpected disconnect while reading sideband packet\n\
             fatal: the remote end hung up unexpectedly",
        ),
        (
            128,
            "fatal: unable to access 'https://github.com/owner/graph.git/': Recv failure: \
             Connection reset by peer",
        ),
        (
            128,
            "fatal: unable to access 'https://github.com/owner/graph.git/': Failed to connect \
             to github.com port 443 after 2 ms: Connection refused",
        ),
        // git 2.43 pushing to a port nothing listens on.
        (
            128,
            "fatal: unable to access 'http://127.0.0.1:1/remote.git/': Failed to connect to \
             127.0.0.1 port 1 after 0 ms: Couldn't connect to server",
        ),
        (
            128,
            "fatal: unable to access 'http://127.0.0.1:1/remote.git/': Failed to connect to \
             127.0.0.1 port 1 after 0 ms: Could not connect to server",
        ),
        (
            128,
            "fatal: unable to access 'https://github.com/owner/graph.git/': Connection timed out \
             after 300000 milliseconds",
        ),
        (
            128,
            "fatal: unable to access 'https://github.com/owner/graph.git/': Operation timed out \
             after 300000 milliseconds with 0 out of 0 bytes received",
        ),
    ];

    /// What git printed for pushes the same push would meet again.
    const REFUSED: &[(i32, &str)] = &[
        (
            1,
            "To https://github.com/owner/graph.git\n \
             ! [rejected]        HEAD -> main (fetch first)\n\
             error: failed to push some refs to 'https://github.com/owner/graph.git'",
        ),
        (
            1,
            " ! [rejected]        HEAD -> main (non-fast-forward)\n\
             error: failed to push some refs to 'https://github.com/owner/graph.git'",
        ),
        (
            1,
            "remote: the remote refused this push\n \
             ! [remote rejected] HEAD -> main (pre-receive hook declined)\n\
             error: failed to push some refs to '/tmp/remote.git'",
        ),
        (
            1,
            "remote: error: GH006: Protected branch update failed for refs/heads/main.\n \
             ! [remote rejected] HEAD -> main (protected branch hook declined)",
        ),
        (
            128,
            "remote: Permission to owner/graph.git denied to app.\n\
             fatal: unable to access 'https://github.com/owner/graph.git/': The requested URL \
             returned error: 403",
        ),
        (
            128,
            "fatal: Authentication failed for 'https://github.com/owner/graph.git/'",
        ),
        (
            1,
            "error: RPC failed; HTTP 413 curl 22 The requested URL returned error: 413\n\
             fatal: the remote end hung up unexpectedly",
        ),
        (
            128,
            "git@github.com: Permission denied (publickey).\n\
             fatal: Could not read from remote repository.",
        ),
        // A failure of the remote's beside a refusal is a refusal.
        (
            1,
            " ! [remote rejected] HEAD -> main (failure)\n\
             fatal: Authentication failed for 'https://github.com/owner/graph.git/'",
        ),
        // Nothing git names.
        (1, "error: something else went wrong"),
    ];

    #[test]
    fn a_push_that_failed_on_the_remotes_side_is_told_from_one_refused() {
        for (code, said) in REMOTE_SIDE {
            assert!(on_the_remotes_side(Some(*code), said), "{said}");
        }
        for (code, said) in REFUSED {
            assert!(!on_the_remotes_side(Some(*code), said), "{said}");
        }
    }

    /// A git stopped by a signal, or one that exited 0, said nothing about a failure.
    #[test]
    fn only_a_git_that_exited_with_a_failure_is_read() {
        assert!(!on_the_remotes_side(None, OUTAGE));
        assert!(!on_the_remotes_side(Some(0), OUTAGE));
    }
}
