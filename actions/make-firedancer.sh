#!/bin/bash
# make-firedancer.sh - Build Firedancer with proper exit code handling

# Shared base-path resolution and log() helper (kept identical to
# update-firedancer.sh so both scripts resolve the same REPO_DIR).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/firedancer-common.sh
source "$SCRIPT_DIR/lib/firedancer-common.sh"

# Configurations
BASE_PATH="$(resolve_base_path)"
LOG_DIR="$BASE_PATH/logs"
LOG_FILE="$LOG_DIR/validator-make-reboot.log"
REPO_DIR="$BASE_PATH/code/firedancer"

# Ensure log directory exists
mkdir -p "$LOG_DIR"

log "🚀 Starting Firedancer make and reboot process! This next step will cause the validator to go delinquent!!"
log "📁 Base path: $BASE_PATH"
log "📁 Repository: $REPO_DIR"

log "🛠️  Building fdctl and solana..."
cd "$REPO_DIR" || { log "❌ ERROR: Could not change directory to $REPO_DIR"; exit 1; }

# Wipe previous compile output so a version switch cannot mix stale objects
# into the new binaries. Do not touch opt/: that is deps.sh, not make.
BUILD_DIR="$REPO_DIR/build"
log "🧹 Removing $BUILD_DIR for a clean build..."
rm -rf -- "$BUILD_DIR" || { log "❌ ERROR: Could not remove $BUILD_DIR"; exit 1; }

# Start timing the make command
START_TIME=$(date +%s)
make -j fdctl solana 2>&1 | tee -a "$LOG_FILE"
MAKE_EXIT_CODE=$?  # ✅ Capture exit code IMMEDIATELY after make
# End timing the make command
END_TIME=$(date +%s)

# Calculate duration
DURATION=$((END_TIME - START_TIME))
log "⏱️  Make command took $DURATION seconds."

# Check if make command was successful using the captured exit code
if [ $MAKE_EXIT_CODE -eq 0 ]; then
    log "✅ Build completed successfully."
    exit 0
else
    log "❌ Build failed. Not rebooting."
    exit $MAKE_EXIT_CODE
fi

# separate script for stopping the validator service

# log "Stopping Validator Service"
# sudo systemctl stop frankendancer.service
# log "Validator Service Stopped "

# # Log reboot and execute it
# log "🔄 Rebooting system to apply changes..."
# echo "Rebooting at $(date)" >> "$LOG_FILE"
# sudo reboot
