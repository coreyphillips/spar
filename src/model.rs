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
    /// What spar should do about one comment somebody left.
    ///
    /// Every value maps to exactly one action, which is the point: a verdict
    /// that needs a second field to decide what it means is one that gets
    /// decided differently in two places.
    pub enum Ask {
        /// A change is asked for, it is right, and it belongs on this branch.
        /// Make it, push it, say so, and resolve the thread.
        Implement = "implement" | "do" | "accept" | "fix",
        /// Right, but really its own piece of work. File it, say where it went.
        Defer = "defer" | "file_issue" | "filed_issue" | "out_of_scope" | "followup",
        /// Should not be made. Reply with the reason, leave the thread open.
        Decline = "decline" | "refute" | "reject" | "disagree" | "wontfix",
        /// A question rather than a request. Answer it in words.
        Answer = "answer" | "question" | "reply" | "clarify",
        /// Nothing is being asked. Praise, or a thread they settled themselves.
        Nothing = "nothing" | "none" | "no_request" | "noop" | "skip",
    }
}

string_enum! {
    /// What one screening pass decided about one recorded follow-up.
    ///
    /// Only `StillRelevant` files anything. The other three all take the entry
    /// out of the queue, which is why the prompt asks for a reason and the log
    /// prints it: they are the verdicts nobody sees the working for.
    pub enum Screened {
        StillRelevant = "still_relevant" | "still-relevant" | "relevant" | "keep" | "file",
        AlreadyFixed = "already_fixed" | "already-fixed" | "fixed" | "done" | "resolved",
        NotWorthIt = "not_worth_it" | "not-worth-it" | "not_worth_doing" | "skip" | "drop" | "wontfix",
        Duplicate = "duplicate" | "dupe" | "dup",
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
        /// Check-in: comments were read and answered.
        Answered = "answered",
        /// Split: the item was broken into parts, which were filed or opened.
        Split = "split",
        /// Split: read, and left as one piece.
        Whole = "whole",
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

/// An optional number that may arrive as 412, "412", "#412", null, or "none".
///
/// Anything unparseable yields None rather than failing. A duplicate pointer is
/// decoration on a verdict that stands without it, and taking a whole batch down
/// over one stray string costs a second full repo pass to learn nothing.
pub fn de_opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(match Option::<serde_json::Value>::deserialize(d)? {
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        Some(serde_json::Value::String(s)) => s.trim().trim_start_matches('#').parse().ok(),
        _ => None,
    })
}

/// A list of strings that may arrive as null or be missing entirely.
fn de_string_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
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
    /// The issue holds context for work filed elsewhere. Never opened, and
    /// never closed either.
    #[serde(default, deserialize_with = "de_bool")]
    pub tracker: bool,
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

/// One agent's ruling on one entry in the local follow-up queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenVerdict {
    /// The entry's position in the list it was given, from 1.
    ///
    /// Matched back by index rather than by title, because the entries are
    /// spar's own data and so can carry a handle a model cannot paraphrase.
    #[serde(deserialize_with = "de_i64")]
    pub entry: i64,
    pub verdict: Screened,
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
    /// The issue or sibling entry a duplicate points at.
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub duplicate_of: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenResponse {
    #[serde(default)]
    pub entries: Vec<ScreenVerdict>,
}

/// One agent's ruling on whether one open item is worth splitting at all.
///
/// The cheap screen the bare `spar split` runs over a whole queue, so that two
/// agent calls per item are only spent on the ones where the answer might be
/// yes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitScreen {
    /// The issue or pull request number, which is spar's own data and so cannot
    /// be paraphrased back wrong.
    #[serde(deserialize_with = "de_i64")]
    pub item: i64,
    #[serde(deserialize_with = "de_bool")]
    pub split: bool,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitScreenDoc {
    #[serde(default)]
    pub items: Vec<SplitScreen>,
}

/// One piece of a proposed decomposition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SplitPart {
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    /// The issue body, or the rationale for a pull request slice.
    #[serde(default, deserialize_with = "de_string")]
    pub body: String,
    /// The paths this slice carries. Empty for an issue split, where there is
    /// no diff to partition.
    #[serde(default, deserialize_with = "de_string_vec")]
    pub files: Vec<String>,
}

