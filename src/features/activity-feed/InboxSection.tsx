/**
 * INBOX — sidebar section above HOSTS.
 *
 * One row per live notification (oldest first). Click a row to jump to
 * the originating window; click × to dismiss. Hidden when empty so the
 * sidebar stays clean for users who haven't accumulated anything yet.
 *
 * Cross-host by design: the inbox surfaces "stuff piling up" across
 * every connected host, not just the active one. Click on a row in
 * another host auto-switches the active host before selecting the
 * window.
 *
 * Notifications are NOT dismissed on click — that's the "peek doesn't
 * dismiss" invariant. Dismissal happens via the × button or by typing
 * in the originating pane (4D, dismiss-on-keystroke). This way the
 * user can investigate without losing their list.
 */

import { useMemo } from 'react'
import { commands } from '@lib/ipc'
import { useStore, workspaceForWindow } from '@lib/store'
import { selectWorkspace } from '@lib/host'
import type { Host, Notification, NotificationKind } from '@bindings'

export function InboxSection() {
  const notifications = useStore((s) => s.notifications)
  const hosts = useStore((s) => s.hosts)
  const sessions = useStore((s) => s.sessions)
  const setActiveHost = useStore((s) => s.setActiveHost)
  const setActiveWindow = useStore((s) => s.setActiveWindow)

  // Sort oldest first — the user reads top-to-bottom and expects the
  // thing they were waiting on longest to be at the top. New events
  // append at the bottom.
  const list = useMemo(() => {
    const out = [...notifications.values()]
    out.sort((a, b) => a.created_at - b.created_at)
    return out
  }, [notifications])

  if (list.length === 0) return null

  const onJump = (n: Notification) => {
    setActiveHost(n.host_id)
    const hs = sessions.get(n.host_id)
    // Resolve workspace_id + window_id from whatever the notification
    // carries, falling back to the live tree via pane_id when the
    // backend's pane index didn't have the breadcrumbs (race during
    // initial connect or after a refresh skip).
    let workspaceId = n.workspace_id ?? null
    let windowId = n.window_id
    if (!workspaceId || !windowId) {
      if (hs) {
        for (const ws of hs.workspaces.values()) {
          const pane = ws.panes.get(n.pane_id)
          if (pane) {
            workspaceId = ws.id
            windowId = pane.windowId
            break
          }
        }
      }
    }
    if (!workspaceId && windowId) {
      const ws = workspaceForWindow(hs, windowId)
      workspaceId = ws?.id ?? null
    }
    if (workspaceId && windowId) {
      setActiveWindow(n.host_id, workspaceId, windowId)
      void selectWorkspace(n.host_id, workspaceId)
      void commands.tmuxSelectWindow(n.host_id, windowId)
    }
  }

  const onDismiss = (n: Notification) => {
    void commands.notificationDismiss(n.id)
  }

  const onClearAll = () => {
    // Optimistic — fire dismisses in parallel; the events come back
    // and remove them from the store. Wrapping in Promise.all isn't
    // necessary, the UI updates as each comes back.
    for (const n of list) void commands.notificationDismiss(n.id)
  }

  return (
    <div className="flex flex-col gap-1">
      <div className="group flex items-center justify-between pb-1 pt-1 pl-2 pr-1">
        <span className="text-[10px] font-medium tracking-[0.08em] text-text-tertiary">
          INBOX · {list.length}
        </span>
        <button
          type="button"
          onClick={onClearAll}
          className="rounded-sm px-1.5 font-mono text-[10px] leading-none text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100 hover:bg-white/[0.04] hover:text-text-secondary"
          title="Clear all notifications"
        >
          clear
        </button>
      </div>
      {list.map((n) => (
        <InboxRow
          key={n.id}
          notification={n}
          host={hosts.get(n.host_id)}
          windowName={resolveWindowName(sessions.get(n.host_id), n)}
          onJump={() => onJump(n)}
          onDismiss={() => onDismiss(n)}
        />
      ))}
      <div className="my-1 border-t border-white/[0.06]" />
    </div>
  )
}

interface InboxRowProps {
  notification: Notification
  host: Host | undefined
  windowName: string
  onJump: () => void
  onDismiss: () => void
}

