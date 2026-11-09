//! Collations: how strings compare.
//!
//! The server's built-in comparison (`convert::compare`, hashing keys, index order) is that of the default collation,
//! `SQL_Latin1_General_CP1_CI_AS`: trailing spaces do not count, case does not matter, accents do. Any other
//! collation is applied by turning a string into a **sort key**: a string that the built-in comparison orders and
//! equates exactly as the collation would. Wherever a comparison, a sort, a grouping, a join or an index key
//! involves a column or expression with a non-default collation, the engine compares sort keys instead of the
//! strings. So the default collation costs nothing, and the others work in indexes, `GROUP BY`, `ORDER BY`, joins,
//! `DISTINCT`, unique constraints and comparisons alike.
//!
//! Supported names: `<family>_<CI|CS>_<AI|AS>[_KS][_WS][_SC][_UTF8]` and `<family>_BIN`, `<family>_BIN2`, where the
//! family is any of the usual Windows or SQL collation families (`Latin1_General`, `SQL_Latin1_General_CP1`,
//! `Latin1_General_100`, `French`, `Turkish`, ...). Only the *sensitivity* options are implemented: letters are ordered
//! by code point of their unaccented, case-folded form (so a language's own alphabetical order, such as Spanish ñ after
//! n or Danish å after z, is not applied), `_KS`/`_WS` (kana and width) are accepted and ignored, and `_BIN` and `_BIN2`
//! both compare by code point.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtOrd};
use std::sync::{Arc, Mutex, OnceLock};
use unicode_normalization::char::{canonical_combining_class, decompose_canonical};

static IN_USE: AtomicBool = AtomicBool::new(false);

/// Has anything non-default been seen (a `COLLATE` clause, a column or database with another collation)? Until it has,
/// the engine skips the work of working out collations.
pub fn in_use() -> bool {
    IN_USE.load(AtOrd::Relaxed)
}

/// Record that a non-default collation exists.
pub fn mark_in_use() {
    IN_USE.store(true, AtOrd::Relaxed);
}

/// The collation every database starts with.
pub const DEFAULT_NAME: &str = "SQL_Latin1_General_CP1_CI_AS";

