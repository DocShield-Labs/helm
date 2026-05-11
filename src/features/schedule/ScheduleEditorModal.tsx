/**
 * ScheduleEditorModal — three-step wizard for creating or editing a
 * scheduled run.
 *
 * The form used to be one tall page that scrolled. Splitting it into
 * three short focused steps means each screen has space to breathe
 * and the user can't be lost in a sea of fields.
 *
 *   1. Action   — what to run (Shell line vs Claude Code launch)
 *   2. Location — which host + working directory
 *   3. Schedule — when to fire + a final review (name, enabled,
 *                 next-fire preview, run-now)
 *
 * Cross-step validation is incremental: Next is gated on the current
 * step being valid, and the stepper is click-jumpable to any
 * already-valid step (so users can revisit earlier decisions without
 * losing draft state).
 */

import { useEffect, useMemo, useState } from 'react'
import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'
import { Button, Input, Modal, Stepper, Toggle } from '@ui'
import type {
  HostId,
  Schedule,
  ScheduleBody,
  ScheduleId,
  Trigger,
  WorkspaceTarget,
} from '@bindings'
import { RemotePathPicker } from './RemotePathPicker'

type BodyKind = 'shell' | 'claude_code'
type TriggerKind = 'cron' | 'once' | 'interval'
type Step = 'action' | 'location' | 'schedule'

const STEPS: { id: Step; label: string }[] = [
  { id: 'action', label: 'Action' },
  { id: 'location', label: 'Location' },
  { id: 'schedule', label: 'Schedule' },
]

interface DraftState {
  name: string
  hostId: HostId | null
  cwd: string
  bodyKind: BodyKind
  shellCommand: string
  claudePrompt: string
  claudeNonInteractive: boolean
  claudeModel: string
  claudeDangerouslySkip: boolean
  triggerKind: TriggerKind
  cronExpr: string
  cronTz: string
  onceLocal: string
  intervalSeconds: string
  workspace: string
  enabled: boolean
}

const DEFAULT_WORKSPACE = 'scheduled'
const DEFAULT_CRON = '0 9 * * 1-5'
const DEFAULT_INTERVAL = '900'

/** Common cron presets surfaced as one-click chips on the schedule
 * step. The custom expression input stays available alongside for
 * power users; the chips just save typing for the 80% case. */
const CRON_PRESETS: { label: string; expr: string }[] = [
  { label: 'Weekdays 9am', expr: '0 9 * * 1-5' },
  { label: 'Weekdays 5pm', expr: '0 17 * * 1-5' },
  { label: 'Mondays 9am', expr: '0 9 * * 1' },
  { label: 'Every hour', expr: '0 * * * *' },
  { label: 'Every 15 min', expr: '*/15 * * * *' },
  { label: 'Daily 8am', expr: '0 8 * * *' },
]

function emptyDraft(): DraftState {
  return {
    name: '',
    hostId: null,
    cwd: '',
    bodyKind: 'shell',
    shellCommand: '',
    claudePrompt: '',
    claudeNonInteractive: false,
    claudeModel: '',
    claudeDangerouslySkip: false,
    triggerKind: 'cron',
    cronExpr: DEFAULT_CRON,
    cronTz: detectTz(),
    onceLocal: defaultOnceLocal(),
    intervalSeconds: DEFAULT_INTERVAL,
    workspace: DEFAULT_WORKSPACE,
    enabled: true,
  }
}

