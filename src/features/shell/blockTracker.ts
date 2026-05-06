/**
 * BlockTracker — turns OSC 133 marker streams into a Warp-style block
 * model anchored to xterm row positions.
 *
 * One tracker per (host, pane) lives for the lifetime of TmuxPane's
 * xterm mount. It owns the `term.write` path: instead of writing the
 * whole `%output` chunk in one shot, it slices the chunk at marker
 * offsets so we can sample `term.buffer.active.cursorY` after each
 * slice has been parsed. Without that, two markers in the same chunk
 * (a common case for `D` from one command + `A` for the next prompt)
 * would both pin to the cursor position at the *end* of the chunk,
 * smearing block boundaries onto the wrong lines.
 *
 * Each block start/end row is held as an `IMarker`, which xterm tracks
 * across scrollback eviction — drag the buffer past 10k lines and old
 * markers' `line` properties become -1, which we treat as "block fell
 * off the top, hide its decoration."
 */

import type { IMarker, Terminal } from '@xterm/xterm'
import type { MarkerAt } from '@bindings'

/** xterm host padding (`pl-6 pr-2 py-2` on the host element). Block
 * chrome and hover hit-tests both need to convert between row index
 * and pane-local pixel coordinates, so the offsets live next to
 * `clipBlockToViewport` for both consumers (`BlockOverlay`, `TmuxPane`)
 * to import. */
export const HOST_PADDING_TOP = 8
export const HOST_PADDING_LEFT = 24

/** How far down to scan from `block.startLine` for the first non-blank
 * row (the cwd · branch header). The integration prints ≤2 blank rows
 * above the header; we allow a couple extra in case a theme hook
 * inserts noise. */
const MAX_HEADER_SCAN_ROWS = 4

// ---------- diagnostic logging ----------
//
// Enable in the browser console:    localStorage.helmDebugBlocks = '1'
// Disable:                           localStorage.removeItem('helmDebugBlocks')
//
// `fmtBytes` shows control characters as escape literals so the dump is
// safe to paste into a bug report. All logging is gated through
// `isDebug()` and the expensive `fmtBytes` calls only run inside that
// guard — when the flag is off there's no per-chunk allocation cost.

function isDebug(): boolean {
  try {
    return (
      typeof localStorage !== 'undefined' &&
      localStorage.getItem('helmDebugBlocks') === '1'
    )
  } catch {
    return false
  }
}

function dlog(...args: unknown[]): void {
  console.log('[helm-blocks]', ...args)
}

function fmtBytes(bytes: Uint8Array | ArrayLike<number>): string {
  let s = ''
  const max = Math.min(bytes.length, 200)
  for (let i = 0; i < max; i++) {
    const b = bytes[i]
    if (b === 0x1b) s += '\\e'
    else if (b === 0x07) s += '\\a'
    else if (b === 0x0a) s += '\\n'
    else if (b === 0x0d) s += '\\r'
    else if (b === 0x09) s += '\\t'
    else if (b >= 0x20 && b < 0x7f) s += String.fromCharCode(b)
    else s += `\\x${b.toString(16).padStart(2, '0')}`
  }
  if (bytes.length > max) s += `…(+${bytes.length - max}b)`
  return s
}

/**
 * Block lifecycle:
 *   - `pending`  : `A` fired, block opened. The header line + prompt
 *     are about to print; the user hasn't run a command yet. Border
 *     stays neutral / transparent in this phase.
 *   - `running`  : `B` fired with a cmdline. The block now owns the
 *     command + output rows. Blue left border + running chip.
 *   - `ok`       : `D` fired with exit_code = 0. Closed block, no border.
 *   - `failed`   : `D` fired with exit_code != 0. Persistent red border.
 *   - `unknown`  : `D` fired without an exit code, or closed defensively
 *     (e.g. shell entered a TUI without a clean prompt cycle).
 */
export type BlockStatus = 'pending' | 'running' | 'ok' | 'failed' | 'unknown'

