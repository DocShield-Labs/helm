/**
 * INBOX — sidebar section above HOSTS.
 *
 * One row per live notification (oldest first). Click a row to jump to
 * the originating session; click × to dismiss. Hidden when empty so the
 * sidebar stays clean for users who haven't accumulated anything yet.
 *
 * Cross-host by design: the inbox surfaces "stuff piling up" across
 * every connected host, not just the active one. Click on a row in
 * another host auto-switches the active host before selecting the
 * session.
 *
 * Notifications are NOT dismissed on click — that's the "peek doesn't
 * dismiss" invariant. Dismissal happens via the × button or by typing
 * in the originating session (4D, dismiss-on-keystroke). This way the
 * user can investigate without losing their list.
 */

import { useMemo, useRef } from 'react'
import { AnimatePresence, motion } from 'motion/react'
import { commands } from '@lib/ipc'
import { useStore, type HostSessions } from '@lib/store'
import { selectSession } from '@lib/host'
import { prettyCwd } from '@lib/path'
import type { Host, Notification, NotificationId, NotificationKind } from '@bindings'

export function InboxSection({ hideHeader = false }: { hideHeader?: boolean } = {}) {
  const notifications = useStore((s) => s.notifications)
  const hosts = useStore((s) => s.hosts)
  const sessions = useStore((s) => s.sessions)
  const activeHostId = useStore((s) => s.activeHostId)

  // Hover-peek glue: a single shared close timer at the section level
  // so quickly traversing rows doesn't flicker the preview.
  const setPeekedInboxId = useStore((s) => s.setPeekedInboxId)
  const peekTimer = useRef<number | null>(null)
  const cancelPeekClose = () => {
    if (peekTimer.current !== null) {
      window.clearTimeout(peekTimer.current)
      peekTimer.current = null
    }
  }
  const onPeekEnter = (id: NotificationId) => {
    cancelPeekClose()
    setPeekedInboxId(id)
  }
  const onPeekLeave = () => {
    cancelPeekClose()
    peekTimer.current = window.setTimeout(() => {
      setPeekedInboxId(null)
      peekTimer.current = null
    }, 120)
  }

  /** True when this notification's session is currently active. */
  const isSelected = (n: Notification): boolean => {
    if (activeHostId !== n.host_id) return false
    const hs = sessions.get(n.host_id)
    if (!hs) return false
    return hs.activeSessionId === n.session_id
  }

  // Newest first. New events drop onto the top of the stack from
  // above, pushing older items down — matches how the rest of the
  // app shows time-ordered surfaces (chat-style "latest at top").
  const list = useMemo(() => {
    const out = [...notifications.values()]
    out.sort((a, b) => b.created_at - a.created_at)
    return out
  }, [notifications])

  // We *always* render the wrapper + AnimatePresence so the section
  // doesn't unmount when the inbox empties. Unmounting kills
  // AnimatePresence's lifecycle memory — the next arriving notification
  // would mount fresh with initial={false} and skip its drop-in
  // animation. Conditionally rendering only the header and divider
  // keeps the visual identical to "return null" while preserving
  // animation continuity.
  const hasItems = list.length > 0

  const onJump = (n: Notification) => {
    // If the peek is currently showing this notification, hand off
    // to the merge animation: keep the panel visible while the new
    // session mounts behind it, then dissolve. NotificationPeek owns
    // the timer that clears both ids when the animation finishes.
    const state = useStore.getState()
    if (state.peekedInboxId === n.id) {
      state.setMergingInboxId(n.id)
    }
    if (sessions.get(n.host_id)?.sessions.has(n.session_id)) {
      selectSession(n.host_id, n.session_id)
    } else {
      state.setActiveHost(n.host_id)
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
      {hasItems && !hideHeader && (
        <div className="group flex items-center justify-between pb-1 pt-1 pl-2 pr-1">
          <span className="text-[11px] font-medium tracking-[0.08em] text-text-tertiary">
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
      )}
      <AnimatePresence initial={false}>
        {list.map((n) => (
          <motion.div
            key={n.id}
            // Enter: drop in from above, push items below down via the
            // height transition. Exit: slide left + fade + collapse so
            // the list closes the gap as it leaves.
            initial={{ opacity: 0, height: 0, y: -8 }}
            animate={{ opacity: 1, height: 'auto', y: 0 }}
            exit={{ opacity: 0, height: 0, x: -32 }}
            transition={{
              opacity: { duration: 0.18 },
              height: { duration: 0.22, ease: [0.2, 0.7, 0.2, 1] },
              y: { duration: 0.22, ease: [0.2, 0.7, 0.2, 1] },
              x: { duration: 0.18, ease: [0.4, 0, 1, 1] },
            }}
            // overflow-hidden so the height collapse clips the row's
            // own padding instead of letting it bleed during the
            // transition (otherwise rows look like they're squashing).
            style={{ overflow: 'hidden' }}
          >
            <InboxRow
              notification={n}
              host={hosts.get(n.host_id)}
              sessionName={resolveSessionName(sessions.get(n.host_id), n)}
              cwd={resolveSessionCwd(sessions.get(n.host_id), n)}
              selected={isSelected(n)}
              onJump={() => onJump(n)}
              onDismiss={() => onDismiss(n)}
              onPeekEnter={() => onPeekEnter(n.id)}
              onPeekLeave={onPeekLeave}
            />
          </motion.div>
        ))}
      </AnimatePresence>
      {hasItems && !hideHeader && <div className="my-1 border-t border-white/[0.06]" />}
    </div>
  )
}

interface InboxRowProps {
  notification: Notification
  host: Host | undefined
  sessionName: string
  cwd: string
  selected: boolean
  onJump: () => void
  onDismiss: () => void
  onPeekEnter: () => void
  onPeekLeave: () => void
}

function InboxRow({
  notification: n,
  host,
  sessionName,
  cwd,
  selected,
  onJump,
  onDismiss,
  onPeekEnter,
  onPeekLeave,
}: InboxRowProps) {
  const tone = notificationTone(n.kind)

  return (
    <button
      type="button"
      onClick={onJump}
      // Always fire — the suppression of "don't show peek when viewing
      // the same session" lives inside NotificationPeek's render
      // condition. Gating it here breaks the re-hover case: if the
      // mouse stays on the row while active flips back via keyboard
      // or sidebar click, mouseEnter never re-fires, so a row-level
      // gate would leave the peek silently disarmed until the user
      // does a manual mouse-out / mouse-in cycle.
      onMouseEnter={onPeekEnter}
      onMouseLeave={onPeekLeave}
      className={`group relative flex w-full flex-col items-stretch gap-0.5 rounded-md px-2 py-1.5 text-left ${
        selected ? 'bg-accent-muted' : 'hover:bg-white/[0.04]'
      }`}
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
        <span className="flex-1 truncate text-[14px] text-text-primary">
          {sessionName}
        </span>
        <span className="shrink-0 font-mono text-[11px] text-text-tertiary">
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
          <span
            className="truncate text-left font-mono text-[11px] text-text-tertiary"
            title={cwd || undefined}
          >
            {host?.name ?? '?'}
            {cwd ? ` · ${prettyCwd(cwd)}` : ''}
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
  if (kind.kind === 'message') {
    return {
      color: 'var(--activity-running)',
      label: kind.text,
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

/** Look up the originating session's working directory. */
function resolveSessionCwd(hs: HostSessions | undefined, n: Notification): string {
  return hs?.sessions.get(n.session_id)?.cwd ?? ''
}

/** Best-effort resolution from the live tree. */
function resolveSessionName(hs: HostSessions | undefined, n: Notification): string {
  return hs?.sessions.get(n.session_id)?.name ?? n.session_id ?? '?'
}
