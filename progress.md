# Helm — Progress

A modern terminal that happens to use tmux. Personal tool; ship for one user.

Phasing follows §11 of the design brief. Each phase ends in a usable build;
nothing internal-ships until phase 2.

---

## Status

| Phase | Theme                                | State        |
| ----- | ------------------------------------ | ------------ |
| 0     | Tauri scaffold + one local PTY       | **done**     |
| 1     | tmux control mode, sidebar live      | **done**     |
| 1.5   | window navigation polish             | **done**     |
| 2A    | Multi-host refactor (no SSH yet)     | **done**     |
| 2B    | SSH transport + multi-workspace UI   | **done**     |
| 2C    | Persistence + Keychain + host editor | **done**     |
| 2D    | Reconnect, host-key TOFU, raw PTY    | **done**     |
| 3     | Command palette + keyboard map       | later        |
| 4     | Inbox notifications + shell integ.   | **done**     |
| 4G    | Multi-control-client tmux refactor   | **done**     |
| 4H    | Tool integration framework           | **done**     |
| 4F    | Warp-style blocks UI from OSC 133    | later        |
| 5     | Performance + IPC optimisation       | later        |

---

## Phase 0 — Foundations · **done**

Proves the rendering pipeline. Cargo workspace compiles, frontend builds,
one zsh prompt renders end-to-end with input/output/resize working.

### Done
- Cargo workspace with five crates (`helm-app`, `-domain`, `-pty`, `-tmux`, `-ssh`)
- Tauri 2 + tauri-specta with auto-generated `src/types/bindings.ts`
  - `cargo run --bin export-bindings` regenerates without launching the GUI
- Frontend: bun + Vite + React 19 + Tailwind 4 + Zustand
- Token system 1:1 with Figma `/📐 Foundations` (CSS vars + Tailwind aliases)
- UI components: `StatusDot`, `ActivityDot`, sidebar rows, status bar segments,
  `Button`, `Input`, `Toggle`, `Modal`, `Palette`
- `helm-pty::spawn()` over `portable-pty`; reader + wait threads, write/resize/kill
- xterm.js + WebGL renderer wrapper
- `Pane` component: spawn, write, resize (debounced 50ms), kill on unmount
- Hidden traffic lights, native window chrome, status bar visible

### Sharp edges (deferred · revisit when they bite)
- **PTY bytes → JSON `number[]`.** ~3–4× wire bloat. Acceptable for one local
  shell; switch to a custom URI scheme protocol or msgpack channel before
  streaming heavy logs (target: phase 5).
- **No 5–8ms output aggregator.** §3.1 calls for one; current naive 64KB-per-read
  is fine because xterm batches its own redraws. Add when measurable.
- **Resize-smaller artefacts.** Inherent to PTY semantics — the shell can't
  redraw scrollback. Same in iTerm/Terminal.app/Ghostty. UX answer: Ctrl+L
  after a big resize. We could add an opt-in "auto-redraw on resize" later.
- **Hardcoded `/bin/zsh -l`.** Settings tab will own this once phase 4 ships.
- **Dev-cycle PTY orphans.** When `cargo` rebuilds and SIGTERMs the dev binary
  (not a clean `Cmd+Q`), the `RunEvent::ExitRequested` cleanup doesn't fire,
  so the spawned shell is left reparented to launchd. Cleanup:
  `pkill -P 1 -f '^/bin/zsh$'`. Real fix in phase 5: write live PIDs to a
  state file and reap on next launch.
- **`bundle.active = false`** in `tauri.conf.json` — placeholder PNG icons
  only. Generate real ones via `bunx @tauri-apps/cli icon ./source.svg` when
  we want to actually distribute.
- **AppState fields unused** (host/workspace/window/pane registries) — phase 1
  populates them.

---

## Phase 1 — tmux integration · **mostly done**

Talk to tmux. Round-trip workspace/window/pane state. Sidebar reads from
the live tree. Typing into the pane works end-to-end.

### Done

**`helm-tmux`:**
- [x] Spawn `tmux -CC new-session -A -s <name>` *inside a PTY* via portable-pty
      — tmux requires a TTY even in control mode; plain pipes fail with
      `tcgetattr: Inappropriate ioctl for device`
- [x] `find_tmux()` probes `/opt/homebrew/bin`, `/usr/local/bin`, `PATH` —
      Tauri-launched processes inherit a minimal PATH that omits Homebrew
- [x] Line-based parser (`parse.rs`) with state-aware reader (`client.rs`):
      `%begin` / `%end` / `%error` blocks, `%output`, `%window-add`,
      `%window-close`, `%window-renamed`, `%session-changed`,
      `%session-renamed`, `%layout-change`, `%pane-mode-changed`,
      `%continue`, `%pause`, `%client-detached`, `%exit`, `%unknown`
      passthrough
- [x] Octal-escape decoding for `%output` byte streams
- [x] Stable ID tracking (`@5`, `$3`, `%1`) — never indices
- [x] `spawn_local()` blocks until first `%session-changed` (5s timeout)
      — without this, `#{pane_id}` etc. return empty because tmux's format
      expansion needs a current target
- [x] stderr drained to `tracing::warn!` so tmux complaints are visible
- [x] `TmuxClient` API: `send_keys` (hex-mode for raw bytes),
      `resize_pane`, `new_window`, `split_pane`, `kill_window`,
      `select_window`, `select_pane`, `rename_window`, `list_windows`,
      `list_panes`, generic `send_command`
- [x] Event stream (`mpsc::UnboundedReceiver<Notification>`) returned from
      `spawn_local`; forwarded by helm-app into a Tauri Channel
- [x] 8 parser unit tests + 3 live integration smoke tests, all green
      (1 send-keys round-trip ignored — wedges on capture-pane, but the
      same path is exercised every keystroke in the actual app)
- [x] `Drop` calls the `ChildKiller` so tmux client process dies; tmux
      *server* lives on, with the session intact for reattach

**`helm-app`:**
- [x] AppState gains `tmux: tokio::sync::Mutex<Option<Arc<TmuxClient>>>`
- [x] `tmux_attach` command spawns the local client and forwards every
      Notification through a typed `Channel<TmuxNotification>`
- [x] `tmux_send_keys`, `tmux_resize_pane`, `tmux_new_window`,
      `tmux_split_pane`, `tmux_kill_window`, `tmux_select_window`,
      `tmux_select_pane`, `tmux_rename_window`, `tmux_list_windows`,
      `tmux_list_panes` commands

**Frontend:**
- [x] `lib/tmux.ts`: per-pane output pub/sub with replay buffer for late
      subscribers; tree-mutation routing into the Zustand store
- [x] Store extended with `tmux` slice (sessionId/Name, windows/panes
      Maps, detachedReason) and reducer-style actions
- [x] `TmuxPane` component bound to a tmux pane id — input via
      `tmux_send_keys`, output via subscribePaneOutput, debounced resize
- [x] App.tsx replaces the phase-0 `<Pane />` with `<TmuxPane>`; sidebar
      rows render from the live store; clicking a window calls
      `tmux_select_window`

### Outstanding (carry into next iteration)

**Refinements before moving to phase 2:**
- [x] Listen for `%session-window-changed` and update the store's `active`
      flags so clicking a sidebar row swaps the rendered pane
- [x] Listen for `%window-pane-changed` for split layouts (notification
      handler + store action; UI rendering of multi-pane layouts is still
      single-pane-only — see split bullet below)
- [x] Inline rename on double-click → `tmux_rename_window`
- [x] Keyboard shortcuts: `Cmd+T` (new window), `Cmd+W` (close window),
      `Cmd+]` / `Cmd+→` (next), `Cmd+[` / `Cmd+←` (prev). xterm's
      `attachCustomKeyEventHandler` vetoes Cmd-prefixed keys so the shell
      never sees them.
- [ ] Pane split: `Cmd+D` / `Cmd+Shift+D` → `tmux_split_pane` (the command
      exists, but rendering multiple panes side-by-side in a single window
      is real layout work — deferred to phase 2 alongside multi-host)
- [ ] Drag-to-reorder windows in the sidebar (persisted order, since tmux
      reuses indices once windows close)
- [ ] First-launch tmux *setup* screen wired to a detector + installer
      (currently we just error out if tmux is missing)
- [ ] Reconnect/reattach when the underlying transport bounces (e.g. the
      tmux server is killed externally)

### Cleanup pass (after phase 1 was working)
After getting the artifacts fixed, did a tech-debt sweep:

- **Removed `helm-pty` crate entirely.** The phase-0 local-PTY path was
  superseded by tmux on every host (per the spec's "tmux always, everywhere"
  principle). One pipeline is better than two parallel ones.
- **Removed `pty_*` Tauri commands** (`pty_spawn` / `pty_write` / `pty_resize`
  / `pty_kill`) and the `PtyEvent` / `PtySpawn` / `PtyId` domain types.
- **Removed the unused `AppState.hosts / .workspaces / .windows / .panes`
  DashMaps.** The frontend Zustand store owns the tree; a parallel Rust
  copy isn't needed until commands act on it server-side, which doesn't
  happen yet. Phase 2 may want them back when multi-host needs server-side
  resolution.
- **Deleted dead `Pane.tsx`** (phase-0 component, replaced by `TmuxPane`).
- **Simplified `lib/tmux.ts`** — output that arrives before a pane has a
  subscriber is dropped, not buffered-then-discarded. TmuxPane subscribes
  synchronously on mount and captures the buffer separately, so any
  pre-subscribe output predates the pane existing in the UI anyway.
