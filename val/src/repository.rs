//! Firedancer repository update and build operations.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::process::{CommandOutcome, CommandSpec, Runner, executable_in, require_success};

/// Updates the Firedancer checkout and installs its dependencies.
pub fn update_firedancer(runner: &dyn Runner, repository: &Path, git_ref: &str) -> Result<()> {
    validate_git_ref(git_ref)?;
    validate_repository(runner, repository)?;

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

    // Discard leftover dirt only after the requested ref is known to exist so a
    // fetch or resolve failure does not wipe the checkout.
    reset_managed_worktree(runner, repository)?;

    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["checkout", "--force", "--detach"])
                .arg(&commit)
                .cwd(repository),
        )?,
        "git checkout",
    )?;
    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["submodule", "sync", "--recursive"])
                .cwd(repository),
        )?,
        "git submodule sync",
    )?;
    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args([
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                    "--force",
                    "--checkout",
                ])
                .cwd(repository),
        )?,
        "git submodule update",
    )?;

    let deps = executable_in(repository, "deps.sh");
    if !deps.is_file() {
        bail!("dependency installer does not exist: {}", deps.display());
    }

    // No-args deps.sh prompts "Continue? (y/N)". Passing the default actions
    // skips that prompt. FD_AUTO_INSTALL_PACKAGES answers the later package
    // and rustup prompts so an unattended update does not block.
    info!("running Firedancer dependency installer");
    require_success(
        runner.streaming(
            &CommandSpec::new(deps)
                .args(["fetch", "check", "install"])
                .env("FD_AUTO_INSTALL_PACKAGES", "1")
                .cwd(repository),
        )?,
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

/// Discards leftover checkout dirt so a managed update can switch refs.
///
/// `~/code/firedancer` is a deployment artifact, not a developer worktree.
/// A previous `deps.sh` or `make` commonly leaves the `agave` submodule with
/// modified or untracked files, and a failed or partial update can leave the
/// submodule pin behind HEAD. Autonomous updates cannot stop for stash or
/// commit, so those changes are logged and thrown away. Ignored outputs such
/// as `build/` and `opt/` are kept.
fn reset_managed_worktree(runner: &dyn Runner, repository: &Path) -> Result<()> {
    let status = worktree_status(runner, repository)?;
    if status.trim().is_empty() {
        return Ok(());
    }

    warn!(
        status = %status.trim(),
        "discarding leftover Firedancer checkout changes before update"
    );
    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["reset", "--hard", "HEAD"])
                .cwd(repository),
        )?,
        "git reset",
    )?;
    require_success(
        runner.streaming(
            &CommandSpec::new("git")
                .args(["clean", "-ffd"])
                .cwd(repository),
        )?,
        "git clean",
    )?;
    // A broken or uninitialized submodule must not abort the update: the later
    // `submodule update --force --init` is what repairs checkout state.
    continue_after_failure(
        runner.streaming(
            &CommandSpec::new("git")
                .args([
                    "submodule",
                    "foreach",
                    "--recursive",
                    "git",
                    "reset",
                    "--hard",
                ])
                .cwd(repository),
        )?,
        "git submodule reset",
    );
    continue_after_failure(
        runner.streaming(
            &CommandSpec::new("git")
                .args([
                    "submodule",
                    "foreach",
                    "--recursive",
                    "git",
                    "clean",
                    "-ffd",
                ])
                .cwd(repository),
        )?,
        "git submodule clean",
    );
    Ok(())
}

/// Logs a failed git step and continues. Execution errors still propagate.
fn continue_after_failure(outcome: CommandOutcome, description: &str) {
    if !outcome.success {
        warn!(
            command = description,
            result = %outcome.exit_description(),
            "continuing Firedancer update after git command failed"
        );
    }
}

