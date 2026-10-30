//! The deployment's exclusions: the source messages it never ingests, named by pointer pattern.
//!
//! A busy stream can drown every other one: a repository whose agents commit thousands of times
//! an hour fills each window with commit messages no record will ever cite, and spends the fetch
//! budget other streams need. An operator names such messages by pointer pattern, in `ctx.toml`
//! (`[ingest] exclude`) or in `CTX_EXCLUDE` (comma-separated), and both lists apply.
//!
//! A pattern is a pointer in which `*` stands for any run of characters, `/` and none included:
//! `github:example-org/busy@*` is every commit of one repository, `slack:C0123/*` every message
//! of one channel. The fetch hands the patterns to each source, so a source can skip reading what
//! would be dropped, and drops every record a pattern names before it writes the window. A delta
//! read from a window also leaves those messages out, so a window fetched before the exclusion
//! existed shrinks without its files being rewritten.

use anyhow::{bail, Result};

use crate::config::TreeConfig;

/// The environment variable that adds patterns to the tree's own, separated by commas.
pub const EXCLUDE_ENV: &str = "CTX_EXCLUDE";

/// The pointer patterns a deployment excludes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exclusion {
    patterns: Vec<String>,
}

impl Exclusion {
    /// The tree's patterns and those `CTX_EXCLUDE` names.
    ///
    /// # Errors
    ///
    /// Returns an error when a pattern is empty.
    pub fn configured(config: &TreeConfig) -> Result<Self> {
        Self::from_parts(
            &config.ingest.exclude,
            std::env::var(EXCLUDE_ENV).ok().as_deref(),
        )
    }

    /// The patterns of a configuration list and of an environment value, in that order.
    ///
    /// # Errors
    ///
    /// Returns an error when a pattern is empty.
    pub fn from_parts(configured: &[String], environment: Option<&str>) -> Result<Self> {
        let listed = environment
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty());
        let mut patterns = Vec::new();
        for pattern in configured.iter().map(String::as_str).chain(listed) {
            if pattern.trim().is_empty() {
                bail!("an exclusion pattern must not be empty");
            }
            if !patterns.iter().any(|kept| kept == pattern) {
                patterns.push(pattern.to_owned());
            }
        }
        Ok(Self { patterns })
    }

    /// An exclusion of exactly these patterns.
    ///
    /// # Errors
    ///
    /// Returns an error when a pattern is empty.
    pub fn new(patterns: &[String]) -> Result<Self> {
        Self::from_parts(patterns, None)
    }

    /// The patterns, as the configuration and the environment gave them.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether a pattern names `pointer`.
    #[must_use]
    pub fn matches(&self, pointer: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| glob(pattern.as_bytes(), pointer.as_bytes()))
    }

    /// Whether every pointer that starts with `prefix` is excluded, so a source need not read
    /// any of them.
    ///
    /// A pattern names every such pointer exactly when it ends in `*` and what comes before that
    /// star matches a prefix of `prefix` (the empty one included): the star then takes the rest
    /// of `prefix` and anything after it. A pattern that ends in anything else misses a pointer
    /// that ends differently.
    #[must_use]
    pub fn covers(&self, prefix: &str) -> bool {
        self.patterns.iter().any(|pattern| {
            pattern.strip_suffix('*').is_some_and(|head| {
                (0..=prefix.len())
                    .filter_map(|end| prefix.get(..end))
                    .any(|start| glob(head.as_bytes(), start.as_bytes()))
            })
        })
    }
}

/// Whether `pattern`, in which `*` matches any run of bytes, matches all of `text`.
///
/// The usual two-pointer match: on a mismatch after a star, the star takes one byte more and
/// the match resumes from there. It never backtracks further than the last star, which is
/// enough because a star matches any run.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                resume = Some((p, t));
                p += 1;
            }
            Some(byte) if Some(byte) == text.get(t) => {
                p += 1;
                t += 1;
            }
            _ => match resume {
                Some((star, taken)) => {
                    p = star + 1;
                    t = taken + 1;
                    resume = Some((star, taken + 1));
                }
                None => return false,
            },
        }
    }
    pattern
        .get(p..)
        .is_some_and(|rest| rest.iter().all(|byte| *byte == b'*'))
}

#[cfg(test)]
mod tests {
    use super::Exclusion;

    fn exclusion(patterns: &[&str]) -> Exclusion {
        let patterns = patterns.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        Exclusion::new(&patterns).unwrap_or_default()
    }

    #[test]
    fn a_star_matches_any_run_and_a_literal_only_itself() {
        let commits = exclusion(&["github:example-org/busy@*"]);
        assert!(commits.matches("github:example-org/busy@0123abc"));
        assert!(!commits.matches("github:example-org/busy#12"));
        assert!(!commits.matches("github:example-org/busy-two@0123abc"));

        let channel = exclusion(&["slack:C0123/*"]);
        assert!(channel.matches("slack:C0123/1793332065.372469"));
        assert!(!channel.matches("slack:C01234/1793332065.372469"));

        let middle = exclusion(&["github:*/busy@*"]);
        assert!(middle.matches("github:example-org/busy@0123abc"));
        assert!(middle.matches("github:other/busy@ff"));
        assert!(!middle.matches("github:other/busy#1"));

        let exact = exclusion(&["gmail:abc"]);
        assert!(exact.matches("gmail:abc"));
        assert!(!exact.matches("gmail:abcd"));
        assert!(!Exclusion::default().matches("gmail:abc"));
    }

    #[test]
    fn a_pattern_covers_a_prefix_only_when_its_star_takes_every_ending() {
        let commits = exclusion(&["github:example-org/busy@*"]);
        assert!(commits.covers("github:example-org/busy@"));
        assert!(!commits.covers("github:example-org/busy#"));
        assert!(!commits.covers("github:example-org/bus"));

        let organisation = exclusion(&["github:example-org/*"]);
        assert!(organisation.covers("github:example-org/busy@"));
        assert!(organisation.covers("github:example-org/busy#"));
        assert!(!organisation.covers("github:other/busy@"));

        let any_repo = exclusion(&["github:*/busy@*"]);
        assert!(any_repo.covers("github:example-org/busy@"));
        assert!(!any_repo.covers("github:example-org/quiet@"));

        // A pattern that ends in a literal misses some ending, so it covers nothing.
        assert!(!exclusion(&["github:example-org/busy@*0"]).covers("github:example-org/busy@"));
    }

    #[test]
    fn the_environment_adds_patterns_to_the_trees_own() {
        let configured = vec!["github:example-org/busy@*".to_owned()];
        let both = Exclusion::from_parts(
            &configured,
            Some(" slack:C0123/* ,, github:example-org/busy@*"),
        )
        .unwrap_or_default();
        assert_eq!(
            both.patterns(),
            ["github:example-org/busy@*", "slack:C0123/*"]
        );
        assert!(Exclusion::from_parts(&[" ".to_owned()], None).is_err());
        assert!(Exclusion::from_parts(&[], Some(""))
            .unwrap_or_default()
            .is_empty());
    }
}
