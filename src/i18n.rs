//! Tiny locale detection.
//!
//! English by default; Spanish when `LANG`, `LC_ALL` or `LC_MESSAGES` starts
//! with `es`. The persisted mode tokens and pactl parsing stay locale-
//! independent regardless of this setting.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

fn detect() -> Lang {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.to_ascii_lowercase();
            if v.starts_with("es") {
                return Lang::Es;
            }
            if !v.is_empty() && !v.starts_with("c") && !v.starts_with("posix") {
                // non-C locale we don't know — stay English unless it's Spanish
            }
        }
    }
    Lang::En
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn lang() -> Lang {
    *LANG.get_or_init(detect)
}

pub fn is_spanish() -> bool {
    lang() == Lang::Es
}
