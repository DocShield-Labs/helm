/**
 * ToastHost — bottom-right portal that renders the active toast stack and
 * manages deferred-action timers.
 *
 * Each toast may carry a `deferredAction` (function) and a `durationMs`. If
 * present, the action fires once `durationMs` has elapsed and the toast
 * gets dismissed. Clicking an action button (e.g. Undo) dismisses the toast
 * before the timer fires, which cancels the deferred action.
 *
 * Mount once near the root of the app. Reads from `useStore.toasts`.
 */

import { useEffect, useState } from 'react'
import { useStore } from '@lib/store'
import { Toast } from './Toast'

export function ToastHost() {
  const toasts = useStore((s) => s.toasts)
  const dismissToast = useStore((s) => s.dismissToast)

  // 60Hz tick used to drive the countdown sliver. Cheap; only running while
  // there's at least one timed toast in flight.
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const hasTimed = toasts.some((t) => t.durationMs)
    if (!hasTimed) return
    let raf = 0
    const loop = () => {
      setNow(Date.now())
      raf = requestAnimationFrame(loop)
    }
    raf = requestAnimationFrame(loop)
    return () => cancelAnimationFrame(raf)
  }, [toasts])

  // One timeout per timed toast. The cleanup cancels timers when a toast
  // is dismissed (the toasts list shrinks → the effect re-runs).
  useEffect(() => {
    const timers: number[] = []
    for (const t of toasts) {
      if (!t.durationMs) continue
      const remaining = Math.max(0, t.startedAt + t.durationMs - Date.now())
      const id = window.setTimeout(() => {
        try {
          t.deferredAction?.()
        } catch (e) {
          console.error('toast deferred action threw:', e)
        }
        dismissToast(t.id)
      }, remaining)
      timers.push(id)
    }
    return () => {
      for (const id of timers) clearTimeout(id)
    }
  }, [toasts, dismissToast])

  if (toasts.length === 0) return null

  return (
    <div className="pointer-events-none fixed bottom-4 right-4 z-50 flex flex-col-reverse gap-2">
      {toasts.map((t) => {
        const progress = t.durationMs
          ? 1 - Math.min(1, Math.max(0, (now - t.startedAt) / t.durationMs))
          : undefined
        return (
          <div key={t.id} className="pointer-events-auto">
            <Toast
              message={t.message}
              action={
                t.action && {
                  label: t.action.label,
                  onClick: () => {
                    try {
                      t.action?.onClick()
                    } finally {
                      dismissToast(t.id)
                    }
                  },
                }
              }
              progress={progress}
            />
          </div>
        )
      })}
    </div>
  )
}
