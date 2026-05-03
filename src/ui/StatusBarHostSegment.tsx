/**
 * StatusBarHostSegment — specialised segment carrying the StatusDot.
 * Reconnecting / offline append an inline label in the matching semantic colour
 * — this is how the app tells you what is happening, instead of a toast.
 */

import { StatusBarSegment } from './StatusBarSegment'
import { StatusDot, type StatusDotState } from './StatusDot'

export interface StatusBarHostSegmentProps {
  hostName: string
  state: StatusDotState
  onClick?: () => void
}

const INLINE_LABEL: Partial<Record<StatusDotState, { text: string; color: string }>> = {
  connecting:   { text: 'reconnecting…', color: 'var(--status-connecting)' },
  disconnected: { text: 'offline',       color: 'var(--status-disconnected)' },
  error:        { text: 'error',         color: 'var(--status-error)' },
}

export function StatusBarHostSegment({ hostName, state, onClick }: StatusBarHostSegmentProps) {
  const inline = INLINE_LABEL[state]
  return (
    <StatusBarSegment onClick={onClick}>
      <StatusDot state={state} />
      <span className="text-[11px] font-medium text-text-primary">{hostName}</span>
      {inline && (
        <span className="font-mono text-[10px] opacity-90" style={{ color: inline.color }}>
          · {inline.text}
        </span>
      )}
    </StatusBarSegment>
  )
}
