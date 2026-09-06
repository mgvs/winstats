//! UI strings. Every locale lives in `locales/<code>.toml` (embedded at build time); keys are
//! `section.key`. Missing keys fall back to English. The language comes from the config or, for
//! "auto", from the Windows UI language.

use std::cell::RefCell;
use std::collections::HashMap;

/// (code, autonym, embedded TOML). The order is the order of the Language menu.
pub const LOCALES: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../locales/en.toml")),
];

thread_local! {
    static TABLE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    static FALLBACK: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    static CURRENT: RefCell<String> = RefCell::new(String::from("en"));
}

fn flatten(prefix: &str, v: &toml::Value, out: &mut HashMap<String, String>) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, v, out);
            }
        }
        toml::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

fn parse(code: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some((_, _, text)) = LOCALES.iter().find(|(c, _, _)| *c == code) {
        match text.parse::<toml::Value>() {
            Ok(v) => flatten("", &v, &mut out),
            Err(e) => crate::log!("locale {code}: parse error: {e}"),
        }
    }
    out
}

/// Windows UI language -> locale code, or "en".
pub fn detect() -> &'static str {
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    let primary = id & 0x3FF;
    let sub = id >> 10;
    match primary {
        0x09 => {
            if sub == 2 { "en-GB" } else { "en" }
        }
        0x07 => {
            if sub == 2 { "de-CH" } else { "de" }
        }
        0x0C => {
            if sub == 3 { "fr-CA" } else { "fr" }
        }
        0x04 => match sub {
            1 | 3 | 5 => "zh-Hant",
            _ => "zh",
        },
        0x02 => "bg",
        0x05 => "cs",
        0x06 => "da",
        0x08 => "el",
        0x0A => "es",
        0x25 => "et",
        0x0E => "hu",
        0x10 => "it",
        0x11 => "ja",
        0x27 => "lt",
        0x26 => "lv",
        0x14 => "nb",
        0x13 => "nl",
        0x15 => "pl",
        0x16 => "pt",
        0x18 => "ro",
        0x19 => "ru",
        0x1A => "sr",
        0x1D => "sv",
        0x1F => "tr",
        0x22 => "uk",
        _ => "en",
    }
}

/// Activate a locale ("auto" picks the Windows UI language).
pub fn set_language(code: &str) {
    let code = if code == "auto" || code.is_empty() { detect() } else { code };
    let code = if LOCALES.iter().any(|(c, _, _)| *c == code) { code } else { "en" };
    FALLBACK.with(|f| {
        if f.borrow().is_empty() {
            *f.borrow_mut() = parse("en");
        }
    });
    TABLE.with(|t| *t.borrow_mut() = parse(code));
    CURRENT.with(|c| *c.borrow_mut() = code.to_string());
}

/// Translated string for `key`, English if the locale lacks it, the key itself if nobody has it.
pub fn t(key: &str) -> String {
    if let Some(s) = TABLE.with(|t| t.borrow().get(key).cloned()) {
        return s;
    }
    if let Some(s) = FALLBACK.with(|f| f.borrow().get(key).cloned()) {
        return s;
    }
    key.to_string()
}

/// `t(key)` with `{name}` placeholders replaced.
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    let mut s = t(key);
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), value);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every embedded locale parses and carries exactly the English key set with the same
    /// placeholders.
    #[test]
    fn locales_are_complete() {
        let en = parse("en");
        assert!(!en.is_empty());
        for (code, _, text) in LOCALES {
            text.parse::<toml::Value>().unwrap_or_else(|e| panic!("{code}: {e}"));
            let table = parse(code);
            for (key, value) in &en {
                let other = table.get(key).unwrap_or_else(|| panic!("{code}: missing {key}"));
                for ph in value.split('{').skip(1).filter_map(|s| s.split('}').next()) {
                    assert!(other.contains(&format!("{{{ph}}}")), "{code}: {key} lacks {{{ph}}}");
                }
            }
            for key in table.keys() {
                assert!(en.contains_key(key), "{code}: extra key {key}");
            }
        }
    }
}
