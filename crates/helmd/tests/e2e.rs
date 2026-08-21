//! End-to-end: daemon on a real unix socket, a real `/bin/sh` child in a
//! real PTY, OSC 133 markers through the full ingest path, then replay
//! and search over the ring.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use helm_proto::{
    encode_frame, ClientMsg, DaemonMsg, FrameDecoder, ReplayFrom, SearchScope, WorkspaceId,
};

struct TestClient {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl TestClient {
    fn connect(path: &PathBuf) -> Self {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(200)))
                        .unwrap();
                    return Self { stream, decoder: FrameDecoder::new() };
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("daemon never came up: {e}"),
            }
        }
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.stream.write_all(&encode_frame(msg).unwrap()).unwrap();
    }

    /// Pump frames until `pred` returns Some, or panic at the deadline.
    /// Every received message is also handed to `all` for accumulation.
    fn recv_until<T>(
        &mut self,
        secs: u64,
        all: &mut Vec<DaemonMsg>,
        pred: impl Fn(&DaemonMsg) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut buf = [0u8; 64 * 1024];
        loop {
            while let Some(msg) = self.decoder.next::<DaemonMsg>().unwrap() {
                let hit = pred(&msg);
                all.push(msg);
                if let Some(v) = hit {
                    return v;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out; saw {} messages: {all:#?}",
                all.len()
            );
            match self.stream.read(&mut buf) {
                Ok(0) => panic!("daemon closed the connection; saw: {all:#?}"),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("read: {e}"),
            }
        }
    }
}

fn output_bytes(msgs: &[DaemonMsg]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in msgs {
        if let DaemonMsg::Output { bytes, .. } = m {
            out.extend_from_slice(bytes);
        }
    }
    out
}

#[test]
fn full_session_lifecycle() {
    let socket = std::env::temp_dir().join(format!("helmd-e2e-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    {
        let socket = socket.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(helmd::server::serve(&socket))
                .unwrap();
        });
    }

    let mut c = TestClient::connect(&socket);
    let mut seen = Vec::new();

    // Hello → HelloAck with a matching protocol version.
    c.send(&ClientMsg::Hello {
        protocol_version: helm_proto::PROTOCOL_VERSION,
        client_name: "e2e-test".into(),
    });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::HelloAck { protocol_version, .. } => {
            assert_eq!(*protocol_version, helm_proto::PROTOCOL_VERSION);
            Some(())
        }
        _ => None,
    });

    // Attach before creating anything so we get all broadcasts.
    c.send(&ClientMsg::Attach { resume: vec![] });

    // New workspace → Created reply (and an initial shell window).
    c.send(&ClientMsg::NewWorkspace { req_id: 1, name: Some("e2e".into()) });
    let ws_id: WorkspaceId = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Created { req_id: 1, workspace, window, pane } => {
            assert!(window.is_some() && pane.is_some(), "workspace should come with a window");
            Some(*workspace)
        }
        _ => None,
    });

    // A window running a script that walks the full OSC 133 block
    // lifecycle (A → B → C → output → D;3), then lingers so the pane
    // survives long enough for search + replay.
    let script = concat!(
        "printf '\\033]133;A\\007'; ",
        "printf '\\033]133;B;cmdline_b64=ZTJl\\007'; ", // "e2e"
        "printf '\\033]133;C\\007'; ",
        "printf 'helm-e2e-out\\n'; ",
        "printf '\\033]133;D;3\\007'; ",
        "sleep 5",
    );
    c.send(&ClientMsg::NewWindow {
        req_id: 2,
        workspace: ws_id,
        name: Some("e2e-window".into()),
        cwd: None,
        command: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
    });
    let (window_id, pane_id) = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Created { req_id: 2, window, pane, .. } => Some((window.unwrap(), pane.unwrap())),
        _ => None,
    });

    // Live output arrives, with the markers stripped.
    c.recv_until(10, &mut seen, |m| match m {
        DaemonMsg::Output { pane, bytes, .. } if *pane == pane_id => {
            String::from_utf8_lossy(bytes).contains("helm-e2e-out").then_some(())
        }
        _ => None,
    });
    let live = output_bytes(&seen);
    assert!(!live.windows(4).any(|w| w == b"133;"), "markers leaked into output");

    // The block closed with exit 3 and the decoded cmdline.
    c.recv_until(10, &mut seen, |m| match m {
        DaemonMsg::Block { pane, block } if *pane == pane_id => (block.exit_code == Some(3)
            && block.cmdline.as_deref() == Some("e2e")
            && block.end_seq.is_some())
        .then_some(()),
        _ => None,
    });

    // Search finds the line with a seq anchor inside the block.
    c.send(&ClientMsg::Search {
        req_id: 3,
        query: "e2e-out".into(),
        regex: false,
        case_sensitive: false,
        scope: SearchScope::All,
        max_results: 10,
    });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::SearchResults { matches, .. } => matches
            .iter()
            .any(|hit| hit.pane == pane_id && hit.line_text.contains("helm-e2e-out"))
            .then_some(()),
        _ => None,
    });

    // Replay returns the same bytes again (exact reattach semantics).
    c.send(&ClientMsg::Replay { pane: pane_id, from: ReplayFrom::LastBytes(100_000) });
    let mut replayed = Vec::new();
    c.recv_until(5, &mut replayed, |m| match m {
        DaemonMsg::ReplayDone { pane, .. } if *pane == pane_id => Some(()),
        _ => None,
    });
    let replay_bytes = output_bytes(&replayed);
    assert!(
        String::from_utf8_lossy(&replay_bytes).contains("helm-e2e-out"),
        "replay missing output: {replay_bytes:?}"
    );

    // Kill the window; it disappears from the tree (the workspace's
    // auto-spawned initial window remains).
    c.send(&ClientMsg::KillWindow { window: window_id });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::TreeChanged { state } => state
            .workspaces
            .iter()
            .flat_map(|w| w.windows.iter())
            .all(|w| w.id != window_id)
            .then_some(()),
        _ => None,
    });

    let _ = std::fs::remove_file(&socket);
}
