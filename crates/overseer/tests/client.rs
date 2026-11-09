use std::time::Duration;

use overseer_daemon::{run, DaemonConfig};
use overseer_proto::{Client, Frame, Request, Response};
use tokio::time::timeout;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");

#[tokio::test]
async fn attach_snapshot_precedes_live_output() {
    let dir = std::env::temp_dir().join(format!("ovcl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let task = tokio::spawn(run(DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
    }));
    let client = Client::connect_or_start(&socket, || Ok(())).await.unwrap();
    let mut rx = client.subscribe();

    let r = client
        .request(Request::StartSlot {
            id: 1,
            label: "t".into(),
            cwd: "/tmp".into(),
            command: None,
        })
        .await
        .unwrap();
    assert_eq!(r, Response::Ok);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        client.request(Request::Attach { id: 1 }).await.unwrap(),
        Response::Ok
    );
    client.send_input(1, b"ping\r").await.unwrap();

    let mut seen = Vec::new();
    timeout(Duration::from_secs(5), async {
        while let Some(f) = rx.recv().await {
            if let Frame::Output { data, .. } = f {
                seen.extend_from_slice(&data);
                if String::from_utf8_lossy(&seen).contains("ECHO:ping") {
                    break;
                }
            }
        }
    })
    .await
    .expect("timed out");
    let text = String::from_utf8_lossy(&seen);
    let ready = text.find("READY").expect("snapshot has READY");
    assert!(ready < text.find("ECHO:ping").unwrap());

    let _ = client.request(Request::Shutdown).await;
    let _ = timeout(Duration::from_secs(2), task).await;
    let _ = std::fs::remove_dir_all(&dir);
}
