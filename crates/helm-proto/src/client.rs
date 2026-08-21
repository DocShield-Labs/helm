//! Typed client for a helmd daemon (feature `client`).
//!
//! Transport-agnostic by construction: the core entry point is
//! `connect_io`, which takes any blocking `Read`/`Write` pair — a local
//! `UnixStream`'s cloned halves, or the stdio pipes of `helmd stdio`
//! running on the far side of an SSH exec channel. One implementation,
//! both transports; the SSH path gets no second code path to drift.
//!
//! Threading model: `connect_io` performs the `Hello` → `HelloAck`
//! handshake synchronously (call it from `spawn_blocking` in async
//! contexts), then spawns two OS threads — a writer draining queued
//! `ClientMsg`s and a reader decoding `DaemonMsg`s into a tokio
//! unbounded channel the app consumes from async code.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{
    encode_frame, ClientMsg, DaemonMsg, FrameDecoder, Notification, NotificationId, PaneId,
    ProtoError, ReplayFrom, SearchScope, TreeSnapshot, WindowId, WorkspaceId, PROTOCOL_VERSION,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),
    #[error("daemon closed the connection during handshake")]
    HandshakeClosed,
    #[error("daemon rejected handshake: {0}")]
    HandshakeRejected(String),
    #[error("unexpected first message from daemon")]
    UnexpectedHello,
    #[error("daemon connection is closed")]
    Closed,
    #[error("helmd did not come up on {0}")]
    SpawnTimeout(PathBuf),
}

/// Everything learned at connect time.
pub struct Connected {
    pub client: HelmdClient,
    /// Daemon → app messages. Output, blocks, tree changes, search
    /// results, notifications — the caller routes these. The channel
    /// closes when the transport drops: that IS the disconnect signal.
    pub events: UnboundedReceiver<DaemonMsg>,
    pub daemon_version: String,
    pub state: TreeSnapshot,
    /// Notifications accumulated while nothing was attached.
    pub pending: Vec<Notification>,
}

pub struct HelmdClient {
    tx: UnboundedSender<ClientMsg>,
}

/// Connect over an arbitrary blocking Read/Write pair. Blocking during
/// the handshake — wrap in `spawn_blocking` from async code.
///
/// `on_close` runs on the writer thread after the command channel
/// closes (client dropped) — the place to shut down a socket half so
/// the daemon sees a clean EOF. Pass `None` for pipe transports where
/// dropping the writer is already the close.
pub fn connect_io(
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
    client_name: &str,
    on_close: Option<Box<dyn FnOnce() + Send>>,
) -> Result<Connected, ClientError> {
    // Handshake, synchronously, before any pump threads exist.
    let hello = encode_frame(&ClientMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: client_name.to_string(),
    })?;
    writer.write_all(&hello)?;
    writer.flush()?;

    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    let (daemon_version, state, pending) = loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(ClientError::HandshakeClosed);
        }
        decoder.feed(&buf[..n]);
        match decoder.next::<DaemonMsg>()? {
            Some(DaemonMsg::HelloAck { daemon_version, state, pending, .. }) => {
                break (daemon_version, state, pending)
            }
            Some(DaemonMsg::Error { message, .. }) => {
                return Err(ClientError::HandshakeRejected(message))
            }
            Some(_) => return Err(ClientError::UnexpectedHello),
            None => continue,
        }
    };

    // Writer thread: queued commands → frames.
    let (tx, mut cmd_rx) = unbounded_channel::<ClientMsg>();
    std::thread::Builder::new()
        .name("helmd-client-write".into())
        .spawn(move || {
            while let Some(msg) = cmd_rx.blocking_recv() {
                let frame = match encode_frame(&msg) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!("helmd client encode: {e}");
                        continue;
                    }
                };
                if writer.write_all(&frame).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
            drop(writer);
            if let Some(close) = on_close {
                close();
            }
        })?;

    // Reader thread: frames → event channel. The decoder carries over
    // from the handshake so a message split across that boundary is
    // preserved.
    let (event_tx, event_rx) = unbounded_channel::<DaemonMsg>();
    std::thread::Builder::new()
        .name("helmd-client-read".into())
        .spawn(move || {
            loop {
                loop {
                    match decoder.next::<DaemonMsg>() {
                        Ok(Some(msg)) => {
                            if event_tx.send(msg).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!("helmd client decode: {e}");
                            return;
                        }
                    }
                }
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return, // consumer sees channel close
                    Ok(n) => decoder.feed(&buf[..n]),
                }
            }
        })?;

    Ok(Connected {
        client: HelmdClient { tx },
        events: event_rx,
        daemon_version,
        state,
        pending,
    })
}

