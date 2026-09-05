//! Full Firedancer update orchestration.

use std::{
    cell::RefCell,
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    monitor,
    paths::AppPaths,
    process::{CompactOutputGuard, Runner},
    progress::{StageProgress, format_duration},
    repository,
    restart::{RestartPhase, RestartReporter, restart_firedancer_with_reporter},
    service::{ServiceManager, ServiceState},
};

/// Runs checkout, dependency install, build, restart, and readiness monitoring.
pub fn update_full(
    runner: &dyn Runner,
    service: &ServiceManager<'_>,
    paths: &AppPaths,
    git_ref: &str,
    service_name: &str,
    compact: bool,
) -> Result<()> {
    monitor::validate_config(&paths.config)?;
    let _compact_guard = compact.then(CompactOutputGuard::enable);
    run_update_full(
        runner,
        service,
        paths,
        git_ref,
        service_name,
        compact,
        &monitor::wait_until_running,
    )
}

fn run_update_full(
    runner: &dyn Runner,
    service: &ServiceManager<'_>,
    paths: &AppPaths,
    git_ref: &str,
    service_name: &str,
    compact: bool,
    wait_for_running: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let started = Instant::now();
    let log_path = paths.log_dir.join("val.log");
    let mut progress = ProgressReporter::new(compact, git_ref, service_name, &log_path);

    progress.begin();

    if let Err(error) = repository::update_firedancer(runner, &paths.repository, git_ref) {
        progress.fail_update(&error, ServiceImpact::Unchanged);
        return Err(error);
    }
    progress.finish_update();

    progress.begin_build();
    let build_duration = match repository::make_firedancer(runner, &paths.repository) {
        Ok(duration) => {
            progress.finish_build(duration);
            duration
        }
        Err(error) => {
            progress.fail_build(&error, ServiceImpact::Unchanged);
            return Err(error);
        }
    };

    let restart_progress = RefCell::new(RestartProgress::new(compact));
    progress.begin_restart();

    if let Err(error) = restart_firedancer_with_reporter(
        runner,
        service,
        &paths.repository,
        &paths.config,
        Some(&RestartProgressReporter {
            inner: &restart_progress,
        }),
    ) {
        restart_progress.borrow_mut().mark_restart_failed();
        let impact = classify_restart_failure(service);
        restart_progress
            .borrow_mut()
            .finalize_start_from_service(service);
        progress.fail_restart(&error, impact, restart_progress.into_inner());
        return Err(error);
    }

    restart_progress
        .borrow_mut()
        .finalize_start_from_service(service);
    let restart_progress = restart_progress.into_inner();
    progress.finish_restart(&restart_progress);
    progress.begin_monitor();
    if let Err(error) = wait_for_running(&paths.config) {
        progress.fail_monitor(&error);
        return Err(error);
    }

    progress.finish_success(started.elapsed(), build_duration, restart_progress);
    Ok(())
}

enum ServiceImpact {
    Unchanged,
    Stopped,
    FailedToRestart,
    ReadinessUnknown,
}

fn classify_restart_failure(service: &ServiceManager<'_>) -> ServiceImpact {
    match service.state() {
        Ok(ServiceState::Active) => ServiceImpact::Unchanged,
        Ok(ServiceState::Activating) | Ok(ServiceState::Deactivating) => {
            ServiceImpact::FailedToRestart
        }
        Ok(ServiceState::Failed) => ServiceImpact::FailedToRestart,
        Ok(_) => ServiceImpact::Stopped,
        Err(_) => ServiceImpact::Stopped,
    }
}

struct ProgressReporter<'a> {
    compact: bool,
    git_ref: &'a str,
    service: &'a str,
    log_path: &'a Path,
    current: Option<StageProgress>,
}

impl<'a> ProgressReporter<'a> {
    fn new(compact: bool, git_ref: &'a str, service: &'a str, log_path: &'a Path) -> Self {
        Self {
            compact,
            git_ref,
            service,
            log_path,
            current: None,
        }
    }

    fn begin(&mut self) {
        if self.compact {
            emit_line(&format!("[1/4] Update Firedancer ({})", self.git_ref));
            self.current = Some(StageProgress::start(
                true,
                "      checkout and dependencies",
            ));
        }
    }

    fn finish_update(&mut self) {
        self.finish_current("done");
    }

    fn begin_build(&mut self) {
        debug_assert!(self.current.is_none());
        if self.compact {
            self.current = Some(StageProgress::start(true, "[2/4] Build Firedancer"));
        }
    }

    fn finish_build(&mut self, duration: Duration) {
        self.finish_current(&format!("done ({})", format_duration(duration)));
    }

    fn begin_restart(&mut self) {
        debug_assert!(self.current.is_none());
        if self.compact {
            self.current = Some(StageProgress::start(true, "[3/4] Restart service"));
        }
    }

