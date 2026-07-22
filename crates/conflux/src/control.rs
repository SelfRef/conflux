//! Synchronous IPC client used by the CLI control subcommands.
//!
//! The CLI stays tokio-free: it opens the Unix socket with `std`, writes one
//! JSON request line, and reads one JSON response line.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{anyhow, Context};
use conflux_core::engine::{PlanOp, Winner};
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
                    Some(secs) => conflux_core::timefmt::local(secs),
                    None => "never".to_string(),
                };
                println!("{} [{}] root={}", g.label, g.trigger, g.root);
                match (&g.last_summary, &g.last_error) {
                    (_, Some(err)) => println!("  last run: ERROR ({when}): {err}"),
                    (Some(s), None) => println!("  last run: {when} — {s}"),
                    (None, None) => println!("  last run: {when}"),
                }
            }
            true
        }
        Response::Synced(outcomes) => {
            let mut ok = true;
            for o in outcomes {
                match (&o.summary, &o.error) {
                    (Some(s), _) => println!("{}: {s}", o.label),
                    (None, Some(err)) => {
                        ok = false;
                        println!("{}: ERROR: {err}", o.label);
                    }
                    (None, None) => println!("{}: done", o.label),
                }
            }
            ok
        }
        Response::Planned(plans) => {
            let mut ok = true;
            for gp in plans {
                if let Some(err) = &gp.error {
                    ok = false;
                    println!("{} (dry run): ERROR: {err}", gp.label);
                    continue;
                }
                // `plan` is always Some when `error` is None.
                let changes = gp.plan.as_ref().map(|p| &p.changes[..]).unwrap_or(&[]);
                if changes.is_empty() {
                    println!("{} (dry run): no changes", gp.label);
                    continue;
                }
                let n = changes.len();
                println!(
                    "{} (dry run): {n} change{}",
                    gp.label,
                    if n == 1 { "" } else { "s" }
                );
                for change in changes {
                    println!(
                        "  {} {:<38} {}",
                        op_symbol(&change.op),
                        op_label(&change.op),
                        change.path
                    );
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

/// A one-character direction marker for a planned op: `↑` uploads, `↓` downloads,
/// `✗` deletes, `!` conflicts, `·` skips.
fn op_symbol(op: &PlanOp) -> char {
    match op {
        PlanOp::Push { .. } | PlanOp::PushPreserveRemote | PlanOp::CreateRemoteDir => '↑',
        PlanOp::Pull { .. } | PlanOp::PullPreserveLocal | PlanOp::CreateLocalDir => '↓',
        PlanOp::DeleteLocal
        | PlanOp::DeleteRemote
        | PlanOp::DeleteLocalDir
        | PlanOp::DeleteRemoteDir => '✗',
        PlanOp::Conflict { .. } => '!',
        PlanOp::OversizeSkip { .. } => '·',
    }
}

/// A short human description of a planned op.
fn op_label(op: &PlanOp) -> String {
    match op {
        PlanOp::Push { update: false } => "upload (new)".into(),
        PlanOp::Push { update: true } => "upload".into(),
        PlanOp::Pull { update: false } => "download (new)".into(),
        PlanOp::Pull { update: true } => "download".into(),
        PlanOp::DeleteLocal => "delete local".into(),
        PlanOp::DeleteRemote => "delete remote".into(),
        PlanOp::Conflict { winner } => format!("conflict (keep {})", winner_side(*winner)),
        PlanOp::PullPreserveLocal => "download (local kept as conflict copy)".into(),
        PlanOp::PushPreserveRemote => "upload (remote kept as conflict copy)".into(),
        PlanOp::OversizeSkip { size } => format!("skip ({} over max_file_size)", human_size(*size)),
        PlanOp::CreateLocalDir => "create local dir".into(),
        PlanOp::CreateRemoteDir => "create remote dir".into(),
        PlanOp::DeleteLocalDir => "delete local dir".into(),
        PlanOp::DeleteRemoteDir => "delete remote dir".into(),
    }
}

fn winner_side(winner: Winner) -> &'static str {
    match winner {
        Winner::Local => "local",
        Winner::Remote => "remote",
    }
}

/// Format a byte count with a binary unit, e.g. `1.5MB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}
