//! Synchronous IPC client used by the CLI control subcommands.
//!
//! The CLI stays tokio-free: it opens the Unix socket with `std`, writes one
//! JSON request line, and reads one JSON response line.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{anyhow, Context};
use conflux_core::ipc::{Request, Response};

/// Send a request to the daemon and return its response.
pub fn send(socket: &Path, request: &Request) -> anyhow::Result<Response> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "could not connect to the daemon at {} (is it running?)",
            socket.display()
        )
    })?;

    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.trim().is_empty() {
        return Err(anyhow!("daemon closed the connection without responding"));
    }
    Ok(serde_json::from_str(response.trim())?)
}

/// Render a [`Response`] for the terminal. Returns whether the command succeeded.
pub fn print_response(response: &Response) -> bool {
    match response {
        Response::Status(groups) => {
            if groups.is_empty() {
                println!("no active sync groups");
            }
            for g in groups {
                let when = match g.last_run {
                    Some(secs) => format!("{}s since epoch", secs),
                    None => "never".to_string(),
                };
                println!("{} [{}] root={}", g.label, g.trigger, g.root);
                match (&g.last_summary, &g.last_error) {
                    (_, Some(err)) => println!("  last run: ERROR ({when}): {err}"),
                    (Some(s), None) => println!(
                        "  last run: {when} — {} pushed, {} pulled, {} del-local, {} del-remote, {} conflicts",
                        s.pushed, s.pulled, s.deleted_local, s.deleted_remote, s.conflicts
                    ),
                    (None, None) => println!("  last run: {when}"),
                }
            }
            true
        }
        Response::Synced(outcomes) => {
            let mut ok = true;
            for o in outcomes {
                match (&o.summary, &o.error) {
                    (Some(s), _) => println!(
                        "{}: {} pushed, {} pulled, {} del-local, {} del-remote, {} conflicts",
                        o.label, s.pushed, s.pulled, s.deleted_local, s.deleted_remote, s.conflicts
                    ),
                    (None, Some(err)) => {
                        ok = false;
                        println!("{}: ERROR: {err}", o.label);
                    }
                    (None, None) => println!("{}: done", o.label),
                }
            }
            ok
        }
        Response::Reloaded { ok, message } => {
            println!("{message}");
            *ok
        }
        Response::Error(err) => {
            eprintln!("error: {err}");
            false
        }
    }
}
