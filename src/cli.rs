//! The command line.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::agent::{self, Agent};
use crate::config::{self, Config};
use crate::error::Result;
use crate::model::{Issue, IssueRun, ItemKind, Ledger, Plan, Status};
use crate::proc::{self, ExecOpts};
use crate::repo::Repo;
use crate::review;
use crate::review_only;
use crate::style;
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
                  for `resume`. Omit them and spar takes everything open, up to --limit.",
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
        /// Which agent reviews next, overriding the PR's saved state.
        #[arg(long = "next", value_name = "AGENT")]
        next_actor: Option<String>,
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
        /// Remove every worktree and branch spar created, even for open PRs.
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
    /// Which agent implements first. A key from the [agents] table.
    #[arg(long)]
    pub first: Option<String>,
    /// Cap on how many open items to take when none are named.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Ignore issues and pull requests numbered below this when picking for
    /// itself. A number you name explicitly is always honoured.
    #[arg(long, value_name = "N")]
    pub min_number: Option<i64>,
}

#[derive(Args, Debug, Clone)]
pub struct LoopFlags {
    /// Review rounds this run may spend before escalating. Resuming grants a
    /// fresh budget; it is not a lifetime cap on the pull request.
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
                match repo.open_pr_for_issue(number) {
                    Some(pr) => {
                        log!("#{number} is an issue; reviewing its open PR {}", pr.url);
                        targets.push(pr.number);
                    }
                    None => logwarn!("#{number} is an issue with no open pull request to review"),
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
            Ok(report(&results, &cfg))
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
            make_plan(&agents, &cfg, &repo, &issues, &plan_out)?;
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
            let mut overrides = Overrides::from(&loop_flags);
            overrides.worktrees = if no_worktrees { Some(false) } else { None };
            overrides.close_skipped =
                match (triage_flags.close_skipped, triage_flags.no_close_skipped) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                };
            let (cfg, repo, agents) = prepare(&common, Some(overrides))?;
            let numbers = pick_issues(&repo, issues, common.limit, cfg.loop_cfg.min_number)?;
            if numbers.is_empty() {
                return Ok(0);
            }
            let sorted = classify(&repo, &numbers)?;
            let mut results = Vec::new();
            let mut ledger = Ledger::new();
            let mut handled: BTreeSet<i64> = BTreeSet::new();
            let mut wave = sorted.issues.clone();

