//! Secure terminal and rotating-file logging.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing::Level;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    EnvFilter, Layer, filter::filter_fn, fmt, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::process::COMMAND_OUTPUT_TARGET;

const MAX_LOG_BYTES: u64 = 25 * 1024 * 1024;
const LOG_BACKUPS: usize = 5;

/// Initializes terminal and bounded file logging.
pub fn init(log_dir: &Path, verbosity: u8, compact_terminal: bool) -> Result<WorkerGuard> {
    fs::create_dir_all(log_dir)
        .with_context(|| format!("could not create log directory {}", log_dir.display()))?;

    let file_appender =
        SecureRotatingFile::new(log_dir.join("val.log"), MAX_LOG_BYTES, LOG_BACKUPS)
            .context("could not initialize secure command log")?;
    let (file_writer, guard) = NonBlockingBuilder::default()
        .lossy(false)
        .finish(file_appender);

    let default_filter = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let terminal_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_filter(filter_fn(move |metadata| {
            terminal_event_enabled(metadata.target(), metadata.level(), compact_terminal)
        }));
    let file_layer = fmt::layer().with_ansi(false).with_writer(file_writer);

    tracing_subscriber::registry()
        .with(filter)
        .with(terminal_layer)
        .with(file_layer)
        .try_init()
        .context("could not initialize logging")?;

    Ok(guard)
}

/// Returns whether a tracing event should appear on the operator terminal.
fn terminal_event_enabled(target: &str, level: &Level, compact_terminal: bool) -> bool {
    if target == COMMAND_OUTPUT_TARGET {
        return false;
    }
    if compact_terminal {
        // tracing::Level is ordered TRACE > DEBUG > INFO > WARN > ERROR.
        *level <= Level::WARN
    } else {
        true
    }
}

struct SecureRotatingFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    max_bytes: u64,
    backups: usize,
}

impl SecureRotatingFile {
    /// Opens a secure bounded log writer.
    fn new(path: PathBuf, max_bytes: u64, backups: usize) -> io::Result<Self> {
        let file = open_secure_append(&path)?;
        let bytes = file.metadata()?.len();
        let mut writer = Self {
            path,
            file: Some(file),
            bytes,
            max_bytes,
            backups,
        };
        if writer.bytes >= writer.max_bytes {
            writer.rotate()?;
        }
        Ok(writer)
    }

    /// Rotates the active log and prunes the oldest backup.
    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        if self.backups > 0 {
            remove_if_exists(&self.backup_path(self.backups))?;
            for index in (1..self.backups).rev() {
                rename_if_exists(&self.backup_path(index), &self.backup_path(index + 1))?;
            }
            rename_if_exists(&self.path, &self.backup_path(1))?;
        } else {
            remove_if_exists(&self.path)?;
        }

        self.file = Some(open_secure_append(&self.path)?);
        self.bytes = 0;
        Ok(())
    }

    /// Returns the numbered backup path.
    fn backup_path(&self, index: usize) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{index}"));
        self.path.with_file_name(name)
    }
}

impl Write for SecureRotatingFile {
    /// Writes bytes, rotating before the configured limit is exceeded.
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes > 0 && self.bytes.saturating_add(buffer.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        let written = self
            .file
            .as_mut()
            .expect("secure log file is always open outside rotation")
            .write(buffer)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    /// Flushes the active log file.
    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("secure log file is always open outside rotation")
            .flush()
    }
}

/// Opens an append-only log with mode 0600.
fn open_secure_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Removes a file while tolerating a missing path.
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Renames a file while tolerating a missing source.
fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};

    use anyhow::Result;
    use tempfile::TempDir;

    use super::{SecureRotatingFile, terminal_event_enabled};
    use crate::process::COMMAND_OUTPUT_TARGET;
    use tracing::Level;

    #[test]
    fn rotates_at_size_limit_and_secures_permissions() -> Result<()> {
        let temp = TempDir::new()?;
        let path = temp.path().join("val.log");
        let mut log = SecureRotatingFile::new(path.clone(), 8, 2)?;
        log.write_all(b"12345678")?;
        log.write_all(b"next")?;
        log.flush()?;

        assert_eq!(fs::read_to_string(&path)?, "next");
        assert_eq!(
            fs::read_to_string(temp.path().join("val.log.1"))?,
            "12345678"
        );
        assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn compact_terminal_hides_info_and_command_output() {
        assert!(!terminal_event_enabled(
            "val::repository",
            &Level::INFO,
            true
        ));
        assert!(terminal_event_enabled(
            "val::repository",
            &Level::WARN,
            true
        ));
        assert!(terminal_event_enabled(
            "val::repository",
            &Level::ERROR,
            true
        ));
        assert!(!terminal_event_enabled(
            COMMAND_OUTPUT_TARGET,
            &Level::INFO,
            true
        ));
        assert!(terminal_event_enabled(
            "val::repository",
            &Level::INFO,
            false
        ));
        assert!(!terminal_event_enabled(
            COMMAND_OUTPUT_TARGET,
            &Level::INFO,
            false
        ));
    }
}
