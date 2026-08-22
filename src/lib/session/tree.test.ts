import { describe, expect, test } from 'bun:test';
import { treeToWorkspaces } from './tree';

describe('treeToWorkspaces', () => {
  test('maps nested tree into workspace maps with inactive selection', () => {
    const ws = treeToWorkspaces({
      workspaces: [
        {
          id: '1',
          name: 'default',
          windows: [
            {
              id: '10',
              name: 'zsh',
              panes: [
                { id: '100', cols: 80, rows: 24, alt_screen: false, cwd: '/home/u/src', branch: 'main', root: '/home/u', command: null, head_seq: 5, buffer_start_seq: 0 },
              ],
            },
            { id: '9', name: 'claude', panes: [] },
          ],
        },
      ],
    });
    expect(ws).toHaveLength(1);
    expect([...ws[0].windows.keys()]).toEqual(['10', '9']);
    expect(ws[0].panes.get('100')).toEqual({
      id: '100', windowId: '10', active: false, command: '', cwd: '/home/u/src', branch: 'main', root: '/home/u',
    });
    expect(ws[0].windows.get('9')!.active).toBe(false);
  });
});
