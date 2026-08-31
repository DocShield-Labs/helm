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
        tokio::select! {
            _ = daemon.shutdown_requested() => break,
            accepted = listener.accept() => {
                let (stream, _addr) = accepted?;
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(daemon, stream).await {
                        tracing::debug!("client ended: {e}");
                    }
                });
            }
        }
    }
    tracing::info!(socket = %socket_path.display(), "helmd stopped");
    Ok(())
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
        let _ = tx.send(DaemonMsg::Error {
            req_id,
            context: context.into(),
            message,
        });
    };
    match msg {
        ClientMsg::Hello {
            protocol_version,
            client_name,
        } => {
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
            if wants_capabilities(&client_name) {
                let _ = tx.send(DaemonMsg::Capabilities {
                    compatibility_baseline: helm_proto::COMPATIBILITY_BASELINE,
                    extensions: vec![
                        helm_proto::extensions::DRAIN.into(),
                        helm_proto::extensions::AGENT_COMMANDS.into(),
                        helm_proto::extensions::FILE_SEARCH.into(),
                    ],
                });
            }
        }
        ClientMsg::Attach => daemon.attach(client_id),
        ClientMsg::Input { session, bytes } => {
            if let Err(e) = daemon.input(session, &bytes) {
                err(None, "input", e);
            }
        }
        ClientMsg::Resize {
            session,
            cols,
            rows,
        } => {
            if let Err(e) = daemon.resize(session, cols, rows) {
                err(None, "resize", e);
            }
        }
        ClientMsg::Screen { req_id, session } => {
            if let Err(e) = daemon.screen(client_id, req_id, session) {
                err(Some(req_id), "screen", e);
            }
        }
        ClientMsg::History {
            req_id,
            session,
            from_line,
            to_line,
        } => {
            if let Err(e) = daemon.history(client_id, req_id, session, from_line, to_line) {
                err(Some(req_id), "history", e);
            }
        }
        ClientMsg::NewSession {
            req_id,
            name,
            cwd,
            command,
        } => match daemon.new_session(name, cwd, command) {
            Ok(session) => {
                let _ = tx.send(DaemonMsg::Created { req_id, session });
            }
            Err(e) => err(Some(req_id), "new_session", e),
        },
        ClientMsg::KillSession { session } => {
            if let Err(e) = daemon.kill_session(session) {
                err(None, "kill_session", e);
            }
        }
        ClientMsg::RenameSession { session, name } => {
            if let Err(e) = daemon.rename_session(session, name) {
                err(None, "rename_session", e);
            }
        }
        ClientMsg::Search {
            req_id,
            query,
            regex: _,
            case_sensitive,
            scope,
            max_results,
        } => {
            // Regex search lands with M6; substring covers the palette
            // flow until then.
            let (matches, truncated) = daemon.search(&query, case_sensitive, scope, max_results);
            let _ = tx.send(DaemonMsg::SearchResults {
                req_id,
                matches,
                truncated,
            });
        }
        ClientMsg::Blocks { req_id, session } => {
            let blocks = daemon.blocks(session);
            let _ = tx.send(DaemonMsg::Blocks {
                req_id,
                session,
                blocks,
            });
        }
        ClientMsg::CompletePath {
            req_id,
            session,
            path,
            directories_only,
            max_results,
        } => {
            let daemon = daemon.clone();
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                match daemon.complete_path(session, &path, directories_only, max_results) {
                    Ok((candidates, truncated)) => {
                        let _ = tx.send(DaemonMsg::PathCompletions {
                            req_id,
                            session,
                            candidates,
                            truncated,
                        });
                    }
                    Err(message) => {
                        let _ = tx.send(DaemonMsg::Error {
                            req_id: Some(req_id),
                            context: "complete_path".into(),
                            message,
                        });
                    }
                }
            });
        }
        ClientMsg::Ping { req_id } => {
            let _ = tx.send(DaemonMsg::Pong { req_id });
        }
        ClientMsg::AckNotifications { up_to } => daemon.ack_notifications(up_to),
        ClientMsg::Shutdown => {
            tracing::info!("shutdown requested by client {client_id}");
            daemon.request_shutdown();
        }
        ClientMsg::Extension {
            req_id,
            name,
            payload,
        } => {
            if name == helm_proto::extensions::DRAIN && payload.is_empty() {
                daemon.begin_drain();
                let _ = tx.send(DaemonMsg::Extension {
                    req_id,
                    name,
                    payload: Vec::new(),
                });
            } else if name == helm_proto::extensions::AGENT_COMMANDS {
                use helm_proto::extensions::AgentCommandsRequest;
                let session = match serde_json::from_slice::<AgentCommandsRequest>(&payload) {
                    Ok(req) => req.session,
                    Err(e) => {
                        err(req_id, "agent_commands", format!("bad payload: {e}"));
                        return;
                    }
                };
                let daemon = daemon.clone();
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let reply = daemon.agent_commands(session).and_then(|commands| {
                        serde_json::to_vec(&commands).map_err(|e| e.to_string())
                    });
                    let _ = match reply {
                        Ok(payload) => tx.send(DaemonMsg::Extension {
                            req_id,
                            name,
                            payload,
                        }),
                        Err(message) => tx.send(DaemonMsg::Error {
                            req_id,
                            context: "agent_commands".into(),
                            message,
                        }),
                    };
                });
            } else if name == helm_proto::extensions::FILE_SEARCH {
                use helm_proto::extensions::FileSearchRequest;
                let req = match serde_json::from_slice::<FileSearchRequest>(&payload) {
                    Ok(req) => req,
                    Err(e) => {
                        err(req_id, "file_search", format!("bad payload: {e}"));
                        return;
                    }
                };
                let Some(req_id) = req_id else {
                    err(None, "file_search", "request id is required".into());
                    return;
                };
                let daemon = daemon.clone();
                let tx = tx.clone();
                // Answered with the typed `PathCompletions` — correlation
                // is by req_id, and reusing the shape keeps one schema.
                tokio::task::spawn_blocking(move || {
                    let _ = match daemon.file_search(req.session, &req.query, req.max_results) {
                        Ok((candidates, truncated)) => tx.send(DaemonMsg::PathCompletions {
                            req_id,
                            session: req.session,
                            candidates,
                            truncated,
                        }),
                        Err(message) => tx.send(DaemonMsg::Error {
                            req_id: Some(req_id),
                            context: "file_search".into(),
                            message,
                        }),
                    };
                });
            } else {
                err(
                    req_id,
                    "extension",
                    format!("unsupported extension {name:?}"),
                );
            }
        }
    }
}

