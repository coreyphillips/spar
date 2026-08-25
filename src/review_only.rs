//! Reviewing a pull request without touching it.
//!
//! The custody loop in [`crate::review`] converges because the diff changes
//! between rounds: a reviewer objects, an author fixes, the next review sees
//! something new. Here nothing changes. The pull request belongs to somebody
//! else, spar has no write access to it, and the product is the finding list
//! rather than a commit.
//!
//! So the loop is not "review until it converges", which would just re-litigate
//! the same unchanged code until the budget ran out. It is three phases:
//!
//! 1. **Independent review.** Both agents review at the same time, neither
//!    seeing the other. A finding both reach on their own is the strongest
//!    signal available, and it costs nothing extra to look for.
//! 2. **Cross-adjudication.** Each reads the other's remaining findings, goes
//!    to the code, and rules on them. A finding one model raised and the other
//!    examined and rejected is usually pattern matching, and saying so is more
//!    useful to a maintainer than forwarding both.
//! 3. **Rebuttal.** Anything rejected goes back to whoever raised it, to
//!    withdraw or to substantiate with the line, the input, the failing case.
//!
//! What survives is sorted by how well it is attested, and anything the two
//! still disagree about is handed to a person rather than resolved by fiat.

use std::path::Path;

use crate::agent::Agent;
use crate::config::Config;
use crate::error::Result;
use crate::jsonx::finding_key;
use crate::model::{
    AdjudicationDoc, Finding, IssueRun, Judged, PrView, Review, Severity, Standing, Status,
};
use crate::repo::Repo;
use crate::style::{self, Style};
use crate::{log, logdim, schema, spar_err};

const REVIEW_ONLY_PROMPT: &str = "\
Review pull request #{number} against `{base}`: {title}

You are reviewing somebody else's work. Your checkout is detached and read only.
Do not modify, commit, or push anything. The only thing you produce is findings.

Review thoroughly: correctness, edge cases, error handling, security, and
whether the change actually does what it claims. Read the surrounding code, do
not only read the diff.

Label every finding by severity, and be honest about which is which:
- blocking: this should not merge as is. Real defects only.
- non-blocking: a genuine improvement that need not gate the merge.
- nit: style or taste.

Confirm anything you label blocking before you label it. Run the code, reproduce
the failure, or point at the exact line that breaks, and say in the detail what
you did to confirm it. Someone else's contribution is on the other end of this.
An unverified blocking finding costs them a round trip and costs the maintainer
their credibility, so if you suspect a problem but could not confirm it, say so
and label it non-blocking.

Set in_scope=false for a real problem that exists but is not caused by this pull
request. next_action is not used in this mode; set it to hand_back.";

const ADJUDICATE_PROMPT: &str = "\
Another reviewer examined this same pull request and raised the findings below.
You have already reviewed it yourself.

For each one, go to the code at the location given and rule on it.

Agree only if you read the code and confirmed the defect is real. Do not defer
to the other reviewer, and do not agree in order to be agreeable. A finding you
cannot confirm wastes the contributor's time and the maintainer's, which is the
thing this whole exercise exists to protect. Disagreeing with a reason is the
most useful thing you can do here.

Give your own severity even where you agree the defect is real: the other
reviewer calling something blocking does not make it so.

Findings:
{findings}";

const REBUT_PROMPT: &str = "\
You raised the findings below. The other reviewer went to the code and rejected
each one, for the reason given under it.

For each, set agrees=true only if you stand by the finding, and then give the
specific evidence that settles it: the line, the input, the failing case. Set
agrees=false to withdraw it, which is the right answer when they are correct.

Withdrawing costs nothing. Defending a point you cannot substantiate puts it in
front of a maintainer with two reviewers' names on it, which is worse than never
having raised it.

Findings, with the objection to each:
{findings}";

/// Review a pull request without changing it.
pub fn review_pr(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr_number: i64,
    dry_run: bool,
) -> IssueRun {
    match review_inner(agents, cfg, repo, pr_number, dry_run) {
        Ok(state) => state,
        Err(e) => {
            log!("PR #{pr_number} review failed: {e}");
            let mut state = IssueRun::new(pr_number, format!("PR #{pr_number}"));
            state.status = Status::Error;
            state.notes.push(e.to_string());
            state
        }
    }
}

