//! What a run spent.
//!
//! A run makes many agent calls at scheduled effort, and the terminal report
//! said nothing about them: no count per agent, no wall time, no retries or
//! hand-overs. `max_rounds`, `absorb_new_issues`, `max_followups` and the
//! effort schedule are all documented in terms of what they cost, and nothing
//! in the output let anybody see whether changing one did what they hoped.
//!
//! Ambient on purpose. Threading a counter through every call site would put
//! accounting in the signature of everything that asks a model a question, and
//! the one thing this must not do is change what the loop decides.

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One call to one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spent {
    pub agent: String,
    /// The kind of call, as the effort schedule names it: `triage`,
    /// `review_1`, `respond`, and the rest.
    pub kind: String,
    /// What it was asked for, empty when the CLI's own default was used.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// The issue or pull request this call was about, when it was about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<i64>,
    pub seconds: f64,
    /// The same agent, asked again after an unusable answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry: bool,
    /// A stand in answering for an agent that could not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// Where the CLI's event stream reports it. Blank where it does not, which
    /// is most of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Whether the call produced a usable answer.
    pub ok: bool,
}

static SPENT: Mutex<Vec<Spent>> = Mutex::new(Vec::new());

/// Record one call, and say so as it happens.
pub fn record(call: Spent) {
    crate::logdim!(
        "{} {} call{}{} took {:.0}s{}",
        call.agent,
        call.kind,
        match call.effort.as_str() {
            "" => String::new(),
            effort => format!(" at {effort}"),
        },
        match (call.retry, call.fallback) {
            (true, _) => " (retry)",
            (_, true) => " (standing in)",
            _ => "",
        },
        call.seconds,
        match call.tokens {
            Some(tokens) => format!(", {tokens} tokens"),
            None => String::new(),
        }
    );
    SPENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(call);
}

/// Everything recorded so far.
pub fn taken() -> Vec<Spent> {
    SPENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
pub fn forget() {
    SPENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// One line per agent, and one for the run, or nothing when nothing was asked.
pub fn summary(calls: &[Spent]) -> Option<String> {
    if calls.is_empty() {
        return None;
    }
    let mut agents: Vec<&str> = calls.iter().map(|c| c.agent.as_str()).collect();
    agents.sort_unstable();
    agents.dedup();

    let mut lines = Vec::new();
    for agent in agents {
        let theirs: Vec<&Spent> = calls.iter().filter(|c| c.agent == agent).collect();
        let seconds: f64 = theirs.iter().map(|c| c.seconds).sum();
        let mut line = format!(
            "  {agent}: {} call(s), {}",
            theirs.len(),
            clock(Duration::from_secs_f64(seconds))
        );
        let retries = theirs.iter().filter(|c| c.retry).count();
        let stood_in = theirs.iter().filter(|c| c.fallback).count();
        let failed = theirs.iter().filter(|c| !c.ok).count();
        for (count, what) in [
            (retries, "retry"),
            (stood_in, "standing in"),
            (failed, "failed"),
        ] {
            if count > 0 {
                line.push_str(&format!(", {count} {what}"));
            }
        }
        let tokens: u64 = theirs.iter().filter_map(|c| c.tokens).sum();
        if tokens > 0 {
            line.push_str(&format!(", {tokens} tokens"));
        }
        lines.push(line);
    }
    let total: f64 = calls.iter().map(|c| c.seconds).sum();
    Some(format!(
        "spent: {} agent call(s), {} of model time\n{}",
        calls.len(),
        clock(Duration::from_secs_f64(total)),
        lines.join("\n")
    ))
}

/// What one issue cost, for the line under its result.
pub fn for_subject(calls: &[Spent], subject: i64) -> Option<String> {
    let theirs: Vec<&Spent> = calls
        .iter()
        .filter(|c| c.subject == Some(subject))
        .collect();
    if theirs.is_empty() {
        return None;
    }
    let seconds: f64 = theirs.iter().map(|c| c.seconds).sum();
    Some(format!(
        "{} call(s), {}",
        theirs.len(),
        clock(Duration::from_secs_f64(seconds))
    ))
}

/// Wall time as somebody reads it, rather than as a float.
fn clock(taken: Duration) -> String {
    let seconds = taken.as_secs();
    if seconds < 90 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 90 {
        return format!("{minutes}m{:02}s", seconds % 60);
    }
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(agent: &str, kind: &str, seconds: f64) -> Spent {
        Spent {
            agent: agent.into(),
            kind: kind.into(),
            effort: "high".into(),
            subject: Some(42),
            seconds,
            retry: false,
            fallback: false,
            tokens: None,
            ok: true,
        }
    }

    /// Tuning was blind: the documented reason `absorb_new_issues` is off is
    /// that it "multiplies what a run costs", and nothing showed the multiplier.
    #[test]
    fn the_summary_says_what_each_agent_was_asked_and_how_long_it_took() {
        let mut calls = vec![
            call("claude", "triage", 30.0),
            call("codex", "triage", 45.0),
            call("claude", "implement", 120.0),
        ];
        calls[2].retry = true;
        calls[2].tokens = Some(12_000);

        let text = summary(&calls).expect("something was spent");
        assert!(text.contains("3 agent call(s)"), "{text}");
        assert!(text.contains("claude: 2 call(s)"), "{text}");
        assert!(text.contains("codex: 1 call(s)"), "{text}");
        assert!(text.contains("1 retry"), "{text}");
        assert!(text.contains("12000 tokens"), "{text}");
        assert_eq!(None, summary(&[]), "a run that asked nothing says nothing");
    }

    #[test]
    fn one_issue_is_accounted_for_on_its_own() {
        let mut other = call("codex", "review_1", 60.0);
        other.subject = Some(43);
        let calls = vec![call("claude", "implement", 30.0), other];
        assert_eq!(Some("1 call(s), 30s".to_string()), for_subject(&calls, 42));
        assert_eq!(None, for_subject(&calls, 99));
    }

    /// Seconds are what the clock gives and minutes are what a person reads.
    #[test]
    fn wall_time_reads_as_time() {
        assert_eq!("45s", clock(Duration::from_secs(45)));
        assert_eq!("2m30s", clock(Duration::from_secs(150)));
        assert_eq!("2h05m", clock(Duration::from_secs(7_500)));
    }
}
