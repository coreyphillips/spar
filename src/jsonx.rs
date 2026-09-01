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
    candidates(text)
        .into_iter()
        .next()
        .ok_or_else(|| SparError::new(format!("no JSON found in response:\n{}", head(text, 800))))
}

/// Every JSON value plausibly present in a model response, most likely first.
///
/// Fenced blocks come first because a model that fences its answer means it,
/// then whole objects matched backwards from the end.
pub fn candidates(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |value: Value| {
        if !out.contains(&value) {
            out.push(value);
        }
    };

    let fenced: Vec<&str> = FENCE
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    for blob in fenced.iter().rev() {
        if let Ok(value) = serde_json::from_str::<Value>(blob) {
            push(value);
        }
    }

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
                    push(value);
                }
            }
            end = rfind_byte(bytes, closer, e);
        }
    }
    out
}

/// Whether a response looks cut off rather than merely malformed.
///
/// A model with an output limit stops mid-object on a long answer. The braces
/// it opened outnumber the ones it closed, and every complete object left is
/// something nested inside the one it was building.
pub fn looks_truncated(text: &str) -> bool {
    let mut opened = 0i64;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => opened += 1,
            '}' if !in_string => opened -= 1,
            _ => {}
        }
    }
    opened > 0
}

/// Parse a model response straight into a typed value.
///
/// Tries every candidate rather than only the last one found. A review cut off
/// before its outer object closed used to yield the last *nested* finding,
/// which parsed as JSON perfectly well and then failed to be a review, and the
/// error blamed the shape rather than the truncation.
pub fn extract_into<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let found = candidates(text);
    if found.is_empty() {
        return Err(SparError::new(if looks_truncated(text) {
            format!(
                "the response was cut off before any complete JSON:\n{}",
                head(text, 400)
            )
        } else {
            format!("no JSON found in response:\n{}", head(text, 800))
        }));
    }

    // Which failure to report is not the same question as which candidate to
    // parse. Any candidate that parses wins, and a stray object never will,
    // because every schema here requires fields it does not have. But when
    // nothing parses, this error is handed straight back to the model on the
    // retry, so it has to be about the answer the model meant.
    //
    // Neither end of the list is that. The order here is really last-closing
    // first, so the last object a response happens to contain leads, and a
    // model that wrote its answer and then a sentence with an object in it gets
    // told about the sentence. The biggest candidate is the better guess: an
    // answer is longer than the fragments around it.
    let mut failures: Vec<(serde_json::Error, &Value)> = Vec::new();
    for value in &found {
        match serde_json::from_value::<T>(value.clone()) {
            Ok(parsed) => return Ok(parsed),
            Err(e) => failures.push((e, value)),
        }
    }
    let (error, value) = failures
        .into_iter()
        .max_by_key(|(_, value)| value.to_string().len())
        .expect("non-empty");
    if looks_truncated(text) {
        return Err(SparError::new(format!(
            "the response was cut off before the answer was complete, so only fragments of it \
             parsed ({error}). Ask for less in one go, or give this agent a CLI flag for native \
             structured output."
        )));
    }
    Err(SparError::new(format!(
        "response did not match the expected shape ({error}).{}\nGot: {}",
        envelope_hint(value),
        head(&value.to_string(), 600)
    )))
}

/// An extra sentence when serde's own message would send the model to the wrong
/// place.
///
/// serde maps a JSON array onto a struct's fields by position, so a bare array
/// of findings tried as a review fails on field zero: "invalid type: map,
/// expected a string", where the string is `verdict`. A model told that goes
/// looking at a field, and the field is not what is wrong. Every schema here
/// asks for one object, so an array is always the envelope rather than the
/// contents.
fn envelope_hint(value: &Value) -> &'static str {
    if value.is_array() {
        " The answer was a JSON array, and the schema asks for a single object: \
         the array belongs in a field of it."
    } else {
        ""
    }
}

fn rfind_byte(haystack: &[u8], needle: u8, before: usize) -> Option<usize> {
    haystack[..before.min(haystack.len())]
        .iter()
        .rposition(|b| *b == needle)
}

fn head(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The original public finding key, retained for persisted-state and library
/// compatibility.
pub fn finding_key(title: &str, file: &str) -> String {
    let basis: String = format!("{} {}", title.trim(), file.trim())
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '/' | '.' | '_' | '-'))
        .collect();
    let basis = basis.split_whitespace().collect::<Vec<_>>().join(" ");
    short_hash(&basis)
}

