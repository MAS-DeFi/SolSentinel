//! Firedancer host configuration.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result, bail};
use tracing::info;

use crate::process::{CommandSpec, Runner, executable_in, require_success};

/// Initializes all Firedancer host configuration stages.
pub fn configure_firedancer(runner: &dyn Runner, repository: &Path, config: &Path) -> Result<()> {
    let fdctl = executable_in(repository, "build/native/gcc/bin/fdctl");
    let metadata = fs::metadata(&fdctl)
        .with_context(|| format!("fdctl binary not found: {}", fdctl.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("fdctl is not executable: {}", fdctl.display());
    }

    let config_metadata = fs::metadata(config)
        .with_context(|| format!("Firedancer config not found: {}", config.display()))?;
    if !config_metadata.is_file() {
        bail!("Firedancer config is not a file: {}", config.display());
    }

    info!(config = %config.display(), "configuring Firedancer host");
    let command = CommandSpec::new("sudo")
        .arg("--")
        .arg(fdctl)
        .args(["configure", "init", "all", "--config"])
        .arg(config)
        .cwd(repository);
    require_success(
        runner.interactive(&command)?,
        "Firedancer host configuration",
    )?;
    info!("Firedancer host configuration completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, sync::Mutex};

    use anyhow::{Result, anyhow};
    use tempfile::TempDir;

    use super::configure_firedancer;
    use crate::process::{CommandOutcome, CommandSpec, Runner};

    struct FakeRunner {
        command: Mutex<Option<CommandSpec>>,
    }

    impl Runner for FakeRunner {
        fn capture(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            Err(anyhow!("unexpected capture command"))
        }

        fn streaming(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            Err(anyhow!("unexpected streaming command"))
        }

        fn interactive(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
            *self.command.lock().expect("command lock") = Some(spec.clone());
            Ok(CommandOutcome::success(""))
        }
    }

    #[test]
    fn invokes_fdctl_with_the_active_config() -> Result<()> {
        let temp = TempDir::new()?;
        let fdctl = temp.path().join("build/native/gcc/bin/fdctl");
        fs::create_dir_all(fdctl.parent().expect("fdctl parent"))?;
        fs::write(&fdctl, "")?;
        let mut permissions = fs::metadata(&fdctl)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fdctl, permissions)?;
        let config = temp.path().join("active-fd-config.toml");
        fs::write(&config, "")?;

        let runner = FakeRunner {
            command: Mutex::new(None),
        };
        configure_firedancer(&runner, temp.path(), &config)?;

        let command = runner
            .command
            .lock()
            .expect("command lock")
            .clone()
            .expect("interactive command");
        assert_eq!(command.program, "sudo");
        assert_eq!(command.cwd.as_deref(), Some(temp.path()));
        assert_eq!(
            command.args,
            [
                "--",
                fdctl.to_str().expect("UTF-8 fdctl path"),
                "configure",
                "init",
                "all",
                "--config",
                config.to_str().expect("UTF-8 config path"),
            ]
        );
        Ok(())
    }
}
