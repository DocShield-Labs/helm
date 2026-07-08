//! SSH transport via russh.
//!
//! ## Why a dedicated I/O thread
//!
//! `russh::Channel<Msg>` carries a `tokio::mpsc::UnboundedReceiver`, which
//! is `Send` but `!Sync`. That makes `&Channel: !Send`, and russh's API
//! takes `&self` on every operation (`request_pty`, `exec`, `data`, …),
//! so the *future* of any sequence of those calls is `!Send`. Tauri's
//! `#[tauri::command]` and `tokio::spawn` both require `Send` futures.
//!
//! Trying to wallpaper that with `Box::pin`/`spawn_blocking` is fool's
//! errand: russh's internal poll machinery expects to keep running on the
//! same runtime that produced its tasks, so we can't tear down the runtime
//! either.
//!
//! Solution: own a current-thread tokio runtime on a dedicated OS thread,
//! and expose nothing-but-bytes to the rest of the app via plain
//! `std::io::pipe()` halves. From helm-app's perspective the SSH channel
//! looks identical to the local-PTY case — `Box<dyn Read>` /
//! `Box<dyn Write>` that block, no async, no `Send` headaches.
//!
//! Single jump host is supported via `russh::client::connect_stream` over a
//! direct-tcpip channel from the bastion.
//!
//! Host-key verification consults `~/.ssh/known_hosts` via russh-keys.
//! Unknown hosts (or changed keys) are surfaced through the
//! [`HostKeyPrompter`] trait so the caller can render a UI and return the
//! user's decision. With no prompter passed, unknown / changed keys are
//! refused — no silent TOFU.

use async_trait::async_trait;
use helm_domain::{HostKeyDecision, HostKeyPromptKind};
use russh::client::{self, Handle, Msg};
use russh::{ChannelStream, Pty};
use russh_keys::agent::client::AgentClient;
use russh_keys::key;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::path::PathBuf;
use std::sync::{mpsc as sync_mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Asynchronous decision-maker for an unknown or changed server key.
/// helm-app implements this; helm-ssh stays unaware of UI plumbing.
#[async_trait]
pub trait HostKeyPrompter: Send + Sync {
    /// Called once per unverified key during the SSH handshake. Return
    /// the user's decision so the handshake can either continue or be
    /// aborted.
    async fn prompt(
        &self,
        hostname: &str,
        port: u16,
        algorithm: &str,
        fingerprint: &str,
        kind: HostKeyPromptKind,
    ) -> HostKeyDecision;
}

/// Raw-mode terminal settings sent with `request_pty`. Mirrors `cfmakeraw(3)`:
/// disable canonical mode, echo, signal generation, CR/NL translation, flow
/// control, and output post-processing.
///
/// Notes on portability:
/// - `IUCLC` (lowercase-to-uppercase translation) is Linux-only. Including
///   it can cause some sshd implementations to silently reject the entire
///   modes list, so we leave it out.
/// - Whether sshd honors any of these on a non-interactive `exec` channel
///   is server-dependent. We don't rely on tabs round-tripping cleanly as
///   a result — see the `|` delimiter in `lib/host.ts::refetchTree`.
const RAW_TERMINAL_MODES: &[(Pty, u32)] = &[
    // input flags
    (Pty::IGNPAR, 1),
    (Pty::INPCK, 0),
    (Pty::ISTRIP, 0),
    (Pty::INLCR, 0),
    (Pty::IGNCR, 0),
    (Pty::ICRNL, 0),
    (Pty::IXON, 0),
    (Pty::IXANY, 0),
    (Pty::IXOFF, 0),
    // local flags
    (Pty::ISIG, 0),
    (Pty::ICANON, 0),
    (Pty::ECHO, 0),
    (Pty::ECHOE, 0),
    (Pty::ECHOK, 0),
    (Pty::ECHONL, 0),
    (Pty::IEXTEN, 0),
    // output flags
    (Pty::OPOST, 0),
    (Pty::ONLCR, 0),
    (Pty::OCRNL, 0),
    (Pty::ONOCR, 0),
    (Pty::ONLRET, 0),
];

#[derive(Debug, Error)]
pub enum SshError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("auth failed: {0}")]
    AuthFailed(String),
    #[error("channel: {0}")]
    Channel(String),
    #[error("agent: {0}")]
    Agent(String),
    #[error("key: {0}")]
    Key(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("russh: {0}")]
    Russh(#[from] russh::Error),
    #[error("ssh thread crashed: {0}")]
    Thread(String),
}

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub jump: Option<Box<SshTarget>>,
}

