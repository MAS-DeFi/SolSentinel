#![cfg(unix)]

use std::{env, fs, os::unix::fs::PermissionsExt, process::Command};

use ed25519_dalek::SigningKey;
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn status_json_runs_end_to_end() {
    let temp = TempDir::new().expect("temporary directory");
    let bin_dir = temp.path().join("bin");
    let log_dir = temp.path().join("logs");
    fs::create_dir_all(&bin_dir).expect("fake binary directory");

    let systemctl = bin_dir.join("systemctl");
    fs::write(
        &systemctl,
        "#!/bin/sh\nprintf 'LoadState=loaded\\nActiveState=active\\n'\n",
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
    assert_eq!(status["active_id_key"], bs58::encode(public).into_string());
    assert_eq!(status["snapshot_fetch"], true);

    let log = fs::read_to_string(log_dir.join("val.log")).expect("val log");
    assert!(log.contains("val command completed"));
}
