//! Alternating custody until a PR converges.
//!
//! Roles are not fixed. Whoever holds the PR may implement, review, fix, or
//! file follow-ups, and then hands custody to the other. An agent never
//! reviews its own most recent edit.
//!
//! Three failure modes are handled explicitly here, because each one breaks a
//! naive loop:
//!
//! - **The nitpick spiral.** Round 6 findings are worse than round 1 findings
//!   and a loop that counts objections cannot tell. Only `blocking` gates.
//! - **Re-litigation.** A refuted point re-raised forever never terminates.
//!   Refutations are hashed into a ledger carried across rounds.
//! - **Approval drift.** Optimising for "get approved" pressures the author
//!   into accepting wrong review comments, so refutation is blessed and the
//!   merge gate is blocking-findings-empty, not reviewer-satisfied.

use std::path::{Path, PathBuf};

use crate::agent::{self, Agent};
use crate::config::{Config, Followups};
use crate::error::Result;
use crate::jsonx::finding_key;
use crate::model::{
    Action, Dispute, Finding, Issue, IssueRun, Ledger, LedgerEntry, NextAction, PersistedState,
    PlanItem, PrView, ResponseDoc, Review, Severity, SkippedItem, Status, STATE_VERSION,
};
use crate::repo::Repo;
use crate::style::{self, Style};
use crate::{log, logdim, schema, spar_err};

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

const IMPLEMENT_PROMPT: &str = "\
Implement GitHub issue #{number} in this repository.

Title: {title}

{body}

Do the work, then commit it on the current branch. Make focused commits with
clear messages. Do not push, do not open a PR, and do not merge; the harness
handles that.

End your final message with a line of exactly this form:
SUMMARY: <one sentence under 120 characters saying what changed>
That line becomes the PR description, so write it for the reviewer who has to
read it, and say what changed rather than that you changed something.

If after reading the code you conclude this issue should not be implemented,
make no commits and explain why in your final message, beginning with
NOT_WORTH_DOING.";

const REVIEW_PROMPT: &str = "\
Review the changes on this branch against `{base}`. They implement issue
#{number}: {title}

Review thoroughly: correctness, edge cases, error handling, security, and
whether the change actually resolves the issue. Read surrounding code, do not
only read the diff.

Label every finding by severity, and be honest about which is which:
- blocking: the PR should not merge as is. Real defects only.
- non-blocking: a genuine improvement that need not gate this PR.
- nit: style or taste.

Confirm anything you label blocking before you label it. Run the code,
reproduce the failure, or point at the exact line that breaks, and say in the
detail what you did to confirm it. An unverified blocking finding is worse than
one you never raised: it stalls a good PR and teaches the author to stop
believing you. If you suspect a problem but could not confirm it, say so and
label it non-blocking.

Set in_scope=false for a real problem that exists but is not caused by this PR.
Those become follow-up issues rather than review comments.

Then choose next_action:
- merge: no blocking findings, the PR is good.
- fix_myself: there are blocking findings and you will fix them directly.
- hand_back: there are blocking findings the author should address.
{settled}";

const FIX_PROMPT: &str = "\
You reviewed this branch and chose to fix the blocking findings yourself.
Implement those fixes now and commit them.

Your findings:
{findings}

Commit your changes. Do not push, do not merge.";

const RESPOND_PROMPT: &str = "\
Here is a review of your PR for issue #{number}.

{findings}

For each point, choose exactly one disposition:
- fixed: the point is valid and in scope. Fix it and commit.
- refuted: the point is wrong, or not worth acting on. Explain why. Refuting is
  a legitimate outcome; do not accept a review comment you believe is incorrect
  just to get the PR approved.
- filed_issue: the point is valid but unrelated to this PR. Supply
  new_issue_title and new_issue_body; the harness files it and skips duplicates.

Copy each finding's title and file across exactly as given, so your answer can
be matched back to the review.

Commit any fixes. Do not push, do not merge.";

/// A worktree is only worth keeping when a person has to look at it locally.
/// Anything else strands a checked-out branch that blocks
/// `gh pr merge --delete-branch`, and since auto_merge is off by default,
/// keeping it on anything but "merged" leaks one per run.
fn should_release(cfg: &Config, status: Status) -> bool {
    if !cfg.loop_cfg.worktrees || cfg.loop_cfg.keep_worktrees {
        return false;
    }
    !matches!(status, Status::Escalated | Status::Error)
}

// ---------------------------------------------------------------------------
// One issue, start to finish
// ---------------------------------------------------------------------------

pub fn run_issue(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    item: &PlanItem,
    issue: &Issue,
    ledger: &mut Ledger,
) -> IssueRun {
    let mut state = IssueRun::new(item.issue, item.title.clone());
    let base = cfg.base_branch().to_string();

    let prepared = if cfg.loop_cfg.worktrees {
        repo.worktree_add(item.issue, &base)
    } else {
        let branch = repo.branch_for_issue(item.issue);
        let start = format!("origin/{base}");
        repo.git(&["checkout", "-B", &branch, &start])
            .map(|_| (repo.root().to_path_buf(), branch))
    };

    let (work_dir, branch) = match prepared {
        Ok(pair) => pair,
        Err(e) => {
            state.status = Status::Error;
            state.notes.push(e.to_string());
            log!("#{} failed: {e}", item.issue);
            return state;
        }
    };

    let outcome = implement_and_review(
        agents, cfg, repo, item, issue, ledger, &mut state, &work_dir, &branch,
    );
    if let Err(e) = outcome {
        state.status = Status::Error;
        state.notes.push(e.to_string());
        log!("#{} failed: {e}", item.issue);
    }

    if should_release(cfg, state.status) {
        repo.worktree_remove(item.issue);
    }
    state
}

