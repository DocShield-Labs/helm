/**
 * StatusDot — host connection state.
 * Mirrors the `StatusDot` component set in Figma.
 */

export type StatusDotState =
  | 'connected'
  | 'connecting'
  | 'disconnected'
  | 'error'
  | 'idle'

const COLOR_VAR: Record<StatusDotState, string> = {
  connected:    'var(--status-connected)',
  connecting:   'var(--status-connecting)',
  disconnected: 'var(--status-disconnected)',
  error:        'var(--status-error)',
  idle:         'var(--status-idle)',
}

const hasRing = (s: StatusDotState) => s === 'connecting' || s === 'error'
const hasGlow = (s: StatusDotState) => s === 'connected'

export function StatusDot({ state }: { state: StatusDotState }) {
  const color = COLOR_VAR[state]
  return (
    <span className="relative inline-block size-4 align-middle leading-none">
      {hasRing(state) && (
        <span
          className="absolute inset-0 rounded-full border"
          style={{ borderColor: color, opacity: 0.4 }}
        />
      )}
      <span
        className="absolute left-1 top-1 size-2 rounded-full"
        style={{
          background: color,
          boxShadow: hasGlow(state) ? `0 0 5px 0 ${color}99` : undefined,
        }}
      />
    </span>
  )
}
