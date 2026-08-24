import { useEffect, useRef, useState } from 'react'
import { commandName } from '@lib/session/agents'
import { useStore } from '@lib/store'
import { Button, Input, Modal } from '@ui'

export function CustomAgentModal() {
  const open = useStore((state) => state.customAgentModalOpen)
  const savedTemplate = useStore((state) => state.customAgentTemplate)
  const close = useStore((state) => state.closeCustomAgentModal)
  const setTemplate = useStore((state) => state.setCustomAgentTemplate)
  const setDefault = useStore((state) => state.setDefaultAgentId)
  const [draft, setDraft] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (!open) return
    setDraft(savedTemplate)
    requestAnimationFrame(() => inputRef.current?.focus())
  }, [open, savedTemplate])

  const trimmed = draft.trim()
  const valid = !!commandName(trimmed) && commandName(trimmed) !== '{prompt}'
  const save = () => {
    if (!valid) return
    setTemplate(trimmed)
    setDefault('custom')
    close()
  }

  return (
    <Modal
      open={open}
      title="Custom agent"
      width={520}
      onClose={close}
      footer={
        <>
          <div className="flex-1" />
          <Button onClick={close}>Cancel</Button>
          <Button kind="primary" onClick={save} disabled={!valid}>Use agent</Button>
        </>
      }
    >
      <form
        onSubmit={(event) => {
          event.preventDefault()
          save()
        }}
        className="space-y-3"
      >
        <label htmlFor="custom-agent-command" className="block text-[12px] font-medium text-text-secondary">
          Launch command
        </label>
        <Input
          ref={inputRef}
          id="custom-agent-command"
          mono
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          placeholder="my-agent --profile work {prompt}"
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          invalid={trimmed.length > 0 && !valid}
        />
        <p className="text-[11px] leading-4 text-text-tertiary">
          Use <span className="font-mono text-text-secondary">{'{prompt}'}</span> where Helm should insert the message.
          If omitted, Helm appends the message to the command.
        </p>
      </form>
    </Modal>
  )
}
