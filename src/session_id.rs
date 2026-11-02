//! The session a `ctx` command runs for, as its caller says.
//!
//! Many sessions reach the server under one identity: every dev box on a tailnet signs in as
//! the same person, and every pod of a deployment as the same service account. So the client
//! names its session to the server in [`HEADER`] with every request, and the server's request
//! log records it. It is the caller's word, never proof: nothing on the server decides who may
//! do what by it. A write also names its session in its body, and its commit's `Session:` line
//! carries it.

use std::ffi::OsString;

/// The variables that name the caller's session, in the order they are read: `CTX_SESSION`,
/// which a person or a runner sets for ctx; `OMP_SESSION_ID`, which an Oh My Pi setup may
/// export for its session; `PI_SESSION_ID`, which Oh My Pi's bash tool sets for every command
/// its agent runs from Oh My Pi 18.12.4 on; and `CLAUDE_CODE_SESSION_ID`, which Claude Code
/// sets. An agent on an older Oh My Pi names its session with `CTX_SESSION`.
pub const VARIABLES: [&str; 4] = [
    "CTX_SESSION",
    "OMP_SESSION_ID",
    "PI_SESSION_ID",
    "CLAUDE_CODE_SESSION_ID",
];

/// The request header that carries the caller's session id.
pub const HEADER: &str = "X-Ctx-Session";

/// The most characters a session id the header carries may hold.
pub const MAX_CHARS: usize = 128;

/// The caller's session id, with the variable it came from: the first of [`VARIABLES`] that is
/// set and not empty. A value of white space is a value, which [`fits`] then refuses: it does
/// not hand the choice to a later variable.
#[must_use]
pub fn from_env() -> Option<(&'static str, String)> {
    first_set(std::env::var_os)
}

fn first_set(read: impl Fn(&'static str) -> Option<OsString>) -> Option<(&'static str, String)> {
    VARIABLES.into_iter().find_map(|name| {
        let value = read(name)?.to_string_lossy().into_owned();
        (!value.is_empty()).then_some((name, value))
    })
}

/// Whether [`HEADER`] may carry `id`: 1 to [`MAX_CHARS`] ASCII letters, digits, `.`, `_`, `:`
/// and `-`. That holds the ids Oh My Pi and Claude Code give (UUIDs) and a job execution's
/// name, and nothing that could start a line of its own or act on whoever reads the log.
#[must_use]
pub fn fits(id: &str) -> bool {
    (1..=MAX_CHARS).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// The name a write's `Session:` line and a marker move carry: the caller's session id, or,
/// when no variable names one, the user it runs as.
#[must_use]
pub fn caller() -> String {
    from_env().map_or_else(user, |(_, id)| id)
}

/// The user this process runs as, from `USER`, or `unknown`.
#[must_use]
pub fn user() -> String {
    std::env::var_os("USER")
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    use super::{first_set, fits, MAX_CHARS};

    /// One variable a stand-in environment sets, and its value.
    type Binding = (&'static str, &'static str);

    /// A stand-in for the environment that holds exactly `set`.
    fn read(set: &[Binding]) -> impl Fn(&'static str) -> Option<OsString> {
        let set: BTreeMap<&'static str, OsString> = set
            .iter()
            .map(|(name, value)| (*name, OsString::from(*value)))
            .collect();
        move |name| set.get(name).cloned()
    }

    /// `CTX_SESSION` names the session when it is set; else an exported Oh My Pi id, else the
    /// id Oh My Pi's bash tool sets, else Claude Code's. An empty variable counts as unset.
    /// White space is a value, which the header refuses, and it keeps a later variable from
    /// being chosen in its place.
    #[test]
    fn the_first_variable_set_names_the_session() {
        let omp = ("OMP_SESSION_ID", "01a1b2c3-0000-7000-8000-000000000001");
        let pi = ("PI_SESSION_ID", "01a1b2c3-0000-7000-8000-000000000002");
        let claude = (
            "CLAUDE_CODE_SESSION_ID",
            "5f0c7d1e-2222-4333-8444-555566667777",
        );
        let cases: [(&[Binding], Option<Binding>); 9] = [
            (
                &[("CTX_SESSION", "cycle-1"), omp, claude],
                Some(("CTX_SESSION", "cycle-1")),
            ),
            (&[omp, pi, claude], Some(omp)),
            (&[pi, claude], Some(pi)),
            (&[("OMP_SESSION_ID", ""), pi], Some(pi)),
            (&[claude], Some(claude)),
            (&[("CTX_SESSION", ""), ("OMP_SESSION_ID", "")], None),
            (&[("CTX_SESSION", ""), claude], Some(claude)),
            (
                &[("CTX_SESSION", " \t"), claude],
                Some(("CTX_SESSION", " \t")),
            ),
            (&[], None),
        ];
        for (set, expected) in cases {
            let found = first_set(read(set));
            assert_eq!(
                found.as_ref().map(|(name, value)| (*name, value.as_str())),
                expected,
                "{set:?}"
            );
        }
    }

    /// The header carries a UUID or a job execution's name, up to 128 characters, and nothing
    /// that is not ASCII letters, digits, `.`, `_`, `:` and `-`.
    #[test]
    fn a_session_id_is_short_plain_ascii() {
        let longest = "a".repeat(MAX_CHARS);
        let too_long = "a".repeat(MAX_CHARS + 1);
        for id in [
            "01a1b2c3-0000-7000-8000-000000000001",
            "ctx-agent-x7k2p-retry",
            "host:session_1.2",
            longest.as_str(),
        ] {
            assert!(fits(id), "{id}");
        }
        for id in [
            "",
            too_long.as_str(),
            "two words",
            "line\nbreak",
            "tab\t",
            "slash/id",
            "quote\"id",
            "ünïcode",
            "semi;colon",
        ] {
            assert!(!fits(id), "{id:?}");
        }
    }
}
