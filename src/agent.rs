//! Driving one CLI.
//!
//! An agent is a command template plus an output adapter, not a class.
//! Supporting a new CLI is a preset file, which is the difference between a
//! tool that works for its author and one that works for anyone who installs
//! it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use crate::config::{AgentSpec, CommandPart, OutputMode, SystemVia};
use crate::error::{ErrorKind, Result, SparError};
use crate::jsonx;
use crate::proc::{self, ExecOpts};
use crate::{bail, log, logdim, logwarn, spar_err};

/// Injected into every request.
///
/// Prompting alone is not sufficient, which is why the rules about what spar
/// posts are also enforced deterministically on the way out; a model that was
/// asked leaves the gate less to fix.
///
/// The rule about comments in the code is the exception, and it is worth being
/// honest that it is one. Nothing can mechanically judge whether a comment
/// earned its length, so that rule is only ever asked for. It is here rather
/// than in the implement prompt because a reviewer that fixes a finding itself
/// writes code too, and a rule that applies to one and not the other produces a
/// file commented two ways.
pub const STYLE_RULES: &str = "\
Style rules for every artifact you produce (commits, PR titles, PR bodies, issue
titles, issue bodies, review comments, and the comments in code you write):
- Never use em-dashes or en-dashes. Use commas, colons, or parentheses.
- Never mention Claude, Codex, OpenAI, ChatGPT, Anthropic, AI, or any tooling
  used to produce the work.
- Never add a Co-Authored-By trailer or a \"Generated with\" footer to commits.
- Be brief. A human engineer with other work has to read this. Lead with the
  point, cut the preamble, stop when you are done. Do not restate the task, do
  not announce what you are about to do, do not summarise what the diff already
  shows.
- Brief means saying fewer things, never packing more into a sentence. Two
  plain sentences beat one that has to be read twice. Split a sentence that
  carries three facts, and split one that makes the reader hold an identifier
  in their head to parse the rest of the clause. A comma splice joining two
  ideas to save a full stop costs the reader more than the full stop would.
- No headings, bullet lists, or bold text in anything only a few sentences long.
- Comment code for the reason, not the change. A comment earns its length from
  what the code cannot say for itself: a constraint that is not local, an
  alternative that was tried and does not work, a surprise the next reader would
  otherwise trip on. Write the reason that holds now, not the investigation that
  found it. A paragraph above a three line change is almost always the debugging
  story, and the reader wants the conclusion of it.
Write as a human engineer would, because the reader neither knows nor cares what
produced the work.";

const JSON_INSTRUCTION: &str = "Respond with ONLY a JSON object matching this \
schema. No prose, no markdown fences, no commentary before or after:";

/// What a run's own instructions arrive under.
///
/// Subordinate on purpose. A person adding "do not wait for CI" should not be
/// able to talk an agent out of the schema it was asked for, and a model told
/// where an instruction came from weighs it against the request rather than
/// over it.
const INSTRUCTIONS_HEADER: &str = "Additional instructions from the person who \
started this run. They change how you work, not what was asked for above and \
not the shape of your answer:";

pub struct Agent {
    pub spec: AgentSpec,
    /// Answers in this agent's place when it cannot answer at all. Never
    /// alongside it: the pair is still two, and the fallback only ever holds
    /// the turn the failed agent was already holding.
    fallback: Option<Box<Agent>>,
    /// Extra instructions for this run, carried onto every request.
    instructions: Option<String>,
    resolved: OnceLock<PathBuf>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} {}>", self.spec.name, self.spec.describe())
    }
}

impl Agent {
    pub fn new(spec: AgentSpec) -> Self {
        let fallback = spec
            .fallback
            .clone()
            .map(|backup| Box::new(Agent::new(*backup)));
        Self {
            spec,
            fallback,
            instructions: None,
            resolved: OnceLock::new(),
        }
    }

    /// Carry this run's instructions, here and on the stand in.
    ///
    /// The fallback gets them too. It answers in this agent's place, so a run
    /// told not to wait on something should not start waiting the moment the
    /// primary hands over.
    pub fn with_instructions(mut self, text: &str) -> Self {
        let text = text.trim();
        if text.is_empty() {
            return self;
        }
        if let Some(backup) = self.fallback.take() {
            self.fallback = Some(Box::new(backup.with_instructions(text)));
        }
        self.instructions = Some(text.to_string());
        self
    }

