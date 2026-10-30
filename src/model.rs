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
    /// What a formal pull request review says. `Verdict` is what a model
    /// answers; this is what spar submits, and spar never asks a model for it.
    /// It is derived from the findings: approve when nothing blocks, request
    /// changes when something both reviewers count does.
    pub enum ReviewVerdict {
        Approve = "approve" | "approved",
        RequestChanges = "request_changes" | "changes_requested" | "request-changes",
    }
}

impl ReviewVerdict {
    /// The `event` the reviews API submits it as.
    pub fn api_event(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
        }
    }

    /// The words that stand in for the badge when GitHub will not take the
    /// verdict itself, on the viewer's own pull request.
    pub fn words(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::RequestChanges => "changes requested",
        }
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
        /// Review only: the head moved while it was read, or GitHub had not
        /// caught up with a push yet, so nothing was posted. It runs again on
        /// the new head.
        Moved = "moved",
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
    /// Two reviewers disagreeing about severity is resolved upward on purpose:
    /// advice that under-reports a real defect is worse than advice that
    /// over-reports a small one.
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
    /// The open issue a duplicate points at.
    ///
    /// Kept apart from `duplicate_of_entry`, because one number cannot say
    /// which it is and the two are acted on differently: an issue already
    /// exists, while a sibling entry may itself be dropped or fail to file.
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub duplicate_of_issue: Option<i64>,
    /// The number of another entry in the same list a duplicate points at.
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub duplicate_of_entry: Option<i64>,
}

impl ScreenVerdict {
    /// Whether this verdict names anything at all as the thing that covers it.
    pub fn names_a_duplicate(&self) -> bool {
        self.duplicate_of_issue.is_some() || self.duplicate_of_entry.is_some()
    }
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

string_enum! {
    /// Who answers one question an issue puts to the viewer.
    pub enum QuestionKind {
        /// A question of fact that the code, the issue, or what it links can
        /// settle. spar may answer it, once both agents have checked it.
        Answer = "answer" | "factual" | "fact" | "spar",
        /// It asks the viewer to decide, commit, own, schedule, approve, or
        /// give a view, or it could not be checked. Only the viewer answers it.
        Yours = "yours" | "person" | "decision" | "human" | "unverified",
    }
}

/// One question an issue puts to the viewer, as the drafting agent read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    /// The handle spar printed beside the mention it came from.
    #[serde(default, deserialize_with = "de_string")]
    pub ref_id: String,
    /// The question, quoted from the issue, or as close to it as reads alone.
    #[serde(default, deserialize_with = "de_string")]
    pub question: String,
    pub kind: QuestionKind,
    /// The reply as it would be posted. For a question that is the viewer's,
    /// a starting point for them, which spar never posts.
    #[serde(default, deserialize_with = "de_string")]
    pub answer: String,
    /// What the answer rests on: paths with lines, or links.
    #[serde(default, deserialize_with = "de_string")]
    pub evidence: String,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerDoc {
    #[serde(default)]
    pub questions: Vec<Question>,
}

/// The second agent's ruling on one drafted answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionCheck {
    /// The handle spar gave the question: q1, q2.
    #[serde(default, deserialize_with = "de_string")]
    pub id: String,
    /// True only when this agent checked the answer and it is right, complete,
    /// and fit to post as the viewer's.
    #[serde(deserialize_with = "de_bool")]
    pub agrees: bool,
    /// Who this agent says should answer it. Both agents have to say answer
    /// before spar does: the two coming apart resolves toward the viewer.
    pub kind: QuestionKind,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerCheckDoc {
    #[serde(default)]
    pub checks: Vec<QuestionCheck>,
    /// Questions put to the viewer that the draft left out.
    #[serde(default, deserialize_with = "de_string_vec")]
    pub missed: Vec<String>,
}

/// What `spar answer` has done on one issue, in `.spar/state/answer-N.json`.
///
/// Every mention that was judged is kept, whether it was answered, left for
/// the viewer, or asked nothing, keyed `body` or `comment:{id}`. A mention is
/// judged once: an edit to it, or the thread moving under spar's own reply,
/// is not a new question, and treating either as one is how a reply answers
/// itself. A mention whose run failed is not kept, so it is tried again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerRecord {
    #[serde(default)]
    pub version: u32,
    /// Mention key to what became of it.
    #[serde(default)]
    pub judged: BTreeMap<String, String>,
    /// Runs `spar listen` started since a person last ran `spar answer` here.
    #[serde(default)]
    pub automatic: u32,
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
    /// A ruling on each acceptance criterion, asked for only when the issue
    /// has some.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<CriterionStatus>,
}

