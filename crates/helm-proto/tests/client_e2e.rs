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
            if let Some(value) = pred(&msg) {
                return value;
            }
        }
    })
    .await
    .expect("timed out waiting for daemon message")
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_client_lifecycle() {
    let socket = std::env::temp_dir().join(format!("helmd-client-e2e-{}.sock", std::process::id()));
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

    let socket_for_connect = socket.clone();
    let mut conn = tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            match helm_proto::client::connect_unix(&socket_for_connect, "client-e2e") {
                Ok(connection) => return connection,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        panic!("daemon never came up");
    })
    .await
    .unwrap();
    assert!(conn.state.sessions.is_empty());
    assert!(conn.pending.is_empty());

    let client = &conn.client;
    client.attach().unwrap();
    client
        .new_session(
            1,
            Some("typed".into()),
            None,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf typed-client-out; sleep 3".into(),
            ]),
        )
        .unwrap();
    let session = recv_until(&mut conn.events, 5, |message| match message {
        DaemonMsg::Created { req_id: 1, session } => Some(*session),
        _ => None,
    })
    .await;

    recv_until(&mut conn.events, 10, |message| match message {
        DaemonMsg::ScreenDiff {
            session: id, rows, ..
        } if *id == session => rows
            .iter()
            .any(|(_, row)| row.text().contains("typed-client-out"))
            .then_some(()),
        DaemonMsg::Screen {
            session: id,
            screen,
            ..
        } if *id == session => screen
            .lines
            .iter()
            .any(|row| row.text().contains("typed-client-out"))
            .then_some(()),
        _ => None,
    })
    .await;

    client
        .search(2, "typed-client".into(), false, false, SearchScope::All, 10)
        .unwrap();
    recv_until(&mut conn.events, 5, |message| match message {
        DaemonMsg::SearchResults { matches, .. } => matches
            .iter()
            .any(|hit| hit.session == session && hit.line_text.contains("typed-client-out"))
            .then_some(()),
        _ => None,
    })
    .await;

    client.screen(3, session).unwrap();
    let screen = recv_until(&mut conn.events, 5, |message| match message {
        DaemonMsg::Screen {
            req_id: Some(3),
            session: id,
            screen,
        } if *id == session => Some(screen.clone()),
        _ => None,
    })
    .await;
    assert!(screen
        .lines
        .iter()
        .any(|row| row.text().contains("typed-client-out")));
    assert_eq!(screen.lines.len(), screen.rows as usize);

    client.history(4, session, 0, u64::MAX).unwrap();
    recv_until(&mut conn.events, 5, |message| match message {
        DaemonMsg::History {
            req_id: 4,
            session: id,
            ..
        } if *id == session => Some(()),
        _ => None,
    })
    .await;

    client.kill_session(session).unwrap();
    recv_until(&mut conn.events, 5, |message| match message {
        DaemonMsg::TreeChanged { state }
            if state.sessions.iter().all(|item| item.id != session) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    let _ = std::fs::remove_file(&socket);
}
