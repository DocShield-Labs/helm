# Helm fish integration.
#
# Source from your ~/.config/fish/config.fish:
#   if test -n "$HELM_INTEGRATION"; and test -f ~/.helm/integration/fish
#       source ~/.helm/integration/fish
#   end
#
# Adds OSC 133 prompt-integration markers via fish's native event system
# (fish_prompt + fish_preexec). Helm's parser turns those into inbox
# notifications and the block boundaries / cwd / branch / cmdline
# metadata for the Warp-style blocks UI.

if test -z "$HELM_INTEGRATION"
    exit 0
end

function __helm_emit
    printf '\e]133;%s\a' $argv[1]
end

function __helm_b64
    printf '%s' $argv[1] | base64 | tr -d '\n'
end

set -g __helm_command_started 0

function __helm_precmd --on-event fish_prompt
    set -l exit_code $status
    if test -z "$HELM_KEEP_PROMPT"
        printf '\e[0m'
    end
    if test "$__helm_command_started" = "1"
        # Top pad belongs to the next block only. See zsh.zshrc.
        __helm_emit "D;$exit_code"
        set -g __helm_command_started 0
    end

    set -l cwd $PWD
    set -l branch (command git symbolic-ref --short HEAD 2>/dev/null)
    set -l cwd_b64 (__helm_b64 $cwd)
    set -l branch_b64 (__helm_b64 "$branch")
    __helm_emit "A;cwd_b64=$cwd_b64;branch_b64=$branch_b64"

    # Top pad: leave A's row blank, push header onto the next row.
    echo
    if test -z "$HELM_KEEP_PROMPT"
        set -l cwd_pretty (string replace -r "^$HOME" "~" $PWD)
        if test -n "$branch"
            printf '\e[38;5;244m%s · %s\e[0m\n' $cwd_pretty $branch
        else
            printf '\e[38;5;244m%s\e[0m\n' $cwd_pretty
        end
    end
end

function __helm_preexec --on-event fish_preexec
    set -l cmdline_b64 (__helm_b64 $argv[1])
    __helm_emit "B;cmdline_b64=$cmdline_b64"
    __helm_emit "C"
    if test -z "$HELM_KEEP_PROMPT"
        printf '\e[38;5;245m'
    end
    set -g __helm_command_started 1
end

# Minimal helm prompt unless the user has opted out.
if test -z "$HELM_KEEP_PROMPT"
    function fish_prompt
        # Bright blue chevron — primary input surface.
        set_color brblue
        printf '❯ '
        set_color normal
    end
end
