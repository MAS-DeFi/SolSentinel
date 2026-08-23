//! Firedancer repository update and build operations.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::process::{CommandSpec, Runner, executable_in, require_success};

/// Updates the Firedancer checkout and installs its dependencies.
pub fn update_firedancer(runner: &dyn Runner, repository: &Path, git_ref: &str) -> Result<()> {
    validate_git_ref(git_ref)?;
    validate_repository(runner, repository)?;
    ensure_clean_worktree(runner, repository)?;

    info!(reference = git_ref, repository = %repository.display(), "fetching Firedancer");
    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["fetch", "--tags", "origin"])
                .cwd(repository),
        )?,
        "git fetch",
    )?;

    let commit = resolve_git_ref(runner, repository, git_ref)?;

    let current = runner.capture(
        &CommandSpec::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .cwd(repository),
    )?;
    if current.success && current.stdout.trim() == commit {
        info!(commit, "repository is already at the requested commit");
    }

    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["checkout", "--detach"])
                .arg(&commit)
                .cwd(repository),
        )?,
        "git checkout",
    )?;
    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["submodule", "update", "--init", "--recursive"])
                .cwd(repository),
        )?,
        "git submodule update",
    )?;

    let deps = executable_in(repository, "deps.sh");
    if !deps.is_file() {
        bail!("dependency installer does not exist: {}", deps.display());
    }

    info!("running Firedancer dependency installer; it may request confirmation");
    require_success(
        runner.interactive(&CommandSpec::new(deps).cwd(repository))?,
        "Firedancer deps.sh",
    )?;

    info!(reference = git_ref, commit, "Firedancer update completed");
    Ok(())
}

/// Builds Firedancer and returns the elapsed duration.
pub fn make_firedancer(runner: &dyn Runner, repository: &Path) -> Result<Duration> {
    validate_repository(runner, repository)?;

    let started = Instant::now();
    info!(repository = %repository.display(), "building Firedancer");
    let outcome = runner.streaming(
        &CommandSpec::new("make")
            .args(["-j", "fdctl", "solana"])
            .cwd(repository),
    )?;
    let duration = started.elapsed();

    if !outcome.success {
        warn!(
            elapsed_seconds = duration.as_secs(),
            result = %outcome.exit_description(),
            "Firedancer build failed"
        );
    }
    require_success(outcome, "Firedancer build")?;
    info!(
        elapsed_seconds = duration.as_secs(),
        "Firedancer build completed"
    );
    Ok(duration)
}

/// Reads the version reported by the fdctl binary in the current checkout.
pub fn built_fdctl_version(runner: &dyn Runner, repository: &Path) -> Result<Option<String>> {
    let fdctl = executable_in(repository, "build/native/gcc/bin/fdctl");
    let metadata = match fs::metadata(&fdctl) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not inspect built fdctl: {}", fdctl.display()));
        }
    };
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("built fdctl is not executable: {}", fdctl.display());
    }

    let outcome = require_success(
        runner.capture(&CommandSpec::new(&fdctl).arg("version").cwd(repository))?,
        "querying the built fdctl version",
    )?;
    let version = outcome.stdout.trim();
    if version.is_empty() {
        bail!("built fdctl returned an empty version");
    }
    Ok(Some(version.to_owned()))
}

/// Verifies that the configured path is a Git working tree.
fn validate_repository(runner: &dyn Runner, repository: &Path) -> Result<()> {
    let metadata = fs::metadata(repository)
        .with_context(|| format!("Firedancer repository not found: {}", repository.display()))?;
    if !metadata.is_dir() {
        bail!(
            "Firedancer repository path is not a directory: {}",
            repository.display()
        );
    }

    let outcome = runner.capture(
        &CommandSpec::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .cwd(repository),
    )?;
    if !outcome.success || outcome.stdout.trim() != "true" {
        bail!("path is not a git working tree: {}", repository.display());
    }
    Ok(())
}

/// Refuses updates when the repository or submodules are dirty.
fn ensure_clean_worktree(runner: &dyn Runner, repository: &Path) -> Result<()> {
    let outcome = require_success(
        runner.capture(
            &CommandSpec::new("git")
                .args([
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=normal",
                    "--ignore-submodules=none",
                ])
                .cwd(repository),
        )?,
        "checking Firedancer working tree",
    )?;
    if !outcome.stdout.trim().is_empty() {
        bail!(
            "Firedancer working tree has tracked or untracked changes; commit, stash, or remove them before updating"
        );
    }
    Ok(())
}

