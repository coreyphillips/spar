//! Configuration, and the agent presets that make a new CLI a data change
//! rather than a code change.
//!
//! Presets are compiled into the binary. That is not an optimisation: a
//! `cargo install`ed binary has no source tree beside it, so a preset read from
//! a relative path would work for the author and fail for everyone else. Files
//! on disk still win over the built in copies, so a preset can be overridden or
//! a new one added without rebuilding.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Value;

use crate::error::Result;
use crate::proc::{expand_tilde, home_dir};
use crate::style::Style;
use crate::{bail, spar_err};

/// Presets that ship inside the binary.
pub const BUILTIN_PRESETS: &[(&str, &str)] = &[
    ("aider", include_str!("../presets/aider.toml")),
    ("claude", include_str!("../presets/claude.toml")),
    ("codex", include_str!("../presets/codex.toml")),
    ("cursor", include_str!("../presets/cursor.toml")),
    ("gemini", include_str!("../presets/gemini.toml")),
];

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// One element of a command template: a bare argument, or a group that is
/// dropped whole when its placeholder is unset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum CommandPart {
    One(String),
    Group(Vec<String>),
}

impl CommandPart {
    pub fn args(&self) -> &[String] {
        match self {
            CommandPart::One(s) => std::slice::from_ref(s),
            CommandPart::Group(v) => v,
        }
    }
}

/// How to read the answer out of what a CLI printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    /// Everything on stdout is the answer.
    Text,
    /// Same as text; named separately because it reads better in a preset.
    Json,
    /// An event stream, one JSON object per line.
    Jsonl,
}

/// Where the style rules go when a CLI has a system prompt flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemVia {
    /// Prepended to the prompt.
    Prompt,
    /// Passed through the `{system}` placeholder.
    Placeholder,
}

fn default_timeout() -> u64 {
    crate::proc::DEFAULT_TIMEOUT_SECS
}

fn default_output() -> OutputMode {
    OutputMode::Text
}

fn default_system_via() -> SystemVia {
    SystemVia::Prompt
}

/// Everything needed to drive one CLI. An agent is data, not a class:
/// supporting a new tool is a preset file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    #[serde(skip)]
    pub name: String,
    pub command: Vec<CommandPart>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default = "default_output")]
    pub output: OutputMode,
    /// For `jsonl`: the event fields that identify the agent's own message.
    #[serde(default)]
    pub message_match: BTreeMap<String, String>,
    /// For `jsonl`: the dotted path to the text inside a matching event.
    #[serde(default)]
    pub message_path: Option<String>,
    /// Extra places to look for the binary when it is not on PATH.
    #[serde(default)]
    pub search_paths: Vec<String>,
    #[serde(default = "default_system_via")]
    pub system_via: SystemVia,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// A stand in for when this agent cannot answer at all: a CLI that is down,
    /// out of quota, or refusing the request on policy grounds.
    ///
    /// Declared as a nested table rather than by naming a third agent, because
    /// spar takes exactly two and a backup is not a third opinion. It never
    /// reviews alongside the pair, it only answers in place of the one that
    /// failed, so the alternation the design rests on is unchanged.
    ///
    /// Built by `build_spec` from the `[agents.NAME.fallback]` table, never
    /// deserialized directly, so a preset of its own still resolves.
    #[serde(skip)]
    pub fallback: Option<Box<AgentSpec>>,

    // -- hints, inert at runtime -----------------------------------------
    //
    // Written into a generated config as comments so nobody has to guess what
    // to put in `model` or `effort`. Deliberately never validated against: a
    // CLI's options drift, and a stale allow list that refuses a model which
    // actually works would be worse than no hint at all.
    /// Model names this CLI is known to accept.
    #[serde(default)]
    pub models: Vec<String>,
    /// Effort levels this CLI is known to accept.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Where to check the current list, when spar cannot enumerate it.
    #[serde(default)]
    pub options_note: Option<String>,
}

impl AgentSpec {
    /// The model as configured, normalised. An unset and an empty value both
    /// mean "let the CLI pick", because `render` drops the flag either way.
    pub fn model_key(&self) -> String {
        self.model.as_deref().unwrap_or("").trim().to_string()
    }

    pub fn describe(&self) -> String {
        format!(
            "{}/{}",
            self.model.as_deref().unwrap_or("default model"),
            self.effort.as_deref().unwrap_or("default effort")
        )
    }
}

// ---------------------------------------------------------------------------
// Loop and style blocks
// ---------------------------------------------------------------------------

/// Where an out of scope finding goes. On your own repository an issue is the
/// right home. On a large repository that is not yours it is somebody else's
/// notification and somebody else's triage queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Followups {
    Issues,
    Local,
    None,
}

impl std::fmt::Display for Followups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Followups::Issues => "issues",
            Followups::Local => "local",
            Followups::None => "none",
        })
    }
}

