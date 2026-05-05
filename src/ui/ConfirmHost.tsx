/**
 * ConfirmHost — bridges the store's `confirmPrompt` slot to the
 * `Modal` shell. Tauri 2 webviews no-op `window.confirm`, so any
 * destructive action that needs user assent goes through
 * `useStore.getState().requestConfirm({...})` and gets a Promise<boolean>
 * back; this component is what the user actually sees and clicks.
 *
 * Esc, the × button, and Cancel all resolve to `false`. Enter and the
 * affirmative button resolve to `true`.
 */

import { useEffect } from 'react'
import { useStore } from '@lib/store'
import { Modal } from './Modal'

export function ConfirmHost() {
  const prompt = useStore((s) => s.confirmPrompt)
  const resolve = useStore((s) => s.resolveConfirm)

  useEffect(() => {
    if (!prompt) return
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault()
        resolve(false)
      } else if (e.key === 'Enter') {
        e.preventDefault()
        resolve(true)
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [prompt, resolve])

  if (!prompt) return null
  const confirmLabel = prompt.confirmLabel ?? 'Confirm'
  const confirmBg = prompt.destructive
    ? 'bg-red-500/85 hover:bg-red-500 text-white'
    : 'bg-accent-muted hover:bg-accent-muted/80 text-text-primary'
  return (
    <Modal
      open
      title={prompt.title}
      onClose={() => resolve(false)}
      footer={
        <>
          <div className="flex-1" />
          <button
            type="button"
            onClick={() => resolve(false)}
            className="rounded-md border border-white/10 px-3 py-1.5 text-[13px] text-text-primary hover:bg-white/[0.04]"
          >
            Cancel
          </button>
          <button
            type="button"
            autoFocus
            onClick={() => resolve(true)}
            className={`rounded-md px-3 py-1.5 text-[13px] font-medium ${confirmBg}`}
          >
            {confirmLabel}
          </button>
        </>
      }
    >
      <p className="whitespace-pre-line text-[13px] leading-relaxed text-text-secondary">
        {prompt.message}
      </p>
    </Modal>
  )
}
