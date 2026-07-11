//! `conflux` — background file-sync service. Single binary providing both the
//! daemon (`conflux daemon`) and CLI control/config subcommands.

mod control;
mod daemon;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use conflux_core::ipc::{Request, SyncTarget};
use conflux_core::{Config, Paths, RunMode};
use tracing_subscriber::{fmt, prelude::*, reload, EnvFilter, Registry};

/// Handle to swap the log filter once the config's `log_level` is known.
type LogReload = reload::Handle<EnvFilter, Registry>;

#[derive(Parser)]
#[command(name = "conflux", version, about = "Background file-sync service")]
struct Cli {
    /// Use system-wide paths (/etc, /var/lib, /run) instead of per-user XDG paths.
    #[arg(long, global = true)]
    system: bool,

    /// Override the config file path (also settable via $CONFLUX_CONFIG).
    #[arg(long, short, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Instance profile, defaulting to "default". Selects which daemon this
    /// command targets: it namespaces the state dir and control socket, and the
    /// daemon runs only this profile's syncs, so one shared config can drive
    /// many hosts.
    #[arg(long, short, global = true, value_name = "NAME")]
    profile: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the sync daemon in the foreground.
    Daemon,
    /// Inspect or validate configuration.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Show daemon status.
    Status,
    /// Trigger a sync now on the targeted profile's daemon.
    Sync {
        /// Sync to run, by its `id` or `remote:remote_path` label; omit to run
        /// every group of this profile.
        group: Option<String>,
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
    let log_reload = init_tracing();
    let cli = Cli::parse();
    match run(&cli, &log_reload) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Start tracing from `CONFLUX_LOG` (default `info`) and return a handle so the
/// daemon can later apply `[daemon] log_level` from the config.
fn init_tracing() -> LogReload {
    let filter = EnvFilter::try_from_env("CONFLUX_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap_or_default();
    let (filter, handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
    handle
}

/// Turn a bare `[daemon] log_level` into a tracing directive, quieting very
/// chatty dependencies so `debug` stays useful. Their own output only shows at
/// `trace` (or when the user sets an explicit `CONFLUX_LOG`).
fn daemon_log_directive(level: &str) -> String {
    if level.eq_ignore_ascii_case("trace") {
        level.to_string()
    } else {
        format!("{level},globset=info")
    }
}

fn run(cli: &Cli, log_reload: &LogReload) -> anyhow::Result<ExitCode> {
    let mode = RunMode::detect(cli.system);
    let mut paths = Paths::resolve(mode, cli.profile.as_deref())?;
    if let Some(path) = &cli.config {
        paths.config = path.clone();
    }

    match &cli.command {
        Command::Config { action } => run_config(action, &paths).map(|()| ExitCode::SUCCESS),
        Command::Daemon => {
            let mut config = Config::load(&paths.config)?;
            // Apply the configured log level, unless CONFLUX_LOG is set (env wins).
            if std::env::var_os("CONFLUX_LOG").is_none() {
                if let Ok(filter) = EnvFilter::try_new(daemon_log_directive(&config.daemon.log_level))
                {
                    let _ = log_reload.modify(|f| *f = filter);
                }
            }
            // An explicit `--profile` overrides the config default so one shared
            // config file can drive different profiles on different hosts.
            if let Some(profile) = &cli.profile {
                config.daemon.profile = Some(profile.clone());
            }
            daemon::run(config, paths).map(|()| ExitCode::SUCCESS)
        }
        Command::Status => {
            let response = control::send(&paths.socket, &Request::Status)?;
            Ok(exit_code(control::print_response(&response)))
        }
        Command::Sync { group } => {
            let target = match group {
                Some(g) => SyncTarget::Group(g.clone()),
                None => SyncTarget::All,
            };
            let response = control::send(&paths.socket, &Request::Sync(target))?;
            Ok(exit_code(control::print_response(&response)))
        }
        Command::Reload => {
            let response = control::send(&paths.socket, &Request::Reload)?;
            Ok(exit_code(control::print_response(&response)))
        }
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
            for lint in cfg.warnings() {
                println!("warning: {lint}");
            }
            Ok(())
        }
        ConfigCmd::Show => {
            let cfg = Config::load(&paths.config)?;
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}
