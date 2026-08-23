#!/bin/bash
# update-firedancer.sh
# Updates the Firedancer checkout to a specified git ref (normally a release
# tag) and installs its dependencies (deps.sh). The actual build is done
# separately by make-firedancer.sh.
#
# Usage: ./update-firedancer.sh <git-ref>
#
# Works whether run directly as the validator user or via sudo. When invoked
# via sudo it re-execs as the original user so the checkout and dependency
# files are owned by that user (not root), keeping a later non-sudo build and
# `git` operations working.
#
# Exit codes propagate to the caller: any failure aborts and returns non-zero.

set -euo pipefail

# Resolve our own absolute path before anything else so the re-exec below is
# robust regardless of the caller's cwd.
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(dirname "$SCRIPT_PATH")"

# If invoked as root via sudo, drop back to the invoking user so git and
# deps.sh do not leave root-owned files under the user's home.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
    exec sudo -u "$SUDO_USER" -H bash "$SCRIPT_PATH" "$@"
fi

# A direct root login has no safe user to drop privileges to. Refuse it rather
# than creating root-owned files in /root or in a validator checkout.
if [ "$(id -u)" -eq 0 ]; then
    echo "❌ ERROR: Do not run as a root login." \
        "Run as the validator user or use sudo from that user." >&2
    exit 1
fi

# Shared base-path resolution and log() helper.
# shellcheck source=lib/firedancer-common.sh
source "$SCRIPT_DIR/lib/firedancer-common.sh"

# Accept the ref as a command-line argument (empty if omitted; no set -u blow-up)
REF="${1:-}"

BASE_PATH="$(resolve_base_path)"
LOG_DIR="$BASE_PATH/logs"
LOG_FILE="$LOG_DIR/firedancer-update.log"
REPO_DIR="$BASE_PATH/code/firedancer"

# Ensure the logging directory exists before we try to write to it.
mkdir -p "$LOG_DIR" || { echo "❌ ERROR: Could not create log directory $LOG_DIR"; exit 1; }

# Validate input.
if [ -z "$REF" ]; then
    log "❌ ERROR: No git ref provided. Usage: $0 <git-ref>"
    exit 1
fi

log "🔥 Updating Firedancer to ref: $REF"
log "📁 Base path: $BASE_PATH"
log "📁 Repository: $REPO_DIR"

# Navigate to the repo and confirm it is a git working tree.
cd "$REPO_DIR" || { log "❌ ERROR: Failed to change directory to $REPO_DIR"; exit 1; }
git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    || { log "❌ ERROR: $REPO_DIR is not a git repository"; exit 1; }

# Fetch the latest refs and tags from origin only. (--all would contact every
# configured remote and abort the whole update if an unrelated extra remote is
# unreachable or auth-gated.)
log "📥 Fetching latest refs and tags from origin..."
git fetch --tags origin 2>&1 | tee -a "$LOG_FILE"

# Validate the ref resolves to a commit. `^{commit}` accepts tags, branches,
# and raw SHAs, preserving the previous script's acceptance of any commit-ish.
if ! git rev-parse -q --verify "${REF}^{commit}" >/dev/null 2>&1; then
    log "❌ ERROR: ref '$REF' not found in repository after fetch"
    exit 1
fi

# This checkout is a managed deployment artifact. A previous deps.sh or make
# commonly leaves the agave submodule dirty, and autonomous updates cannot
# stop for commit or stash. Discard leftover tracked/untracked files only after
# the requested ref is known to exist. Keep ignored outputs such as build/ and
# opt/. Submodule cleanup failures must not block the forced submodule update.
STATUS="$(git status --porcelain=v1 --untracked-files=normal --ignore-submodules=none)"
if [ -n "$STATUS" ]; then
    log "⚠️ Discarding leftover Firedancer checkout changes before update:"
    printf '%s\n' "$STATUS" | tee -a "$LOG_FILE"
    git reset --hard HEAD 2>&1 | tee -a "$LOG_FILE"
    git clean -ffd 2>&1 | tee -a "$LOG_FILE"
    git submodule foreach --recursive git reset --hard 2>&1 | tee -a "$LOG_FILE" \
        || log "⚠️ submodule reset failed; continuing with forced checkout"
    git submodule foreach --recursive git clean -ffd 2>&1 | tee -a "$LOG_FILE" \
        || log "⚠️ submodule clean failed; continuing with forced checkout"
fi

# Check out the ref in detached HEAD: avoids polluting the branch namespace and
# the tag/branch name-collision ambiguity that a local branch named after a tag
# would create on repeated runs.
log "🌿 Checking out ref: $REF"
git checkout --force --detach "$REF" 2>&1 | tee -a "$LOG_FILE"

log "✅ Now at: $(git describe --tags --always)"

log "🔁 Updating submodules..."
git submodule sync --recursive 2>&1 | tee -a "$LOG_FILE"
git submodule update --init --recursive --force --checkout 2>&1 | tee -a "$LOG_FILE"
log "✅ Submodules updated"

# Run deps.sh attached to the terminal (NOT piped through tee) so its manual
# approval prompt stays visible and interactive, as the README flow requires.
log "📦 Installing dependencies (deps.sh) — approve the prompt if asked..."
if ! ./deps.sh; then
    log "❌ ERROR: deps.sh failed"
    exit 1
fi
log "✅ deps.sh ran successfully"

log "🎉 Update complete. Run make-firedancer.sh to build."
