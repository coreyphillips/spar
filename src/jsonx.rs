//! Getting structured data back out of a model, and hashing it stably.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use regex::Regex;

use crate::error::{Result, SparError};

static FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)```(?:json)?\s*(\{.*?\}|\[.*?\])\s*```").expect("fence pattern")
});

/// Pull the last JSON value out of a model response.
///
/// Models wrap JSON in prose or fences despite explicit instructions not to,
/// and some emit a draft before the real answer, so the *last* well formed
/// value wins: fenced blocks first, then brace matching backwards from the end.
pub fn extract_json(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Err(SparError::new("empty response, expected JSON"));
    }

    let fenced: Vec<&str> = FENCE
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    for blob in fenced.iter().rev() {
        if let Ok(value) = serde_json::from_str::<Value>(blob) {
            return Ok(value);
        }
    }

    // Delimiters are ASCII and UTF-8 is self synchronising, so byte scanning
    // can never land inside a multi byte character.
    let bytes = text.as_bytes();
    for (opener, closer) in [(b'{', b'}'), (b'[', b']')] {
        let mut end = rfind_byte(bytes, closer, bytes.len());
        while let Some(e) = end {
            let mut depth = 0i32;
            let mut start = None;
            for i in (0..=e).rev() {
                if bytes[i] == closer {
                    depth += 1;
                } else if bytes[i] == opener {
                    depth -= 1;
                    if depth == 0 {
                        start = Some(i);
                        break;
                    }
                }
            }
            if let Some(s) = start {
                if let Ok(value) = serde_json::from_str::<Value>(&text[s..=e]) {
                    return Ok(value);
                }
            }
            end = rfind_byte(bytes, closer, e);
        }
    }

    Err(SparError::new(format!(
        "no JSON found in response:\n{}",
        head(text, 800)
    )))
}

/// Parse a model response straight into a typed value.
pub fn extract_into<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let value = extract_json(text)?;
    serde_json::from_value(value.clone()).map_err(|e| {
        SparError::new(format!(
            "response did not match the expected shape ({e}).\nGot: {}",
            head(&value.to_string(), 600)
        ))
    })
}

fn rfind_byte(haystack: &[u8], needle: u8, before: usize) -> Option<usize> {
    haystack[..before.min(haystack.len())]
        .iter()
        .rposition(|b| *b == needle)
}

fn head(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// A stable identity for a review finding, so a refutation survives across
/// rounds even when the reviewer rewords the point.
///
/// Wording noise, punctuation, and case are all discarded; the file is not,
/// because the same complaint about two different files is two complaints.
pub fn finding_key(title: &str, file: &str) -> String {
    let basis: String = format!("{} {}", title.trim(), file.trim())
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '/' | '.' | '_' | '-'))
        .collect();
    let basis = basis.split_whitespace().collect::<Vec<_>>().join(" ");

    let digest = Sha256::digest(basis.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_object() {
        assert_eq!(
            serde_json::json!({"a": 1}),
            extract_json(r#"{"a": 1}"#).unwrap()
        );
    }

    #[test]
    fn fenced_block() {
        let out = extract_json("here you go:\n```json\n{\"a\": 1}\n```\n").unwrap();
        assert_eq!(serde_json::json!({"a": 1}), out);
    }

    #[test]
    fn trailing_prose() {
        let out = extract_json("Thoughts...\n{\"verdict\": \"approve\"}\nDone.").unwrap();
        assert_eq!(serde_json::json!({"verdict": "approve"}), out);
    }

    #[test]
    fn picks_the_last_fenced_block() {
        let text = "```json\n{\"n\": 1}\n```\nrevised:\n```json\n{\"n\": 2}\n```";
        assert_eq!(serde_json::json!({"n": 2}), extract_json(text).unwrap());
    }

    #[test]
    fn nested_braces() {
        let payload = r#"{"findings": [{"severity": "nit", "d": {"x": [1, 2]}}]}"#;
        let out = extract_json(&format!("blah {payload} blah")).unwrap();
        assert_eq!(1, out["findings"].as_array().unwrap().len());
    }

    #[test]
    fn top_level_array() {
        let out = extract_json("result: [1, 2, 3]").unwrap();
        assert_eq!(3, out.as_array().unwrap().len());
    }

    #[test]
    fn multibyte_prose_around_the_payload_does_not_panic() {
        let out = extract_json("\u{1f600}\u{1f600} {\"a\": 1} \u{1f600}").unwrap();
        assert_eq!(serde_json::json!({"a": 1}), out);
    }

    #[test]
    fn raises_when_there_is_none() {
        assert!(extract_json("no json here at all").is_err());
    }

    #[test]
    fn raises_on_empty() {
        assert!(extract_json("   ").is_err());
    }

    #[test]
    fn malformed_trailing_object_falls_back_to_an_earlier_one() {
        let text = "{\"good\": true}\nthen: {\"bad\": ,}";
        assert_eq!(
            serde_json::json!({"good": true}),
            extract_json(text).unwrap()
        );
    }

    // -- finding_key -----------------------------------------------------

    #[test]
    fn key_is_stable_across_wording_noise() {
        assert_eq!(
            finding_key("Unbounded loop!", "src/x.rs"),
            finding_key("unbounded loop", "src/x.rs")
        );
    }

    #[test]
    fn key_differs_by_file() {
        assert_ne!(finding_key("t", "a.rs"), finding_key("t", "b.rs"));
    }

    #[test]
    fn key_is_case_insensitive_in_the_path_too() {
        assert_eq!(
            finding_key("t", "src/Main.rs"),
            finding_key("t", "src/main.rs")
        );
    }

    #[test]
    fn key_is_stable_across_whitespace() {
        assert_eq!(finding_key("a  b", "x.rs"), finding_key(" a b ", "x.rs"));
    }

    #[test]
    fn key_is_twelve_hex_characters() {
        let key = finding_key("anything", "file.rs");
        assert_eq!(12, key.len());
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
