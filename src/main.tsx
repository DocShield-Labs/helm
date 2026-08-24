import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'
import './index.css'

// Expose the typed commands surface on `window` so devtools pastes can
// drive Tauri without enabling `withGlobalTauri`. Cheap; harmless in prod.
;(window as unknown as { helm?: typeof commands }).helm = commands

// Dev helper: dump what the frontend store *thinks* is true.
;(window as unknown as { dbg?: unknown }).dbg = {
  state() {
    const s = useStore.getState()
    return {
      activeHostId: s.activeHostId,
      hosts: [...s.hosts.values()].map((h) => ({ id: h.id, name: h.name, port: h.port })),
      statuses: Object.fromEntries(s.statuses),
      sessions: Object.fromEntries(
        [...s.sessions.entries()].map(([hid, hs]) => [
          hid,
          {
            activeSessionId: hs.activeSessionId,
            sessions: [...hs.sessions.values()],
          },
        ]),
      ),
    }
  },
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
