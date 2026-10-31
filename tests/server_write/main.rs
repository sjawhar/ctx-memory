//! The server is the only writer of records: it checks every write, commits it under the
//! caller's name and pushes it.

#[path = "../support/mod.rs"]
mod support;

mod batch;
mod clone_and_follow;
mod fixtures;
mod lease_and_shutdown;
mod logging;
mod requests;
mod validation;
