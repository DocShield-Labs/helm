//! End-to-end: daemon on a real unix socket, a real `/bin/sh` child in a
//! real PTY, OSC 133 markers through the full ingest path, then screen,
//! history and search over the model.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use helm_proto::{encode_frame, ClientMsg, DaemonMsg, FrameDecoder, PathEntryKind, SearchScope};

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
                    return Self {
                        stream,
                        decoder: FrameDecoder::new(),
                    };
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

/// A daemon on a fresh temp socket plus a connected, attached client.
fn start_daemon(tag: &str) -> (PathBuf, TestClient, Vec<DaemonMsg>) {
    let socket = std::env::temp_dir().join(format!("helmd-e2e-{tag}-{}.sock", std::process::id()));
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
        client_name: format!("e2e-{tag}"),
    });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::HelloAck {
            protocol_version, ..
        } => {
            assert_eq!(*protocol_version, helm_proto::PROTOCOL_VERSION);
            Some(())
        }
        _ => None,
    });
    // Attach before creating anything so we get all broadcasts.
    c.send(&ClientMsg::Attach);
    (socket, c, seen)
}

/// Every row text a session's screen messages carried, in arrival order.
fn screen_texts(msgs: &[DaemonMsg], session_id: helm_proto::SessionId) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs {
        match m {
            DaemonMsg::Screen {
                session, screen, ..
            } if *session == session_id => out.extend(screen.lines.iter().map(|r| r.text())),
            DaemonMsg::ScreenDiff { session, rows, .. } if *session == session_id => {
                out.extend(rows.iter().map(|(_, r)| r.text()))
            }
            _ => {}
        }
    }
    out
}

#[test]
fn full_session_lifecycle() {
    let (socket, mut c, mut seen) = start_daemon("lifecycle");
    let completion_root =
        std::env::temp_dir().join(format!("helmd-completion-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&completion_root);
    std::fs::create_dir_all(completion_root.join("Code")).unwrap();
    std::fs::write(completion_root.join("config.toml"), "").unwrap();

    // A session running a script that walks the full OSC 133 block
    // lifecycle (A → B → C → output → D;3), then lingers so the session
    // survives long enough for search + replay.
    let script = concat!(
        "printf '\\033]133;A\\007'; ",
        "printf '\\033]133;B;cmdline_b64=ZTJl\\007'; ", // "e2e"
        "printf '\\033]133;C\\007'; ",
        "printf 'helm-e2e-out\\n'; ",
        "printf '\\033]133;D;3\\007'; ",
        "sleep 5",
    );
    c.send(&ClientMsg::NewSession {
        req_id: 2,
        name: Some("e2e-session".into()),
        cwd: Some(completion_root.to_string_lossy().into_owned()),
        command: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
    });
    let session_id = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Created { req_id: 2, session } => Some(*session),
        _ => None,
    });

    // The block closes with exit 3, the decoded cmdline, and line
    // positions inside the session's line space. Blocks are broadcast at
    // ingest; the screen flush that paints the output trails by a tick.
    let block = c.recv_until(10, &mut seen, |m| match m {
        DaemonMsg::Block { session, block } if *session == session_id => (block.exit_code
            == Some(3)
            && block.cmdline.as_deref() == Some("e2e")
            && block.end_line.is_some())
        .then_some(block.clone()),
        _ => None,
    });
    assert!(block.output_line.unwrap() <= block.end_line.unwrap());
    assert!(block.start_line <= block.cmd_line.unwrap());

    // Live output arrives as screen rows, with the markers stripped.
    let has_output = |m: &DaemonMsg| {
        screen_texts(std::slice::from_ref(m), session_id)
            .iter()
            .any(|t| t.contains("helm-e2e-out"))
            .then_some(())
    };
    if !seen.iter().any(|m| has_output(m).is_some()) {
        c.recv_until(10, &mut seen, has_output);
    }
    assert!(
        !screen_texts(&seen, session_id)
            .iter()
            .any(|t| t.contains("133;")),
        "markers leaked into rows"
    );

    // Search finds the line with a line anchor inside the block.
    c.send(&ClientMsg::Search {
        req_id: 3,
        query: "e2e-out".into(),
        regex: false,
        case_sensitive: false,
        scope: SearchScope::All,
        max_results: 10,
    });
    let hit_line = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::SearchResults { matches, .. } => matches
            .iter()
            .find(|hit| hit.session == session_id && hit.line_text.contains("helm-e2e-out"))
            .map(|hit| hit.line),
        _ => None,
    });
    assert!(hit_line >= block.output_line.unwrap() && hit_line < block.end_line.unwrap());

    // A fresh paint from the model carries the output (exact reattach,
    // no byte replay), sized to the session.
    c.send(&ClientMsg::Screen {
        req_id: 4,
        session: session_id,
    });
    let screen = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Screen {
            req_id: Some(4),
            session,
            screen,
        } if *session == session_id => Some(screen.clone()),
        _ => None,
    });
    assert_eq!(screen.lines.len(), screen.rows as usize);
    assert!(screen
        .lines
        .iter()
        .any(|r| r.text().contains("helm-e2e-out")));
    assert_eq!(
        screen.top_line, 0,
        "one line of output can't have scrolled a 24-row session"
    );

    // History paging answers even when nothing has scrolled out yet.
    c.send(&ClientMsg::History {
        req_id: 5,
        session: session_id,
        from_line: 0,
        to_line: u64::MAX,
    });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::History {
            req_id: 5,
            session,
            rows,
            history_start,
            top_line,
            ..
        } if *session == session_id => {
            assert_eq!((*history_start, *top_line), (0, 0));
            assert!(rows.is_empty());
            Some(())
        }
        _ => None,
    });

    // Path completion resolves against the session's own cwd and returns
    // canonical entry casing without running shell code.
    c.send(&ClientMsg::CompletePath {
        req_id: 6,
        session: session_id,
        path: "co".into(),
        directories_only: true,
        max_results: 20,
    });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::PathCompletions {
            req_id: 6,
            candidates,
            truncated,
            ..
        } => {
            assert!(!truncated);
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].value, "Code/");
            assert_eq!(candidates[0].kind, PathEntryKind::Directory);
            Some(())
        }
        _ => None,
    });

    // Kill the session; it disappears from the flat tree.
    c.send(&ClientMsg::KillSession {
        session: session_id,
    });
    c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::TreeChanged { state } => state
            .sessions
            .iter()
            .all(|session| session.id != session_id)
            .then_some(()),
        _ => None,
    });

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(completion_root);
}

