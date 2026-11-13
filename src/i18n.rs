//! Internationalization module.
//!
//! Language detection: Telegram `language_code` field.
//! - `zh-hans` / `zh-hant` / `zh` -> Chinese
//! - Everything else -> English
//!
//! Storage: settings table key `lang:<user_id>` = "zh" | "en".
//! Switch: /set panel language button.
//!
//! UI strings live in `locales/zh.conf` and `locales/en.conf` (one
//! `key=value` line per key) and are embedded into the binary at compile time.
//! `\n`, `\r`, `\t`, `\"` and `\\` escapes are decoded when a language map is
//! first used.
//!
//! To add a language: add a variant, extend `db_value`, `Lang::ALL`,
//! `source()` and the `strings()` map, then add `locales/<code>.conf`.
//! The file must define its own `language.<code>` key (the language's
//! self-name) plus every `cmd_menu.*` key; menu registration and the language
//! picker iterate `Lang::ALL`, so no per-language match arms elsewhere are
//! needed. The `t()` fallback routes every non-English language to English.
//! The locale tests iterate `Lang::ALL`, so the new file is covered
//! automatically.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Supported languages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// All supported languages, in UI cycle/picker order. Keep in sync with the
    /// enum variants.
    pub const ALL: [Self; 2] = [Self::Zh, Self::En];

    /// Parse from Telegram `language_code` (e.g. "zh-hans", "en", "ru").
    /// A language matches its base code and any regional subtag of it, so
    /// `zh-hans` -> Zh and `ja-JP` -> Ja.
    pub fn from_language_code(code: Option<&str>) -> Self {
        let Some(code) = code else {
            return Lang::En;
        };
        let lower = code.to_lowercase();
        for lang in Self::ALL {
            let base = lang.db_value();
            if lower == base
                || lower
                    .strip_prefix(base)
                    .is_some_and(|rest| rest.starts_with('-'))
            {
                return lang;
            }
        }
        Lang::En
    }

    /// Parse a stored or callback language code strictly.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|lang| lang.db_value() == code)
    }

    /// Parse from stored DB value.
    pub fn from_db_value(v: &str) -> Self {
        Self::from_code(v).unwrap_or(Lang::En)
    }

    /// DB storage value.
    pub fn db_value(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// Settings key for a user.
    pub fn setting_key(user_id: i64) -> String {
        format!("lang:{user_id}")
    }

    /// Locale key for this language's display name (`language.<code>`).
    /// Derived from `db_value` so the language picker does not need a
    /// per-language match arm; only the language's own locale file defines
    /// this key.
    pub fn label_key(self) -> String {
        format!("language.{}", self.db_value())
    }

    /// The language's own display name, read from its own locale file so a
    /// picker option is never translated by the current UI language.
    pub fn label(self) -> &'static str {
        t(self, &self.label_key())
    }

    fn source(self) -> &'static str {
        match self {
            Lang::Zh => include_str!("../locales/zh.conf"),
            Lang::En => include_str!("../locales/en.conf"),
        }
    }

    fn strings(self) -> &'static HashMap<&'static str, String> {
        static ZH: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
        static EN: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
        match self {
            Lang::Zh => ZH.get_or_init(|| parse(self.source())),
            Lang::En => EN.get_or_init(|| parse(self.source())),
        }
    }
}

/// Translate a static string by key.
/// A missing non-English key falls back to the English text; a missing English
/// key returns the "[?]" marker. The locale tests compare all files and scan
/// every literal key used in the crate.
pub fn t(lang: Lang, key: &str) -> &'static str {
    if let Some(value) = lang.strings().get(key) {
        return value.as_str();
    }
    match lang {
        Lang::En => "[?]",
        _ => t(Lang::En, key),
    }
}

fn parse(source: &'static str) -> HashMap<&'static str, String> {
    let mut strings = HashMap::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        strings.insert(key, unescape(value));
    }
    strings
}

