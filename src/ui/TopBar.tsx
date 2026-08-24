/** Full-width draggable title bar with navigation and search controls. */

import { useStore } from '@lib/store'
import type { AvailableUpdate } from '@lib/updater'
import { PanelLeftIcon, SearchIcon } from '@features/sessions/icons'
import { TITLE_BAR_HEIGHT, TITLE_BAR_CONTENT_INSET } from '@bindings'

export function TopBar({ title, update }: { title: string; update: AvailableUpdate | null }) {
  const collapsed = useStore((s) => s.sidebarCollapsed)
  const toggleSidebar = useStore((s) => s.toggleSidebar)
  const openPalette = useStore((s) => s.openPalette)
  return (
    <div
      data-tauri-drag-region
      // Both numbers are owned by the Rust side (titlebar.rs): it centres
      // macOS's traffic lights on this bar's midline and reserves the room
      // they need on the left. Changing them there moves both sides together.
      style={{ height: TITLE_BAR_HEIGHT, paddingLeft: TITLE_BAR_CONTENT_INSET }}
      className="relative flex shrink-0 items-center gap-2 border-b border-[var(--stroke-default)] pr-2"
    >
      <button
        type="button"
        onClick={toggleSidebar}
        title={`${collapsed ? 'Show' : 'Hide'} sidebar (⌘\\)`}
        className="flex size-6 shrink-0 items-center justify-center rounded-md text-text-tertiary hover:bg-[var(--stroke-subtle)] hover:text-text-primary"
      >
        <PanelLeftIcon size={15} />
      </button>
      {/* The sidebar already names the session (the selected card) and
          its host (the group header), so the title would just repeat it.
          With the sidebar collapsed it's the only thing that does. */}
      {collapsed && (
        <span data-tauri-drag-region className="truncate text-[12px] text-text-tertiary">
          {title}
        </span>
      )}
      <button
        type="button"
        onClick={() => openPalette('/')}
        title="Search output across hosts (⌘K then /)"
        className="absolute left-1/2 top-1/2 flex h-7 w-[320px] max-w-[38vw] -translate-x-1/2 -translate-y-1/2 items-center gap-2 rounded-md bg-[var(--stroke-subtle)] px-3 text-text-tertiary hover:bg-[var(--stroke-default)]"
      >
        <SearchIcon size={14} />
        <span className="flex-1 truncate text-left text-[13px]">Search output across hosts…</span>
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
    </div>
  )
}
