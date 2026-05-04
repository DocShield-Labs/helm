/**
 * StatusBarSegment — segment in the status bar. Interactive (button +
 * 4% white-tint hover) when an `onClick` is supplied, otherwise a flat
 * non-interactive `<div>` so segments without an action don't lie about
 * being clickable.
 */

import type { ReactNode } from 'react'

export interface StatusBarSegmentProps {
  children: ReactNode
  onClick?: () => void
  title?: string
}

export function StatusBarSegment({ children, onClick, title }: StatusBarSegmentProps) {
  if (!onClick) {
    return (
      <div title={title} className="flex h-6 items-center gap-2 px-3">
        {children}
      </div>
    )
  }
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