/// An exact identity for a review finding within one answer.
///
/// Wording noise, punctuation, and title case are discarded. The full location
/// is retained because the same complaint at two sites in one file is two
/// complaints. Cross-round matching uses `stable_finding_key` as a guarded
/// fallback instead of making this identity lossy.
pub(crate) fn exact_finding_key(title: &str, file: &str) -> String {
    identity_key(title, file.trim())
}

/// A location-tolerant identity used only for unambiguous cross-round matches.
pub(crate) fn stable_finding_key(title: &str, file: &str) -> String {
    identity_key(title, &finding_file(file))
}

/// A title with a leading severity tag removed, such as `[blocking] `.
///
/// Reviewers and authors are told to copy a finding's title across exactly so
/// the two can be matched up, and they mostly do, but one side decorating it
/// with the severity it already reports in its own field is common enough to
/// cost a round: the disposition matches nothing, the finding it answered stays
/// open, and both are reported as unresolved. Only a bracketed tag whose word
/// is a severity is dropped, so a title that genuinely opens with a bracketed
/// subject keeps it and two findings that differ only there stay distinct.
pub(crate) fn untagged_title(title: &str) -> &str {
    let trimmed = title.trim();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return trimmed;
    };
    let Some((tag, rest)) = rest.split_once(']') else {
        return trimmed;
    };
    if crate::model::Severity::parse_lenient(tag.trim()).is_none() {
        return trimmed;
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return trimmed;
    }
    rest
}

fn identity_key(title: &str, file: &str) -> String {
    let title: String = untagged_title(title)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let basis = format!("{title}\0{file}");

    short_hash(&basis)
}

