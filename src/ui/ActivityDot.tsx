/**
 * ActivityDot — per-pane state, rolled up to the window.
 * `none` renders an invisible 8×8 box so layout never shifts.
 */

export type ActivityDotState =
  | 'running'
  | 'attention'
  | 'failed'
  | 'idle'
  | 'none'

const COLOR_VAR: Record<Exclude<ActivityDotState, 'none'>, string> = {
  running:   'var(--activity-running)',
  attention: 'var(--activity-attention)',
  failed:    'var(--activity-failed)',
  idle:      'var(--activity-idle)',
}

export function ActivityDot({ state }: { state: ActivityDotState }) {
  if (state === 'none') {
    return <span className="inline-block size-2" />
  }
  const color = COLOR_VAR[state]
  return (
    <span className="relative inline-block size-2 align-middle leading-none">
      <span
        className="absolute left-px top-px size-1.5 rounded-full"
        style={{
          background: color,
          opacity: state === 'idle' ? 0.5 : 1,
          boxShadow: state === 'attention' ? `0 0 4px 0 ${color}8c` : undefined,
        }}
      />
    </span>
  )
}
