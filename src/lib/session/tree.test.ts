import { describe, expect, test } from 'bun:test'
import { treeToSessions } from './tree'

describe('treeToSessions', () => {
  test('maps the daemon snapshot into flat sessions', () => {
    const session = treeToSessions({
      sessions: [{
        id: '3',
        name: 'shell',
        cols: 80,
        rows: 24,
        alt_screen: false,
        cwd: '/repo',
        branch: 'main',
        root: '/repo',
        command: 'zsh',
      }],
    })[0]
    expect(session).toEqual({
      id: '3',
      name: 'shell',
      command: 'zsh',
      cwd: '/repo',
      branch: 'main',
      root: '/repo',
    })
  })
})
