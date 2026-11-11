//! N11: re-attaching a slot on the same connection (a view recreated for the
//! same slot) must deliver exactly the new snapshot followed by live output,
//! never stale live output from the earlier attach.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::{run, DaemonConfig};
use overseer_gui::FrameHub;
use overseer_proto::{Client, Request, Response};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");

async fn attach(hub: &FrameHub, client: &Client, gen: u64) {
    assert_eq!(hub.attach(client, 1, gen).await.unwrap(), Response::Ok);
}

/// Reads `out` until it holds at least `want` bytes.
async fn collect(out: &mut mpsc::UnboundedReceiver<Bytes>, want: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    timeout(Duration::from_secs(20), async {
        while buf.len() < want {
            match out.recv().await {
                Some(d) => buf.extend_from_slice(&d),
                None => panic!("route closed after {} bytes", buf.len()),
            }
        }
    })
    .await
    .expect("timed out collecting output");
    buf
}

/// Checks that the complete lines of `buf` (after the first, which the
/// snapshot may cut) are consecutive integers. Returns how many were checked.
fn assert_consecutive(buf: &[u8]) -> usize {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.split('\n');
    lines.next(); // possibly partial
    let mut prev: Option<u64> = None;
    let mut checked = 0;
    let all: Vec<&str> = lines.collect();
    // The last piece may be partial too.
    for line in &all[..all.len().saturating_sub(1)] {
        let n: u64 = line
            .trim_end_matches('\r')
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {line:?}"));
        if let Some(p) = prev {
            assert_eq!(n, p + 1, "stream jumped from {p} to {n}");
        }
        prev = Some(n);
        checked += 1;
    }
    checked
}

struct Setup {
    dir: std::path::PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: Arc<Client>,
    hub: FrameHub,
}

/// A daemon in a temp dir with slot 1 printing endless numbered lines, so
/// any replayed or stale chunk shows as a jump.
async fn setup(name: &str) -> Setup {
    let dir = std::path::PathBuf::from(format!("/tmp/ovgr-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let task = tokio::spawn(run(DaemonConfig {
        socket: socket.clone(),
        default_command: vec![SCRIPT.to_string()],
        env: overseer_daemon::EnvSource::Inherit,
        state_dir: dir.join("state"),
        hook_program: std::path::PathBuf::from("/bin/true"),
        settings: overseer_daemon::Config::default(),
    }));
    let client = Arc::new(Client::connect_or_start(&socket, || Ok(())).await.unwrap());
    let hub = FrameHub::start(&client, tokio::runtime::Handle::current());
    let start = Request::StartSlot {
        id: 1,
        label: "t".into(),
        cwd: "/tmp".into(),
        command: Some(vec!["/usr/bin/seq".into(), "1".into(), "1000000000".into()]),
        prompt: None,
    };
    assert_eq!(client.request(start).await.unwrap(), Response::Ok);
    Setup {
        dir,
        task,
        client,
        hub,
    }
}

async fn teardown(s: Setup) {
    let _ = s.client.request(Request::Shutdown).await;
    let _ = timeout(Duration::from_secs(2), s.task).await;
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattach_delivers_only_the_new_snapshot_and_live_output() {
    let s = setup("seq").await;
    let (client, hub) = (&s.client, &s.hub);

    // First view attaches and sees live output.
    let (gen_a, mut out_a) = hub.route(1);
    attach(hub, client, gen_a).await;
    collect(&mut out_a, 64 * 1024).await;

    // Recreate the view several times while output is streaming at full speed.
    for round in 0..5 {
        let (gen_b, mut out_b) = hub.route(1);
        attach(hub, client, gen_b).await;
        let buf = collect(&mut out_b, 512 * 1024).await;
        let checked = assert_consecutive(&buf);
        assert!(checked > 1000, "round {round}: only {checked} lines");
    }

    teardown(s).await;
}

/// A superseded view whose Attach is still pending (its pump was in an
/// earlier round trip when the newer view replaced it) must not send it:
/// the daemon would restart the stream and replay a second snapshot into
/// the newer, already live route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn superseded_attach_is_never_sent() {
    let s = setup("late").await;
    let (client, hub) = (&s.client, &s.hub);

    let (gen_old, _out_old) = hub.route(1);
    let (gen_new, mut out_new) = hub.route(1);
    attach(hub, client, gen_new).await;
    let mut buf = collect(&mut out_new, 64 * 1024).await;

    // The old view's Attach arrives late, after the newer one went live.
    let late = hub.attach(client, 1, gen_old).await;

    // The newer route keeps one unbroken stream: no second snapshot.
    buf.extend(collect(&mut out_new, 512 * 1024).await);
    assert!(assert_consecutive(&buf) > 1000);
    let err = late.unwrap_err();
    assert!(err.is::<overseer_proto::client::Cancelled>(), "{err:#}");

    // Same after the old view is dropped (unroute, then a late Attach).
    assert!(!hub.unroute(1, gen_old));
    assert!(hub.attach(client, 1, gen_old).await.is_err());
    teardown(s).await;
}
