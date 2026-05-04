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
 *
 * Click-to-merge: when the user clicks the inbox row, the active pane
 * synchronously switches to the notification's source window. The new
 * pane is already mounted (TmuxPane keep-alive) sitting underneath
 * the peek. Instead of dismissing the peek instantly, we hold it
 * visible and animate scale 1→1.02 + opacity 1→0 over 380ms so the
 * peek visually dissolves to reveal the live pane. The user perceives
 * one continuous transition rather than peek snap → blank → pane.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { AnimatePresence, motion } from 'motion/react'
import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'

const PEEK_CLOSE_GRACE_MS = 120
const PEEK_MERGE_MS = 460

export function NotificationPeek() {
  const peekedId = useStore((s) => s.peekedInboxId)
  const setPeekedInboxId = useStore((s) => s.setPeekedInboxId)
  const mergingInboxId = useStore((s) => s.mergingInboxId)
  const setMergingInboxId = useStore((s) => s.setMergingInboxId)
  const notifications = useStore((s) => s.notifications)
  const paneCaptures = useStore((s) => s.paneCaptures)
  const sessions = useStore((s) => s.sessions)
  const hosts = useStore((s) => s.hosts)
  const activeHostId = useStore((s) => s.activeHostId)

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

  // True when the peeked notification's window is the user's currently
  // active pane. Suppresses the peek render in the "you're already
  // looking at it" case — the user gets no value from a snapshot of
  // the same content.
  const activeMatchesPeek = useMemo(() => {
    if (!notif) return false
    if (activeHostId !== notif.host_id) return false
    const hs = sessions.get(notif.host_id)
    if (!hs) return false
    const ws = hs.activeWorkspaceId ? hs.workspaces.get(hs.activeWorkspaceId) : undefined
    if (!ws) return false
    for (const w of ws.windows.values()) {
      if (w.active && w.id === notif.window_id) return true
    }
    return false
  }, [notif, activeHostId, sessions])

  // Merge fires when InboxSection's onJump explicitly tags this
  // notification as "the click that just happened was on a row
  // whose peek was visible." We don't infer it from active-pane
  // changes — that ambiguates "user clicked" from "user navigated
  // away while still hovering" — so the click handler in InboxSection
  // is the single source of truth for the merge intent.
  const merging = !!mergingInboxId && mergingInboxId === peekedId

  useEffect(() => {
    if (!merging) return
    const t = window.setTimeout(() => {
      setMergingInboxId(null)
      setPeekedInboxId(null)
    }, PEEK_MERGE_MS)
    return () => window.clearTimeout(t)
  }, [merging, setMergingInboxId, setPeekedInboxId])

  // Show the peek when there's a hovered notification, with two carve-
  // outs: hide it when the user is already on that pane (nothing new
  // to show), and force-show during the merge so the dissolve has
  // something to dissolve.
  const visible = !!notif && (!activeMatchesPeek || merging)

  return (
    <AnimatePresence>
      {visible && notif && (
        <motion.div
          // Keying on the notif id makes the peek crossfade between
          // sources rather than re-mounting (no animation jank when the
          // user moves from one row to the next).
          key={notif.id}
          initial={{ y: '-100%', opacity: 0, scale: 1, filter: 'blur(0px)' }}
          animate={
            merging
              ? {
                  // The dissolve. A larger scale-up + a real upward
                  // float + an 8px Gaussian blur give the panel a
                  // sense of dissipating into the air, not just fading.
                  // Combined with the opacity ramp, the eye reads it as
                  // "this thing got absorbed into the world" rather
                  // than "the layer turned off."
                  y: -12,
                  opacity: 0,
                  scale: 1.06,
                  filter: 'blur(8px)',
                }
              : { y: 0, opacity: 1, scale: 1, filter: 'blur(0px)' }
          }
          exit={{ y: '-100%', opacity: 0, scale: 1, filter: 'blur(0px)' }}
          transition={
            merging
              ? {
                  // Slightly longer than a "feedback" animation so the
                  // dissolve has weight — the eye registers the
                  // multi-property change over ~half a second. EaseOut
                  // on the back end so the panel decelerates into
                  // nothing, mirroring how a vapor trail fades.
                  duration: PEEK_MERGE_MS / 1000,
                  ease: [0.32, 0, 0.4, 1],
                }
              : {
                  y: { duration: 0.22, ease: [0.2, 0.7, 0.2, 1] },
                  opacity: { duration: 0.16 },
                }
          }
          onMouseEnter={merging ? undefined : cancelClose}
          onMouseLeave={merging ? undefined : scheduleClose}
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