/// Decode the small escape set used by the locale files. Unknown escapes are
/// kept verbatim so a typo is visible instead of silently dropping a character.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Fill `{}` placeholders in a single left-to-right pass.
///
/// Values are inserted verbatim and are never rescanned: a user-controlled
/// value containing `{}` (a channel title, feed title, URL or error chain)
/// cannot shift or swallow the following placeholders. Extra placeholders are
/// left as literal `{}` and extra values are ignored.
pub fn fill(template: &str, values: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut next = 0usize;
    while let Some(pos) = rest.find("{}") {
        out.push_str(&rest[..pos]);
        match values.get(next) {
            Some(v) => {
                out.push_str(v);
                next += 1;
            }
            None => out.push_str("{}"),
        }
        rest = &rest[pos + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    fn raw_keys(source: &str) -> Vec<&str> {
        let mut keys = Vec::new();
        for line in source.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (key, _) = line.split_once('=').expect("locale line without '='");
            keys.push(key.trim());
        }
        keys
    }

    #[test]
    fn language_code_detection() {
        assert_eq!(Lang::from_language_code(Some("zh-hans")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("zh-hant")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("zh")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("ZH-HANS")), Lang::Zh);
        assert_eq!(Lang::from_language_code(Some("en")), Lang::En);
        assert_eq!(Lang::from_language_code(Some("ru")), Lang::En);
        assert_eq!(Lang::from_language_code(Some("ja")), Lang::En);
        assert_eq!(Lang::from_language_code(None), Lang::En);
    }

    #[test]
    fn db_value_roundtrip() {
        assert_eq!(Lang::from_db_value(Lang::Zh.db_value()), Lang::Zh);
        assert_eq!(Lang::from_db_value(Lang::En.db_value()), Lang::En);
        assert_eq!(Lang::from_db_value("unknown"), Lang::En);
        assert_eq!(Lang::from_code("zh"), Some(Lang::Zh));
        assert_eq!(Lang::from_code("en"), Some(Lang::En));
        assert_eq!(Lang::from_code("unknown"), None);
    }

    #[test]
    fn language_labels_are_owned_by_their_locale() {
        for owner in Lang::ALL {
            let key = owner.label_key();
            for lang in Lang::ALL {
                let defined = lang.strings().contains_key(key.as_str());
                assert_eq!(
                    defined,
                    lang == owner,
                    "{key} must be defined only in the {owner:?} locale"
                );
            }
            assert_ne!(owner.label(), "[?]", "missing label for {owner:?}");
            assert!(
                !owner.label().is_empty(),
                "empty {} label",
                owner.label_key()
            );
        }
        assert_eq!(Lang::Zh.label_key(), "language.zh");
        assert_eq!(Lang::En.label_key(), "language.en");
    }

    #[test]
    fn language_picker_labels_are_self_names() {
        // Escapes keep source files outside the CJK allowlist free of CJK.
        assert_eq!(Lang::Zh.label(), "\u{4e2d}\u{6587}");
        assert_eq!(Lang::En.label(), "English");
    }

    #[test]
    fn missing_keys_fall_back_to_marker() {
        for lang in Lang::ALL {
            assert_eq!(t(lang, "__missing_key__"), "[?]");
        }
    }

    #[test]
    fn setting_key_format() {
        assert_eq!(Lang::setting_key(12345), "lang:12345");
    }

    #[test]
    fn all_language_codes_roundtrip() {
        let codes: HashSet<_> = Lang::ALL.iter().map(|lang| lang.db_value()).collect();
        assert_eq!(
            codes.len(),
            Lang::ALL.len(),
            "duplicate language code in Lang::ALL"
        );
        for lang in Lang::ALL {
            assert_eq!(
                Lang::from_db_value(lang.db_value()),
                lang,
                "db roundtrip failed for {lang:?}"
            );
            assert_eq!(
                Lang::from_language_code(Some(lang.db_value())),
                lang,
                "client-code roundtrip failed for {lang:?}"
            );
        }
    }

    #[test]
    fn locale_files_have_matching_non_empty_keys() {
        let baseline = Lang::En.strings();
        assert!(baseline.len() > 200, "only {} en keys", baseline.len());
        let label_keys: HashSet<String> = Lang::ALL.iter().map(|lang| lang.label_key()).collect();
        let baseline_keys: HashSet<_> = baseline
            .keys()
            .copied()
            .filter(|key| !label_keys.contains(*key))
            .collect();
        for lang in Lang::ALL {
            let strings = lang.strings();
            let keys: HashSet<_> = strings
                .keys()
                .copied()
                .filter(|key| !label_keys.contains(*key))
                .collect();
            assert_eq!(keys, baseline_keys, "{lang:?} key set differs from en");
            for (key, value) in strings {
                if label_keys.contains(*key) {
                    continue;
                }
                assert!(!value.is_empty(), "empty {lang:?} translation for {key}");
            }
        }
    }

    #[test]
    fn locale_files_have_no_duplicate_keys_and_use_lf() {
        for lang in Lang::ALL {
            let name = format!("{}.conf", lang.db_value());
            let source = lang.source();
            assert!(!source.contains('\r'), "{name} contains CR; use LF");
            let keys = raw_keys(source);
            let unique: HashSet<_> = keys.iter().copied().collect();
            assert_eq!(keys.len(), unique.len(), "{name} contains duplicate keys");
        }
    }

    /// Keys passed to `i18n::t` through a variable rather than a literal at the
    /// call site. Keep this list in sync with those call sites; a new dynamic
    /// key that is missing here is reported by `locale_files_have_no_unused_keys`,
    /// and a stale entry fails the quoted-literal check below.
    const DYNAMIC_KEYS: &[&str] = &[
        "back",
        "chpage_back",
        "chpage_header",
        "label_entry",
        "label_message",
        "label_source",
        "list_header",
        "notif_media_expired",
        "notif_media_unreadable",
        "notif_timeout_72h",
        "unsub_back",
    ];

    fn source_text() -> String {
        let mut text = String::new();
        for path in rust_sources() {
            if path.file_name().and_then(|name| name.to_str()) == Some("i18n.rs") {
                continue;
            }
            text.push_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|err| panic!("read {}: {err}", path.display())),
            );
            text.push('\n');
        }
        text
    }

    fn referenced_keys() -> (HashSet<String>, usize) {
        let source = source_text();
        let re = regex::Regex::new(r#"i18n::t\s*\(\s*[^,]+,\s*"([a-z0-9_.]+)""#).unwrap();
        let mut keys: HashSet<String> = DYNAMIC_KEYS.iter().map(|key| (*key).to_string()).collect();
        // Language-picker labels are looked up through `Lang::label()` /
        // `label_key()` rather than a literal `i18n::t` call; register every
        // supported key so a new language's missing `language.<code>` fails
        // the key-set test.
        for lang in Lang::ALL {
            keys.insert(lang.label_key());
        }
        let mut literal_count = 0;
        for caps in re.captures_iter(&source) {
            keys.insert(caps[1].to_string());
            literal_count += 1;
        }
        (keys, literal_count)
    }

    #[test]
    fn locale_keys_referenced_in_source_exist() {
        let source = source_text();
        for key in DYNAMIC_KEYS {
            assert!(
                source.contains(&format!("\"{key}\"")),
                "dynamic key {key} is not passed as a quoted literal in source"
            );
        }
        let (referenced, literal_count) = referenced_keys();
        assert!(
            literal_count > 300,
            "only {literal_count} literal i18n::t calls scanned"
        );
        let label_keys: HashSet<String> = Lang::ALL.iter().map(|lang| lang.label_key()).collect();
        for lang in Lang::ALL {
            let strings = lang.strings();
            let own_label = lang.label_key();
            for key in &referenced {
                if label_keys.contains(key) && *key != own_label {
                    continue;
                }
                assert!(
                    strings.contains_key(key.as_str()),
                    "missing {lang:?} key {key}"
                );
            }
        }
    }

    #[test]
    fn locale_files_have_no_unused_keys() {
        let baseline = Lang::En.strings();
        let (referenced, _) = referenced_keys();
        let mut unused: Vec<&str> = baseline
            .keys()
            .copied()
            .filter(|key| !referenced.contains(*key))
            .collect();
        unused.sort_unstable();
        assert!(
            unused.is_empty(),
            "unused i18n keys ({}): {}",
            unused.len(),
            unused.join(", ")
        );
    }

    #[test]
    fn placeholders_match_between_languages() {
        let baseline = Lang::En.strings();
        let label_keys: HashSet<String> = Lang::ALL.iter().map(|lang| lang.label_key()).collect();
        for lang in Lang::ALL {
            if lang == Lang::En {
                continue;
            }
            for (key, value) in lang.strings() {
                let key = *key;
                if label_keys.contains(key) {
                    continue;
                }
                let base = baseline
                    .get(key)
                    .unwrap_or_else(|| panic!("missing en key {key}"));
                assert_eq!(
                    value.matches("{}").count(),
                    base.matches("{}").count(),
                    "{lang:?} placeholder count differs for {key}"
                );
                assert_eq!(
                    value.matches("{:#}").count(),
                    base.matches("{:#}").count(),
                    "{lang:?} error placeholder count differs for {key}"
                );
            }
        }
    }

    #[test]
    fn escape_decoding() {
        assert_eq!(unescape(r"a\nb"), "a\nb");
        assert_eq!(unescape(r#"a\"b"#), "a\"b");
        assert_eq!(unescape(r"a\\b"), "a\\b");
        assert_eq!(unescape(r"a\qb"), r"a\qb");
    }

    #[test]
    fn fill_does_not_rescan_inserted_values() {
        assert_eq!(fill("{} / {}", &["A {}", "B"]), "A {} / B");
        assert_eq!(fill("x={} y={}", &["{}{}", "ok"]), "x={}{} y=ok");
        assert_eq!(fill("no placeholders", &["ignored"]), "no placeholders");
        assert_eq!(fill("{} and {}", &["only-one"]), "only-one and {}");
        assert_eq!(
            fill(
                "<a href=\"{}\">{}</a>",
                &["https://e/x?q=%7B%7D", "A {demo}"]
            ),
            "<a href=\"https://e/x?q=%7B%7D\">A {demo}</a>"
        );
    }

    fn rust_sources() -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read src directory") {
                let path = entry.expect("read src entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &mut out);
        out
    }
}
