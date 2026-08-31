//! Alternating custody until a PR converges.
//!
//! Roles are not fixed. Whoever holds the PR may implement, review, fix, or
//! file follow-ups, and then hands custody to the other. An agent never
//! reviews its own most recent edit, and custody follows the commit that
//! landed rather than the action a reviewer asked for: a call that returns is
//! not a call that wrote anything.
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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::agent::{self, Agent};
use crate::config::{Config, Drafts, Followups, PrComments};
use crate::error::{ErrorKind, Result, SparError};
use crate::jsonx::{exact_finding_key as finding_key, finding_file, stable_finding_key};
use crate::model::{
    Action, Disposition, Dispute, Finding, Followup, Implementation, Issue, IssueRun, Ledger,
    LedgerEntry, NextAction, PersistedState, PlanItem, PrView, ResponseDoc, Review, Settled,
    Severity, SkippedItem, Status, STATE_VERSION,
};
use crate::repo::Repo;
use crate::style::{self, Style};
use crate::{bail, log, logdim, logwarn, schema, spar_err};

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

const IMPLEMENT_PROMPT: &str = "\
Implement GitHub issue #{number} in this repository.

Title: {title}
URL: {url}

{body}

That is the issue body as filed. The discussion since is not included, so read
the thread at the URL above if the body leaves anything open. If you cannot
reach the network, work from what is here.

Do the work and run the relevant tests. Leave the changes uncommitted. Do not
commit, push, open a PR, or merge; the harness validates and commits the working
tree after your report is accepted.

Then report it. Your answer becomes the pull request description, and the
reviewer reads that cold, with nothing but the diff and a link to the issue:
say what you found wrong, what the change does about it, and how they confirm
it for themselves. Say what you actually ran, not what could be run.

If after reading the code you conclude this issue should not be implemented,
make no changes and set not_worth_doing, with the reason.";

const REVIEW_PROMPT: &str = "\
Review the changes on this branch against `{base}`. They implement issue
#{number}: {title}

Review thoroughly: correctness, edge cases, error handling, security, and
whether the change actually resolves the issue. Read surrounding code, do not
only read the diff.

Label every finding by severity, and be honest about which is which:
- blocking: the PR should not merge as is. Real defects only.
- non-blocking: real, and smaller than another round. A minor defect belongs
  here as much as an improvement does.
- nit: style or taste.

Blocking is the only severity that costs a round, and a round is another commit
somebody has to read before this can merge. Being right that something is wrong
is not enough to block. It has to be wrong in a way that would cost somebody.

Confirm anything you label blocking before you label it. Run the code,
reproduce the failure, or point at the exact line that breaks, and say in the
detail what you did to confirm it. When you need to run something to check a
claim, write a scratch file under the system temporary directory and run that,
rather than passing a long program on the command line: it is easier to read
back, easier to rerun, and less likely to be refused by a sandbox or a safety
filter part way through your work. An unverified blocking finding is worse than
one you never raised: it stalls a good PR and teaches the author to stop
believing you. If you suspect a problem but could not confirm it, say so and
label it non-blocking.

Set in_scope=false for a real defect that exists, that this PR did not cause, and
that is worth somebody stopping to fix. Each one becomes a tracked item a
maintainer has to read and triage, so the bar is a defect and not an observation.
A thorough reviewer can always find something adjacent to what it is reading;
that is not a reason to file it. If you are not sure it is worth a maintainer's
time, say your piece in the finding and label it non-blocking.

Reviewing one issue should not manufacture ten more. If you find yourself with
several out of scope findings, keep the ones that would bite somebody and drop
the rest.

Then choose next_action:
- merge: no blocking findings, the PR is good.
- fix_myself: there are blocking findings and you will fix them directly.
- hand_back: there are blocking findings the author should address.
{open}{answers}{settled}{round}";

const CLOSE_PROMPT: &str = "\
This closes the review of issue #{number}: {title}

This is the final merge-safety audit. Read the full branch against `{base}` and
answer one question: does the branch still contain anything that must not
merge. Do not spend this pass on optional improvements or style.
{landed}{open}{answers}{settled}
Something blocks here when the branch still contains a confirmed defect that
means it should not merge. That includes an open point above that the code does
not answer, a defect in what landed since the last round, or a serious defect an
earlier round missed. Go and look before you raise it. Run the test that covers
it, or read the relevant lines and follow them to the caller, and say in the
detail what you did.

Keep this pass focused on merge safety. Minor defects and improvements are
non-blocking. Keep in_scope=false for what it has always meant, a real defect
this pull request did not cause, which the harness handles according to the
configured follow-up policy. A confirmed in-scope defect does not become
non-blocking merely because an earlier round missed it.

Nothing you raise here will be fixed, because there is no round after this. A
blocking finding means the pull request stays open and the finding is reported
for a person to weigh. Non-blocking findings are reported without holding it
open. No blocking findings means the branch is signed off on your word, so do
not omit one because the list was long. Both mistakes cost somebody. Only one
of them ships.

Set next_action to merge when you raise nothing blocking, and hand_back when you
do. Nothing acts on it here, and the findings are what decide.

This call reads and nothing else. Do not edit the code, do not commit, and do
not push. A pass that writes has judged a branch the rollback then takes away,
so anything you leave behind is rolled back and the sign off does not stand.
Put any scratch file under the system temporary directory, not in the working
tree.";

const FIX_PROMPT: &str = "\
You reviewed this branch and chose to fix the blocking findings yourself.
Implement those fixes now. Leave the changes uncommitted so the harness can
validate and commit them.

Your findings:
{findings}

Fix what the point says and nothing else. The smallest change that answers it is
the right one: no refactor alongside it, no capability nobody asked for, no
handling for cases nobody raised. Every line you add is what the next pass
reviews, so a fix that grows the branch buys another round of findings about the
fix. If a point cannot be answered without a change bigger than the point, say so
rather than making the change.

Do not commit, push, or merge.";

const RESPOND_PROMPT: &str = "\
Here is a review of your PR for issue #{number}.

{findings}

For each point, choose exactly one disposition:
- fixed: the point is valid and in scope. Fix it and leave the change
  uncommitted for the harness.
- refuted: the point is wrong, or the change it asks for is bigger than the
  problem it names. Explain why. Refuting is a legitimate outcome; do not accept
  a review comment you believe is incorrect just to get the PR approved.
- filed_issue: the point is valid but unrelated to this PR. Supply
  new_issue_title and new_issue_body; the harness files it and skips duplicates.

Copy each finding's title and file across exactly as given, so your answer can
be matched back to the review. Give a reason for every disposition. For fixed,
say what changed and how it answers the point. For refuted, say why the point
does not stand. For filed_issue, say why it belongs outside this pull request.

Fix what the point says and nothing else. The smallest change that answers it is
the right one: no refactor alongside it, no capability nobody asked for, no
handling for cases nobody raised. Every line you add is what the next pass
reviews, so a fix that grows the branch buys another round of findings about the
fix. If a point cannot be answered without a change bigger than the point, say so
rather than making the change.

Leave any fixes uncommitted. Do not commit, push, or merge.";

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// What the branch looked like at one point in a round.
///
/// Untracked files are deliberately not dirt. The review prompt asks for a
/// scratch file when a claim needs running to check it, so counting one as a
/// mutation would reject every review that did as it was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub head: String,
    /// Tracked files differing from the index or the head.
    pub dirty: bool,
}

impl Snapshot {
    /// Whether a commit landed between the two. An empty head means git could
    /// not be read, which is not evidence that anything was written.
    pub fn landed_over(&self, before: &Snapshot) -> bool {
        !self.head.is_empty() && self.head != before.head
    }
}

pub fn snapshot(repo: &Repo, work_dir: &Path) -> Snapshot {
    Snapshot {
        head: repo
            .git_try_at(Some(work_dir), &["rev-parse", "HEAD"])
            .trim()
            .to_string(),
        dirty: !repo
            .git_try_at(
                Some(work_dir),
                &["status", "--porcelain", "--untracked-files=no"],
            )
            .trim()
            .is_empty(),
    }
}

fn checked_head(repo: &Repo, work_dir: &Path) -> Result<String> {
    let head = repo.git_at(Some(work_dir), &["rev-parse", "HEAD"])?;
    let head = head.trim().to_string();
    if head.is_empty() {
        return Err(spar_err!("could not read the pull request head"));
    }
    Ok(head)
}

/// Whether branch-dependent review state can be applied to the checked-out PR.
///
/// A missing checkpoint is a fresh run. A saved checkpoint must name this exact
/// published head; otherwise automatic custody would let the previous reviewer
/// read a commit it may have written. An explicit override supplies the human
/// decision and starts branch-dependent state fresh.
fn reconcile_saved_head(
    recorded_head: Option<&str>,
    actual_head: &str,
    holder_override: Option<&str>,
    pr_number: i64,
) -> Result<bool> {
    let Some(recorded_head) = recorded_head else {
        return Ok(true);
    };
    if !recorded_head.is_empty() && recorded_head == actual_head {
        return Ok(true);
    }
    if holder_override.is_some() {
        return Ok(false);
    }
    let recorded = if recorded_head.is_empty() {
        "legacy or unknown"
    } else {
        recorded_head
    };
    Err(spar_err!(
        "saved review state applies to head {recorded}, but PR #{pr_number} is at {actual_head}; \
         resume with --next <agent> to choose who reviews this head"
    ))
}

/// Copy the tracked edits in the tree to somewhere they can be got back from.
///
/// `git stash create` writes them as a dangling commit and, unlike `git stash
/// push`, leaves the stash stack alone: the stack belongs to whoever is working
/// in the repository, and every worktree of it shares the same one. `None` when
/// there was nothing to save.
pub fn park(repo: &Repo, work_dir: &Path) -> Option<String> {
    let saved = repo
        .git_try_at(Some(work_dir), &["stash", "create"])
        .trim()
        .to_string();
    (!saved.is_empty()).then_some(saved)
}

/// Reset the tree to `target`, saving what that throws away.
///
/// With `--no-worktrees` the checkout is the user's own, and nothing here can
/// tell an edit an agent left behind from one a person made while a call was
/// running. So the discard is never silent and never final: the changes are
/// parked first and the log says how to put them back.
fn reset_saving(repo: &Repo, work_dir: &Path, target: &str) {
    let parked = park(repo, work_dir);
    if let Err(e) = repo.git_at(Some(work_dir), &["reset", "--hard", target]) {
        logdim!("could not roll the working tree back: {e}");
        return;
    }
    if let Some(saved) = parked {
        logdim!("`git stash apply {saved}` puts the discarded changes back");
    }
}

/// Put the branch back where the review found it.
///
/// Nothing here was ever pushed: the loop pushes at the end of a round, so the
/// head a review starts from is the head the pull request already has. What is
/// discarded is therefore only what the review wrote after being told not to,
/// and keeping it would hand the reviewer its own commit to review next round.
///
/// Returns the state afterwards, which equals `before` when the rollback took.
/// The caller compares, because a rollback that did not take means the reviewer
/// wrote the head and custody has to follow it there.
pub fn undo_edits(repo: &Repo, work_dir: &Path, before: &Snapshot) -> Snapshot {
    let current = snapshot(repo, work_dir);
    if before.head.is_empty() {
        return current;
    }
    if current.landed_over(before) {
        logdim!(
            "the commits being rolled back are still at {}",
            current.head
        );
    }
    reset_saving(repo, work_dir, &before.head);
    snapshot(repo, work_dir)
}

/// Keep a prohibited closing commit reachable without publishing it.
///
/// A later resume rebuilds the worktree from the pull request branch. The ref
/// preserves the local commit for inspection while keeping custody based on
/// the unchanged remote head.
fn preserve_closing_commit(repo: &Repo, ctx: &LoopCtx) -> Option<String> {
    let current = snapshot(repo, &ctx.work_dir);
    if current.head.is_empty() {
        return None;
    }
    let reference = format!(
        "refs/spar/recovery/pr-{}/closing-{}",
        ctx.pr_number, current.head
    );
    match repo.git_at(
        Some(&ctx.work_dir),
        &["update-ref", &reference, &current.head],
    ) {
        Ok(_) => Some(reference),
        Err(e) => {
            logdim!("could not preserve the closing pass commit: {e}");
            None
        }
    }
}

/// Drop what a call left uncommitted, keeping whatever it committed.
///
/// Only commits reach the pull request, but the next review reads the working
/// tree, so an edit left behind is code the reviewer judges and the diff does
/// not have. That is how an agent comes to approve a fix of its own that
/// nobody else can see.
pub fn drop_uncommitted(repo: &Repo, work_dir: &Path) -> Snapshot {
    let current = snapshot(repo, work_dir);
    if !current.dirty || current.head.is_empty() {
        return current;
    }
    reset_saving(repo, work_dir, &current.head);
    snapshot(repo, work_dir)
}

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

fn uncommitted_implementation_error(work_dir: &Path, detail: Option<&str>) -> SparError {
    let detail = detail.unwrap_or_default().trim();
    let prefix = if detail.is_empty() {
        String::new()
    } else {
        format!("{detail}\n")
    };
    spar_err!(
        "{prefix}The implementation left uncommitted changes in {}. Commit or recover them \
         before running this issue again.",
        work_dir.display()
    )
}

