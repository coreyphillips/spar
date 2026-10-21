//! Listening for what happens after a review.
//!
//! Every other command does one pass and exits, so a reply to a review spar
//! posted, or a push that answered it, waited for a person to notice and run
//! spar again. `spar listen` waits instead, and it waits for free: the only
//! thing that calls an agent is a review, a second look, or a check-in, and
//! none of those starts until GitHub says something happened.
//!
//! Two sources feed it, because neither is enough alone. GitHub's notifications
//! say why a thread moved, a review request, a mention, a comment, and they are
//! polled with a conditional request that costs nothing against the rate limit
//! when nothing changed. But a push raises no notification for a reviewer, so a
//! slower sweep of the open pull requests compares each head with the commit
//! spar last read. Notifications are never raised for the viewer's own actions,
//! which is also why spar's own comments never wake it.
//!
//! Events settle before anything runs: a push followed by three replies in the
//! same minute is one second look, not four. What runs is decided by a table
//! that is a pure function of the event, a few facts about the item, and the
//! `[listen]` settings, so it can be read and tested on its own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::checkin;
use crate::clock;
use crate::config::{Config, ListenCfg, OwnPr, ReviewOrIgnore};
use crate::error::Result;
use crate::model::{IssueRun, ItemKind, PrSweepRow, Status};
use crate::proc;
use crate::repo::{ApiResponse, Repo};
use crate::review_only::{self, RecheckMode, ReviewOpts};
use crate::spend;
use crate::{bail, log, logdim, logwarn, spar_err};

/// Polling never goes below this, whatever the config says.
const FLOOR_SECS: u64 = 15;
/// A `seen` entry older than this is forgotten.
const SEEN_FOR_SECS: u64 = 7 * 86_400;
/// Most notification pages read in one poll.
const MAX_PAGES: usize = 5;
/// Failed dispatches of one item before it is left alone until it changes.
const MAX_FAILURES: u32 = 5;

pub const STATE_VERSION: u32 = 1;

/// How one `spar listen` was asked to run.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// The issues and pull requests to watch. Empty means the repository.
    pub items: Vec<i64>,
    /// One poll and one sweep, act on what is due, exit.
    pub once: bool,
    /// Log every event and what it would trigger, and trigger nothing.
    pub dry_run: bool,
    /// How far back the first run looks.
    pub since: Option<Duration>,
    /// Most pending second looks the first sweep may queue.
    pub limit: usize,
}

// ---------------------------------------------------------------------------
// What GitHub returns
// ---------------------------------------------------------------------------

/// One notification thread, as `GET /repos/{owner}/{repo}/notifications`
/// returns it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Thread {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub subject: Subject,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Subject {
    #[serde(default)]
    pub title: String,
    /// The API URL of the issue or pull request; its number is the last path
    /// segment.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub latest_comment_url: Option<String>,
    /// `PullRequest`, `Issue`, or something spar does not watch.
    #[serde(default, rename = "type")]
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Events, and what is kept between ticks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The head moved since spar last read it.
    Pushed,
    /// Somebody other than the viewer said something.
    Replied,
    Mentioned,
    ReviewRequested,
    Assigned,
    Closed,
}

impl Trigger {
    /// The trigger as a reason in a sentence.
    fn noun(self) -> &'static str {
        match self {
            Trigger::Pushed => "a push",
            Trigger::Replied => "a reply",
            Trigger::Mentioned => "a mention",
            Trigger::ReviewRequested => "a review request",
            Trigger::Assigned => "an assignment",
            Trigger::Closed => "a close",
        }
    }
}

/// One thing that happened to one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub number: i64,
    pub kind: ItemKind,
    pub trigger: Trigger,
    /// Who did it, when GitHub said.
    pub who: Option<String>,
    /// The old and new head, for a push.
    pub head: Option<(String, String)>,
    /// New commits since the review, when cheap to know.
    pub commits: Option<u64>,
}

/// What has happened to one item since spar last acted on it. Persisted, so a
/// restart in the middle of settling loses nothing: the notification is
/// already past the watermark and will not come again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pending {
    pub kind: ItemKind,
    pub first_seen: u64,
    pub last_seen: u64,
    pub triggers: BTreeSet<Trigger>,
    #[serde(default)]
    pub who: Vec<String>,
    #[serde(default)]
    pub head_from: Option<String>,
    #[serde(default)]
    pub head_to: Option<String>,
}

impl Pending {
    /// "a push and 2 replies", for the line that says why something ran.
    pub fn because(&self) -> String {
        let mut parts: Vec<String> = self.triggers.iter().map(|t| t.noun().to_string()).collect();
        if parts.is_empty() {
            parts.push("nothing in particular".into());
        }
        match parts.len() {
            1 => parts.remove(0),
            2 => format!("{} and {}", parts[0], parts[1]),
            _ => {
                let last = parts.pop().unwrap_or_default();
                format!("{}, and {last}", parts.join(", "))
            }
        }
    }
}

/// What the listener knows about one item it is watching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Watched {
    pub kind: ItemKind,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    /// The head the listener last saw. A sweep raises Pushed only when this
    /// changes, so a commit spar itself just read is not news.
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub requested_noticed: bool,
    #[serde(default)]
    pub assigned_noticed: bool,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub retry_after: u64,
    #[serde(default)]
    pub said_capped: bool,
    #[serde(default)]
    pub said_resume: bool,
    #[serde(default)]
    pub last_dispatched: Option<u64>,
}

impl Watched {
    fn new(kind: ItemKind) -> Self {
        Self {
            kind,
            title: String::new(),
            author: String::new(),
            head: None,
            requested_noticed: false,
            assigned_noticed: false,
            failures: 0,
            retry_after: 0,
            said_capped: false,
            said_resume: false,
            last_dispatched: None,
        }
    }
}

/// Everything the listener keeps between ticks and between runs, in
/// `.spar/state/listen.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListenState {
    pub version: u32,
    /// Threads updated after this are new. RFC 3339, the shape GitHub takes.
    #[serde(default)]
    pub watermark: String,
    /// `Last-Modified` of the last poll that answered, sent back as
    /// `If-Modified-Since` so a poll that finds nothing costs nothing.
    #[serde(default)]
    pub last_modified: Option<String>,
    /// Thread id to the `updated_at` it was last seen with, so one thread
    /// returned twice is one event.
    #[serde(default)]
    pub seen: BTreeMap<String, String>,
    #[serde(default)]
    pub items: BTreeMap<i64, Watched>,
    #[serde(default)]
    pub pending: BTreeMap<i64, Pending>,
    #[serde(default)]
    pub swept_once: bool,
}

impl ListenState {
    fn watched(&mut self, number: i64, kind: ItemKind) -> &mut Watched {
        self.items
            .entry(number)
            .or_insert_with(|| Watched::new(kind))
    }
}

// ---------------------------------------------------------------------------
// Pure pieces
// ---------------------------------------------------------------------------

/// `30m`, `2h`, `1d`, `1h30m`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let bad = || {
        spar_err!(
            "`--since` takes a number and a unit, like 30m, 2h or 1d, not {:?}",
            text.trim()
        )
    };
    let mut total = 0u64;
    let mut digits = String::new();
    let mut any = false;
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: u64 = digits.parse().map_err(|_| bad())?;
        digits.clear();
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(bad()),
        };
        total = total.saturating_add(n.saturating_mul(unit));
        any = true;
    }
    if !digits.is_empty() || !any {
        return Err(bad());
    }
    Ok(Duration::from_secs(total))
}

/// How long to wait before the next poll: the config, what GitHub asked for,
/// and the floor, whichever is largest.
pub fn poll_interval(cfg: &ListenCfg, header: Option<&str>) -> u64 {
    let asked = header
        .and_then(|h| h.trim().parse::<u64>().ok())
        .unwrap_or(0);
    cfg.interval.max(asked).max(FLOOR_SECS)
}

/// How long a rate limited answer says to wait, clamped to a minute and an
/// hour, or None when the answer was not about the rate limit.
pub fn rate_limit_wait(resp: &ApiResponse, now: u64) -> Option<u64> {
    if resp.status != 403 && resp.status != 429 {
        return None;
    }
    let retry_after = resp
        .headers
        .get("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok());
    let exhausted = resp
        .headers
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v.trim() == "0");
    let reset = resp
        .headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|at| at.saturating_sub(now));
    let wait = match (retry_after, exhausted, reset) {
        (Some(secs), _, _) => secs,
        (None, true, Some(secs)) => secs,
        _ => return None,
    };
    Some(wait.clamp(60, 3600))
}

