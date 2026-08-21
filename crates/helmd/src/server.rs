//! Unix-socket server (`helmd serve`) and the stdio bridge
//! (`helmd stdio`) that helm runs over an SSH exec channel.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::unbounded_channel;

use helm_proto::{encode_frame, ClientMsg, DaemonMsg, FrameDecoder};

use crate::daemon::{ClientId, Daemon};

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default socket: `~/.helm/helmd.sock`.
pub fn default_socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".helm")
        .join("helmd.sock")
}

/// Run the daemon on `socket_path` until `Shutdown` (or forever).
pub async fn serve(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Stale socket handling: if something answers, another daemon owns
    // it — refuse to double-run. If nothing answers, it's a leftover
    // file from a crash; remove and rebind.
    if socket_path.exists() {
        match std::os::unix::net::UnixStream::connect(socket_path) {
            Ok(_) => anyhow::bail!("another helmd already serves {}", socket_path.display()),
            Err(_) => std::fs::remove_file(socket_path)?,
        }
    }
    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path.display(), version = DAEMON_VERSION, "helmd listening");

    let (daemon, events_rx) = Daemon::new();
    tokio::spawn(daemon.clone().run(events_rx));

    loop {
        let (stream, _addr) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(daemon, stream).await {
                tracing::debug!("client ended: {e}");
            }
        });
    }
}

async fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) -> anyhow::Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();

    let (tx, mut rx) = unbounded_channel::<DaemonMsg>();
    let client_id = daemon.add_client(tx.clone());

    // Writer task: daemon messages → frames on the socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let frame = match encode_frame(&msg) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("encode: {e}");
                    continue;
                }
            };
            if write_half.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    // Read loop: frames → dispatch.
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    let result: anyhow::Result<()> = loop {
        let n = match read_half.read(&mut buf).await {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(e.into()),
        };
        decoder.feed(&buf[..n]);
        loop {
            match decoder.next::<ClientMsg>() {
                Ok(Some(msg)) => dispatch(&daemon, client_id, &tx, msg),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("client {client_id}: corrupt stream: {e}");
                    daemon.remove_client(client_id);
                    writer.abort();
                    return Err(e.into());
                }
            }
        }
    };

    daemon.remove_client(client_id);
    writer.abort();
    result
}

/// Handle one message.
fn dispatch(
    daemon: &Arc<Daemon>,
    client_id: ClientId,
    tx: &tokio::sync::mpsc::UnboundedSender<DaemonMsg>,
    msg: ClientMsg,
) {
    let err = |req_id: Option<u64>, context: &str, message: String| {
        let _ = tx.send(DaemonMsg::Error { req_id, context: context.into(), message });
    };
    match msg {
        ClientMsg::Hello { protocol_version, client_name } => {
            if protocol_version != helm_proto::PROTOCOL_VERSION {
                err(
                    None,
                    "hello",
                    format!(
                        "protocol mismatch: client {protocol_version}, daemon {}",
                        helm_proto::PROTOCOL_VERSION
                    ),
                );
                return;
            }
            tracing::info!("client {client_id} connected: {client_name}");
            let _ = tx.send(daemon.hello_ack(DAEMON_VERSION));
        }
        ClientMsg::Attach { resume } => daemon.attach(client_id, &resume),
        ClientMsg::Input { pane, bytes } => {
            if let Err(e) = daemon.input(pane, &bytes) {
                err(None, "input", e);
            }
        }
        ClientMsg::Resize { pane, cols, rows } => {
            if let Err(e) = daemon.resize(pane, cols, rows) {
                err(None, "resize", e);
            }
        }
        ClientMsg::Replay { pane, from } => daemon.replay(client_id, pane, from),
        ClientMsg::NewWorkspace { req_id, name } => match daemon.new_workspace(name) {
            Ok((workspace, window, pane)) => {
                let _ = tx.send(DaemonMsg::Created {
                    req_id,
                    workspace,
                    window: Some(window),
                    pane: Some(pane),
                });
            }
            Err(e) => err(Some(req_id), "new_workspace", e),
        },
        ClientMsg::NewWindow { req_id, workspace, name, cwd, command } => {
            match daemon.new_window(workspace, name, cwd, command) {
                Ok((window, pane)) => {
                    let _ = tx.send(DaemonMsg::Created {
                        req_id,
                        workspace,
                        window: Some(window),
                        pane: Some(pane),
                    });
                }
                Err(e) => err(Some(req_id), "new_window", e),
            }
        }
        ClientMsg::KillWindow { window } => {
            if let Err(e) = daemon.kill_window(window) {
                err(None, "kill_window", e);
            }
        }
        ClientMsg::KillWorkspace { workspace } => {
            if let Err(e) = daemon.kill_workspace(workspace) {
                err(None, "kill_workspace", e);
            }
        }
        ClientMsg::RenameWorkspace { workspace, name } => {
            if let Err(e) = daemon.rename_workspace(workspace, name) {
                err(None, "rename_workspace", e);
            }
        }
        ClientMsg::RenameWindow { window, name } => {
            if let Err(e) = daemon.rename_window(window, name) {
                err(None, "rename_window", e);
            }
        }
        ClientMsg::Search { req_id, query, regex: _, case_sensitive, scope, max_results } => {
            // Regex search lands with M6; substring covers the palette
            // flow until then.
            let (matches, truncated) = daemon.search(&query, case_sensitive, scope, max_results);
            let _ = tx.send(DaemonMsg::SearchResults { req_id, matches, truncated });
        }
        ClientMsg::Blocks { req_id, pane } => {
            let blocks = daemon.blocks(pane);
            let _ = tx.send(DaemonMsg::Blocks { req_id, pane, blocks });
        }
        ClientMsg::Ping { req_id } => {
            let _ = tx.send(DaemonMsg::Pong { req_id });
        }
        ClientMsg::AckNotifications { up_to } => daemon.ack_notifications(up_to),
        ClientMsg::Shutdown => {
            tracing::info!("shutdown requested by client {client_id}");
            std::process::exit(0);
        }
    }
}

// -------------------------------------------------------------------
// stdio bridge
// -------------------------------------------------------------------

/// `helmd stdio`: bridge stdin/stdout ⇄ the local daemon socket,
/// spawning `helmd serve` first if nothing is listening. This is the
/// whole remote transport: helm runs it over an SSH exec channel and
/// speaks the same frames it would over a local socket.
pub fn stdio_bridge(socket_path: &Path) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    let exe = std::env::current_exe()?;
    let stream = helm_proto::connect_or_spawn_socket(socket_path, &exe)?;
    let mut sock_read = stream.try_clone()?;
    let mut sock_write = stream;

    // stdin → socket
    let to_sock = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sock_write.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // SSH channel closed — shut down our socket half so the daemon
        // sees a clean disconnect.
        let _ = sock_write.shutdown(std::net::Shutdown::Write);
    });

    // socket → stdout
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match sock_read.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }
    let _ = to_sock.join();
    Ok(())
}
