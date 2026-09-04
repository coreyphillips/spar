//! The command line.

use std::collections::{BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::agent::{self, Agent};
use crate::checkin;
use crate::config::{self, Config};
use crate::error::Result;
use crate::followups;
use crate::model::{Issue, IssueRun, ItemKind, Plan, PlanItem, Status};
use crate::proc::{self, ExecOpts};
use crate::repo::{Repo, WriteSummary};
use crate::review;
use crate::review_only;
use crate::split;
use crate::style;
use crate::tracker;
use crate::triage;
use crate::{bail, log, logdim, logging, logwarn, spar_err};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "spar",
    version = VERSION,
    about = "Two coding agents alternate implementing and reviewing GitHub issues.",
    long_about = "Two coding agents alternate implementing and reviewing GitHub issues until a \
                  pull request converges. Neither agent reviews its own most recent edit.\n\n\
                  Arguments are issue numbers for `run` and `triage`, and pull request numbers \
                  for `resume`, `review`, and `checkin`. `split` takes either. Omit them and \
                  spar takes everything open, up to --limit. `split` applies that limit once to \
                  issues and once to pull requests. `followup` takes none: it works the queue in \
                  .spar/followups.md, and an entry there has no number to name.",
    max_term_width = 96
)]
pub struct Cli {
    /// Suppress progress logging. Warnings, errors, and the final summary still print.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Triage the issues, then work them in dependency order.
    Run {
        /// Issue numbers. Omit to take every open issue, up to --limit.
        issues: Vec<i64>,
        #[command(flatten)]
        common: Common,
        #[command(flatten)]
        loop_flags: LoopFlags,
        #[command(flatten)]
        triage_flags: TriageFlags,
        /// Where to write the triage plan.
        #[arg(long, default_value = "plan.json")]
        plan_out: PathBuf,
        /// Work in the main checkout instead of an isolated worktree per issue.
        #[arg(long)]
        no_worktrees: bool,
    },

    /// Work the follow-ups recorded in .spar/followups.md.
    ///
    /// One agent reads every entry against the current checkout and rules on
    /// it: still there, already fixed, not worth it, or a duplicate. What
    /// survives is filed as an issue and worked like any other, which means
    /// both agents still triage it before anything is implemented. An entry
    /// that was filed or dropped leaves the queue and is kept in
    /// .spar/followups.done.md.
    ///
    /// Takes no numbers: an entry has no number a person could type. --limit
    /// caps how many are taken, and --min-number does nothing here.
    Followup {
        #[command(flatten)]
        common: Common,
        #[command(flatten)]
        loop_flags: LoopFlags,
        #[command(flatten)]
        triage_flags: TriageFlags,
        /// Read this instead of .spar/followups.md.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Print the verdicts and stop. Nothing is filed and no file is touched.
        #[arg(long)]
        screen_only: bool,
        /// File the issues and stop, leaving them for a later `spar run`.
        #[arg(long, conflicts_with = "screen_only")]
        file_only: bool,
        /// Where to write the triage plan.
        #[arg(long, default_value = "plan.json")]
        plan_out: PathBuf,
        /// Work in the main checkout instead of an isolated worktree per issue.
        #[arg(long)]
        no_worktrees: bool,
    },

    /// Triage only. Writes the plan and touches nothing else.
    Triage {
        /// Issue numbers. Omit to take every open issue, up to --limit.
        issues: Vec<i64>,
        #[command(flatten)]
        common: Common,
        #[arg(long, default_value = "plan.json")]
        plan_out: PathBuf,
    },

    /// Continue the review loop on existing PRs, including ones spar did not create.
    Resume {
        /// Pull request numbers. Omit to take every open PR, up to --limit.
        prs: Vec<i64>,
        #[command(flatten)]
        common: Common,
        #[command(flatten)]
        loop_flags: LoopFlags,
        /// Which agent reviews next, overriding saved custody after the PR head changed.
        #[arg(long = "next", value_name = "AGENT")]
        next_actor: Option<String>,
    },

    /// Answer the comments on a pull request, and act on the ones worth acting on.
    ///
    /// Reads every comment somebody else left that has not been answered. Both
    /// agents judge each one. A change they both agree is right and belongs
    /// here is made, pushed, answered in its own thread, and the thread marked
    /// resolved. One they both judge wrong gets the reason and the thread is
    /// left open for you. One they disagree about is parked.
    Checkin {
        /// Pull request numbers. An issue number resolves to its open PR.
        /// Omit to take every open PR, up to --limit.
        items: Vec<i64>,
        #[command(flatten)]
        common: Common,
        /// Print every reply and every change instead of posting or pushing.
        #[arg(long)]
        dry_run: bool,
        /// Answer in words only. Nothing is committed, pushed, or resolved.
        #[arg(long)]
        reply_only: bool,
        /// Act on a comment from anyone, not only from somebody who can write
        /// to this repository.
        #[arg(long)]
        any_author: bool,
        /// Answer comments spar already answered, ignoring what it recorded.
        #[arg(long)]
        again: bool,
        /// Leave worktrees in place afterwards, for inspection.
        #[arg(long)]
        keep_worktrees: bool,
    },

    /// Review pull requests without changing them, including from a fork.
    ///
    /// Both agents review independently, then rule on each other's findings,
    /// then answer the objections. Nothing is committed, pushed, or merged.
    Review {
        /// Pull request numbers. An issue number resolves to its open PR.
        /// Omit to take every open PR, up to --limit.
        items: Vec<i64>,
        #[command(flatten)]
        common: Common,
        /// Print the review instead of posting it.
        #[arg(long)]
        dry_run: bool,
        /// Adjudication passes. 1 is two independent reviews with no
        /// cross-checking, 2 adds it, 3 adds a rebuttal on what they dispute.
        #[arg(long)]
        max_rounds: Option<u32>,
    },

    /// Break an issue or a pull request into smaller ones.
    ///
    /// One agent proposes the parts with the code open, the other rules on the
    /// proposal: accept, reject, or accept with named parts struck.
    /// Disagreement resolves toward not splitting.
    ///
    /// An issue's parts are filed as issues and the parent is rewritten into a
    /// checklist that points at them. A pull request's parts each get their own
    /// branch and their own pull request, and the original is left open and
    /// otherwise untouched: split branches use create-only pushes, and the
    /// parent is never closed or rebased. A pull request from a fork is proposed
    /// in a comment rather than split.
    ///
    /// It decomposes and stops. Nothing is triaged, implemented, or merged. Run
    /// the reported child numbers next, or enable `decompose_trackers` and run
    /// the issue parent.
    Split {
        /// Issue or pull request numbers. Omit to go through every open issue
        /// and every open pull request, and split what is worth splitting.
        items: Vec<i64>,
        #[command(flatten)]
        common: Common,
        /// Print the proposal and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Start a separate split even when one is recorded or retained. Does
        /// not resume retained branches.
        #[arg(long)]
        again: bool,
    },

    /// Post a review a dry run produced, without running the agents again.
    ///
    /// `spar review <pr> --dry-run` saves what it produced. Read it, edit the
    /// file if you like, then post exactly that.
    Post {
        /// Pull request numbers whose saved review should be posted.
        #[arg(required = true)]
        prs: Vec<i64>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Post this file instead of the saved review.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Print what would be posted and stop.
        #[arg(long)]
        dry_run: bool,
    },

    /// Detect installed agent CLIs and write a spar.toml.
    ///
    /// On an existing config, `--update` appends any settings it does not
    /// mention, which is how to pick up options added by a newer release.
    Init {
        #[arg(long, default_value = "spar.toml")]
        out: PathBuf,
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
        /// Append settings the existing config does not mention, as comments.
        /// Nothing already in the file is changed.
        #[arg(long, conflicts_with = "force")]
        update: bool,
    },

    /// Remove worktrees, branches, and state whose PR is merged or closed.
    Clean {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Remove every local worktree and local branch spar recorded, even for
        /// open PRs.
        #[arg(long)]
        all: bool,
        /// Also delete state comments left on finished PRs.
        #[arg(long)]
        pr_state: bool,
    },

