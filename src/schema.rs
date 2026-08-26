//! JSON schemas for the three structured exchanges.
//!
//! These are what make convergence machine checkable instead of regex matching
//! prose for "LGTM". Three properties are load bearing for strict structured
//! output and are asserted in the tests: every property appears in `required`,
//! every object sets `additionalProperties: false`, and an optional field is
//! spelled as one that may be null rather than one that may be absent.
//!
//! The `description` on each field is also the cheapest place to ask for
//! brevity, since it travels with the request rather than sitting a thousand
//! tokens back in the prompt.

use serde_json::{json, Value};

pub fn triage() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "issues": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "issue": {"type": "integer", "description": "The issue number."},
                        "worth_doing": {
                            "type": "boolean",
                            "description": "False for duplicates, stale requests, things already fixed, vague reports with nothing reproducible, changes that would make the codebase worse, and tracking issues. Set tracker as well for the last of those."
                        },
                        "tracker": {
                            "type": "boolean",
                            "description": "True when the issue exists to hold context for work that is filed elsewhere: an umbrella, an epic, a meta issue whose parts are their own issues. Judge it by what the issue is, not by whether you agree with it. Nothing is opened for a tracker, but it is not finished either: its parts are still open, and the shared context and rejected alternatives it records are why somebody wrote it. False for an ordinary issue, whatever you decided about it."
                        },
                        "reason": {
                            "type": "string",
                            "description": "One sentence. This is posted verbatim on the issue when both agents decline it, and the issue may well stay open afterwards, so write it for the person who opened it rather than as a verdict."
                        },
                        "complexity": {"type": "string", "enum": ["s", "m", "l"]},
                        "depends_on": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "description": "Issue numbers from this same list that should land first. Empty if none."
                        },
                        "risk": {"type": "string", "enum": ["low", "med", "high"]}
                    },
                    "required": ["issue", "worth_doing", "tracker", "reason", "complexity", "depends_on", "risk"]
                }
            }
        },
        "required": ["issues"]
    })
}

pub fn review() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "verdict": {"type": "string", "enum": ["approve", "changes_requested"]},
            "next_action": {"type": "string", "enum": ["merge", "fix_myself", "hand_back"]},
            "summary": {
                "type": "string",
                "description": "One sentence, at most 200 characters. No preamble, no restating the diff."
            },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "severity": {
                            "type": "string",
                            "enum": ["blocking", "non-blocking", "nit"],
                            "description": "blocking: the PR should not merge as is, real defects only. non-blocking: a genuine improvement that need not gate this PR. nit: style or taste."
                        },
                        "title": {
                            "type": "string",
                            "description": "Under 80 characters. State the defect, not the fix."
                        },
                        "detail": {
                            "type": "string",
                            "description": "Say what goes wrong, how to reproduce it, and where in the code. For a blocking finding, say what you did to confirm it. Do not restate the title. Lead with one sentence that stands on its own: a shortened form of this appears in the pull request thread, while the full text becomes the body if this is filed as its own issue. A fenced code block is welcome and is never truncated."
                        },
                        "file": {
                            "type": "string",
                            "description": "Path, with a line number if you have one. Empty string if the finding is general."
                        },
                        "problem": {
                            "type": ["string", "null"],
                            "description": "Only when in_scope is false, null otherwise. What is wrong, with the specifics: the function, the call it does not make, the condition it does not check. Name things in backticks. This becomes the Problem section of an issue somebody picks up cold, so write what they need rather than what fits on a line."
                        },
                        "reproduction": {
                            "type": ["string", "null"],
                            "description": "Only when in_scope is false, null otherwise. Numbered steps to reproduce it, then a short 'Actual result:' list of what happens. If part of what happens is correct and only part is the defect, say which, so nobody chases the wrong thing."
                        },
                        "impact": {
                            "type": ["string", "null"],
                            "description": "Only when in_scope is false, null otherwise. What it costs somebody: what an operator or a user can do, or loses, because of this. One short paragraph."
                        },
                        "expected": {
                            "type": ["string", "null"],
                            "description": "Only when in_scope is false, null otherwise. What it should do instead, as a list of requirements specific enough to implement and to test. Say if the behaviour predates this branch."
                        },
                        "in_scope": {
                            "type": "boolean",
                            "description": "False only for a real defect that exists, that this PR did not cause, and that is worth somebody stopping to fix. It becomes a tracked item a maintainer has to read and triage, so the bar is a defect, not an observation. A thorough reviewer can always find something adjacent; that is not a reason to file it. If you are not sure it is worth a maintainer's time, leave this true and say your piece in the finding."
                        }
                    },
                    "required": [
                        "severity",
                        "title",
                        "detail",
                        "file",
                        "in_scope",
                        "problem",
                        "reproduction",
                        "impact",
                        "expected"
                    ]
                }
            }
        },
        "required": ["verdict", "next_action", "summary", "findings"]
    })
}

