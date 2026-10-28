//! Cloud Storage leases: one writer per role, across machines.
//!
//! Two roles write the shared directory and neither may ever run twice at once: the fetch job
//! writes raw messages and cursors, the server writes the clone and the marker. The share has
//! no file locks, so the lease is the only thing that keeps each role single. These tests drive
//! the real lease code against a fake Cloud Storage that implements generations and
//! preconditions, so what they prove is the request Cloud Storage would have received.

#[path = "../support/mod.rs"]
mod support;

mod fetch_integration;
mod fixtures;
mod object_protocol;
mod server_integration;