/** Live block record — owns xterm `IMarker` handles for row tracking. */
export interface BlockRecord {
  id: string
  /** Marker placed at the top of the block. Anchored to OSC 133 `A`
   * (PromptStart) so the cwd · branch header line printed by the
   * integration script's precmd, the prompt itself, the typed command,
   * and the output all sit *inside* the block's row range. */
  startMarker: IMarker | null
  /** Marker placed at the row of the `D` (CommandDone) marker. Null
   * while the command is still running. */
  endMarker: IMarker | null
  command: string | null
  status: BlockStatus
  exitCode: number | null
  /** Wall-clock when `A` fired. */
  openedAt: number
  /** Wall-clock when `B` fired with a cmdline. Drives the running
   * chip's elapsed counter and the final duration on `D`. Null while
   * still in `pending`. */
  commandStartedAt: number | null
  endedAt: number | null
  /** Cached cwd · branch row, found by scanning forward from
   * `startMarker.line` for the first non-blank row. Null until the
   * header has actually printed (between A and the first `print -P`
   * output). Once found, stays valid because IMarker tracks the row. */
  headerRow: number | null
  /** Cached cwd · branch text. Co-stored with `headerRow` so consumers
   * don't reach back into the xterm buffer. */
  headerText: string | null
}

/** Plain snapshot pushed to React. `startLine` / `endLine` are sampled
 * from the `IMarker`s at snapshot time; -1 means "fell off the top of
 * scrollback, don't render decorations." */
export interface BlockSnapshot {
  id: string
  startLine: number
  endLine: number | null
  /** First non-blank row at or below `startLine` — the cwd · branch
   * header rendered by the integration. -1 until the header has
   * actually printed (or if the block has fallen out of scrollback). */
  headerRow: number
  /** Trimmed text content of the header row. Used by the chip-position
   * math in BlockOverlay so it can land the inline duration chip
   * immediately after the cwd · branch text. Empty until found. */
  headerText: string
  command: string | null
  status: BlockStatus
  exitCode: number | null
  openedAt: number
  commandStartedAt: number | null
  endedAt: number | null
}

export interface PromptState {
  /** True between a `D` (or initial state) and a `B` — i.e. the shell
   * is waiting for the user to type. Currently only used to gate the
   * re-run chip's enabled state. */
  atPrompt: boolean
  /** True while xterm's active buffer is the alternate (DECSET 1049)
   * buffer — i.e. a TUI like vim/htop is running. Block chrome stays
   * out of TUIs' way. */
  altScreen: boolean
}

export interface BlockTrackerCallbacks {
  onBlocksChanged?: (snapshots: BlockSnapshot[]) => void
  onPromptStateChanged?: (state: PromptState) => void
}

export class BlockTracker {
  private term: Terminal
  private paneKey: string
  private blocks: BlockRecord[] = []
  private current: BlockRecord | null = null
  private state: PromptState = {
    // Default false until the first `A` marker arrives. Shells without
    // helm's integration (older builds, bash/fish without manual setup,
    // HELM_KEEP_PROMPT=1) never emit OSC 133 — they get no chrome and
    // type into xterm exactly as before.
    atPrompt: false,
    altScreen: false,
  }
  private nextId = 0
  private callbacks: BlockTrackerCallbacks
  private disposed = false

  constructor(term: Terminal, paneKey: string, callbacks: BlockTrackerCallbacks = {}) {
    this.term = term
    this.paneKey = paneKey
    this.callbacks = callbacks
  }

  /** Write a `%output` chunk to xterm, splitting at marker offsets so
   * each marker is processed at the cursor position it semantically
   * belongs to. */
  ingest(bytes: Uint8Array, markers: MarkerAt[]): void {
    if (this.disposed) return
    const debug = isDebug()
    if (markers.length === 0) {
      if (debug && bytes.length > 0) {
        dlog('ingest no-markers', { len: bytes.length, bytes: fmtBytes(bytes) })
      }
      this.term.write(bytes)
      // Even with no markers, the alt-screen state can flip if the
      // chunk contained `\x1b[?1049h` / `1049l`. Sample once.
      this.term.write(new Uint8Array(0), () => this.refreshAltScreen())
      return
    }

    if (debug) {
      dlog('ingest', {
        len: bytes.length,
        bytes: fmtBytes(bytes),
        markers: markers.map((m) => ({ kind: m.marker.kind, offset: m.offset })),
      })
    }

    let cursor = 0
    for (const m of markers) {
      const offset = Math.min(m.offset, bytes.length)
      if (offset > cursor) {
        const slice = bytes.subarray(cursor, offset)
        this.term.write(slice, () => {
          if (debug) {
            const buf = this.term.buffer.active
            dlog('  pre-marker', {
              slice: fmtBytes(slice),
              kind: m.marker.kind,
              offset,
              cursorAbsY: buf.baseY + buf.cursorY,
              cursorY: buf.cursorY,
              baseY: buf.baseY,
            })
          }
          this.applyMarker(m)
        })
      } else {
        this.term.write(new Uint8Array(0), () => {
          if (debug) {
            const buf = this.term.buffer.active
            dlog('  marker-no-slice', {
              kind: m.marker.kind,
              offset,
              cursorAbsY: buf.baseY + buf.cursorY,
            })
          }
          this.applyMarker(m)
        })
      }
      cursor = offset
    }
    if (cursor < bytes.length) {
      this.term.write(bytes.subarray(cursor))
    }
    this.term.write(new Uint8Array(0), () => this.refreshAltScreen())
  }