/// Auth method + transient secret material.
///
/// Owns its secrets rather than borrowing — borrows trip HRTB Send checks
/// in async fn that cross await points. Stage C will look at zeroizing the
/// password on drop; for now, the scope is "keep it tight".
#[derive(Clone)]
pub enum SshAuth {
    Agent,
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    Password {
        secret: String,
    },
}

/// Live SSH session — the I/O thread runs a current-thread tokio runtime
/// that owns the russh `Handle`. Drop or call `.disconnect()` to
/// terminate; either signals the thread to clean up and join.
///
/// Multi-channel: a single `SshSession` can host any number of exec
/// channels, opened one at a time via `open_exec`. The I/O thread is a
/// long-running event loop that handles channel-open requests + the
/// teardown signal, so each tmux control client (one per workspace)
/// shares the same TCP/auth handshake instead of re-doing it N times.
pub struct SshSession {
    /// Send a session request to the I/O thread. Variants cover the
    /// long-lived PTY-bound exec channel (used for tmux control
    /// clients) and the short-lived no-PTY exec used for one-shot
    /// remote commands (read/write a config file, query env, etc.).
    request_tx: tokio::sync::mpsc::UnboundedSender<SessionRequest>,
    /// Set once per session. Sending fires the cleanup path on the I/O
    /// thread (russh disconnect, drop all channels, exit the runtime).
    teardown: parking_lot::Mutex<Option<oneshot::Sender<()>>>,
    /// Joined on Drop so the OS thread exits before we go.
    join: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

/// Request from the caller's thread to the I/O thread. The I/O thread
/// dispatches based on variant — `Exec` opens a long-lived PTY channel
/// for tmux to drive, `OneShot` runs a non-PTY command and captures
/// its full stdout/stderr/exit_code. Shape lets the I/O thread report
/// errors back without the caller blocking on a tokio runtime.
enum SessionRequest {
    Exec {
        command: String,
        /// Sync channel — caller is on a regular OS thread (helm-app's
        /// connect path runs inside `spawn_blocking`), so a tokio oneshot
        /// would force them onto an async context.
        response: sync_mpsc::SyncSender<Result<OpenedChannel, SshError>>,
    },
    OneShot {
        command: String,
        response: sync_mpsc::SyncSender<Result<OneShotResult, SshError>>,
    },
}

/// Result of `SshSession::open_exec` — sync byte streams plus a guard
/// that, when dropped, signals the I/O thread to close the underlying
/// channel. The pipes themselves close on drop too, so the pumps inside
/// the I/O thread will see EOF naturally and tear down their channel.
pub struct OpenedChannel {
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

/// Result of `SshSession::run_oneshot` — full captured stdout/stderr
/// and the exit code if the remote sent one (most do; missing only on
/// transport drop or signal-based termination).
#[derive(Debug, Clone)]
pub struct OneShotResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<u32>,
}

impl SshSession {
    /// Open a new long-lived exec channel on this session. Reuses the
    /// existing TCP + auth + russh `Handle`; only the per-channel
    /// `request_pty` + `exec` runs over the wire. Cheap (a couple of
    /// round-trips) so it's fine to call once per workspace at connect
    /// time.
    ///
    /// Sync-blocking with a 15s timeout. If the I/O thread has died
    /// (transport drop, cleanup in flight), returns
    /// `SshError::Thread`. Subject to the server's `MaxSessions` limit
    /// (OpenSSH default 10) — surfaces as a channel-open error.
    pub fn open_exec(&self, command: String) -> Result<OpenedChannel, SshError> {
        let (tx, rx) = sync_mpsc::sync_channel(1);
        self.request_tx
            .send(SessionRequest::Exec {
                command,
                response: tx,
            })
            .map_err(|_| SshError::Thread("session I/O thread is gone".into()))?;
        rx.recv_timeout(Duration::from_secs(15))
            .map_err(|_| SshError::Channel("open_exec timed out".into()))?
    }