/// Whether a pull request spar opens starts as a draft, and when it stops being
/// one.
///
/// A draft says the work is not for a person yet, which is exactly true while
/// two agents are still arguing about it. `UntilApproved` makes that state mean
/// something and clear itself: the loop marks the pull request ready the moment
/// it has no blocking findings left. `Always` is for somebody who promotes
/// every pull request by hand and wants spar to keep out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Drafts {
    /// Open ordinary pull requests. The default, and what spar has always done.
    Never,
    /// Open as a draft, and mark it ready when the review converges.
    UntilApproved,
    /// Open as a draft and leave it that way.
    Always,
}

impl std::fmt::Display for Drafts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Drafts::Never => "never",
            Drafts::UntilApproved => "until_approved",
            Drafts::Always => "always",
        })
    }
}

/// How much of its own working spar narrates into a pull request thread.
///
/// The agents never read the PR: they receive findings through their prompts,
/// so nothing in the loop depends on any of this being posted. It exists purely
/// for the person who reads the thread later, which is why the default is the
/// outcome rather than the play by play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrComments {
    /// One comment when the run finishes, and only if it has something to say.
    Outcome,
    /// A comment per review and per response, as it happens. An audit trail,
    /// at the cost of a thread nobody wants to read.
    Rounds,
    /// Never comment on a pull request. Everything goes to the terminal.
    None,
}

impl std::fmt::Display for PrComments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PrComments::Outcome => "outcome",
            PrComments::Rounds => "rounds",
            PrComments::None => "none",
        })
    }
}

/// Where resume state lives. Local keeps the PR clean and costs no API calls;
/// writing to the PR only buys anything if a run might be resumed from a
/// different checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateStore {
    Local,
    Pr,
    Both,
}

impl StateStore {
    pub fn writes_local(self) -> bool {
        matches!(self, StateStore::Local | StateStore::Both)
    }
    pub fn writes_pr(self) -> bool {
        matches!(self, StateStore::Pr | StateStore::Both)
    }
}

/// Whose comments `spar checkin` will act on.
///
/// The default is not timidity. Acting on a comment means a commit pushed to a
/// branch because somebody typed a sentence, and `authorAssociation` is one
/// field GitHub already returns on every comment endpoint that says whether
/// they can write to this repository at all. Everybody is still answered in
/// words either way; this governs only whether a comment can produce a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Anybody GitHub says can write here: OWNER, MEMBER, COLLABORATOR.
    Write,
    /// Anybody at all. Both agents still have to agree before anything changes.
    Anyone,
}

impl std::fmt::Display for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Trust::Write => "write",
            Trust::Anyone => "anyone",
        })
    }
}

