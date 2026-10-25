//! Tiny locale detection.
//!
//! Only English and Spanish are supported. The lookup follows gettext's
//! precedence: `LC_ALL` wins over everything (including `LANGUAGE`), then
//! `LANGUAGE` picks among the message languages, then `LC_MESSAGES` and
//! `LANG` name the base locale. `C` and `POSIX` mean English.
//!
//! The persisted mode tokens and the `pactl` parsing stay locale-independent
//! regardless of this setting.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

/// First language of a gettext language list such as `es:en` or `es_AR.UTF-8`.
fn first_language(value: &str) -> &str {
    value
        .split(':')
        .next()
        .unwrap_or(value)
        .split('.')
        .next()
        .unwrap_or(value)
        .split('@')
        .next()
        .unwrap_or(value)
}

fn is_c_locale(value: &str) -> bool {
    let first = first_language(value);
    first.eq_ignore_ascii_case("c") || first.eq_ignore_ascii_case("posix")
}

fn language_of(value: &str) -> Lang {
    if first_language(value).to_ascii_lowercase().starts_with("es") {
        Lang::Es
    } else {
        Lang::En
    }
}

/// Resolve the interface language from a variable lookup.
///
/// Taking the lookup as an argument keeps the precedence rules testable without
/// mutating the process environment.
pub fn detect_from(get: impl Fn(&str) -> Option<String>) -> Lang {
    let value = |key: &str| {
        get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    // LC_ALL overrides every other variable, and under a C locale gettext
    // ignores LANGUAGE entirely, so both rules are applied before the rest.
    if let Some(locale) = value("LC_ALL") {
        return if is_c_locale(&locale) {
            Lang::En
        } else {
            language_of(&locale)
        };
    }

    let base = value("LC_MESSAGES").or_else(|| value("LANG"));
    if base.as_deref().is_some_and(is_c_locale) {
        return Lang::En;
    }

    // LANGUAGE is a preference list: `es:en` asks for Spanish first, `en:es`
    // asks for English first. It only applies when the base locale is not C.
    if let Some(list) = value("LANGUAGE") {
        return language_of(&list);
    }

    base.map(|value| language_of(&value)).unwrap_or(Lang::En)
}

fn detect() -> Lang {
    detect_from(|key| std::env::var(key).ok())
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn lang() -> Lang {
    *LANG.get_or_init(detect)
}

pub fn is_spanish() -> bool {
    lang() == Lang::Es
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Lang {
        detect_from(|key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        })
    }

    #[test]
    fn defaults_to_english_without_any_locale_variable() {
        assert_eq!(from(&[]), Lang::En);
    }

    #[test]
    fn reads_a_spanish_base_locale() {
        assert_eq!(from(&[("LANG", "es_AR.UTF-8")]), Lang::Es);
        assert_eq!(from(&[("LANG", "es_ES.UTF-8")]), Lang::Es);
        assert_eq!(from(&[("LANG", "es")]), Lang::Es);
    }

    #[test]
    fn reads_an_english_base_locale() {
        assert_eq!(from(&[("LANG", "en_US.UTF-8")]), Lang::En);
        assert_eq!(from(&[("LANG", "en")]), Lang::En);
    }

    #[test]
    fn falls_back_to_english_for_unsupported_locales() {
        assert_eq!(from(&[("LANG", "fr_FR.UTF-8")]), Lang::En);
        assert_eq!(from(&[("LANG", "pt_BR.UTF-8")]), Lang::En);
    }

    #[test]
    fn language_list_picks_its_first_entry() {
        assert_eq!(
            from(&[("LANG", "es_AR.UTF-8"), ("LANGUAGE", "es:en")]),
            Lang::Es
        );
        assert_eq!(
            from(&[("LANG", "es_AR.UTF-8"), ("LANGUAGE", "en:es")]),
            Lang::En
        );
    }

    #[test]
    fn lc_messages_beats_lang() {
        assert_eq!(
            from(&[("LANG", "es_AR.UTF-8"), ("LC_MESSAGES", "en_US.UTF-8")]),
            Lang::En
        );
    }

    #[test]
    fn lc_all_beats_language() {
        assert_eq!(
            from(&[
                ("LC_ALL", "C"),
                ("LANGUAGE", "es:en"),
                ("LANG", "es_AR.UTF-8"),
            ]),
            Lang::En
        );
        assert_eq!(
            from(&[("LC_ALL", "es_ES.UTF-8"), ("LANGUAGE", "en")]),
            Lang::Es
        );
    }

    #[test]
    fn empty_values_are_ignored() {
        assert_eq!(from(&[("LC_ALL", ""), ("LANG", "es_AR.UTF-8")]), Lang::Es);
    }
}