/// One agent's decomposition of an issue or a pull request.
///
/// Deliberately one proposal rather than two. Two agents asked to decompose the
/// same thing return two decompositions that cannot be reconciled mechanically,
/// and reconciling them is a third judgement nobody asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SplitProposal {
    #[serde(default, deserialize_with = "de_bool")]
    pub should_split: bool,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
    /// Whether the parts are sequential, so each is based on its predecessor
    /// rather than on the base branch. A property of the change, not of the
    /// repository, which is why it rides on the proposal.
    #[serde(default, deserialize_with = "de_bool")]
    pub stacked: bool,
    #[serde(default)]
    pub parts: Vec<SplitPart>,
}

/// The second agent's ruling on the first one's decomposition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SplitCheck {
    #[serde(default, deserialize_with = "de_bool")]
    pub accept: bool,
    #[serde(default, deserialize_with = "de_bool")]
    pub stacked: bool,
    /// Part numbers, from 1, that should not be split out.
    #[serde(default, deserialize_with = "de_i64_vec")]
    pub strike: Vec<i64>,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
}

/// One agent's judgement of one comment somebody left on a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentVerdict {
    /// The handle spar printed beside the comment, copied back so the answer
    /// can be matched to it. Not a title: a finding's title is the only handle
    /// there is because findings are model authored, but a comment is spar's
    /// own data, so it gets one that cannot be paraphrased or collided.
    #[serde(default, deserialize_with = "de_string")]
    pub ref_id: String,
    pub ask: Ask,
    /// What is being asked for, in one sentence, in the agent's own words.
    /// How spar checks the comment was understood before acting on it.
    #[serde(default, deserialize_with = "de_string")]
    pub request: String,
    /// The whole argument. For a decline this is posted in the thread, so it is
    /// written for the person who raised the point.
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
    /// False when the comment could be read more than one way. spar answers an
    /// ambiguous comment in words and never guesses at a commit.
    #[serde(deserialize_with = "de_bool")]
    pub unambiguous: bool,
    #[serde(default)]
    pub new_issue_title: Option<String>,
    #[serde(default)]
    pub new_issue_body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckinDoc {
    #[serde(default)]
    pub verdicts: Vec<CommentVerdict>,
}

/// The second agent's ruling on the first one's call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentCheck {
    #[serde(default, deserialize_with = "de_string")]
    pub ref_id: String,
    #[serde(deserialize_with = "de_bool")]
    pub agrees: bool,
    /// What this agent would do. It has to say implement alongside
    /// `agrees` before anything is implemented: the two coming apart is the
    /// model contradicting itself, and that resolves toward saying rather than
    /// doing.
    pub ask: Ask,
    #[serde(deserialize_with = "de_bool")]
    pub unambiguous: bool,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckDoc {
    #[serde(default)]
    pub checks: Vec<CommentCheck>,
}

/// What the fix pass did about one comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixOutcome {
    #[serde(default, deserialize_with = "de_string")]
    pub ref_id: String,
    /// False when the change turned out to be wrong once the code was open.
    /// Declining here is a better answer than making a change you now believe
    /// is a mistake.
    #[serde(deserialize_with = "de_bool")]
    pub changed: bool,
    /// One sentence naming what changed, or why it was left alone. Posted in
    /// the thread, so it is written for the person who asked.
    #[serde(default, deserialize_with = "de_string")]
    pub summary: String,
    /// The paths this comment's change touched, checked against the diff so a
    /// reply never claims a fix that is not in it.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixReport {
    #[serde(default)]
    pub done: Vec<FixOutcome>,
}

/// What spar has already answered on one pull request or issue.
///
/// Keyed by thread or comment, valued by the newest message in it that spar did
/// not write. A thread that has moved since spar answered carries a different
/// value and is read again, which is what makes "they replied to my reply"
/// work. GitHub's own resolved flag covers the threads spar fixed; this covers
/// the ones it argued with, which stay open on purpose and would otherwise be
/// re-argued once per run, forever.
///
/// Written only after a reply has posted. A run that could not post is a run
/// that has not answered, and recording it as answered would lose the comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Answered {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub seen: BTreeMap<String, String>,
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
    /// The issue should not be implemented. No changes, and `reason` says why.
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
    /// An umbrella or epic, which spar comments on and leaves open.
    ///
    /// Set when *either* agent said so, unlike everything else here, which
    /// needs both. Closing already requires agreement, on the principle that
    /// one agent's opinion is not enough to close somebody's report; one agent
    /// saying the issue is not finished is the same principle from the other
    /// side.
    #[serde(default)]
    pub tracker: bool,
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

