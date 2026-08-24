//! CLI entry point and command dispatch.

mod cli;
mod configure;
mod lock;
mod logging;
mod monitor;
mod paths;
mod privilege;
mod process;
mod progress;
mod repository;
mod restart;
mod service;
mod status;
mod update_full;

use std::{
    io::{self, Write},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use crate::{
    cli::{Cli, Commands},
    paths::AppPaths,
    process::SystemRunner,
    service::ServiceManager,
    status::StatusReport,
};

/// Parses arguments, initializes runtime services, and executes one command.
fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(error) = privilege::reexec_as_validator_user_if_needed(&cli.command) {
        eprintln!("error: {error:#}");
        return ExitCode::FAILURE;
    }

    let paths = match AppPaths::resolve(&cli) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let _command_lock = if cli.command.holds_command_lock() {
        match lock::CommandLock::acquire(&paths.base) {
            Ok(lock) => Some(lock),
            Err(error) => {
                eprintln!("error: {error:#}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let compact_terminal = cli.verbose == 0
        && matches!(
            &cli.command,
            Commands::UpdateFull { .. } | Commands::Monitor { .. }
        );
    let _log_guard = match logging::init(&paths.log_dir, cli.verbose, compact_terminal) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
    };

    info!(
        command = cli.command.name(),
        base_path = %paths.base.display(),
        "val command started"
    );
    match dispatch(&cli, &paths) {
        Ok(()) => {
            info!(command = cli.command.name(), "val command completed");
            ExitCode::SUCCESS
        }
        Err(command_error) => {
            error!(
                command = cli.command.name(),
                error = %format!("{command_error:#}"),
                "val command failed"
            );
            ExitCode::FAILURE
        }
    }
}

/// Routes the selected subcommand to its implementation.
fn dispatch(cli: &Cli, paths: &AppPaths) -> Result<()> {
    let runner = SystemRunner;
    let service = ServiceManager::new(&runner, &cli.service, privilege::is_root());

    match &cli.command {
        Commands::UpdateFiredancer { git_ref } => {
            repository::update_firedancer(&runner, &paths.repository, git_ref)
        }
        Commands::UpdateFull { git_ref } => update_full::update_full(
            &runner,
            &service,
            paths,
            git_ref,
            &cli.service,
            cli.verbose == 0,
        ),
        Commands::MakeFiredancer => {
            repository::make_firedancer(&runner, &paths.repository)?;
            Ok(())
        }
        Commands::ConfigureFiredancer => {
            configure::configure_firedancer(&runner, &paths.repository, &paths.config)
        }
        Commands::StartFiredancer => {
            service.start()?;
            Ok(())
        }
        Commands::StopFiredancer => {
            service.stop()?;
            Ok(())
        }
        Commands::RestartFiredancer => {
            restart::restart_firedancer(&runner, &service, &paths.repository, &paths.config)
        }
        Commands::Status { json } => {
            let report = StatusReport::collect(
                paths,
                service.state()?,
                service.running_fdctl_version()?,
                repository::built_fdctl_version(&runner, &paths.repository)?,
            )?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            if *json {
                serde_json::to_writer_pretty(&mut output, &report)
                    .context("could not serialize status as JSON")?;
                writeln!(output).context("could not write status output")?;
            } else {
                report
                    .write_human(&mut output)
                    .context("could not write status output")?;
            }
            Ok(())
        }
        Commands::Monitor { all, url } => monitor::run(&paths.config, url.as_deref(), *all),
    }
}