    /// Run `command` on the remote in a no-PTY exec channel and
    /// capture its full stdout/stderr + exit code. Use for short
    /// one-shot operations: cat a file, write a heredoc, query env.
    /// Long-running streaming work belongs in [`open_exec`].
    ///
    /// Sync-blocking with a 30s timeout (longer than open_exec since
    /// the remote command may take time to produce all its output).
    pub fn run_oneshot(&self, command: String) -> Result<OneShotResult, SshError> {
        let (tx, rx) = sync_mpsc::sync_channel(1);
        self.request_tx
            .send(SessionRequest::OneShot {
                command,
                response: tx,
            })
            .map_err(|_| SshError::Thread("session I/O thread is gone".into()))?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| SshError::Channel("run_oneshot timed out".into()))?
    }

    /// Best-effort clean teardown. Idempotent.
    pub fn disconnect(&self) {
        if let Some(tx) = self.teardown.lock().take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join.lock().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Sync byte streams to feed helm-tmux. The reader and writer are backed
/// by `std::io::pipe()` halves; the I/O thread on the other end pumps
/// bytes between these and the russh channel.
///
/// Returned by the legacy `connect()` shorthand — modern callers should
/// use `connect_session` + `SshSession::open_exec` to multiplex many
/// channels over one session.
pub struct SshConnection {
    pub session: SshSession,
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

/// Open an SSH session: TCP + auth + russh `Handle` setup. No exec yet.
/// Use `SshSession::open_exec(command)` afterwards to start one or more
/// concurrent channels on the session.
///
/// `connect_timeout` covers the entire negotiation up through auth.
/// 15s is a reasonable default.
///
/// `prompter` is invoked when the server key isn't in `~/.ssh/known_hosts`
/// (or differs from the recorded entry). Pass `None` to refuse unknown
/// hosts outright — useful for tests.
pub fn connect_session(
    target: SshTarget,
    auth: SshAuth,
    connect_timeout: Duration,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<SshSession, SshError> {
    let (ready_tx, ready_rx) = sync_mpsc::sync_channel::<Result<(), SshError>>(1);
    let (teardown_tx, teardown_rx) = oneshot::channel::<()>();
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel::<SessionRequest>();

    let join = thread::Builder::new()
        .name("helm-ssh".into())
        .spawn(move || {
            session_io_thread(target, auth, ready_tx, request_rx, teardown_rx, prompter);
        })
        .map_err(|e| SshError::Thread(format!("spawn: {e}")))?;

    // Wait for the I/O thread to finish auth (or fail).
    let result = ready_rx
        .recv_timeout(connect_timeout)
        .map_err(|_| SshError::Connect(format!("negotiation timed out after {connect_timeout:?}")));

    match result {
        Ok(Ok(())) => Ok(SshSession {
            request_tx,
            teardown: parking_lot::Mutex::new(Some(teardown_tx)),
            join: parking_lot::Mutex::new(Some(join)),
        }),
        Ok(Err(e)) => {
            // Negotiation failed; thread is shutting itself down.
            let _ = join.join();
            Err(e)
        }
        Err(e) => {
            // Timeout. Send teardown and best-effort join.
            let _ = teardown_tx.send(());
            let _ = join.join();
            Err(e)
        }
    }
}

/// Backwards-compatible single-channel shorthand. Equivalent to:
/// `let s = connect_session(...); let c = s.open_exec(command)?; Ok(SshConnection { ... })`.
///
/// New callers should use `connect_session` + `open_exec` directly so
/// multiple tmux control clients can share one TCP/auth handshake.
pub fn connect(
    target: SshTarget,
    auth: SshAuth,
    command: String,
    connect_timeout: Duration,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<SshConnection, SshError> {
    let session = connect_session(target, auth, connect_timeout, prompter)?;
    let opened = session.open_exec(command)?;
    Ok(SshConnection {
        session,
        reader: opened.reader,
        writer: opened.writer,
    })
}

/// Body of the dedicated SSH I/O thread.
///
/// Builds a current-thread tokio runtime, runs the russh auth handshake
/// inside it, and on success enters a long-running event loop that
/// services per-channel open requests until the teardown signal fires.
fn session_io_thread(
    target: SshTarget,
    auth: SshAuth,
    ready: sync_mpsc::SyncSender<Result<(), SshError>>,
    mut request_rx: tokio::sync::mpsc::UnboundedReceiver<SessionRequest>,
    teardown: oneshot::Receiver<()>,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(SshError::Thread(format!("build runtime: {e}"))));
            return;
        }
    };

    rt.block_on(async move {
        let handle = match auth_handshake(target, auth, prompter).await {
            Ok(h) => {
                let _ = ready.send(Ok(()));
                h
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };

        // Event loop: handle channel-open requests + teardown until
        // either the request channel closes (SshSession dropped) or
        // teardown fires.
        let mut teardown = teardown;
        loop {
            tokio::select! {
                _ = &mut teardown => {
                    debug!("ssh: teardown signal received");
                    break;
                }
                req = request_rx.recv() => {
                    let Some(req) = req else {
                        debug!("ssh: request channel closed");
                        break;
                    };
                    match req {
                        SessionRequest::Exec { command, response } => {
                            match open_exec_channel(&handle, command).await {
                                Ok((reader, writer)) => {
                                    let _ = response.send(Ok(OpenedChannel {
                                        reader,
                                        writer,
                                    }));
                                }
                                Err(e) => {
                                    let _ = response.send(Err(e));
                                }
                            }
                        }
                        SessionRequest::OneShot { command, response } => {
                            let result = run_oneshot_channel(&handle, command).await;
                            let _ = response.send(result);
                        }
                    }
                }
            }
        }

        // Best-effort clean disconnect. All channels close as the
        // runtime drops their pump tasks.
        if !handle.is_closed() {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "helm exit", "en")
                .await;
        }
    });
}

/// Open a russh exec channel on `handle`, request a PTY, exec the
/// command, set up the duplex pumps, and return the caller-side pipe
/// halves. The pump tasks are spawned on the current runtime and live
/// until either side of the duplex closes (channel EOF or app pipe drop).
async fn open_exec_channel(
    handle: &Handle<Client>,
    command: String,
) -> Result<(PipeReader, PipeWriter), SshError> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| SshError::Channel(format!("open session: {e}")))?;

