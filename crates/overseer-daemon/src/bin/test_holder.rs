//! The holder as its own binary, for the integration tests:
//! `overseer-test-holder hold <state_dir>`. With
//! `OVERSEER_TEST_HOLDER_VERSION` set it announces that protocol version,
//! standing in for another build's holder. `overseer-test-holder idle`
//! only sleeps for a minute: a live process with a holder's name that
//! serves nothing.

fn main() {
    if std::env::args().nth(1).as_deref() == Some("idle") {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    let version = std::env::var("OVERSEER_TEST_HOLDER_VERSION")
        .ok()
        .and_then(|v| v.parse().ok());
    match version {
        Some(v) => overseer_daemon::holder::main_announcing(v),
        None => overseer_daemon::holder::main(),
    }
}