fn review_inner(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr_number: i64,
    dry_run: bool,
) -> Result<IssueRun> {
    let pr: PrView = repo.pr_view(pr_number)?;
    if !pr.is_open() {
        return Err(spar_err!("PR #{pr_number} is {}", pr.state.to_lowercase()));
    }
    let base = if pr.base_ref_name.trim().is_empty() {
        cfg.base_branch().to_string()
    } else {
        pr.base_ref_name.clone()
    };

    let mut state = IssueRun::new(pr_number, pr.title.clone());
    state.pr = Some(pr.url.clone());

    let work_dir = repo.worktree_for_pr_head(pr_number)?;
    let outcome = run_phases(
        agents, cfg, repo, &pr, &base, &work_dir, &mut state, dry_run,
    );
    repo.release_review_worktree(pr_number);
    outcome?;
    Ok(state)
}

#[allow(clippy::too_many_arguments)]
fn run_phases(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    pr: &PrView,
    base: &str,
    work_dir: &Path,
    state: &mut IssueRun,
    dry_run: bool,
) -> Result<()> {
    let budget = cfg.loop_cfg.max_rounds;

    // -- phase 1: two independent reviews, at the same time ---------------
    log!(
        "PR #{}: {} reviewing independently",
        pr.number,
        agents
            .iter()
            .map(Agent::name)
            .collect::<Vec<_>>()
            .join(" and ")
    );
    let prompt = REVIEW_ONLY_PROMPT
        .replace("{number}", &pr.number.to_string())
        .replace("{base}", base)
        .replace("{title}", &pr.title);

    let reviews = concurrently(agents, |a| {
        let effort = cfg.effort_for_round(&a.spec, 1);
        a.review::<Review>(
            base,
            &prompt,
            &schema::review(),
            work_dir,
            effort.as_deref(),
        )
    });

    let mut by_agent: Vec<(String, Vec<Finding>)> = Vec::new();
    for (name, result) in reviews {
        match result {
            Ok(review) => by_agent.push((name, review.findings)),
            Err(e) => {
                // One reviewer failing is a degraded review, not a dead one,
                // but the report has to say so rather than quietly halving the
                // coverage the whole design rests on.
                logdim!("{name} could not review PR #{}: {e}", pr.number);
                state
                    .notes
                    .push(format!("{name} did not return a review: {e}"));
            }
        }
    }
    if by_agent.is_empty() {
        return Err(spar_err!("neither reviewer returned a usable review"));
    }
    if by_agent.len() == 1 {
        // The whole design is one model checking another. A single reviewer is
        // a materially weaker result, not a footnote, so it is said loudly and
        // marked on every finding in the comment.
        crate::logging::warn(format!(
            "only {} answered on PR #{}. Nothing was cross-checked, so these findings carry one \
             model's judgement rather than two.",
            by_agent[0].0, pr.number
        ));
        state
            .notes
            .push("only one reviewer answered, so nothing was cross-checked".into());
    }

    let mut judged = corroborate(&by_agent);

    // -- phase 2: each rules on what only the other raised ----------------
    if budget >= 2 && by_agent.len() == 2 {
        adjudicate(agents, cfg, repo, work_dir, &mut judged, 2)?;
    } else if budget < 2 {
        for j in judged.iter_mut() {
            if j.standing == Standing::Unverified {
                j.counterpoint = Some("not cross-checked, max_rounds was 1".into());
            }
        }
    }

    // -- phase 3: whoever raised a rejected point defends it or drops it --
    if budget >= 3 && judged.iter().any(|j| j.standing == Standing::Disputed) {
        rebut(agents, cfg, repo, work_dir, &mut judged, 3)?;
    }

    state.rounds = budget.min(3);
    finish(repo, pr, state, &judged, dry_run)
}