/// The body's `message`, or the status line, for a failure that is not a
/// rate limit.
fn status_message(resp: &ApiResponse) -> String {
    serde_json::from_str::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(|m| format!("HTTP {}: {m}", resp.status))
        })
        .unwrap_or_else(|| format!("HTTP {}", resp.status))
}

/// The URL of the next page, from a `Link` header.
pub fn next_link(headers: &BTreeMap<String, String>) -> Option<String> {
    let link = headers.get("link")?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        if !rel.contains("rel=\"next\"") {
            return None;
        }
        let url = url.trim().trim_start_matches('<').trim_end_matches('>');
        (!url.is_empty()).then(|| url.to_string())
    })
}

/// The number at the end of an issue or pull request API URL.
pub fn number_from_url(url: &str) -> Option<i64> {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|last| last.parse().ok())
}

/// Turn a poll's threads into events, skipping what was already seen, and
/// advance the watermark past them.
pub fn events_from_threads(threads: &[Thread], state: &mut ListenState) -> Vec<Event> {
    let mut out = Vec::new();
    for thread in threads {
        if state.seen.get(&thread.id) == Some(&thread.updated_at) {
            continue;
        }
        if !thread.updated_at.is_empty() {
            state
                .seen
                .insert(thread.id.clone(), thread.updated_at.clone());
            if thread.updated_at > state.watermark {
                state.watermark = thread.updated_at.clone();
            }
        }
        let kind = match thread.subject.kind.as_str() {
            "PullRequest" => ItemKind::Pr,
            "Issue" => ItemKind::Issue,
            _ => continue,
        };
        let Some(number) = thread.subject.url.as_deref().and_then(number_from_url) else {
            continue;
        };
        let trigger = match thread.reason.as_str() {
            "review_requested" => Trigger::ReviewRequested,
            "assign" => Trigger::Assigned,
            "mention" | "team_mention" => Trigger::Mentioned,
            // comment, author, manual, state_change: somebody did something
            // on a thread the viewer is part of. The facts call learns whether
            // it closed.
            _ => Trigger::Replied,
        };
        out.push(Event {
            number,
            kind,
            trigger,
            who: None,
            head: None,
            commits: None,
        });
    }
    out
}

/// What a sweep of the open pull requests says happened.
///
/// `record_head` answers with the commit a posted review record holds, so a
/// pull request seen for the first time whose head already moved since its
/// review is a pending second look rather than nothing.
pub fn events_from_sweep(
    rows: &[PrSweepRow],
    viewer: &str,
    state: &mut ListenState,
    named: &BTreeSet<i64>,
    min_number: i64,
    record_head: &dyn Fn(i64) -> Option<String>,
    repo_mode: bool,
) -> Vec<Event> {
    let mut out = Vec::new();
    let mut present: BTreeSet<i64> = BTreeSet::new();
    for row in rows {
        if row.number < min_number && !named.contains(&row.number) {
            continue;
        }
        present.insert(row.number);
        let watched = state.watched(row.number, ItemKind::Pr);
        if !row.title.is_empty() {
            watched.title = row.title.clone();
        }
        if let Some(author) = &row.author {
            watched.author = author.login.clone();
        }
        if !row.state.eq_ignore_ascii_case("OPEN") {
            out.push(Event {
                number: row.number,
                kind: ItemKind::Pr,
                trigger: Trigger::Closed,
                who: None,
                head: None,
                commits: None,
            });
            continue;
        }
        let from = match &watched.head {
            Some(old) => Some(old.clone()),
            None => record_head(row.number),
        };
        if let Some(old) = from {
            if !row.head_ref_oid.is_empty() && !old.eq_ignore_ascii_case(&row.head_ref_oid) {
                out.push(Event {
                    number: row.number,
                    kind: ItemKind::Pr,
                    trigger: Trigger::Pushed,
                    who: None,
                    head: Some((old, row.head_ref_oid.clone())),
                    commits: None,
                });
            }
        }
        if !row.head_ref_oid.is_empty() {
            watched.head = Some(row.head_ref_oid.clone());
        }
        let asked = row
            .review_requests
            .iter()
            .any(|l| crate::comments::same_login(&l.login, viewer));
        if asked && !watched.requested_noticed {
            watched.requested_noticed = true;
            out.push(Event {
                number: row.number,
                kind: ItemKind::Pr,
                trigger: Trigger::ReviewRequested,
                who: None,
                head: None,
                commits: None,
            });
        }
        let assigned = row
            .assignees
            .iter()
            .any(|l| crate::comments::same_login(&l.login, viewer));
        if assigned && !watched.assigned_noticed {
            watched.assigned_noticed = true;
            out.push(Event {
                number: row.number,
                kind: ItemKind::Pr,
                trigger: Trigger::Assigned,
                who: None,
                head: None,
                commits: None,
            });
        }
    }
    if repo_mode {
        // A pull request that was open last time and is not in the list now
        // has closed or merged.
        let gone: Vec<i64> = state
            .items
            .iter()
            .filter(|(n, w)| w.kind == ItemKind::Pr && !present.contains(n))
            .map(|(n, _)| *n)
            .collect();
        for number in gone {
            out.push(Event {
                number,
                kind: ItemKind::Pr,
                trigger: Trigger::Closed,
                who: None,
                head: None,
                commits: None,
            });
        }
    }
    out
}

/// On the first sweep ever, keep only the `limit` lowest numbered pushes and
/// say how many were held back. Later sweeps see one push at a time.
pub fn cap_first_sweep(events: Vec<Event>, limit: usize, first: bool) -> (Vec<Event>, usize) {
    if !first {
        return (events, 0);
    }
    let mut pushes: Vec<&Event> = events
        .iter()
        .filter(|e| e.trigger == Trigger::Pushed)
        .collect();
    pushes.sort_by_key(|e| e.number);
    let allowed: BTreeSet<i64> = pushes.iter().take(limit).map(|e| e.number).collect();
    let held = pushes.len().saturating_sub(allowed.len());
    let kept = events
        .into_iter()
        .filter(|e| e.trigger != Trigger::Pushed || allowed.contains(&e.number))
        .collect();
    (kept, held)
}

/// Fold one event into what is pending for its item.
pub fn merge(pending: &mut BTreeMap<i64, Pending>, event: &Event, now: u64) {
    let p = pending.entry(event.number).or_insert_with(|| Pending {
        kind: event.kind,
        first_seen: now,
        last_seen: now,
        triggers: BTreeSet::new(),
        who: Vec::new(),
        head_from: None,
        head_to: None,
    });
    p.last_seen = now;
    p.triggers.insert(event.trigger);
    if let Some(who) = &event.who {
        if !p.who.contains(who) {
            p.who.push(who.clone());
        }
    }
    if let Some((from, to)) = &event.head {
        if p.head_from.is_none() {
            p.head_from = Some(from.clone());
        }
        p.head_to = Some(to.clone());
    }
}

/// Whether an item has settled: quiet for `settle` seconds, or pending for
/// `max_wait` however noisy, or `--once`.
pub fn due(p: &Pending, now: u64, cfg: &ListenCfg, once: bool) -> bool {
    once || now.saturating_sub(p.last_seen) >= cfg.settle
        || now.saturating_sub(p.first_seen) >= cfg.max_wait
}

/// When the earliest pending item becomes due.
pub fn next_due(pending: &BTreeMap<i64, Pending>, cfg: &ListenCfg) -> Option<u64> {
    pending
        .values()
        .map(|p| (p.last_seen + cfg.settle).min(p.first_seen + cfg.max_wait))
        .min()
}

/// What the dispatch table decided, before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A second look at this pull request. An issue routes to its pull request.
    Recheck(i64),
    /// A first look.
    Review,
    /// `spar checkin`, with or without the power to push.
    Checkin { reply_only: bool },
    /// Rechecked as often as the config allows; a person runs `spar review`.
    Capped,
    /// Custody state and no review record: `spar resume` owns it.
    BelongsToResume,
    /// Closed or merged: forget it.
    Drop,
    /// Nothing, with the reason.
    Ignore(&'static str),
}

impl Action {
    /// What the log says is about to happen to item `number`.
    pub fn phrase(&self, number: i64) -> String {
        match self {
            Action::Recheck(pr) if *pr == number => "take a second look".to_string(),
            Action::Recheck(pr) => format!("take a second look at #{pr}"),
            Action::Review => "review it".to_string(),
            Action::Checkin { reply_only: false } => "check in on it".to_string(),
            Action::Checkin { reply_only: true } => "answer the comments on it".to_string(),
            Action::Capped | Action::BelongsToResume => "leave it".to_string(),
            Action::Drop => "drop it".to_string(),
            Action::Ignore(_) => "ignore it".to_string(),
        }
    }

