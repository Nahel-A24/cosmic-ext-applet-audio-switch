//! Registers a default global shortcut for the applet, once, without overriding
//! the user's later choices.
//!
//! A Wayland client cannot grab a global hotkey, so the shortcut is a
//! cosmic-settings custom shortcut whose action is `<binary> --cycle` (see
//! [`crate::control`]). Binding it by hand works, but a fresh install has no
//! binding at all, so the applet also registers a sensible default the first
//! time it runs.
//!
//! The user's shortcuts live in the cosmic-config store
//! `com.system76.CosmicSettings.Shortcuts`, version `1`, file `custom`, i.e. at
//! `<config_dir>/cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom` (see
//! [`store_path`]), in a RON-like format with struct-style tuple keys. This
//! module edits that file textually, for a few deliberate reasons:
//!
//! - **The file holds far more than our entry.** It is the user's whole custom
//!   shortcut set; a parse-and-reserialize round trip would reformat, reorder or
//!   drop fields this applet does not model, silently rewriting choices that
//!   have nothing to do with the audio output.
//! - **A textual insert preserves everything else byte-for-byte.** The new entry
//!   goes immediately before the closing brace, and the original text must still
//!   be a substring of the result.
//! - **A syntax gate stands between the edit and the disk.** Before writing, the
//!   result must parse with `ron` and must still contain the original text. If
//!   either check fails, nothing is written, so a bug here cannot corrupt the
//!   user's shortcut configuration.
//!
//! Registration is deliberately one-shot and idempotent. Once the applet has
//! made the decision, a flag file records that fact; the applet then never adds
//! the binding again and never re-adds one the user removed or changed.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// cosmic-config store id that owns the custom shortcuts.
const STORE_ID: &str = "com.system76.CosmicSettings.Shortcuts";

/// Store version whose `custom` file holds the user's bindings.
///
/// Stored as a number on purpose: cosmic-config lays the directory out as
/// `v<version>` (see [`store_path`]), so the `v` prefix belongs to the path
/// formatting, not to this value.
const STORE_VERSION: u64 = 1;

/// Key file name inside the store version directory.
const STORE_KEY: &str = "custom";

/// Parent directory cosmic-config puts every store under, inside the config dir.
const COSMIC_DIR: &str = "cosmic";

/// File that only exists inside a Flatpak sandbox.
///
/// cosmic-config itself detects Flatpak through the `FLATPAK_ID` environment
/// variable; this marker is accepted in addition, because it is the cheapest
/// reliable signal available without trusting any single variable.
const FLATPAK_INFO: &str = "/.flatpak-info";

/// Flatpak application id, used to build the `flatpak run` command.
const FLATPAK_APP_ID: &str = "io.github.Nahel_A24.CosmicExtAppletAudioSwitch";

/// Argument that forwards one cycle request to the running applet.
///
/// Re-declared here instead of imported so the exact text this module writes is
/// visible at the point of use; it must match [`crate::control::CYCLE`].
const CYCLE: &str = "--cycle";

/// Marker substrings that prove the file already carries a binding for us.
///
/// The native executable name and the Flatpak application id together cover both
/// ways the command can be written, so either form counts as "the user already
/// has a binding".
const OWN_MARKERS: [&str; 2] = [
    "cosmic-ext-applet-audio-switch",
    "CosmicExtAppletAudioSwitch",
];

/// Description shown next to the binding in cosmic-settings.
const DESCRIPTION: &str = "Cosmic Audio Switch";

/// Flag file name that records the one-shot registration decision.
const FLAG_FILE: &str = "shortcut-registered";

/// What [`ensure_default`] did.
///
/// Every caller-visible action has its own variant so the outcome can be logged
/// and asserted in tests without guessing from side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The entry was inserted and written.
    Added,
    /// The file already referenced this applet; only the flag was written.
    Present,
    /// The flag file already existed, so nothing else was read or written.
    AlreadyDecided,
    /// `Super+Shift+A` is bound to something else; the flag was left unwritten.
    Taken,
    /// A path could not be resolved, the file could not be read, or the edit did
    /// not pass the safety gate; nothing was written.
    Unavailable,
}

