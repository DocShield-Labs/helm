/**
 * ActivityDot — per-pane state, rolled up to the window.
 * `none` renders an invisible 8×8 box so layout never shifts.
 *
 * State semantics:
 *   - `running`   : a command is currently executing in this window
 *                   (live signal sourced from OSC 133 markers). Renders
 *                   a soft grey dot with a slow breathe — work in
 *                   progress reads as quietly *alive*, not as a status
 *                   that competes for attention. Colour is reserved for
 *                   states that actually need the user (attention/failed).
 *   - `attention` : a bell fired, the user is being summoned.
 *   - `failed`    : the most recent finished command exited non-zero.
 *   - `completed` : the most recent finished command exited cleanly.
 *                   Named after the lifecycle phase, NOT live activity.
 *   - `idle`      : we have signal but it's stale.
 *   - `none`      : nothing to surface — render the invisible spacer.
 */

export type ActivityDotState =
  | 'running'
  | 'attention'
  | 'failed'
  | 'completed'
  | 'idle'
  | 'none'

// `--activity-running` is the legacy CSS token name for the blue
// "command finished, no error" colour. The state was renamed to
// `completed` in code; the token stays put to avoid touching tokens.css.
const COLOR_VAR: Record<Exclude<ActivityDotState, 'none' | 'running'>, string> = {
  attention: 'var(--activity-attention)',
  failed:    'var(--activity-failed)',
  completed: 'var(--activity-running)',
  idle:      'var(--activity-idle)',
}

export function ActivityDot({ state }: { state: ActivityDotState }) {
  if (state === 'none') {
    return <span className="inline-block size-2" />
  }
  if (state === 'running') {
    // Soft grey dot with a slow opacity breathe (keyframe in index.css).
    // Pure CSS — no JS timer, no per-row React renders; the animation
    // lives in the compositor even with many panes running at once.
    // `prefers-reduced-motion` settles it to a static dot. Shares the
    // idle grey so running/idle read as one calm family, distinguished
    // only by the breathe.
    return (
      <span className="relative inline-block size-2 align-middle leading-none">
        <span
          aria-hidden
          className="activity-running-pulse absolute left-px top-px size-1.5 rounded-full"
          style={{ background: 'var(--activity-idle)' }}
        />
      </span>
    )
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