fn commit_accepted_changes(
    cfg: &Config,
    repo: &Repo,
    work_dir: &Path,
    baseline: &crate::repo::WorktreeBaseline,
    preferred_subject: &str,
    fallback_subject: &str,
) -> Result<bool> {
    repo.refuse_changed_attributes(work_dir, baseline)?;
    if !cfg.loop_cfg.worktrees && repo.has_uncommitted_changes(work_dir)? {
        bail!(
            "the implementation changed the shared checkout at {}, but spar cannot distinguish \
             those files from edits made concurrently by its owner. The files were kept. Commit \
             or recover them, then use the default worktree mode for managed commits.",
            work_dir.display()
        );
    }
    let committed =
        repo.commit_pending_changes(work_dir, baseline, preferred_subject, fallback_subject)?;
    repo.refuse_unrepresented_tracked_changes(work_dir, baseline)?;
    Ok(committed)
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
) -> IssueRun {
    // Continue an existing PR rather than implementing over the top of it.
    //
    // Without this, a second `spar run 42` deletes the local branch, rebuilds
    // it from the base, implements from scratch, and force pushes. The lease
    // holds because the remote tracking ref survives the local branch being
    // deleted, so the push succeeds and the previous round's work is gone from
    // the PR with nothing to say it ever existed.
    if let Some(existing) = repo.open_pr_for_issue(item.issue) {
        log!(
            "#{}: {} is already open, continuing it instead of implementing again",
            item.issue,
            existing.url
        );
        return resume_pr(agents, cfg, repo, existing.number, None);
    }

    let mut state = IssueRun::new(item.issue, item.title.clone());
    // One ledger per pull request. It was one per invocation, held by
    // `work_issues` and lent to every issue in the run, so the second issue was
    // handed the first one's points and told to treat as settled a defect in a
    // file its own branch does not touch. Both state files on this repository
    // record it: `pr-34.json` carries `pr-33.json`'s two `src/tracker.rs`
    // entries, on a branch with no tracker in it.
    let mut ledger = Ledger::new();
    let base = cfg.base_branch().to_string();

    let prepared = if cfg.loop_cfg.worktrees {
        repo.worktree_add(item.issue, &base)
    } else {
        (|| {
            if repo.has_uncommitted_changes(repo.root())? {
                bail!(
                    "the shared checkout at {} has uncommitted changes. Refusing to reset it for \
                     issue #{}.",
                    repo.root().display(),
                    item.issue
                );
            }
            if repo.has_changes_checked(repo.root(), &base)? {
                let preserved = repo.current_branch_is_preserved(repo.root()).map_err(|e| {
                    spar_err!(
                        "could not verify whether a pull request preserves the shared checkout \
                             at {}: {}. Refusing to reset it for issue #{}.",
                        repo.root().display(),
                        e.last_line(),
                        item.issue
                    )
                })?;
                if !preserved {
                    bail!(
                        "the shared checkout at {} has commits that are not on {base}. Refusing \
                         to reset them for issue #{}.",
                        repo.root().display(),
                        item.issue
                    );
                }
            }
            let branch = repo.branch_for_issue(item.issue);
            let start = format!("origin/{base}");
            repo.refuse_issue_branch_rebuild(item.issue, &base)?;
            if repo.has_uncommitted_changes(repo.root())? {
                bail!(
                    "the shared checkout at {} changed while issue #{} was being prepared. \
                     Refusing to reset it.",
                    repo.root().display(),
                    item.issue
                );
            }
            repo.git(&["checkout", "-B", &branch, &start])?;
            repo.record_branch(&branch, "issue", item.issue);
            Ok((repo.root().to_path_buf(), branch))
        })()
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
        agents,
        cfg,
        repo,
        item,
        issue,
        &mut ledger,
        &mut state,
        &work_dir,
        &branch,
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
    // Fixing triage alone would have been worse than fixing neither: an issue
    // correctly judged worth doing on its whole text, then built from the first
    // few thousand characters of it, raises confidence without raising
    // fidelity.
    let (body, shortened) = issue.body_for_prompt(cfg.loop_cfg.max_issue_chars);
    if shortened {
        logwarn!(
            "#{number}: the issue body was shortened to fit the prompt. Raise max_issue_chars if \
             the rest matters."
        );
    }
    let prompt = implement_prompt(number, &item.title, &issue.url, &body);
    let worktree_baseline = repo.worktree_baseline(work_dir)?;
    let answer: Result<Implementation> = implementor.edit_json(
        &prompt,
        &schema::implementation(),
        work_dir,
        cfg.effort_for_round(&implementor.spec, 1).as_deref(),
    );

    if answer.is_ok() {
        repo.refuse_changed_attributes(work_dir, &worktree_baseline)?;
    }

    if let Err(e) = &answer {
        if e.kind() == crate::error::ErrorKind::UncertainWrite {
            return Err(e.clone());
        }
    }

    // A call that fails with commits on the branch is not the same as one that
    // fails with nothing to show. Custom editing commands can still commit
    // directly before their report fails. The review loop needs that retained
    // diff, not the missing summary.
    let has_commits = repo.has_changes_checked(work_dir, &base)?;
    let has_uncommitted = repo.has_uncommitted_changes(work_dir)?;
    let mut work = match answer {
        Err(e) if has_uncommitted => {
            return Err(uncommitted_implementation_error(
                work_dir,
                Some(e.message()),
            ));
        }
        Ok(work) => work,
        Err(e) if has_commits => {
            logwarn!(
                "#{number}: {holder} failed after committing: {e}\nContinuing from the commits, \
                 with a pull request body written from their messages."
            );
            state
                .notes
                .push(format!("{holder} failed after committing: {e}"));
            from_commits(repo, work_dir, &base)
        }
        Err(e) => return Err(e),
    };

    if work.not_worth_doing {
        repo.refuse_unrepresented_tracked_changes(work_dir, &worktree_baseline)?;
        repo.refuse_changed_existing_untracked(work_dir, &worktree_baseline)?;
        if has_uncommitted || has_commits {
            bail!(
                "{holder} declined issue #{number} after changing {}. The worktree was kept for \
                 recovery.",
                work_dir.display()
            );
        }
        repo.refuse_new_ignored_files(work_dir, &worktree_baseline)?;
        state.status = Status::Abandoned;
        let reason = no_pr_note(&work, &repo.style);
        state.notes.push(reason.clone());
        if let Err(e) = repo.comment_issue(number, &reason) {
            logdim!("could not comment on #{number}: {e}");
        }
        return Ok(());
    }

    commit_accepted_changes(
        cfg,
        repo,
        work_dir,
        &worktree_baseline,
        &work.summary,
        &item.title,
    )?;
    if repo.has_uncommitted_changes(work_dir)? {
        return Err(uncommitted_implementation_error(
            work_dir,
            work.notes.as_deref(),
        ));
    }
    if !repo.has_changes_checked(work_dir, &base)? {
        repo.refuse_new_ignored_files(work_dir, &worktree_baseline)?;
        state.status = Status::Abandoned;
        let reason = no_pr_note(&work, &repo.style);
        state.notes.push(reason.clone());
        if let Err(e) = repo.comment_issue(number, &reason) {
            logdim!("could not comment on #{number}: {e}");
        }
        return Ok(());
    }

    // A body that leads with nothing is a body nobody reads past. The issue
    // title is a poor substitute for a sentence about the change, and a better
    // one than a blank first line.
    if work.summary.trim().is_empty() {
        work.summary = item.title.clone();
    }

    repo.rewrite_commits_if_needed(work_dir, &base)?;
    repo.push(work_dir, branch)?;

    let pr = match repo.pr_for_branch(branch) {
        Some(existing) => existing,
        None => {
            let body = pr_body(number, &work, &repo.style);
            repo.create_pr(
                work_dir,
                branch,
                &base,
                &format!("{} (#{number})", item.title),
                &body,
            )?
        }
    };
    repo.record_branch(branch, "pr", pr.number);
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
    review_loop(agents, cfg, repo, &ctx, state, ledger, Vec::new())
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
    let failed = |e: SparError| {
        log!("PR #{pr_number} failed: {e}");
        let mut state = IssueRun::new(pr_number, format!("PR #{pr_number}"));
        state.status = Status::Error;
        state.notes.push(e.to_string());
        state
    };

    let pr = match repo.pr_view(pr_number) {
        Ok(pr) => pr,
        Err(e) => return failed(e),
    };

    // A pull request from a fork cannot be pushed to, so the loop that fixes
    // things cannot run on it. Reviewing it is still the useful thing, and it
    // is what a maintainer wants from an outside contribution anyway, so do
    // that rather than refusing.
    if pr.is_cross_repository {
        log!("PR #{pr_number} comes from a fork, reviewing it without changing it");
        return crate::review_only::review_pr(agents, cfg, repo, pr_number, false);
    }

    match resume_inner(agents, cfg, repo, pr, holder_override) {
        Ok(state) => state,
        Err(e) => failed(e),
    }
}

fn resume_inner(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr: PrView,
    holder_override: Option<&str>,
) -> Result<IssueRun> {
    let pr_number = pr.number;
    if !pr.is_open() {
        return Err(spar_err!("PR #{pr_number} is {}", pr.state.to_lowercase()));
    }

    let subject = pr
        .closing_issues_references
        .first()
        .map(|r| r.number)
        .unwrap_or(pr_number);

    if let Some(holder) = holder_override {
        if !cfg.has_agent(holder) {
            return Err(spar_err!(
                "--next must name one of: {}",
                cfg.agent_names().join(", ")
            ));
        }
    }
    let (work_dir, branch) = repo.worktree_for_pr(&pr)?;
    let actual_head = checked_head(repo, &work_dir)?;
    let saved = repo.read_state_for_head(&pr, &actual_head);
    let state_matches_head = reconcile_saved_head(
        saved.as_ref().map(|state| state.pr_head.as_str()),
        &actual_head,
        holder_override,
        pr_number,
    )?;

    let mut ledger: Ledger = saved
        .as_ref()
        .filter(|_| state_matches_head)
        .map(|s| s.ledger.clone())
        .unwrap_or_default();
    normalise_ledger_keys(&mut ledger);
    let open_findings = saved
        .as_ref()
        .filter(|_| state_matches_head)
        .map(|s| blocking_findings(&s.open_findings))
        .unwrap_or_default();
    let start_round = saved
        .as_ref()
        .filter(|_| state_matches_head)
        .map(|s| s.round + 1)
        .unwrap_or(1);

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
            "PR #{pr_number}: resuming at round {start_round}, {} point(s) on record, next up \
             {holder}",
            ledger.len()
        ),
        None => log!("PR #{pr_number}: no prior spar state, starting fresh with {holder}"),
    }

    let mut state = IssueRun::new(subject, pr.title.clone());
    state.pr = Some(pr.url.clone());
    if let Some(s) = &saved {
        state.filed = s.filed.clone();
        if state_matches_head {
            state.disputes = s.disputes.clone();
            state.noted = s.noted.clone();
        } else {
            log!(
                "PR #{pr_number}: saved state does not match {actual_head}; branch-dependent \
                 review state was cleared"
            );
        }
    }

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

    let outcome = review_loop(
        agents,
        cfg,
        repo,
        &ctx,
        &mut state,
        &mut ledger,
        open_findings,
    );
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
    fn release(&self, repo: &Repo) -> bool {
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
    mut open_findings: Vec<Finding>,
) -> Result<()> {
    let base = cfg.base_branch().to_string();
    // Never the agent that made the last commit, on entry and after every
    // round. An approval or a deadlock ends the round with nothing edited, so
    // those paths persist it unchanged.
    let mut holder = ctx.holder.clone();

    // `max_rounds` is a budget for this invocation, not a lifetime cap on the
    // pull request. Running spar again on a PR that already spent its rounds is
    // a deliberate act by a person who has looked at it, so it gets a fresh
    // budget rather than an error telling them to raise a number they cannot
    // see from the outside.
    let (first, last_allowed) = round_window(ctx.start_round, cfg.loop_cfg.max_rounds);
    let mut published_head = checked_head(repo, &ctx.work_dir)?;
    persist(
        repo,
        ctx.pr_number,
        state,
        ledger,
        &open_findings,
        &published_head,
        first.saturating_sub(1),
        &holder,
    )?;
    // The head the last review in this invocation read. Empty until one has
    // run, and in-invocation on purpose: a resumed run's closing pass reads what
    // this invocation's rounds produced, not what some earlier one did.
    let mut audited_head = String::new();
    let mut last_round = first.saturating_sub(1);

    for round in first..=last_allowed {
        last_round = round;
        state.rounds = round;
        let reviewer = agent::find(agents, &holder)?;
        let effort = cfg.effort_for_round(&reviewer.spec, round);
        log!(
            "{}: round {round}, {holder} reviewing ({})",
            ctx.label,
            effort.as_deref().unwrap_or("default effort")
        );

        let prompt = review_prompt(
            &base,
            ctx.subject,
            &ctx.title,
            ledger,
            &open_findings,
            round,
            last_allowed,
        );
        let before_review = snapshot(repo, &ctx.work_dir);
        let review_baseline = repo.worktree_baseline(&ctx.work_dir)?;
        // The commit this round is judging, kept for the closing pass, which
        // reads what landed after the last one of these.
        audited_head = before_review.head.clone();
        let review = reviewer.review::<Review>(
            &base,
            &prompt,
            &schema::review(),
            &ctx.work_dir,
            effort.as_deref(),
        );
        if let Err(error) = &review {
            if error.kind() == crate::error::ErrorKind::UncertainWrite {
                return Err(error.clone());
            }
        }
        repo.refuse_unrepresented_tracked_changes(&ctx.work_dir, &review_baseline)?;
        repo.refuse_new_ignored_files(&ctx.work_dir, &review_baseline)?;
        let review = review?;
        if repo.has_uncommitted_changes(&ctx.work_dir)? {
            bail!(
                "{}: {holder} left uncommitted files while reviewing. They were kept at {} and \
                 the review did not continue.",
                ctx.label,
                ctx.work_dir.display()
            );
        }

        // Who actually wrote the head this round, which is the only thing that
        // decides who reviews it next. None so far: a review is not supposed to
        // write anything.
        let mut editor: Option<String> = None;
        let review_wrote = snapshot(repo, &ctx.work_dir) != before_review;
        if review_wrote {
            logwarn!(
                "{}: {holder} changed the branch while reviewing it, which the review prompt \
                 forbids. Rolling it back.",
                ctx.label
            );
            if undo_edits(repo, &ctx.work_dir, &before_review).head != before_review.head {
                state
                    .notes
                    .push(format!("{holder} committed during its own review"));
                editor = Some(holder.clone());
            }
        }

        let blocking = blocking_findings(&review.findings);
        update_open_findings(&mut open_findings, &blocking, !review_wrote);

        if repo.style.pr_comments == PrComments::Rounds {
            if let Err(e) = repo.comment_pr(
                ctx.pr_number,
                &review_comment(&holder, round, &review, &repo.style),
            ) {
                logdim!("could not post the review comment: {e}");
            }
        }

        // Filed every round, not only on approval: a run that escalates or runs
        // out of rounds would otherwise drop these on the floor. Filing
        // deduplicates by title, so repeats across rounds are free.
        file_out_of_scope(repo, &review.findings, ctx.subject, state, cfg);
        file_nonblocking(repo, &review.findings, ctx.subject, state, cfg);
        remove_findings(&mut state.noted, &blocking);

        if check_relitigation(ledger, &blocking, state) {
            state.status = Status::Escalated;
            post_outcome(
                repo,
                ctx.pr_number,
                state,
                ledger,
                Ending::Deadlocked(&blocking),
            );
            persist(
                repo,
                ctx.pr_number,
                state,
                ledger,
                &open_findings,
                &published_head,
                round,
                &holder,
            )?;
            return Ok(());
        }

        if approval_stands(&blocking, review_wrote) {
            open_findings.clear();
            return approve(
                cfg,
                repo,
                ctx,
                state,
                ledger,
                &published_head,
                round,
                &holder,
            );
        }

        // Checkpoint the review before any fixer, responder, rewrite, or push
        // can fail. A confirmed blocker must survive those failures.
        persist(
            repo,
            ctx.pr_number,
            state,
            ledger,
            &open_findings,
            &published_head,
            round,
            &holder,
        )?;

        let mut edit_error = None;

        if blocking.is_empty() {
            // Nothing blocking, but the branch it said that about is not the
            // branch that is there now. Falling through gives the next round
            // whatever the rollback left: the same reviewer when it took, the
            // other agent when the review's commit survived it.
            logwarn!(
                "{}: {holder} found nothing blocking on a branch it had changed itself, so the \
                 approval does not carry.",
                ctx.label
            );
            state.notes.push(format!(
                "{holder} passed the branch in round {round} after editing it; the edit was rolled \
                 back and the approval did not stand"
            ));
        } else if review.next_action == NextAction::FixMyself {
            log!("{}: {holder} fixing its own findings", ctx.label);
            let prompt = FIX_PROMPT.replace("{findings}", &findings_for_prompt(&blocking));
            let before_fix = repo.head_oid_checked(&ctx.work_dir)?;
            let worktree_baseline = repo.worktree_baseline(&ctx.work_dir)?;
            let fix_error = match reviewer.edit(&prompt, &ctx.work_dir, effort.as_deref()) {
                Ok(summary) => {
                    commit_accepted_changes(
                        cfg,
                        repo,
                        &ctx.work_dir,
                        &worktree_baseline,
                        &summary,
                        "Address blocking review findings",
                    )?;
                    None
                }
                Err(error) => Some(defer_clean_edit_error(
                    repo,
                    &ctx.work_dir,
                    &worktree_baseline,
                    error,
                )?),
            };
            match editor_after(repo, &ctx.work_dir, &before_fix, &ctx.label, &holder)? {
                Some(who) => {
                    // Recorded like an author's fix, and for the same reason.
                    // These points were answered in code too, and leaving them
                    // out left this path with the hole the other one had: the
                    // next pass reads a fix with nothing saying it was asked
                    // for, and the guard that ends an argument cannot count it.
                    // The reviewer wrote both the finding and the fix, so its
                    // own detail is the claim.
                    record_own_fixes(&blocking, ledger, state, round);
                    remove_findings(&mut open_findings, &blocking);
                    editor = Some(who);
                }
                None if fix_error.is_none() => {
                    repo.refuse_new_ignored_files(&ctx.work_dir, &worktree_baseline)?;
                    // Handing over here is what the bug was: the head is still
                    // the author's, so the author would be reading its own work.
                    logwarn!(
                        "{}: {holder} said it would fix its own findings and committed nothing, \
                         so it keeps the pull request.",
                        ctx.label
                    );
                    state.notes.push(format!(
                        "{holder} chose to fix its own findings in round {round} and committed \
                         nothing"
                    ));
                }
                None => {}
            }
            edit_error = fix_error;
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
            let before_response = repo.head_oid_checked(&ctx.work_dir)?;
            let worktree_baseline = repo.worktree_baseline(&ctx.work_dir)?;
            let response: Result<ResponseDoc> = author.edit_json(
                &prompt,
                &schema::response(),
                &ctx.work_dir,
                cfg.effort_for_round(&author.spec, round).as_deref(),
            );
            let response = match response {
                Ok(response) => {
                    commit_accepted_changes(
                        cfg,
                        repo,
                        &ctx.work_dir,
                        &worktree_baseline,
                        &response.summary,
                        "Address blocking review findings",
                    )?;
                    Some(response)
                }
                Err(error) => {
                    edit_error = Some(defer_clean_edit_error(
                        repo,
                        &ctx.work_dir,
                        &worktree_baseline,
                        error,
                    )?);
                    None
                }
            };
            if let Some(who) = editor_after(
                repo,
                &ctx.work_dir,
                &before_response,
                &ctx.label,
                &author_name,
            )? {
                editor = Some(who);
            } else if let Some(response) = &response {
                repo.refuse_new_ignored_files(&ctx.work_dir, &worktree_baseline)?;
                if response
                    .dispositions
                    .iter()
                    .any(|d| d.action == Action::Fixed)
                {
                    logwarn!(
                        "{}: {author_name} reported fixes but committed nothing, so the diff does \
                         not have them.",
                        ctx.label
                    );
                }
            }
            if let Some(response) = response {
                let unresolved = apply_dispositions(
                    repo,
                    cfg,
                    &response,
                    &blocking,
                    ledger,
                    state,
                    round,
                    ctx.subject,
                    ctx.pr_number,
                    &author_name,
                    editor.is_some(),
                );
                remove_findings(&mut open_findings, &blocking);
                extend_findings(&mut open_findings, &unresolved);
            }
        }

        if editor.is_some() {
            repo.rewrite_commits_if_needed(&ctx.work_dir, &base)?;
            repo.push(&ctx.work_dir, &ctx.branch)?;
            published_head = checked_head(repo, &ctx.work_dir)?;
        }
        holder = next_reviewer(cfg, &holder, editor.as_deref());
        persist(
            repo,
            ctx.pr_number,
            state,
            ledger,
            &open_findings,
            &published_head,
            round,
            &holder,
        )?;
        if let Some(error) = edit_error {
            return Err(error);
        }
    }

    // Falling out of the budget is not an outcome. Every path above returns
    // with the head already read by somebody who did not write it; this is the
    // one that does not, because a round is review and then fix and the fix
    // comes last. Leaving it as the ending is what made every long run finish
    // on a commit nobody had seen, and made "we stopped" the only thing spar
    // could say about a pull request it had spent an hour on.
    close_out(
        agents,
        cfg,
        repo,
        ctx,
        state,
        ledger,
        &mut open_findings,
        &holder,
        last_round,
        &audited_head,
        &published_head,
    )
}

/// The closing pass: one look at what the last round left, and the verdict.
///
/// Not a round. It cannot ask for a fix, there is nothing after it, and it is
/// the only way a run that spends its whole budget ends in an approval. Kept out
/// of the `for` so every round keeps one shape, and the call that behaves
/// differently is the one with a different name.
///
/// It inherits PR #24's invariant from the same place the rounds do, and not
/// from a rule of its own: the closer is `holder`, which `next_reviewer` has
/// already moved off whoever wrote the head.
#[allow(clippy::too_many_arguments)]
fn close_out(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    ctx: &LoopCtx,
    state: &mut IssueRun,
    ledger: &mut Ledger,
    open_findings: &mut Vec<Finding>,
    holder: &str,
    round: u32,
    audited_head: &str,
    published_head: &str,
) -> Result<()> {
    let stop =
        |state: &mut IssueRun, ledger: &Ledger, open_findings: &[Finding], ending: Ending<'_>| {
            state.status = Status::Escalated;
            state.notes.push(exhausted_note(ctx.start_round, round));
            post_outcome(repo, ctx.pr_number, state, ledger, ending);
            persist(
                repo,
                ctx.pr_number,
                state,
                ledger,
                open_findings,
                published_head,
                round,
                holder,
            )
        };

    // Nothing to close over. The last round changed no code and claimed no fix,
    // so the branch in front of the closer is the branch a round already read at
    // full breadth, and one more call over it buys nothing. An empty head is the
    // same answer for a different reason: git could not be read, so there is no
    // range to hand the pass. Either way it ends on its own sentence rather than
    // the one about unread fixes, because on this path there are none.
    let landed = (!audited_head.is_empty())
        .then(|| repo.commits_since(&ctx.work_dir, audited_head, "HEAD"))
        .flatten();
    if audited_head.is_empty()
        || (landed.as_ref().is_some_and(|l| l.is_empty()) && !any_fixes(ledger, round))
    {
        logdim!(
            "{}: nothing landed after the last review, so there is nothing to close over",
            ctx.label
        );
        if !open_findings.is_empty() {
            state.notes.push(unresolved_note(open_findings.len()));
        }
        stop(
            state,
            ledger,
            open_findings,
            ending_without_landing(open_findings),
        )?;
        return Ok(());
    }

    let closer = agent::find(agents, holder)?;
    let effort = cfg.effort_for_round(&closer.spec, closing_effort_round(round));
    log!(
        "{}: closing, {holder} checking what the last round left ({})",
        ctx.label,
        effort.as_deref().unwrap_or("default effort")
    );

    let prompt = close_prompt(
        cfg.base_branch(),
        ctx.subject,
        &ctx.title,
        audited_head,
        landed.as_deref(),
        ledger,
        open_findings,
        round,
    );
    let before = snapshot(repo, &ctx.work_dir);
    let closing_baseline = repo.worktree_baseline(&ctx.work_dir)?;
    // `ask_json` rather than `Agent::review`: this prompt already defines the
    // full merge-safety scope and calls out the unread delta and carried points.
    // Appending a second scope would make the closing instructions compete.
    let pass = closer.ask_json(&prompt, &schema::review(), &ctx.work_dir, effort.as_deref());
    if let Err(e) = &pass {
        if e.kind() == crate::error::ErrorKind::UncertainWrite {
            return Err(e.clone());
        }
    }
    repo.refuse_unrepresented_tracked_changes(&ctx.work_dir, &closing_baseline)?;
    repo.refuse_new_ignored_files(&ctx.work_dir, &closing_baseline)?;
    if repo.has_uncommitted_changes(&ctx.work_dir)? {
        bail!(
            "{}: {holder} left uncommitted files during the closing pass. They were kept at {} \
             and the pass did not continue.",
            ctx.label,
            ctx.work_dir.display()
        );
    }

    // Held to the same rule as a review, for the same reason: a pass that judged
    // a tree the rollback then takes away judged code that is not there.
    let close_wrote = snapshot(repo, &ctx.work_dir) != before;
    // The closing pass never publishes code. If rollback fails, its prohibited
    // commit is kept under a recovery ref and the remote branch remains on the
    // head that `holder` is allowed to review on a later run.
    let next = closing_next_actor(holder);
    if close_wrote {
        if let Err(error) = &pass {
            logwarn!(
                "{}: the closing pass failed after changing the branch: {error}",
                ctx.label
            );
        }
        logwarn!(
            "{}: {holder} changed the branch while closing, which the prompt forbids. Rolling it \
             back.",
            ctx.label
        );
        let after_undo = undo_edits(repo, &ctx.work_dir, &before);
        if after_undo != before {
            state.notes.push(format!(
                "{holder}'s closing-pass changes could not be fully rolled back"
            ));
            if after_undo.head != before.head {
                if let Some(reference) = preserve_closing_commit(repo, ctx) {
                    state.notes.push(format!(
                        "the closing pass commit was not pushed and remains at {reference}"
                    ));
                }
            }
        }
        state.status = Status::Escalated;
        state.notes.push(format!(
            "{holder} edited the branch during the closing pass, so its answer did not stand"
        ));
        post_unread_outcome(repo, ctx.pr_number, state, ledger, open_findings);
        persist(
            repo,
            ctx.pr_number,
            state,
            ledger,
            open_findings,
            published_head,
            round,
            &next,
        )?;
        return Ok(());
    }

    let pass: Review = match pass {
        Ok(pass) => pass,
        Err(e) => {
            // Never propagated. The run has an account of itself by now, and
            // losing all of it to an unreachable model on the last call is worse
            // than ending where it would have ended before this existed.
            logwarn!("{}: the closing pass failed: {e}", ctx.label);
            state.status = Status::Escalated;
            state.notes.push(exhausted_note(ctx.start_round, round));
            post_unread_outcome(repo, ctx.pr_number, state, ledger, open_findings);
            persist(
                repo,
                ctx.pr_number,
                state,
                ledger,
                open_findings,
                published_head,
                round,
                holder,
            )?;
            return Ok(());
        }
    };

    file_out_of_scope(repo, &pass.findings, ctx.subject, state, cfg);
    file_nonblocking(repo, &pass.findings, ctx.subject, state, cfg);

    let blocking = blocking_findings(&pass.findings);
    remove_findings(&mut state.noted, &blocking);
    update_open_findings(open_findings, &blocking, true);

    if check_relitigation(ledger, &blocking, state) {
        state.status = Status::Escalated;
        post_outcome(
            repo,
            ctx.pr_number,
            state,
            ledger,
            Ending::Deadlocked(&blocking),
        );
        persist(
            repo,
            ctx.pr_number,
            state,
            ledger,
            open_findings,
            published_head,
            round,
            &next,
        )?;
        return Ok(());
    }

    if approval_stands(&blocking, false) {
        open_findings.clear();
        return approve(cfg, repo, ctx, state, ledger, published_head, round, &next);
    }

    state.status = Status::Escalated;
    state.notes.push(unresolved_note(open_findings.len()));
    post_outcome(
        repo,
        ctx.pr_number,
        state,
        ledger,
        Ending::Unresolved(open_findings),
    );
    persist(
        repo,
        ctx.pr_number,
        state,
        ledger,
        open_findings,
        published_head,
        round,
        &next,
    )?;
    Ok(())
}

/// Whether the last round left a claimed fix for the closing pass to ask about.
///
/// Scoped to the round rather than to the pull request. Asked over the whole
/// ledger, a resumed run with an old fix in it would never take the skip, and
/// the pass would be handed a branch nothing had changed.
fn any_fixes(ledger: &Ledger, round: u32) -> bool {
    ledger
        .values()
        .any(|e| e.outcome == Settled::Fixed && e.round >= round)
}

/// What the run says about itself when the closing pass did not sign off.
///
/// Not "no convergence after three rounds". A count of rounds is a fact about
/// spar, and what is left is a fact about the branch.
fn unresolved_note(left: usize) -> String {
    match left {
        1 => "one point left after the closing pass".to_string(),
        n => format!("{n} points left after the closing pass"),
    }
}

/// End the run on a pass: post, persist, leave draft, and merge if asked.
///
/// Extracted because the closing pass ends the same way a round does. Written
/// twice, the two would drift, and the half that drifted would be the one that
/// merges.
fn ensure_reviewed_head(pr_number: i64, reviewed_head: &str, live_head: &str) -> Result<()> {
    if live_head == reviewed_head {
        return Ok(());
    }
    Err(spar_err!(
        "PR #{pr_number} changed from {reviewed_head} to {live_head} after it was reviewed; refusing to approve or merge an unread head"
    ))
}

