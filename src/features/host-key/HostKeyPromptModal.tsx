/**
 * Host-key trust prompt.
 *
 * Surfaces when an SSH server presents a key that's either unknown to
 * `~/.ssh/known_hosts` or differs from the recorded entry. The Rust
 * connect future is parked until the user picks Reject, Accept Once,
 * or Trust Permanently. Mounted once at the App level — the first
 * pending prompt in `useStore.hostKeyPrompts` is rendered.
 */

import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'
import { Button, Modal } from '@ui'
import type { HostKeyDecision } from '@bindings'

export function HostKeyPromptModal() {
  const prompts = useStore((s) => s.hostKeyPrompts)
  const clear = useStore((s) => s.clearHostKeyPrompt)
  // Surface one prompt at a time. SSH connects serialize per host, so
  // multiple prompts only happen across distinct hosts; queueing them
  // sequentially is fine.
  const next = prompts.values().next()
  const prompt = next.done ? null : next.value
  if (!prompt) return null

  const isChanged = prompt.kind.kind === 'changed'

  const respond = async (decision: HostKeyDecision) => {
    // Optimistically clear so the modal closes immediately. If the
    // command fails it'll error in the console — the connect future
    // either resolves on its own or times out, neither requires us
    // to bring the modal back.
    clear(prompt.hostId)
    const res = await commands.hostKeyPromptResponse(prompt.hostId, decision)
    if (res.status !== 'ok') {
      console.error('host-key response failed:', res.error)
    }
  }

  return (
    <Modal
      open
      title={isChanged ? 'Host key has changed' : 'Unknown host'}
      onClose={() => void respond('reject')}
      footer={
        <>
          <span className="flex-1" />
          <Button kind="tertiary" onClick={() => void respond('reject')}>
            Reject
          </Button>
          <Button kind="secondary" onClick={() => void respond('accept_once')}>
            Accept once
          </Button>
          {!isChanged && (
            <Button kind="primary" onClick={() => void respond('trust_permanently')}>
              Trust permanently
            </Button>
          )}
        </>
      }
    >
      <div className="flex flex-col gap-4 text-[13px] text-text-primary">
        <p className="text-text-secondary">
          {isChanged ? (
            <>
              The server's host key does not match the entry in{' '}
              <code className="font-mono text-[12px]">~/.ssh/known_hosts</code>
              {prompt.kind.kind === 'changed' && (
                <> (line {prompt.kind.previous_line})</>
              )}
              . This could mean the host was reinstalled — or that someone is
              intercepting your connection. Accept only if you have a reason
              to expect the change.
            </>
          ) : (
            <>
              This is the first time connecting to{' '}
              <span className="font-mono">{prompt.hostname}</span>. Verify the
              fingerprint out-of-band before trusting it.
            </>
          )}
        </p>

        <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 font-mono text-[12px]">
          <dt className="text-text-tertiary">Host</dt>
          <dd>
            {prompt.hostname}
            {prompt.port !== 22 && `:${prompt.port}`}
          </dd>
          <dt className="text-text-tertiary">Algorithm</dt>
          <dd>{prompt.algorithm}</dd>
          <dt className="text-text-tertiary">Fingerprint</dt>
          <dd className="break-all">{prompt.fingerprint}</dd>
        </dl>
      </div>
    </Modal>
  )
}
