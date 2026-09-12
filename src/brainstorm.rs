//! Brainstorming with the pair: existing pieces, paired in ways nobody has.
//!
//! The premise is that the parts of an answer usually exist already, in the
//! problem's own field or an adjacent one, and what is missing is the pairing.
//! So the agents are not asked to invent. They are asked to take inventory of
//! what exists and then to combine, and every idea has to name its parts and
//! the nearest thing that already does something like it.
//!
//! Nothing is edited between rounds, so the custody loop in [`crate::review`]
//! does not apply. The shape is the one `spar review` uses:
//!
//! 1. **Diverge.** Both agents propose at the same time, neither seeing the
//!    other. An idea both reach on their own is the strongest signal there is.
//! 2. **Cross.** Each reads only the other's ideas and, for each, builds on it
//!    with a further piece, challenges it by naming where it already exists or
//!    why it cannot work, or keeps it. These are the middle rounds.
//! 3. **Converge.** Whoever proposed a challenged idea defends it with
//!    specifics or withdraws it, and both rank what is left.
//!
//! The settle is mechanical. Nothing here lets one agent overrule the other:
//! an idea leaves the set only when its own proposer withdraws it or never
//! answers the objection, and the order is the sum of two rankings.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::config::{Call, Config};
use crate::error::{ErrorKind, Result};
use crate::model::{ConvergeDoc, CrossDoc, CrossKind, Idea, IdeasDoc};
use crate::repo::{Repo, FOLLOWUP_MARKER};
use crate::review_only::concurrently;
use crate::style::{self, Style};
use crate::{bail, log, logdim, logwarn, schema};

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

/// The one sentence every prompt here carries. A brainstorm reads, and an
/// ambient write in the working tree discards the answer.
const READ_ONLY: &str = "\
Do not modify, commit, or push anything in your working directory. If you need
scratch space, use the system temporary directory.";

const DIVERGE_PROMPT: &str = "\
{opening}

The premise of this session is that the pieces of an answer usually exist
already, in this field or an adjacent one, and what is missing is the pairing.
So do not try to invent from nothing. Work in two steps.

First, take inventory. List, for yourself, the existing techniques, primitives,
results, tools, and patterns that bear on this, from its own field and from
fields that solve a similar shaped problem differently. Include things that
seem unrelated at first: the useful pairings are usually the ones nobody has
had a reason to try.

Then propose {count} ideas. Each pairs two or more pieces from your inventory
in a way that has not been done. For every idea:
- title: one line naming it by what it does, not by its ingredients.
- combines: name the pieces. An idea that cannot name its parts is a wish.
- how_it_works: the mechanism, concretely enough that somebody could start.
- why_new: the nearest thing that already exists, and what this does that it
  does not. If you cannot name a nearest thing, look harder before claiming
  there is none.
- enables: what becomes possible that was not.
- risks: what would make it fail, honestly.
- first_experiment: the cheapest thing that would show whether it works.

Prefer an idea that would be surprising and real over one that is safe and
obvious. The other agent proposes on the same subject without seeing your
answer, then reads yours and challenges anything that already exists or cannot
work, so an idea that survives is one you could defend with specifics.

{read_only}";

const TOPIC_OPENING: &str = "\
Brainstorm: {subject}

Your working directory is a code repository. It is context, not the subject,
unless the subject is about it.";

const REPO_OPENING: &str = "\
Brainstorm ideas for the repository in your working directory.

Read it first: the README, the code, and the recent history, until you can say
what it is for, who uses it, and what it does not do yet. The ideas are for
this project: things it could do, or do differently, that would matter to the
people who use it.{open_issues}";

const CROSS_PROMPT: &str = "\
Brainstorm, continued: {subject}

Both agents proposed ideas independently. The full set is numbered below. The
ones marked \"rule on this\" were proposed by the other agent and you have not
seen them before. Rule on each of those, and only those. An idea marked \"both
of you proposed this\" was reached independently by each of you, which is the
strongest signal this session produces: build on it if you can, do not
challenge it. Your own are listed so you can see the whole set.

For each idea you rule on, one verdict:
- build: you can see a further piece that completes it or makes it stronger,
  or a way to combine it with another idea in the set. Write the revised idea
  out in full in build, naming everything it combines including the new piece.
  It stands beside the original rather than replacing it.
- challenge: it already exists, and you can name where, or it cannot work, and
  you can say why. Put the thing or the reason in reason. The proposer answers
  you with specifics, so an objection you cannot substantiate costs a round for
  nothing.
- keep: real and new as far as you can tell, and you have nothing to add.

Do not challenge to seem rigorous or build to seem agreeable. The set is only
as good as the honesty of this round.

{read_only}

Ideas:
{ideas}";

const CONVERGE_PROMPT: &str = "\
Brainstorm, last round: {subject}

The full set is below, with what was said about each idea.

Two things.

First, {defend} Set stands=true only if you can give the specific evidence
that settles the objection in reply: the prior art that does not actually do
this, the mechanism the objection missed, the number that makes it work. Set
stands=false to withdraw the idea, which is the right answer when the
objection is correct. Withdrawing costs nothing. Defending a point you cannot
substantiate puts it in front of a person with your name on it.

Second, rank every idea that is not withdrawn, best first, in ranking. Weigh
novelty and feasibility together: an idea nobody has done that could be tried
this month beats a moonshot, and a moonshot beats a small improvement on
something that exists. Include every number, your own and the ones you
challenged. The other agent ranks the same set and the two rankings are
combined mechanically, so rank by what you actually think.

{read_only}

Ideas:
{ideas}";

// ---------------------------------------------------------------------------
// What a session is
// ---------------------------------------------------------------------------

/// What one `spar brainstorm` was asked for.
#[derive(Debug, Clone)]
pub struct Session {
    /// The subject, or `None` to brainstorm the repository itself.
    pub topic: Option<String>,
    /// Rounds, from `[loop] max_rounds`.
    pub budget: u32,
    /// How many ideas survive the settle.
    pub wanted: usize,
}

