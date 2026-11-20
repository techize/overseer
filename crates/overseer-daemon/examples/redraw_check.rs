//! Does a resize one column narrower and back make the installed Claude
//! Code redraw its whole screen? Starts a real `claude` (no prompt is ever
//! sent), then compares the live screen with one rebuilt from only the tail
//! of the output ring plus the redraw a resize provokes.
//!
//! Usage: redraw_check <trusted-folder> [tail-bytes, default 2048]

use std::path::Path;
use std::time::{Duration, Instant};

use overseer_daemon::relay::screen::{input_box, InputBox};
use overseer_daemon::slot::Slot;
use overseer_term::{Snapshot, TermBridge};
use tokio::sync::broadcast;

const COLS: u16 = 120;
const ROWS: u16 = 40;

struct KillOnDrop(Slot);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.kill();
    }
}

fn rows_of(snap: &Snapshot) -> Vec<String> {
    (0..snap.rows)
        .map(|r| {
            let line: String = snap.cells[r * snap.cols..(r + 1) * snap.cols]
                .iter()
                .map(|c| c.ch)
                .collect();
            line.trim().to_string()
        })
        .collect()
}

fn show(title: &str, rows: &[String]) {
    println!("--- {title} ---");
    for (i, r) in rows.iter().enumerate() {
        println!("{i:2}|{r}");
    }
}

#[tokio::main]
async fn main() {
    let folder = std::env::args()
        .nth(1)
        .expect("usage: redraw_check <folder>");
    let keep: usize = std::env::args()
        .nth(2)
        .map_or(2048, |v| v.parse().expect("tail bytes"));
    let (tx, mut rx) = broadcast::channel(4096);
    let slot = Slot::spawn(
        1,
        "redraw".into(),
        Path::new(&folder),
        &["claude".to_string()],
        COLS,
        ROWS,
        tx,
    )
    .expect("spawn claude");
    let slot = KillOnDrop(slot);

    let start = Instant::now();
    loop {
        if input_box(&slot.0.screen().0) != InputBox::NotFound {
            break;
        }
        if start.elapsed() > Duration::from_secs(30) {
            show("screen at timeout", &rows_of(&slot.0.screen().0));
            println!("BLOCKED: no prompt box within 30s");
            std::process::exit(2);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Let the startup draw settle.
    tokio::time::sleep(Duration::from_secs(2)).await;
    while rx.try_recv().is_ok() {}

    let recorded = rows_of(&slot.0.screen().0);
    let ring = slot.0.snapshot();
    let tail = &ring[ring.len().saturating_sub(keep)..];
    println!("ring {} bytes, using last {}", ring.len(), tail.len());
    let mut fresh = TermBridge::new(COLS as usize, ROWS as usize, 0);
    fresh.feed(tail);
    let tail_only = rows_of(&fresh.snapshot());
    println!("tail alone already matches: {}", tail_only == recorded);
    let mut redraw_bytes = 0usize;

    slot.0.resize(COLS - 1, ROWS).expect("resize narrower");
    tokio::time::sleep(Duration::from_millis(300)).await;
    slot.0.resize(COLS, ROWS).expect("resize back");

    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(b)) => {
                redraw_bytes += b.len();
                fresh.feed(&b)
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => println!("lagged"),
            _ => break,
        }
    }

    println!("bytes after resize: {redraw_bytes}");
    let rebuilt = rows_of(&fresh.snapshot());
    if rebuilt == recorded {
        println!("PASS");
    } else {
        println!("FAIL");
        show("recorded (live)", &recorded);
        show("rebuilt (tail + resize redraw)", &rebuilt);
    }
    drop(slot);
}
