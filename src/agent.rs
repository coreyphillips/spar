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
use crate::error::{Result, SparError};
use crate::jsonx;
use crate::proc::{self, ExecOpts};
use crate::{bail, spar_err};

/// Injected into every request. Prompting alone is not sufficient for any of
/// these, which is why each one is also enforced deterministically on the way
/// out, but a model that was asked produces less for the gate to fix.
pub const STYLE_RULES: &str = "\
Style rules for every artifact you produce (commits, PR titles, PR bodies, issue
titles, issue bodies, review comments):
- Never use em-dashes or en-dashes. Use commas, colons, or parentheses.
- Never mention Claude, Codex, OpenAI, ChatGPT, Anthropic, AI, or any tooling
  used to produce the work.
- Never add a Co-Authored-By trailer or a \"Generated with\" footer to commits.
- Be brief. A human engineer with other work has to read this. Lead with the
  point, cut the preamble, stop when you are done. Do not restate the task, do
  not announce what you are about to do, do not summarise what the diff already
  shows. One sentence beats one paragraph.
- No headings, bullet lists, or bold text in anything only a few sentences long.
Write as a human engineer would, because the reader neither knows nor cares what
produced the work.";

const JSON_INSTRUCTION: &str = "Respond with ONLY a JSON object matching this \
schema. No prose, no markdown fences, no commentary before or after:";

pub struct Agent {
    pub spec: AgentSpec,
    resolved: OnceLock<PathBuf>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} {}>", self.spec.name, self.spec.describe())
    }
}

impl Agent {
    pub fn new(spec: AgentSpec) -> Self {
        Self {
            spec,
            resolved: OnceLock::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.spec.name
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

        let env_key = format!(
            "SPAR_{}_BIN",
            self.spec.name.to_uppercase().replace('-', "_")
        );
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

    /// True when the template has somewhere to put a schema file, meaning the
    /// CLI can do structured output natively.
    pub fn supports_schema(&self) -> bool {
        self.spec
            .command
            .iter()
            .flat_map(|p| p.args())
            .any(|a| a.contains("{schema_file}"))
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
        let mut errors: Vec<String> = Vec::new();

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
            } else if matches!(
                event.get("type").and_then(Value::as_str),
                Some("turn.failed") | Some("error")
            ) {
                errors.push(truncate(&event.to_string(), 400));
            }
        }

        if messages.is_empty() && !errors.is_empty() {
            bail!("agent '{}' failed: {}", self.spec.name, errors.join("; "));
        }
        Ok(messages.join("\n").trim().to_string())
    }

    // -- the two operations everything else is built from -------------------

    pub fn ask(&self, prompt: &str, cwd: &Path, effort: Option<&str>) -> Result<String> {
        self.ask_inner(prompt, cwd, effort, None)
    }

    fn ask_inner(
        &self,
        prompt: &str,
        cwd: &Path,
        effort: Option<&str>,
        schema_file: Option<&Path>,
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
        };
        let argv = self.render(&values)?;
        let opts = ExecOpts::new().cwd(cwd).timeout_secs(self.spec.timeout);
        let stdout = proc::run(&argv, &opts)?;
        self.extract(&stdout)
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
        let text = if self.supports_schema() {
            let file = TempJson::write(schema)?;
            self.ask_inner(prompt, cwd, effort, Some(file.path()))?
        } else {
            let full = format!(
                "{prompt}\n\n{JSON_INSTRUCTION}\n{}",
                serde_json::to_string_pretty(schema).unwrap_or_default()
            );
            self.ask_inner(&full, cwd, effort, None)?
        };
        jsonx::extract_into(&text).map_err(|e| {
            spar_err!(
                "agent '{}' returned an unusable answer: {e}",
                self.spec.name
            )
        })
    }

    /// Review the branch against `base`.
    ///
    /// Deliberately generic. `codex exec review` was tried and rejected: it
    /// refuses a custom prompt alongside `--base` and returns prose regardless
    /// of `--output-schema`, so it cannot yield a machine checkable verdict.
    /// Running inside the worktree is what makes an agent repo aware, not a
    /// subcommand.
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
             judging. Do not review only the diff."
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
    pub schema_file: Option<String>,
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
            _ => None,
        };
        value.filter(|v| !v.is_empty())
    }

    /// Substitute every placeholder in one argument. `None` means a placeholder
    /// in this argument had no value, so the whole group is dropped.
    fn substitute(&self, arg: &str) -> Option<String> {
        const KEYS: [&str; 6] = ["prompt", "system", "model", "effort", "cwd", "schema_file"];
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
    let agents: Vec<Agent> = cfg.agents.iter().cloned().map(Agent::new).collect();
    for agent in &agents {
        agent.resolve_bin()?;
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
}
