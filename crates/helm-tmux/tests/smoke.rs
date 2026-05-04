//! Integration smoke tests against a real local tmux server.
//!
//! Skipped automatically if `tmux` isn't on PATH so machines without tmux
//! don't spuriously fail. Each test creates its own uniquely-named session
//! after attaching, runs assertions against it, and tears it down. The
//! tests share the user's running tmux server (rather than spinning up a
//! private one) — keeps things simple, and the unique-uuid session names
//! prevent collisions.

use helm_tmux::{Notification, TmuxClient};
use std::process::Command;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use uuid::Uuid;

fn require_tmux() {
    let ok = Command::new("tmux")
        .arg("-V")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !ok {
        panic!(
            "tmux not on PATH — install with `brew install tmux`. \
             (Tests must fail loudly when tmux is missing rather than silently \
             passing as a no-op.)"
        );
    }
}

/// Spin up a control client and create a fresh, uniquely-named session for
/// the test to target. The control client itself attaches to whatever the
/// user already had running (post-attach-first refactor); the test session
/// is a sibling that we explicitly own and clean up.
async fn fresh_session() -> (String, TmuxClient, UnboundedReceiver<Notification>) {
    require_tmux();
    let (client, events) = TmuxClient::spawn_local("helm-test-fallback")
        .await
        .expect("spawn tmux -CC");
    let session = format!("helm-test-{}", Uuid::new_v4().simple());
    client
        .send_command(format!("new-session -d -s '{}'", session))
        .await
        .expect("new-session");
    // Switch the control client to our test session so notifications like
    // `%window-add` (which only fire for the *current* session) are
    // observable in tests targeting it.
    client
        .send_command(format!("switch-client -t '{}'", session))
        .await
        .expect("switch-client");
    (session, client, events)
}

async fn cleanup(client: &TmuxClient, session: &str) {
    let _ = client
        .send_command(format!("kill-session -t '{}'", session))
        .await;
}

#[tokio::test]
async fn round_trip_display_message() {
    let (session, client, _events) = fresh_session().await;
    // Target our session explicitly so the response doesn't depend on
    // whichever session the control client happened to attach to.
    let res = client
        .send_command(format!("display-message -t '{}' -p '#{{session_name}}'", session))
        .await
        .expect("display-message");
    assert!(
        res.trim() == session,
        "expected session name {session:?}, got {res:?}"
    );
    cleanup(&client, &session).await;
}

#[tokio::test]
async fn new_window_shows_in_list() {
    let (session, client, _events) = fresh_session().await;

    // Create the test window IN the test session; otherwise the global
    // -a listing might be picking up windows from the user's other sessions.
    client
        .new_window(Some(&session), Some("api-server"))
        .await
        .expect("new-window");

    let listing = client
        .list_windows("#{session_name}|#{window_name}")
        .await
        .expect("list-windows");
    let mine: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{}|", session)))
        .collect();
    assert!(
        mine.iter().any(|n| *n == "api-server"),
        "expected api-server in {mine:?}"
    );

    cleanup(&client, &session).await;
}

#[tokio::test]
async fn window_add_notification_fires() {
    let (session, client, mut events) = fresh_session().await;

    // Drain any startup notifications first.
    while let Ok(_) = tokio::time::timeout(Duration::from_millis(100), events.recv()).await {}

    client
        .new_window(Some(&session), Some("logs"))
        .await
        .expect("new-window");

    // Wait up to 2s for the %window-add notification.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_add = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Some(Notification::WindowAdded { .. })) => {
                saw_add = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    assert!(saw_add, "did not see %window-add within 2s");

    cleanup(&client, &session).await;
}

#[tokio::test]
async fn send_keys_hex_round_trip() {
    let (session, client, _events) = fresh_session().await;

    // Query the session's first pane id rather than assuming index 0
    // — users with `base-index 1` / `pane-base-index 1` set break
    // any hard-coded `0.0` target.
    let pane_target = client
        .send_command(format!(
            "display-message -t '{}' -p '#{{pane_id}}'",
            session
        ))
        .await
        .expect("display-message pane_id")
        .trim()
        .to_string();

    client
        .send_keys(&pane_target, b"echo helm\n")
        .await
        .expect("send-keys");

    // Give the shell a moment to redraw.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let buf = client
        .send_command(format!("capture-pane -p -t {}", pane_target))
        .await
        .expect("capture-pane");
    assert!(
        buf.contains("helm"),
        "expected 'helm' in pane buffer, got: {buf:?}"
    );

    cleanup(&client, &session).await;
}
