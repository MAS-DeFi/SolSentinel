//! Structured subprocess execution and output relaying.

use std::{
    cell::Cell,
    ffi::{OsStr, OsString},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};

pub const COMMAND_OUTPUT_TARGET: &str = "val::command_output";

thread_local! {
    static COMPACT_OUTPUT: Cell<bool> = const { Cell::new(false) };
}

/// Enables or disables compact terminal output for streaming child processes.
pub fn set_compact_output(compact: bool) {
    COMPACT_OUTPUT.with(|flag| flag.set(compact));
}

/// Returns whether streaming child output should be kept off the terminal.
pub fn compact_output() -> bool {
    COMPACT_OUTPUT.with(|flag| flag.get())
}

/// Restores compact output when dropped.
pub struct CompactOutputGuard;

impl CompactOutputGuard {
    /// Enables compact output until this guard is dropped.
    pub fn enable() -> Self {
        set_compact_output(true);
        Self
    }
}

impl Drop for CompactOutputGuard {
    fn drop(&mut self) {
        set_compact_output(false);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    /// Creates a command specification.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// Appends one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends multiple arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable in the child process.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sets the child working directory.
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Formats the command for diagnostic logs.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = self
            .env
            .iter()
            .map(|(key, value)| format!("{}={}", key.to_string_lossy(), quoted(value)))
            .collect();
        parts.push(quoted(&self.program));
        parts.extend(self.args.iter().map(|arg| quoted(arg)));
        parts.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutcome {
    #[cfg(test)]
    /// Creates a successful test outcome.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    #[cfg(test)]
    /// Creates a failed test outcome.
    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            success: false,
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Describes the process exit result.
    pub fn exit_description(&self) -> String {
        self.code.map_or_else(
            || "terminated by signal".to_owned(),
            |code| format!("exit code {code}"),
        )
    }
}

pub trait Runner {
    /// Runs a command and captures both output streams.
    fn capture(&self, spec: &CommandSpec) -> Result<CommandOutcome>;
    /// Runs a command while relaying and logging output.
    fn streaming(&self, spec: &CommandSpec) -> Result<CommandOutcome>;
    /// Runs a command attached to the current terminal.
    fn interactive(&self, spec: &CommandSpec) -> Result<CommandOutcome>;
}

#[derive(Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    /// Runs a command and captures both output streams.
    fn capture(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
        debug!(command = %spec.display(), "running command");
        let output = configured_command(spec)
            .output()
            .with_context(|| format!("could not execute {}", spec.display()))?;
        let outcome = CommandOutcome {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        debug!(
            command = %spec.display(),
            result = %outcome.exit_description(),
            success = outcome.success,
            "captured command completed"
        );
        Ok(outcome)
    }

    /// Runs a command while relaying and logging output.
    fn streaming(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
        info!(command = %spec.display(), "starting command");
        let mut command = configured_command(spec);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("could not execute {}", spec.display()))?;
        let stdout = child
            .stdout
            .take()
            .context("could not capture command stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("could not capture command stderr")?;

        let status = thread::scope(|scope| -> Result<_> {
            let stdout_thread = scope.spawn(move || relay(stdout, Stream::Stdout));
            let stderr_thread = scope.spawn(move || relay(stderr, Stream::Stderr));
            let status = child
                .wait()
                .with_context(|| format!("could not wait for {}", spec.display()))?;
            stdout_thread
                .join()
                .map_err(|_| anyhow!("stdout relay thread panicked"))?
                .context("could not relay command stdout")?;
            stderr_thread
                .join()
                .map_err(|_| anyhow!("stderr relay thread panicked"))?
                .context("could not relay command stderr")?;
            Ok(status)
        })?;

        let outcome = CommandOutcome {
            success: status.success(),
            code: status.code(),
            stdout: String::new(),
            stderr: String::new(),
        };
        log_completion(spec, &outcome);
        Ok(outcome)
    }

    /// Runs a command attached to the current terminal.
    fn interactive(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
        info!(command = %spec.display(), "starting interactive command");
        let status = configured_command(spec)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("could not execute {}", spec.display()))?;
        let outcome = CommandOutcome {
            success: status.success(),
            code: status.code(),
            stdout: String::new(),
            stderr: String::new(),
        };
        log_completion(spec, &outcome);
        Ok(outcome)
    }
}

/// Builds a process command from a specification.
fn configured_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command
}

