#![cfg(unix)]

use std::{env, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use clap::CommandFactory;
use clap_complete::{generate, shells::Bash};
use ed25519_dalek::SigningKey;
use serde_json::Value;
use tempfile::TempDir;

#[allow(dead_code)]
#[path = "../src/cli.rs"]
mod cli;

#[test]
fn packaged_bash_completion_matches_cli() {
    let mut generated = Vec::new();
    let mut command = cli::Cli::command();
    generate(Bash, &mut command, "val", &mut generated);

    let completion_path = format!("{}/completions/val", env!("CARGO_MANIFEST_DIR"));
    let packaged = fs::read(&completion_path).expect("packaged Bash completion");
    assert_eq!(
        generated, packaged,
        "regenerate with: cargo run --example generate-bash-completion > completions/val"
    );

    let syntax_check = Command::new("bash")
        .args(["-n", &completion_path])
        .output()
        .expect("check Bash completion syntax");
    assert!(
        syntax_check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&syntax_check.stderr)
    );
}

#[test]
fn installer_bundles_binary_and_bash_completion() {
    let temp = TempDir::new().expect("temporary directory");
    let bin_dir = temp.path().join("usr/local/bin");
    let completion_dir = temp.path().join("usr/share/bash-completion/completions");
    let installer = format!("{}/install.sh", env!("CARGO_MANIFEST_DIR"));

    let output = Command::new("sh")
        .arg(installer)
        .env("VAL_BINARY", env!("CARGO_BIN_EXE_val"))
        .env("VAL_BIN_DIR", &bin_dir)
        .env("VAL_BASH_COMPLETION_DIR", &completion_dir)
        .output()
        .expect("run val installer");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let installed_binary = bin_dir.join("val");
    let installed_completion = completion_dir.join("val");
    assert_eq!(
        fs::read(&installed_binary).expect("installed val binary"),
        fs::read(env!("CARGO_BIN_EXE_val")).expect("built val binary")
    );
    assert_eq!(
        fs::read(&installed_completion).expect("installed Bash completion"),
        fs::read(format!("{}/completions/val", env!("CARGO_MANIFEST_DIR")))
            .expect("packaged Bash completion")
    );
    assert_eq!(
        fs::metadata(installed_binary)
            .expect("installed binary metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(installed_completion)
            .expect("installed completion metadata")
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[test]
fn lifecycle_commands_use_hyphens_only() {
    let commands = [
        ("update-firedancer", "update_firedancer"),
        ("update-full", "update_full"),
        ("make-firedancer", "make_firedancer"),
        ("configure-firedancer", "configure_firedancer"),
        ("start-firedancer", "start_firedancer"),
        ("stop-firedancer", "stop_firedancer"),
        ("restart-firedancer", "restart_firedancer"),
    ];

    for (canonical, removed) in commands {
        let accepted = Command::new(env!("CARGO_BIN_EXE_val"))
            .args([canonical, "--help"])
            .output()
            .expect("run canonical command help");
        assert!(
            accepted.status.success(),
            "{canonical} stderr: {}",
            String::from_utf8_lossy(&accepted.stderr)
        );

        let rejected = Command::new(env!("CARGO_BIN_EXE_val"))
            .args([removed, "--help"])
            .output()
            .expect("run removed command help");
        assert!(
            !rejected.status.success(),
            "removed command {removed} was unexpectedly accepted"
        );
    }
}

#[test]
fn status_json_runs_end_to_end() {
    let temp = TempDir::new().expect("temporary directory");
    let bin_dir = temp.path().join("bin");
    let log_dir = temp.path().join("logs");
    fs::create_dir_all(&bin_dir).expect("fake binary directory");

    let systemctl = bin_dir.join("systemctl");
    fs::write(
        &systemctl,
        "#!/bin/sh\ncase \"$*\" in\n  *--property=MainPID*) printf '0\\n' ;;\n  *) printf 'LoadState=loaded\\nActiveState=active\\n' ;;\nesac\n",
    )
    .expect("fake systemctl");
    let mut permissions = fs::metadata(&systemctl)
        .expect("systemctl metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&systemctl, permissions).expect("systemctl permissions");

    let fdctl = temp
        .path()
        .join("code/firedancer/build/native/gcc/bin/fdctl");
    fs::create_dir_all(fdctl.parent().expect("fdctl parent")).expect("fdctl directory");
    fs::write(&fdctl, "#!/bin/sh\nprintf 'v2.0.0\\n'\n").expect("fake fdctl");
    let mut permissions = fs::metadata(&fdctl).expect("fdctl metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fdctl, permissions).expect("fdctl permissions");

    let identity = temp.path().join("active-id.json");
    let secret = [9_u8; 32];
    let public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
    let mut keypair = secret.to_vec();
    keypair.extend(public);
    fs::write(
        &identity,
        serde_json::to_vec(&keypair).expect("serialize keypair"),
    )
    .expect("identity keypair");

    let config = temp.path().join("active-fd-config.toml");
    fs::write(
        &config,
        format!(
            "[consensus]\nidentity_path = {:?}\nsnapshot_fetch = true\n",
            identity.to_string_lossy()
        ),
    )
    .expect("Firedancer config");

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_val"))
        .args([
            "--base-path",
            temp.path().to_str().expect("UTF-8 base path"),
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--log-dir",
            log_dir.to_str().expect("UTF-8 log path"),
            "status",
            "--json",
        ])
        .env("PATH", path)
        .output()
        .expect("run val status");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(status["service"], "active");
    assert!(status["running_fdctl_version"].is_null());
    assert_eq!(status["built_fdctl_version"], "v2.0.0");
    assert_eq!(status["active_id_key"], bs58::encode(public).into_string());
    assert_eq!(status["snapshot_fetch"], true);

    let log = fs::read_to_string(log_dir.join("val.log")).expect("val log");
    assert!(log.contains("val command completed"));
}

fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory");
    }
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("executable permissions");
}

fn first_line_with_word(log: &str, word: &str) -> Option<usize> {
    log.lines()
        .position(|line| line.split_whitespace().any(|token| token == word))
}

#[test]
fn restart_runs_stop_configure_configure_start() {
    let temp = TempDir::new().expect("temporary directory");
    let bin_dir = temp.path().join("bin");
    let log_dir = temp.path().join("logs");
    let state = temp.path().join("service-state");
    let systemctl_log = temp.path().join("systemctl.log");
    let fdctl_log = temp.path().join("fdctl.log");
    fs::write(&state, "active\n").expect("initial service state");

    write_executable(
        &bin_dir.join("sudo"),
        "#!/bin/sh\n[ \"$1\" = -- ] && shift\nexec \"$@\"\n",
    );
    write_executable(
        &bin_dir.join("systemctl"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
case " $* " in
  *" show "*) printf 'LoadState=loaded\nActiveState=%s\n' "$(cat "$SYSTEMCTL_STATE")" ;;
  *" stop "*) printf 'inactive\n' > "$SYSTEMCTL_STATE" ;;
  *" start "*) printf 'active\n' > "$SYSTEMCTL_STATE" ;;
  *) echo unexpected: "$*" >&2; exit 1 ;;
esac
"#,
    );

    let repo = temp.path().join("firedancer");
    let fdctl = repo.join("build/native/gcc/bin/fdctl");
    write_executable(
        &fdctl,
        "#!/bin/sh\nprintf '%s\n' \"$*\" >> \"$FDCTL_LOG\"\n",
    );

    let config = temp.path().join("active-fd-config.toml");
    fs::write(&config, "").expect("Firedancer config");

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_val"))
        .args([
            "--base-path",
            temp.path().to_str().expect("UTF-8 base path"),
            "--repo-path",
            repo.to_str().expect("UTF-8 repo path"),
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--log-dir",
            log_dir.to_str().expect("UTF-8 log path"),
            "restart-firedancer",
        ])
        .env("PATH", path)
        .env("SYSTEMCTL_STATE", &state)
        .env("SYSTEMCTL_LOG", &systemctl_log)
        .env("FDCTL_LOG", &fdctl_log)
        .output()
        .expect("run val restart-firedancer");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&state)
            .expect("final service state")
            .trim(),
        "active"
    );
    let systemctl = fs::read_to_string(&systemctl_log).expect("systemctl log");
    let stop_at = first_line_with_word(&systemctl, "stop").expect("systemctl stop");
    let start_at = first_line_with_word(&systemctl, "start").expect("systemctl start");
    assert!(
        stop_at < start_at,
        "stop should run before start: {systemctl}"
    );
    let fdctl_invocations = fs::read_to_string(&fdctl_log).expect("fdctl log");
    let configure_runs = fdctl_invocations
        .lines()
        .filter(|line| line.contains("configure init all"))
        .count();
    assert_eq!(configure_runs, 2, "{fdctl_invocations}");
    assert!(
        fdctl_invocations.contains(config.to_str().expect("UTF-8 config path")),
        "{fdctl_invocations}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stop_segment = stderr
        .find("starting restart segment: stop")
        .expect("stop segment log");
    let configure_1 = stderr
        .find("starting restart segment: configure 1/2")
        .expect("configure 1/2 segment log");
    let configure_2 = stderr
        .find("starting restart segment: configure 2/2")
        .expect("configure 2/2 segment log");
    let start_segment = stderr
        .find("starting restart segment: start")
        .expect("start segment log");
    assert!(
        stop_segment < configure_1 && configure_1 < configure_2 && configure_2 < start_segment,
        "restart segments should print in order: {stderr}"
    );
}

