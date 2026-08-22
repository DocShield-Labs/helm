# Helm zsh integration — .zprofile forwarder. See .zshenv for why this
# exists and the hand-off contract.

__helm_shim_dir="${${(%):-%x}:A:h}"

ZDOTDIR="${HELM_USER_ZDOTDIR:-$HOME}"
if [[ -r "$ZDOTDIR/.zprofile" ]]; then
    builtin source "$ZDOTDIR/.zprofile"
fi

# Back to us for .zshrc.
ZDOTDIR="$__helm_shim_dir"
unset __helm_shim_dir
