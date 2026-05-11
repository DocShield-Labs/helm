/**
 * Schedule-scoped actions.
 *
 * Three palette entry points:
 *   - `schedule.new`        — open the editor for a fresh schedule.
 *   - `schedule.new-claude` — same but pre-set to a Claude Code body.
 *   - `schedule.list`       — drill-in projection of every saved
 *                             schedule, with sub-actions per row
 *                             (run-now, edit, enable/disable, delete).
 *
 * Schedule firing itself is the backend's job — these actions are
 * purely the surface the user touches in the palette to manage them.
 */

import { commands } from '@lib/ipc'
import { useStore } from '@lib/store'
import { paneCwdFor } from '@lib/path'
import type { Action } from './types'
import type { Schedule } from '@bindings'
import { activeHostId, activeWorkspace } from './workspace'

/** Best-effort cwd for the new-schedule editor's prefill: the active
 * pane's reported cwd. None when no host is active or the active
 * workspace has no pane yet (very early in a connect). */
function activePaneCwd(): string | undefined {
  const ws = activeWorkspace()
  if (!ws) return undefined
  // Pick the active window's first pane.
  const active = [...ws.windows.values()].find((w) => w.active)
  if (!active) return undefined
  const cwd = paneCwdFor(active.id, ws)
  return cwd || undefined
}

function openNew(opts?: { prefillBodyKind?: 'shell' | 'claude_code' }) {
  const hostId = activeHostId() ?? undefined
  const cwd = activePaneCwd()
  useStore.getState().openScheduleEditor({
    prefillBodyKind: opts?.prefillBodyKind,
    prefillHostId: hostId ?? undefined,
    prefillCwd: cwd,
  })
}

/** Sub-actions for a specific schedule row in the `schedule.list`
 * drill-in. The primary action (when the user hits Enter on the row)
 * is "Run now" — that's the verb most likely to be useful from the
 * palette. Editing, enabling, deleting all live as sub-actions. */
function scheduleSubActions(s: Schedule): Action[] {
  return [
    {
      id: `schedule.${s.id}.run-now`,
      kind: 'action',
      label: 'Run now',
      icon: '⏵',
      run: () => {
        void commands.scheduleRunNow(s.id)
      },
    },
    {
      id: `schedule.${s.id}.edit`,
      kind: 'action',
      label: 'Edit',
      icon: '✎',
      run: () => {
        useStore.getState().openScheduleEditor({ editing: s })
      },
    },
    {
      id: `schedule.${s.id}.toggle`,
      kind: 'action',
      label: s.enabled ? 'Disable' : 'Enable',
      icon: s.enabled ? '⏸' : '⏵',
      run: () => {
        void commands.scheduleSetEnabled(s.id, !s.enabled)
      },
    },
    {
      id: `schedule.${s.id}.delete`,
      kind: 'action',
      label: 'Delete',
      icon: '×',
      destructive: true,
      run: () => {
        void commands.scheduleDelete(s.id)
      },
    },
  ]
}

/** Format a schedule's trigger as a single-line palette sublabel. */
function triggerSummary(s: Schedule): string {
  if (s.trigger.kind === 'cron') return `cron · ${s.trigger.expr}`
  if (s.trigger.kind === 'interval') return `every ${s.trigger.seconds}s`
  return `once · ${new Date(s.trigger.at).toLocaleString()}`
}

/** Build the per-schedule row that drills into its sub-actions.
 * Primary action mirrors "Run now" so the user can hit Enter on a row
 * and have it fire immediately. ↦ drills into the four sub-actions. */
function scheduleRowAction(s: Schedule): Action {
  const host = useStore.getState().hosts.get(s.host_id)
  const hostLabel = host?.name ?? 'unknown host'
  const status = s.enabled ? 'on' : 'paused'
  return {
    id: `schedule.${s.id}`,
    kind: 'action',
    label: s.name,
    sublabel: `· ${hostLabel} · ${triggerSummary(s)} · ${status}`,
    icon: s.enabled ? '◷' : '◌',
    run: () => {
      void commands.scheduleRunNow(s.id)
    },
    subActions: () => scheduleSubActions(s),
  }
}

export const scheduleActions: Action[] = [
  {
    id: 'schedule.new',
    kind: 'action',
    label: 'New scheduled run',
    icon: '◷',
    keybinding: 'Cmd+Shift+S',
    run: () => openNew(),
  },
  {
    id: 'schedule.new-claude',
    kind: 'action',
    label: 'New scheduled Claude Code run',
    icon: '◷',
    run: () => openNew({ prefillBodyKind: 'claude_code' }),
  },
  {
    id: 'schedule.list',
    kind: 'action',
    label: 'Manage scheduled runs',
    icon: '◷',
    drillOnEnter: true,
    canRun: () => useStore.getState().schedules.size > 0,
    run: () => {
      // Falling-through Enter when there ARE schedules — but
      // drillOnEnter means the palette opens the sub-list instead, so
      // this body never runs in practice.
    },
    subActions: () => {
      const map = useStore.getState().schedules
      return [...map.values()]
        .sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()))
        .map(scheduleRowAction)
    },
  },
]
