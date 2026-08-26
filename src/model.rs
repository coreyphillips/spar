//! The shapes that cross the boundary between spar and a model, and the shapes
//! spar keeps for itself.
//!
//! Everything a model produces is parsed leniently: an LLM that answers
//! "medium" where the schema said "med" is not a reason to abandon a run that
//! has already spent real money. Anything genuinely unrecognisable is still an
//! error, because silently downgrading a blocking finding is worse than
//! stopping.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

fn norm_token(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $canonical:literal $( | $alias:literal )* ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name { $( $(#[$vmeta])* $variant ),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $canonical ),+ }
            }

            /// Accept the canonical spelling, any listed alias, and any
            /// difference in case, spacing, hyphens, or underscores.
            pub fn parse_lenient(text: &str) -> Option<Self> {
                let got = norm_token(text);
                $(
                    if got == norm_token($canonical) $( || got == norm_token($alias) )* {
                        return Some($name::$variant);
                    }
                )+
                None
            }

            pub fn valid_values() -> String {
                [$( $canonical ),+].join(", ")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                $name::parse_lenient(&raw).ok_or_else(|| {
                    de::Error::custom(format!(
                        "{} is not one of: {}",
                        raw,
                        $name::valid_values()
                    ))
                })
            }
        }
    };
}

string_enum! {
    /// How much work an issue is. Drives ordering: cheap unblocking work first.
    pub enum Complexity {
        S = "s" | "small" | "sm" | "xs" | "trivial",
        M = "m" | "medium" | "med" | "moderate",
        L = "l" | "large" | "lg" | "xl" | "big" | "huge",
    }
}

string_enum! {
    /// How likely a change here is to break something.
    pub enum Risk {
        Low = "low" | "l" | "minimal" | "none",
        Med = "med" | "medium" | "m" | "moderate",
        High = "high" | "h" | "severe" | "critical",
    }
}

string_enum! {
    /// Only `Blocking` gates a merge. This is the whole defence against the
    /// nitpick spiral: a competent reviewer can always find something, so
    /// "no objections remaining" is not a stopping condition but "no blocking
    /// objections" is.
    pub enum Severity {
        Blocking = "blocking" | "block" | "major" | "critical",
        NonBlocking = "non-blocking" | "nonblocking" | "non_blocking" | "minor" | "suggestion",
        Nit = "nit" | "nitpick" | "style" | "trivial",
    }
}

string_enum! {
    pub enum Verdict {
        Approve = "approve" | "approved" | "lgtm",
        ChangesRequested = "changes_requested" | "changes-requested" | "request_changes" | "reject",
    }
}

string_enum! {
    pub enum NextAction {
        Merge = "merge" | "approve" | "ship",
        FixMyself = "fix_myself" | "fix-myself" | "fix" | "self_fix",
        HandBack = "hand_back" | "hand-back" | "handback" | "return",
    }
}

string_enum! {
    /// A reviewer's point gets exactly one of these. Refutation is a first
    /// class outcome, not friction: an agent that accepts every comment to get
    /// approved produces worse code, not better.
    pub enum Action {
        Fixed = "fixed" | "fix" | "accepted" | "done",
        Refuted = "refuted" | "refute" | "rejected" | "disagree" | "wontfix",
        FiledIssue = "filed_issue" | "filed-issue" | "filed" | "deferred" | "out_of_scope",
    }
}

string_enum! {
    /// Terminal state of one issue or one resumed PR.
    pub enum Status {
        Pending = "pending",
        Abandoned = "abandoned",
        Approved = "approved",
        Merged = "merged",
        Escalated = "escalated",
        Error = "error",
        /// Review only: findings were produced and posted, nothing was changed.
        Reviewed = "reviewed",
        /// Review only: both reviewers found nothing that blocks a merge.
        Clean = "clean",
    }
}