impl Trust {
    /// Whether a comment from somebody with this association may produce a
    /// commit.
    pub fn may_act_on(self, association: &str) -> bool {
        match self {
            Trust::Anyone => true,
            Trust::Write => matches!(
                association.trim().to_uppercase().as_str(),
                "OWNER" | "MEMBER" | "COLLABORATOR"
            ),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffortSchedule {
    /// The deep first review.
    pub round_1: Option<String>,
    /// Later rounds only see a small delta.
    pub rest: Option<String>,
}

/// Every field takes its value from `LoopCfg::default()` when a config does not
/// mention it, rather than from a per-field function saying the same thing in a
/// second place. Two places is how a default goes stale.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopCfg {
    pub max_rounds: u32,
    pub auto_merge: bool,
    pub first_implementor: Option<String>,
    pub base_branch: String,
    pub worktrees: bool,
    pub keep_worktrees: bool,
    pub state_store: StateStore,
    pub branch_prefix: String,
    pub followups: Followups,
    /// File a non-blocking finding as a follow-up.
    ///
    /// Off by default, and this is the setting that stops a run breeding. A
    /// thorough reviewer always finds improvements, and turning each one into a
    /// tracker item made a single issue spawn ten, which spawned more: mean
    /// offspring above one never terminates. Not gating a merge is not the same
    /// as being worth somebody's triage queue.
    pub file_non_blocking: bool,
    /// Most follow-ups one run may record before it stops and says what it
    /// dropped. A backstop, not a target.
    pub max_followups: usize,
    /// Most parts `spar split` will make out of one issue or pull request.
    ///
    /// A backstop, not a target, in the shape of `max_followups`. There is no
    /// setting to turn splitting off: it is a command somebody types, and a
    /// setting to disable a command nobody has to run only ever confuses. There
    /// is no mechanical threshold either, because a floor on files changed
    /// would split a forty file rename and hold back a three file mess.
    pub max_split_parts: usize,
    /// Nits stay in the PR thread by default. A filed nit is somebody else's
    /// notification: a run on a production codebase once opened an issue titled
    /// "Log wording".
    pub file_nits: bool,
    /// Close an issue that both agents independently declined, after posting
    /// the shared reasoning. One agent's opinion is never enough.
    pub close_skipped: bool,
    /// Ask both agents to triage at the same time. They only read during
    /// triage, so there is nothing to serialise.
    pub parallel_triage: bool,
    /// Ignore issues and pull requests numbered below this when spar is picking
    /// for itself. 0 is no floor.
    ///
    /// A repository that has been going a while carries a tail of old issues
    /// nobody is going to reach, and since spar takes the lowest numbered open
    /// items it walks straight into them. A number you name explicitly is still
    /// honoured: naming it is the point.
    pub min_number: i64,
    /// Waves of newly filed follow-ups to fold back into the same run, rather
    /// than leaving them for the next one.
    ///
    /// Off by default because it multiplies what a run costs, and because each
    /// wave can file follow-ups of its own. Every wave is triaged like any
    /// other issue, so both agents still have to agree it is worth doing.
    pub absorb_new_issues: u32,
    /// Whether a pull request spar opens starts as a draft.
    pub drafts: Drafts,
    /// Extra instructions handed to both agents with every request.
    ///
    /// For what a person wants of this repository that the code cannot say and
    /// spar has no setting for: how far to go, what not to touch, what not to
    /// wait on. A CLI reads its own conventions file already, CLAUDE.md or
    /// AGENTS.md, but each reads only its own, and two agents given different
    /// standing instructions are not the pair this design rests on.
    ///
    /// Subordinate to the request and to the schema, which is said in the
    /// header they arrive under: they change how the work is done, never what
    /// was asked for or the shape of the answer.
    pub instructions: String,
    /// The most of one issue body that reaches a prompt.
    ///
    /// Sized so that no issue a person wrote is ever cut. It was 2000 for
    /// triage and 6000 for implement, silently, and both were small enough to
    /// clip an ordinary bug report: an agent given half an issue judges and
    /// implements the half it saw and has no way to know the rest existed.
    /// When this does fire it is said out loud, in the log and in the prompt.
    pub max_issue_chars: usize,
    /// The most every issue body together may add to one triage prompt.
    ///
    /// Triage reads the whole queue at once, so the only unbounded thing here
    /// is the queue. Past this, whole issues are left for the next run rather
    /// than every issue being shortened: a verdict is posted on the issue and
    /// can close it, so judging one on part of its body is worse than not
    /// reaching it yet.
    pub max_triage_chars: usize,
    /// Whose comments `spar checkin` will act on.
    pub checkin_trust: Trust,
    /// Mark a review thread resolved when spar made the change it asked for.
    ///
    /// A thread spar disagreed with is left open whatever this says: the person
    /// who raised it has not had their say yet, and it is their thread.
    pub checkin_resolve: bool,
    /// Most unanswered comments spar answers on one pull request in a run.
    ///
    /// A backstop against a long argument being read back to somebody, not a
    /// target. What it held back is said out loud.
    pub max_checkin_comments: usize,
    pub effort_schedule: EffortSchedule,
}

impl Default for LoopCfg {
    fn default() -> Self {
        Self {
            max_rounds: 3,
            auto_merge: false,
            first_implementor: None,
            base_branch: "main".into(),
            worktrees: true,
            keep_worktrees: false,
            state_store: StateStore::Local,
            branch_prefix: String::new(),
            followups: Followups::Local,
            file_non_blocking: false,
            max_followups: 5,
            max_split_parts: 4,
            file_nits: false,
            close_skipped: true,
            parallel_triage: true,
            min_number: 0,
            absorb_new_issues: 0,
            drafts: Drafts::Never,
            instructions: String::new(),
            max_issue_chars: 60_000,
            max_triage_chars: 200_000,
            checkin_trust: Trust::Write,
            checkin_resolve: true,
            max_checkin_comments: 20,
            effort_schedule: EffortSchedule::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StyleCfg {
    pub ban_em_dash: bool,
    pub ban_ai_attribution: bool,
    pub terse: bool,
    pub max_detail_chars: usize,
    pub max_summary_chars: usize,
    pub max_body_chars: usize,
    /// A filed issue's body. Far larger than a PR comment's on purpose: a
    /// comment is read with the diff in front of you, an issue is picked up
    /// cold months later by somebody who needs the whole story.
    pub max_issue_body_chars: usize,
    pub max_title_chars: usize,
    pub pr_comments: PrComments,
}

impl Default for StyleCfg {
    /// Taken from `Style`, which is where the budgets are decided, rather than
    /// written out again here.
    ///
    /// They were written out again here, and they drifted. The functions
    /// supplying them to serde were still named `d90`, `d200`, `d320`, `d900`
    /// and `d4000` while returning 140, 2000, 6000, 8000 and 20000, and the
    /// config `spar init` generated offered the old numbers as though they
    /// were current. Uncommenting one of those lines to see what it did cut
    /// every comment spar posts to a fifth of its length.
    fn default() -> Self {
        let style = Style::default();
        Self {
            ban_em_dash: style.ban_em_dash,
            ban_ai_attribution: style.ban_ai_attribution,
            terse: style.terse,
            max_detail_chars: style.max_detail_chars,
            max_summary_chars: style.max_summary_chars,
            max_body_chars: style.max_body_chars,
            max_issue_body_chars: style.max_issue_body_chars,
            max_title_chars: style.max_title_chars,
            pr_comments: style.pr_comments,
        }
    }
}

impl StyleCfg {
    pub fn to_style(&self) -> Style {
        Style {
            ban_em_dash: self.ban_em_dash,
            ban_ai_attribution: self.ban_ai_attribution,
            terse: self.terse,
            max_detail_chars: self.max_detail_chars,
            max_summary_chars: self.max_summary_chars,
            max_body_chars: self.max_body_chars,
            max_issue_body_chars: self.max_issue_body_chars,
            max_title_chars: self.max_title_chars,
            pr_comments: self.pr_comments,
        }
    }
}

// ---------------------------------------------------------------------------
// The whole config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    /// In declaration order, which is what `first_implementor` defaults to.
    pub agents: Vec<AgentSpec>,
    pub loop_cfg: LoopCfg,
    pub style: Style,
    /// Resolved: never empty, always one of the configured agents.
    pub first_implementor: String,
    /// Where this config was read from, for error messages.
    pub source: Option<PathBuf>,
}

impl Config {
    pub fn agent_names(&self) -> Vec<String> {
        self.agents.iter().map(|a| a.name.clone()).collect()
    }

    pub fn has_agent(&self, name: &str) -> bool {
        self.agents.iter().any(|a| a.name == name)
    }

    pub fn spec(&self, name: &str) -> Result<&AgentSpec> {
        self.agents.iter().find(|a| a.name == name).ok_or_else(|| {
            spar_err!(
                "no agent named '{name}' ({})",
                self.agent_names().join(", ")
            )
        })
    }

    /// The other agent. With exactly two configured, custody alternates by
    /// definition.
    pub fn other(&self, name: &str) -> String {
        let names = self.agent_names();
        if names.first().map(String::as_str) == Some(name) {
            names.get(1).cloned().unwrap_or_else(|| name.to_string())
        } else {
            names.first().cloned().unwrap_or_else(|| name.to_string())
        }
    }

    /// Round 1 gets the deep pass; later rounds only see a small delta, and a
    /// full ultra review of a three line delta is money on fire.
    pub fn effort_for_round(&self, spec: &AgentSpec, round: u32) -> Option<String> {
        let scheduled = if round <= 1 {
            self.loop_cfg.effort_schedule.round_1.clone()
        } else {
            self.loop_cfg.effort_schedule.rest.clone()
        };
        scheduled
            .filter(|s| !s.trim().is_empty())
            .or_else(|| spec.effort.clone())
    }

    pub fn base_branch(&self) -> &str {
        &self.loop_cfg.base_branch
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    agents: toml::Table,
    #[serde(default)]
    #[serde(rename = "loop")]
    loop_cfg: Option<LoopCfg>,
    #[serde(default)]
    style: Option<StyleCfg>,
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

/// Directories searched for preset overrides, nearest first.
///
/// Deliberately *not* a bare `presets/`. spar runs from inside the user's own
/// repository, where `presets/` is a perfectly ordinary directory name for
/// something unrelated (sampler configs, prompt libraries, editor themes), and
/// a stray `presets/claude.toml` shadowing the built in preset produces a
/// baffling failure: `spar init` reports Claude Code as missing while it sits
/// on PATH. Overrides live somewhere that names spar.
pub fn preset_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(custom) = std::env::var_os("SPAR_PRESET_DIR") {
        dirs.push(PathBuf::from(custom));
    }
    dirs.push(PathBuf::from(".spar").join("presets"));
    if let Some(home) = home_dir() {
        dirs.push(home.join(".config").join("spar").join("presets"));
    }
    dirs
}

/// Every preset name available, built in and on disk, sorted.
pub fn available_presets() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_PRESETS.iter().map(|(n, _)| n.to_string()).collect();
    for dir in preset_dirs() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Parse a whole TOML document into a `Value`.
///
/// `"...".parse::<Value>()` parses a single TOML *value*, not a document, so it
/// rejects the first comment line of every preset.
fn parse_document(text: &str, what: &str) -> Result<Value> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| spar_err!("{what} is not valid TOML: {e}"))?;
    Ok(Value::Table(table))
}

/// Load a preset. A file on disk wins over the built in copy of the same name,
/// so a drifting CLI can be corrected without waiting for a release.
pub fn load_preset(name: &str) -> Result<Value> {
    for dir in preset_dirs() {
        let path = dir.join(format!("{name}.toml"));
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| spar_err!("could not read preset {}: {e}", path.display()))?;
            return parse_document(&text, &format!("preset {}", path.display()));
        }
    }
    for (builtin, text) in BUILTIN_PRESETS {
        if *builtin == name {
            return parse_document(text, &format!("built in preset {name}"));
        }
    }
    Err(spar_err!(
        "unknown preset '{name}'. Available: {}",
        available_presets().join(", ")
    ))
}

/// Deep merge, with `over` winning. Used so a config block can override one
/// field of a preset without restating the whole command template.
fn merge(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Table(b), Value::Table(o)) => {
            let mut out = b.clone();
            for (key, value) in o {
                let merged = match out.get(key) {
                    Some(existing) => merge(existing, value),
                    None => value.clone(),
                };
                out.insert(key.clone(), merged);
            }
            Value::Table(out)
        }
        _ => over.clone(),
    }
}