    fn finish_restart(&mut self, restart: &RestartProgress) {
        self.finish_current("done");
        restart.write_lines(io::stdout()).ok();
    }

    fn begin_monitor(&self) {
        if self.compact {
            emit_line("[4/4] Wait for validator");
        }
    }

    fn finish_success(
        &mut self,
        total: Duration,
        build_duration: Duration,
        restart: RestartProgress,
    ) {
        self.finish_current("done");
        if !self.compact {
            return;
        }

        emit_line("");
        emit_line(&format!("Update complete: {}", self.git_ref));
        emit_line(&format!(
            "Service: {} ({})",
            self.service, restart.final_state
        ));
        emit_line(&format!(
            "Build: {} | Total: {}",
            format_duration(build_duration),
            format_duration(total)
        ));
        emit_line(&format!("Detailed log: {}", self.log_path.display()));
    }

    fn fail_update(&mut self, error: &anyhow::Error, impact: ServiceImpact) {
        self.finish_current("failed");
        self.fail("update", error, impact, None);
    }

    fn fail_build(&mut self, error: &anyhow::Error, impact: ServiceImpact) {
        self.finish_current("failed");
        self.fail("build", error, impact, None);
    }

    fn fail_restart(
        &mut self,
        error: &anyhow::Error,
        impact: ServiceImpact,
        restart: RestartProgress,
    ) {
        self.finish_current("failed");
        self.fail("restart", error, impact, Some(restart));
    }

    fn fail_monitor(&mut self, error: &anyhow::Error) {
        self.finish_current("failed");
        self.fail(
            "validator readiness",
            error,
            ServiceImpact::ReadinessUnknown,
            None,
        );
    }

    fn finish_current(&mut self, status: &str) {
        if let Some(stage) = self.current.take() {
            stage.finish(status);
        }
    }

    fn fail(
        &self,
        stage: &str,
        error: &anyhow::Error,
        impact: ServiceImpact,
        restart: Option<RestartProgress>,
    ) {
        if !self.compact {
            return;
        }

        emit_line("");
        emit_line(&format!("Update failed during {stage}."));
        emit_line(&format!("Error: {error:#}"));
        if let Some(restart) = restart {
            restart.write_lines(io::stdout()).ok();
        }
        match impact {
            ServiceImpact::Unchanged => {
                emit_line("Service was not stopped.");
            }
            ServiceImpact::Stopped => {
                emit_line(&format!("Service remains stopped ({}).", self.service));
            }
            ServiceImpact::FailedToRestart => {
                emit_line(&format!(
                    "Service did not become active ({}).",
                    self.service
                ));
            }
            ServiceImpact::ReadinessUnknown => {
                emit_line(&format!(
                    "Service restart completed, but validator readiness is unknown ({}).",
                    self.service
                ));
            }
        }
        emit_line(&format!("Detailed log: {}", self.log_path.display()));
    }
}

fn emit_line(line: &str) {
    let _ = writeln!(io::stdout(), "{line}");
    let _ = io::stdout().flush();
}

struct RestartProgressReporter<'a> {
    inner: &'a RefCell<RestartProgress>,
}

impl RestartReporter for RestartProgressReporter<'_> {
    fn on_phase(&self, phase: RestartPhase) {
        self.inner.borrow_mut().on_phase(phase);
    }

    fn on_configure_first_failed(&self) {
        self.inner.borrow_mut().on_configure_first_failed();
    }
}

#[derive(Debug, Clone)]
enum StageStatus {
    Pending,
    Done(String),
    Warning(String),
    Failed,
}

impl std::fmt::Display for StageStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => formatter.write_str("pending"),
            Self::Done(detail) if detail.is_empty() => formatter.write_str("done"),
            Self::Done(detail) => write!(formatter, "done ({detail})"),
            Self::Warning(detail) => write!(formatter, "warning; {detail}"),
            Self::Failed => formatter.write_str("failed"),
        }
    }
}

struct RestartProgress {
    compact: bool,
    stop: StageStatus,
    configure_first: StageStatus,
    configure_second: StageStatus,
    start: StageStatus,
    final_state: String,
}

impl RestartProgress {
    fn new(compact: bool) -> Self {
        Self {
            compact,
            stop: StageStatus::Pending,
            configure_first: StageStatus::Pending,
            configure_second: StageStatus::Pending,
            start: StageStatus::Pending,
            final_state: String::new(),
        }
    }