- **Replaced `disposed` flag pattern in `TmuxPane`** with `AbortController`
  for idiomatic cancellation through the async chain.
- **Switched the cmd channel from `tokio::sync::mpsc` to `std::sync::mpsc`.**
  PTY I/O is fundamentally blocking, so the writer is a `std::thread`; using
  a plain sync channel is more honest about that than `blocking_recv()` on a
  tokio queue. The event channel stays tokio because the consumer is
  genuinely async.
- **Reduced `%session-changed` ready-gate timeout from 5s to 1s.** Local
  tmux startup is consistently 10–50ms; 5s was over-conservative.
- **Re-enabled `send_keys_hex_round_trip` smoke test.** The title-escape
  strip fix unwedged it — full 11/11 tests passing again.

### Lessons learned (the bugs that actually cost time)

- **tmux `-CC` requires a TTY.** It calls `tcgetattr` at startup; without
  a controlling terminal it dies with "Inappropriate ioctl for device".
  Solution: always spawn it inside a `portable-pty` openpty pair.
- **Tauri-launched processes get a stripped PATH.** Even with tmux installed
  via brew, the helm process can't find `/opt/homebrew/bin/tmux` unless
  we probe known locations. Fix: `find_tmux()`.
- **Format expansions are silent until tmux is "ready".** `#{pane_id}`,
  `#{session_name}`, etc. return empty until the control client has a
  current target, signaled by the first `%session-changed` notification.
  `spawn_local()` now blocks on that before returning.
- **`%`-prefixed lines inside `%begin/%end` are data, not notifications.**
  tmux pane IDs literally start with `%` (`%0`, `%1`, …). A naive
  line-by-line parser will drop them as "Unknown notification" and the
  command response comes back empty. State machine needs to know whether
  it's inside a response block; only `%end`/`%error` close the block,
  everything else is data.
- **Strip screen/tmux DCS title escapes (`ESC k <title> ESC \`) before
  forwarding to xterm.js.** oh-my-zsh's `omz_termsupport_*` precmd hooks
  emit these on every command to set the tmux window name. xterm.js
  doesn't recognise this older format (it handles the OSC variant
  `\033]2;…\007` only) and falls through to writing the title text as
  literal characters at the cursor position. Result: the post-Enter
  cursor is at col 0 of the output row, the title text gets written
  there as `ls` / `echo` / `cd`, then the actual command output prints
  next to it — yielding the infamous `lsApplications`, `echohi`, `cd%`
  artefacts. Stripped in `helm-tmux::parse::strip_screen_title_escapes`.
- **`%output` data must be parsed from raw bytes, not from `&str`.**
  `BufReader::read_line` deserialises into a `String` and panics on
  invalid UTF-8 — which happens whenever tmux's output buffer splits a
  multi-byte codepoint at a chunk boundary. Even a `from_utf8_lossy`
  fallback is wrong: it replaces partial sequences with `U+FFFD`, so
  box-drawing chars in TUIs (`─` = `\xE2\x94\x80`, claude's intro
  screen) come through as `���`. Fix is byte-precise: `read_until(b'\n')`,
  detect `%output ` prefix on the raw bytes, dispatch to
  `parse_output_bytes(&[u8])` which preserves the data section verbatim
  for xterm.js to re-stitch with its own multi-chunk UTF-8 buffering.
  Lossy String conversion is fine for everything else (block markers,
  notifications) since those are protocol ASCII.
- **xterm.js's WebGL renderer crashes under TUI load** with
  `this._renderer.value.dimensions is undefined`. Full-screen redraws
  at 60fps (claude, vim, htop) can stress the GPU process enough to
  drop the WebGL context; xterm's renderer becomes a half-disposed
  husk that throws on every layout call. Fix: wire `WebglAddon.onContextLoss`
  to `dispose()` the addon, leaving xterm to fall back to its built-in
  canvas renderer. Slower but resilient. Kicks in transparently.
- **`tmux_attach` must be idempotent across webview reloads.** `Cmd+R`
  resets all JS state and re-runs `attachTmux()`, but Rust still holds
  the prior `TmuxClient`. Original implementation returned
  `Err("already attached")`, leaving the user looking at an empty pane
  with no recovery. Fixed: every attach drops the old client first
  (its `Drop` SIGKILLs the old `-CC` process; the tmux *server* and
  session keep running, so anything you had going stays going).
- **Smoke tests must fail loudly when tmux isn't installed.** The original
  helper silently `return`ed if `tmux -V` failed, which cargo reports as
  "ok" — so the tests "passed" without exercising anything for ages. Fixed
  to `panic!()` with an install hint.

### Decisions still open
- **Min tmux version.** Brief says ≥ 3.2 for control mode features. Keep it.
- **Adopt-or-overwrite existing sessions.** Currently we always attach to
  the session named `helm`, creating it if missing. If the user has tmux
  running with unrelated sessions, those don't show in the sidebar yet —
  good enough for now; revisit before phase 2.
- **Persistence schema.** Window order overrides + custom names need
  somewhere to live. Probably JSON at `~/Library/Application Support/Helm/state.json`
  via Tauri's `path` plugin.

---

## Phase 2A — Multi-host refactor · **done**

Pre-SSH plumbing. Generalised the `tmux` plumbing so a host is the unit of
attachment; localhost became "host id 0" rather than the only path.

### Done
- `helm-tmux::TmuxClient::spawn_with_io(reader, writer, cleanup, timeout)` —
  IO-agnostic core. `spawn_local` is now a thin wrapper. Cleanup hook
  generalised from `ChildKiller` to `Box<dyn FnOnce>`.
- `helm-domain::HostEvent` tagged enum: `Tmux | Status | HostAdded | HostRemoved`,
  all carrying `host_id`.
- `helm-app::AppState` ⇒ `DashMap<HostId, Arc<Mutex<HostEntry>>>` + stable
  `local_host_id` + global `event_tx`. Localhost seeded by default.
- New commands: `host_list`, `host_local_id`, `host_subscribe`, `host_connect`,
  `host_disconnect`, `host_add` (in-memory), `host_remove`. Every `tmux_*`
  command takes `host_id` first.
- Frontend store: `Map<HostId, …>` per host; `lib/host.ts` (was `tmux.ts`)
  demuxes the single tagged channel.

---

## Phase 2B — SSH transport + multi-workspace UI · **done**

SSH dial-tone, plus a real workspace model on top: every tmux session on
a host is a workspace; helm doesn't claim ownership of a particular
"helm" session anymore.

### SSH transport (russh)
- `helm-ssh::connect(target, auth, command, timeout)` over russh 0.46.
- Auth: agent (walks identities), key file (passphrase-aware), password
  (transient `String`, Keychain wiring is 2C).
- Single jump host via `channel_open_direct_tcpip` + `connect_stream`.
- TOFU host-key handler — real `~/.ssh/known_hosts` check is 2D.
- **Architecture surprise:** `russh::Channel<Msg>` carries a
  `tokio::mpsc::UnboundedReceiver` which is `!Sync`, so any future that
  borrows the channel (`request_pty`, `exec`, `data` all take `&self`)
  is `!Send`. Wallpapering with `Box::pin` / `spawn_blocking` doesn't
  help because russh expects to keep running on the runtime that
  produced its tasks. Solution: the SSH session owns a *dedicated OS
  thread* with its own current-thread tokio runtime. `helm-ssh`
  exposes blocking `std::io::pipe()` halves; from helm-app's view the
  SSH transport looks identical to a local PTY.
- App-side bridge: `helm_ssh::connect` runs on `spawn_blocking`. The
  returned `PipeReader`/`PipeWriter` plus a `disconnect()` cleanup
  closure go straight into `TmuxClient::spawn_with_io`.

### Multi-workspace
- Connect command is now `tmux -CC attach 2>/dev/null || tmux -CC new-session -A -s <default>`.
  We attach to whatever the user already has running; only create the
  fallback session if no sessions exist on the server.
- `TmuxClient::list_sessions / new_session / kill_session / rename_session`,
  exposed via `tmux_list_sessions / tmux_new_session / tmux_kill_session /
  tmux_rename_session`.
- `tmux_new_window` accepts an optional `session_id` so windows can be
  created in a specific workspace.
- Frontend store: per-host `HostSessions { workspaces: Map<sessionId, TmuxWorkspace>, activeWorkspaceId }`.
  Each `TmuxWorkspace` owns its own windows + panes maps.
- `lib/host.ts::refetchTree` lists sessions, then list-windows -a and
  list-panes -a with `#{session_id}` in the format. Notifications
  (%session-changed, %sessions-changed, %session-renamed, %window-add,
  %window-close) trigger re-runs.
- Sidebar: 3-level (host → workspace → window). Click a workspace to
  set active; double-click renames; right-click triggers kill (with
  confirm). `+ workspace` button auto-names `workspace N` (first free
  integer). Cmd+T new window in active workspace; Cmd+Shift+T new
  workspace; Cmd+Shift+W kill workspace.
- Active-workspace fallback: when the active workspace is killed, the
  store picks the first remaining workspace (or null if none).