fn wants_capabilities(client_name: &str) -> bool {
    let marker = helm_proto::CAPABILITIES_MARKER.trim_start_matches("; ");
    client_name
        .split(';')
        .map(str::trim)
        .any(|token| token == marker)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_only_sent_to_clients_that_opt_in() {
        let (daemon, _events) = Daemon::new();

        let (legacy_tx, mut legacy_rx) = unbounded_channel();
        let legacy_id = daemon.add_client(legacy_tx.clone());
        dispatch(
            &daemon,
            legacy_id,
            &legacy_tx,
            ClientMsg::Hello {
                protocol_version: helm_proto::PROTOCOL_VERSION,
                client_name: "helm-app 0.2.7".into(),
            },
        );
        assert!(matches!(
            legacy_rx.try_recv(),
            Ok(DaemonMsg::HelloAck { .. })
        ));
        assert!(legacy_rx.try_recv().is_err());

        let (current_tx, mut current_rx) = unbounded_channel();
        let current_id = daemon.add_client(current_tx.clone());
        dispatch(
            &daemon,
            current_id,
            &current_tx,
            ClientMsg::Hello {
                protocol_version: helm_proto::PROTOCOL_VERSION,
                client_name: format!("helm-app 0.2.8{}", helm_proto::CAPABILITIES_MARKER),
            },
        );
        assert!(matches!(
            current_rx.try_recv(),
            Ok(DaemonMsg::HelloAck { .. })
        ));
        assert!(matches!(
            current_rx.try_recv(),
            Ok(DaemonMsg::Capabilities { extensions, .. })
                if extensions.iter().any(|name| name == helm_proto::extensions::DRAIN)
        ));
        assert!(!wants_capabilities(
            "helm-app 0.2.8; note=helm-capabilities=1-but-not-opted-in"
        ));
    }

    #[test]
    fn drain_extension_rejects_new_sessions() {
        let (daemon, _events) = Daemon::new();
        let (tx, mut rx) = unbounded_channel();
        let client_id = daemon.add_client(tx.clone());
        dispatch(
            &daemon,
            client_id,
            &tx,
            ClientMsg::Extension {
                req_id: Some(7),
                name: helm_proto::extensions::DRAIN.into(),
                payload: Vec::new(),
            },
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Extension {
                req_id: Some(7),
                ..
            })
        ));
        assert!(daemon
            .new_session(None, None, None)
            .unwrap_err()
            .contains("draining"));
    }
}
