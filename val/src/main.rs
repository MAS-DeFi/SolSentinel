//! CLI entry point and command dispatch.

mod cli;
mod configure;
mod lock;
mod logging;
mod paths;
mod privilege;
mod process;
mod repository;
mod service;
mod status;

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
    let _command_lock = match lock::CommandLock::acquire(&paths.base) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("error: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let _log_guard = match logging::init(&paths.log_dir, cli.verbose) {
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
        Commands::Status { json } => {
            let report = StatusReport::load(
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
    }
}
