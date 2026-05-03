/**
 * Reconnecting overlay.
 *
 * Renders a frosted card centered over the pane area while the Rust
 * supervisor is running its backoff ladder. The TmuxPane underneath
 * stays mounted with its last frozen frame, so when reconnect succeeds
 * the user picks up exactly where they left off — minus any output the
 * remote produced while the link was down (a `refetchTree` runs on the
 * `connected` transition, but xterm scrollback during the gap is gone).
 *
 * Surfaces for both remote (SSH transport drop) and localhost (tmux
 * server died and is being respawned). For local, the "transport" is
 * the local PTY hosting the `tmux -CC` client; the supervisor re-runs
 * `spawn_local` each attempt, which `exec`s `tmux -CC new-session -A`
 * and brings up a fresh server.
 */

import type { Host } from '@bindings'
import { Button } from '@ui'
import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'

interface Props {
  host: Host
}

export function ReconnectingOverlay({ host }: Props) {
  // The supervisor stamps each Reconnecting emit with its last connect
  // error so we can show *why* — most useful for the stuck-forever case
  // (tmux not installed, binary missing) where the spinner alone is
  // misleading.
  const lastError = useStore((s) => s.hostErrors.get(host.id))
  return (
    <div className="absolute inset-0 z-30 flex items-center justify-center
                    bg-canvas/60 backdrop-blur-sm">
      <div className="flex flex-col items-center gap-3 rounded-lg
                      border border-white/[0.08] bg-elevated/95 px-6 py-5
                      shadow-lg min-w-[320px] max-w-[480px]"
           style={{ boxShadow: 'var(--elevation-2)' }}>
        <div className="flex items-center gap-2">
          <span className="h-2 w-2 rounded-full bg-amber-400 animate-pulse" />
          <span className="text-[13px] font-medium text-text-primary">
            Reconnecting to {host.name}…
          </span>
        </div>
        <p className="text-center text-[11px] text-text-tertiary">
          {host.port === 0
            ? 'Local tmux is being respawned.'
            : 'The transport dropped. Retrying with backoff.'}
        </p>
        {lastError && (
          <p className="text-center font-mono text-[11px] text-status-error break-words">
            {lastError}
          </p>
        )}
        <Button
          kind="secondary"
          onClick={() => {
            void commands.hostConnect(host.id, null)
          }}
        >
          Retry now
        </Button>
      </div>
    </div>
  )
}
