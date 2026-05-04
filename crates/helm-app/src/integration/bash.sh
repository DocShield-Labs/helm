# Helm bash integration.
#
# Source from your ~/.bashrc:
#   [ -n "$HELM_INTEGRATION" ] && [ -f "$HOME/.helm/integration/bash" ] \
#       && source "$HOME/.helm/integration/bash"
#
# Adds OSC 133 prompt-integration markers via PROMPT_COMMAND + DEBUG trap.
# Helm's parser turns those into the inbox notifications you see for
# command-done events plus the block boundaries / cwd / branch / cmdline
# metadata for the Warp-style blocks UI. Bell detection works without
# this (any process emitting BEL fires a notification), but command
# tracking with exit codes / durations / commands needs the integration.

if [ -z "$HELM_INTEGRATION" ]; then
    return 0 2>/dev/null
fi

__helm_emit() {
    printf '\033]133;%s\a' "$1"
}

__helm_b64() {
    printf '%s' "$1" | base64 | tr -d '\n'
}

__helm_command_started=0
# Last command captured by the DEBUG trap. Bash's DEBUG trap runs
# before EACH simple command (so a pipeline fires it multiple times),
# but we only want the FIRST hit per user-entered command line. Stash
# what BASH_COMMAND was the first time we saw it post-precmd; clear
# in __helm_precmd.
__helm_pending_cmdline=""

__helm_precmd() {
    local exit_code=$?
    if [ -z "$HELM_KEEP_PROMPT" ]; then
        printf '\033[0m'
    fi
    if [ "$__helm_command_started" -eq 1 ]; then
        # Top pad belongs to the next block only; D ends the prev
        # block on whatever row the command's trailing \n left us on.
        # See zsh.zshrc for full reasoning.
        __helm_emit "D;$exit_code"
        __helm_command_started=0
    fi
    __helm_pending_cmdline=""

    local cwd="$PWD"
    local branch
    branch=$(command git symbolic-ref --short HEAD 2>/dev/null)
    local cwd_b64 branch_b64
    cwd_b64=$(__helm_b64 "$cwd")
    branch_b64=$(__helm_b64 "$branch")
    __helm_emit "A;cwd_b64=${cwd_b64};branch_b64=${branch_b64}"

    # Top pad: keep A's row blank, push header onto the next row.
    echo
    if [ -z "$HELM_KEEP_PROMPT" ]; then
        local cwd_pretty="${PWD/#$HOME/~}"
        if [ -n "$branch" ]; then
            printf '\033[38;5;244m%s · %s\033[0m\n' "$cwd_pretty" "$branch"
        else
            printf '\033[38;5;244m%s\033[0m\n' "$cwd_pretty"
        fi
    fi
}

# DEBUG trap fires before EVERY simple command — including the commands
# in PROMPT_COMMAND, the trap function itself, and individual stages of
# pipelines. We:
#   - skip our own machinery (BASH_COMMAND matches one of our function
#     names)
#   - only emit the "command starting" markers once per user command
#     (the __helm_command_started latch handles that)
__helm_preexec() {
    case "$BASH_COMMAND" in
        __helm_precmd|__helm_preexec|__helm_emit|__helm_b64)
            return ;;
    esac
    if [ "$__helm_command_started" -eq 0 ]; then
        __helm_pending_cmdline="$BASH_COMMAND"
        local cmdline_b64
        cmdline_b64=$(__helm_b64 "$__helm_pending_cmdline")
        __helm_emit "B;cmdline_b64=${cmdline_b64}"
        __helm_emit "C"
        if [ -z "$HELM_KEEP_PROMPT" ]; then
            printf '\033[38;5;245m'
        fi
        __helm_command_started=1
    fi
}

# Minimal helm prompt unless the user has opted out. Bash's PS1 uses
# \[ \] to hint readline about non-printing escapes — keeps cursor
# math correct.
if [ -z "$HELM_KEEP_PROMPT" ]; then
    # Bright blue chevron — the shell prompt is the canonical place
    # to type. Helm's block chrome above renders cwd/branch/status.
    PS1='\[\033[38;5;75m\]❯\[\033[0m\] '
    PS2='\[\033[38;5;244m\]…\[\033[0m\] '
fi

# Prepend our precmd to PROMPT_COMMAND so the OSC 133 markers fire
# *before* the user's prompt rendering — keeps the markers anchored to
# the line the prompt is about to print on.
case ";${PROMPT_COMMAND};" in
    *";__helm_precmd;"*) ;;
    *) PROMPT_COMMAND="__helm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac

trap '__helm_preexec' DEBUG
