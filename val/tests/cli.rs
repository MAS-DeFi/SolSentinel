#![cfg(unix)]

use std::{env, fs, os::unix::fs::PermissionsExt, process::Command};

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
    assert_eq!(status["active_id_key"], bs58::encode(public).into_string());
    assert_eq!(status["snapshot_fetch"], true);

    let log = fs::read_to_string(log_dir.join("val.log")).expect("val log");
    assert!(log.contains("val command completed"));
}
