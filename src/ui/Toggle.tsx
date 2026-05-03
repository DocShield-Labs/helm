/**
 * Toggle — binary switch. Coral on, neutral off.
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
      className={`relative inline-flex h-5 w-[34px] shrink-0 items-center rounded-full
                  transition-colors duration-[var(--duration-fast)]
                  ${checked ? 'bg-accent' : 'bg-white/[0.08] border border-white/[0.08]'}`}
    >
      <span
        className={`absolute top-[3px] block size-3.5 rounded-full transition-transform duration-[var(--duration-fast)]
                    ${checked ? 'translate-x-[17px] bg-black/85' : 'translate-x-[3px] bg-text-secondary'}`}
      />
    </button>
  )
}
