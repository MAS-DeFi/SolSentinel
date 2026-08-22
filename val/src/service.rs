//! Idempotent systemd service management.

use std::fmt;

use anyhow::{Context, Result, bail};
use tracing::{error, info};

use crate::process::{CommandOutcome, CommandSpec, Runner, require_success};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Active,
    Activating,
    Deactivating,
    Inactive,
    Failed,
    Missing,
    Unknown(String),
}

impl fmt::Display for ServiceState {
    /// Formats the normalized systemd state.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => formatter.write_str("active"),
            Self::Activating => formatter.write_str("activating"),
            Self::Deactivating => formatter.write_str("deactivating"),
            Self::Inactive => formatter.write_str("inactive"),
            Self::Failed => formatter.write_str("failed"),
            Self::Missing => formatter.write_str("not-found"),
            Self::Unknown(value) => formatter.write_str(value),
        }
    }
}

pub struct ServiceManager<'a> {
    runner: &'a dyn Runner,
    unit: &'a str,
    run_as_root: bool,
}

impl<'a> ServiceManager<'a> {
    /// Creates a manager for one systemd unit.
    pub fn new(runner: &'a dyn Runner, unit: &'a str, run_as_root: bool) -> Self {
        Self {
            runner,
            unit,
            run_as_root,
        }
    }

    /// Reads the unit's current load and active states.
    pub fn state(&self) -> Result<ServiceState> {
        let outcome = require_success(
            self.runner.capture(&CommandSpec::new("systemctl").args([
                "show",
                "--property=LoadState",
                "--property=ActiveState",
                "--",
                self.unit,
            ]))?,
            &format!("querying systemd unit {}", self.unit),
        )?;
        parse_state(&outcome.stdout)
    }

    /// Reads the version from the running service's executable.
    pub fn running_fdctl_version(&self) -> Result<Option<String>> {
        let outcome = require_success(
            self.runner.capture(&CommandSpec::new("systemctl").args([
                "show",
                "--property=MainPID",
                "--value",
                "--",
                self.unit,
            ]))?,
            &format!("querying main PID for {}", self.unit),
        )?;
        let pid =
            outcome.stdout.trim().parse::<u32>().with_context(|| {
                format!("systemctl returned an invalid MainPID for {}", self.unit)
            })?;
        if pid == 0 {
            return Ok(None);
        }

        let executable = format!("/proc/{pid}/exe");
        let command = if self.run_as_root {
            CommandSpec::new(executable).arg("version")
        } else {
            CommandSpec::new("sudo")
                .arg("--")
                .arg(executable)
                .arg("version")
        };
        let version = require_success(
            self.runner.capture(&command)?,
            "querying the running fdctl version",
        )?;
        let version = version.stdout.trim();
        if version.is_empty() {
            bail!("running fdctl returned an empty version");
        }
        Ok(Some(version.to_owned()))
    }

    /// Starts the unit unless it is already active.
    pub fn start(&self) -> Result<bool> {
        let initial = self.state()?;
        match initial {
            ServiceState::Active => {
                info!(service = self.unit, "service is already active");
                return Ok(false);
            }
            ServiceState::Missing => {
                bail!("systemd unit {} was not found", self.unit);
            }
            _ => {}
        }

        info!(service = self.unit, state = %initial, "starting service");
        let outcome = self.runner.interactive(&self.action("start"))?;
        if !outcome.success {
            self.log_recent_journal();
            bail!(
                "could not start {}: {}",
                self.unit,
                outcome.exit_description()
            );
        }

        let final_state = self.state()?;
        if final_state != ServiceState::Active {
            self.log_recent_journal();
            bail!(
                "{} did not become active after start (state: {})",
                self.unit,
                final_state
            );
        }

        info!(service = self.unit, "service started");
        Ok(true)
    }

    /// Stops the unit unless it is already inactive.
    pub fn stop(&self) -> Result<bool> {
        let initial = self.state()?;
        match initial {
            ServiceState::Inactive => {
                info!(service = self.unit, "service is already inactive");
                return Ok(false);
            }
            ServiceState::Missing => {
                bail!("systemd unit {} was not found", self.unit);
            }
            _ => {}
        }

        info!(service = self.unit, state = %initial, "stopping service");
        let outcome = self.runner.interactive(&self.action("stop"))?;
        require_success(outcome, &format!("stopping {}", self.unit))?;

        let final_state = self.state()?;
        if final_state != ServiceState::Inactive {
            bail!(
                "{} did not become inactive after stop (state: {})",
                self.unit,
                final_state
            );
        }

        info!(service = self.unit, "service stopped");
        Ok(true)
    }

    /// Builds an optionally elevated systemctl command.
    fn action(&self, action: &str) -> CommandSpec {
        if self.run_as_root {
            CommandSpec::new("systemctl").args([action, "--", self.unit])
        } else {
            CommandSpec::new("sudo").args(["--", "systemctl", action, "--", self.unit])
        }
    }

