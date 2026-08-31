/**
 * App-chrome actions — verbs that don't belong to any single
 * host/session/palette: sidebar, undo, etc.
 */

import { recentErrors, sessionDiags } from '@lib/diag'
import { commands } from '@lib/ipc'
import { domAdvancePx, domLinePx } from '@lib/terminal/cellHeight'
import { useStore } from '@lib/store'
import type { Action } from './types'

/** The frontend's half of the diagnostics dump, merged next to the
 * Rust side's daemon-reported truth: window + font environment (the
 * classic machine-to-machine variable), the measured metrics every
 * sizing decision derives from, each mounted session's live geometry,
 * and the recent window-error ring. Best-effort throughout — a probe
 * that throws reports its failure rather than sinking the dump. */
function frontendDiagnostics(): Record<string, unknown> {
  const rootStyle = getComputedStyle(document.documentElement)
  const cssVar = (name: string) => rootStyle.getPropertyValue(name).trim() || null

  let advance: number | null = null
  try {
    advance = Math.round(domAdvancePx() * 1000) / 1000
  } catch {
    /* unmeasured */
  }

  // Which faces of the terminal's font stack this machine actually
  // resolves — a missing primary face changes every measured metric.
  let stack: string | null = null
  let faces: Array<{ family: string; available: boolean }> = []
  try {
    const probe = document.createElement('pre')
    probe.className = 'helm-block-output'
    probe.style.position = 'absolute'
    probe.style.visibility = 'hidden'
    document.body.appendChild(probe)
    stack = getComputedStyle(probe).fontFamily
    probe.remove()
    faces = stack
      .split(',')
      .map((f) => f.trim().replace(/^["']|["']$/g, ''))
      .map((family) => ({ family, available: document.fonts.check(`12px "${family}"`) }))
  } catch {
    /* leave nulls */
  }

  const state = useStore.getState()
  return {
    window: {
      inner_width: window.innerWidth,
      inner_height: window.innerHeight,
      device_pixel_ratio: window.devicePixelRatio,
    },
    fonts: {
      status: document.fonts.status,
      stack,
      faces,
    },
    metrics: {
      dom_line_px: domLinePx(),
      dom_advance_px: advance,
      pad_x: cssVar('--helm-pad-x'),
      bar_h: cssVar('--helm-bar-h'),
      bar_margin: cssVar('--helm-bar-margin'),
    },
    active: {
      host: state.activeHostId,
      session: state.activeHostId
        ? (state.sessions.get(state.activeHostId)?.activeSessionId ?? null)
        : null,
    },
    sessions: sessionDiags(),
    recent_errors: recentErrors(),
  }
}

export const chromeActions: Action[] = [
  {
    id: 'chrome.copy-diagnostics',
    kind: 'action',
    label: 'Copy diagnostics',
    icon: '⚕',
    run: () => {
      void (async () => {
        const push = useStore.getState().pushToast
        try {
          const res = await commands.diagnostics()
          if (res.status !== 'ok') throw new Error(res.error)
          let dump = res.data
          try {
            const merged = JSON.parse(res.data) as Record<string, unknown>
            merged.frontend = frontendDiagnostics()
            dump = JSON.stringify(merged, null, 2)
          } catch {
            /* the Rust half alone still beats nothing */
          }
          await navigator.clipboard.writeText(dump)
          push({
            id: 'diagnostics',
            message: 'Diagnostics copied — paste into a bug report.',
            durationMs: 5_000,
          })
        } catch (error) {
          push({
            id: 'diagnostics',
            message: `Couldn't gather diagnostics: ${String(error)}`,
            durationMs: 8_000,
          })
        }
      })()
    },
  },
  {
    id: 'chrome.toggle-sidebar',
    kind: 'action',
    label: 'Toggle sidebar',
    icon: '⏵',
    keybinding: 'Cmd+\\',
    run: () => {
      useStore.getState().toggleSidebar()
    },
  },
  {
    id: 'chrome.undo',
    kind: 'action',
    label: 'Undo last action',
    icon: '↶',
    keybinding: 'Cmd+z',
    canRun: () => {
      // Only enabled when there's an in-flight toast carrying a
      // deferredAction. Dismissing the toast cancels the timer — the
      // ToastHost cleanup effect handles that on its own.
      return useStore.getState().toasts.some((t) => t.deferredAction)
    },
    run: () => {
      const state = useStore.getState()
      for (let i = state.toasts.length - 1; i >= 0; i--) {
        const t = state.toasts[i]
        if (!t.deferredAction) continue
        try {
          t.action?.onClick()
        } catch {
          /* ignore */
        }
        state.dismissToast(t.id)
        return
      }
    },
  },
]