/// What the implementor reports back, and the pull request body it becomes.
///
/// The body used to be one scraped `SUMMARY:` line under a `Closes #N`, which
/// told a reviewer opening the diff cold nothing: not what was wrong, not what
/// the change does about it, not how to check it. Asking for those separately
/// is what puts them there, and composing the body from the fields rather than
/// from the model's prose is what keeps it short enough to read.
pub fn implementation() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "not_worth_doing": {
                "type": "boolean",
                "description": "True if, having read the code, this should not be implemented: a duplicate, already fixed, too vague to act on, or a change that would make the codebase worse. Make no commits when this is true."
            },
            "reason": {
                "type": "string",
                "description": "Only when not_worth_doing is true, empty string otherwise. One or two sentences, posted verbatim on the issue, so write it for the person who opened it."
            },
            "summary": {
                "type": "string",
                "description": "One sentence, at most 200 characters, saying what changed. It leads the pull request body, so say what changed rather than that you changed something. No preamble."
            },
            "problem": {
                "type": "string",
                "description": "Two or three sentences on what was actually wrong and what it cost, as you understand it now that you have read the code. Not a restatement of the issue, which the reviewer can open for themselves: what you found. Empty string for a feature request with no defect behind it, where a sentence on why it is worth having belongs here instead."
            },
            "changes": {
                "type": "array",
                "items": {"type": "string"},
                "description": "One short line per change that alters behaviour, in the order a reader should meet them. Say what the code now does, and name the function or file in backticks. Not a list of touched files: the diff already has that. Empty when the summary covers it, which for a small change it does."
            },
            "testing": {
                "type": "array",
                "items": {"type": "string"},
                "description": "How a reviewer confirms this works, as lines they can act on: the exact command in backticks, or the steps and what to look for. Say what you actually ran, not what could be run. Name the test that covers the fix. Empty only when there is genuinely nothing to run."
            },
            "notes": {
                "type": ["string", "null"],
                "description": "Null unless there is something the reviewer would otherwise have to ask about: a deliberate omission, a decision worth defending, a risk you are taking knowingly. Not a summary of the above, and not an apology."
            }
        },
        "required": ["not_worth_doing", "reason", "summary", "problem", "changes", "testing", "notes"]
    })
}

pub fn response() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": {
                "type": "string",
                "description": "One sentence, at most 200 characters."
            },
            "dispositions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Copy the reviewer's finding title exactly, so the two can be matched up."
                        },
                        "file": {
                            "type": "string",
                            "description": "Copy the reviewer's file for this finding exactly. Empty string if it had none."
                        },
                        "action": {
                            "type": "string",
                            "enum": ["fixed", "refuted", "filed_issue"],
                            "description": "fixed: valid and in scope, you fixed it. refuted: the point is wrong or not worth acting on. filed_issue: valid but unrelated to this PR."
                        },
                        "reasoning": {
                            "type": "string",
                            "description": "One or two sentences. For a refutation this is the whole argument, so make it the reason and not an apology."
                        },
                        "new_issue_title": {
                            "type": ["string", "null"],
                            "description": "Only for filed_issue, null otherwise."
                        },
                        "new_issue_body": {
                            "type": ["string", "null"],
                            "description": "Only for filed_issue, null otherwise. This becomes an issue body somebody picks up cold, so use these markdown sections, skipping any that do not apply: `## Problem` with the specifics, `## Reproduction` with numbered steps and an Actual result list, `## Impact` with what it costs somebody, and `## Expected behavior` as requirements specific enough to implement and to test. Substance rather than length: no preamble, no restating the title. A fenced code block is welcome and is never truncated."
                        }
                    },
                    "required": ["title", "file", "action", "reasoning", "new_issue_title", "new_issue_body"]
                }
            }
        },
        "required": ["summary", "dispositions"]
    })
}