    /// The request with this run's instructions after it.
    ///
    /// After, because the task is what the agent is doing and these modify how.
    /// Before the schema, which `ask_json` appends afterwards, so the shape of
    /// the answer stays the last thing read.
    fn instructed(&self, prompt: &str) -> String {
        match &self.instructions {
            Some(extra) => format!("{prompt}\n\n{INSTRUCTIONS_HEADER}\n{extra}"),
            None => prompt.to_string(),
        }
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// The stand in, if one is configured.
    pub fn fallback(&self) -> Option<&Agent> {
        self.fallback.as_deref()
    }

    /// The program the template names, before any resolution. What somebody
    /// has to install when spar reports it missing.
    pub fn program(&self) -> &str {
        match self.spec.command.first() {
            Some(CommandPart::One(program)) => program,
            _ => self.name(),
        }
    }

    /// The environment variable that points this agent's binary somewhere else.
    pub fn env_key(&self) -> String {
        format!(
            "SPAR_{}_BIN",
            self.spec.name.to_uppercase().replace('-', "_")
        )
    }

    /// Used by the tests, and by `doctor` when it wants to report a path it
    /// already knows.
    #[doc(hidden)]
    pub fn with_bin(spec: AgentSpec, bin: impl Into<PathBuf>) -> Self {
        let agent = Self::new(spec);
        let _ = agent.resolved.set(bin.into());
        agent
    }

    // -- binary resolution -------------------------------------------------

    /// `SPAR_<NAME>_BIN` first, then the template's own program name on PATH or
    /// as an absolute path, then the preset's search paths. Never guess
    /// silently: a miss reports every location tried, because a tool that
    /// quietly runs the wrong binary is worse than one that fails.
    pub fn resolve_bin(&self) -> Result<&Path> {
        if let Some(found) = self.resolved.get() {
            return Ok(found.as_path());
        }
        let found = self.locate()?;
        let _ = self.resolved.set(found);
        Ok(self.resolved.get().expect("just set").as_path())
    }

    fn locate(&self) -> Result<PathBuf> {
        let wanted = match self.spec.command.first() {
            Some(CommandPart::One(program)) => program.clone(),
            _ => bail!("agent '{}' has no command configured", self.spec.name),
        };

        let env_key = self.env_key();
        let env_override = std::env::var(&env_key)
            .ok()
            .filter(|v| !v.trim().is_empty());

        let mut tried: Vec<String> = Vec::new();

        for candidate in env_override
            .iter()
            .map(String::as_str)
            .chain([wanted.as_str()])
        {
            let path = Path::new(candidate);
            if path.is_absolute() || candidate.contains(std::path::MAIN_SEPARATOR) {
                let expanded = proc::expand_tilde(candidate);
                tried.push(expanded.display().to_string());
                if proc::is_executable(&expanded) {
                    return Ok(expanded);
                }
            } else {
                tried.push(format!("{candidate} (PATH)"));
                if let Some(found) = proc::which(candidate) {
                    return Ok(found);
                }
            }
        }

        for base in &self.spec.search_paths {
            let base = proc::expand_tilde(base);
            let candidate = if base.file_name().and_then(|n| n.to_str()) == Some(wanted.as_str()) {
                base
            } else {
                base.join(&wanted)
            };
            tried.push(candidate.display().to_string());
            if proc::is_executable(&candidate) {
                return Ok(candidate);
            }
        }

        Err(spar_err!(
            "could not find the binary for agent '{}'. Tried:\n  {}\nSet agents.{}.command[0] to \
             an absolute path, or {}=/path/to/binary.",
            self.spec.name,
            tried.join("\n  "),
            self.spec.name,
            env_key
        ))
    }

    // -- command rendering -------------------------------------------------

    /// Substitute placeholders. A group whose placeholder is unset is dropped
    /// whole, so omitting `model` drops `--model` with it rather than passing
    /// an empty string that the CLI would reject or, worse, accept.
    pub fn render(&self, values: &Placeholders) -> Result<Vec<String>> {
        let mut out = vec![self.resolve_bin()?.display().to_string()];
        for part in self.spec.command.iter().skip(1) {
            let mut rendered = Vec::new();
            let mut skip = false;
            for arg in part.args() {
                match values.substitute(arg) {
                    Some(text) => rendered.push(text),
                    None => {
                        skip = true;
                        break;
                    }
                }
            }
            if !skip {
                out.extend(rendered);
            }
        }
        Ok(out)
    }

    /// True when the template has somewhere to put a schema, meaning the CLI can
    /// do structured output natively rather than being asked in the prompt.
    ///
    /// Either form counts: a path for a CLI that reads the schema from disk, or
    /// the schema itself for one that takes it as an argument.
    pub fn supports_schema(&self) -> bool {
        self.spec
            .command
            .iter()
            .flat_map(|p| p.args())
            .any(|a| a.contains("{schema_file}") || a.contains("{schema}"))
    }

    // -- output adapters ---------------------------------------------------

    pub fn extract(&self, stdout: &str) -> Result<String> {
        match self.spec.output {
            OutputMode::Text | OutputMode::Json => Ok(stdout.trim().to_string()),
            OutputMode::Jsonl => self.extract_jsonl(stdout),
        }
    }

    fn extract_jsonl(&self, stdout: &str) -> Result<String> {
        let mut messages: Vec<String> = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if !line.starts_with('{') {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if matches(&event, &self.spec.message_match) {
                if let Some(text) = dig(&event, self.spec.message_path.as_deref().unwrap_or("")) {
                    if let Some(text) = as_text(text) {
                        messages.push(text);
                    }
                }
            }
        }

        if messages.is_empty() {
            let reasons = self.error_events(stdout);
            if !reasons.is_empty() {
                // The CLI reported failure rather than answering badly, so this
                // is not something asking again corrects.
                return Err(SparError::call_failed(format!(
                    "agent '{}' failed: {}",
                    self.spec.name,
                    reasons.join("; ")
                )));
            }
        }
        Ok(messages.join("\n").trim().to_string())
    }

    /// Why an event stream says it failed, in words rather than as JSON.
    ///
    /// The reason a CLI gives is a field inside the event, not the event, and
    /// printing the object around it is what made a failure unreadable. Two
    /// shapes cover what the CLIs here emit: a `message` on the event, and a
    /// `message` on an `error` inside it.
    ///
    /// Deduplicated, because one refusal reported as an `error` twice and a
    /// `turn.failed` once is one reason and not three.
    fn error_events(&self, stdout: &str) -> Vec<String> {
        let mut reasons: Vec<String> = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if !line.starts_with('{') {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if !matches!(
                event.get("type").and_then(Value::as_str),
                Some("turn.failed") | Some("error")
            ) {
                continue;
            }
            let reason = dig(&event, "message")
                .or_else(|| dig(&event, "error.message"))
                .and_then(as_text)
                .unwrap_or_else(|| truncate(&event.to_string(), 400));
            if !reason.trim().is_empty() && !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
        reasons
    }

    /// Why the call failed, said in the agent's own terms.
    ///
    /// For a `jsonl` agent the streams are an event log, and `proc` tailing
    /// 1500 characters of one starts mid object: the reason is in there, after
    /// a thousand characters of whatever a tool call happened to return. The
    /// adapter already knows how to find the error events, so it finds them
    /// here too and the raw dump is what happens when there are none.
    ///
    /// stderr is kept either way. It is short, and it is where one CLI reports
    /// the condition that led to the refusal while the refusal itself goes to
    /// stdout.
    fn call_failure(&self, argv: &[String], out: &proc::Output) -> SparError {
        if self.spec.output != OutputMode::Jsonl {
            return SparError::call_failed(proc::failure_message(argv, out));
        }
        let reasons = self.error_events(&out.stdout);
        if reasons.is_empty() {
            return SparError::call_failed(proc::failure_message(argv, out));
        }
        let mut text = format!(
            "agent '{}' could not answer (exit {}): {}",
            self.spec.name,
            out.code,
            reasons.join("; ")
        );
        let stderr = out.stderr.trim();
        if !stderr.is_empty() {
            text.push_str(&format!("\n--- stderr ---\n{stderr}"));
        }
        text.push_str(&format!("\n--- command ---\n{}", proc::abbreviate(argv)));
        SparError::call_failed(text)
    }

    // -- the two operations everything else is built from -------------------

    pub fn ask(&self, prompt: &str, cwd: &Path, effort: Option<&str>) -> Result<String> {
        let prompt = &self.instructed(prompt);
        match self.ask_inner(prompt, cwd, effort, None, None) {
            Ok(text) => Ok(text),
            Err(e) => self.hand_over(e, |backup| backup.ask(prompt, cwd, None)),
        }
    }

    /// Give a failed call to the fallback, if there is one.
    ///
    /// Every failure qualifies, a deadline included. Asking the same CLI again
    /// after a timeout buys another wait of the same length for the same
    /// answer, which is why `ask_json` does not; asking a different CLI is a
    /// different question, and the alternative here is losing the run.
    ///
    /// The scheduled effort is deliberately not passed on. Effort words are
    /// each CLI's own vocabulary, and the one in hand belongs to the agent that
    /// just failed, so the fallback uses whatever its own config asked for.
    fn hand_over<T>(&self, primary: SparError, run: impl FnOnce(&Agent) -> Result<T>) -> Result<T> {
        let Some(backup) = self.fallback() else {
            return Err(primary);
        };
        logwarn!(
            "{} could not answer. Handing the call to {}.\n{primary}",
            self.name(),
            backup.name()
        );
        match run(backup) {
            Ok(answer) => {
                log!("{} answered in place of {}", backup.name(), self.name());
                Ok(answer)
            }
            // Both messages, primary first. The fallback's failure is usually
            // the less interesting of the two, and is often just "not
            // installed", which explains nothing about why the run stopped.
            Err(second) => Err(spar_err!(
                "agent '{}' failed and its fallback '{}' could not stand in.\n{}\n\n{}: {}",
                self.name(),
                backup.name(),
                primary.message(),
                backup.name(),
                second.message()
            )),
        }
    }

    fn ask_inner(
        &self,
        prompt: &str,
        cwd: &Path,
        effort: Option<&str>,
        schema_file: Option<&Path>,
        schema: Option<&str>,
    ) -> Result<String> {
        let body = match self.spec.system_via {
            SystemVia::Placeholder => prompt.to_string(),
            SystemVia::Prompt => format!("{STYLE_RULES}\n\n{prompt}"),
        };
        let values = Placeholders {
            prompt: Some(body),
            system: Some(STYLE_RULES.to_string()),
            model: self.spec.model.clone(),
            effort: effort
                .map(str::to_string)
                .or_else(|| self.spec.effort.clone()),
            cwd: Some(cwd.display().to_string()),
            schema_file: schema_file.map(|p| p.display().to_string()),
            schema: schema.map(str::to_string),
        };
        let argv = self.render(&values)?;
        // `check(false)` so the whole output is still in hand when the call
        // fails: `proc::run` would hand back a tail of it as a message, and a
        // tail of an event stream is the part this agent can read least.
        let opts = ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(self.spec.timeout)
            .check(false);
        let out = proc::exec(&argv, &opts)?;
        if !out.ok() {
            return Err(self.call_failure(&argv, &out));
        }
        self.extract(&out.stdout)
    }

    /// Structured output through the CLI's own mechanism when the template
    /// exposes one, otherwise by asking for JSON in the prompt and parsing it
    /// back out.
    pub fn ask_json<T: serde::de::DeserializeOwned>(
        &self,
        prompt: &str,
        schema: &Value,
        cwd: &Path,
        effort: Option<&str>,
    ) -> Result<T> {
        // Once here, not inside the retry, so the second ask carries the same
        // instructions as the first alongside the parser's complaint.
        let prompt = &self.instructed(prompt);
        match self.ask_json_retrying(prompt, schema, cwd, effort) {
            Ok(parsed) => Ok(parsed),
            Err(e) => self.hand_over(e, |backup| {
                backup.ask_json_retrying::<T>(prompt, schema, cwd, None)
            }),
        }
    }

    /// Whether to spend a second call on this same agent.
    ///
    /// The retry exists for an answer that arrived and could not be parsed.
    /// Models correct a shape error readily when told what was wrong, which is
    /// why the parser's own complaint goes back with the question.
    ///
    /// Two failures are not that. A deadline never is: the wait is the same
    /// length for the same answer. And a failure the CLI itself reported is not
    /// either, once there is a stand in to send the call to, because a
    /// different CLI is a different question while the same one twice is a
    /// refusal, a quota, or a crash repeated at full price. With no stand in
    /// configured the retry is the only thing left, so it still happens.
    fn worth_asking_again(&self, e: &SparError) -> bool {
        match e.kind() {
            ErrorKind::TimedOut => false,
            ErrorKind::UncertainWrite => false,
            ErrorKind::CallFailed => self.fallback().is_none(),
            ErrorKind::Other => true,
        }
    }

    /// The same question, asked at most twice of this agent alone.
    fn ask_json_retrying<T: serde::de::DeserializeOwned>(
        &self,
        prompt: &str,
        schema: &Value,
        cwd: &Path,
        effort: Option<&str>,
    ) -> Result<T> {
        // One retry, with the parser's own complaint handed back.
        //
        // A single malformed answer used to cost half a review: the other agent
        // carried on alone, which is the one thing this design exists to avoid.
        // Models correct a shape error readily when told what was wrong.
        const ATTEMPTS: usize = 2;
        let mut last: Option<SparError> = None;

        for attempt in 1..=ATTEMPTS {
            let asked = match &last {
                None => prompt.to_string(),
                Some(e) => format!(
                    "{prompt}\n\nYour previous answer could not be used: {}\nReturn the whole \
                     object this time, exactly matching the schema, and nothing else.",
                    e.first_line()
                ),
            };
            match self.ask_json_once::<T>(&asked, schema, cwd, effort) {
                Ok(parsed) => {
                    if attempt > 1 {
                        logdim!("{} answered on the retry", self.spec.name);
                    }
                    return Ok(parsed);
                }
                // A deadline is not a bad answer. Asking again buys another
                // wait of exactly the same length, which on a long review is
                // the most expensive way to learn nothing.
                Err(e) if !self.worth_asking_again(&e) => return Err(e),
                Err(e) => {
                    if attempt < ATTEMPTS {
                        // The whole error, not its first line. The first line is
                        // the command; the reason is in the stderr underneath
                        // it, and printing only the first line made a retry
                        // impossible to diagnose from the log.
                        logwarn!("{} failed, asking again.\n{e}", self.spec.name);
                    }
                    last = Some(e);
                }
            }
        }
        Err(spar_err!(
            "agent '{}' returned an unusable answer twice: {}",
            self.spec.name,
            last.expect("at least one attempt").message()
        ))
    }

    fn ask_json_once<T: serde::de::DeserializeOwned>(
        &self,
        prompt: &str,
        schema: &Value,
        cwd: &Path,
        effort: Option<&str>,
    ) -> Result<T> {
        let text = if self.supports_schema() {
            let inline = serde_json::to_string(schema).unwrap_or_default();
            let file = TempJson::write(schema)?;
            self.ask_inner(prompt, cwd, effort, Some(file.path()), Some(&inline))?
        } else {
            let full = format!(
                "{prompt}\n\n{JSON_INSTRUCTION}\n{}",
                serde_json::to_string_pretty(schema).unwrap_or_default()
            );
            self.ask_inner(&full, cwd, effort, None, None)?
        };
        jsonx::extract_into(&text)
    }

    /// Review the branch against `base`.
    ///
    /// Deliberately generic. `codex exec review` was tried and rejected: it
    /// refuses a custom prompt alongside `--base` and returns prose regardless
    /// of `--output-schema`, so it cannot yield a machine checkable verdict.
    /// Running inside the worktree is what makes an agent repo aware, not a
    /// subcommand.
    ///
    /// The call has write access, so the paragraph saying not to write is not
    /// decoration: an agent that commits while reviewing ends up holding the
    /// head it is about to be handed back, which is the one thing the
    /// alternating loop exists to prevent. `review::review_loop` rolls back
    /// what this asks for anyway, because a prompt is not a permission.
    pub fn review<T: serde::de::DeserializeOwned>(
        &self,
        base: &str,
        prompt: &str,
        schema: &Value,
        cwd: &Path,
        effort: Option<&str>,
    ) -> Result<T> {
        let scoped = format!(
            "{prompt}\n\nThe changes under review are the diff between `{base}` and HEAD in your \
             working directory. Inspect them with git, then read the surrounding code before \
             judging. Do not review only the diff.\n\nThis call is a review and nothing else. Do \
             not edit the code under review, do not commit, and do not push: somebody else acts \
             on what you find, and a reviewer that writes ends up reviewing its own work. Writing \
             a scratch file to check a claim is fine, and anything else you leave behind is rolled \
             back."
        );
        self.ask_json(&scoped, schema, cwd, effort)
    }
}

// ---------------------------------------------------------------------------
// Placeholders
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Placeholders {
    pub prompt: Option<String>,
    pub system: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    /// A path to the schema, for a CLI that reads one from disk.
    pub schema_file: Option<String>,
    /// The schema itself, for a CLI that takes it as an argument.
    pub schema: Option<String>,
}

impl Placeholders {
    fn get(&self, key: &str) -> Option<&str> {
        let value = match key {
            "prompt" => self.prompt.as_deref(),
            "system" => self.system.as_deref(),
            "model" => self.model.as_deref(),
            "effort" => self.effort.as_deref(),
            "cwd" => self.cwd.as_deref(),
            "schema_file" => self.schema_file.as_deref(),
            "schema" => self.schema.as_deref(),
            _ => None,
        };
        value.filter(|v| !v.is_empty())
    }

    /// Substitute every placeholder in one argument. `None` means a placeholder
    /// in this argument had no value, so the whole group is dropped.
    fn substitute(&self, arg: &str) -> Option<String> {
        const KEYS: [&str; 7] = [
            "prompt",
            "system",
            "model",
            "effort",
            "cwd",
            "schema_file",
            "schema",
        ];
        let mut out = arg.to_string();
        for key in KEYS {
            let token = format!("{{{key}}}");
            if out.contains(&token) {
                let value = self.get(key)?;
                out = out.replace(&token, value);
            }
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// JSONL helpers
// ---------------------------------------------------------------------------

/// Follow a dotted path, returning None if any hop is missing.
fn dig<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return None;
    }
    let mut node = value;
    for part in path.split('.') {
        node = node.as_object()?.get(part)?;
    }
    Some(node)
}

fn matches(event: &Value, wanted: &BTreeMap<String, String>) -> bool {
    if wanted.is_empty() {
        return false;
    }
    wanted
        .iter()
        .all(|(path, expected)| dig(event, path).and_then(Value::as_str) == Some(expected.as_str()))
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// Temporary schema file
// ---------------------------------------------------------------------------

/// A schema written somewhere the CLI can read it, removed when it goes out of
/// scope even if the agent call fails.
struct TempJson {
    path: PathBuf,
}

impl TempJson {
    fn write(value: &Value) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "spar-schema-{}-{nanos}-{unique}.json",
            std::process::id()
        ));
        std::fs::write(&path, serde_json::to_vec_pretty(value)?)
            .map_err(|e| spar_err!("could not write a schema file to {}: {e}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempJson {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------

/// Whether two paths name the same executable.
///
/// Comparing the raw strings misses aliases: a symlink or a hard link points at
/// the same binary under a different path, which would let two agents run the
/// identical CLI without tripping the warning below. Device and inode see
/// through both.
fn same_executable(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Two agents are only an independent review if they can actually disagree.
///
/// Config keys are arbitrary, so `alpha` and `beta` can both be Claude on the
/// same model. Compare what actually runs: the resolved binary and the
/// configured model, never the names.
pub fn correlation_warning(agents: &[Agent]) -> Option<String> {
    for i in 0..agents.len() {
        for j in (i + 1)..agents.len() {
            let (a, b) = (&agents[i], &agents[j]);
            let (Ok(pa), Ok(pb)) = (a.resolve_bin(), b.resolve_bin()) else {
                continue;
            };
            if !same_executable(pa, pb) || a.spec.model_key() != b.spec.model_key() {
                continue;
            }
            let model = if a.spec.model_key().is_empty() {
                "the CLI's default".to_string()
            } else {
                a.spec.model_key()
            };
            let where_at = if pa == pb {
                pa.display().to_string()
            } else {
                format!(
                    "the same executable ({} and {} are the same file)",
                    pa.display(),
                    pb.display()
                )
            };
            return Some(format!(
                "agents '{}' and '{}' both resolve to {where_at} at model {model}. Review \
                 findings will be correlated: the same model reviewing itself shares the blind \
                 spots of the model that wrote the code, so it is far less likely to catch what \
                 the implementer missed. That produces an approval indistinguishable from a real \
                 review, which is worse than no review at all. Give the two agents different \
                 CLIs or different models.",
                a.name(),
                b.name()
            ));
        }
    }
    None
}

/// Build every configured agent, resolving each binary up front so a missing
/// CLI fails before any model is billed.
pub fn build(cfg: &crate::config::Config) -> Result<Vec<Agent>> {
    let agents: Vec<Agent> = cfg
        .agents
        .iter()
        .cloned()
        .map(Agent::new)
        .map(|agent| agent.with_instructions(&cfg.loop_cfg.instructions))
        .collect();
    for agent in &agents {
        agent.resolve_bin()?;
        // A backup that is not installed must not stop a run whose pair is
        // fine. Said once here, at the start, rather than an hour in at the
        // moment it was needed and could not be reached.
        if let Some(backup) = agent.fallback() {
            if backup.resolve_bin().is_err() {
                logwarn!(
                    "{} has a fallback ({}) that is not installed, so it will not stand in",
                    agent.name(),
                    backup.program()
                );
            }
        }
    }
    Ok(agents)
}

/// Look an agent up by name in a built list.
pub fn find<'a>(agents: &'a [Agent], name: &str) -> Result<&'a Agent> {
    agents.iter().find(|a| a.name() == name).ok_or_else(|| {
        SparError::new(format!(
            "no agent named '{name}' ({})",
            agents
                .iter()
                .map(Agent::name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OutputMode, SystemVia};

    fn spec(command: Vec<CommandPart>) -> AgentSpec {
        AgentSpec {
            name: "test".into(),
            command,
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
        }
    }

    fn one(s: &str) -> CommandPart {
        CommandPart::One(s.into())
    }

    fn group(parts: &[&str]) -> CommandPart {
        CommandPart::Group(parts.iter().map(|s| s.to_string()).collect())
    }

    fn agent(command: Vec<CommandPart>) -> Agent {
        Agent::with_bin(spec(command), "/fake/bin")
    }

    fn values() -> Placeholders {
        Placeholders {
            prompt: Some("hi".into()),
            ..Default::default()
        }
    }

    // -- rendering -------------------------------------------------------

    #[test]
    fn placeholders_are_substituted() {
        let a = agent(vec![one("x"), group(&["-m", "{model}"]), one("{prompt}")]);
        let v = Placeholders {
            model: Some("m1".into()),
            ..values()
        };
        assert_eq!(vec!["/fake/bin", "-m", "m1", "hi"], a.render(&v).unwrap());
    }

    #[test]
    fn an_unset_placeholder_drops_the_whole_group() {
        let a = agent(vec![one("x"), group(&["-m", "{model}"]), one("{prompt}")]);
        assert_eq!(vec!["/fake/bin", "hi"], a.render(&values()).unwrap());
    }

    #[test]
    fn an_empty_string_drops_the_group_too() {
        let a = agent(vec![one("x"), group(&["-e", "{effort}"]), one("{prompt}")]);
        let v = Placeholders {
            effort: Some(String::new()),
            ..values()
        };
        assert_eq!(vec!["/fake/bin", "hi"], a.render(&v).unwrap());
    }

    #[test]
    fn a_bare_arg_with_an_unset_placeholder_drops() {
        let a = agent(vec![one("x"), one("{model}"), one("{prompt}")]);
        assert_eq!(vec!["/fake/bin", "hi"], a.render(&values()).unwrap());
    }

    #[test]
    fn literal_args_survive() {
        let a = agent(vec![
            one("x"),
            one("exec"),
            one("--json"),
            one("--"),
            one("{prompt}"),
        ]);
        assert_eq!(
            vec!["/fake/bin", "exec", "--json", "--", "hi"],
            a.render(&values()).unwrap()
        );
    }

    #[test]
    fn an_embedded_placeholder_substitutes_in_place() {
        let a = agent(vec![
            one("x"),
            group(&["-c", "model_reasoning_effort={effort}"]),
        ]);
        let v = Placeholders {
            effort: Some("ultra".into()),
            ..Default::default()
        };
        assert_eq!(
            vec!["/fake/bin", "-c", "model_reasoning_effort=ultra"],
            a.render(&v).unwrap()
        );
    }

    #[test]
    fn a_group_with_two_placeholders_needs_both() {
        let a = agent(vec![
            one("x"),
            group(&["--a", "{model}", "--b", "{effort}"]),
        ]);
        let v = Placeholders {
            model: Some("m".into()),
            ..Default::default()
        };
        assert_eq!(vec!["/fake/bin"], a.render(&v).unwrap());
    }

    #[test]
    fn supports_schema_detects_the_placeholder() {
        assert!(agent(vec![one("x"), group(&["--schema", "{schema_file}"])]).supports_schema());
        assert!(!agent(vec![one("x"), one("{prompt}")]).supports_schema());
    }

    // -- output adapters -------------------------------------------------

    #[test]
    fn text_passes_through_trimmed() {
        assert_eq!("hello", agent(vec![one("x")]).extract("  hello\n").unwrap());
    }

    #[test]
    fn jsonl_picks_the_matching_event() {
        let mut spec = spec(vec![one("x")]);
        spec.output = OutputMode::Jsonl;
        spec.message_path = Some("item.text".into());
        spec.message_match = BTreeMap::from([
            ("type".to_string(), "item.completed".to_string()),
            ("item.type".to_string(), "agent_message".to_string()),
        ]);
        let a = Agent::with_bin(spec, "/fake/bin");
        let stream = [
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"item.completed","item":{"type":"command_execution","text":"ls"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"the answer"}}"#,
            "not json at all",
        ]
        .join("\n");
        assert_eq!("the answer", a.extract(&stream).unwrap());
    }

    #[test]
    fn jsonl_raises_on_an_error_with_no_message() {
        let mut spec = spec(vec![one("x")]);
        spec.output = OutputMode::Jsonl;
        spec.message_path = Some("item.text".into());
        spec.message_match = BTreeMap::from([("type".into(), "item.completed".into())]);
        let a = Agent::with_bin(spec, "/fake/bin");
        assert!(a
            .extract(r#"{"type":"turn.failed","error":"boom"}"#)
            .is_err());
    }

    #[test]
    fn jsonl_joins_several_agent_messages() {
        let mut spec = spec(vec![one("x")]);
        spec.output = OutputMode::Jsonl;
        spec.message_path = Some("text".into());
        spec.message_match = BTreeMap::from([("type".into(), "msg".into())]);
        let a = Agent::with_bin(spec, "/fake/bin");
        let stream = "{\"type\":\"msg\",\"text\":\"one\"}\n{\"type\":\"msg\",\"text\":\"two\"}";
        assert_eq!("one\ntwo", a.extract(stream).unwrap());
    }

    #[test]
    fn dig_walks_a_dotted_path() {
        let v: Value = serde_json::from_str(r#"{"a":{"b":{"c":1}}}"#).unwrap();
        assert_eq!(Some(&Value::from(1)), dig(&v, "a.b.c"));
        assert_eq!(None, dig(&v, "a.b.missing"));
        assert_eq!(None, dig(&v, ""));
    }

    // -- binary resolution -----------------------------------------------

    #[test]
    fn a_missing_binary_lists_everywhere_it_looked() {
        let mut s = spec(vec![one("definitely-not-installed-xyz")]);
        s.search_paths = vec!["/nowhere/at/all".into()];
        s.name = "codex".into();
        let err = Agent::new(s).resolve_bin().unwrap_err().to_string();
        assert!(err.contains("definitely-not-installed-xyz (PATH)"), "{err}");
        assert!(
            err.contains("/nowhere/at/all/definitely-not-installed-xyz"),
            "{err}"
        );
        assert!(err.contains("SPAR_CODEX_BIN"), "{err}");
    }

    #[test]
    fn a_search_path_that_already_names_the_binary_is_used_as_is() {
        let dir = std::env::temp_dir().join(format!("spar-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mytool");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut s = spec(vec![one("mytool")]);
        s.search_paths = vec![bin.display().to_string()];
        assert_eq!(bin, Agent::new(s).resolve_bin().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- correlation -----------------------------------------------------

    fn named(name: &str, bin: &str, model: Option<&str>) -> Agent {
        let mut s = spec(vec![one("prog")]);
        s.name = name.into();
        s.model = model.map(str::to_string);
        Agent::with_bin(s, bin)
    }

    #[test]
    fn same_bin_same_model_warns() {
        let agents = vec![
            named("alpha", "/usr/local/bin/claude", Some("fable")),
            named("beta", "/usr/local/bin/claude", Some("fable")),
        ];
        let msg = correlation_warning(&agents).expect("should warn");
        assert!(msg.contains("alpha") && msg.contains("beta"), "{msg}");
    }

    #[test]
    fn different_model_does_not_warn() {
        let agents = vec![
            named("a", "/usr/local/bin/claude", Some("fable")),
            named("b", "/usr/local/bin/claude", Some("opus")),
        ];
        assert!(correlation_warning(&agents).is_none());
    }

    #[test]
    fn different_bin_does_not_warn() {
        let agents = vec![
            named("a", "/usr/local/bin/claude", Some("fable")),
            named("b", "/usr/local/bin/codex", Some("fable")),
        ];
        assert!(correlation_warning(&agents).is_none());
    }

    #[test]
    fn unset_and_empty_model_both_mean_the_default_and_warn() {
        let agents = vec![
            named("a", "/usr/local/bin/claude", None),
            named("b", "/usr/local/bin/claude", Some("")),
        ];
        let msg = correlation_warning(&agents).expect("should warn");
        assert!(msg.contains("the CLI's default"), "{msg}");
    }

    #[test]
    fn a_padded_model_still_warns() {
        let agents = vec![
            named("a", "/usr/local/bin/claude", Some("fable")),
            named("b", "/usr/local/bin/claude", Some(" fable ")),
        ];
        assert!(correlation_warning(&agents).is_some());
    }

    #[test]
    fn an_empty_model_against_a_named_one_does_not_warn() {
        let agents = vec![
            named("a", "/usr/local/bin/claude", Some("")),
            named("b", "/usr/local/bin/claude", Some("fable")),
        ];
        assert!(correlation_warning(&agents).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_binary_warns_and_names_both_paths() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("spar-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("claude");
        let link = dir.join("claude-alias");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let agents = vec![
            named("alpha", real.to_str().unwrap(), Some("fable")),
            named("beta", link.to_str().unwrap(), Some("fable")),
        ];
        let msg = correlation_warning(&agents).expect("should warn");
        assert!(msg.contains(real.to_str().unwrap()), "{msg}");
        assert!(msg.contains(link.to_str().unwrap()), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn two_distinct_real_binaries_stay_quiet() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("spar-distinct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for name in ["claude", "codex"] {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            paths.push(path);
        }
        let agents = vec![
            named("a", paths[0].to_str().unwrap(), Some("fable")),
            named("b", paths[1].to_str().unwrap(), Some("fable")),
        ];
        assert!(correlation_warning(&agents).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_style_rules_ask_for_brevity_and_no_attribution() {
        let lower = STYLE_RULES.to_lowercase();
        assert!(lower.contains("brief"));
        assert!(lower.contains("co-authored-by"));
        assert!(lower.contains("em-dash"));
    }

    /// Brevity was measured in sentences, and "one sentence beats one paragraph"
    /// is what a model satisfies by joining three facts with commas. A summary
    /// came back as a changelog line the reader had to decipher, which is
    /// shorter and worse.
    #[test]
    fn brevity_is_about_facts_per_sentence_not_sentence_count() {
        let lower = STYLE_RULES.to_lowercase();
        assert!(lower.contains("saying fewer things"), "{STYLE_RULES}");
        assert!(
            !lower.contains("one sentence beats one paragraph"),
            "the rule that produced the density is still there"
        );
    }

    /// The rules were scoped to what spar posts, so nothing had ever asked an
    /// agent for anything about the comments it writes in the code. A three
    /// line change came back under eight lines of comment, most of it the
    /// debugging story rather than the reason.
    #[test]
    fn the_style_rules_reach_the_code_and_not_only_what_is_posted() {
        let lower = STYLE_RULES.to_lowercase();
        assert!(lower.contains("comments in code you write"), "not in scope");
        assert!(
            lower.contains("comment code for the reason"),
            "no rule for it"
        );
    }

    // -- a failure said in the agent's own terms ---------------------------

    /// The event stream from the run that prompted this: a wall of file
    /// contents a tool call returned, with the reason as the last two lines.
    fn refusal_stream() -> String {
        let noise = "{\"type\":\"item.completed\",\"item\":{\"id\":\"i\",\"type\":\"command_execution\",\"output\":\"".to_string()
            + &"const x = 1;\\n".repeat(200)
            + "\"}}";
        [
            noise.as_str(),
            r#"{"type":"error","message":"This content was flagged for possible cybersecurity risk."}"#,
            r#"{"type":"error","message":"This content was flagged for possible cybersecurity risk."}"#,
            r#"{"type":"turn.failed","error":{"message":"This content was flagged for possible cybersecurity risk."}}"#,
        ]
        .join("\n")
    }

    fn jsonl_agent(name: &str) -> Agent {
        let mut spec = spec(vec![one("codex")]);
        spec.name = name.into();
        spec.output = OutputMode::Jsonl;
        spec.message_path = Some("item.text".into());
        Agent::with_bin(spec, "/fake/codex")
    }

    fn failed(stdout: &str, stderr: &str) -> proc::Output {
        proc::Output {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            code: 1,
        }
    }

    /// The reason a CLI gives is a field inside the event, not the event.
    /// Printing the object around it is what buried it.
    #[test]
    fn a_jsonl_failure_reports_the_reason_and_not_the_stream() {
        let agent = jsonl_agent("codex");
        let err = agent.call_failure(&["codex".to_string()], &failed(&refusal_stream(), ""));
        let text = err.message();
        assert!(
            text.contains("flagged for possible cybersecurity risk"),
            "{text}"
        );
        assert!(
            !text.contains("const x = 1;"),
            "the stream leaked in:\n{text}"
        );
        assert!(text.len() < 400, "still {} characters:\n{text}", text.len());
    }

    /// One refusal reported as two errors and a turn.failed is one reason.
    #[test]
    fn the_same_reason_reported_three_times_is_said_once() {
        let agent = jsonl_agent("codex");
        let err = agent.call_failure(&["codex".to_string()], &failed(&refusal_stream(), ""));
        assert_eq!(
            1,
            err.message().matches("flagged for possible").count(),
            "{}",
            err.message()
        );
    }

    /// stderr is where one CLI reports the condition behind the refusal, and it
    /// is short, so it survives whatever stdout turns out to hold.
    #[test]
    fn stderr_is_kept_because_it_is_where_the_other_half_arrives() {
        let agent = jsonl_agent("codex");
        let err = agent.call_failure(
            &["codex".to_string()],
            &failed(
                &refusal_stream(),
                "ERROR router: agent thread limit reached",
            ),
        );
        assert!(
            err.message().contains("agent thread limit reached"),
            "{}",
            err.message()
        );
    }

    /// A CLI that dies without emitting an error event leaves nothing else to
    /// go on, so the raw dump is still what happens.
    #[test]
    fn a_stream_with_no_error_event_falls_back_to_the_raw_output() {
        let agent = jsonl_agent("codex");
        let err = agent.call_failure(
            &["codex".to_string()],
            &failed("{\"type\":\"system\"}", "segmentation fault"),
        );
        assert!(
            err.message().contains("segmentation fault"),
            "{}",
            err.message()
        );
        assert!(
            err.message().starts_with("command failed"),
            "{}",
            err.message()
        );
    }

    /// A text agent has no events to read, so nothing changes for it.
    #[test]
    fn a_text_agent_is_reported_exactly_as_before() {
        let agent = agent(vec![one("mytool")]);
        let err = agent.call_failure(&["mytool".to_string()], &failed("some prose", "boom"));
        assert!(
            err.message().starts_with("command failed"),
            "{}",
            err.message()
        );
        assert!(err.message().contains("some prose"), "{}", err.message());
    }

    /// Whatever the shape, it is still the CLI failing rather than answering
    /// badly, so it still goes straight to the stand in.
    #[test]
    fn a_reworded_failure_is_still_a_failed_call() {
        let agent = jsonl_agent("codex");
        let err = agent.call_failure(&["codex".to_string()], &failed(&refusal_stream(), ""));
        assert_eq!(ErrorKind::CallFailed, err.kind());
    }

    // -- this run's own instructions --------------------------------------

    #[test]
    fn a_request_carries_the_instructions_after_the_task() {
        let agent = Agent::with_bin(shell("a", "true"), "/bin/sh")
            .with_instructions("Do not wait for CI. Pick it up next pass.");
        let asked = agent.instructed("Review the changes on this branch.");
        assert!(
            asked.starts_with("Review the changes on this branch."),
            "{asked}"
        );
        assert!(asked.contains("Do not wait for CI"), "{asked}");
    }

    /// A person adding an instruction should not be able to talk an agent out
    /// of the schema it was asked for, so where the instruction came from is
    /// said rather than left to read as part of the request.
    #[test]
    fn the_instructions_arrive_subordinate_to_the_request() {
        let agent = Agent::with_bin(shell("a", "true"), "/bin/sh").with_instructions("Be quick.");
        let asked = agent.instructed("Do the work.").to_lowercase();
        assert!(
            asked.contains("from the person who started this run"),
            "{asked}"
        );
        assert!(asked.contains("not the shape of your answer"), "{asked}");
    }

    #[test]
    fn nothing_is_added_when_there_are_none() {
        let agent = Agent::with_bin(shell("a", "true"), "/bin/sh");
        assert_eq!("Do the work.", agent.instructed("Do the work."));
        // Whitespace is not an instruction.
        let blank = Agent::with_bin(shell("b", "true"), "/bin/sh").with_instructions("   \n  ");
        assert_eq!("Do the work.", blank.instructed("Do the work."));
    }

    /// The stand in answers in this agent's place, so a run told not to wait on
    /// something must not start waiting the moment the primary hands over.
    #[test]
    fn the_stand_in_carries_them_too() {
        let agent = with_fallback(shell("primary", "true"), shell("backup", "true"))
            .with_instructions("Do not wait for CI.");
        let backup = agent.fallback().expect("a stand in");
        assert!(backup
            .instructed("Do the work.")
            .contains("Do not wait for CI."));
    }

    // -- fallback --------------------------------------------------------

    /// An agent whose command is a literal shell line, so a test can make the
    /// call succeed or fail on purpose.
    fn shell(name: &str, line: &str) -> AgentSpec {
        let mut spec = spec(vec![one("sh"), one("-c"), one(line)]);
        spec.name = name.into();
        spec
    }

    fn with_fallback(mut primary: AgentSpec, backup: AgentSpec) -> Agent {
        primary.fallback = Some(Box::new(backup));
        Agent::with_bin(primary, "/bin/sh")
    }

    #[test]
    fn a_failed_call_is_answered_by_the_fallback() {
        let agent = with_fallback(
            shell("primary", "echo refused >&2; exit 1"),
            shell("backup", "echo stood in"),
        );
        let answer = agent.ask("hi", Path::new("."), None).expect("fallback");
        assert_eq!("stood in", answer);
    }

    #[test]
    fn without_a_fallback_the_original_error_is_what_surfaces() {
        let agent = Agent::with_bin(shell("primary", "echo refused >&2; exit 1"), "/bin/sh");
        let err = agent
            .ask("hi", Path::new("."), None)
            .expect_err("no backup");
        assert!(err.message().contains("refused"), "{err}");
    }

    /// The reason the run stopped is the primary's, not the backup's, so it
    /// leads. A backup that is simply not installed explains nothing.
    #[test]
    fn both_failing_reports_the_primary_first() {
        let agent = with_fallback(
            shell("primary", "echo policy refusal >&2; exit 1"),
            shell("backup", "echo out of quota >&2; exit 1"),
        );
        let err = agent
            .ask("hi", Path::new("."), None)
            .expect_err("both fail");
        let text = err.message();
        let primary_at = text.find("policy refusal").expect("primary reason");
        let backup_at = text.find("out of quota").expect("backup reason");
        assert!(primary_at < backup_at, "{text}");
        assert!(
            text.contains("primary") && text.contains("backup"),
            "{text}"
        );
    }

    /// A counter file, so a test can say how many times the CLI was actually
    /// run rather than only what came back.
    fn attempts(name: &str) -> (PathBuf, String) {
        let path = std::env::temp_dir().join(format!("spar-attempts-{name}"));
        let _ = std::fs::remove_file(&path);
        let line = format!("echo x >> {}", path.display());
        (path, line)
    }

    fn counted(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    /// The failure that prompted this. A policy refusal came back twice, at
    /// full effort, before the stand in was given the call. The second was
    /// never going to be different: nothing about a refusal, a quota, or a
    /// crash is corrected by being asked the same thing again.
    #[test]
    fn a_cli_that_could_not_answer_is_not_asked_twice_when_there_is_a_stand_in() {
        let (path, count) = attempts("refused-with-standin");
        let agent = with_fallback(
            shell("primary", &format!("{count}; echo refused >&2; exit 1")),
            shell("backup", "echo '{}'"),
        );
        let answer: Value = agent
            .ask_json(
                "q",
                &serde_json::json!({"type": "object"}),
                Path::new("."),
                None,
            )
            .expect("the stand in answers");
        assert!(answer.is_object());
        assert_eq!(1, counted(&path), "the primary was asked more than once");
    }

    /// With nowhere to send the call, the retry is the only thing left, so it
    /// still happens. A transient failure is the case it was there for.
    #[test]
    fn with_no_stand_in_a_failed_call_is_still_retried() {
        let (path, count) = attempts("refused-alone");
        let agent = Agent::with_bin(
            shell("solo", &format!("{count}; echo refused >&2; exit 1")),
            "/bin/sh",
        );
        let err = agent
            .ask_json::<Value>(
                "q",
                &serde_json::json!({"type": "object"}),
                Path::new("."),
                None,
            )
            .expect_err("nothing answers");
        assert!(err.message().contains("twice"), "{err}");
        assert_eq!(2, counted(&path));
    }

    /// The retry that must survive. An answer that arrived and could not be
    /// parsed is exactly what it is for, and a model corrects a shape error
    /// readily when handed the parser's complaint.
    #[test]
    fn an_unusable_answer_is_still_worth_asking_again() {
        let (path, count) = attempts("unparsable");
        let agent = with_fallback(
            shell("primary", &format!("{count}; echo not json at all")),
            shell("backup", "echo '{}'"),
        );
        let answer: Value = agent
            .ask_json(
                "q",
                &serde_json::json!({"type": "object"}),
                Path::new("."),
                None,
            )
            .expect("the stand in answers in the end");
        assert!(answer.is_object());
        assert_eq!(2, counted(&path), "a shape error is worth one more ask");
    }

    /// A deadline is not worth asking the same CLI again, and `ask_json` does
    /// not. A different CLI is a different question, and the alternative is
    /// losing the run.
    #[test]
    fn a_timeout_still_reaches_the_fallback() {
        let mut primary = shell("primary", "sleep 30");
        primary.timeout = 1;
        let agent = with_fallback(primary, shell("backup", "echo stood in"));
        assert_eq!(
            "stood in",
            agent.ask("hi", Path::new("."), None).expect("fallback")
        );
    }

    /// The fallback is built with the agent, not looked up later, so a spec
    /// that carries one produces an agent that carries one.
    #[test]
    fn the_fallback_is_built_alongside_the_agent() {
        let mut primary = shell("primary", "true");
        primary.fallback = Some(Box::new(shell("backup", "true")));
        let agent = Agent::new(primary);
        assert_eq!(Some("backup"), agent.fallback().map(Agent::name));
        assert!(Agent::new(shell("solo", "true")).fallback().is_none());
    }
}

#[cfg(test)]
mod schema_placeholder_tests {
    use super::*;
    use crate::config::{OutputMode, SystemVia};

    fn spec_with(command: Vec<CommandPart>) -> AgentSpec {
        AgentSpec {
            name: "claude".into(),
            command,
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
        }
    }

    fn one(s: &str) -> CommandPart {
        CommandPart::One(s.into())
    }
    fn group(parts: &[&str]) -> CommandPart {
        CommandPart::Group(parts.iter().map(|s| s.to_string()).collect())
    }

    /// Claude Code takes the schema as an argument, not a path, so the file
    /// form alone was not enough to give it native structured output.
    #[test]
    fn either_schema_form_counts_as_native_support() {
        let inline = Agent::with_bin(
            spec_with(vec![one("x"), group(&["--json-schema", "{schema}"])]),
            "/b",
        );
        let byfile = Agent::with_bin(
            spec_with(vec![one("x"), group(&["--output-schema", "{schema_file}"])]),
            "/b",
        );
        let neither = Agent::with_bin(spec_with(vec![one("x"), one("{prompt}")]), "/b");
        assert!(inline.supports_schema());
        assert!(byfile.supports_schema());
        assert!(!neither.supports_schema());
    }

    #[test]
    fn the_inline_schema_is_substituted_whole() {
        let agent = Agent::with_bin(
            spec_with(vec![
                one("x"),
                group(&["--json-schema", "{schema}"]),
                one("{prompt}"),
            ]),
            "/b",
        );
        let values = Placeholders {
            prompt: Some("review it".into()),
            schema: Some(r#"{"type":"object"}"#.into()),
            ..Default::default()
        };
        assert_eq!(
            vec!["/b", "--json-schema", r#"{"type":"object"}"#, "review it"],
            agent.render(&values).unwrap()
        );
    }

    /// A call that wants prose back passes no schema, and the flag must go with
    /// it rather than being handed an empty string.
    #[test]
    fn the_schema_flag_drops_when_no_schema_is_wanted() {
        let agent = Agent::with_bin(
            spec_with(vec![
                one("x"),
                group(&["--json-schema", "{schema}"]),
                one("{prompt}"),
            ]),
            "/b",
        );
        let values = Placeholders {
            prompt: Some("implement it".into()),
            ..Default::default()
        };
        assert_eq!(vec!["/b", "implement it"], agent.render(&values).unwrap());
    }

    /// `{schema_file}` must not be mistaken for `{schema}`.
    #[test]
    fn the_two_schema_placeholders_do_not_collide() {
        let agent = Agent::with_bin(
            spec_with(vec![one("x"), group(&["--output-schema", "{schema_file}"])]),
            "/b",
        );
        let values = Placeholders {
            schema: Some("INLINE".into()),
            schema_file: Some("/tmp/s.json".into()),
            ..Default::default()
        };
        assert_eq!(
            vec!["/b", "--output-schema", "/tmp/s.json"],
            agent.render(&values).unwrap()
        );
    }

    /// The preset that was failing in the field.
    #[test]
    fn the_shipped_claude_preset_now_has_native_structured_output() {
        let raw = crate::config::load_preset("claude").unwrap();
        let table = raw.as_table().cloned().unwrap();
        let mut spec: AgentSpec = toml::Value::Table(table).try_into().unwrap();
        spec.name = "claude".into();
        assert!(
            Agent::with_bin(spec, "/b").supports_schema(),
            "without this a long review is parsed out of prose and truncates"
        );
    }
}
