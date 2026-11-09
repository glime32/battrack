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
  git merge-base --is-ancestor "$up" HEAD && return 0 # rien de nouveau, ou commits locaux pas encore poussés
  # .allowed_signers est lu dans la version déjà installée : un push qui le modifie ne change rien
  if ! git -c gpg.ssh.allowedSignersFile="$PWD/.allowed_signers" verify-commit "$up" 2>/dev/null; then
    echo "battrack : $up n'a pas de signature connue, mise à jour refusée" >&2
    return 1
  fi
  if ! git merge --ff-only --quiet "$up" 2>/dev/null; then
    echo "battrack : mise à jour bloquée (modifications locales dans $(pwd))" >&2
    return 0
  fi
  ./install.sh --maj
}
main "$@"; exit
