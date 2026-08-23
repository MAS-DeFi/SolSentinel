//! Firedancer status collection and rendering.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use tracing::warn;
use zeroize::Zeroizing;

use crate::{paths::AppPaths, service::ServiceState};

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StatusReport {
    pub service: String,
    pub running_fdctl_version: Option<String>,
    pub built_fdctl_version: Option<String>,
    pub active_id_key: String,
    pub identity_path: String,
    pub snapshot_fetch: bool,
    pub snapshot_fetch_source: SnapshotFetchSource,
    pub config_path: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotFetchSource {
    Configured,
    FiredancerDefault,
}

impl StatusReport {
    /// Loads service, identity, and snapshot-fetch status.
    pub fn load(
        paths: &AppPaths,
        service_state: ServiceState,
        running_fdctl_version: Option<String>,
        built_fdctl_version: Option<String>,
    ) -> Result<Self> {
        let config_text = fs::read_to_string(&paths.config).with_context(|| {
            format!(
                "could not read active Firedancer config {}",
                paths.config.display()
            )
        })?;
        let config: FiredancerConfig = toml::from_str(&config_text).with_context(|| {
            format!(
                "could not parse active Firedancer config {}",
                paths.config.display()
            )
        })?;

        let (snapshot_fetch, snapshot_fetch_source) =
            match config.consensus.as_ref().and_then(|c| c.snapshot_fetch) {
                Some(value) => (value, SnapshotFetchSource::Configured),
                None => (true, SnapshotFetchSource::FiredancerDefault),
            };

        let identity_path = resolve_identity_path(&config, paths)?;
        warn_if_key_permissions_are_open(&identity_path);
        let active_id_key = read_public_key(&identity_path)?;

        Ok(Self {
            service: service_state.to_string(),
            running_fdctl_version,
            built_fdctl_version,
            active_id_key,
            identity_path: identity_path.to_string_lossy().into_owned(),
            snapshot_fetch,
            snapshot_fetch_source,
            config_path: paths.config.to_string_lossy().into_owned(),
        })
    }

    /// Writes the human-readable status report.
    pub fn write_human(&self, mut output: impl Write) -> io::Result<()> {
        let source = match self.snapshot_fetch_source {
            SnapshotFetchSource::Configured => "configured",
            SnapshotFetchSource::FiredancerDefault => "Firedancer default",
        };
        writeln!(output, "service: {}", self.service)?;
        writeln!(
            output,
            "running fdctl version: {}",
            self.running_fdctl_version
                .as_deref()
                .unwrap_or("not running")
        )?;
        writeln!(
            output,
            "built fdctl version: {}",
            self.built_fdctl_version.as_deref().unwrap_or("not built")
        )?;
        writeln!(output, "active-id key: {}", self.active_id_key)?;
        writeln!(output, "identity path: {}", self.identity_path)?;
        writeln!(
            output,
            "snapshot fetch: {} ({source})",
            if self.snapshot_fetch {
                "enabled"
            } else {
                "disabled"
            }
        )?;
        writeln!(output, "config: {}", self.config_path)
    }
}

#[derive(Debug, Deserialize)]
struct FiredancerConfig {
    name: Option<String>,
    user: Option<String>,
    scratch_directory: Option<String>,
    consensus: Option<ConsensusConfig>,
}

#[derive(Debug, Deserialize)]
struct ConsensusConfig {
    identity_path: Option<String>,
    snapshot_fetch: Option<bool>,
}

/// Resolves Firedancer's effective identity keypair path.
fn resolve_identity_path(config: &FiredancerConfig, paths: &AppPaths) -> Result<PathBuf> {
    let user = config
        .user
        .as_deref()
        .filter(|user| !user.is_empty())
        .unwrap_or(&paths.username);
    let name = config
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or("fd1");
    let configured = config
        .consensus
        .as_ref()
        .and_then(|consensus| consensus.identity_path.as_deref())
        .filter(|path| !path.is_empty());

    let path = if let Some(configured) = configured {
        PathBuf::from(expand_firedancer_path(configured, user, name))
    } else {
        let scratch = config
            .scratch_directory
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or("/home/{user}/.firedancer/{name}");
        PathBuf::from(expand_firedancer_path(scratch, user, name)).join("identity.json")
    };
    if !path.is_absolute() {
        bail!(
            "effective Firedancer identity path must be absolute, got {}",
            path.display()
        );
    }
    Ok(path)
}

/// Expands Firedancer's supported path placeholders.
fn expand_firedancer_path(path: &str, user: &str, name: &str) -> String {
    path.replace("{user}", user).replace("{name}", name)
}

/// Validates a keypair and returns its base58 public key.
fn read_public_key(path: &Path) -> Result<String> {
    let key_text =
        Zeroizing::new(fs::read_to_string(path).with_context(|| {
            format!("could not read active identity keypair {}", path.display())
        })?);
    let keypair =
        Zeroizing::new(serde_json::from_str::<Vec<u8>>(&key_text).with_context(|| {
            format!(
                "active identity keypair is not valid JSON: {}",
                path.display()
            )
        })?);
    if keypair.len() != 64 {
        bail!(
            "active identity keypair must contain exactly 64 bytes, found {} in {}",
            keypair.len(),
            path.display()
        );
    }

    let secret = Zeroizing::new(
        keypair[..32]
            .try_into()
            .expect("keypair length was checked above"),
    );
    let stored_public: [u8; 32] = keypair[32..]
        .try_into()
        .expect("keypair length was checked above");
    let derived_public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
    if stored_public != derived_public {
        bail!(
            "active identity keypair has a public key that does not match its private key: {}",
            path.display()
        );
    }

    Ok(bs58::encode(derived_public).into_string())
}

#[cfg(unix)]
/// Warns when the identity keypair has permissive Unix modes.
fn warn_if_key_permissions_are_open(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            warn!(
                path = %path.display(),
                mode = format_args!("{:04o}", mode & 0o777),
                "identity keypair is accessible by group or other users"
            );
        }
    }
}