string_enum! {
    /// Where one acceptance criterion stands. The author says done and the
    /// reviewer says met, and both mean the branch does what the line asks.
    pub enum Mark {
        Met = "met" | "done",
        NotMet = "not_met" | "not_done" | "unmet",
        NeedsPerson = "needs_person" | "needs_a_person" | "manual",
    }
}

/// One agent's word on one numbered acceptance criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionStatus {
    pub number: u32,
    pub status: Mark,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
}

/// An acceptance criterion and the signing review's ruling on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruled {
    /// The line exactly as it stood, which is what a tick finds it by.
    pub raw: String,
    /// Where it stood when it was read, so a criterion moved since is skipped
    /// rather than ticked in its new place.
    #[serde(default)]
    pub line: usize,
    pub text: String,
    /// None when the review said nothing about this one.
    pub status: Option<Mark>,
    #[serde(default)]
    pub reason: String,
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

/// What a reviewer did about one of its own findings when it chose to fix them.
///
/// The other three editing calls return a structured report. This one returned
/// free text, and three things were inferred from it: that every finding was
/// fixed, what the commit subject should be, and that the author had nothing to
/// answer. A reviewer that fixed two of three points and said so in prose had
/// all three recorded as fixed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnFix {
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    /// Carried for the same reason a disposition carries it: the ledger key has
    /// to be the one the finding hashes to.
    #[serde(default, deserialize_with = "de_string")]
    pub file: String,
    #[serde(deserialize_with = "de_bool")]
    pub fixed: bool,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnFixDoc {
    #[serde(default, deserialize_with = "de_string")]
    pub summary: String,
    #[serde(default)]
    pub fixes: Vec<OwnFix>,
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

string_enum! {
    /// What a second look found about one earlier finding.
    pub enum Ruling {
        /// The code now answers the point.
        Fixed = "fixed" | "resolved" | "addressed" | "done",
        /// The code did not change for it, and a response showed the point
        /// was wrong or does not apply.
        Rebutted = "rebutted" | "refuted" | "answered" | "withdrawn" | "not_a_bug",
        /// Neither. The point stands, or the change made for it does not
        /// settle it.
        Open = "open" | "still_open" | "unresolved" | "stands" | "unfixed",
    }
}

/// One reviewer's ruling on one earlier finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecheckRuling {
    /// The 1-based number the prompt gave the earlier finding. A number rather
    /// than a title, for the reason the follow-up screen uses one: a model
    /// rewrites titles, and a number it copies.
    #[serde(deserialize_with = "de_i64")]
    pub index: i64,
    pub outcome: Ruling,
    #[serde(default, deserialize_with = "de_string")]
    pub reasoning: String,
}

/// What one reviewer answers on a second look: a ruling per earlier finding,
/// and ordinary findings for anything new.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecheckDoc {
    #[serde(default, deserialize_with = "de_string")]
    pub summary: String,
    #[serde(default)]
    pub rulings: Vec<RecheckRuling>,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

// ---------------------------------------------------------------------------
// Brainstorming
// ---------------------------------------------------------------------------