    fn runs(&self) -> bool {
        matches!(
            self,
            Action::Recheck(_) | Action::Review | Action::Checkin { .. }
        )
    }
}

/// What `plan` needs to know about an item beyond what is pending. Read once
/// per dispatch, never per tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub number: i64,
    pub named: bool,
    pub open: bool,
    pub author_is_viewer: bool,
    /// A review record the author has seen.
    pub has_posted_record: bool,
    pub recheck_capped: bool,
    /// The custody loop's `pr-N.json`.
    pub has_custody_state: bool,
    pub below_floor: bool,
    /// For an issue: its open pull request, when spar reviewed that.
    pub linked_pr_with_record: Option<i64>,
}

/// The dispatch table.
pub fn plan(p: &Pending, f: &Facts, cfg: &ListenCfg) -> Action {
    use Trigger::*;
    if !f.open || p.triggers.contains(&Closed) {
        return Action::Drop;
    }
    if f.below_floor && !f.named {
        return Action::Ignore("it is below min_number and was not named");
    }
    match p.kind {
        ItemKind::Issue => {
            let asked = p.triggers.contains(&Mentioned)
                || p.triggers.contains(&Assigned)
                || (f.named && p.triggers.contains(&Replied));
            if !asked {
                return Action::Ignore("nothing on the issue asked for spar");
            }
            match f.linked_pr_with_record {
                Some(pr) => Action::Recheck(pr),
                None => Action::Ignore("an issue with no reviewed pull request"),
            }
        }
        ItemKind::Pr => {
            // The viewer's own pull request first: it outranks a review
            // record, so a `spar review` somebody ran on their own branch does
            // not make every push a second look the same account cannot
            // approve.
            if f.author_is_viewer {
                let spoken_to = p
                    .triggers
                    .iter()
                    .any(|t| matches!(t, Replied | Mentioned | ReviewRequested));
                if !spoken_to {
                    return Action::Ignore("a push to your own pull request");
                }
                return match cfg.on_own_pr {
                    OwnPr::Checkin => Action::Checkin { reply_only: false },
                    OwnPr::Reply => Action::Checkin { reply_only: true },
                    OwnPr::Ignore => Action::Ignore("on_own_pr is ignore"),
                };
            }
            if f.has_posted_record {
                return if f.recheck_capped {
                    Action::Capped
                } else {
                    Action::Recheck(f.number)
                };
            }
            if f.has_custody_state {
                return Action::BelongsToResume;
            }
            if f.named {
                return Action::Review;
            }
            if p.triggers.contains(&ReviewRequested) {
                return match cfg.on_review_requested {
                    ReviewOrIgnore::Review => Action::Review,
                    ReviewOrIgnore::Ignore => Action::Ignore("on_review_requested is ignore"),
                };
            }
            if p.triggers.contains(&Assigned) {
                return match cfg.on_assigned {
                    ReviewOrIgnore::Review => Action::Review,
                    ReviewOrIgnore::Ignore => Action::Ignore("on_assigned is ignore"),
                };
            }
            if p.triggers.contains(&Mentioned) {
                return Action::Review;
            }
            Action::Ignore("a push or a comment on a pull request spar has not reviewed")
        }
    }
}

/// Forget threads not seen for a week.
pub fn prune_seen(state: &mut ListenState, now: u64) {
    state.seen.retain(|_, at| {
        clock::parse_rfc3339(at).is_none_or(|t| now.saturating_sub(t) < SEEN_FOR_SECS)
    });
}

/// A minute, doubling, to a quarter of an hour.
pub fn backoff_secs(failures: u32) -> u64 {
    (60u64 << failures.saturating_sub(1).min(10)).min(900)
}

/// A quarter of an hour, doubling, to six hours.
pub fn dispatch_backoff_secs(failures: u32) -> u64 {
    (900u64 << failures.saturating_sub(1).min(10)).min(6 * 3600)
}

/// What a `gh` failure is retried after, said loudly once and quietly after.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
    said: bool,
}

impl Backoff {
    fn failed(&mut self, what: &str, why: &str) -> u64 {
        self.failures += 1;
        let wait = backoff_secs(self.failures);
        let line = format!(
            "could not {what}: {why}. Trying again in {}.",
            spend::clock(Duration::from_secs(wait))
        );
        if self.said {
            logdim!("{line}");
        } else {
            logwarn!("{line}");
            self.said = true;
        }
        wait
    }

