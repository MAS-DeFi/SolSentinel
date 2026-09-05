//! Command-line arguments and subcommand definitions.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "val",
    version,
    about = "Production-safe Firedancer lifecycle management",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Validator user's home directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub base_path: Option<PathBuf>,

    /// Firedancer git checkout. Defaults to <base-path>/code/firedancer.
    #[arg(long, global = true, value_name = "PATH")]
    pub repo_path: Option<PathBuf>,

    /// Active Firedancer TOML configuration.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Directory containing val.log. Defaults to <base-path>/logs.
    #[arg(long, global = true, value_name = "PATH")]
    pub log_dir: Option<PathBuf>,

    /// systemd service managed by start, stop, restart, and status.
    #[arg(
        long,
        global = true,
        default_value = "frankendancer.service",
        value_name = "UNIT"
    )]
    pub service: String,

    /// Increase diagnostic logging (-v for debug, -vv for trace).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Fetch a Firedancer git ref, check it out, update submodules, and run deps.sh.
    UpdateFiredancer {
        /// Git tag, branch, or commit to check out.
        #[arg(value_name = "GIT_REF")]
        git_ref: String,
    },

    /// Update, build, restart, then wait for the validator to report running.
    UpdateFull {
        /// Git tag, branch, or commit to check out.
        #[arg(value_name = "GIT_REF")]
        git_ref: String,
    },

    /// Remove the Firedancer build directory, then build fdctl and solana.
    MakeFiredancer,

    /// Initialize all Firedancer host configuration stages.
    ConfigureFiredancer,

    /// Start the Firedancer systemd service, if it is not already active.
    StartFiredancer,

    /// Stop the Firedancer systemd service, if it is not already inactive.
    StopFiredancer,

    /// Stop the service, run configure-firedancer twice, then start it.
    RestartFiredancer,

    /// Show service state, validator boot/startup state, identity, and snapshot-fetch state.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Watch the Firedancer GUI websocket for boot and startup state.
    Monitor {
        /// Print every websocket message instead of only startup/boot progress.
        #[arg(long)]
        all: bool,

        /// GUI websocket URL. Defaults to the active config listen address.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
    },
}

impl Commands {
    /// Returns whether the command must run as the validator user.
    pub fn requires_validator_user(&self) -> bool {
        matches!(
            self,
            Self::UpdateFiredancer { .. }
                | Self::UpdateFull { .. }
                | Self::MakeFiredancer
                | Self::ConfigureFiredancer
                | Self::RestartFiredancer
        )
    }

    /// Returns whether the command takes the exclusive val process lock.
    pub fn holds_command_lock(&self) -> bool {
        !matches!(self, Self::Monitor { .. })
    }

    /// Returns the stable command name used in logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::UpdateFiredancer { .. } => "update-firedancer",
            Self::UpdateFull { .. } => "update-full",
            Self::MakeFiredancer => "make-firedancer",
            Self::ConfigureFiredancer => "configure-firedancer",
            Self::StartFiredancer => "start-firedancer",
            Self::StopFiredancer => "stop-firedancer",
            Self::RestartFiredancer => "restart-firedancer",
            Self::Status { .. } => "status",
            Self::Monitor { .. } => "monitor",
        }
    }
}
