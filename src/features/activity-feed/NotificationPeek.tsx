/**
 * NotificationPeek — hover-revealed snapshot of an inbox notification's
 * source window.
 *
 * Slides down from the top of the main pane area when the user hovers
 * an inbox row, showing the last visible text from that window's pane.
 * Dismisses on mouse-leave with a short grace period so quickly moving
 * between rows doesn't make it flicker.
 *
 * Source data is `paneCaptures` — already populated on connect by
 * helm-host's prehydrate pass and kept warm by live %output events,
 * so the peek is essentially free to render.
 */

import { useEffect, useRef, useState } from 'react'
import { AnimatePresence, motion } from 'motion/react'
import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'

const PEEK_CLOSE_GRACE_MS = 120

export function NotificationPeek() {
  const peekedId = useStore((s) => s.peekedInboxId)
  const setPeekedInboxId = useStore((s) => s.setPeekedInboxId)
  const notifications = useStore((s) => s.notifications)
  const paneCaptures = useStore((s) => s.paneCaptures)
  const sessions = useStore((s) => s.sessions)
  const hosts = useStore((s) => s.hosts)

  const closeTimer = useRef<number | null>(null)
  const cancelClose = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current)
      closeTimer.current = null
    }
  }
  const scheduleClose = () => {
    cancelClose()
    closeTimer.current = window.setTimeout(() => {
      setPeekedInboxId(null)
      closeTimer.current = null
    }, PEEK_CLOSE_GRACE_MS)
  }

  const notif = peekedId ? notifications.get(peekedId) : undefined
  const host = notif ? hosts.get(notif.host_id) : undefined
  const captureKey = notif ? `${notif.host_id}::${notif.pane_id}` : ''
  // The store's paneCaptures are populated on connect by the prehydrate
  // pass — they're a snapshot from then, not live. Live `%output` bytes
  // stream into xterm but don't write back here. Show the cached
  // capture for instant perceived response, then issue a fresh
  // tmux_capture_pane when the peek opens and swap in the up-to-date
  // text. The fetch is one cheap IPC call (single-digit ms locally,
  // RTT remote) so it's fine to fire on every hover.
  const cached = paneCaptures.get(captureKey)
  const [fresh, setFresh] = useState<{ key: string; data: string } | null>(null)

  useEffect(() => {
    if (!notif) return
    const key = `${notif.host_id}::${notif.pane_id}`
    let cancelled = false
    void commands.tmuxCapturePane(notif.host_id, notif.pane_id, 0).then((res) => {
      if (cancelled) return
      if (res.status === 'ok') setFresh({ key, data: res.data })
    })
    return () => { cancelled = true }
  }, [notif?.id, notif?.host_id, notif?.pane_id])

  const data =
    fresh && fresh.key === captureKey
      ? fresh.data
      : cached?.data ?? ''
  // Take a generous slice — the panel itself caps the visible height
  // via maxHeight + flex-col-reverse + overflow-hidden, so extra
  // lines just fill the buffer that gets clipped from the top.
  const text = data ? lastLines(stripAnsi(data), 60) : ''

  // Resolve a friendly "workspace · window" label from the live tree.
  let windowLabel = ''
  if (notif) {
    const hs = sessions.get(notif.host_id)
    if (hs && notif.window_id) {
      for (const ws of hs.workspaces.values()) {
        const win = ws.windows.get(notif.window_id)
        if (win) { windowLabel = `${ws.name} · ${win.name}`; break }
      }
    }
  }

  return (
    <AnimatePresence>
      {notif && (
        <motion.div
          // Keying on the notif id makes the peek crossfade between
          // sources rather than re-mounting (no animation jank when the
          // user moves from one row to the next).
          key={notif.id}
          initial={{ y: '-100%', opacity: 0 }}
          animate={{ y: 0, opacity: 1 }}
          exit={{ y: '-100%', opacity: 0 }}
          transition={{
            y: { duration: 0.22, ease: [0.2, 0.7, 0.2, 1] },
            opacity: { duration: 0.16 },
          }}
          onMouseEnter={cancelClose}
          onMouseLeave={scheduleClose}
          className="pointer-events-auto absolute left-2 right-2 top-2 z-30 flex flex-col overflow-hidden rounded-xl border border-white/[0.06] bg-elevated"
          style={{ boxShadow: 'var(--elevation-2)', maxHeight: 'calc(100% - 16px)' }}
        >
          <div className="flex shrink-0 items-center gap-2 border-b border-white/[0.04] px-4 pt-3 pb-2 font-mono text-[10px] tracking-[0.08em] text-text-tertiary">
            <span className="uppercase">peek</span>
            <span className="opacity-50">·</span>
            <span className="text-text-secondary">{host?.name ?? '?'}</span>
            <span className="opacity-50">·</span>
            <span>{windowLabel || notif.pane_id}</span>
          </div>
          {/* The body uses flex-col-reverse so the pre is positioned
              at the visual bottom of the available space. min-h-0
              + flex-shrink lets the body shrink below the pre's
              intrinsic height when the panel hits maxHeight, and
              overflow-hidden then clips the pre from the top — so
              the latest output (at the pre's bottom) stays visible.
              When content fits, the body sizes to the pre and the
              panel auto-sizes to content (no empty space). */}
          <div className="flex min-h-0 flex-col-reverse overflow-hidden">
            <pre className="m-0 whitespace-pre px-4 py-3 font-mono text-[11px] leading-[1.55] text-text-secondary">
              {text || 'No capture available yet.'}
            </pre>
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  )
}

/** Strip the most common ANSI sequences (CSI for color/cursor, OSC for
 * titles/hyperlinks) so the capture renders as plain text. We don't try
 * to be exhaustive — anything we miss just shows up as a control glyph,
 * which is rare in practice and only in edge cases like custom prompts
 * with unusual escape patterns. */
function stripAnsi(s: string): string {
  return s
    .replace(/\x1b\[[\d;?]*[a-zA-Z]/g, '')
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, '')
    .replace(/\x1b[()][0-9A-Z]/g, '')
    .replace(/\r/g, '')
}

function lastLines(s: string, n: number): string {
  const lines = s.split('\n')
  while (lines.length > 0 && lines[lines.length - 1].trim() === '') lines.pop()
  return lines.slice(-n).join('\n')
}