export function ScheduleEditorModal() {
  const editor = useStore((s) => s.scheduleEditor)
  const close = useStore((s) => s.closeScheduleEditor)
  const hosts = useStore((s) => s.hosts)
  const localHostId = useStore((s) => {
    for (const h of s.hosts.values()) if (h.port === 0) return h.id
    return null
  })

  const [draft, setDraft] = useState<DraftState>(emptyDraft())
  const [step, setStep] = useState<Step>('action')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // Locally hide the "last run failed" banner after the user clicks
  // Dismiss inside this modal session. Re-opening the modal reads the
  // schedule's persisted `last_run_status` again, so the banner returns
  // if the underlying state hasn't changed.
  const [bannerDismissed, setBannerDismissed] = useState(false)

  // Reset on every open. The dependency on `editor` is a single object
  // we replace wholesale — its identity changes only on open/close.
  useEffect(() => {
    if (!editor.open) return
    setError(null)
    setBusy(false)
    setStep('action')
    setBannerDismissed(false)
    if (editor.editing) {
      setDraft(draftFromSchedule(editor.editing))
    } else {
      const base = emptyDraft()
      base.hostId = editor.prefillHostId ?? localHostId ?? null
      if (editor.prefillCwd) base.cwd = editor.prefillCwd
      if (editor.prefillBodyKind) base.bodyKind = editor.prefillBodyKind
      setDraft(base)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editor.open, editor.editing, editor.prefillHostId, editor.prefillCwd, editor.prefillBodyKind])

  const isEdit = !!editor.editing
  const stepIndex = STEPS.findIndex((s) => s.id === step)

  // Auto-suggest a schedule name from whatever the user has typed for
  // the body. Keeps the schedule step's name field useful by default;
  // the user can still override.
  useEffect(() => {
    if (!editor.open || isEdit) return
    if (draft.name.trim().length > 0) return
    const suggestion = suggestName(draft)
    if (suggestion) setDraft((p) => ({ ...p, name: suggestion }))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [step, draft.bodyKind, draft.shellCommand, draft.claudePrompt])

  const stepValidation = useMemo(() => validateStep(step, draft), [step, draft])
  const fullValidation = useMemo(() => validateAll(draft), [draft])
  const canSave = fullValidation.ok

  const triggerPreview = useMemo(
    () => previewNextFires(draft),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [draft.triggerKind, draft.cronExpr, draft.cronTz, draft.onceLocal, draft.intervalSeconds],
  )

  const set = <K extends keyof DraftState>(k: K, v: DraftState[K]) => {
    setDraft((prev) => ({ ...prev, [k]: v }))
  }

  /** Highest step the user could legitimately jump to without
   * skipping a still-incomplete earlier step. The stepper passes
   * this as the click-target gate. */
  const maxJumpableIndex = useMemo(() => {
    let i = 0
    for (; i < STEPS.length - 1; i++) {
      const v = validateStep(STEPS[i].id, draft)
      if (!v.ok) return i
    }
    return i
  }, [draft])

  function go(direction: 1 | -1) {
    const next = Math.max(0, Math.min(STEPS.length - 1, stepIndex + direction))
    setStep(STEPS[next].id)
  }

  async function save() {
    if (!canSave) return
    setBusy(true)
    setError(null)
    const id: ScheduleId = editor.editing?.id ?? crypto.randomUUID()
    const schedule = buildSchedule(draft, id, editor.editing)
    if (!schedule) {
      setError('internal: could not assemble schedule')
      setBusy(false)
      return
    }
    const res = await commands.scheduleSave(schedule)
    setBusy(false)
    if (res.status !== 'ok') {
      setError(res.error)
      return
    }
    close()
  }

  async function runNow() {
    if (!canSave) return
    setBusy(true)
    setError(null)
    const id: ScheduleId = editor.editing?.id ?? crypto.randomUUID()
    const schedule = buildSchedule(draft, id, editor.editing)
    if (!schedule) {
      setError('internal: could not assemble schedule')
      setBusy(false)
      return
    }
    const saveRes = await commands.scheduleSave(schedule)
    if (saveRes.status !== 'ok') {
      setError(saveRes.error)
      setBusy(false)
      return
    }
    const runRes = await commands.scheduleRunNow(id)
    setBusy(false)
    if (runRes.status !== 'ok') {
      setError(runRes.error)
      return
    }
    close()
  }

  async function deleteIt() {
    if (!editor.editing) return
    setBusy(true)
    setError(null)
    const res = await commands.scheduleDelete(editor.editing.id)
    setBusy(false)
    if (res.status !== 'ok') {
      setError(res.error)
      return
    }
    close()
  }

  /** Retry the last run without saving the current draft. Different
   * from `runNow` (which saves first): used by the failure banner so
   * the user can re-trigger the exact persisted schedule that failed,
   * even if their draft has unsaved changes (which would be a confusing
   * thing to fire). The schedule doesn't need to exist in the draft —
   * if the modal was opened with a `editing` reference, that record is
   * the source of truth on disk. */
  async function retryLast() {
    const id = editor.editing?.id
    if (!id) return
    setBusy(true)
    setError(null)
    const runRes = await commands.scheduleRunNow(id)
    setBusy(false)
    if (runRes.status !== 'ok') {
      setError(runRes.error)
      return
    }
    close()
  }

  // Pull the live schedule out of the store so the banner reflects
  // any in-flight retry (which updates last_run_status via the
  // schedule_upserted event) without us having to thread state.
  const liveSchedule = useStore((s) =>
    editor.editing ? s.schedules.get(editor.editing.id) ?? null : null,
  )
  const failure =
    liveSchedule?.last_run_status?.kind === 'failed'
      ? liveSchedule.last_run_status.reason
      : null
  const showBanner = !!failure && !bannerDismissed

  const footer = (
    <>
      <span className="flex-1 text-[12px] text-status-error">{error ?? ''}</span>
      {isEdit && step === 'schedule' && (
        <Button kind="tertiary" onClick={deleteIt} disabled={busy}>
          Delete
        </Button>
      )}
      <Button kind="tertiary" onClick={close} disabled={busy}>
        Cancel
      </Button>
      {stepIndex > 0 && (
        <Button kind="secondary" onClick={() => go(-1)} disabled={busy}>
          Back
        </Button>
      )}
      {stepIndex < STEPS.length - 1 && (
        <Button
          kind="primary"
          onClick={() => go(+1)}
          disabled={!stepValidation.ok || busy}
        >
          Next
        </Button>
      )}
      {stepIndex === STEPS.length - 1 && (
        <>
          <Button kind="secondary" onClick={runNow} disabled={!canSave || busy}>
            Run now
          </Button>
          <Button kind="primary" onClick={save} disabled={!canSave || busy}>
            {busy ? 'Saving…' : isEdit ? 'Save' : 'Create'}
          </Button>
        </>
      )}
    </>
  )

  return (
    <Modal
      open={editor.open}
      title={isEdit ? `Edit "${editor.editing!.name}"` : 'New scheduled run'}
      onClose={close}
      width={640}
      footer={footer}
    >
      <div className="flex flex-col gap-6">
        {showBanner && failure && (
          <FailureBanner
            reason={failure}
            busy={busy}
            onRetry={retryLast}
            onDismiss={() => setBannerDismissed(true)}
          />
        )}

        <Stepper
          steps={STEPS}
          activeIndex={stepIndex}
          onJump={(i) => {
            // Always allow jumping back; only allow forward jumps to
            // steps whose preceding steps are valid.
            if (i <= stepIndex || i <= maxJumpableIndex) {
              setStep(STEPS[i].id)
            }
          }}
        />

        {step === 'action' && <ActionStep draft={draft} set={set} />}
        {step === 'location' && (
          <LocationStep draft={draft} set={set} hosts={hosts} />
        )}
        {step === 'schedule' && (
          <ScheduleStep
            draft={draft}
            set={set}
            triggerPreview={triggerPreview}
            stepValidation={stepValidation}
            fullValidation={fullValidation}
            hosts={hosts}
          />
        )}

        {!stepValidation.ok && step !== 'schedule' && (
          <div className="text-[12px] text-text-tertiary">{stepValidation.reason}</div>
        )}
      </div>
    </Modal>
  )
}

// ---------- Step 1: Action ----------

function ActionStep({
  draft,
  set,
}: {
  draft: DraftState
  set: <K extends keyof DraftState>(k: K, v: DraftState[K]) => void
}) {
  return (
    <div className="flex flex-col gap-5">
      <SectionHeader
        title="What runs"
        hint="Pick a command type, then describe what should run when this schedule fires."
      />

      <div className="grid grid-cols-2 gap-3">
        <BigChoice
          checked={draft.bodyKind === 'shell'}
          onChange={() => set('bodyKind', 'shell')}
          icon="$_"
          label="Shell command"
          hint="Any line you'd type at the prompt — pipes, redirects, scripts."
        />
        <BigChoice
          checked={draft.bodyKind === 'claude_code'}
          onChange={() => set('bodyKind', 'claude_code')}
          icon="✶"
          label="Claude Code"
          hint="Launch Claude with a starter prompt — TUI or one-shot."
        />
      </div>

      {draft.bodyKind === 'shell' && (
        <Field label="Command">
          <textarea
            value={draft.shellCommand}
            onChange={(e) => set('shellCommand', e.target.value)}
            placeholder="bun run nightly-report"
            rows={3}
            className="w-full resize-y rounded-md border border-white/[0.08] bg-sidebar px-3 py-2
                       font-mono text-[12px] text-text-primary placeholder:text-text-disabled
                       focus:border-accent focus:outline-none"
          />
        </Field>
      )}

      {draft.bodyKind === 'claude_code' && (
        <div className="flex flex-col gap-4">
          <Field
            label="Prompt"
            hint={
              draft.claudeNonInteractive
                ? 'piped via -p; claude prints output and exits'
                : 'passed via argv; helm presses Enter once the TUI is up'
            }
          >
            <textarea
              value={draft.claudePrompt}
              onChange={(e) => set('claudePrompt', e.target.value)}
              placeholder="Triage today's failing tests and summarize the top three."
              rows={4}
              className="w-full resize-y rounded-md border border-white/[0.08] bg-sidebar px-3 py-2
                         font-mono text-[12px] text-text-primary placeholder:text-text-disabled
                         focus:border-accent focus:outline-none"
            />
          </Field>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Mode" hint={draft.claudeNonInteractive ? '`-p` print mode' : 'interactive TUI'}>
              <ToggleRow
                checked={draft.claudeNonInteractive}
                onChange={(v) => set('claudeNonInteractive', v)}
                onLabel="One-shot"
                offLabel="Interactive"
              />
            </Field>
            <Field label="Model" hint="omit for default">
              <Input
                mono
                value={draft.claudeModel}
                onChange={(e) => set('claudeModel', e.target.value)}
                placeholder="claude-opus-4-7"
              />
            </Field>
          </div>
          <Field label="Skip permissions" hint="adds --dangerously-skip-permissions">
            <ToggleRow
              checked={draft.claudeDangerouslySkip}
              onChange={(v) => set('claudeDangerouslySkip', v)}
              onLabel="Skipping"
              offLabel="Prompt as usual"
            />
          </Field>
        </div>
      )}
    </div>
  )
}

// ---------- Step 2: Location ----------

function LocationStep({
  draft,
  set,
  hosts,
}: {
  draft: DraftState
  set: <K extends keyof DraftState>(k: K, v: DraftState[K]) => void
  hosts: Map<string, { id: HostId; name: string; port: number }>
}) {
  return (
    <div className="flex flex-col gap-5">
      <SectionHeader
        title="Where it runs"
        hint="The host the schedule fires on, the directory the new window opens in, and the workspace it lands in."
      />

      <div className="grid grid-cols-[200px_1fr] gap-4">
        <Field label="Host">
          <select
            value={draft.hostId ?? ''}
            onChange={(e) => set('hostId', (e.target.value as HostId) || null)}
            className="h-9 w-full rounded-md border border-white/[0.08] bg-sidebar px-2 text-[13px] text-text-primary focus:border-accent focus:outline-none"
          >
            <option value="" disabled>
              choose a host
            </option>
            {[...hosts.values()].map((h) => (
              <option key={h.id} value={h.id}>
                {h.name}
                {h.port === 0 ? ' · local' : ''}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Workspace" hint="tmux session — created if missing">
          <Input
            mono
            value={draft.workspace}
            onChange={(e) => set('workspace', e.target.value)}
            placeholder={DEFAULT_WORKSPACE}
          />
        </Field>
      </div>

      <Field label="Working directory" hint="click to navigate; the schedule's window opens here">
        <RemotePathPicker
          hostId={draft.hostId}
          value={draft.cwd}
          onChange={(p) => set('cwd', p)}
        />
      </Field>
    </div>
  )
}

// ---------- Step 3: Schedule ----------

function ScheduleStep({
  draft,
  set,
  triggerPreview,
  stepValidation,
  fullValidation,
  hosts,
}: {
  draft: DraftState
  set: <K extends keyof DraftState>(k: K, v: DraftState[K]) => void
  triggerPreview: string[]
  stepValidation: { ok: true } | { ok: false; reason: string }
  fullValidation: { ok: true } | { ok: false; reason: string }
  hosts: Map<string, { id: HostId; name: string; port: number }>
}) {
  return (
    <div className="flex flex-col gap-5">
      <SectionHeader
        title="When it runs"
        hint="Pick a recurring schedule, an interval, or a one-shot fire time."
      />

      <div className="grid grid-cols-3 gap-2">
        <SmallChoice
          checked={draft.triggerKind === 'cron'}
          onChange={() => set('triggerKind', 'cron')}
          label="Cron"
          hint="recurring"
        />
        <SmallChoice
          checked={draft.triggerKind === 'interval'}
          onChange={() => set('triggerKind', 'interval')}
          label="Interval"
          hint="every N seconds"
        />
        <SmallChoice
          checked={draft.triggerKind === 'once'}
          onChange={() => set('triggerKind', 'once')}
          label="Once"
          hint="single fire"
        />
      </div>

      {draft.triggerKind === 'cron' && (
        <div className="flex flex-col gap-3">
          <div className="flex flex-wrap gap-1.5">
            {CRON_PRESETS.map((p) => (
              <button
                key={p.label}
                type="button"
                onClick={() => set('cronExpr', p.expr)}
                className={`rounded-full border px-2.5 py-1 text-[11px] font-medium transition-colors
                            ${draft.cronExpr === p.expr
                              ? 'border-accent bg-accent-muted text-text-primary'
                              : 'border-white/[0.08] text-text-secondary hover:bg-white/[0.04]'}`}
              >
                {p.label}
              </button>
            ))}
          </div>
          <div className="grid grid-cols-[1fr_220px] gap-3">
            <Field label="Expression" hint="m h dom mon dow (5-field standard cron)">
              <Input
                mono
                value={draft.cronExpr}
                onChange={(e) => set('cronExpr', e.target.value)}
                placeholder={DEFAULT_CRON}
              />
            </Field>
            <Field label="Timezone" hint="IANA name">
              <Input
                mono
                value={draft.cronTz}
                onChange={(e) => set('cronTz', e.target.value)}
                placeholder="America/Los_Angeles"
              />
            </Field>
          </div>
        </div>
      )}

      {draft.triggerKind === 'interval' && (
        <Field label="Interval (seconds)" hint="900 = every 15 minutes">
          <Input
            mono
            value={draft.intervalSeconds}
            onChange={(e) => set('intervalSeconds', e.target.value)}
            placeholder={DEFAULT_INTERVAL}
          />
        </Field>
      )}

      {draft.triggerKind === 'once' && (
        <Field label="Fire at" hint="local time on this machine">
          <Input
            mono
            type="datetime-local"
            value={draft.onceLocal}
            onChange={(e) => set('onceLocal', e.target.value)}
          />
        </Field>
      )}

      {/* Preview */}
      <PreviewCard preview={triggerPreview} validation={stepValidation} />

      {/* Final identity row + summary. Lives at the bottom of the
          last step so the user sees naming + state alongside their
          first concrete preview of the schedule. */}
      <div className="grid grid-cols-[1fr_140px] gap-4">
        <Field label="Name" hint="shown in the palette + as the tmux window name">
          <Input
            value={draft.name}
            onChange={(e) => set('name', e.target.value)}
            placeholder="morning standup"
          />
        </Field>
        <Field label="Status">
          <ToggleRow
            checked={draft.enabled}
            onChange={(v) => set('enabled', v)}
            onLabel="Enabled"
            offLabel="Paused"
          />
        </Field>
      </div>

      <ReviewSummary draft={draft} hosts={hosts} fullValidation={fullValidation} />
    </div>
  )
}

// ---------- Reusable bits ----------

/** Sticky-feeling banner shown at the top of the editor when the
 * underlying schedule's `last_run_status` is `failed`. Surfaces the
 * raw failure reason (so the user knows *what* broke) plus a Retry
 * button that re-runs the persisted schedule as-is. Dismissable
 * locally — re-opening the modal brings it back if the failure
 * hasn't been resolved. */
function FailureBanner({
  reason,
  busy,
  onRetry,
  onDismiss,
}: {
  reason: string
  busy: boolean
  onRetry: () => void
  onDismiss: () => void
}) {
  return (
    <div
      role="alert"
      className="flex items-start gap-3 rounded-md border border-status-error/40 bg-status-error/[0.08] px-3 py-2.5"
    >
      <span
        aria-hidden
        className="mt-[3px] inline-flex h-4 w-4 items-center justify-center rounded-full bg-status-error/20 text-[11px] font-semibold text-status-error"
      >
        !
      </span>
      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
        <div className="text-[12px] font-semibold text-text-primary">
          Last run failed
        </div>
        <div className="break-words font-mono text-[11px] text-text-secondary">
          {reason}
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-1">
        <button
          type="button"
          onClick={onRetry}
          disabled={busy}
          className="rounded-md border border-status-error/40 bg-status-error/[0.12] px-2 py-1 text-[11px] font-medium text-text-primary
                     hover:bg-status-error/[0.18] disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? 'Retrying…' : 'Retry'}
        </button>
        <button
          type="button"
          onClick={onDismiss}
          className="rounded-md px-2 py-1 text-[11px] text-text-tertiary hover:bg-white/[0.04] hover:text-text-secondary"
        >
          Dismiss
        </button>
      </div>
    </div>
  )
}

function SectionHeader({ title, hint }: { title: string; hint: string }) {
  return (
    <div className="flex flex-col gap-0.5">
      <h3 className="text-[14px] font-semibold tracking-tight text-text-primary">{title}</h3>
      <p className="text-[12px] text-text-tertiary">{hint}</p>
    </div>
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
    <label className="flex min-w-0 flex-col gap-1.5">
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

/** Big two-up card pattern used on step 1. */
function BigChoice({
  checked,
  onChange,
  icon,
  label,
  hint,
}: {
  checked: boolean
  onChange: () => void
  icon: string
  label: string
  hint: string
}) {
  return (
    <button
      type="button"
      onClick={onChange}
      className={`flex flex-col gap-2 rounded-lg border px-4 py-3 text-left transition-all
                  ${checked
                    ? 'border-accent bg-accent-muted'
                    : 'border-white/[0.08] hover:border-white/[0.16] hover:bg-white/[0.03]'}`}
    >
      <span className="flex h-7 w-7 items-center justify-center rounded-md bg-white/[0.06] font-mono text-[13px] text-text-primary">
        {icon}
      </span>
      <span className="text-[13px] font-medium text-text-primary">{label}</span>
      <span className="text-[11px] leading-snug text-text-tertiary">{hint}</span>
    </button>
  )
}

/** Compact three-up card used on step 3. */
function SmallChoice({
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
      className={`flex flex-col gap-0.5 rounded-md border px-3 py-2 text-left transition-colors
                  ${checked
                    ? 'border-accent bg-accent-muted'
                    : 'border-white/[0.08] hover:bg-white/[0.04]'}`}
    >
      <span className="text-[12px] font-medium text-text-primary">{label}</span>
      <span className="text-[10px] text-text-tertiary">{hint}</span>
    </button>
  )
}

/** Inline toggle + verbose label. Aligns the toggle's track with the
 * baseline of the label text so a row of these reads as one strip. */
function ToggleRow({
  checked,
  onChange,
  onLabel,
  offLabel,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  onLabel: string
  offLabel: string
}) {
  return (
    <div className="flex h-9 items-center gap-3 rounded-md border border-white/[0.08] bg-sidebar px-3">
      <Toggle checked={checked} onChange={onChange} ariaLabel={checked ? onLabel : offLabel} />
      <span className="text-[12px] text-text-secondary">{checked ? onLabel : offLabel}</span>
    </div>
  )
}

function PreviewCard({
  preview,
  validation,
}: {
  preview: string[]
  validation: { ok: true } | { ok: false; reason: string }
}) {
  return (
    <div className="rounded-md border border-white/[0.06] bg-sidebar/60 px-3 py-2.5">
      <div className="mb-1 text-[11px] uppercase tracking-[0.06em] text-text-tertiary">
        Next fires
      </div>
      {preview.length === 0 ? (
        <div className="font-mono text-[12px] text-text-tertiary">—</div>
      ) : (
        <ul className="flex flex-col gap-0.5 font-mono text-[12px] text-text-secondary">
          {preview.map((p, i) => (
            <li key={i}>{p}</li>
          ))}
        </ul>
      )}
      {!validation.ok && (
        <div className="mt-1 text-[11px] text-status-error">{validation.reason}</div>
      )}
    </div>
  )
}

function ReviewSummary({
  draft,
  hosts,
  fullValidation,
}: {
  draft: DraftState
  hosts: Map<string, { id: HostId; name: string; port: number }>
  fullValidation: { ok: true } | { ok: false; reason: string }
}) {
  const host = draft.hostId ? hosts.get(draft.hostId) : undefined
  const summary = describeBody(draft)
  return (
    <div className="rounded-md border border-white/[0.06] bg-sidebar/40 px-3 py-2.5 text-[12px]">
      <div className="mb-1 text-[11px] uppercase tracking-[0.06em] text-text-tertiary">
        Summary
      </div>
      <div className="font-mono text-text-secondary">
        <span className="text-text-primary">{summary}</span>
        {host && (
          <>
            {' '}
            on <span className="text-text-primary">{host.name}</span>
          </>
        )}
        {draft.cwd && (
          <>
            {' '}
            in <span className="text-text-primary">{draft.cwd}</span>
          </>
        )}
      </div>
      {!fullValidation.ok && (
        <div className="mt-1 text-[11px] text-status-error">{fullValidation.reason}</div>
      )}
    </div>
  )
}

// ---------- helpers ----------

function detectTz(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'
  } catch {
    return 'UTC'
  }
}

function defaultOnceLocal(): string {
  const d = new Date(Date.now() + 60 * 60 * 1000)
  d.setMinutes(0, 0, 0)
  return toLocalInputValue(d)
}

function toLocalInputValue(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function fromLocalInputValue(s: string): Date | null {
  if (!s) return null
  const d = new Date(s)
  return isNaN(d.getTime()) ? null : d
}

function draftFromSchedule(s: Schedule): DraftState {
  const base = emptyDraft()
  base.name = s.name
  base.hostId = s.host_id
  base.cwd = s.cwd
  base.workspace = s.workspace_target.name
  base.enabled = s.enabled
  if (s.body.kind === 'shell') {
    base.bodyKind = 'shell'
    base.shellCommand = s.body.command
  } else {
    base.bodyKind = 'claude_code'
    base.claudePrompt = s.body.prompt
    base.claudeNonInteractive = s.body.non_interactive
    base.claudeModel = s.body.model ?? ''
    base.claudeDangerouslySkip = s.body.dangerously_skip_permissions
  }
  if (s.trigger.kind === 'cron') {
    base.triggerKind = 'cron'
    base.cronExpr = s.trigger.expr
    base.cronTz = s.trigger.tz || detectTz()
  } else if (s.trigger.kind === 'interval') {
    base.triggerKind = 'interval'
    base.intervalSeconds = String(s.trigger.seconds)
  } else {
    base.triggerKind = 'once'
    base.onceLocal = toLocalInputValue(new Date(s.trigger.at))
  }
  return base
}

function buildBody(d: DraftState): ScheduleBody | null {
  if (d.bodyKind === 'shell') {
    if (!d.shellCommand.trim()) return null
    return { kind: 'shell', command: d.shellCommand.trim() }
  }
  return {
    kind: 'claude_code',
    prompt: d.claudePrompt,
    non_interactive: d.claudeNonInteractive,
    model: d.claudeModel.trim() ? d.claudeModel.trim() : null,
    dangerously_skip_permissions: d.claudeDangerouslySkip,
  }
}

function buildTrigger(d: DraftState): Trigger | null {
  if (d.triggerKind === 'cron') {
    if (!d.cronExpr.trim()) return null
    return { kind: 'cron', expr: d.cronExpr.trim(), tz: d.cronTz.trim() }
  }
  if (d.triggerKind === 'interval') {
    const n = parseInt(d.intervalSeconds, 10)
    if (!Number.isFinite(n) || n <= 0) return null
    return { kind: 'interval', seconds: n }
  }
  const dt = fromLocalInputValue(d.onceLocal)
  if (!dt) return null
  return { kind: 'once', at: dt.getTime() }
}

function buildWorkspace(d: DraftState): WorkspaceTarget {
  return { kind: 'named', name: d.workspace.trim() || DEFAULT_WORKSPACE }
}

function buildSchedule(
  d: DraftState,
  id: ScheduleId,
  prior: Schedule | null,
): Schedule | null {
  if (!d.hostId) return null
  const body = buildBody(d)
  const trigger = buildTrigger(d)
  if (!body || !trigger) return null
  return {
    id,
    name: d.name.trim() || suggestName(d) || 'scheduled run',
    host_id: d.hostId,
    cwd: d.cwd.trim(),
    body,
    trigger,
    workspace_target: buildWorkspace(d),
    enabled: d.enabled,
    last_fired_at: prior?.last_fired_at ?? null,
    last_run_status: prior?.last_run_status ?? null,
  }
}

/** Produce a human-readable one-liner for the body for the review
 * summary. Trims long shell lines / prompts so the summary card
 * doesn't wrap onto multiple lines. */
function describeBody(d: DraftState): string {
  const trim = (s: string, n: number) =>
    s.length > n ? s.slice(0, n - 1) + '…' : s
  if (d.bodyKind === 'shell') {
    return d.shellCommand ? `$ ${trim(d.shellCommand, 60)}` : '$ (empty command)'
  }
  if (!d.claudePrompt) return 'claude (no prompt)'
  const prefix = d.claudeNonInteractive ? 'claude -p' : 'claude'
  return `${prefix} "${trim(d.claudePrompt, 50)}"`
}

/** Suggest a reasonable schedule name from the body text — first few
 * words of the prompt or command. Non-destructive: only fills in when
 * the name field is still empty. */
function suggestName(d: DraftState): string {
  const text =
    d.bodyKind === 'shell'
      ? d.shellCommand.trim()
      : d.claudePrompt.trim()
  if (!text) return ''
  const firstLine = text.split('\n')[0].trim()
  const words = firstLine.split(/\s+/).slice(0, 5).join(' ')
  if (!words) return ''
  return words.length > 32 ? words.slice(0, 31) + '…' : words
}

/** Per-step validation. Lets the wizard gate Next on having enough
 * info to proceed without blocking the user with the entire schedule's
 * worth of validation upfront. */
function validateStep(
  step: Step,
  d: DraftState,
): { ok: true } | { ok: false; reason: string } {
  if (step === 'action') {
    if (d.bodyKind === 'shell' && !d.shellCommand.trim())
      return { ok: false, reason: 'shell command is empty' }
    if (d.bodyKind === 'claude_code' && d.claudeNonInteractive && !d.claudePrompt.trim())
      return { ok: false, reason: 'one-shot Claude needs a prompt' }
    return { ok: true }
  }
  if (step === 'location') {
    if (!d.hostId) return { ok: false, reason: 'pick a host' }
    if (!d.cwd.trim()) return { ok: false, reason: 'pick a working directory' }
    return { ok: true }
  }
  // 'schedule'
  if (d.triggerKind === 'cron') {
    if (!d.cronExpr.trim()) return { ok: false, reason: 'cron expression is empty' }
    const parts = d.cronExpr.trim().split(/\s+/)
    if (parts.length < 5 || parts.length > 6) {
      return { ok: false, reason: 'cron expression must have 5 or 6 fields' }
    }
  }
  if (d.triggerKind === 'interval') {
    const n = parseInt(d.intervalSeconds, 10)
    if (!Number.isFinite(n) || n <= 0)
      return { ok: false, reason: 'interval must be a positive number of seconds' }
  }
  if (d.triggerKind === 'once') {
    const dt = fromLocalInputValue(d.onceLocal)
    if (!dt) return { ok: false, reason: 'once: invalid date' }
    if (dt.getTime() <= Date.now())
      return { ok: false, reason: 'once: pick a future time' }
  }
  return { ok: true }
}

function validateAll(d: DraftState): { ok: true } | { ok: false; reason: string } {
  for (const s of STEPS) {
    const v = validateStep(s.id, d)
    if (!v.ok) return v
  }
  return { ok: true }
}

function previewNextFires(d: DraftState): string[] {
  if (d.triggerKind === 'cron') {
    return [`${d.cronExpr} (${d.cronTz || detectTz()})`]
  }
  if (d.triggerKind === 'interval') {
    const n = parseInt(d.intervalSeconds, 10)
    if (!Number.isFinite(n) || n <= 0) return []
    const out: string[] = []
    for (let i = 1; i <= 3; i++) {
      out.push(formatRelative(Date.now() + i * n * 1000))
    }
    return out
  }
  const dt = fromLocalInputValue(d.onceLocal)
  if (!dt) return []
  return [formatRelative(dt.getTime())]
}

function formatRelative(ms: number): string {
  const d = new Date(ms)
  const delta = Math.round((ms - Date.now()) / 1000)
  const abs = d.toLocaleString()
  if (delta < 0) return `${abs} (past)`
  if (delta < 60) return `${abs} (in ${delta}s)`
  if (delta < 3600) return `${abs} (in ${Math.round(delta / 60)}m)`
  if (delta < 86400) return `${abs} (in ${Math.round(delta / 3600)}h)`
  return `${abs} (in ${Math.round(delta / 86400)}d)`
}
