#!/bin/sh
# Install onedrive-rs for the current user:
#   binary          ~/.local/bin/onedrive-rs
#   systemd unit    ~/.config/systemd/user/onedrive-rs.service (not enabled)
#   menu entry      ~/.local/share/applications + icon
# Options: --autostart  also show the tray icon at login.
set -eu
cd "$(dirname "$0")/.."

cargo install --path app --locked --root "$HOME/.local"

install -Dm644 contrib/systemd/onedrive-rs.service "$HOME/.config/systemd/user/onedrive-rs.service"
install -Dm644 contrib/desktop/io.github.onedrive_rs.desktop "$HOME/.local/share/applications/io.github.onedrive_rs.desktop"
install -Dm644 contrib/desktop/io.github.onedrive_rs.svg "$HOME/.local/share/icons/hicolor/scalable/apps/io.github.onedrive_rs.svg"
if [ "${1:-}" = "--autostart" ]; then
    install -Dm644 contrib/desktop/io.github.onedrive_rs-autostart.desktop "$HOME/.config/autostart/io.github.onedrive_rs.desktop"
fi
systemctl --user daemon-reload

echo "Installed. Next: onedrive-rs login, then either"
echo "  - a fresh sync:            systemctl --user enable --now onedrive-rs"
echo "  - taking over the D client: see README.md, section \"Switching from the D client\""
