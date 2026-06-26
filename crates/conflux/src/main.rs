//! `conflux` — background file-sync service. Single binary providing both the
//! daemon (`conflux daemon`) and CLI control/config subcommands.

mod control;
mod daemon;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use conflux_core::ipc::{Request, SyncTarget};
use conflux_core::{Config, Paths, RunMode};

#[derive(Parser)]
#[command(name = "conflux", version, about = "Background file-sync service")]
struct Cli {
    /// Use system-wide paths (/etc, /var/lib, /run) instead of per-user XDG paths.
    #[arg(long, global = true)]
    system: bool,

    /// Override the config file path (also settable via $CONFLUX_CONFIG).
    #[arg(long, short, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the sync daemon (invoked by systemd).
    Daemon,
    /// Inspect or validate configuration.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Show daemon status.
    Status,
    /// Trigger a sync now.
    Sync {
        /// Sync group to run; omit with --all to run everything.
        group: Option<String>,
        /// Run all groups.
        #[arg(long)]
        all: bool,
        /// Restrict to a named profile.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Reload the daemon configuration.
    Reload,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Validate the configuration file.
    Validate,
    /// Print the resolved configuration.
    Show,
    /// Print resolved paths (config/state/socket).
    Paths,
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("CONFLUX_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap_or_default();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    let mode = RunMode::detect(cli.system);
    let mut paths = Paths::resolve(mode)?;
    if let Some(path) = &cli.config {
        paths.config = path.clone();
    }

    match &cli.command {
        Command::Config { action } => run_config(action, &paths).map(|()| ExitCode::SUCCESS),
        Command::Daemon => {
            let config = Config::load(&paths.config)?;
            daemon::run(config, paths).map(|()| ExitCode::SUCCESS)
        }
        Command::Status => {
            let response = control::send(&paths.socket, &Request::Status)?;
            Ok(exit_code(control::print_response(&response)))
        }
        Command::Sync {
            group,
            all,
            profile,
        } => {
            let target = sync_target(group.clone(), *all, profile.clone())?;
            let response = control::send(&paths.socket, &Request::Sync(target))?;
            Ok(exit_code(control::print_response(&response)))
        }
        Command::Reload => {
            let response = control::send(&paths.socket, &Request::Reload)?;
            Ok(exit_code(control::print_response(&response)))
        }
    }
}

/// Translate CLI sync arguments into an IPC target.
fn sync_target(
    group: Option<String>,
    all: bool,
    profile: Option<String>,
) -> anyhow::Result<SyncTarget> {
    match (group, profile, all) {
        (Some(g), None, false) => Ok(SyncTarget::Group(g)),
        (None, Some(p), false) => Ok(SyncTarget::Profile(p)),
        (None, None, true) => Ok(SyncTarget::All),
        (None, None, false) => Ok(SyncTarget::All),
        _ => anyhow::bail!("specify at most one of: a group, --profile, or --all"),
    }
}

/// Map a command's success flag to a process exit code.
fn exit_code(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_config(action: &ConfigCmd, paths: &Paths) -> anyhow::Result<()> {
    match action {
        ConfigCmd::Paths => {
            println!("config: {}", paths.config.display());
            println!("state:  {}", paths.state.display());
            println!("socket: {}", paths.socket.display());
            Ok(())
        }
        ConfigCmd::Validate => {
            let cfg = Config::load(&paths.config)?;
            println!(
                "ok: {} remote(s), {} sync group(s)",
                cfg.remotes.len(),
                cfg.syncs.len()
            );
            Ok(())
        }
        ConfigCmd::Show => {
            let cfg = Config::load(&paths.config)?;
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}