/// One reviewer judging the other reviewer's findings.
///
/// Used only in review only mode, where nobody is going to fix anything and the
/// product is the finding list itself. Asking each model to read the code and
/// rule on the other's claims is what separates a defect worth a maintainer's
/// attention from one model's pattern match.
pub fn adjudication() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "verdicts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Copy the finding's title exactly, so it can be matched up."
                        },
                        "file": {
                            "type": "string",
                            "description": "Copy the finding's file exactly. Empty string if it had none."
                        },
                        "agrees": {
                            "type": "boolean",
                            "description": "True only if you read the code and the defect is real. Do not defer to the other reviewer, and do not agree to be agreeable: a finding you cannot confirm is one a maintainer should not have to spend time on."
                        },
                        "severity": {
                            "type": "string",
                            "enum": ["blocking", "non-blocking", "nit"],
                            "description": "Your own view of how badly it matters, even where you agree the defect is real."
                        },
                        "reasoning": {
                            "type": "string",
                            "description": "One or two sentences. If you disagree, this is the whole argument, so give the reason rather than an opinion."
                        }
                    },
                    "required": ["title", "file", "agrees", "severity", "reasoning"]
                }
            }
        },
        "required": ["verdicts"]
    })
}

pub fn all() -> Vec<(&'static str, Value)> {
    vec![
        ("triage", triage()),
        ("implementation", implementation()),
        ("review", review()),
        ("response", response()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yield every object schema, however deeply nested.
    fn objects(node: &Value, path: String, out: &mut Vec<(String, Value)>) {
        if let Some(map) = node.as_object() {
            if map.get("type").and_then(Value::as_str) == Some("object")
                && map.contains_key("properties")
            {
                out.push((path.clone(), node.clone()));
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (key, child) in props {
                        objects(child, format!("{path}.{key}"), out);
                    }
                }
            }
            if let Some(items) = map.get("items") {
                objects(items, format!("{path}[]"), out);
            }
        }
    }

    fn walk(name: &str, schema: &Value) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        objects(schema, name.to_string(), &mut out);
        out
    }

    /// Strict structured output rejects any property that is not also in
    /// `required`. The Python original violated this in the response schema
    /// from the start and nothing caught it, because the response schema is
    /// only reached when a review is handed back with blocking findings, and
    /// almost every run approved in round one.
    #[test]
    fn every_property_is_required() {
        for (name, schema) in all() {
            for (path, node) in walk(name, &schema) {
                let props: Vec<&String> = node["properties"].as_object().unwrap().keys().collect();
                let required: Vec<String> = node["required"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                for prop in &props {
                    assert!(
                        required.contains(prop),
                        "{path}: {prop} is in properties but not in required. \
                         Make optional fields nullable instead."
                    );
                }
                assert_eq!(props.len(), required.len(), "{path}: required has extras");
            }
        }
    }

    /// The guard that was missing. A schema field can be added to the struct
    /// and forgotten in the schema, and every test still passes: the tests
    /// build the struct in Rust, so they never notice the model was never
    /// asked. That shipped once, as four bug-report fields the agents were
    /// never told about, which quietly did nothing.
    #[test]
    fn the_review_schema_asks_for_every_field_a_finding_holds() {
        use crate::model::Finding;

        let asked: Vec<String> = review()["properties"]["findings"]["items"]["properties"]
            .as_object()
            .expect("finding properties")
            .keys()
            .cloned()
            .collect();

        // Round-tripping a fully populated Finding names every field serde
        // knows about, without repeating the list here to drift out of date.
        let populated = Finding {
            problem: Some("p".into()),
            reproduction: Some("r".into()),
            impact: Some("i".into()),
            expected: Some("e".into()),
            ..Finding::default()
        };
        let held: Vec<String> = serde_json::to_value(&populated)
            .expect("serialisable")
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();

        for field in &held {
            assert!(
                asked.contains(field),
                "a Finding holds `{field}` and the schema never asks for it, so the model will \
                 not fill it and the code reading it will always see nothing"
            );
        }
    }

    /// The same guard for the implementation exchange, where a forgotten field
    /// means a pull request body with an empty section in it and nobody the
    /// wiser.
    #[test]
    fn the_implementation_schema_asks_for_every_field_it_holds() {
        use crate::model::Implementation;

        let asked: Vec<String> = implementation()["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect();

        let populated = Implementation {
            notes: Some("n".into()),
            ..Implementation::default()
        };
        let held: Vec<String> = serde_json::to_value(&populated)
            .expect("serialisable")
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();

        for field in &held {
            assert!(
                asked.contains(field),
                "an Implementation holds `{field}` and the schema never asks for it, so the \
                 pull request body will always be missing that part"
            );
        }
    }

    /// Same guard for the other direction of the same exchange.
    #[test]
    fn the_response_schema_asks_for_every_field_a_disposition_holds() {
        use crate::model::{Action, Disposition};

        let asked: Vec<String> = response()["properties"]["dispositions"]["items"]["properties"]
            .as_object()
            .expect("disposition properties")
            .keys()
            .cloned()
            .collect();

        let populated = Disposition {
            title: "t".into(),
            file: "f".into(),
            action: Action::Fixed,
            reasoning: "r".into(),
            new_issue_title: Some("t".into()),
            new_issue_body: Some("b".into()),
        };
        let held: Vec<String> = serde_json::to_value(&populated)
            .expect("serialisable")
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();

        for field in &held {
            assert!(
                asked.contains(field),
                "a Disposition holds `{field}`, unasked for"
            );
        }
    }

    #[test]
    fn objects_forbid_additional_properties() {
        for (name, schema) in all() {
            for (path, node) in walk(name, &schema) {
                assert_eq!(
                    Some(false),
                    node["additionalProperties"].as_bool(),
                    "{path} allows additional properties"
                );
            }
        }
    }

    #[test]
    fn optional_fields_are_spelled_as_nullable() {
        let item = &response()["properties"]["dispositions"]["items"];
        for field in ["new_issue_title", "new_issue_body"] {
            let types = item["properties"][field]["type"].to_string();
            assert!(types.contains("null"), "{field} must accept null: {types}");
        }
    }

    /// The re-litigation guard hashes a refutation by title *and* file. If the
    /// disposition cannot carry the file, the key it records can never match
    /// the key the next round's finding hashes to, and the guard is dead code.
    #[test]
    fn a_disposition_carries_the_file_so_the_ledger_key_can_match() {
        let props = response()["properties"]["dispositions"]["items"]["properties"].clone();
        assert!(
            props.get("file").is_some(),
            "dispositions must carry a file"
        );
    }

    #[test]
    fn severity_and_verdict_enums_match_the_parser() {
        use crate::model::{Severity, Verdict};
        let sev =
            review()["properties"]["findings"]["items"]["properties"]["severity"]["enum"].clone();
        for value in sev.as_array().unwrap() {
            assert!(
                Severity::parse_lenient(value.as_str().unwrap()).is_some(),
                "schema offers {value} but the parser rejects it"
            );
        }
        let verdicts = review()["properties"]["verdict"]["enum"].clone();
        for value in verdicts.as_array().unwrap() {
            assert!(Verdict::parse_lenient(value.as_str().unwrap()).is_some());
        }
    }

    #[test]
    fn triage_enums_match_the_parser() {
        use crate::model::{Complexity, Risk};
        let item = &triage()["properties"]["issues"]["items"]["properties"];
        for value in item["complexity"]["enum"].as_array().unwrap() {
            assert!(Complexity::parse_lenient(value.as_str().unwrap()).is_some());
        }
        for value in item["risk"]["enum"].as_array().unwrap() {
            assert!(Risk::parse_lenient(value.as_str().unwrap()).is_some());
        }
    }

    #[test]
    fn response_action_enum_matches_the_parser() {
        use crate::model::Action;
        let actions = response()["properties"]["dispositions"]["items"]["properties"]["action"]
            ["enum"]
            .clone();
        for value in actions.as_array().unwrap() {
            assert!(Action::parse_lenient(value.as_str().unwrap()).is_some());
        }
    }
}
