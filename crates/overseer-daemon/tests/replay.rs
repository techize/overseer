//! What a client replays on attach: terminal modes survive the ring
//! wrapping (I2), and old terminal queries are not replayed (I1).

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use overseer_daemon::slot::{Slot, RING_CAP};
use overseer_term::TermBridge;
use tokio::sync::broadcast;
use tokio::time::timeout;

/// Prints 6 MiB of lines made of `ch`, more than the 4 MiB ring holds.
fn flood(ch: char) -> String {
    format!(
        "awk 'BEGIN {{ s = sprintf(\"%1023s\", \"\"); gsub(/ /, \"{ch}\", s); for (i = 0; i < 6 * 1024; i++) print s }}'"
    )
}

/// Runs `script` under /bin/sh in a slot and waits until its output stream
/// has ended, so the ring holds everything it printed.
async fn run_to_end(id: u8, script: &str) -> Slot {
    let (tx, mut rx) = broadcast::channel::<Bytes>(1024);
    let slot = Slot::spawn(
        id,
        "replay".into(),
        Path::new("/tmp"),
        &["/bin/sh".into(), "-c".into(), script.into()],
        80,
        24,
        tx,
    )
    .unwrap();
    timeout(Duration::from_secs(30), async {
        loop {
            if let Err(broadcast::error::RecvError::Closed) = rx.recv().await {
                break;
            }
        }
    })
    .await
    .expect("output did not end");
    slot
}

/// Replays `snapshot` into a fresh 80x24 terminal.
fn replay(snapshot: &[u8]) -> TermBridge {
    let mut b = TermBridge::new(80, 24, 1000);
    b.feed(snapshot);
    b
}

fn screen_text(b: &TermBridge) -> String {
    let snap = b.snapshot();
    let mut s = String::new();
    for row in 0..snap.rows {
        for col in 0..snap.cols {
            s.push(snap.cell(row, col).map_or(' ', |c| c.ch));
        }
        s.push('\n');
    }
    s
}

#[tokio::test]
async fn modes_set_before_the_ring_wraps_survive_attach() {
    let script = format!(
        "printf '\\033[?2004h\\033[?1004h\\033[?1h\\033[?1006h\\033[?1002h'; {}; printf 'DONE\\n'",
        flood('x')
    );
    let slot = run_to_end(0, &script).await;
    let (snap, _) = slot.attach();
    assert!(snap.len() >= RING_CAP, "the ring wrapped: {}", snap.len());
    let b = replay(&snap);
    let m = b.modes();
    assert!(m.bracketed_paste, "bracketed paste lost: {m:?}");
    assert!(m.focus_reporting, "focus reporting lost: {m:?}");
    assert!(m.app_cursor, "application cursor lost: {m:?}");
    assert!(m.sgr_mouse, "SGR mouse lost: {m:?}");
    assert_eq!(m.mouse, overseer_term::mouse::MouseMode::Drag, "{m:?}");
    assert!(!m.alt_screen);
}

#[tokio::test]
async fn alt_screen_left_after_the_wrap_replays_onto_the_right_screen() {
    // Entered the alt screen long ago (the 1049h is evicted), drew a lot on
    // it, then left it and printed to the main screen.
    let script = format!(
        "printf '\\033[?1049h'; {}; printf '\\033[?1049lMAIN-AFTER\\n'",
        flood('x')
    );
    let slot = run_to_end(1, &script).await;
    let (snap, _) = slot.attach();
    let b = replay(&snap);
    assert!(!b.modes().alt_screen);
    let text = screen_text(&b);
    assert!(text.contains("MAIN-AFTER"), "{text}");
    // The flood was drawn on the alt screen, so none of it is on main.
    assert!(!text.contains('x'), "alt screen output leaked:\n{text}");
}

#[tokio::test]
async fn alt_screen_entered_after_the_wrap_stays_clean() {
    let script = format!("{}; printf '\\033[?1049hALT-NOW'", flood('y'));
    let slot = run_to_end(2, &script).await;
    let (snap, _) = slot.attach();
    let b = replay(&snap);
    assert!(b.modes().alt_screen);
    let text = screen_text(&b);
    assert!(text.contains("ALT-NOW"), "{text}");
    assert!(!text.contains('y'), "main screen output on alt:\n{text}");
}

#[tokio::test]
async fn old_queries_are_not_replayed_but_live_ones_arrive() {
    let (tx, mut rx) = broadcast::channel::<Bytes>(1024);
    let script =
        "printf 'A\\033[c\\033[6n\\033]11;?\\007\\033[14tB\\n'; read x; printf 'L\\033[c'; sleep 5";
    let slot = Slot::spawn(
        3,
        "q".into(),
        Path::new("/tmp"),
        &["/bin/sh".into(), "-c".into(), script.into()],
        80,
        24,
        tx,
    )
    .unwrap();
    let mut seen = Vec::new();
    timeout(Duration::from_secs(5), async {
        while !String::from_utf8_lossy(&seen).contains("B") {
            seen.extend_from_slice(&rx.recv().await.unwrap());
        }
    })
    .await
    .expect("no output");
    let (snap, live) = slot.attach();
    let text = String::from_utf8_lossy(&snap).into_owned();
    assert!(text.contains("AB"), "{text:?}");
    assert!(!text.contains('\x1b'), "query left in snapshot: {text:?}");
    let mut b = replay(&snap);
    assert!(b.take_pty_writes().is_empty(), "replay answered a query");

    slot.write(b"go\r").unwrap();
    let mut live = live.unwrap();
    let mut got = Vec::new();
    timeout(Duration::from_secs(5), async {
        while !got.ends_with(b"\x1b[c") {
            got.extend_from_slice(&live.recv().await.unwrap());
        }
    })
    .await
    .expect("live query did not arrive");
    b.feed(&got);
    assert!(!b.take_pty_writes().is_empty(), "live query unanswered");
    slot.kill();
}