fn short_hash(basis: &str) -> String {
    let digest = Sha256::digest(basis.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_string()
}

/// A finding's repository path without an optional trailing line or column.
///
/// Review locations move as fixes land. The path identifies the point across
/// rounds, while the full location is still kept on the finding for display.
pub(crate) fn finding_file(file: &str) -> String {
    let mut path = file.trim();
    while let Some((head, suffix)) = path.rsplit_once(':') {
        let is_number = !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit());
        let is_range = suffix.split_once('-').is_some_and(|(start, end)| {
            !start.is_empty()
                && !end.is_empty()
                && start.chars().all(|c| c.is_ascii_digit())
                && end.chars().all(|c| c.is_ascii_digit())
        });
        if !is_number && !is_range {
            break;
        }
        path = head.trim_end();
    }
    path.to_string()
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
    fn public_key_keeps_its_original_case_insensitive_paths() {
        assert_eq!(
            finding_key("t", "src/Main.rs"),
            finding_key("t", "src/main.rs")
        );
        assert_ne!(
            exact_finding_key("t", "src/Main.rs"),
            exact_finding_key("t", "src/main.rs")
        );
    }

    #[test]
    fn key_is_stable_across_whitespace() {
        assert_eq!(finding_key("a  b", "x.rs"), finding_key(" a b ", "x.rs"));
    }

    #[test]
    fn a_severity_tag_does_not_change_a_title() {
        assert_eq!(
            "Unbounded loop",
            untagged_title("[blocking] Unbounded loop")
        );
        assert_eq!("Unbounded loop", untagged_title("  [ NIT ]Unbounded loop "));
        assert_eq!(
            exact_finding_key("Unbounded loop", "x.rs"),
            exact_finding_key("[non-blocking] Unbounded loop", "x.rs")
        );
    }

    #[test]
    fn a_bracketed_subject_is_part_of_the_title() {
        assert_eq!("[iOS] Startup crash", untagged_title("[iOS] Startup crash"));
        assert_eq!("[blocking]", untagged_title("[blocking]"));
        assert_eq!("[blocking Unbounded", untagged_title("[blocking Unbounded"));
        assert_ne!(
            exact_finding_key("[iOS] Startup crash", "x.rs"),
            exact_finding_key("[Android] Startup crash", "x.rs")
        );
    }

    #[test]
    fn exact_key_keeps_distinct_locations() {
        assert_ne!(
            exact_finding_key("t", "src/net.rs:88"),
            exact_finding_key("t", "src/net.rs:91")
        );
        assert_eq!(
            stable_finding_key("t", "src/net.rs:88"),
            stable_finding_key("t", "src/net.rs:91")
        );
        assert_eq!(
            stable_finding_key("t", "src/net.rs:88-94"),
            stable_finding_key("t", "src/net.rs")
        );
        assert_eq!(
            stable_finding_key("t", "src/net.rs:88:12"),
            stable_finding_key("t", "src/net.rs")
        );
    }

    #[test]
    fn a_numeric_filename_is_not_treated_as_a_line_number() {
        assert_eq!("fixtures/2024", finding_file("fixtures/2024"));
    }

    #[test]
    fn key_is_twelve_hex_characters() {
        let key = finding_key("anything", "file.rs");
        assert_eq!(12, key.len());
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Review {
        verdict: String,
        findings: Vec<Finding>,
    }
    #[derive(Debug, Deserialize)]
    struct Finding {
        title: String,
    }

    /// What actually happened on a real pull request. A long review hit the
    /// model's output limit and stopped before its outer object closed, so the
    /// last complete JSON in the response was a nested finding. It parsed, it
    /// was not a review, and the error blamed the shape.
    const TRUNCATED: &str = r#"Here is my review.
{"verdict":"changes_requested","next_action":"hand_back","summary":"Two problems.",
 "findings":[
   {"severity":"blocking","title":"First","detail":"one","file":"a.ts","in_scope":true},
   {"severity":"non-blocking","title":"Second","detail":"numbers unnamed keys by position"#;

    #[test]
    fn a_truncated_review_is_reported_as_truncated_not_as_the_wrong_shape() {
        let err = extract_into::<Review>(TRUNCATED).unwrap_err().to_string();
        assert!(err.contains("cut off"), "{err}");
        assert!(!err.contains("did not match the expected shape"), "{err}");
    }

    #[test]
    fn truncation_is_detected_from_the_unclosed_braces() {
        assert!(looks_truncated(TRUNCATED));
        assert!(!looks_truncated(r#"{"a":1}"#));
        // A brace inside a string is not an open brace.
        assert!(!looks_truncated(r#"{"a":"a { in a string"}"#));
        assert!(!looks_truncated(r#"{"a":"an escaped \" quote { here"}"#));
    }

    /// The fix that matters: the right object is found even when it is not the
    /// last one in the response.
    #[test]
    fn the_review_is_found_even_with_nested_objects_after_it() {
        let text = r#"Thinking out loud first.
{"verdict":"approve","next_action":"merge","summary":"Fine.","findings":[{"severity":"nit","title":"Wording","detail":"d","file":"a.ts","in_scope":true}]}
And here is a stray object afterwards: {"title":"not the review"}"#;
        let review: Review = extract_into(text).unwrap();
        assert_eq!("approve", review.verdict);
        assert_eq!(1, review.findings.len());
        assert_eq!("Wording", review.findings[0].title);
    }

    #[test]
    fn candidates_are_offered_most_likely_first() {
        let text = "```json\n{\"verdict\":\"approve\",\"findings\":[]}\n```\ntrailing {\"x\":1}";
        let review: Review = extract_into(text).unwrap();
        assert_eq!("approve", review.verdict);
    }

    #[test]
    fn a_genuinely_wrong_shape_still_says_so() {
        let err = extract_into::<Review>(r#"{"colour":"blue"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not match the expected shape"), "{err}");
        assert!(!err.contains("cut off"), "{err}");
    }

    /// What a real round two review produced. The model wrote a review object
    /// and a findings array, the object failed to parse, and the complaint that
    /// went back to it described the array: "invalid type: map, expected a
    /// string", which is field zero of a struct serde had mapped an array onto
    /// by position. The model was sent to look at a field, and the field was
    /// not what was wrong.
    #[test]
    fn the_complaint_is_about_the_answer_the_model_meant() {
        let text = r#"Here is my review.
{"verdict":"changes_requested","next_action":"hand_back","summary":"Two problems.","findings":"should have been a list"}
Supporting detail: [{"detail":"The working tree bumps 0.5.9 to 0.5.10."}]"#;
        let err = extract_into::<Review>(text).unwrap_err().to_string();
        // The review object is what failed, and its own field is named.
        assert!(err.contains("findings"), "{err}");
        assert!(
            err.contains("changes_requested"),
            "the object is shown:\n{err}"
        );
        assert!(
            !err.contains("The working tree"),
            "the stray array leaked in:\n{err}"
        );
    }

    /// serde maps a JSON array onto a struct by position, so a bare array of
    /// findings fails on the first field and says "expected a string". Left at
    /// that, the model reads it as a field problem.
    #[test]
    fn a_bare_array_is_named_as_the_envelope_problem() {
        let err = extract_into::<Review>(r#"[{"title":"First"},{"title":"Second"}]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("JSON array"), "{err}");
        assert!(err.contains("single object"), "{err}");
    }

    /// And an object that is merely the wrong shape says nothing about arrays.
    #[test]
    fn a_wrong_object_is_not_told_it_was_an_array() {
        let err = extract_into::<Review>(r#"{"colour":"blue"}"#)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("JSON array"), "{err}");
    }

    #[test]
    fn nothing_parseable_is_still_reported_plainly() {
        let err = extract_into::<Review>("no json at all")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no JSON found"), "{err}");
    }
}
