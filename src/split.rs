//! Breaking one issue or one pull request into smaller ones.
//!
//! The unit of work was fixed: one issue became one branch and one pull
//! request whatever its size, and the only thing that changed it was somebody
//! filing the smaller issues by hand. The review loop is the product and its
//! quality falls off with diff size, so the size of the unit is the biggest
//! lever there is on how well spar works, and it was the one a person could not
//! pull.
//!
//! `split` decomposes and stops. It never triages, never implements a child,
//! and never merges: it produces smaller units and hands them to the commands
//! that already exist. That keeps one invocation cheap and comprehensible, and
//! it keeps the blast radius of a wrong split to some issues and some branches
//! rather than to hours of implementation.
//!
//! Two agents, asymmetrically. One proposes the parts with the code open, the
//! other rules on the proposal: accept, reject, or accept with named parts
//! struck. Not two independent proposals, because two decompositions of one
//! thing cannot be reconciled mechanically and reconciling them is a third
//! judgement nobody asked for. Disagreement resolves toward not splitting, on
//! the same asymmetry `checkin` runs on: getting a decline wrong costs one
//! person one read of something that stays as it was, and getting the write
//! wrong costs them a queue to clean up.
//!
//! **spar never rewrites the branch behind somebody's pull request.** Splitting
//! a pull request is purely additive: new branches, new pull requests, one
//! comment. Nothing is force pushed, nothing closed, nothing rebased under
//! anybody. Removing half of a pull request in place is destroying work in
//! place, and two models agreeing does not make that reversible for the person
//! who wrote it. `additive` is that invariant as code.

use std::collections::BTreeSet;
use std::path::Path;

use crate::agent::{self, Agent};
use crate::config::Config;
use crate::error::Result;
use crate::model::{
    Implementation, Issue, IssueRun, ItemKind, PrRow, PrView, SplitCheck, SplitPart, SplitProposal,
    SplitScreen, SplitScreenDoc, Status,
};
use crate::repo::Repo;
use crate::style::{self, Style};
use crate::{bail, log, logdim, logwarn, schema, spar_err};

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

const SCREEN_PROMPT: &str = "\
Below are the open issues and pull requests on this repository. For each, say
whether it is worth splitting into smaller pieces.

Read the code in your working directory before judging. Do not modify anything.

Split is for something that is plainly several separate pieces of work, and
would be reviewed better as several: three unrelated fixes filed as one issue, a
pull request that grew a refactor while it was being corrected. Size alone is not
the test. One change across forty files is one piece of work; a mess across three
files can be three.

Say false when you are unsure. No is the common answer, and a split proposed on a
whim is a proposal somebody now has to read.

Items:
";

const PROPOSE_ISSUE_PROMPT: &str = "\
Issue #{number}: {title}
{url}

Somebody has read this and decided it is too big to work in one piece. Read the
code in your working directory before you decide anything. Do not modify,
commit, or push anything.

Say whether it really is several separate pieces of work, and if it is, what they
are. Each part becomes its own issue, implemented and reviewed on its own branch,
so a part has to be worth a pull request by itself: implementable without the
others and reviewable without them.

Being large is not the test. One change that touches forty files is one part.

Set should_split=false if this is one piece of work. That is a fine answer and it
is the common one. Set files to null: there is no diff here to partition.

The issue:
{body}";

const PROPOSE_PR_PROMPT: &str = "\
Pull request #{number} against `{base}`: {title}

Somebody has read this and decided it is too big to review in one piece. Your
checkout is the head of that pull request, detached and read only. Do not modify,
commit, or push anything.

Say whether the change is several separate pieces, and if it is, which files each
piece carries. Every part is then built on its own branch, carrying only its own
files, and has to build and pass there. A part that cannot do that is not a part:
fold its files into another part, or leave them out.

Use only paths from the list below, copied exactly. A path belongs to at most one
part, and leaving a path out is allowed: what no part carries is reported as left
over on the original pull request, which stays open.

Set should_split=false if this is one change, however large.

The {count} file(s) this pull request changes:
{files}";

const CHECK_PROMPT: &str = "\
Another agent read {what} and proposed splitting it into the parts below. You did
not make this call.

Go to the code and rule on it. Do not defer to them, and do not agree to be
agreeable. Getting a rejection wrong costs one person one read of something that
stays as it was. Getting an acceptance wrong costs them issues to close, a
checklist to strip out of somebody's body, and branches and pull requests to
delete.

Reject the proposal outright, or accept it with the parts that do not hold
struck. Striking so many that fewer than two remain means nothing is split, which
is the right answer when that is what you think.

Their reason: {reason}
They say the parts are {shape}.

The parts:
{parts}";

const STAND_ALONE_PROMPT: &str = "\
This branch holds one part of pull request #{parent}, split out of it. The other
parts are not here and are not coming.

Make this part stand on its own against `{base}`. Read what is here, add whatever
it needs to build and to pass its tests without the rest, and commit.

If it cannot stand on its own, set not_worth_doing=true and give the reason. Make
no further commits in that case, and the part is dropped rather than pushed
broken. The whole value of splitting is that each part can be reviewed and merged
independently, and a part that does not build has none of it.

Write summary, problem, changes and testing for this part alone. They become the
body of its own pull request, read by somebody who has not seen the parent.

Part {index} of {total}: {title}

{body}

The files it carries:
{files}";

// ---------------------------------------------------------------------------
// What says a split already happened
// ---------------------------------------------------------------------------

/// Written into a split parent's body and into the comment left on a split
/// pull request. An HTML comment, so GitHub renders it as nothing.
pub const SPLIT_MARKER: &str = "<!-- spar:split -->";

/// Whether this text already carries a checklist or a comment spar wrote.
///
/// The marker rather than a shape, for the reason `FOLLOWUP_MARKER` exists: a
/// checklist is exactly what somebody would write by hand, so its shape can
/// never establish who wrote it.
pub fn already_split(text: &str) -> bool {
    text.contains(SPLIT_MARKER)
}

/// A split parent's body: what it said, then the checklist of its parts.
///
/// The original text comes through untouched, byte for byte, and the checklist
/// is appended after it. This is the one place spar rewrites prose somebody
/// else wrote, and appending is the only shape of that which cannot lose any
/// of it.
///
/// Each line carries its `#N`, which is what makes the parent an ordinary
/// tracker: a checklist whose items are issue numbers.
pub fn tracker_body(original: &str, parts: &[(String, i64)]) -> String {
    let mut out = original.trim_end().to_string();
    if !out.is_empty() {
        if unclosed_fence(&out) {
            // A fence somebody left open swallows everything after it, so the
            // checklist would render as code rather than as a checklist.
            out.push_str("\n```");
        }
        out.push_str("\n\n");
    }
    out.push_str(SPLIT_MARKER);
    out.push_str("\n\n## Parts\n\nThis is now a tracker. Each part below is its own issue.\n\n");
    for (title, number) in parts {
        out.push_str(&format!("- [ ] #{number} {}\n", title.trim()));
    }
    out
}

