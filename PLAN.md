# Helm 2.0 — helmd migration plan

Decision (2026-08-21): replace tmux outright with **helmd**, a per-host persistence
daemon we own. No tmux compatibility mode — tmux's protocol shape (control-mode
line/octal framing, one `-CC` client per session, `%output` session scoping,
capture/live seam, resize union across clients) is the source of the rendering
jank, so we don't carry it forward. The remaining non-tmux jank fixes (shell
integration, OSC 8 underlines) ride along as cleanup.

Design reference: Figma "Helm 2.0" — https://www.figma.com/design/jmqGQ6q2ORS0kwltXDDCra
Block treatment copies Warp exactly (thin separators inset from the left, failed
commands get a full-width red wash + flag-pole stripe, hover action toolbar,
alt-screen replaces the block list with a plain grid). This is now possible —
and jank-free — because blocks become *data* (helmd segments the stream by
OSC 133 at ingest) and each block is its own DOM element in document flow, not
overlay chrome positioned by cell-pixel math over one xterm grid (the June
failure mode).

## Architecture

```
┌───────────────────────────── helm (Tauri app) ─────────────────────────────┐
│  BlockList renderer (per window)                                           │
│    finished blocks → static ANSI→DOM spans (native drag-select, DOM find)  │
│    active tail + alt-screen TUIs → one xterm.js instance                   │
│  Tauri events: binary-safe, batched by helmd (no per-line JSON number[])   │
└──────────────┬─────────────────────────────────────────────┬───────────────┘
               │ unix socket (localhost)                     │ ssh exec channel
               │                                             │  `helmd stdio`
┌──────────────▼──────────────┐              ┌───────────────▼───────────────┐
│  helmd serve (local)        │              │  helmd stdio ⇄ unix socket    │
│                             │              │  (bridges; auto-spawns serve) │
│  PTY supervisor             │              └───────────────┬───────────────┘
│  per-pane ring buffer       │                              │
│  OSC 133 block index        │              ┌───────────────▼───────────────┐
│  seq-numbered replay        │              │  helmd serve (remote host)    │
│  search over scrollback     │              │  …same code…                  │
│  offline notification queue │              └───────────────────────────────┘
└─────────────────────────────┘
```

### Crates

- **`helm-proto`** — wire types + framing shared by app and daemon.
  Length-prefixed (u32 LE) bincode messages. Client→daemon: `Hello{version}`,
  `Attach{last_seq per pane}`, `Input{pane, bytes}`, `Resize{pane, cols, rows}`,
  `NewWindow/NewWorkspace/Kill/Rename/Select`, `Replay{pane, from_seq | last_bytes}`,
  `Search{query, regex, case_sensitive, scope}`, `AckNotifications`.
  Daemon→client: `HelloAck{version, state snapshot}`, `Output{pane, seq, bytes}`
  (batched: 5 ms / 64 KB flush), `BlockEvent{pane, block}` (started / command /
  finished{exit, duration}), `ModeChange{pane, alt_screen}`, `TreeChanged`,
  `SearchResults{matches: [{pane, block_id, line, seq, context}]}`,
  `Notification{pane, kind, preview, at}`, `Exit{pane, status}`.
- **`helmd`** — the daemon binary (also `helmd stdio` bridge subcommand).
  Tokio + portable-pty. State: workspaces → windows → panes (one PTY per pane;
  splits are a frontend/layout concern later). IDs are daemon-monotonic u64,
  stable for the daemon's lifetime; metadata (names, cwd) persisted to
  `~/.helm/state.json` so the tree survives daemon restart (processes don't —
  same guarantee tmux gives across server kill, minus tmux).
- `helm-app` — drops `helm-tmux` for a `Transport` (unix socket | ssh stdio
  bridge) + `HelmdClient`. `helm-ssh` stays as-is (transport + oneshot exec).
- `helm-tmux` — deleted at the end (M7).

### Core daemon behaviors

