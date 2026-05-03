# Helm bash integration.
#
# Source from your ~/.bashrc:
#   [ -n "$HELM_INTEGRATION" ] && [ -f "$HOME/.helm/integration/bash" ] \
#       && source "$HOME/.helm/integration/bash"
#
# Adds OSC 133 prompt-integration markers via PROMPT_COMMAND + DEBUG trap.
# Helm's parser turns those into the inbox notifications you see for
# command-done events. Bell detection works without this (any process
# emitting BEL fires a notification), but command-done with exit codes
# and durations needs the integration.

if [ -z "$HELM_INTEGRATION" ]; then
    return 0 2>/dev/null
fi

__helm_emit() {
    printf '\033]133;%s\a' "$1"
}

__helm_command_started=0

__helm_precmd() {
    local exit_code=$?
    if [ "$__helm_command_started" -eq 1 ]; then
        __helm_emit "D;$exit_code"
        __helm_command_started=0
    fi
    __helm_emit "A"
}

# DEBUG trap fires before EVERY simple command — including the commands
# in PROMPT_COMMAND, the trap function itself, and individual stages of
# pipelines. We:
#   - skip our own machinery (BASH_COMMAND matches one of our function
#     names or a known PROMPT_COMMAND substring)
#   - only emit the "command starting" markers once per user command
#     (the __helm_command_started latch handles that)
__helm_preexec() {
    case "$BASH_COMMAND" in
        __helm_precmd|__helm_preexec|__helm_emit)
            return ;;
    esac
    if [ "$__helm_command_started" -eq 0 ]; then
        __helm_emit "B"
        __helm_emit "C"
        __helm_command_started=1
    fi
}

# Prepend our precmd to PROMPT_COMMAND so the OSC 133 markers fire
# *before* the user's prompt rendering — keeps the markers anchored to
# the line the prompt is about to print on.
case ";${PROMPT_COMMAND};" in
    *";__helm_precmd;"*) ;;
    *) PROMPT_COMMAND="__helm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac

trap '__helm_preexec' DEBUG