/// Whether the text ends inside a fenced code block.
///
/// Tracked rather than counted, because a fence of one character inside a block
/// opened with the other is content and not a fence at all. Counting them would
/// close a block that was never open, which puts the checklist inside a fence
/// this wrote.
fn unclosed_fence(text: &str) -> bool {
    let mut open: Option<char> = None;
    for line in text.lines() {
        let start = line.trim_start();
        let Some(ch @ ('`' | '~')) = start.chars().next() else {
            continue;
        };
        if start.chars().take_while(|c| *c == ch).count() < 3 {
            continue;
        }
        match open {
            None => open = Some(ch),
            // A closing fence is the same character and carries no info string.
            Some(c) if c == ch && start.trim_end().chars().all(|c| c == ch) => open = None,
            Some(_) => {}
        }
    }
    open.is_some()
}

// ---------------------------------------------------------------------------
// The invariant
// ---------------------------------------------------------------------------

/// Refuse any branch a split did not itself create.
///
/// **spar never rewrites the branch behind somebody's pull request.** This is
/// the one write path a pull request split has, and it is worth an assertion
/// rather than a paragraph in a prompt: everything else here adds objects, and
/// a push to the wrong name is the single change that would destroy work in
/// place.
pub fn additive(branch: &str, parent_head: &str, prefix: &str) -> Result<()> {
    let wanted = format!("{prefix}split-");
    if !branch.starts_with(&wanted) {
        bail!("refusing to push to {branch}: a split only ever writes {wanted}* branches");
    }
    if branch == parent_head.trim() {
        bail!("refusing to push to {branch}: it is the branch behind the pull request being split");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deciding
// ---------------------------------------------------------------------------

/// Where a run of `spar split` stops.
#[derive(Debug, Clone, Copy)]
pub struct Mode {
    /// Print the proposal and write nothing.
    pub dry_run: bool,
    /// Split something that already carries a split spar made.
    pub again: bool,
}

/// What survived the proposal, the check, and the cap.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub parts: Vec<SplitPart>,
    /// Each part branched off the one before it, rather than off the base.
    pub stacked: bool,
    /// Why nothing is being split, when nothing is.
    pub declined: Option<String>,
    /// Parts that were proposed and are not being made, said out loud because a
    /// cap that fires silently is a cap nobody can raise.
    pub dropped: Vec<String>,
}

impl Decision {
    pub fn splits(&self) -> bool {
        self.declined.is_none() && self.parts.len() > 1
    }
}

/// Rule on a proposal, given the check and the cap.
///
/// Every path out of disagreement is "do not split". The proposing agent
/// declining, the checking agent rejecting, and the checking agent striking
/// enough parts that one is left all mean the same thing here, because a split
/// into one part is not a split.
pub fn decide(proposal: &SplitProposal, check: &SplitCheck, cap: usize) -> Decision {
    let mut out = Decision {
        // Stacked whenever either agent says so. The two shapes are not equally
        // safe to be wrong about: parts built independently out of a change that
        // is really sequential do not build, and a part that does not build is
        // dropped. A stacked part always applies on its predecessor.
        stacked: proposal.stacked || check.stacked,
        ..Decision::default()
    };

    if !proposal.should_split {
        out.declined = Some(reason_or(&proposal.reason, "it is one piece of work"));
        return out;
    }
    if !check.accept {
        out.declined = Some(reason_or(
            &check.reasoning,
            "the second agent did not accept the split",
        ));
        return out;
    }

    let struck: BTreeSet<i64> = check.strike.iter().copied().collect();
    for (i, part) in proposal.parts.iter().enumerate() {
        let number = i as i64 + 1;
        if struck.contains(&number) {
            out.dropped
                .push(format!("{} (struck by the second agent)", label(part)));
            continue;
        }
        if part.title.trim().is_empty() {
            out.dropped.push("a part with no title".to_string());
            continue;
        }
        out.parts.push(part.clone());
    }

    if out.parts.len() < 2 {
        out.declined = Some(reason_or(
            &check.reasoning,
            "fewer than two parts survived, and a split into one part is not a split",
        ));
        out.parts.clear();
        return out;
    }

    if out.parts.len() > cap {
        for part in out.parts.split_off(cap) {
            out.dropped
                .push(format!("{} (over the max_split_parts cap)", label(&part)));
        }
    }
    out
}

fn label(part: &SplitPart) -> String {
    style::clip(part.title.trim(), 80)
}

fn reason_or(text: &str, fallback: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The files a change touches that none of `carried` holds.
///
/// Reported on the original, which stays open, so that nothing goes missing
/// without somebody being told where it went. Takes the file lists rather than
/// the parts, because what a proposal claims and what was actually built are
/// different answers and the record has to be the second one.
pub fn leftover<'a>(
    changed: &[String],
    carried: impl IntoIterator<Item = &'a [String]>,
) -> Vec<String> {
    let taken: BTreeSet<&str> = carried.into_iter().flatten().map(String::as_str).collect();
    changed
        .iter()
        .filter(|path| !taken.contains(path.as_str()))
        .cloned()
        .collect()
}

/// Hold every part to the paths the change actually touches.
///
/// A path the model invented carries nothing and would be checked out of the
/// pull request head as a failure, and a path with `..` in it is one that would
/// be written outside the worktree. The list came from spar, so anything not in
/// it is a paraphrase.
fn confine(parts: &mut [SplitPart], changed: &[String]) -> Vec<String> {
    let known: BTreeSet<&str> = changed.iter().map(String::as_str).collect();
    let mut unknown = Vec::new();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    for part in parts.iter_mut() {
        part.files.retain(|path| {
            if !known.contains(path.as_str()) {
                unknown.push(path.clone());
                return false;
            }
            // A path in two parts would be carried twice and reviewed twice.
            claimed.insert(path.clone())
        });
    }
    unknown
}

// ---------------------------------------------------------------------------
// Screening a whole queue
// ---------------------------------------------------------------------------

/// One open item the bare `spar split` is considering.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub number: i64,
    pub kind: ItemKind,
    pub title: String,
    /// The issue body, or the pull request's size.
    pub detail: String,
}

impl Candidate {
    pub fn from_issue(issue: &Issue) -> Self {
        Self {
            number: issue.number,
            kind: ItemKind::Issue,
            title: issue.title.clone(),
            detail: issue.body_text().trim().to_string(),
        }
    }

    pub fn from_pr(row: &PrRow) -> Self {
        Self {
            number: row.number,
            kind: ItemKind::Pr,
            title: row.title.clone(),
            detail: row.size(),
        }
    }
}