/// Ensures the default `Super+Shift+A` binding exists, once.
///
/// Resolves the environment-dependent paths and delegates to
/// [`ensure_default_in`]. This is the entry point called from `main`.
pub fn ensure_default() -> Outcome {
    let Some(config) = config_path() else {
        tracing::warn!(
            "could not resolve the shortcuts config path; skipping shortcut registration"
        );
        return Outcome::Unavailable;
    };

    let Some(flag) = flag_path() else {
        tracing::warn!("could not resolve the shortcut flag path; skipping shortcut registration");
        return Outcome::Unavailable;
    };

    let Some(command) = command() else {
        tracing::warn!(
            "could not determine this applet's executable path; skipping shortcut registration"
        );
        return Outcome::Unavailable;
    };

    let outcome = ensure_default_in(&config, &flag, &command);

    tracing::info!(
        ?outcome,
        config = %config.display(),
        "default shortcut registration finished"
    );

    outcome
}

/// The core of [`ensure_default`], with the environment already resolved.
///
/// Split out so tests can point it at temporary directories and never touch the
/// user's real configuration or mutate process-wide environment variables.
fn ensure_default_in(config: &Path, flag: &Path, command: &str) -> Outcome {
    // The flag means the decision was already made, on a previous run. Do not
    // read or write anything else: the user may have changed or removed the
    // binding since, and re-adding it would override them.
    if flag.exists() {
        return Outcome::AlreadyDecided;
    }

    let original = match fs::read_to_string(config) {
        Ok(text) => text,
        // A missing file is the normal first run; an empty map is created.
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            tracing::warn!(config = %config.display(), %error, "could not read the shortcuts config; leaving it untouched");
            return Outcome::Unavailable;
        }
    };

    // A binding for us may exist under a different combination, or through the
    // Flatpak id. Either way the choice is already made; record it and stop.
    if references_applet(&original) {
        write_flag(flag);
        return Outcome::Present;
    }

    // Someone else owns the combination. Do not write the flag, so a later run
    // (after the user frees the combination) can still add ours.
    if combination_taken(&original) {
        tracing::info!("Super+Shift+A is already bound to another command; not overriding it");
        return Outcome::Taken;
    }

    let Some(result) = insert_entry(&original, command) else {
        tracing::warn!(
            "the shortcuts config has no closing brace to insert before; leaving it untouched"
        );
        return Outcome::Unavailable;
    };

    if !gate(&original, &result) {
        tracing::warn!(
            "the edited shortcuts config did not pass the syntax gate; leaving it untouched"
        );
        return Outcome::Unavailable;
    }

    if let Err(error) = write_atomically(config, &result) {
        tracing::warn!(config = %config.display(), %error, "could not write the shortcuts config; leaving it untouched");
        return Outcome::Unavailable;
    }

    // Only a successful write earns the flag, so a failed attempt is retried on
    // the next start instead of being permanently skipped.
    write_flag(flag);
    Outcome::Added
}

/// Relative layout of the shortcuts file inside a config directory.
///
/// This mirrors `cosmic-config`'s `Config::new_inner` exactly, and it is the
/// part that is easy to get wrong: the store sits under a `cosmic/` parent, and
/// the version directory is `v<version>`, not the bare number. The path is
/// therefore `cosmic/<STORE_ID>/v<STORE_VERSION>/<STORE_KEY>`. Writing anywhere
/// else (e.g. without `cosmic/`, or into `1/`) produces a file cosmic-comp never
/// reads, so the shortcut would silently never exist.
fn store_path() -> PathBuf {
    Path::new(COSMIC_DIR)
        .join(STORE_ID)
        .join(format!("v{STORE_VERSION}"))
        .join(STORE_KEY)
}

/// Are we running inside a Flatpak sandbox?
///
/// cosmic-config checks `FLATPAK_ID`; the marker file is accepted too, so a
/// sandbox that exposes the file but not the variable is still recognised.
fn in_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || Path::new(FLATPAK_INFO).exists()
}

