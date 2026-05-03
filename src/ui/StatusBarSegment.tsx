/**
 * StatusBarSegment — generic clickable segment in the status bar.
 * Hover lifts the segment with a 4% white tint; never with the accent.
 */

import type { ReactNode } from 'react'

export interface StatusBarSegmentProps {
  children: ReactNode
  onClick?: () => void
  title?: string
}

export function StatusBarSegment({ children, onClick, title }: StatusBarSegmentProps) {
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      className="flex h-6 items-center gap-2 rounded-sm px-3 hover:bg-white/[0.04]"
    >
      {children}
    </button>
  )
}

export function StatusBarDivider() {
  return <span className="inline-block h-3 w-px bg-white/[0.08]" aria-hidden />
}