- **Ring buffer**: per pane, raw bytes, default 8 MB, `seq` = absolute byte
  offset since pane creation (u64, never resets). Replay = "bytes from seq N"
  or "last N KB"; reattach is exact — no capture-pane, no rewrap seam, no
  cursor restoration hack, no `term.reset()` races.
- **Block index**: OSC 133 A/B/C/D parsed at ingest (port `parse.rs` marker
  logic, minus the octal decode — we own raw bytes now). Blocks carry
  `{start_seq, cmd_seq, end_seq, cmdline, cwd, branch, exit_code, started_at,
  duration}`. BEL stripped into notifications exactly as today.
- **Alt-screen detection**: DECSET 1049/47 tracked at ingest → `ModeChange`.
  Frontend swaps BlockList ⇄ full-pane xterm grid on this signal (Warp's
  AltScreenElement approach).
- **Search**: lazily-built ANSI-stripped line index per pane; search runs in
  the daemon, returns matches with `seq` anchors; the app scrolls the block
  list to the block/line (finished blocks are DOM — jump is trivial).
- **Notifications while detached**: bell + nonzero-exit events queue in the
  daemon when no client is attached; delivered in `HelloAck` on attach.
  Replaces `#{window_bell_flag}` backfill with strictly more information
  (per-event previews + timestamps, not one flag bit).
- **Resize**: focused client is the single writer; last-writer-wins. No union
  semantics, no `refresh-client -C` fan-out.

### Remote install/upgrade

Reuse the existing base64-over-exec install path (same as integration scripts):
upload a per-arch static binary to `~/.helm/bin/helmd`, `chmod +x`, version
handshake on `Hello`; on mismatch, re-upload and restart `serve` (sessions die
on upgrade — acceptable, same as a tmux server upgrade; later: graceful
`helmd upgrade` with PTY fd handoff if it ever matters).

## Milestones

- **M0 — cleanup (now, independent of helmd)**
  - `integration/zsh.zshrc`: strip all blocks-era behavior — no PROMPT/RPROMPT
    clearing, no cwd·branch header printing, no grey preexec tint, no injected
    blank lines. Markers (A/B/C/D + cwd/branch/cmdline b64) stay — helmd's
    block index depends on them. `HELM_KEEP_PROMPT` gone (passive is the only
    mode).
  - ~~OSC 8 underline tweak~~ — superseded by a confirmed root cause: the
    underline/doubling/long-output jank is `extract_markers_and_strip`
    (`helm-tmux/src/parse.rs:245`) being **stateless per `%output` chunk**.
    Escape sequences split across chunk boundaries get mangled: a trailing
    bare `ESC` passes through raw so the next chunk's `]8;;…` renders as
    literal glyphs; an OSC 8 BEL terminator arriving in the next chunk is
    classified as a standalone bell — stripped (xterm never exits OSC-collect
    mode → swallowed output, leaked attributes) *and* upserted as a phantom
    inbox notification; a split `ESC k` title drops the rest of the chunk.
    Not patched (throwaway per no-compat decision); helmd M2's ingest parser
    MUST be stateful across reads, with split-envelope tests covering every
    boundary position.
- **M1 — `helm-proto`**: crate with message enums + framing codec + roundtrip
  tests. `cargo check` green.
- **M2 — `helmd` core, local**: serve + PTY supervisor + ring buffer + replay +
  block index + unix socket. Testable with a raw socket client; unit tests for
  ring/replay/OSC-133 segmentation.
- **M3 — app integration, localhost-first**: `HelmdClient` in helm-app behind
  the existing command surface (connect/subscribe/send-keys/resize/new-window/
  kill/rename); localhost connects via socket, spawns `helmd serve` if absent.
  Frontend still renders one xterm per pane at this stage (bytes are bytes) —
  this proves transport + replay before any UI rework. Events move to batched
  binary (base64 payload or Tauri binary channel), killing the number[] bloat.
- **M4 — remote transport**: `helmd stdio` bridge over the existing russh exec
  channel; auto-install/upgrade; reconnect ladder + wake probe unchanged.
