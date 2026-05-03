/**
 * Global keyboard map. Each key is registered once at the App root.
 * Phase 3 implements this; for now it's a typed placeholder.
 */

export interface KeyBinding {
  combo: string
  description: string
  handler?: () => void
}

export const KEYBINDINGS: KeyBinding[] = [
  { combo: 'Cmd+K',       description: 'Command palette' },
  { combo: 'Cmd+P',       description: 'Quick switcher' },
  { combo: 'Cmd+Shift+N', description: 'New workspace' },
  { combo: 'Cmd+T',       description: 'New window in current workspace' },
  { combo: 'Cmd+W',       description: 'Close window' },
  { combo: 'Cmd+]',       description: 'Next window' },
  { combo: 'Cmd+[',       description: 'Previous window' },
  { combo: 'Cmd+D',       description: 'Split right' },
  { combo: 'Cmd+Shift+D', description: 'Split down' },
  { combo: 'Cmd+\\',      description: 'Toggle sidebar' },
  { combo: 'Cmd+Shift+A', description: 'Toggle activity feed' },
  { combo: 'Cmd+,',       description: 'Open settings' },
]