#[allow(clippy::too_many_arguments)]
fn approve(
    cfg: &Config,
    repo: &Repo,
    ctx: &LoopCtx,
    state: &mut IssueRun,
    ledger: &Ledger,
    published_head: &str,
    round: u32,
    holder: &str,
) -> Result<()> {
    let live_head = repo.pr_head_oid(ctx.pr_number)?;
    ensure_reviewed_head(ctx.pr_number, published_head, &live_head)?;
    state.status = Status::Approved;
    persist(
        repo,
        ctx.pr_number,
        state,
        ledger,
        &[],
        published_head,
        round,
        holder,
    )?;
    let live_head = repo.pr_head_oid(ctx.pr_number)?;
    ensure_reviewed_head(ctx.pr_number, published_head, &live_head)?;
    post_outcome(repo, ctx.pr_number, state, ledger, Ending::Approved);
    // Before the merge, not after: a draft cannot be merged, and the state the
    // draft was signalling, that two agents were still arguing about it, has
    // just stopped being true.
    if cfg.loop_cfg.drafts == Drafts::UntilApproved && repo.mark_ready(ctx.pr_number) {
        log!("{}: out of draft", ctx.label);
    }
    if cfg.loop_cfg.auto_merge {
        // Release the worktree first. `gh pr merge --delete-branch` fails if
        // anything still has the branch checked out, and it fails *after*
        // merging, so the merge lands while the command reports failure.
        let released = ctx.release(repo);
        if !released {
            log!(
                "{}: kept the worktree at {} and will leave its branch in place",
                ctx.label,
                ctx.work_dir.display()
            );
        }
        repo.merge_pr_at_head(ctx.pr_number, published_head, released)?;
        state.status = Status::Merged;
        repo.clear_state(ctx.pr_number); // nothing left to resume
        log!("{}: merged", ctx.label);
    } else {
        log!("{}: approved, awaiting human merge", ctx.label);
    }
    Ok(())
}

/// Whether a review with nothing blocking can end the run.
///
/// A review that wrote to the branch judged a tree the rollback then takes
/// away, so "nothing blocking" was said about code that is not there any more:
/// a reviewer that quietly fixes what it finds and reports clean would merge
/// the defect it fixed. Another round on the restored branch is cheaper than
/// that.
fn approval_stands(blocking: &[Finding], review_wrote: bool) -> bool {
    blocking.is_empty() && !review_wrote
}

/// Who reviews the next round: never the agent that wrote the head it will
/// read.
///
/// `editor` is whoever moved HEAD this round, observed rather than inferred
/// from `next_action`. The two came apart in both directions: a `fix_myself`
/// call that returned without committing handed the author its own commit back,
/// and a reviewer that committed during `hand_back` kept a PR whose head it had
/// written.
///
/// Nothing landing at all leaves the head with the author, which by this rule's
/// own invariant is not the reviewer, so the reviewer keeps the pull request and
/// reads the same commit again.
fn next_reviewer(cfg: &Config, reviewer: &str, editor: Option<&str>) -> String {
    match editor {
        Some(editor) => cfg.other(editor),
        None => reviewer.to_string(),
    }
}

fn defer_clean_edit_error(
    repo: &Repo,
    work_dir: &Path,
    baseline: &crate::repo::WorktreeBaseline,
    error: SparError,
) -> Result<SparError> {
    if error.kind() == ErrorKind::UncertainWrite {
        return Err(error);
    }
    repo.refuse_changed_attributes(work_dir, baseline)?;
    if repo.has_uncommitted_changes(work_dir)? {
        return Err(error);
    }
    repo.refuse_new_ignored_files(work_dir, baseline)?;
    repo.refuse_unrepresented_tracked_changes(work_dir, baseline)?;
    Ok(error)
}

/// Who wrote the head after a call that was asked to commit, if anybody did.
///
/// A call that returns successfully is not evidence of a commit, and custody is
/// decided on this answer, so it is read from git rather than taken from the
/// agent's word for it. Anything it left uncommitted goes the same way as a
/// review's edits, and for the same reason: the round it hands over is the diff
/// on the branch, not the state of somebody's checkout.
fn editor_after(
    repo: &Repo,
    work_dir: &Path,
    before_head: &str,
    label: &str,
    who: &str,
) -> Result<Option<String>> {
    if repo.has_uncommitted_changes(work_dir)? {
        bail!(
            "{label}: {who} left uncommitted files at {}. They were kept and the review did not \
             continue.",
            work_dir.display()
        );
    }
    let after_head = repo.head_oid_checked(work_dir)?;
    if after_head != before_head && !repo.is_ancestor_checked(work_dir, before_head, &after_head)? {
        bail!(
            "{label}: {who} moved HEAD to {after_head}, but it does not contain the previous \
             branch tip {before_head}. The worktree was kept for recovery and nothing was \
             published. Restore {before_head} or reapply the intended commits on top of it before \
             resuming."
        );
    }
    Ok((after_head != before_head).then(|| who.to_string()))
}

/// The inclusive range of round numbers this invocation will work through.
///
/// Round numbers keep counting up across sessions so the ledger and the PR
/// history stay coherent, while the budget resets each time a person chooses to
/// run spar again.
fn round_window(start_round: u32, budget: u32) -> (u32, u32) {
    (start_round, start_round + budget.saturating_sub(1))
}

/// How many rounds this invocation spent, and how many the PR has seen in
/// total. A resumed PR that stops at round 8 did not have 8 rounds of budget,
/// and saying so would misreport both the cost and the history.
fn spent(start_round: u32, last_round: u32) -> (u32, u32) {
    (last_round.saturating_sub(start_round) + 1, last_round)
}

fn exhausted_note(start_round: u32, last_round: u32) -> String {
    let (this_run, total) = spent(start_round, last_round);
    if this_run == total {
        format!("no convergence after {this_run} rounds")
    } else {
        format!("no convergence after {this_run} more rounds ({total} in total)")
    }
}

#[allow(clippy::too_many_arguments)]
fn persist(
    repo: &Repo,
    pr_number: i64,
    state: &IssueRun,
    ledger: &Ledger,
    open_findings: &[Finding],
    published_head: &str,
    round: u32,
    next_actor: &str,
) -> Result<()> {
    let payload = PersistedState {
        version: STATE_VERSION,
        checkpoint: 0,
        round,
        next_actor: next_actor.to_string(),
        status: state.status,
        pr_head: published_head.to_string(),
        ledger: ledger.clone(),
        filed: state.filed.clone(),
        open_findings: open_findings.to_vec(),
        disputes: state.disputes.clone(),
        noted: state.noted.clone(),
    };
    repo.write_state(pr_number, &payload)
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

fn settled_block(ledger: &Ledger) -> String {
    if ledger.is_empty() {
        return String::new();
    }
    // A fixed point has no line here. The code changed for it, which is the
    // opposite of what this block says, and it goes in the answers block
    // instead, where it reads as a claim to check rather than an argument
    // already won.
    let lines: Vec<String> = ledger
        .values()
        .filter_map(|e| {
            let point = match e.file.trim() {
                "" => e.title.clone(),
                file => format!("{} ({file})", e.title),
            };
            match e.outcome {
                Settled::Refuted => Some(format!("- {point}: refuted because {}", e.reasoning)),
                Settled::Filed => Some(format!(
                    "- {point}: out of scope here, and filed. {}",
                    e.reasoning
                )),
                Settled::Dropped => Some(format!(
                    "- {point}: out of scope here, and not filed. {}",
                    e.reasoning
                )),
                Settled::Fixed => None,
            }
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\nThe following points were already raised and settled, by a refutation or by a \
         follow-up issue. Treat them as settled. Do not raise them again unless you have new \
         evidence:\n{}",
        lines.join("\n")
    )
}

/// The points the author says it fixed, one line each, with the claim attached.
///
/// One formatter for the two blocks that print them, because two copies of a
/// list are two copies to drift.
///
/// `since` is what keeps the list from growing without end. A fix is a claim for
/// whoever reads the branch next, and once that pass has read it and not raised
/// it again, it has been checked. Carrying every fix a pull request ever saw
/// would put a resumed run's tenth round in front of nine rounds of answered
/// points, which is the same unbounded surface this whole change exists to
/// bound.
fn fixed_lines(ledger: &Ledger, since: u32) -> Vec<String> {
    ledger
        .values()
        .filter(|e| e.outcome == Settled::Fixed && e.round >= since)
        .map(|e| {
            let claim = match e.reasoning.trim() {
                "" => "a committed change claims to address this point",
                reasoning => reasoning,
            };
            match e.file.trim() {
                "" => format!("- {}. Recorded answer: {claim}", e.title),
                file => format!("- {} ({file}). Recorded answer: {claim}", e.title),
            }
        })
        .collect()
}

/// What a later round is told about the fixes it asked for.
///
/// The ledger used to hold only the points the reviewer lost, so a round that
/// fixed nine findings left nothing behind and the next round met the fix as
/// ordinary code. Rendered apart from the settled block on purpose: a settled
/// point is an argument to weigh, a fixed point is a claim to check, and printed
/// under one heading a claim to check reads as an argument already won.
fn answers_block(ledger: &Ledger, round: u32) -> String {
    // The round before this one: what the pass this reviewer is following up on
    // asked for, and got.
    let lines = fixed_lines(ledger, round.saturating_sub(1));
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\nThese points were raised on this pull request in earlier rounds and the author says \
         it fixed them. The code is on the branch and the claim is the author's:\n{}\n\nCheck the \
         answer rather than taking it. If one of them is still not fixed, raise it again under \
         the same title, so the run can tell a point that was not answered from a new one.\n",
        lines.join("\n")
    )
}

/// What the reviewer is told about where in the run it is.
///
/// Empty until the last round that can ask for anything, where it says one thing
/// the prompt could not say before: when the asking stops. The deadline holds
/// whether or not the reviewer is told, so saying it out loud only lets the
/// reviewer spend the round it has. It says nothing about severity, because a
/// reviewer that lowers its bar to finish is the failure this loop was built
/// against.
fn round_note(round: u32, last: u32) -> String {
    if round < last {
        return String::new();
    }
    "\nThis is the last round in this run that can ask the author for anything. After it, one \
     pass reads what landed and the pull request is either signed off or goes to a person with \
     what is left. Raise everything you mean to raise now. A point held back for a later round \
     does not get one.\n"
        .to_string()
}

/// Record a point as settled, keeping any re-raise count it already carries.
/// Answering the same point a second time does not reset the argument, and
/// zeroing the count here would put the escalation guard out of reach: the
/// count is spent every round and rebuilt from nothing every round.
#[cfg(test)]
fn matching_ledger_key(ledger: &Ledger, title: &str, file: &str) -> Option<String> {
    matching_ledger_key_with_fallback(ledger, title, file, true)
}

fn matching_ledger_key_with_fallback(
    ledger: &Ledger,
    title: &str,
    file: &str,
    allow_stable_fallback: bool,
) -> Option<String> {
    let exact = finding_key(title, file);
    if ledger.contains_key(&exact) {
        return Some(exact);
    }
    let legacy = crate::jsonx::finding_key(title, file);
    if ledger
        .get(&legacy)
        .is_some_and(|entry| same_finding_parts(&entry.title, &entry.file, title, file))
    {
        return Some(legacy);
    }
    if !allow_stable_fallback {
        return None;
    }

    let stable = stable_finding_key(title, file);
    let path = finding_file(file);
    let mut matches = ledger
        .iter()
        .filter(|(saved_key, entry)| {
            if stable_finding_key(&entry.title, &entry.file) == stable {
                return true;
            }
            finding_file(&entry.file) == path
                && saved_key.as_str() == crate::jsonx::finding_key(title, &entry.file)
        })
        .map(|(key, _)| key.clone());
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn matching_ledger_entry<'a>(
    ledger: &'a Ledger,
    title: &str,
    file: &str,
) -> Option<&'a LedgerEntry> {
    matching_ledger_entry_with_fallback(ledger, title, file, true)
}

fn matching_ledger_entry_with_fallback<'a>(
    ledger: &'a Ledger,
    title: &str,
    file: &str,
    allow_stable_fallback: bool,
) -> Option<&'a LedgerEntry> {
    let key = matching_ledger_key_with_fallback(ledger, title, file, allow_stable_fallback)?;
    ledger.get(&key)
}

fn settle(
    ledger: &mut Ledger,
    title: &str,
    file: &str,
    allow_stable_fallback: bool,
    entry: LedgerEntry,
) {
    let old_key = matching_ledger_key_with_fallback(ledger, title, file, allow_stable_fallback);
    let reraised = old_key
        .as_ref()
        .and_then(|key| ledger.get(key))
        .map(|entry| entry.reraised)
        .unwrap_or(0);
    if let Some(old_key) = old_key {
        ledger.remove(&old_key);
    }
    ledger.insert(finding_key(title, file), LedgerEntry { reraised, ..entry });
}

/// Re-key state from the raw title and full location stored in each entry.
fn normalise_ledger_keys(ledger: &mut Ledger) {
    let mut normalised = Ledger::new();
    for (saved_key, mut entry) in std::mem::take(ledger) {
        let key = if saved_key.len() == 12
            && saved_key
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            saved_key
        } else {
            finding_key(&entry.title, &entry.file)
        };
        if let Some(previous) = normalised.get_mut(&key) {
            let reraised = previous.reraised.max(entry.reraised);
            if entry.round >= previous.round {
                entry.reraised = reraised;
                *previous = entry;
            } else {
                previous.reraised = reraised;
            }
        } else {
            normalised.insert(key, entry);
        }
    }
    *ledger = normalised;
}

/// Blocking findings, once each, in review order.
fn blocking_findings(findings: &[Finding]) -> Vec<Finding> {
    let mut kept = Vec::new();
    for finding in findings.iter().filter(|finding| finding.blocks()) {
        if let Some(existing) = kept
            .iter_mut()
            .find(|existing| same_finding(existing, finding))
        {
            *existing = finding.clone();
        } else {
            kept.push(finding.clone());
        }
    }
    kept
}

fn matching_finding_index(
    findings: &[Finding],
    target: &Finding,
    allow_stable_fallback: bool,
) -> Option<usize> {
    if let Some(index) = findings
        .iter()
        .position(|finding| same_finding(finding, target))
    {
        return Some(index);
    }
    if !allow_stable_fallback {
        return None;
    }

    let stable = stable_finding_key(&target.title, &target.file);
    let mut matches = findings
        .iter()
        .enumerate()
        .filter(|(_, finding)| stable_finding_key(&finding.title, &finding.file) == stable);
    let first = matches.next().map(|(index, _)| index);
    first.filter(|_| matches.next().is_none())
}

fn unique_stable_finding(findings: &[Finding], target: &Finding) -> bool {
    let stable = stable_finding_key(&target.title, &target.file);
    findings
        .iter()
        .filter(|finding| stable_finding_key(&finding.title, &finding.file) == stable)
        .count()
        == 1
}

/// Add findings without losing their newest location or explanation.
fn extend_findings(target: &mut Vec<Finding>, additions: &[Finding]) {
    for finding in additions {
        if let Some(index) =
            matching_finding_index(target, finding, unique_stable_finding(additions, finding))
        {
            target[index] = finding.clone();
        } else {
            target.push(finding.clone());
        }
    }
}

fn update_open_findings(
    open_findings: &mut Vec<Finding>,
    current: &[Finding],
    answer_stands: bool,
) {
    if answer_stands {
        *open_findings = current.to_vec();
    } else {
        extend_findings(open_findings, current);
    }
}

fn remove_findings(target: &mut Vec<Finding>, removed: &[Finding]) {
    for finding in removed {
        if let Some(index) =
            matching_finding_index(target, finding, unique_stable_finding(removed, finding))
        {
            target.remove(index);
        }
    }
}

fn ending_without_landing(open_findings: &[Finding]) -> Ending<'_> {
    if open_findings.is_empty() {
        Ending::Unchanged
    } else {
        Ending::Unresolved(open_findings)
    }
}

fn closing_effort_round(round: u32) -> u32 {
    round.saturating_add(1)
}

fn closing_next_actor(holder: &str) -> String {
    holder.to_string()
}

/// Keep the real points a reviewer chose not to gate on.
///
/// The severity ladder is the whole defence against the nitpick spiral, and it
/// only works if a reviewer can put a real defect somewhere other than blocking.
/// Somewhere has to be a place, though: under the defaults a non-blocking
/// finding is filed nowhere and commented nowhere, so downgrading one deleted
/// it. Now downgrading costs the reviewer a line on the pull request in its own
/// words, and a run that merges with fourteen of them says so where a person
/// will see it.
///
/// Nits are not kept. They are taste, and a list of them is the noise the
/// outcome comment exists to avoid.
fn remember_noted(state: &mut IssueRun, finding: &Finding, allow_stable_fallback: bool) {
    if let Some(index) = matching_finding_index(&state.noted, finding, allow_stable_fallback) {
        state.noted[index] = finding.clone();
    } else {
        state.noted.push(finding.clone());
    }
}

fn forget_noted(state: &mut IssueRun, finding: &Finding, allow_stable_fallback: bool) {
    if let Some(index) = matching_finding_index(&state.noted, finding, allow_stable_fallback) {
        state.noted.remove(index);
    }
}

fn remember_dispute(state: &mut IssueRun, dispute: Dispute, allow_stable_fallback: bool) {
    let exact = finding_key(&dispute.title, &dispute.file);
    let stable = stable_finding_key(&dispute.title, &dispute.file);
    let exact_index = state
        .disputes
        .iter()
        .position(|kept| finding_key(&kept.title, &kept.file) == exact);
    let stable_index = if exact_index.is_none() && allow_stable_fallback {
        let mut matches = state
            .disputes
            .iter()
            .enumerate()
            .filter(|(_, kept)| stable_finding_key(&kept.title, &kept.file) == stable);
        let first = matches.next().map(|(index, _)| index);
        first.filter(|_| matches.next().is_none())
    } else {
        None
    };
    if let Some(index) = exact_index.or(stable_index) {
        state.disputes[index] = dispute;
    } else {
        state.disputes.push(dispute);
    }
}

fn forget_dispute(state: &mut IssueRun, finding: &Finding, allow_stable_fallback: bool) {
    let exact = finding_key(&finding.title, &finding.file);
    if let Some(index) = state
        .disputes
        .iter()
        .position(|kept| finding_key(&kept.title, &kept.file) == exact)
    {
        state.disputes.remove(index);
        return;
    }
    if !allow_stable_fallback {
        return;
    }

    let stable = stable_finding_key(&finding.title, &finding.file);
    let mut matches = state
        .disputes
        .iter()
        .enumerate()
        .filter(|(_, kept)| stable_finding_key(&kept.title, &kept.file) == stable);
    let first = matches.next().map(|(index, _)| index);
    if let Some(index) = first.filter(|_| matches.next().is_none()) {
        state.disputes.remove(index);
    }
}

#[cfg(test)]
fn record_nonblocking_outcome(state: &mut IssueRun, finding: &Finding, outcome: Option<&Followup>) {
    record_nonblocking_outcome_with_match(state, finding, outcome, true);
}

fn record_nonblocking_outcome_with_match(
    state: &mut IssueRun,
    finding: &Finding,
    outcome: Option<&Followup>,
    allow_stable_fallback: bool,
) {
    forget_dispute(state, finding, allow_stable_fallback);
    if let Some(Followup::Recorded(url)) = outcome {
        if !state.filed.iter().any(|filed| filed == url) {
            state.filed.push(url.clone());
        }
        forget_noted(state, finding, allow_stable_fallback);
    } else {
        remember_noted(state, finding, allow_stable_fallback);
    }
}

/// Put the findings a reviewer fixed itself in the ledger.
///
/// The other path has an author's disposition to record, naming which points it
/// answered. Here the reviewer both raised and fixed them, so there is no
/// disposition and the findings themselves are the record. Only reached when a
/// commit landed, which the caller has just observed.
fn record_own_fixes(blocking: &[Finding], ledger: &mut Ledger, state: &mut IssueRun, round: u32) {
    for finding in blocking {
        let allow_stable_fallback = unique_stable_finding(blocking, finding);
        settle(
            ledger,
            &finding.title,
            &finding.file,
            allow_stable_fallback,
            LedgerEntry {
                title: finding.title.clone(),
                file: finding.file.clone(),
                reasoning: "a committed change was made for this point".to_string(),
                round,
                reraised: 0,
                outcome: Settled::Fixed,
            },
        );
        forget_noted(state, finding, allow_stable_fallback);
        forget_dispute(state, finding, allow_stable_fallback);
    }
}

/// A settled point raised twice more goes to a person rather than looping
/// forever.
fn check_relitigation(ledger: &mut Ledger, blocking: &[Finding], state: &mut IssueRun) -> bool {
    let mut escalate = false;
    // One re-raise per round, however many times a review says it. Counting
    // each finding separately let a review that listed one title twice take an
    // entry from nothing to escalated in a single pass, without the author ever
    // being asked. Rare while only refutations were recorded, and not rare now
    // that every fix leaves an entry.
    let mut counted: BTreeSet<String> = BTreeSet::new();
    for finding in blocking {
        let allow_stable_fallback = unique_stable_finding(blocking, finding);
        let Some(key) = matching_ledger_key_with_fallback(
            ledger,
            &finding.title,
            &finding.file,
            allow_stable_fallback,
        ) else {
            continue;
        };
        if !counted.insert(key.clone()) {
            continue;
        }
        if let Some(entry) = ledger.get_mut(&key) {
            entry.reraised += 1;
            if entry.outcome == Settled::Refuted {
                remember_dispute(
                    state,
                    Dispute {
                        title: finding.title.clone(),
                        file: finding.file.clone(),
                        reasoning: entry.reasoning.clone(),
                    },
                    allow_stable_fallback,
                );
            }
            if entry.reraised >= 2 {
                state.notes.push(format!(
                    "'{}' {}",
                    finding.title,
                    why_escalated(entry.outcome)
                ));
                escalate = true;
            }
        }
    }
    escalate
}

/// What a person is told about a point that ran out of tries.
///
/// A fix that missed twice is not an argument nobody would give up, and calling
/// it one sends a maintainer to the wrong side of it. The code changed twice for
/// this point and the reviewer still says it is wrong, which is a different
/// thing to look at and a more likely one to be right about.
fn why_escalated(outcome: Settled) -> &'static str {
    match outcome {
        Settled::Fixed => "was fixed twice and raised again; escalating.",
        _ => "was settled and re-raised twice; escalating.",
    }
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
/// Whether two titles name the same point, ignoring wording noise.
pub(crate) fn same_point(a: &str, b: &str) -> bool {
    normalise(a) == normalise(b)
}

pub(crate) fn same_finding(a: &Finding, b: &Finding) -> bool {
    same_finding_parts(&a.title, &a.file, &b.title, &b.file)
}

pub(crate) fn same_finding_parts(a_title: &str, a_file: &str, b_title: &str, b_file: &str) -> bool {
    finding_key(a_title, a_file) == finding_key(b_title, b_file)
}

