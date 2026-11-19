//! The holder as its own binary, for the integration tests:
//! `overseer-test-holder hold <state_dir>`. With
//! `OVERSEER_TEST_HOLDER_VERSION` set it announces that protocol version,
//! standing in for another build's holder. `overseer-test-holder idle`
//! only sleeps for a minute: a live process with a holder's name that
//! serves nothing. With `OVERSEER_TEST_HOLDER_LATE=<socket>` it first
//! holds `<socket>` bound but not listening for a moment, as between a
//! holder's bind and its listen.

fn main() {
    if std::env::args().nth(1).as_deref() == Some("idle") {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    if let Some(socket) = std::env::var_os("OVERSEER_TEST_HOLDER_LATE") {
        bound_not_listening(std::path::Path::new(&socket));
    }
    let version = std::env::var("OVERSEER_TEST_HOLDER_VERSION")
        .ok()
        .and_then(|v| v.parse().ok());
    match version {
        Some(v) => overseer_daemon::holder::main_announcing(v),
        None => overseer_daemon::holder::main(),
    }
}

/// Binds `socket` without listening, so connects are refused, then lets it
/// go after a moment.
fn bound_not_listening(socket: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Some(dir) = socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // SAFETY: a plain socket, bound to a path that fits sun_path, and
    // closed below.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path = socket.as_os_str().as_bytes();
        for (dst, src) in addr.sun_path.iter_mut().zip(path) {
            *dst = *src as libc::c_char;
        }
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        libc::close(fd);
    }
    let _ = std::fs::remove_file(socket);
}
