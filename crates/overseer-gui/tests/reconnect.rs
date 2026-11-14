//! The app reconnects when the daemon restarts: `reconnect` waits for the
//! socket to answer again and hands back a fresh client.

use std::path::{Path, PathBuf};
use std::time::Duration;

use overseer_daemon::{run, DaemonConfig};
use overseer_gui::reconnect::{reconnect, reconnect_delay};
use overseer_gui::FrameHub;
use overseer_proto::{Client, Request, Response};
use tokio::time::timeout;

fn daemon(dir: &Path, socket: &Path) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(run(DaemonConfig {
        socket: socket.to_path_buf(),
        default_command: vec!["/bin/cat".to_string()],
        env: overseer_daemon::EnvSource::Inherit,
        state_dir: dir.join("state"),
        files: overseer_daemon::SettingsFiles::in_dir(dir),
        hook_program: PathBuf::from("/bin/true"),
        // No real beads or Jira.
        patterns: overseer_daemon::locks::targets::default_patterns(),
        settings: overseer_daemon::Config {
            beads_dir: "/nonexistent/overseer-test-beads".into(),
            brain_dir: "/nonexistent/overseer-test-brain".into(),
            jira_command: vec![],
            ..overseer_daemon::Config::default()
        },
    }))
}

#[test]
fn delays_grow_and_stop_growing() {
    assert_eq!(reconnect_delay(0), Duration::from_millis(250));
    assert_eq!(reconnect_delay(1), Duration::from_millis(500));
    assert_eq!(reconnect_delay(2), Duration::from_secs(1));
    assert_eq!(reconnect_delay(5), Duration::from_secs(5));
    assert_eq!(reconnect_delay(u32::MAX), Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_daemon_is_reached_again() {
    let dir = PathBuf::from(format!("/tmp/ovgr-{}-reconnect", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let first = daemon(&dir, &socket);
    let client = Client::connect_or_start(&socket, || Ok(())).await.unwrap();
    let hub = FrameHub::start(&client, tokio::runtime::Handle::current());
    let mut events = hub.take_events().unwrap();

    // The daemon goes away: the app's event stream ends.
    assert_eq!(
        client.request(Request::Shutdown).await.unwrap(),
        Response::Ok
    );
    let _ = timeout(Duration::from_secs(5), first).await;
    timeout(Duration::from_secs(5), async {
        while events.recv().await.is_some() {}
    })
    .await
    .expect("the event stream did not end");

    // Nobody answers for a while, then a new daemon starts.
    let waiting = tokio::spawn(reconnect(socket.clone()));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!waiting.is_finished(), "connected with no daemon running");
    let second = daemon(&dir, &socket);
    let client = timeout(Duration::from_secs(10), waiting)
        .await
        .expect("did not reconnect")
        .unwrap();
    let hub = FrameHub::start(&client, tokio::runtime::Handle::current());
    assert!(hub.take_events().is_some());
    assert!(matches!(
        client.request(Request::ListSlots).await.unwrap(),
        Response::Slots(_)
    ));

    let _ = client.request(Request::Shutdown).await;
    let _ = timeout(Duration::from_secs(5), second).await;
    let _ = std::fs::remove_dir_all(&dir);
}
