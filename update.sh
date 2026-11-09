#!/usr/bin/env bash
# Mise à jour auto (lancée par battrack-maj.timer) : si GitHub a du nouveau,
# on avance le dépôt local, puis install.sh --maj recompile et relance.
# Des modifications locales bloquent la mise à jour : rien n'est écrasé.
# Tout est dans main() pour que bash lise le script en entier avant que git le remplace.
main() {
  set -uo pipefail
  cd "$(dirname "$(readlink -f "$0")")" || return 0
  timeout 20 git fetch --quiet origin 2>/dev/null || return 0 # pas de réseau : prochain tour
  local up
  up=$(git rev-parse --verify --quiet '@{u}') || return 0
  [ "$(git rev-parse HEAD)" = "$up" ] && return 0
  if ! git merge --ff-only --quiet "$up" 2>/dev/null; then
    echo "battrack : mise à jour bloquée (modifications locales dans $(pwd))" >&2
    return 0
  fi
  ./install.sh --maj
}
main "$@"; exit