impl Complexity {
    pub fn rank(self) -> u8 {
        match self {
            Complexity::S => 0,
            Complexity::M => 1,
            Complexity::L => 2,
        }
    }
}

impl Severity {
    /// How badly it matters, independent of the order the variants happen to
    /// be declared in. Relying on derived `Ord` here would silently invert the
    /// moment somebody reorders the enum.
    pub fn rank(self) -> u8 {
        match self {
            Severity::Nit => 0,
            Severity::NonBlocking => 1,
            Severity::Blocking => 2,
        }
    }

    /// The graver of two judgements.
    ///
    /// Two reviewers disagreeing about severity is resolved upward on purpose.
    /// Nothing here gates a merge, it is all advice to a person, and advice
    /// that under-reports a real defect is worse than advice that over-reports
    /// a small one.
    pub fn graver(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

impl Risk {
    pub fn rank(self) -> u8 {
        match self {
            Risk::Low => 0,
            Risk::Med => 1,
            Risk::High => 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Lenient scalar helpers
// ---------------------------------------------------------------------------

/// An integer that may arrive as a number, a float, or a quoted string, with or
/// without a leading `#`.
pub fn de_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an issue number")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            v.trim()
                .trim_start_matches('#')
                .parse()
                .map_err(|_| E::custom(format!("{v} is not a number")))
        }
    }
    d.deserialize_any(V)
}

fn de_i64_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i64>, D::Error> {
    #[derive(Deserialize)]
    struct One(#[serde(deserialize_with = "de_i64")] i64);
    let raw = Option::<Vec<One>>::deserialize(d)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .map(|One(n)| n)
        .collect())
}

/// A boolean that may arrive as `true`, `"true"`, `"yes"`, or `1`.
pub fn de_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a boolean")
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
            match norm_token(v).as_str() {
                "true" | "yes" | "y" | "1" => Ok(true),
                "false" | "no" | "n" | "0" => Ok(false),
                other => Err(E::custom(format!("{other} is not a boolean"))),
            }
        }
    }
    d.deserialize_any(V)
}

fn de_bool_default_true<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    struct Wrap(#[serde(deserialize_with = "de_bool")] bool);
    Ok(Option::<Wrap>::deserialize(d)?
        .map(|Wrap(b)| b)
        .unwrap_or(true))
}

fn de_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// What the models return
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageVerdict {
    #[serde(deserialize_with = "de_i64")]
    pub issue: i64,
    #[serde(deserialize_with = "de_bool")]
    pub worth_doing: bool,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
    pub complexity: Complexity,
    #[serde(default, deserialize_with = "de_i64_vec")]
    pub depends_on: Vec<i64>,
    pub risk: Risk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageResponse {
    #[serde(default)]
    pub issues: Vec<TriageVerdict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    #[serde(default, deserialize_with = "de_string")]
    pub detail: String,
    #[serde(default, deserialize_with = "de_string")]
    pub file: String,
    /// A real problem that this PR did not cause. Those become follow-ups
    /// rather than review comments, so they cannot gate an unrelated merge.
    #[serde(default = "yes", deserialize_with = "de_bool_default_true")]
    pub in_scope: bool,

    // -- the parts of a bug report ---------------------------------------
    //
    // Filled when a finding is going to become an issue somebody picks up
    // cold. `detail` is the one line the pull request thread shows; these are
    // what a person needs when the thread is not in front of them. All
    // optional: a finding that stays in the thread has no use for them.
    /// What is wrong, with the specifics.
    #[serde(default)]
    pub problem: Option<String>,
    /// Steps to reproduce it, and what actually happens.
    #[serde(default)]
    pub reproduction: Option<String>,
    /// What it costs somebody.
    #[serde(default)]
    pub impact: Option<String>,
    /// What it should do instead.
    #[serde(default)]
    pub expected: Option<String>,
}

impl Default for Finding {
    /// A blank finding, for building one field at a time.
    ///
    /// Severity is spelled out here rather than derived, because a severity
    /// arriving by default is exactly the mistake this codebase refuses
    /// elsewhere: it is the field that decides whether a merge is gated, and
    /// the least severe value is the only safe thing to assume.
    fn default() -> Self {
        Self {
            severity: Severity::Nit,
            title: String::new(),
            detail: String::new(),
            file: String::new(),
            in_scope: true,
            problem: None,
            reproduction: None,
            impact: None,
            expected: None,
        }
    }
}

impl Finding {
    /// The parts of a bug report this finding carries, in the order they are
    /// written, skipping the ones it does not.
    pub fn report_sections(&self) -> Vec<(&'static str, &str)> {
        [
            ("Problem", self.problem.as_deref()),
            ("Reproduction", self.reproduction.as_deref()),
            ("Impact", self.impact.as_deref()),
            ("Expected behavior", self.expected.as_deref()),
        ]
        .into_iter()
        .filter_map(|(heading, text)| {
            text.map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| (heading, t))
        })
        .collect()
    }
}

