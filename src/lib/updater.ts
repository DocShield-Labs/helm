/**
 * Self-update check.
 *
 * Polls the release manifest (GitHub Releases `latest.json`, configured
 * in tauri.conf.json → plugins.updater) on launch and every 6 hours.
 * When a newer signed build exists, the hook returns it and the status
 * bar renders an "update available" segment — nothing installs without
 * a click, so a live SSH session is never yanked out from under the
 * user by a surprise relaunch. Install = download + verify signature +
 * swap the .app + relaunch; tmux servers (local and remote) survive the
 * restart, so sessions reattach exactly where they were.
 *
 * Check failures (offline, rate-limited, endpoint missing) are
 * intentionally silent — a personal terminal app shouldn't nag about
 * its own update plumbing; we just try again next interval.
 */

import { useEffect, useState } from 'react'
import { check, type Update } from '@tauri-apps/plugin-updater'
import { relaunch } from '@tauri-apps/plugin-process'
import { useStore } from './store'

const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000

export interface AvailableUpdate {
  version: string
  installing: boolean
  install: () => void
}

export function useAppUpdate(): AvailableUpdate | null {
  const [update, setUpdate] = useState<Update | null>(null)
  const [installing, setInstalling] = useState(false)

  useEffect(() => {
    // Dev builds report tauri.conf.json's version and would nag about
    // every published release; updates only make sense for installed
    // bundles.
    if (import.meta.env.DEV) return
    let cancelled = false
    const run = async () => {
      try {
        const u = await check()
        if (!cancelled && u) setUpdate(u)
      } catch {
        // Silent by design — see module comment.
      }
    }
    void run()
    const id = window.setInterval(() => void run(), CHECK_INTERVAL_MS)
    return () => {
      cancelled = true
      window.clearInterval(id)
    }
  }, [])

  if (!update) return null
  return {
    version: update.version,
    installing,
    install: () => {
      if (installing) return
      setInstalling(true)
      update
        .downloadAndInstall()
        .then(() => relaunch())
        .catch((e) => {
          setInstalling(false)
          useStore.getState().pushToast({
            id: 'app-update-error',
            message: `Update to ${update.version} failed: ${String(e)}`,
            durationMs: 8_000,
          })
        })
    },
  }
}
