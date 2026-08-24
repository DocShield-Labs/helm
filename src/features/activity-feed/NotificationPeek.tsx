/**
 * NotificationPeek — hover an inbox row and the session's recent output
 * slides down from the top of the terminal area.
 *
 * Source data is the session's model mirror (`lib/session/screen`): the
 * grid plus the last few history rows, paged in on demand. The peek
 * re-renders on every screen change while open, so a live session's
 * output keeps moving under the cursor.
 *
 * Two behaviours layered on the hover:
 *   - a short grace period on mouse-leave so the pointer can cross
 *     from the row into the panel;
 *   - a "merge" animation when the row is clicked: the panel lifts and
 *     blurs into the session that's taking over, then the peek closes.
 */

import { useEffect, useMemo, useRef } from 'react'
import { AnimatePresence, motion } from 'motion/react'
import { useStore } from '@lib/store'
import * as screen from '@lib/session/screen'

const PEEK_CLOSE_GRACE_MS = 120
const PEEK_MERGE_MS = 460
const PEEK_ROWS = 60

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

  // Make sure the session's grid and a few rows above it are known, then
  // follow every change while the peek is open.
  const peekHost = notif?.host_id ?? ''
  const peekSession = notif?.session_id ?? ''
  useEffect(() => {
    if (!peekHost || !peekSession) return
    void screen.ensureScreen(peekHost, peekSession).then(() => {
      const s = screen.getSessionScreen(peekHost, peekSession)
      void screen.ensureHistory(peekHost, peekSession, s.topLine - PEEK_ROWS)
    })
  }, [peekHost, peekSession])
  const version = screen.useScreenVersion(peekHost, peekSession)

  const text = useMemo(() => {
    if (!peekHost || !peekSession) return ''
    return screen.tailText(screen.getSessionScreen(peekHost, peekSession), PEEK_ROWS)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [peekHost, peekSession, version])

  let sessionLabel = ''
  if (notif) {
    const hs = sessions.get(notif.host_id)
    sessionLabel = hs?.sessions.get(notif.session_id)?.name ?? ''
  }

  // Don't peek at the session the user is already looking at.
  const activeMatchesPeek = useMemo(() => {
    if (!notif) return false
    if (activeHostId !== notif.host_id) return false
    const hs = sessions.get(notif.host_id)
    if (!hs) return false
    return hs.activeSessionId === notif.session_id
  }, [notif, activeHostId, sessions])

  const merging = !!mergingInboxId && mergingInboxId === peekedId

  useEffect(() => {
    if (!merging) return
    const t = window.setTimeout(() => {
      setMergingInboxId(null)
      setPeekedInboxId(null)
    }, PEEK_MERGE_MS)
    return () => window.clearTimeout(t)
  }, [merging, setMergingInboxId, setPeekedInboxId])

  const visible = !!notif && (!activeMatchesPeek || merging)

  return (
    <AnimatePresence>
      {visible && notif && (
        <motion.div
          key={notif.id}
          initial={{ y: '-100%', opacity: 0, scale: 1, filter: 'blur(0px)' }}
          animate={
            merging
              ? {
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
            <span>{sessionLabel || notif.session_id}</span>
          </div>
          {/* flex-col-reverse pins the pre to the visual bottom; when
              the panel hits maxHeight the body clips from the top so
              the latest output stays visible. */}
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