fn build_spec(name: &str, raw: &Value) -> Result<AgentSpec> {
    let table = raw
        .as_table()
        .ok_or_else(|| spar_err!("agent '{name}' must be a table"))?;

    let merged = match table.get("preset").and_then(Value::as_str) {
        Some(preset) => merge(&load_preset(preset)?, raw),
        None => raw.clone(),
    };

    let mut merged_table = merged
        .as_table()
        .cloned()
        .ok_or_else(|| spar_err!("agent '{name}' must be a table"))?;
    merged_table.remove("preset");
    // Lifted out before the spec is deserialized: a fallback is a whole agent,
    // preset and all, and only this function knows how to resolve a preset.
    let fallback_raw = merged_table.remove("fallback");

    if !merged_table.contains_key("command") {
        bail!(
            "agent '{name}' has no command and no preset. Set one of them, or pick a preset: {}",
            available_presets().join(", ")
        );
    }

    let mut spec: AgentSpec = Value::Table(merged_table)
        .try_into()
        .map_err(|e| spar_err!("agent '{name}': {e}"))?;
    spec.name = name.to_string();

    if spec.command.is_empty() {
        bail!("agent '{name}' has an empty command");
    }
    if matches!(spec.command.first(), Some(CommandPart::Group(_))) {
        bail!("agent '{name}': the first command element must be the program name, not a group");
    }
    if spec.output == OutputMode::Jsonl && spec.message_path.as_deref().unwrap_or("").is_empty() {
        bail!(
            "agent '{name}': output = \"jsonl\" needs a message_path saying where the answer lives"
        );
    }

    if let Some(raw) = fallback_raw {
        if !raw.is_table() {
            bail!(
                "agent '{name}': fallback is a whole agent, so write it as a table:\n                   [agents.{name}.fallback]\n  preset = \"cursor\""
            );
        }
        // Named for the env override it answers to, SPAR_<NAME>_FALLBACK_BIN,
        // and so a log line says which agent stood in for which.
        let backup = build_spec(&format!("{name}-fallback"), &raw)?;
        if backup.fallback.is_some() {
            bail!(
                "agent '{name}': a fallback may not have a fallback of its own. Each one costs \
                 another full timeout on a call that has already failed once."
            );
        }
        spec.fallback = Some(Box::new(backup));
    }

    Ok(spec)
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// One option the parser accepts, with the value it takes when unset.
#[derive(Debug, Clone)]
pub struct OptionInfo {
    pub section: &'static str,
    pub key: String,
    pub default: String,
}

/// Every option a config file may set, with its default.
///
/// Derived from the defaults themselves rather than written out by hand, so an
/// option added to the code cannot go missing here. That is what lets `doctor`
/// tell somebody upgrading which settings are new since they wrote their file.
pub fn known_options() -> Vec<OptionInfo> {
    fn lines<T: Serialize>(section: &'static str, value: &T) -> Vec<OptionInfo> {
        toml::to_string(value)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(" = "))
            .map(|(key, default)| OptionInfo {
                section,
                key: key.trim().to_string(),
                default: default.trim().to_string(),
            })
            .collect()
    }
    let mut out = lines("loop", &LoopCfg::default());
    out.extend(lines("style", &StyleCfg::default()));
    out.extend(lines(
        "loop.effort_schedule",
        &EffortSchedule {
            round_1: Some("high".into()),
            rest: Some("low".into()),
        },
    ));
    out
}