/// The M8 guarantee: output that scrolls out of the grid is retained as
/// history, addressed by the same absolute lines the block index and
/// search use, and pages back in full — a 24-row session holds a 3000-line
/// command without losing its start.
#[test]
fn long_output_pages_through_history() {
    let (socket, mut c, mut seen) = start_daemon("history");

    // One block whose command prints 3000 numbered lines ("c2Vx" = "seq").
    let script = concat!(
        "printf '\\033]133;A\\007'; ",
        "printf '\\033]133;B;cmdline_b64=c2Vx\\007'; ",
        "printf '\\033]133;C\\007'; ",
        "seq 1 3000; ",
        "printf '\\033]133;D;0\\007'; ",
        "sleep 5",
    );
    c.send(&ClientMsg::NewSession {
        req_id: 2,
        name: None,
        cwd: None,
        command: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
    });
    let session_id = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Created { req_id: 2, session } => Some(*session),
        _ => None,
    });
    let block = c.recv_until(30, &mut seen, |m| match m {
        DaemonMsg::Block { session, block } if *session == session_id => {
            (block.cmdline.as_deref() == Some("seq") && block.end_line.is_some())
                .then_some(block.clone())
        }
        _ => None,
    });
    let out = block.output_line.unwrap();
    let end = block.end_line.unwrap();
    assert!(end - out >= 3000, "block spans {out}..{end}");

    // The first rows of the command, long gone from the 24-row grid.
    c.send(&ClientMsg::History {
        req_id: 3,
        session: session_id,
        from_line: out,
        to_line: out + 100,
    });
    let rows = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::History {
            req_id: 3, rows, ..
        } => Some(rows.clone()),
        _ => None,
    });
    assert_eq!(rows.len(), 100);
    assert_eq!(rows[0].text(), "1");
    assert_eq!(rows[99].text(), "100");

    // An open-ended request pages from the end, clamped to one page.
    c.send(&ClientMsg::History {
        req_id: 4,
        session: session_id,
        from_line: 0,
        to_line: u64::MAX,
    });
    let (from, rows, history_start, top_line) = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::History {
            req_id: 4,
            from_line,
            rows,
            history_start,
            top_line,
            ..
        } => Some((*from_line, rows.clone(), *history_start, *top_line)),
        _ => None,
    });
    assert!(rows.len() as u64 <= helm_proto::MAX_HISTORY_PAGE);
    assert_eq!(from + rows.len() as u64, top_line);
    assert_eq!(history_start, 0);
    assert!(top_line >= 2976, "top_line {top_line}");
    // The grid still holds the last 24 lines; history ends just above.
    assert!(rows.iter().any(|r| r.text() == "2900"));
    let last: u64 = rows.last().unwrap().text().trim().parse().unwrap_or(0);
    assert!(last >= 2960 && last < 3000, "history ends at {last}");

    // Search anchors deep in history agree with the block's line space.
    c.send(&ClientMsg::Search {
        req_id: 5,
        query: "1500".into(),
        regex: false,
        case_sensitive: false,
        scope: SearchScope::Session(session_id),
        max_results: 10,
    });
    let hit_line = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::SearchResults {
            req_id: 5, matches, ..
        } => matches
            .iter()
            .find(|h| h.line_text == "1500")
            .map(|h| h.line),
        _ => None,
    });
    assert!(hit_line >= out && hit_line < end);
    assert_eq!(
        hit_line,
        out + 1499,
        "line N of seq lands at output_line + N - 1"
    );

    let _ = std::fs::remove_file(&socket);
}