/// One idea from a brainstorm: existing pieces, paired in a way that has not
/// been done.
///
/// `combines` is the load bearing field. The premise of `spar brainstorm` is
/// that the parts of an answer already exist and have not been put together, so
/// an idea that cannot name its parts is a wish rather than a proposal, and
/// `why_new` has to say what the nearest existing thing is rather than assert
/// novelty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Idea {
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
    /// The existing techniques, primitives, results, or patterns this pairs.
    #[serde(default, deserialize_with = "de_string_vec")]
    pub combines: Vec<String>,
    #[serde(default, deserialize_with = "de_string")]
    pub how_it_works: String,
    /// The nearest thing that already exists, and how this differs from it.
    #[serde(default, deserialize_with = "de_string")]
    pub why_new: String,
    #[serde(default, deserialize_with = "de_string")]
    pub enables: String,
    #[serde(default, deserialize_with = "de_string")]
    pub risks: String,
    /// The cheapest thing that would show whether it works.
    #[serde(default, deserialize_with = "de_string")]
    pub first_experiment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdeasDoc {
    #[serde(default)]
    pub ideas: Vec<Idea>,
}

string_enum! {
    /// What one agent does with an idea the other proposed.
    pub enum CrossKind {
        Build = "build" | "extend" | "improve" | "combine",
        Challenge = "challenge" | "reject" | "exists" | "object",
        Keep = "keep" | "agree" | "accept" | "fine",
    }
}

/// One agent's ruling on an idea the *other* agent proposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossVerdict {
    /// The number the idea was given in the list, so the ruling can be matched
    /// back to it. Titles are not used for this: a model rewrites them.
    #[serde(default, deserialize_with = "de_i64")]
    pub idea: i64,
    pub verdict: CrossKind,
    #[serde(default, deserialize_with = "de_string")]
    pub reason: String,
    /// The revised idea, when the verdict is build.
    #[serde(default)]
    pub build: Option<Idea>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossDoc {
    #[serde(default)]
    pub verdicts: Vec<CrossVerdict>,
}

/// What the agent that proposed a challenged idea says to the objection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defence {
    #[serde(default, deserialize_with = "de_i64")]
    pub idea: i64,
    /// True to stand by it with specifics, false to withdraw it.
    #[serde(deserialize_with = "de_bool")]
    pub stands: bool,
    #[serde(default, deserialize_with = "de_string")]
    pub reply: String,
}

/// The last round's answer: defences for the agent's own challenged ideas, and
/// its ranking of everything still standing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConvergeDoc {
    #[serde(default)]
    pub defences: Vec<Defence>,
    /// Idea numbers, best first.
    #[serde(default, deserialize_with = "de_i64_vec")]
    pub ranking: Vec<i64>,
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
    /// The other reviewer's own severity, when the cross-check had it agree
    /// the defect is real. Not kept in the review record: the counterpoint
    /// says it in words, and nothing after the cross-check reads it.
    pub rated: Option<Severity>,
}

