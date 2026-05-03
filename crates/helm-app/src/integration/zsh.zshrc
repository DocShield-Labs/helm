# Helm zsh integration — auto-sourced when ZDOTDIR points here.
#
# tmux sets ZDOTDIR=~/.helm/integration/zsh in its server env, which makes
# zsh read THIS .zshrc instead of the user's. We restore the user's
# real ZDOTDIR (captured into HELM_USER_ZDOTDIR before we clobbered it),
# source their actual .zshrc, and then install the OSC 133 prompt
# integration hooks helm uses for the inbox + (later) blocks UI.
#
# Safe to source twice — the precmd/preexec registration is idempotent
# (we check before appending to the *_functions arrays).

if [[ -z "$HELM_INTEGRATION" ]]; then
    # Sourced outside of helm. Fall through silently — the user's normal
    # zsh startup will handle everything.
    return 0
fi

# Restore the user's real dotfile location so any hooks they add to
# precmd_functions / preexec_functions resolve relative to their config.
__helm_user_zdotdir="${HELM_USER_ZDOTDIR:-$HOME}"
ZDOTDIR="$__helm_user_zdotdir"

# Source the user's real startup files in zsh's documented order. We
# only do .zshrc here because .zshenv / .zprofile have already run
# (zsh loads them before .zshrc regardless of ZDOTDIR being changed
# mid-startup — well, almost; the prior ZDOTDIR was used for .zshenv).
if [[ -f "$ZDOTDIR/.zshrc" ]]; then
    source "$ZDOTDIR/.zshrc"
fi

# ----- OSC 133 hooks -----

# Emit `ESC ] 1 3 3 ; <body> BEL`. printf with $'…' for the literal ESC.
__helm_emit() {
    printf '\e]133;%s\a' "$1"
}

# Track whether we're currently between `preexec` (command starting) and
# the next `precmd` (prompt about to redraw). Without this, the very
# first prompt after shell startup would emit a spurious "command done"
# (the `D` marker is paired with the most recent `B` — we shouldn't
# emit `D` if we never emitted `B`).
__helm_command_started=0

__helm_precmd() {
    local exit_code=$?
    if [[ "$__helm_command_started" -eq 1 ]]; then
        __helm_emit "D;$exit_code"
        __helm_command_started=0
    fi
    __helm_emit "A"
}

__helm_preexec() {
    __helm_emit "B"
    __helm_emit "C"
    __helm_command_started=1
}

# Idempotent registration — append to the arrays only if we're not
# already there. Avoids stacking duplicate hooks if the user's .zshrc
# sources us a second time.
typeset -ga precmd_functions
typeset -ga preexec_functions
if [[ -z "${precmd_functions[(r)__helm_precmd]}" ]]; then
    precmd_functions+=(__helm_precmd)
fi
if [[ -z "${preexec_functions[(r)__helm_preexec]}" ]]; then
    preexec_functions+=(__helm_preexec)
fi
