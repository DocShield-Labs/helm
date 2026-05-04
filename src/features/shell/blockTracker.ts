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
}

/** Plain snapshot pushed to React. `startLine` / `endLine` are sampled
 * from the `IMarker`s at snapshot time; -1 means "fell off the top of
 * scrollback, don't render decorations." */
export interface BlockSnapshot {
  id: string
  startLine: number
  endLine: number | null
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
    if (markers.length === 0) {
      this.term.write(bytes)
      // Even with no markers, the alt-screen state can flip if the
      // chunk contained `\x1b[?1049h` / `1049l`. Sample once.
      this.term.write(new Uint8Array(0), () => this.refreshAltScreen())
      return
    }

    let cursor = 0
    for (const m of markers) {
      const offset = Math.min(m.offset, bytes.length)
      if (offset > cursor) {
        // Write up to (but not including) the marker's offset, then
        // process the marker once xterm has parsed those bytes. The
        // callback form of `term.write` fires after the parse pass
        // finishes, so cursor reads are accurate.
        const slice = bytes.subarray(cursor, offset)
        this.term.write(slice, () => this.applyMarker(m))
      } else {
        // Marker sits at the very start of the chunk (or coincides
        // with the previous marker's offset). Fire it directly —
        // there are no bytes to flush first.
        this.term.write(new Uint8Array(0), () => this.applyMarker(m))
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
        // The row that A fires on is where precmd is about to print
        // the cwd · branch header line, then zsh draws the prompt,
        // then the user types, then the command runs. The entire
        // cycle stays inside this single block — that's why the
        // border can run uninterrupted from header to last output line.
        //
        // Defensive: if we still have a "current" block that never saw
        // `D` (shell entered a TUI then dropped out without a clean
        // exit code), close it now.
        if (this.current && !this.current.endMarker) {
          this.current.endMarker = this.term.registerMarker(0)
          this.current.status = 'unknown'
          this.current.endedAt = Date.now()
          this.current = null
        }

        const block: BlockRecord = {
          id: `${this.paneKey}#${this.nextId++}`,
          startMarker: this.term.registerMarker(0),
          endMarker: null,
          command: null,
          status: 'pending',
          exitCode: null,
          openedAt: Date.now(),
          commandStartedAt: null,
          endedAt: null,
        }
        this.blocks.push(block)
        this.current = block

        this.state = { ...this.state, atPrompt: !this.state.altScreen }
        this.notifyPromptState()
        this.notifyBlocks()
        break
      }

      case 'command_start': {
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
          // Most commands end with `\n`, which leaves the cursor on a
          // fresh blank row when precmd fires. Registering at offset 0
          // would capture *that* blank row — and the very next `A`
          // (firing at the same cursor) would claim the same row as
          // its start, so two adjacent blocks would both tint it.
          // Detect the trailing-blank case and back the end-marker up
          // one row so the blank cleanly belongs to the next block
          // alone (its top pad).
          //
          // For commands that didn't end with `\n` (rare: `printf 'x'`)
          // the cursor row has content; we close at offset 0 normally.
          const buf = this.term.buffer.active
          const cursorAbsY = buf.baseY + buf.cursorY
          const cursorLine = buf.getLine(cursorAbsY)
          const cursorRowBlank =
            !cursorLine || cursorLine.translateToString(true).trim() === ''
          const offset = cursorRowBlank && cursorAbsY > 0 ? -1 : 0
          this.current.endMarker = this.term.registerMarker(offset)
          this.current.exitCode = exitCode
          this.current.status =
            exitCode === null ? 'unknown' : exitCode === 0 ? 'ok' : 'failed'
          this.current.endedAt = Date.now()
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
    return {
      id: b.id,
      startLine: b.startMarker?.line ?? -1,
      endLine: b.endMarker?.line ?? null,
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
