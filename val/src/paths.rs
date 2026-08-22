//! Runtime path and operating-user resolution.

use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};
use nix::unistd::{Uid, User};

use crate::cli::Cli;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub base: PathBuf,
    pub repository: PathBuf,
    pub config: PathBuf,
    pub log_dir: PathBuf,
    pub username: String,
}

impl AppPaths {
    /// Resolves validated absolute paths from CLI options and defaults.
    pub fn resolve(cli: &Cli) -> Result<Self> {
        validate_service_name(&cli.service)?;

        let username = operating_username()?;
        let base = absolute_path(match &cli.base_path {
            Some(path) => path.clone(),
            None => operating_home(&username)?,
        })?;
        let repository = absolute_path(
            cli.repo_path
                .clone()
                .unwrap_or_else(|| base.join("code/firedancer")),
        )?;
        let config = absolute_path(
            cli.config
                .clone()
                .unwrap_or_else(|| base.join("active-fd-config.toml")),
        )?;
        let log_dir = absolute_path(cli.log_dir.clone().unwrap_or_else(|| base.join("logs")))?;

        Ok(Self {
            base,
            repository,
            config,
            log_dir,
            username,
        })
    }
}

/// Converts a relative path to an absolute path.
fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }

    Ok(env::current_dir()
        .context("could not determine the current directory")?
        .join(path))
}

/// Resolves the validator operator's username.
fn operating_username() -> Result<String> {
    if let Some(user) = non_root_sudo_user() {
        return Ok(user);
    }

    if let Some(user) = env::var_os("USER").filter(|value| !value.is_empty()) {
        return Ok(user.to_string_lossy().into_owned());
    }

    let user = User::from_uid(Uid::effective())
        .context("could not look up the current operating-system user")?
        .context("current operating-system user has no passwd entry")?;
    Ok(user.name)
}

/// Resolves the validator operator's home directory.
fn operating_home(username: &str) -> Result<PathBuf> {
    if non_root_sudo_user().is_some() {
        let user = User::from_name(username)
            .with_context(|| format!("could not look up sudo user '{username}'"))?
            .with_context(|| format!("sudo user '{username}' has no passwd entry"))?;
        return Ok(user.dir);
    }

    if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }

    let user = User::from_uid(Uid::effective())
        .context("could not look up the current user's home directory")?
        .context("current user has no passwd entry")?;
    Ok(user.dir)
}

/// Returns a non-root user recorded by sudo.
pub fn non_root_sudo_user() -> Option<String> {
    env::var("SUDO_USER")
        .ok()
        .filter(|user| !user.is_empty() && user != "root")
}

/// Rejects ambiguous or option-like systemd unit names.
fn validate_service_name(service: &str) -> Result<()> {
    if service.is_empty() {
        bail!("systemd service name cannot be empty");
    }
    if service.starts_with('-') {
        bail!("systemd service name cannot begin with '-'");
    }
    if service.chars().any(char::is_whitespace) {
        bail!("systemd service name cannot contain whitespace");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_service_name;

    #[test]
    fn service_name_validation_rejects_unsafe_values() {
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("--now").is_err());
        assert!(validate_service_name("bad unit.service").is_err());
        assert!(validate_service_name("frankendancer.service").is_ok());
    }
}