/// The queue as one prompt carries it, and what did not fit.
fn render(items: &[Candidate], cfg: &Config) -> (String, usize) {
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut deferred = 0usize;
    for item in items {
        if deferred > 0 {
            deferred += 1;
            continue;
        }
        let block = format!(
            "{} #{}: {}\n{}",
            item.kind, item.number, item.title, item.detail
        );
        let len = block.chars().count();
        // The first item goes in whatever its size, for the reason
        // `followups::render` does it: a queue of one that does not fit is a
        // command that does nothing, forever.
        if !parts.is_empty() && total + len > cfg.loop_cfg.max_triage_chars {
            deferred += 1;
            continue;
        }
        total += len;
        parts.push(block);
    }
    (parts.join("\n\n"), deferred)
}

/// One agent's verdict on the whole queue in one call.
///
/// The asymmetry here is the opposite of the follow-up screen's. There the
/// screen says keep when unsure because what survives still faces two agent
/// triage. Here what survives faces propose and check, which is also two
/// agents, so a permissive screen costs money rather than damage. The prompt
/// still says not to split when unsure: no is the common answer, and a split
/// proposed on a whim is a proposal somebody now has to read.
pub fn screen(
    agent: &Agent,
    cfg: &Config,
    repo: &Repo,
    items: &[Candidate],
) -> Result<Vec<SplitScreen>> {
    let (text, deferred) = render(items, cfg);
    if deferred > 0 {
        logwarn!(
            "{deferred} item(s) did not fit in one screening prompt and were left for a later run"
        );
    }
    let answer: SplitScreenDoc = agent.ask_json(
        &format!("{SCREEN_PROMPT}{text}"),
        &schema::split_screen(),
        repo.root(),
        cfg.effort_for_round(&agent.spec, 1).as_deref(),
    )?;
    Ok(answer.items)
}

// ---------------------------------------------------------------------------
// Splitting an issue
// ---------------------------------------------------------------------------

pub fn split_issue(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    number: i64,
    mode: &Mode,
) -> IssueRun {
    match issue_inner(agents, cfg, repo, number, mode) {
        Ok(state) => state,
        Err(e) => failed(number, format!("#{number}"), e),
    }
}

fn failed(number: i64, label: String, e: crate::error::SparError) -> IssueRun {
    log!("{label} split failed: {e}");
    let mut state = IssueRun::new(number, label);
    state.status = Status::Error;
    state.notes.push(e.to_string());
    state
}

fn issue_inner(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    number: i64,
    mode: &Mode,
) -> Result<IssueRun> {
    let issue = repo
        .fetch_issues(&[number])?
        .into_iter()
        .next()
        .ok_or_else(|| spar_err!("#{number} is closed"))?;
    let mut state = IssueRun::new(number, issue.title.clone());

    if already_split(issue.body_text()) && !mode.again {
        log!("#{number} already carries a checklist spar wrote. --again splits it anyway.");
        state.status = Status::Whole;
        state.notes.push("already split".into());
        return Ok(state);
    }

    let (body, shortened) = issue.body_for_prompt(cfg.loop_cfg.max_issue_chars);
    if shortened {
        logwarn!(
            "#{number}: the issue body was shortened to fit the prompt. Raise max_issue_chars if \
             the rest matters."
        );
    }
    let prompt = PROPOSE_ISSUE_PROMPT
        .replace("{number}", &number.to_string())
        .replace("{title}", &issue.title)
        .replace("{url}", &issue.url)
        .replace("{body}", &body);

    let decision = propose_and_check(
        agents,
        cfg,
        repo.root(),
        &format!("#{number}"),
        &prompt,
        &format!("issue #{number}"),
    )?;

    for note in &decision.dropped {
        log!("  dropped {note}");
        state.notes.push(format!("dropped {note}"));
    }
    if !decision.splits() {
        let why = decision
            .declined
            .unwrap_or_else(|| "nothing survived the check".to_string());
        log!("#{number} left whole: {why}");
        state.status = Status::Whole;
        state.notes.push(why);
        return Ok(state);
    }

    if mode.dry_run {
        print_proposal(number, "issue", &decision, &[]);
        state.status = Status::Whole;
        state.notes.push(format!(
            "dry run: {} part(s) proposed",
            decision.parts.len()
        ));
        return Ok(state);
    }

    // Filed first, the parent rewritten after. An issue filed with no checklist
    // yet is findable and says where it came from; a checklist pointing at
    // issues that were never filed is not recoverable by reading it.
    let mut listed: Vec<(String, i64)> = Vec::new();
    for (i, part) in decision.parts.iter().enumerate() {
        // Cleaned here rather than only inside `file_as_issue`, because the
        // same string also goes into the parent's checklist, and that write
        // deliberately does not run the body through the style gate.
        let title = match repo.clean_title(&part.title) {
            Ok(title) if !title.trim().is_empty() => title,
            _ => {
                logwarn!("nothing left of '{}' after cleaning it", label(part));
                state.notes.push(format!(
                    "dropped {}: its title would not clean",
                    label(part)
                ));
                continue;
            }
        };
        let body = format!(
            "{}\n\nPart {} of {}, split out of #{number}.",
            part.body.trim(),
            i + 1,
            decision.parts.len()
        );
        match crate::review::file_as_issue(repo, &title, &body) {
            Ok(filed) => {
                log!("  {}", filed.describe(&title));
                if let Some(url) = filed.url() {
                    state.filed.push(url.to_string());
                }
                // `file_as_issue` answers with an existing issue when the part
                // is already filed, and what a part resembles most is the issue
                // it came out of. A parent listed as its own child is a tracker
                // pointing at itself, and two parts that resolved to one issue
                // are one part twice.
                match filed.number() {
                    Some(n) if n == number => {
                        logwarn!("'{title}' came back as #{number} itself, so it is not a part");
                        state
                            .notes
                            .push(format!("dropped {title}: it matched #{number} itself"));
                    }
                    Some(n) if listed.iter().any(|(_, listed)| *listed == n) => {
                        logwarn!("'{title}' came back as #{n}, which another part already is");
                        state
                            .notes
                            .push(format!("dropped {title}: #{n} is already a part"));
                    }
                    Some(n) => listed.push((title, n)),
                    None => state.notes.push(format!("{title}: {}", filed.note())),
                }
            }
            Err(e) => {
                logwarn!("could not file '{title}': {e}");
                state.notes.push(format!("could not file {title}: {e}"));
            }
        }
    }

    if listed.len() < 2 {
        // Nothing was decomposed in the end, so the parent is not a tracker and
        // must not be rewritten into one.
        log!("#{number} left whole: fewer than two parts were filed");
        state.status = Status::Whole;
        return Ok(state);
    }

    let wanted = tracker_body(issue.body_text(), &listed);
    match repo.edit_issue_body(number, issue.body_text(), &wanted) {
        Ok(()) => {
            log!("#{number} is now a tracker for {} part(s)", listed.len());
            state.status = Status::Split;
        }
        Err(e) => {
            // The parts exist and the parent does not point at them, so `run`
            // would work the parent as a whole again. That needs somebody, so
            // it is an error rather than a note on a success.
            state.status = Status::Error;
            state.notes.push(format!(
                "{e}\nThe parts were filed. Add them to #{number} by hand:\n{}",
                checklist(&listed)
            ));
        }
    }
    Ok(state)
}