string_enum! {
    /// How a finding stands after both reviewers have had their say. A word
    /// rather than a bare enum because the review record keeps it on disk.
    pub enum Standing {
        /// Both reviewers raised it independently. The strongest signal there is.
        Corroborated = "corroborated" | "both",
        /// One raised it, the other read the code and agreed.
        Confirmed = "confirmed",
        /// One raised it, the other read the code and rejected it, and it survived
        /// a rebuttal. Or the other agreed it is real but not on whether it
        /// blocks. A person decides.
        Disputed = "disputed",
        /// Raised, rejected, and withdrawn by the reviewer who raised it.
        Withdrawn = "withdrawn",
        /// Raised with nobody left to check it, because the round budget ran out.
        Unverified = "unverified" | "one_reviewer",
    }
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
    /// The pull request title: what the change ships, which is not always
    /// the whole of what the issue asks for. Empty falls back to the issue's.
    #[serde(default, deserialize_with = "de_string")]
    pub title: String,
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
    /// The author's status on each acceptance criterion. A claim for the PR
    /// body, and never what ticks a box. Asked for only when the issue has
    /// criteria, which is why an empty list is not written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<CriterionStatus>,
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

/// An issue triage disagreed about, remembered between runs.
///
/// A disagreement is the one triage outcome that needs a person, and it was
/// also the one that reached them least reliably: a log line suppressed by
/// `--quiet`, an entry in `plan.json`, and two calls spent on it again on every
/// later run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Contested {
    #[serde(default)]
    pub title: String,
    /// Agent name to "do" or "skip".
    #[serde(default)]
    pub positions: BTreeMap<String, String>,
    #[serde(default)]
    pub reasons: BTreeMap<String, String>,
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
    /// The issue's acceptance criteria as the review that signed the pull
    /// request off ruled them. Set only on that path, so a ruling from a review
    /// that did not sign off never reaches a tick.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<Ruled>,
    /// Whether a met criterion will be ticked once the work lands. When not,
    /// the outcome comment is the only record of the rulings.
    #[serde(skip)]
    pub ticks_criteria: bool,
    /// Runtime circuit breaker after an external follow-up write could not be
    /// verified. A later invocation rechecks GitHub before any new write.
    #[serde(skip)]
    pub followup_writes_uncertain: bool,
    /// How many criteria the signing review did not rule met, when the pull
    /// request says `Part of` its issue and so leaves it open on merge.
    #[serde(skip)]
    pub left_open: Option<usize>,
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
            criteria: Vec::new(),
            ticks_criteria: false,
            followup_writes_uncertain: false,
            left_open: None,
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

/// One finding as the review record keeps it: enough to list it again, rule on
/// it, and post it, and nothing a model is not asked for. The parts of a bug
/// report a `Finding` may carry are for an issue filed from it, which a record
/// never does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedFinding {
    pub title: String,
    #[serde(default)]
    pub file: String,
    pub severity: Severity,
    #[serde(default)]
    pub detail: String,
    #[serde(default = "yes")]
    pub in_scope: bool,
    #[serde(default)]
    pub raised_by: String,
    pub standing: Standing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defence: Option<String>,
}

impl RecordedFinding {
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

    /// Back to the in-memory shape, for the renderer and the record that
    /// follows a second look.
    pub fn to_judged(&self) -> Judged {
        Judged {
            finding: Finding {
                severity: self.severity,
                title: self.title.clone(),
                detail: self.detail.clone(),
                file: self.file.clone(),
                in_scope: self.in_scope,
                ..Finding::default()
            },
            raised_by: self.raised_by.clone(),
            standing: self.standing,
            counterpoint: self.counterpoint.clone(),
            defence: self.defence.clone(),
            rated: None,
        }
    }
}

impl From<&Judged> for RecordedFinding {
    fn from(j: &Judged) -> Self {
        Self {
            title: j.finding.title.clone(),
            file: j.finding.file.clone(),
            severity: j.finding.severity,
            detail: j.finding.detail.clone(),
            in_scope: j.finding.in_scope,
            raised_by: j.raised_by.clone(),
            standing: j.standing,
            counterpoint: j.counterpoint.clone(),
            defence: j.defence.clone(),
        }
    }
}

/// What `spar review` leaves behind, and what a second look reads and rewrites.
///
/// `spar review` used to leave nothing machine readable: not the commit it
/// read, not the points it raised. There was therefore nothing to check a later
/// push against, and the only second look possible was a first look again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub version: u32,
    /// The commit the review worktree had checked out when the reviewers read
    /// it. From `git rev-parse HEAD` there, never from a second `gh` call.
    pub head: String,
    /// When the review began, in the one shape GitHub returns,
    /// `2026-10-20T18:04:05Z`, so it compares with a comment's `created_at`
    /// as a string. The start rather than the end, so a reply made while a
    /// long review ran still counts as new on the next look.
    pub reviewed_at: String,
    #[serde(default)]
    pub findings: Vec<RecordedFinding>,
    /// Second looks taken so far, which `[listen] max_rechecks` caps.
    #[serde(default)]
    pub rechecks: u32,
    /// The formal verdict posted, when one was. None for a plain comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ReviewVerdict>,
    /// Whether the review reached the pull request. A dry run's record is kept
    /// so `spar post` can complete it, but nobody has seen its findings, so it
    /// does not count as a review to take a second look after.
    #[serde(default)]
    pub posted: bool,
}

pub const REVIEW_RECORD_VERSION: u32 = 1;

impl ReviewRecord {
    pub fn short_head(&self) -> &str {
        let end = self.head.len().min(7);
        &self.head[..end]
    }