  /** Stop tracking. xterm `IMarker` instances are owned by the
   * Terminal — disposing the Terminal disposes the markers, so the
   * tracker just nulls itself out. */
  dispose(): void {
    this.disposed = true
    this.blocks = []
    this.current = null
  }

  getSnapshots(): BlockSnapshot[] {
    return this.blocks.map((b) => this.snapshot(b))
  }

  getPromptState(): PromptState {
    return { ...this.state }
  }

  // ---------- internals ----------

  private applyMarker(m: MarkerAt): void {
    if (this.disposed) return

    switch (m.marker.kind) {
      case 'bell':
        // No block-level effect — already handled by the inbox layer.
        break

      case 'prompt_start': {
        if (this.current && !this.current.endMarker) {
          this.current.endMarker = this.term.registerMarker(0)
          this.current.status = 'unknown'
          this.current.endedAt = Date.now()
          dlog('A: closed lingering block as unknown', {
            id: this.current.id,
            endLine: this.current.endMarker?.line,
          })
          this.current = null
        }

        const startMarker = this.term.registerMarker(0)
        const block: BlockRecord = {
          id: `${this.paneKey}#${this.nextId++}`,
          startMarker,
          endMarker: null,
          command: null,
          status: 'pending',
          exitCode: null,
          openedAt: Date.now(),
          commandStartedAt: null,
          endedAt: null,
          headerRow: null,
          headerText: null,
        }
        this.blocks.push(block)
        this.current = block

        if (isDebug()) {
          const buf = this.term.buffer.active
          dlog('A prompt_start: NEW BLOCK', {
            id: block.id,
            startLine: startMarker?.line,
            cursorAbsY: buf.baseY + buf.cursorY,
            cursorY: buf.cursorY,
            baseY: buf.baseY,
          })
        }

        this.state = { ...this.state, atPrompt: !this.state.altScreen }
        this.notifyPromptState()
        this.notifyBlocks()
        break
      }

      case 'command_start': {
        if (isDebug()) {
          const buf = this.term.buffer.active
          dlog('B command_start', {
            id: this.current?.id,
            command: m.marker.command,
            cursorAbsY: buf.baseY + buf.cursorY,
            currentStartLine: this.current?.startMarker?.line,
          })
        }
        // The user submitted a command. The block was already created
        // at A; here we fill in the cmdline + flip status to running.
        if (this.current && this.current.status === 'pending') {
          this.current.command = m.marker.command ?? null
          this.current.status = 'running'
          this.current.commandStartedAt = Date.now()
        } else if (!this.current) {
          // Defensive: B without a preceding A (older integration
          // dropped the A marker, or pane was attached mid-cycle).
          // Open a block here so output still has a visual home.
          const block: BlockRecord = {
            id: `${this.paneKey}#${this.nextId++}`,
            startMarker: this.term.registerMarker(0),
            endMarker: null,
            command: m.marker.command ?? null,
            status: 'running',
            exitCode: null,
            openedAt: Date.now(),
            commandStartedAt: Date.now(),
            endedAt: null,
            headerRow: null,
            headerText: null,
          }
          this.blocks.push(block)
          this.current = block
        }
        this.state = { ...this.state, atPrompt: false }
        this.notifyPromptState()
        this.notifyBlocks()
        break
      }

      case 'output_start': {
        // C — used by older integrations that don't ship cmdline on B.
        // Block creation handled in A / B above; nothing to do here
        // unless we somehow got here with no current block.
        if (!this.current) {
          const block: BlockRecord = {
            id: `${this.paneKey}#${this.nextId++}`,
            startMarker: this.term.registerMarker(0),
            endMarker: null,
            command: null,
            status: 'running',
            exitCode: null,
            openedAt: Date.now(),
            commandStartedAt: Date.now(),
            endedAt: null,
            headerRow: null,
            headerText: null,
          }
          this.blocks.push(block)
          this.current = block
          this.state = { ...this.state, atPrompt: false }
          this.notifyPromptState()
          this.notifyBlocks()
        }
        break
      }

      case 'command_done': {
        const exitCode = m.marker.exit_code ?? null
        if (this.current) {
          const buf = this.term.buffer.active
          const cursorAbsY = buf.baseY + buf.cursorY
          const cursorLine = buf.getLine(cursorAbsY)
          const cursorLineText =
            cursorLine?.translateToString(true) ?? '<no line>'
          const cursorRowBlank =
            !cursorLine || cursorLineText.trim() === ''
          const offset = cursorRowBlank && cursorAbsY > 0 ? -1 : 0
          const endMarker = this.term.registerMarker(offset)
          this.current.endMarker = endMarker
          this.current.exitCode = exitCode
          this.current.status =
            exitCode === null ? 'unknown' : exitCode === 0 ? 'ok' : 'failed'
          this.current.endedAt = Date.now()

          if (isDebug()) {
            dlog('D command_done', {
              id: this.current.id,
              exitCode,
              cursorAbsY,
              cursorY: buf.cursorY,
              baseY: buf.baseY,
              cursorLineText: JSON.stringify(cursorLineText),
              cursorRowBlank,
              offset,
              endLine: endMarker?.line,
              startLine: this.current.startMarker?.line,
            })
            // Sample a few rows around the end so we can verify what
            // actually rendered there.
            const samples: Record<string, string> = {}
            for (let r = Math.max(0, cursorAbsY - 3); r <= cursorAbsY + 1; r++) {
              const ln = buf.getLine(r)
              samples[`r${r}`] = JSON.stringify(
                ln?.translateToString(true) ?? '<missing>',
              )
            }
            dlog('  rows around D', samples)
          }

          this.current = null
        }
        this.notifyBlocks()
        break
      }
    }
  }