fn checklist(parts: &[(String, i64)]) -> String {
    parts
        .iter()
        .map(|(title, n)| format!("- [ ] #{n} {}", title.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Splitting a pull request
// ---------------------------------------------------------------------------

pub fn split_pr(agents: &[Agent], cfg: &Config, repo: &Repo, number: i64, mode: &Mode) -> IssueRun {
    match pr_inner(agents, cfg, repo, number, mode) {
        Ok(state) => state,
        Err(e) => failed(number, format!("PR #{number}"), e),
    }
}

fn pr_inner(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    number: i64,
    mode: &Mode,
) -> Result<IssueRun> {
    let pr: PrView = repo.pr_view(number)?;
    if !pr.is_open() {
        bail!("PR #{number} is {}", pr.state.to_lowercase());
    }
    let mut state = IssueRun::new(number, pr.title.clone());
    state.pr = Some(pr.url.clone());

    if !mode.again && has_parts(repo, number)? {
        log!("PR #{number} already has parts spar made. --again splits it again.");
        state.status = Status::Whole;
        state.notes.push("already split".into());
        return Ok(state);
    }

    let base = if pr.base_ref_name.trim().is_empty() {
        cfg.base_branch().to_string()
    } else {
        pr.base_ref_name.clone()
    };
    repo.git_try(&["fetch", "origin", &base]);

    let head_ref = crate::repo::review_ref(number);
    let read_only = repo.worktree_for_pr_head(number)?;
    let outcome = split_pr_inner(
        agents, cfg, repo, &pr, &base, &head_ref, &read_only, mode, &mut state,
    );
    if !cfg.loop_cfg.keep_worktrees {
        repo.release_review_worktree(number);
    }
    outcome?;
    Ok(state)
}

#[allow(clippy::too_many_arguments)]
fn split_pr_inner(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr: &PrView,
    base: &str,
    head_ref: &str,
    read_only: &Path,
    mode: &Mode,
    state: &mut IssueRun,
) -> Result<()> {
    let number = pr.number;
    let changed = repo.changed_files(read_only, base);
    if changed.is_empty() {
        log!("PR #{number} changes no files, so there is nothing to split");
        state.status = Status::Whole;
        return Ok(());
    }

    let prompt = PROPOSE_PR_PROMPT
        .replace("{number}", &number.to_string())
        .replace("{title}", &pr.title)
        .replace("{base}", base)
        .replace("{count}", &changed.len().to_string())
        .replace("{files}", &listed(&changed));

    let mut decision = propose_and_check(
        agents,
        cfg,
        read_only,
        &format!("PR #{number}"),
        &prompt,
        &format!("pull request #{number}"),
    )?;
    for path in confine(&mut decision.parts, &changed) {
        logdim!("PR #{number}: a part named `{path}`, which this change does not touch");
    }

    for note in &decision.dropped {
        log!("  dropped {note}");
        state.notes.push(format!("dropped {note}"));
    }
    if !decision.splits() {
        let why = decision
            .declined
            .clone()
            .unwrap_or_else(|| "nothing survived the check".to_string());
        log!("PR #{number} left whole: {why}");
        state.status = Status::Whole;
        state.notes.push(why);
        return Ok(());
    }

    let proposed_left = proposed_leftover(&changed, &decision);
    if mode.dry_run {
        print_proposal(number, "pull request", &decision, &proposed_left);
        state.status = Status::Whole;
        state.notes.push(format!(
            "dry run: {} part(s) proposed",
            decision.parts.len()
        ));
        return Ok(());
    }

    // A fork is proposed, not split. spar cannot push to the fork, and carving
    // somebody's contribution into pull requests of your own without asking is
    // not something to do automatically. This is how `review` already treats a
    // fork.
    if pr.is_cross_repository {
        log!("PR #{number} comes from a fork, so the parts are proposed rather than made");
        let body = proposal_comment(number, &decision, &proposed_left, &repo.style);
        repo.comment_pr(number, &body)?;
        state.status = Status::Whole;
        state
            .notes
            .push("from a fork, so the split was proposed rather than made".into());
        return Ok(());
    }

    let parent = Parent {
        number,
        base,
        head_ref,
        head_branch: pr.head_ref_name.trim(),
    };
    let made = build_parts(agents, cfg, repo, &parent, &decision, state)?;
    if made.is_empty() {
        log!("PR #{number} left whole: no part would stand on its own");
        state.status = Status::Whole;
        return Ok(());
    }

    // From the parts that exist, not the ones that were proposed. A part
    // dropped for carrying nothing, for failing, or for not standing on its own
    // leaves its files on the original, and this comment is the only permanent
    // record of where anything went.
    let left = leftover(&changed, made.iter().map(|m| m.files.as_slice()));
    let body = parts_comment(&made, &left, &repo.style);
    if let Err(e) = repo.comment_pr(number, &body) {
        logwarn!(
            "made {} part(s) but could not say so on #{number}: {e}",
            made.len()
        );
        state
            .notes
            .push(format!("could not comment on #{number}: {e}"));
    }
    // A split into one part is not a split: everything that part carries is
    // still on the original, so the two would be reviewed twice over. It was
    // pushed before that was knowable and is named on the original for that
    // reason, but this run did not decompose anything.
    if made.len() < 2 {
        log!("PR #{number} left whole: only one part stood on its own");
        state.status = Status::Whole;
        state
            .notes
            .push("only one part stood on its own, so nothing was decomposed".into());
        return Ok(());
    }
    state.status = Status::Split;
    Ok(())
}

/// What no proposed part claims. For the two paths that build nothing: a dry
/// run and a fork, where the proposal is all there is.
fn proposed_leftover(changed: &[String], decision: &Decision) -> Vec<String> {
    leftover(changed, decision.parts.iter().map(|p| p.files.as_slice()))
}

/// One part that exists: its number, its title, its pull request, and the files
/// it took with it.
struct Made {
    index: usize,
    title: String,
    url: String,
    files: Vec<String>,
}

/// The pull request being split, as the part builder needs it.
struct Parent<'a> {
    number: i64,
    /// What an independent part is branched from and opened against.
    base: &'a str,
    /// The fetched head, which the slices are taken out of.
    head_ref: &'a str,
    /// The branch behind it, which nothing here may ever write to.
    head_branch: &'a str,
}

fn build_parts(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    parent: &Parent<'_>,
    decision: &Decision,
    state: &mut IssueRun,
) -> Result<Vec<Made>> {
    let implementor = agent::find(agents, &cfg.first_implementor)?;
    let total = decision.parts.len();
    let mut made: Vec<Made> = Vec::new();

    // What the next part is branched from, and what its pull request is opened
    // against. Independent parts both stay on the base; stacked parts move to
    // each part that was actually made, so a dropped part does not strand the
    // ones after it.
    let mut start = format!("origin/{}", parent.base);
    let mut against = parent.base.to_string();

    for (i, part) in decision.parts.iter().enumerate() {
        let index = i + 1;
        if part.files.is_empty() {
            log!("  part {index} carries no files, dropping it");
            state
                .notes
                .push(format!("dropped {}: it carried no files", label(part)));
            continue;
        }

        let (dir, branch) = repo.worktree_for_split(parent.number, index, &start)?;
        let outcome = build_one(
            repo,
            implementor,
            cfg,
            parent,
            index,
            total,
            part,
            &dir,
            &branch,
            &against,
        );
        match outcome {
            Ok(Some(pr)) => {
                log!("  part {index}: {}", pr.url);
                state.filed.push(pr.url.clone());
                made.push(Made {
                    index,
                    title: part.title.clone(),
                    url: pr.url,
                    files: part.files.clone(),
                });
                if decision.stacked {
                    start = branch.clone();
                    against = branch;
                }
            }
            Ok(None) => {
                repo.release_split_worktree(&dir, &branch);
            }
            Err(e) => {
                logwarn!("  part {index} could not be made: {e}");
                state.notes.push(format!("dropped {}: {e}", label(part)));
                repo.release_split_worktree(&dir, &branch);
            }
        }
    }
    Ok(made)
}

/// Build one part on its own branch, or say it will not stand.
///
/// Returns None for a part the agent declined, which is dropped rather than
/// pushed: a part that does not build has none of the value of splitting.
#[allow(clippy::too_many_arguments)]
fn build_one(
    repo: &Repo,
    implementor: &Agent,
    cfg: &Config,
    parent: &Parent<'_>,
    index: usize,
    total: usize,
    part: &SplitPart,
    dir: &Path,
    branch: &str,
    against: &str,
) -> Result<Option<crate::model::PrRef>> {
    let number = parent.number;
    log!(
        "PR #{number}: building part {index} of {total} on {branch} ({} file(s))",
        part.files.len()
    );
    if !apply_slice(repo, dir, parent.base, parent.head_ref, &part.files)? {
        bail!("applying its files changed nothing");
    }
    let subject = format!("{} (part {index} of #{number})", part.title.trim());
    repo.git_at(Some(dir), &["commit", "-m", &subject])
        .map_err(|e| spar_err!("could not commit the slice: {}", e.last_line()))?;

    let prompt = STAND_ALONE_PROMPT
        .replace("{parent}", &number.to_string())
        .replace("{base}", against)
        .replace("{index}", &index.to_string())
        .replace("{total}", &total.to_string())
        .replace("{title}", part.title.trim())
        .replace("{body}", part.body.trim())
        .replace("{files}", &listed(&part.files));
    let work: Implementation = implementor.ask_json(
        &prompt,
        &schema::implementation(),
        dir,
        cfg.effort_for_round(&implementor.spec, 1).as_deref(),
    )?;
    if work.not_worth_doing {
        let reason = style::sentence(&work.reason, &repo.style);
        log!(
            "  part {index} will not stand on its own, dropping it: {}",
            if reason.is_empty() {
                "no reason given"
            } else {
                &reason
            }
        );
        return Ok(None);
    }

    // The slice is already committed, so a push would succeed with whatever the
    // stand-alone pass left in the tree missing from it. That is the one shape
    // of failure this cannot see afterwards: the part arrives without exactly
    // the fixes that were supposed to make it stand on its own.
    if crate::review::snapshot(repo, dir).dirty {
        bail!("it left its stand-alone fixes uncommitted");
    }

    // The invariant, asserted at the one place a split writes a branch.
    additive(branch, parent.head_branch, &repo.branch_prefix)?;
    repo.rewrite_commits_if_needed(dir, against)?;
    repo.push(dir, branch)?;

    let title = format!("{} (part {index} of #{number})", part.title.trim());
    let body = part_body(number, index, total, part, &work, &repo.style);
    repo.create_pr(dir, branch, against, &title, &body)
        .map(Some)
}

/// Put one slice of the parent's change on this branch.
///
/// The parent's own diff for those paths, applied, rather than its versions of
/// those files copied over. The branch starts from the base as it is now, and
/// copying would revert whatever the base did to the same file after the pull
/// request was opened, which is a deletion nobody asked for in a change
/// advertised as one part of somebody else's.
///
/// Reports whether anything is actually staged, because a slice that changed
/// nothing is a part with no content.
///
/// Public so that it can be tested against a real repository, which is where it
/// goes wrong.
pub fn apply_slice(
    repo: &Repo,
    dir: &Path,
    base: &str,
    head_ref: &str,
    files: &[String],
) -> Result<bool> {
    let from = repo.merge_base(dir, base, head_ref)?;
    let patch = patch_path(dir);
    let written = write_patch(repo, dir, &from, head_ref, files, &patch);
    let outcome = written.and_then(|carries| {
        if !carries {
            return Ok(false);
        }
        // Three way so the slice still lands when the base has moved under the
        // hunks. A conflict is an error rather than markers left in a commit:
        // the part is dropped and said out loud, which is what every other way
        // a part fails to stand on its own already does.
        repo.git_at(
            Some(dir),
            &["apply", "--index", "--3way", &patch.display().to_string()],
        )
        .map_err(|e| spar_err!("could not apply its files onto {base}: {}", e.last_line()))?;
        Ok(!repo
            .git_try_at(Some(dir), &["status", "--porcelain"])
            .trim()
            .is_empty())
    });
    let _ = std::fs::remove_file(&patch);
    outcome
}

/// The parent's diff for these paths alone, written to `patch`. False when it
/// is empty.
fn write_patch(
    repo: &Repo,
    dir: &Path,
    from: &str,
    head_ref: &str,
    files: &[String],
    patch: &Path,
) -> Result<bool> {
    let mut args: Vec<String> = vec![
        "diff".into(),
        // `--binary` so a change to an image or a fixture survives the round
        // trip, `--no-renames` to match `changed_files`: a part carries paths,
        // and a rename it holds only one end of has to stay a deletion and an
        // addition rather than become a patch git cannot apply.
        "--binary".into(),
        "--no-renames".into(),
        // Straight to a file rather than through stdout, since a diff of a
        // file that is not valid UTF-8 does not survive being read as a string.
        format!("--output={}", patch.display()),
        from.to_string(),
        head_ref.to_string(),
        "--".into(),
    ];
    // `:(literal)` because `--` ends the options and does nothing to pathspec
    // matching: a file actually named `*.txt` would otherwise pull in every
    // other `.txt` the change touches, including ones another part carries.
    args.extend(files.iter().map(|path| format!(":(literal){path}")));

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.git_at(Some(dir), &argv).map_err(|e| {
        spar_err!(
            "could not read its files out of the head: {}",
            e.last_line()
        )
    })?;
    Ok(std::fs::metadata(patch)
        .map(|m| m.len() > 0)
        .unwrap_or(false))
}

/// Beside the worktree rather than in it, so the patch is never something the
/// slice could commit and never something `--output` writes into the tree it
/// describes. The worktree directory is already spar's and already excluded
/// from git.
fn patch_path(dir: &Path) -> std::path::PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    let holder = dir.parent().unwrap_or_else(|| Path::new("."));
    holder.join(format!("{name}.patch"))
}

/// Whether spar has already split this pull request.
///
/// An error rather than false when the comments cannot be read. This is the
/// only thing standing between a rerun and a second set of branches and pull
/// requests, and a failed read is not evidence that the first set is not there.
fn has_parts(repo: &Repo, number: i64) -> Result<bool> {
    let comments = repo.try_issue_comments(number).map_err(|e| {
        spar_err!(
            "could not read the comments on #{number}, so whether it has already been split is \
             unknown: {}",
            e.last_line()
        )
    })?;
    Ok(comments.iter().any(|c| {
        c.get("body")
            .and_then(serde_json::Value::as_str)
            .is_some_and(already_split)
    }))
}

// ---------------------------------------------------------------------------
// Asking the pair
// ---------------------------------------------------------------------------

/// One agent proposes with the code open, the other rules on the proposal.
fn propose_and_check(
    agents: &[Agent],
    cfg: &Config,
    work_dir: &Path,
    label: &str,
    propose_prompt: &str,
    what: &str,
) -> Result<Decision> {
    let proposer_name = cfg.first_implementor.clone();
    let proposer = agent::find(agents, &proposer_name)?;
    let checker_name = cfg.other(&proposer_name);
    let checker = agent::find(agents, &checker_name)?;

    log!("{label}: {proposer_name} proposing a split");
    let proposal: SplitProposal = proposer.ask_json(
        propose_prompt,
        &schema::split_proposal(),
        work_dir,
        cfg.effort_for_round(&proposer.spec, 1).as_deref(),
    )?;
    if !proposal.should_split {
        return Ok(decide(
            &proposal,
            &SplitCheck::default(),
            cfg.loop_cfg.max_split_parts,
        ));
    }

    log!(
        "{label}: {checker_name} checking {} proposed part(s)",
        proposal.parts.len()
    );
    let check: SplitCheck = checker.ask_json(
        &CHECK_PROMPT
            .replace("{what}", what)
            .replace("{reason}", proposal.reason.trim())
            .replace(
                "{shape}",
                if proposal.stacked {
                    "stacked, each needing the one before it"
                } else {
                    "independent of each other"
                },
            )
            .replace("{parts}", &render_parts(&proposal.parts)),
        &schema::split_check(),
        work_dir,
        cfg.effort_for_round(&checker.spec, 2).as_deref(),
    )?;

    Ok(decide(&proposal, &check, cfg.loop_cfg.max_split_parts))
}

fn render_parts(parts: &[SplitPart]) -> String {
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let files = if p.files.is_empty() {
                String::new()
            } else {
                format!("\n   files: {}", p.files.join(", "))
            };
            format!("{}. {}\n   {}{files}", i + 1, p.title.trim(), p.body.trim())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// What a person reads
// ---------------------------------------------------------------------------

fn listed(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn bullets(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of one part's pull request.
///
/// It says which pull request it came out of, because that is the parent record:
/// parts get no issues of their own, and the comment on the original is the
/// list of them.
fn part_body(
    parent: i64,
    index: usize,
    total: usize,
    part: &SplitPart,
    work: &Implementation,
    style: &Style,
) -> String {
    let mut out = vec![format!("Part {index} of {total}, split out of #{parent}.")];
    for lead in [&work.summary, &work.problem] {
        let text = style::sentence(lead, style);
        if !text.is_empty() {
            out.push(text);
        }
    }
    if out.len() == 1 {
        // Nothing was said about the change, so say what the part is for.
        let text = style::sentence(&part.body, style);
        if !text.is_empty() {
            out.push(text);
        }
    }
    for (heading, lines) in [
        ("What changed", &work.changes),
        ("How to test", &work.testing),
    ] {
        let items: Vec<String> = lines
            .iter()
            .map(|line| style::summary(line, style))
            .filter(|line| !line.is_empty())
            .collect();
        if !items.is_empty() {
            out.push(format!("## {heading}\n\n{}", bullets(&items)));
        }
    }
    let notes = style::sentence(work.notes.as_deref().unwrap_or_default(), style);
    if !notes.is_empty() {
        out.push(format!("## Notes\n\n{notes}"));
    }
    style::body(&out.join("\n\n"), style)
}

/// The comment left on a pull request that has been split.
///
/// It says what was made and what was left over, and it says that the pull
/// request itself was not touched, because that is the thing somebody opening
/// this wants to know first.
///
/// One part is a separate sentence rather than a count of one. Every other part
/// was dropped, so nothing was decomposed and the one that exists duplicates a
/// piece of this pull request. Somebody has to be told that plainly, because
/// the only thing to do with it is close it or close this.
fn parts_comment(made: &[Made], left: &[String], style: &Style) -> String {
    let listed: Vec<String> = made
        .iter()
        .map(|m| format!("part {}: {} {}", m.index, m.url, m.title.trim()))
        .collect();
    let lead = if made.len() < 2 {
        "Only one part of this stood on its own, so it has not been split. That part was already \
         opened as its own pull request, and it carries files this one still carries:"
            .to_string()
    } else {
        format!("Split into {} pull request(s):", made.len())
    };
    let mut out = vec![SPLIT_MARKER.to_string(), lead, bullets(&listed)];
    if !left.is_empty() {
        out.push(format!(
            "{} file(s) are in no part, and are still only here:\n{}",
            left.len(),
            bullets(left)
        ));
    }
    out.push(
        "This pull request has not been changed. Its branch, its commits, and its own review are \
         untouched, and it is still open."
            .to_string(),
    );
    style::body(&out.join("\n\n"), style)
}

/// The comment left on a fork's pull request, which is proposed and not split.
fn proposal_comment(number: i64, decision: &Decision, left: &[String], style: &Style) -> String {
    let listed: Vec<String> = decision
        .parts
        .iter()
        .map(|p| {
            if p.files.is_empty() {
                p.title.trim().to_string()
            } else {
                format!("{} ({})", p.title.trim(), p.files.join(", "))
            }
        })
        .collect();
    let mut out = vec![
        SPLIT_MARKER.to_string(),
        format!(
            "#{number} comes from a fork, so this is a proposal rather than a change. Two agents \
             read it and agreed it would review better as {} pieces:",
            decision.parts.len()
        ),
        bullets(&listed),
    ];
    if decision.stacked {
        out.push("They have to land in that order.".to_string());
    }
    if !left.is_empty() {
        out.push(format!(
            "{} file(s) are in none of them:\n{}",
            left.len(),
            bullets(left)
        ));
    }
    out.push("Nothing has been changed here.".to_string());
    style::body(&out.join("\n\n"), style)
}

/// What `--dry-run` prints. `println!`, not `log!`: it is the whole output of
/// the command in that mode.
fn print_proposal(number: i64, kind: &str, decision: &Decision, left: &[String]) {
    println!(
        "\n{kind} #{number} would be split into {} part(s):",
        decision.parts.len()
    );
    for (i, part) in decision.parts.iter().enumerate() {
        println!("  {}. {}", i + 1, style::clip(part.title.trim(), 90));
        if !part.files.is_empty() {
            println!("     {}", part.files.join(", "));
        }
    }
    if decision.stacked {
        println!("  each part is based on the one before it");
    }
    if !left.is_empty() {
        println!("  left over: {}", left.join(", "));
    }
    for note in &decision.dropped {
        println!("  dropped {note}");
    }
    println!("Nothing was written.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(title: &str, files: &[&str]) -> SplitPart {
        SplitPart {
            title: title.into(),
            body: format!("what {title} is"),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn proposal(parts: Vec<SplitPart>) -> SplitProposal {
        SplitProposal {
            should_split: true,
            reason: "three things".into(),
            stacked: false,
            parts,
        }
    }

    fn accept() -> SplitCheck {
        SplitCheck {
            accept: true,
            stacked: false,
            strike: vec![],
            reasoning: "read it".into(),
        }
    }

    /// The multiplication guard. One command that turns twenty open items into
    /// sixty is a queue nobody asked for, and this codebase has been burned by
    /// exactly that once already.
    #[test]
    fn the_cap_holds_and_says_what_it_held_back() {
        let parts = vec![
            part("one", &[]),
            part("two", &[]),
            part("three", &[]),
            part("four", &[]),
        ];
        let out = decide(&proposal(parts), &accept(), 2);
        assert_eq!(2, out.parts.len());
        assert!(out.splits());
        assert_eq!(2, out.dropped.len(), "{:?}", out.dropped);
        assert!(out.dropped[0].contains("three"), "{:?}", out.dropped);
        assert!(out.dropped[0].contains("cap"), "{:?}", out.dropped);
    }

    /// The proposing agent declining is the cheapest way out, and it must not
    /// cost a second call or produce parts.
    #[test]
    fn a_proposal_that_declines_splits_nothing() {
        let mut p = proposal(vec![part("one", &[]), part("two", &[])]);
        p.should_split = false;
        p.reason = "it is one change".into();
        let out = decide(&p, &accept(), 4);
        assert!(!out.splits());
        assert_eq!(Some("it is one change".to_string()), out.declined);
        assert!(out.parts.is_empty());
    }

    /// Disagreement resolves toward not splitting.
    #[test]
    fn a_rejected_proposal_splits_nothing() {
        let check = SplitCheck {
            accept: false,
            reasoning: "these are the same change".into(),
            ..accept()
        };
        let out = decide(
            &proposal(vec![part("one", &[]), part("two", &[])]),
            &check,
            4,
        );
        assert!(!out.splits());
        assert!(out.declined.unwrap().contains("same change"));
    }

    #[test]
    fn a_struck_part_is_not_made() {
        let parts = vec![part("one", &[]), part("two", &[]), part("three", &[])];
        let check = SplitCheck {
            strike: vec![2],
            ..accept()
        };
        let out = decide(&proposal(parts), &check, 4);
        assert_eq!(2, out.parts.len());
        assert_eq!(vec!["one", "three"], titles(&out));
        assert!(out.dropped[0].contains("two"), "{:?}", out.dropped);
    }

    /// A split into one part is not a split, so striking enough parts is a way
    /// of saying no.
    #[test]
    fn striking_all_but_one_part_leaves_it_whole() {
        let parts = vec![part("one", &[]), part("two", &[])];
        let check = SplitCheck {
            strike: vec![1],
            reasoning: "only one of these stands alone".into(),
            ..accept()
        };
        let out = decide(&proposal(parts), &check, 4);
        assert!(!out.splits());
        assert!(out.parts.is_empty());
        assert!(out.declined.unwrap().contains("stands alone"));
    }

    /// Whether the parts are stacked is a property of the change, and being
    /// wrong about it is not symmetric: parts built independently out of a
    /// sequential change do not build, and a part that does not build is
    /// dropped.
    #[test]
    fn either_agent_calling_it_stacked_makes_it_stacked() {
        let mut p = proposal(vec![part("one", &[]), part("two", &[])]);
        assert!(!decide(&p, &accept(), 4).stacked);

        p.stacked = true;
        assert!(decide(&p, &accept(), 4).stacked);

        p.stacked = false;
        let check = SplitCheck {
            stacked: true,
            ..accept()
        };
        assert!(decide(&p, &check, 4).stacked);
    }

    fn titles(d: &Decision) -> Vec<String> {
        d.parts.iter().map(|p| p.title.clone()).collect()
    }

    /// The whole text somebody wrote comes through, and the checklist is added
    /// after it. This is the one place spar rewrites prose it did not write.
    #[test]
    fn a_tracker_body_keeps_every_byte_of_the_original() {
        let original = "The retry loop spins.\n\n```rust\nfn go() {}\n```\n\n## Impact\n\nBad.";
        let out = tracker_body(original, &[("First".into(), 101), ("Second".into(), 102)]);
        assert!(out.starts_with(original.trim_end()), "{out}");
        assert!(out.contains("- [ ] #101 First"), "{out}");
        assert!(out.contains("- [ ] #102 Second"), "{out}");
    }

    /// The parent is what #29 consumes, so what is written has to be what is
    /// recognised.
    #[test]
    fn a_body_this_wrote_reads_back_as_already_split() {
        assert!(!already_split("just an issue"));
        let out = tracker_body("just an issue", &[("a".into(), 1), ("b".into(), 2)]);
        assert!(already_split(&out), "{out}");
    }

    /// A fence somebody left open would otherwise swallow the checklist, which
    /// then renders as code while `already_split` still says the parent is a
    /// tracker: it looks untouched and no later run comes back to it.
    #[test]
    fn a_fence_left_open_is_closed_before_the_checklist() {
        let out = tracker_body("Here:\n\n```rust\nfn unfinished() {}", &[("a".into(), 1)]);
        assert!(out.contains("fn unfinished"), "{out}");
        let after = out.split("fn unfinished() {}").nth(1).unwrap();
        assert!(after.trim_start().starts_with("```"), "{out}");
        assert!(already_split(&out), "{out}");
    }

    /// The other half of that: a body whose fences are balanced must not have
    /// one opened for it, which would put the checklist inside a code block
    /// this wrote.
    #[test]
    fn a_closed_fence_is_left_alone() {
        for original in [
            "Here:\n\n```rust\nfn done() {}\n```",
            // A fence of the other character inside a block is content, not a
            // fence, so counting them would find three and close one.
            "Here:\n\n```\n~~~\n```",
            "no fences at all",
        ] {
            let out = tracker_body(original, &[("a".into(), 1)]);
            assert!(
                out.starts_with(&format!("{original}\n\n{SPLIT_MARKER}")),
                "{out}"
            );
        }
    }

    /// An empty body is not a reason to write a checklist with a blank line
    /// above it, or to lose the marker.
    #[test]
    fn a_tracker_body_from_nothing_is_still_a_tracker() {
        let out = tracker_body("", &[("a".into(), 1)]);
        assert!(already_split(&out), "{out}");
        assert!(out.starts_with(SPLIT_MARKER), "{out}");
    }

    /// The invariant, as code. Splitting a pull request touches code somebody
    /// wrote, and the only thing that makes it safe is that it is purely
    /// additive.
    #[test]
    fn only_a_branch_the_split_made_may_be_pushed_to() {
        assert!(additive("split-12-1", "pr-12", "").is_ok());
        assert!(additive("spar/split-12-1", "spar/pr-12", "spar/").is_ok());

        for branch in ["main", "pr-12", "issue-12", "their-feature", "split-12-1"] {
            assert!(
                additive(branch, "main", "spar/").is_err(),
                "{branch} was allowed outside the split namespace"
            );
        }
        // Even inside the namespace, never the parent's own branch.
        assert!(additive("split-12-1", "split-12-1", "").is_err());
    }

    fn carried(parts: &[SplitPart]) -> Vec<&[String]> {
        parts.iter().map(|p| p.files.as_slice()).collect()
    }

    #[test]
    fn what_no_part_carries_is_reported_as_left_over() {
        let changed = vec!["a.rs".to_string(), "b.rs".into(), "c.rs".into()];
        let parts = vec![part("one", &["a.rs"]), part("two", &["c.rs"])];
        assert_eq!(
            vec!["b.rs".to_string()],
            leftover(&changed, carried(&parts))
        );
        let all = [part("all", &["a.rs", "b.rs", "c.rs"])];
        assert!(leftover(&changed, carried(&all)).is_empty());
    }

    /// A part that was proposed and then dropped leaves its files on the
    /// original, and the comment on the original is the only permanent record
    /// of that. Counting from the proposal instead loses them silently.
    #[test]
    fn a_dropped_part_leaves_its_files_in_the_leftover_report() {
        let changed = vec!["a.rs".to_string(), "b.rs".into()];
        let made = vec![Made {
            index: 1,
            title: "one".into(),
            url: "https://example.invalid/pull/1".into(),
            files: vec!["a.rs".to_string()],
        }];
        let left = leftover(&changed, made.iter().map(|m| m.files.as_slice()));
        assert_eq!(vec!["b.rs".to_string()], left);
        assert!(
            parts_comment(&made, &left, &Style::default()).contains("b.rs"),
            "the file of the dropped part went unsaid"
        );
    }

    /// The file list came from spar, so anything not in it is a paraphrase. A
    /// path with `..` in it would otherwise be written outside the worktree.
    #[test]
    fn a_part_may_only_carry_paths_the_change_actually_touches() {
        let changed = vec!["a.rs".to_string(), "b.rs".into()];
        let mut parts = vec![part("one", &["a.rs", "../../etc/passwd", "invented.rs"])];
        let unknown = confine(&mut parts, &changed);
        assert_eq!(vec!["a.rs".to_string()], parts[0].files);
        assert_eq!(2, unknown.len(), "{unknown:?}");
    }

    /// A path in two parts would be carried twice and reviewed twice, and for a
    /// stacked split the second copy would conflict with the first.
    #[test]
    fn a_path_claimed_twice_stays_with_the_first_part() {
        let changed = vec!["a.rs".to_string(), "b.rs".into()];
        let mut parts = vec![part("one", &["a.rs", "b.rs"]), part("two", &["b.rs"])];
        confine(&mut parts, &changed);
        assert_eq!(vec!["a.rs".to_string(), "b.rs".to_string()], parts[0].files);
        assert!(parts[1].files.is_empty(), "{:?}", parts[1].files);
    }

    /// Both comments a pull request split can leave have to be recognisable
    /// again, or the next run proposes the same split, and for a fork that
    /// means the same comment once a run forever.
    #[test]
    fn a_comment_this_wrote_reads_back_as_already_split() {
        let style = Style::default();
        let made = vec![Made {
            index: 1,
            title: "First".into(),
            url: "https://example.invalid/pull/201".into(),
            files: vec!["first.rs".to_string()],
        }];
        let comment = parts_comment(&made, &["left.rs".to_string()], &style);
        assert!(already_split(&comment), "{comment}");
        assert!(comment.contains("/pull/201"), "{comment}");
        assert!(comment.contains("left.rs"), "{comment}");
        assert!(comment.contains("has not been changed"), "{comment}");

        let decision = decide(
            &proposal(vec![part("one", &["a.rs"]), part("two", &["b.rs"])]),
            &accept(),
            4,
        );
        let proposed = proposal_comment(12, &decision, &[], &style);
        assert!(already_split(&proposed), "{proposed}");
        assert!(proposed.contains("Nothing has been changed"), "{proposed}");
    }

    /// One surviving part is not a decomposition: the original still carries
    /// everything that part carries. It exists and cannot be unmade, so it is
    /// still named, but the comment must not call that a split.
    #[test]
    fn one_surviving_part_is_not_announced_as_a_split() {
        let made = vec![Made {
            index: 1,
            title: "First".into(),
            url: "https://example.invalid/pull/201".into(),
            files: vec!["first.rs".to_string()],
        }];
        let comment = parts_comment(&made, &[], &Style::default());
        assert!(!comment.contains("Split into"), "{comment}");
        assert!(comment.contains("has not been split"), "{comment}");
        assert!(comment.contains("/pull/201"), "{comment}");
        // Still recognisable, or the next run makes a second copy of it.
        assert!(already_split(&comment), "{comment}");
    }

    /// A part with no title cannot be filed and cannot be branched, so it is
    /// dropped with the rest still made.
    #[test]
    fn a_part_with_no_title_is_dropped_rather_than_filed() {
        let parts = vec![part("one", &[]), part("", &[]), part("three", &[])];
        let out = decide(&proposal(parts), &accept(), 4);
        assert_eq!(vec!["one", "three"], titles(&out));
        assert!(out.dropped[0].contains("no title"), "{:?}", out.dropped);
    }
}
