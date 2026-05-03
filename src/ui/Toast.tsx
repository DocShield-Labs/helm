/**
 * Toast — a single dismissible notification with optional action.
 *
 * Typically rendered through `ToastHost`, which manages the deferred-action
 * timer and the stacking. Standalone here so we can compose it elsewhere
 * if we ever need an inline notification.
 */

interface ToastProps {
  message: string
  /** Optional action button (e.g. Undo). Clicking dismisses the toast. */
  action?: {
    label: string
    onClick: () => void
  }
  /** Progress 0..1 of the deferred-action countdown — drives the bottom
   * sliver indicator. Omit to render no progress. */
  progress?: number
}

export function Toast({ message, action, progress }: ToastProps) {
  return (
    <div className="relative overflow-hidden rounded-md border border-white/[0.08]
                    bg-sidebar/95 px-3 py-2 shadow-lg backdrop-blur-md
                    flex items-center gap-3 min-w-[280px]">
      <span className="flex-1 text-[12px] text-text-primary">{message}</span>
      {action && (
        <button
          type="button"
          onClick={action.onClick}
          className="text-[11px] font-medium text-accent hover:text-accent-hover
                     transition-colors duration-[var(--duration-fast)]"
        >
          {action.label}
        </button>
      )}
      {progress !== undefined && (
        <div
          className="absolute bottom-0 left-0 h-0.5 bg-accent/70"
          style={{ width: `${Math.max(0, Math.min(1, progress)) * 100}%` }}
        />
      )}
    </div>
  )
}