/// Whether a config file mentions an option at all, set or commented out.
pub fn mentions(config_text: &str, key: &str) -> bool {
    config_text.lines().any(|line| {
        let bare = line.trim_start().trim_start_matches('#').trim_start();
        bare.starts_with(&format!("{key} ")) || bare.starts_with(&format!("{key}="))
    })
}

/// Options this config file has never heard of, which is what somebody who
/// upgraded wants to know.
pub fn unmentioned_options(config_text: &str) -> Vec<OptionInfo> {
    known_options()
        .into_iter()
        .filter(|o| !mentions(config_text, &o.key))
        .collect()
}

pub const CONFIG_NAMES: &[&str] = &["spar.toml", ".spar.toml"];

/// Find a config: an explicit path, then the working directory, then
/// `~/.config/spar/spar.toml`.
pub fn find_config(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("config not found: {}", path.display());
        }
        return Ok(Some(path.to_path_buf()));
    }
    for name in CONFIG_NAMES {
        let path = PathBuf::from(name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    if let Some(home) = home_dir() {
        let path = home.join(".config").join("spar").join("spar.toml");
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let Some(path) = find_config(explicit)? else {
        bail!(
            "no spar.toml found. Run `spar init` to generate one from the CLIs you have installed."
        );
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| spar_err!("could not read {}: {e}", path.display()))?;
    let mut cfg = parse(&text).map_err(|e| spar_err!("{}: {e}", path.display()))?;
    cfg.source = Some(path);
    Ok(cfg)
}

pub fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;

    if raw.agents.len() != 2 {
        bail!(
            "spar needs exactly two agents, found {}. The whole design is one reviewing the other.",
            raw.agents.len()
        );
    }

    let mut agents = Vec::new();
    for (name, value) in raw.agents.iter() {
        agents.push(build_spec(name, value)?);
    }

    let loop_cfg = raw.loop_cfg.unwrap_or_default();
    let style = raw.style.unwrap_or_default().to_style();

    if loop_cfg.max_rounds == 0 {
        bail!("max_rounds must be at least 1");
    }
    // A draft cannot be merged, so merging one means promoting it first, which
    // is the one thing `always` asks spar not to do. Refusing is better than
    // picking a winner: either setting alone is coherent and only somebody who
    // set both can say which they meant.
    if loop_cfg.auto_merge && loop_cfg.drafts == Drafts::Always {
        bail!(
            "auto_merge cannot be on with drafts = \"always\": merging a draft means marking it \
             ready, which is what \"always\" asks spar not to do. Use drafts = \"until_approved\" \
             to have it promoted when the review converges, or turn auto_merge off."
        );
    }

    let first = match &loop_cfg.first_implementor {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => agents[0].name.clone(),
    };
    if !agents.iter().any(|a| a.name == first) {
        bail!(
            "first_implementor '{first}' is not a configured agent ({})",
            agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(Config {
        agents,
        loop_cfg,
        style,
        first_implementor: first,
        source: None,
    })
}

/// Resolve a configured search path, expanding a leading `~`.
pub fn resolve_search_path(raw: &str) -> PathBuf {
    expand_tilde(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_AGENTS: &str = r#"
[agents.claude]
preset = "claude"
model = "fable"

[agents.codex]
preset = "codex"
model = "gpt-5.6-sol"
"#;

    // -- fallback --------------------------------------------------------

    #[test]
    fn a_fallback_is_a_whole_agent_with_its_own_preset() {
        let text = format!(
            "{TWO_AGENTS}\n[agents.codex.fallback]\npreset = \"cursor\"\nmodel = \"kimi-k3\"\n"
        );
        let cfg = parse(&text).expect("parses");
        // Still a pair. A backup is not a third opinion.
        assert_eq!(2, cfg.agents.len());
        let codex = cfg.spec("codex").expect("codex");
        let backup = codex.fallback.as_ref().expect("fallback");
        assert_eq!("codex-fallback", backup.name);
        assert_eq!(Some("kimi-k3"), backup.model.as_deref());
        assert_eq!(
            Some(&CommandPart::One("cursor-agent".into())),
            backup.command.first()
        );
    }

    #[test]
    fn the_agent_without_a_fallback_does_not_grow_one() {
        let cfg = parse(TWO_AGENTS).expect("parses");
        assert!(cfg.agents.iter().all(|a| a.fallback.is_none()));
    }

    #[test]
    fn a_fallback_may_not_have_one_of_its_own() {
        let text = format!(
            "{TWO_AGENTS}\n[agents.codex.fallback]\npreset = \"cursor\"\n\
             [agents.codex.fallback.fallback]\npreset = \"gemini\"\n"
        );
        let err = parse(&text).expect_err("rejected");
        assert!(err.message().contains("may not have a fallback"), "{err}");
    }

    #[test]
    fn a_fallback_written_as_a_string_says_what_it_should_be() {
        let text = "[agents.claude]\npreset = \"claude\"\n\n\
                    [agents.codex]\npreset = \"codex\"\nfallback = \"cursor\"\n";
        let err = parse(text).expect_err("rejected");
        assert!(err.message().contains("[agents.codex.fallback]"), "{err}");
    }

    /// A block that names one setting keeps the defaults for every setting it
    /// did not name. That is what the container level serde default buys: each
    /// field used to carry its own default function repeating a number that
    /// also lived in `Default`, and the two copies stopped agreeing.
    #[test]
    fn a_partial_block_keeps_the_defaults_it_did_not_name() {
        let text = format!("{TWO_AGENTS}\n[loop]\nmax_rounds = 9\n\n[style]\nterse = false\n");
        let cfg = parse(&text).expect("parses");

        assert_eq!(9, cfg.loop_cfg.max_rounds);
        assert_eq!(LoopCfg::default().followups, cfg.loop_cfg.followups);
        assert_eq!(LoopCfg::default().close_skipped, cfg.loop_cfg.close_skipped);

        assert!(!cfg.style.terse);
        assert_eq!(Style::default().max_body_chars, cfg.style.max_body_chars);
        assert_eq!(Style::default().max_title_chars, cfg.style.max_title_chars);
    }

    /// The budgets are decided in `Style` and read from there by the config
    /// layer. When they were written out in both places they drifted, and the
    /// generated config offered the older set for months.
    #[test]
    fn the_config_layer_does_not_keep_its_own_copy_of_the_budgets() {
        assert_eq!(Style::default(), StyleCfg::default().to_style());
    }

    // -- drafts ------------------------------------------------------------

    #[test]
    fn pull_requests_are_not_drafts_unless_asked_for() {
        assert_eq!(Drafts::Never, parse(TWO_AGENTS).unwrap().loop_cfg.drafts);
    }

    #[test]
    fn each_draft_setting_parses() {
        for (text, want) in [
            ("never", Drafts::Never),
            ("until_approved", Drafts::UntilApproved),
            ("always", Drafts::Always),
        ] {
            let cfg = parse(&format!("{TWO_AGENTS}\n[loop]\ndrafts = \"{text}\"\n"))
                .unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(want, cfg.loop_cfg.drafts, "{text}");
        }
    }

    /// Merging a draft means marking it ready, which is the one thing `always`
    /// asks spar not to do. Either setting alone is coherent, so refusing beats
    /// picking a winner between them.
    #[test]
    fn auto_merge_and_a_permanent_draft_are_refused_together() {
        let text = format!("{TWO_AGENTS}\n[loop]\nauto_merge = true\ndrafts = \"always\"\n");
        let err = parse(&text).expect_err("refused");
        assert!(err.message().contains("auto_merge"), "{err}");
        assert!(
            err.message().contains("until_approved"),
            "says the way out: {err}"
        );
    }

    /// The pairing that does make sense: the draft clears when the review
    /// converges, and then it can merge.
    #[test]
    fn auto_merge_is_fine_with_a_draft_that_clears() {
        let text =
            format!("{TWO_AGENTS}\n[loop]\nauto_merge = true\ndrafts = \"until_approved\"\n");
        assert!(parse(&text).is_ok());
    }

    #[test]
    fn every_builtin_preset_parses() {
        for (name, _) in BUILTIN_PRESETS {
            let value = load_preset(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(value.get("command").is_some(), "{name} has no command");
        }
    }

    #[test]
    fn every_builtin_preset_builds_a_spec() {
        for (name, _) in BUILTIN_PRESETS {
            let raw = parse_document(&format!("preset = \"{name}\""), "test").unwrap();
            build_spec(name, &raw).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    /// `--allowedTools` is variadic, so the separate form swallows the
    /// following positional prompt unless another flag happens to sit between
    /// them. The equals form is not cosmetic.
    #[test]
    fn claude_preset_uses_the_equals_form_for_allowed_tools() {
        let spec = build_spec(
            "claude",
            &parse_document("preset = \"claude\"", "test").unwrap(),
        )
        .unwrap();
        let flat: Vec<&String> = spec.command.iter().flat_map(|p| p.args()).collect();
        assert!(flat.iter().any(|a| a.starts_with("--allowedTools=")));
        assert!(!flat.iter().any(|a| a.as_str() == "--allowedTools"));
    }

    #[test]
    fn codex_preset_declares_where_its_answer_lives() {
        let spec = build_spec(
            "codex",
            &parse_document("preset = \"codex\"", "test").unwrap(),
        )
        .unwrap();
        assert_eq!(OutputMode::Jsonl, spec.output);
        assert_eq!(Some("item.text"), spec.message_path.as_deref());
        assert!(!spec.message_match.is_empty());
    }

    #[test]
    fn agent_order_follows_declaration_order() {
        let cfg = parse(TWO_AGENTS).unwrap();
        assert_eq!(vec!["claude", "codex"], cfg.agent_names());
        assert_eq!("claude", cfg.first_implementor);
    }

    #[test]
    fn other_alternates() {
        let cfg = parse(TWO_AGENTS).unwrap();
        assert_eq!("codex", cfg.other("claude"));
        assert_eq!("claude", cfg.other("codex"));
    }

    #[test]
    fn a_config_block_overrides_one_preset_field() {
        let cfg = parse(TWO_AGENTS).unwrap();
        let claude = cfg.spec("claude").unwrap();
        assert_eq!(Some("fable"), claude.model.as_deref());
        assert!(claude.command.len() > 1, "the preset command survived");
    }

    #[test]
    fn exactly_two_agents_are_required() {
        let one = "[agents.claude]\npreset = \"claude\"\n";
        assert!(parse(one).unwrap_err().to_string().contains("exactly two"));
    }

    #[test]
    fn an_unknown_agent_option_is_named() {
        let text = "[agents.a]\ncommand = [\"x\"]\nwidget = 3\n[agents.b]\ncommand = [\"y\"]\n";
        let err = parse(text).unwrap_err().to_string();
        assert!(err.contains("widget"), "{err}");
    }

    #[test]
    fn an_unknown_loop_option_is_named() {
        let text = format!("{TWO_AGENTS}\n[loop]\nmax_round = 4\n");
        let err = parse(&text).unwrap_err().to_string();
        assert!(err.contains("max_round"), "{err}");
    }

    #[test]
    fn an_agent_with_no_command_and_no_preset_is_rejected() {
        let text = "[agents.a]\nmodel = \"x\"\n[agents.b]\ncommand = [\"y\"]\n";
        let err = parse(text).unwrap_err().to_string();
        assert!(err.contains("no command and no preset"), "{err}");
    }

    #[test]
    fn jsonl_without_a_message_path_is_rejected() {
        let text =
            "[agents.a]\ncommand = [\"x\"]\noutput = \"jsonl\"\n[agents.b]\ncommand = [\"y\"]\n";
        let err = parse(text).unwrap_err().to_string();
        assert!(err.contains("message_path"), "{err}");
    }

    #[test]
    fn first_implementor_must_name_a_configured_agent() {
        let text = format!("{TWO_AGENTS}\n[loop]\nfirst_implementor = \"nobody\"\n");
        let err = parse(&text).unwrap_err().to_string();
        assert!(err.contains("not a configured agent"), "{err}");
    }

    #[test]
    fn defaults_are_the_conservative_ones() {
        let cfg = parse(TWO_AGENTS).unwrap();
        assert!(
            !cfg.loop_cfg.auto_merge,
            "auto_merge must be off by default"
        );
        assert!(cfg.loop_cfg.worktrees);
        assert!(
            !cfg.loop_cfg.file_nits,
            "a filed nit is somebody else's triage queue"
        );
        assert_eq!(3, cfg.loop_cfg.max_rounds);
        assert_eq!(
            Followups::Local,
            cfg.loop_cfg.followups,
            "the tracker is somebody's queue; the default must not write to it"
        );
        assert!(
            !cfg.loop_cfg.file_non_blocking,
            "a suggestion is not a tracker item"
        );
        assert_eq!(StateStore::Local, cfg.loop_cfg.state_store);
        assert!(cfg.style.terse);
    }

    #[test]
    fn effort_schedule_splits_round_one_from_the_rest() {
        let text =
            format!("{TWO_AGENTS}\n[loop.effort_schedule]\nround_1 = \"ultra\"\nrest = \"high\"\n");
        let cfg = parse(&text).unwrap();
        let spec = cfg.spec("claude").unwrap();
        assert_eq!(Some("ultra".into()), cfg.effort_for_round(spec, 1));
        assert_eq!(Some("high".into()), cfg.effort_for_round(spec, 2));
        assert_eq!(Some("high".into()), cfg.effort_for_round(spec, 9));
    }

    #[test]
    fn effort_falls_back_to_the_agents_own_setting() {
        let text = format!("{TWO_AGENTS}effort = \"low\"\n");
        let cfg = parse(&text).unwrap();
        let spec = cfg.spec("codex").unwrap();
        assert_eq!(Some("low".into()), cfg.effort_for_round(spec, 1));
    }

    #[test]
    fn an_unset_model_and_an_empty_model_normalise_the_same() {
        let a = AgentSpec {
            name: "a".into(),
            command: vec![CommandPart::One("x".into())],
            model: None,
            effort: None,
            output: OutputMode::Text,
            message_match: BTreeMap::new(),
            message_path: None,
            search_paths: vec![],
            system_via: SystemVia::Prompt,
            timeout: 60,
            fallback: None,
            models: vec![],
            efforts: vec![],
            options_note: None,
        };
        let b = AgentSpec {
            model: Some("  ".into()),
            ..a.clone()
        };
        assert_eq!(a.model_key(), b.model_key());
    }

    #[test]
    fn max_rounds_zero_is_rejected() {
        let text = format!("{TWO_AGENTS}\n[loop]\nmax_rounds = 0\n");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn an_inline_command_needs_no_preset() {
        let text = r#"
[agents.custom]
command = ["mytool", ["-m", "{model}"], "--prompt", "{prompt}"]
output = "text"

[agents.other]
command = ["othertool", "{prompt}"]
"#;
        let cfg = parse(text).unwrap();
        assert_eq!(4, cfg.spec("custom").unwrap().command.len());
    }

    #[test]
    fn style_budgets_are_configurable() {
        let text = format!("{TWO_AGENTS}\n[style]\nterse = false\nmax_detail_chars = 40\n");
        let cfg = parse(&text).unwrap();
        assert!(!cfg.style.terse);
        assert_eq!(40, cfg.style.max_detail_chars);
    }
}
