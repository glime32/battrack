#!/usr/bin/env bash
# Désinstalle battrack. Garde le dossier du dépôt et l'historique, sauf avec --tout.
set -uo pipefail
systemctl --user disable --now battrack-maj.timer battrack.service 2>/dev/null
rm -f "$HOME/.local/bin/battrack" \
  "$HOME/.config/systemd/user/battrack.service" \
  "$HOME/.config/systemd/user/battrack-maj.service" \
  "$HOME/.config/systemd/user/battrack-maj.timer" \
  "$HOME/.local/share/applications/battrack-copy.desktop" \
  "$HOME/.config/net.imput.helium/NativeMessagingHosts/com.glime.battrack.json"
systemctl --user daemon-reload
[ "${1:-}" = "--tout" ] && rm -rf "$HOME/.local/state/battrack"
echo "battrack désinstallé. Retire aussi l'extension dans chrome://extensions."
