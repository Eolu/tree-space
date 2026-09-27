//! Single-instance IPC for tree-space.
//!
//! Launching `tree-space` while an instance is already running must hand the
//! new invocation's intent to that instance (toggle visibility, open a pane,
//! choose a dock side) rather than starting a second panel. GApplication's own
//! single-instance machinery can't carry our `--side` flag or directory
//! arguments through the freshly-launched process cleanly, so we use a small
//! Unix domain socket under the runtime directory:
//!
//! * The first instance binds the socket and runs an accept thread that
//!   decodes each incoming line into a [`Command`] and hands it to the app.
//! * Later instances try to connect; on success they write their serialized
//!   [`Command`] and exit without ever touching the GUI.
//! * A stale socket (crashed instance) is detected by a failed connect and
//!   removed before re-binding.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::cmd::Command;

/// The socket file name below the runtime directory.
const SOCKET_NAME: &str = "tree-space.sock";

/// Path of the instance socket for the current environment.
///
/// `$XDG_RUNTIME_DIR` is present for any user session (in particular any
/// Wayland session); `/tmp` is only a fallback for bare environments.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(SOCKET_NAME)
}

/// Try to deliver `cmd` to a running instance. Returns `true` when a server
/// accepted the request; `false` when no server is listening.
pub fn deliver(cmd: &Command) -> bool {
    deliver_at(&socket_path(), cmd)
}

/// Deliver `cmd` to the server listening on `path`.
pub fn deliver_at(path: &Path, cmd: &Command) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let line = format!("{}\n", cmd.encode());
    if stream.write_all(line.as_bytes()).is_err() || stream.flush().is_err() {
        return false;
    }
    true
}

/// Bind the instance socket as the (single) server.
///
/// If the path is already in use, a connect is attempted to distinguish a live
/// server (in which case we must not become a second instance — the socket is
/// left alone and the error returned) from a stale socket left by a crashed
/// instance (which is removed and the bind retried).
pub fn bind() -> std::io::Result<UnixListener> {
    bind_at(&socket_path())
}

/// Bind the instance socket at an explicit path (see [`bind`]).
pub fn bind_at(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                // A live server holds the socket.
                return Err(err);
            }
            // Stale socket from a crashed instance: clean up and retry.
            let _ = std::fs::remove_file(path);
            UnixListener::bind(path)
        }
        Err(err) => Err(err),
    }
}

/// Spawn a thread that accepts instance connections and forwards every decoded
/// [`Command`] to `deliver` (which must hop onto the UI thread itself, e.g. via
/// `MainContext::invoke`).
pub fn spawn_listener(listener: UnixListener, deliver: impl Fn(Command) + Send + 'static) {
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(stream) = connection else { continue };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            if let Some(cmd) = Command::decode(line.trim_end()) {
                deliver(cmd);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn deliver_and_listener_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let listener = bind_at(&path).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_listener(listener, move |cmd| tx.send(cmd).unwrap());

        let expected = Command {
            side: Some(crate::config::PanelSide::Right),
            roots: vec![PathBuf::from("/home/eolu/x")],
            ..Command::default()
        };
        assert!(deliver_at(&path, &expected));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(expected));
    }

    #[test]
    fn deliver_fails_without_a_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        assert!(!deliver_at(&path, &Command::default()));
    }

    #[test]
    fn deliver_toggle_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let listener = bind_at(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        spawn_listener(listener, move |cmd| tx.send(cmd).unwrap());
        assert!(deliver_at(&path, &Command::default()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Command::default()));
    }

    #[test]
    fn bind_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        // A leftover socket from a crashed instance (no one listening).
        std::fs::write(&path, b"stale").unwrap();
        let listener = bind_at(&path).unwrap();
        assert!(listener.local_addr().is_ok());
    }

    #[test]
    fn bind_refuses_when_a_live_server_holds_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let _server = bind_at(&path).unwrap();
        // Connecting succeeds because the server is listening, so bind must not
        // clobber the socket.
        assert!(bind_at(&path).is_err());
    }
}