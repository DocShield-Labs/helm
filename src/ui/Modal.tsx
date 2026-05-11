/**
 * Modal — floating card with header / body / footer slots.
 *
 * Body scrolls when content overflows; header + footer stay pinned. The
 * card itself caps at the viewport height with breathing room so it
 * never bleeds into the OS chrome on a small window. Esc closes the
 * modal — the listener is gated on `open` so multiple stacked modals
 * (rare, but the schedule editor opens over the palette in some flows)
 * each get their own handler and only the topmost is registered when
 * the others are closed.
 */

import { useEffect } from 'react'
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
  // Esc closes. Registered only while the modal is open so closed
  // modals don't intercept Esc from other consumers (palette, confirm
  // dialog, sidebar inline editors).
  useEffect(() => {
    if (!open || !onClose) return
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault()
        onClose()
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [open, onClose])

  if (!open) return null
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-8">
      <div
        role="dialog"
        aria-modal
        aria-label={title}
        style={{ width, maxHeight: 'calc(100vh - 64px)', boxShadow: 'var(--elevation-3)' }}
        className="flex flex-col rounded-xl border border-white/10 bg-elevated"
      >
        <header className="flex h-15 shrink-0 items-center gap-2 border-b border-white/[0.06] pl-6 pr-4">
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
        <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">{children}</div>
        {footer && (
          <footer className="flex h-16 shrink-0 items-center gap-3 border-t border-white/[0.06] bg-sidebar px-6">
            {footer}
          </footer>
        )}
      </div>
    </div>
  )
}