/// Resolves the config directory the same way cosmic-config's `get_config_dir`
/// does.
///
/// Pure on purpose: every branch is decided from the arguments, so the tests can
/// exercise Flatpak and non-Flatpak layouts without mutating the process
/// environment. `flatpak` is the Flatpak decision, and the other arguments are
/// the relevant variables already read from the environment.
///
/// - In Flatpak, `HOST_XDG_CONFIG_HOME` (the host directory the manifest exposes
///   through `--filesystem=xdg-config`) wins; otherwise `$HOME/.config`, since
///   in Flatpak `HOME` is the real home.
/// - Outside Flatpak, `XDG_CONFIG_HOME` wins when set and non-empty; otherwise
///   `$HOME/.config`.
fn resolve_config_dir(
    flatpak: bool,
    host_xdg_config_home: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if flatpak {
        if let Some(host) = host_xdg_config_home.filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(host));
        }
        return home
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".config"));
    }

    if let Some(config_home) = xdg_config_home.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(config_home));
    }

    home.filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config"))
}

/// Absolute path of the shortcuts config file, or `None` when it cannot be built.
fn config_path() -> Option<PathBuf> {
    let dir = resolve_config_dir(
        in_flatpak(),
        std::env::var("HOST_XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;

    Some(dir.join(store_path()))
}

/// Absolute path of the flag file, or `None` when it cannot be built.
///
/// The state directory is shared with the mode persistence (see
/// [`crate::state::state_dir`]), so both files live under the applet's single
/// state directory instead of resolving it twice.
fn flag_path() -> Option<PathBuf> {
    Some(crate::state::state_dir()?.join(FLAG_FILE))
}

/// The command a shortcut should run, resolved for the current environment.
///
/// Inside Flatpak the host compositor cannot see the sandbox binary, so the
/// command goes through `flatpak run`. Natively the applet's own absolute path
/// is used.
fn command() -> Option<String> {
    if in_flatpak() {
        return Some(format!("flatpak run {FLATPAK_APP_ID} {CYCLE}"));
    }

    let exe = std::env::current_exe().ok()?;
    Some(format!("{} {CYCLE}", exe.display()))
}

/// Does the file text already mention this applet?
///
/// A mention means the user (or a previous run) already has a binding for us,
/// whatever combination it uses.
fn references_applet(text: &str) -> bool {
    OWN_MARKERS.iter().any(|marker| text.contains(marker))
}

/// Is `Super+Shift+A` already bound to something else?
///
/// Detection is whitespace-insensitive and modifier-order-insensitive: the text
/// is compacted by removing all whitespace, then every `key:"a"` is matched with
/// the nearest preceding `modifiers:[ ... ]` list, which must contain both
/// `Super` and `Shift`. Reconstructing the list from the raw text avoids
/// depending on the exact line layout cosmic-config writes.
fn combination_taken(text: &str) -> bool {
    const KEY: &str = "key:\"a\"";
    const MODIFIERS: &str = "modifiers:[";

    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();

    let mut rest = compact.as_str();
    while let Some(key_pos) = rest.find(KEY) {
        if let Some(mod_pos) = rest[..key_pos].rfind(MODIFIERS) {
            let list_start = mod_pos + MODIFIERS.len();
            if let Some(end) = rest[list_start..].find(']') {
                let list = &rest[list_start..list_start + end];
                if list.contains("Super") && list.contains("Shift") {
                    return true;
                }
            }
        }

        // Continue after this occurrence so a later `key:"a"` is also checked.
        rest = &rest[key_pos + KEY.len()..];
    }

    false
}

/// The text of our entry, ready to be inserted into the map.
///
/// The layout mirrors what cosmic-config writes, so a hand-inspected file looks
/// the same as one the applet wrote.
fn entry_text(command: &str) -> String {
    format!(
        r#"    (
        modifiers: [
            Super,
            Shift,
        ],
        key: "a",
        description: Some("{DESCRIPTION}"),
    ): Spawn("{command}"),
"#
    )
}

/// Produces the new file text, or `None` when there is no closing brace to
/// insert before.
///
/// An empty (or `{}`) file becomes a fresh map holding only our entry; any other
/// file keeps everything before the last `}` byte-for-byte.
fn insert_entry(original: &str, command: &str) -> Option<String> {
    let entry = entry_text(command);

    // An empty store becomes a fresh map. cosmic-config puts the opening brace on
    // its own line.
    if original.trim().is_empty() {
        return Some(format!("{{\n{entry}}}"));
    }

    let end = original.rfind('}')?;

    let mut result = String::with_capacity(original.len() + entry.len() + 1);
    result.push_str(&original[..end]);
    // cosmic-config separates entries with a bare newline, never a blank line.
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result.push_str(&entry);
    result.push_str(&original[end..]);
    Some(result)
}

/// The safety gate applied to the edited text before it is written.
///
/// Passing requires two things:
///
/// 1. The result parses as RON, so a bad insert can never be written.
/// 2. Nothing from the original was lost.
///
/// The entry is placed immediately before the final `}`, so the original cannot
/// remain one contiguous substring of the result — the inserted entry sits where
/// the brace used to be adjacent. "Nothing lost" is therefore checked as the two
/// surviving halves: everything before the insertion point (the last `}`) must
/// still appear in the result, and the tail from that point must still end the
/// result. A truncated or rewritten result fails one of the two.
///
/// An empty original has nothing to lose, so only the parse check applies.
fn gate(original: &str, result: &str) -> bool {
    if ron::from_str::<serde::de::IgnoredAny>(result).is_err() {
        return false;
    }

    if original.trim().is_empty() {
        return true;
    }

    match original.rfind('}') {
        Some(end) => result.contains(&original[..end]) && result.ends_with(&original[end..]),
        None => false,
    }
}

/// Writes `content` to `path` through a temp file and a rename.
///
/// cosmic-comp watches this file and reloads it live, so a truncating in-place
/// write could be observed as an empty or half-written file. Writing a sibling
/// temp file and renaming over the target makes the replacement atomic.
fn write_atomically(path: &Path, content: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "config path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    let tmp = dir.join(format!(".{STORE_KEY}.tmp-{}", std::process::id()));
    fs::write(&tmp, content)?;

    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    Ok(())
}

/// Creates the flag file, creating its parent directories as needed.
///
/// A failure here is logged but not fatal: the flag only prevents future
/// re-registration, and the applet must still start.
fn write_flag(flag: &Path) {
    if let Some(dir) = flag.parent() {
        if let Err(error) = fs::create_dir_all(dir) {
            tracing::warn!(flag = %flag.display(), %error, "could not create the shortcut flag directory");
            return;
        }
    }

    if let Err(error) = fs::write(flag, "registered\n") {
        tracing::warn!(flag = %flag.display(), %error, "could not write the shortcut flag");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique, empty directory under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "audio-switch-shortcut-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the scratch directory must be writable");
        path
    }

    /// The command the tests register, so assertions never depend on the test
    /// binary's real path.
    const COMMAND: &str = "/opt/cosmic-ext-applet-audio-switch --cycle";

    /// One entry in the same shape cosmic-config writes for clip-keep.
    const ENTRY_CLIP: &str = "\
    (
        modifiers: [
            Super,
        ],
        key: \"v\",
        description: Some(\"Clip Keep\"),
    ): Spawn(\"flatpak run io.example.ClipKeep --toggle\"),
";

    /// A second, unrelated entry.
    const ENTRY_HUD: &str = "\
    (
        modifiers: [
            Super,
            Alt,
        ],
        key: \"s\",
        description: Some(\"Hud\"),
    ): Spawn(\"example-hud\"),
";

    /// A map holding the two entries above.
    fn existing_map() -> String {
        format!("{{\n{ENTRY_CLIP}{ENTRY_HUD}}}")
    }

    /// The config dir a scratch test uses as its `$config_dir`.
    fn config_dir(dir: &Path) -> PathBuf {
        dir.join("config")
    }

    /// The config and flag paths under one scratch directory.
    ///
    /// The config path is derived from [`store_path`], so the fixtures always
    /// sit at the layout the code actually targets.
    fn paths(dir: &Path) -> (PathBuf, PathBuf) {
        (
            config_dir(dir).join(store_path()),
            dir.join("state/shortcut-registered"),
        )
    }

    /// The layout cosmic-config uses, which the code must match byte for byte.
    ///
    /// A regression here is what wrote the shortcut where cosmic-comp never read
    /// it: the `cosmic/` parent went missing and the version directory was `1`
    /// instead of `v1`.
    #[test]
    fn resolves_the_cosmic_config_layout() {
        assert_eq!(
            store_path(),
            Path::new("cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom"),
        );
        assert_ne!(
            store_path(),
            Path::new("com.system76.CosmicSettings.Shortcuts/1/custom"),
        );
    }

    #[test]
    fn resolves_the_config_dir_outside_flatpak() {
        // XDG_CONFIG_HOME wins when set...
        assert_eq!(
            resolve_config_dir(false, None, Some("/xdg/config"), Some("/home/u")),
            Some(PathBuf::from("/xdg/config")),
        );
        // ...an empty one does not, and HOME is the fallback.
        assert_eq!(
            resolve_config_dir(false, None, Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config")),
        );
        assert_eq!(
            resolve_config_dir(false, None, None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config")),
        );
        // With no HOME and no XDG_CONFIG_HOME the path cannot be built.
        assert_eq!(resolve_config_dir(false, None, None, None), None);
    }

    #[test]
    fn resolves_the_config_dir_inside_flatpak() {
        // HOST_XDG_CONFIG_HOME is the host dir the manifest exposed, so it wins
        // over both the sandbox XDG_CONFIG_HOME and HOME.
        assert_eq!(
            resolve_config_dir(
                true,
                Some("/host/config"),
                Some("/sandbox/xdg"),
                Some("/home/u")
            ),
            Some(PathBuf::from("/host/config")),
        );
        // Without it, HOME (the real home inside Flatpak) is the fallback; the
        // sandbox XDG_CONFIG_HOME must not be used.
        assert_eq!(
            resolve_config_dir(true, None, Some("/sandbox/xdg"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config")),
        );
        assert_eq!(resolve_config_dir(true, None, None, None), None);
    }

    #[test]
    fn adds_the_entry_to_a_missing_file() {
        let dir = scratch("missing");
        let (config, flag) = paths(&dir);

        assert_eq!(ensure_default_in(&config, &flag, COMMAND), Outcome::Added);

        let text = fs::read_to_string(&config).expect("the config file must be created");
        assert!(
            ron::from_str::<serde::de::IgnoredAny>(&text).is_ok(),
            "the created file must parse: {text}"
        );
        assert!(text.contains(COMMAND), "the command must be registered");
        assert!(
            text.contains(DESCRIPTION),
            "the description must be present"
        );
        assert!(
            flag.exists(),
            "the flag must be written after a successful add"
        );

        // The write must land at the layout cosmic-comp reads, and must not
        // create a stray file at the old, wrong path.
        assert_eq!(
            config,
            config_dir(&dir).join("cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom"),
        );
        let wrong = config_dir(&dir).join("com.system76.CosmicSettings.Shortcuts/1/custom");
        assert!(
            !wrong.exists(),
            "the applet must not write to the old wrong path: {}",
            wrong.display()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adds_the_entry_without_losing_existing_entries() {
        let dir = scratch("existing");
        let (config, flag) = paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, existing_map()).unwrap();

        assert_eq!(ensure_default_in(&config, &flag, COMMAND), Outcome::Added);

        let text = fs::read_to_string(&config).unwrap();
        assert!(
            ron::from_str::<serde::de::IgnoredAny>(&text).is_ok(),
            "the edited file must parse: {text}"
        );
        assert!(
            text.contains(ENTRY_CLIP),
            "the clip-keep entry must survive verbatim"
        );
        assert!(
            text.contains(ENTRY_HUD),
            "the hud entry must survive verbatim"
        );
        assert!(text.contains(COMMAND), "our entry must have been added");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_presence_without_touching_a_file_that_already_mentions_us() {
        let dir = scratch("present");
        let (config, flag) = paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();

        let original = "{\n    (\n        modifiers: [\n            Super,\n        ],\n        key: \"y\",\n        description: Some(\"Ours already\"),\n    ): Spawn(\"/home/user/.local/bin/cosmic-ext-applet-audio-switch --cycle\"),\n}\n";
        fs::write(&config, original).unwrap();
        let before = fs::read(&config).unwrap();

        assert_eq!(ensure_default_in(&config, &flag, COMMAND), Outcome::Present);

        assert_eq!(
            before,
            fs::read(&config).unwrap(),
            "the file must be unchanged"
        );
        assert!(
            flag.exists(),
            "the flag records that the decision was already made"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_taken_when_the_combination_belongs_to_another_command() {
        let dir = scratch("taken");
        let (config, flag) = paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();

        let original = "{\n    (\n        modifiers: [\n            Super,\n            Shift,\n        ],\n        key: \"a\",\n        description: Some(\"Other thing\"),\n    ): Spawn(\"something-else\"),\n}\n";
        fs::write(&config, original).unwrap();
        let before = fs::read(&config).unwrap();

        assert_eq!(ensure_default_in(&config, &flag, COMMAND), Outcome::Taken);

        assert_eq!(
            before,
            fs::read(&config).unwrap(),
            "the file must be unchanged"
        );
        assert!(!flag.exists(), "a takeover must not be recorded as decided");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_the_combination_regardless_of_modifier_order() {
        let dir = scratch("order");
        let (config, flag) = paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();

        // Same combination, written with the modifiers reversed.
        let original = "{\n    (\n        modifiers: [\n            Shift,\n            Super,\n        ],\n        key: \"a\",\n        description: Some(\"Other thing\"),\n    ): Spawn(\"something-else\"),\n}\n";
        fs::write(&config, original).unwrap();

        assert_eq!(ensure_default_in(&config, &flag, COMMAND), Outcome::Taken);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_does_not_add_a_duplicate() {
        let dir = scratch("idempotent");
        let (config, flag) = paths(&dir);

        assert_eq!(ensure_default_in(&config, &flag, COMMAND), Outcome::Added);
        assert_eq!(
            ensure_default_in(&config, &flag, COMMAND),
            Outcome::AlreadyDecided
        );

        let text = fs::read_to_string(&config).unwrap();
        assert_eq!(
            text.matches(COMMAND).count(),
            1,
            "the command must appear exactly once: {text}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_unavailable_when_the_config_cannot_be_read() {
        let dir = scratch("unreadable");
        let (config, flag) = paths(&dir);
        // A directory cannot be read as a file, which stands in for any
        // unreadable path without needing to manipulate permissions.
        fs::create_dir_all(&config).unwrap();

        assert_eq!(
            ensure_default_in(&config, &flag, COMMAND),
            Outcome::Unavailable
        );
        assert!(!flag.exists(), "a failed read must not record a decision");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_unavailable_when_the_file_has_no_closing_brace() {
        let dir = scratch("truncated");
        let (config, flag) = paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();

        let original =
            "{\n    (\n        modifiers: [\n            Super,\n        ],\n        key: \"v\",";
        fs::write(&config, original).unwrap();
        let before = fs::read(&config).unwrap();

        assert_eq!(
            ensure_default_in(&config, &flag, COMMAND),
            Outcome::Unavailable
        );

        assert_eq!(
            before,
            fs::read(&config).unwrap(),
            "the file must be unchanged"
        );
        assert!(!flag.exists(), "a rejected edit must not record a decision");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_parse_gate_accepts_valid_ron_and_rejects_garbage() {
        let valid = existing_map();
        assert!(gate(&valid, &valid), "a valid map must pass the gate");

        assert!(
            !gate(&valid, "garbage {"),
            "malformed RON must fail the gate"
        );

        // The gate must also require the original text to survive.
        assert!(
            !gate(&valid, "{\n    (\n        modifiers: [Super],\n        key: \"v\",\n    ): Spawn(\"x\"),\n}"),
            "a result that dropped the original must fail the gate"
        );
    }
}