    /// Check prerequisites and resolve each configured agent.
    Doctor {
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Read a commit message on stdin and write the scrubbed version to stdout.
    ///
    /// Used by `git filter-branch`, not by people.
    #[command(hide = true)]
    ScrubFilter,
}

#[derive(Args, Debug, Clone)]
pub struct Common {
    /// Path to the git repository.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Path to spar.toml.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Base branch. Defaults to whatever origin/HEAD points at.
    #[arg(long)]
    pub base: Option<String>,
    /// Which agent implements first. A key from the `[agents]` table.
    #[arg(long)]
    pub first: Option<String>,
    /// Cap on open items of each kind when none are named. Bare `split` may take
    /// this many issues and this many pull requests.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Ignore issues and pull requests numbered below this when picking for
    /// itself. A number you name explicitly is always honoured.
    #[arg(long, value_name = "N")]
    pub min_number: Option<i64>,
    /// Extra instructions for both agents, for this run only. Added to any
    /// already in the config rather than replacing them.
    #[arg(long, value_name = "TEXT")]
    pub instructions: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LoopFlags {
    /// Review rounds this run may spend asking for changes. A closing pass,
    /// when needed, is not one of them. Resuming grants a fresh budget.
    #[arg(long)]
    pub max_rounds: Option<u32>,
    /// Merge when no blocking findings remain. Off by default, deliberately.
    #[arg(long)]
    pub auto_merge: bool,
    /// Leave worktrees in place after a run, for inspection.
    #[arg(long)]
    pub keep_worktrees: bool,
    /// Waves of newly filed follow-ups to fold back into this run instead of
    /// leaving them for the next one. Each wave is triaged like any issue.
    #[arg(long, value_name = "N")]
    pub absorb: Option<u32>,
}

/// Only `run` triages, so only `run` can decline an issue. Offering these on
/// `resume` would accept a flag that does nothing.
#[derive(Args, Debug, Clone)]
pub struct TriageFlags {
    /// Close an issue both agents declined, after posting the reasoning.
    #[arg(long, conflicts_with = "no_close_skipped")]
    pub close_skipped: bool,
    /// Comment on a declined issue but leave it open.
    #[arg(long)]
    pub no_close_skipped: bool,
    /// Ask again about issues a previous run's triage disagreed on.
    ///
    /// A disagreement is parked for a person to decide. Without this it is not
    /// re-triaged, because asking two agents the same question again costs two
    /// calls per run and very likely gets the same answer.
    #[arg(long)]
    pub retriage: bool,
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

pub fn main() -> i32 {
    let cli = Cli::parse();
    logging::init_color();
    logging::set_quiet(cli.quiet);

    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            logging::error(e.to_string());
            2
        }
    }
}

fn dispatch(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::ScrubFilter => cmd_scrub_filter(),
        Command::Doctor { config } => cmd_doctor(config.as_deref()),
        Command::Review {
            items,
            common,
            dry_run,
            max_rounds,
        } => {
            let overrides = Overrides {
                max_rounds,
                ..Overrides::default()
            };
            let (cfg, repo, agents) = prepare(&common, Some(overrides))?;
            let numbers = if items.is_empty() {
                let found = repo.list_open_prs(common.limit, cfg.loop_cfg.min_number)?;
                if found.is_empty() {
                    log!("no open PRs");
                    return Ok(0);
                }
                log!("no PRs given, reviewing {} open", found.len());
                found
            } else {
                items
            };
            let sorted = classify(&repo, &numbers)?;
            let mut targets = sorted.prs;
            for number in sorted.issues {
                match repo.try_open_pr_for_issue(number, cfg.base_branch()) {
                    Ok(Some(pr)) => {
                        log!("#{number} is an issue; reviewing its open PR {}", pr.url);
                        targets.push(pr.number);
                    }
                    Ok(None) => {
                        logwarn!("#{number} is an issue with no open pull request to review")
                    }
                    Err(e) => logwarn!("#{number}: {e}"),
                }
            }
            let mut results = Vec::new();
            for number in targets {
                results.push(review_only::review_pr(
                    &agents, &cfg, &repo, number, dry_run,
                ));
            }
            if results.is_empty() {
                return Ok(0);
            }
            Ok(report(&results, &cfg, &repo))
        }

        Command::Checkin {
            items,
            common,
            dry_run,
            reply_only,
            any_author,
            again,
            keep_worktrees,
        } => {
            let overrides = Overrides {
                keep_worktrees: keep_worktrees.then_some(true),
                ..Overrides::default()
            };
            let (cfg, repo, agents) = prepare(&common, Some(overrides))?;
            let mode = checkin::Mode {
                dry_run,
                reply_only,
                trust: if any_author {
                    crate::config::Trust::Anyone
                } else {
                    cfg.loop_cfg.checkin_trust
                },
                again,
                resolve: cfg.loop_cfg.checkin_resolve,
                posts: checkin::posts(&cfg),
            };
            let numbers = if items.is_empty() {
                let found = repo.list_open_prs(common.limit, cfg.loop_cfg.min_number)?;
                if found.is_empty() {
                    log!("no open PRs");
                    return Ok(0);
                }
                log!(
                    "no PRs given, checking in on {} open: {}",
                    found.len(),
                    found
                        .iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                found
            } else {
                items
            };
            let sorted = classify(&repo, &numbers)?;
            let mut results = Vec::new();
            for number in sorted.prs {
                results.push(checkin::checkin_pr(&agents, &cfg, &repo, number, &mode));
            }
            // A change request left on the issue is exactly what this exists to
            // catch, and when the issue has work open there is a branch to act
            // on, so route to it rather than refusing.
            for number in sorted.issues {
                match repo.try_open_pr_for_issue(number, cfg.base_branch()) {
                    Ok(Some(pr)) => {
                        log!(
                            "#{number} is an issue; checking in on its open PR {}",
                            pr.url
                        );
                        results.push(checkin::checkin_pr(&agents, &cfg, &repo, pr.number, &mode));
                    }
                    Ok(None) => {
                        results.push(checkin::checkin_issue(&agents, &cfg, &repo, number, &mode))
                    }
                    // Answering the issue instead would be answering on the
                    // wrong thing when a pull request is what could not be
                    // seen, and this command's replies are visible to
                    // everybody reading it.
                    Err(e) => results.push(checkin::failed(number, format!("#{number}"), e)),
                }
            }
            if results.is_empty() {
                return Ok(0);
            }
            Ok(report(&results, &cfg, &repo))
        }

        Command::Split {
            items,
            common,
            dry_run,
            again,
        } => {
            let (cfg, repo, agents) = prepare(&common, None)?;
            let mode = split::Mode { dry_run, again };
            let picked = if items.is_empty() {
                pick_for_split(&agents, &cfg, &repo, common.limit, &mode)?
            } else {
                let sorted = classify(&repo, &items)?;
                let mut out: Vec<(i64, ItemKind)> = sorted
                    .issues
                    .iter()
                    .map(|n| (*n, ItemKind::Issue))
                    .collect();
                out.extend(sorted.prs.iter().map(|n| (*n, ItemKind::Pr)));
                out
            };

            let mut results = Vec::new();
            let mut seen: BTreeSet<i64> = BTreeSet::new();
            for (number, kind) in picked {
                // An issue whose work is half done produces children describing
                // work that already exists on a branch, so it routes to the
                // branch instead, the way `checkin` and `review` already do.
                let target = match kind {
                    ItemKind::Pr => Some(number),
                    ItemKind::Issue => {
                        match repo.try_open_pr_for_issue(number, cfg.base_branch()) {
                            Ok(found) => found.map(|pr| {
                                log!("#{number} is an issue; splitting its open PR {}", pr.url);
                                pr.number
                            }),
                            Err(e) => {
                                logwarn!("#{number}: {e}");
                                continue;
                            }
                        }
                    }
                };
                // An issue and the pull request it routes to can both be named,
                // and the queue can hold both. Splitting one pull request twice
                // makes two sets of branches and pull requests out of it, which
                // `--again` would not stop.
                if !seen.insert(target.unwrap_or(number)) {
                    logdim!("#{number} was already covered by this run, skipping it");
                    continue;
                }
                results.push(match target {
                    Some(pr) => split::split_pr(&agents, &cfg, &repo, pr, &mode),
                    None => split::split_issue(&agents, &cfg, &repo, number, &mode),
                });
            }
            if results.is_empty() {
                return Ok(0);
            }
            Ok(report(&results, &cfg, &repo))
        }

        Command::Post {
            prs,
            repo: repo_path,
            config,
            file,
            dry_run,
        } => cmd_post(
            &prs,
            &repo_path,
            config.as_deref(),
            file.as_deref(),
            dry_run,
        ),

        Command::Init { out, force, update } => {
            if update {
                cmd_init_update(&out)
            } else {
                cmd_init(&out, force)
            }
        }
        Command::Clean {
            repo,
            config,
            all,
            pr_state,
        } => cmd_clean(&repo, config.as_deref(), all, pr_state),
        Command::Triage {
            issues,
            common,
            plan_out,
        } => {
            let (cfg, repo, agents) = prepare(&common, None)?;
            let numbers = pick_issues(&repo, issues, common.limit, cfg.loop_cfg.min_number)?;
            if numbers.is_empty() {
                return Ok(0);
            }
            let sorted = classify(&repo, &numbers)?;
            for number in &sorted.prs {
                log!("#{number} is a pull request, nothing to triage");
            }
            if sorted.issues.is_empty() {
                log!("no issues to triage");
                return Ok(0);
            }
            let issues = repo.fetch_issues(&sorted.issues)?;
            // Deliberately no act_on_plan here. `triage` is the command you
            // reach for to look before leaping, and a preview that comments on
            // and closes issues is a trap.
            let plan = make_plan(&agents, &cfg, &repo, &issues, &plan_out)?;
            // A preview that files issues and rewrites somebody's issue body is
            // that same trap, so this prints the decomposition and writes none
            // of it. It is where the first few real trackers should be checked.
            if cfg.loop_cfg.decompose_trackers {
                for item in plan.skipped.iter().filter(|i| i.tracker) {
                    tracker::preview(&cfg, &repo, item.issue);
                }
            }
            Ok(0)
        }
        Command::Run {
            issues,
            common,
            loop_flags,
            triage_flags,
            plan_out,
            no_worktrees,
        } => {
            let overrides = Overrides::for_working(&loop_flags, &triage_flags, no_worktrees);
            let (cfg, repo, agents) = prepare(&common, Some(overrides))?;
            let numbers = pick_issues(&repo, issues, common.limit, cfg.loop_cfg.min_number)?;
            if numbers.is_empty() {
                return Ok(0);
            }
            let sorted = classify(&repo, &numbers)?;
            let mut results = Vec::new();
            let mut stopped = Vec::new();
            let mut parked = Vec::new();
            work_issues(
                &agents,
                &cfg,
                &repo,
                sorted.issues.clone(),
                &plan_out,
                &mut results,
                &mut stopped,
                &mut parked,
                triage_flags.retriage,
            )?;

            // Reached even when a wave failed: these were named on the command
            // line and do not depend on triage.
            for number in sorted.prs {
                results.push(review::resume_pr(&agents, &cfg, &repo, number, None));
            }

            if results.is_empty() {
                // Nothing was worked, so the table would be an empty banner.
                // What was parked or stopped still has to reach the person.
                log!("nothing scheduled");
                for line in parked.iter().chain(stopped.iter()) {
                    println!("{line}");
                }
                return Ok(report_writes(&repo).max(i32::from(!stopped.is_empty())));
            }
            Ok(report_with(&results, &cfg, &repo, &stopped, &parked))
        }
        Command::Followup {
            common,
            loop_flags,
            triage_flags,
            file,
            screen_only,
            file_only,
            plan_out,
            no_worktrees,
        } => {
            let overrides = Overrides::for_working(&loop_flags, &triage_flags, no_worktrees);
            let (cfg, repo, agents) = prepare(&common, Some(overrides))?;
            let path = file.unwrap_or_else(|| repo.followups_path());
            let mode = match (screen_only, file_only) {
                (true, _) => followups::Mode::ScreenOnly,
                (_, true) => followups::Mode::FileOnly,
                _ => followups::Mode::Work,
            };
            let outcome = followups::run(&agents, &cfg, &repo, &path, common.limit, mode)?;

            let wave = followups::wave(&outcome);
            if mode != followups::Mode::Work || wave.is_empty() {
                return Ok(report_writes(&repo));
            }
            let mut results = Vec::new();
            let mut stopped = Vec::new();
            let mut parked = Vec::new();
            work_issues(
                &agents,
                &cfg,
                &repo,
                wave,
                &plan_out,
                &mut results,
                &mut stopped,
                &mut parked,
                triage_flags.retriage,
            )?;
            if results.is_empty() {
                // Nothing was worked, so the table would be an empty banner.
                // What was parked or stopped still has to reach the person.
                log!("nothing scheduled");
                for line in parked.iter().chain(stopped.iter()) {
                    println!("{line}");
                }
                return Ok(report_writes(&repo).max(i32::from(!stopped.is_empty())));
            }
            Ok(report_with(&results, &cfg, &repo, &stopped, &parked))
        }

        Command::Resume {
            prs,
            common,
            loop_flags,
            next_actor,
        } => {
            let (cfg, repo, agents) = prepare(&common, Some(Overrides::from(&loop_flags)))?;
            if let Some(name) = &next_actor {
                if !cfg.has_agent(name) {
                    bail!("--next must be one of: {}", cfg.agent_names().join(", "));
                }
            }
            let numbers = if prs.is_empty() {
                let found = repo.list_open_prs(common.limit, cfg.loop_cfg.min_number)?;
                if found.is_empty() {
                    log!("no open PRs");
                    return Ok(0);
                }
                log!(
                    "no PRs given, taking {} open: {}",
                    found.len(),
                    found
                        .iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                found
            } else {
                prs
            };
            let sorted = classify(&repo, &numbers)?;
            let mut results = Vec::new();
            for number in sorted.prs {
                results.push(review::resume_pr(
                    &agents,
                    &cfg,
                    &repo,
                    number,
                    next_actor.as_deref(),
                ));
            }
            // An issue number handed to `resume` is not a mistake worth
            // refusing over. If work is already open for it, continue that.
            for number in sorted.issues {
                match repo.try_open_pr_for_issue(number, cfg.base_branch()) {
                    Ok(Some(pr)) => {
                        log!("#{number} is an issue; continuing its open PR {}", pr.url);
                        results.push(review::resume_pr(
                            &agents,
                            &cfg,
                            &repo,
                            pr.number,
                            next_actor.as_deref(),
                        ));
                    }
                    Ok(None) => logwarn!(
                        "#{number} is an issue with no open pull request. Use `spar run {number}` \
                         to implement it."
                    ),
                    Err(e) => logwarn!("#{number}: {e}"),
                }
            }
            if results.is_empty() {
                return Ok(0);
            }
            Ok(report(&results, &cfg, &repo))
        }
    }
}

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Overrides {
    max_rounds: Option<u32>,
    auto_merge: Option<bool>,
    keep_worktrees: Option<bool>,
    worktrees: Option<bool>,
    close_skipped: Option<bool>,
    absorb: Option<u32>,
}

impl From<&LoopFlags> for Overrides {
    fn from(flags: &LoopFlags) -> Self {
        Self {
            max_rounds: flags.max_rounds,
            auto_merge: flags.auto_merge.then_some(true),
            keep_worktrees: flags.keep_worktrees.then_some(true),
            worktrees: None,
            close_skipped: None,
            absorb: flags.absorb,
        }
    }
}

impl Overrides {
    /// What `run` and `followup` share past the loop flags: both triage, so
    /// both can decline, and both work issues in a worktree apiece.
    fn for_working(loop_flags: &LoopFlags, triage: &TriageFlags, no_worktrees: bool) -> Self {
        let mut over = Overrides::from(loop_flags);
        over.worktrees = if no_worktrees { Some(false) } else { None };
        over.close_skipped = match (triage.close_skipped, triage.no_close_skipped) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        };
        over
    }
}

/// Triage a set of issues, work them in dependency order, and fold each wave of
/// newly filed follow-ups back in as the absorb budget allows.
///
/// Shared by `run` and `followup`, which differ only in where the first wave
/// comes from: `run` takes it from the tracker, `followup` from the issues it
/// just filed out of the local queue. Everything after that is the same
/// pipeline, and it has to stay the same. An issue spar filed for itself gets
/// no easier a ride through triage than one a person opened, which is the whole
/// reason the screening pass is one agent and this is two.
#[allow(clippy::too_many_arguments)]
fn work_issues(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    first_wave: Vec<i64>,
    plan_out: &Path,
    results: &mut Vec<IssueRun>,
    stopped: &mut Vec<String>,
    parked: &mut Vec<String>,
    retriage: bool,
) -> Result<()> {
    // Issues a previous run's triage disagreed about. They are waiting on a
    // person, and asking the pair again costs two calls and gets the same
    // answer, so they are left alone until somebody says otherwise.
    let waiting = repo.read_contested();
    if retriage && !waiting.is_empty() {
        repo.forget_contested(&waiting.keys().copied().collect::<Vec<_>>());
    }
    let mut handled: BTreeSet<i64> = BTreeSet::new();
    // Issues whose work is on the base branch by the time the next one is
    // built. Only a merge puts it there, which is why `auto_merge = false`,
    // the default, means a dependent waits for a person.
    let mut landed: BTreeSet<i64> = BTreeSet::new();
    let mut leftover: BTreeSet<i64> = BTreeSet::new();
    // How many issues this run has actually implemented, which is what the
    // alternating first implementor policy counts. Across waves, so a
    // follow-up wave keeps swapping rather than restarting on the same agent.
    let mut worked = 0usize;
    let mut queue: VecDeque<Wave> = VecDeque::from([Wave::first(first_wave)]);
    let mut plans_written = 0usize;

    // The first wave is what was asked for. A wave of follow-ups the previous
    // one filed is folded back in as the absorb budget allows, and a wave of
    // children extracted from a tracker's checklist joins the same run because
    // it is named work rather than picked work. Every wave is triaged like
    // anything else, so both agents still have to agree each one is worth
    // doing.
    while let Some(mut wave) = queue.pop_front() {
        wave.numbers.retain(|n| !handled.contains(n));
        if !retriage {
            wave.numbers.retain(|n| {
                let Some(known) = waiting.get(n) else {
                    return true;
                };
                log!(
                    "#{n} was contested on an earlier run and is waiting on you: {}. Pass \
                     --retriage to ask again.",
                    positions_of(known)
                );
                parked.push(format!(
                    "#{n} contested and parked for you: {}",
                    positions_of(known)
                ));
                false
            });
        }
        if wave.numbers.is_empty() {
            continue;
        }
        match wave.parent {
            Some(tracker) => log!(
                "working {} item(s) from the checklist in #{tracker}: {}",
                wave.numbers.len(),
                numbers(&wave.numbers)
            ),
            None if wave.absorbed > 0 => log!(
                "absorbing {} newly filed issue(s): {}",
                wave.numbers.len(),
                numbers(&wave.numbers)
            ),
            None => {}
        }
        handled.extend(wave.numbers.iter().copied());

        let fetched = match repo.fetch_issues(&wave.numbers) {
            Ok(fetched) => fetched,
            Err(e) => {
                logdim!("could not read the next wave: {e}");
                continue;
            }
        };
        // Named for the order the plans were written in, so the first is
        // plan.json exactly as before and no two waves overwrite each other.
        let plan_path = if plans_written == 0 {
            plan_out.to_path_buf()
        } else {
            plan_out.with_extension(format!("wave{plans_written}.json"))
        };
        plans_written += 1;
        // A triage failure is one CLI out of quota or refusing, in the cheapest
        // step of the next wave, on a run that may have spent an hour. It ends
        // the waves; it does not throw away the record of what the earlier ones
        // did, which is the only place a person learns what the run made.
        let plan = match make_plan(agents, cfg, repo, &fetched, &plan_path) {
            Ok(plan) => plan,
            Err(e) => {
                logwarn!("triage failed for {}: {e}", numbers(&wave.numbers));
                stopped.push(format!(
                    "triage failed for {}, so it was not worked: {}",
                    numbers(&wave.numbers),
                    first_line(&e.to_string())
                ));
                for later in &queue {
                    stopped.push(format!(
                        "{} was not reached after that failure",
                        numbers(&later.numbers)
                    ));
                }
                queue.clear();
                break;
            }
        };
        act_on_plan(cfg, repo, &plan);
        repo.remember_contested(&plan.contested);
        for item in &plan.contested {
            parked.push(format!(
                "#{} contested and parked for you: {}",
                item.issue,
                positions(&item.positions, &item.reasons)
            ));
        }
        for item in &plan.skipped {
            let what = if item.tracker {
                "left open as a tracker"
            } else if cfg.loop_cfg.close_skipped {
                "commented on and closed"
            } else {
                "commented on and left open"
            };
            parked.push(format!(
                "#{} skipped by both reviewers, {what}: {}",
                item.issue,
                item.reasons
                    .values()
                    .next()
                    .map(|r| first_line(r))
                    .unwrap_or_default()
            ));
        }

        // No recursion: a child that triage calls a tracker in its turn is
        // commented on and held like any other. `file_non_blocking` records
        // what happened the last time this codebase let something multiply.
        if cfg.loop_cfg.decompose_trackers && wave.parent.is_none() {
            for item in plan.skipped.iter().filter(|i| i.tracker) {
                let children = tracker::decompose(cfg, repo, item.issue);
                if !children.is_empty() {
                    queue.push_back(wave.child(item.issue, children));
                }
            }
        }

        let before = results.len();
        for item in &plan.order {
            let Some(issue) = fetched.iter().find(|i| i.number == item.issue) else {
                continue;
            };
            if let Some(why) = held_back(item, &plan, &landed) {
                log!("#{}: {why}", item.issue);
                let mut held = IssueRun::new(item.issue, item.title.clone());
                held.status = Status::Pending;
                held.notes.push(why);
                results.push(held);
                continue;
            }
            let run = review::run_nth_issue(agents, cfg, repo, item, issue, worked);
            worked += 1;
            if run.status == Status::Merged {
                landed.insert(item.issue);
            }
            results.push(run);
        }

        // Whatever this wave filed becomes the next one, budget allowing.
        let filed: Vec<i64> = results[before..]
            .iter()
            .flat_map(|r| r.filed.iter())
            .filter_map(|url| review::filed_issue_number(url))
            .filter(|n| !handled.contains(n))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if filed.is_empty() {
            continue;
        }
        match wave.absorbed < cfg.loop_cfg.absorb_new_issues {
            true => queue.push_back(wave.absorb(filed)),
            false => leftover.extend(filed),
        }
    }
    if !leftover.is_empty() && cfg.loop_cfg.absorb_new_issues > 0 {
        log!(
            "{} issue(s) filed in the last wave were left for a later run: {}",
            leftover.len(),
            numbers(&leftover.iter().copied().collect::<Vec<_>>())
        );
    }
    Ok(())
}

/// Both sides of a disagreement on one line, for the report and the log.
fn positions(
    positions: &std::collections::BTreeMap<String, String>,
    reasons: &std::collections::BTreeMap<String, String>,
) -> String {
    if positions.is_empty() {
        return "no positions recorded".to_string();
    }
    positions
        .iter()
        .map(|(name, side)| match reasons.get(name) {
            Some(why) if !why.trim().is_empty() => {
                format!("{name} says {side} ({})", first_line(why))
            }
            _ => format!("{name} says {side}"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn positions_of(known: &crate::model::Contested) -> String {
    positions(&known.positions, &known.reasons)
}

/// Why an item is not being worked yet, or `None` to work it.
///
/// Triage collects `depends_on` and `order` sorts by it, and the base was never
/// changed: `run_issue` creates every worktree from `origin/<base>` whatever
/// came before it. With `auto_merge = false`, the default and the recommended
/// setting, a dependency that ends approved is not on the base when the
/// dependent is implemented, and the implementor either fails, re-implements
/// it, or declares the issue not worth doing.
///
/// Holding is the honest answer: the work is not there, and building on a base
/// that lacks it spends a full issue's budget to find that out.
fn held_back(item: &PlanItem, plan: &Plan, landed: &BTreeSet<i64>) -> Option<String> {
    for dep in &item.depends_on {
        if landed.contains(dep) {
            continue;
        }
        if plan.order.iter().any(|other| other.issue == *dep) {
            return Some(format!(
                "held back: #{dep} is in this run and has not merged, so its work is not on the \
                 base this would be built from. Merge #{dep} and run #{} again.",
                item.issue
            ));
        }
        if plan.skipped.iter().any(|s| s.issue == *dep) {
            return Some(format!(
                "held back: #{dep}, which it depends on, was declined by both reviewers"
            ));
        }
        if plan.contested.iter().any(|c| c.issue == *dep) {
            return Some(format!(
                "held back: the reviewers disagree about #{dep}, which it depends on"
            ));
        }
    }
    None
}

/// One pass of the pipeline, and where it came from.
struct Wave {
    numbers: Vec<i64>,
    /// Absorb rounds spent to reach it. A tracker's children inherit their
    /// parent's, because extracting named work is not absorbing a follow-up:
    /// `absorb_new_issues` is off by default, and spending it here would make
    /// `decompose_trackers` silently do nothing.
    absorbed: u32,
    /// The tracker whose checklist this wave came out of.
    parent: Option<i64>,
}

impl Wave {
    fn first(numbers: Vec<i64>) -> Self {
        Self {
            numbers,
            absorbed: 0,
            parent: None,
        }
    }

    fn child(&self, tracker: i64, numbers: Vec<i64>) -> Self {
        Self {
            numbers,
            absorbed: self.absorbed,
            parent: Some(tracker),
        }
    }

    fn absorb(&self, numbers: Vec<i64>) -> Self {
        Self {
            numbers,
            absorbed: self.absorbed + 1,
            parent: None,
        }
    }
}

fn numbers(items: &[i64]) -> String {
    items
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn prepare(common: &Common, overrides: Option<Overrides>) -> Result<(Config, Repo, Vec<Agent>)> {
    let mut cfg = config::load(common.config.as_deref())?;

    if let Some(first) = &common.first {
        if !cfg.has_agent(first) {
            bail!("--first must be one of: {}", cfg.agent_names().join(", "));
        }
        cfg.first_implementor = first.clone();
    }
    if let Some(base) = &common.base {
        cfg.loop_cfg.base_branch = base.clone();
    }
    if let Some(min) = common.min_number {
        cfg.loop_cfg.min_number = min;
    }
    // Added to the config's, not in place of them. One is what this repository
    // always wants and the other is what today wants, and a flag that silenced
    // the standing set would be a trap: you would notice it the run after.
    if let Some(extra) = common.instructions.as_deref().map(str::trim) {
        if !extra.is_empty() {
            let standing = cfg.loop_cfg.instructions.trim();
            cfg.loop_cfg.instructions = if standing.is_empty() {
                extra.to_string()
            } else {
                format!("{standing}\n{extra}")
            };
        }
    }
    if let Some(over) = overrides {
        if let Some(v) = over.max_rounds {
            if v == 0 {
                bail!("--max-rounds must be at least 1");
            }
            cfg.loop_cfg.max_rounds = v;
        }
        if let Some(v) = over.auto_merge {
            cfg.loop_cfg.auto_merge = v;
        }
        if let Some(v) = over.keep_worktrees {
            cfg.loop_cfg.keep_worktrees = v;
        }
        if let Some(v) = over.worktrees {
            cfg.loop_cfg.worktrees = v;
        }
        if let Some(v) = over.close_skipped {
            cfg.loop_cfg.close_skipped = v;
        }
        if let Some(v) = over.absorb {
            cfg.loop_cfg.absorb_new_issues = v;
        }
    }

    let repo = Repo::open(&common.repo, &cfg)?;
    if common.base.is_none() {
        cfg.loop_cfg.base_branch = repo.default_branch(cfg.base_branch());
    }

    let agents = agent::build(&cfg)?;
    if let Some(warning) = agent::correlation_warning(&agents) {
        logging::warn(warning);
    }

    // Sweep finished worktrees before starting, so they cannot accumulate.
    for stale in repo.prune_worktrees(false) {
        let what = if stale.starts_with("branch ") {
            stale
        } else {
            format!("worktree {stale}")
        };
        logdim!("cleaned up finished {what}");
    }

    log!("repo {} base {}", repo.root().display(), cfg.base_branch());
    log!(
        "agents: {}, first {}",
        agents
            .iter()
            .map(|a| format!("{}={}", a.name(), a.spec.describe()))
            .collect::<Vec<_>>()
            .join(", "),
        cfg.first_implementor_policy()
    );
    Ok((cfg, repo, agents))
}

fn pick_issues(repo: &Repo, given: Vec<i64>, limit: usize, min_number: i64) -> Result<Vec<i64>> {
    if !given.is_empty() {
        // Naming a number is the point, so a floor never overrides it.
        if min_number > 0 {
            let below: Vec<String> = given
                .iter()
                .filter(|n| **n < min_number)
                .map(|n| format!("#{n}"))
                .collect();
            if !below.is_empty() {
                logdim!(
                    "{} below the #{min_number} floor, taking them because you named them",
                    below.join(", ")
                );
            }
        }
        return Ok(given);
    }
    let found = repo.list_open_issues(limit, min_number)?;
    if found.is_empty() {
        log!("no open issues");
        return Ok(found);
    }
    log!(
        "no issues given, taking {} open: {}",
        found.len(),
        found
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(found)
}

/// The bare `spar split`: every open issue and every open pull request, then
/// one screening call over both.
///
/// The first command that means both kinds when given nothing, so it says so
/// out loud. `--limit` applies to each list rather than to the two together: a
/// single budget shared across both would fill up on issues and starve the pull
/// requests. `min_number` applies because this is picked work rather than named
/// work.
///
/// One agent rules on the whole list in one call rather than two calls per
/// item, because two agent calls per item across a whole queue is the wrong
/// price for a question whose answer is usually no.
fn pick_for_split(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    limit: usize,
    mode: &split::Mode,
) -> Result<Vec<(i64, ItemKind)>> {
    let issues = repo.list_open_issues(limit, cfg.loop_cfg.min_number)?;
    let prs = repo.list_open_prs(limit, cfg.loop_cfg.min_number)?;
    log!(
        "no numbers given, considering {} open issue(s) and {} open pull request(s)",
        issues.len(),
        prs.len()
    );
    if issues.is_empty() && prs.is_empty() {
        return Ok(Vec::new());
    }

    let issue_rows = repo.open_issue_rows();
    let pr_rows = repo.open_pr_rows();
    let mut candidates: Vec<split::Candidate> = Vec::new();
    let mut already = 0usize;

    for number in &issues {
        let Some(row) = issue_rows.iter().find(|i| i.number == *number) else {
            logdim!("could not read #{number}, leaving it alone");
            continue;
        };
        // Free here, because the body is already in hand. A pull request that
        // has been split is caught by `split_pr`, which reads its comments.
        if split::already_split(row.body_text()) && !mode.again {
            already += 1;
            continue;
        }
        candidates.push(split::Candidate::from_issue(row));
    }
    for number in &prs {
        match pr_rows.iter().find(|p| p.number == *number) {
            Some(row) => candidates.push(split::Candidate::from_pr(row)),
            None => logdim!("could not read PR #{number}, leaving it alone"),
        }
    }
    if already > 0 {
        log!("{already} issue(s) already split, skipped. --again reopens them.");
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let agent = agent::find(agents, &cfg.first_implementor)?;
    log!(
        "screening {} item(s) with {}",
        candidates.len(),
        agent.name()
    );
    let verdicts = split::screen(agent, cfg, repo, &candidates)?;

    let mut picked = Vec::new();
    for candidate in &candidates {
        match verdicts.iter().find(|v| v.item == candidate.number) {
            Some(v) if v.split => {
                log!("  split #{}: {}", candidate.number, v.reason.trim());
                picked.push((candidate.number, candidate.kind));
            }
            // Not a warning: no is the expected answer, and one line per item
            // saying so is the whole screen printed twice.
            Some(v) => logdim!("  leave #{} whole: {}", candidate.number, v.reason.trim()),
            None => logdim!("  no verdict for #{}, leaving it whole", candidate.number),
        }
    }
    if picked.is_empty() {
        log!("nothing worth splitting");
    }
    Ok(picked)
}

/// Numbers split by what they actually name.
///
/// Issues and pull requests share one number sequence per repository, so a
/// person should not have to remember which command takes which. Both `run` and
/// `resume` sort the numbers themselves and route each one.
#[derive(Debug, Default)]
struct Sorted {
    issues: Vec<i64>,
    prs: Vec<i64>,
}

fn classify(repo: &Repo, numbers: &[i64]) -> Result<Sorted> {
    let mut sorted = Sorted::default();
    for number in numbers {
        match repo.item_kind(*number)? {
            ItemKind::Issue => sorted.issues.push(*number),
            ItemKind::Pr => sorted.prs.push(*number),
        }
    }
    if !sorted.issues.is_empty() && !sorted.prs.is_empty() {
        log!(
            "{} issue(s) and {} pull request(s) given",
            sorted.issues.len(),
            sorted.prs.len()
        );
    }
    Ok(sorted)
}

fn make_plan(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    issues: &[Issue],
    plan_out: &Path,
) -> Result<Plan> {
    let plan = triage::triage(agents, cfg, repo, issues)?;

    std::fs::write(plan_out, serde_json::to_vec_pretty(&plan)?)
        .map_err(|e| spar_err!("could not write {}: {e}", plan_out.display()))?;
    log!("plan written to {}", plan_out.display());

    for item in &plan.order {
        // The dependencies too: they decide the order, and an item held back
        // because one of them did not land is easier to read next to them.
        let after = if item.depends_on.is_empty() {
            String::new()
        } else {
            format!(
                " after {}",
                item.depends_on
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        log!(
            "  do   #{} [{}/{}]{after} {}",
            item.issue,
            item.complexity,
            item.risk,
            item.title
        );
    }
    for item in &plan.skipped {
        if item.tracker {
            log!(
                "  hold #{} (both reviewers: tracks work filed elsewhere)",
                item.issue
            );
        } else {
            log!("  skip #{} (both reviewers: not worth doing)", item.issue);
        }
    }
    for item in &plan.contested {
        log!("  ??   #{} contested, parked for you to decide", item.issue);
    }
    Ok(plan)
}

/// Post the shared reasoning on every issue both agents declined, and close it
/// when the config says so. Contested issues are never touched.
///
/// A tracker is never closed, whatever `close_skipped` says. Declining to open
/// a pull request for an umbrella is right, and closing it does not follow from
/// that: its parts are still open, and the shared context and the alternatives
/// somebody recorded against are the reason the issue exists. spar closed a
/// real one as "not planned" while all three of its subtasks were open, which
/// is what this exists to stop.
fn act_on_plan(cfg: &Config, repo: &Repo, plan: &Plan) {
    for item in &plan.skipped {
        let body = review::skip_comment(item, &repo.style);
        let close = cfg.loop_cfg.close_skipped && !item.tracker;
        let outcome = if close {
            repo.close_issue(item.issue, &body)
        } else {
            repo.comment_issue(item.issue, &body)
        };
        match outcome {
            Ok(()) if close => log!("  closed #{}", item.issue),
            Ok(()) if item.tracker => {
                log!(
                    "  left #{} open, it tracks work filed elsewhere",
                    item.issue
                )
            }
            Ok(()) => {}
            Err(e) => logdim!("could not update #{}: {e}", item.issue),
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn cmd_scrub_filter() -> Result<i32> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| spar_err!("could not read a commit message from stdin: {e}"))?;
    let out = style::scrub(&input, &crate::repo::style_from_env());
    let mut stdout = std::io::stdout();
    stdout
        .write_all(out.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|e| spar_err!("could not write the scrubbed message: {e}"))?;
    Ok(0)
}

fn cmd_clean(
    repo_path: &Path,
    config_path: Option<&Path>,
    all: bool,
    pr_state: bool,
) -> Result<i32> {
    let cfg = config::load(config_path)?;
    let repo = Repo::open(repo_path, &cfg)?;
    let mut removed = repo.prune_worktrees(all);
    removed.extend(repo.prune_state());
    if pr_state {
        removed.extend(repo.prune_pr_state(None));
    }
    if removed.is_empty() {
        if repo.write_summary().failed > 0 {
            println!("nothing removed");
        } else {
            println!("nothing to clean");
        }
    } else {
        for item in removed {
            println!("removed {item}");
        }
    }
    Ok(report_writes(&repo))
}

/// Post a review that was produced earlier and not sent.
fn cmd_post(
    prs: &[i64],
    repo_path: &Path,
    config_path: Option<&Path>,
    file: Option<&Path>,
    dry_run: bool,
) -> Result<i32> {
    let cfg = config::load(config_path)?;
    let repo = Repo::open(repo_path, &cfg)?;

    if file.is_some() && prs.len() > 1 {
        bail!("--file posts one review, so give it one pull request number");
    }

    let mut failed = false;
    for number in prs {
        let text = match file {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| spar_err!("could not read {}: {e}", path.display()))?,
            None => match repo.read_pending_comment(*number) {
                Some(text) => text,
                None => {
                    logging::error(format!(
                        "no saved review for PR #{number}. `spar review {number} --dry-run` \
                         produces one, or pass --file."
                    ));
                    failed = true;
                    continue;
                }
            },
        };
        if text.trim().is_empty() {
            logging::error(format!("the saved review for PR #{number} is empty"));
            failed = true;
            continue;
        }
        if dry_run {
            println!("\n{}\n", text.trim());
            log!("would post the above to PR #{number}");
            continue;
        }
        // Through the style gate like anything else spar sends, so an edit that
        // reintroduces a banned dash is caught rather than published.
        match repo.comment_pr(*number, &text) {
            Ok(()) => log!("posted to PR #{number}"),
            Err(e) => {
                logging::error(format!("could not post to PR #{number}: {e}"));
                failed = true;
            }
        }
    }
    Ok(i32::from(failed).max(report_writes(&repo)))
}

/// Append the settings a config does not mention, commented out.
///
/// Append only by design. Rewriting somebody's config to insert options would
/// take their comments and their ordering with it, and `--force` already exists
/// for anyone who wants the generated file back.
fn cmd_init_update(out: &Path) -> Result<i32> {
    let text = std::fs::read_to_string(out)
        .map_err(|e| spar_err!("could not read {}: {e}", out.display()))?;
    // Refuse to append to something that does not parse, rather than making a
    // broken config longer.
    config::parse(&text).map_err(|e| spar_err!("{} does not parse: {e}", out.display()))?;

    let unset = config::unmentioned_options(&text);
    if unset.is_empty() {
        println!("{} already mentions every setting.", out.display());
        return Ok(0);
    }

    let mut block = String::new();
    if !text.ends_with('\n') {
        block.push('\n');
    }
    block.push_str("\n# Added by `spar init --update`: settings this file did not mention,\n");
    block.push_str("# shown at their defaults. Uncomment one to change it.\n");
    let mut section = "";
    for option in &unset {
        if option.section != section {
            section = option.section;
            block.push_str(&format!("\n# [{section}]\n"));
        }
        // With the same note `spar init` writes. A config that gained a setting
        // this way used to gain a bare line and nothing saying what it was for,
        // which is the half of the setting that matters when you are reading it
        // for the first time.
        block.push('\n');
        block.push_str(&wrap_comment(note_for(&option.key)));
        block.push_str(&format!("# {} = {}\n", option.key, option.default));
    }

    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(out)
        .and_then(|mut f| f.write_all(block.as_bytes()))
        .map_err(|e| spar_err!("could not append to {}: {e}", out.display()))?;

    println!(
        "added {} setting(s) to {} as comments",
        unset.len(),
        out.display()
    );
    Ok(0)
}

fn cmd_init(out: &Path, force: bool) -> Result<i32> {
    if out.exists() && !force {
        logging::error(format!(
            "{} already exists. `--update` appends any settings it does not mention, \
             `--force` overwrites it.",
            out.display()
        ));
        return Ok(1);
    }

    let presets = config::available_presets();
    if presets.is_empty() {
        bail!("no presets available, which should be impossible in a released build");
    }

    let mut found: Vec<(String, PathBuf, config::AgentSpec)> = Vec::new();
    for name in &presets {
        let raw = config::load_preset(name)?;
        // A preset that will not build is a broken preset, not an uninstalled
        // CLI. Skipping it silently reported it as "missing" and sent people
        // looking for an install problem that was not there.
        let mut spec: config::AgentSpec = match raw
            .as_table()
            .cloned()
            .ok_or_else(|| spar_err!("not a table"))
            .and_then(|t| {
                toml::Value::Table(t)
                    .try_into()
                    .map_err(|e| spar_err!("{e}"))
            }) {
            Ok(spec) => spec,
            Err(e) => {
                println!("  BROKEN   {name:10} {}", e.first_line());
                continue;
            }
        };
        spec.name = name.clone();
        match Agent::new(spec.clone()).resolve_bin() {
            Ok(path) => {
                println!("  found    {name:10} {}", path.display());
                found.push((name.clone(), path.to_path_buf(), spec));
            }
            Err(_) => println!("  missing  {name}"),
        }
    }

    if found.len() < 2 {
        logging::error(format!(
            "need two agent CLIs, found {}. Install another, or write {} by hand using the \
             presets as a reference.",
            found.len(),
            out.display()
        ));
        return Ok(1);
    }

    // Prefer a pair that cannot share blind spots, if one is available.
    let chosen: Vec<&(String, PathBuf, config::AgentSpec)> = found.iter().take(2).collect();
    if found.len() > 2 {
        log!(
            "{} agents available, picking {} and {}. Edit {} to change.",
            found.len(),
            chosen[0].0,
            chosen[1].0,
            out.display()
        );
    }

    let mut text = String::from(
        "# Generated by `spar init`. Each agent inherits a command template from a\n\
         # built in preset; anything set here overrides it.\n\
         #\n\
         # Commented lines are the other options, each with a working value.\n\
         # Uncomment one to change it.\n\n",
    );
    for (name, _, spec) in &chosen {
        text.push_str(&agent_block(name, spec));
    }
    text.push_str(&settings_block(&chosen[0].0));

    std::fs::write(out, text).map_err(|e| spar_err!("could not write {}: {e}", out.display()))?;
    println!("\nwrote {}", out.display());
    println!("Next: `spar doctor` to check it, then `spar run` in a repo you have push access to.");
    Ok(0)
}

/// An agent's stand in, under the agent it stands in for.
///
/// Never counted against `doctor`'s exit code, deliberately. A fallback that is
/// not installed does not stop a run either, and a check that disagrees with
/// the runtime teaches people to ignore it.
fn report_fallback(agent: &Agent) {
    let Some(backup) = agent.fallback() else {
        return;
    };
    match backup.resolve_bin() {
        Ok(bin) => println!(
            "        fallback    {}  ({})",
            bin.display(),
            backup.spec.describe()
        ),
        Err(_) => println!(
            "        fallback    {} not found, so it will not stand in. Set {} to its path.",
            backup.program(),
            backup.env_key()
        ),
    }
}

/// One settable option: whether the generated config leaves it commented out,
/// its key, and the note beside it.
///
/// The value is deliberately absent. Every value comes from the defaults
/// themselves, because a value typed in here is a second copy of a number that
/// lives somewhere else, and the second copy is the one that goes stale. This
/// one did: the generated config offered a title budget of 90, a summary of
/// 200, a detail of 320, a body of 900 and an issue body of 4000, long after
/// those became 140, 2000, 6000, 8000 and 20000. Uncommenting a line to see
/// what it did cut every comment spar posts to a fifth of its length.
type Setting = (bool, &'static str, &'static str);

const LOOP_OPTIONS: &[Setting] = &[
    (false, "max_rounds", "In the custody loop, review rounds one invocation may spend asking for changes. A full-budget run may then use one extra closing pass. In `spar review`, this selects up to three phases: independent review, cross-adjudication, and rebuttal. Resuming a custody run grants a fresh budget, so this is not a lifetime cap."),
    (false, "auto_merge", "Merge when no blocking findings remain. Off on purpose: two models agreeing is not the same as being right, and neither carries the consequences."),
    (false, "first_implementor", "Which agent takes the first pass. The other one reviews it."),
    (false, "worktrees", "Isolate each issue in its own git worktree. Set false to work in the main checkout."),
    (false, "close_skipped", "Close an issue both reviewers declined, after posting the shared reasoning. A tracking issue is left open whatever this says."),
    (false, "followups", "Where a follow-up goes. issues files them, local writes .spar/followups.md and leaves the tracker alone, none drops them. `spar followup` works that file."),
    (true, "file_non_blocking", "File a non-blocking finding as a follow-up. Off, because not gating a merge is not the same as deserving somebody's triage queue."),
    (true, "max_followups", "Most follow-ups one run may record before it stops and says what it dropped. A backstop, not a target. `spar followup` is bounded by --limit instead."),
    (true, "max_split_parts", "Most parts `spar split` will make out of one issue or pull request. A backstop against a queue nobody asked for, not a target, and what it holds back is said out loud. A part is never itself split, so if one is still too big, `spar split <part>` is one command away."),
    (true, "keep_worktrees", "Keep worktrees after a run, for inspection."),
    (true, "min_number", "Ignore issues and pull requests numbered below this when spar picks for itself. 0 is no floor, and a number you name explicitly is always honoured."),
    (true, "parallel_triage", "Ask both agents to triage at once. They only read during triage, so there is nothing to serialise."),
    (true, "absorb_new_issues", "Waves of newly filed follow-ups to fold back into this run rather than leaving them for the next one. Multiplies what a run costs."),
    (true, "decompose_trackers", "Turn the checklist in a tracking issue into issues, link each item to the one covering it, tick an item off when its issue closes, and work the children in this run. Only ever acts on markdown task list items, so it is opt in per issue as well. `spar triage` prints what it would do without writing any of it."),
    (true, "max_tracker_children", "Most unchecked items from one tracker's checklist that one run will act on. A cap, not a target, and what it left is named out loud."),
    (true, "file_nits", "File nits as follow-ups too. Off, because a filed nit is somebody else's notification."),
    (true, "base_branch", "Only a fallback. Whatever origin/HEAD points at wins when it resolves."),
    (true, "branch_prefix", "Namespace the branches spar creates, for example \"spar/\". Without it they are issue-N, pr-N, and split-N-I with a suffix on repeated split names."),
    (true, "state_store", "Where resume state is kept. local uses .spar/state and keeps it off the pull request."),
    (true, "drafts", "Whether a pull request starts as a draft. until_approved opens one and marks it ready when the review converges, which is what the draft was saying while two agents were still arguing about it. always opens one and leaves it, and cannot be combined with auto_merge."),
    (true, "instructions", "Extra instructions handed to both agents with every request, for what this repository always wants that spar has no setting for. --instructions adds to this for one run."),
    (true, "max_issue_chars", "Most of one issue body that reaches a prompt. Sized so nothing a person wrote is cut, and a cut is said out loud when it happens."),
    (true, "max_triage_chars", "Most every issue body together may add to one triage prompt, or every recorded follow-up in one screening prompt. Past it, whole items wait for the next run rather than all of them losing their tails."),
    (true, "checkin_trust", "Whose comments `spar checkin` will act on. write is anybody GitHub says can write to this repository, which is the default because acting on a comment means pushing a commit to somebody's branch. anyone answers everyone, and still only changes code when both agents agree."),
    (true, "checkin_resolve", "Mark a review thread resolved when spar made the change it asked for. A thread spar disagreed with is left open whatever this says."),
    (true, "max_checkin_comments", "Most unanswered comments spar will answer on one pull request in a run. A backstop against a long argument being read back to somebody, not a target."),
];

const STYLE_OPTIONS: &[Setting] = &[
    (false, "ban_em_dash", "Strip em-dashes and en-dashes from everything spar posts, then refuse to post text that still has one."),
    (false, "ban_ai_attribution", "Strip mentions of the tooling, and Co-Authored-By trailers, from everything spar posts."),
    (false, "terse", "Hold model prose to a length budget. false removes the valves entirely."),
    (true, "pr_comments", "How much of its own working spar narrates into a pull request thread. outcome is one comment at the end, rounds is an audit trail, none never comments at all."),
    (true, "max_title_chars", "A finding, issue, or pull request title. Never ellipsised: a title ending in three dots reads as broken."),
    (true, "max_summary_chars", "A one line verdict, or a refutation's argument."),
    (true, "max_detail_chars", "A blocking finding's explanation, as it appears in the pull request thread."),
    (true, "max_body_chars", "A pull request body."),
    (true, "max_issue_body_chars", "A filed issue's body. Far larger on purpose: an issue is picked up cold months later. Fenced code blocks are never truncated and never count against it."),
];

/// The `[loop]` and `[style]` blocks of a generated config.
///
/// Safety valves, not editors: the length budgets here are sized so real
/// content is never touched, which is why they read as large numbers.
fn settings_block(first_implementor: &str) -> String {
    let defaults: std::collections::BTreeMap<String, String> = config::known_options()
        .into_iter()
        .map(|option| (option.key, option.default))
        .collect();
    // first_implementor has no default: it is whichever agent was written
    // first, and until there is a config there is no answer to give.
    let value = |key: &str| match key {
        "first_implementor" => format!("\"{first_implementor}\""),
        other => defaults.get(other).cloned().unwrap_or_default(),
    };

    let mut out = String::from("[loop]\n");
    out.push_str(&option_lines(LOOP_OPTIONS, &value));
    out.push_str(concat!(
        "\n[loop.effort_schedule]\n",
        "# One key per kind of call, so a value means what its name says. Values\n",
        "# are whatever each agent's own CLI accepts, listed above, so these are\n",
        "# examples rather than defaults. Left out, a call falls back to\n",
        "# round_1 or rest, and then to the effort the agent's own block asked\n",
        "# for. Where the two CLIs do not share a vocabulary, put the schedule\n",
        "# under [agents.NAME.effort_schedule] instead, with the same keys.\n",
        "# triage      = \"low\"    # both agents, over the whole queue, per wave\n",
        "# implement   = \"high\"   # writing the first draft\n",
        "# review_1    = \"high\"   # the deep first review\n",
        "# review_rest = \"low\"    # later rounds, which see a small delta\n",
        "# respond     = \"high\"   # the author answering a review\n",
        "# close       = \"low\"    # the closing merge safety pass\n",
        "# screen      = \"low\"    # the follow-up screen, one call for the queue\n",
        "# checkin     = \"high\"   # judging comments other people left\n",
        "# split       = \"high\"   # proposing and checking a split\n",
        "# round_1     = \"high\"   # read by any call above with no key set\n",
        "# rest        = \"low\"    # the same, for later rounds and the close\n\n",
    ));
    out.push_str("[style]\n");
    out.push_str(&option_lines(STYLE_OPTIONS, &value));
    out
}

/// Option lines with their notes lined up in a column, a long note wrapping
/// onto continuation lines that stay in the column rather than running off the
/// edge or restarting at the margin.
fn option_lines(options: &[Setting], value: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    for (commented, key, note) in options {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&wrap_comment(note));
        let lead = if *commented { "# " } else { "" };
        out.push_str(&format!("{lead}{key} = {}\n", value(key)));
    }
    out
}

/// One prerequisite check: a label and something that either reports a version
/// or explains what is missing.
type Probe = Box<dyn Fn() -> Result<String>>;

/// One agent's block, with the options commented out beside a working value.
///
/// The values come from the preset rather than from here, so a CLI that adds a
/// model is a file edit. They are hints only: nothing validates against them,
/// because a stale list that refused a model which actually works would be
/// worse than no hint at all.
fn agent_block(name: &str, spec: &config::AgentSpec) -> String {
    let mut out = format!("[agents.{name}]\npreset = \"{name}\"\n");

    // Only what the preset has hints for. An option with none used to be
    // written as `# effort = "..."`, and a placeholder is not a working value:
    // the line fails the moment somebody takes the file at its word and
    // uncomments it. Cursor has no effort setting at all, so for that agent the
    // line should not exist rather than exist and be wrong.
    //
    // The first entry of each list is the one written as the suggested value,
    // which is why the presets put the sensible default there rather than in
    // whatever order a CLI's help happens to print.
    // Once, above both. The preset's note is about the pair, and repeating it
    // under each put a sentence about models underneath the effort line.
    if let Some(extra) = &spec.options_note {
        if !spec.models.is_empty() || !spec.efforts.is_empty() {
            out.push('\n');
            out.push_str(&wrap_comment(extra));
        }
    }
    for (key, choices) in [("model", &spec.models), ("effort", &spec.efforts)] {
        let Some(suggested) = choices.first() else {
            continue;
        };
        let mut note = format!("Omit {key} to use the CLI's own default.");
        if choices.len() > 1 {
            note.push_str(&format!(" One of: {}.", choices.join(" | ")));
        }
        out.push('\n');
        out.push_str(&wrap_comment(&note));
        out.push_str(&format!("# {key} = \"{suggested}\"\n"));
    }

    // The value from the spec, not a number typed here, for the reason the
    // [loop] block learned: a second copy of a default is the one that goes
    // stale.
    out.push('\n');
    out.push_str(&wrap_comment(
        "Seconds one call may take before spar gives up. A timeout costs the whole call and is \
         never retried, so err long.",
    ));
    out.push_str(&format!("# timeout = {}\n", spec.timeout));

    // Anything but this agent's own preset: a CLI that has just refused is not
    // a stand in for itself.
    let backup = if name == "cursor" { "gemini" } else { "cursor" };
    out.push('\n');
    out.push_str(&wrap_comment(
        "A stand in for when this CLI refuses, stalls, or runs out of quota. It answers in place \
         of this agent, never alongside it.",
    ));
    out.push_str(&format!(
        "# [agents.{name}.fallback]\n# preset = \"{backup}\"\n"
    ));

    // The rest of what an agent block takes defines a CLI rather than tunes
    // one, so it is pointed at rather than offered: a generated file that
    // invites somebody to edit `command` or `output` on a working preset is
    // offering them a way to break it.
    out.push('\n');
    out.push_str(&wrap_comment(
        "command, output, search_paths and the rest are in spar.example.toml, for pairing a CLI \
         that has no preset.",
    ));
    out.push('\n');
    out
}

/// What `spar init` says about an option, for `--update` to say too.
///
/// Empty for one with nothing written about it, and for the effort schedule,
/// whose two keys are examples rather than settings and are described by the
/// stanza they sit in rather than one at a time.
fn note_for(key: &str) -> &'static str {
    LOOP_OPTIONS
        .iter()
        .chain(STYLE_OPTIONS)
        .find(|(_, name, _)| *name == key)
        .map(|(_, _, note)| *note)
        .unwrap_or("")
}

/// Wrap a note across comment lines so a long one does not run off the edge.
fn wrap_comment(text: &str) -> String {
    const WIDTH: usize = 76;
    let mut out = String::new();
    let mut line = String::from("#");
    for word in text.split_whitespace() {
        if line.chars().count() + 1 + word.chars().count() > WIDTH && line.len() > 1 {
            out.push_str(&line);
            out.push('\n');
            line = String::from("#");
        }
        line.push(' ');
        line.push_str(word);
    }
    if line.len() > 1 {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn cmd_doctor(config_path: Option<&Path>) -> Result<i32> {
    let mut ok = true;

    let probes: Vec<(&str, Probe)> = vec![
        (
            "git",
            Box::new(|| {
                proc::run_str(&["git", "--version"], &ExecOpts::new().timeout_secs(30))
                    .map(|s| first_line(&s))
            }),
        ),
        (
            "gh",
            Box::new(|| {
                proc::run_str(&["gh", "--version"], &ExecOpts::new().timeout_secs(30))
                    .map(|s| first_line(&s))
            }),
        ),
        (
            "gh auth",
            Box::new(|| {
                let out = proc::exec(
                    &["gh".into(), "auth".into(), "status".into()],
                    &ExecOpts::new().check(false).timeout_secs(60),
                )?;
                let text = format!("{}\n{}", out.stderr.trim(), out.stdout.trim());
                if out.ok() {
                    Ok(first_line(&text))
                } else {
                    Err(spar_err!("not authenticated. Run `gh auth login`."))
                }
            }),
        ),
    ];

    for (label, probe) in probes {
        match probe() {
            Ok(detail) => println!("  ok    {label:12} {detail}"),
            Err(e) => {
                println!("  FAIL  {label:12} {}", e.first_line());
                ok = false;
            }
        }
    }

    let found = config::find_config(config_path)?;
    let Some(path) = found else {
        println!("\n  no spar.toml found. Run `spar init` to generate one.");
        println!(
            "  presets available: {}",
            config::available_presets().join(", ")
        );
        return Ok(if ok { 0 } else { 1 });
    };

    println!("\n  config: {}", path.display());
    let cfg = match config::load(Some(&path)) {
        Ok(cfg) => cfg,
        Err(e) => {
            println!("  FAIL  config       {e}");
            return Ok(1);
        }
    };

    // Kept apart from `ok`: a missing gh says nothing about whether the two
    // agents are the same CLI, and must not silence the warning below.
    let mut resolved = Vec::new();
    for spec in &cfg.agents {
        let agent = Agent::new(spec.clone());
        match agent.resolve_bin() {
            Ok(bin) => {
                println!(
                    "  ok    {:12} {}  ({})",
                    spec.name,
                    bin.display(),
                    spec.describe()
                );
                report_fallback(&agent);
                resolved.push(agent);
            }
            Err(e) => {
                println!("  FAIL  {:12} {}", spec.name, e.first_line());
                ok = false;
            }
        }
    }

    if resolved.len() == cfg.agents.len() {
        if let Some(warning) = agent::correlation_warning(&resolved) {
            println!("\n  WARNING  {warning}");
        }
    }

    for line in unknown_effort_values(&cfg) {
        println!("\n  WARNING  {line}");
    }

    println!(
        "\n  settings: first={} max_rounds={} auto_merge={} worktrees={} followups={} terse={}",
        cfg.first_implementor_policy(),
        cfg.loop_cfg.max_rounds,
        cfg.loop_cfg.auto_merge,
        cfg.loop_cfg.worktrees,
        cfg.loop_cfg.followups,
        cfg.style.terse
    );
    // What somebody upgrading wants to know. `spar init` refuses to touch an
    // existing config, so without this there is no way to learn that a release
    // added a setting short of reading the source.
    if let Ok(text) = std::fs::read_to_string(&path) {
        let unset = config::unmentioned_options(&text);
        if !unset.is_empty() {
            println!(
                "\n  {} setting(s) this config does not mention, all at their defaults:",
                unset.len()
            );
            for option in &unset {
                println!(
                    "      [{}] {} = {}",
                    option.section, option.key, option.default
                );
            }
            println!(
                "  `spar init --update {}` appends them as comments.",
                path.display()
            );
        }
    }

    println!(
        "{}",
        if ok {
            "\nready"
        } else {
            "\nmissing prerequisites"
        }
    );
    Ok(if ok { 0 } else { 1 })
}

/// Scheduled effort values the agent that would receive them is not known to
/// accept.
///
/// A warning rather than a refusal, for the reason the hints themselves carry:
/// a CLI's options drift, and a stale allow list that refused a value which
/// actually works would be worse than no hint at all. But `round_1 = "ultra"`
/// against a claude agent is a call that fails at the CLI, and nothing said so
/// until it ran.
fn unknown_effort_values(cfg: &config::Config) -> Vec<String> {
    let mut out = Vec::new();
    for spec in &cfg.agents {
        if spec.efforts.is_empty() {
            continue;
        }
        let mut said: Vec<String> = Vec::new();
        for call in config::Call::every() {
            let Some(value) = cfg.effort_for(spec, call) else {
                continue;
            };
            let known = spec
                .efforts
                .iter()
                .any(|hint| hint.eq_ignore_ascii_case(value.trim()));
            if known || said.contains(&value) {
                continue;
            }
            said.push(value.clone());
            out.push(format!(
                "{} is asked for effort \"{value}\" on the {} call, which is not one it is known \
                 to accept ({}). Put this agent's values under \
                 [agents.{}.effort_schedule].",
                spec.name,
                call.key(),
                spec.efforts.join(" | "),
                spec.name
            ));
        }
    }
    out
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn report(results: &[IssueRun], cfg: &Config, repo: &Repo) -> i32 {
    report_with(results, cfg, repo, &[], &[])
}

/// The same, plus what needs a person: waves that never ran, and the issues
/// triage parked or declined.
///
/// `stopped` is work that did not happen and makes the run non-zero. `parked`
/// is an outcome rather than a failure, and is the record of what is waiting on
/// a decision, which used to reach a person only through plan.json.
fn report_with(
    results: &[IssueRun],
    cfg: &Config,
    repo: &Repo,
    stopped: &[String],
    parked: &[String],
) -> i32 {
    println!("\n{}", "=".repeat(60));
    for r in results {
        println!(
            "#{:<5} {:<10} rounds={} {}",
            r.issue,
            r.status.to_string(),
            r.rounds,
            r.pr.as_deref().unwrap_or("")
        );
        for note in &r.notes {
            println!("       {}", first_line(note));
        }
        for url in &r.filed {
            println!("       filed {url}");
        }
        for dispute in &r.disputes {
            println!(
                "       disputed: {}",
                report_item(&dispute.title, &dispute.file)
            );
        }
        for finding in &r.noted {
            println!(
                "       noted, not blocking: {}",
                report_item(&finding.title, &finding.file)
            );
        }
    }
    println!("{}", "=".repeat(60));

    if cfg.alternate_first && results.len() > 1 {
        println!(
            "\nfirst implementor alternated, starting with {}. Who wrote each first draft is in \
             .spar/state/implementors.json.",
            cfg.first_implementor
        );
    }
    if !cfg.loop_cfg.auto_merge && results.iter().any(|r| r.status == Status::Approved) {
        println!("\nApproved PRs are waiting on you to merge.");
    }
    let recorded: usize = results.iter().map(|r| r.filed.len()).sum();
    if recorded > 0 && cfg.loop_cfg.followups == crate::config::Followups::Local {
        println!(
            "\n{recorded} follow-up(s) recorded in .spar/followups.md, not on the tracker. \
             Set followups = \"issues\" to file them."
        );
    }
    for lines in [parked, stopped] {
        if lines.is_empty() {
            continue;
        }
        println!();
        for line in lines {
            println!("{line}");
        }
    }
    let issue_exit = i32::from(!results.iter().all(IssueRun::succeeded) || !stopped.is_empty());
    issue_exit.max(report_writes(repo))
}

/// A failed write makes the final status non-zero, including a partial failure.
/// Independent writes still finish first, so one failure does not discard work
/// that another item can land. The non-zero status lets automation retry what
/// was missed.
fn report_writes(repo: &Repo) -> i32 {
    let writes = repo.write_summary();
    if let Some(line) = write_summary_line(writes) {
        println!("\n{line}");
    }
    write_exit_code(writes)
}

fn write_summary_line(writes: WriteSummary) -> Option<String> {
    (writes.attempted > 0).then(|| {
        format!(
            "writes: {} attempted, {} succeeded, {} failed",
            writes.attempted,
            writes.succeeded(),
            writes.failed
        )
    })
}

fn write_exit_code(writes: WriteSummary) -> i32 {
    i32::from(writes.failed > 0)
}

fn report_item(title: &str, file: &str) -> String {
    match file.trim() {
        "" => title.to_string(),
        location => format!("{title} ({location})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn planned(issue: i64, depends_on: Vec<i64>) -> PlanItem {
        PlanItem {
            issue,
            title: format!("issue {issue}"),
            complexity: crate::model::Complexity::S,
            risk: crate::model::Risk::Low,
            depends_on,
            reason: "worth doing".into(),
        }
    }

    /// Ordering by dependency did half a job: it sorted, and then every
    /// worktree was still built from origin/<base>.
    ///
    /// With auto_merge off, the default and the recommended setting, a
    /// dependency that ends approved is not on the base when the dependent is
    /// implemented. The implementor then fails, re-implements the dependency,
    /// or declares the issue not worth doing, having spent a full issue's
    /// budget to find that out.
    #[test]
    fn a_dependent_waits_for_a_dependency_that_has_not_landed() {
        let plan = Plan {
            order: vec![planned(9, vec![]), planned(10, vec![9])],
            ..Plan::default()
        };
        let mut landed = BTreeSet::new();

        let why = held_back(&plan.order[1], &plan, &landed).expect("held");
        assert!(why.contains("#9"), "{why}");
        assert!(why.contains("has not merged"), "{why}");
        assert!(held_back(&plan.order[0], &plan, &landed).is_none());

        // Merged is the only thing that puts the work on the base.
        landed.insert(9);
        assert!(held_back(&plan.order[1], &plan, &landed).is_none());
    }

    /// The case the ordering stayed quietest about: a dependency nobody is
    /// going to do.
    #[test]
    fn a_dependent_on_a_declined_or_contested_issue_is_held_with_the_reason() {
        let declined = Plan {
            order: vec![planned(10, vec![9])],
            skipped: vec![crate::model::SkippedItem {
                issue: 9,
                title: "issue 9".into(),
                reasons: Default::default(),
                tracker: false,
            }],
            ..Plan::default()
        };
        let why = held_back(&declined.order[0], &declined, &BTreeSet::new()).expect("held");
        assert!(why.contains("declined by both reviewers"), "{why}");

        let contested = Plan {
            order: vec![planned(10, vec![9])],
            contested: vec![crate::model::ContestedItem {
                issue: 9,
                title: "issue 9".into(),
                positions: Default::default(),
                reasons: Default::default(),
                note: None,
            }],
            ..Plan::default()
        };
        let why = held_back(&contested.order[0], &contested, &BTreeSet::new()).expect("held");
        assert!(why.contains("disagree about #9"), "{why}");
    }

    /// A dependency that is not in this run at all is somebody else's business:
    /// it may well be merged already, and holding on it would stop work that is
    /// perfectly buildable. It is logged by `report_dependencies` instead.
    #[test]
    fn a_dependency_outside_the_run_does_not_hold_anything() {
        let plan = Plan {
            order: vec![planned(10, vec![900])],
            ..Plan::default()
        };
        assert!(held_back(&plan.order[0], &plan, &BTreeSet::new()).is_none());
    }

    /// A disagreement is the one triage outcome that needs a person, and it
    /// reached them least reliably: a log line `--quiet` suppresses, an entry
    /// in plan.json, and nothing in the report at all.
    /// `round_1 = "ultra"` against a claude agent is a call that fails at the
    /// CLI, and nothing said so until it ran.
    #[test]
    fn doctor_says_when_a_scheduled_effort_is_not_one_the_agent_accepts() {
        let text = "\
[agents.claude]
preset = \"claude\"

[agents.codex]
preset = \"codex\"

[loop.effort_schedule]
review_1 = \"ultra\"
";
        let cfg = config::parse(text).expect("parses");
        let said = unknown_effort_values(&cfg);
        assert_eq!(1, said.len(), "{said:?}");
        assert!(said[0].starts_with("claude"), "{}", said[0]);
        assert!(said[0].contains("ultra"), "{}", said[0]);
        assert!(said[0].contains("review_1"), "{}", said[0]);
        assert!(
            said[0].contains("[agents.claude.effort_schedule]"),
            "it has to say where to put the right value: {}",
            said[0]
        );

        // With the value in each agent's own words, nothing is said.
        let quiet = text.replace(
            "[loop.effort_schedule]\nreview_1 = \"ultra\"\n",
            "[agents.claude.effort_schedule]\nreview_1 = \"xhigh\"\n\n\
             [agents.codex.effort_schedule]\nreview_1 = \"ultra\"\n",
        );
        assert!(unknown_effort_values(&config::parse(&quiet).expect("parses")).is_empty());
    }

    #[test]
    fn a_contested_issue_reads_as_both_positions_with_their_reasons() {
        let mut positions_map = std::collections::BTreeMap::new();
        positions_map.insert("claude".to_string(), "do".to_string());
        positions_map.insert("codex".to_string(), "skip".to_string());
        let mut reasons = std::collections::BTreeMap::new();
        reasons.insert(
            "claude".to_string(),
            "the retry path is still wrong".to_string(),
        );
        reasons.insert(
            "codex".to_string(),
            "fixed in the rewrite last month".to_string(),
        );

        let line = positions(&positions_map, &reasons);
        assert!(line.contains("claude says do"), "{line}");
        assert!(line.contains("codex says skip"), "{line}");
        assert!(line.contains("the retry path is still wrong"), "{line}");
        assert!(line.contains("fixed in the rewrite"), "{line}");

        // A verdict with no reason still names who said what.
        let bare = positions(&positions_map, &Default::default());
        assert!(bare.contains("claude says do"), "{bare}");
    }

    #[test]
    fn report_items_include_their_location() {
        assert_eq!(
            "Same title (src/a.rs:10)",
            report_item("Same title", "src/a.rs:10")
        );
        assert_eq!("General point", report_item("General point", ""));
    }

    #[test]
    fn every_attempted_write_failing_is_a_failed_run() {
        let writes = WriteSummary {
            attempted: 3,
            failed: 3,
        };

        assert_eq!(1, write_exit_code(writes));
        assert_eq!(
            Some("writes: 3 attempted, 0 succeeded, 3 failed".to_string()),
            write_summary_line(writes)
        );
    }

    #[test]
    fn a_partial_write_failure_is_reported_after_the_run() {
        let writes = WriteSummary {
            attempted: 3,
            failed: 1,
        };

        assert_eq!(1, write_exit_code(writes));
        assert_eq!(2, writes.succeeded());
    }

    #[test]
    fn the_parser_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn quiet_is_accepted_before_or_after_the_subcommand() {
        for argv in [
            vec!["spar", "--quiet", "run", "42"],
            vec!["spar", "run", "42", "--quiet"],
            vec!["spar", "resume", "--quiet"],
            vec!["spar", "init", "-q"],
        ] {
            assert!(Cli::parse_from(&argv).quiet, "{argv:?}");
        }
        assert!(!Cli::parse_from(["spar", "run", "42"]).quiet);
    }

    #[test]
    fn several_issue_numbers_are_accepted() {
        let cli = Cli::parse_from(["spar", "run", "42", "51", "60"]);
        match cli.command {
            Command::Run { issues, .. } => assert_eq!(vec![42, 51, 60], issues),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn issue_numbers_and_flags_can_be_interleaved() {
        let cli = Cli::parse_from(["spar", "run", "42", "--auto-merge", "51"]);
        match cli.command {
            Command::Run {
                issues, loop_flags, ..
            } => {
                assert_eq!(vec![42, 51], issues);
                assert!(loop_flags.auto_merge);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_command_that_reads_a_config_accepts_one() {
        for argv in [
            vec!["spar", "run", "42"],
            vec!["spar", "triage"],
            vec!["spar", "resume"],
            vec!["spar", "followup"],
            vec!["spar", "checkin"],
            vec!["spar", "split"],
            vec!["spar", "clean"],
            vec!["spar", "doctor"],
        ] {
            let mut full = argv.clone();
            full.extend(["--config", "other.toml"]);
            let cli = Cli::parse_from(&full);
            let config = match cli.command {
                Command::Run { common, .. }
                | Command::Triage { common, .. }
                | Command::Resume { common, .. }
                | Command::Followup { common, .. }
                | Command::Split { common, .. }
                | Command::Checkin { common, .. } => common.config,
                Command::Clean { config, .. } | Command::Doctor { config } => config,
                other => panic!("{other:?}"),
            };
            assert_eq!(Some(PathBuf::from("other.toml")), config, "{argv:?}");
        }
    }

    #[test]
    fn auto_merge_is_off_unless_asked_for() {
        let cli = Cli::parse_from(["spar", "run"]);
        match cli.command {
            Command::Run { loop_flags, .. } => assert!(!loop_flags.auto_merge),
            other => panic!("{other:?}"),
        }
    }

    /// The flag reaches resume as well as run, which is what makes "stop, edit
    /// the config, carry on with something extra to say" a thing you can do.
    #[test]
    fn every_command_that_reads_a_config_takes_instructions() {
        // `followup` is given no number, because it takes none: its entries
        // have no identity a person could type.
        for argv in [
            vec!["spar", "run", "7", "--instructions", "Do not wait for CI."],
            vec![
                "spar",
                "triage",
                "7",
                "--instructions",
                "Do not wait for CI.",
            ],
            vec![
                "spar",
                "resume",
                "7",
                "--instructions",
                "Do not wait for CI.",
            ],
            vec![
                "spar",
                "review",
                "7",
                "--instructions",
                "Do not wait for CI.",
            ],
            vec!["spar", "followup", "--instructions", "Do not wait for CI."],
            vec![
                "spar",
                "checkin",
                "7",
                "--instructions",
                "Do not wait for CI.",
            ],
            vec![
                "spar",
                "split",
                "7",
                "--instructions",
                "Do not wait for CI.",
            ],
        ] {
            let parsed = Cli::parse_from(&argv);
            let common = match parsed.command {
                Command::Run { common, .. }
                | Command::Triage { common, .. }
                | Command::Resume { common, .. }
                | Command::Review { common, .. }
                | Command::Followup { common, .. }
                | Command::Split { common, .. }
                | Command::Checkin { common, .. } => common,
                other => panic!("{other:?}"),
            };
            assert_eq!(
                Some("Do not wait for CI."),
                common.instructions.as_deref(),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn the_two_close_skipped_flags_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from(["spar", "run", "--close-skipped", "--no-close-skipped"]).is_err()
        );
    }

    /// Only `run` triages, so only `run` can decline an issue. Accepting the
    /// flag on `resume` would silently do nothing.
    #[test]
    fn close_skipped_is_offered_only_where_it_means_something() {
        assert!(Cli::try_parse_from(["spar", "run", "--close-skipped"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "run", "--no-close-skipped"]).is_ok());
        // `followup` triages what it files, so it can decline it too.
        assert!(Cli::try_parse_from(["spar", "followup", "--close-skipped"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "resume", "--close-skipped"]).is_err());
        assert!(Cli::try_parse_from(["spar", "review", "--close-skipped"]).is_err());
        assert!(Cli::try_parse_from(["spar", "triage", "--close-skipped"]).is_err());
    }

    /// An entry in the follow-up queue has no number and its title is prose, so
    /// a number on the command line could only be silently ignored.
    #[test]
    fn followup_takes_no_numbers() {
        assert!(Cli::try_parse_from(["spar", "followup"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "followup", "42"]).is_err());
    }

    /// `--screen-only` stops before `--file-only` does, so asking for both says
    /// nothing about where to stop.
    #[test]
    fn the_two_stopping_points_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["spar", "followup", "--screen-only"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "followup", "--file-only"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "followup", "--screen-only", "--file-only"]).is_err());
    }

    #[test]
    fn the_close_skipped_pair_resolves_to_a_tristate() {
        let read = |argv: &[&str]| match Cli::parse_from(argv).command {
            Command::Run { triage_flags, .. } => {
                match (triage_flags.close_skipped, triage_flags.no_close_skipped) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                }
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(None, read(&["spar", "run"]));
        assert_eq!(Some(true), read(&["spar", "run", "--close-skipped"]));
        assert_eq!(Some(false), read(&["spar", "run", "--no-close-skipped"]));
    }

    #[test]
    fn the_default_limit_is_twenty() {
        let cli = Cli::parse_from(["spar", "run"]);
        match cli.command {
            Command::Run { common, .. } => assert_eq!(20, common.limit),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_scrub_filter_subcommand_is_hidden_but_reachable() {
        assert!(matches!(
            Cli::parse_from(["spar", "scrub-filter"]).command,
            Command::ScrubFilter
        ));
        let help = Cli::command().render_long_help().to_string();
        assert!(
            !help.contains("scrub-filter"),
            "it is plumbing, not a command"
        );
    }

    #[test]
    fn review_takes_pr_numbers_and_a_dry_run() {
        let cli = Cli::parse_from(["spar", "review", "101", "102", "--dry-run"]);
        match cli.command {
            Command::Review { items, dry_run, .. } => {
                assert_eq!(vec![101, 102], items);
                assert!(dry_run);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn review_posts_unless_told_not_to() {
        match Cli::parse_from(["spar", "review", "101"]).command {
            Command::Review { dry_run, .. } => assert!(!dry_run),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn review_with_no_numbers_is_allowed() {
        match Cli::parse_from(["spar", "review"]).command {
            Command::Review { items, .. } => assert!(items.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn review_takes_its_own_round_budget() {
        match Cli::parse_from(["spar", "review", "101", "--max-rounds", "2"]).command {
            Command::Review { max_rounds, .. } => assert_eq!(Some(2), max_rounds),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn checkin_takes_pr_numbers_and_a_dry_run() {
        match Cli::parse_from(["spar", "checkin", "108", "112", "--dry-run"]).command {
            Command::Checkin { items, dry_run, .. } => {
                assert_eq!(vec![108, 112], items);
                assert!(dry_run);
            }
            other => panic!("{other:?}"),
        }
        match Cli::parse_from(["spar", "checkin"]).command {
            Command::Checkin { items, dry_run, .. } => {
                assert!(items.is_empty());
                assert!(!dry_run);
            }
            other => panic!("{other:?}"),
        }
    }

    /// `--auto-merge` must not exist on the one command whose input is written
    /// by somebody else, and `--max-rounds` would be worse than useless: the
    /// judgement is two passes by construction, so `--max-rounds 1` could only
    /// mean "let one agent decide alone", which removes the one thing standing
    /// between a stranger's comment and a push.
    #[test]
    fn checkin_offers_no_flag_that_would_weaken_the_pair() {
        assert!(Cli::try_parse_from(["spar", "checkin", "--auto-merge"]).is_err());
        assert!(Cli::try_parse_from(["spar", "checkin", "--max-rounds", "1"]).is_err());
        assert!(Cli::try_parse_from(["spar", "checkin", "--absorb", "1"]).is_err());
        assert!(Cli::try_parse_from(["spar", "checkin", "--close-skipped"]).is_err());
        // What it does offer.
        assert!(Cli::try_parse_from(["spar", "checkin", "--reply-only"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "checkin", "--any-author"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "checkin", "--again"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "checkin", "--keep-worktrees"]).is_ok());
    }

    #[test]
    fn resume_takes_a_next_override() {
        let cli = Cli::parse_from(["spar", "resume", "108", "--next", "codex"]);
        match cli.command {
            Command::Resume {
                prs, next_actor, ..
            } => {
                assert_eq!(vec![108], prs);
                assert_eq!(Some("codex".to_string()), next_actor);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod absorb_tests {
    use super::*;

    #[test]
    fn absorb_is_off_unless_asked_for() {
        match Cli::parse_from(["spar", "run"]).command {
            Command::Run { loop_flags, .. } => assert_eq!(None, loop_flags.absorb),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn absorb_takes_a_wave_count() {
        match Cli::parse_from(["spar", "run", "--absorb", "2"]).command {
            Command::Run { loop_flags, .. } => assert_eq!(Some(2), loop_flags.absorb),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn absorb_is_only_offered_where_issues_are_worked() {
        assert!(Cli::try_parse_from(["spar", "run", "--absorb", "1"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "resume", "--absorb", "1"]).is_ok());
        assert!(Cli::try_parse_from(["spar", "review", "--absorb", "1"]).is_err());
        // `split` decomposes and stops, so there is nothing for a wave of newly
        // filed issues to be absorbed into.
        assert!(Cli::try_parse_from(["spar", "split", "--absorb", "1"]).is_err());
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    #[test]
    fn split_takes_numbers_of_either_kind() {
        match Cli::parse_from(["spar", "split", "10", "12"]).command {
            Command::Split { items, .. } => assert_eq!(vec![10, 12], items),
            other => panic!("{other:?}"),
        }
    }

    /// The bare form means both kinds, which no other command does, so it has
    /// to be allowed to take nothing.
    #[test]
    fn split_with_no_numbers_is_allowed() {
        match Cli::parse_from(["spar", "split"]).command {
            Command::Split {
                items,
                dry_run,
                again,
                ..
            } => {
                assert!(items.is_empty());
                assert!(!dry_run);
                assert!(!again);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A wrong split is N issues to close, a checklist to strip out of
    /// somebody's body, and branches and pull requests to delete, so the read
    /// only half is not optional.
    #[test]
    fn split_can_be_told_to_write_nothing() {
        match Cli::parse_from(["spar", "split", "10", "--dry-run"]).command {
            Command::Split { dry_run, .. } => assert!(dry_run),
            other => panic!("{other:?}"),
        }
    }

    /// The flag `checkin` established for exactly this: do it again on
    /// something spar already dealt with.
    #[test]
    fn split_can_be_told_to_split_something_it_already_split() {
        match Cli::parse_from(["spar", "split", "10", "--again"]).command {
            Command::Split { again, .. } => assert!(again),
            other => panic!("{other:?}"),
        }
    }

    /// It picks for itself when given nothing, so it takes both of the flags
    /// that govern picking, and its config like everything else.
    #[test]
    fn split_takes_the_flags_that_govern_picking() {
        match Cli::parse_from([
            "spar",
            "split",
            "--limit",
            "5",
            "--min-number",
            "480",
            "--config",
            "other.toml",
        ])
        .command
        {
            Command::Split { common, .. } => {
                assert_eq!(5, common.limit);
                assert_eq!(Some(480), common.min_number);
                assert_eq!(Some(PathBuf::from("other.toml")), common.config);
            }
            other => panic!("{other:?}"),
        }
    }

    /// `split` decomposes and stops. A flag that implied it works, reviews, or
    /// merges anything would be a flag that does nothing.
    #[test]
    fn split_offers_no_flag_that_would_make_it_a_second_run() {
        for flag in [
            vec!["--auto-merge"],
            vec!["--max-rounds", "2"],
            vec!["--close-skipped"],
            vec!["--no-close-skipped"],
            vec!["--keep-worktrees"],
        ] {
            let mut argv = vec!["spar", "split", "10"];
            argv.extend(flag.iter().copied());
            assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?}");
        }
    }
}

#[cfg(test)]
mod min_number_tests {
    use super::*;

    fn read(argv: &[&str]) -> Option<i64> {
        match Cli::parse_from(argv).command {
            Command::Run { common, .. }
            | Command::Triage { common, .. }
            | Command::Resume { common, .. }
            | Command::Review { common, .. }
            | Command::Followup { common, .. }
            | Command::Split { common, .. }
            | Command::Checkin { common, .. } => common.min_number,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn there_is_no_floor_unless_one_is_asked_for() {
        assert_eq!(None, read(&["spar", "run"]));
    }

    #[test]
    fn every_command_that_picks_for_itself_accepts_a_floor() {
        for cmd in ["run", "triage", "resume", "review", "checkin", "split"] {
            assert_eq!(
                Some(480),
                read(&["spar", cmd, "--min-number", "480"]),
                "{cmd}"
            );
        }
    }
}

#[cfg(test)]
mod settings_block_tests {
    use super::*;

    /// The value a config line offers, with its trailing note removed. Quote
    /// aware, since a note is free to contain a `#` and several do.
    fn written(line: &str) -> String {
        let after = line.split_once('=').expect("an assignment").1;
        let mut quoted = false;
        for (i, c) in after.char_indices() {
            match c {
                '"' => quoted = !quoted,
                '#' if !quoted => return after[..i].trim().to_string(),
                _ => {}
            }
        }
        after.trim().to_string()
    }

    fn line_for(text: &str, key: &str) -> String {
        text.lines()
            .find(|l| {
                let bare = l.trim_start().trim_start_matches('#').trim_start();
                bare.starts_with(&format!("{key} ")) || bare.starts_with(&format!("{key}="))
            })
            .unwrap_or_else(|| panic!("{key} is not offered at all:\n{text}"))
            .to_string()
    }

    /// The guard that was missing. Every number in the generated config used to
    /// be typed in beside its comment, which is a second copy of a default that
    /// lives in the code, and the copies stopped agreeing: it offered a title
    /// budget of 90 against a real 140, a body of 900 against 8000, and three
    /// more like it. Uncommenting one to see what it did cut every comment spar
    /// posts to a fifth of its length.
    #[test]
    fn every_value_it_offers_is_the_default_it_actually_has() {
        let text = settings_block("claude");
        for option in config::known_options() {
            // The effort words are per CLI, so the schedule's are examples of
            // what one accepts rather than defaults. There is no default
            // effort: an agent that names none uses its own CLI's.
            if option.section == "loop.effort_schedule" {
                continue;
            }
            let line = line_for(&text, &option.key);
            assert_eq!(
                option.default,
                written(&line),
                "the generated config offers `{}`, but the default is {}",
                line.trim(),
                option.default
            );
        }
    }

    /// `doctor` reports what a config does not mention, so a generated one
    /// should send nobody to that list on the day it was written. pr_comments
    /// was missing from it for exactly that long.
    #[test]
    fn it_offers_every_option_the_parser_knows_about() {
        let text = settings_block("claude");
        let missing: Vec<String> = config::unmentioned_options(&text)
            .into_iter()
            .map(|o| format!("[{}] {}", o.section, o.key))
            .collect();
        assert!(missing.is_empty(), "not offered: {}", missing.join(", "));
    }

    /// The strongest of these: every line the file suggests has to be a line
    /// that works. A commented option is an invitation to uncomment it, and one
    /// that then fails to load is worse than never having offered it.
    #[test]
    fn every_option_it_offers_can_be_uncommented_and_still_load() {
        let mut text = String::from(
            "[agents.claude]\ncommand = [\"claude\"]\n\n\
             [agents.codex]\ncommand = [\"codex\"]\n\n",
        );
        for line in settings_block("claude").lines() {
            text.push_str(uncomment(line).unwrap_or(line));
            text.push('\n');
        }
        let cfg = config::parse(&text).expect("a config of its own suggestions");
        assert_eq!("claude", cfg.first_implementor);
    }

    /// A commented assignment with its `#` removed, or None for a line of
    /// prose, which stays a comment.
    fn uncomment(line: &str) -> Option<&str> {
        let bare = line.trim_start().strip_prefix('#')?.trim_start();
        // An assignment, not a wrapped note that happens to contain an `=`:
        // the key has to be one bare word.
        let key = bare.split_once('=')?.0.trim();
        let named = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        named.then_some(bare)
    }

    #[test]
    fn the_agent_that_goes_first_is_the_one_that_was_chosen() {
        assert!(settings_block("codex").contains("first_implementor = \"codex\""));
    }

    /// Every line starts at the margin, and an option's note sits above the
    /// option rather than beside it. Beside meant three different columns in
    /// one file, and a note that wrapped left the reader tracking indentation
    /// to work out which setting it belonged to.
    #[test]
    fn a_note_sits_above_the_option_it_describes() {
        let text = settings_block("claude");
        assert!(
            text.lines().all(|l| l.chars().count() <= 80),
            "a line runs off the edge:\n{text}"
        );
        assert!(
            text.lines().all(|l| !l.starts_with(' ')),
            "a line is indented, so the columns are back:\n{text}"
        );

        // The line before an option carries its note, not another option.
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("max_rounds"))
            .expect("max_rounds");
        assert!(lines[at - 1].starts_with('#'), "{:?}", lines[at - 1]);
        assert!(
            lines[at - 1].contains("lifetime cap"),
            "the note above it is the end of its own note: {:?}",
            lines[at - 1]
        );
    }

    /// A note reads as a sentence now that it leads rather than trails.
    #[test]
    fn every_note_starts_as_a_sentence() {
        for (_, key, note) in LOOP_OPTIONS.iter().chain(STYLE_OPTIONS) {
            let first = note.chars().next().expect("a note");
            assert!(
                first.is_uppercase(),
                "{key} reads as a margin scribble rather than a sentence: {note}"
            );
        }
    }
}

#[cfg(test)]
mod agent_block_tests {
    use super::*;

    fn spec(models: &[&str], efforts: &[&str]) -> config::AgentSpec {
        let mut spec: config::AgentSpec =
            toml::Value::Table(toml::from_str("command = [\"x\"]").expect("a minimal preset"))
                .try_into()
                .expect("builds");
        spec.models = models.iter().map(|s| s.to_string()).collect();
        spec.efforts = efforts.iter().map(|s| s.to_string()).collect();
        spec
    }

    /// A placeholder is not a working value. `# effort = "..."` was written for
    /// every preset that lists no efforts, and taking the file at its word by
    /// uncommenting it passes `...` to the CLI as a real setting.
    #[test]
    fn an_option_with_no_hints_is_left_out_rather_than_guessed_at() {
        let block = agent_block("cursor", &spec(&["composer-2.5", "auto"], &[]));
        assert!(!block.contains("..."), "{block}");
        assert!(!block.contains("effort"), "{block}");
        assert!(block.contains("# model = \"composer-2.5\""), "{block}");
    }

    /// Each option is introduced by its own line, so a block with no effort
    /// setting never mentions one.
    #[test]
    fn only_the_options_that_follow_are_introduced() {
        let model_only = agent_block("cursor", &spec(&["auto"], &[]));
        assert!(model_only.contains("Omit model to use"), "{model_only}");
        assert!(!model_only.contains("Omit effort"), "{model_only}");

        let both = agent_block("claude", &spec(&["fable"], &["high"]));
        assert!(both.contains("Omit model to use"), "{both}");
        assert!(both.contains("Omit effort to use"), "{both}");
    }

    /// A preset with no hints at all still has to produce a loadable block,
    /// which is every preset that has never listed any: gemini and aider.
    #[test]
    fn a_preset_with_no_hints_still_writes_a_usable_block() {
        let block = agent_block("gemini", &spec(&[], &[]));
        assert!(!block.contains("..."), "{block}");
        assert!(!block.contains("Omit"), "{block}");
        assert!(
            block.starts_with("[agents.gemini]\npreset = \"gemini\"\n"),
            "{block}"
        );
        // What the block does still carry is unaffected.
        assert!(block.contains("[agents.gemini.fallback]"), "{block}");
        assert!(block.contains("# timeout = "), "{block}");
    }

    /// The timeout comes from the spec rather than a number typed into the
    /// generator, for the reason the [loop] block learned the hard way.
    #[test]
    fn the_timeout_offered_is_the_one_the_agent_would_use() {
        let mut spec = spec(&["a"], &[]);
        spec.timeout = 7200;
        assert!(
            agent_block("custom", &spec).contains("# timeout = 7200"),
            "the generator kept its own copy"
        );
    }

    /// Alternatives are named, and a single choice is not dressed up as one.
    #[test]
    fn alternatives_are_listed_only_when_there_are_any() {
        assert!(agent_block("a", &spec(&["one", "two"], &[])).contains("One of: one | two."));
        let single = agent_block("b", &spec(&["only"], &[]));
        assert!(!single.contains("One of:"), "{single}");
    }

    /// The preset's own note is about the pair, so it appears once above them
    /// rather than under each, which put a sentence about models beneath the
    /// effort line.
    #[test]
    fn the_presets_note_is_said_once() {
        let mut spec = spec(&["m1", "m2"], &["e1", "e2"]);
        spec.options_note = Some("Check the current sets with: mytool --help".into());
        let block = agent_block("mytool", &spec);
        assert_eq!(
            1,
            block.matches("Check the current sets").count(),
            "{block}"
        );
    }
}
