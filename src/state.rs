//! Persistence of the last successfully applied output mode.
//!
//! The applet starts at [`OutputMode::Auto`](crate::audio::OutputMode::Auto)
//! unless it can recover the mode the user picked in a previous session. This
//! module stores that choice in a plain-text file so a reboot (or an applet
//! restart) can re-apply it instead of leaving the driver's boot-time state in
//! place.
//!
//! Only a *successfully applied* mode is ever written: the file records an
//! intent the hardware actually honoured, not just a button the user pressed.
//! [`save`] is therefore best-effort and never panics — losing the file costs
//! one restore on the next start, while failing to start costs the panel icon.
//!
//! The state directory resolution mirrors [`crate::shortcut`]'s flag file (both
//! live under the same app directory, see [`state_dir`]); the flag file name is
//! kept there because it is that module's private detail.

use std::fs;
use std::path::{Path, PathBuf};

use crate::audio::OutputMode;

/// Directory name the applet owns inside the state directory.
///
/// XDG puts per-application state under `$XDG_STATE_HOME` (or
/// `$HOME/.local/state`), and this applet is its own application, so it gets its
/// own directory rather than writing to a shared location.
const STATE_DIR: &str = "cosmic-ext-applet-audio-switch";

/// File holding the last applied mode, one token per file.
const MODE_FILE: &str = "mode";

/// The user's home directory, from `HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The applet's state directory, or `None` when it cannot be resolved.
///
/// `XDG_STATE_HOME` wins when set and non-empty; otherwise the XDG default
/// `$HOME/.local/state` is used. This is the shared resolution both this module
/// and the shortcut flag rely on, so the two never drift apart.
pub(crate) fn state_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local").join("state")))?;

    Some(state.join(STATE_DIR))
}

/// Absolute path of the mode file, or `None` when it cannot be built.
pub(crate) fn mode_path() -> Option<PathBuf> {
    Some(state_dir()?.join(MODE_FILE))
}

/// Reads a mode from `path`.
///
/// Every failure — a missing file, an unreadable file, an unknown token — is
/// `None`, never an error: the caller interprets "nothing persisted" the same
/// way in all of those cases, and must never fail to start because of this file.
/// Surrounding whitespace and the trailing newline [`save_to`] writes are
/// trimmed before the token is matched.
pub(crate) fn load_from(path: &Path) -> Option<OutputMode> {
    let text = fs::read_to_string(path).ok()?;
    OutputMode::from_token(text.trim())
}

/// Writes `mode` to `path`, creating the parent directories as needed.
///
/// The token is written with a trailing newline so the file is a normal text
/// file; [`load_from`] tolerates it. Returns the I/O error so tests can assert
/// on it; the production wrapper [`save`] logs and moves on.
pub(crate) fn save_to(path: &Path, mode: OutputMode) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    fs::write(path, format!("{}\n", mode.token()))
}

/// Reads the persisted mode, or `None` when there is none.
pub(crate) fn load() -> Option<OutputMode> {
    load_from(&mode_path()?)
}

/// Persists `mode` on a best-effort basis.
///
/// A failure is logged and swallowed: the applet keeps working, and the worst
/// case is that the next start does not restore this choice. It must never
/// panic, because the user's click already succeeded by the time this runs.
pub(crate) fn save(mode: OutputMode) {
    let Some(path) = mode_path() else {
        tracing::warn!("could not resolve the state path; the output mode was not persisted");
        return;
    };

    if let Err(error) = save_to(&path, mode) {
        tracing::warn!(path = %path.display(), %error, "could not persist the output mode");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique, empty directory under the system temp dir.
    ///
    /// Tests use the path-parameterised functions below, so no environment
    /// variable is read (or mutated) and the user's real state is never touched.
    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("audio-switch-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the scratch directory must be writable");
        path
    }

    /// The mode file under a scratch directory, at the layout the code targets.
    fn mode_file(dir: &Path) -> PathBuf {
        dir.join(STATE_DIR).join(MODE_FILE)
    }

    #[test]
    fn missing_file_loads_none() {
        let dir = scratch("missing");
        assert_eq!(load_from(&mode_file(&dir)), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_every_known_token() {
        let dir = scratch("known");
        let path = mode_file(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for mode in [
            OutputMode::Speakers,
            OutputMode::Headphones,
            OutputMode::Auto,
        ] {
            fs::write(&path, format!("{}\n", mode.token())).unwrap();
            assert_eq!(load_from(&path), Some(mode));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_token_loads_none() {
        let dir = scratch("unknown");
        let path = mode_file(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "surround\n").unwrap();

        // A format from a different version must not be mistaken for a mode.
        assert_eq!(load_from(&path), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let dir = scratch("whitespace");
        let path = mode_file(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for text in ["  headphones \n", "\nheadphones\n", "\tspeakers\n"] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                load_from(&path),
                OutputMode::from_token(text.trim()),
                "whitespace must not hide a valid token: {text:?}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_and_loads_back_the_same_mode() {
        let dir = scratch("round-trip");

        for mode in [
            OutputMode::Speakers,
            OutputMode::Headphones,
            OutputMode::Auto,
        ] {
            let path = mode_file(&dir);
            save_to(&path, mode).expect("the scratch directory must accept the write");
            assert_eq!(load_from(&path), Some(mode));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_the_parent_directories() {
        // The state directory does not exist on a fresh install, so the write
        // must create it rather than fail.
        let dir = scratch("create-dirs");
        let path = mode_file(&dir);
        assert!(!path.parent().unwrap().exists());

        save_to(&path, OutputMode::Auto).expect("the write must create its directories");

        assert!(path.exists(), "the mode file must exist after a save");
        assert_eq!(load_from(&path), Some(OutputMode::Auto));

        let _ = fs::remove_dir_all(&dir);
    }
}