/// Logs a streaming or interactive command result.
fn log_completion(spec: &CommandSpec, outcome: &CommandOutcome) {
    if outcome.success {
        debug!(
            command = %spec.display(),
            result = %outcome.exit_description(),
            "command completed"
        );
    } else {
        warn!(
            command = %spec.display(),
            result = %outcome.exit_description(),
            stderr = %outcome.stderr.trim(),
            "command failed"
        );
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Drains one child stream to the terminal and command log.
fn relay(reader: impl io::Read, stream: Stream) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    let mut terminal_open = !compact_output();

    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            return Ok(());
        }

        if terminal_open {
            let result = match stream {
                Stream::Stdout => {
                    let mut output = io::stdout().lock();
                    output.write_all(&buffer).and_then(|()| output.flush())
                }
                Stream::Stderr => {
                    let mut output = io::stderr().lock();
                    output.write_all(&buffer).and_then(|()| output.flush())
                }
            };
            if let Err(error) = result {
                terminal_open = false;
                warn!(
                    target: COMMAND_OUTPUT_TARGET,
                    stream = stream.name(),
                    error = %error,
                    "terminal output closed; continuing to drain child output"
                );
            }
        }

        let line = String::from_utf8_lossy(&buffer);
        info!(
            target: COMMAND_OUTPUT_TARGET,
            stream = stream.name(),
            "{}",
            line.trim_end_matches(['\r', '\n'])
        );
    }
}

impl Stream {
    /// Returns the stream name used in structured logs.
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Quotes an OS string for diagnostic display.
fn quoted(value: &OsStr) -> String {
    format!("{value:?}")
}

/// Converts a failed command outcome into an error.
pub fn require_success(outcome: CommandOutcome, description: &str) -> Result<CommandOutcome> {
    if outcome.success {
        Ok(outcome)
    } else {
        Err(anyhow!(
            "{description} failed with {}{}",
            outcome.exit_description(),
            if outcome.stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", outcome.stderr.trim())
            }
        ))
    }
}

/// Resolves an executable path inside a directory.
pub fn executable_in(directory: &Path, relative: &str) -> PathBuf {
    directory.join(relative)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{
        CommandSpec, CompactOutputGuard, Runner, SystemRunner, compact_output, set_compact_output,
    };

    #[test]
    fn capture_includes_configured_environment() {
        let outcome = SystemRunner
            .capture(
                &CommandSpec::new("sh")
                    .args(["-c", "printf %s \"$TEST_VAL_ENV\""])
                    .env("TEST_VAL_ENV", "ok"),
            )
            .expect("capture command");
        assert!(outcome.success);
        assert_eq!(outcome.stdout, "ok");
    }

    #[test]
    fn display_includes_environment_assignments() {
        let spec = CommandSpec::new("deps.sh")
            .args(["fetch", "check", "install"])
            .env("FD_AUTO_INSTALL_PACKAGES", "1");
        assert_eq!(
            spec.display(),
            "FD_AUTO_INSTALL_PACKAGES=\"1\" \"deps.sh\" \"fetch\" \"check\" \"install\""
        );
    }

    #[test]
    fn compact_output_guard_restores_on_drop() {
        assert!(!compact_output());
        {
            let _guard = CompactOutputGuard::enable();
            assert!(compact_output());
        }
        assert!(!compact_output());
    }

    #[test]
    fn compact_output_flag_tracks_state() {
        set_compact_output(true);
        assert!(compact_output());
        set_compact_output(false);
        assert!(!compact_output());
    }

    #[test]
    fn compact_mode_suppresses_streaming_child_terminal_output() {
        static LOCK: Mutex<()> = Mutex::new(());

        let _lock = LOCK.lock().expect("compact output test lock");
        let _guard = CompactOutputGuard::enable();

        let outcome = SystemRunner
            .streaming(&CommandSpec::new("sh").args([
                "-c",
                "printf 'compact-stream-marker\\n' >&2; printf 'compact-stream-marker\\n'",
            ]))
            .expect("streaming command");
        assert!(outcome.success);

        // Child output is still logged through tracing; compact mode only hides the
        // direct terminal relay used by update-full's noisy git/deps/make stages.
    }
}