#[test]
fn update_full_runs_update_make_restart_and_prints_compact_progress() {
    let temp = TempDir::new().expect("temporary directory");
    let bin_dir = temp.path().join("bin");
    let log_dir = temp.path().join("logs");
    let state = temp.path().join("service-state");
    let systemctl_log = temp.path().join("systemctl.log");
    let fdctl_log = temp.path().join("fdctl.log");
    let git_log = temp.path().join("git.log");
    let make_log = temp.path().join("make.log");
    fs::write(&state, "active\n").expect("initial service state");

    write_executable(
        &bin_dir.join("sudo"),
        "#!/bin/sh\n[ \"$1\" = -- ] && shift\nexec \"$@\"\n",
    );
    write_executable(
        &bin_dir.join("systemctl"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
case " $* " in
  *" show "*) printf 'LoadState=loaded\nActiveState=%s\n' "$(cat "$SYSTEMCTL_STATE")" ;;
  *" stop "*) printf 'inactive\n' > "$SYSTEMCTL_STATE" ;;
  *" start "*) printf 'active\n' > "$SYSTEMCTL_STATE" ;;
  *) echo unexpected: "$*" >&2; exit 1 ;;
esac
"#,
    );
    write_executable(
        &bin_dir.join("git"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$GIT_LOG"
case "$1" in
  rev-parse)
    last="${@: -1}"
    if [ "$2" = "--is-inside-work-tree" ]; then
      printf 'true\n'
    elif [ "$2" = "--verify" ]; then
      case "$last" in
        HEAD)
          printf 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n'
          ;;
        *vTEST*)
          printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n'
          ;;
        *)
          exit 1
          ;;
      esac
    fi
    ;;
  fetch)
    printf 'git-fetch-noise\n'
    printf 'git-fetch-noise\n' >&2
    exit 0
    ;;
  status|checkout|submodule|reset|clean) exit 0 ;;
  *) exit 0 ;;
