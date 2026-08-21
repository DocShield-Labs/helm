/**
 * Top bar over the pane area: 34px + hairline, draggable, a centered
 * search pill that opens ⌘K, the sidebar toggle at the right. When the
 * sidebar is hidden the traffic lights sit here, so the left reserves
 * room for them.
 */

import { useStore } from '@lib/store'
import type { AvailableUpdate } from '@lib/updater'
import { PanelLeftIcon, SearchIcon } from '@features/sessions/icons'

export function TopBar({ title, update }: { title: string; update: AvailableUpdate | null }) {
  const collapsed = useStore((s) => s.sidebarCollapsed)
  const toggleSidebar = useStore((s) => s.toggleSidebar)
  const openPalette = useStore((s) => s.openPalette)
  return (
    <div
      data-tauri-drag-region
      className="relative flex h-[35px] shrink-0 items-center border-b border-[var(--stroke-default)] pr-2"
      style={{ paddingLeft: collapsed ? 80 : 12 }}
    >
      <span data-tauri-drag-region className="truncate text-[12px] text-text-tertiary">
        {title}
      </span>
      <button
        type="button"
        onClick={() => openPalette()}
        className="absolute left-1/2 top-1/2 flex h-6 w-[300px] max-w-[40vw] -translate-x-1/2 -translate-y-1/2 items-center gap-2 rounded-md bg-[var(--stroke-subtle)] px-2.5 text-text-tertiary hover:bg-[var(--stroke-default)]"
      >
        <SearchIcon size={13} />
        <span className="flex-1 truncate text-left text-[12px]">Search sessions, agents, output…</span>
        <span className="font-mono text-[10px] text-text-disabled">⌘K</span>
      </button>
      <span className="flex-1" />
      {update && (
        <button
          type="button"
          onClick={update.installing ? undefined : update.install}
          title={`Install Helm ${update.version} and relaunch — your sessions survive`}
          className="mr-1 rounded-md bg-accent-muted px-2 py-0.5 text-[11px] text-accent-text hover:bg-[var(--accent-border)]"
        >
          {update.installing ? `installing ${update.version}…` : `${update.version} available`}
        </button>
      )}
      <button
        type="button"
        onClick={toggleSidebar}
        title={`${collapsed ? 'Show' : 'Hide'} sidebar (⌘\\)`}
        className="flex size-6 items-center justify-center rounded-md text-text-tertiary hover:bg-[var(--stroke-subtle)] hover:text-text-primary"
      >
        <PanelLeftIcon size={15} />
      </button>
    </div>
  )
}
