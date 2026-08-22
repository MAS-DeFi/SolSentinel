//! Cross-process command serialization.

use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

pub struct CommandLock {
    _file: File,
}

impl CommandLock {
    /// Acquires the validator user's exclusive val command lock.
    pub fn acquire(base_path: &Path) -> Result<Self> {
        let metadata = fs::metadata(base_path)
            .with_context(|| format!("base path does not exist: {}", base_path.display()))?;
        if !metadata.is_dir() {
            bail!("base path is not a directory: {}", base_path.display());
        }
        let path = base_path.join(".val.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("could not open command lock {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not secure command lock {}", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { _file: file }),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                bail!("another val command is already running")
            }
            Err(error) => Err(error)
                .with_context(|| format!("could not acquire command lock {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tempfile::TempDir;

    use super::CommandLock;

    #[test]
    fn permits_only_one_command_at_a_time() -> Result<()> {
        let temp = TempDir::new()?;
        let first = CommandLock::acquire(temp.path())?;
        assert!(CommandLock::acquire(temp.path()).is_err());
        drop(first);
        CommandLock::acquire(temp.path())?;
        Ok(())
    }
}