/// Resolves a tag, fetched origin branch, or commit to a full commit ID.
fn resolve_git_ref(runner: &dyn Runner, repository: &Path, git_ref: &str) -> Result<String> {
    let mut candidates = Vec::new();
    if git_ref.starts_with("refs/")
        || (git_ref.len() >= 7 && git_ref.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        candidates.push(git_ref.to_owned());
    } else {
        candidates.push(format!("refs/tags/{git_ref}"));
        candidates.push(format!("refs/remotes/origin/{git_ref}"));
        candidates.push(git_ref.to_owned());
    }

    for candidate in candidates {
        let revision = format!("{candidate}^{{commit}}");
        let outcome = runner.capture(
            &CommandSpec::new("git")
                .args(["rev-parse", "--verify", "--end-of-options"])
                .arg(&revision)
                .cwd(repository),
        )?;
        if !outcome.success {
            continue;
        }

        let commit = outcome.stdout.trim();
        if !(40..=64).contains(&commit.len())
            || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("git ref '{git_ref}' resolved to an invalid commit id");
        }
        return Ok(commit.to_owned());
    }

    bail!("git ref '{git_ref}' was not found after fetching origin")
}

/// Rejects empty, option-like, or control-character Git refs.
fn validate_git_ref(git_ref: &str) -> Result<()> {
    if git_ref.is_empty() {
        bail!("git ref cannot be empty");
    }
    if git_ref.starts_with('-') {
        bail!("git ref cannot begin with '-'");
    }
    if git_ref.chars().any(char::is_control) {
        bail!("git ref cannot contain control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, fs, os::unix::fs::PermissionsExt, sync::Mutex};

    use anyhow::{Result, bail};
    use tempfile::TempDir;

    use super::{built_fdctl_version, resolve_git_ref, update_firedancer, validate_git_ref};
    use crate::process::{CommandOutcome, CommandSpec, Runner};

    struct FakeRunner {
        captures: Mutex<VecDeque<CommandOutcome>>,
        capture_specs: Mutex<Vec<CommandSpec>>,
        streaming: Mutex<VecDeque<CommandOutcome>>,
        interactive: Mutex<VecDeque<CommandOutcome>>,
    }

    impl Runner for FakeRunner {
        fn capture(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
            self.capture_specs
                .lock()
                .expect("capture specs lock")
                .push(spec.clone());
            self.captures
                .lock()
                .expect("capture lock")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected capture"))
        }

        fn streaming(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            self.streaming
                .lock()
                .expect("streaming lock")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected streaming command"))
        }

        fn interactive(&self, _: &CommandSpec) -> Result<CommandOutcome> {
            self.interactive
                .lock()
                .expect("interactive lock")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected interactive command"))
        }
    }

    #[test]
    fn rejects_option_like_and_control_character_refs() {
        assert!(validate_git_ref("--force").is_err());
        assert!(validate_git_ref("release\nmain").is_err());
        assert!(validate_git_ref("v1.2.3").is_ok());
    }

    #[test]
    fn update_stops_before_fetching_a_dirty_worktree() -> Result<()> {
        let repository = TempDir::new()?;
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("true\n"),
                CommandOutcome::success("?? local-file\n"),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        let error = update_firedancer(&runner, repository.path(), "v1.2.3")
            .expect_err("dirty worktree must fail");
        assert!(error.to_string().contains("working tree has"));
        assert!(runner.streaming.lock().expect("stream lock").is_empty());
        Ok(())
    }

    #[test]
    fn missing_deps_script_fails_after_checkout() -> Result<()> {
        let repository = TempDir::new()?;
        fs::write(repository.path().join("placeholder"), "")?;
        let commit = "0123456789012345678901234567890123456789\n";
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("true\n"),
                CommandOutcome::success(""),
                CommandOutcome::success(commit),
                CommandOutcome::failure(128, "no HEAD"),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::from([
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ])),
            interactive: Mutex::new(VecDeque::new()),
        };

        let error = update_firedancer(&runner, repository.path(), "v1.2.3")
            .expect_err("missing deps.sh must fail");
        if !error.to_string().contains("dependency installer") {
            bail!("unexpected error: {error:#}");
        }
        Ok(())
    }

    #[test]
    fn branch_resolution_prefers_the_fetched_origin_branch() -> Result<()> {
        let repository = TempDir::new()?;
        let commit = "fedcba9876543210fedcba9876543210fedcba98\n";
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::failure(128, "tag not found"),
                CommandOutcome::success(commit),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        assert_eq!(
            resolve_git_ref(&runner, repository.path(), "main")?,
            commit.trim()
        );
        Ok(())
    }

    #[test]
    fn reads_version_from_the_built_fdctl() -> Result<()> {
        let repository = TempDir::new()?;
        let fdctl = repository.path().join("build/native/gcc/bin/fdctl");
        fs::create_dir_all(fdctl.parent().expect("fdctl parent"))?;
        fs::write(&fdctl, "")?;
        let mut permissions = fs::metadata(&fdctl)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fdctl, permissions)?;

        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([CommandOutcome::success("v2.0.0\n")])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        assert_eq!(
            built_fdctl_version(&runner, repository.path())?.as_deref(),
            Some("v2.0.0")
        );
        let captures = runner.capture_specs.lock().expect("capture specs lock");
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].program, fdctl.as_os_str());
        assert_eq!(captures[0].args, ["version"]);
        assert_eq!(captures[0].cwd.as_deref(), Some(repository.path()));
        Ok(())
    }
}