/// Findings both reviewers reached on their own, matched by finding key.
fn corroborate(by_agent: &[(String, Vec<Finding>)]) -> Vec<Judged> {
    let mut judged: Vec<Judged> = Vec::new();

    for (name, findings) in by_agent {
        for finding in findings {
            let key = finding_key(&finding.title, &finding.file);
            match judged
                .iter_mut()
                .find(|j| finding_key(&j.finding.title, &j.finding.file) == key)
            {
                Some(existing) => {
                    // Both reached it independently. Keep the graver severity:
                    // one reviewer calling it blocking is a reason to look.
                    existing.finding.severity = existing.finding.severity.graver(finding.severity);
                    existing.standing = Standing::Corroborated;
                    existing.raised_by = format!("{} and {name}", existing.raised_by);
                }
                None => judged.push(Judged {
                    finding: finding.clone(),
                    raised_by: name.clone(),
                    standing: Standing::Unverified,
                    counterpoint: None,
                    defence: None,
                }),
            }
        }
    }
    judged
}

fn adjudicate(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    work_dir: &Path,
    judged: &mut [Judged],
    round: u32,
) -> Result<()> {
    let pending: Vec<usize> = judged
        .iter()
        .enumerate()
        .filter(|(_, j)| j.standing == Standing::Unverified)
        .map(|(i, _)| i)
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    log!(
        "cross-checking {} finding{} raised by one reviewer",
        pending.len(),
        plural(pending.len())
    );

    let answers = concurrently(agents, |adjudicator| {
        // Each agent rules on what the *other* raised, never on its own.
        let theirs: Vec<&Judged> = pending
            .iter()
            .map(|i| &judged[*i])
            .filter(|j| j.raised_by != adjudicator.name())
            .collect();
        if theirs.is_empty() {
            return Ok(AdjudicationDoc { verdicts: vec![] });
        }
        let listed: Vec<Finding> = theirs.iter().map(|j| j.finding.clone()).collect();
        let prompt =
            ADJUDICATE_PROMPT.replace("{findings}", &crate::review::findings_for_prompt(&listed));
        adjudicator.ask_json::<AdjudicationDoc>(
            &prompt,
            &schema::adjudication(),
            work_dir,
            cfg.effort_for_round(&adjudicator.spec, round).as_deref(),
        )
    });

    for (name, result) in answers {
        let doc = match result {
            Ok(doc) => doc,
            Err(e) => {
                logdim!("{name} could not adjudicate: {e}");
                continue;
            }
        };
        for verdict in doc.verdicts {
            let key = finding_key(&verdict.title, &verdict.file);
            let Some(target) = judged.iter_mut().find(|j| {
                j.raised_by != name
                    && (finding_key(&j.finding.title, &j.finding.file) == key
                        || crate::review::same_point(&j.finding.title, &verdict.title))
            }) else {
                continue;
            };
            if target.standing != Standing::Unverified {
                continue;
            }
            target.counterpoint = Some(style::summary(&verdict.reasoning, &repo.style));
            if verdict.agrees {
                target.standing = Standing::Confirmed;
                // A second reader who agrees it is real but calls it a nit is
                // exactly the signal that stops a nitpick reaching a
                // maintainer as a blocker.
                target.finding.severity = target.finding.severity.graver(verdict.severity);
            } else {
                target.standing = Standing::Disputed;
            }
        }
    }
    Ok(())
}