### Lessons learned (2B-specific)
- **SSH PTY mangles tabs.** sshd allocates the PTY in default cooked
  mode (canonical, ECHO, ICANON, ICRNL, …). When we send a tab-delimited
  format string to remote tmux as command input, the line discipline
  rewrites the tabs into something else (looks like 0x5F `_` from the
  far side). `dbg.tabwin()` confirmed: same format with `|` delimiter
  round-trips cleanly, with `\t` it doesn't. Worked around by switching
  the bootstrap delimiter to `|`. Proper fix: pass explicit terminal
  modes to `request_pty` (raw mode, ECHO/ICANON off) so tmux's stdin is
  byte-clean. Tracked in **Deferred** below.
- **tmux escapes control chars in command-response blocks.** Block data
  between `%begin` / `%end` has `\t` → `\011`, `\` → `\\`, etc., the
  same scheme as `%output`. We previously decoded only on `%output`.
  Now `decode_octal` runs on block data too.
- **block_in_place panics on a current-thread runtime.** The SSH I/O
  thread builds a current-thread runtime; the original `from_app.read()`
  in `block_in_place` produced a silent panic that killed both pumps,
  surfacing as "tmux did not emit %session-changed within 10s" much
  later. Replaced with `spawn_blocking`.
- **Non-interactive SSH on macOS has a stripped PATH.** Same class of
  bug as Tauri's stripped PATH locally. Prefixed the connect command
  with `export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/homebrew/bin:$PATH"`
  so remote tmux is discoverable without depending on the user's shell init.
- **`window.__TAURI__` isn't exposed by default in Tauri 2.** We expose
  the typed `commands` surface as `window.helm` from `main.tsx` so
  devtools pastes can drive the IPC layer. Cheap, and pays off
  whenever we need to reach into the running app for diagnostics.
- **HRTB Send inference in the `#[tauri::command]` macro.** Borrows
  across awaits with inferred lifetimes (`&Host`, `&SshAuth`, …) trip
  the macro's Send bound check. Two patterns we now rely on:
  (1) take owned values past any `State<'_, …>` prelude;
  (2) push russh-typed work onto a separate task / dedicated thread so
  the command future never carries russh types directly.

### Stage-2B follow-up polish

A round of UX bugs surfaced once the user actually drove the multi-workspace
flow. All fixed in-place; documenting the non-obvious ones:

- **Stale workspace tree on disconnect.** Killing the last workspace's last
  window was leaving the dead window/pane in the sidebar plus a frozen
  xterm capture. `lib/host.ts` now clears workspaces on the
  `connected → disconnected` transition; `refetchTree` no longer clobbers
  the store on transient list-sessions failures (a parallel refetch from
  the connected-status handler would otherwise wipe the good data).
- **Workspace expand/collapse independent of active.** Clicking a
  workspace row used to *force* it active (auto-collapsing siblings).
  Now it just toggles its own expansion. Active-workspace is implicit:
  whichever workspace owns the window the user last clicked. State
  tracked as `collapsedWorkspaces: Set<sid>` per host (default expanded).
- **`new-session -d` panes render at 80×24 in our larger xterm.** A
  detached session has no attached client, so tmux never resizes its
  panes. Rendering it shows cursor offset + invisible typing until the
  user switches away and back. Fix: `tmux_switch_client(session_id)`,
  called by `selectWorkspace` whenever the active workspace changes.
  tmux re-evaluates sizing against the now-attached client and emits
  fresh layout.
- **Connect command was racy against a dying tmux server.** The old
  `tmux -CC attach 2>/dev/null || tmux -CC new-session -A -s X` shell
  wrapper let `attach` "succeed" briefly against a server in the middle
  of shutting down — short-circuiting the `||` and leaving us with a
  Connected→immediate-Disconnected sequence. Replaced with a
  `tmux list-sessions` probe that branches deterministically:
  ```sh
  if [ -n "$(tmux list-sessions -F '#{session_id}' 2>/dev/null)" ]; then
      exec tmux -CC attach
  else
      exec tmux -CC new-session -A -s '<bootstrap>'
  fi
  ```
- **Localhost should never *look* disconnected.** Network-style
  disconnection is a meaningless concept for the local machine — the
  user's mental model is "localhost is always available; sometimes I
  have a workspace open here, sometimes I don't." `displayedHostStatus`
  pins localhost's dot to `connected` regardless of underlying tmux
  client state. Click on the localhost row no longer auto-reconnects;
  the `+ workspace` button drives the lifecycle.
- **`+ workspace` works from any state.** Old behavior failed silently
  with "host not connected" when clicked while disconnected. Now:
  parametrized `host_connect(host_id, bootstrap_workspace?)` lets the
  `+` button pass the new workspace's name as the bootstrap, so if no
  sessions exist on the server we create exactly that workspace
  (instead of a stray `main` first). After connect, find by name; if
  bootstrap created it we just select; otherwise call `tmux_new_session`.
- **`Cmd+W` on the active workspace's last window made *all* workspaces
  disappear.** Tmux's default `detach-on-destroy on` kicked our
  control client out the moment the session it was attached to died,
  even though other sessions still existed on the server. Set
  `set-option -g detach-on-destroy off` once at connect time so tmux
  *switches* us to the most-recently-active remaining session instead
  of detaching. Caveat: this is server-global, so other clients on the
  same tmux server inherit the behavior until the server restarts.
- **Dropped the WebGL renderer.** xterm's `@xterm/addon-webgl` allocates
  one WebGL context per Terminal. Browsers cap WebGL contexts per
  page (Chromium ≈ 16, Safari similar) and contexts release on GC, not
  synchronously on `dispose()`. Combined with React strict-mode's
  double-mount in dev and rapid window switching, we'd blow past the
  cap, see "too many active WebGL contexts", and xterm starts throwing
  `_renderer.value.dimensions is undefined` as evicted contexts leave
  half-disposed renderers behind. Built-in canvas renderer has no cap
  and is fast enough for typing/reading. Reintroducing WebGL with a
  proper context-budget manager is a Phase-5 perf task — see Deferred.
- **Native terminals use platform-GPU APIs (Metal / OpenGL / WebGPU)
  that don't have these caps**, which is why iTerm2 / Alacritty /
  Kitty / WezTerm / Ghostty / Warp can ship GPU rendering everywhere
  without context-loss drama. Webview-based terminals (VS Code, Hyper,
  Tabby — and helm) all have to deal with the same constraints we just
  ran into. VS Code's solution is sophisticated context-lifecycle
  management; we can borrow that pattern when the time comes.
- **Keep-alive panes + pre-hydration cache.** Every window switch was
  paying the `tmux capture-pane` round-trip on remount — perceptibly
  slow over SSH (50–500ms depending on scrollback size). Two changes
  fixed this:
  1. `App.tsx` now mounts each visited pane at `position: absolute;
     inset: 0` and toggles `display: none` on inactive ones. The xterm
     stays alive with its full buffer + live subscription, so switching
     back is instant. A GC effect drops keys whose underlying tmux
     pane no longer exists.
  2. After every connect, `prehydrateCaptures` walks every pane and
     stashes a *visible-buffer* `capture-pane` (no scrollback) in
     `store.paneCaptures`. Each capture is ~5KB instead of ~500KB, so
     bulk pre-hydrating a remote host with 12+ panes finishes in
     ~120ms total without saturating tmux's command queue. First mount
     of any pane reads from cache instantly; cache misses (newly-created
     panes) fall back to a live `capture-pane` *with* scrollback so
     fresh panes still get their full history.
  - Critically, pre-hydration is gated by `prehydratedHosts: Set<HostId>`
    so it runs *exactly once per connect transition*. Earlier we kicked
    it off from inside `refetchTree`, which fires on every tmux event —
    multiple parallel pre-hydrate passes would queue dozens of capture
    requests, starving user input behind the backlog. Lesson:
    background work over a serialized command queue (which tmux's
    control client is) needs a coalescing guard.
- **Optimistic active-window selection.** Clicking a window in a host
  other than the currently-active one used to flash whichever window
  was previously active in that host — because `win.active` is the
  tmux-provided flag and only updates after `%session-window-changed`
  round-trips back from the server. Fix: the click handler writes
  `setActiveHost` + `setActiveWindow` to the local store synchronously,
  then dispatches `tmux_select_window` async. Tmux's later confirmation
  is idempotent. Belt-and-braces: the focused-state derivation also
  gates on `activeHostId === h.id` so a non-active host's stale
  active flag can never leak a highlight.

---

## Phase 2C — Persistence + Keychain + host editor · **done**

Promoted the in-memory `host_add` testing handle to a real persistence
layer plus UI. Adding/editing/deleting hosts is now end-to-end through
the helm window — no more devtools paste required.

### Done
- **Persistence** (`helm-app/src/persistence.rs`):
  `~/Library/Application Support/Helm/hosts.json` (via `dirs::config_dir()`).
  Atomic write — serialize to a `.tmp` sibling, rename over the canonical
  path. Read on `AppState::default()`; missing file → empty registry; parse
  failures surface as errors. Localhost is intentionally not persisted
  (its id is process-local each boot).
- **Commands**: `host_add` / `host_remove` are gone. Replaced with
  `host_save(host)` (upsert + persist) and `host_delete(id)` (drop +
  persist + Keychain cleanup). `HostAdded` event doubles as upsert in
  the frontend store.
- **Keychain wrapper** (`helm-app/src/keychain.rs`): `security-framework`
  generic-password store, service `app.helm.host`, account = host UUID.
  `host_save_password(id, password)` writes; `connect_for_host` reads
  via `keychain::get_password` only at connect time. `errSecItemNotFound`
  on delete is treated as success — the user wants it gone, it's gone.
  Non-mac builds get a stub that returns `"macOS-only for now"`.
