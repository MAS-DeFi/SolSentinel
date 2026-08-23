//! Stop, configure twice, and start Firedancer.

use std::path::Path;

use anyhow::Result;
use tracing::info;

use crate::{configure, process::Runner, service::ServiceManager};

/// Stops the service, runs configure-firedancer twice, then starts it.
///
/// Host configuration is applied twice because some Firedancer stages only
/// finish after an earlier pass has taken effect. Failure after stop leaves
/// the service stopped.
pub fn restart_firedancer(
    runner: &dyn Runner,
    service: &ServiceManager<'_>,
    repository: &Path,
    config: &Path,
) -> Result<()> {
    info!("restarting Firedancer (stop, configure, configure, start)");
    service.stop()?;
    configure::configure_firedancer(runner, repository, config)?;
    configure::configure_firedancer(runner, repository, config)?;
    service.start()?;
    info!("Firedancer restart completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque, fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Mutex,
    };

    use anyhow::{Result, anyhow};
    use tempfile::TempDir;

    use super::restart_firedancer;
    use crate::{
        process::{CommandOutcome, CommandSpec, Runner},
        service::ServiceManager,
    };

    struct FakeRunner {
        captures: Mutex<VecDeque<CommandOutcome>>,
        interactive_outcomes: Mutex<VecDeque<CommandOutcome>>,
        interactive_commands: Mutex<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        fn new(captures: Vec<CommandOutcome>, interactive_outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                captures: Mutex::new(VecDeque::from(captures)),
                interactive_outcomes: Mutex::new(VecDeque::from(interactive_outcomes)),
                interactive_commands: Mutex::new(Vec::new()),
            }
        }

        fn interactive_commands(&self) -> Vec<CommandSpec> {
            self.interactive_commands
                .lock()
                .expect("interactive commands lock")
                .clone()
        }

        fn unused_captures(&self) -> usize {
            self.captures.lock().expect("capture lock").len()
        }
    }

    impl Runner for FakeRunner {
        fn capture(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            self.captures
                .lock()
                .expect("capture lock")
                .pop_front()
                .ok_or_else(|| anyhow!("unexpected capture"))
        }

        fn streaming(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            Err(anyhow!("unexpected streaming command"))
        }

        fn interactive(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
            self.interactive_commands
                .lock()
                .expect("interactive commands lock")
                .push(spec.clone());
            self.interactive_outcomes
                .lock()
                .expect("interactive outcomes lock")
                .pop_front()
                .ok_or_else(|| anyhow!("unexpected interactive command"))
        }
    }

    struct PreparedRepo {
        _temp: TempDir,
        repository: PathBuf,
        fdctl: PathBuf,
        config: PathBuf,
    }

    fn prepared_repo() -> Result<PreparedRepo> {
        let temp = TempDir::new()?;
        let repository = temp.path().to_path_buf();
        let fdctl = repository.join("build/native/gcc/bin/fdctl");
        fs::create_dir_all(fdctl.parent().expect("fdctl parent"))?;
        fs::write(&fdctl, "")?;
        let mut permissions = fs::metadata(&fdctl)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fdctl, permissions)?;
        let config = repository.join("active-fd-config.toml");
        fs::write(&config, "")?;
        Ok(PreparedRepo {
            _temp: temp,
            repository,
            fdctl,
            config,
        })
    }

    fn show(state: &str) -> CommandOutcome {
        CommandOutcome::success(format!("LoadState=loaded\nActiveState={state}\n"))
    }

    fn configure_spec(repo: &PreparedRepo) -> CommandSpec {
        CommandSpec::new("sudo")
            .arg("--")
            .arg(&repo.fdctl)
            .args(["configure", "init", "all", "--config"])
            .arg(&repo.config)
            .cwd(&repo.repository)
    }

    fn systemctl_spec(action: &str) -> CommandSpec {
        CommandSpec::new("sudo").args(["--", "systemctl", action, "--", "frankendancer.service"])
    }

    #[test]
    fn runs_stop_configure_configure_start() -> Result<()> {
        let repo = prepared_repo()?;
        let runner = FakeRunner::new(
            vec![
                show("active"),
                show("inactive"),
                show("inactive"),
                show("active"),
            ],
            vec![
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        restart_firedancer(&runner, &service, &repo.repository, &repo.config)?;

        assert_eq!(runner.unused_captures(), 0);
        assert_eq!(
            runner.interactive_commands(),
            [
                systemctl_spec("stop"),
                configure_spec(&repo),
                configure_spec(&repo),
                systemctl_spec("start"),
            ]
        );
        Ok(())
    }

    #[test]
    fn skips_stop_when_already_inactive() -> Result<()> {
        let repo = prepared_repo()?;
        let runner = FakeRunner::new(
            vec![show("inactive"), show("inactive"), show("active")],
            vec![
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        restart_firedancer(&runner, &service, &repo.repository, &repo.config)?;

        assert_eq!(runner.unused_captures(), 0);
        assert_eq!(
            runner.interactive_commands(),
            [
                configure_spec(&repo),
                configure_spec(&repo),
                systemctl_spec("start"),
            ]
        );
        Ok(())
    }

    #[test]
    fn does_not_start_when_the_first_configure_fails() -> Result<()> {
        let repo = prepared_repo()?;
        let runner = FakeRunner::new(
            vec![show("active"), show("inactive")],
            vec![
                CommandOutcome::success(""),
                CommandOutcome::failure(1, "configure failed"),
            ],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        let error = restart_firedancer(&runner, &service, &repo.repository, &repo.config)
            .expect_err("first configure should fail");
        assert!(
            format!("{error:#}").contains("Firedancer host configuration failed"),
            "{error:#}"
        );
        assert_eq!(
            runner.interactive_commands(),
            [systemctl_spec("stop"), configure_spec(&repo)]
        );
        Ok(())
    }

    #[test]
    fn does_not_start_when_the_second_configure_fails() -> Result<()> {
        let repo = prepared_repo()?;
        let runner = FakeRunner::new(
            vec![show("active"), show("inactive")],
            vec![
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::failure(1, "configure failed"),
            ],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        let error = restart_firedancer(&runner, &service, &repo.repository, &repo.config)
            .expect_err("second configure should fail");
        assert!(
            format!("{error:#}").contains("Firedancer host configuration failed"),
            "{error:#}"
        );
        assert_eq!(
            runner.interactive_commands(),
            [
                systemctl_spec("stop"),
                configure_spec(&repo),
                configure_spec(&repo)
            ]
        );
        Ok(())
    }
}
