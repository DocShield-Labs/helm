import { describe, expect, test } from 'bun:test'
import { groupKeyOf, groupRows, relativeCwd } from './sidebarGroups'

const H = '/Users/x'
const row = (id: number, root: string, cwd: string) => ({ id, root, cwd })

describe('sidebar groups', () => {
  test('a repo is one group whatever the subdirectory; agents included', () => {
    const gs = groupRows([
      row(1, `${H}/Code/bento`, `${H}/Code/bento`),
      row(2, `${H}/Code/bento`, `${H}/Code/bento/crates/helmd`),
      row(3, `${H}/Code/bento`, `${H}/Code/bento`),
    ])
    expect(gs.map((g) => g.label)).toEqual(['~/Code/bento'])
    expect(gs[0].rows.map((r) => r.id)).toEqual([1, 2, 3])
  })

  test('outside a repo the cwd itself is the group', () => {
    const gs = groupRows([row(1, '', `${H}`), row(2, '', `${H}/Downloads`), row(3, '', `${H}`)])
    expect(gs.map((g) => g.label)).toEqual(['~', '~/Downloads'])
    expect(gs[0].rows.map((r) => r.id)).toEqual([1, 3])
  })

  test('groups keep first-appearance order; a new project appends', () => {
    const before = groupRows([row(1, '/p/zeta', '/p/zeta'), row(2, '/p/alpha', '/p/alpha')])
    expect(before.map((g) => g.key)).toEqual(['/p/zeta', '/p/alpha'])
    const after = groupRows([row(1, '/p/zeta', '/p/zeta'), row(2, '/p/alpha', '/p/alpha'), row(3, '/p/beta', '/p/beta')])
    expect(after.map((g) => g.key)).toEqual(['/p/zeta', '/p/alpha', '/p/beta'])
  })

  test('worktrees are separate groups', () => {
    const gs = groupRows([row(1, '/p/app', '/p/app'), row(2, '/p/app-wt-feature', '/p/app-wt-feature/src')])
    expect(gs.map((g) => g.key)).toEqual(['/p/app', '/p/app-wt-feature'])
  })

  test('sessions with no directory go last, headerless', () => {
    const gs = groupRows([row(1, '', ''), row(2, '/p/app', '/p/app')])
    expect(gs.map((g) => g.label)).toEqual(['/p/app', ''])
    expect(gs[1].rows.map((r) => r.id)).toEqual([1])
  })

  test('relative cwd within the group', () => {
    expect(relativeCwd(row(1, '/p/app', '/p/app'))).toBe('')
    expect(relativeCwd(row(1, '/p/app', '/p/app/crates/helmd'))).toBe('crates/helmd')
    expect(relativeCwd(row(1, '', `${H}/Downloads`))).toBe('')
    expect(relativeCwd(row(1, '/p/app', `${H}/elsewhere`))).toBe('~/elsewhere')
    expect(groupKeyOf(row(1, '', ''))).toBe('')
  })
})
