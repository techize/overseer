//! Getting the connection back after the daemon goes away (a restart, a
//! crash, or a connection dropped because the app fell behind). The app
//! keeps trying with a growing pause until the socket answers, then starts
//! over with a fresh client: a new frame hub, the lists asked for again and
//! new terminal views.

use std::path::PathBuf;
use std::time::Duration;

use overseer_proto::Client;

/// The first pause before trying again.
const FIRST_DELAY: Duration = Duration::from_millis(250);
/// The longest pause between tries.
const MAX_DELAY: Duration = Duration::from_secs(5);

/// The pause before try `attempt` (from 0): 250 ms, doubling, at most 5 s.
pub fn reconnect_delay(attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
    FIRST_DELAY.saturating_mul(factor).min(MAX_DELAY)
}

/// Connects to the daemon at `socket`, trying until it answers. Each try
/// waits its `reconnect_delay` first, so a daemon that is just stopping is
/// not reached again by mistake. Never starts a daemon: that is the
/// LaunchAgent's job (or Jonathan's), not the app's. Runs on the tokio
/// runtime, as `Client::connect` needs it.
pub async fn reconnect(socket: PathBuf) -> Client {
    let mut attempt = 0u32;
    loop {
        tokio::time::sleep(reconnect_delay(attempt)).await;
        match Client::connect(&socket).await {
            Ok(client) => return client,
            Err(e) => tracing::debug!("reconnect try {attempt}: {e}"),
        }
        attempt = attempt.saturating_add(1);
    }
}
