import { AGENT_PRESETS, agentName } from '@lib/session/agents'
import { useStore } from '@lib/store'
import type { Action } from './types'

export const agentActions: Action[] = [
  {
    id: 'agent.default',
    kind: 'action',
    label: 'Default agent',
    icon: '✦',
    get sublabel() {
      const state = useStore.getState()
      return `· ${agentName(state.defaultAgentId, state.customAgentTemplate)}`
    },
    drillOnEnter: true,
    run: () => {},
    subActions: () => {
      const state = useStore.getState()
      const builtIns = AGENT_PRESETS.map(
        (preset): Action => ({
          id: `agent.default.${preset.id}`,
          kind: 'action',
          label: preset.name,
          icon: state.defaultAgentId === preset.id ? '✓' : '·',
          run: () => useStore.getState().setDefaultAgentId(preset.id),
        }),
      )
      return [
        ...builtIns,
        {
          id: 'agent.default.custom',
          kind: 'action',
          label: 'Custom…',
          sublabel: state.customAgentTemplate ? `· ${agentName('custom', state.customAgentTemplate)}` : undefined,
          icon: state.defaultAgentId === 'custom' ? '✓' : '·',
          run: () => useStore.getState().openCustomAgentModal(),
        },
      ]
    },
  },
]
