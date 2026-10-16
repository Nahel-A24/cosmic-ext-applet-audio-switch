//! Control socket that lets a one-shot `--cycle` invocation reach the running applet.
//!
//! A Wayland client cannot grab a global hotkey, so the shortcut is a
//! cosmic-settings custom shortcut that runs `<binary> --cycle`. That subprocess
//! has no panel icon of its own — the running applet does. If the subprocess
//! applied the mode itself, the action (ALSA/PulseAudio) would still happen, but
//! the applet would never learn about it, so the panel icon would keep showing
//! the previous mode until the user next touched the popup. That icon's one job
//! is to report the current mode, so a stale icon is a real defect, not a
//! cosmetic one. The request is therefore forwarded to the running applet over a
//! unix socket and applied in-process, so the icon, the popup check mark and the
//! internal state all move together.
//!
//! Placement is `$XDG_RUNTIME_DIR`, the per-user directory (already `0700`)
//! Wayland and PulseAudio use for exactly this kind of control surface. Unlike
//! `cosmic-ext-applet-clip-keep`, which solves multi-instance with a random
//! socket name per applet and a sweep, this applet has a single deterministic
//! path: one user runs one panel, and a second instance must not clobber the
//! first (see [`open_listener`]).
//!
//! ## Protocol
//!
//! The client connects and writes a single byte, then closes. The request bytes
//! are deliberately a tiny open set so a future action can be added without
//! redesigning the transport:
//!
//! - [`CYCLE_REQUEST`] (`b'c'`) — advance the output mode.
//!
//! There is no reply. The shortcut only needs fire-and-forget delivery; the
//! applet is the side that owns the visible state.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cosmic::iced::Subscription;
use futures::SinkExt;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

/// Argument that makes the binary forward one cycle request instead of starting
/// the applet. It is the command a cosmic-settings custom shortcut runs.
pub const CYCLE: &str = "--cycle";

/// File name of the control socket inside `$XDG_RUNTIME_DIR`.
const SOCKET_NAME: &str = "cosmic-ext-applet-audio-switch.sock";

/// Request byte asking the running applet to advance the output mode.
///
/// A single byte because the transport carries one command today; the constant
/// exists (rather than a bare `b'c'` at the call site) so the byte value is
/// defined once and named in the protocol above.
const CYCLE_REQUEST: u8 = b'c';

/// Bound on the client's write and on the server's read of the request byte.
///
/// Both sides must be able to give up: the client runs from a hotkey and must
/// not hang, and the server must not stall its accept loop on a peer that
/// connects and then writes nothing.
const TIMEOUT: Duration = Duration::from_millis(250);

/// Pause before retrying after an accept error, so a broken listener does not
/// spin.
const RETRY: Duration = Duration::from_secs(1);

/// Absolute path of the applet's control socket.
///
/// Returns `None` when `XDG_RUNTIME_DIR` is unset (or empty), because that
/// variable is what identifies the per-user runtime directory; guessing a
/// fallback could put the socket somewhere shared or non-existent.
pub fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;

    if runtime.is_empty() {
        return None;
    }

    Some(PathBuf::from(runtime).join(SOCKET_NAME))
}

/// Forwards one cycle request to the running applet.
///
/// Returns whether the request byte was delivered. `main` calls this for the
/// `--cycle` argument; showing a dialog or crashing on failure would be a worse
/// experience than doing nothing, so every failure is logged and the process
/// still exits cleanly.
pub fn request_cycle() -> bool {
    let Some(path) = socket_path() else {
        tracing::warn!("XDG_RUNTIME_DIR is not set, so the running applet cannot be reached");
        return false;
    };

    match deliver(&path, CYCLE_REQUEST) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                socket = %path.display(),
                %error,
                "could not reach the running applet; is it running in the panel?"
            );
            false
        }
    }
}

/// Connects to the control socket and writes one request byte.
fn deliver(path: &Path, request: u8) -> std::io::Result<()> {
    // A plain blocking stream: the client runs once and exits, so there is no
    // runtime to drive an async socket here.
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(&[request])
}

/// The control subscription.
///
/// Yields one `()` per delivered request. The applet maps each item to
/// `Message::Cycle`, so the action runs on the applet's own thread with its own
/// state — which is the whole point of routing the request through the socket.
pub fn subscription() -> Subscription<()> {
    Subscription::run(|| {
        cosmic::iced::stream::channel(
            1,
            |mut emitter: futures::channel::mpsc::Sender<()>| async move {
                let Some(path) = socket_path() else {
                    tracing::warn!(
                        "XDG_RUNTIME_DIR is not set, so the --cycle shortcut will not work"
                    );
                    return;
                };

                let Some(listener) = open_listener(&path) else {
                    return;
                };

                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            if read_request(stream).await == Some(CYCLE_REQUEST) {
                                // The emitter only errors when the subscription is
                                // being torn down, at which point there is nobody
                                // left to cycle for.
                                if emitter.send(()).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            // Do not panic: a dead listener should cost the shortcut,
                            // not the applet. Backing off keeps this loop from
                            // spinning if the socket stays broken.
                            tracing::warn!(%error, "the control socket refused a connection, retrying");
                            tokio::time::sleep(RETRY).await;
                        }
                    }
                }
            },
        )
    })
}

