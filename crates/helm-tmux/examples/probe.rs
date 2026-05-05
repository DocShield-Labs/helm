//! Probe: spawn tmux, exercise window operations, dump every notification
//! that comes back. Helps verify the actual protocol format.

use helm_tmux::{Notification, TmuxClient};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

async fn drain(label: &str, ms: u64, events: &mut UnboundedReceiver<Notification>) {
    println!("--- {label} ---");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(n)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            println!("  {n:?}");
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("[1] spawning tmux -CC new-session -A -s helm-probe");
    let (client, mut events) =
        match tokio::time::timeout(Duration::from_secs(5), TmuxClient::spawn_local("helm-probe"))
            .await
        {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                eprintln!("spawn error: {e}");
                return;
            }
            Err(_) => {
                eprintln!("spawn timeout");
                return;
            }
        };
    println!("[1] spawned");

    drain("initial", 500, &mut events).await;

    println!("[2] new-window -n test1");
    let _ = client.new_window(None, Some("test1"), None).await;
    drain("after new-window test1", 500, &mut events).await;

    println!("[3] new-window -n test2");
    let _ = client.new_window(None, Some("test2"), None).await;
    drain("after new-window test2", 500, &mut events).await;

    println!("[4] select-window -t @0  (back to first)");
    let _ = client.select_window("@0").await;
    drain("after select-window @0", 500, &mut events).await;

    println!("[5] rename-window @0 renamed-first");
    let _ = client.rename_window("@0", "renamed-first").await;
    drain("after rename-window @0", 500, &mut events).await;

    println!("[6] kill-session");
    let _ = client
        .send_command("kill-session -t helm-probe")
        .await;
    println!("[done]");
}
