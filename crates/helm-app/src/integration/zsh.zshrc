# Helm zsh integration — auto-sourced when ZDOTDIR points here.
#
# tmux sets ZDOTDIR=~/.helm/integration/zsh in its server env, which makes
# zsh read THIS .zshrc instead of the user's. We restore the user's
# real ZDOTDIR (captured into HELM_USER_ZDOTDIR before we clobbered it),
# source their actual .zshrc, and then install the OSC 133 prompt
# integration hooks helm uses for the inbox + blocks UI.
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

# Phase 4F: helm renders block headers (cwd · branch) inline as ANSI
# output on each new prompt and uses a minimal `❯ ` PROMPT, so the
# Warp-style block list looks right out of the box. Set HELM_KEEP_PROMPT=1
# to opt out and keep your real prompt + skip the header.
if [[ -z "$HELM_KEEP_PROMPT" ]]; then
    # Empty PROMPT — Warp-style "single clean pane." The cwd · branch
    # header printed by precmd above the prompt already tells you
    # where you are; the blinking cursor tells you where you'll type.
    # No leading chevron, no decoration on the input row.
    PROMPT=''
    RPROMPT=''
fi

# Print one helm-styled block header line: "<cwd> · <branch>" in dim.
# Called from __helm_precmd before the prompt itself prints. Using
# `print -P` so zsh's prompt expansion handles colour codes for us.
__helm_emit_block_header() {
    if [[ -n "$HELM_KEEP_PROMPT" ]]; then
        return
    fi
    local cwd_pretty branch
    cwd_pretty="${PWD/#$HOME/~}"
    branch=$(command git symbolic-ref --short HEAD 2>/dev/null)
    if [[ -n "$branch" ]]; then
        print -P "%F{244}${cwd_pretty} · ${branch}%f"
    else
        print -P "%F{244}${cwd_pretty}%f"
    fi
}

__helm_precmd() {
    local exit_code=$?
    # Reset SGR — preexec tinted command output dim grey, so without
    # this the prompt below would inherit the dim colour.
    if [[ -z "$HELM_KEEP_PROMPT" ]]; then
        printf '\e[0m'
    fi
    if [[ "$__helm_command_started" -eq 1 ]]; then
        # No blank before D: prev block ends on whatever row the
        # command's trailing \n left the cursor on. The blank
        # produced by the `print` below belongs solely to the new
        # block as its top padding, so consecutive blocks (including
        # two reds in a row) don't double-tint a shared row.
        __helm_emit "D;$exit_code"
        __helm_command_started=0
    fi

    local cwd="$PWD"
    local branch
    branch=$(command git symbolic-ref --short HEAD 2>/dev/null)
    local cwd_b64 branch_b64
    cwd_b64=$(__helm_b64 "$cwd")
    branch_b64=$(__helm_b64 "$branch")
    # Two blank rows of breathing room ABOVE A — they sit in the gap
    # between blocks (no block "owns" them). Putting them BEFORE A means
    # A captures the row where the cwd · branch header is about to
    # print, so the block's startLine == its visible header row. This
    # makes BlockOverlay's chip + divider math anchor off a single
    # row (no "blank top-pad row inside the block" to count past).
    print
    print
    __helm_emit "A;cwd_b64=${cwd_b64};branch_b64=${branch_b64}"
    __helm_emit_block_header
}

__helm_preexec() {
    # `$1` is the full command line as the user typed it. Base64 it so
    # any byte (semicolons, BELs, embedded newlines from heredocs)
    # survives the OSC envelope without quoting hazards.
    local cmdline_b64
    cmdline_b64=$(__helm_b64 "$1")
    __helm_emit "B;cmdline_b64=${cmdline_b64}"
    __helm_emit "C"
    # Tint output dim grey for the command's lifetime. Programs that
    # emit their own SGR overrides win as usual; programs without
    # explicit colour render dim, putting visual emphasis on the
    # typed command + prompt above. Reset happens in precmd.
    if [[ -z "$HELM_KEEP_PROMPT" ]]; then
        printf '\e[38;5;245m'
    fi
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