    /// The points a second look rules on: everything but what was withdrawn.
    pub fn open_points(&self) -> impl Iterator<Item = &RecordedFinding> {
        self.findings
            .iter()
            .filter(|f| f.standing != Standing::Withdrawn)
    }
}

/// A finding placed on the line it is about, to travel with a review.
///
/// `line` is on the new side of the diff, and `start_line`, when there is one,
/// makes it a range ending there. Both are lines GitHub will accept: it refuses
/// the whole review if any comment names a line the diff does not show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    pub start_line: Option<u32>,
    pub body: String,
}

// ---------------------------------------------------------------------------
// What gh returns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Author {
    #[serde(default)]
    pub login: String,
}

/// A login, never empty. A deleted account is null on GitHub and becomes
/// `ghost`, which no trust setting but `anyone` will act on.
pub fn login_of(author: Option<&Author>) -> &str {
    match author.map(|a| a.login.trim()) {
        Some(login) if !login.is_empty() => login,
        _ => "ghost",
    }
}

/// One top level comment on an issue, as `gh issue view --json comments`
/// returns it, oldest first.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IssueComment {
    /// Null when the account was deleted.
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub author_association: String,
    /// Whether the authenticated GitHub user wrote this comment.
    #[serde(default)]
    pub viewer_did_author: bool,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub is_minimized: bool,
}

impl IssueComment {
    pub fn login(&self) -> &str {
        login_of(self.author.as_ref())
    }
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
    /// Why it was closed: `completed` or `not_planned`, empty when open or
    /// when the field was not asked for.
    ///
    /// The tracker's tick means the work landed, and a closed issue on its own
    /// does not say that. With `close_skipped = true`, the default, an item
    /// both agents declined is closed as not planned, and ticking it writes
    /// "this was done" into somebody's checklist.
    #[serde(default, rename = "stateReason")]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Who filed it. None when the field was not asked for.
    #[serde(default)]
    pub author: Option<Author>,
    /// What was said on it since it was filed. Empty when the field was not
    /// asked for, which a listing of the whole queue never does.
    #[serde(default)]
    pub comments: Vec<IssueComment>,
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

    /// Closed because the work was done, rather than because somebody decided
    /// not to do it.
    ///
    /// An unknown reason counts as completed: GitHub leaves `stateReason`
    /// empty on issues closed before it existed, and reading those as declined
    /// would leave every old checklist unticked.
    pub fn closed_as_completed(&self) -> bool {
        self.is_closed() && !self.closed_as_not_planned()
    }

    pub fn closed_as_not_planned(&self) -> bool {
        self.is_closed()
            && self
                .state_reason
                .as_deref()
                .is_some_and(|reason| reason.eq_ignore_ascii_case("not_planned"))
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
    /// What tells this repository's issue from one of the same number in
    /// another, since a closing link can cross repositories.
    #[serde(default)]
    pub url: String,
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
    /// The description, which is the author's own account of the change. Read
    /// by every pass that judges whether the change does what it claims.
    #[serde(default)]
    pub body: String,
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
    /// Who owns the repository the head branch is in: for a pull request
    /// from a fork, the fork's owner. That is what tells the authenticated
    /// user's own fork, which spar can push to, from anybody else's.
    #[serde(default)]
    pub head_repository_owner: Option<Author>,
    /// The repository the head branch is in.
    #[serde(default)]
    pub head_repository: Option<HeadRepository>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HeadRepository {
    #[serde(default)]
    pub name: String,
}

/// The fields a sweep reads from `gh pr list` and `gh pr view`: enough to
/// notice a head that moved, a review request, an assignment, and a close.
pub const PR_SWEEP_FIELDS: &str =
    "number,title,headRefOid,updatedAt,author,isDraft,isCrossRepository,reviewRequests,assignees,state";

/// One row of `gh pr list --json PR_SWEEP_FIELDS`, or of `gh pr view` with
/// the same fields.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSweepRow {
    pub number: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub head_ref_oid: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub is_cross_repository: bool,
    #[serde(default)]
    pub review_requests: Vec<Login>,
    #[serde(default)]
    pub assignees: Vec<Login>,
    /// OPEN, CLOSED, or MERGED.
    #[serde(default)]
    pub state: String,
}

/// A user in a `reviewRequests` or `assignees` array. A team entry has `name`
/// and `slug` instead, which reads as an empty login and never matches the
/// viewer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Login {
    #[serde(default)]
    pub login: String,
    /// A team's `org/slug`, as gh gives it. Empty for a user.
    #[serde(default)]
    pub slug: String,
}