/// A session's environment comes from the fixed base in `helmd::env`, not
/// from whatever the daemon inherited — here, this test process, which
/// we dress up as a tmux-inside-iTerm launcher.
#[test]
fn session_env_is_pristine_not_inherited() {
    std::env::set_var("TMUX", "/tmp/tmux-e2e/default,1,0");
    std::env::set_var("ITERM_SESSION_ID", "w0t0p0:e2e");
    std::env::set_var("HELM_E2E_LEAK", "should-not-reach-the-session");
    let (socket, mut c, mut seen) = start_daemon("env");

    c.send(&ClientMsg::NewSession {
        req_id: 2,
        name: Some("env".into()),
        cwd: None,
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "env | sort; printf 'helm-e2e-env-done\\n'; sleep 5".into(),
        ]),
    });
    let session_id = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Created { req_id: 2, session } => Some(*session),
        _ => None,
    });
    let done = |m: &DaemonMsg| {
        screen_texts(std::slice::from_ref(m), session_id)
            .iter()
            .any(|t| t.contains("helm-e2e-env-done"))
            .then_some(())
    };
    if !seen.iter().any(|m| done(m).is_some()) {
        c.recv_until(10, &mut seen, done);
    }
    // Pull the final screen so every env line is in one snapshot.
    c.send(&ClientMsg::Screen {
        req_id: 3,
        session: session_id,
    });
    let screen = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::Screen {
            req_id: Some(3),
            session,
            screen,
        } if *session == session_id => Some(screen.clone()),
        _ => None,
    });
    // The environment can exceed the 24-row grid. Fetch rows that
    // scrolled into history so required variables are not mistaken for
    // missing merely because they moved above the final snapshot.
    c.send(&ClientMsg::History {
        req_id: 4,
        session: session_id,
        from_line: 0,
        to_line: screen.top_line,
    });
    let history = c.recv_until(5, &mut seen, |m| match m {
        DaemonMsg::History {
            req_id: 4, rows, ..
        } => Some(rows.clone()),
        _ => None,
    });
    let mut lines: Vec<String> = history
        .iter()
        .map(|r| r.text().trim_end().to_string())
        .collect();
    lines.extend(screen.lines.iter().map(|r| r.text().trim_end().to_string()));
    let has = |prefix: &str| lines.iter().any(|l| l.starts_with(prefix));

    assert!(
        !has("TMUX="),
        "TMUX leaked from the daemon's launcher: {lines:?}"
    );
    assert!(
        !has("ITERM_SESSION_ID="),
        "ITERM_SESSION_ID leaked: {lines:?}"
    );
    assert!(
        !has("HELM_E2E_LEAK="),
        "arbitrary launcher env leaked: {lines:?}"
    );
    assert!(
        has(&format!("PATH={}", helmd::env::SYSTEM_PATH)),
        "PATH is not the system base: {lines:?}"
    );
    assert!(has("TERM_PROGRAM=Helm"), "{lines:?}");
    assert!(has("HELM_TTY=/dev/"), "{lines:?}");
    assert!(has("HELM_INTEGRATION=1"), "{lines:?}");
    assert!(has("ZDOTDIR="), "{lines:?}");
    assert!(has("HOME="), "{lines:?}");

    let _ = std::fs::remove_file(&socket);
}
