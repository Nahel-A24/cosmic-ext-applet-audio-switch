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
    for key in ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.to_ascii_lowercase();
            // LANGUAGE can be like "en:es"
            if v.starts_with("es") || v.contains(":es") || v.contains(" es") {
                return Lang::Es;
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