  private refreshAltScreen(): void {
    if (this.disposed) return
    const isAlt = this.term.buffer.active.type === 'alternate'
    if (isAlt === this.state.altScreen) return
    // Force atPrompt off whenever a TUI takes over; on exit, the next
    // `A` (which the shell emits when it redraws the prompt after a
    // TUI exits) will flip it back on.
    this.state = {
      altScreen: isAlt,
      atPrompt: isAlt ? false : this.state.atPrompt,
    }
    this.notifyPromptState()
  }

  private snapshot(b: BlockRecord): BlockSnapshot {
    const startLine = b.startMarker?.line ?? -1

    // Locate the cwd · branch header row by scanning forward from
    // `startLine` for the first non-blank row. Cache on the record so
    // we only do this work until the row is found — IMarker tracks the
    // row across scrollback growth, so once we've got it we're done.
    if (b.headerRow === null && startLine >= 0) {
      const buf = this.term.buffer.active
      const max = startLine + MAX_HEADER_SCAN_ROWS
      for (let row = startLine; row <= max; row++) {
        const line = buf.getLine(row)
        if (!line) continue
        const text = line.translateToString(true)
        if (text.trim().length > 0) {
          b.headerRow = row
          b.headerText = text.replace(/\s+$/, '')
          break
        }
      }
    }

    return {
      id: b.id,
      startLine,
      endLine: b.endMarker?.line ?? null,
      headerRow: startLine < 0 ? -1 : b.headerRow ?? -1,
      headerText: startLine < 0 ? '' : b.headerText ?? '',
      command: b.command,
      status: b.status,
      exitCode: b.exitCode,
      openedAt: b.openedAt,
      commandStartedAt: b.commandStartedAt,
      endedAt: b.endedAt,
    }
  }

  private notifyBlocks(): void {
    this.callbacks.onBlocksChanged?.(this.getSnapshots())
  }

  private notifyPromptState(): void {
    this.callbacks.onPromptStateChanged?.(this.getPromptState())
  }
}

/** Clip a block's row range to the visible viewport. Returns null when
 * the block is entirely off-screen. Used by both `BlockOverlay`
 * (rendering) and `TmuxPane` (hover detection) so they agree on which
 * blocks are interactive at a given scroll position. */
export function clipBlockToViewport(
  startLine: number,
  endLine: number | null,
  term: Terminal,
): { visibleTop: number; visibleBottom: number } | null {
  if (startLine < 0) return null
  const buf = term.buffer.active
  const viewportY = buf.viewportY
  const viewportEnd = viewportY + term.rows - 1
  const cursorRow = buf.baseY + buf.cursorY
  const liveEnd = endLine ?? cursorRow
  const visibleTop = Math.max(startLine, viewportY)
  const visibleBottom = Math.min(liveEnd, viewportEnd)
  if (visibleBottom < visibleTop) return null
  return { visibleTop, visibleBottom }
}
