/**
 * Stepper — horizontal progress indicator for multi-step modals.
 *
 * Rendered at the top of a wizard. Steps before the current one show
 * a filled accent circle with a check; the current shows a filled
 * accent circle with its number; steps after are hollow with their
 * number. The connecting line between adjacent steps is filled past
 * the active position.
 *
 * Click-to-jump is opt-in via `onJump` — use it for forms where the
 * user can revisit completed steps. Default is non-interactive, so
 * the stepper is purely informational.
 */

export interface StepperStep {
  id: string
  label: string
}

export interface StepperProps {
  steps: StepperStep[]
  /** Index of the currently-active step. */
  activeIndex: number
  /** Optional jump-to-step handler. When provided, completed and
   * future steps become clickable. */
  onJump?: (index: number) => void
}

export function Stepper({ steps, activeIndex, onJump }: StepperProps) {
  return (
    <div className="flex items-center gap-2">
      {steps.map((step, i) => {
        const isActive = i === activeIndex
        const isComplete = i < activeIndex
        const clickable = !!onJump
        return (
          <div key={step.id} className="flex flex-1 items-center gap-2">
            <button
              type="button"
              disabled={!clickable}
              onClick={() => onJump?.(i)}
              className={`flex flex-1 items-center gap-2 rounded-md px-2 py-1 text-left transition-colors
                          ${clickable ? 'hover:bg-white/[0.04]' : ''}`}
            >
              <span
                className={`flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-[11px] font-medium
                            transition-colors duration-[var(--duration-fast)]
                            ${isActive
                              ? 'bg-accent text-text-inverse'
                              : isComplete
                                ? 'bg-accent/40 text-text-primary'
                                : 'bg-white/[0.06] text-text-tertiary'}`}
              >
                {isComplete ? '✓' : i + 1}
              </span>
              <span
                className={`text-[12px] font-medium tracking-tight transition-colors duration-[var(--duration-fast)]
                            ${isActive
                              ? 'text-text-primary'
                              : isComplete
                                ? 'text-text-secondary'
                                : 'text-text-tertiary'}`}
              >
                {step.label}
              </span>
            </button>
            {i < steps.length - 1 && (
              <div
                className={`h-px flex-1 transition-colors duration-[var(--duration-fast)]
                            ${i < activeIndex ? 'bg-accent/40' : 'bg-white/[0.06]'}`}
              />
            )}
          </div>
        )
      })}
    </div>
  )
}