- **`~/.ssh/config` parsing**: `ssh2-config` crate. `ssh_config_aliases()`
  returns flattened `{alias, hostname?, user?, port?}` records, sorted
  alphabetically. Wildcards (`Host *`) are skipped because they're not
  useful as identities. Failure to read or parse → empty list, not an
  error (host editor stays usable on machines without an ssh config).
- **Host editor modal** (`src/features/host-editor/HostEditorModal.tsx`):
  fields for name / hostname / user / port / auth / key-path / password
  / default workspace. Hostname field uses an HTML `<datalist>` populated
  from `ssh_config_aliases()` — typing an alias auto-fills hostname,
  user, and port. Password field only saves to Keychain if the user
  actually typed something; in edit mode an empty password leaves the
  existing entry alone. Triggered by a `+` next to the HOSTS section
  header in the sidebar.
- **Right-click delete** on host rows (skipped for localhost). Confirm
  dialog notes that tmux sessions on the remote machine are unaffected.
  Double-click on a host row opens the editor in edit mode.
- **Welcome empty-state**: when no remote hosts exist, the sidebar
  renders a dashed-border "Add a remote host" CTA explaining what helm
  is. Clicking opens the editor.

### Lessons learned (2C-specific)
- **Tauri's specta-typed bindings are unforgiving about field names.**
  I hit one round of "Cannot read properties of …" because I was
  passing `host.jumpHost` (camelCase) to a serialized `Host` whose
  Rust field is `jump_host`. Specta's TS bindings *do* preserve the
  Rust serde name. Lesson: just use the type from `@bindings` and let
  TS catch the mismatch.
- **`<datalist>` is a free SSH-config autocomplete.** No custom
  combobox component, no react-select, no popover library — a plain
  `<input list="…">` plus `<datalist>` gets you a dropdown filtered by
  what the user types, with no library or accessibility code. The
  styling is the OS-default, which fits a dev tool fine. We may want a
  custom component later for richer hints (showing the resolved
  hostname inline) but this is the right starting point.

---

## Phase 2D — Resilience · **done**

The polish layer that makes "remote" feel local.

### Done

**SSH PTY raw modes (`helm-ssh`):**
- `RAW_TERMINAL_MODES` slice passed to `request_pty` mirrors `cfmakeraw(3)`:
  ECHO/ICANON/ISIG/IEXTEN off, OPOST/ONLCR off, ICRNL/INLCR/IXON/IXOFF off.
  Helps with echo and canonical-mode side effects.
- **`\t` delimiter not yet reliable.** First pass tried to flip
  `lib/host.ts` from `|` back to `\t` on the assumption that the new
  modes would make tabs round-trip cleanly. Some sshd configurations
  ignore terminal modes on non-interactive `exec` channels (and one
  mode we initially included, `IUCLC`, is Linux-only and may make
  BSD/macOS sshd silently reject the whole list). Result: `\t`
  mangled into `_`, format strings parsed as one literal field, the
  workspace tree refetch produced empty/garbled records → green
  status but "opening session…" forever. Reverted to `|`. The terminal
  modes are still set (defensive) but we don't rely on byte-clean
  tabs.

**Toast component + undo-kill-workspace:**
- New `src/ui/Toast.tsx` + `src/ui/ToastHost.tsx`. Bottom-right portal,
  one timer per timed toast, optional `deferredAction` fires on
  expiration unless the user dismisses first.
- `App.tsx::killWorkspace` replaced its `window.confirm` with a 5s
  toast — Undo cancels the deferred kill, otherwise `tmux_kill_session`
  fires.

**Real `~/.ssh/known_hosts` host-key prompt:**
- `helm-ssh::Client::check_server_key` now consults
  `russh_keys::check_known_hosts`. On `Ok(false)` (unknown) or
  `Err(KeyChanged{line})` (mismatch), a new `HostKeyPrompter` trait is
  invoked. With no prompter the connection is refused — never silent
  TOFU.
- New domain types: `HostKeyPromptKind { Unknown | Changed{ line } }`,
  `HostKeyDecision { Reject | AcceptOnce | TrustPermanently }`,
  `HostEvent::HostKeyPrompt { … }`.
- `AppHostKeyPrompter` in `helm-app` bridges the trait to the global
  event channel: stashes a `oneshot::Sender<HostKeyDecision>` in
  `AppState.pending_host_key_prompts: Arc<DashMap<HostId, _>>`, emits
  the prompt event, awaits the answer.
- `host_key_prompt_response(host_id, decision)` command pops the oneshot
  and fires it. `TrustPermanently` calls `learn_known_hosts` (only for
  `Unknown` — appending a duplicate for a `Changed` key would conflict
  with the existing entry).
- Frontend: `useStore.hostKeyPrompts: Map<HostId, HostKeyPrompt>`,
  routed in `lib/host.ts`, rendered by
  `src/features/host-key/HostKeyPromptModal.tsx` mounted once at App
  level. Three buttons: Reject / Accept once / Trust permanently
  (`Changed` hides the third — OpenSSH-style "edit your known_hosts to
  resolve").

**Reconnect ladder (the supervisor refactor):**
- New `HostStatus::Reconnecting` distinct from `Connecting`. UI maps it
  to the amber `connecting` dot color but to a distinct overlay so the
  user can read "Reconnecting · Retry now."
- `HostEntry` gained `voluntary_disconnect: bool` and
  `supervisor: Option<AbortHandle>`.
- `do_connect` no longer carries the forwarder loop. The first
  successful `connect_for_host` spawns `supervise(...)`, which owns
  *both* the forwarder loop and the reconnect ladder for the lifetime
  of the connection.
- On EOF (transport drop, tmux %exit, etc.):
  - If `voluntary_disconnect` is set → emit Disconnected, exit.
  - Else emit Reconnecting, sleep `[1, 2, 4, 8, 30]s` (clamped at 30s
    after that), retry `connect_for_host`. Reset the index on success.
  - Localhost (`port == 0`) originally capped at 3 retries — see
    Phase 2D.1, which removed the cap after it caused unrecoverable
    silent failures.
- `host_disconnect` / `host_save` / `host_delete` set
  `voluntary_disconnect = true` and abort the supervisor handle, so
  user-driven teardowns never race with a backoff retry.
- `host_connect` aborts any prior supervisor and resets the flag
  before starting fresh.

**Reconnecting overlay:**
- `src/features/workspace/ReconnectingOverlay.tsx`: frosted card over
  the pane area with a "Retry now" button (calls `host_connect`).
  Mounted as a sibling of the keep-alive pane stack inside `<main>`,
  gated on `activeStatus === 'reconnecting' && activeHost.port !== 0`.
  The TmuxPanes underneath stay mounted so the user keeps seeing their
  last frame instead of an empty terminal.
- `lib/host.ts` adds a `reconnecting` arm that does NOT clear the
  workspace tree or pane captures — that defeats the keep-alive UX.
  Cleanup happens on the eventual `disconnected` / `error` transition.

**SCNetworkReachability monitor:**
- New `crates/helm-app/src/reachability.rs`. Mac-only impl spawns a
  dedicated `helm-reachability` thread that owns a `CFRunLoop`,
  registers `SCNetworkReachability::set_callback` for `0.0.0.0` (the
  Apple "any usable network" idiom), and forwards `online: bool` into
  a `tokio::sync::watch`. Non-mac stub returns a watch that's always
  online.
- `AppState.network_online: watch::Receiver<bool>`. Supervisor's
  backoff sleep is a `tokio::select!` over `sleep | watch.changed()`;
  on a `false → true` transition the backoff index resets to 0 and the
  next attempt fires after a 1s tick.
- Cargo: `system-configuration = "0.7"` and `core-foundation = "0.9"`
  added under `[target.'cfg(target_os = "macos")'.dependencies]`. The
  `core-foundation` version is pinned to 0.9 because
  system-configuration 0.7's API takes a `&core_foundation::CFRunLoop`
  from the 0.9 line — using 0.10 produces a "two different versions of
  the same crate" type mismatch.

### Lessons learned (2D-specific)

- **`#[serde(tag = "kind")]` collides with a payload field also named
  `kind`.** First pass had `HostEvent::HostKeyPrompt { kind:
  HostKeyPromptKind, … }` which serde rejected with `variant field name
  "kind" conflicts with internal tag`. Renamed the payload field to
  `prompt`. Trap to remember whenever extending a tagged enum.
- **`russh_keys::learn_known_hosts` isn't re-exported at the crate
  root.** Only `check_known_hosts` and `check_known_hosts_path` are
  pub-used in lib.rs; everything else lives behind the
  `russh_keys::known_hosts::` module path. Took a confused minute
  before re-checking the source.
- **`russh::Pty` enum names and the meaning of "1" vs "0".** The
  numeric value passed alongside each Pty constant is the *new value*
  for that tcsetattr-style flag, not a boolean enable. ECHO=0 means
  echo off; IGNPAR=1 means ignore framing-error chars. Got it right
  the first time but worth flagging — easy to write IGNPAR=0 thinking
  "off."
- **`core_foundation` dep alignment.** Adding `core-foundation = "0.10"`
  alongside `system-configuration = "0.7"` (which still depends on 0.9
  internally) produces a silent type mismatch — same struct name,
  different generic parameterization. Pin the direct dep to whatever
  the transitively-requesting crate uses, or refactor when both sides
  upgrade together.
- **`SCNetworkReachability` isn't `Send` by default** — the upstream
  test file even has its own `unsafe impl Send` for one specific test.
  Solution: own everything inside the dedicated thread and only
  publish primitive `bool` through a `watch` channel. The reachability
  object never crosses thread boundaries.
- **Reconnect ladder vs initial connect.** Auto-retrying *initial*
  connects is wrong: a rejected host key looks identical to a TCP
  failure from the supervisor's vantage, and you'd loop forever
  surfacing prompts. Resolution: the supervisor is only spawned after
  one successful connect. Initial-connect failures bubble straight up
  to `Status::Error` — user re-clicks to retry. The ladder only
  protects established connections that drop.
- **`watch::changed()` has subtle initial-state semantics.** A fresh
  `Receiver` is "marked unseen" until you call `borrow()` or
  `borrow_and_update()`, which means the first `.changed()` returns
  immediately. Used `borrow_and_update()` to mark the current value
  seen *and* capture it (`was_offline`), so the early-wake only
  triggers on a real `false → true` transition.

---

## Phase 2D.1 — Localhost reliability follow-up · **done**

Fixes a class of silent failures on localhost that surfaced once 2D's
reconnect ladder was in place. User-facing symptom: "localhost looks
connected but typing does nothing and Cmd+T doesn't spawn windows" —
sometimes from a fresh launch, sometimes after a few dev cycles. Three
independent root causes, all converging on the same symptom.

1. **Localhost retry cap was too aggressive.** 2D capped local
   reconnects at 3 attempts then exited with `Status::Error`. But
   `displayedHostStatus()` pinned `port == 0` to green regardless, so
   the dot stayed green while the underlying tmux client was actually
   gone. Sidebar's reconnect-on-click was also gated on `port !== 0`.
   Result: a dead local tmux became unrecoverable from the UI — the
   user had to restart helm.

2. **Orphan `tmux -CC` clients accumulating across dev cycles.** Cargo's
   SIGTERM during rebuild bypasses our `Drop`-driven cleanup, leaking
   the master PTY fd. The orphan keeps the slave fd open with a kernel
   buffer that nobody drains. Once that buffer fills, tmux's broadcast
   loop blocks writing to the dead reader, wedging the entire server's
   command queue. New helm instances would attach successfully (the
   initial handshake fits in the buffer), but every subsequent command
   would hang silently — typing, window creation, navigation, all dead
   with no errors.

3. **React 19 StrictMode double-spawn.** The boot effect's
   `connectHost(localhost)` ran twice in dev (mount → unmount → mount).
   Two parallel `host_connect` futures raced through `do_connect`, both
   spawning `tmux -CC` clients. Whichever lost the `entry.tmux = ...`
   write got dropped, but the `Drop`-fired kill signal occasionally
   arrived too late and a client leaked.

### Done

**Backend (`helm-app::commands::supervise`):**
- Removed `LOCAL_MAX_RETRIES`. Localhost now retries indefinitely with
  the same `[1, 2, 4, 8, 30]s` schedule as remote — each attempt re-runs
  `spawn_local`, which `exec`s `tmux -CC new-session -A` and brings up
  a fresh server. If tmux genuinely can't be brought up (binary missing,
  broken install), the supervisor sits in `Reconnecting` and the
  frontend surfaces the reason rather than silently giving up.