    // PTY is required because tmux calls `tcgetattr` on startup. 80×24 is a
    // placeholder; the frontend resizes immediately on TmuxPane mount.
    //
    // Terminal modes disable the line discipline so the remote PTY is
    // byte-clean for tmux's stdin: no canonical mode, no echo, no signal
    // generation, no \r↔\n translation, no flow control, no output post-
    // processing. Without this, sshd's default cooked-mode PTY rewrites
    // `\t` (and other control bytes) on its way into the tmux process,
    // corrupting our format-string round-trips.
    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, RAW_TERMINAL_MODES)
        .await
        .map_err(|e| SshError::Channel(format!("request_pty: {e}")))?;

    channel
        .exec(false, command.into_bytes())
        .await
        .map_err(|e| SshError::Channel(format!("exec: {e}")))?;

    let stream = channel.into_stream();

    // Two unidirectional pipes form the duplex bridge.
    //   app→ssh:  caller writes to `app_to_ssh_writer`; pump reads from
    //             `app_to_ssh_reader` and forwards to the SSH channel.
    //   ssh→app:  pump reads from the SSH channel and writes to
    //             `ssh_to_app_writer`; caller reads from `ssh_to_app_reader`.
    let (app_to_ssh_reader, app_to_ssh_writer) = std::io::pipe()?;
    let (ssh_to_app_reader, ssh_to_app_writer) = std::io::pipe()?;

    spawn_channel_pumps(stream, app_to_ssh_reader, ssh_to_app_writer);

    Ok((ssh_to_app_reader, app_to_ssh_writer))
}

/// Spawn the duplex pump pair for one channel. The pumps `spawn_blocking`
/// on the sync pipe ends so the current-thread runtime stays responsive
/// while sync I/O blocks.
fn spawn_channel_pumps(
    stream: ChannelStream<Msg>,
    from_app: PipeReader,
    to_app: PipeWriter,
) {
    let (mut chan_read, mut chan_write) = tokio::io::split(stream);

    // app → ssh: drain the sync pipe on the blocking pool, forward each
    // chunk to the channel.
    tokio::spawn(async move {
        let mut from_app = from_app;
        loop {
            let join = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; 8 * 1024];
                let res = from_app.read(&mut buf);
                (res, buf, from_app)
            })
            .await;
            let (res, mut buf, returned) = match join {
                Ok(t) => t,
                Err(_) => break,
            };
            from_app = returned;
            match res {
                Ok(0) => break, // app side closed
                Ok(n) => {
                    buf.truncate(n);
                    if chan_write.write_all(&buf).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = chan_write.shutdown().await;
    });

    // ssh → app: read async from the channel, hand each chunk to the
    // blocking pool to push into the sync pipe.
    tokio::spawn(async move {
        let mut to_app = to_app;
        let mut buf = vec![0u8; 8 * 1024];
        loop {
            let n = match chan_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let chunk: Vec<u8> = buf[..n].to_vec();
            let join = tokio::task::spawn_blocking(move || {
                let r = to_app.write_all(&chunk);
                (r, to_app)
            })
            .await;
            let (res, returned) = match join {
                Ok(t) => t,
                Err(_) => break,
            };
            to_app = returned;
            if res.is_err() {
                break;
            }
        }
    });
}