/// Returns porcelain status for the superproject and submodules.
fn worktree_status(runner: &dyn Runner, repository: &Path) -> Result<String> {
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
    Ok(outcome.stdout)
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
    use std::{
        collections::VecDeque, fs, os::unix::fs::PermissionsExt, path::Path, process::Command,
        sync::Mutex,
    };

    use anyhow::{Context, Result, bail};
    use tempfile::TempDir;

    use super::{
        built_fdctl_version, reset_managed_worktree, resolve_git_ref, update_firedancer,
        validate_git_ref,
    };
    use crate::process::{CommandOutcome, CommandSpec, Runner, SystemRunner};

    struct FakeRunner {
        captures: Mutex<VecDeque<CommandOutcome>>,
        capture_specs: Mutex<Vec<CommandSpec>>,
        streaming: Mutex<VecDeque<CommandOutcome>>,
        streaming_specs: Mutex<Vec<CommandSpec>>,
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

        fn streaming(&self, spec: &CommandSpec) -> Result<CommandOutcome> {
            self.streaming_specs
                .lock()
                .expect("streaming specs lock")
                .push(spec.clone());
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

    fn arg_strings(spec: &CommandSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env_strings(spec: &CommandSpec) -> Vec<(String, String)> {
        spec.env
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    fn run_git(path: &Path, args: &[&str]) -> Result<String> {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_COMMON_DIR");
        let global_config = path.join(".git").join("val-test-empty-global");
        if path.join(".git").is_dir() {
            fs::write(&global_config, "")?;
            command.env("GIT_CONFIG_GLOBAL", &global_config);
        }
        let output = command
            .output()
            .with_context(|| format!("execute git {}", args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn init_git_repo(path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        run_git(path, &["init", "--quiet"])?;
        run_git(path, &["config", "user.email", "val-test@example.com"])?;
        run_git(path, &["config", "user.name", "val test"])?;
        run_git(path, &["config", "commit.gpgsign", "false"])?;
        fs::write(path.join("README"), "initial\n")?;
        fs::write(path.join(".gitignore"), "/build\n")?;
        run_git(path, &["add", "README", ".gitignore"])?;
        run_git(path, &["commit", "--quiet", "-m", "initial"])?;
        Ok(())
    }

    fn porcelain(path: &Path) -> Result<String> {
        run_git(
            path,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        )
    }

    #[test]
    fn rejects_option_like_and_control_character_refs() {
        assert!(validate_git_ref("--force").is_err());
        assert!(validate_git_ref("release\nmain").is_err());
        assert!(validate_git_ref("v1.2.3").is_ok());
    }

    #[test]
    fn update_does_not_discard_changes_when_the_ref_is_missing() -> Result<()> {
        let repository = TempDir::new()?;
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("true\n"),
                CommandOutcome::failure(128, "tag not found"),
                CommandOutcome::failure(128, "branch not found"),
                CommandOutcome::failure(128, "ref not found"),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::from([CommandOutcome::success("")])),
            streaming_specs: Mutex::new(Vec::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        let error = update_firedancer(&runner, repository.path(), "v1.2.3")
            .expect_err("missing ref must fail");
        if !error.to_string().contains("was not found") {
            bail!("unexpected error: {error:#}");
        }
        let specs = runner.streaming_specs.lock().expect("streaming specs lock");
        assert_eq!(specs.len(), 1);
        assert_eq!(arg_strings(&specs[0]), ["fetch", "--tags", "origin"]);
        Ok(())
    }

    #[test]
    fn update_discards_a_dirty_worktree_before_checkout() -> Result<()> {
        let repository = TempDir::new()?;
        let commit = "0123456789012345678901234567890123456789\n";
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("true\n"),
                CommandOutcome::success(commit),
                CommandOutcome::failure(128, "no HEAD"),
                CommandOutcome::success(" m agave\n?? local-file\n"),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::from(vec![CommandOutcome::success(""); 8])),
            streaming_specs: Mutex::new(Vec::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        let error = update_firedancer(&runner, repository.path(), "v1.2.3")
            .expect_err("missing deps.sh must fail after reset");
        if !error.to_string().contains("dependency installer") {
            bail!("unexpected error: {error:#}");
        }

        let specs = runner.streaming_specs.lock().expect("streaming specs lock");
        assert_eq!(arg_strings(&specs[0]), ["fetch", "--tags", "origin"]);
        assert_eq!(arg_strings(&specs[1]), ["reset", "--hard", "HEAD"]);
        assert_eq!(arg_strings(&specs[2]), ["clean", "-ffd"]);
        assert_eq!(
            arg_strings(&specs[3]),
            [
                "submodule",
                "foreach",
                "--recursive",
                "git",
                "reset",
                "--hard"
            ]
        );
        assert_eq!(
            arg_strings(&specs[4]),
            [
                "submodule",
                "foreach",
                "--recursive",
                "git",
                "clean",
                "-ffd"
            ]
        );
        assert_eq!(
            arg_strings(&specs[5]),
            [
                "checkout",
                "--force",
                "--detach",
                "0123456789012345678901234567890123456789"
            ]
        );
        Ok(())
    }

    #[test]
    fn update_continues_when_submodule_cleanup_fails() -> Result<()> {
        let repository = TempDir::new()?;
        let commit = "0123456789012345678901234567890123456789\n";
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("true\n"),
                CommandOutcome::success(commit),
                CommandOutcome::failure(128, "no HEAD"),
                CommandOutcome::success(" m agave\n"),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::from([
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::failure(1, "foreach reset failed"),
                CommandOutcome::failure(1, "foreach clean failed"),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ])),
            streaming_specs: Mutex::new(Vec::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        let error = update_firedancer(&runner, repository.path(), "v1.2.3")
            .expect_err("missing deps.sh must fail after continuing");
        if !error.to_string().contains("dependency installer") {
            bail!("unexpected error: {error:#}");
        }
        let specs = runner.streaming_specs.lock().expect("streaming specs lock");
        assert_eq!(
            arg_strings(&specs[5]),
            [
                "checkout",
                "--force",
                "--detach",
                "0123456789012345678901234567890123456789"
            ]
        );
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
                CommandOutcome::success(commit),
                CommandOutcome::failure(128, "no HEAD"),
                CommandOutcome::success(""),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::from([
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
                CommandOutcome::success(""),
            ])),
            streaming_specs: Mutex::new(Vec::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        let error = update_firedancer(&runner, repository.path(), "v1.2.3")
            .expect_err("missing deps.sh must fail");
        if !error.to_string().contains("dependency installer") {
            bail!("unexpected error: {error:#}");
        }
        let specs = runner.streaming_specs.lock().expect("streaming specs lock");
        assert_eq!(arg_strings(&specs[0]), ["fetch", "--tags", "origin"]);
        assert_eq!(
            arg_strings(&specs[1]),
            [
                "checkout",
                "--force",
                "--detach",
                "0123456789012345678901234567890123456789"
            ]
        );
        assert_eq!(arg_strings(&specs[2]), ["submodule", "sync", "--recursive"]);
        assert_eq!(
            arg_strings(&specs[3]),
            [
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--force",
                "--checkout"
            ]
        );
        Ok(())
    }

    #[test]
    fn update_runs_deps_sh_without_confirmation_prompts() -> Result<()> {
        let repository = TempDir::new()?;
        fs::write(repository.path().join("deps.sh"), "#!/bin/sh\n")?;
        let commit = "0123456789012345678901234567890123456789\n";
        let runner = FakeRunner {
            captures: Mutex::new(VecDeque::from([
                CommandOutcome::success("true\n"),
                CommandOutcome::success(commit),
                CommandOutcome::failure(128, "no HEAD"),
                CommandOutcome::success(""),
            ])),
            capture_specs: Mutex::new(Vec::new()),
            streaming: Mutex::new(VecDeque::from(vec![CommandOutcome::success(""); 5])),
            streaming_specs: Mutex::new(Vec::new()),
            interactive: Mutex::new(VecDeque::new()),
        };

        update_firedancer(&runner, repository.path(), "v1.2.3")?;

        let specs = runner.streaming_specs.lock().expect("streaming specs lock");
        assert_eq!(specs.len(), 5);
        assert_eq!(
            specs[4].program,
            repository.path().join("deps.sh").as_os_str()
        );
        assert_eq!(arg_strings(&specs[4]), ["fetch", "check", "install"]);
        assert_eq!(
            env_strings(&specs[4]),
            [("FD_AUTO_INSTALL_PACKAGES".to_owned(), "1".to_owned())]
        );
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
    fn reset_discards_tracked_and_untracked_superproject_files() -> Result<()> {
        let repository = TempDir::new()?;
        init_git_repo(repository.path())?;
        fs::write(repository.path().join("README"), "local edit\n")?;
        fs::write(repository.path().join("scratch.txt"), "untracked\n")?;
        fs::create_dir_all(repository.path().join("build"))?;
        fs::write(repository.path().join("build/artifact"), "keep\n")?;

        reset_managed_worktree(&SystemRunner, repository.path())?;

        assert_eq!(
            fs::read_to_string(repository.path().join("README"))?,
            "initial\n"
        );
        assert!(!repository.path().join("scratch.txt").exists());
        assert_eq!(
            fs::read_to_string(repository.path().join("build/artifact"))?,
            "keep\n"
        );
        assert!(porcelain(repository.path())?.trim().is_empty());
        Ok(())
    }

    #[test]
    fn reset_discards_dirty_submodule_content() -> Result<()> {
        let root = TempDir::new()?;
        let parent = root.path().join("parent");
        let child = root.path().join("child");
        init_git_repo(&child)?;
        init_git_repo(&parent)?;
        run_git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                child.to_str().context("child path")?,
                "agave",
            ],
        )?;
        run_git(&parent, &["commit", "--quiet", "-m", "add agave"])?;
        fs::write(parent.join("agave/README"), "submodule edit\n")?;
        fs::write(parent.join("agave/scratch.txt"), "untracked\n")?;

        reset_managed_worktree(&SystemRunner, &parent)?;

        assert_eq!(
            fs::read_to_string(parent.join("agave/README"))?,
            "initial\n"
        );
        assert!(!parent.join("agave/scratch.txt").exists());
        assert!(porcelain(&parent)?.trim().is_empty());
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
            streaming_specs: Mutex::new(Vec::new()),
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
            streaming_specs: Mutex::new(Vec::new()),
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