#[cfg(not(unix))]
/// Performs no permission check on non-Unix platforms.
fn warn_if_key_permissions_are_open(_: &Path) {}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use anyhow::Result;
    use ed25519_dalek::SigningKey;
    use tempfile::TempDir;

    use super::{SnapshotFetchSource, StatusReport};
    use crate::{paths::AppPaths, service::ServiceState};

    fn paths(temp: &TempDir) -> AppPaths {
        AppPaths {
            base: temp.path().to_owned(),
            repository: temp.path().join("code/firedancer"),
            config: temp.path().join("active-fd-config.toml"),
            log_dir: temp.path().join("logs"),
            username: "validator".to_owned(),
        }
    }

    #[test]
    fn reports_configured_identity_and_disabled_snapshot_fetch() -> Result<()> {
        let temp = TempDir::new()?;
        let paths = paths(&temp);
        let identity = temp.path().join("active-id.json");
        let secret = [7_u8; 32];
        let public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        let mut bytes = secret.to_vec();
        bytes.extend(public);
        fs::write(&identity, serde_json::to_string(&bytes)?)?;
        fs::write(
            &paths.config,
            format!(
                "[consensus]\nidentity_path = {:?}\nsnapshot_fetch = false\n",
                identity.to_string_lossy()
            ),
        )?;

        let report = StatusReport::load(
            &paths,
            ServiceState::Active,
            Some("v1.2.3".to_owned()),
            Some("v1.2.4".to_owned()),
        )?;
        assert_eq!(report.service, "active");
        assert_eq!(report.running_fdctl_version.as_deref(), Some("v1.2.3"));
        assert_eq!(report.built_fdctl_version.as_deref(), Some("v1.2.4"));
        assert_eq!(report.active_id_key, bs58::encode(public).into_string());
        assert!(!report.snapshot_fetch);
        assert_eq!(
            report.snapshot_fetch_source,
            SnapshotFetchSource::Configured
        );
        Ok(())
    }

    #[test]
    fn uses_effective_firedancer_identity_and_snapshot_defaults() -> Result<()> {
        let temp = TempDir::new()?;
        let paths = paths(&temp);
        let scratch = temp.path().join("fd-scratch");
        let identity = scratch.join("identity.json");
        fs::create_dir_all(&scratch)?;
        let secret = [1_u8; 32];
        let mut bytes = secret.to_vec();
        bytes.extend(SigningKey::from_bytes(&secret).verifying_key().to_bytes());
        fs::write(&identity, serde_json::to_string(&bytes)?)?;
        fs::write(
            &paths.config,
            format!(
                "scratch_directory = {:?}\n[consensus]\n",
                scratch.to_string_lossy()
            ),
        )?;

        let report = StatusReport::load(&paths, ServiceState::Inactive, None, None)?;
        assert!(report.snapshot_fetch);
        assert_eq!(
            report.snapshot_fetch_source,
            SnapshotFetchSource::FiredancerDefault
        );
        assert_eq!(report.identity_path, identity.to_string_lossy());
        Ok(())
    }

    #[test]
    fn expands_firedancer_user_and_name_placeholders() -> Result<()> {
        let temp = TempDir::new()?;
        let mut paths = paths(&temp);
        paths.username = "validator".to_owned();
        let identity = PathBuf::from("/tmp/fd-user-fd-main-id.json");
        fs::write(
            &paths.config,
            "name = \"fd-main\"\nuser = \"fd-user\"\n[consensus]\nidentity_path = \"/tmp/{user}-{name}-id.json\"\n",
        )?;

        let config_text = fs::read_to_string(&paths.config)?;
        let config = toml::from_str(&config_text)?;
        assert_eq!(super::resolve_identity_path(&config, &paths)?, identity);
        Ok(())
    }
}
