# Helm zsh integration — .zshrc forwarder + OSC 133 hooks.
#
# helmd sets ZDOTDIR=~/.helm/integration/zsh on every shell it spawns, so
# zsh reads THIS .zshrc instead of the user's. By the time we run, our
# .zshenv and .zprofile forwarders have already handed off to the user's
# real ones (see .zshenv for the contract). Here we hand off .zshrc the
# same way, leave ZDOTDIR pointing at the user's directory for good, and
# then install the OSC 133 marker hooks.
#
# This integration is PASSIVE: it only emits invisible OSC 133 markers
# (command boundaries + cwd/branch/cmdline metadata) that helm consumes as
# signals — for the inbox, the sidebar, and block segmentation. It never
# touches PROMPT, prints no output of its own, and injects no SGR state.
# The user's prompt and output render exactly as they would in any terminal.
#
# Safe to source twice — the precmd/preexec registration is idempotent
# (we check before appending to the *_functions arrays).

__helm_shim_dir="${${(%):-%x}:A:h}"

# From here on ZDOTDIR is the user's real directory: .zlogin and .zlogout
# come straight from it, and child shells see exactly the configuration
# they would in any other terminal.
ZDOTDIR="${HELM_USER_ZDOTDIR:-$HOME}"

# macOS's /etc/zshrc ran before us, while ZDOTDIR still pointed here, and
# set HISTFILE relative to it. Repoint history at the user's directory so
# Helm panes share one history with every other terminal; the user's
# .zshrc below can still override it.
if [[ -n "$HISTFILE" && "$HISTFILE" == "$__helm_shim_dir"/* ]]; then
    HISTFILE="$ZDOTDIR/${HISTFILE:t}"
fi
unset __helm_shim_dir

if [[ -r "$ZDOTDIR/.zshrc" ]]; then
    builtin source "$ZDOTDIR/.zshrc"
fi

if [[ -z "$HELM_INTEGRATION" ]]; then
    # Sourced outside of helm: the hand-off above is all that's needed.
    return 0
fi

# ----- OSC 133 hooks -----

# Emit `ESC ] 1 3 3 ; <body> BEL`. printf with $'…' for the literal ESC.
__helm_emit() {
    printf '\e]133;%s\a' "$1"
}

# Base64-encode a string with no line wraps. Both BSD and GNU base64 read
# from stdin and emit on stdout; `tr -d '\n'` strips any line breaks
# (BSD-base64 wraps at 76 cols by default).
__helm_b64() {
    printf '%s' "$1" | base64 | tr -d '\n'
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

    # Physical cwd canonicalizes casing on case-insensitive filesystems
    # and keeps grouping/spawn inheritance on one stable path.
    local cwd
    cwd=$(command pwd -P)
    local branch root
    branch=$(command git symbolic-ref --short HEAD 2>/dev/null)
    # The repo root (a worktree's own root): the sidebar groups
    # sessions by it. Empty outside a repo.
    root=$(command git rev-parse --show-toplevel 2>/dev/null)
    local cwd_b64 branch_b64 root_b64
    cwd_b64=$(__helm_b64 "$cwd")
    branch_b64=$(__helm_b64 "$branch")
    root_b64=$(__helm_b64 "$root")
    __helm_emit "A;cwd_b64=${cwd_b64};branch_b64=${branch_b64};root_b64=${root_b64}"
}

__helm_preexec() {
    # `$1` is the full command line as the user typed it. Base64 it so
    # any byte (semicolons, BELs, embedded newlines from heredocs)
    # survives the OSC envelope without quoting hazards.
    local cmdline_b64
    cmdline_b64=$(__helm_b64 "$1")
    __helm_emit "B;cmdline_b64=${cmdline_b64}"
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
