/**
 * Modal — floating card with header / body / footer slots.
 * Real backdrop + keyboard handling lands when we wire the host editor.
 */

import type { ReactNode } from 'react'

export interface ModalProps {
  open: boolean
  title: string
  onClose?: () => void
  children?: ReactNode
  footer?: ReactNode
  width?: number
}

export function Modal({ open, title, onClose, children, footer, width = 560 }: ModalProps) {
  if (!open) return null
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55">
      <div
        role="dialog"
        aria-modal
        aria-label={title}
        style={{ width, boxShadow: 'var(--elevation-3)' }}
        className="rounded-xl border border-white/10 bg-elevated"
      >
        <header className="flex h-15 items-center gap-2 border-b border-white/[0.06] pl-6 pr-4">
          <h2 className="flex-1 text-[16px] font-semibold tracking-tight text-text-primary">
            {title}
          </h2>
          <button
            type="button"
            onClick={onClose}
            className="h-6 w-6 rounded-sm text-[16px] leading-none text-text-secondary hover:bg-white/[0.04]"
            aria-label="Close"
          >
            ×
          </button>
        </header>
        <div className="px-8 py-6">{children}</div>
        {footer && (
          <footer className="flex h-16 items-center gap-3 border-t border-white/[0.06] bg-sidebar px-6">
            {footer}
          </footer>
        )}
      </div>
    </div>
  )
}
