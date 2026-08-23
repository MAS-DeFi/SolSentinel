# `val` Firedancer CLI

`val` is a small Linux CLI for repeatable Firedancer update, build, and
systemd lifecycle operations. It replaces the corresponding shell-script
workflow while keeping the existing SolSentinel defaults:

- validator home: the invoking user's home (or `SUDO_USER`'s home)
- Firedancer checkout: `~/code/firedancer`
- active config: `~/active-fd-config.toml`
- systemd unit: `frankendancer.service`
- log file: `~/logs/val.log`

All paths and the unit name can be overridden with global CLI options.

## Requirements

- Linux with systemd
- Git, GNU Make, and `sudo`
- Bash with the standard `bash-completion` package for tab completion
- Rust 1.89 or newer to build
- An existing Firedancer checkout and active config

Run update, build, and configure as the validator user. If invoked through
`sudo`, `val` re-executes itself as `SUDO_USER` before opening logs or touching
the checkout. A direct root login is rejected for these commands to prevent
root-owned build artifacts.

## Build and install

```sh
cargo build --release --locked --manifest-path val/Cargo.toml
sudo val/install.sh
```

The installer installs the binary to `/usr/local/bin/val` and its Bash
completion to `/usr/share/bash-completion/completions/val`. Completion is
loaded by Bash automatically; operators do not run a separate setup command.

## Commands

Use a maintenance window for the full lifecycle:

```sh
val stop-firedancer
val update-firedancer v0.708.30009
val make-firedancer
val configure-firedancer
val start-firedancer
val status
```

### `update-firedancer <GIT_REF>`

1. Verifies the checkout is a Git working tree.
2. Refuses tracked, staged, or untracked local changes.
3. Fetches tags and refs from `origin`.
4. Resolves the requested ref to a commit and performs a detached checkout.
5. Reconciles all submodules.
6. Runs `deps.sh` interactively.

The command is safe to repeat for the same ref. Dependency installation is
still run on repeats so an interrupted first attempt can repair itself.

### `make-firedancer`

Runs `make -j fdctl solana`, streams output to the terminal and log, and
propagates build failure as a non-zero exit. It does not stop or restart the
validator.

### `configure-firedancer`

Validates the built `fdctl` and active config, then runs:
`sudo fdctl configure init all --config <active-config>`.

### `start-firedancer` / `stop-firedancer`

Checks the systemd unit before taking action. Starting an active service and
stopping an inactive service are successful no-ops. After a state change, the
command verifies the final state. Start failures include the latest 20 journal
lines in `val.log`.

### `status`

Prints:

- systemd service state
- the running service's `fdctl version` (read through `sudo /proc/<pid>/exe`)
- the current checkout's built `fdctl version`, even when the service is stopped
- the active identity **public** key
- identity keypair path
- whether `[consensus].snapshot_fetch` is enabled
- active config path

The keypair bytes are never printed or logged. `consensus.identity_path` is
authoritative. If it is empty or omitted, the effective Firedancer default
`<scratch_directory>/identity.json` is used. The CLI applies Firedancer's
`user`, `name`, and `scratch_directory` defaults and substitutions.
Firedancer's effective default of `snapshot_fetch = true` is reported when
the setting is omitted.

For automation:

```sh
val status --json
```

## Global options

```text
--base-path <PATH>   Validator home
--repo-path <PATH>   Firedancer checkout
--config <PATH>      Active Firedancer TOML
--log-dir <PATH>     Log directory
--service <UNIT>     systemd unit
-v, --verbose        Debug logging; repeat for trace logging
```

Set `RUST_LOG` to override the log level when deeper diagnostics are needed.
Logs are mode `0600`, rotate at 25 MiB, and retain five backups. A process
lock prevents update, build, service, and status operations from overlapping.

## Maintaining Bash completion

The packaged completion is generated from the Clap command definitions and
checked into `completions/val`. When the CLI changes, regenerate it from the
repository root:

```sh
cargo run --locked --manifest-path val/Cargo.toml \
  --example generate-bash-completion > val/completions/val
```

The test suite rejects an out-of-date completion file.