fn yes() -> bool {
    true
}

impl Finding {
    pub fn blocks(&self) -> bool {
        self.severity == Severity::Blocking && self.in_scope
    }

    pub fn where_at(&self) -> &str {
        if self.file.trim().is_empty() {
            "general"
        } else {
            self.file.trim()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub verdict: Verdict,
    pub next_action: NextAction,
    #[serde(default, deserialize_with = "de_string")]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disposition {
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    /// Carried so a refutation lands on the same ledger key the reviewer's
    /// finding will hash to next round. Without it the re-litigation guard
    /// silently never fires for any finding that names a file.
    #[serde(default, deserialize_with = "de_string")]
    pub file: String,
    pub action: Action,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
    #[serde(default)]
    pub new_issue_title: Option<String>,
    #[serde(default)]
    pub new_issue_body: Option<String>,
}

/// One reviewer's judgement of a finding the *other* reviewer raised.
///
/// This is the whole point of review only mode. A finding both models raise
/// independently is worth a maintainer's attention; a finding one raised and
/// the other examined and rejected is usually not, and saying so is more useful
/// than forwarding both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adjudication {
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    #[serde(default, deserialize_with = "de_string")]
    pub file: String,
    /// Whether the defect is real, judged by reading the code rather than by
    /// deferring to the other reviewer.
    #[serde(deserialize_with = "de_bool")]
    pub agrees: bool,
    /// This reviewer's own view of how badly it matters.
    pub severity: Severity,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjudicationDoc {
    #[serde(default)]
    pub verdicts: Vec<Adjudication>,
}

/// A finding after both reviewers have had their say.
#[derive(Debug, Clone)]
pub struct Judged {
    pub finding: Finding,
    /// Who first raised it.
    pub raised_by: String,
    /// How it ended up.
    pub standing: Standing,
    /// The other reviewer's reasoning, when they had something to say.
    pub counterpoint: Option<String>,
    /// What the reviewer who raised it said when the objection came back.
    /// Kept apart from the objection: running both together behind a single
    /// "the other says" turns the most valuable content in the comment into
    /// one unreadable sentence.
    pub defence: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Both reviewers raised it independently. The strongest signal there is.
    Corroborated,
    /// One raised it, the other read the code and agreed.
    Confirmed,
    /// One raised it, the other read the code and rejected it, and it survived
    /// a rebuttal. A person decides.
    Disputed,
    /// Raised, rejected, and withdrawn by the reviewer who raised it.
    Withdrawn,
    /// Raised with nobody left to check it, because the round budget ran out.
    Unverified,
}

/// What the implementor did, and what the pull request body is built from.
///
/// Structured for the reason every other exchange here is structured: a model
/// asked for a description writes a paragraph about having written one, while a
/// model asked for a problem, a change list, and a way to check them answers
/// each of those. The body's substance comes from the fields being asked for
/// separately, and its brevity from spar composing them rather than the model
/// narrating.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Implementation {
    /// The issue should not be implemented. No commits, and `reason` says why.
    #[serde(default, deserialize_with = "de_bool")]
    pub not_worth_doing: bool,
    /// Only when declining. Posted on the issue, so it is written for whoever
    /// opened it rather than for the harness.
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
    /// One sentence saying what changed. Leads the body.
    #[serde(default, deserialize_with = "de_string")]
    pub summary: String,
    /// What was actually wrong, as understood after reading the code. Not a
    /// restatement of the issue: the reviewer can follow the link.
    #[serde(default, deserialize_with = "de_string")]
    pub problem: String,
    /// One line per change that alters behaviour.
    #[serde(default)]
    pub changes: Vec<String>,
    /// How a reviewer confirms the change works.
    #[serde(default)]
    pub testing: Vec<String>,
    /// Anything the reviewer would otherwise have to ask about: a deliberate
    /// omission, a decision worth defending, a risk. Usually nothing.
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseDoc {
    #[serde(default, deserialize_with = "de_string")]
    pub summary: String,
    #[serde(default)]
    pub dispositions: Vec<Disposition>,
}

// ---------------------------------------------------------------------------
// What spar keeps
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub issue: i64,
    pub title: String,
    pub complexity: Complexity,
    pub risk: Risk,
    pub depends_on: Vec<i64>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedItem {
    pub issue: i64,
    pub title: String,
    /// Keyed by agent name, so the plan file says who said what.
    pub reasons: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestedItem {
    pub issue: i64,
    pub title: String,
    /// Agent name to "do" or "skip".
    pub positions: BTreeMap<String, String>,
    pub reasons: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub order: Vec<PlanItem>,
    #[serde(default)]
    pub skipped: Vec<SkippedItem>,
    #[serde(default)]
    pub contested: Vec<ContestedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub title: String,
    pub file: String,
    pub reasoning: String,
    pub round: u32,
    #[serde(default)]
    pub reraised: u32,
}

/// Refuted points, keyed by `finding_key`. Ordered so the settled block in a
/// prompt is stable between rounds, which keeps prompt caches warm and diffs
/// readable.
pub type Ledger = BTreeMap<String, LedgerEntry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispute {
    pub title: String,
    pub reasoning: String,
}

/// The outcome of working one issue, or resuming one PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRun {
    pub issue: i64,
    pub title: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    #[serde(default)]
    pub rounds: u32,
    #[serde(default)]
    pub disputes: Vec<Dispute>,
    #[serde(default)]
    pub filed: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl IssueRun {
    pub fn new(issue: i64, title: impl Into<String>) -> Self {
        Self {
            issue,
            title: title.into(),
            status: Status::Pending,
            pr: None,
            rounds: 0,
            disputes: Vec::new(),
            filed: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Whether this outcome counts as the run having done its job.
    ///
    /// A review that produced findings did its job: the findings are the
    /// product, and a PR needing work is not a failure of the reviewer.
    pub fn succeeded(&self) -> bool {
        matches!(
            self.status,
            Status::Merged
                | Status::Approved
                | Status::Abandoned
                | Status::Reviewed
                | Status::Clean
        )
    }
}

/// Everything needed to pick a review back up, including on another machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    pub round: u32,
    pub next_actor: String,
    pub status: Status,
    #[serde(default)]
    pub ledger: Ledger,
    #[serde(default)]
    pub filed: Vec<String>,
}

pub const STATE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// What gh returns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub labels: Vec<Label>,
}

impl Issue {
    pub fn body_text(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }

    pub fn is_closed(&self) -> bool {
        self.state.eq_ignore_ascii_case("closed")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrRef {
    pub number: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueRef {
    pub number: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrView {
    pub number: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub closing_issues_references: Vec<IssueRef>,
    /// True when the PR's head branch lives on a fork rather than this
    /// repository.
    #[serde(default)]
    pub is_cross_repository: bool,
}

/// Issues and pull requests share one number sequence per repository, so a
/// number names exactly one of them and spar can work out which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Issue,
    Pr,
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ItemKind::Issue => "issue",
            ItemKind::Pr => "pull request",
        })
    }
}

impl PrView {
    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("open")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_accepts_the_canonical_spelling() {
        assert_eq!(
            Some(Severity::NonBlocking),
            Severity::parse_lenient("non-blocking")
        );
    }

    #[test]
    fn severity_accepts_near_misses() {
        for text in ["NonBlocking", "non_blocking", " NON-BLOCKING ", "minor"] {
            assert_eq!(
                Some(Severity::NonBlocking),
                Severity::parse_lenient(text),
                "{text}"
            );
        }
    }

    #[test]
    fn severity_rejects_nonsense() {
        assert_eq!(None, Severity::parse_lenient("catastrophic-ish"));
    }

    #[test]
    fn complexity_ordering_is_cheapest_first() {
        assert!(Complexity::S.rank() < Complexity::M.rank());
        assert!(Complexity::M.rank() < Complexity::L.rank());
    }

    #[test]
    fn finding_defaults_to_in_scope() {
        let f: Finding = serde_json::from_value(serde_json::json!({
            "severity": "blocking", "title": "t", "detail": "d", "file": "a.rs"
        }))
        .unwrap();
        assert!(f.in_scope);
        assert!(f.blocks());
    }

    #[test]
    fn out_of_scope_blocking_does_not_block() {
        let f: Finding = serde_json::from_value(serde_json::json!({
            "severity": "blocking", "title": "t", "detail": "d",
            "file": "a.rs", "in_scope": false
        }))
        .unwrap();
        assert!(!f.blocks());
    }

    #[test]
    fn triage_tolerates_a_quoted_issue_number() {
        let v: TriageVerdict = serde_json::from_value(serde_json::json!({
            "issue": "#42", "worth_doing": "yes", "reason": "r",
            "complexity": "medium", "depends_on": ["39"], "risk": "low"
        }))
        .unwrap();
        assert_eq!(42, v.issue);
        assert!(v.worth_doing);
        assert_eq!(Complexity::M, v.complexity);
        assert_eq!(vec![39], v.depends_on);
    }

    #[test]
    fn triage_tolerates_a_missing_depends_on() {
        let v: TriageVerdict = serde_json::from_value(serde_json::json!({
            "issue": 1, "worth_doing": false, "reason": "r",
            "complexity": "s", "risk": "high"
        }))
        .unwrap();
        assert!(v.depends_on.is_empty());
    }

    #[test]
    fn review_tolerates_a_missing_findings_array() {
        let r: Review = serde_json::from_value(serde_json::json!({
            "verdict": "approve", "next_action": "merge", "summary": "fine"
        }))
        .unwrap();
        assert!(r.findings.is_empty());
    }

    #[test]
    fn a_null_reason_is_an_empty_string_not_a_failure() {
        let v: TriageVerdict = serde_json::from_value(serde_json::json!({
            "issue": 1, "worth_doing": true, "reason": null,
            "complexity": "s", "depends_on": [], "risk": "low"
        }))
        .unwrap();
        assert_eq!("", v.reason);
    }

    #[test]
    fn unknown_severity_is_an_error_not_a_silent_downgrade() {
        let out: Result<Finding, _> = serde_json::from_value(serde_json::json!({
            "severity": "showstopper-maybe", "title": "t", "detail": "d", "file": "a.rs"
        }));
        assert!(out.is_err());
    }

    #[test]
    fn status_round_trips_through_json() {
        let run = IssueRun::new(4, "t");
        let text = serde_json::to_string(&run).unwrap();
        let back: IssueRun = serde_json::from_str(&text).unwrap();
        assert_eq!(Status::Pending, back.status);
    }
}
