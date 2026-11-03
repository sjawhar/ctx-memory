//! The Matrix source reads the rooms of one account through the client-server API. These checks
//! drive the real source and client against an emulated homeserver, so the requests a
//! homeserver would receive, and the records and cursors a fetch would store, are asserted
//! rather than assumed.

#[path = "../support/mod.rs"]
mod support;

mod cli;
mod fetch;
mod fixtures;
mod resolve;
mod server_behaviour;
mod untrusted;
