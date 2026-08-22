# Helm zsh integration — .zshenv forwarder.
#
# helmd starts every pane with ZDOTDIR pointed at this directory so that
# zsh picks up OUR .zshrc, which installs the OSC 133 hooks. But zsh reads
# *every* startup file from $ZDOTDIR, not just .zshrc:
#
#     /etc/zshenv   $ZDOTDIR/.zshenv
#     /etc/zprofile $ZDOTDIR/.zprofile   (login shells)
#     /etc/zshrc    $ZDOTDIR/.zshrc      (interactive shells)
#     /etc/zlogin   $ZDOTDIR/.zlogin     (login shells)
#
# so this file and .zprofile beside it exist purely to hand off to the
# user's real ones. Without them ~/.zshenv and ~/.zprofile silently never
# run inside Helm — no cargo, no `brew shellenv`, no pyenv, nothing the
# user put there — while every other terminal on the machine sees them.
#
# Contract for each forwarder: point ZDOTDIR at the user's real directory
# while their file runs (so anything relative to $ZDOTDIR resolves the way
# it does in any other terminal), then point it back at us so the next
# startup file zsh reads is ours again. zsh re-evaluates ZDOTDIR before
# each file, which is what makes this hand-off possible.

__helm_shim_dir="${${(%):-%x}:A:h}"

ZDOTDIR="${HELM_USER_ZDOTDIR:-$HOME}"
if [[ -r "$ZDOTDIR/.zshenv" ]]; then
    builtin source "$ZDOTDIR/.zshenv"
fi

# Their .zshenv may relocate ZDOTDIR (the ~/.config/zsh pattern). Whatever
# it is now is the real location for the rest of startup and for every
# child shell; remember it so .zprofile/.zshrc forward to the right place.
export HELM_USER_ZDOTDIR="$ZDOTDIR"

# Only an interactive shell under Helm gets our .zprofile/.zshrc. A
# non-interactive login shell (`zsh -l script`) reads the user's remaining
# files directly — there is no prompt to hook.
if [[ -o interactive && -n "$HELM_INTEGRATION" ]]; then
    ZDOTDIR="$__helm_shim_dir"
fi
unset __helm_shim_dir