/// Run `command` on the remote in a fresh non-PTY exec channel and
/// drain its stdout / stderr / exit-status to completion. Closes the
/// channel before returning. Suitable for short one-shot operations
/// (read a config file, write a heredoc, query env). Long-running
/// streaming commands belong in `open_exec_channel` instead.
///
/// Output is captured as `String` via lossy UTF-8 conversion — the
/// only consumer (tool-integration JSON read/write) is text-only.
async fn run_oneshot_channel(
    handle: &Handle<Client>,
    command: String,
) -> Result<OneShotResult, SshError> {
    use russh::ChannelMsg;

    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| SshError::Channel(format!("open session: {e}")))?;
    // No PTY — we want clean stdout (no terminal-mode mangling, no
    // CR/LF injection) and the remote shell stays non-interactive.
    channel
        .exec(true, command.into_bytes())
        .await
        .map_err(|e| SshError::Channel(format!("exec: {e}")))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code: Option<u32> = None;

    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            // ext == 1 is stderr per RFC 4254 §5.2; other extended
            // data types are ignored (we don't expect any).
            ChannelMsg::ExtendedData { data, ext } if ext == 1 => {
                stderr.extend_from_slice(&data)
            }
            ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }

    Ok(OneShotResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
    })
}

/// TCP + auth + jump-host. Returns the russh `Handle` for the session.
/// Channels are opened on top via `open_exec_channel` /
/// `run_oneshot_channel`.
async fn auth_handshake(
    target: SshTarget,
    auth: SshAuth,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<Handle<Client>, SshError> {
    // Keepalive is what surfaces a dead connection. Without it, a socket
    // left half-open by laptop sleep (or any silent network drop) is never
    // probed: reads block forever with no EOF and writes just buffer into
    // the frozen kernel send queue, so the channel never errors, the
    // supervisor never sees ClientDied, and the UI keeps showing
    // "connected" while keystrokes vanish. With an interval set, russh
    // sends SSH global keepalives when idle and — after `keepalive_max`
    // (default 3) go unanswered — tears the session down, which errors the
    // channel and triggers reconnect. The timer is frozen during sleep, so
    // detection lands ~interval × max seconds after wake (~45s here). This
    // is the equivalent of OpenSSH's ServerAliveInterval/ServerAliveCountMax.
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    });

    let handle = if let Some(jump) = target.jump.as_deref() {
        let jump_client = Client::new(jump.hostname.clone(), jump.port, prompter.clone());
        let bastion = client::connect(
            config.clone(),
            (jump.hostname.clone(), jump.port),
            jump_client,
        )
        .await
        .map_err(|e| SshError::Connect(format!("jump tcp: {e}")))?;
        let bastion = authenticate(bastion, jump.user.clone(), auth.clone()).await?;
        let bastion_chan = bastion
            .channel_open_direct_tcpip(target.hostname.clone(), target.port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| SshError::Channel(format!("jump tcpip: {e}")))?;
        let stream = bastion_chan.into_stream();
        let inner_client = Client::new(target.hostname.clone(), target.port, prompter);
        let inner = client::connect_stream(config.clone(), stream, inner_client)
            .await
            .map_err(|e| SshError::Connect(format!("inner over jump: {e}")))?;
        authenticate(inner, target.user.clone(), auth).await?
    } else {
        let client = Client::new(target.hostname.clone(), target.port, prompter);
        let h = client::connect(
            config.clone(),
            (target.hostname.clone(), target.port),
            client,
        )
        .await
        .map_err(|e| SshError::Connect(format!("tcp: {e}")))?;
        authenticate(h, target.user.clone(), auth).await?
    };
    Ok(handle)
}

