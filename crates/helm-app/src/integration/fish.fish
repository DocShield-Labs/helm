# Helm fish integration.
#
# Source from your ~/.config/fish/config.fish:
#   if test -n "$HELM_INTEGRATION"; and test -f ~/.helm/integration/fish
#       source ~/.helm/integration/fish
#   end
#
# Adds OSC 133 prompt-integration markers via fish's native event system
# (fish_prompt + fish_preexec). Helm's parser turns those into inbox
# notifications for command-done events.

if test -z "$HELM_INTEGRATION"
    exit 0
end

function __helm_emit
    printf '\e]133;%s\a' $argv[1]
end

set -g __helm_command_started 0

function __helm_precmd --on-event fish_prompt
    set -l exit_code $status
    if test "$__helm_command_started" = "1"
        __helm_emit "D;$exit_code"
        set -g __helm_command_started 0
    end
    __helm_emit "A"
end

function __helm_preexec --on-event fish_preexec
    __helm_emit "B"
    __helm_emit "C"
    set -g __helm_command_started 1
end
