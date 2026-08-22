//! Typed-client e2e: `HelmdClient` against an in-process daemon.
//! (Run with `--features client`; the transport-level equivalent lives
//! in helmd's own e2e test.)

use std::time::Duration;

use helm_proto::{DaemonMsg, SearchScope};

async fn recv_until<T>(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<DaemonMsg>,
    secs: u64,
    mut pred: impl FnMut(&DaemonMsg) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let msg = events.recv().await.expect("daemon event stream closed");
            if let Some(v) = pred(&msg) {
                return v;
            }
        }
    })
    .await
    .expect("timed out waiting for daemon message")
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_client_lifecycle() {
    let socket =
        std::env::temp_dir().join(format!("helmd-client-e2e-{}.sock", std::process::id()));
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

    // Retry connect until the daemon binds. The client API is blocking
    // by design (one implementation for sockets and SSH pipes) — from
    // async code it runs under spawn_blocking, exactly as the app does.
    let socket_for_connect = socket.clone();
    let mut conn = tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            match helm_proto::client::connect_unix(&socket_for_connect, "client-e2e") {
                Ok(c) => return c,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        panic!("daemon never came up");
    })
    .await
    .unwrap();
    assert!(conn.state.workspaces.is_empty());
    assert!(conn.pending.is_empty());

    let client = &conn.client;
    client.attach().unwrap();
    client.new_workspace(1, Some("typed".into())).unwrap();
    let ws = recv_until(&mut conn.events, 5, |m| match m {
        DaemonMsg::Created { req_id: 1, workspace, .. } => Some(*workspace),
        _ => None,
    })
    .await;

    client
        .new_window(
            2,
            ws,
            None,
            None,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf typed-client-out; sleep 3".into(),
            ]),
        )
        .unwrap();
    let pane = recv_until(&mut conn.events, 5, |m| match m {
        DaemonMsg::Created { req_id: 2, pane, .. } => *pane,
        _ => None,
    })
    .await;

    // The live grid arrives as screen diffs (or a full screen).
    recv_until(&mut conn.events, 10, |m| match m {
        DaemonMsg::ScreenDiff { pane: p, rows, .. } if *p == pane => rows
            .iter()
            .any(|(_, row)| row.text().contains("typed-client-out"))
            .then_some(()),
        DaemonMsg::Screen { pane: p, screen, .. } if *p == pane => screen
            .lines
            .iter()
            .any(|row| row.text().contains("typed-client-out"))
            .then_some(()),
        _ => None,
    })
    .await;

    // Default window name derives from the spawned command.
    // (argv[0] basename of "/bin/sh" → "sh")
    client.search(3, "typed-client".into(), false, false, SearchScope::All, 10).unwrap();
    recv_until(&mut conn.events, 5, |m| match m {
        DaemonMsg::SearchResults { matches, .. } => matches
            .iter()
            .any(|hit| hit.pane == pane && hit.line_text.contains("typed-client-out"))
            .then_some(()),
        _ => None,
    })
    .await;

    // A fresh client paints from the model: the full screen still holds
    // the output (exact reattach, no byte replay).
    client.screen(4, pane).unwrap();
    let screen = recv_until(&mut conn.events, 5, |m| match m {
        DaemonMsg::Screen { req_id: Some(4), pane: p, screen } if *p == pane => Some(screen.clone()),
        _ => None,
    })
    .await;
    assert!(screen.lines.iter().any(|row| row.text().contains("typed-client-out")));
    assert_eq!(screen.lines.len(), screen.rows as usize);

    client.history(5, pane, 0, u64::MAX).unwrap();
    recv_until(&mut conn.events, 5, |m| match m {
        DaemonMsg::History { req_id: 5, pane: p, .. } if *p == pane => Some(()),
        _ => None,
    })
    .await;

    let _ = std::fs::remove_file(&socket);
}