string_enum! {
    /// How a point stands, one round to the next.
    ///
    /// Three of these are endings: the code will not change for the point here,
    /// so raising it again only spends a round. They do not mean the same thing
    /// to a person, which is why `Dropped` is not folded into `Filed`: only
    /// `Filed` promises that somewhere holds the point.
    ///
    /// `Fixed` is not an ending. The code changed, the change is the author's
    /// claim about the point, and nothing has checked it. It is here because the
    /// ledger is the only thing a later round reads, and recording nothing for a
    /// fix left it holding only the points the reviewer lost. Six fix rounds
    /// across two pull requests produced no entry at all, and the guard that
    /// ends an argument had nothing to match.
    pub enum Settled {
        Refuted = "refuted" | "refute" | "rejected",
        Filed = "filed" | "filed_issue" | "filed-issue" | "out_of_scope",
        Dropped = "dropped" | "not_filed" | "not-filed" | "unfiled",
        Fixed = "fixed" | "fix" | "changed",
    }
}

impl Default for Settled {
    /// State written before the ledger held anything but refutations.
    fn default() -> Self {
        Settled::Refuted
    }
}

/// What one attempt to record a follow-up actually did.
///
/// The distinction the ledger needs. A point written down somewhere is settled
/// and tracked; a point deliberately not written down is settled and untracked;
/// a point that failed to be written down is not settled at all, and saying it
/// was loses it. A single `Option<String>` collapsed all three into "no URL".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followup {
    /// A tracker issue or a local note now carries it. The string is what a
    /// reader is shown: an issue URL, or a note line.
    Recorded(String),
    /// Something already covers it and nothing was written: a closed issue, or
    /// a note the local queue still holds. There is a reference to point at,
    /// but no new work to hand anyone.
    Covered(String),
    /// Deliberately not recorded, with the reason. Follow-ups are off, or the
    /// run has spent its cap.
    Dropped(&'static str),
    /// Nothing was written and nothing covers it. The point is still open, so
    /// the next round is free to try again.
    Failed,
}

impl Followup {
    /// The reference to add to the run's filed list, when there is a live one.
    ///
    /// `Covered` is deliberately excluded: a closed issue reported as filed
    /// goes back into a wave to be implemented again.
    pub fn url(&self) -> Option<&str> {
        match self {
            Followup::Recorded(url) => Some(url),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub title: String,
    pub file: String,
    pub reasoning: String,
    pub round: u32,
    #[serde(default)]
    pub reraised: u32,
    #[serde(default)]
    pub outcome: Settled,
}

/// Settled points, keyed by the review loop's finding identity. Ordered so the settled block in a
/// prompt is stable between rounds, which keeps prompt caches warm and diffs
/// readable.
pub type Ledger = BTreeMap<String, LedgerEntry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dispute {
    pub title: String,
    #[serde(default, deserialize_with = "de_string")]
    pub file: String,
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
    /// Real points a reviewer judged smaller than another round.
    ///
    /// Nothing else carries them. The round comment is off under the default
    /// `pr_comments = "outcome"` and a non-blocking finding is not filed under
    /// the default `file_non_blocking = false`, so without this the severity
    /// ladder is a way to make a finding disappear rather than a way to stop it
    /// costing a round. Silence on a pull request should mean nothing was
    /// found, not that nothing was gated.
    #[serde(default)]
    pub noted: Vec<Finding>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Runtime circuit breaker after an external follow-up write could not be
    /// verified. A later invocation rechecks GitHub before any new write.
    #[serde(skip)]
    pub followup_writes_uncertain: bool,
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
            noted: Vec::new(),
            notes: Vec::new(),
            followup_writes_uncertain: false,
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
                | Status::Answered
                // A split that correctly decided nothing needed splitting did
                // its job. Without Whole here, `spar split` over a tidy queue
                // exits non-zero, which in a script reads as a failure.
                | Status::Split
                | Status::Whole
        )
    }
}

/// Everything needed to pick a review back up, including on another machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    /// Monotonic checkpoint order for writes within this repository.
    #[serde(default)]
    pub checkpoint: u64,
    pub round: u32,
    pub next_actor: String,
    pub status: Status,
    /// Published pull request head to which this checkpoint applies.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pr_head: String,
    #[serde(default)]
    pub ledger: Ledger,
    #[serde(default)]
    pub filed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disputes: Vec<Dispute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub noted: Vec<Finding>,
}