async fn authenticate(
    mut handle: Handle<Client>,
    user: String,
    auth: SshAuth,
) -> Result<Handle<Client>, SshError> {
    let ok = match auth {
        SshAuth::Agent => auth_with_agent(&mut handle, user).await?,
        SshAuth::KeyFile { path, passphrase } => {
            let key = russh_keys::load_secret_key(&path, passphrase.as_deref())
                .map_err(|e| SshError::Key(format!("load {}: {e}", path.display())))?;
            handle
                .authenticate_publickey(user, Arc::new(key))
                .await
                .map_err(|e| SshError::AuthFailed(format!("publickey: {e}")))?
        }
        SshAuth::Password { secret } => handle
            .authenticate_password(user, secret)
            .await
            .map_err(|e| SshError::AuthFailed(format!("password: {e}")))?,
    };
    if !ok {
        return Err(SshError::AuthFailed("server rejected credentials".into()));
    }
    Ok(handle)
}

/// Try every identity the agent offers, in order, until one authenticates.
/// Mirrors what OpenSSH's `ssh` does when `IdentitiesOnly=no`.
async fn auth_with_agent(handle: &mut Handle<Client>, user: String) -> Result<bool, SshError> {
    let agent = AgentClient::connect_env()
        .await
        .map_err(|e| SshError::Agent(format!("connect: {e}")))?;
    let mut agent = agent.dynamic();
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| SshError::Agent(format!("list identities: {e}")))?;
    if identities.is_empty() {
        return Err(SshError::Agent("no identities loaded".into()));
    }
    for pubkey in identities {
        let (returned, result) = handle
            .authenticate_future(user.clone(), pubkey, agent)
            .await;
        agent = returned;
        match result {
            Ok(true) => {
                debug!("ssh agent: identity accepted");
                return Ok(true);
            }
            Ok(false) => continue,
            Err(e) => {
                warn!("ssh agent: auth attempt errored: {e}");
                continue;
            }
        }
    }
    Ok(false)
}

/// Host-key handler. Consults `~/.ssh/known_hosts` via russh-keys; on a
/// miss or mismatch, asks the [`HostKeyPrompter`] (if any) for the user's
/// decision. With no prompter, defaults to refusing — never silent TOFU.
struct Client {
    hostname: String,
    port: u16,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
}

impl Client {
    fn new(hostname: String, port: u16, prompter: Option<Arc<dyn HostKeyPrompter>>) -> Self {
        Self {
            hostname,
            port,
            prompter,
        }
    }
}

#[async_trait]
impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let kind = match russh_keys::check_known_hosts(&self.hostname, self.port, server_public_key)
        {
            Ok(true) => return Ok(true),
            Ok(false) => HostKeyPromptKind::Unknown,
            Err(russh_keys::Error::KeyChanged { line }) => HostKeyPromptKind::Changed {
                previous_line: line as u32,
            },
            Err(e) => {
                warn!("known_hosts check failed: {e}");
                return Ok(false);
            }
        };

        let Some(prompter) = self.prompter.clone() else {
            warn!(
                "ssh: refusing unknown/changed host key for {}:{} — no prompter configured",
                self.hostname, self.port
            );
            return Ok(false);
        };

        let algorithm = server_public_key.name();
        let fingerprint = format!("SHA256:{}", server_public_key.fingerprint());

        let decision = prompter
            .prompt(&self.hostname, self.port, algorithm, &fingerprint, kind.clone())
            .await;

        match decision {
            HostKeyDecision::Reject => Ok(false),
            HostKeyDecision::AcceptOnce => Ok(true),
            HostKeyDecision::TrustPermanently => {
                // Only safe to learn for genuinely Unknown hosts. A
                // changed key would append a second entry that conflicts
                // with the existing one — OpenSSH refuses to use either
                // until the user resolves the conflict by hand. Better
                // to accept once and let the user edit known_hosts.
                if matches!(kind, HostKeyPromptKind::Unknown) {
                    if let Err(e) = russh_keys::known_hosts::learn_known_hosts(
                        &self.hostname,
                        self.port,
                        server_public_key,
                    ) {
                        warn!(
                            "ssh: failed to add {}:{} to known_hosts: {e}",
                            self.hostname, self.port
                        );
                    }
                }
                Ok(true)
            }
        }
    }
}