    /// Logs recent journal entries after a start failure.
    fn log_recent_journal(&self) {
        let journal_args = [
            "journalctl",
            "--unit",
            self.unit,
            "--lines",
            "20",
            "--no-pager",
        ];
        let spec = if self.run_as_root {
            CommandSpec::new("journalctl").args(&journal_args[1..])
        } else {
            CommandSpec::new("sudo").arg("--").args(journal_args)
        };
        match self.runner.capture(&spec) {
            Ok(CommandOutcome {
                success: true,
                stdout,
                ..
            }) if !stdout.trim().is_empty() => {
                error!(service = self.unit, journal = %stdout.trim(), "recent service journal");
            }
            Ok(CommandOutcome { success: true, .. }) => {
                error!(service = self.unit, "recent service journal was empty");
            }
            Ok(outcome) => {
                error!(
                    service = self.unit,
                    result = %outcome.exit_description(),
                    stderr = %outcome.stderr.trim(),
                    "could not read recent service journal"
                );
            }
            Err(journal_error) => {
                error!(
                    service = self.unit,
                    error = %journal_error,
                    "could not read recent service journal"
                );
            }
        }
    }
}

/// Parses systemctl properties into a normalized state.
fn parse_state(output: &str) -> Result<ServiceState> {
    let mut load_state = None;
    let mut active_state = None;

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("LoadState=") {
            load_state = Some(value);
        } else if let Some(value) = line.strip_prefix("ActiveState=") {
            active_state = Some(value);
        }
    }

    if load_state == Some("not-found") {
        return Ok(ServiceState::Missing);
    }

    let state = match active_state {
        Some("active") => ServiceState::Active,
        Some("activating") => ServiceState::Activating,
        Some("deactivating") => ServiceState::Deactivating,
        Some("inactive") => ServiceState::Inactive,
        Some("failed") => ServiceState::Failed,
        Some(other) => ServiceState::Unknown(other.to_owned()),
        None => bail!("systemctl output did not include ActiveState"),
    };
    Ok(state)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use anyhow::{Result, anyhow};

    use super::{ServiceManager, ServiceState, parse_state};
    use crate::process::{CommandOutcome, CommandSpec, Runner};

    struct FakeRunner {
        captures: Mutex<VecDeque<CommandOutcome>>,
        interactive: Mutex<VecDeque<CommandOutcome>>,
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

        fn interactive(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            self.interactive
                .lock()
                .expect("interactive lock")
                .pop_front()
                .ok_or_else(|| anyhow!("unexpected interactive command"))
        }
    }

    #[test]
    fn parses_loaded_and_missing_units() -> Result<()> {
        assert_eq!(
            parse_state("LoadState=loaded\nActiveState=active\n")?,
            ServiceState::Active
        );
        assert_eq!(
            parse_state("LoadState=not-found\nActiveState=inactive\n")?,
            ServiceState::Missing
        );
        Ok(())
    }

    #[test]
    fn reads_version_from_the_running_process() -> Result<()> {
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("4321\n"),
                CommandOutcome::success("v1.2.3\n"),
            ])),
            interactive: Mutex::new(VecDeque::new()),
        };
        let manager = ServiceManager::new(&runner, "frankendancer.service", false);

        assert_eq!(manager.running_fdctl_version()?.as_deref(), Some("v1.2.3"));
        Ok(())
    }

    #[test]
    fn start_is_a_noop_when_already_active() -> Result<()> {
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([CommandOutcome::success(
                "LoadState=loaded\nActiveState=active\n",
            )])),
            interactive: Mutex::new(VecDeque::new()),
        };
        let manager = ServiceManager::new(&runner, "frankendancer.service", false);

        assert!(!manager.start()?);
        assert!(
            runner
                .interactive
                .lock()
                .expect("interactive lock")
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn stop_is_a_noop_when_already_inactive() -> Result<()> {
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([CommandOutcome::success(
                "LoadState=loaded\nActiveState=inactive\n",
            )])),
            interactive: Mutex::new(VecDeque::new()),
        };
        let manager = ServiceManager::new(&runner, "frankendancer.service", false);

        assert!(!manager.stop()?);
        assert!(
            runner
                .interactive
                .lock()
                .expect("interactive lock")
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn start_verifies_the_final_state() -> Result<()> {
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("LoadState=loaded\nActiveState=inactive\n"),
                CommandOutcome::success("LoadState=loaded\nActiveState=active\n"),
            ])),
            interactive: Mutex::new(VecDeque::from([CommandOutcome::success("")])),
        };
        let manager = ServiceManager::new(&runner, "frankendancer.service", true);

        assert!(manager.start()?);
        Ok(())
    }
}