impl Session {
    /// The subject as the prompts and the file name it.
    pub fn subject(&self) -> String {
        match &self.topic {
            Some(topic) => topic.clone(),
            None => "this repository".to_string(),
        }
    }
}

/// Where an idea stands after the rounds that have run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Proposed by one agent and not yet ruled on by the other.
    Unverified,
    /// Both agents reached it independently. Never challenged.
    Corroborated,
    /// The other agent objected and the proposer has not answered.
    Challenged,
    /// Objected to and defended with specifics. A person decides.
    Defended,
    /// Objected to and withdrawn by the agent that proposed it.
    Withdrawn,
}

/// One idea, with everything the rounds said about it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The number the agents see. Stable for the whole session, so a ruling
    /// from any round matches back.
    pub id: u32,
    pub idea: Idea,
    /// Who proposed it. Two names when both reached it independently.
    pub proposers: Vec<String>,
    /// The idea this was built on, when a cross round produced it.
    pub parent: Option<u32>,
    /// Who objected, and what they said.
    pub objection: Option<(String, String)>,
    /// What the proposer said back, when it stood by the idea.
    pub defence: Option<String>,
    pub standing: Standing,
    /// The sum of its positions in the final rankings, lower first.
    pub rank_sum: u32,
}

impl Candidate {
    fn new(id: u32, idea: Idea, proposer: &str) -> Self {
        Self {
            id,
            idea,
            proposers: vec![proposer.to_string()],
            parent: None,
            objection: None,
            defence: None,
            standing: Standing::Unverified,
            rank_sum: 0,
        }
    }

    fn proposed_by(&self, name: &str) -> bool {
        self.proposers.iter().any(|p| p == name)
    }
}

/// What a session produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub subject: String,
    pub agents: Vec<String>,
    pub rounds: u32,
    /// Ideas kept, best first.
    pub kept: Vec<Candidate>,
    /// Ideas set aside, each with the reason.
    pub dropped: Vec<(Candidate, String)>,
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// The rounds
// ---------------------------------------------------------------------------

/// Run the rounds and settle the set.
pub fn run(agents: &[Agent], cfg: &Config, repo: &Repo, session: &Session) -> Result<Outcome> {
    let budget = session.budget.max(1);
    let work_dir = repo.root();
    let subject = session.subject();
    let names: Vec<String> = agents.iter().map(|a| a.name().to_string()).collect();
    let mut notes = Vec::new();

    // -- round 1: both propose, neither seeing the other ------------------
    log!("brainstorm: {} diverging on {subject}", names.join(" and "));
    let prompt = diverge_prompt(session, repo);
    let answers = concurrently(agents, |agent| {
        let effort = agent.effort(cfg, Call::Brainstorm(1));
        agent.ask_json::<IdeasDoc>(&prompt, &schema::brainstorm_ideas(), work_dir, &effort)
    });
    let mut by_agent: Vec<(String, Vec<Idea>)> = Vec::new();
    for (name, result) in answers {
        match result {
            Ok(doc) => {
                let ideas: Vec<Idea> = doc
                    .ideas
                    .into_iter()
                    .filter(|idea| !idea.title.trim().is_empty())
                    .collect();
                logdim!(
                    "{name} proposed {} idea{}",
                    ideas.len(),
                    plural(ideas.len())
                );
                by_agent.push((name, ideas));
            }
            Err(e) if e.kind() == ErrorKind::UncertainWrite => return Err(e),
            Err(e) => {
                logdim!("{name} could not brainstorm: {e}");
                notes.push(format!("{name} did not answer: {e}"));
            }
        }
    }
    if by_agent.is_empty() {
        bail!("neither agent returned any ideas");
    }
    let paired = by_agent.len() == 2;
    if !paired {
        // One voice is a materially weaker result, not a footnote: nothing
        // was cross-checked, so nothing below carries two models' judgement.
        crate::logging::warn(format!(
            "only {} answered. Nothing was cross-checked, so these ideas carry one model's \
             judgement rather than two.",
            by_agent[0].0
        ));
        notes.push("only one agent answered, so nothing was cross-checked".into());
    }
    let mut set = merge(&by_agent);
    if set.is_empty() {
        bail!("neither agent proposed anything with a title");
    }
    let shared = set
        .iter()
        .filter(|c| c.standing == Standing::Corroborated)
        .count();
    if shared > 0 {
        log!(
            "brainstorm: {} idea{} in the set, {shared} reached by both",
            set.len(),
            plural(set.len())
        );
    } else {
        log!(
            "brainstorm: {} idea{} in the set",
            set.len(),
            plural(set.len())
        );
    }

    // -- the middle rounds: each rules on what only the other proposed ----
    let mut rounds = 1;
    if paired {
        let mut round = 2;
        while round < budget {
            cross(agents, cfg, work_dir, &subject, &mut set, round)?;
            rounds = round;
            round += 1;
        }
        // -- the last round: defend or withdraw, then rank ----------------
        if budget >= 2 {
            converge(agents, cfg, work_dir, &subject, &mut set, budget)?;
            rounds = budget;
        }
    }

    let (kept, dropped) = settle(set, session.wanted);
    Ok(Outcome {
        subject,
        agents: names,
        rounds,
        kept,
        dropped,
        notes,
    })
}

fn diverge_prompt(session: &Session, repo: &Repo) -> String {
    let opening = match &session.topic {
        Some(topic) => TOPIC_OPENING.replace("{subject}", topic.trim()),
        None => REPO_OPENING.replace("{open_issues}", &open_issues_block(repo)),
    };
    DIVERGE_PROMPT
        .replace("{opening}", &opening)
        .replace("{count}", &session.wanted.to_string())
        .replace("{read_only}", READ_ONLY)
}

