/**
 * Documentation table of global keyboard bindings. Source of truth for
 * dispatch is now the action registry (`@lib/actions`) consumed by
 * `@lib/keymap-engine`; this file is a human-readable catalogue of
 * what's wired (and what's planned but not yet implemented). Edit when
 * adding new bindings so the list stays useful for the future
 * Settings → Keyboard tab.
 */

export interface KeyBinding {
  combo: string
  description: string
  handler?: () => void
}

export const KEYBINDINGS: KeyBinding[] = [
  { combo: 'Cmd+K',       description: 'Command palette' },
  { combo: 'Cmd+P',       description: 'Quick switcher' },
  { combo: 'Cmd+T',       description: 'New session' },
  { combo: 'Cmd+W',       description: 'Close session' },
  { combo: 'Cmd+]',       description: 'Next session' },
  { combo: 'Cmd+[',       description: 'Previous session' },
  { combo: 'Cmd+D',       description: 'Split right' },
  { combo: 'Cmd+Shift+D', description: 'Split down' },
  { combo: 'Cmd+\\',      description: 'Toggle sidebar' },
  { combo: 'Cmd+Shift+A', description: 'Toggle activity feed' },
  { combo: 'Cmd+,',       description: 'Open settings' },
  // Phase 4F — block navigation.
  { combo: 'Cmd+Up',      description: 'Select previous block in session' },
  { combo: 'Cmd+Down',    description: 'Select next block in session' },
  { combo: 'Cmd+C',       description: 'Copy selected block output (when no xterm selection)' },
  { combo: 'Cmd+Shift+C', description: 'Copy selected block command' },
  { combo: 'Cmd+R',       description: 'Re-run selected block' },
]