fn disposition_matches(finding: &Finding, disposition: &Disposition) -> bool {
    same_point(&finding.title, &disposition.title) && finding.file.trim() == disposition.file.trim()
}

fn matching_disposition<'a>(
    finding: &Finding,
    dispositions: &'a [Disposition],
) -> std::result::Result<(usize, &'a Disposition), &'static str> {
    let mut matches = dispositions
        .iter()
        .enumerate()
        .filter(|(_, disposition)| disposition_matches(finding, disposition));
    let first = matches.next().ok_or("no matching disposition")?;
    if matches.next().is_some() {
        return Err("more than one matching disposition");
    }
    Ok(first)
}

fn fixed_disposition_resolves(committed: bool) -> bool {
    committed
}

#[allow(clippy::too_many_arguments)]
fn apply_dispositions(
    repo: &Repo,
    cfg: &Config,
    response: &ResponseDoc,
    blocking: &[Finding],
    ledger: &mut Ledger,
    state: &mut IssueRun,
    round: u32,
    subject: i64,
    pr_number: i64,
    author: &str,
    committed: bool,
) -> Vec<Finding> {
    let mut fixed = Vec::new();
    let mut refuted = Vec::new();
    let mut filed = Vec::new();
    let mut unresolved = Vec::new();
    let mut used = vec![false; response.dispositions.len()];

    for source in blocking {
        let (index, d) = match matching_disposition(source, &response.dispositions) {
            Ok((index, disposition)) if !used[index] => (index, disposition),
            Ok(_) => {
                logwarn!(
                    "'{}' has more than one matching disposition, so it stays open",
                    source.title
                );
                unresolved.push(source.clone());
                continue;
            }
            Err(reason) => {
                logwarn!("'{}' has {reason}, so it stays open", source.title);
                unresolved.push(source.clone());
                continue;
            }
        };
        used[index] = true;
        let file = source.file.clone();
        // Hash the reviewer's wording, not the author's. The response may vary
        // punctuation while still matching the point, and the next round must
        // look up the same identity the review created.
        let canonical = source.title.as_str();
        let title = style::title(canonical, &repo.style);
        let located_title = match file.trim() {
            "" => title.clone(),
            location => format!("{title} ({location})"),
        };
        let allow_stable_fallback = unique_stable_finding(blocking, source);

        match d.action {
            Action::Refuted => {
                let reasoning = style::summary(&d.reasoning, &repo.style);
                settle(
                    ledger,
                    canonical,
                    &file,
                    allow_stable_fallback,
                    LedgerEntry {
                        title: canonical.to_string(),
                        file: file.clone(),
                        reasoning: reasoning.clone(),
                        round,
                        reraised: 0,
                        outcome: Settled::Refuted,
                    },
                );
                remember_dispute(
                    state,
                    Dispute {
                        title: canonical.to_string(),
                        file: file.clone(),
                        reasoning: reasoning.clone(),
                    },
                    allow_stable_fallback,
                );
                forget_noted(state, source, allow_stable_fallback);
                refuted.push(format!("{located_title}. {reasoning}"));
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
                let recorded = file_followup(repo, &new_title, &new_body, subject, cfg, state);
                if let Some(url) = recorded.url() {
                    state.filed.push(url.to_string());
                    filed.push(url.to_string());
                }
                // Settled like a refutation, because it ends the same way: the
                // code will not change for this point on this branch. Without
                // the entry the reviewer keeping the PR raises it again next
                // round, the author files a duplicate, and the round budget
                // goes on one point nobody disagrees about.
                //
                // Unless nothing holds the point, in which case there is no
                // entry to write: see `filed_entry`.
                let Some((outcome, reasoning)) =
                    filed_entry(&recorded, &style::summary(&d.reasoning, &repo.style))
                else {
                    logwarn!(
                        "'{title}' was not recorded anywhere, so it stays open for the next round"
                    );
                    unresolved.push(source.clone());
                    continue;
                };
                settle(
                    ledger,
                    canonical,
                    &file,
                    allow_stable_fallback,
                    LedgerEntry {
                        title: canonical.to_string(),
                        file: file.clone(),
                        reasoning,
                        round,
                        reraised: 0,
                        outcome,
                    },
                );
                if outcome == Settled::Dropped {
                    remember_noted(state, source, allow_stable_fallback);
                } else {
                    forget_noted(state, source, allow_stable_fallback);
                }
                forget_dispute(state, source, allow_stable_fallback);
            }
            Action::Fixed => {
                // Recorded like every other disposition, on the reviewer's own
                // wording, so a re-raise next round hashes to this entry.
                //
                // Fixing is what most dispositions are, and it was the one that
                // left nothing behind. The next round met the fix as ordinary
                // code with no sign anybody had asked for it, and the guard that
                // ends an argument had only refutations to match, so across six
                // fix rounds on two pull requests it never fired once.
                //
                // Only when something was actually committed, on the same rule
                // `filed_entry` keeps for a follow-up that failed: an entry says
                // the point was dealt with and it outlives the run, so writing
                // one for a fix that does not exist tells every later pass to
                // check code nobody wrote.
                if fixed_disposition_resolves(committed) {
                    settle(
                        ledger,
                        canonical,
                        &file,
                        allow_stable_fallback,
                        LedgerEntry {
                            title: canonical.to_string(),
                            file: file.clone(),
                            reasoning: style::summary(&d.reasoning, &repo.style),
                            round,
                            reraised: 0,
                            outcome: Settled::Fixed,
                        },
                    );
                    forget_noted(state, source, allow_stable_fallback);
                    forget_dispute(state, source, allow_stable_fallback);
                    fixed.push(located_title);
                } else {
                    unresolved.push(source.clone());
                }
            }
        }
    }

    for (index, disposition) in response.dispositions.iter().enumerate() {
        if !used[index] {
            logwarn!(
                "ignoring an unmatched or duplicate disposition for '{}' ({})",
                disposition.title,
                disposition.file
            );
        }
    }

    if repo.style.pr_comments == PrComments::Rounds {
        let comment = disposition_comment(author, response, &fixed, &refuted, &filed, &repo.style);
        if let Some(text) = comment {
            if let Err(e) = repo.comment_pr(pr_number, &text) {
                logdim!("could not post the disposition comment: {e}");
            }
        }
    }
    unresolved
}

/// What the ledger should say about a point the author moved out of this pull
/// request, and whether it should say anything at all.
///
/// Nothing, for a follow-up that failed. An entry tells every later round the
/// point was dealt with, and it outlives the run: recording one for a write
/// that never happened suppresses a real defect for good, on the strength of a
/// transient error.
fn filed_entry(recorded: &Followup, reasoning: &str) -> Option<(Settled, String)> {
    let (outcome, tail) = match recorded {
        Followup::Recorded(reference) => (
            Settled::Filed,
            format!("Tracked in {}.", as_reference(reference)),
        ),
        Followup::Covered(reference) => (
            Settled::Filed,
            format!("Already covered by {}.", as_reference(reference)),
        ),
        Followup::Dropped(why) => (Settled::Dropped, format!("Not filed anywhere: {why}.")),
        Followup::Failed => return None,
    };
    let reasoning = match reasoning.trim() {
        "" => tail,
        said => format!("{said} {tail}"),
    };
    Some((outcome, reasoning))
}

// ---------------------------------------------------------------------------
// Follow-ups
// ---------------------------------------------------------------------------

// One uncertain external write stops the rest for this process. A later run
// performs exact and similarity prechecks before it writes again.
fn external_followup_write_paused(destination: Followups, state: &IssueRun) -> bool {
    destination == Followups::Issues && state.followup_writes_uncertain
}

fn failed_followup(state: &mut IssueRun, error: &SparError) -> Followup {
    if error.kind() == ErrorKind::UncertainWrite {
        state.followup_writes_uncertain = true;
        if !state
            .notes
            .iter()
            .any(|note| note.contains("external follow-up writes were paused"))
        {
            state.notes.push(
                "An external follow-up write could not be verified, so further external \
                 follow-up writes were paused for this run. Inspect recent issues and comments \
                 before trying them again."
                    .to_string(),
            );
        }
    }
    Followup::Failed
}

/// Record a finding that is real but out of scope for this PR.
///
/// On your own repository an issue is the right home. On a large repository
/// that is not yours it is somebody else's notification and somebody else's
/// triage queue, so `local` keeps the same information in `.spar/followups.md`
/// and `none` drops it.
///
/// The answer says which of those happened, because the caller settles the
/// point on it. A failure and a deliberate drop look identical from the outside
/// and mean opposite things to the next round.
pub fn file_followup(
    repo: &Repo,
    title: &str,
    body: &str,
    source: i64,
    cfg: &Config,
    state: &mut IssueRun,
) -> Followup {
    if repo.followups == Followups::None {
        return Followup::Dropped("follow-ups are off for this repository");
    }
    if external_followup_write_paused(repo.followups, state) {
        logdim!(
            "not attempting another external follow-up write after an earlier result could not \
             be verified"
        );
        return Followup::Failed;
    }
    // A backstop against a run that will not stop finding things. Silent
    // truncation is not on offer: what was dropped is said out loud.
    if state.filed.len() >= cfg.loop_cfg.max_followups {
        logwarn!(
            "already recorded {} follow-ups, not recording '{}'. Raise max_followups if you want \
             them all.",
            state.filed.len(),
            style::title(title, &repo.style)
        );
        return Followup::Dropped("this run had already recorded as many follow-ups as it may");
    }
    // The exact string that will land on GitHub. Searching for anything else
    // means the duplicate check can never hit, and every round files another
    // copy of the same follow-up.
    //
    // A title the style gate cannot clean is a failure rather than a drop: the
    // next round words the point differently, and that wording may pass.
    let title = match repo.clean_title(title) {
        Ok(title) => title,
        Err(e) => {
            logdim!("could not clean a follow-up title: {e}");
            return Followup::Failed;
        }
    };
    if title.trim().is_empty() {
        logdim!("nothing left of a follow-up title after cleaning it");
        return Followup::Failed;
    }
    // Not style::body: that is the budget for a pull request comment, read with
    // the diff in front of you. This is a work item somebody picks up cold.
    let body = format!(
        "{}\n\nFound while working on #{source}.",
        style::issue_body(body, &repo.style)
    );

    if repo.followups == Followups::Local {
        return repo.append_local_followup(&title, &body);
    }

    match file_as_issue(repo, &title, &body) {
        Ok(filed) => filed.into(),
        Err(e) => {
            logdim!("could not file a follow-up for '{title}': {e}");
            failed_followup(state, &e)
        }
    }
}

/// What happened to one finding on the way to the tracker.
#[derive(Debug, Clone)]
pub enum Filed {
    /// A new issue.
    Opened(i64, String),
    /// An open issue already covered it, and this pass had something to add.
    AddedTo(i64, String),
    /// An open issue already covered it, and this pass added nothing.
    Covered(i64, String),
    /// A closed issue already covered it. Nothing was written.
    AlreadyClosed(i64, String),
}

impl From<Filed> for Followup {
    fn from(filed: Filed) -> Self {
        match filed {
            Filed::Opened(_, url) | Filed::AddedTo(_, url) | Filed::Covered(_, url) => {
                Followup::Recorded(url)
            }
            // Covered rather than recorded: the point is genuinely tracked, so
            // raising it again is waste, but the issue holding it is closed and
            // must not be handed out as work.
            Filed::AlreadyClosed(_, url) => Followup::Covered(url),
        }
    }
}

impl Filed {
    pub fn url(&self) -> Option<&str> {
        match self {
            Filed::Opened(_, url) | Filed::AddedTo(_, url) | Filed::Covered(_, url) => Some(url),
            // The work is done and closed. Reporting it as filed would put it
            // back into a wave to be implemented again.
            Filed::AlreadyClosed(_, _) => None,
        }
    }

    /// The issue this went to, whatever state it is in. `number` answers the
    /// narrower question of what there is to work.
    pub fn issue(&self) -> i64 {
        match self {
            Filed::Opened(n, _)
            | Filed::AddedTo(n, _)
            | Filed::Covered(n, _)
            | Filed::AlreadyClosed(n, _) => *n,
        }
    }

    /// The issue to work, when there is one to work.
    pub fn number(&self) -> Option<i64> {
        match self {
            Filed::Opened(n, _) | Filed::AddedTo(n, _) | Filed::Covered(n, _) => Some(*n),
            Filed::AlreadyClosed(_, _) => None,
        }
    }

    /// One clause saying where it went, for a log line or an archive entry.
    pub fn note(&self) -> String {
        match self {
            Filed::Opened(n, _) => format!("#{n}"),
            Filed::AddedTo(n, _) => format!("added to #{n}"),
            Filed::Covered(n, _) => format!("#{n} already says this"),
            Filed::AlreadyClosed(n, _) => format!("#{n} covers it and is closed"),
        }
    }

    pub fn describe(&self, title: &str) -> String {
        let title = style::clip(title.trim(), 80);
        match self {
            Filed::Opened(n, _) => format!("filed #{n}: {title}"),
            Filed::AddedTo(n, _) => format!("added to #{n}: {title}"),
            Filed::Covered(n, _) => format!("#{n} already says this: {title}"),
            Filed::AlreadyClosed(n, _) => format!("#{n} covers it and is closed: {title}"),
        }
    }
}

/// File an issue, or add to the one that already covers it.
///
/// Exact title matching let duplicates through: two agents, or two runs a week
/// apart, never word one defect identically, and a real run filed two that had
/// to be closed by hand. Filing a second copy is the complaint; silently
/// dropping the new wording is not much better, because a later pass often
/// carries evidence the first did not.
///
/// The title arrives cleaned by the caller, and it has to: searching for
/// anything but the exact string that will land on GitHub means the duplicate
/// check can never hit.
pub fn file_as_issue(repo: &Repo, title: &str, body: &str) -> Result<Filed> {
    file_as_issue_apart_from(repo, title, body, None)
}

/// The same, with one issue this cannot be a duplicate of.
///
/// A checklist item is quoted in the tracker it was read from, so the tracker
/// is the closest match for every item in it. Without this the run would
/// comment an item onto its own tracker and call it covered.
pub fn file_as_issue_apart_from(
    repo: &Repo,
    title: &str,
    body: &str,
    apart_from: Option<i64>,
) -> Result<Filed> {
    let title = repo.clean_title(title)?;
    if title.trim().is_empty() {
        return Err(spar_err!("nothing left of the title after cleaning it"));
    }
    let issue_body = repo.clean_issue_body(body)?;
    if let Some(existing) = repo.try_exact_issue_apart_from(&title, &issue_body, apart_from)? {
        return Ok(if existing.open {
            Filed::Covered(existing.number, existing.url)
        } else {
            Filed::AlreadyClosed(existing.number, existing.url)
        });
    }
    if let Some(existing) =
        repo.try_find_similar_issue_apart_from(&title, &issue_body, apart_from)?
    {
        let known = format!("{} {}", existing.title, existing.body);
        if !existing.open {
            return Ok(Filed::AlreadyClosed(existing.number, existing.url));
        }
        if crate::textsim::adds_information(&issue_body, &known) {
            repo.comment_issue(existing.number, &issue_body)?;
            return Ok(Filed::AddedTo(existing.number, existing.url));
        }
        return Ok(Filed::Covered(existing.number, existing.url));
    }
    let url = repo.create_issue_apart_from(&title, &issue_body, apart_from)?;
    let number = filed_issue_number(&url)
        .ok_or_else(|| spar_err!("filed an issue but could not read its number from {url}"))?;
    Ok(Filed::Opened(number, url))
}

fn file_out_of_scope(
    repo: &Repo,
    findings: &[Finding],
    subject: i64,
    state: &mut IssueRun,
    cfg: &Config,
) {
    for finding in findings.iter().filter(|f| !f.in_scope) {
        let body = issue_report(finding);
        let recorded = file_followup(repo, &finding.title, &body, subject, cfg, state);
        if finding.severity != Severity::Nit || matches!(recorded, Followup::Recorded(_)) {
            record_nonblocking_outcome_with_match(
                state,
                finding,
                Some(&recorded),
                unique_stable_finding(findings, finding),
            );
        }
    }
}

/// A finding written as a bug report, when it carries the parts of one.
///
/// The thread gets one line; an issue gets the whole thing under headings, in
/// the order somebody reads a bug report: what is wrong, how to see it, what it
/// costs, what it should do instead. A finding with none of those falls back to
/// its detail, which is every finding that was never going to be filed.
pub fn issue_report(finding: &Finding) -> String {
    let sections = finding.report_sections();
    if sections.is_empty() {
        return finding.detail.clone();
    }
    let mut out: Vec<String> = sections
        .iter()
        .map(|(heading, text)| format!("## {heading}\n\n{text}"))
        .collect();
    // Keep the one line summary when it says something the sections do not,
    // rather than dropping it or repeating it.
    if !finding.detail.trim().is_empty()
        && !sections
            .iter()
            .any(|(_, text)| crate::textsim::same_point(text, &finding.detail))
    {
        out.insert(0, finding.detail.trim().to_string());
    }
    out.join("\n\n")
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
    cfg: &Config,
) {
    for finding in findings {
        if !finding.in_scope || finding.severity == Severity::Blocking {
            continue;
        }
        let should_file = match finding.severity {
            Severity::NonBlocking => cfg.loop_cfg.file_non_blocking,
            Severity::Nit => cfg.loop_cfg.file_nits,
            Severity::Blocking => false,
        };
        if !should_file {
            if finding.severity == Severity::NonBlocking {
                record_nonblocking_outcome_with_match(
                    state,
                    finding,
                    None,
                    unique_stable_finding(findings, finding),
                );
            }
            continue;
        }
        let recorded = file_followup(repo, &finding.title, &finding.detail, subject, cfg, state);
        if finding.severity == Severity::NonBlocking {
            record_nonblocking_outcome_with_match(
                state,
                finding,
                Some(&recorded),
                unique_stable_finding(findings, finding),
            );
        } else if let Some(url) = recorded.url() {
            state.filed.push(url.to_string());
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

/// How the run ended, which is the only thing about the run a reader needs.
pub enum Ending<'a> {
    /// Nothing blocks a merge.
    Approved,
    /// The closing pass could not run, so the last round's fixes were pushed and
    /// nothing has read them, which is the part a maintainer has to know.
    OutOfRounds,
    /// The budget ran out on a branch the last round did not change. Nothing is
    /// unread, and nothing cleared the points that were raised either.
    Unchanged,
    /// The closing pass read what the last round left and did not sign it off.
    /// Nothing more will be fixed here, so what is left is a person's to weigh.
    Unresolved(&'a [Finding]),
    /// A point that ran out of tries: refuted and raised again anyway, or fixed
    /// twice and raised again. Nobody is going to break the tie but a person.
    Deadlocked(&'a [Finding]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeSink {
    PullRequest,
    Terminal,
}

fn outcome_sink(mode: PrComments) -> OutcomeSink {
    match mode {
        PrComments::Outcome | PrComments::Rounds => OutcomeSink::PullRequest,
        PrComments::None => OutcomeSink::Terminal,
    }
}

fn emit_outcome(repo: &Repo, pr_number: i64, text: &str) {
    if outcome_sink(repo.style.pr_comments) == OutcomeSink::Terminal {
        println!("\n{text}\n");
        return;
    }
    if let Err(e) = repo.comment_pr(pr_number, text) {
        logdim!("could not post the outcome comment: {e}");
        println!("\n{text}\n");
    }
}

/// Post the one comment a run leaves behind, if it has anything to say.
///
/// Everything spar used to write here was an account of its own working: which
/// agent spoke, which round it was, how many findings of each severity, that it
/// had stopped. None of that is about the code. Worse, the running commentary
/// could contradict itself, ending a thread with "5 fixed" immediately followed
/// by "no convergence", which reads as a failure rather than as fixes nobody
/// has checked yet.
///
/// So the loop is silent and this says what is left: what is unresolved, what
/// was argued down, and where the follow-ups went.
pub fn post_outcome(
    repo: &Repo,
    pr_number: i64,
    state: &IssueRun,
    ledger: &Ledger,
    ending: Ending<'_>,
) {
    let Some(text) = outcome_comment(state, ledger, &ending, &repo.style) else {
        return;
    };
    emit_outcome(repo, pr_number, &text);
}

fn post_unread_outcome(
    repo: &Repo,
    pr_number: i64,
    state: &IssueRun,
    ledger: &Ledger,
    open_findings: &[Finding],
) {
    let Some(text) = outcome_comment_with_unread(
        state,
        ledger,
        &Ending::OutOfRounds,
        open_findings,
        &repo.style,
    ) else {
        return;
    };
    emit_outcome(repo, pr_number, &text);
}

/// How a point was settled and why: this run's disputes first, then the ledger,
/// which is what survives across a resume.
fn settled_as(finding: &Finding, state: &IssueRun, ledger: &Ledger) -> Option<(Settled, String)> {
    if let Some(d) = state
        .disputes
        .iter()
        .find(|d| same_finding_parts(&d.title, &d.file, &finding.title, &finding.file))
    {
        if !d.reasoning.trim().is_empty() {
            return Some((Settled::Refuted, d.reasoning.clone()));
        }
    }
    matching_ledger_entry(ledger, &finding.title, &finding.file)
        .filter(|entry| !entry.reasoning.trim().is_empty())
        .map(|entry| (entry.outcome, entry.reasoning.clone()))
}

/// `#123` from a filed issue URL, falling back to the URL when it does not look
/// like one. Shorter, and GitHub renders it as a link either way.
/// The issue number a filed follow-up URL points at, when it is one. Local
/// notes and anything unparseable yield nothing.
pub fn filed_issue_number(filed: &str) -> Option<i64> {
    filed
        .rsplit('/')
        .next()
        .and_then(|tail| tail.parse::<i64>().ok())
        .filter(|n| *n > 0)
}

fn as_reference(url: &str) -> String {
    match url.rsplit('/').next().and_then(|n| n.parse::<u64>().ok()) {
        Some(number) => format!("#{number}"),
        None => url.to_string(),
    }
}

pub fn outcome_comment(
    state: &IssueRun,
    ledger: &Ledger,
    ending: &Ending<'_>,
    style: &Style,
) -> Option<String> {
    outcome_comment_with_unread(state, ledger, ending, &[], style)
}

fn outcome_comment_with_unread(
    state: &IssueRun,
    ledger: &Ledger,
    ending: &Ending<'_>,
    unread_open: &[Finding],
    style: &Style,
) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    // Points rendered in the deadlock block, so the refutation list below does
    // not print the same title a second time.
    let mut already: Vec<(String, String)> = Vec::new();

    match ending {
        Ending::Approved => {
            if state.disputes.is_empty() && state.filed.is_empty() && state.noted.is_empty() {
                // A clean approval with nothing outstanding needs no comment.
                // The absence of objections is the message. `noted` is in that
                // condition because the message has to be true: a reviewer that
                // found six real problems and gated on none of them did not find
                // nothing, and silence would say it did.
                return None;
            }
            out.push("Reviewed, nothing blocking a merge.".into());
        }
        Ending::OutOfRounds => {
            out.push(
                "Not signed off: the last round of fixes was pushed but has not been reviewed."
                    .into(),
            );
            if !unread_open.is_empty() {
                let lines: Vec<String> = unread_open
                    .iter()
                    .map(|finding| {
                        already.push((finding.title.clone(), finding.file.clone()));
                        format!(
                            "{}. {}",
                            located(finding, style),
                            style::sentence(&finding.detail, style)
                        )
                    })
                    .collect();
                out.push("These points were already open:".into());
                out.push(bullets(&lines));
            }
        }
        // Deliberately not the sentence above. Nothing was pushed on this path,
        // and telling a maintainer to go and read a commit that does not exist
        // is worse than saying nothing.
        Ending::Unchanged => out.push(
            "Not signed off: the last round changed nothing, so the branch is the one that was \
             already reviewed."
                .into(),
        ),
        Ending::Unresolved(points) => {
            let lines: Vec<String> = points
                .iter()
                .map(|f| {
                    already.push((f.title.clone(), f.file.clone()));
                    format!(
                        "{}. {}",
                        located(f, style),
                        style::sentence(&f.detail, style)
                    )
                })
                .collect();
            out.push("Not signed off. These points are still open:".into());
            out.push(bullets(&lines));
        }
        Ending::Deadlocked(points) => {
            // Rendered once, with the argument attached. A deadlocked point is
            // by definition one that was settled earlier, so the reasoning is
            // the whole reason a person is being asked to look. On a resumed
            // run `state.disputes` is empty (only `filed` is restored), so the
            // ledger is the only place that argument survives.
            let lines: Vec<String> = points
                .iter()
                .map(|f| {
                    let where_at = match f.where_at() {
                        "general" => String::new(),
                        file => format!(" ({file})"),
                    };
                    let title = style::title(&f.title, style);
                    already.push((f.title.clone(), f.file.clone()));
                    match settled_as(f, state, ledger) {
                        Some((Settled::Refuted, reason)) => format!(
                            "{title}{where_at}. Refuted as: {}",
                            style::summary(&reason, style)
                        ),
                        Some((Settled::Filed, reason)) => format!(
                            "{title}{where_at}. Filed as out of scope: {}",
                            style::summary(&reason, style)
                        ),
                        // Never "filed": nothing holds this point but the
                        // comment you are reading.
                        Some((Settled::Dropped, reason)) => format!(
                            "{title}{where_at}. Out of scope here, and not filed: {}",
                            style::summary(&reason, style)
                        ),
                        // Not a refutation, so it must not read as one. Nobody
                        // argued this point down: it was fixed, raised again,
                        // fixed again, and raised again, and what a person has
                        // to weigh is a fix that keeps missing rather than an
                        // argument neither agent would give up.
                        Some((Settled::Fixed, reason)) => format!(
                            "{title}{where_at}. Fixed and raised again. Recorded answer: {}",
                            style::summary(&reason, style)
                        ),
                        None => format!("{title}{where_at}"),
                    }
                })
                .collect();
            out.push("Needs your decision. The reviewers could not settle this:".into());
            out.push(bullets(&lines));
        }
    }

    let disputes: Vec<&crate::model::Dispute> = state
        .disputes
        .iter()
        .filter(|d| {
            !already
                .iter()
                .any(|(title, file)| same_finding_parts(title, file, &d.title, &d.file))
        })
        .collect();
    if !disputes.is_empty() {
        // The one thing invisible anywhere else. The diff shows what was fixed;
        // nothing shows what was argued down, or why.
        let lines: Vec<String> = disputes
            .iter()
            .map(|d| {
                let title = style::title(&d.title, style);
                let title = if d.file.trim().is_empty() {
                    title
                } else {
                    format!("{title} ({})", d.file.trim())
                };
                format!("{}. {}", title, style::sentence(&d.reasoning, style))
            })
            .collect();
        out.push(format!("Raised and refuted:\n{}", bullets(&lines)));
    }

    if !state.noted.is_empty() {
        // The only place a downgraded point survives. The diff shows what was
        // fixed and the refutation list shows what was argued down; a finding
        // the reviewer judged real and chose not to gate on had nothing.
        let lines: Vec<String> = state
            .noted
            .iter()
            .filter(|f| {
                !already
                    .iter()
                    .any(|(title, file)| same_finding_parts(title, file, &f.title, &f.file))
            })
            .map(|f| located(f, style))
            .collect();
        if !lines.is_empty() {
            out.push(format!("Noted, not blocking:\n{}", bullets(&lines)));
        }
    }

    if !state.filed.is_empty() {
        let refs: Vec<String> = state.filed.iter().map(|u| as_reference(u)).collect();
        out.push(format!("Filed separately: {}", refs.join(", ")));
    }

    Some(out.join("\n\n"))
}

/// What the closing pass is asked, with the last delta called out inside the
/// full merge-safety audit.
///
/// `landed` is `None` when the harness cannot say what is new. Commit messages
/// are rewritten when they break the style rules, which moves every hash from
/// the first offender onward, so a head recorded before a round can stop being
/// on the branch. Saying that plainly is the only honest option: the alternative
/// is `git log` reporting the whole branch as newly landed.
#[allow(clippy::too_many_arguments)]
fn close_prompt(
    base: &str,
    number: i64,
    title: &str,
    from: &str,
    landed: Option<&[String]>,
    ledger: &Ledger,
    open_findings: &[Finding],
    round: u32,
) -> String {
    let landed = match landed {
        Some([]) => "\nNothing landed after the last round of review. What it asked for was \
                     answered in words rather than in code, so the branch in front of you is the \
                     branch that was already read.\n"
            .to_string(),
        Some(lines) => format!(
            "\nThis landed after the last round of review, and nobody has read it:\n{}\n\nRead it \
             first with `git diff {from}..HEAD`, then inspect the full branch with `git diff \
             {base}...HEAD`.\n",
            lines
                .iter()
                .map(|l| format!("- {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        None => format!(
            "\nThe commits on this branch were rewritten after the last round of review, so the \
             harness cannot say which of them are new. Inspect the full branch with `git diff \
             {base}...HEAD`.\n"
        ),
    };
    CLOSE_PROMPT
        .replace("{number}", &number.to_string())
        .replace("{title}", title)
        .replace("{base}", base)
        .replace("{landed}", &landed)
        .replace("{open}", &open_findings_block(open_findings))
        .replace("{answers}", &closing_answers(ledger, round))
        .replace("{settled}", &settled_block(ledger))
}

fn open_findings_block(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    format!(
        "\nThese blocking findings were left open by an earlier response. Recheck each one:\n{}\n\
         \nIf one still blocks, return it under the same title and file. Omission means you checked \
         it and found that it no longer blocks.\n",
        findings_for_prompt(findings)
    )
}

/// The claimed fixes, as the closing pass is told about them.
///
/// The same points `answers_block` gives a round, asked as the thing this pass
/// is for rather than as context for a wider read.
fn closing_answers(ledger: &Ledger, round: u32) -> String {
    let lines = fixed_lines(ledger, round);
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\nThese points were raised on this pull request and the author says it fixed them. \
         Nobody has checked that:\n{}\n",
        lines.join("\n")
    )
}

/// What the reviewer is asked, with what it already answered behind it.
///
/// Built here rather than inline in the loop, because a prompt built inline is a
/// prompt with no test.
fn review_prompt(
    base: &str,
    number: i64,
    title: &str,
    ledger: &Ledger,
    open_findings: &[Finding],
    round: u32,
    last: u32,
) -> String {
    REVIEW_PROMPT
        .replace("{base}", base)
        .replace("{number}", &number.to_string())
        .replace("{title}", title)
        .replace("{open}", &open_findings_block(open_findings))
        .replace("{answers}", &answers_block(ledger, round))
        .replace("{settled}", &settled_block(ledger))
        .replace("{round}", &round_note(round, last))
}

/// What the implementor is asked, with the issue in front of it.
///
/// The body is passed rather than only the link, because one of the two agents
/// cannot follow a link: codex runs under `-s workspace-write`, which has no
/// network at all, so a URL alone would leave it judging the title. The link is
/// there for the agent that can follow it, and for the comments spar does not
/// fetch.
fn implement_prompt(number: i64, title: &str, url: &str, body: &str) -> String {
    IMPLEMENT_PROMPT
        .replace("{number}", &number.to_string())
        .replace("{title}", title)
        .replace("{url}", url)
        .replace("{body}", body)
}

/// The pull request body.
///
/// What it closes, then the change in one sentence, then what was wrong, then
/// only the sections that have something in them. The lead is two paragraphs
/// rather than two headings: a heading over a single sentence is a label on a
/// label, and those two parts are the ones every body has.
///
/// GitHub renders the file count and the plus and minus figures immediately
/// above this, so neither appears here.
pub fn pr_body(issue: i64, work: &Implementation, style: &Style) -> String {
    let mut parts = vec![format!("Closes #{issue}")];

    for lead in [&work.summary, &work.problem] {
        let text = style::sentence(lead, style);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.extend(section("What changed", &work.changes, style));
    parts.extend(section("How to test", &work.testing, style));

    let notes = style::sentence(work.notes.as_deref().unwrap_or_default(), style);
    if !notes.is_empty() {
        parts.push(format!("## Notes\n\n{notes}"));
    }

    style::body(&parts.join("\n\n"), style)
}

/// A headed list, or nothing at all when there is nothing to list.
///
/// Nothing at all on purpose. A heading with an empty body under it reads as a
/// section somebody forgot to write, which is worse than the absence, and a
/// small change that needs no change list should not be made to look like one
/// that is missing its.
fn section(heading: &str, lines: &[String], style: &Style) -> Option<String> {
    let items: Vec<String> = lines
        .iter()
        .map(|line| style::summary(line, style))
        .filter(|line| !line.is_empty())
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(format!("## {heading}\n\n{}", bullets(&items)))
}

/// A pull request body for work whose author never got to describe it.
///
/// The implement call failed after the commits were made, so what those commits
/// say about themselves is the only account of them there is. It is a poor one,
/// and better than an empty body over work nobody would otherwise know was
/// there; the note says as much, so a reviewer does not read the list as the
/// author's own summary.
pub fn from_commits(repo: &Repo, work_dir: &Path, base: &str) -> Implementation {
    Implementation {
        changes: repo.commit_subjects(work_dir, "HEAD", base),
        notes: Some(
            "The implement call failed after these commits were made, so this body is assembled \
             from their messages rather than written by their author. Read the diff."
                .to_string(),
        ),
        ..Implementation::default()
    }
}

/// What gets posted on an issue that produced no pull request.
///
/// The agent's own reason when it gave one, since that is the part written for
/// the person who opened the issue. Never the summary: an issue that produced
/// no commits has no change for a summary to describe, and one that claims
/// otherwise is worse than a flat sentence saying nothing happened.
fn no_pr_note(work: &Implementation, style: &Style) -> String {
    let reason = style::sentence(&work.reason, style);
    if !reason.is_empty() {
        return reason;
    }
    if work.not_worth_doing {
        "Left alone after reading the code, with no reason given.".to_string()
    } else {
        "Nothing was committed, so there is nothing to review.".to_string()
    }
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

    let _ = (holder, round, headline);
    let mut out = Vec::new();
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
        ("non-blocking", &non_blocking),
        ("nits", &nits),
        ("out of scope", &out_of_scope),
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

    let _ = (author, counts);
    let mut out = Vec::new();
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
/// What is posted on an issue both reviewers declined.
///
/// Just the reasons. GitHub already shows that it was closed as not planned,
/// and which model held which opinion is a fact about the run rather than about
/// the issue. Duplicates are collapsed, since two reviewers reaching the same
/// conclusion often reach it in the same words.
pub fn skip_comment(item: &SkippedItem, style: &Style) -> String {
    let reasons = item
        .reasons
        .values()
        .map(|reason| style::sentence(reason, style));
    // Two reviewers declining one issue almost always decline it for the same
    // reason, worded differently. On the run that prompted this, both cited the
    // issue it duplicated and the reader saw the point twice.
    let lines = crate::textsim::dedupe_by(reasons, crate::textsim::same_reason);
    bullets(&lines)
}

/// Findings as a model should see them: full detail, since this one is not for
/// a human to read.
pub(crate) fn findings_for_prompt(findings: &[Finding]) -> String {
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
            ..Default::default()
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

    fn disposition(title: &str, file: &str, action: Action) -> Disposition {
        Disposition {
            title: title.into(),
            file: file.into(),
            action,
            reasoning: "because".into(),
            new_issue_title: None,
            new_issue_body: None,
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
    fn an_uncommitted_implementation_names_its_diagnostic_and_recovery_path() {
        let path = Path::new("/tmp/issue worktree");
        let err =
            uncommitted_implementation_error(path, Some("git add could not create index.lock"))
                .to_string();
        assert!(err.contains("could not create index.lock"), "{err}");
        assert!(err.contains("/tmp/issue worktree"), "{err}");
        assert!(err.contains("Commit or recover"), "{err}");
        assert!(!should_release(&cfg_with(true, false), Status::Error));
    }

    #[test]
    fn the_keep_flag_overrides_everything() {
        assert!(!should_release(&cfg_with(true, true), Status::Approved));
    }

    #[test]
    fn nothing_is_released_when_worktrees_are_off() {
        assert!(!should_release(&cfg_with(false, false), Status::Approved));
    }

    // -- custody ---------------------------------------------------------

    /// The reviewer fixed the findings itself, so it wrote the head and the
    /// other agent takes round 2.
    #[test]
    fn fixing_your_own_findings_hands_the_pr_over() {
        let cfg = cfg_with(true, false);
        assert_eq!("a", next_reviewer(&cfg, "b", Some("b")));
        assert_eq!("b", next_reviewer(&cfg, "a", Some("a")));
    }

    /// The author wrote the head, so the reviewer keeps the PR. Flipping here
    /// gave the author its own fix to review in round 2, and an approval of it
    /// ended the loop.
    #[test]
    fn handing_back_keeps_the_reviewer_for_the_next_round() {
        let cfg = cfg_with(true, false);
        assert_eq!("b", next_reviewer(&cfg, "b", Some("a")));
        assert_eq!("a", next_reviewer(&cfg, "a", Some("b")));
    }

    /// Whoever holds round 2 did not write what it is reading, whoever wrote
    /// it. `a` implements, so `b` reviews round 1.
    #[test]
    fn nobody_reviews_their_own_edit() {
        let cfg = cfg_with(true, false);
        let round_1 = cfg.other(&cfg.first_implementor);
        assert_eq!("b", round_1);
        for editor in ["a", "b"] {
            assert_ne!(editor, next_reviewer(&cfg, &round_1, Some(editor)));
        }
    }

    /// The `fix_myself` half of the bug. The reviewer said it would fix its own
    /// findings and the call returned without committing, so the head is still
    /// the author's and handing over would put the author in front of its own
    /// work.
    #[test]
    fn a_fix_that_committed_nothing_leaves_the_pr_where_it_is() {
        let cfg = cfg_with(true, false);
        assert_eq!("b", next_reviewer(&cfg, "b", None));
        assert_eq!("a", next_reviewer(&cfg, "a", None));
    }

    /// The `hand_back` half. The reviewer committed while reviewing and the
    /// author answered without committing, so the head is the reviewer's and
    /// keeping it would have it read its own commit.
    #[test]
    fn a_reviewer_that_wrote_the_head_gives_the_pr_up() {
        let cfg = cfg_with(true, false);
        assert_eq!("a", next_reviewer(&cfg, "b", Some("b")));
    }

    /// A reviewer that fixes what it finds and then reports nothing blocking
    /// approved its own fix, and the rollback takes that fix out again. The
    /// head that would merge is not the head that passed.
    #[test]
    fn a_review_that_wrote_cannot_approve_what_is_left() {
        assert!(!approval_stands(&[], true));
    }

    #[test]
    fn a_clean_review_of_an_untouched_branch_approves() {
        assert!(approval_stands(&[], false));
    }

    #[test]
    fn a_blocking_finding_never_approves() {
        let blocking = vec![finding("blocking", "Broken", "detail", "src/x.rs", true)];
        assert!(!approval_stands(&blocking, false));
    }

    #[test]
    fn approval_refuses_a_head_that_changed_after_review() {
        assert!(ensure_reviewed_head(36, "abc123", "abc123").is_ok());
        let error = ensure_reviewed_head(36, "abc123", "def456").unwrap_err();
        assert!(error.to_string().contains("unread head"));
    }

    /// Custody is decided on what git says, not on the call returning.
    #[test]
    fn only_a_moved_head_counts_as_a_commit() {
        let before = Snapshot {
            head: "abc".into(),
            dirty: false,
        };
        assert!(!Snapshot {
            head: "abc".into(),
            dirty: true,
        }
        .landed_over(&before));
        assert!(Snapshot {
            head: "def".into(),
            dirty: false,
        }
        .landed_over(&before));
        // git could not be read, which is not evidence that anything landed.
        assert!(!Snapshot {
            head: String::new(),
            dirty: false,
        }
        .landed_over(&before));
    }

    // -- round budget ----------------------------------------------------

    /// A fresh PR gets rounds 1 through max_rounds.
    #[test]
    fn a_fresh_run_starts_at_one() {
        assert_eq!((1, 3), round_window(1, 3));
        assert_eq!((1, 5), round_window(1, 5));
    }

    /// The budget is per invocation, not a lifetime cap. Running spar again on
    /// a PR that already spent five rounds gives it five more, because a person
    /// looked at it and chose to.
    #[test]
    fn a_resumed_run_gets_a_full_fresh_budget() {
        assert_eq!((6, 10), round_window(6, 5));
        assert_eq!((11, 13), round_window(11, 3));
    }

    #[test]
    fn a_budget_of_one_is_a_single_round() {
        assert_eq!((6, 6), round_window(6, 1));
    }

    #[test]
    fn round_numbers_keep_counting_across_sessions() {
        // Three sessions of three rounds each land on 1..3, 4..6, 7..9.
        let mut start = 1;
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (first, last) = round_window(start, 3);
            seen.push((first, last));
            start = last + 1;
        }
        assert_eq!(vec![(1, 3), (4, 6), (7, 9)], seen);
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
                outcome: Settled::Refuted,
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

    /// Fixing is what most dispositions are, and it recorded nothing, so the
    /// guard had only refutations to match and never fired on a real run. Three
    /// tries at one point is a person's problem, not another round's.
    #[test]
    fn a_point_fixed_twice_and_raised_again_escalates() {
        let mut ledger = ledger_with("Unbounded loop", "src/x.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Fixed;
        }
        let mut state = IssueRun::new(1, "t");
        let blocking = [finding("blocking", "Unbounded loop", "d", "src/x.rs", true)];

        assert!(!check_relitigation(&mut ledger, &blocking, &mut state));
        assert!(check_relitigation(&mut ledger, &blocking, &mut state));
    }

    /// A maintainer reading "settled and re-raised" about a fix that genuinely
    /// did not work sides with the author, and is wrong.
    #[test]
    fn a_fix_that_missed_twice_is_not_reported_as_a_refutation() {
        assert!(why_escalated(Settled::Fixed).contains("fixed twice"));
        for outcome in [Settled::Refuted, Settled::Filed, Settled::Dropped] {
            assert!(why_escalated(outcome).contains("settled"), "{outcome}");
        }
    }

    /// A reviewer that fixes its own findings answers them in code too. Leaving
    /// them out left that path with the hole the other one had: the next pass
    /// reads a fix with nothing saying it was asked for, and the guard cannot
    /// count it.
    #[test]
    fn a_reviewer_that_fixes_its_own_findings_records_them_too() {
        let mut ledger = Ledger::new();
        let blocking = vec![finding(
            "blocking",
            "Unbounded loop",
            "spins",
            "src/x.rs",
            true,
        )];
        let mut state = IssueRun::new(1, "t");

        record_own_fixes(&blocking, &mut ledger, &mut state, 1);

        let entry = ledger
            .get(&finding_key("Unbounded loop", "src/x.rs"))
            .expect("keyed where the next round will look");
        assert_eq!(Settled::Fixed, entry.outcome);
        assert_eq!(
            "a committed change was made for this point",
            entry.reasoning
        );

        // And the guard can now count it, which it could not before.
        assert!(!check_relitigation(&mut ledger, &blocking, &mut state));
        assert!(check_relitigation(&mut ledger, &blocking, &mut state));
    }

    /// The settled block tells a reviewer the code will not change for a point.
    /// That is the opposite of what happened to a fix, and a fixed point printed
    /// there reads as an argument already won.
    #[test]
    fn a_fixed_point_is_not_in_the_settled_block() {
        let mut ledger = ledger_with("refuted point", "a.rs");
        ledger.extend(ledger_with("fixed point", "b.rs"));
        for entry in ledger.values_mut() {
            if entry.title == "fixed point" {
                entry.outcome = Settled::Fixed;
            }
        }
        let block = settled_block(&ledger);
        assert!(block.contains("refuted point"));
        assert!(!block.contains("fixed point"));
    }

    /// And a ledger holding nothing but fixes has no settled block at all,
    /// rather than a heading with no points under it.
    #[test]
    fn a_ledger_of_only_fixes_says_nothing_is_settled() {
        let mut ledger = ledger_with("fixed point", "b.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Fixed;
        }
        assert_eq!("", settled_block(&ledger));
    }

    /// A review that lists one point twice used to take its entry from nothing
    /// to escalated in a single pass, without the author ever being asked. Rare
    /// while only refutations were recorded, and not rare now that every fix
    /// leaves an entry.
    #[test]
    fn one_review_spends_one_re_raise_however_often_it_says_it() {
        let mut ledger = ledger_with("Missing error handling", "src/net.rs");
        let mut state = IssueRun::new(1, "t");
        let twice = vec![
            finding(
                "blocking",
                "Missing error handling",
                "d",
                "src/net.rs",
                true,
            ),
            finding(
                "blocking",
                "Missing error handling",
                "e",
                "src/net.rs",
                true,
            ),
        ];

        assert!(!check_relitigation(&mut ledger, &twice, &mut state));
        assert_eq!(1, ledger.values().next().unwrap().reraised);
        assert!(check_relitigation(&mut ledger, &twice, &mut state));
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

    #[test]
    fn persisted_ledger_entries_are_rekeyed_for_stable_locations() {
        let mut ledger = Ledger::new();
        ledger.insert(
            "legacy-key".into(),
            LedgerEntry {
                title: "Unbounded loop".into(),
                file: "src/x.rs:88".into(),
                reasoning: "bounded by the caller".into(),
                round: 2,
                reraised: 1,
                outcome: Settled::Refuted,
            },
        );
        normalise_ledger_keys(&mut ledger);
        let key = matching_ledger_key(&ledger, "Unbounded loop", "src/x.rs:91").unwrap();
        assert_eq!(1, ledger[&key].reraised);
    }

    #[test]
    fn same_title_at_two_sites_keeps_both_blockers() {
        let findings = vec![
            finding(
                "blocking",
                "Unchecked error",
                "first site",
                "src/net.rs:10",
                true,
            ),
            finding(
                "blocking",
                "Unchecked error",
                "second site",
                "src/net.rs:200",
                true,
            ),
        ];

        let blocking = blocking_findings(&findings);
        assert_eq!(2, blocking.len());
        assert_eq!("src/net.rs:10", blocking[0].file);
        assert_eq!("src/net.rs:200", blocking[1].file);
    }

    #[test]
    fn moved_location_fallback_refuses_an_ambiguous_ledger() {
        let mut ledger = ledger_with("Unchecked error", "src/net.rs:10");
        ledger.extend(ledger_with("Unchecked error", "src/net.rs:200"));
        assert!(matching_ledger_key(&ledger, "Unchecked error", "src/net.rs:30").is_none());
    }

    #[test]
    fn two_current_sites_do_not_relocate_one_old_ledger_entry() {
        let mut ledger = ledger_with("Unchecked error", "src/net.rs:5");
        let blocking = vec![
            finding(
                "blocking",
                "Unchecked error",
                "first",
                "src/net.rs:10",
                true,
            ),
            finding(
                "blocking",
                "Unchecked error",
                "second",
                "src/net.rs:200",
                true,
            ),
        ];
        let mut state = IssueRun::new(1, "t");
        record_own_fixes(&blocking, &mut ledger, &mut state, 2);
        assert!(ledger.contains_key(&finding_key("Unchecked error", "src/net.rs:10")));
        assert!(ledger.contains_key(&finding_key("Unchecked error", "src/net.rs:200")));
    }

    #[test]
    fn two_current_sites_remain_two_open_findings() {
        let current = vec![
            finding(
                "blocking",
                "Unchecked error",
                "first",
                "src/net.rs:10",
                true,
            ),
            finding(
                "blocking",
                "Unchecked error",
                "second",
                "src/net.rs:200",
                true,
            ),
        ];
        let mut open = Vec::new();

        extend_findings(&mut open, &current);

        assert_eq!(2, open.len());
        assert_eq!("src/net.rs:10", open[0].file);
        assert_eq!("src/net.rs:200", open[1].file);
    }

    #[test]
    fn display_limits_do_not_change_persisted_finding_identity() {
        let point = finding(
            "blocking",
            "abcdefghij",
            "still wrong",
            "src/net.rs:10",
            true,
        );
        let mut ledger = Ledger::new();
        let mut state = IssueRun::new(1, "t");
        record_own_fixes(std::slice::from_ref(&point), &mut ledger, &mut state, 1);
        normalise_ledger_keys(&mut ledger);

        let key = matching_ledger_key(&ledger, "abcdefghij", "src/net.rs:12").unwrap();
        assert_eq!("abcdefghij", ledger[&key].title);
    }

    #[test]
    fn a_clipped_legacy_entry_keeps_its_original_lookup_key() {
        let mut ledger = Ledger::new();
        let key = crate::jsonx::finding_key("abcdefghij", "src/net.rs:10");
        ledger.insert(
            key.clone(),
            LedgerEntry {
                title: "abcde".into(),
                file: "src/net.rs:10".into(),
                reasoning: "bounded by the caller".into(),
                round: 1,
                reraised: 1,
                outcome: Settled::Refuted,
            },
        );
        normalise_ledger_keys(&mut ledger);

        assert_eq!(
            Some(key),
            matching_ledger_key(&ledger, "abcdefghij", "src/net.rs:12")
        );
    }

    #[test]
    fn a_legacy_key_collision_does_not_merge_case_distinct_paths() {
        let mut ledger = Ledger::new();
        let key = crate::jsonx::finding_key("Unchecked error", "src/Main.rs:10");
        ledger.insert(
            key.clone(),
            LedgerEntry {
                title: "Unchecked error".into(),
                file: "src/Main.rs:10".into(),
                reasoning: "bounded by the caller".into(),
                round: 1,
                reraised: 0,
                outcome: Settled::Refuted,
            },
        );

        assert_eq!(
            Some(key),
            matching_ledger_key(&ledger, "Unchecked error", "src/Main.rs:10")
        );
        assert!(matching_ledger_key(&ledger, "Unchecked error", "src/main.rs:10").is_none());
    }

    /// The key a refutation records has to be the key the next round's finding
    /// hashes to. Recording it without the file made the guard dead code for
    /// every finding that named one, which is nearly all of them.
    #[test]
    fn a_refutation_lands_on_the_key_the_next_round_will_look_up() {
        let blocking = [finding("blocking", "Unbounded loop", "d", "src/x.rs", true)];
        let recorded = finding_key(&blocking[0].title, &blocking[0].file);
        let answer = disposition("unbounded loop!", "src/x.rs", Action::Refuted);
        assert!(disposition_matches(&blocking[0], &answer));
        assert_eq!(recorded, finding_key(&answer.title, &answer.file));
    }

    /// Title punctuation is wording noise, while the path remains part of the
    /// identity. A response can vary punctuation without losing the point.
    #[test]
    fn the_ledger_key_ignores_title_punctuation() {
        let findings = [finding(
            "blocking",
            "Panic on multi-byte input",
            "d",
            "src/style.rs",
            true,
        )];
        let reworded = "Panic on multibyte input";
        let source = &findings[0];
        assert_eq!(
            finding_key(reworded, &source.file),
            finding_key(&source.title, &source.file)
        );
        let recorded = finding_key(&source.title, &source.file);
        let looked_up = finding_key(&findings[0].title, &findings[0].file);
        assert_eq!(recorded, looked_up);
    }

    #[test]
    fn a_disposition_matches_its_finding_despite_wording_noise() {
        let findings = [finding(
            "blocking",
            "Unbounded loop!",
            "d",
            "src/x.rs",
            true,
        )];
        assert!(disposition_matches(
            &findings[0],
            &disposition("unbounded loop", "src/x.rs", Action::Refuted)
        ));
        assert!(!disposition_matches(
            &findings[0],
            &disposition("something else", "src/x.rs", Action::Refuted)
        ));
        assert!(!disposition_matches(
            &findings[0],
            &disposition("unbounded loop", "src/y.rs", Action::Refuted)
        ));
    }

    #[test]
    fn an_omitted_disposition_leaves_the_blocker_unmatched() {
        let blocker = finding("blocking", "Unbounded loop", "d", "src/x.rs", true);
        assert!(matches!(
            matching_disposition(&blocker, &[]),
            Err("no matching disposition")
        ));
    }

    #[test]
    fn duplicate_dispositions_are_ambiguous() {
        let blocker = finding("blocking", "Unbounded loop", "d", "src/x.rs", true);
        let answers = vec![
            disposition("Unbounded loop", "src/x.rs", Action::Fixed),
            disposition("Unbounded loop", "src/x.rs", Action::Refuted),
        ];
        assert!(matches!(
            matching_disposition(&blocker, &answers),
            Err("more than one matching disposition")
        ));
    }

    #[test]
    fn same_titled_findings_in_different_files_need_separate_dispositions() {
        let left = finding("blocking", "Unchecked error", "d", "src/a.rs", true);
        let right = finding("blocking", "Unchecked error", "d", "src/b.rs", true);
        let answers = vec![
            disposition("Unchecked error", "src/a.rs", Action::Fixed),
            disposition("Unchecked error", "src/b.rs", Action::Refuted),
        ];
        assert_eq!(
            0,
            matching_disposition(&left, &answers)
                .expect("left answer")
                .0
        );
        assert_eq!(
            1,
            matching_disposition(&right, &answers)
                .expect("right answer")
                .0
        );
    }

    #[test]
    fn a_reported_fix_without_a_commit_stays_open() {
        assert!(!fixed_disposition_resolves(false));
        assert!(fixed_disposition_resolves(true));
    }

    #[test]
    fn the_settled_block_is_empty_when_nothing_is_settled() {
        assert_eq!("", settled_block(&Ledger::new()));
    }

    #[test]
    fn the_settled_block_names_each_refutation() {
        let block = settled_block(&ledger_with("a point", "x.rs"));
        assert!(block.contains("a point"));
        assert!(block.contains("x.rs"));
        assert!(block.contains("settled"));
    }

    #[test]
    fn same_title_settlements_name_each_location() {
        let mut ledger = ledger_with("Unchecked error", "a.rs:10");
        ledger.extend(ledger_with("Unchecked error", "b.rs:20"));

        let block = settled_block(&ledger);

        assert!(block.contains("Unchecked error (a.rs:10)"), "{block}");
        assert!(block.contains("Unchecked error (b.rs:20)"), "{block}");
    }

    /// A point the author moved to its own issue is done with on this branch.
    /// Leaving it out of the block let the reviewer that keeps the PR raise it
    /// again every round until the budget ran out.
    #[test]
    fn a_filed_point_is_settled_too() {
        let mut ledger = ledger_with("out of scope", "x.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Filed;
            entry.reasoning = "Tracked in #9.".into();
        }
        let block = settled_block(&ledger);
        assert!(block.contains("out of scope"));
        assert!(block.contains("#9"));
    }

    /// The author answers the point again every round it is re-raised, so
    /// recording the answer must not wipe the count that ends the argument.
    #[test]
    fn answering_a_point_again_keeps_its_re_raise_count() {
        let mut ledger = ledger_with("a point", "x.rs");
        let entry = ledger.values().next().unwrap().clone();
        let mut state = IssueRun::new(1, "t");
        let blocking = vec![finding("blocking", "a point", "d", "x.rs", true)];

        assert!(!check_relitigation(&mut ledger, &blocking, &mut state));
        settle(&mut ledger, "a point", "x.rs", true, entry);
        assert!(check_relitigation(&mut ledger, &blocking, &mut state));
    }

    // -- brevity ---------------------------------------------------------

    #[test]
    /// No agent name, no round number, and no count of things listed below.
    /// The reader wants the review, not an account of who produced it.
    fn a_clean_review_is_just_the_verdict() {
        let text = review_comment("codex", 1, &review("Looks correct.", vec![]), &style());
        assert_eq!("Looks correct.", text);
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
        assert!(text.starts_with("One real problem."), "{text}");
        assert!(!text.contains("codex"), "no agent name: {text}");
        assert!(!text.contains("round 2"), "no round number: {text}");
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
    /// A finding's explanation is what the author acts on. Cutting it to save
    /// characters leaves them nothing to act on and saves nothing worth having.
    fn a_thorough_explanation_reaches_the_author_intact() {
        let detail = "Reproduced by running the 429 test with max_attempts unset. ".repeat(8);
        let text = review_comment(
            "codex",
            1,
            &review(
                "One problem.",
                vec![finding("blocking", "T", &detail, "a.rs", true)],
            ),
            &style(),
        );
        assert!(
            text.contains(detail.trim()),
            "the explanation was cut:\n{text}"
        );
    }

    /// A runaway is still bounded, just nowhere near tightly.
    #[test]
    fn a_runaway_model_is_still_bounded() {
        let long = "filler words. ".repeat(20_000);
        let text = review_comment(
            "codex",
            1,
            &review(&long, vec![finding("blocking", "T", &long, "a.rs", true)]),
            &style(),
        );
        assert!(
            text.len() < 30_000,
            "review comment was {} chars",
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
        assert!(text.contains("out of scope"), "{text}");
        assert!(text.contains("Old bug"), "{text}");
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
        assert!(text.starts_with("Two of three were right."), "{text}");
        assert!(!text.contains("claude"), "no agent name: {text}");
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

    // -- the closing pass -------------------------------------------------

    /// A closing pass is not allowed to publish its own commit. The remote head
    /// therefore keeps the same eligible reviewer on a later run.
    #[test]
    fn a_local_closing_commit_does_not_change_remote_custody() {
        for holder in ["a", "b"] {
            assert_eq!(holder, closing_next_actor(holder));
        }
    }

    #[test]
    fn a_matching_head_keeps_saved_custody() {
        assert!(reconcile_saved_head(Some("abc123"), "abc123", None, 42).unwrap());
        assert!(reconcile_saved_head(None, "abc123", None, 42).unwrap());
    }

    #[test]
    fn a_changed_head_refuses_automatic_custody() {
        let error = reconcile_saved_head(Some("abc123"), "def456", None, 42).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("abc123"), "{text}");
        assert!(text.contains("def456"), "{text}");
        assert!(text.contains("--next <agent>"), "{text}");
    }

    #[test]
    fn an_explicit_holder_resets_state_for_a_changed_or_legacy_head() {
        assert!(!reconcile_saved_head(Some("abc123"), "def456", Some("b"), 42).unwrap());
        assert!(!reconcile_saved_head(Some(""), "def456", Some("b"), 42).unwrap());
        assert!(reconcile_saved_head(Some(""), "def456", None, 42).is_err());
    }

    #[test]
    fn an_invalid_review_cannot_clear_a_carried_blocker() {
        let mut open = vec![finding(
            "blocking",
            "Unchecked error",
            "still fails",
            "src/a.rs:12",
            true,
        )];
        update_open_findings(&mut open, &[], false);
        assert_eq!(1, open.len());

        update_open_findings(&mut open, &[], true);
        assert!(open.is_empty());
    }

    #[test]
    fn a_final_round_with_no_commit_names_open_blockers() {
        let open = vec![finding(
            "blocking",
            "Unchecked error",
            "still fails",
            "src/a.rs",
            true,
        )];
        assert!(matches!(
            ending_without_landing(&open),
            Ending::Unresolved(points) if points.len() == 1
        ));
        assert!(matches!(ending_without_landing(&[]), Ending::Unchanged));
    }

    #[test]
    fn a_closing_pass_uses_the_later_effort_tier() {
        assert_eq!(2, closing_effort_round(1));
        assert_eq!(8, closing_effort_round(7));
    }

    #[test]
    fn a_ledger_with_no_claimed_fix_has_nothing_to_close_over() {
        assert!(!any_fixes(&ledger_with("refuted point", "a.rs"), 1));
        let mut fixed = ledger_with("fixed point", "b.rs");
        for entry in fixed.values_mut() {
            entry.outcome = Settled::Fixed;
        }
        assert!(any_fixes(&fixed, 1));
    }

    /// A count of rounds is a fact about spar, and what is left is a fact about
    /// the branch.
    #[test]
    fn the_closing_note_counts_points_rather_than_rounds() {
        assert_eq!("one point left after the closing pass", unresolved_note(1));
        assert_eq!("3 points left after the closing pass", unresolved_note(3));
        assert!(!unresolved_note(2).contains("round"));
    }

    fn fixed_ledger() -> Ledger {
        let mut ledger = ledger_with("Unbounded loop", "src/x.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Fixed;
            entry.reasoning = "bounded it on max_attempts".into();
        }
        ledger
    }

    #[test]
    fn the_closing_prompt_names_every_fix_it_has_to_check() {
        let landed = vec!["abc1234 Bound the retry loop".to_string()];
        let prompt = close_prompt(
            "main",
            42,
            "Retry a 429",
            "9f8e7d6",
            Some(&landed),
            &fixed_ledger(),
            &[],
            1,
        );
        assert!(prompt.contains("Unbounded loop"), "{prompt}");
        assert!(prompt.contains("bounded it on max_attempts"), "{prompt}");
        assert!(prompt.contains("abc1234 Bound the retry loop"), "{prompt}");
        assert!(prompt.contains("git diff 9f8e7d6..HEAD"), "{prompt}");
        assert!(prompt.contains("git diff main...HEAD"), "{prompt}");
        assert!(!prompt.contains('{'), "{prompt}");
    }

    /// Nothing landed is a real answer and a different one from "the harness
    /// cannot tell", and neither may leave a heading with nothing under it.
    #[test]
    fn a_close_with_nothing_landed_says_so_rather_than_leaving_a_hole() {
        let prompt = close_prompt(
            "main",
            42,
            "Retry a 429",
            "9f8e7d6",
            Some(&[]),
            &Ledger::new(),
            &[],
            1,
        );
        assert!(
            prompt.contains("Nothing landed after the last round"),
            "{prompt}"
        );
        assert!(!prompt.contains('{'), "{prompt}");
    }

    /// A commit message that breaks the style rules is rewritten, which moves
    /// every hash after it, so the head a round recorded can stop being on the
    /// branch. `git log` answers that with the whole branch, and reporting all
    /// of it as newly landed would be false. The full branch remains the audit
    /// scope either way.
    #[test]
    fn a_rewritten_branch_admits_it_cannot_say_what_landed() {
        let prompt = close_prompt(
            "main",
            42,
            "Retry a 429",
            "9f8e7d6",
            None,
            &fixed_ledger(),
            &[],
            1,
        );
        assert!(prompt.contains("were rewritten"), "{prompt}");
        assert!(prompt.contains("git diff main...HEAD"), "{prompt}");
        assert!(!prompt.contains("nobody has read it"), "{prompt}");
        assert!(!prompt.contains('{'), "{prompt}");
    }

    /// The pass may not write, and the loop rolls back and says the prompt
    /// forbids it, so the prompt has to actually forbid it.
    #[test]
    fn the_closing_prompt_forbids_the_writing_the_loop_rolls_back() {
        let prompt = close_prompt(
            "main",
            42,
            "t",
            "9f8e7d6",
            Some(&[]),
            &Ledger::new(),
            &[],
            1,
        );
        assert!(prompt.contains("do not commit"), "{prompt}");
    }

    /// Missing a serious defect in an earlier round does not make it safe.
    #[test]
    fn the_closing_prompt_keeps_confirmed_merge_blockers_blocking() {
        let prompt = close_prompt(
            "main",
            42,
            "t",
            "9f8e7d6",
            Some(&[]),
            &Ledger::new(),
            &[],
            1,
        );
        assert!(
            prompt.contains("serious defect an\nearlier round missed"),
            "{prompt}"
        );
        assert!(prompt.contains("final merge-safety audit"), "{prompt}");
        assert!(!prompt.contains("not another\naudit"), "{prompt}");
        assert!(!prompt.contains("A\nfinding means"), "{prompt}");
        assert!(prompt.contains("A\nblocking finding means"), "{prompt}");
        let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("does not become non-blocking"), "{prompt}");
        assert!(flat.contains("Only one of them ships"), "{prompt}");
    }

    /// The closing pass had two routes for a real point, block or in_scope=false,
    /// and `blocks()` is `severity == Blocking && in_scope`, so the second one
    /// silently opens the merge gate. A closer taking it filed an issue saying
    /// the branch must not merge and merged the branch.
    #[test]
    fn the_closing_pass_is_offered_a_severity_rather_than_the_field_that_gates() {
        let prompt = close_prompt(
            "main",
            42,
            "t",
            "9f8e7d6",
            Some(&[]),
            &Ledger::new(),
            &[],
            1,
        );
        assert!(
            prompt.contains("Minor defects and improvements are\nnon-blocking"),
            "{prompt}"
        );
        assert!(
            prompt.contains("a real defect\nthis pull request did not cause"),
            "{prompt}"
        );
    }

    /// A point that only ever reached `in_scope = false` never reaches
    /// `blocking`, whatever severity it carries, so the run merges.
    #[test]
    fn an_out_of_scope_point_cannot_gate_the_close() {
        let out_of_scope = finding("blocking", "Adjacent leak", "d", "o.rs", false);
        assert!(!out_of_scope.blocks());
        assert!(approval_stands(&[], false));
    }

    /// The closing pass reads what the last round left. Carrying every fix a
    /// pull request ever saw would hand a resumed run's close nine rounds of
    /// answered points, which is the unbounded surface this replaces.
    #[test]
    fn a_fix_is_shown_to_the_pass_that_has_to_check_it_and_not_after() {
        let mut ledger = ledger_with("Unbounded loop", "src/x.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Fixed;
            entry.round = 2;
        }
        // Round 3 follows the round that claimed it.
        assert!(answers_block(&ledger, 3).contains("Unbounded loop"));
        // Round 4 does not: round 3 read it and did not raise it again.
        assert_eq!("", answers_block(&ledger, 4));
        assert!(any_fixes(&ledger, 2));
        assert!(!any_fixes(&ledger, 3));
    }

    // -- the review prompt ----------------------------------------------

    /// A round that fixed nine findings left nothing behind, so the next round
    /// met the fix as ordinary code with no sign anybody had asked for it.
    #[test]
    fn the_answers_block_asks_the_reviewer_to_check_rather_than_to_trust() {
        let mut ledger = ledger_with("Unbounded loop", "src/x.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Fixed;
            entry.reasoning = "bounded it on max_attempts".into();
        }
        let block = answers_block(&ledger, 2);
        assert!(block.contains("Unbounded loop"), "{block}");
        assert!(block.contains("src/x.rs"), "{block}");
        assert!(block.contains("bounded it on max_attempts"), "{block}");
        assert!(block.contains("Check the answer"), "{block}");
        assert!(!block.contains("settled"), "{block}");
    }

    /// A refutation is an argument to weigh and a fix is a claim to check, and
    /// the two blocks say opposite things. Neither may carry the other's points.
    #[test]
    fn a_fix_and_a_refutation_do_not_share_a_heading() {
        let mut ledger = ledger_with("refuted point", "a.rs");
        ledger.extend(ledger_with("fixed point", "b.rs"));
        for entry in ledger.values_mut() {
            if entry.title == "fixed point" {
                entry.outcome = Settled::Fixed;
            }
        }
        let answers = answers_block(&ledger, 2);
        let settled = settled_block(&ledger);
        assert!(answers.contains("fixed point") && !answers.contains("refuted point"));
        assert!(settled.contains("refuted point") && !settled.contains("fixed point"));
    }

    #[test]
    fn an_empty_ledger_adds_no_answers_block() {
        assert_eq!("", answers_block(&Ledger::new(), 2));
    }

    /// A point held back for a later round does not get one, so the reviewer is
    /// told which round is the last that can ask for anything.
    #[test]
    fn the_last_round_that_can_ask_for_anything_says_so() {
        assert_eq!("", round_note(1, 3));
        assert_eq!("", round_note(2, 3));
        assert!(round_note(3, 3).contains("last round"));
        // Round numbers keep counting up across a resume, so the last round of
        // an invocation is not round `max_rounds`.
        assert!(round_note(6, 6).contains("last round"));
    }

    /// Telling a reviewer when the asking stops must never tell it to want less.
    /// A reviewer that lowers its bar to finish is the failure this loop was
    /// built against, so the note carries no severity vocabulary at all. That
    /// the pull request may merge afterwards is a fact about the harness, and
    /// saying it is not the same as asking for an approval.
    #[test]
    fn saying_when_the_asking_stops_says_nothing_about_severity() {
        let note = round_note(3, 3);
        for word in ["approve", "blocking", "severity", "nit"] {
            assert!(!note.contains(word), "{word} in: {note}");
        }
    }

    #[test]
    fn the_review_prompt_leaves_nothing_unsubstituted() {
        let empty = review_prompt("main", 42, "Retry a 429", &Ledger::new(), &[], 1, 3);
        assert!(!empty.contains('{'), "{empty}");
        assert!(empty.contains("main") && empty.contains("#42") && empty.contains("Retry a 429"));

        let mut ledger = ledger_with("refuted point", "a.rs");
        ledger.extend(ledger_with("fixed point", "b.rs"));
        for entry in ledger.values_mut() {
            if entry.title == "fixed point" {
                entry.outcome = Settled::Fixed;
                entry.round = 2;
            }
        }
        let full = review_prompt("main", 42, "Retry a 429", &ledger, &[], 3, 3);
        assert!(!full.contains('{'), "{full}");
        assert!(full.contains("fixed point") && full.contains("refuted point"));
        assert!(full.contains("last round"), "{full}");
    }

    #[test]
    fn a_resumed_open_finding_reaches_review_and_closing_prompts() {
        let open = vec![finding(
            "blocking",
            "Retry bypasses the limit",
            "reproduced with max_attempts set to one",
            "src/net.rs:88",
            true,
        )];
        let review = review_prompt("main", 42, "Retry a 429", &Ledger::new(), &open, 2, 3);
        let close = close_prompt(
            "main",
            42,
            "Retry a 429",
            "9f8e7d6",
            Some(&[]),
            &Ledger::new(),
            &open,
            2,
        );
        for prompt in [review, close] {
            assert!(prompt.contains("Retry bypasses the limit"), "{prompt}");
            assert!(prompt.contains("src/net.rs:88"), "{prompt}");
            assert!(
                prompt.contains("reproduced with max_attempts set to one"),
                "{prompt}"
            );
            assert!(!prompt.contains('{'), "{prompt}");
        }
    }

    /// A confirmed defect that is minor had no label but blocking: non-blocking
    /// was defined as an improvement, and nit as taste. Severity gating is the
    /// whole defence against the nitpick spiral, and it had a hole in it.
    #[test]
    fn a_minor_defect_has_a_severity_that_is_not_blocking() {
        assert!(
            REVIEW_PROMPT.contains("A minor defect belongs\n  here as much as an improvement does")
        );
        // The schema is shared with `spar review`, which has no rounds, so it
        // and that prompt carry the same ladder without the round neither can
        // spend. Two definitions of one enum value in one request is how a
        // reviewer ends up applying a cost model that does not exist.
        for text in [
            schema::review().to_string(),
            crate::review_only::review_only_prompt().to_string(),
        ] {
            assert!(text.contains("as much as an improvement does"), "{text}");
            assert!(!text.contains("a genuine improvement"), "{text}");
        }
    }

    /// Doubt used to resolve onto `in_scope = true`, which is half of what gates
    /// a merge. It resolves onto the severity instead, which is not.
    #[test]
    fn doubt_resolves_away_from_the_field_that_gates() {
        for text in [REVIEW_PROMPT.to_string(), schema::review().to_string()] {
            assert!(text.contains("say your piece in the finding and label it non-blocking"));
            assert!(!text.contains("leave in_scope true"));
        }
    }

    /// Every line a fix adds is what the next pass reviews, so a fix that grows
    /// the branch buys another round of findings about the fix.
    #[test]
    fn both_edit_prompts_ask_for_the_smallest_change_that_answers_the_point() {
        for prompt in [FIX_PROMPT, RESPOND_PROMPT] {
            assert!(
                prompt.contains("The smallest change that answers it is"),
                "{prompt}"
            );
        }
        assert!(RESPOND_PROMPT.contains("bigger than the\n  problem it names"));
    }

    #[test]
    fn a_fixed_disposition_must_explain_what_changed() {
        let flat = RESPOND_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            flat.contains("For fixed, say what changed and how it answers the point"),
            "{RESPOND_PROMPT}"
        );
    }

    #[test]
    fn an_empty_fix_reason_still_renders_as_a_claim_to_check() {
        let mut ledger = ledger_with("Unchecked error", "src/net.rs");
        for entry in ledger.values_mut() {
            entry.outcome = Settled::Fixed;
            entry.reasoning.clear();
        }

        let lines = fixed_lines(&ledger, 0);

        assert_eq!(1, lines.len());
        assert!(lines[0].contains("a committed change claims to address this point"));
        assert!(!lines[0].contains("The author said"));
    }

    /// Both, not either. The link is how an agent that can reach the network
    /// reads the discussion spar does not fetch, and the body is what the one
    /// that cannot works from: codex runs with no network, so a link alone
    /// would leave it building from the title.
    #[test]
    fn the_implementor_is_given_the_link_and_the_body() {
        let prompt = implement_prompt(
            42,
            "Retry a 429",
            "https://github.com/o/r/issues/42",
            "A rate limited response was treated as fatal.",
        );
        assert!(
            prompt.contains("https://github.com/o/r/issues/42"),
            "{prompt}"
        );
        assert!(
            prompt.contains("A rate limited response was treated as fatal."),
            "{prompt}"
        );
        assert!(prompt.contains("#42"), "{prompt}");
        assert!(prompt.contains("Retry a 429"), "{prompt}");
        // Nothing left unsubstituted.
        assert!(!prompt.contains('{'), "{prompt}");
    }

    /// An agent that cannot reach the link is told what it is missing, so it
    /// works from the body rather than assuming the body is everything.
    #[test]
    fn the_prompt_says_the_discussion_is_not_included() {
        let prompt = implement_prompt(1, "t", "u", "b");
        // Flattened, so the assertion does not turn on where the prompt wraps.
        let lower = prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            lower.contains("discussion since is not included"),
            "{prompt}"
        );
        assert!(lower.contains("cannot reach the network"), "{prompt}");
    }

    /// A fully reported implementation, for the body tests.
    fn worked() -> Implementation {
        Implementation {
            summary: "Retry a 429 instead of failing the run.".into(),
            problem: "A rate limited response was treated as fatal, so one throttled call ended \
                      a run that had hours of work left in it."
                .into(),
            changes: vec![
                "`send` retries a 429 with the delay the header asks for".into(),
                "the retry budget is bounded, so a permanent 429 still ends".into(),
            ],
            testing: vec![
                "`cargo test retries_a_429`".into(),
                "point it at a throttled endpoint and watch it finish".into(),
            ],
            ..Implementation::default()
        }
    }

    #[test]
    /// GitHub renders the file count and the plus and minus figures in the
    /// header, immediately above whatever spar writes, so neither is here.
    fn a_pr_body_is_what_it_closes_and_what_changed() {
        let body = pr_body(42, &worked(), &style());
        assert_eq!(
            "Closes #42\n\n\
             Retry a 429 instead of failing the run.\n\n\
             A rate limited response was treated as fatal, so one throttled call \
             ended a run that had hours of work left in it.\n\n\
             ## What changed\n\n\
             - `send` retries a 429 with the delay the header asks for\n\
             - the retry budget is bounded, so a permanent 429 still ends\n\n\
             ## How to test\n\n\
             - `cargo test retries_a_429`\n\
             - point it at a throttled endpoint and watch it finish",
            body
        );
    }

    /// The sections are optional and the lead is not. A one line fix should
    /// read as one, not as a form with most of it left blank.
    #[test]
    fn a_body_with_nothing_to_list_carries_no_empty_headings() {
        let work = Implementation {
            summary: "Retry a 429 instead of failing the run.".into(),
            ..Implementation::default()
        };
        assert_eq!(
            "Closes #42\n\nRetry a 429 instead of failing the run.",
            pr_body(42, &work, &style())
        );
    }

    #[test]
    fn a_pr_body_survives_an_implementor_that_said_nothing() {
        assert_eq!(
            "Closes #7",
            pr_body(7, &Implementation::default(), &style())
        );
    }

    /// Blank entries are the model's, not the reader's problem. A heading whose
    /// only bullet was an empty string used to be possible.
    #[test]
    fn blank_list_entries_do_not_earn_a_heading() {
        let work = Implementation {
            summary: "Did a thing.".into(),
            changes: vec![String::new(), "   ".into()],
            ..Implementation::default()
        };
        let body = pr_body(42, &work, &style());
        assert!(!body.contains("What changed"), "{body}");
    }

    #[test]
    fn notes_appear_only_when_there_is_something_to_note() {
        let mut work = worked();
        assert!(!pr_body(42, &work, &style()).contains("## Notes"));
        work.notes = Some("The retry is not applied to streaming calls.".into());
        let body = pr_body(42, &work, &style());
        assert!(body.contains("## Notes"), "{body}");
        assert!(body.contains("streaming calls"), "{body}");
    }

    /// An issue that produced no commits is told so. Never the summary, which
    /// describes a change that is not in the branch.
    #[test]
    fn declining_posts_the_reason_and_not_the_summary() {
        let work = Implementation {
            not_worth_doing: true,
            reason: "Already fixed in 1.2, and the report predates it.".into(),
            summary: "Nothing to do.".into(),
            ..Implementation::default()
        };
        assert_eq!(
            "Already fixed in 1.2, and the report predates it.",
            no_pr_note(&work, &style())
        );
    }

    #[test]
    fn reporting_work_and_committing_none_says_that_rather_than_the_summary() {
        let work = Implementation {
            summary: "Retry a 429 instead of failing the run.".into(),
            ..Implementation::default()
        };
        let note = no_pr_note(&work, &style());
        assert_eq!(
            "Nothing was committed, so there is nothing to review.",
            note
        );
    }

    #[test]
    fn declining_without_a_reason_still_says_something() {
        let work = Implementation {
            not_worth_doing: true,
            ..Implementation::default()
        };
        assert!(no_pr_note(&work, &style()).contains("no reason given"));
    }

    #[test]
    fn a_skip_comment_is_only_the_reasoning() {
        let item = SkippedItem {
            issue: 3,
            title: "t".into(),
            tracker: false,
            reasons: [
                ("claude".to_string(), "Already fixed in 1.2.".to_string()),
                ("codex".to_string(), "Duplicate of #2.".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let text = skip_comment(&item, &style());
        assert!(text.contains("Already fixed in 1.2."), "{text}");
        assert!(text.contains("Duplicate of #2."), "{text}");
        assert!(
            !text.contains("claude") && !text.contains("codex"),
            "{text}"
        );
        assert!(!text.to_lowercase().contains("not scheduled"), "{text}");
        assert!(text.lines().count() <= 3, "{text}");
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

#[cfg(test)]
mod outcome_tests {
    use super::*;
    use crate::model::{Dispute, Severity};

    fn style() -> Style {
        Style::default()
    }

    fn state_with(disputes: Vec<(&str, &str)>, filed: Vec<&str>) -> IssueRun {
        let mut s = IssueRun::new(482, "t");
        s.disputes = disputes
            .into_iter()
            .map(|(title, reasoning)| Dispute {
                title: title.into(),
                file: String::new(),
                reasoning: reasoning.into(),
            })
            .collect();
        s.filed = filed.into_iter().map(String::from).collect();
        s
    }

    fn finding(title: &str, file: &str) -> Finding {
        Finding {
            severity: Severity::Blocking,
            title: title.into(),
            detail: "d".into(),
            file: file.into(),
            in_scope: true,
            ..Default::default()
        }
    }

    fn graded(severity: Severity, title: &str, file: &str, in_scope: bool) -> Finding {
        Finding {
            severity,
            in_scope,
            ..finding(title, file)
        }
    }

    #[test]
    fn outcome_mode_routes_final_results_to_the_configured_sink() {
        assert_eq!(OutcomeSink::PullRequest, outcome_sink(PrComments::Outcome));
        assert_eq!(OutcomeSink::PullRequest, outcome_sink(PrComments::Rounds));
        assert_eq!(OutcomeSink::Terminal, outcome_sink(PrComments::None));
    }

    /// The absence of objections is the message. A PR that reviewed cleanly and
    /// filed nothing should leave no trace in the thread at all.
    #[test]
    fn a_clean_approval_says_nothing() {
        let state = state_with(vec![], vec![]);
        assert!(outcome_comment(&state, &Ledger::new(), &Ending::Approved, &style()).is_none());
    }

    /// Widening the ladder makes downgrading the easy answer, and under the
    /// defaults a non-blocking finding is filed nowhere and commented nowhere.
    /// Without this, a reviewer could make a real defect disappear by relabelling
    /// it, and the pull request would look exactly like a clean one.
    #[test]
    fn a_downgraded_finding_still_reaches_the_pull_request() {
        let mut state = state_with(vec![], vec![]);
        let kept = graded(
            Severity::NonBlocking,
            "Timeout is not configurable",
            "n.rs",
            true,
        );
        record_nonblocking_outcome(&mut state, &kept, None);

        // A nit is taste, and an out of scope point is filed rather than noted.
        // Neither belongs in a list a person reads for what was let through.
        assert_eq!(1, state.noted.len());

        let text = outcome_comment(&state, &Ledger::new(), &Ending::Approved, &style()).unwrap();
        assert!(text.contains("Noted, not blocking"), "{text}");
        assert!(
            text.contains("Timeout is not configurable (n.rs)"),
            "{text}"
        );
    }

    /// With follow-ups on, every one of these is already an issue and already
    /// named under "Filed separately". Two headings for one point reads as two.
    #[test]
    fn a_point_that_was_filed_is_not_also_noted() {
        let mut state = state_with(vec![], vec![]);
        let finding = graded(Severity::NonBlocking, "Timeout", "n.rs", true);
        record_nonblocking_outcome(&mut state, &finding, None);
        record_nonblocking_outcome(
            &mut state,
            &finding,
            Some(&Followup::Recorded("https://example.invalid/9".into())),
        );
        assert!(state.noted.is_empty());
        assert_eq!(vec!["https://example.invalid/9"], state.filed);
    }

    #[test]
    fn filing_a_moved_point_removes_its_earlier_note() {
        let mut state = state_with(vec![], vec![]);
        let earlier = graded(Severity::NonBlocking, "Timeout", "src/net.rs:10", true);
        let moved = graded(Severity::NonBlocking, "Timeout", "src/net.rs:12", false);
        record_nonblocking_outcome(&mut state, &earlier, None);

        record_nonblocking_outcome(
            &mut state,
            &moved,
            Some(&Followup::Recorded("https://example.invalid/10".into())),
        );

        assert!(state.noted.is_empty());
        assert_eq!(vec!["https://example.invalid/10"], state.filed);
    }

    #[test]
    fn an_unrecorded_nonblocking_followup_remains_noted() {
        let finding = graded(Severity::NonBlocking, "Timeout", "n.rs", true);
        for outcome in [
            Followup::Covered("https://example.invalid/closed".into()),
            Followup::Dropped("follow-ups are off"),
            Followup::Failed,
        ] {
            let mut state = state_with(vec![], vec![]);
            record_nonblocking_outcome(&mut state, &finding, Some(&outcome));
            assert_eq!(1, state.noted.len(), "{outcome:?}");
            assert!(state.filed.is_empty(), "{outcome:?}");
        }
    }

    /// The same point raised again in a later round is one point, not three.
    #[test]
    fn a_point_noted_twice_is_listed_once() {
        let mut state = state_with(vec![], vec![]);
        let raised = graded(
            Severity::NonBlocking,
            "Timeout is not configurable",
            "n.rs",
            true,
        );
        let reworded = graded(
            Severity::NonBlocking,
            "timeout is not configurable!",
            "n.rs",
            true,
        );
        record_nonblocking_outcome(&mut state, &raised, None);
        record_nonblocking_outcome(&mut state, &reworded, None);
        assert_eq!(1, state.noted.len());
    }

    #[test]
    fn same_title_notes_in_different_files_are_both_kept() {
        let mut state = state_with(vec![], vec![]);
        for file in ["src/a.rs", "src/b.rs"] {
            let finding = graded(Severity::NonBlocking, "Unchecked error", file, true);
            record_nonblocking_outcome(&mut state, &finding, None);
        }
        assert_eq!(2, state.noted.len());
    }

    #[test]
    fn same_title_notes_at_two_sites_in_one_review_are_both_kept() {
        let findings = vec![
            graded(
                Severity::NonBlocking,
                "Unchecked error",
                "src/a.rs:10",
                true,
            ),
            graded(
                Severity::NonBlocking,
                "Unchecked error",
                "src/a.rs:200",
                true,
            ),
        ];
        let mut state = state_with(vec![], vec![]);

        for finding in &findings {
            record_nonblocking_outcome_with_match(
                &mut state,
                finding,
                None,
                unique_stable_finding(&findings, finding),
            );
        }

        assert_eq!(2, state.noted.len());
    }

    #[test]
    fn settling_one_of_two_same_title_notes_keeps_the_other() {
        let first = graded(
            Severity::NonBlocking,
            "Unchecked error",
            "src/a.rs:10",
            true,
        );
        let second = graded(
            Severity::NonBlocking,
            "Unchecked error",
            "src/a.rs:200",
            true,
        );
        let mut state = state_with(vec![], vec![]);
        remember_noted(&mut state, &first, false);
        remember_noted(&mut state, &second, false);

        forget_noted(&mut state, &first, false);

        assert_eq!(1, state.noted.len());
        assert_eq!("src/a.rs:200", state.noted[0].file);
    }

    #[test]
    fn same_title_disputes_at_two_sites_are_both_kept() {
        let mut state = state_with(vec![], vec![]);
        for file in ["src/a.rs:10", "src/a.rs:200"] {
            remember_dispute(
                &mut state,
                Dispute {
                    title: "Unchecked error".into(),
                    file: file.into(),
                    reasoning: "the caller handles it".into(),
                },
                false,
            );
        }

        assert_eq!(2, state.disputes.len());
    }

    #[test]
    fn a_later_nonblocking_verdict_replaces_a_prior_dispute() {
        let mut state = state_with(vec![], vec![]);
        let finding = graded(Severity::NonBlocking, "Unchecked error", "src/a.rs", true);
        remember_dispute(
            &mut state,
            Dispute {
                title: finding.title.clone(),
                file: finding.file.clone(),
                reasoning: "the caller handles it".into(),
            },
            true,
        );

        record_nonblocking_outcome(&mut state, &finding, None);

        assert!(state.disputes.is_empty());
        assert_eq!(1, state.noted.len());
    }

    #[test]
    fn a_note_moving_lines_in_the_same_file_is_updated() {
        let mut state = state_with(vec![], vec![]);
        let first = graded(
            Severity::NonBlocking,
            "Unchecked error",
            "src/a.rs:12",
            true,
        );
        let moved = graded(
            Severity::NonBlocking,
            "Unchecked error",
            "src/a.rs:19",
            true,
        );
        record_nonblocking_outcome(&mut state, &first, None);
        record_nonblocking_outcome(&mut state, &moved, None);
        assert_eq!(1, state.noted.len());
        assert_eq!("src/a.rs:19", state.noted[0].file);
    }

    #[test]
    fn a_settled_point_removes_its_stale_note() {
        for outcome in [Settled::Fixed, Settled::Refuted, Settled::Filed] {
            let noted = graded(Severity::NonBlocking, "Unchecked error", "src/a.rs", true);
            let mut state = state_with(vec![], vec![]);
            record_nonblocking_outcome(&mut state, &noted, None);
            forget_noted(&mut state, &noted, true);
            assert!(state.noted.is_empty(), "{outcome}");
        }
    }

    #[test]
    fn settling_a_moved_point_removes_its_old_dispute() {
        let old = graded(Severity::Blocking, "Unchecked error", "src/a.rs:10", true);
        let moved = graded(Severity::Blocking, "Unchecked error", "src/a.rs:12", true);
        let mut state = state_with(vec![], vec![]);
        remember_dispute(
            &mut state,
            Dispute {
                title: old.title,
                file: old.file,
                reasoning: "the caller handles it".into(),
            },
            true,
        );

        forget_dispute(&mut state, &moved, true);

        assert!(state.disputes.is_empty());
    }

    /// A point already printed with its argument attached is not printed again
    /// under a second heading.
    #[test]
    fn a_deadlocked_point_is_not_also_noted() {
        let mut state = state_with(vec![], vec![]);
        let noted = graded(Severity::NonBlocking, "Unbounded loop", "x.rs", true);
        record_nonblocking_outcome(&mut state, &noted, None);
        let points = vec![finding("Unbounded loop", "x.rs")];
        let text = outcome_comment(
            &state,
            &Ledger::new(),
            &Ending::Deadlocked(&points),
            &style(),
        )
        .unwrap();
        assert!(!text.contains("Noted, not blocking"), "{text}");
    }

    #[test]
    fn clipping_a_rendered_title_does_not_break_duplicate_suppression() {
        let mut compact = style();
        compact.max_title_chars = 5;
        let mut state = state_with(
            vec![("abcdefghij", "the caller already handles it")],
            vec![],
        );
        state
            .noted
            .push(graded(Severity::NonBlocking, "abcdefghij", "x.rs", true));
        state.disputes[0].file = "x.rs".into();
        let points = vec![finding("abcdefghij", "x.rs")];

        let text = outcome_comment(
            &state,
            &Ledger::new(),
            &Ending::Unresolved(&points),
            &compact,
        )
        .unwrap();

        assert!(!text.contains("Raised and refuted"), "{text}");
        assert!(!text.contains("Noted, not blocking"), "{text}");
    }

    #[test]
    fn an_approval_that_filed_follow_ups_links_them() {
        let state = state_with(
            vec![],
            vec![
                "https://github.com/you/thing/issues/485",
                "https://github.com/you/thing/issues/486",
            ],
        );
        let text = outcome_comment(&state, &Ledger::new(), &Ending::Approved, &style()).unwrap();
        assert!(text.contains("Filed separately: #485, #486"), "{text}");
    }

    /// The skip path is taken when nothing landed, so the sentence about fixes
    /// that were pushed and not read is false there. Sending a maintainer to
    /// read a commit that does not exist is worse than saying nothing.
    #[test]
    fn a_run_that_changed_nothing_does_not_claim_there_is_something_to_read() {
        let state = state_with(vec![], vec![]);
        let text = outcome_comment(&state, &Ledger::new(), &Ending::Unchanged, &style()).unwrap();
        assert!(text.contains("changed nothing"), "{text}");
        assert!(!text.contains("was pushed"), "{text}");
    }

    /// Telling a maintainer that the last round was pushed and nobody read it
    /// gives them nothing they can act on. What is left, with where it is, is
    /// three lines and a decision.
    #[test]
    fn an_unresolved_close_names_what_is_still_wrong() {
        let state = state_with(vec![], vec![]);
        let left = vec![Finding {
            detail: "The guard sits after the early return.".into(),
            ..finding("The retry fix never reaches the 429 path", "src/net.rs:88")
        }];
        let text =
            outcome_comment(&state, &Ledger::new(), &Ending::Unresolved(&left), &style()).unwrap();
        assert!(text.contains("These points are still open"), "{text}");
        assert!(
            text.contains("The retry fix never reaches the 429 path (src/net.rs:88)"),
            "{text}"
        );
        assert!(
            text.contains("The guard sits after the early return."),
            "{text}"
        );
        // The sentence the budget used to end on, which is now only true when
        // the closing pass could not run at all.
        assert!(!text.contains("has not been reviewed"), "{text}");
    }

    /// The real PR ended with "5 fixed" followed by "no convergence", which
    /// reads as a contradiction. What a maintainer needs is that the fixes went
    /// in and nobody checked them.
    #[test]
    fn running_out_of_rounds_says_what_that_means_for_the_reader() {
        let state = state_with(vec![], vec![]);
        let text = outcome_comment(&state, &Ledger::new(), &Ending::OutOfRounds, &style()).unwrap();
        assert!(text.contains("has not been reviewed"), "{text}");
        assert!(
            !text.to_lowercase().contains("round 3"),
            "no round numbers: {text}"
        );
        assert!(!text.to_lowercase().contains("convergence"), "{text}");
    }

    #[test]
    fn a_failed_close_reports_unread_fixes_and_carried_blockers() {
        let state = state_with(vec![], vec![]);
        let open = vec![Finding {
            detail: "the failure is still discarded".into(),
            ..finding("Unchecked error", "src/net.rs:88")
        }];
        let text = outcome_comment_with_unread(
            &state,
            &Ledger::new(),
            &Ending::OutOfRounds,
            &open,
            &style(),
        )
        .unwrap();
        assert!(text.contains("has not been reviewed"), "{text}");
        assert!(text.contains("These points were already open"), "{text}");
        assert!(text.contains("Unchecked error (src/net.rs:88)"), "{text}");
    }

    #[test]
    fn a_deadlock_names_the_point_they_could_not_settle() {
        let state = state_with(vec![], vec![]);
        let points = [finding("Retry loop never terminates", "src/net.rs:88")];
        let text = outcome_comment(
            &state,
            &Ledger::new(),
            &Ending::Deadlocked(&points),
            &style(),
        )
        .unwrap();
        assert!(
            text.contains("Retry loop never terminates (src/net.rs:88)"),
            "{text}"
        );
        assert!(text.contains("could not settle"), "{text}");
    }

    /// The diff records what was fixed. Nothing records what was argued down.
    #[test]
    fn refutations_survive_because_nothing_else_carries_them() {
        let state = state_with(
            vec![(
                "Error is swallowed",
                "the caller already validates the file",
            )],
            vec![],
        );
        let text = outcome_comment(&state, &Ledger::new(), &Ending::Approved, &style()).unwrap();
        assert!(text.contains("Raised and refuted:"), "{text}");
        assert!(
            text.contains("The caller already validates the file"),
            "{text}"
        );
    }

    #[test]
    fn no_agent_names_counts_or_round_numbers_reach_the_thread() {
        let state = state_with(
            vec![("A point", "a reason")],
            vec!["https://github.com/you/thing/issues/485"],
        );
        let left = vec![finding("A point", "a.rs")];
        for ending in [
            Ending::Approved,
            Ending::OutOfRounds,
            Ending::Unresolved(&left),
        ] {
            let text = outcome_comment(&state, &Ledger::new(), &ending, &style()).unwrap();
            let lower = text.to_lowercase();
            for banned in ["claude", "codex", "blocking,", "nit,", " fixed."] {
                assert!(
                    !lower.contains(banned),
                    "{banned:?} leaked into the thread:\n{text}"
                );
            }
            // "the last round of fixes" is prose. "round 3" is narration.
            for n in 1..9 {
                assert!(
                    !lower.contains(&format!("round {n}")),
                    "a round number leaked into the thread:\n{text}"
                );
            }
        }
    }

    #[test]
    /// A refutation is an argument, and an argument that stops mid clause is
    /// not one. Bounded, but with room to make the case.
    fn a_refutation_is_allowed_to_make_its_case() {
        let reasoning = "The caller validates against the schema first. \
                         The discarded error is therefore unreachable in practice. ";
        let state = state_with(
            vec![("A point", &reasoning.repeat(6))],
            vec!["https://github.com/you/thing/issues/485"],
        );
        let text = outcome_comment(&state, &Ledger::new(), &Ending::OutOfRounds, &style()).unwrap();
        assert!(
            !text.contains("..."),
            "nothing was cut mid thought:\n{text}"
        );
        assert!(text.len() < 4000, "{} chars", text.len());
    }

    #[test]
    fn a_url_that_is_not_an_issue_link_is_left_alone() {
        assert_eq!(
            "#485",
            as_reference("https://github.com/you/thing/issues/485")
        );
        assert_eq!("note: something", as_reference("note: something"));
    }
}

#[cfg(test)]
mod filed_reference_tests {
    use super::*;

    #[test]
    fn an_issue_url_yields_its_number() {
        assert_eq!(
            Some(485),
            filed_issue_number("https://github.com/you/thing/issues/485")
        );
    }

    /// Local mode records a note rather than a URL, and a run with
    /// followups = "local" must not try to absorb it as an issue.
    #[test]
    fn a_local_note_yields_nothing() {
        assert_eq!(None, filed_issue_number("note: Retry is unbounded"));
        assert_eq!(None, filed_issue_number(""));
        assert_eq!(
            None,
            filed_issue_number("https://github.com/you/thing/issues/")
        );
    }
}

#[cfg(test)]
mod followup_restraint_tests {
    use super::*;
    use crate::model::Severity;

    fn cfg_with(followups: Followups, non_blocking: bool, nits: bool, cap: usize) -> Config {
        let mut cfg =
            crate::config::parse("[agents.a]\ncommand = [\"x\"]\n[agents.b]\ncommand = [\"y\"]\n")
                .unwrap();
        cfg.loop_cfg.followups = followups;
        cfg.loop_cfg.file_non_blocking = non_blocking;
        cfg.loop_cfg.file_nits = nits;
        cfg.loop_cfg.max_followups = cap;
        cfg
    }

    fn finding(severity: Severity, title: &str, in_scope: bool) -> Finding {
        Finding {
            severity,
            title: title.into(),
            detail: "d".into(),
            file: "a.rs".into(),
            in_scope,
            ..Default::default()
        }
    }

    /// The defaults are what let one issue spawn ten, which spawned more. A
    /// thorough reviewer always finds improvements; not gating a merge is not
    /// the same as deserving somebody's triage queue.
    #[test]
    fn a_non_blocking_finding_is_not_a_tracker_item_by_default() {
        let cfg = cfg_with(Followups::Issues, false, false, 5);
        assert!(!cfg.loop_cfg.file_non_blocking);
        assert!(!cfg.loop_cfg.file_nits);
    }

    #[test]
    fn follow_ups_stay_off_the_tracker_by_default() {
        let cfg =
            crate::config::parse("[agents.a]\ncommand = [\"x\"]\n[agents.b]\ncommand = [\"y\"]\n")
                .unwrap();
        assert_eq!(
            Followups::Local,
            cfg.loop_cfg.followups,
            "the tracker is somebody's queue; the default must not write to it"
        );
        assert_eq!(5, cfg.loop_cfg.max_followups);
    }

    /// Which severities survive the filter, at the defaults and when opened up.
    #[test]
    fn only_out_of_scope_defects_qualify_at_the_defaults() {
        let cfg = cfg_with(Followups::Issues, false, false, 5);
        let qualifies = |f: &Finding| match f.severity {
            Severity::NonBlocking => cfg.loop_cfg.file_non_blocking && f.in_scope,
            Severity::Nit => cfg.loop_cfg.file_nits && f.in_scope,
            Severity::Blocking => false,
        } || !f.in_scope;

        assert!(qualifies(&finding(
            Severity::Blocking,
            "pre-existing",
            false
        )));
        assert!(!qualifies(&finding(
            Severity::NonBlocking,
            "improvement",
            true
        )));
        assert!(!qualifies(&finding(Severity::Nit, "taste", true)));
        assert!(!qualifies(&finding(
            Severity::Blocking,
            "fix it here",
            true
        )));
    }

    #[test]
    fn opening_it_up_lets_non_blocking_findings_through_again() {
        let cfg = cfg_with(Followups::Issues, true, false, 5);
        assert!(cfg.loop_cfg.file_non_blocking);
    }

    /// A run that will not stop finding things is stopped, and says so.
    #[test]
    fn the_cap_is_a_real_backstop() {
        let cfg = cfg_with(Followups::Issues, false, false, 3);
        let mut state = IssueRun::new(1, "t");
        state.filed = (0..3).map(|n| format!("url{n}")).collect();
        assert!(state.filed.len() >= cfg.loop_cfg.max_followups);
    }

    /// The number that matters. Reviewing one issue produced ten follow-ups on
    /// a real repository, each of which could be run in turn: mean offspring
    /// above one never terminates.
    #[test]
    fn the_cap_bounds_what_one_run_can_spawn() {
        let cfg = cfg_with(Followups::Issues, false, false, 5);
        assert!(
            cfg.loop_cfg.max_followups <= 5,
            "a run that can file ten follow-ups is a branching process"
        );
    }
}

/// What the ledger is told about a point the author moved out of the pull
/// request. Every case here used to record "filed", including the ones where
/// nothing was written anywhere.
#[cfg(test)]
mod followup_outcome_tests {
    use super::*;

    const URL: &str = "https://github.com/you/thing/issues/485";

    fn entry(recorded: Followup) -> Option<(Settled, String)> {
        filed_entry(&recorded, "It predates this branch.")
    }

    /// The bug. A tracker request or a local write that failed left no
    /// follow-up, and the ledger said it had been filed, which is a claim that
    /// survives every later round and every resume.
    #[test]
    fn a_failed_followup_settles_nothing() {
        assert_eq!(None, entry(Followup::Failed));
    }

    #[test]
    fn an_uncertain_external_write_blocks_later_issue_followups_for_this_run() {
        let mut state = IssueRun::new(1, "review");
        let uncertain = SparError::uncertain_write("the result could not be verified");
        assert_eq!(Followup::Failed, failed_followup(&mut state, &uncertain));
        assert!(external_followup_write_paused(Followups::Issues, &state));
        assert!(!external_followup_write_paused(Followups::Local, &state));
        assert_eq!(1, state.notes.len());

        assert_eq!(Followup::Failed, failed_followup(&mut state, &uncertain));
        assert_eq!(1, state.notes.len(), "the recovery note was duplicated");

        let mut ordinary = IssueRun::new(2, "review");
        let error = SparError::new("permission denied");
        assert_eq!(Followup::Failed, failed_followup(&mut ordinary, &error));
        assert!(!external_followup_write_paused(
            Followups::Issues,
            &ordinary
        ));
    }

    #[test]
    fn a_recorded_followup_is_filed_and_says_where() {
        let (outcome, reasoning) = entry(Followup::Recorded(URL.into())).unwrap();
        assert_eq!(Settled::Filed, outcome);
        assert!(
            reasoning.contains("It predates this branch."),
            "{reasoning}"
        );
        assert!(reasoning.contains("#485"), "{reasoning}");
    }

    /// A closed issue already carries the point, so raising it again is waste.
    /// It is still not something to hand anybody as work.
    #[test]
    fn a_closed_issue_covering_the_point_settles_it_without_offering_work() {
        let recorded = Followup::from(Filed::AlreadyClosed(9, URL.into()));
        assert_eq!(Followup::Covered(URL.into()), recorded);
        assert_eq!(
            None,
            recorded.url(),
            "a closed issue is not work to pick up"
        );

        let (outcome, reasoning) = entry(recorded).unwrap();
        assert_eq!(Settled::Filed, outcome);
        assert!(reasoning.contains("#485"), "{reasoning}");
    }

    /// An open issue that already covers the point is worth linking from the
    /// pull request, and worth counting against the cap.
    #[test]
    fn an_open_issue_that_already_covers_the_point_is_still_a_reference() {
        for filed in [
            Filed::Opened(9, URL.into()),
            Filed::AddedTo(9, URL.into()),
            Filed::Covered(9, URL.into()),
        ] {
            assert_eq!(Some(URL), Followup::from(filed).url());
        }
    }

    /// Configuration, not failure: retrying it every round would spend the
    /// budget on a write that is never going to happen. The entry has to be
    /// honest about it, because nothing else holds the point.
    #[test]
    fn a_dropped_followup_is_settled_but_never_reported_as_filed() {
        let (outcome, reasoning) = entry(Followup::Dropped("follow-ups are off")).unwrap();
        assert_eq!(Settled::Dropped, outcome);
        assert!(reasoning.contains("follow-ups are off"), "{reasoning}");
        assert!(reasoning.contains("Not filed"), "{reasoning}");
    }

    fn ledger_of(outcome: Settled, reasoning: &str) -> Ledger {
        let mut ledger = Ledger::new();
        ledger.insert(
            finding_key("A pre-existing leak", "src/x.rs"),
            LedgerEntry {
                title: "A pre-existing leak".into(),
                file: "src/x.rs".into(),
                reasoning: reasoning.into(),
                round: 1,
                reraised: 0,
                outcome,
            },
        );
        ledger
    }

    /// The next reviewer is told to leave settled points alone either way, so
    /// the wording is all that separates them. Saying "filed" of a point
    /// nothing holds is the lie that loses it.
    #[test]
    fn the_settled_block_tells_a_filed_point_from_a_dropped_one() {
        let filed = settled_block(&ledger_of(Settled::Filed, "Tracked in #9."));
        assert!(filed.contains("out of scope here, and filed"), "{filed}");

        let dropped = settled_block(&ledger_of(Settled::Dropped, "Not filed anywhere: off."));
        assert!(
            dropped.contains("out of scope here, and not filed"),
            "{dropped}"
        );
        assert!(dropped.contains("A pre-existing leak"), "{dropped}");
    }

    /// A deadlock goes to a person, and the first thing they do is look for the
    /// issue the comment says exists.
    #[test]
    fn a_deadlocked_point_that_was_never_filed_does_not_claim_to_be() {
        let points = [Finding {
            severity: Severity::Blocking,
            title: "A pre-existing leak".into(),
            detail: "d".into(),
            file: "src/x.rs".into(),
            in_scope: false,
            ..Default::default()
        }];
        let text = outcome_comment(
            &IssueRun::new(1, "t"),
            &ledger_of(Settled::Dropped, "Not filed anywhere: follow-ups are off."),
            &Ending::Deadlocked(&points),
            &Style::default(),
        )
        .unwrap();
        assert!(text.contains("not filed"), "{text}");
        assert!(!text.contains("Filed as out of scope"), "{text}");
    }
}

#[cfg(test)]
mod issue_report_tests {
    use super::*;
    use crate::model::Severity;

    /// Shaped after a bug report written by hand that reads the way one should:
    /// what is wrong, how to see it, what it costs, what it should do instead.
    fn reported() -> Finding {
        Finding {
            severity: Severity::Blocking,
            title: "sendPaymentAsync bypasses drain mode and spending limits".into(),
            detail: "The async path skips every admission check payInvoice applies.".into(),
            file: "src/node.ts:412".into(),
            in_scope: false,
            problem: Some(
                "`BeignetNode.sendPaymentAsync()` submits a payment directly to the Lightning \
                 engine without applying the safeguards used by `payInvoice()`.\n\nThe async path \
                 does not:\n\n- call `_checkDraining()`\n- call `_checkSpendLimit()`"
                    .into(),
            ),
            reproduction: Some(
                "1. Create a `BeignetNode` with `dailySpendLimitSats: 1`.\n2. Enable drain mode.\n\
                 3. Submit a 1,000 sat invoice.\n\nActual result:\n\n- The engine is called.\n\
                 - `spentSats` remains 0."
                    .into(),
            ),
            impact: Some(
                "An authorized client can submit async payments up to the available outbound \
                 liquidity despite the configured limits."
                    .into(),
            ),
            expected: Some(
                "- Reject new payments while draining.\n- Enforce the per-payment limit before \
                 submission.\n- Cover both paths with regression tests.\n\nThis predates the \
                 current branch."
                    .into(),
            ),
        }
    }

    #[test]
    fn a_reported_finding_becomes_a_bug_report() {
        let body = issue_report(&reported());
        for heading in [
            "## Problem",
            "## Reproduction",
            "## Impact",
            "## Expected behavior",
        ] {
            assert!(body.contains(heading), "missing {heading}:\n{body}");
        }
        // In the order somebody reads a bug report.
        let at = |h: &str| body.find(h).unwrap();
        assert!(at("## Problem") < at("## Reproduction"));
        assert!(at("## Reproduction") < at("## Impact"));
        assert!(at("## Impact") < at("## Expected behavior"));
    }

    #[test]
    fn the_substance_survives_the_outbound_gates() {
        let repo_style = Style::default();
        let body = crate::style::issue_body(&issue_report(&reported()), &repo_style);
        for kept in [
            "_checkDraining()",
            "Actual result:",
            "outbound liquidity",
            "regression tests",
            "predates the current branch",
        ] {
            assert!(body.contains(kept), "the gate ate {kept:?}:\n{body}");
        }
        assert!(!body.contains("..."), "something was cut:\n{body}");
    }

    /// A finding that was never going to be filed carries none of this, and
    /// must not gain empty headings for the sake of a format.
    #[test]
    fn an_ordinary_finding_is_still_just_its_detail() {
        let plain = Finding {
            severity: Severity::NonBlocking,
            title: "Name is vague".into(),
            detail: "The variable could say what it holds.".into(),
            file: "a.rs".into(),
            in_scope: true,
            ..Default::default()
        };
        assert_eq!(
            "The variable could say what it holds.",
            issue_report(&plain)
        );
    }

    /// Partial reports are normal: a defect with no useful reproduction should
    /// not sprout an empty Reproduction heading.
    #[test]
    fn only_the_sections_that_were_written_appear() {
        let partial = Finding {
            problem: Some("The guard is inverted.".into()),
            expected: Some("It should reject rather than accept.".into()),
            ..reported()
        };
        let partial = Finding {
            reproduction: None,
            impact: None,
            ..partial
        };
        let body = issue_report(&partial);
        assert!(body.contains("## Problem") && body.contains("## Expected behavior"));
        assert!(!body.contains("## Reproduction"), "{body}");
        assert!(!body.contains("## Impact"), "{body}");
    }

    /// The one line the thread shows is not repeated when a section already
    /// says it.
    #[test]
    fn the_summary_line_is_not_printed_twice() {
        let echoed = Finding {
            detail: "The guard is inverted so it rejects valid input.".into(),
            problem: Some("The guard is inverted so it rejects valid input.".into()),
            reproduction: None,
            impact: None,
            expected: None,
            ..reported()
        };
        let body = issue_report(&echoed);
        assert_eq!(1, body.matches("The guard is inverted").count(), "{body}");
    }
}