- **M5 — block-native frontend**: BlockList renderer (finished blocks →
  ANSI→DOM spans; active tail → xterm; alt-screen swap). Warp-exact block
  chrome per the Figma: separators, red wash + pole on failure, hover toolbar
  (copy command / copy output / bookmark), sticky command header. Composer from
  the mockup (shell + agent modes).
- **M6 — search + notifications**: daemon search wired into the ⌘K palette
  (all-host fan-out, jump-to-line), in-pane find over the DOM + xterm tail;
  notification queue replaces bell-flag backfill; inbox unchanged UX-wise.
- **M7 — rip tmux out**: delete `helm-tmux`, `connection.rs` multi-client
  supervisor machinery, capture/prehydrate paths, `store.ts` tmux-id
  projections ($N/@N/%N → helmd u64 ids), reconcile/orphan-reaper code. Update
  claude_code.rs hook (BEL to pane tty still works — helmd owns the tty now;
  simpler: hook can write a marker file or use `helmd notify` subcommand).

## Risks / open questions

- **Daemon crash = lost processes.** tmux is battle-hardened; helmd won't be at
  first. Mitigation: keep helmd tiny (supervise PTYs, buffer bytes, nothing
  else), panic=abort + auto-respawn via the stdio bridge, integration tests.
- **Existing tmux sessions are stranded** on migration (no compat by decision).
  One-time migration note in the release; users finish tmux work before
  upgrading.
- **Shells without integration** (bash w/o hooks, fish, ssh-within-ssh): no
  OSC 133 → no blocks. Fallback: whole scrollback renders as one "block"
  (plain grid) — degrade to exactly today's experience. Ship bash + fish
  integration scripts eventually.
- **Windows hosts**: out of scope (same as today).

## Status