const FAMILIES: &[&str] = &[
    "SQL_Latin1_General_CP1", "SQL_Latin1_General_CP1250", "SQL_Latin1_General_CP1251", "SQL_Latin1_General_CP1253", "SQL_Latin1_General_CP1254", "SQL_Latin1_General_CP1255",
    "SQL_Latin1_General_CP1256", "SQL_Latin1_General_CP1257", "SQL_Latin1_General_CP437", "SQL_Latin1_General_CP850", "SQL_Latin1_General_Pref_CP1", "SQL_Latin1_General_CP1_CS",
    "Latin1_General", "Latin1_General_100", "Latin1_General_90", "Latin1_General_140", "Latin1_General_PREF", "Albanian", "Arabic", "Chinese_PRC", "Chinese_Taiwan_Stroke", "Cyrillic_General",
    "Czech", "Danish_Norwegian", "Estonian", "Finnish_Swedish", "French", "Georgian_Modern_Sort", "German_PhoneBook", "Greek", "Hebrew", "Hungarian", "Icelandic", "Japanese", "Korean_Wansung",
    "Latvian", "Lithuanian", "Modern_Spanish", "Polish", "Romanian", "Slovak", "Slovenian", "Thai", "Traditional_Spanish", "Turkish", "Ukrainian", "Vietnamese", "Maltese", "Macedonian_FYROM_90",
    "Mohawk_100", "Welsh_100", "Indic_General_90", "Divehi_90", "Persian_100", "Lao_100", "Bengali_100", "Hindi_100",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collation {
    pub name: String,
    pub case_insensitive: bool,
    pub accent_insensitive: bool,
    pub binary: bool,
}

impl Collation {
    /// Parse a collation name (case-insensitive); `None` if it is not one this server understands.
    pub fn parse(name: &str) -> Option<Collation> {
        let up = name.trim().to_uppercase();
        let (family, flags) = if let Some(i) = up.rfind("_BIN2") {
            if i + 5 == up.len() { (&up[..i], vec!["BIN"]) } else { return None }
        } else if let Some(i) = up.rfind("_BIN") {
            if i + 4 == up.len() { (&up[..i], vec!["BIN"]) } else { return None }
        } else {
            // strip trailing option words, then CI/CS and AI/AS
            let mut parts: Vec<&str> = up.split('_').collect();
            let mut flags = vec![];
            while let Some(last) = parts.last() {
                if matches!(*last, "KS" | "WS" | "SC" | "UTF8" | "VSS" | "BIN") {
                    parts.pop();
                } else {
                    break;
                }
            }
            for _ in 0..2 {
                match parts.last() {
                    Some(&f) if matches!(f, "CI" | "CS" | "AI" | "AS") => {
                        flags.push(f);
                        parts.pop();
                    }
                    _ => break,
                }
            }
            if flags.len() != 2 || !(flags.contains(&"CI") ^ flags.contains(&"CS")) || !(flags.contains(&"AI") ^ flags.contains(&"AS")) {
                return None;
            }
            let family: &str = &up[..parts.join("_").len()];
            (family, flags)
        };
        if !FAMILIES.iter().any(|f| f.eq_ignore_ascii_case(family)) {
            return None;
        }
        Some(Collation { name: name.trim().to_string(), case_insensitive: flags.contains(&"CI"), accent_insensitive: flags.contains(&"AI"), binary: flags.contains(&"BIN") })
    }

    /// The built-in comparison already behaves like this collation (case-insensitive, accent-sensitive).
    pub fn is_default(&self) -> bool {
        !self.binary && self.case_insensitive && !self.accent_insensitive
    }

    /// The sort key of `s`: comparing keys with the built-in comparison compares the strings in this collation.
    ///
    /// Non-binary keys are `primary \u{1} secondary \u{1} tertiary`: the string with accents removed and letters
    /// folded to lower case; one character per source character naming its accent (private-use range, or `\u{2}` for
    /// none); and `0`/`1` per character for lower/upper case. They are compared left to right, so letters decide
    /// first, accents next, case last, and a shorter string sorts before a longer one it is a prefix of.
    pub fn key(&self, s: &str) -> String {
        let s = s.trim_end_matches(' ');
        if self.binary {
            // code points as fixed-width hex, so the built-in comparison (which folds case) cannot disturb the order
            let mut out = String::with_capacity(s.len() * 6);
            for c in s.chars() {
                push_hex(&mut out, c as u32, 6);
            }
            return out;
        }
        let mut out = String::with_capacity(s.len() * 2 + 8);
        let (mut secondary, mut tertiary) = (String::new(), String::new());
        for c in s.chars() {
            let (base, accent) = split_accent(c);
            out.push(base.to_lowercase().next().unwrap_or(base));
            if !self.accent_insensitive {
                secondary.push(if accent == 0 { '\u{2}' } else { char::from_u32(0xE000 + accent % 0x1000).unwrap_or('\u{2}') });
            }
            if !self.case_insensitive {
                // lower case sorts before upper case, as in SQL Server's Latin1_General collations
                tertiary.push(if c.is_uppercase() { '1' } else { '0' });
            }
        }
        out.push('\u{1}');
        out.push_str(&secondary);
        out.push('\u{1}');
        out.push_str(&tertiary);
        out
    }

    /// Three-way comparison of two strings in this collation.
    pub fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering {
        if self.is_default() {
            return a.trim_end_matches(' ').to_lowercase().cmp(&b.trim_end_matches(' ').to_lowercase());
        }
        self.key(a).cmp(&self.key(b))
    }

    /// How this collation folds a character before comparing it in a `LIKE` pattern: case and accent are dropped
    /// when the collation ignores them, a binary collation keeps the character as it is.
    pub fn fold_char(&self, c: char) -> char {
        if self.binary {
            return c;
        }
        let base = if self.accent_insensitive { split_accent(c).0 } else { c };
        if self.case_insensitive {
            base.to_lowercase().next().unwrap_or(base)
        } else {
            base
        }
    }
}

fn push_hex(out: &mut String, v: u32, width: usize) {
    use std::fmt::Write;
    let _ = write!(out, "{:0w$x}", v, w = width);
}

/// A character split into its unaccented base and an identifier for its accent (0 for none).
fn split_accent(c: char) -> (char, u32) {
    if c.is_ascii() {
        return (c, 0);
    }
    let mut base: Option<char> = None;
    let mut accent = 0u32;
    decompose_canonical(c, |d| {
        if canonical_combining_class(d) == 0 {
            if base.is_none() {
                base = Some(d);
            }
        } else {
            accent = accent * 31 + d as u32;
        }
    });
    (base.unwrap_or(c), accent & 0xFFFF)
}

/// Parsed collations by name (parsing and key building are cheap, but they happen once per index key).
pub fn get(name: &str) -> Option<Arc<Collation>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<Collation>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut m = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = m.get(name) {
        return c.clone();
    }
    let c = Collation::parse(name).map(Arc::new);
    if c.as_ref().is_some_and(|c| !c.is_default()) {
        mark_in_use();
    }
    m.insert(name.to_string(), c.clone());
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn c(n: &str) -> Collation {
        Collation::parse(n).unwrap_or_else(|| panic!("{} should parse", n))
    }

    #[test]
    fn names_are_parsed_and_checked() {
        assert!(c("SQL_Latin1_General_CP1_CI_AS").is_default());
        assert!(c("Latin1_General_CI_AS").is_default());
        assert!(!c("Latin1_General_CS_AS").is_default());
        assert!(c("latin1_general_100_ci_ai_ks_ws").accent_insensitive);
        assert!(c("Latin1_General_BIN2").binary);
        assert!(c("French_CS_AS").case_insensitive == false);
        for bad in ["", "nonsense", "Latin1_General", "Latin1_General_CI", "Latin1_General_CI_CS", "Klingon_CI_AS", "Latin1_General_CI_AS_X"] {
            assert!(Collation::parse(bad).is_none(), "{} must be rejected", bad);
        }
    }

    #[test]
    fn case_and_accent_sensitivity() {
        let (ci_as, cs_as, ci_ai, cs_ai) = (c("Latin1_General_CI_AS"), c("Latin1_General_CS_AS"), c("Latin1_General_CI_AI"), c("Latin1_General_CS_AI"));
        assert_eq!(ci_as.compare("Hello", "hello"), Equal);
        assert_ne!(ci_as.compare("résumé", "resume"), Equal);
        assert_ne!(cs_as.compare("Hello", "hello"), Equal);
        assert_eq!(cs_as.compare("hello", "Hello"), Less, "lower case sorts first");
        assert_eq!(ci_ai.compare("Résumé", "resume"), Equal);
        assert_eq!(cs_ai.compare("résumé", "resume"), Equal);
        assert_ne!(cs_ai.compare("Resume", "resume"), Equal);
        // trailing spaces never count
        assert_eq!(cs_as.compare("abc  ", "abc"), Equal);
        assert_eq!(cs_as.key("abc  "), cs_as.key("abc"));
        // alphabetical order dominates case, which dominates nothing else: a < B < c even with mixed case
        let mut v = vec!["b", "A", "c", "a", "B"];
        v.sort_by(|x, y| cs_as.compare(x, y));
        assert_eq!(v, ["a", "A", "b", "B", "c"]);
        // a prefix sorts before a longer string
        assert_eq!(cs_as.compare("ab", "abc"), Less);
        assert_eq!(cs_as.compare("AB", "abc"), Less);
    }

    #[test]
    fn binary_collations_compare_code_points() {
        let b = c("Latin1_General_BIN2");
        assert_eq!(b.compare("a", "A"), Greater);
        assert_eq!(b.compare("Z", "a"), Less);
        assert_eq!(b.compare("é", "z"), Greater);
        assert_ne!(b.compare("a", "A"), Equal);
    }

    #[test]
    fn keys_order_like_the_collation_under_the_builtin_comparison() {
        // the key is compared by the built-in (lower-casing) comparison, which must not disturb the order
        let cs = c("Latin1_General_CS_AS");
        let words = ["apple", "Apple", "APPLE", "apples", "Banana", "banana", "äpple", "Äpple"];
        for a in words {
            for b in words {
                let by_key = cs.key(a).to_lowercase().cmp(&cs.key(b).to_lowercase());
                assert_eq!(by_key, cs.compare(a, b), "{} vs {}", a, b);
            }
        }
    }
}
