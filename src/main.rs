//! Entry point of the audio-output selector applet.
//!
//! The COSMIC panel owns this process' lifetime and launches it from the applet
//! list (see the `.desktop` file's `X-CosmicApplet=true`).
//!
//! When `--cycle` is passed, the binary is being run as a one-shot shortcut
//! client rather than as an applet: it forwards a cycle request to the applet
//! already in the panel (see [`control`]) and exits without starting a second
//! applet.

mod app;
mod audio;
mod control;
mod shortcut;
mod state;

fn main() -> cosmic::iced::Result {
    // A custom shortcut spawns a fresh process; that process must not become an
    // applet of its own. It only delivers the request and leaves.
    if std::env::args().any(|arg| arg == control::CYCLE) {
        control::request_cycle();
        return Ok(());
    }

    // Only the real applet registers the default shortcut, and only the first
    // time it runs; a few small file operations, so startup is not delayed.
    shortcut::ensure_default();

    cosmic::applet::run::<app::AppModel>(())
}
