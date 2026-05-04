/**
 * Bottom bar showing always-available palette controls. Static for now;
 * if a sub-mode wants a different hint set later, this can take a `hints`
 * prop and the host will pick.
 */

import { Kbd } from './Row'

export function Footer() {
  return (
    <div
      className="flex h-9 items-center gap-4 border-t px-[18px]"
      style={{
        background: 'var(--bg-sidebar)',
        borderColor: 'rgba(255,255,255,0.06)',
        color: 'var(--text-tertiary)',
        fontSize: 11,
      }}
    >
      <span className="flex items-center gap-1">
        <Kbd>↑↓</Kbd>navigate
      </span>
      <span className="flex items-center gap-1">
        <Kbd>↵</Kbd>run
      </span>
      <span className="flex-1" />
      <span className="flex items-center gap-1">
        <Kbd>esc</Kbd>close
      </span>
    </div>
  )
}
