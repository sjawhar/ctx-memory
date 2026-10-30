//! The GitHub source acts as the deployment's GitHub App: it mints a read-only installation
//! token for every installation it is told to read, and touches no other installation. These
//! checks drive the real client against a fake GitHub so the request GitHub would receive is
//! asserted, not assumed.

#[path = "../support/mod.rs"]
mod support;

mod client_contract;
mod cursors_and_commits;
mod fixtures;
mod pagination;
