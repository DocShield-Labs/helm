/**
 * Machine diagnostics: a window-error ring plus per-session geometry
 * probes, merged into the ⌘K "Copy diagnostics" dump next to the Rust
 * side's daemon-reported truth. The goal is ONE authoritative paste
 * when a bug shows on one machine and not another — every number the
 * terminal's sizing chain depends on (fonts, cell heights, container
 * boxes, PTY dims on all three sides) in a single snapshot, instead of
 * a piecemeal back-and-forth.
 */

interface CapturedError {
  ts: string
  kind: 'error' | 'unhandledrejection'
  message: string
}

const errors: CapturedError[] = []
const MAX_ERRORS = 50

function pushError(kind: CapturedError['kind'], message: string): void {
  errors.push({ ts: new Date().toISOString(), kind, message: message.slice(0, 500) })
  if (errors.length > MAX_ERRORS) errors.shift()
}

/** Start collecting window errors and unhandled rejections. Called once
 * at boot; the ring feeds the diagnostics dump. */
export function installErrorCapture(): void {
  window.addEventListener('error', (e) => {
    pushError('error', e.message || String(e.error ?? 'unknown'))
  })
  window.addEventListener('unhandledrejection', (e) => {
    pushError('unhandledrejection', String(e.reason ?? 'unknown'))
  })
}

export function recentErrors(): CapturedError[] {
  return [...errors]
}

/** A mounted session view's live geometry snapshot. Registered by
 * SessionView; read at dump time so the numbers are current, not
 * captured at mount. */
export type SessionDiagProbe = () => Record<string, unknown>

const probes = new Map<string, SessionDiagProbe>()

export function registerSessionDiag(key: string, probe: SessionDiagProbe): () => void {
  probes.set(key, probe)
  return () => {
    probes.delete(key)
  }
}

export function sessionDiags(): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  for (const [key, probe] of probes) {
    try {
      out[key] = probe()
    } catch (error) {
      // A probe must never sink the dump — the failure itself is data.
      out[key] = { probe_error: String(error) }
    }
  }
  return out
}