#[allow(clippy::too_many_arguments)]
fn implement_and_review(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    item: &PlanItem,
    issue: &Issue,
    ledger: &mut Ledger,
    state: &mut IssueRun,
    work_dir: &Path,
    branch: &str,
) -> Result<()> {
    let number = item.issue;
    let holder = cfg.first_implementor.clone();
    let implementor = agent::find(agents, &holder)?;
    let base = cfg.base_branch().to_string();

    log!("#{number}: {holder} implementing");
    let body: String = issue.body_text().trim().chars().take(6000).collect();
    let prompt = IMPLEMENT_PROMPT
        .replace("{number}", &number.to_string())
        .replace("{title}", &item.title)
        .replace("{body}", &body);
    let out = implementor.ask(
        &prompt,
        work_dir,
        cfg.effort_for_round(&implementor.spec, 1).as_deref(),
    )?;

    if out.to_uppercase().contains("NOT_WORTH_DOING") || !repo.has_changes(work_dir, &base) {
        state.status = Status::Abandoned;
        let reason = style::body(&out, &repo.style);
        state.notes.push(reason.clone());
        let comment = format!("No PR opened.\n\n{reason}");
        if let Err(e) = repo.comment_issue(number, &comment) {
            logdim!("could not comment on #{number}: {e}");
        }
        return Ok(());
    }

    repo.rewrite_commits_if_needed(work_dir, &base)?;
    repo.push(work_dir, branch)?;

    let pr = match repo.pr_for_branch(branch) {
        Some(existing) => existing,
        None => {
            let summary = extract_summary(&out).unwrap_or_else(|| item.title.clone());
            let body = pr_body(
                number,
                &summary,
                &repo.diff_stat(work_dir, &base),
                &repo.style,
            );
            repo.create_pr(
                work_dir,
                branch,
                &base,
                &format!("{} (#{number})", item.title),
                &body,
            )?
        }
    };
    state.pr = Some(pr.url.clone());
    log!("#{number}: PR {}", pr.url);

    let ctx = LoopCtx {
        work_dir: work_dir.to_path_buf(),
        branch: branch.to_string(),
        pr_number: pr.number,
        label: format!("#{number}"),
        subject: number,
        title: item.title.clone(),
        start_round: 1,
        holder: cfg.other(&holder),
        release: Release::Issue(number),
    };
    review_loop(agents, cfg, repo, &ctx, state, ledger)
}

// ---------------------------------------------------------------------------
// Resuming an existing PR
// ---------------------------------------------------------------------------

/// Pick up an existing PR and continue the loop.
///
/// The PR need not have been created by spar. Anything with a branch and a diff
/// can be reviewed, including work a person or a different tool started, which
/// is also the cheapest way to adopt spar: no agent writes a feature from
/// scratch, it only reviews what already exists.
pub fn resume_pr(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr_number: i64,
    holder_override: Option<&str>,
) -> IssueRun {
    match resume_inner(agents, cfg, repo, pr_number, holder_override) {
        Ok(state) => state,
        Err(e) => {
            log!("PR #{pr_number} failed: {e}");
            let mut state = IssueRun::new(pr_number, format!("PR #{pr_number}"));
            state.status = Status::Error;
            state.notes.push(e.to_string());
            state
        }
    }
}