/// Binds the control socket, refusing to clobber a live applet.
///
/// A socket path that already exists may be either a *live* listener (a second
/// applet instance started while the first is running) or a *stale* file left by
/// a crash. The two are told apart by connecting: a successful connection proves
/// something is listening, so the existing applet is kept and this instance
/// steps aside. Only a refused connection means the file is abandoned, and only
/// then is it unlinked before binding.
fn open_listener(path: &Path) -> Option<UnixListener> {
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                tracing::warn!(
                    socket = %path.display(),
                    "another applet is already listening; not replacing it"
                );
                return None;
            }
            Err(error) if is_abandoned(&error) => {
                tracing::debug!(
                    socket = %path.display(),
                    "removing a stale control socket left by a previous run"
                );
                if let Err(error) = fs::remove_file(path) {
                    tracing::warn!(
                        socket = %path.display(),
                        %error,
                        "could not remove a stale control socket"
                    );
                    return None;
                }
            }
            Err(error) => {
                // Some other error (e.g. permissions) makes it unsafe to conclude
                // the file is dead; leave it alone and give up the socket rather
                // than risk stealing a live applet's control path.
                tracing::warn!(
                    socket = %path.display(),
                    %error,
                    "could not tell whether the control socket is live; leaving it alone"
                );
                return None;
            }
        }
    }

    match UnixListener::bind(path) {
        Ok(listener) => {
            tracing::debug!(socket = %path.display(), "listening for the --cycle shortcut");
            Some(listener)
        }
        Err(error) => {
            tracing::warn!(
                socket = %path.display(),
                %error,
                "could not bind the control socket; the --cycle shortcut will not work"
            );
            None
        }
    }
}

/// Reads one request byte from an accepted connection.
///
/// Returns `None` (after logging at debug) when the peer closes without sending
/// or sends nothing within [`TIMEOUT`]. An unknown byte is returned as-is and
/// left for the caller to ignore, so a future action byte can be added without
/// disturbing the running applet; neither case is fatal.
async fn read_request(mut stream: UnixStream) -> Option<u8> {
    let mut byte = [0u8; 1];

    match tokio::time::timeout(TIMEOUT, stream.read_exact(&mut byte)).await {
        Ok(Ok(_)) => Some(byte[0]),
        Ok(Err(error)) => {
            tracing::debug!(%error, "the control connection closed without a request");
            None
        }
        Err(_) => {
            tracing::debug!("the control connection sent no request in time");
            None
        }
    }
}

/// Is this connect error evidence that nothing is listening on the path?
///
/// `ConnectionRefused` is the usual signal for a socket file whose owner is
/// gone; `NotFound` means the path disappeared between the existence check and
/// the connect. Both mean the file is not a live endpoint.
fn is_abandoned(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdListener;

    /// A unique, empty directory under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("audio-switch-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the scratch directory must be writable");
        path
    }

    /// Runs `f` with a Tokio runtime context.
    ///
    /// `open_listener` binds through `tokio::net`, which registers the socket
    /// with a reactor and panics outside a runtime — the same context the
    /// iced executor provides in the running applet. Nothing here is awaited, so
    /// a current-thread runtime is enough.
    fn with_runtime<R>(f: impl FnOnce() -> R) -> R {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime must build");
        let _guard = runtime.enter();
        f()
    }

    #[test]
    fn refuses_to_clobber_a_live_socket() {
        // A second applet instance must step aside instead of stealing the
        // control path from the applet already in the panel.
        let dir = scratch("live");
        let path = dir.join(SOCKET_NAME);
        let live = StdListener::bind(&path).expect("a live listener must bind");

        assert!(
            with_runtime(|| open_listener(&path)).is_none(),
            "a live socket must be left to its owner"
        );
        assert!(path.exists(), "the live socket must not be removed");

        drop(live);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_a_stale_socket() {
        // Binding and dropping leaves the file behind with nothing listening —
        // the state a crash produces. That file must be removed and rebound.
        let dir = scratch("stale");
        let path = dir.join(SOCKET_NAME);
        drop(StdListener::bind(&path).expect("a listener must bind"));

        assert!(
            with_runtime(|| open_listener(&path)).is_some(),
            "a stale socket must be replaced"
        );
        assert!(path.exists(), "a fresh socket must be bound in its place");

        let _ = fs::remove_dir_all(&dir);
    }
}