/// One review request from a pull request's timeline: when it was made, and
/// who it asked, a user by `login` or a team by `slug` as in `Login`.
#[derive(Debug, Clone, Default)]
pub struct ReviewRequested {
    pub at: String,
    pub who: Login,
}

/// Issues and pull requests share one number sequence per repository, so a
/// number names exactly one of them and spar can work out which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// Where a pull request's head stands right now, asked two ways.
///
/// GitHub moves a pull request to a pushed commit some time after the push,
/// usually seconds and sometimes minutes, and until then `headRefOid` and
/// `refs/pull/N/head` both name the commit before it. The branch itself moves
/// with the push. A look that trusted the pull request alone read the commit
/// before a push, and by the time it posted its verdict was two pushes old.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHead {
    /// The head GitHub's pull request names.
    pub pull: String,
    /// Where the branch is, for a branch in this repository. None for a fork,
    /// whose branch `origin` does not have.
    pub branch: Option<String>,
}

impl LiveHead {
    /// The head, once the pull request and its branch agree on it.
    pub fn settled(&self) -> Option<&str> {
        match &self.branch {
            Some(branch) if !branch.eq_ignore_ascii_case(&self.pull) => None,
            _ => Some(&self.pull),
        }
    }

    /// The newer head when either differs from the one a look read. The
    /// branch is asked first, since it moves before the pull request does.
    pub fn moved_from(&self, reviewed: &str) -> Option<&str> {
        self.branch
            .as_deref()
            .filter(|branch| !branch.eq_ignore_ascii_case(reviewed))
            .or_else(|| (!self.pull.eq_ignore_ascii_case(reviewed)).then_some(self.pull.as_str()))
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

    /// Nothing was posted, so a script that runs `spar review` has something
    /// to try again.
    #[test]
    fn a_look_whose_head_moved_did_not_do_its_job() {
        let mut run = IssueRun::new(4, "t");
        run.status = Status::Moved;
        assert!(!run.succeeded());
        let text = serde_json::to_string(&run).unwrap();
        assert!(text.contains("\"moved\""), "{text}");
        let back: IssueRun = serde_json::from_str(&text).unwrap();
        assert_eq!(Status::Moved, back.status);
    }

    fn live(pull: &str, branch: Option<&str>) -> LiveHead {
        LiveHead {
            pull: pull.into(),
            branch: branch.map(String::from),
        }
    }

    /// A branch can be at the pushed commit for minutes while its pull request
    /// still names the one before.
    #[test]
    fn a_head_is_settled_only_once_the_pull_request_has_caught_up_with_its_branch() {
        assert_eq!(None, live("fb3a7ab", Some("2d5e2b5")).settled());
        assert_eq!(Some("2d5e2b5"), live("2d5e2b5", Some("2d5e2b5")).settled());
        assert_eq!(Some("2d5e2b5"), live("2d5e2b5", Some("2D5E2B5")).settled());
        // A fork's branch is not on origin, so the pull request is all there is.
        assert_eq!(Some("fb3a7ab"), live("fb3a7ab", None).settled());
    }

    #[test]
    fn a_head_moved_from_the_reviewed_one_names_the_newer_commit() {
        assert_eq!(None, live("fb3a7ab", Some("fb3a7ab")).moved_from("fb3a7ab"));
        assert_eq!(None, live("FB3A7AB", None).moved_from("fb3a7ab"));
        // The branch moves first, so it is the newer of the two.
        assert_eq!(
            Some("c493635"),
            live("2d5e2b5", Some("c493635")).moved_from("fb3a7ab")
        );
        assert_eq!(
            Some("c493635"),
            live("fb3a7ab", Some("c493635")).moved_from("fb3a7ab")
        );
        assert_eq!(Some("2d5e2b5"), live("2d5e2b5", None).moved_from("fb3a7ab"));
        assert_eq!(
            Some("2d5e2b5"),
            live("2d5e2b5", Some("fb3a7ab")).moved_from("fb3a7ab")
        );
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

#[cfg(test)]
mod review_record_tests {
    use super::*;

    fn judged() -> Judged {
        Judged {
            finding: Finding {
                severity: Severity::Blocking,
                title: "Retry loop never terminates".into(),
                detail: "the guard compares against Some(0)".into(),
                file: "src/net.rs:88".into(),
                in_scope: true,
                problem: Some("dropped on the way to the record".into()),
                ..Finding::default()
            },
            raised_by: "claude and codex".into(),
            standing: Standing::Corroborated,
            counterpoint: None,
            defence: Some("stands".into()),
            rated: None,
        }
    }

    #[test]
    fn a_review_record_round_trips_through_json_and_keeps_its_standing_as_a_word() {
        let record = ReviewRecord {
            version: REVIEW_RECORD_VERSION,
            head: "0123abcdef".into(),
            reviewed_at: "2026-10-20T18:04:05Z".into(),
            findings: vec![RecordedFinding::from(&judged())],
            rechecks: 2,
            verdict: Some(ReviewVerdict::RequestChanges),
            posted: true,
        };
        let text = serde_json::to_string_pretty(&record).unwrap();
        assert!(text.contains("\"standing\": \"corroborated\""), "{text}");
        assert!(text.contains("\"verdict\": \"request_changes\""), "{text}");
        assert!(text.contains("\"severity\": \"blocking\""), "{text}");
        assert!(!text.contains("problem"), "{text}");
        let back: ReviewRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(record, back);
        assert_eq!("0123abc", back.short_head());
    }

    #[test]
    fn a_record_missing_the_newer_fields_still_reads() {
        let text = r#"{"version":1,"head":"abc","reviewed_at":"2026-10-20T18:04:05Z",
            "findings":[{"title":"T","severity":"nit","standing":"one_reviewer"}]}"#;
        let back: ReviewRecord = serde_json::from_str(text).unwrap();
        assert_eq!(0, back.rechecks);
        assert!(!back.posted);
        assert_eq!(None, back.verdict);
        assert!(back.findings[0].in_scope);
        assert_eq!(Standing::Unverified, back.findings[0].standing);
        assert_eq!("general", back.findings[0].where_at());
    }

    #[test]
    fn a_recorded_finding_turns_back_into_a_judged_finding() {
        let original = judged();
        let back = RecordedFinding::from(&original).to_judged();
        assert_eq!(original.finding.title, back.finding.title);
        assert_eq!(original.finding.file, back.finding.file);
        assert_eq!(original.finding.severity, back.finding.severity);
        assert_eq!(original.raised_by, back.raised_by);
        assert_eq!(original.standing, back.standing);
        assert_eq!(original.defence, back.defence);
        assert_eq!(None, back.finding.problem);
        assert!(back.finding.blocks());
    }

    #[test]
    fn a_ruling_tolerates_a_quoted_index_and_a_synonym_outcome() {
        let doc: RecheckDoc = serde_json::from_str(
            r#"{"summary":"holds","rulings":[{"index":"2","outcome":"Resolved","reasoning":"line 4"}],"findings":[]}"#,
        )
        .unwrap();
        assert_eq!(2, doc.rulings[0].index);
        assert_eq!(Ruling::Fixed, doc.rulings[0].outcome);
        let doc: RecheckDoc =
            serde_json::from_str(r#"{"summary":"","rulings":[],"findings":[]}"#).unwrap();
        assert!(doc.rulings.is_empty());
        assert_eq!("APPROVE", ReviewVerdict::Approve.api_event());
        assert_eq!("changes requested", ReviewVerdict::RequestChanges.words());
    }
}
