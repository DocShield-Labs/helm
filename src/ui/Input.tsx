/**
 * Input — single-line text field. Mono variant for technical strings.
 */

import { forwardRef } from 'react'
import type { InputHTMLAttributes } from 'react'

export interface InputProps extends Omit<InputHTMLAttributes<HTMLInputElement>, 'size'> {
  mono?: boolean
  invalid?: boolean
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  { mono = false, invalid = false, className = '', ...props },
  ref,
) {
  return (
    <input
      ref={ref}
      className={`h-9 w-full rounded-md border px-3 text-[13px]
                  bg-sidebar text-text-primary placeholder:text-text-disabled
                  ${mono ? 'font-mono text-[12px]' : ''}
                  ${invalid ? 'border-status-disconnected' : 'border-white/[0.08]'}
                  focus:outline-none focus:border-accent
                  ${className}`}
      {...props}
    />
  )
})