    fn on_phase(&mut self, phase: RestartPhase) {
        match phase {
            RestartPhase::Stop => {
                self.stop = StageStatus::Done("inactive".to_owned());
            }
            RestartPhase::ConfigureFirst => {
                self.configure_first = StageStatus::Done(String::new());
            }
            RestartPhase::ConfigureSecond => {
                self.configure_second = StageStatus::Done(String::new());
            }
            RestartPhase::Start => {
                self.start = StageStatus::Done("active".to_owned());
            }
        }
    }

    fn on_configure_first_failed(&mut self) {
        self.configure_first = StageStatus::Warning("retrying".to_owned());
    }

    fn mark_restart_failed(&mut self) {
        if matches!(self.stop, StageStatus::Pending) {
            self.stop = StageStatus::Failed;
        } else if matches!(self.configure_second, StageStatus::Pending) {
            self.configure_second = StageStatus::Failed;
        } else if matches!(self.start, StageStatus::Pending) {
            self.start = StageStatus::Failed;
        }
    }

    fn finalize_start_from_service(&mut self, service: &ServiceManager<'_>) {
        match service.state() {
            Ok(ServiceState::Active) => {
                if matches!(self.start, StageStatus::Pending) {
                    self.start = StageStatus::Done("active".to_owned());
                }
                self.final_state = "active".to_owned();
            }
            Ok(state) => {
                if matches!(self.start, StageStatus::Pending) {
                    self.start = StageStatus::Failed;
                }
                self.final_state = state.to_string();
            }
            Err(_) => {
                if matches!(self.start, StageStatus::Pending) {
                    self.start = StageStatus::Failed;
                }
                if self.final_state.is_empty() {
                    self.final_state = "unknown".to_owned();
                }
            }
        }
    }

    fn write_lines(&self, mut output: impl Write) -> Result<()> {
        if !self.compact {
            return Ok(());
        }
        writeln!(
            output,
            "      stop ................................. {}",
            self.stop
        )?;
        writeln!(
            output,
            "      configure 1/2 ........................ {}",
            self.configure_first
        )?;
        writeln!(
            output,
            "      configure 2/2 ........................ {}",
            self.configure_second
        )?;
        writeln!(
            output,
            "      start ................................ {}",
            self.start
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell, collections::VecDeque, fs, os::unix::fs::PermissionsExt, path::PathBuf,
        sync::Mutex,
    };

    use anyhow::{Result, anyhow};
    use tempfile::TempDir;

    use super::{
        RestartProgress, ServiceImpact, classify_restart_failure, run_update_full, update_full,
    };
    use crate::{
        paths::AppPaths,
        process::{CommandOutcome, CommandSpec, Runner},
        service::ServiceManager,
    };

    struct FakeRunner {
        repository: PathBuf,
        captures: Mutex<VecDeque<CommandOutcome>>,
        streaming: Mutex<VecDeque<CommandOutcome>>,
        interactive: Mutex<VecDeque<CommandOutcome>>,
    }

    impl FakeRunner {
        fn new(
            repository: PathBuf,
            captures: Vec<CommandOutcome>,
            streaming: Vec<CommandOutcome>,
            interactive: Vec<CommandOutcome>,
        ) -> Self {
            Self {
                repository,
                captures: Mutex::new(VecDeque::from(captures)),
                streaming: Mutex::new(VecDeque::from(streaming)),
                interactive: Mutex::new(VecDeque::from(interactive)),
            }
        }

        fn write_fdctl(&self) -> Result<()> {
            let fdctl = self.repository.join("build/native/gcc/bin/fdctl");
            fs::create_dir_all(fdctl.parent().expect("fdctl parent"))?;
            fs::write(&fdctl, "")?;
            let mut permissions = fs::metadata(&fdctl)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&fdctl, permissions)?;
            Ok(())
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

        fn streaming(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
            let outcome = self
                .streaming
                .lock()
                .expect("streaming lock")
                .pop_front()
                .ok_or_else(|| anyhow!("unexpected streaming command"))?;
            if outcome.success && spec.program == "make" {
                self.write_fdctl()?;
            }
            Ok(outcome)
        }

        fn interactive(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
            self.interactive
                .lock()
                .expect("interactive lock")
                .pop_front()
                .ok_or_else(|| anyhow!("unexpected interactive command: {}", spec.display()))
        }
    }

    fn prepared_paths() -> Result<(TempDir, AppPaths)> {
        let temp = TempDir::new()?;
        let base = temp.path().to_path_buf();
        let repository = base.join("firedancer");
        fs::create_dir_all(&repository)?;
        fs::write(repository.join("deps.sh"), "#!/bin/sh\n")?;
        let mut permissions = fs::metadata(repository.join("deps.sh"))?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(repository.join("deps.sh"), permissions)?;

        let fdctl = repository.join("build/native/gcc/bin/fdctl");
        fs::create_dir_all(fdctl.parent().expect("fdctl parent"))?;
        fs::write(&fdctl, "")?;
        let mut permissions = fs::metadata(&fdctl)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fdctl, permissions)?;

        let config = base.join("active-fd-config.toml");
        fs::write(&config, "")?;
        let log_dir = base.join("logs");

        Ok((
            temp,
            AppPaths {
                base,
                repository,
                config,
                log_dir,
                username: "validator".to_owned(),
            },
        ))
    }