fn resume_inner(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr_number: i64,
    holder_override: Option<&str>,
) -> Result<IssueRun> {
    let pr: PrView = repo.pr_view(pr_number)?;
    if !pr.is_open() {
        return Err(spar_err!("PR #{pr_number} is {}", pr.state.to_lowercase()));
    }

    let subject = pr
        .closing_issues_references
        .first()
        .map(|r| r.number)
        .unwrap_or(pr_number);

    let saved = repo.read_state(&pr);
    let mut ledger: Ledger = saved.as_ref().map(|s| s.ledger.clone()).unwrap_or_default();
    let start_round = saved.as_ref().map(|s| s.round + 1).unwrap_or(1);

    let default_holder = cfg.other(&cfg.first_implementor);
    let mut holder = holder_override
        .map(str::to_string)
        .or_else(|| saved.as_ref().map(|s| s.next_actor.clone()))
        .unwrap_or_else(|| default_holder.clone());
    if !cfg.has_agent(&holder) {
        log!("state named unknown agent '{holder}', using {default_holder}");
        holder = default_holder;
    }

    match &saved {
        Some(_) => log!(
            "PR #{pr_number}: resuming at round {start_round}, {} settled point(s), next up {holder}",
            ledger.len()
        ),
        None => log!("PR #{pr_number}: no prior spar state, starting fresh with {holder}"),
    }

    if start_round > cfg.loop_cfg.max_rounds {
        return Err(spar_err!(
            "PR #{pr_number} already used {} of {} rounds. Raise --max-rounds to continue.",
            start_round - 1,
            cfg.loop_cfg.max_rounds
        ));
    }

    let mut state = IssueRun::new(subject, pr.title.clone());
    state.pr = Some(pr.url.clone());
    if let Some(s) = &saved {
        state.filed = s.filed.clone();
    }

    let (work_dir, branch) = repo.worktree_for_pr(&pr)?;
    let ctx = LoopCtx {
        work_dir,
        branch,
        pr_number,
        label: format!("PR #{pr_number}"),
        subject,
        title: pr.title.clone(),
        start_round,
        holder,
        release: Release::Pr(pr_number),
    };

    let outcome = review_loop(agents, cfg, repo, &ctx, &mut state, &mut ledger);
    if let Err(e) = outcome {
        state.status = Status::Error;
        state.notes.push(e.to_string());
        log!("PR #{pr_number} failed: {e}");
    }
    if should_release(cfg, state.status) {
        repo.release_pr_worktree(pr_number);
    }
    Ok(state)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Release {
    Issue(i64),
    Pr(i64),
}

struct LoopCtx {
    work_dir: PathBuf,
    branch: String,
    pr_number: i64,
    label: String,
    subject: i64,
    title: String,
    start_round: u32,
    holder: String,
    release: Release,
}

impl LoopCtx {
    fn release(&self, repo: &Repo) {
        match self.release {
            Release::Issue(n) => repo.worktree_remove(n),
            Release::Pr(n) => repo.release_pr_worktree(n),
        }
    }
}

fn review_loop(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    ctx: &LoopCtx,
    state: &mut IssueRun,
    ledger: &mut Ledger,
) -> Result<()> {
    let base = cfg.base_branch().to_string();
    let mut holder = ctx.holder.clone();

    for round in ctx.start_round..=cfg.loop_cfg.max_rounds {
        state.rounds = round;
        let reviewer = agent::find(agents, &holder)?;
        let effort = cfg.effort_for_round(&reviewer.spec, round);
        log!(
            "{}: round {round}, {holder} reviewing ({})",
            ctx.label,
            effort.as_deref().unwrap_or("default effort")
        );

        let prompt = REVIEW_PROMPT
            .replace("{base}", &base)
            .replace("{number}", &ctx.subject.to_string())
            .replace("{title}", &ctx.title)
            .replace("{settled}", &settled_block(ledger));
        let review: Review = reviewer.review(
            &base,
            &prompt,
            &schema::review(),
            &ctx.work_dir,
            effort.as_deref(),
        )?;

        let blocking: Vec<Finding> = review
            .findings
            .iter()
            .filter(|f| f.blocks())
            .cloned()
            .collect();

        if let Err(e) = repo.comment_pr(
            ctx.pr_number,
            &review_comment(&holder, round, &review, &repo.style),
        ) {
            logdim!("could not post the review comment: {e}");
        }

        // Filed every round, not only on approval: a run that escalates or runs
        // out of rounds would otherwise drop these on the floor. Filing
        // deduplicates by title, so repeats across rounds are free.
        file_out_of_scope(repo, &review.findings, ctx.subject, state);
        file_nonblocking(
            repo,
            &review.findings,
            ctx.subject,
            state,
            cfg.loop_cfg.file_nits,
        );

        if check_relitigation(ledger, &blocking, state) {
            state.status = Status::Escalated;
            let titles: Vec<String> = blocking
                .iter()
                .map(|f| style::title(&f.title, &repo.style))
                .collect();
            let note = format!(
                "Escalating: a point already refuted was raised again twice. Needs a human \
                 decision.\n\n{}",
                bullets(&titles)
            );
            let _ = repo.comment_pr(ctx.pr_number, &note);
            persist(
                repo,
                ctx.pr_number,
                state,
                ledger,
                round,
                &cfg.other(&holder),
            );
            return Ok(());
        }

        if blocking.is_empty() {
            state.status = Status::Approved;
            persist(
                repo,
                ctx.pr_number,
                state,
                ledger,
                round,
                &cfg.other(&holder),
            );
            if cfg.loop_cfg.auto_merge {
                // Release the worktree first. `gh pr merge --delete-branch`
                // fails if anything still has the branch checked out, and it
                // fails *after* merging, so the merge lands while the command
                // reports failure.
                ctx.release(repo);
                repo.merge_pr(ctx.pr_number)?;
                state.status = Status::Merged;
                repo.clear_state(ctx.pr_number); // nothing left to resume
                log!("{}: merged", ctx.label);
            } else {
                log!("{}: approved, awaiting human merge", ctx.label);
            }
            return Ok(());
        }

        if review.next_action == NextAction::FixMyself {
            log!("{}: {holder} fixing its own findings", ctx.label);
            let prompt = FIX_PROMPT.replace("{findings}", &findings_for_prompt(&blocking));
            reviewer.ask(&prompt, &ctx.work_dir, effort.as_deref())?;
        } else {
            let author_name = cfg.other(&holder);
            let author = agent::find(agents, &author_name)?;
            log!(
                "{}: handing {} finding(s) to {author_name}",
                ctx.label,
                blocking.len()
            );
            let prompt = RESPOND_PROMPT
                .replace("{number}", &ctx.subject.to_string())
                .replace("{findings}", &findings_for_prompt(&blocking));
            let response: ResponseDoc = author.ask_json(
                &prompt,
                &schema::response(),
                &ctx.work_dir,
                cfg.effort_for_round(&author.spec, round).as_deref(),
            )?;
            apply_dispositions(
                repo,
                &response,
                &blocking,
                ledger,
                state,
                round,
                ctx.subject,
                ctx.pr_number,
                &author_name,
            );
        }

        repo.rewrite_commits_if_needed(&ctx.work_dir, &base)?;
        repo.push(&ctx.work_dir, &ctx.branch)?;
        holder = cfg.other(&holder);
        persist(repo, ctx.pr_number, state, ledger, round, &holder);
    }

    state.status = Status::Escalated;
    state.notes.push(format!(
        "no convergence after {} rounds",
        cfg.loop_cfg.max_rounds
    ));
    let _ = repo.comment_pr(
        ctx.pr_number,
        &format!(
            "Stopping after {} review rounds without convergence. Needs a human decision.",
            cfg.loop_cfg.max_rounds
        ),
    );
    persist(
        repo,
        ctx.pr_number,
        state,
        ledger,
        cfg.loop_cfg.max_rounds,
        &holder,
    );
    Ok(())
}

fn persist(
    repo: &Repo,
    pr_number: i64,
    state: &IssueRun,
    ledger: &Ledger,
    round: u32,
    next_actor: &str,
) {
    let payload = PersistedState {
        version: STATE_VERSION,
        round,
        next_actor: next_actor.to_string(),
        status: state.status,
        ledger: ledger.clone(),
        filed: state.filed.clone(),
    };
    if let Err(e) = repo.write_state(pr_number, &payload) {
        logdim!("could not persist state for PR #{pr_number}: {e}");
    }
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

fn settled_block(ledger: &Ledger) -> String {
    if ledger.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = ledger
        .values()
        .map(|e| format!("- {}: refuted because {}", e.title, e.reasoning))
        .collect();
    format!(
        "\nThe following points were already raised and refuted. Treat them as settled. Do not \
         raise them again unless you have new evidence:\n{}",
        lines.join("\n")
    )
}

/// A point refuted and then raised twice more goes to a person rather than
/// looping forever.
fn check_relitigation(ledger: &mut Ledger, blocking: &[Finding], state: &mut IssueRun) -> bool {
    let mut escalate = false;
    for finding in blocking {
        let key = finding_key(&finding.title, &finding.file);
        if let Some(entry) = ledger.get_mut(&key) {
            entry.reraised += 1;
            if entry.reraised >= 2 {
                state.notes.push(format!(
                    "'{}' was refuted and re-raised twice; escalating.",
                    finding.title
                ));
                escalate = true;
            }
        }
    }
    escalate
}

fn normalise(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Match a disposition back to the finding it answers, so the ledger key it
/// records is the same key the next round's finding will hash to. Without this
/// the re-litigation guard is dead code for any finding that names a file.
fn matching_finding<'a>(findings: &'a [Finding], title: &str) -> Option<&'a Finding> {
    let wanted = normalise(title);
    findings.iter().find(|f| normalise(&f.title) == wanted)
}

#[allow(clippy::too_many_arguments)]
fn apply_dispositions(
    repo: &Repo,
    response: &ResponseDoc,
    blocking: &[Finding],
    ledger: &mut Ledger,
    state: &mut IssueRun,
    round: u32,
    subject: i64,
    pr_number: i64,
    author: &str,
) {
    let mut fixed = Vec::new();
    let mut refuted = Vec::new();
    let mut filed = Vec::new();

    for d in &response.dispositions {
        let source = matching_finding(blocking, &d.title);
        let file = source
            .map(|f| f.file.clone())
            .filter(|f| !f.trim().is_empty())
            .unwrap_or_else(|| d.file.clone());
        // Hash the *reviewer's* wording, not the author's. `matching_finding`
        // is deliberately looser than `finding_key` (it ignores hyphens, dots,
        // slashes, and underscores), so an author who writes "multibyte" where
        // the reviewer wrote "multi-byte" matches here and yet hashes to a
        // different key. Recording that key means next round's lookup misses
        // and the re-litigation guard tracks nothing at all.
        let canonical = source.map(|f| f.title.as_str()).unwrap_or(d.title.as_str());
        let title = style::title(canonical, &repo.style);

        match d.action {
            Action::Refuted => {
                let reasoning = style::summary(&d.reasoning, &repo.style);
                ledger.insert(
                    finding_key(canonical, &file),
                    LedgerEntry {
                        title: title.clone(),
                        file: file.clone(),
                        reasoning: reasoning.clone(),
                        round,
                        reraised: 0,
                    },
                );
                state.disputes.push(Dispute {
                    title: title.clone(),
                    reasoning: reasoning.clone(),
                });
                refuted.push(format!("{title}. {reasoning}"));
            }
            Action::FiledIssue => {
                let new_title = d
                    .new_issue_title
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| d.title.clone());
                let new_body = d
                    .new_issue_body
                    .clone()
                    .filter(|b| !b.trim().is_empty())
                    .unwrap_or_else(|| d.reasoning.clone());
                if let Some(url) = file_followup(repo, &new_title, &new_body, subject) {
                    state.filed.push(url.clone());
                    filed.push(url);
                }
            }
            Action::Fixed => fixed.push(title),
        }
    }

    let comment = disposition_comment(author, response, &fixed, &refuted, &filed, &repo.style);
    if let Some(text) = comment {
        if let Err(e) = repo.comment_pr(pr_number, &text) {
            logdim!("could not post the disposition comment: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Follow-ups
// ---------------------------------------------------------------------------

/// Record a finding that is real but out of scope for this PR.
///
/// On your own repository an issue is the right home. On a large repository
/// that is not yours it is somebody else's notification and somebody else's
/// triage queue, so `local` keeps the same information in `.spar/followups.md`
/// and `none` drops it.
fn file_followup(repo: &Repo, title: &str, body: &str, source: i64) -> Option<String> {
    if repo.followups == Followups::None {
        return None;
    }
    // The exact string that will land on GitHub. Searching for anything else
    // means the duplicate check can never hit, and every round files another
    // copy of the same follow-up.
    let title = match repo.clean_title(title) {
        Ok(title) => title,
        Err(e) => {
            logdim!("could not clean a follow-up title: {e}");
            return None;
        }
    };
    if title.trim().is_empty() {
        return None;
    }
    let body = format!(
        "{}\n\nFound while working on #{source}.",
        style::body(body, &repo.style)
    );

    if repo.followups == Followups::Local {
        return repo.append_local_followup(&title, &body, source);
    }
    if let Some(existing) = repo.find_issue_by_title(&title) {
        logdim!("follow-up already exists: {existing}");
        return None;
    }
    match repo.create_issue(&title, &body) {
        Ok(url) => Some(url),
        Err(e) => {
            logdim!("could not file a follow-up for '{title}': {e}");
            None
        }
    }
}

fn file_out_of_scope(repo: &Repo, findings: &[Finding], subject: i64, state: &mut IssueRun) {
    for finding in findings.iter().filter(|f| !f.in_scope) {
        if let Some(url) = file_followup(repo, &finding.title, &finding.detail, subject) {
            state.filed.push(url);
        }
    }
}

/// Non-blocking findings become follow-ups so they do not gate the merge.
///
/// Nits are excluded by default. On a shared repository a filed nit is somebody
/// else's notification and somebody else's triage queue: an early run on a
/// production codebase opened an issue titled "Log wording". Worth saying in
/// the PR thread, not worth an issue.
fn file_nonblocking(
    repo: &Repo,
    findings: &[Finding],
    subject: i64,
    state: &mut IssueRun,
    file_nits: bool,
) {
    for finding in findings {
        let keep = match finding.severity {
            Severity::NonBlocking => true,
            Severity::Nit => file_nits,
            Severity::Blocking => false,
        };
        if !keep || !finding.in_scope {
            continue;
        }
        if let Some(url) = file_followup(repo, &finding.title, &finding.detail, subject) {
            state.filed.push(url);
        }
    }
}

// ---------------------------------------------------------------------------
// What a human actually reads
// ---------------------------------------------------------------------------
//
// spar composes every comment itself from structured fields, rather than
// forwarding whatever prose a model produced. That is the only reliable way to
// keep a PR thread readable: the model supplies facts, the harness supplies the
// shape, and each field is held to a budget on the way out.

fn bullets(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn located(finding: &Finding, style: &Style) -> String {
    let title = style::title(&finding.title, style);
    match finding.where_at() {
        "general" => title,
        file => format!("{title} ({file})"),
    }
}

/// The PR body: what it closes, one sentence of what changed, and the diffstat.
/// GitHub already shows the file list, so repeating it is noise.
pub fn pr_body(issue: i64, summary: &str, stat: &str, style: &Style) -> String {
    let mut parts = vec![format!("Closes #{issue}")];
    let summary = style::summary(summary, style);
    if !summary.is_empty() {
        parts.push(summary);
    }
    if !stat.trim().is_empty() {
        parts.push(stat.trim().to_string());
    }
    parts.join("\n\n")
}

/// The last `SUMMARY:` line an implementor emitted, if it left one.
pub fn extract_summary(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .find_map(|line| {
            let trimmed = line.trim().trim_start_matches(['*', '#', '-', ' ']);
            trimmed
                .strip_prefix("SUMMARY:")
                .or_else(|| trimmed.strip_prefix("Summary:"))
        })
        .map(|s| {
            s.trim()
                .trim_start_matches(['*', '_', ':', ' '])
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
}

/// One review, as a reviewer would write it if they were in a hurry: a count
/// line, a sentence, and one bullet per finding. Only blocking findings carry
/// their detail, because only those are something the author has to act on now.
pub fn review_comment(holder: &str, round: u32, review: &Review, style: &Style) -> String {
    let by = |severity: Severity| -> Vec<&Finding> {
        review
            .findings
            .iter()
            .filter(|f| f.severity == severity && f.in_scope)
            .collect()
    };
    let blocking = by(Severity::Blocking);
    let non_blocking = by(Severity::NonBlocking);
    let nits = by(Severity::Nit);
    let out_of_scope: Vec<&Finding> = review.findings.iter().filter(|f| !f.in_scope).collect();

    let mut counts = Vec::new();
    if !blocking.is_empty() {
        counts.push(format!("{} blocking", blocking.len()));
    }
    if !non_blocking.is_empty() {
        counts.push(format!("{} non-blocking", non_blocking.len()));
    }
    if !nits.is_empty() {
        counts.push(format!("{} nit", nits.len()));
    }
    if !out_of_scope.is_empty() {
        counts.push(format!("{} out of scope", out_of_scope.len()));
    }
    let headline = if counts.is_empty() {
        "no findings".to_string()
    } else {
        counts.join(", ")
    };

    let mut out = vec![format!("{holder} round {round}: {headline}.")];
    let summary = style::summary(&review.summary, style);
    if !summary.is_empty() {
        out.push(summary);
    }

    if !blocking.is_empty() {
        let lines: Vec<String> = blocking
            .iter()
            .map(|f| {
                let detail = style::detail(&f.detail, style);
                if detail.is_empty() {
                    located(f, style)
                } else {
                    format!("{}. {detail}", located(f, style))
                }
            })
            .collect();
        out.push(format!("blocking\n{}", bullets(&lines)));
    }

    // Everything below is filed as a follow-up, so the thread only needs the
    // title: the detail lives on the issue where it can be acted on.
    for (label, group) in [
        ("non-blocking, filed as follow-ups", &non_blocking),
        ("nits", &nits),
        ("out of scope, filed separately", &out_of_scope),
    ] {
        if group.is_empty() {
            continue;
        }
        let lines: Vec<String> = group.iter().map(|f| located(f, style)).collect();
        out.push(format!("{label}\n{}", bullets(&lines)));
    }

    out.join("\n\n")
}

/// One response to a review. Refutations carry their reasoning because that is
/// the whole argument; fixes are a list of titles because the diff says the
/// rest.
pub fn disposition_comment(
    author: &str,
    response: &ResponseDoc,
    fixed: &[String],
    refuted: &[String],
    filed: &[String],
    style: &Style,
) -> Option<String> {
    if fixed.is_empty() && refuted.is_empty() && filed.is_empty() {
        return None;
    }
    let mut counts = Vec::new();
    if !fixed.is_empty() {
        counts.push(format!("{} fixed", fixed.len()));
    }
    if !refuted.is_empty() {
        counts.push(format!("{} refuted", refuted.len()));
    }
    if !filed.is_empty() {
        counts.push(format!("{} filed", filed.len()));
    }

    let mut out = vec![format!("{author}: {}.", counts.join(", "))];
    let summary = style::summary(&response.summary, style);
    if !summary.is_empty() {
        out.push(summary);
    }
    if !refuted.is_empty() {
        out.push(format!("refuted\n{}", bullets(refuted)));
    }
    if !fixed.is_empty() {
        out.push(format!("fixed\n{}", bullets(fixed)));
    }
    if !filed.is_empty() {
        out.push(format!("filed\n{}", bullets(filed)));
    }
    Some(out.join("\n\n"))
}

/// What is posted on an issue both agents declined.
pub fn skip_comment(item: &SkippedItem, style: &Style) -> String {
    let lines: Vec<String> = item
        .reasons
        .iter()
        .map(|(name, reason)| format!("{name}: {}", style::summary(reason, style)))
        .collect();
    format!(
        "Reviewed by two independent reviewers and not scheduled for a PR.\n\n{}",
        bullets(&lines)
    )
}

/// Findings as a model should see them: full detail, since this one is not for
/// a human to read.
fn findings_for_prompt(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "(none)".to_string();
    }
    findings
        .iter()
        .map(|f| {
            let scope = if f.in_scope { "" } else { " [out of scope]" };
            format!(
                "- [{}]{scope} {} ({})\n  {}",
                f.severity,
                f.title,
                f.where_at(),
                f.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Verdict;

    fn style() -> Style {
        Style::default()
    }

    fn finding(severity: &str, title: &str, detail: &str, file: &str, in_scope: bool) -> Finding {
        Finding {
            severity: Severity::parse_lenient(severity).unwrap(),
            title: title.into(),
            detail: detail.into(),
            file: file.into(),
            in_scope,
        }
    }

    fn review(summary: &str, findings: Vec<Finding>) -> Review {
        Review {
            verdict: Verdict::Approve,
            next_action: NextAction::Merge,
            summary: summary.into(),
            findings,
        }
    }

    // -- worktree release ------------------------------------------------

    fn cfg_with(worktrees: bool, keep: bool) -> Config {
        let text = "[agents.a]\ncommand = [\"x\"]\n[agents.b]\ncommand = [\"y\"]\n";
        let mut cfg = crate::config::parse(text).unwrap();
        cfg.loop_cfg.worktrees = worktrees;
        cfg.loop_cfg.keep_worktrees = keep;
        cfg
    }

    #[test]
    fn a_worktree_is_released_on_every_finished_outcome() {
        let cfg = cfg_with(true, false);
        for status in [Status::Approved, Status::Merged, Status::Abandoned] {
            assert!(should_release(&cfg, status), "{status}");
        }
    }

    /// Releasing only on "merged" leaked one worktree per run, because
    /// auto_merge is off by default and runs end at "approved".
    #[test]
    fn a_worktree_is_kept_only_where_a_human_has_to_look() {
        let cfg = cfg_with(true, false);
        assert!(!should_release(&cfg, Status::Escalated));
        assert!(!should_release(&cfg, Status::Error));
    }

    #[test]
    fn the_keep_flag_overrides_everything() {
        assert!(!should_release(&cfg_with(true, true), Status::Approved));
    }

    #[test]
    fn nothing_is_released_when_worktrees_are_off() {
        assert!(!should_release(&cfg_with(false, false), Status::Approved));
    }

    // -- the ledger ------------------------------------------------------

    fn ledger_with(title: &str, file: &str) -> Ledger {
        let mut ledger = Ledger::new();
        ledger.insert(
            finding_key(title, file),
            LedgerEntry {
                title: title.into(),
                file: file.into(),
                reasoning: "no".into(),
                round: 1,
                reraised: 0,
            },
        );
        ledger
    }

    #[test]
    fn a_point_refuted_and_re_raised_twice_escalates() {
        let mut ledger = ledger_with("nit about naming", "a.rs");
        let mut state = IssueRun::new(1, "t");
        let blocking = vec![finding("blocking", "nit about naming", "d", "a.rs", true)];
        assert!(!check_relitigation(&mut ledger, &blocking, &mut state));
        assert!(check_relitigation(&mut ledger, &blocking, &mut state));
    }

    #[test]
    fn an_untracked_finding_does_not_escalate() {
        let mut state = IssueRun::new(1, "t");
        let blocking = vec![finding("blocking", "brand new", "d", "a.rs", true)];
        assert!(!check_relitigation(
            &mut Ledger::new(),
            &blocking,
            &mut state
        ));
    }

    /// The key a refutation records has to be the key the next round's finding
    /// hashes to. Recording it without the file made the guard dead code for
    /// every finding that named one, which is nearly all of them.
    #[test]
    fn a_refutation_lands_on_the_key_the_next_round_will_look_up() {
        let blocking = vec![finding("blocking", "Unbounded loop", "d", "src/x.rs", true)];
        let recorded = finding_key(&blocking[0].title, &blocking[0].file);

        let matched = matching_finding(&blocking, "unbounded loop!").expect("should match");
        assert_eq!(recorded, finding_key("unbounded loop!", &matched.file));
    }

    /// `matching_finding` ignores hyphens, dots, slashes, and underscores;
    /// `finding_key` keeps them. A disposition that differs only in those
    /// characters therefore matches its finding while hashing to a different
    /// key, so recording the author's wording made the guard track nothing.
    #[test]
    fn the_ledger_key_uses_the_reviewers_wording_not_the_authors() {
        let findings = vec![finding(
            "blocking",
            "Panic on multi-byte input",
            "d",
            "src/style.rs",
            true,
        )];
        let reworded = "Panic on multibyte input";

        let source = matching_finding(&findings, reworded).expect("still matches");
        assert_ne!(
            finding_key(reworded, &source.file),
            finding_key(&source.title, &source.file),
            "the two spellings must genuinely hash apart, or this test proves nothing"
        );

        // What apply_dispositions records, and what the next round looks up.
        let recorded = finding_key(&source.title, &source.file);
        let looked_up = finding_key(&findings[0].title, &findings[0].file);
        assert_eq!(recorded, looked_up);
    }

    #[test]
    fn a_disposition_matches_its_finding_despite_wording_noise() {
        let findings = vec![finding(
            "blocking",
            "Unbounded loop!",
            "d",
            "src/x.rs",
            true,
        )];
        assert!(matching_finding(&findings, "unbounded loop").is_some());
        assert!(matching_finding(&findings, "something else").is_none());
    }

    #[test]
    fn the_settled_block_is_empty_when_nothing_is_settled() {
        assert_eq!("", settled_block(&Ledger::new()));
    }

    #[test]
    fn the_settled_block_names_each_refutation() {
        let block = settled_block(&ledger_with("a point", "x.rs"));
        assert!(block.contains("a point"));
        assert!(block.contains("settled"));
    }

    // -- brevity ---------------------------------------------------------

    #[test]
    fn a_clean_review_is_two_lines() {
        let text = review_comment("codex", 1, &review("Looks correct.", vec![]), &style());
        assert_eq!("codex round 1: no findings.\n\nLooks correct.", text);
    }

    #[test]
    fn a_review_leads_with_the_counts() {
        let text = review_comment(
            "codex",
            2,
            &review(
                "One real problem.",
                vec![
                    finding(
                        "blocking",
                        "Loop never terminates",
                        "Confirmed by running it.",
                        "src/a.rs",
                        true,
                    ),
                    finding("non-blocking", "Name is vague", "d", "src/b.rs", true),
                    finding("nit", "Log wording", "d", "", true),
                ],
            ),
            &style(),
        );
        assert!(
            text.starts_with("codex round 2: 1 blocking, 1 non-blocking, 1 nit."),
            "{text}"
        );
    }

    /// Only blocking findings carry their detail into the thread. Everything
    /// else is filed, and the detail belongs on the issue.
    #[test]
    fn only_blocking_findings_carry_their_detail() {
        let text = review_comment(
            "codex",
            1,
            &review(
                "s",
                vec![
                    finding("blocking", "Loop", "BLOCKING DETAIL", "a.rs", true),
                    finding("non-blocking", "Name", "NONBLOCKING DETAIL", "b.rs", true),
                ],
            ),
            &style(),
        );
        assert!(text.contains("BLOCKING DETAIL"), "{text}");
        assert!(!text.contains("NONBLOCKING DETAIL"), "{text}");
    }

    #[test]
    fn a_verbose_model_is_clipped_not_forwarded() {
        let long_summary = "This is a very thorough summary. ".repeat(40);
        let long_detail = "Here is an extremely long explanation. ".repeat(40);
        let text = review_comment(
            "codex",
            1,
            &review(
                &long_summary,
                vec![finding("blocking", "T", &long_detail, "a.rs", true)],
            ),
            &style(),
        );
        assert!(
            text.len() < 900,
            "review comment was {} chars:\n{text}",
            text.len()
        );
    }

    #[test]
    fn a_general_finding_has_no_empty_parenthesis() {
        let text = review_comment(
            "codex",
            1,
            &review("s", vec![finding("blocking", "Something", "d", "", true)]),
            &style(),
        );
        assert!(!text.contains("()"), "{text}");
        assert!(!text.contains("(general)"), "{text}");
    }

    #[test]
    fn out_of_scope_findings_are_counted_separately() {
        let text = review_comment(
            "codex",
            1,
            &review(
                "s",
                vec![finding("blocking", "Old bug", "d", "a.rs", false)],
            ),
            &style(),
        );
        assert!(text.contains("1 out of scope"), "{text}");
        assert!(!text.contains("1 blocking"), "{text}");
    }

    #[test]
    fn a_disposition_comment_leads_with_counts_and_keeps_refutations() {
        let response = ResponseDoc {
            summary: "Two of three were right.".into(),
            dispositions: vec![],
        };
        let text = disposition_comment(
            "claude",
            &response,
            &["Fixed thing".to_string()],
            &["Wrong thing. Because the caller already checks.".to_string()],
            &[],
            &style(),
        )
        .unwrap();
        assert!(text.starts_with("claude: 1 fixed, 1 refuted."), "{text}");
        assert!(
            text.contains("Because the caller already checks."),
            "{text}"
        );
    }

    #[test]
    fn an_empty_disposition_comment_is_not_posted() {
        let response = ResponseDoc {
            summary: "s".into(),
            dispositions: vec![],
        };
        assert!(disposition_comment("claude", &response, &[], &[], &[], &style()).is_none());
    }

    #[test]
    fn a_pr_body_is_three_short_parts() {
        let body = pr_body(
            42,
            "Retry on a 429 instead of failing.",
            "2 files changed, 30 insertions(+)",
            &style(),
        );
        assert_eq!(
            "Closes #42\n\nRetry on a 429 instead of failing.\n\n2 files changed, 30 insertions(+)",
            body
        );
    }

    #[test]
    fn a_pr_body_survives_a_missing_summary_and_diffstat() {
        assert_eq!("Closes #7", pr_body(7, "", "", &style()));
    }

    #[test]
    fn the_summary_line_is_lifted_out_of_the_final_message() {
        let out = "I did some work.\n\nSUMMARY: Retry on a 429 instead of failing.\n";
        assert_eq!(
            Some("Retry on a 429 instead of failing.".to_string()),
            extract_summary(out)
        );
    }

    #[test]
    fn a_decorated_summary_line_still_parses() {
        assert_eq!(
            Some("Did a thing.".to_string()),
            extract_summary("**SUMMARY:** Did a thing.")
        );
    }

    #[test]
    fn a_missing_summary_line_is_none() {
        assert_eq!(None, extract_summary("no marker here"));
    }

    #[test]
    fn the_last_summary_line_wins() {
        let out = "SUMMARY: first draft\nmore work\nSUMMARY: final answer";
        assert_eq!(Some("final answer".to_string()), extract_summary(out));
    }

    #[test]
    fn a_skip_comment_names_both_reviewers() {
        let item = SkippedItem {
            issue: 3,
            title: "t".into(),
            reasons: [
                ("claude".to_string(), "Already fixed in 1.2.".to_string()),
                ("codex".to_string(), "Duplicate of #2.".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let text = skip_comment(&item, &style());
        assert!(text.contains("claude: Already fixed in 1.2."), "{text}");
        assert!(text.contains("codex: Duplicate of #2."), "{text}");
        assert!(text.lines().count() <= 5, "{text}");
    }

    #[test]
    fn findings_for_a_model_keep_full_detail() {
        let long = "x".repeat(2000);
        let text = findings_for_prompt(&[finding("blocking", "T", &long, "a.rs", true)]);
        assert!(
            text.contains(&long),
            "a model needs the whole finding, only humans need brevity"
        );
    }

    #[test]
    fn findings_for_a_model_are_never_empty() {
        assert_eq!("(none)", findings_for_prompt(&[]));
    }
}