- [x] M0: zsh.zshrc de-blocked (passive markers only)
- [x] M0: split-envelope parser bug root-caused (documented above; fix lives
      in helmd's stateful ingest parser, not the doomed tmux path)
- [x] M1: helm-proto crate (ids, ClientMsg/DaemonMsg, length-prefixed bincode
      framing, stateful FrameDecoder; roundtrip tests incl. byte-at-a-time)
- [x] M2: `helmd` core complete and green (20 tests incl. a full e2e over a
      real unix socket + real `/bin/sh` in a PTY):
      · `ring.rs` — seq-addressed byte ring, exact replay semantics
      · `markers.rs` — stateful streaming parser (OSC 133 strip→events,
        bells, alt-screen, OSC 8/DCS passthrough, ESC-k drop, runaway cap;
        `split_at_every_boundary` locks in the split-envelope fix)
      · `pane.rs` — PTY spawn + reader/wait threads, seq assigned under the
        ring lock; last-writer-wins resize
      · `daemon.rs` — tree, client fan-out, block table from markers,
        offline notification queue, substring search with seq anchors
      · `server.rs` — `serve` (unix socket, stale-socket recovery) and
        `stdio` bridge (auto-spawns serve; the entire remote transport)
      Deferred out of M2: output aggregation beyond natural 8KB read batching
      (do in M3 if profiling demands), tree-name persistence to state.json
      (M3), block-table pruning against ring eviction (M3), regex search (M6).
- [x] M3 + M4 + M7 (backend): hard cutover done — helm-app runs on helmd.
      · `helm-proto::client` is blocking-IO (`connect_io` over any
        Read/Write pair) so the unix socket and the SSH `helmd stdio`
        bridge share one implementation; request/reply correlation
        (`req_id` → `Created` / `SearchResults`) added to the protocol
      · `helm-ssh::open_exec_raw` — no-PTY exec channel (a PTY's line
        discipline would corrupt binary frames; sshd ignores raw modes)
      · `connection.rs` rewritten: establish (local socket / remote
        install+upgrade helmd by streaming the binary over stdin, then
        `exec helmd stdio`), pump (DaemonMsg → HostEvent, base64 output,
        breadcrumb index, notifications, tool detection), supervisor
        (reconnect ladder + wake probe) — ~190 lines of tmux multi-client
        dedupe machinery deleted with nothing replacing it
      · `helm-domain`: `TmuxNotification`/markers → `SessionEvent` +
        `SessionTree`/`BlockInfo`/`SearchHit`; `HostEvent::Session`
      · commands: `tmux_*` → `session_*` / `workspace_*` / `window_*`
        (creating ops return ids; search returns results directly)
      · notifications fed by the daemon (preview/kind from helmd,
        cmdline/duration from blocks, focus suppression app-side)
      · scheduler fires via `workspace_new`/`window_new` request-reply
      · Claude hook rewritten to `$HELM_TTY` (exported by all three
        integration scripts); v1 tmux-era hooks migrate on install
      · `helm-tmux` crate deleted; workspace builds clean
      Deferred: cross-platform helmd bundles for remote hosts (currently
      requires matching OS/arch — fine for a homogeneous Mac fleet);
      Tauri sidecar bundling of helmd for release builds (dev finds
      target/debug/helmd next to the app binary); handshake timeout on
      the remote bridge; foreground-process tracking in helmd (replaces
      tmux's pane_current_command for the scheduler's Claude Enter-press,
      which is a fixed 2.5s delay for now).
- [x] M5 (core): block-native frontend landed — `tsc` clean, vite builds.
      · `lib/session/ansi.ts` — stateful ANSI/VT → styled spans (SGR 16/256/
        truecolor, `\r` overwrite, `ESC[K/J`, cursor moves so multi-line
        progress collapses, OSC 8 links), 12 bun tests
      · `lib/session/seqbuffer.ts` — seq-addressed chunk buffer: in-order
        append, overlap trim, gap detection + heal, bridged-bytes delivery,
        capacity eviction; 7 bun tests
      · `lib/session/stream.ts` — per-pane streams (prime on first frame,
        history fetch, gap → replay, tail listeners, slices)
      · `lib/session/blocks.ts` — per-pane block tables outside zustand
        (useSyncExternalStore), primed by `session_blocks`
      · `lib/session/tree.ts` — SessionTree → store projection;
        `store.setWorkspaces` now carries selection (`active` flags are
        pure frontend state) and `sortById` is numeric
      · `lib/host.ts` rewritten as a `SessionEvent` router; `selectWindow`
        replaces all 11 `tmuxSelectWindow` sites
      · `features/shell/BlockPane.tsx` + `BlockList.tsx` + `Block.tsx`:
        finished blocks as DOM (Warp chrome: inset separators, red wash +
        flag pole on failure, hover copy toolbar, ❯ cmdline header with
        duration/exit), one xterm live tail at the bottom of the same
        scroll container, reset+re-feed on block finish, alt-screen swap,
        jump-to-latest pill, dismiss-on-keystroke preserved
      · `terminal/index.ts`: `convertEol` off (raw PTY bytes), wheel passes
        through at xterm's scroll edges and entirely in alt-screen
      · `actions/search.ts`: `session_search` fan-out across connected
        hosts, debounced + cached, palette re-renders on arrival, ⏎ jumps to
        the window and scrolls/flashes the block
      · NotificationPeek reads the live stream; TmuxPane/TerminalScrollbar
        deleted; Helm palette in tokens.css + default theme
      · Sticky command headers; Cmd+F searches DOM blocks (CSS Custom
        Highlight API) + the xterm tail as one ordered match list
      · helmd ships as a Tauri sidecar: `scripts/build-helmd.sh` stages
        `binaries/helmd-<triple>` (run by `beforeBuildCommand`), Tauri
        places it next to the app binary where `helmd_bin_path` looks
      · Smoke-tested live (`bun run tauri:dev`): connect in ~2s, BlockPane
        hydration (`session_blocks` + `session_replay`) 33ms after connect,
        daemon kill → supervisor reconnect + respawn in 1.3s
      Remaining polish: list virtualization past 250 blocks, settings
      surface, the agent composer from the Figma, regex search daemon-side
      (M6), foreground-process tracking in helmd, cross-platform helmd
      bundles for heterogeneous fleets.
