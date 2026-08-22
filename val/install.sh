#!/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
val_binary=${VAL_BINARY:-"$script_dir/target/release/val"}
val_bin_dir=${VAL_BIN_DIR:-/usr/local/bin}
bash_completion_dir=${VAL_BASH_COMPLETION_DIR:-/usr/share/bash-completion/completions}
bash_completion="$script_dir/completions/val"

if [ ! -f "$val_binary" ]; then
    printf 'error: VAL binary not found at %s\n' "$val_binary" >&2
    printf 'build it with: cargo build --release --locked --manifest-path val/Cargo.toml\n' >&2
    exit 1
fi

if [ ! -f "$bash_completion" ]; then
    printf 'error: Bash completion not found at %s\n' "$bash_completion" >&2
    exit 1
fi

install -d "$val_bin_dir" "$bash_completion_dir"
install -m 0755 "$val_binary" "$val_bin_dir/val"
install -m 0644 "$bash_completion" "$bash_completion_dir/val"

printf 'Installed val and Bash completion.\n'