fn rebut(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    work_dir: &Path,
    judged: &mut [Judged],
    round: u32,
) -> Result<()> {
    let disputed: Vec<usize> = judged
        .iter()
        .enumerate()
        .filter(|(_, j)| j.standing == Standing::Disputed)
        .map(|(i, _)| i)
        .collect();
    log!(
        "{} disputed finding{} going back to whoever raised them",
        disputed.len(),
        plural(disputed.len())
    );

    let answers = concurrently(agents, |author| {
        let mine: Vec<&Judged> = disputed
            .iter()
            .map(|i| &judged[*i])
            .filter(|j| j.raised_by == author.name())
            .collect();
        if mine.is_empty() {
            return Ok(AdjudicationDoc { verdicts: vec![] });
        }
        let listed = mine
            .iter()
            .map(|j| {
                format!(
                    "- [{}] {} ({})\n  {}\n  OBJECTION: {}",
                    j.finding.severity,
                    j.finding.title,
                    j.finding.where_at(),
                    j.finding.detail,
                    j.counterpoint.as_deref().unwrap_or("(none given)")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = REBUT_PROMPT.replace("{findings}", &listed);
        author.ask_json::<AdjudicationDoc>(
            &prompt,
            &schema::adjudication(),
            work_dir,
            cfg.effort_for_round(&author.spec, round).as_deref(),
        )
    });

    for (name, result) in answers {
        let doc = match result {
            Ok(doc) => doc,
            Err(e) => {
                logdim!("{name} could not answer the objections: {e}");
                continue;
            }
        };
        for verdict in doc.verdicts {
            let key = finding_key(&verdict.title, &verdict.file);
            let Some(target) = judged.iter_mut().find(|j| {
                j.raised_by == name
                    && j.standing == Standing::Disputed
                    && (finding_key(&j.finding.title, &j.finding.file) == key
                        || crate::review::same_point(&j.finding.title, &verdict.title))
            }) else {
                continue;
            };
            if verdict.agrees {
                // Stands by it. Kept separate from the objection so a person
                // can weigh the two arguments rather than read them spliced.
                target.defence = Some(style::sentence(&verdict.reasoning, &repo.style));
            } else {
                target.standing = Standing::Withdrawn;
            }
        }
    }
    Ok(())
}

fn finish(
    repo: &Repo,
    pr: &PrView,
    state: &mut IssueRun,
    judged: &[Judged],
    dry_run: bool,
) -> Result<()> {
    let blocking = judged
        .iter()
        .filter(|j| j.finding.blocks() && j.standing.counts())
        .count();

    state.status = if blocking == 0 {
        Status::Clean
    } else {
        Status::Reviewed
    };
    for j in judged.iter().filter(|j| j.standing == Standing::Disputed) {
        state.disputes.push(crate::model::Dispute {
            title: style::title(&j.finding.title, &repo.style),
            reasoning: j.counterpoint.clone().unwrap_or_default(),
        });
    }

    let comment = verdict_comment(judged, &repo.style);
    // `pr_comments = "none"` promises spar will not comment on a pull request.
    // Review mode used to post regardless, which made the promise false and left
    // --dry-run as the only way to keep it.
    let silent = dry_run || repo.style.pr_comments == crate::config::PrComments::None;
    if silent {
        println!("\n{comment}\n");
        let why = if dry_run {
            "dry run"
        } else {
            "pr_comments is none"
        };
        match repo.save_pending_comment(pr.number, &comment) {
            Ok(path) => log!(
                "{why}, nothing posted. Saved to {}. Post it with `spar post {}`, or edit that \
                 file first.",
                path.display(),
                pr.number
            ),
            Err(e) => logdim!("{why}, nothing posted, and could not save it: {e}"),
        }
        return Ok(());
    }
    match repo.comment_pr(pr.number, &comment) {
        Ok(()) => log!(
            "PR #{}: {}",
            pr.number,
            if blocking == 0 {
                "no blocking findings, review posted".to_string()
            } else {
                format!(
                    "{blocking} blocking finding{}, review posted",
                    plural(blocking)
                )
            }
        ),
        Err(e) => {
            state.notes.push(format!("could not post the review: {e}"));
            println!("\n{comment}\n");
        }
    }
    Ok(())
}

impl Standing {
    /// Whether a finding should be put in front of a maintainer as real.
    pub fn counts(self) -> bool {
        matches!(
            self,
            Standing::Corroborated | Standing::Confirmed | Standing::Unverified
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Standing::Corroborated => "both reviewers raised this independently",
            Standing::Confirmed => "raised by one reviewer, confirmed by the other",
            Standing::Disputed => "the reviewers disagree",
            Standing::Withdrawn => "withdrawn",
            Standing::Unverified => "raised by one reviewer, not cross-checked",
        }
    }
}

/// The one thing a maintainer reads.
pub fn verdict_comment(judged: &[Judged], style: &Style) -> String {
    let live: Vec<&Judged> = judged.iter().filter(|j| j.standing.counts()).collect();
    let pick = |severity: Severity| -> Vec<&Judged> {
        live.iter()
            .copied()
            .filter(|j| j.finding.severity == severity && j.finding.in_scope)
            .collect()
    };
    let blocking = pick(Severity::Blocking);
    let non_blocking = pick(Severity::NonBlocking);
    let nits = pick(Severity::Nit);
    let disputed: Vec<&Judged> = judged
        .iter()
        .filter(|j| j.standing == Standing::Disputed)
        .collect();
    let withdrawn = judged
        .iter()
        .filter(|j| j.standing == Standing::Withdrawn)
        .count();

    // "Two independent reviews" stays: it is the only thing that makes [both],
    // [one reviewer only] and the disagreement heading below mean anything. The
    // counts go, because everything they count is listed immediately after.
    let mut out = vec![if blocking.is_empty() && disputed.is_empty() {
        "Two independent reviews, nothing blocking a merge.".to_string()
    } else {
        "Two independent reviews.".to_string()
    }];
    let _ = withdrawn;

    let line = |j: &Judged| -> String {
        let where_at = match j.finding.where_at() {
            "general" => String::new(),
            file => format!(" ({file})"),
        };
        let detail = style::detail(&j.finding.detail, style);
        let attested = if j.standing == Standing::Corroborated {
            " [both]"
        } else if j.standing == Standing::Unverified {
            " [one reviewer only]"
        } else {
            ""
        };
        if detail.is_empty() {
            format!(
                "- {}{where_at}{attested}",
                style::title(&j.finding.title, style)
            )
        } else {
            format!(
                "- {}{where_at}{attested}. {detail}",
                style::title(&j.finding.title, style)
            )
        }
    };

    if !blocking.is_empty() {
        out.push(format!(
            "needs changing before merge\n{}",
            blocking
                .iter()
                .copied()
                .map(line)
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !non_blocking.is_empty() {
        out.push(format!(
            "worth doing, does not block\n{}",
            non_blocking
                .iter()
                .copied()
                .map(line)
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !nits.is_empty() {
        out.push(format!(
            "nits\n{}",
            nits.iter()
                .copied()
                .map(line)
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !disputed.is_empty() {
        let lines: Vec<String> = disputed
            .iter()
            .map(|j| {
                let mut line = format!(
                    "- {} ({})",
                    style::title(&j.finding.title, style),
                    j.finding.where_at()
                );
                if let Some(objection) = &j.counterpoint {
                    line.push_str(&format!(
                        ". Objection: {}",
                        style::sentence(objection, style)
                    ));
                }
                if let Some(defence) = &j.defence {
                    line.push_str(&format!(" Answer: {}", style::sentence(defence, style)));
                }
                line
            })
            .collect();
        out.push(format!(
            "the reviewers disagree, your call\n{}",
            lines.join("\n")
        ));
    }

    out.join("\n\n")
}

/// "s" unless there is exactly one of the thing.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Run the same closure on every agent at once.
fn concurrently<T, F>(agents: &[Agent], work: F) -> Vec<(String, Result<T>)>
where
    T: Send,
    F: Fn(&Agent) -> Result<T> + Sync,
{
    std::thread::scope(|scope| {
        let handles: Vec<_> = agents
            .iter()
            .map(|agent| scope.spawn(|| (agent.name().to_string(), work(agent))))
            .collect();
        handles
            .into_iter()
            .zip(agents)
            .map(|(handle, agent)| {
                handle.join().unwrap_or_else(|_| {
                    (
                        agent.name().to_string(),
                        Err(spar_err!("thread for '{}' panicked", agent.name())),
                    )
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(severity: &str, title: &str, file: &str) -> Finding {
        Finding {
            severity: Severity::parse_lenient(severity).unwrap(),
            title: title.into(),
            detail: "why it matters".into(),
            file: file.into(),
            in_scope: true,
        }
    }

    fn from(name: &str, findings: Vec<Finding>) -> (String, Vec<Finding>) {
        (name.to_string(), findings)
    }

    // -- corroboration ---------------------------------------------------

    /// The whole thesis of the tool: a defect two models reach independently,
    /// with different training and different blind spots, is worth more than
    /// either one saying it twice.
    #[test]
    fn a_finding_both_reviewers_reached_alone_is_corroborated() {
        let judged = corroborate(&[
            from(
                "claude",
                vec![finding("blocking", "Retry loop spins", "src/net.rs")],
            ),
            from(
                "codex",
                vec![finding("blocking", "retry loop spins!", "src/net.rs")],
            ),
        ]);
        assert_eq!(1, judged.len(), "the same point must not be listed twice");
        assert_eq!(Standing::Corroborated, judged[0].standing);
        assert!(judged[0].raised_by.contains("claude"));
        assert!(judged[0].raised_by.contains("codex"));
    }

    #[test]
    fn a_finding_only_one_reviewer_raised_starts_unverified() {
        let judged = corroborate(&[
            from(
                "claude",
                vec![finding("blocking", "Only claude saw this", "a.rs")],
            ),
            from("codex", vec![]),
        ]);
        assert_eq!(Standing::Unverified, judged[0].standing);
        assert_eq!("claude", judged[0].raised_by);
    }

    #[test]
    fn the_same_title_in_a_different_file_is_two_findings() {
        let judged = corroborate(&[
            from("claude", vec![finding("nit", "Naming", "a.rs")]),
            from("codex", vec![finding("nit", "Naming", "b.rs")]),
        ]);
        assert_eq!(2, judged.len());
    }

    /// Resolved upward on purpose. Nothing here gates a merge, so advice that
    /// under-reports a real defect is worse than advice that over-reports.
    #[test]
    fn disagreement_about_severity_keeps_the_graver_one() {
        let judged = corroborate(&[
            from("claude", vec![finding("nit", "Unbounded loop", "a.rs")]),
            from("codex", vec![finding("blocking", "unbounded loop", "a.rs")]),
        ]);
        assert_eq!(Severity::Blocking, judged[0].finding.severity);

        // And the other way round, so it is not an artefact of ordering.
        let judged = corroborate(&[
            from(
                "claude",
                vec![finding("blocking", "Unbounded loop", "a.rs")],
            ),
            from("codex", vec![finding("nit", "unbounded loop", "a.rs")]),
        ]);
        assert_eq!(Severity::Blocking, judged[0].finding.severity);
    }

    #[test]
    fn severity_ordering_does_not_depend_on_declaration_order() {
        assert_eq!(Severity::Blocking, Severity::Blocking.graver(Severity::Nit));
        assert_eq!(Severity::Blocking, Severity::Nit.graver(Severity::Blocking));
        assert_eq!(
            Severity::NonBlocking,
            Severity::Nit.graver(Severity::NonBlocking)
        );
        assert!(Severity::Blocking.rank() > Severity::NonBlocking.rank());
        assert!(Severity::NonBlocking.rank() > Severity::Nit.rank());
    }

    #[test]
    fn a_single_reviewer_still_produces_a_list() {
        let judged = corroborate(&[from("claude", vec![finding("blocking", "A", "a.rs")])]);
        assert_eq!(1, judged.len());
        assert_eq!(Standing::Unverified, judged[0].standing);
    }

    // -- what reaches a maintainer ---------------------------------------

    #[test]
    fn only_surviving_standings_count() {
        assert!(Standing::Corroborated.counts());
        assert!(Standing::Confirmed.counts());
        assert!(Standing::Unverified.counts());
        assert!(
            !Standing::Disputed.counts(),
            "a disputed point is listed separately"
        );
        assert!(
            !Standing::Withdrawn.counts(),
            "a withdrawn point is not a finding"
        );
    }

    fn judged(standing: Standing, severity: &str, title: &str) -> Judged {
        Judged {
            finding: finding(severity, title, "src/net.rs"),
            raised_by: "claude".into(),
            standing,
            counterpoint: None,
            defence: None,
        }
    }

    #[test]
    fn a_clean_pr_says_so_in_one_breath() {
        let text = verdict_comment(&[], &Style::default());
        assert!(
            text.starts_with("Two independent reviews, nothing blocking a merge."),
            "{text}"
        );
    }

    #[test]
    fn a_corroborated_blocker_is_marked_as_such() {
        let text = verdict_comment(
            &[judged(
                Standing::Corroborated,
                "blocking",
                "Retry loop spins",
            )],
            &Style::default(),
        );
        assert!(text.contains("needs changing before merge"), "{text}");
        assert!(text.contains("[both]"), "{text}");
    }

    #[test]
    fn an_uncrosschecked_finding_is_flagged_as_one_reviewers_opinion() {
        let text = verdict_comment(
            &[judged(
                Standing::Unverified,
                "blocking",
                "Only one saw this",
            )],
            &Style::default(),
        );
        assert!(text.contains("[one reviewer only]"), "{text}");
    }

    #[test]
    fn a_confirmed_finding_carries_no_qualifier() {
        let text = verdict_comment(
            &[judged(Standing::Confirmed, "blocking", "Checked and real")],
            &Style::default(),
        );
        assert!(
            !text.contains("[both]") && !text.contains("[one reviewer only]"),
            "{text}"
        );
    }

    /// The point of the whole exercise: a claim one model made and the other
    /// read the code and rejected does not go to a maintainer as fact.
    #[test]
    fn a_withdrawn_finding_never_reaches_the_maintainer() {
        let text = verdict_comment(
            &[judged(
                Standing::Withdrawn,
                "blocking",
                "Wrong on a second look",
            )],
            &Style::default(),
        );
        assert!(!text.contains("Wrong on a second look"), "{text}");
        assert!(
            !text.to_lowercase().contains("withdrawn"),
            "a point nobody can see or act on is not worth a sentence: {text}"
        );
        assert!(text.contains("nothing blocking a merge"), "{text}");
    }

    #[test]
    fn a_disputed_finding_goes_to_a_person_with_both_sides() {
        let mut j = judged(Standing::Disputed, "blocking", "Error is swallowed");
        j.counterpoint = Some("the caller already validates the file".into());
        let text = verdict_comment(&[j], &Style::default());
        assert!(text.contains("the reviewers disagree, your call"), "{text}");
        assert!(
            text.contains("Objection: The caller already validates"),
            "{text}"
        );
        assert!(
            !text.contains("needs changing before merge"),
            "disputed does not block: {text}"
        );
    }

    #[test]
    fn the_three_severities_are_kept_apart() {
        let text = verdict_comment(
            &[
                judged(Standing::Corroborated, "blocking", "Must fix"),
                judged(Standing::Confirmed, "non-blocking", "Could improve"),
                judged(Standing::Confirmed, "nit", "Taste"),
            ],
            &Style::default(),
        );
        assert!(
            !text.contains("1 blocking"),
            "counts are listed below, not above: {text}"
        );
        assert!(text.contains("needs changing before merge"), "{text}");
        assert!(text.contains("worth doing, does not block"), "{text}");
        assert!(text.contains("nits"), "{text}");
    }

    /// A reviewer with a lot to say is not the problem, and clipping the
    /// explanation of a defect helps nobody. Only a runaway is bounded.
    #[test]
    fn a_thorough_reviewer_is_not_cut_short() {
        let mut j = judged(Standing::Corroborated, "blocking", "A real problem");
        j.finding.detail = "Here is a step of the reproduction. ".repeat(20);
        let text = verdict_comment(&[j], &Style::default());
        assert!(
            text.contains(
                &"Here is a step of the reproduction. "
                    .repeat(20)
                    .trim()
                    .to_string()
            ) || text.len() > 600,
            "the explanation survived: {} chars",
            text.len()
        );
    }

    #[test]
    fn a_runaway_reviewer_is_still_bounded() {
        let mut j = judged(Standing::Corroborated, "blocking", "A real problem");
        j.finding.detail = "filler ".repeat(20_000);
        let text = verdict_comment(&[j], &Style::default());
        assert!(text.len() < 6000, "{} chars", text.len());
    }

    #[test]
    fn an_out_of_scope_finding_does_not_ask_the_contributor_to_fix_it() {
        let mut j = judged(Standing::Corroborated, "blocking", "Pre-existing bug");
        j.finding.in_scope = false;
        let text = verdict_comment(&[j], &Style::default());
        assert!(!text.contains("needs changing before merge"), "{text}");
        assert!(text.contains("nothing blocking a merge"), "{text}");
    }
}