- Track `last_error: Option<String>` across the loop and stamp it on
  subsequent `Reconnecting` status emits, so the overlay shows *why*
  a reconnect is stuck (e.g. "tmux not found") instead of a generic
  spinner. Cleared on a successful Connected transition.
- Dropped the speculative "refresh host from entry" line in the
  reconnect path — `host_save` aborts the supervisor before mutating
  the entry, so the refresh was never reachable.

**Backend (`helm-tmux::client::spawn_local`):**
- New `probe_tmux()` returning `Healthy(known_pids) | NoServer | Wedged`,
  driven by `tmux display-message -p '#{pid}'` (server PID, also the
  healthy/wedged probe) plus `list-clients -F '#{client_pid}'`. Both
  with a 500ms timeout.
- `kill_orphan_cc_clients(&known)` walks `ps -axo pid=,command=` for
  any `tmux -CC` match whose pid is *not* in `known` and SIGTERMs it.
  Wired into the top of `spawn_local`: probe → reap when healthy, skip
  when no server, return error when wedged. The error propagates to
  `last_error` and the user sees it in the `ReconnectingOverlay`.

**Frontend `displayedHostStatus()`:**
- Localhost stays green only when actually `connected` / `idle` /
  unknown. `reconnecting` / `error` / `disconnected` flow through to
  the real `hostRowStatus()` mapping. The "localhost is always
  available" mental model still holds for the steady state; trouble
  surfaces honestly.

**Frontend recovery affordances:**
- Sidebar host-row click triggers `connectHost` whenever status isn't
  `connected`/`idle`, for both local and remote. Drops the prior
  `port !== 0` gate, giving localhost a manual recovery path it never
  had.
- `ReconnectingOverlay` no longer gated on `port !== 0`. Distinct copy
  for local ("Local tmux is being respawned.") vs remote ("The
  transport dropped. Retrying with backoff."). Reads `hostErrors[id]`
  and surfaces the supervisor's last error in red mono text below the
  spinner.
- Empty pane area surfaces `hostErrors[activeHostId]` when status is
  `error` — "error · tmux not found" instead of an upbeat "no
  workspaces, press ⌘⇧T."
- New `useStore.hostErrors: Map<HostId, string>` + `setHostError`,
  populated from `Status` events that carry an error and cleared on
  Connected.

**Frontend tree-on-reconnect:**
- For a localhost `connected → reconnecting` transition, clear the
  workspace tree and pane captures. Local tmux server state usually
  goes away with whatever killed it, so the cached session/window/
  pane ids point at fossils — typing into them silently no-ops while
  the supervisor respawns. Remote keeps the existing freeze-frame UX
  (remote tmux state survives transport drops).

**Frontend StrictMode guard:**
- Module-level `bootStarted` flag in `App.tsx`. The boot effect bails
  on the StrictMode-induced second mount, so the localhost connect
  fires exactly once per process. A `useRef` doesn't work here —
  StrictMode creates a fresh ref on the second mount.

**Silent-failure surfacing:**
- `tmuxSendKeys` in `TmuxPane` and `tmuxSelectWindow` in `App`'s
  keyboard handler now `.then` and `console.warn` on failure instead
  of being fully `void`-ed. The dot/overlay/empty-state covers the
  macro state for the user; the log is for our own debugging when
  helm commands fail mid-session.

### Lessons learned (2D.1-specific)

- **`displayedHostStatus` lying about localhost is an antipattern.**
  The original "localhost is always green" override was correct for
  the network-disconnection mental model but generalized incorrectly
  to every non-network failure mode. Any time the visible state
  diverges from the operational state, silent breakage is one
  refactor away. Fix: the override applies only to states that don't
  represent operational trouble (Connected, Idle, unknown).

- **Tmux servers daemonize with PPID=1 and keep the original argv.**
  An obvious, devastating false positive: `tmux -CC new-session ...`
  in `ps` with PPID=1 looks identical to a leaked `-CC` client, but
  it's actually the legitimate server (tmux fork+detaches the server
  child internally). Killing it nukes every session on the machine.
  We discovered this only after several launches' worth of "session
  creation timestamps mysteriously advancing on every relaunch" — the
  reaper was silently destroying tmux state on every connect. The
  right discriminator is to ask tmux itself (`display-message`,
  `list-clients`); never guess from process attributes alone.

- **`splitn(N, char::is_whitespace)` does not collapse runs of
  whitespace.** First version of the orphan ps-output parser used
  `splitn(3, char::is_whitespace)`, which split on the *first* space
  and produced an empty PPID field for ps's right-aligned columns.
  Looked like the reaper "didn't fire"; it fired but matched zero
  rows. `split_whitespace().collect()` is the right tool for
  alignment-padded columnar output.

- **`kill -0 PPID` succeeds for zombies.** A child whose parent just
  got SIGTERM'd is briefly `PPID=K` where K is in zombie state until
  the OS finishes reaping it. `kill -0 K` returns success during that
  window, so a "is the parent alive?" probe based on it misses the
  race. We initially patched around this by checking `ps -o stat= -p
  PID` for the `Z` state, then made it irrelevant by switching to
  "ask tmux directly" instead of inferring orphan-ness from process
  ancestry.

- **React 19 StrictMode + module-level boot flags.** `useRef(false)`
  is per-component-instance; StrictMode mounts a fresh instance on
  the re-mount, so the ref starts false again. Boot effects that
  should fire exactly once per *process* need module-level state,
  not component-level state.

- **`void`-ing a Promise hides its rejection.** `void
  commands.tmuxSendKeys(...)` was the source of the original
  hours-long debugging loop — a "host not connected" rejection
  silently disappeared, and the user just saw typing not work.
  Replacement is `.then(res => { if (res.status !== 'ok')
  console.warn(...) })` for backend `Result<T, E>`, or a real toast
  for user-actionable errors. Fire-and-forget is fine; fire-and-
  discard is a bug.

### Outstanding (carry into phase 5)

- **Cleanup-on-exit.** The orphan reaper is defense-in-depth; the
  structural fix is making sure `RunEvent::ExitRequested` (or a
  signal handler equivalent) actually closes the PTY master fd on
  every shutdown path — including SIGTERM/SIGKILL. Once that lands,
  orphans stop accumulating in the first place during dev cycles,
  and the reaper becomes pure belt-and-braces.