            // Wave 0 is what was asked for. Each further wave is the follow-ups
            // the previous one filed, folded back in rather than left for the
            // next run. Every wave is triaged like anything else, so both
            // agents still have to agree each one is worth doing.
            for round in 0..=cfg.loop_cfg.absorb_new_issues {
                wave.retain(|n| !handled.contains(n));
                if wave.is_empty() {
                    break;
                }
                if round > 0 {
                    log!(
                        "absorbing {} newly filed issue(s): {}",
                        wave.len(),
                        wave.iter()
                            .map(|n| format!("#{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                handled.extend(wave.iter().copied());

                let fetched = match repo.fetch_issues(&wave) {
                    Ok(fetched) => fetched,
                    Err(e) => {
                        logdim!("could not read the next wave: {e}");
                        break;
                    }
                };
                let plan_path = if round == 0 {
                    plan_out.clone()
                } else {
                    plan_out.with_extension(format!("wave{round}.json"))
                };
                let plan = make_plan(&agents, &cfg, &repo, &fetched, &plan_path)?;
                act_on_plan(&cfg, &repo, &plan);

                let before = results.len();
                for item in &plan.order {
                    let Some(issue) = fetched.iter().find(|i| i.number == item.issue) else {
                        continue;
                    };
                    results.push(review::run_issue(
                        &agents,
                        &cfg,
                        &repo,
                        item,
                        issue,
                        &mut ledger,
                    ));
                }

                // Whatever this wave filed becomes the next one.
                wave = results[before..]
                    .iter()
                    .flat_map(|r| r.filed.iter())
                    .filter_map(|url| review::filed_issue_number(url))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
            }
            if !wave.is_empty() && cfg.loop_cfg.absorb_new_issues > 0 {
                log!(
                    "{} issue(s) filed in the last wave were left for a later run: {}",
                    wave.len(),
                    wave.iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            for number in sorted.prs {
                results.push(review::resume_pr(&agents, &cfg, &repo, number, None));
            }

            if results.is_empty() {
                log!("nothing scheduled");
                return Ok(0);
            }
            Ok(report(&results, &cfg))
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
                match repo.open_pr_for_issue(number) {
                    Some(pr) => {
                        log!("#{number} is an issue; continuing its open PR {}", pr.url);
                        results.push(review::resume_pr(
                            &agents,
                            &cfg,
                            &repo,
                            pr.number,
                            next_actor.as_deref(),
                        ));
                    }
                    None => logwarn!(
                        "#{number} is an issue with no open pull request. Use `spar run {number}` \
                         to implement it."
                    ),
                }
            }
            if results.is_empty() {
                return Ok(0);
            }
            Ok(report(&results, &cfg))
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
        "agents: {}",
        agents
            .iter()
            .map(|a| format!("{}={}", a.name(), a.spec.describe()))
            .collect::<Vec<_>>()
            .join(", ")
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
        log!(
            "  do   #{} [{}/{}] {}",
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
        println!("nothing to clean");
    } else {
        for item in removed {
            println!("removed {item}");
        }
    }
    Ok(0)
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
    Ok(if failed { 1 } else { 0 })
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
            block.push_str(&format!("# [{section}]\n"));
        }
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
    (
        false,
        "max_rounds",
        "review rounds ONE invocation may spend. Resuming grants a fresh budget, so this is not a lifetime cap on a PR.",
    ),
    (
        false,
        "auto_merge",
        "off on purpose: two models agreeing is not the same as being right",
    ),
    (false, "first_implementor", ""),
    (false, "worktrees", "false works in the main checkout"),
    (
        false,
        "close_skipped",
        "close an issue both reviewers declined",
    ),
    (
        false,
        "followups",
        "issues | local | none. local writes .spar/followups.md, not the tracker",
    ),
    (
        true,
        "file_non_blocking",
        "a suggestion is not a tracker item",
    ),
    (
        true,
        "max_followups",
        "backstop on what one run can spawn",
    ),
    (
        true,
        "keep_worktrees",
        "true leaves them behind to inspect",
    ),
    (
        true,
        "min_number",
        "ignore anything numbered below this when picking for itself. 0 is no floor.",
    ),
    (
        true,
        "parallel_triage",
        "false asks the agents one at a time",
    ),
    (
        true,
        "absorb_new_issues",
        "waves of newly filed follow-ups to fold back into this run. Costs more.",
    ),
    (true, "file_nits", "true files nits as issues too"),
    (
        true,
        "base_branch",
        "only a fallback; origin/HEAD wins when it resolves",
    ),
    (
        true,
        "branch_prefix",
        "e.g. \"spar/\" to namespace the branches spar creates",
    ),
    (true, "state_store", "local | pr | both"),
    (
        true,
        "max_issue_chars",
        "most of one issue body a prompt carries. Sized so nothing a person wrote is cut, and a cut is said out loud when it happens.",
    ),
    (
        true,
        "max_triage_chars",
        "most every issue body together may add to one triage prompt. Past it, whole issues wait for the next run rather than all of them losing their tails.",
    ),
];

const STYLE_OPTIONS: &[Setting] = &[
    (false, "ban_em_dash", ""),
    (false, "ban_ai_attribution", ""),
    (
        false,
        "terse",
        "hold model prose to a length budget. false removes the valves entirely",
    ),
    (
        true,
        "pr_comments",
        "outcome | rounds | none. How much of its own working spar narrates into a PR thread. none never comments at all.",
    ),
    (
        true,
        "max_title_chars",
        "a finding, issue, or PR title. Never ellipsised",
    ),
    (
        true,
        "max_summary_chars",
        "a one line verdict or refutation",
    ),
    (
        true,
        "max_detail_chars",
        "a blocking finding, in the PR thread",
    ),
    (true, "max_body_chars", "a PR body"),
    (
        true,
        "max_issue_body_chars",
        "a filed issue's body. Far larger on purpose: an issue is picked up cold. Fenced code blocks in one are never truncated and never count against this.",
    ),
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
        "# Values are whatever each agent's own CLI accepts, listed above, so\n",
        "# these are examples rather than defaults. Left out, each agent uses\n",
        "# the effort its own block asked for.\n",
        "# round_1 = \"high\"   # the deep first review\n",
        "# rest    = \"low\"    # later rounds only see a small delta\n\n",
    ));
    out.push_str("[style]\n");
    out.push_str(&option_lines(STYLE_OPTIONS, &value));
    out
}

/// Option lines with their notes lined up in a column, a long note wrapping
/// onto continuation lines that stay in the column rather than running off the
/// edge or restarting at the margin.
fn option_lines(options: &[Setting], value: &dyn Fn(&str) -> String) -> String {
    let rows: Vec<(String, String)> = options
        .iter()
        .map(|(commented, key, note)| {
            let lead = if *commented { "# " } else { "" };
            (format!("{lead}{key} = {}", value(key)), note.to_string())
        })
        .collect();
    aligned(&rows)
}

/// Assignments with their notes lined up in one column, a long note wrapping
/// onto continuation lines that stay in the column rather than running off the
/// edge or restarting at the margin.
///
/// Shared by the `[loop]` and `[style]` blocks and by an agent's own, which is
/// how a list of eight effort levels beside a long model name stays inside a
/// line somebody can read.
fn aligned(rows: &[(String, String)]) -> String {
    const WIDTH: usize = 78;

    let column = rows
        .iter()
        .map(|(assignment, _)| assignment.chars().count())
        .max()
        .unwrap_or(0)
        + 2;

    let mut out = String::new();
    for (assignment, note) in rows {
        if note.is_empty() {
            out.push_str(assignment);
            out.push('\n');
            continue;
        }
        let mut first = true;
        let mut line = String::new();
        for word in note.split_whitespace() {
            let would_be = column + 2 + line.chars().count() + 1 + word.chars().count();
            if !line.is_empty() && would_be > WIDTH {
                out.push_str(&noted(assignment, &line, column, &mut first));
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push_str(&noted(assignment, &line, column, &mut first));
        }
    }
    out
}

/// One rendered line: the assignment and the start of its note, then the rest
/// of the note alone, indented to the same column so it reads as one paragraph.
fn noted(assignment: &str, note: &str, column: usize, first: &mut bool) -> String {
    let lead = if *first {
        let pad = column.saturating_sub(assignment.chars().count());
        format!("{assignment}{}", " ".repeat(pad))
    } else {
        " ".repeat(column)
    };
    *first = false;
    format!("{lead}# {note}\n")
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
    let offered: Vec<(&str, &[String])> = [
        ("model ", spec.models.as_slice()),
        ("effort", spec.efforts.as_slice()),
    ]
    .into_iter()
    .filter(|(_, choices)| !choices.is_empty())
    .collect();

    if !offered.is_empty() {
        let named: Vec<&str> = offered.iter().map(|(key, _)| key.trim()).collect();
        out.push_str(&format!(
            "# Omit {} to use the CLI's own default.\n",
            named.join(" or ")
        ));

        // The alternatives sit beside the suggestion, so somebody editing the
        // file can see what else the CLI takes without going to look it up.
        // Nothing beside a single choice: there is nothing to choose.
        let rows: Vec<(String, String)> = offered
            .iter()
            .map(|(key, choices)| {
                let note = if choices.len() > 1 {
                    choices.join(" | ")
                } else {
                    String::new()
                };
                (format!("# {key} = \"{}\"", choices[0]), note)
            })
            .collect();
        out.push_str(&aligned(&rows));
    }

    if let Some(note) = &spec.options_note {
        out.push_str(&wrap_comment(note));
    }
    // Anything but this agent's own preset: a CLI that has just refused is not
    // a stand in for itself.
    let backup = if name == "cursor" { "gemini" } else { "cursor" };
    out.push_str("# A stand in for when this CLI refuses, stalls, or runs out of quota.\n");
    out.push_str("# It answers in place of this agent, never alongside it.\n");
    out.push_str(&format!(
        "# [agents.{name}.fallback]\n# preset = \"{backup}\"\n"
    ));
    out.push('\n');
    out
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

    println!(
        "\n  settings: max_rounds={} auto_merge={} worktrees={} followups={} terse={}",
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

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn report(results: &[IssueRun], cfg: &Config) -> i32 {
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
            println!("       disputed: {}", dispute.title);
        }
    }
    println!("{}", "=".repeat(60));

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
    if results.iter().all(IssueRun::succeeded) {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
            vec!["spar", "clean"],
            vec!["spar", "doctor"],
        ] {
            let mut full = argv.clone();
            full.extend(["--config", "other.toml"]);
            let cli = Cli::parse_from(&full);
            let config = match cli.command {
                Command::Run { common, .. }
                | Command::Triage { common, .. }
                | Command::Resume { common, .. } => common.config,
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
        assert!(Cli::try_parse_from(["spar", "resume", "--close-skipped"]).is_err());
        assert!(Cli::try_parse_from(["spar", "review", "--close-skipped"]).is_err());
        assert!(Cli::try_parse_from(["spar", "triage", "--close-skipped"]).is_err());
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
            | Command::Review { common, .. } => common.min_number,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn there_is_no_floor_unless_one_is_asked_for() {
        assert_eq!(None, read(&["spar", "run"]));
    }

    #[test]
    fn every_command_that_picks_for_itself_accepts_a_floor() {
        for cmd in ["run", "triage", "resume", "review"] {
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

    /// A note long enough to wrap stays in its column rather than restarting at
    /// the margin, where it would read as a new option.
    #[test]
    fn a_wrapped_note_stays_in_its_column() {
        let text = settings_block("claude");
        let column = text
            .lines()
            .find(|l| l.starts_with("max_rounds"))
            .and_then(|l| l.find('#'))
            .expect("a note on max_rounds");
        let continuation = text
            .lines()
            .find(|l| l.starts_with("    ") && l.trim_start().starts_with('#'))
            .expect("a wrapped note");
        assert_eq!(Some(column), continuation.find('#'));
        assert!(text.lines().all(|l| l.chars().count() <= 80), "{text}");
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
        assert!(block.contains("# model  = \"composer-2.5\""), "{block}");
    }

    /// The header has to name what is actually below it. Saying "model or
    /// effort" over a block with no effort line sends somebody looking for a
    /// setting the CLI does not have.
    #[test]
    fn the_header_names_only_the_options_that_follow() {
        assert!(agent_block("cursor", &spec(&["auto"], &[])).contains("Omit model to use"));
        assert!(
            agent_block("claude", &spec(&["fable"], &["high"])).contains("Omit model or effort")
        );
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
    }

    /// Alternatives are listed beside the suggestion, and a single choice is
    /// not padded out with a column that has nothing in it.
    /// A long list wraps into its column rather than running off the edge, so
    /// codex's eight effort levels beside a long model name stay readable.
    #[test]
    fn a_long_list_of_choices_wraps_into_its_column() {
        let block = agent_block(
            "codex",
            &spec(
                &[
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                    "gpt-5.6-pro",
                ],
                &[
                    "ultra", "max", "xhigh", "high", "medium", "low", "minimal", "none",
                ],
            ),
        );
        assert!(
            block.lines().all(|l| l.chars().count() <= 80),
            "a line runs off the edge:\n{block}"
        );
        // Every choice survives the wrapping.
        for choice in ["gpt-5.6-pro", "minimal", "none"] {
            assert!(block.contains(choice), "{choice} was lost:\n{block}");
        }
    }

    #[test]
    fn alternatives_are_listed_only_when_there_are_any() {
        assert!(agent_block("a", &spec(&["one", "two"], &[])).contains("# one | two"));
        let single = agent_block("b", &spec(&["only"], &[]));
        assert!(!single.contains('|'), "{single}");
    }
}
