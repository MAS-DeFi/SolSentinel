//! Privilege checks and validator-user re-execution.

use std::{env, os::unix::process::CommandExt, process::Command};

use anyhow::{Context, Result, bail};
use nix::unistd::Uid;

use crate::{cli::Commands, paths::non_root_sudo_user};

/// Re-executes sudo invocations as the validator user when needed.
pub fn reexec_as_validator_user_if_needed(command: &Commands) -> Result<()> {
    if !Uid::effective().is_root() {
        return Ok(());
    }

    let Some(sudo_user) = non_root_sudo_user() else {
        if command.requires_validator_user() {
            bail!(
                "{} must not run from a direct root login; run it as the validator user or via sudo from that user",
                command.name()
            );
        }
        return Ok(());
    };

    let executable = env::current_exe().context("could not resolve the val executable path")?;
    let error = Command::new("sudo")
        .arg("-u")
        .arg(&sudo_user)
        .arg("-H")
        .arg("--")
        .arg(executable)
        .args(env::args_os().skip(1))
        .exec();

    Err(error).with_context(|| {
        format!(
            "could not re-execute {} as validator user '{sudo_user}'",
            command.name()
        )
    })
}

/// Returns whether the process currently has root privileges.
pub fn is_root() -> bool {
    Uid::effective().is_root()
}