---

## Phase 3 — Command palette + keyboard · later

Make every action reachable in two keystrokes.

### Outstanding work
- [ ] Global keyboard handler reading `KEYBINDINGS` from `lib/keymap.ts`
- [ ] `Palette` feature: fuzzy filter (probably `fzf-for-js` or hand-rolled),
      sub-modes (`@`, `#`, `$`, `/`), recent-actions stack
- [ ] Quick switcher (`Cmd+P`) — alias of palette pre-scoped to
      workspaces + windows
- [ ] Action registry — every Tauri command + every UI action declared once,
      surfaced in palette + keymap
- [ ] User-editable shortcuts (settings → Keyboard tab)

---

## Phase 4 — Inbox notifications + shell integration · **done**

Surface "things that want your attention" across every connected host
in a single sidebar inbox section. Two signal layers feed it: BEL
(0x07) bytes detected directly in pane output, and OSC 133 prompt
markers emitted by an auto-installed shell integration. The same OSC
133 capture is the data foundation for Warp-style blocks later (4F).

We deliberately skipped tmux's `monitor-activity` / `monitor-silence`
"layer 2" — heuristic, lossy, and superseded by OSC 133's exact
exit-code semantics.

### Done

**Domain (`helm-domain`):**
- New `OutputMarker` enum — `Bell`, `PromptStart`, `CommandStart`,
  `OutputStart`, `CommandDone { exit_code }`. Carried alongside the
  cleaned bytes in every `TmuxNotification::Output`.
- New `Notification` struct + `NotificationKind` (`Bell` /
  `CommandDone { exit_code, command, duration_ms }`), `NotificationId`
  newtype, `HostEvent::Notification` and `::NotificationDismissed`
  variants.

**Parser (`helm-tmux::parse`):**
- `extract_markers_and_strip(bytes) → (cleaned, markers)` — single-pass
  scan over decoded `%output` data. Recognises envelope-style escapes
  (`ESC ] / P / X / ^ / _`) so the BEL terminating an OSC 0 / OSC 1337
  sequence isn't misclassified as a standalone bell. Stripped from
  forwarded bytes so xterm doesn't beep or render the OSC params.
- 7 new tests cover bell, prompt/command/output markers, exit codes,
  ST vs BEL terminators, and OSC passthrough for non-133 sequences.

**Coalesce + post-processing (`helm-app::notifications`):**
- One inbox slot per `(host_id, pane_id)`. Repeats bump `count` and
  `updated_at`; new events of higher priority replace the kind.
- Priority order: `CommandDone(failed)` > `Bell` > `CommandDone(ok)`.
  This is the rule that fixed `printf '\a'` showing as a blue exit-0
  dot — the explicit bell now wins over the trailing successful exit.
- Per-pane runtime tracks command timing (B → D), an output preview
  ring (~512 bytes), and the resolved window/session id for breadcrumbs.
- `refresh_pane_index` runs on connect, reconnect, and after
  `%window-add` / `%window-close` / `%layout-changed` /
  `%sessions-changed`. Resolves pane → window/session ids and dismisses
  notifications for panes that have disappeared.
- `dismiss_for_host` on voluntary disconnect / delete keeps the inbox
  honest. Reconnect-style EOF deliberately preserves notifications so
  the user doesn't lose pending events when the transport bounces.

**Active-window suppression:**
- Frontend pushes `(activeHostId, activeWindow.id)` to backend on every
  active-window change via `set_focus`, and clears focus on
  `visibilitychange` / `blur` (helm minimised, on a different macOS
  desktop, etc.). Notifications post-processor skips creating a new
  inbox row when the pane's window matches focus — the user is staring
  at the output, an inbox entry would be noise. Backgrounded windows
  still notify normally.

**Shell integration (`helm-app::integration`):**
- Three scripts embedded via `include_str!`: `zsh.zshrc`, `bash.sh`,
  `fish.fish`. Each emits OSC 133 A/B/C/D markers via the shell's
  native hooks (`precmd_functions`/`preexec_functions`,
  `PROMPT_COMMAND` + `DEBUG` trap, `fish_prompt`/`fish_preexec`).
- `install_local()` writes to `~/.helm/integration/{zsh/.zshrc, bash, fish}`
  on app boot. Idempotent overwrite — bytes shipped in the binary are
  always what's on disk.
- Remote hosts: `remote_install_command()` builds a base64-encoded
  heredoc that recreates the directory + scripts on the far end at the
  start of every connect command. Same idempotent overwrite. base64 is
  in coreutils on every platform we target.
- **zsh auto-injection via ZDOTDIR.** Helm's process env exports
  `ZDOTDIR=~/.helm/integration/zsh`, `HELM_USER_ZDOTDIR=$ZDOTDIR-or-$HOME`,
  and `HELM_INTEGRATION=1` at app boot — so the *first* tmux server we
  spawn inherits them. `configure_tmux_env` then sets the same vars in
  tmux server-globally + per-session so new windows in pre-existing
  sessions also pick them up. Our wrapper `.zshrc` restores
  `ZDOTDIR=$HELM_USER_ZDOTDIR` before sourcing the user's real
  `.zshrc`, then registers the OSC 133 hooks.
- bash + fish have no equivalent of ZDOTDIR — they require the user to
  add a one-line source to their rc file. For now bell detection works
  out of the box; the setup-needed toast (4E) lands separately.

**Frontend store (`src/lib/store.ts`):**
- `notifications: Map<NotificationId, Notification>` slice with
  `upsertNotification` / `removeNotification` /
  `dismissNotificationsForHost`.
- Selector helpers `notificationsForHost` / `notificationsForWorkspace`
  / `notificationsForWindow` with a fallback that resolves missing
  `window_id` via the local workspace tree (handles the brief race
  where pane index hasn't refreshed yet).
- `subscribeHostEvents` calls `notifications_list` after subscribe so a
  `Cmd+R` webview reload finds the inbox the way it left it.

**INBOX section (`src/features/activity-feed/InboxSection.tsx`):**
- Renders above HOSTS in the expanded sidebar; hidden when empty.
  Cross-host by design — one global inbox.
- Each row: urgency dot (yellow=bell, blue=ok, red=failed) ·
  `workspace · window` · time-ago + ×count · hover-reveal × button.
  Secondary line shows `host · kind-label · preview-text`.
- Click a row → switches active host/workspace/window and fires
  `tmuxSelectWindow`. Notification stays — peek doesn't dismiss.
- Header has hover-reveal `clear` button that dismisses every entry.

**Roll-up dots:**
- `SidebarHostRow` shows a coral count badge next to the status dot
  when host has notifications.
- `SidebarWorkspaceRow.activity` upgrades to `attention` (bell) or
  `failed` (any non-zero exit) based on rolled-up notifications inside.
- Same logic for `SidebarWindowRow.activity`.

**Dismiss-on-keystroke:**
- `TmuxPane.onData` now resolves the pane's `window_id` from the
  store, checks if any notifications match, and fires
  `notification_dismiss_for_window` before sending bytes. Pure
  modifier presses naturally don't fire `onData`, so they're excluded.

**Cmd+Shift+I shortcut:**
- Jumps to the oldest inbox entry, switching active host if needed.

**Tauri commands:** `notifications_list`, `notification_dismiss`,
`notification_dismiss_for_window`, `set_focus`. Bindings regenerated;
specta config updated to emit `u64` as `number` (timestamps fit
comfortably under `Number.MAX_SAFE_INTEGER`).

### Sharp edges (revisit later)

- **Coalesce key is per-pane, not per-window.** Fine for single-pane
  windows (which is everything we render today). When split-pane
  rendering lands, two panes in the same window can produce two
  separate inbox rows. Either upgrade the coalesce key to
  `(host_id, window_id)` or render them as one row with two
  origin-pane affordances.
- **Command text is empty in `CommandDone`.** Integration script
  doesn't pass `cmdline`. Fine for the inbox preview (the output snippet
  in `preview` is more informative anyway), but the future blocks UI
  will want it. Plumb through `OSC 133;C;cmdline=…` extension when 4F
  starts.
- **bash + fish need manual rc edit.** Setup-needed toast is the 4E
  story — detect missing OSC 133 within ~3s of first prompt for non-zsh
  shells and surface a copy-pastable one-liner.
- **Existing tmux server.** When the user already had a tmux server
  running with sessions before this build first connected, those
  sessions' panes were started with the OLD env (no ZDOTDIR). They
  need to be restarted (Cmd+T new window) to get integration. New
  windows in *new* workspaces always get it.

### Lessons learned

