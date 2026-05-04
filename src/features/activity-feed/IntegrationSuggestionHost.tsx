/**
 * Renders sticky suggestion cards for tool-integration setup (Claude
 * Code's bell hooks today; future: pgcli, mosh, anything else that
 * benefits from helm-aware bells).
 *
 * Visual model is "card stack at the bottom-right, above the regular
 * Toast stack." Each card has a name + description + Install/Not now
 * pair. Sticky: no auto-dismiss. The user explicitly chooses, the
 * backend records the decision for the rest of the session, and the
 * card disappears. Next app launch reconsiders.
 *
 * Why a separate component (instead of overloading Toast):
 *   - Toast is for transient single-action acknowledgements; these
 *     suggestions are persistent decisions
 *   - We want a description line + two buttons; Toast supports one
 *   - Keeps the regular toast UX clean (toasts auto-dismiss; these
 *     don't)
 */

import { useState } from 'react'
import { commands } from '@lib/ipc'
import { useStore, type ToolIntegrationSuggestion } from '@lib/store'

export function IntegrationSuggestionHost() {
  const suggestions = useStore((s) => s.toolSuggestions)
  const dismiss = useStore((s) => s.dismissToolSuggestion)

  if (suggestions.length === 0) return null

  return (
    <div className="pointer-events-none fixed bottom-4 right-4 z-40 flex flex-col-reverse gap-2">
      {suggestions.map((s) => (
        <div key={`${s.hostId}::${s.integrationId}`} className="pointer-events-auto">
          <SuggestionCard
            suggestion={s}
            onAccepted={() => dismiss(s.hostId, s.integrationId)}
            onDeclined={() => dismiss(s.hostId, s.integrationId)}
          />
        </div>
      ))}
    </div>
  )
}

interface SuggestionCardProps {
  suggestion: ToolIntegrationSuggestion
  onAccepted: () => void
  onDeclined: () => void
}

function SuggestionCard({ suggestion, onAccepted, onDeclined }: SuggestionCardProps) {
  // Three-state local UI: idle → installing → installed (with the
  // post-install note shown briefly), or idle → error. The card
  // dismisses itself shortly after a successful install so the user
  // gets the confirmation but the chrome doesn't linger.
  const [state, setState] = useState<
    | { kind: 'idle' }
    | { kind: 'installing' }
    | { kind: 'installed' }
    | { kind: 'error'; message: string }
  >({ kind: 'idle' })

  const onInstall = async () => {
    setState({ kind: 'installing' })
    const res = await commands.toolIntegrationInstall(
      suggestion.hostId,
      suggestion.integrationId,
    )
    if (res.status !== 'ok') {
      setState({ kind: 'error', message: res.error })
      return
    }
    setState({ kind: 'installed' })
    // Give the user 4 seconds to read the post-install note, then
    // clear the card. Onboarding done.
    window.setTimeout(onAccepted, 4_000)
  }

  const onNotNow = async () => {
    // Backend persists the dismissal so we don't re-suggest until
    // next app launch. Best-effort — even if the IPC fails, we drop
    // the card from the store so the user isn't stuck staring at it.
    void commands.toolIntegrationDismiss(suggestion.hostId, suggestion.integrationId)
    onDeclined()
  }

  return (
    <div
      className="w-[340px] overflow-hidden rounded-lg border border-white/[0.08]
                 bg-sidebar/95 px-3 py-3 shadow-lg backdrop-blur-md"
      role="dialog"
    >
      <div className="flex items-start gap-2 pb-2">
        <span className="text-[12px] font-medium text-text-primary">
          {suggestion.name}
        </span>
        <span className="ml-auto rounded-full bg-accent-muted px-2 py-0.5 font-mono text-[9px] uppercase tracking-wide text-accent-text">
          integration
        </span>
      </div>
      {state.kind === 'installed' ? (
        <p className="text-[11px] leading-snug text-text-secondary">
          Installed. {suggestion.postInstallNote}
        </p>
      ) : state.kind === 'error' ? (
        <>
          <p className="text-[11px] leading-snug text-text-secondary">
            {suggestion.description}
          </p>
          <p className="mt-2 font-mono text-[10px] text-[color:var(--activity-failed)]">
            install failed: {state.message}
          </p>
          <div className="mt-2 flex items-center justify-end gap-2">
            <button
              type="button"
              onClick={onNotNow}
              className="rounded-md px-2 py-1 text-[11px] text-text-tertiary hover:text-text-secondary"
            >
              Dismiss
            </button>
            <button
              type="button"
              onClick={() => void onInstall()}
              className="rounded-md bg-accent-muted px-2 py-1 text-[11px] font-medium text-accent-text hover:bg-accent-muted/80"
            >
              Retry
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="text-[11px] leading-snug text-text-secondary">
            {suggestion.description}
          </p>
          <div className="mt-2 flex items-center justify-end gap-2">
            <button
              type="button"
              onClick={onNotNow}
              className="rounded-md px-2 py-1 text-[11px] text-text-tertiary hover:text-text-secondary"
              disabled={state.kind === 'installing'}
            >
              Not now
            </button>
            <button
              type="button"
              onClick={() => void onInstall()}
              disabled={state.kind === 'installing'}
              className="rounded-md bg-accent-muted px-2 py-1 text-[11px] font-medium text-accent-text hover:bg-accent-muted/80 disabled:opacity-50"
            >
              {state.kind === 'installing' ? 'Installing…' : 'Install'}
            </button>
          </div>
        </>
      )}
    </div>
  )
}