esac
"#,
    );
    write_executable(
        &bin_dir.join("make"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MAKE_LOG"
printf 'make-noise\n'
printf 'make-noise\n' >&2
mkdir -p "$REPO_PATH/build/native/gcc/bin"
cat > "$REPO_PATH/build/native/gcc/bin/fdctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$FDCTL_LOG"
EOF
chmod 755 "$REPO_PATH/build/native/gcc/bin/fdctl"
exit 0
"#,
    );

    let repo = temp.path().join("firedancer");
    fs::create_dir_all(&repo).expect("repository directory");
    write_executable(
        &repo.join("deps.sh"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$DEPS_LOG\"\nprintf 'deps-noise\\n'\nprintf 'deps-noise\\n' >&2\n",
    );

    let config = temp.path().join("active-fd-config.toml");
    fs::write(&config, "").expect("Firedancer config");

    let deps_log = temp.path().join("deps.log");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_val"))
        .args([
            "--base-path",
            temp.path().to_str().expect("UTF-8 base path"),
            "--repo-path",
            repo.to_str().expect("UTF-8 repo path"),
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--log-dir",
            log_dir.to_str().expect("UTF-8 log path"),
            "update-full",
            "vTEST",
        ])
        .env("PATH", path)
        .env("SYSTEMCTL_STATE", &state)
        .env("SYSTEMCTL_LOG", &systemctl_log)
        .env("FDCTL_LOG", &fdctl_log)
        .env("GIT_LOG", &git_log)
        .env("MAKE_LOG", &make_log)
        .env("DEPS_LOG", &deps_log)
        .env("REPO_PATH", repo.to_str().expect("UTF-8 repo path"))
        .env_remove("RUST_LOG")
        .output()
        .expect("run val update-full");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&state)
            .expect("final service state")
            .trim(),
        "active"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("[1/3] Update Firedancer (vTEST)"));
    assert!(stdout.contains("[2/3] Build Firedancer"));
    assert!(stdout.contains("[3/3] Restart service"));
    assert!(stdout.contains("Update complete: vTEST"));
    assert!(stdout.contains("Detailed log:"));
    assert!(
        !stdout.contains("git-fetch-noise")
            && !stderr.contains("git-fetch-noise")
            && !stdout.contains("make-noise")
            && !stderr.contains("make-noise")
            && !stdout.contains("deps-noise")
            && !stderr.contains("deps-noise"),
        "compact update-full should hide git/make/deps streams\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("val command started"),
        "compact update-full should hide info tracing on stderr: {stderr}"
    );

    let log = fs::read_to_string(log_dir.join("val.log")).expect("val log");
    assert!(log.contains("git-fetch-noise"), "{log}");
    assert!(log.contains("make-noise"), "{log}");
    assert!(log.contains("deps-noise"), "{log}");

    let systemctl = fs::read_to_string(&systemctl_log).expect("systemctl log");
    let stop_at = first_line_with_word(&systemctl, "stop").expect("systemctl stop");
    let start_at = first_line_with_word(&systemctl, "start").expect("systemctl start");
    assert!(
        stop_at < start_at,
        "stop should run before start: {systemctl}"
    );

    let make = fs::read_to_string(&make_log).expect("make log");
    assert!(make.contains("fdctl"), "{make}");

    let deps = fs::read_to_string(&deps_log).expect("deps log");
    assert!(deps.contains("fetch"), "{deps}");
    assert!(deps.contains("install"), "{deps}");
}
