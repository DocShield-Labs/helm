/**
 * Toggle — binary switch. Coral on, neutral off.
 *
 * Uses absolute + translate-y from the track's vertical center for the
 * thumb so the dot is always pixel-perfect regardless of track height
 * tweaks. The earlier `top-[3px]` form drifted off-center on retina
 * displays at certain zoom levels.
 */

export interface ToggleProps {
  checked: boolean
  onChange?: (checked: boolean) => void
  ariaLabel?: string
}

export function Toggle({ checked, onChange, ariaLabel }: ToggleProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      onClick={() => onChange?.(!checked)}
      className={`relative h-5 w-9 shrink-0 rounded-full transition-colors duration-[var(--duration-fast)]
                  ${checked ? 'bg-accent' : 'bg-white/[0.08] border border-white/[0.08]'}`}
    >
      <span
        className={`absolute top-1/2 left-0 block size-3.5 -translate-y-1/2 rounded-full
                    transition-transform duration-[var(--duration-fast)]
                    ${checked
                      ? 'translate-x-[19px] bg-text-inverse'
                      : 'translate-x-[3px] bg-text-secondary'}`}
      />
    </button>
  )
}
