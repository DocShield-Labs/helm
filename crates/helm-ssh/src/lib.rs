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
/// that owns the russh `Handle` + `Channel`. Drop or call `.disconnect()`
/// to terminate; either signals the thread to clean up and join.
pub struct SshSession {
    /// Set once per session. Sending fires the cleanup path on the I/O
    /// thread (russh disconnect, drop the channel, exit the runtime).
    teardown: parking_lot::Mutex<Option<oneshot::Sender<()>>>,
    /// Joined on Drop so the OS thread exits before we go.
    join: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl SshSession {
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
pub struct SshConnection {
    pub session: SshSession,
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

/// Connect, authenticate, request a PTY, exec `command`, and return a
/// blocking byte-stream pair. The SSH negotiation is synchronous from the
/// caller's perspective — a dedicated OS thread handles the async russh
/// internals end-to-end.
///
/// `connect_timeout` covers the entire negotiation (TCP connect through
/// auth + channel open + exec). 15s is a reasonable default.
///
/// `prompter` is invoked when the server key isn't in `~/.ssh/known_hosts`
/// (or differs from the recorded entry). Pass `None` to refuse unknown
/// hosts outright — useful for tests.
pub fn connect(
    target: SshTarget,
    auth: SshAuth,
    command: String,
    connect_timeout: Duration,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<SshConnection, SshError> {
    // Two unidirectional pipes form a duplex bridge between the SSH
    // channel and helm-tmux's blocking reader/writer threads.
    //   ssh_to_app:  SSH thread writes bytes from the channel here;
    //                helm-tmux reader thread reads.
    //   app_to_ssh:  helm-tmux writer thread writes here; SSH thread
    //                reads and forwards to the channel.
    let (app_reader, ssh_writer) = std::io::pipe()?;
    let (ssh_reader, app_writer) = std::io::pipe()?;

    let (ready_tx, ready_rx) = sync_mpsc::sync_channel::<Result<(), SshError>>(1);
    let (teardown_tx, teardown_rx) = oneshot::channel::<()>();

    let join = thread::Builder::new()
        .name("helm-ssh".into())
        .spawn(move || {
            io_thread(
                target,
                auth,
                command,
                ssh_reader,
                ssh_writer,
                ready_tx,
                teardown_rx,
                prompter,
            );
        })
        .map_err(|e| SshError::Thread(format!("spawn: {e}")))?;

    // Wait for the I/O thread to finish negotiation (or fail).
    let result = ready_rx
        .recv_timeout(connect_timeout)
        .map_err(|_| SshError::Connect(format!("negotiation timed out after {connect_timeout:?}")));

    match result {
        Ok(Ok(())) => Ok(SshConnection {
            session: SshSession {
                teardown: parking_lot::Mutex::new(Some(teardown_tx)),
                join: parking_lot::Mutex::new(Some(join)),
            },
            reader: app_reader,
            writer: app_writer,
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

/// Body of the dedicated SSH I/O thread.
///
/// Builds a current-thread tokio runtime, runs the russh negotiation
/// inside it, and on success enters a duplex pump loop until EOF on
/// either pipe or until the teardown signal fires.
#[allow(clippy::too_many_arguments)]
fn io_thread(
    target: SshTarget,
    auth: SshAuth,
    command: String,
    from_app: PipeReader,
    to_app: PipeWriter,
    ready: sync_mpsc::SyncSender<Result<(), SshError>>,
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
        let conn_result = negotiate(target, auth, command, prompter).await;
        let (handle, stream) = match conn_result {
            Ok(parts) => {
                let _ = ready.send(Ok(()));
                parts
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };

        // Bridge bytes between the two pipes and the channel stream.
        // The pipes are sync (`PipeReader`/`PipeWriter`); the channel is
        // async. We use `spawn_blocking` rather than `block_in_place`
        // because we're on a current-thread runtime — `block_in_place`
        // panics there, silently killing both pumps and producing the
        // "tmux did not emit %session-changed" timeout downstream.
        let (mut chan_read, mut chan_write) = tokio::io::split(stream);

        // app → ssh: drain the sync pipe on the blocking pool, forward
        // each chunk to the channel.
        let app_to_ssh = tokio::spawn(async move {
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
        let ssh_to_app = tokio::spawn(async move {
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

        // Wait for either pump to finish or the teardown signal.
        tokio::select! {
            _ = app_to_ssh => debug!("ssh: app→ssh pump ended"),
            _ = ssh_to_app => debug!("ssh: ssh→app pump ended"),
            _ = teardown => debug!("ssh: teardown signal received"),
        }

        // Best-effort clean disconnect.
        if !handle.is_closed() {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "helm exit", "en")
                .await;
        }
    });
}

/// Run the russh negotiation: TCP, auth, channel open, PTY request, exec.
/// Returns the handle (kept alive for the session) and the channel stream
/// (split into halves for the pump loop).
async fn negotiate(
    target: SshTarget,
    auth: SshAuth,
    command: String,
    prompter: Option<Arc<dyn HostKeyPrompter>>,
) -> Result<(Handle<Client>, ChannelStream<Msg>), SshError> {
    let config = Arc::new(client::Config::default());

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
    Ok((handle, stream))
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