pub const STATE_VERSION: u32 = 2;

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

    /// The body as a prompt carries it, and whether anything was left off.
    ///
    /// Shortened only past `max`, which is sized so that nothing a person
    /// wrote ever reaches it. When it does fire the cut is announced in the
    /// text itself: an agent handed a fragment with no marker has no way to
    /// tell it from an issue that simply ended there, so it judges the part it
    /// saw and reports the confidence of having seen all of it.
    ///
    /// The cut lands on a line boundary, and an unbalanced code fence is closed
    /// rather than left hanging. Broken markdown reads as a defect in the issue
    /// and costs the model attention to rule out.
    pub fn body_for_prompt(&self, max: usize) -> (String, bool) {
        let body = self.body_text().trim();
        if body.chars().count() <= max {
            return (body.to_string(), false);
        }
        let clipped: String = body.chars().take(max).collect();
        let mut kept = match clipped.rfind('\n') {
            Some(at) => clipped[..at].to_string(),
            None => clipped,
        };
        if kept.matches("```").count() % 2 == 1 {
            kept.push_str("\n```");
        }
        kept.push_str("\n\n[Shortened to fit. The rest of this issue was not included.]");
        (kept, true)
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

/// An open pull request's size, as one listing call reports it.
///
/// Enough for the screen to say whether a change is worth splitting without
/// fetching its head, which would be one network round trip per pull request
/// for a question whose answer is usually no.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrRow {
    pub number: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub changed_files: i64,
    #[serde(default)]
    pub additions: i64,
    #[serde(default)]
    pub deletions: i64,
}

impl PrRow {
    /// The size, as the screen prompt carries it.
    pub fn size(&self) -> String {
        format!(
            "{} file(s), +{} -{}",
            self.changed_files, self.additions, self.deletions
        )
    }
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

    /// A `spar split` that correctly decided nothing needed splitting did its
    /// job. Without both of these in `succeeded`, it exits non-zero over a tidy
    /// queue, which in a script reads as a failure.
    #[test]
    fn deciding_not_to_split_is_not_a_failure() {
        for status in [Status::Split, Status::Whole] {
            let mut run = IssueRun::new(1, "t");
            run.status = status;
            assert!(run.succeeded(), "{status}");
        }
    }

    #[test]
    fn status_round_trips_through_json() {
        let run = IssueRun::new(4, "t");
        let text = serde_json::to_string(&run).unwrap();
        let back: IssueRun = serde_json::from_str(&text).unwrap();
        assert_eq!(Status::Pending, back.status);
    }
}

#[cfg(test)]
mod body_for_prompt_tests {
    use super::*;

    fn issue(body: &str) -> Issue {
        let mut i: Issue = serde_json::from_value(serde_json::json!({
            "number": 1, "title": "t", "state": "open", "url": "u"
        }))
        .expect("an issue");
        i.body = Some(body.to_string());
        i
    }

    /// The case that is every real issue: nothing is touched and nothing is
    /// claimed to be.
    #[test]
    fn an_issue_that_fits_is_handed_over_whole() {
        let (body, cut) = issue("The guard is inverted.").body_for_prompt(60_000);
        assert_eq!("The guard is inverted.", body);
        assert!(!cut);
    }

    /// A fragment with no marker is indistinguishable from an issue that ended
    /// there, so the agent judges what it saw with the confidence of having
    /// seen everything. That is what the silent caps did.
    #[test]
    fn a_shortened_body_says_so_in_the_text() {
        let long = "line of text\n".repeat(500);
        let (body, cut) = issue(&long).body_for_prompt(200);
        assert!(cut);
        assert!(body.contains("Shortened to fit"), "{body}");
        assert!(body.len() < long.len());
    }

    /// Broken markdown reads as a defect in the issue, and costs the model
    /// attention to rule out.
    #[test]
    fn a_cut_never_leaves_a_code_fence_open() {
        let body = format!("intro\n\n```rust\n{}\n```\n", "let x = 1;\n".repeat(200));
        let (out, cut) = issue(&body).body_for_prompt(120);
        assert!(cut);
        assert_eq!(0, out.matches("```").count() % 2, "{out}");
    }

    /// Cutting mid-word turns the last thing the agent reads into nonsense.
    #[test]
    fn a_cut_lands_on_a_line_boundary() {
        let body = "aaaa bbbb cccc\n".repeat(100);
        let (out, _) = issue(&body).body_for_prompt(100);
        let kept = out.split("\n\n[Shortened").next().expect("the kept part");
        assert!(kept.ends_with("cccc"), "{kept:?}");
    }
}