    fn show(state: &str) -> CommandOutcome {
        CommandOutcome::success(format!("LoadState=loaded\nActiveState={state}\n"))
    }

    #[test]
    fn update_full_runs_monitor_after_restart() -> Result<()> {
        let (_temp, paths) = prepared_paths()?;
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        let runner = FakeRunner::new(
            paths.repository.clone(),
            vec![
                CommandOutcome::success(paths.repository.display().to_string()),
                CommandOutcome::failure(128, "tag not found"),
                CommandOutcome::failure(128, "branch not found"),
                CommandOutcome::success(commit),
                CommandOutcome::success(commit),
                CommandOutcome::success(""),
                CommandOutcome::success(paths.repository.display().to_string()),
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
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ],
            vec![
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        let monitored = Cell::new(false);
        run_update_full(
            &runner,
            &service,
            &paths,
            "v1.2.3",
            "frankendancer.service",
            false,
            &|config| {
                assert_eq!(config, paths.config);
                monitored.set(true);
                Ok(())
            },
        )?;

        assert!(monitored.get(), "monitor should run after restart");
        assert!(runner.streaming.lock().expect("stream lock").is_empty());
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
    fn update_full_does_not_restart_when_build_fails() -> Result<()> {
        let (_temp, paths) = prepared_paths()?;
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        let runner = FakeRunner::new(
            paths.repository.clone(),
            vec![
                CommandOutcome::success(paths.repository.display().to_string()),
                CommandOutcome::failure(128, "tag not found"),
                CommandOutcome::failure(128, "branch not found"),
                CommandOutcome::success(commit),
                CommandOutcome::success(commit),
                CommandOutcome::success(""),
                CommandOutcome::success(paths.repository.display().to_string()),
            ],
            vec![
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::failure(2, "make failed"),
            ],
            vec![],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        let monitored = Cell::new(false);
        let error = run_update_full(
            &runner,
            &service,
            &paths,
            "v1.2.3",
            "frankendancer.service",
            false,
            &|_| {
                monitored.set(true);
                Ok(())
            },
        )
        .expect_err("build failure should abort");
        assert!(format!("{error:#}").contains("Firedancer build failed"));
        assert!(!monitored.get(), "monitor must not run after build failure");
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
    fn update_full_rejects_disabled_gui_before_making_changes() -> Result<()> {
        let (_temp, paths) = prepared_paths()?;
        fs::write(&paths.config, "[tiles.gui]\nenabled = false\n")?;
        let runner = FakeRunner::new(paths.repository.clone(), vec![], vec![], vec![]);
        let service = ServiceManager::new(&runner, "frankendancer.service", false);

        let error = update_full(
            &runner,
            &service,
            &paths,
            "v1.2.3",
            "frankendancer.service",
            false,
        )
        .expect_err("disabled GUI should stop update-full before checkout");

        assert!(
            format!("{error:#}").contains("GUI is disabled"),
            "{error:#}"
        );
        assert!(runner.captures.lock().expect("capture lock").is_empty());
        assert!(runner.streaming.lock().expect("stream lock").is_empty());
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
    fn classify_restart_failure_when_service_still_active() -> Result<()> {
        let runner = FakeRunner::new(
            PathBuf::from("/unused"),
            vec![show("active")],
            vec![],
            vec![],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);
        assert!(matches!(
            classify_restart_failure(&service),
            ServiceImpact::Unchanged
        ));
        Ok(())
    }

    #[test]
    fn classify_restart_failure_when_service_is_inactive() -> Result<()> {
        let runner = FakeRunner::new(
            PathBuf::from("/unused"),
            vec![show("inactive")],
            vec![],
            vec![],
        );
        let service = ServiceManager::new(&runner, "frankendancer.service", false);
        assert!(matches!(
            classify_restart_failure(&service),
            ServiceImpact::Stopped
        ));
        Ok(())
    }

    #[test]
    fn mark_restart_failed_marks_configure_second_after_stop() -> Result<()> {
        let mut progress = RestartProgress::new(true);
        progress.on_phase(super::RestartPhase::Stop);
        progress.on_configure_first_failed();
        progress.mark_restart_failed();
        let mut output = Vec::new();
        progress.write_lines(&mut output)?;
        let lines = String::from_utf8(output)?;
        assert!(lines.contains("configure 2/2 ........................ failed"));
        Ok(())
    }
}
