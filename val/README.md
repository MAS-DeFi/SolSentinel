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

Run update, build, configure, and restart as the validator user. If invoked through
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
val update-full v0.708.30009
val status
```

Or run the steps individually:

```sh
val update-firedancer v0.708.30009
val make-firedancer
val restart-firedancer
val status
```

To bounce an already-built validator without updating or rebuilding:

```sh
val restart-firedancer
```

### `update-full <GIT_REF>`

Runs the full maintenance flow in one locked command:

1. `update-firedancer` — fetch, check out the requested ref, reconcile submodules,
   and install dependencies.
2. `make-firedancer` — remove `build/`, then build `fdctl` and `solana`.
3. `restart-firedancer` — stop the service, configure twice, start, and verify
   the unit is active.

The validator keeps running through checkout, dependency installation, and the
clean build. The service is stopped only for the restart segment. Any update or
build failure exits before touching systemd.

By default, `update-full` prints compact stage progress on stdout and keeps
detailed git, dependency, and make output in `val.log`. On an interactive
terminal, the current stage shows a spinner and elapsed time so a long
checkout, dependency install, or build does not look stalled. When stdout is
piped, each stage prints `in progress` immediately and `done` when it finishes.
Pass `-v` to restore live command streams and diagnostic tracing on stderr.

If the first configure pass fails, `val` logs a warning and continues to the
second pass. Start runs only when the second pass succeeds. A failed second
pass leaves the service stopped.

### `update-firedancer <GIT_REF>`

1. Verifies the checkout is a Git working tree.
2. Fetches tags and refs from `origin`.
3. Resolves the requested ref to a commit.
4. If the checkout or its submodules have leftover tracked or untracked
   files, logs them and discards them. This path is a managed deployment
   artifact: a previous `make-firedancer` commonly dirties the `agave`
   submodule, and autonomous updates cannot stop for commit or stash.
   Ignored outputs such as `build/` and `opt/` are kept. A missing ref
   does not wipe the checkout. Submodule cleanup failures are logged and
   the update continues so a forced submodule update can repair the tree.
5. Performs a forced detached checkout of the resolved commit.
6. Syncs submodule URLs and force-updates all submodules.
7. Runs `deps.sh fetch check install` with `FD_AUTO_INSTALL_PACKAGES=1` so
   the Continue/package/rustup prompts are not waiting for a human.

The command is safe to repeat for the same ref. Dependency installation is
still run on repeats so an interrupted first attempt can repair itself.

### `make-firedancer`

Removes `<repo>/build` if it exists, then runs `make -j fdctl solana`, streams
output to the terminal and log, and propagates build failure as a non-zero
exit. `opt/` is left in place. It does not stop or restart the validator.

### `configure-firedancer`

Validates the built `fdctl` and active config, then runs:
`sudo fdctl configure init all --config <active-config>`.

### `start-firedancer` / `stop-firedancer`

Checks the systemd unit before taking action. Starting an active service and
stopping an inactive service are successful no-ops. After a state change, the
command verifies the final state. Start failures include the latest 20 journal
lines in `val.log`.

### `restart-firedancer`

Stops the systemd unit if it is running, runs `configure-firedancer` twice, then
starts the unit. Configure is run twice because some Firedancer stages only
finish after an earlier pass has applied. If the first configure pass fails,
`val` logs a warning and continues to the second pass. Start runs only when the
second pass succeeds; otherwise later steps are skipped and the service is left
stopped. This is not a `systemctl restart`.

### `status`

Prints:

- systemd service state
- the current validator boot/startup state from the Firedancer GUI websocket
  (the same source as `val monitor`)
- the running service's `fdctl version` (read through `sudo /proc/<pid>/exe`)
- the current checkout's built `fdctl version`, even when the service is stopped
- the active identity **public** key
- identity keypair path
- whether `[consensus].snapshot_fetch` is enabled
- active config path

The validator state is a one-shot read of the GUI `startup_progress` /
`boot_progress` phase. If the GUI is disabled, down, or does not publish a
phase in time, the field is `unavailable` (JSON `null`) and the rest of the
status report is still printed.

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

### `monitor`

Watches the Firedancer GUI websocket and prints the current boot/startup state.
This does not take the exclusive `val` lock, so `status` and other commands
can still run.

```sh
val monitor
```

The active state is one compact line with a live elapsed timer. When the phase
changes, the completed state remains on its own line with its final duration
and the next state starts counting from zero:

```text
downloading full snapshot .............. 3m 28s
loading ledger ......................... 12s
```

For the unfiltered websocket payloads:

```sh
val monitor --all
```

The URL comes from the active config's `[tiles.gui]` listen address, defaulting
to `ws://127.0.0.1:80/websocket`. Override it with `--url`. The GUI tile must
be enabled. If the validator restarts or the GUI is down, `monitor` prints
`service not available; retrying` and keeps trying until it can reconnect.

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