/// What is already filed, so repo mode does not propose it again. Best
/// effort: with no `gh`, no remote, or no access the list is empty, and the
/// reader is told so rather than left to wonder why a filed idea came back.
fn open_issues_block(repo: &Repo) -> String {
    let rows = repo.open_issue_rows();
    if rows.is_empty() {
        logdim!("no open issues to exclude, or none could be listed");
        return String::new();
    }
    let mut out =
        String::from("\n\nThese are already filed as open issues, so do not propose them again:\n");
    for row in rows {
        out.push_str(&format!(
            "- #{} {}\n",
            row.number,
            style::one_line(&row.title)
        ));
    }
    out.trim_end().to_string()
}

/// Both lists into one numbered set. An idea both agents reached is kept once,
/// in the longer wording, with both names on it.
pub fn merge(by_agent: &[(String, Vec<Idea>)]) -> Vec<Candidate> {
    let mut set: Vec<Candidate> = Vec::new();
    for (name, ideas) in by_agent {
        for idea in ideas {
            let same = set
                .iter_mut()
                .find(|c| !c.proposed_by(name) && same_idea(&c.idea, idea));
            match same {
                Some(existing) => {
                    if gist(idea).len() > gist(&existing.idea).len() {
                        existing.idea = idea.clone();
                    }
                    existing.proposers.push(name.clone());
                    existing.standing = Standing::Corroborated;
                }
                None => {
                    let id = set.len() as u32 + 1;
                    set.push(Candidate::new(id, idea.clone(), name));
                }
            }
        }
    }
    set
}

/// Whether two ideas are the same idea in different words.
///
/// Title plus mechanism, because `same_point` wants three shared significant
/// words and two short titles rarely have them even when they agree.
fn same_idea(a: &Idea, b: &Idea) -> bool {
    crate::textsim::same_point(&gist(a), &gist(b))
}

fn gist(idea: &Idea) -> String {
    format!("{} {}", idea.title, idea.how_it_works)
}

fn cross(
    agents: &[Agent],
    cfg: &Config,
    work_dir: &Path,
    subject: &str,
    set: &mut Vec<Candidate>,
    round: u32,
) -> Result<()> {
    let pending = set
        .iter()
        .filter(|c| c.standing == Standing::Unverified)
        .count();
    if pending == 0 {
        return Ok(());
    }
    log!(
        "brainstorm round {round}: each agent rules on the {pending} idea{} only the other \
         proposed",
        plural(pending)
    );
    let answers = concurrently(agents, |agent| {
        let theirs = set
            .iter()
            .filter(|c| c.standing == Standing::Unverified && !c.proposed_by(agent.name()))
            .count();
        if theirs == 0 {
            return Ok(CrossDoc::default());
        }
        let prompt = CROSS_PROMPT
            .replace("{subject}", subject)
            .replace("{read_only}", READ_ONLY)
            .replace("{ideas}", &ideas_for_prompt(set, Some(agent.name())));
        agent.ask_json::<CrossDoc>(
            &prompt,
            &schema::brainstorm_cross(),
            work_dir,
            &agent.effort(cfg, Call::Brainstorm(round)),
        )
    });
    let mut builds = Vec::new();
    for (name, result) in answers {
        let doc = match result {
            Ok(doc) => doc,
            Err(e) if e.kind() == ErrorKind::UncertainWrite => return Err(e),
            Err(e) => {
                logdim!("{name} could not rule on the other's ideas: {e}");
                continue;
            }
        };
        for warning in apply_cross(set, &name, doc, &mut builds) {
            logwarn!("{warning}");
        }
    }
    if !builds.is_empty() {
        log!(
            "brainstorm round {round}: {} idea{} built on another",
            builds.len(),
            plural(builds.len())
        );
    }
    set.extend(builds);
    Ok(())
}

/// One agent's rulings onto the set. Builds are collected rather than pushed
/// so a build cannot be ruled on in the round that produced it.
///
/// Returns what was dropped and why, for the caller to log.
pub fn apply_cross(
    set: &mut [Candidate],
    name: &str,
    doc: CrossDoc,
    builds: &mut Vec<Candidate>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for verdict in doc.verdicts {
        let Some(target) = set.iter_mut().find(|c| i64::from(c.id) == verdict.idea) else {
            warnings.push(format!(
                "{name} ruled on idea {} which is not in the set, so that ruling was dropped",
                verdict.idea
            ));
            continue;
        };
        if target.standing == Standing::Corroborated {
            // Both proposed it, so neither may challenge it, and the prompt
            // said so. Building on it is welcome from either.
            if verdict.verdict != CrossKind::Build {
                continue;
            }
        } else if target.proposed_by(name) {
            warnings.push(format!(
                "{name} ruled on its own idea {} and that ruling was dropped",
                target.id
            ));
            continue;
        } else if target.standing != Standing::Unverified && verdict.verdict != CrossKind::Build {
            // One already ruled on stays ruled on.
            continue;
        }
        match verdict.verdict {
            CrossKind::Keep => {}
            CrossKind::Challenge => {
                target.standing = Standing::Challenged;
                target.objection = Some((name.to_string(), verdict.reason));
            }
            CrossKind::Build => {
                let Some(idea) = verdict.build.filter(|i| !i.title.trim().is_empty()) else {
                    warnings.push(format!(
                        "{name} said it built on idea {} but wrote nothing, so that was dropped",
                        target.id
                    ));
                    continue;
                };
                let mut built = Candidate::new(0, idea, name);
                built.parent = Some(target.id);
                builds.push(built);
            }
        }
    }
    // Numbered after the set, densely, however many rulings were dropped and
    // whichever agent's builds came first.
    let mut id = set.len() as u32;
    for built in builds.iter_mut() {
        id += 1;
        built.id = id;
    }
    warnings
}

