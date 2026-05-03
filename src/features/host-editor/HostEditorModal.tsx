/**
 * HostEditorModal — add or edit a remote host.
 *
 * Fields map directly to the `Host` domain type plus a transient
 * password (which goes to Keychain via `host_save_password`, never
 * back into the `Host` record itself).
 *
 * SSH-config autocomplete: when the user types into the hostname field,
 * suggestions come from `~/.ssh/config` aliases. Picking one fills in
 * the resolved hostname, user, and port — saves typing for hosts the
 * user already has set up for `ssh`.
 */

import { useEffect, useMemo, useState } from 'react'
import { commands } from '@lib/ipc'
import { Button, Input, Modal } from '@ui'
import type { Host, SshConfigAlias } from '@bindings'

type AuthChoice = 'agent' | 'keyfile' | 'password'

export interface HostEditorModalProps {
  open: boolean
  /** When provided, the modal is in "edit" mode for that host. */
  initial?: Host
  onClose: () => void
  /** Fired after a successful save. The id is freshly-generated for new
   * hosts, or the existing id for edits. */
  onSaved: (id: string) => void
}

export function HostEditorModal({ open, initial, onClose, onSaved }: HostEditorModalProps) {
  const isEdit = !!initial

  const [name, setName] = useState('')
  const [hostname, setHostname] = useState('')
  const [user, setUser] = useState('')
  const [port, setPort] = useState('22')
  const [authChoice, setAuthChoice] = useState<AuthChoice>('agent')
  const [keyPath, setKeyPath] = useState('~/.ssh/id_ed25519')
  const [password, setPassword] = useState('')
  const [defaultWorkspace, setDefaultWorkspace] = useState('main')
  const [aliases, setAliases] = useState<SshConfigAlias[]>([])
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  // Reset form whenever the modal opens. Loading from `initial` for
  // edits, sensible defaults for new hosts.
  useEffect(() => {
    if (!open) return
    setError(null)
    setBusy(false)
    if (initial) {
      setName(initial.name)
      setHostname(initial.hostname)
      setUser(initial.user)
      setPort(String(initial.port || 22))
      setDefaultWorkspace(initial.default_workspace)
      setPassword('')
      if (initial.auth === 'Agent') setAuthChoice('agent')
      else if (initial.auth === 'Password') setAuthChoice('password')
      else if (typeof initial.auth === 'object' && 'KeyFile' in initial.auth) {
        setAuthChoice('keyfile')
        setKeyPath(initial.auth.KeyFile.path)
      }
    } else {
      setName('')
      setHostname('')
      setUser(systemUser())
      setPort('22')
      setAuthChoice('agent')
      setKeyPath('~/.ssh/id_ed25519')
      setPassword('')
      setDefaultWorkspace('main')
    }
  }, [open, initial])

  // Pull `~/.ssh/config` aliases for autocomplete on first open.
  useEffect(() => {
    if (!open) return
    void (async () => {
      const res = await commands.sshConfigAliases()
      if (res.status === 'ok') setAliases(res.data)
    })()
  }, [open])

  const matchedAlias = useMemo(
    () => aliases.find((a) => a.alias === hostname),
    [aliases, hostname],
  )

  const applyAlias = (alias: SshConfigAlias) => {
    setHostname(alias.hostname ?? alias.alias)
    if (alias.user) setUser(alias.user)
    if (alias.port) setPort(String(alias.port))
    if (!name) setName(alias.alias)
  }

  const valid =
    name.trim().length > 0 &&
    hostname.trim().length > 0 &&
    user.trim().length > 0 &&
    /^\d+$/.test(port.trim()) &&
    parseInt(port, 10) > 0 &&
    parseInt(port, 10) <= 65535 &&
    defaultWorkspace.trim().length > 0 &&
    (authChoice !== 'keyfile' || keyPath.trim().length > 0) &&
    (authChoice !== 'password' || isEdit || password.length > 0)
  // For edit mode we tolerate an empty password — user might just be
  // tweaking metadata without changing the secret. New hosts using
  // password auth must supply one so connect actually works later.

  async function save() {
    if (!valid) return
    setBusy(true)
    setError(null)
    try {
      const id = initial?.id ?? crypto.randomUUID()
      const auth =
        authChoice === 'agent'
          ? 'Agent'
          : authChoice === 'password'
            ? 'Password'
            : ({ KeyFile: { path: keyPath.trim() } } as const)
      const host: Host = {
        id,
        name: name.trim(),
        hostname: hostname.trim(),
        port: parseInt(port, 10),
        user: user.trim(),
        auth,
        jump_host: initial?.jump_host ?? null,
        tmux_integration: true,
        default_workspace: defaultWorkspace.trim(),
        startup_commands: initial?.startup_commands ?? [],
      }
      const res = await commands.hostSave(host)
      if (res.status !== 'ok') {
        setError(res.error)
        return
      }
      // Keychain only when the user actually typed a password. Empty
      // string in edit mode means "leave the existing entry alone."
      if (authChoice === 'password' && password.length > 0) {
        const pwRes = await commands.hostSavePassword(id, password)
        if (pwRes.status !== 'ok') {
          setError(`saved host but Keychain failed: ${pwRes.error}`)
          return
        }
      }
      onSaved(id)
      onClose()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Modal
      open={open}
      title={isEdit ? `Edit ${initial?.name ?? 'host'}` : 'Add host'}
      onClose={onClose}
      width={520}
      footer={
        <>
          <span className="flex-1 text-[12px] text-status-error">{error ?? ''}</span>
          <Button kind="tertiary" onClick={onClose}>
            Cancel
          </Button>
          <Button kind="primary" onClick={save} disabled={!valid || busy}>
            {busy ? 'Saving…' : isEdit ? 'Save' : 'Add'}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-4">
        <Field label="Name">
          <Input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="prod-api"
            autoFocus
          />
        </Field>

        <Field
          label="Hostname"
          hint={
            matchedAlias
              ? `using ${matchedAlias.hostname ?? matchedAlias.alias}${
                  matchedAlias.user ? ` (user ${matchedAlias.user})` : ''
                }${matchedAlias.port ? ` :${matchedAlias.port}` : ''}`
              : aliases.length > 0
                ? `${aliases.length} ssh config aliases available`
                : undefined
          }
        >
          <Input
            mono
            list="ssh-aliases"
            value={hostname}
            onChange={(e) => {
              const v = e.target.value
              setHostname(v)
              const alias = aliases.find((a) => a.alias === v)
              if (alias) applyAlias(alias)
            }}
            placeholder="prod.example.com"
          />
          <datalist id="ssh-aliases">
            {aliases.map((a) => (
              <option key={a.alias} value={a.alias}>
                {a.hostname ?? a.alias}
              </option>
            ))}
          </datalist>
        </Field>

        <div className="grid grid-cols-[1fr_140px] gap-4">
          <Field label="User">
            <Input
              mono
              value={user}
              onChange={(e) => setUser(e.target.value)}
              placeholder="azhar"
            />
          </Field>
          <Field label="Port">
            <Input
              mono
              value={port}
              onChange={(e) => setPort(e.target.value)}
              placeholder="22"
            />
          </Field>
        </div>

        <Field label="Auth">
          <div className="flex gap-2">
            <AuthRadio
              checked={authChoice === 'agent'}
              onChange={() => setAuthChoice('agent')}
              label="Agent"
              hint="ssh-agent"
            />
            <AuthRadio
              checked={authChoice === 'keyfile'}
              onChange={() => setAuthChoice('keyfile')}
              label="Key file"
              hint="private key"
            />
            <AuthRadio
              checked={authChoice === 'password'}
              onChange={() => setAuthChoice('password')}
              label="Password"
              hint="Keychain"
            />
          </div>
        </Field>

        {authChoice === 'keyfile' && (
          <Field label="Key file">
            <Input
              mono
              value={keyPath}
              onChange={(e) => setKeyPath(e.target.value)}
              placeholder="~/.ssh/id_ed25519"
            />
          </Field>
        )}

        {authChoice === 'password' && (
          <Field
            label="Password"
            hint={
              isEdit
                ? 'leave blank to keep the existing Keychain entry'
                : 'stored only in your macOS Keychain'
            }
          >
            <Input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder={isEdit ? '••••••••' : 'required for new hosts'}
            />
          </Field>
        )}

        <Field
          label="Default workspace"
          hint="created on first connect if no tmux sessions already exist"
        >
          <Input
            mono
            value={defaultWorkspace}
            onChange={(e) => setDefaultWorkspace(e.target.value)}
            placeholder="main"
          />
        </Field>
      </div>
    </Modal>
  )
}

function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: React.ReactNode
}) {
  return (
    <label className="flex flex-col gap-1.5">
      <span className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.06em] text-text-tertiary">
        {label}
        {hint && (
          <span className="font-normal normal-case tracking-normal text-[10px] text-text-tertiary">
            {hint}
          </span>
        )}
      </span>
      {children}
    </label>
  )
}

function AuthRadio({
  checked,
  onChange,
  label,
  hint,
}: {
  checked: boolean
  onChange: () => void
  label: string
  hint: string
}) {
  return (
    <button
      type="button"
      onClick={onChange}
      className={`flex flex-1 flex-col gap-0.5 rounded-md border px-3 py-2 text-left
                  ${
                    checked
                      ? 'border-accent bg-accent-muted'
                      : 'border-white/[0.08] hover:bg-white/[0.04]'
                  }`}
    >
      <span className="text-[12px] font-medium text-text-primary">{label}</span>
      <span className="text-[10px] text-text-tertiary">{hint}</span>
    </button>
  )
}

function systemUser(): string {
  // crude fallback — the backend's localhost row knows the real user,
  // but we don't have a sync API for it from the frontend without
  // adding one. For new hosts on first launch this is just a hint
  // anyway; the user can edit.
  return ''
}
