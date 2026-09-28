//! Message catalogue.
//!
//! English is the source language: every user facing string is written in
//! English at its call site and wrapped in [`t!`] or [`tf!`].  The catalogue
//! then maps it to the language of the machine the client is running on.  A
//! missing entry falls back to English, so the ui can never end up blank — and
//! a test walks the source to make sure nothing is missing.
//!
//! Translations live in `lang/zh_*.rs`, one file per area, so they can be
//! edited independently.
//!
//! Tone matters as much as the words: the Chinese is written the way a person
//! would say it, not the way a translation tool would.  No "积极拒绝", no
//! "进行同步操作", just plain sentences.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

mod zh_client;
mod zh_ui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    fn tag(self) -> u8 {
        match self {
            Lang::En => 0,
            Lang::Zh => 1,
        }
    }

    fn from_tag(t: u8) -> Option<Lang> {
        match t {
            0 => Some(Lang::En),
            1 => Some(Lang::Zh),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
        }
    }
}

/// The chosen language.  255 means "not decided yet".
static LANG: AtomicU8 = AtomicU8::new(255);

/// The language to speak.  Decided on first use and remembered, but it can be
/// changed at any time — the ui reads it on every frame.
pub fn lang() -> Lang {
    let tag = LANG.load(Ordering::Relaxed);
    if let Some(l) = Lang::from_tag(tag) {
        return l;
    }
    let l = detect();
    LANG.store(l.tag(), Ordering::Relaxed);
    l
}

/// Picks the language once, at startup: an explicit setting wins over the
/// machine's own language.
pub fn init(spec: Option<&str>) {
    let l = spec.and_then(parse).unwrap_or_else(detect);
    LANG.store(l.tag(), Ordering::Relaxed);
}

/// Switches language at runtime.
pub fn set(l: Lang) {
    LANG.store(l.tag(), Ordering::Relaxed);
}

fn detect() -> Lang {
    // Tests assert on the English source strings, so keep them predictable
    // regardless of the machine the suite runs on.
    if cfg!(test) {
        return Lang::En;
    }
    if let Some(l) = std::env::var("TABSSH_LANG").ok().as_deref().and_then(parse) {
        return l;
    }
    parse(&system_locale()).unwrap_or(Lang::En)
}

/// The machine's own language, ignoring any setting.
pub fn detected() -> Lang {
    if let Some(l) = std::env::var("TABSSH_LANG").ok().as_deref().and_then(parse) {
        return l;
    }
    parse(&system_locale()).unwrap_or(Lang::En)
}

fn parse(spec: &str) -> Option<Lang> {
    let s = spec.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("zh")
        || s.contains("chinese")
        || s.contains("中文")
        || s.contains("简体")
        || s.contains("繁體")
    {
        Some(Lang::Zh)
    } else if s.starts_with("en") {
        Some(Lang::En)
    } else {
        None
    }
}

/// Parses a language name from the command line (`--lang`).
pub fn parse_spec(spec: &str) -> Option<Lang> {
    parse(spec)
}

/// The machine's language, from the environment first.
fn system_locale() -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    platform_locale()
}

fn platform_locale() -> String {
    // The Windows UI language, e.g. 0x0804 for Simplified Chinese.  The low ten
    // bits are the primary language, and 0x04 is LANG_CHINESE.
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    if id & 0x3ff == 0x04 {
        "zh".into()
    } else {
        "en".into()
    }
}

fn catalogue() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        for &(en, tr) in zh_client::ZH.iter().chain(zh_ui::ZH.iter()) {
            m.insert(en, tr);
        }
        m
    })
}

/// Translates an English string.
pub fn t(en: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Zh => catalogue().get(en).copied().unwrap_or(en),
    }
}

/// Translates a template and fills its `{}` placeholders in order.
pub fn tf(en: &'static str, args: &[&str]) -> String {
    let tmpl = t(en);
    let mut out = String::with_capacity(tmpl.len() + 16);
    let mut parts = tmpl.split("{}");
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for (i, part) in parts.enumerate() {
        if let Some(a) = args.get(i) {
            out.push_str(a);
        }
        out.push_str(part);
    }
    out
}

/// `t!(...)` — translate a fixed string.
#[macro_export]
macro_rules! t {
    ($key:expr) => {
        $crate::lang::t($key)
    };
}

