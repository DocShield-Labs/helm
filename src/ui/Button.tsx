/**
 * Button — primary / secondary / tertiary, plus an optional keybind chip.
 */

import type { ReactNode } from 'react'

export type ButtonKind = 'primary' | 'secondary' | 'tertiary'

export interface ButtonProps {
  children: ReactNode
  kind?: ButtonKind
  kbHint?: string
  onClick?: () => void
  type?: 'button' | 'submit'
  disabled?: boolean
  fullWidth?: boolean
}

const KIND_CLASSES: Record<ButtonKind, string> = {
  primary:   'bg-accent text-text-inverse hover:bg-accent-hover',
  secondary: 'bg-white/[0.05] text-text-primary border border-white/[0.08] hover:bg-white/[0.08]',
  tertiary:  'bg-transparent text-text-primary border border-white/[0.06] hover:bg-white/[0.04]',
}

export function Button({
  children,
  kind = 'secondary',
  kbHint,
  onClick,
  type = 'button',
  disabled,
  fullWidth,
}: ButtonProps) {
  return (
    <button
      type={type}
      disabled={disabled}
      onClick={onClick}
      className={`flex h-9 items-center gap-3 rounded-md px-4 text-[13px] font-medium
                  ${KIND_CLASSES[kind]}
                  ${fullWidth ? 'w-full' : ''}
                  ${disabled ? 'opacity-50 cursor-not-allowed' : ''}
                  transition-colors duration-[var(--duration-fast)]`}
    >
      <span className="flex-1 text-left">{children}</span>
      {kbHint && (
        <span
          className={`font-mono rounded-sm px-1.5 py-0.5 text-[10px]
                      ${kind === 'primary' ? 'bg-black/20 text-black/70' : 'bg-white/[0.06] text-text-secondary'}`}
        >
          {kbHint}
        </span>
      )}
    </button>
  )
}