function InboxRow({
  notification: n,
  host,
  windowName,
  onJump,
  onDismiss,
}: InboxRowProps) {
  const tone = notificationTone(n.kind)

  return (
    <button
      type="button"
      onClick={onJump}
      className="group relative flex w-full flex-col items-stretch gap-0.5 rounded-md px-2 py-1.5 text-left hover:bg-white/[0.04]"
    >
      <div className="flex items-center gap-2">
        <span
          className="size-2 shrink-0 rounded-full"
          style={{
            background: tone.color,
            opacity: 0.95,
            boxShadow: `0 0 4px 0 ${tone.color}80`,
          }}
        />
        <span className="flex-1 truncate text-[12px] text-text-primary">
          {windowName}
        </span>
        <span className="shrink-0 font-mono text-[10px] text-text-tertiary">
          {timeAgo(Date.now() - n.updated_at)}
          {n.count > 1 ? ` · ×${n.count}` : ''}
        </span>
        <span
          role="button"
          aria-label="Dismiss"
          onClick={(e) => {
            e.stopPropagation()
            onDismiss()
          }}
          className="rounded-sm px-1 font-mono text-[12px] leading-none text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100 hover:bg-white/[0.06] hover:text-text-secondary"
          title="Dismiss"
        >
          ×
        </span>
      </div>
      <div className="flex items-center gap-2 pl-4">
        <span className="truncate text-left font-mono text-[10px] text-text-tertiary">
          {host?.name ?? '?'} · {tone.label}
          {n.preview ? ` · ${n.preview}` : ''}
        </span>
      </div>
    </button>
  )
}

interface Tone {
  color: string
  label: string
}

/** Map a notification kind to its dot color + the inline label that
 * sits next to the breadcrumb ("bell", "exit 0", "exit 1"). */
function notificationTone(kind: NotificationKind): Tone {
  if (kind.kind === 'bell') {
    return {
      color: 'var(--activity-attention)',
      label: 'bell',
    }
  }
  // command_done
  const code = kind.exit_code
  if (code === null || code === undefined) {
    return {
      color: 'var(--activity-running)',
      label: kind.duration_ms != null ? `done · ${formatMs(kind.duration_ms)}` : 'done',
    }
  }
  if (code === 0) {
    return {
      color: 'var(--activity-running)',
      label:
        kind.duration_ms != null ? `exit 0 · ${formatMs(kind.duration_ms)}` : 'exit 0',
    }
  }
  return {
    color: 'var(--activity-failed)',
    label:
      kind.duration_ms != null
        ? `exit ${code} · ${formatMs(kind.duration_ms)}`
        : `exit ${code}`,
  }
}

/** "12s ago", "4m ago", "2h ago", "yesterday". Coarse enough that it
 * doesn't tick visibly while the user looks at it. */
function timeAgo(ms: number): string {
  if (ms < 10_000) return 'just now'
  if (ms < 60_000) return `${Math.round(ms / 1000)}s ago`
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m ago`
  if (ms < 86_400_000) return `${Math.round(ms / 3_600_000)}h ago`
  return 'yesterday'
}

function formatMs(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s`
  return `${(ms / 60_000).toFixed(1)}m`
}

/** Best-effort resolution: prefer the live tree (always current names)
 * via window_id, fall back to pane_id lookup when window_id wasn't
 * populated, fall back to the raw window_id, fall back to '?'. */
function resolveWindowName(
  hs: ReturnType<typeof useStore.getState>['sessions'] extends Map<infer _K, infer V>
    ? V | undefined
    : never,
  n: Notification,
): string {
  if (!hs) return n.window_id || '?'
  if (n.window_id) {
    for (const ws of hs.workspaces.values()) {
      const win = ws.windows.get(n.window_id)
      if (win) return `${ws.name} · ${win.name}`
    }
  }
  // Pane-based fallback for notifications whose backend lookup
  // raced and never got window_id populated.
  for (const ws of hs.workspaces.values()) {
    const pane = ws.panes.get(n.pane_id)
    if (pane) {
      const win = ws.windows.get(pane.windowId)
      return win ? `${ws.name} · ${win.name}` : `${ws.name} · ${pane.windowId}`
    }
  }
  return n.window_id || n.pane_id || '?'
}
