#!/usr/bin/env bash
# Installe / met à jour battrack : binaire, démon systemd --user, mise à jour auto,
# hôte natif Helium, lanceur KDE.
#   curl -fsSL https://raw.githubusercontent.com/glime32/battrack/main/install.sh | bash
#   ou : ~/battrack/install.sh        (--maj : mode silencieux utilisé par la mise à jour auto)
set -euo pipefail

REPO="https://github.com/glime32/battrack.git"
DIR="$HOME/battrack"
MAJ=0
[ "${1:-}" = "--maj" ] && MAJ=1

# Lancé via curl | bash : on récupère le dépôt, puis on relance depuis le dossier
SRC="$(dirname "$(readlink -f "${BASH_SOURCE[0]:-$0}")")"
if [ ! -f "$SRC/battrack.c" ]; then
  command -v git >/dev/null || { echo "Installe git d'abord : sudo pacman -S git"; exit 1; }
  if [ -d "$DIR/.git" ]; then
    git -C "$DIR" pull --ff-only --quiet
  else
    [ -e "$DIR" ] && mv "$DIR" "$DIR.ancien-$(date +%Y%m%d-%H%M%S)"
    git clone --quiet "$REPO" "$DIR"
  fi
  exec bash "$DIR/install.sh" "$@"
fi
cd "$SRC"

say() { [ "$MAJ" = 1 ] || echo "$@"; }
BIN="$HOME/.local/bin/battrack"
UNIT="$HOME/.config/systemd/user"

say "→ compilation"
cc -O2 -pipe -Wall -Wextra -Wno-format-truncation -o battrack.tmp battrack.c # si ça échoue, l'ancien binaire reste
mkdir -p "$HOME/.local/bin"
CHANGED=1
if [ -f "$BIN" ] && cmp -s battrack.tmp "$BIN"; then CHANGED=0; fi
if [ "$CHANGED" = 1 ]; then install -m755 battrack.tmp "$BIN.new" && mv -f "$BIN.new" "$BIN"; fi
rm -f battrack.tmp

say "→ services systemd (utilisateur)"
mkdir -p "$UNIT"
UNITS_CHANGED=0
for u in systemd/*; do
  dest="$UNIT/$(basename "$u")"
  sed "s|@DIR@|$SRC|g" "$u" > "$dest.tmp"
  if cmp -s "$dest.tmp" "$dest"; then rm -f "$dest.tmp"; else mv -f "$dest.tmp" "$dest"; UNITS_CHANGED=1; fi
done
if [ "$UNITS_CHANGED" = 1 ]; then systemctl --user daemon-reload; fi
systemctl --user enable battrack.service battrack-maj.timer >/dev/null 2>&1
systemctl --user start battrack-maj.timer
if [ "$CHANGED" = 1 ] || [ "$UNITS_CHANGED" = 1 ] || ! systemctl --user is-active --quiet battrack.service; then
  systemctl --user restart battrack.service
fi

say "→ hôte natif pour l'extension Helium"
for d in "$HOME/.config/net.imput.helium"; do
  [ -d "$d" ] || continue
  mkdir -p "$d/NativeMessagingHosts"
  sed "s|@BIN@|$BIN|" nmhost.json > "$d/NativeMessagingHosts/com.glime.battrack.json"
  say "   $d/NativeMessagingHosts/com.glime.battrack.json"
done

say "→ lanceur « Rapport batterie (copier) »"
mkdir -p "$HOME/.local/share/applications"
sed "s|@BIN@|$BIN|" battrack-copy.desktop > "$HOME/.local/share/applications/battrack-copy.desktop"

if [ "$MAJ" = 1 ]; then
  if command -v notify-send >/dev/null; then
    notify-send -a battrack -i battery "battrack mis à jour" "$(git log -1 --format='%s' 2>/dev/null || echo 'nouvelle version')" || true
  fi
  exit 0
fi

echo
systemctl --user --no-pager --lines=0 status battrack.service | head -3 || true
echo
echo "OK. Commandes : battrack report | battrack copy | battrack now"
echo "Mise à jour auto : active (2 min après l'ouverture de session, puis toutes les heures)."
echo "Extension (1re fois) : chrome://extensions → Mode développeur → Charger l'extension non empaquetée → $SRC/extension"
