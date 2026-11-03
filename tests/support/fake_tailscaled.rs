//! A fake tailscaled local API on a unix socket, for the server's tagged-caller tests.
//!
//! It answers `GET /localapi/v0/whois?addr=<ip>` as tailscaled does: the node and its tags for a
//! known address, 404 for any other.

#![allow(dead_code)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

/// A fake local API listening at `socket` until the test process exits.
pub struct FakeTailscaled {
    pub socket: PathBuf,
}

impl FakeTailscaled {
    /// Serves `nodes` (address, tags) from a socket inside `dir`.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be bound.
    pub fn start(dir: &std::path::Path, nodes: &[(&str, &[&str])]) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let socket = dir.join("tailscaled.sock");
        let listener = UnixListener::bind(&socket)?;
        let nodes: Vec<(String, Vec<String>)> = nodes
            .iter()
            .map(|(address, tags)| {
                (
                    (*address).to_owned(),
                    tags.iter().map(|tag| (*tag).to_owned()).collect(),
                )
            })
            .collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Ok(reader) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(reader);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let target = request_line.split(' ').nth(1).unwrap_or("");
                let address = target
                    .split_once("addr=")
                    .map(|(_, address)| address.replace("%3A", ":"))
                    .unwrap_or_default();
                let (status, body) = match nodes.iter().find(|(known, _)| *known == address) {
                    Some((_, tags)) => (
                        "200 OK",
                        json!({ "Node": { "Tags": tags }, "UserProfile": {} }).to_string(),
                    ),
                    None => ("404 Not Found", "no match for IP:port\n".to_owned()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Ok(Self { socket })
    }
}