/// Connect to a daemon on a local unix socket. Blocking.
pub fn connect_unix(socket: &Path, client_name: &str) -> Result<Connected, ClientError> {
    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    let reader = stream.try_clone()?;
    let closer = stream.try_clone()?;
    connect_io(
        Box::new(reader),
        Box::new(stream),
        client_name,
        Some(Box::new(move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        })),
    )
}

/// Connect to a local daemon, spawning `helmd_bin serve` first if
/// nothing is listening. Blocking.
pub fn connect_or_spawn_unix(
    socket: &Path,
    helmd_bin: &Path,
    client_name: &str,
) -> Result<Connected, ClientError> {
    let stream = crate::connect_or_spawn_socket(socket, helmd_bin)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut => ClientError::SpawnTimeout(socket.to_path_buf()),
            _ => ClientError::Io(e),
        })?;
    let reader = stream.try_clone()?;
    let closer = stream.try_clone()?;
    connect_io(
        Box::new(reader),
        Box::new(stream),
        client_name,
        Some(Box::new(move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        })),
    )
}

impl HelmdClient {
    fn send(&self, msg: ClientMsg) -> Result<(), ClientError> {
        self.tx.send(msg).map_err(|_| ClientError::Closed)
    }

    pub fn attach(&self, resume: Vec<(PaneId, u64)>) -> Result<(), ClientError> {
        self.send(ClientMsg::Attach { resume })
    }
    pub fn input(&self, pane: PaneId, bytes: Vec<u8>) -> Result<(), ClientError> {
        self.send(ClientMsg::Input { pane, bytes })
    }
    pub fn resize(&self, pane: PaneId, cols: u16, rows: u16) -> Result<(), ClientError> {
        self.send(ClientMsg::Resize { pane, cols, rows })
    }
    pub fn replay(&self, pane: PaneId, from: ReplayFrom) -> Result<(), ClientError> {
        self.send(ClientMsg::Replay { pane, from })
    }
    pub fn new_workspace(&self, req_id: u64, name: Option<String>) -> Result<(), ClientError> {
        self.send(ClientMsg::NewWorkspace { req_id, name })
    }
    pub fn new_window(
        &self,
        req_id: u64,
        workspace: WorkspaceId,
        name: Option<String>,
        cwd: Option<String>,
        command: Option<Vec<String>>,
    ) -> Result<(), ClientError> {
        self.send(ClientMsg::NewWindow { req_id, workspace, name, cwd, command })
    }
    pub fn kill_window(&self, window: WindowId) -> Result<(), ClientError> {
        self.send(ClientMsg::KillWindow { window })
    }
    pub fn kill_workspace(&self, workspace: WorkspaceId) -> Result<(), ClientError> {
        self.send(ClientMsg::KillWorkspace { workspace })
    }
    pub fn rename_workspace(&self, workspace: WorkspaceId, name: String) -> Result<(), ClientError> {
        self.send(ClientMsg::RenameWorkspace { workspace, name })
    }
    pub fn rename_window(&self, window: WindowId, name: String) -> Result<(), ClientError> {
        self.send(ClientMsg::RenameWindow { window, name })
    }
    pub fn search(
        &self,
        req_id: u64,
        query: String,
        regex: bool,
        case_sensitive: bool,
        scope: SearchScope,
        max_results: u32,
    ) -> Result<(), ClientError> {
        self.send(ClientMsg::Search { req_id, query, regex, case_sensitive, scope, max_results })
    }
    pub fn blocks(&self, req_id: u64, pane: PaneId) -> Result<(), ClientError> {
        self.send(ClientMsg::Blocks { req_id, pane })
    }
    pub fn ping(&self, req_id: u64) -> Result<(), ClientError> {
        self.send(ClientMsg::Ping { req_id })
    }
    pub fn ack_notifications(&self, up_to: NotificationId) -> Result<(), ClientError> {
        self.send(ClientMsg::AckNotifications { up_to })
    }
}