fn converge(
    agents: &[Agent],
    cfg: &Config,
    work_dir: &Path,
    subject: &str,
    set: &mut [Candidate],
    round: u32,
) -> Result<()> {
    let challenged = set
        .iter()
        .filter(|c| c.standing == Standing::Challenged)
        .count();
    log!(
        "brainstorm round {round}: {challenged} objection{} to answer, then both rank",
        plural(challenged)
    );
    let answers = concurrently(agents, |agent| {
        let mine: Vec<String> = set
            .iter()
            .filter(|c| c.standing == Standing::Challenged && c.proposed_by(agent.name()))
            .map(|c| c.id.to_string())
            .collect();
        let defend = if mine.is_empty() {
            "nothing you proposed was challenged, so defences is empty.".to_string()
        } else {
            format!(
                "answer each objection to an idea you proposed. Yours that were challenged: \
                 {}. One entry in defences for each.",
                mine.join(", ")
            )
        };
        let prompt = CONVERGE_PROMPT
            .replace("{subject}", subject)
            .replace("{defend}", &defend)
            .replace("{read_only}", READ_ONLY)
            .replace("{ideas}", &ideas_for_prompt(set, None));
        agent.ask_json::<ConvergeDoc>(
            &prompt,
            &schema::brainstorm_converge(),
            work_dir,
            &agent.effort(cfg, Call::Brainstorm(round)),
        )
    });
    for (name, result) in answers {
        let doc = match result {
            Ok(doc) => doc,
            Err(e) if e.kind() == ErrorKind::UncertainWrite => return Err(e),
            Err(e) => {
                logdim!("{name} could not rank: {e}");
                continue;
            }
        };
        for warning in apply_converge(set, &name, doc) {
            logwarn!("{warning}");
        }
    }
    Ok(())
}

/// One agent's defences and ranking onto the set.
///
/// A defence counts only from an agent that proposed the idea. An idea an
/// agent left out of its ranking scores one place below its last, so an
/// omission costs the idea rather than the agent that forgot it.
pub fn apply_converge(set: &mut [Candidate], name: &str, doc: ConvergeDoc) -> Vec<String> {
    let mut warnings = Vec::new();
    for defence in doc.defences {
        let Some(target) = set.iter_mut().find(|c| i64::from(c.id) == defence.idea) else {
            warnings.push(format!(
                "{name} defended idea {} which is not in the set, so that was dropped",
                defence.idea
            ));
            continue;
        };
        if !target.proposed_by(name) || target.standing != Standing::Challenged {
            continue;
        }
        if defence.stands {
            target.standing = Standing::Defended;
            target.defence = Some(defence.reply);
        } else {
            target.standing = Standing::Withdrawn;
        }
    }
    let mut placed: Vec<u32> = Vec::new();
    for (position, id) in doc.ranking.iter().enumerate() {
        let Some(target) = set.iter_mut().find(|c| i64::from(c.id) == *id) else {
            warnings.push(format!(
                "{name} ranked idea {id} which is not in the set, so that place was dropped"
            ));
            continue;
        };
        if placed.contains(&target.id) {
            continue;
        }
        placed.push(target.id);
        target.rank_sum += position as u32 + 1;
    }
    let last = placed.len() as u32 + 1;
    for c in set.iter_mut() {
        if c.standing != Standing::Withdrawn && !placed.contains(&c.id) {
            c.rank_sum += last;
        }
    }
    warnings
}

/// Keep the best `wanted`, and say why the rest were set aside.
///
/// Reached by both first, then by rank, then by number. A challenged idea whose
/// proposer never answered goes with the withdrawn ones: nobody stood by it.
pub fn settle(set: Vec<Candidate>, wanted: usize) -> (Vec<Candidate>, Vec<(Candidate, String)>) {
    let mut standing = Vec::new();
    let mut dropped = Vec::new();
    for c in set {
        match c.standing {
            Standing::Withdrawn => {
                let why = objection_line(&c, "withdrawn after");
                dropped.push((c, why));
            }
            Standing::Challenged => {
                let why = objection_line(&c, "not defended against");
                dropped.push((c, why));
            }
            _ => standing.push(c),
        }
    }
    standing.sort_by_key(|c| (c.standing != Standing::Corroborated, c.rank_sum, c.id));
    let rest = standing.split_off(wanted.min(standing.len()));
    for c in rest {
        dropped.push((c, "ranked below the cut".into()));
    }
    (standing, dropped)
}

