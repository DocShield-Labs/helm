/**
 * NotificationPeek — hover-revealed snapshot of an inbox notification's
 * source window.
 *
 * Slides down from the top of the main pane area when the user hovers
 * an inbox row, showing the last visible text from that window's pane.
 * Dismisses on mouse-leave with a short grace period so quickly moving
 * between rows doesn't make it flicker.
 *
 * Source data is the pane's byte stream (`lib/session/stream`) — the
 * same buffer the pane renders from, so the peek is essentially free
 * and always current.
 *
 * Click-to-merge: when the user clicks the inbox row, the active pane
 * synchronously switches to the notification's source window. The new
 * pane is already mounted (BlockPane keep-alive) sitting underneath
 * the peek. Instead of dismissing the peek instantly, we hold it
 * visible and animate scale 1→1.02 + opacity 1→0 over 380ms so the
 * peek visually dissolves to reveal the live pane. The user perceives
 * one continuous transition rather than peek snap → blank → pane.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { AnimatePresence, motion } from 'motion/react'
import { useStore } from '@lib/store'
import * as stream from '@lib/session/stream'
import { lastLines, linesToText, renderAnsi } from '@lib/session/ansi'

const PEEK_CLOSE_GRACE_MS = 120
const PEEK_MERGE_MS = 460

export function NotificationPeek() {
  const peekedId = useStore((s) => s.peekedInboxId)
  const setPeekedInboxId = useStore((s) => s.setPeekedInboxId)
  const mergingInboxId = useStore((s) => s.mergingInboxId)
  const setMergingInboxId = useStore((s) => s.setMergingInboxId)
  const notifications = useStore((s) => s.notifications)
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
  // Re-render as the pane's stream grows while the peek is open, and
  // pull the pane's history if this is the first time we look at it.
  const [tick, setTick] = useState(0)
  const peekHost = notif?.host_id
  const peekPane = notif?.pane_id
  useEffect(() => {
    if (!peekHost || !peekPane) return
    let cancelled = false
    const unsub = stream.subscribeTail(peekHost, peekPane, () => setTick((t) => t + 1))
    void stream.ensureHistory(peekHost, peekPane).then(() => {
      if (!cancelled) setTick((t) => t + 1)
    })
    return () => {
      cancelled = true
      unsub()
    }
  }, [peekHost, peekPane])

  const text = useMemo(() => {
    if (!peekHost || !peekPane) return ''
    const end = stream.head(peekHost, peekPane)
    const from = Math.max(stream.start(peekHost, peekPane), end - 8192)
    const bytes = stream.slice(peekHost, peekPane, from, end)
    if (!bytes || bytes.length === 0) return ''
    return linesToText(lastLines(renderAnsi(new TextDecoder().decode(bytes)), 60))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [peekHost, peekPane, tick])

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
              {text || 'No output yet.'}
            </pre>
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  )
}