- **OSC envelopes use BEL as a terminator.** First version of the
  marker scanner stripped every `\x07` byte and counted them as bells.
  This misclassified the BEL ending an OSC 0 (set window title) as a
  standalone application bell — every `oh-my-zsh` prompt would create
  a phantom inbox entry. Fix: recognise `ESC ] / P / X / ^ / _`
  envelopes as units, scan to their ST (BEL or `ESC \`), pass through
  intact unless it's our OSC 133. Then standalone BELs are by
  elimination "not part of any envelope" → real bells.

- **The user expects `printf '\a'` to ring the bell.** First coalesce
  rule had CommandDone always beating Bell on the theory that "more
  information is better." But `printf '\a'` produces both a Bell and a
  CommandDone(0) within a few hundred ms; the user typed it
  specifically to test the bell and only saw a blue dot. Revised
  priority: Failure > Bell > Success. Bell is an explicit attention
  request; success is just informational.

- **specta forbids u64 in TS bindings by default.** `Number` in JS is
  f64, so naively serializing an arbitrary u64 risks precision loss.
  Hit this immediately when adding `created_at: u64` to `Notification`.
  Fix: configure the exporter with
  `BigIntExportBehavior::Number` — timestamps in ms sit comfortably
  under `Number.MAX_SAFE_INTEGER` (2^53), no BigInt overhead at the
  call site.

- **tmux server captures env at startup.** `set-environment -g` after
  the server is already running only affects sessions/windows created
  *afterward*. The bootstrap session's first pane (created as part of
  `tmux new-session -A`) misses the update. Fix: also export the
  integration vars in helm's process env at boot, *before* the very
  first connect — so the server inherits them at startup. Then
  `configure_tmux_env`'s server-global + per-session updates handle
  changes for later connects.

- **ZDOTDIR-only auto-injection.** zsh has ZDOTDIR; bash and fish
  don't. The temptation is to write our line into `~/.bashrc` /
  `~/.config/fish/config.fish` automatically — same approach iTerm2's
  installer takes — but that's hostile (chezmoi/yadm/managed dotfiles
  break, line ends up in version control, surprising). Better: write
  the file, surface a one-time setup toast, let the user paste the
  one-liner if they want command tracking. Bell still works without it.

- **Active-window suppression belongs in the backend.** First instinct
  was to filter in the frontend (let the notification arrive, store
  upsert it, then immediately dismiss if it's the active window). That
  works but produces a flash and leaves dismissed entries in the
  backend registry until cleared. Pushing focus state down to the
  backend (one Tauri command per active-window change) means the
  notification is never created — cleaner state, no flash, no
  registry leak across `Cmd+R` reloads.

- **`watch::Receiver`-style reactivity vs. command pushes.** Looked at
  using a Tauri channel for the focus state, but the rate is right
  (a few pushes per second at most under heavy switching) and the
  command form is simpler — no need for the frontend to manage a
  long-lived channel just for focus updates.

### Outstanding (in scope, not yet shipped)

- [ ] **4E: bash/fish setup-needed toast.** Detect missing OSC 133 by
      sampling `markers` from the first ~3s of pane output after a new
      pane is created on a non-zsh shell. If none arrive, surface a
      sticky toast with the copy-pastable source line. Use `$SHELL` env
      reported on connect to gate the detection.
- [ ] Drop the unused git-branch placeholder code from `App.tsx` /
      `store.ts` / `host.ts` (commented-out segment + `branch: string`
      field). Status-bar work is parked indefinitely, dead code is
      noise.

### Out of scope for phase 4 (deferred to dedicated future phases)

These were originally folded into "phase 4" in the early progress doc
but are now their own phases:

- **Settings window** — separate Tauri window, General / Keyboard /
  Shells / Tmux / Hosts / Advanced tabs. Defer until there's a real
  setting that needs surfacing (font, theme, integration toggle).
- **Theming** — three shipped themes + JSON theme directory. Same
  reasoning as settings.
- **Status bar polish** — click handlers, tmux uptime, git branch
  segment (latter blocked on `host_run_shell` IPC — see the parked
  notes elsewhere).

---

## Phase 4G — Multi-control-client tmux refactor · **done**

The phase-4 inbox worked beautifully on a single workspace and silently
broke across workspaces. Tmux's `-CC` mode only delivers `%output`
notifications for the session a control client is attached to —
cross-workspace bells, command-done events, and OSC 133 markers were
buffered until the user switched sessions, making the inbox concept
incomplete for the multi-workspace use case.

Fix: maintain one control client *per tmux session* on each host, all
permanently attached. Output flows for every session in real time.

### Done

**Multi-channel SSH (`helm-ssh`):**
- Refactored the I/O thread from one-channel-per-session to a
  long-running event loop accepting channel-open requests over an mpsc.
- New API: `connect_session(target, auth, timeout, prompter) → SshSession`
  (TCP + auth, no exec) and `SshSession::open_exec(command) → OpenedChannel`
  (one round-trip channel open + PTY + exec, returns blocking pipe halves).
- Legacy `connect()` retained as a thin wrapper for back-compat.
- One TCP+auth handshake per host; N exec channels multiplexed over it.
  Avoids OpenSSH's `MaxStartups` limit and saves N-1 handshakes.

**Per-session local clients (`helm-tmux`):**
- New `bootstrap_local(default_workspace) → Vec<String>`: one-shot
  enumerate-or-create that returns every session id on the local server.
  Uses sync subprocess invocations (no control client). Sibling sync
  helpers `probe_tmux_sync` + `list_local_sessions`.
- New `spawn_attach_local(session_id) → (TmuxClient, events)`: opens a
  fresh PTY pair, runs `tmux -CC attach -t $session_id`, wraps in a
  TmuxClient. One PTY per session.
- Legacy `spawn_local()` becomes a thin wrapper over both for tests.

**Multi-client `HostEntry` (`helm-app::state`):**
- `tmux: Option<Arc<TmuxClient>>` → `clients: HashMap<String /* session_id */, Arc<SessionClient>>`.
- `primary_session_id: Option<String>` — first session at connect time;
  used as the routing target for global commands (every existing tmux
  command works through any client since pane/window/session ids are
  server-globally unique).
- `SessionClient { tmux, forwarder: AbortHandle }` — one per session.
- `supervisor_tx` — the mpsc sender into the live supervisor's signal
  channel, cloned by per-client forwarders + `spawn_missing_clients`.
- `shutdown_clients()` helper — aborts every forwarder, drops every
  client, clears ssh + supervisor_tx in one call.

**Connection state machine (`helm-app::connection`):**
- `do_connect` rebuilds: bootstrap step → spawn N clients → register all
  in HostEntry → spawn host supervisor.
- `client_forwarder_loop` per client: drains its TmuxNotification
  receiver, forwards each event to the global channel, post-processes
  `%output` markers. On EOF / `%exit` signals supervisor via
  `SupervisorSignal::ClientDied(session_id)`.
- `supervise` consolidated: `select!` on supervisor signals + reachability
  watch. ClientDied promotes a new primary or triggers full reconnect
  when no clients remain. SessionsChanged invokes `spawn_missing_clients`
  to incrementally attach to newly-created workspaces.
- `spawn_session_client(...)` extracted helper used by all three spawn
  sites (do_connect, supervise reconnect, spawn_missing_clients).
- Reconnect ladder unchanged — `[1, 2, 4, 8, 30]s` clamped to 30s, with
  reachability-watch early-wake on `false → true` transitions.

**Remote connect command:**
- First exec channel installs integration + bootstraps a session if
  none exist + `tmux set-environment -g HELM_INTEGRATION/ZDOTDIR/...`
  server-globally + per-session, then attaches the first control client.
- Per-session attach commands run with env exports + `exec tmux -CC
  attach -t $sid`, no install heredoc (already done by the first).
- Per-session channels open *concurrently* via `futures_join_attach_channels`
  — each pays the cost of one remote shell startup (~1-3s with heavy
  `.zshrc`); parallelizing drops total wall time to ~one shell-init
  period regardless of N.

**Frontend (minor):**
- `selectWorkspace` no longer calls `tmux_switch_client` (now a no-op
  on the backend). With one permanent control client per session, every
  session is always attached at the right viewport — there's nothing to
  switch.

### Sharp edges (revisit later)

- **Single-channel SSH error → full reconnect.** A transient channel
  drop on one session today brings every session back through the
  reconnect ladder. Could be smarter (respawn just the dead client) but
  the current behavior is simple and correct; deferred.
- **Pre-existing remote panes don't get integration.** Documented in
  phase 4. Same constraint here — the user opens a new window to pick
  it up.
- **Slow `.zshrc` on remote.** Even parallelized, the wall time is
  bounded by the slowest single shell init. Heavy oh-my-zsh setups can
  push connect time to 3-5s. Future fix: a small `~/.helm/bin/helm-attach`
  shim on the remote that uses `/bin/sh` instead of the user's login
  shell; deferred.

### Lessons learned

- **`%output` is scoped to the attached session.** Spent two hours
  diagnosing "cross-workspace notifications don't fire" before realizing
  this is a fundamental tmux constraint. Empirically confirmed by
  buffered burst-delivery on session switch — events fire at the right
  *time* but tmux holds them until the control client comes back.
  Multi-client is the only real fix; iTerm2 went through the same
  architectural decision.

- **Tabs in primary_id are lost over SSH PTY.** `display-message -p
  '#{client_session}'` returns the session *name* (e.g. `"workspace 1"`),
  not the *id* (`$3`). Earlier code used the name and dedupe-failed
  against `list-sessions -F '#{session_id}'` results — opening a
  *second* control client on the session our first one was already
  attached to. Both then forwarded the same `%output` for every
  keystroke, producing the "eeechhoo hheelllloo" double-character
  artefact. Use `#{session_id}` exclusively for protocol-level
  identifiers.

- **Concurrent `refresh_pane_index` races wipe valid entries.** With
  one client per session, every forwarder sees `%window-added` /
  `%sessions-changed` and queues a refresh. N parallel `list-panes`
  calls race their stale-cleanup steps — refresh A reads `alive={X,Y}`
  just before pane Z is added; refresh B sees Z and writes it; A
  processes its alive set, finds Z in `pane_runtime` but not in *its*
  alive set, dismisses Z's notification + runtime entry. Manifests as
  inbox rows with no breadcrumb (window_id was empty). Fixed with a
  per-host serialization mutex on the refresh path + an empty-result
  guard (skip stale cleanup if `list-panes` returned nothing — almost
  always a transient state, not a real "all panes killed").

- **Slow remote shell init compounds with serial attach.** Each
  per-session attach channel pays the full `.zshrc` startup cost —
  zsh in PTY mode is interactive (sshd allocates a PTY because tmux
  needs one), so it sources rc files even though we're about to
  `exec tmux`. Heavy configs hit 1-3s per session; opening 5 sessions
  serially = 10-15s connect. Fixed by parallelizing per-session opens
  via `tokio::spawn` + collecting via `join_all`-equivalent.

- **`tmux set-environment` is per-server-and-per-session.** Tmux's
  session env is snapshotted at session creation; our `export
  HELM_INTEGRATION=1` in the connect shell only affects the bootstrap
  session's first pane (and only because we set it before tmux's first
  ever connect). Existing remote sessions need explicit
  `set-environment -g` (server-globally) AND `set-environment -t $sess`
  per existing session. Without the per-session line, new windows in
  pre-existing workspaces miss the env entirely. Documented as a sharp
  edge in phase 4.

