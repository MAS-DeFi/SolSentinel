#!/bin/bash
BASE_PATH=$(eval echo ~$SUDO_USER)
LOG_FILE="$BASE_PATH/logs/configure-firedancer.log"

# Some callers (Ansible become) run this whole script as root, so the
# log-append redirect below would create the file root-owned inside the
# validator's logs/ dir and break later unprivileged appends. Create the
# log up front and hand it to the validator user.
mkdir -p "$BASE_PATH/logs"
touch "$LOG_FILE"
if [ "$(id -u)" -eq 0 ] && [ -n "$SUDO_USER" ] && [ "$SUDO_USER" != "root" ]; then
    chown "$SUDO_USER:" "$BASE_PATH/logs" "$LOG_FILE"
fi

# Function to log messages
log_message() {
    echo "$(date '+%Y-%m-%d %H:%M:%S') - $1" >> "$LOG_FILE"
}

# Attempt to configure Frankendancer with error handling
cd "$BASE_PATH/code/firedancer"
if sudo ./build/native/gcc/bin/fdctl configure init all --config $HOME/active-fd-config.toml; then
    log_message "Successfully configured Firedancer."
else
    log_message "Failed to configure Firedancer."
    exit 1
fi