    fn recovered(&mut self) {
        if self.said {
            log!("gh is answering again");
        }
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// The lock
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct LockFile {
    pid: u32,
    started: String,
    touched: u64,
}

/// One listener per checkout. Two would read the same events and run the
/// same reviews twice.
#[derive(Debug)]
struct Lock {
    path: PathBuf,
}

impl Lock {
    fn acquire(repo: &Repo, stale_after: u64, now: u64) -> Result<Lock> {
        Lock::at(repo.listen_lock_path(), stale_after, now)
    }

    fn at(path: PathBuf, stale_after: u64, now: u64) -> Result<Lock> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    serde_json::to_writer_pretty(
                        file,
                        &LockFile {
                            pid: std::process::id(),
                            started: clock::rfc3339(now),
                            touched: now,
                        },
                    )?;
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let held: Option<LockFile> = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| serde_json::from_str(&text).ok());
                    let stale = held.as_ref().is_none_or(|h| {
                        !alive(h.pid) || now.saturating_sub(h.touched) > stale_after
                    });
                    if stale && attempt == 0 {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    let pid = held
                        .map(|h| h.pid.to_string())
                        .unwrap_or_else(|| "unknown".into());
                    bail!(
                        "another `spar listen` (pid {pid}) is already watching this checkout. \
                         Stop it first, or delete {} if that process is gone.",
                        path.display()
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("could not take the listen lock at {}", path.display())
    }

    fn touch(&self, now: u64) {
        let _ = crate::repo::write_json_atomic(
            &self.path,
            &LockFile {
                pid: std::process::id(),
                started: clock::rfc3339(now),
                touched: now,
            },
        );
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // Signal zero checks for the process without touching it. EPERM means it
    // is there and somebody else's.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

// ---------------------------------------------------------------------------
// The two sources
// ---------------------------------------------------------------------------

enum Polled {
    /// Nothing new, poll again after this many seconds.
    Nothing(u64),
    Threads(Vec<Thread>, u64),
    /// Rate limited, wait this long.
    RateLimited(u64),
    Failed(String),
}

fn poll(repo: &Repo, cfg: &ListenCfg, state: &mut ListenState, now: u64) -> Polled {
    let endpoint = format!(
        "repos/{{owner}}/{{repo}}/notifications?participating=true&all=true&per_page=100&since={}",
        state.watermark
    );
    let last_modified = state.last_modified.clone();
    let headers: Vec<(&str, &str)> = last_modified
        .as_deref()
        .map(|lm| vec![("If-Modified-Since", lm)])
        .unwrap_or_default();
    let resp = match repo.gh_api_raw(&endpoint, &headers) {
        Ok(resp) => resp,
        Err(e) => return Polled::Failed(e.last_line().to_string()),
    };
    let interval = poll_interval(cfg, resp.headers.get("x-poll-interval").map(String::as_str));
    match resp.status {
        304 => Polled::Nothing(interval),
        200 => {
            state.last_modified = resp
                .headers
                .get("last-modified")
                .or_else(|| resp.headers.get("date"))
                .cloned();
            let mut threads: Vec<Thread> = match serde_json::from_str(&resp.body) {
                Ok(threads) => threads,
                Err(e) => return Polled::Failed(format!("unexpected notification data: {e}")),
            };
            let mut next = next_link(&resp.headers);
            let mut pages = 1;
            while let Some(url) = next.take() {
                if pages >= MAX_PAGES {
                    break;
                }
                match repo.gh_api_raw(&url, &[]) {
                    Ok(more) if more.status == 200 => {
                        if let Ok(page) = serde_json::from_str::<Vec<Thread>>(&more.body) {
                            threads.extend(page);
                        }
                        next = next_link(&more.headers);
                        pages += 1;
                    }
                    _ => break,
                }
            }
            Polled::Threads(threads, interval)
        }
        401 => Polled::Failed("gh is not authenticated; run `gh auth status`".into()),
        403 | 429 => match rate_limit_wait(&resp, now) {
            Some(wait) => Polled::RateLimited(wait),
            None => Polled::Failed(status_message(&resp)),
        },
        _ => Polled::Failed(status_message(&resp)),
    }
}

/// The rows a sweep looks at: every open pull request, or the named ones.
fn sweep_rows(
    repo: &Repo,
    cfg: &Config,
    named_prs: &[i64],
    named_issues: &[i64],
    state: &mut ListenState,
) -> Result<Vec<PrSweepRow>> {
    if named_prs.is_empty() && named_issues.is_empty() {
        return repo.sweep_open_prs();
    }
    let mut rows = Vec::new();
    for number in named_prs {
        rows.push(repo.sweep_pr(*number)?);
    }
    for issue in named_issues {
        // A named issue's open pull request joins the watch once spar has
        // reviewed it, so a push there is a second look.
        if let Ok(Some(pr)) = repo.try_open_pr_for_issue(*issue, cfg.base_branch()) {
            if repo.read_review_record(pr.number).is_some_and(|r| r.posted) {
                state.watched(pr.number, ItemKind::Pr);
                rows.push(repo.sweep_pr(pr.number)?);
            }
        }
    }
    Ok(rows)
}

fn facts(repo: &Repo, cfg: &Config, number: i64, kind: ItemKind, named: bool) -> Result<Facts> {
    let viewer = repo.viewer_login()?.to_string();
    let below_floor = number < cfg.loop_cfg.min_number;
    match kind {
        ItemKind::Pr => {
            let row = repo.sweep_pr(number)?;
            let record = repo.read_review_record(number);
            let posted = record.as_ref().is_some_and(|r| r.posted);
            Ok(Facts {
                number,
                named,
                open: row.state.eq_ignore_ascii_case("OPEN"),
                author_is_viewer: row
                    .author
                    .as_ref()
                    .is_some_and(|a| crate::comments::same_login(&a.login, &viewer)),
                has_posted_record: posted,
                recheck_capped: posted
                    && record
                        .as_ref()
                        .is_some_and(|r| r.rechecks >= cfg.listen.max_rechecks),
                has_custody_state: repo.state_path(number).exists(),
                below_floor,
                linked_pr_with_record: None,
            })
        }
        ItemKind::Issue => {
            let issue = repo.read_issue(number)?;
            let linked = repo
                .try_open_pr_for_issue(number, cfg.base_branch())
                .ok()
                .flatten()
                .map(|pr| pr.number)
                .filter(|pr| repo.read_review_record(*pr).is_some_and(|r| r.posted));
            Ok(Facts {
                number,
                named,
                open: issue.state.eq_ignore_ascii_case("OPEN"),
                author_is_viewer: false,
                has_posted_record: false,
                recheck_capped: false,
                has_custody_state: false,
                below_floor,
                linked_pr_with_record: linked,
            })
        }
    }
}

fn dispatch(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    action: &Action,
    number: i64,
    p: &Pending,
) -> IssueRun {
    match action {
        Action::Recheck(pr) => review_only::recheck_pr(
            agents,
            cfg,
            repo,
            *pr,
            RecheckMode {
                dry_run: false,
                requested: p.triggers.contains(&Trigger::ReviewRequested),
                verdict_on_fallback: cfg.listen.verdicts,
            },
        ),
        Action::Review => review_only::review_pr(
            agents,
            cfg,
            repo,
            number,
            ReviewOpts {
                dry_run: false,
                verdict: cfg.listen.verdicts,
            },
        ),
        Action::Checkin { reply_only } => checkin::checkin_pr(
            agents,
            cfg,
            repo,
            number,
            &checkin::Mode {
                dry_run: false,
                reply_only: *reply_only,
                trust: cfg.loop_cfg.checkin_trust,
                again: false,
                resolve: cfg.loop_cfg.checkin_resolve,
                posts: checkin::posts(cfg),
            },
        ),
        other => {
            let mut run = IssueRun::new(number, format!("#{number}"));
            run.status = Status::Error;
            run.notes
                .push(format!("nothing to run: {}", other.phrase(number)));
            run
        }
    }
}

/// One event, as the log says it.
fn describe(repo: &Repo, event: &Event) -> String {
    let n = event.number;
    match event.trigger {
        Trigger::Pushed => {
            let (from, to) = event.head.clone().unwrap_or_default();
            let short = |s: &str| s.chars().take(7).collect::<String>();
            let commits = repo
                .commits_between(&from, &to)
                .map(|c| format!(", {c} new commit{} since the review", plural(c as usize)))
                .unwrap_or_default();
            format!(
                "#{n}: head moved {} to {}{commits}",
                short(&from),
                short(&to)
            )
        }
        Trigger::Replied => match &event.who {
            Some(who) => format!("#{n}: {who} commented"),
            None => format!("#{n}: somebody commented"),
        },
        Trigger::Mentioned => match &event.who {
            Some(who) => format!("#{n}: mentioned by {who}"),
            None => format!("#{n}: mentioned"),
        },
        Trigger::ReviewRequested => format!("#{n}: your review was requested"),
        Trigger::Assigned => format!("#{n}: assigned to you"),
        Trigger::Closed => format!("#{n}: closed, no longer watched"),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

enum Ending {
    Interrupted,
    Once { failed: bool },
    Done,
}

/// Listen until stopped, or once.
pub fn run(agents: &[Agent], cfg: &Config, repo: &Repo, opts: Opts) -> Result<i32> {
    proc::stop_on_signal()?;
    if !cfg!(unix) {
        logdim!(
            "Ctrl-C ends spar at once on this platform; an agent call in flight is not stopped"
        );
    }
    let now = clock::now_secs();
    let stale_after = 3 * cfg.listen.interval.max(cfg.listen.sweep);
    let lock = Lock::acquire(repo, stale_after, now)?;
    let viewer = repo.viewer_login()?.to_string();
    let name = repo
        .repo_name()
        .unwrap_or_else(|| repo.root().display().to_string());

    // Named items, sorted into kinds. An unknown number is a hard error, as
    // it is everywhere else: naming something that does not exist is a typo.
    let named: BTreeSet<i64> = opts.items.iter().copied().collect();
    let mut named_prs = Vec::new();
    let mut named_issues = Vec::new();
    for number in &named {
        match repo.item_kind(*number)? {
            ItemKind::Pr => named_prs.push(*number),
            ItemKind::Issue => named_issues.push(*number),
        }
    }
    let repo_mode = named.is_empty();

    let mut state = read_state(repo);
    if state.watermark.is_empty() || opts.since.is_some() {
        let back = opts.since.map(|d| d.as_secs()).unwrap_or(0);
        state.watermark = clock::rfc3339(now.saturating_sub(back));
        state.last_modified = None;
        log!("watching from {}", state.watermark);
    }
    for number in &named_prs {
        state.watched(*number, ItemKind::Pr);
    }
    for number in &named_issues {
        state.watched(*number, ItemKind::Issue);
    }
    log!(
        "listening to {name}{}: polling every {}s, sweeping {} every {}s. Ctrl-C to stop.",
        if repo_mode {
            String::new()
        } else {
            format!(
                " for {}",
                named
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        cfg.listen.interval.max(FLOOR_SECS),
        if repo_mode {
            "open pull requests"
        } else {
            "the named items"
        },
        cfg.listen.sweep
    );

    let started = now;
    let mut next_poll = now;
    let mut next_sweep = now;
    let mut backoff = Backoff::default();
    let mut events_seen = 0usize;
    let mut dispatches = 0usize;
    let mut tick_failed = false;
    let mut last_prune = now;

    let ending = loop {
        if proc::interrupted() {
            break Ending::Interrupted;
        }
        let now = clock::now_secs();
        let mut events: Vec<Event> = Vec::new();

        if now >= next_poll {
            match poll(repo, &cfg.listen, &mut state, now) {
                Polled::Nothing(interval) => {
                    backoff.recovered();
                    next_poll = now + interval;
                }
                Polled::Threads(threads, interval) => {
                    backoff.recovered();
                    events.extend(events_from_threads(&threads, &mut state));
                    next_poll = now + interval;
                }
                Polled::RateLimited(wait) => {
                    logwarn!(
                        "rate limited by GitHub, waiting {} until it resets",
                        spend::clock(Duration::from_secs(wait))
                    );
                    next_poll = now + wait;
                    tick_failed = true;
                }
                Polled::Failed(why) => {
                    next_poll = now + backoff.failed("read notifications", &why);
                    tick_failed = true;
                }
            }
        }

        if now >= next_sweep {
            match sweep_rows(repo, cfg, &named_prs, &named_issues, &mut state) {
                Ok(rows) => {
                    backoff.recovered();
                    let record_head = |n: i64| {
                        repo.read_review_record(n)
                            .filter(|r| r.posted)
                            .map(|r| r.head)
                    };
                    let swept = events_from_sweep(
                        &rows,
                        &viewer,
                        &mut state,
                        &named,
                        cfg.loop_cfg.min_number,
                        &record_head,
                        repo_mode,
                    );
                    let (kept, held) = cap_first_sweep(swept, opts.limit, !state.swept_once);
                    if held > 0 {
                        log!(
                            "{held} more reviewed pull request{} moved since their review; raise \
                             --limit or run `spar review` on them",
                            plural(held)
                        );
                    }
                    events.extend(kept);
                    state.swept_once = true;
                    next_sweep = now + cfg.listen.sweep;
                }
                Err(e) => {
                    next_sweep = now + backoff.failed("sweep the pull requests", e.last_line());
                    tick_failed = true;
                }
            }
        }

        // Events are merged with no filter by number: in numbers mode the
        // table drops what was not named, after the watermark has moved past
        // it, so a thread about something else is read once and never again.
        for event in &events {
            if !repo_mode && !named.contains(&event.number) {
                // Unless it is the pull request of a named issue, which the
                // sweep put on the watch.
                if !state.items.contains_key(&event.number) {
                    continue;
                }
            }
            if event.trigger != Trigger::Closed && event.who.is_none() {
                // Nothing to look up cheaply here; the actor comes with the
                // thread only through its latest comment, which the sweep
                // does not carry.
            }
            events_seen += 1;
            log!("{}", describe(repo, event));
            if event.trigger == Trigger::Closed {
                state.items.remove(&event.number);
                state.pending.remove(&event.number);
                continue;
            }
            state.watched(event.number, event.kind);
            merge(&mut state.pending, event, now);
        }
        write_state(repo, &state);
        lock.touch(now);

        let due_now: Vec<i64> = state
            .pending
            .iter()
            .filter(|(n, p)| {
                due(p, now, &cfg.listen, opts.once)
                    && state.items.get(n).is_none_or(|w| w.retry_after <= now)
            })
            .map(|(n, _)| *n)
            .collect();
        for number in due_now {
            let Some(p) = state.pending.get(&number).cloned() else {
                continue;
            };
            let named_item = named.contains(&number);
            let f = match facts(repo, cfg, number, p.kind, named_item) {
                Ok(f) => f,
                Err(e) => {
                    let w = state.watched(number, p.kind);
                    w.failures += 1;
                    w.retry_after = now + dispatch_backoff_secs(w.failures);
                    logwarn!("#{number}: could not read it: {}", e.last_line());
                    tick_failed = true;
                    write_state(repo, &state);
                    continue;
                }
            };
            let action = plan(&p, &f, &cfg.listen);
            match &action {
                Action::Ignore(why) => {
                    logdim!("#{number}: ignoring {why}");
                    state.pending.remove(&number);
                }
                Action::Drop => {
                    log!("#{number}: closed, no longer watched");
                    state.items.remove(&number);
                    state.pending.remove(&number);
                }
                Action::Capped => {
                    let w = state.watched(number, p.kind);
                    if !w.said_capped {
                        log!(
                            "#{number}: rechecked {} time{} already; run `spar review {number}` to \
                             look again",
                            cfg.listen.max_rechecks,
                            plural(cfg.listen.max_rechecks as usize)
                        );
                        w.said_capped = true;
                    }
                    state.pending.remove(&number);
                }
                Action::BelongsToResume => {
                    let w = state.watched(number, p.kind);
                    if !w.said_resume {
                        log!("#{number}: in custody, `spar resume {number}` continues it");
                        w.said_resume = true;
                    }
                    state.pending.remove(&number);
                }
                action if action.runs() => {
                    if opts.dry_run {
                        log!(
                            "#{number}: would {} after {}, dry run so nothing runs",
                            action.phrase(number),
                            p.because()
                        );
                        state.pending.remove(&number);
                    } else {
                        log!(
                            "#{number}: about to {} after {}",
                            action.phrase(number),
                            p.because()
                        );
                        let run = dispatch(agents, cfg, repo, action, number, &p);
                        dispatches += 1;
                        let spent = spend::drain();
                        if let Err(e) = repo.record_spend(&spent) {
                            logdim!("could not record what this run spent: {e}");
                        }
                        crate::cli::print_outcome(&run, &spent);
                        if proc::interrupted() {
                            log!("interrupted while working on #{number}; its worktree was kept");
                            write_state(repo, &state);
                            break;
                        }
                        let acted_on = match action {
                            Action::Recheck(pr) => *pr,
                            _ => number,
                        };
                        if let Some(head) = repo.read_review_record(acted_on).map(|r| r.head) {
                            // The commit spar just read is not news to the
                            // next sweep.
                            state.watched(acted_on, ItemKind::Pr).head = Some(head);
                        }
                        let w = state.watched(number, p.kind);
                        if run.status == Status::Error {
                            w.failures += 1;
                            w.retry_after = now + dispatch_backoff_secs(w.failures);
                            if w.failures >= MAX_FAILURES {
                                logwarn!("#{number}: giving up on it until it changes again");
                                state.pending.remove(&number);
                            } else if let Some(p) = state.pending.get_mut(&number) {
                                p.first_seen = now;
                            }
                            tick_failed = true;
                        } else {
                            w.failures = 0;
                            w.retry_after = 0;
                            w.last_dispatched = Some(now);
                            state.pending.remove(&number);
                        }
                    }
                }
                _ => {}
            }
            write_state(repo, &state);
        }
        if proc::interrupted() {
            break Ending::Interrupted;
        }

        if opts.once {
            break Ending::Once {
                failed: tick_failed,
            };
        }
        if !repo_mode && named.iter().all(|n| !state.items.contains_key(n)) {
            log!("every named item is closed, nothing left to watch");
            break Ending::Done;
        }
        if now.saturating_sub(last_prune) >= 3600 {
            prune_seen(&mut state, now);
            last_prune = now;
            write_state(repo, &state);
        }

        let next = [next_poll, next_sweep]
            .into_iter()
            .chain(next_due(&state.pending, &cfg.listen))
            .min()
            .unwrap_or(now + FLOOR_SECS);
        let wait = next.saturating_sub(clock::now_secs()).clamp(1, 3600);
        if proc::sleep_unless_interrupted(Duration::from_secs(wait)) {
            break Ending::Interrupted;
        }
    };

    write_state(repo, &state);
    drop(lock);
    log!(
        "listened for {}: {events_seen} event{}, {dispatches} dispatch{}",
        spend::clock(Duration::from_secs(
            clock::now_secs().saturating_sub(started)
        )),
        plural(events_seen),
        if dispatches == 1 { "" } else { "es" }
    );
    let writes = crate::cli::report_writes(repo);
    Ok(match ending {
        Ending::Interrupted => 130.max(writes),
        Ending::Once { failed: true } => 1.max(writes),
        Ending::Once { failed: false } | Ending::Done => writes,
    })
}

fn read_state(repo: &Repo) -> ListenState {
    let path = repo.listen_state_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<ListenState>(&text) {
            Ok(state) => state,
            Err(e) => {
                logdim!("could not read {}: {e}. Starting fresh.", path.display());
                ListenState::default()
            }
        },
        Err(_) => ListenState::default(),
    }
}

fn write_state(repo: &Repo, state: &ListenState) {
    let mut state = state.clone();
    state.version = STATE_VERSION;
    if let Err(e) = crate::repo::write_json_atomic(&repo.listen_state_path(), &state) {
        logdim!("could not write the listen state: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Login};

    fn cfg() -> ListenCfg {
        ListenCfg::default()
    }

    fn thread(id: &str, reason: &str, kind: &str, number: i64, at: &str) -> Thread {
        Thread {
            id: id.into(),
            reason: reason.into(),
            updated_at: at.into(),
            subject: Subject {
                title: format!("thing {number}"),
                url: Some(format!(
                    "https://api.github.com/repos/o/r/{}/{number}",
                    if kind == "PullRequest" {
                        "pulls"
                    } else {
                        "issues"
                    }
                )),
                latest_comment_url: None,
                kind: kind.into(),
            },
        }
    }

    fn row(number: i64, head: &str, author: &str, state: &str) -> PrSweepRow {
        PrSweepRow {
            number,
            title: format!("PR {number}"),
            head_ref_oid: head.into(),
            author: Some(Author {
                login: author.into(),
            }),
            state: state.into(),
            ..Default::default()
        }
    }

    fn pending(kind: ItemKind, triggers: &[Trigger], now: u64) -> Pending {
        Pending {
            kind,
            first_seen: now,
            last_seen: now,
            triggers: triggers.iter().copied().collect(),
            who: Vec::new(),
            head_from: None,
            head_to: None,
        }
    }

    fn open_pr() -> Facts {
        Facts {
            number: 7,
            open: true,
            ..Default::default()
        }
    }

    fn response(status: u16, headers: &[(&str, &str)]) -> ApiResponse {
        ApiResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: String::new(),
        }
    }

    // -- timing ----------------------------------------------------------------

    #[test]
    fn the_poll_interval_is_the_largest_of_the_config_the_header_and_the_floor() {
        let mut c = cfg();
        assert_eq!(60, poll_interval(&c, None));
        assert_eq!(90, poll_interval(&c, Some("90")));
        assert_eq!(60, poll_interval(&c, Some("30")));
        assert_eq!(60, poll_interval(&c, Some("soon")));
        c.interval = 5;
        assert_eq!(15, poll_interval(&c, None));
    }

    #[test]
    fn a_rate_limit_answer_says_how_long_to_wait_and_caps_it_at_an_hour() {
        let now = 1_000_000;
        assert_eq!(
            Some(120),
            rate_limit_wait(&response(429, &[("retry-after", "120")]), now)
        );
        assert_eq!(
            Some(600),
            rate_limit_wait(
                &response(
                    403,
                    &[
                        ("x-ratelimit-remaining", "0"),
                        ("x-ratelimit-reset", "1000600")
                    ]
                ),
                now
            )
        );
        assert_eq!(
            Some(3600),
            rate_limit_wait(&response(403, &[("retry-after", "99999")]), now)
        );
        assert_eq!(
            Some(60),
            rate_limit_wait(&response(429, &[("retry-after", "1")]), now)
        );
    }

    #[test]
    fn a_forbidden_answer_without_a_rate_limit_header_is_a_failure_not_a_wait() {
        assert_eq!(None, rate_limit_wait(&response(403, &[]), 0));
        assert_eq!(
            None,
            rate_limit_wait(&response(403, &[("x-ratelimit-remaining", "40")]), 0)
        );
        assert_eq!(
            None,
            rate_limit_wait(&response(200, &[("retry-after", "5")]), 0)
        );
    }

    #[test]
    fn next_link_reads_the_next_page_and_nothing_else() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "link".to_string(),
            "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\"".to_string(),
        );
        assert_eq!(
            Some("https://api.github.com/x?page=2".to_string()),
            next_link(&headers)
        );
        headers.insert(
            "link".to_string(),
            "<https://api.github.com/x?page=1>; rel=\"prev\"".to_string(),
        );
        assert_eq!(None, next_link(&headers));
        assert_eq!(None, next_link(&BTreeMap::new()));
    }

    // -- notifications ------------------------------------------------------------

    #[test]
    fn a_thread_becomes_an_event_keyed_by_its_number_and_reason() {
        let mut state = ListenState::default();
        let threads = vec![
            thread(
                "1",
                "review_requested",
                "PullRequest",
                7,
                "2026-10-20T10:00:00Z",
            ),
            thread("2", "mention", "Issue", 42, "2026-10-20T10:01:00Z"),
            thread("3", "comment", "PullRequest", 8, "2026-10-20T10:02:00Z"),
            thread("4", "assign", "PullRequest", 9, "2026-10-20T10:03:00Z"),
            thread("5", "team_mention", "Issue", 43, "2026-10-20T10:04:00Z"),
            thread(
                "6",
                "state_change",
                "PullRequest",
                10,
                "2026-10-20T10:05:00Z",
            ),
        ];
        let events = events_from_threads(&threads, &mut state);
        let got: Vec<(i64, ItemKind, Trigger)> = events
            .iter()
            .map(|e| (e.number, e.kind, e.trigger))
            .collect();
        assert_eq!(
            vec![
                (7, ItemKind::Pr, Trigger::ReviewRequested),
                (42, ItemKind::Issue, Trigger::Mentioned),
                (8, ItemKind::Pr, Trigger::Replied),
                (9, ItemKind::Pr, Trigger::Assigned),
                (43, ItemKind::Issue, Trigger::Mentioned),
                (10, ItemKind::Pr, Trigger::Replied),
            ],
            got
        );
    }

    #[test]
    fn a_thread_seen_at_the_same_update_time_is_not_an_event_twice() {
        let mut state = ListenState::default();
        let same = vec![thread(
            "1",
            "comment",
            "PullRequest",
            7,
            "2026-10-20T10:00:00Z",
        )];
        assert_eq!(1, events_from_threads(&same, &mut state).len());
        assert_eq!(0, events_from_threads(&same, &mut state).len());
        let moved = vec![thread(
            "1",
            "comment",
            "PullRequest",
            7,
            "2026-10-20T10:05:00Z",
        )];
        assert_eq!(1, events_from_threads(&moved, &mut state).len());
    }

    #[test]
    fn a_thread_about_a_release_or_discussion_is_ignored() {
        let mut state = ListenState::default();
        let threads = vec![
            thread("1", "subscribed", "Release", 1, "2026-10-20T10:00:00Z"),
            thread("2", "mention", "Discussion", 2, "2026-10-20T10:01:00Z"),
            Thread {
                subject: Subject {
                    url: None,
                    kind: "PullRequest".into(),
                    ..Default::default()
                },
                ..thread("3", "comment", "PullRequest", 3, "2026-10-20T10:02:00Z")
            },
        ];
        assert!(events_from_threads(&threads, &mut state).is_empty());
        // Seen and past the watermark all the same, so they are never read
        // again.
        assert_eq!(3, state.seen.len());
    }

    #[test]
    fn the_watermark_advances_to_the_newest_update_seen() {
        let mut state = ListenState {
            watermark: "2026-10-20T09:00:00Z".into(),
            ..Default::default()
        };
        let threads = vec![
            thread("1", "comment", "PullRequest", 7, "2026-10-20T10:02:00Z"),
            thread("2", "comment", "PullRequest", 8, "2026-10-20T10:01:00Z"),
        ];
        events_from_threads(&threads, &mut state);
        assert_eq!("2026-10-20T10:02:00Z", state.watermark);
    }

    // -- the sweep ----------------------------------------------------------------

    fn no_record(_: i64) -> Option<String> {
        None
    }

    #[test]
    fn a_sweep_notices_a_head_that_moved_and_says_it_once() {
        let mut state = ListenState::default();
        let named = BTreeSet::new();
        let first = events_from_sweep(
            &[row(7, "aaa", "alice", "OPEN")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            true,
        );
        assert!(first.is_empty(), "{first:?}");
        let moved = events_from_sweep(
            &[row(7, "bbb", "alice", "OPEN")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            true,
        );
        assert_eq!(1, moved.len());
        assert_eq!(Trigger::Pushed, moved[0].trigger);
        assert_eq!(Some(("aaa".to_string(), "bbb".to_string())), moved[0].head);
        let again = events_from_sweep(
            &[row(7, "bbb", "alice", "OPEN")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            true,
        );
        assert!(again.is_empty(), "{again:?}");
        assert_eq!("alice", state.items[&7].author);
    }

    #[test]
    fn the_first_sweep_treats_a_reviewed_pull_request_that_already_moved_as_due() {
        let mut state = ListenState::default();
        let record = |n: i64| (n == 7).then(|| "aaa".to_string());
        let events = events_from_sweep(
            &[row(7, "bbb", "alice", "OPEN"), row(8, "ccc", "bob", "OPEN")],
            "me",
            &mut state,
            &BTreeSet::new(),
            0,
            &record,
            true,
        );
        assert_eq!(1, events.len());
        assert_eq!(7, events[0].number);
        assert_eq!(Trigger::Pushed, events[0].trigger);
    }

    #[test]
    fn a_sweep_notices_a_review_request_once_not_every_five_minutes() {
        let mut state = ListenState::default();
        let mut asked = row(7, "aaa", "alice", "OPEN");
        asked.review_requests = vec![Login { login: "ME".into() }];
        asked.assignees = vec![Login { login: "me".into() }];
        let rows = [asked];
        let first = events_from_sweep(
            &rows,
            "me",
            &mut state,
            &BTreeSet::new(),
            0,
            &no_record,
            true,
        );
        let triggers: BTreeSet<Trigger> = first.iter().map(|e| e.trigger).collect();
        assert_eq!(
            [Trigger::ReviewRequested, Trigger::Assigned]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            triggers
        );
        let second = events_from_sweep(
            &rows,
            "me",
            &mut state,
            &BTreeSet::new(),
            0,
            &no_record,
            true,
        );
        assert!(second.is_empty(), "{second:?}");
    }

    #[test]
    fn a_sweep_drops_a_pull_request_that_closed_or_vanished() {
        let mut state = ListenState::default();
        let named = BTreeSet::new();
        events_from_sweep(
            &[row(7, "aaa", "alice", "OPEN"), row(8, "bbb", "bob", "OPEN")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            true,
        );
        let events = events_from_sweep(
            &[row(7, "aaa", "alice", "MERGED")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            true,
        );
        let closed: Vec<i64> = events
            .iter()
            .filter(|e| e.trigger == Trigger::Closed)
            .map(|e| e.number)
            .collect();
        assert_eq!(vec![7, 8], closed);

        // In numbers mode a pull request missing from the rows means it was
        // not asked about, not that it closed.
        let mut state = ListenState::default();
        let named: BTreeSet<i64> = [7, 8].into_iter().collect();
        events_from_sweep(
            &[row(7, "aaa", "alice", "OPEN"), row(8, "bbb", "bob", "OPEN")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            false,
        );
        let events = events_from_sweep(
            &[row(7, "aaa", "alice", "OPEN")],
            "me",
            &mut state,
            &named,
            0,
            &no_record,
            false,
        );
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_sweep_ignores_a_number_below_the_floor_unless_it_was_named() {
        let mut state = ListenState::default();
        let named: BTreeSet<i64> = [3].into_iter().collect();
        let record = |_: i64| Some("old".to_string());
        let events = events_from_sweep(
            &[
                row(3, "new", "alice", "OPEN"),
                row(4, "new", "alice", "OPEN"),
                row(500, "new", "alice", "OPEN"),
            ],
            "me",
            &mut state,
            &named,
            480,
            &record,
            true,
        );
        let numbers: Vec<i64> = events.iter().map(|e| e.number).collect();
        assert_eq!(vec![3, 500], numbers);
        assert!(!state.items.contains_key(&4));
    }

    #[test]
    fn the_first_sweep_caps_pending_rechecks_at_the_limit_and_counts_the_rest() {
        let push = |n: i64| Event {
            number: n,
            kind: ItemKind::Pr,
            trigger: Trigger::Pushed,
            who: None,
            head: None,
            commits: None,
        };
        let asked = Event {
            trigger: Trigger::ReviewRequested,
            ..push(50)
        };
        let events = vec![push(9), push(3), push(7), asked.clone()];
        let (kept, held) = cap_first_sweep(events.clone(), 2, true);
        let numbers: Vec<i64> = kept.iter().map(|e| e.number).collect();
        assert_eq!(vec![3, 7, 50], numbers, "{kept:?}");
        assert_eq!(1, held);
        let (kept, held) = cap_first_sweep(events, 2, false);
        assert_eq!(4, kept.len());
        assert_eq!(0, held);
    }

    // -- the table -------------------------------------------------------------------

    #[test]
    fn a_push_or_a_reply_on_a_reviewed_pull_request_is_a_recheck() {
        let f = Facts {
            has_posted_record: true,
            ..open_pr()
        };
        for t in [
            Trigger::Pushed,
            Trigger::Replied,
            Trigger::Mentioned,
            Trigger::ReviewRequested,
        ] {
            assert_eq!(
                Action::Recheck(7),
                plan(&pending(ItemKind::Pr, &[t], 0), &f, &cfg()),
                "{t:?}"
            );
        }
    }

    #[test]
    fn a_review_request_or_an_assignment_on_an_unreviewed_pull_request_follows_the_config() {
        let f = open_pr();
        let mut c = cfg();
        assert_eq!(
            Action::Review,
            plan(
                &pending(ItemKind::Pr, &[Trigger::ReviewRequested], 0),
                &f,
                &c
            )
        );
        assert_eq!(
            Action::Review,
            plan(&pending(ItemKind::Pr, &[Trigger::Assigned], 0), &f, &c)
        );
        assert_eq!(
            Action::Review,
            plan(&pending(ItemKind::Pr, &[Trigger::Mentioned], 0), &f, &c)
        );
        c.on_review_requested = ReviewOrIgnore::Ignore;
        c.on_assigned = ReviewOrIgnore::Ignore;
        assert!(matches!(
            plan(
                &pending(ItemKind::Pr, &[Trigger::ReviewRequested], 0),
                &f,
                &c
            ),
            Action::Ignore(_)
        ));
        assert!(matches!(
            plan(&pending(ItemKind::Pr, &[Trigger::Assigned], 0), &f, &c),
            Action::Ignore(_)
        ));
        // A mention is always answered with a review.
        assert_eq!(
            Action::Review,
            plan(&pending(ItemKind::Pr, &[Trigger::Mentioned], 0), &f, &c)
        );
    }

    #[test]
    fn a_push_to_an_unreviewed_pull_request_is_ignored_unless_it_was_named() {
        let f = open_pr();
        assert!(matches!(
            plan(&pending(ItemKind::Pr, &[Trigger::Pushed], 0), &f, &cfg()),
            Action::Ignore(_)
        ));
        assert!(matches!(
            plan(&pending(ItemKind::Pr, &[Trigger::Replied], 0), &f, &cfg()),
            Action::Ignore(_)
        ));
        let named = Facts {
            named: true,
            ..open_pr()
        };
        assert_eq!(
            Action::Review,
            plan(
                &pending(ItemKind::Pr, &[Trigger::Pushed], 0),
                &named,
                &cfg()
            )
        );
        assert_eq!(
            Action::Review,
            plan(
                &pending(ItemKind::Pr, &[Trigger::Replied], 0),
                &named,
                &cfg()
            )
        );
    }

    #[test]
    fn your_own_pull_request_follows_on_own_pr_and_a_push_to_it_is_ignored() {
        let f = Facts {
            author_is_viewer: true,
            has_posted_record: true,
            ..open_pr()
        };
        let mut c = cfg();
        assert_eq!(
            Action::Checkin { reply_only: false },
            plan(&pending(ItemKind::Pr, &[Trigger::Replied], 0), &f, &c)
        );
        assert_eq!(
            Action::Checkin { reply_only: false },
            plan(
                &pending(ItemKind::Pr, &[Trigger::ReviewRequested], 0),
                &f,
                &c
            )
        );
        assert!(matches!(
            plan(&pending(ItemKind::Pr, &[Trigger::Pushed], 0), &f, &c),
            Action::Ignore(_)
        ));
        c.on_own_pr = OwnPr::Reply;
        assert_eq!(
            Action::Checkin { reply_only: true },
            plan(&pending(ItemKind::Pr, &[Trigger::Mentioned], 0), &f, &c)
        );
        c.on_own_pr = OwnPr::Ignore;
        assert!(matches!(
            plan(&pending(ItemKind::Pr, &[Trigger::Replied], 0), &f, &c),
            Action::Ignore(_)
        ));
    }

    #[test]
    fn a_pull_request_in_custody_belongs_to_resume_and_a_capped_one_waits_for_a_person() {
        let custody = Facts {
            has_custody_state: true,
            ..open_pr()
        };
        assert_eq!(
            Action::BelongsToResume,
            plan(
                &pending(ItemKind::Pr, &[Trigger::Pushed], 0),
                &custody,
                &cfg()
            )
        );
        let capped = Facts {
            has_posted_record: true,
            recheck_capped: true,
            ..open_pr()
        };
        assert_eq!(
            Action::Capped,
            plan(
                &pending(ItemKind::Pr, &[Trigger::Pushed], 0),
                &capped,
                &cfg()
            )
        );
    }

    #[test]
    fn an_issue_mention_without_a_pull_request_is_ignored_and_with_one_rechecks_it() {
        let issue = Facts {
            number: 42,
            open: true,
            ..Default::default()
        };
        assert!(matches!(
            plan(
                &pending(ItemKind::Issue, &[Trigger::Mentioned], 0),
                &issue,
                &cfg()
            ),
            Action::Ignore(_)
        ));
        assert!(matches!(
            plan(
                &pending(ItemKind::Issue, &[Trigger::Replied], 0),
                &issue,
                &cfg()
            ),
            Action::Ignore(_)
        ));
        let linked = Facts {
            linked_pr_with_record: Some(108),
            ..issue.clone()
        };
        assert_eq!(
            Action::Recheck(108),
            plan(
                &pending(ItemKind::Issue, &[Trigger::Mentioned], 0),
                &linked,
                &cfg()
            )
        );
        assert_eq!(
            Action::Recheck(108),
            plan(
                &pending(ItemKind::Issue, &[Trigger::Assigned], 0),
                &linked,
                &cfg()
            )
        );
        // A plain comment on a named issue counts as asking.
        let named = Facts {
            named: true,
            ..linked
        };
        assert_eq!(
            Action::Recheck(108),
            plan(
                &pending(ItemKind::Issue, &[Trigger::Replied], 0),
                &named,
                &cfg()
            )
        );
    }

    #[test]
    fn a_closed_item_is_dropped_and_one_below_the_floor_is_ignored_unless_named() {
        let closed = Facts {
            open: false,
            has_posted_record: true,
            ..open_pr()
        };
        assert_eq!(
            Action::Drop,
            plan(
                &pending(ItemKind::Pr, &[Trigger::Pushed], 0),
                &closed,
                &cfg()
            )
        );
        let f = Facts {
            has_posted_record: true,
            ..open_pr()
        };
        assert_eq!(
            Action::Drop,
            plan(
                &pending(ItemKind::Pr, &[Trigger::Pushed, Trigger::Closed], 0),
                &f,
                &cfg()
            )
        );
        let low = Facts {
            below_floor: true,
            has_posted_record: true,
            ..open_pr()
        };
        assert!(matches!(
            plan(&pending(ItemKind::Pr, &[Trigger::Pushed], 0), &low, &cfg()),
            Action::Ignore(_)
        ));
        let named_low = Facts { named: true, ..low };
        assert_eq!(
            Action::Recheck(7),
            plan(
                &pending(ItemKind::Pr, &[Trigger::Pushed], 0),
                &named_low,
                &cfg()
            )
        );
    }

    // -- settling --------------------------------------------------------------------

    #[test]
    fn an_item_is_due_once_quiet_for_the_settle_time_or_at_max_wait_anyway() {
        let c = cfg();
        let mut p = pending(ItemKind::Pr, &[Trigger::Pushed], 1000);
        assert!(!due(&p, 1000, &c, false));
        assert!(!due(&p, 1000 + c.settle - 1, &c, false));
        assert!(due(&p, 1000 + c.settle, &c, false));
        // Noise every minute keeps resetting the quiet period.
        p.last_seen = 1000 + c.max_wait - 30;
        assert!(!due(&p, 1000 + c.max_wait - 1, &c, false));
        assert!(due(&p, 1000 + c.max_wait, &c, false));
        assert!(due(&p, 1000, &c, true), "--once waits for nothing");
    }

    #[test]
    fn events_merge_into_one_pending_record_per_item_and_the_next_due_is_the_earliest() {
        let c = cfg();
        let mut pending = BTreeMap::new();
        let push = Event {
            number: 7,
            kind: ItemKind::Pr,
            trigger: Trigger::Pushed,
            who: None,
            head: Some(("aaa".into(), "bbb".into())),
            commits: None,
        };
        merge(&mut pending, &push, 1000);
        let reply = Event {
            trigger: Trigger::Replied,
            who: Some("alice".into()),
            head: Some(("bbb".into(), "ccc".into())),
            ..push.clone()
        };
        merge(&mut pending, &reply, 1060);
        let other = Event {
            number: 8,
            ..push.clone()
        };
        merge(&mut pending, &other, 1100);
        assert_eq!(2, pending.len());
        let p = &pending[&7];
        assert_eq!(1000, p.first_seen);
        assert_eq!(1060, p.last_seen);
        assert_eq!(Some("aaa".to_string()), p.head_from);
        assert_eq!(Some("ccc".to_string()), p.head_to);
        assert_eq!(vec!["alice".to_string()], p.who);
        assert_eq!("a push and a reply", p.because());
        assert_eq!(Some(1060 + c.settle), next_due(&pending, &c));
        assert_eq!(None, next_due(&BTreeMap::new(), &c));
    }

    // -- durations, state, backoff ------------------------------------------------------

    #[test]
    fn since_accepts_minutes_hours_days_and_combinations() {
        assert_eq!(Duration::from_secs(1800), parse_duration("30m").unwrap());
        assert_eq!(Duration::from_secs(7200), parse_duration("2h").unwrap());
        assert_eq!(Duration::from_secs(86_400), parse_duration("1d").unwrap());
        assert_eq!(
            Duration::from_secs(5400),
            parse_duration(" 1h30m ").unwrap()
        );
        assert_eq!(Duration::from_secs(45), parse_duration("45s").unwrap());
    }

    #[test]
    fn since_rejects_a_bare_number_and_says_what_it_takes() {
        for bad in ["", "30", "2 hours", "h", "1w", "1h30"] {
            let err = parse_duration(bad).unwrap_err().to_string();
            assert!(err.contains("like 30m, 2h or 1d"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn listen_state_round_trips_through_json_and_old_seen_entries_are_pruned() {
        let mut state = ListenState {
            version: STATE_VERSION,
            watermark: "2026-10-20T10:00:00Z".into(),
            last_modified: Some("Tue, 20 Oct 2026 10:00:00 GMT".into()),
            ..Default::default()
        };
        state
            .seen
            .insert("old".into(), "2026-10-01T10:00:00Z".into());
        state
            .seen
            .insert("new".into(), "2026-10-20T09:00:00Z".into());
        state.seen.insert("junk".into(), "yesterday".into());
        state.watched(7, ItemKind::Pr).head = Some("aaa".into());
        state
            .pending
            .insert(7, pending(ItemKind::Pr, &[Trigger::Pushed], 5));
        let text = serde_json::to_string_pretty(&state).unwrap();
        assert!(text.contains("\"pushed\""), "{text}");
        assert!(text.contains("\"kind\": \"pr\""), "{text}");
        let back: ListenState = serde_json::from_str(&text).unwrap();
        assert_eq!(state, back);

        let now = clock::parse_rfc3339("2026-10-20T10:00:00Z").unwrap();
        prune_seen(&mut state, now);
        let left: Vec<&String> = state.seen.keys().collect();
        assert_eq!(vec!["junk", "new"], left);
    }

    #[test]
    fn gh_failures_back_off_from_a_minute_to_fifteen_and_dispatches_from_fifteen_to_six_hours() {
        assert_eq!(60, backoff_secs(1));
        assert_eq!(120, backoff_secs(2));
        assert_eq!(480, backoff_secs(4));
        assert_eq!(900, backoff_secs(5));
        assert_eq!(900, backoff_secs(40));
        assert_eq!(900, dispatch_backoff_secs(1));
        assert_eq!(3600, dispatch_backoff_secs(3));
        assert_eq!(6 * 3600, dispatch_backoff_secs(9));
    }

    #[test]
    fn number_from_url_reads_the_last_segment() {
        assert_eq!(
            Some(108),
            number_from_url("https://api.github.com/repos/o/r/pulls/108")
        );
        assert_eq!(
            Some(42),
            number_from_url("https://api.github.com/repos/o/r/issues/42/")
        );
        assert_eq!(
            None,
            number_from_url("https://api.github.com/repos/o/r/pulls")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_live_listener_refuses_a_second_one_and_a_dead_pid_is_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "spar-listen-lock-{}-{}",
            std::process::id(),
            clock::now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("listen.lock");

        let held = Lock::at(path.clone(), 900, 1000).expect("first lock");
        let err = Lock::at(path.clone(), 900, 1000).unwrap_err().to_string();
        assert!(err.contains("already watching"), "{err}");
        assert!(err.contains(&std::process::id().to_string()), "{err}");
        drop(held);
        assert!(!path.exists(), "dropping the lock removes the file");

        // A lock left by a process that is gone is taken over.
        let child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        let mut child = child;
        child.wait().unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&LockFile {
                pid: dead,
                started: "x".into(),
                touched: 1000,
            })
            .unwrap(),
        )
        .unwrap();
        let taken = Lock::at(path.clone(), 900, 1000).expect("a dead pid is replaced");
        drop(taken);

        // So is one whose owner stopped touching it long ago, whoever it was.
        std::fs::write(
            &path,
            serde_json::to_string(&LockFile {
                pid: std::process::id(),
                started: "x".into(),
                touched: 1000,
            })
            .unwrap(),
        )
        .unwrap();
        let err = Lock::at(path.clone(), 900, 1500).unwrap_err().to_string();
        assert!(err.contains("already watching"), "{err}");
        let taken = Lock::at(path.clone(), 900, 5000).expect("a stale lock is replaced");
        drop(taken);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
