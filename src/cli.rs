//! The command line.

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

    /// Detect installed agent CLIs and write a spar.toml.
    Init {
        #[arg(long, default_value = "spar.toml")]
        out: PathBuf,
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
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
                let found = repo.list_open_prs(common.limit)?;
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

        Command::Init { out, force } => cmd_init(&out, force),
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
            let numbers = pick_issues(&repo, issues, common.limit)?;
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
            let numbers = pick_issues(&repo, issues, common.limit)?;
            if numbers.is_empty() {
                return Ok(0);
            }
            let sorted = classify(&repo, &numbers)?;
            let mut results = Vec::new();

            if !sorted.issues.is_empty() {
                let fetched = repo.fetch_issues(&sorted.issues)?;
                let plan = make_plan(&agents, &cfg, &repo, &fetched, &plan_out)?;
                act_on_plan(&cfg, &repo, &plan);
                let mut ledger = Ledger::new();
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
                let found = repo.list_open_prs(common.limit)?;
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
}

impl From<&LoopFlags> for Overrides {
    fn from(flags: &LoopFlags) -> Self {
        Self {
            max_rounds: flags.max_rounds,
            auto_merge: flags.auto_merge.then_some(true),
            keep_worktrees: flags.keep_worktrees.then_some(true),
            worktrees: None,
            close_skipped: None,
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

fn pick_issues(repo: &Repo, given: Vec<i64>, limit: usize) -> Result<Vec<i64>> {
    if !given.is_empty() {
        return Ok(given);
    }
    let found = repo.list_open_issues(limit)?;
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
        log!("  skip #{} (both reviewers: not worth doing)", item.issue);
    }
    for item in &plan.contested {
        log!("  ??   #{} contested, parked for you to decide", item.issue);
    }
    Ok(plan)
}

/// Post the shared reasoning on every issue both agents declined, and close it
/// when the config says so. Contested issues are never touched.
fn act_on_plan(cfg: &Config, repo: &Repo, plan: &Plan) {
    for item in &plan.skipped {
        let body = review::skip_comment(item, &repo.style);
        let outcome = if cfg.loop_cfg.close_skipped {
            repo.close_issue(item.issue, &body)
        } else {
            repo.comment_issue(item.issue, &body)
        };
        match outcome {
            Ok(()) if cfg.loop_cfg.close_skipped => log!("  closed #{}", item.issue),
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

fn cmd_init(out: &Path, force: bool) -> Result<i32> {
    if out.exists() && !force {
        logging::error(format!(
            "{} already exists, pass --force to overwrite",
            out.display()
        ));
        return Ok(1);
    }

    let presets = config::available_presets();
    if presets.is_empty() {
        bail!("no presets available, which should be impossible in a released build");
    }

    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for name in &presets {
        let raw = config::load_preset(name)?;
        let table: toml::Table = match raw.as_table() {
            Some(t) => t.clone(),
            None => continue,
        };
        let mut spec: crate::config::AgentSpec = match toml::Value::Table(table).try_into() {
            Ok(spec) => spec,
            Err(_) => continue,
        };
        spec.name = name.clone();
        match Agent::new(spec).resolve_bin() {
            Ok(path) => {
                println!("  found    {name:10} {}", path.display());
                found.push((name.clone(), path.to_path_buf()));
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
    let chosen: Vec<&(String, PathBuf)> = found.iter().take(2).collect();
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
         # built in preset; anything set here overrides it.\n\n",
    );
    for (name, _) in &chosen {
        text.push_str(&format!(
            "[agents.{name}]\npreset = \"{name}\"\n# model  = \"...\"   # omit to use the CLI's \
             own default\n# effort = \"...\"\n\n"
        ));
    }
    text.push_str(&format!(
        "[loop]\n\
         max_rounds        = 3\n\
         auto_merge        = false\n\
         first_implementor = \"{}\"\n\
         worktrees         = true\n\
         close_skipped     = true   # close an issue both reviewers declined\n\
         followups         = \"issues\"  # issues | local | none\n\n\
         [loop.effort_schedule]\n\
         # round_1 = \"high\"   # the deep first review\n\
         # rest    = \"low\"    # later rounds only see a small delta\n\n\
         [style]\n\
         ban_em_dash        = true\n\
         ban_ai_attribution = true\n\
         terse              = true   # hold model prose to a length budget\n",
        chosen[0].0
    ));

    std::fs::write(out, text).map_err(|e| spar_err!("could not write {}: {e}", out.display()))?;
    println!("\nwrote {}", out.display());
    println!("Next: `spar doctor` to check it, then `spar run` in a repo you have push access to.");
    Ok(0)
}

/// One prerequisite check: a label and something that either reports a version
/// or explains what is missing.
type Probe = Box<dyn Fn() -> Result<String>>;

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