/// One line for the header list. The objection is clipped to a sentence or
/// two: the whole argument is in the run's log, and a paragraph per dropped
/// idea would push the ideas that were kept off the first screen.
fn objection_line(c: &Candidate, lead: &str) -> String {
    match &c.objection {
        Some((who, why)) => format!(
            "{lead} {who}'s objection: {}",
            style::clip(&style::one_line(why), 240)
        ),
        None => format!("{lead} an objection"),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The set as the agents read it: numbered, tagged by whose it is, with what
/// has been said about each. `reader` is the agent being asked, so the tags
/// can say which ideas are theirs to rule on.
fn ideas_for_prompt(set: &[Candidate], reader: Option<&str>) -> String {
    let mut out = String::new();
    for c in set {
        let tag = match (reader, c.standing) {
            (_, Standing::Corroborated) => "both of you proposed this".to_string(),
            (Some(me), Standing::Unverified) if c.proposed_by(me) => "yours".to_string(),
            (Some(_), Standing::Unverified) => "rule on this".to_string(),
            (_, Standing::Withdrawn) => "withdrawn".to_string(),
            _ => format!("proposed by {}", c.proposers.join(" and ")),
        };
        out.push_str(&format!(
            "{}. {} [{tag}]\n",
            c.id,
            style::one_line(&c.idea.title)
        ));
        if let Some(parent) = c.parent {
            out.push_str(&format!("   builds on idea {parent}\n"));
        }
        out.push_str(&format!(
            "   combines: {}\n",
            c.idea
                .combines
                .iter()
                .map(|s| style::one_line(s))
                .collect::<Vec<_>>()
                .join("; ")
        ));
        for (label, text) in [
            ("how it works", &c.idea.how_it_works),
            ("why new", &c.idea.why_new),
            ("enables", &c.idea.enables),
            ("risks", &c.idea.risks),
            ("first experiment", &c.idea.first_experiment),
        ] {
            if !text.trim().is_empty() {
                out.push_str(&format!("   {label}: {}\n", style::one_line(text)));
            }
        }
        if let Some((who, why)) = &c.objection {
            out.push_str(&format!(
                "   objection from {who}: {}\n",
                style::one_line(why)
            ));
        }
        if let Some(reply) = &c.defence {
            out.push_str(&format!("   reply: {}\n", style::one_line(reply)));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// The six sections an idea is written in, wherever it is written.
///
/// One renderer for the session file and the issue body, so a person who read
/// one recognises the other, and so the headings stay `### `: the session
/// parser splits entries on `## ` and would take a `## Risks` for a new idea.
pub fn idea_sections(idea: &Idea, style: &Style) -> String {
    let mut out = String::new();
    out.push_str("### Combines\n");
    let combines: Vec<String> = idea
        .combines
        .iter()
        .map(|s| style::scrub(&style::one_line(s), style))
        .filter(|s| !s.is_empty())
        .collect();
    if combines.is_empty() {
        out.push_str("(not named)\n");
    }
    for piece in combines {
        out.push_str(&format!("- {piece}\n"));
    }
    for (heading, text) in [
        ("How it works", &idea.how_it_works),
        ("Why it is new", &idea.why_new),
        ("What it enables", &idea.enables),
        ("Risks", &idea.risks),
        ("First experiment", &idea.first_experiment),
    ] {
        out.push_str(&format!("\n### {heading}\n"));
        let text = style::scrub(text, style);
        if text.is_empty() {
            out.push_str("(not given)\n");
        } else {
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

/// The objection and the reply, for a reader deciding whether to try it.
/// Unnamed on purpose: who said it is provenance, and provenance stays out of
/// anything that may be filed.
fn discussion(c: &Candidate, style: &Style) -> String {
    let mut out = String::new();
    if let Some((_, why)) = &c.objection {
        out.push_str(&format!(
            "\n### Discussion\nObjection: {}\n",
            style::scrub(why, style)
        ));
        if let Some(reply) = &c.defence {
            out.push_str(&format!("\nReply: {}\n", style::scrub(reply, style)));
        }
    }
    out
}

/// The whole body of an idea as it is filed: the sections and the discussion.
pub fn issue_body(c: &Candidate, style: &Style) -> String {
    let mut body = idea_sections(&c.idea, style);
    body.push_str(&discussion(c, style));
    body.trim_end().to_string()
}

/// Who proposed it and what happened to it. Written above the sections in the
/// session file, and stripped again by [`parse_session`], so it never reaches
/// an issue: agent names are usually vendor names, and the attribution gate
/// exists to keep those off a tracker.
fn provenance(c: &Candidate, set: &[Candidate]) -> String {
    let mut line = if c.standing == Standing::Corroborated {
        format!("Reached independently by {}.", c.proposers.join(" and "))
    } else {
        match c.parent.and_then(|id| set.iter().find(|p| p.id == id)) {
            Some(parent) => format!(
                "Proposed by {}, building on \"{}\".",
                c.proposers.join(" and "),
                style::one_line(&parent.idea.title)
            ),
            None => format!("Proposed by {}.", c.proposers.join(" and ")),
        }
    };
    if let (Some((who, _)), Standing::Defended) = (&c.objection, c.standing) {
        line.push_str(&format!(
            " Challenged by {who}, defended by {}.",
            c.proposers.join(" and ")
        ));
    }
    line
}

const PROVENANCE_LEADS: [&str; 2] = ["Proposed by ", "Reached independently by "];

/// The session file.
///
/// A header a person reads, then one entry per kept idea in the follow-up
/// queue's shape, so `parse_session` and `spar followup --file` both read it
/// back. Two rules keep that true and the tests hold them: no `## ` line other
/// than an idea's title, and nothing after the last idea, which is why the
/// ideas set aside are listed in the header rather than at the end.
pub fn render_session(outcome: &Outcome, when: &str, style: &Style) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Brainstorm: {}\n",
        style::scrub(&style::one_line(&outcome.subject), style)
    ));
    out.push_str(&format!("Date: {when}\n"));
    out.push_str(&format!("Agents: {}\n", outcome.agents.join(", ")));
    out.push_str(&format!("Rounds: {}\n", outcome.rounds));
    for note in &outcome.notes {
        out.push_str(&format!(
            "Note: {}\n",
            style::scrub(&style::one_line(note), style)
        ));
    }
    if !outcome.dropped.is_empty() {
        out.push_str("\nAlso considered, and set aside:\n");
        for (c, why) in &outcome.dropped {
            out.push_str(&format!(
                "- {}: {}\n",
                style::scrub(&style::one_line(&c.idea.title), style),
                style::scrub(why, style)
            ));
        }
    }
    let all: Vec<Candidate> = outcome
        .kept
        .iter()
        .chain(outcome.dropped.iter().map(|(c, _)| c))
        .cloned()
        .collect();
    for c in &outcome.kept {
        out.push_str(&format!(
            "\n{FOLLOWUP_MARKER}\n## {}\n\n{}\n\n",
            style::title(&c.idea.title, style),
            style::scrub(&provenance(c, &all), style)
        ));
        out.push_str(&idea_sections(&c.idea, style));
        out.push_str(&discussion(c, style));
    }
    out.trim_end().to_string() + "\n"
}

/// The ideas in a saved session, as title and body, with the provenance line
/// taken out of each body so what is filed carries no agent's name.
pub fn parse_session(text: &str) -> Vec<(String, String)> {
    crate::followups::parse(text)
        .into_iter()
        .filter(|entry| !entry.title.trim().is_empty())
        .map(|entry| {
            let body: Vec<&str> = entry
                .body
                .lines()
                .filter(|line| !PROVENANCE_LEADS.iter().any(|lead| line.starts_with(lead)))
                .collect();
            (entry.title, body.join("\n").trim().to_string())
        })
        .collect()
}

/// What the kept ideas become when filed from a live run.
pub fn issue_entries(outcome: &Outcome, style: &Style) -> Vec<(String, String)> {
    outcome
        .kept
        .iter()
        .map(|c| (c.idea.title.clone(), issue_body(c, style)))
        .collect()
}

/// File each idea, or add it to the issue that already covers it. One failure
/// does not stop the rest; the write ledger makes the exit status say so.
pub fn file_ideas(repo: &Repo, entries: &[(String, String)]) {
    for (title, body) in entries {
        match crate::review::file_as_issue(repo, title, body) {
            Ok(filed) => println!("  {}", filed.describe(title)),
            Err(e) => {
                crate::logging::error(format!("could not file '{}': {e}", style::clip(title, 60)))
            }
        }
    }
}

/// What the terminal gets: the kept ideas in order, and where the rest went.
pub fn print_summary(outcome: &Outcome, path: &Path) {
    println!();
    println!(
        "{} idea{} kept, {} set aside, {} round{}",
        outcome.kept.len(),
        plural(outcome.kept.len()),
        outcome.dropped.len(),
        outcome.rounds,
        plural(outcome.rounds as usize)
    );
    for (i, c) in outcome.kept.iter().enumerate() {
        let mark = match c.standing {
            Standing::Corroborated => " (reached by both)",
            Standing::Defended => " (challenged and defended)",
            _ => "",
        };
        println!(
            "  {}. {}{mark}",
            i + 1,
            style::clip_bare(&style::one_line(&c.idea.title), 90)
        );
    }
    for note in &outcome.notes {
        println!("  note: {note}");
    }
    println!("\nwritten to {}", path.display());
}

/// Where a session goes unless `--out` says otherwise.
pub fn default_path(repo: &Repo, session: &Session, now_secs: u64) -> PathBuf {
    let name = match &session.topic {
        Some(topic) => slug(topic),
        None => "repo".to_string(),
    };
    repo.brainstorms_dir()
        .join(format!("{}-{name}.md", file_stamp(now_secs)))
}

/// A file name fragment from a topic: lowercase, runs of anything but a letter
/// or digit collapsed to one hyphen, cut at a word, "idea" when nothing is left.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(ch);
        } else {
            gap = true;
        }
    }
    if out.len() > 40 {
        let cut = out[..40].rfind('-').unwrap_or(40);
        out.truncate(cut);
    }
    if out.is_empty() {
        "idea".to_string()
    } else {
        out
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `yyyymmdd-hhmmss` in UTC, for a file name that sorts by when it was made.
pub fn file_stamp(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(secs);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// `2026-09-12 14:03 UTC`, for the header a person reads.
pub fn date_line(secs: u64) -> String {
    let (y, m, d, hh, mm, _) = civil(secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// Seconds since the epoch to a civil date and time, without a date crate.
/// The days-to-civil arithmetic is Howard Hinnant's, valid for any date this
/// will ever see.
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CrossVerdict, Defence};

    fn idea(title: &str, how: &str) -> Idea {
        Idea {
            title: title.into(),
            combines: vec!["hash time locks".into(), "batch auctions".into()],
            how_it_works: how.into(),
            why_new: "The nearest thing is a plain escrow, which cannot batch.".into(),
            enables: "Settling many trades in one transaction.".into(),
            risks: "Fee spikes make the batch uneconomic.".into(),
            first_experiment: "Simulate ten trades on regtest.".into(),
        }
    }

    fn pair(a: Vec<Idea>, b: Vec<Idea>) -> Vec<Candidate> {
        merge(&[("a".to_string(), a), ("b".to_string(), b)])
    }

    #[test]
    fn an_idea_both_agents_reach_is_kept_once_with_both_names() {
        let set = pair(
            vec![idea(
                "Batch settlement with timelocks",
                "Each trade posts a hash timelock and a batch auction clears them together at the deadline.",
            )],
            vec![idea(
                "Timelocked batch auction settlement",
                "Trades post hash timelocks, and one batch auction clears every one of them at the deadline together.",
            )],
        );
        assert_eq!(1, set.len());
        assert_eq!(Standing::Corroborated, set[0].standing);
        assert_eq!(vec!["a", "b"], set[0].proposers);
    }

    #[test]
    fn different_ideas_get_their_own_numbers_in_order() {
        let set = pair(
            vec![idea(
                "One",
                "A covenant restricts the spend path to a template.",
            )],
            vec![idea(
                "Two",
                "A watchtower gossips penalty transactions over a mesh radio.",
            )],
        );
        assert_eq!(vec![1, 2], set.iter().map(|c| c.id).collect::<Vec<_>>());
        assert!(set.iter().all(|c| c.standing == Standing::Unverified));
    }

    fn two() -> Vec<Candidate> {
        pair(
            vec![idea(
                "One",
                "A covenant restricts the spend path to a template.",
            )],
            vec![idea(
                "Two",
                "A watchtower gossips penalty transactions over a mesh radio.",
            )],
        )
    }

    fn verdict(id: i64, kind: CrossKind, build: Option<Idea>) -> CrossVerdict {
        CrossVerdict {
            idea: id,
            verdict: kind,
            reason: "because".into(),
            build,
        }
    }

    #[test]
    fn a_challenge_records_who_objected_and_why() {
        let mut set = two();
        let mut builds = Vec::new();
        let doc = CrossDoc {
            verdicts: vec![verdict(1, CrossKind::Challenge, None)],
        };
        let warnings = apply_cross(&mut set, "b", doc, &mut builds);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(Standing::Challenged, set[0].standing);
        assert_eq!(
            Some(("b".to_string(), "because".to_string())),
            set[0].objection
        );
        assert_eq!(Standing::Unverified, set[1].standing);
    }

    /// An agent rules on the other's ideas, never its own, and a ruling on a
    /// number that is not in the set says so rather than landing somewhere.
    #[test]
    fn rulings_on_your_own_idea_or_an_unknown_one_are_dropped_with_a_reason() {
        let mut set = two();
        let mut builds = Vec::new();
        let doc = CrossDoc {
            verdicts: vec![
                verdict(2, CrossKind::Challenge, None),
                verdict(9, CrossKind::Challenge, None),
            ],
        };
        let warnings = apply_cross(&mut set, "b", doc, &mut builds);
        assert_eq!(2, warnings.len(), "{warnings:?}");
        assert!(warnings[0].contains("its own idea 2"));
        assert!(warnings[1].contains("idea 9"));
        assert!(set.iter().all(|c| c.standing == Standing::Unverified));
    }

    #[test]
    fn a_build_becomes_a_new_idea_numbered_after_the_set_and_linked_to_its_parent() {
        let mut set = two();
        let mut builds = Vec::new();
        let doc = CrossDoc {
            verdicts: vec![
                verdict(1, CrossKind::Keep, None),
                verdict(
                    1,
                    CrossKind::Build,
                    Some(idea(
                        "One, plus",
                        "The covenant template also commits to a fee.",
                    )),
                ),
                verdict(1, CrossKind::Build, None),
            ],
        };
        let warnings = apply_cross(&mut set, "b", doc, &mut builds);
        assert_eq!(1, warnings.len(), "{warnings:?}");
        assert!(warnings[0].contains("wrote nothing"));
        assert_eq!(1, builds.len());
        assert_eq!(3, builds[0].id);
        assert_eq!(Some(1), builds[0].parent);
        assert_eq!(vec!["b"], builds[0].proposers);
        assert_eq!(Standing::Unverified, builds[0].standing);
    }

    #[test]
    fn an_idea_both_reached_cannot_be_challenged_but_can_be_built_on() {
        let mut set = pair(
            vec![idea(
                "Batch settlement with timelocks",
                "Each trade posts a hash timelock and a batch auction clears them together at the deadline.",
            )],
            vec![idea(
                "Timelocked batch auction settlement",
                "Trades post hash timelocks, and one batch auction clears every one of them at the deadline together.",
            )],
        );
        // A challenge is ignored, as the prompt said it would be. A build is
        // not, from either of them.
        let mut builds = Vec::new();
        let warnings = apply_cross(
            &mut set,
            "b",
            CrossDoc {
                verdicts: vec![
                    verdict(1, CrossKind::Challenge, None),
                    verdict(
                        1,
                        CrossKind::Build,
                        Some(idea("Batched, plus fees", "The auction also clears fees.")),
                    ),
                ],
            },
            &mut builds,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(Standing::Corroborated, set[0].standing);
        assert!(set[0].objection.is_none());
        assert_eq!(1, builds.len());
        assert_eq!(Some(1), builds[0].parent);
        assert_eq!(2, builds[0].id);
    }

    #[test]
    fn a_defence_stands_or_withdraws_and_only_the_proposer_may_give_one() {
        let mut set = two();
        set[0].standing = Standing::Challenged;
        set[0].objection = Some(("b".into(), "exists".into()));
        set[1].standing = Standing::Challenged;
        set[1].objection = Some(("a".into(), "cannot work".into()));
        let doc = ConvergeDoc {
            defences: vec![
                Defence {
                    idea: 1,
                    stands: true,
                    reply: "the prior art cannot batch".into(),
                },
                Defence {
                    idea: 2,
                    stands: false,
                    reply: String::new(),
                },
            ],
            ranking: vec![1, 2],
        };
        // "a" proposed 1, so its word on 2 does not count.
        let warnings = apply_converge(&mut set, "a", doc);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(Standing::Defended, set[0].standing);
        assert_eq!(
            Some("the prior art cannot batch".to_string()),
            set[0].defence
        );
        assert_eq!(Standing::Challenged, set[1].standing);
    }

    /// Leaving an idea out of a ranking is not a way to bury it silently: it
    /// scores one place below the last one named.
    #[test]
    fn an_omitted_idea_ranks_last_and_two_rankings_add_up() {
        let mut set = two();
        set.push(Candidate::new(3, idea("Three", "A third mechanism."), "a"));
        apply_converge(
            &mut set,
            "a",
            ConvergeDoc {
                defences: vec![],
                ranking: vec![3, 1],
            },
        );
        apply_converge(
            &mut set,
            "b",
            ConvergeDoc {
                defences: vec![],
                ranking: vec![1, 2, 3, 1],
            },
        );
        // a: 3 first, 1 second, 2 omitted so third. b: 1, 2, 3 in order.
        assert_eq!(2 + 1, set[0].rank_sum);
        assert_eq!(3 + 2, set[1].rank_sum);
        assert_eq!(1 + 3, set[2].rank_sum);
    }

    #[test]
    fn settle_keeps_the_best_and_says_why_each_of_the_rest_went() {
        let mut set = two();
        set.push(Candidate::new(3, idea("Three", "A third mechanism."), "a"));
        set.push(Candidate::new(4, idea("Four", "A fourth mechanism."), "b"));
        set[0].rank_sum = 5;
        set[1].rank_sum = 2;
        set[1].standing = Standing::Corroborated;
        set[2].standing = Standing::Withdrawn;
        set[2].objection = Some(("b".into(), "already in bip 119".into()));
        set[3].standing = Standing::Challenged;
        set[3].objection = Some(("a".into(), "breaks under reorg".into()));
        let (kept, dropped) = settle(set, 1);
        assert_eq!(vec![2], kept.iter().map(|c| c.id).collect::<Vec<_>>());
        let reasons: Vec<(u32, String)> =
            dropped.iter().map(|(c, why)| (c.id, why.clone())).collect();
        assert_eq!(3, reasons.len());
        assert_eq!(
            (
                3,
                "withdrawn after b's objection: already in bip 119".to_string()
            ),
            reasons[0]
        );
        assert_eq!(
            (
                4,
                "not defended against a's objection: breaks under reorg".to_string()
            ),
            reasons[1]
        );
        assert_eq!((1, "ranked below the cut".to_string()), reasons[2]);
    }

    #[test]
    fn reached_by_both_comes_first_whatever_the_ranking_said() {
        let mut set = two();
        set[0].rank_sum = 2;
        set[1].rank_sum = 4;
        set[1].standing = Standing::Corroborated;
        let (kept, _) = settle(set, 5);
        assert_eq!(vec![2, 1], kept.iter().map(|c| c.id).collect::<Vec<_>>());
    }

    fn outcome() -> Outcome {
        let mut set = two();
        set[0].standing = Standing::Defended;
        set[0].objection = Some(("b".into(), "CTV does this already".into()));
        set[0].defence = Some("CTV cannot commit to the fee".into());
        let mut built = Candidate::new(3, idea("Three", "A third mechanism."), "b");
        built.parent = Some(1);
        set.push(built);
        set[1].standing = Standing::Withdrawn;
        set[1].objection = Some(("a".into(), "radios are not the bottleneck".into()));
        let (kept, dropped) = settle(set, 5);
        Outcome {
            subject: "novel uses of covenants".into(),
            agents: vec!["a".into(), "b".into()],
            rounds: 3,
            kept,
            dropped,
            notes: vec![],
        }
    }

    /// The whole reason the file is shaped as it is: what spar wrote, spar can
    /// read back, and so can `spar followup --file`.
    #[test]
    fn the_session_file_round_trips_through_the_follow_up_parser() {
        let style = Style::default();
        let text = render_session(&outcome(), "2026-09-12 14:03 UTC", &style);
        let entries = parse_session(&text);
        assert_eq!(2, entries.len(), "{text}");
        assert_eq!("One", entries[0].0);
        assert_eq!("Three", entries[1].0);
        for (_, body) in &entries {
            assert!(body.starts_with("### Combines"), "{body}");
            assert!(!body.contains("Proposed by"), "{body}");
            assert!(!body.contains("Challenged by"), "{body}");
            assert!(body.contains("### First experiment"), "{body}");
        }
        assert!(entries[0].1.contains("Objection: CTV does this already"));
        assert!(entries[0].1.contains("Reply: CTV cannot commit to the fee"));
        // Nothing after the last idea, and no heading that would open one.
        let headings: Vec<&str> = text.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(vec!["## One", "## Three"], headings);
        assert!(
            text.contains("Also considered, and set aside:\n- Two: withdrawn after a's objection")
        );
        assert!(text.contains("Proposed by b, building on \"One\"."));
        assert!(text.contains("Challenged by b, defended by a."));
    }

    #[test]
    fn what_is_written_passes_the_style_gate() {
        let style = Style::default();
        let mut out = outcome();
        out.kept[0].idea.how_it_works =
            "A covenant \u{2014} the template kind \u{2014} restricts it.".into();
        out.kept[0]
            .idea
            .combines
            .push("Generated with Claude Code".into());
        let text = render_session(&out, "2026-09-12 14:03 UTC", &style);
        assert!(style::violations(&text, &style).is_empty(), "{text}");
        assert!(text.contains("A covenant, the template kind, restricts it."));
        let body = issue_body(&out.kept[0], &style);
        assert!(style::violations(&body, &style).is_empty(), "{body}");
        assert!(!body.contains("Proposed by"));
    }

    #[test]
    fn the_prompt_listing_tags_whose_ideas_are_whose() {
        let mut set = two();
        set.push(Candidate::new(3, idea("Three", "A third mechanism."), "a"));
        set[2].standing = Standing::Corroborated;
        set[2].proposers.push("b".into());
        let text = ideas_for_prompt(&set, Some("a"));
        assert!(text.contains("1. One [yours]"), "{text}");
        assert!(text.contains("2. Two [rule on this]"), "{text}");
        assert!(
            text.contains("3. Three [both of you proposed this]"),
            "{text}"
        );
        let text = ideas_for_prompt(&set, None);
        assert!(text.contains("1. One [proposed by a]"), "{text}");
    }

    #[test]
    fn slugs_are_file_names() {
        assert_eq!(
            "novel-uses-of-bitcoin-script",
            slug("Novel uses of Bitcoin Script!")
        );
        assert_eq!("idea", slug("   ???  "));
        let long = slug("one two three four five six seven eight nine ten eleven twelve");
        assert!(long.len() <= 40, "{long}");
        assert!(!long.ends_with('-'));
        assert_eq!("one-two-three-four-five-six-seven-eight", long);
    }

    #[test]
    fn the_stamp_is_a_utc_civil_date() {
        // 2026-09-12 14:03:07 UTC.
        let secs = 1_789_221_787;
        assert_eq!("20260912-140307", file_stamp(secs));
        assert_eq!("2026-09-12 14:03 UTC", date_line(secs));
        assert_eq!("19700101-000000", file_stamp(0));
        // The day after a leap day.
        assert_eq!("20240301-000000", file_stamp(1_709_251_200));
    }

    /// The prompts and the schemas say the same things about the same fields.
    #[test]
    fn the_prompts_name_every_field_the_schema_asks_for() {
        let schema = schema::brainstorm_ideas();
        let fields = schema["properties"]["ideas"]["items"]["properties"]
            .as_object()
            .unwrap();
        for field in fields.keys() {
            assert!(
                DIVERGE_PROMPT.contains(field.as_str()),
                "{field} is in the schema but not the diverge prompt"
            );
        }
        for verdict in ["build", "challenge", "keep"] {
            assert!(CROSS_PROMPT.contains(&format!("- {verdict}:")));
        }
        assert!(CONVERGE_PROMPT.contains("stands=true"));
        assert!(CONVERGE_PROMPT.contains("ranking"));
    }
}