/// `tf!(...)` — translate a template with any number of values.
#[macro_export]
macro_rules! tf {
    ($key:expr $(, $arg:expr)*) => {{
        let owned: ::std::vec::Vec<::std::string::String> =
            ::std::vec![$( ::std::string::ToString::to_string(&$arg) ),*];
        let refs: ::std::vec::Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        $crate::lang::tf($key, &refs)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_translation_has_a_unique_key() {
        let mut seen = std::collections::HashSet::new();
        for &(en, _) in zh_client::ZH.iter().chain(zh_ui::ZH.iter()) {
            assert!(!en.is_empty(), "empty key");
            assert!(seen.insert(en), "duplicate translation key: {en}");
        }
    }

    #[test]
    fn translations_are_not_left_in_english() {
        // A handful of keys are legitimately identical in both languages
        // (product names, units).  Everything else must actually differ.
        let allowed = [
            "tabssh",
            "tabssh-agent",
            "kbit/s",
            "WxH",
            "pid",
            "cpu",
            "GB",
            "MB",
        ];
        for &(en, zh) in zh_client::ZH.iter().chain(zh_ui::ZH.iter()) {
            if allowed.contains(&en) {
                continue;
            }
            assert_ne!(en, zh, "untranslated key: {en}");
        }
    }

    /// A translation must have the same number of `{}` placeholders as its
    /// English source, or `tf!` would silently drop a value or leave a hole.
    #[test]
    fn translations_keep_their_placeholders() {
        let count = |s: &str| s.matches("{}").count();
        for &(en, zh) in zh_client::ZH.iter().chain(zh_ui::ZH.iter()) {
            assert_eq!(
                count(en),
                count(zh),
                "placeholder count differs: {en:?} -> {zh:?}"
            );
        }
    }

    /// Walks the source and checks that every `t!(...)` has a translation.
    /// Without this a typo would silently leave one line in English.
    #[test]
    fn every_literal_used_in_the_source_is_translated() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let cat = catalogue();
        let mut missing: Vec<String> = Vec::new();
        let mut files = vec![root.clone()];
        while let Some(dir) = files.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for lit in literals(&text) {
                    if !cat.contains_key(lit.as_str()) {
                        missing.push(format!("{}: {lit}", path.display()));
                    }
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "these strings are not translated:\n{}",
            missing.join("\n")
        );
    }

    /// Extracts the literals passed to `t!(...)` / `tf!(...)`.
    fn literals(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in text.match_indices("t!(\"") {
            if starts_a_macro(text, i) {
                if let Some(s) = read_literal(&text[i + 3..]) {
                    out.push(s);
                }
            }
        }
        for (i, _) in text.match_indices("tf!(\"") {
            if starts_a_macro(text, i) {
                if let Some(s) = read_literal(&text[i + 4..]) {
                    out.push(s);
                }
            }
        }
        out
    }

    /// True when the match at `i` really starts a `t!` / `tf!` call and is not
    /// just the tail of an identifier such as `format!`.
    fn starts_a_macro(text: &str, i: usize) -> bool {
        match text[..i].chars().next_back() {
            Some(c) => !(c.is_alphanumeric() || c == '_'),
            None => true,
        }
    }

    /// Reads a Rust string literal at the start of `s`, honouring `\"` and `\\`.
    fn read_literal(s: &str) -> Option<String> {
        let mut chars = s.chars();
        if chars.next()? != '"' {
            return None;
        }
        let mut out = String::new();
        loop {
            match chars.next()? {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    // A backslash at end of line swallows the newline and the
                    // indentation after it, exactly as rustc does — otherwise a
                    // wrapped literal would never match its catalogue entry.
                    '\n' => {
                        while matches!(chars.clone().next(), Some(' ') | Some('\t')) {
                            chars.next();
                        }
                    }
                    '\r' => {
                        if chars.clone().next() == Some('\n') {
                            chars.next();
                        }
                        while matches!(chars.clone().next(), Some(' ') | Some('\t')) {
                            chars.next();
                        }
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
    }

    #[test]
    fn detection_understands_common_locales() {
        assert_eq!(parse("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(parse("zh-TW"), Some(Lang::Zh));
        assert_eq!(parse("Chinese (Simplified)_China"), Some(Lang::Zh));
        assert_eq!(parse("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(parse("C"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn templates_fill_placeholders_in_order() {
        assert_eq!(tf("{} of {}", &["1", "2"]), "1 of 2");
        assert_eq!(tf("no placeholders", &[]), "no placeholders");
        // A missing argument leaves the hole rather than panicking.
        assert_eq!(tf("a{}b{}c", &["1"]), "a1bc");
    }
}