---

## Phase 4H — Tool integration framework · **done**

Generalized "ask the tool to bell at the right moments" framework on
top of phase 4's bell detection. Bell stays the canonical attention
signal — universal, every tool can emit it, our pipeline already
handles it. Per-tool integration is then just *config that makes the
tool ring at semantically meaningful moments*. v1 ships Claude Code as
the first integration, but the framework is built so future
integrations (pgcli, mosh, anything) drop in as a single trait impl.

### Done

**`ToolIntegration` trait + registry (`helm-app::tool_integrations`):**
- Stable id + display name + description + post-install note + process
  names (matched against `pane_current_command`).
- `is_installed`, `install`, `uninstall` — all idempotent, async, take
  a primary `TmuxClient` for integrations that need to query/write
  remote state.
- `pane_matches(current_command) → bool` with a default impl that
  checks `process_names`. Override for tools that mutate their process
  title (e.g. Claude Code reports its semver as the title).
- `registry()` returns all integrations the binary ships.
- `find(id)` for command lookup.

**`ClaudeCodeIntegration`:**
- Reads `~/.claude/settings.json`, idempotently merges
  `Notification` + `Stop` hooks emitting `printf '\a' > /dev/tty
  2>/dev/null || true`. Uninstall removes only entries matching our
  exact command — preserves any other hooks the user authored.
- Atomic write (sibling tmp file + rename).
- `is_semver_like` heuristic for the process-title quirk: Claude Code
  reports `"2.1.126"` as `pane_current_command` instead of `"claude"`,
  so `pane_matches` accepts both the literal binary name AND any
  string matching `^[\d.]+$` containing at least one dot. False
  positives vanishingly rare; install is gated on a user click anyway.
- v1 supports local hosts only (`host.port == 0`); remote returns an
  error. Remote support is gated on adding a sync `SshSession::run_oneshot`
  helper to `helm-ssh` for one-shot file ops.

**Detection (`detect_and_suggest`):**
- Three triggers:
  1. **Connect.** `do_connect` runs the sweep after the first
     `refresh_pane_index`.
  2. **Window mutations.** Forwarder triggers a sweep on
     `%window-added` / `%window-closed` / `%layout-changed` /
     `%sessions-changed`.
  3. **After-Enter.** `tmux_send_keys` checks for `\r`/`\n` in the
     bytes; if present and any integration is still un-suggested,
     schedules a sweep 400ms later (gives tmux time to update
     `pane_current_command` after the new process starts). The third
     trigger catches the common "user types `claude<enter>` in an
     existing pane" case — tmux doesn't notify on foreground command
     changes, so without this trigger detection waits until the next
     window mutation.
- Per-host dedup via `tool_integration_seen: DashMap<(HostId, String), ()>`.
  Once an integration is suggested, installed, or dismissed for a host,
  we don't sweep for it again this session.
- Cheap fast-path: when all integrations are already seen for a host,
  the trigger does a `HashMap.contains_key` check and skips the IPC
  entirely.

**Tauri commands + suggestion event:**
- `tool_integrations_list(host_id)`, `tool_integration_install(host_id, id)`,
  `tool_integration_uninstall(host_id, id)`, `tool_integration_dismiss(host_id, id)`.
- `HostEvent::ToolIntegrationSuggested` carries id + name + description
  + post-install note. Pushed by the backend, consumed by the frontend.

**Frontend (`src/features/activity-feed/IntegrationSuggestionHost.tsx`):**
- Sticky suggestion card bottom-right, separate from the regular Toast
  stack. Two buttons: `Install` / `Not now`. After install: card flips
  to "Restart Claude Code (close and reopen the session)" then
  auto-dismisses after 4s.
- `Install` calls `tool_integration_install`. `Not now` calls
  `tool_integration_dismiss`. Both record the decision so we don't
  re-prompt this session.

### Sharp edges

- **Process-title detection is heuristic.** Tools that mutate their
  process name in unexpected ways won't be detected without an explicit
  `pane_matches` override. Claude is the first instance; future
  integrations may need similar special-casing.
- **Remote integrations are local-only in v1.** Remote support requires
  a sync `SshSession::run_oneshot` helper to read/write the remote
  `~/.claude/settings.json`. Tracked for a follow-up.

### Lessons learned

- **Process titles are fair game.** Node CLIs commonly set
  `process.title = version` as a debugging convenience. tmux's
  `pane_current_command` returns whatever's in argv[0], including
  these mutations. The default `process_names`-only matcher misses
  these; teach the trait to allow per-integration custom matching
  rather than relying on a static list.

- **Frontend filter for `onData` is needed for click-doesn't-dismiss.**
  When the user clicks an inbox row to jump to a Claude pane, helm
  switches the visible pane and calls `term.focus()`. xterm with
  focus tracking enabled (Claude does, via DECSET 1004) fires a
  focus-in sequence (`\x1b[I`) back through `term.onData`. Without
  filtering, this counts as a "user keystroke" and dismisses the
  notification instantly — breaking peek-doesn't-dismiss. Added
  `isUserKeystroke(data)` in `TmuxPane.tsx` that excludes focus
  events, mouse events (DECSET 1006/1000), cursor-position responses,
  bracketed-paste markers. Real keystrokes still dismiss; xterm's
  background reports don't.

---

## Phase 4F — Warp-style blocks UI · later

Layer command-block decorations on top of the OSC 133 markers we're
already capturing in phase 4. Each `B → D` span is a block; we can
render block boundaries / collapse / re-run / search without changing
how xterm itself renders.

### Outstanding work

- [ ] Per-pane span tracker — record buffer offsets at each
      OSC 133 marker against xterm's serialized buffer
- [ ] Marginal block boundary decorations in xterm (xterm.js
      `registerDecoration`) with hover affordances
- [ ] Collapsible blocks (folded view shows command + status only)
- [ ] Per-block actions: copy command, copy output, re-run, share
- [ ] Filter / search across blocks in the active pane
- [ ] Plumb `cmdline=…` through the integration scripts so `command`
      on `CommandDone` is non-empty (currently empty until 4F)

---

## Phase 5 — Performance + IPC · later

When measurable matters.

### Outstanding work
- [ ] Replace JSON-array byte transport with custom URI scheme protocol
      (Tauri `register_uri_scheme_protocol`) or msgpack channel
- [ ] 5–8ms / 200KB output aggregator in `helm-tmux`
- [ ] Profile under `tail -f` + `cat` of large logs; budget ≤ 16ms per frame
- [ ] **Reintroduce GPU rendering with a context-budget manager.** We
      dropped `@xterm/addon-webgl` in 2B because rapid pane churn blew
      past the browser's WebGL context cap. Reintroducing it requires
      VS Code-style management: cap N active GL contexts, spill the
      least-recently-used to canvas, restore on focus. The native
      terminals that *do* ship GPU rendering (iTerm2/Metal, Alacritty/
      Kitty/OpenGL, WezTerm/WebGPU, Ghostty/Metal, Warp/Metal) bypass
      this constraint by using platform-native APIs — not an option in
      a webview, so we'd want the budgeter. Worth doing only if
      profiling shows canvas drops frames under realistic load
      (vim repaint, big-log tailing, claude streaming).
- [ ] Cold-start budget < 250ms to first prompt on `localhost`
- [ ] Memory: aim for ≤ 200MB RSS with five attached panes idle

---

## Out-of-scope (per §8)

Listed once so we don't accidentally start them.

- Block-based command grouping (Warp-style) — possibly later, opt-in
- AI features (Claude Code already covers this)
- Workflow scripts / saved commands
- Synchronised inputs across panes
- SFTP / file browser
- Plugins / theme marketplace
- Mobile
- Sharing / collaboration
- Cloud sync of settings (maybe iCloud Drive JSON, defer)

---

## Workflow notes

```sh
# develop
bun run tauri:dev          # opens native window, vite HMR + cargo rebuild

# regenerate TS bindings without launching the GUI
cargo run --bin export-bindings

# typecheck only (fast feedback loop)
bun run typecheck

# production build (frontend only — bundle.active is false until icons land)
bun run build
```
