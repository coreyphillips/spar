//! git and gh. Every outbound string passes through the style and concision
//! gates before it reaches GitHub.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::config::{Config, Drafts, Followups, StateStore};
use crate::error::Result;
use crate::model::{Followup, Issue, IssueRef, ItemKind, PersistedState, PrRef, PrRow, PrView};
use crate::proc::{self, ExecOpts};
use crate::style::{self, Style};
use crate::textsim;
use crate::{bail, logdim, spar_err};

/// gh returns newest first, so its `--limit` cannot be used to take the lowest
/// numbered items: it would slice the newest N and then sorting that slice
/// silently drops the older ones. Fetch a generous page, sort, then truncate.
pub const FETCH_CEILING: usize = 500;

/// An unclosed HTML comment on purpose. The payload is written after it and
/// terminated with `-->`, so GitHub renders the whole block as nothing.
pub const STATE_MARKER: &str = "<!-- spar:state";

/// An entry boundary in the local follow-up note, on the same principle as
/// `STATE_MARKER` and rendered as nothing for the same reason.
///
/// A follow-up's own sections are written as `## Problem` and friends, at the
/// same heading level as the entry's title, so the file's shape does not say
/// which of two `## ` lines starts an entry. This does. Files written before it
/// existed are still read, by the heuristic in `followups::parse`.
pub const FOLLOWUP_MARKER: &str = "<!-- spar:followup -->";

const WORKTREE_DIR: &str = ".spar-worktrees";
const STATE_DIR: &str = ".spar";

/// How many names one part of a split may be tried on before giving up. High
/// enough that nobody reaches it by splitting the same pull request again, low
/// enough that a repository where every name is taken says so rather than
/// looping.
const SPLIT_SLOTS: u32 = 20;

#[derive(Debug, Clone)]
pub struct SplitPushError {
    message: String,
    retain_worktree: bool,
}

impl SplitPushError {
    pub(crate) fn new(message: impl Into<String>, retain_worktree: bool) -> Self {
        Self {
            message: message.into(),
            retain_worktree,
        }
    }

    pub fn retain_worktree(&self) -> bool {
        self.retain_worktree
    }
}

impl std::fmt::Display for SplitPushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SplitPushError {}

/// The branch and worktree name for one part, on its `attempt`th name.
///
/// The unsuffixed name first, so the ordinary case reads as `split-12-1` and
/// only a repeat split carries a suffix.
fn split_slot(parent: i64, index: usize, attempt: u32) -> String {
    match attempt {
        1 => format!("split-{parent}-{index}"),
        n => format!("split-{parent}-{index}-{n}"),
    }
}

#[derive(Debug)]
pub struct Repo {
    root: PathBuf,
    pub style: Style,
    pub branch_prefix: String,
    pub state_store: StateStore,
    pub followups: Followups,
    pub drafts: Drafts,
    /// The login `gh` is authenticated as, asked at most once.
    ///
    /// `OnceLock` rather than `OnceCell` because `&Repo` crosses a
    /// `std::thread::scope` whenever both agents are asked at the same time,
    /// and only `OnceLock` is `Sync`.
    viewer: OnceLock<String>,
    /// Highest persisted checkpoint observed for each pull request.
    ///
    /// Kept in memory so a transient state read cannot reset the sequence
    /// after a resume already loaded a newer checkpoint.
    checkpoints: Mutex<BTreeMap<i64, u64>>,
    writes: WriteStats,
}

#[derive(Debug, Default)]
struct WriteStats {
    attempted: AtomicUsize,
    failed: AtomicUsize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriteSummary {
    pub(crate) attempted: usize,
    pub(crate) failed: usize,
}

impl WriteSummary {
    pub(crate) fn succeeded(self) -> usize {
        self.attempted.saturating_sub(self.failed)
    }
}

/// Ignored, untracked paths present before an editing call starts.
///
/// Existing build artifacts are deliberately part of the baseline. Callers can
/// therefore distinguish them from an ignored file the editing call created
/// and avoid deleting the latter as if no work had happened.
#[derive(Debug, Clone)]
pub(crate) struct WorktreeBaseline {
    attributes: AttributeState,
    ignored_untracked: IgnoredState,
    git_state: GitState,
}

/// The recorded Git state of a worktree that must remain read only.
///
/// Read-only agent calls are still ordinary processes. Capturing the complete
/// working state lets callers refuse to publish their answer or delete the
/// checkout if a call writes despite its instructions.
#[derive(Debug, Clone)]
pub(crate) struct WorktreeCheckpoint {
    path: PathBuf,
    attributes: AttributeState,
    git_state: GitState,
    ignored_untracked: IgnoredState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttributeState {
    files: BTreeMap<PathBuf, [u8; 32]>,
}

/// Exact paths and fingerprints for every untracked file, plus ignored paths.
///
/// Paths stay as operating-system strings so a non-UTF-8 filename cannot be
/// merged with another path by lossy command-output conversion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IgnoredState {
    files: BTreeMap<PathBuf, UntrackedFile>,
    ignored: BTreeSet<PathBuf>,
}

/// A bounded-cost fingerprint for an untracked filesystem entry.
///
/// Content hashing every ignored compiler artifact made each checkpoint read
/// gigabytes. File identity, type, size, timestamps, and mode detect ordinary
/// writes without rereading build output. Unix change time also changes when a
/// writer restores the modification time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UntrackedFile {
    kind: u8,
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    readonly: bool,
    symlink_target: Option<Vec<u8>>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GitState {
    repositories: BTreeMap<PathBuf, RepositoryState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositoryState {
    head: String,
    unsafe_index_flags: Vec<u8>,
    tracked: BTreeMap<PathBuf, TrackedEntry>,
    gitlinks: BTreeMap<PathBuf, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedEntry {
    index_mode: String,
    index_oid: String,
    worktree: Option<WorktreeFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeFile {
    mode: String,
    #[cfg(unix)]
    permissions: u32,
    raw_oid: String,
    fingerprint: [u8; 32],
    content: [u8; 32],
}

struct Gitlink {
    path: PathBuf,
    oid: String,
}

struct IndexEntry {
    path: PathBuf,
    mode: String,
    oid: String,
}

impl IgnoredState {
    fn is_ignored(&self, path: &Path) -> bool {
        self.ignored.contains(path)
    }

    fn changed_paths(&self, after: &Self) -> Vec<PathBuf> {
        let mut paths: BTreeSet<PathBuf> = self.files.keys().cloned().collect();
        paths.extend(after.files.keys().cloned());
        paths
            .into_iter()
            .filter(|path| self.files.get(path) != after.files.get(path))
            .collect()
    }

    fn changed_existing_paths(&self, after: &Self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|(path, state)| after.files.get(*path) != Some(*state))
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn new_ordinary_paths(&self, after: &Self) -> Vec<PathBuf> {
        after
            .files
            .keys()
            .filter(|path| !after.is_ignored(path) && !self.files.contains_key(*path))
            .cloned()
            .collect()
    }

    /// Whether anything moved here that is not recognized build or cache
    /// output.
    ///
    /// A read-only call is held to its word by comparing the worktree before
    /// and after. Verifying a finding usually means building the project and
    /// running its tests, which rewrites that output, and holding a reviewer to
    /// a byte-identical `dist/` threw away the review it had just done. What
    /// the call was asked to judge is the tracked tree, and that is compared as
    /// strictly as ever, along with every other untracked file.
    pub(crate) fn changed_beyond_generated(&self, after: &Self) -> bool {
        let mut paths: BTreeSet<&PathBuf> = self.files.keys().collect();
        paths.extend(after.files.keys());
        paths.into_iter().any(|path| {
            if self.files.get(path) == after.files.get(path)
                && self.is_ignored(path) == after.is_ignored(path)
            {
                return false;
            }
            !(is_generated_artifact(path) && self.disposable(path) && after.disposable(path))
        })
    }

    /// Whether this state has nothing at `path` worth keeping: either the path
    /// is not there at all, or it is there as an ignored file.
    fn disposable(&self, path: &Path) -> bool {
        !self.files.contains_key(path) || self.is_ignored(path)
    }
}

/// Build output one commit attempt let through, gathered for a single report.
///
/// The checks that allow it run more than once per attempt, before staging,
/// after staging, and again after the commit, because the tree could have moved
/// under any of them. Reporting from inside each check said the same thing
/// about the same files two and three times over.
#[derive(Default)]
struct GeneratedArtifacts {
    new_paths: BTreeSet<PathBuf>,
    changed_paths: BTreeSet<PathBuf>,
}

impl GeneratedArtifacts {
    fn left(&mut self, paths: Vec<PathBuf>) {
        self.new_paths.extend(paths);
    }

    fn changed(&mut self, paths: Vec<PathBuf>) {
        self.changed_paths.extend(paths);
    }

    /// Said once, and not as a warning. The files stay out of the commit,
    /// whatever wrote them writes them again, and they no longer keep the
    /// worktree from being removed.
    fn report(&self, cwd: &Path) {
        if !self.new_paths.is_empty() {
            logdim!(
                "the editing call left {} generated artifact(s) under a known build or cache \
                 directory in {}. They are not part of the commit.",
                self.new_paths.len(),
                cwd.display()
            );
        }
        if !self.changed_paths.is_empty() {
            logdim!(
                "the editing call changed {} existing generated artifact(s) under a known build \
                 or cache directory in {}. They are not part of the commit.",
                self.changed_paths.len(),
                cwd.display()
            );
        }
    }
}

/// Generated directories that test and build commands routinely create.
///
/// Files under these directories are never committed, and they do not keep a
/// worktree that is otherwise finished, so running the requested tests neither
/// stops the tracked change reaching review nor leaves a checkout behind.
fn is_generated_artifact(path: &Path) -> bool {
    const DIRECTORIES: &[&str] = &[
        "target",
        "dist",
        "node_modules",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".tox",
        ".nox",
        ".venv",
        "venv",
        ".gradle",
        ".build",
        "DerivedData",
        ".next",
        ".nuxt",
        ".svelte-kit",
        ".turbo",
        "coverage",
    ];
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        DIRECTORIES
            .iter()
            .any(|directory| name == OsStr::new(directory))
    })
}

fn merge_pr_args<'a>(
    number: &'a str,
    expected_head: Option<&'a str>,
    delete_branch: bool,
) -> Vec<&'a str> {
    let mut args = vec!["pr", "merge", number, "--squash"];
    if delete_branch {
        args.push("--delete-branch");
    }
    if let Some(expected_head) = expected_head {
        args.extend(["--match-head-commit", expected_head]);
    }
    args
}

fn reconcile_pr_creation(
    branch: &str,
    created: Result<String>,
    found: Result<Option<PrRef>>,
) -> Result<PrRef> {
    match (created, found) {
        (_, Ok(Some(pr))) => Ok(pr),
        (Ok(_), Ok(None)) => Err(crate::error::SparError::uncertain_write(format!(
            "PR creation reported success but none was found for {branch}"
        ))),
        (Err(create), Ok(None)) => Err(spar_err!(
            "could not open a PR for {branch}. {}",
            create.last_line()
        )),
        (Ok(_), Err(check)) => Err(crate::error::SparError::uncertain_write(format!(
            "PR creation reported success for {branch}, but it could not be verified. {}",
            check.last_line()
        ))),
        (Err(create), Err(check)) => Err(crate::error::SparError::uncertain_write(format!(
            "could not open a PR for {branch}. {} The result could not be verified: {}",
            create.last_line(),
            check.last_line()
        ))),
    }
}

fn pr_for_base(text: &str, branch: &str, base: &str) -> Result<Option<PrRef>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Row {
        number: i64,
        #[serde(default)]
        url: String,
        #[serde(default)]
        title: String,
        base_ref_name: String,
    }

    let rows = serde_json::from_str::<Vec<Row>>(text.trim()).map_err(|e| {
        spar_err!("unexpected pull request list for branch {branch} against {base}: {e}")
    })?;
    Ok(rows
        .into_iter()
        .find(|row| row.base_ref_name == base)
        .map(|row| PrRef {
            number: row.number,
            url: row.url,
            title: row.title,
        }))
}

fn has_exact_comment(comments: &[Value], body: &str) -> bool {
    comments.iter().any(|comment| {
        comment
            .get("body")
            .and_then(Value::as_str)
            .is_some_and(|seen| seen == body)
    })
}

fn reconcile_comment_post(
    number: i64,
    body: &str,
    post_error: crate::error::SparError,
    comments: Result<Vec<Value>>,
) -> Result<()> {
    match comments {
        Ok(comments) if has_exact_comment(&comments, body) => Ok(()),
        Ok(_) => Err(post_error),
        Err(read_error) => Err(crate::error::SparError::uncertain_write(format!(
            "could not comment on #{number}. {} The result could not be verified: {}",
            post_error.last_line(),
            read_error.last_line()
        ))),
    }
}

fn reconcile_issue_edit(
    number: i64,
    wanted: &str,
    edit_error: crate::error::SparError,
    observed: Result<String>,
) -> Result<()> {
    match observed {
        Ok(body) if body == wanted => Ok(()),
        Ok(_) => Err(spar_err!(
            "could not rewrite the body of #{number}. {}",
            edit_error.last_line()
        )),
        Err(read_error) => Err(crate::error::SparError::uncertain_write(format!(
            "could not rewrite the body of #{number}. {} The result could not be verified: {}",
            edit_error.last_line(),
            read_error.last_line()
        ))),
    }
}

fn issue_url_has_number(url: &str) -> bool {
    url.trim()
        .rsplit('/')
        .next()
        .and_then(|tail| tail.parse::<i64>().ok())
        .is_some_and(|number| number > 0)
}

fn reconcile_issue_creation(
    title: &str,
    created: Result<String>,
    found: Result<Option<ExistingIssue>>,
) -> Result<String> {
    match (created, found) {
        (Ok(url), _) if issue_url_has_number(&url) => Ok(url.trim().to_string()),
        (_, Ok(Some(issue))) => Ok(issue.url),
        (Ok(_), Ok(None)) => Err(crate::error::SparError::uncertain_write(format!(
            "issue creation reported success but no matching issue was found for {title:?}"
        ))),
        (Err(create), Ok(None)) => Err(spar_err!(
            "could not file issue {title:?}. {}",
            create.last_line()
        )),
        (Ok(_), Err(check)) => Err(crate::error::SparError::uncertain_write(format!(
            "issue creation reported success for {title:?}, but it could not be verified. {}",
            check.last_line()
        ))),
        (Err(create), Err(check)) => Err(crate::error::SparError::uncertain_write(format!(
            "could not file issue {title:?}. {} The result could not be verified: {}",
            create.last_line(),
            check.last_line()
        ))),
    }
}

fn remote_head_oid(output: &str, remote_ref: &str) -> Result<Option<String>> {
    if output.trim().is_empty() {
        return Ok(None);
    }
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let oid = fields.next().unwrap_or_default();
        let name = fields.next().unwrap_or_default();
        if name == remote_ref && !oid.is_empty() {
            return Ok(Some(oid.to_string()));
        }
    }
    Err(spar_err!(
        "origin returned an unexpected ref listing for {remote_ref}"
    ))
}

fn reconcile_failed_split_push(
    branch: &str,
    push_error: crate::error::SparError,
    local: Result<String>,
    remote: Result<String>,
) -> std::result::Result<(), SplitPushError> {
    let remote_ref = format!("refs/heads/{branch}");
    match (local, remote) {
        (Ok(local), Ok(remote)) => match remote_head_oid(&remote, &remote_ref) {
            Ok(Some(oid)) if oid == local.trim() => Ok(()),
            Ok(_) => Err(SplitPushError::new(
                format!(
                    "could not create origin/{branch}. {} The remote branch is absent or points \
                     somewhere else. Nothing was overwritten.",
                    push_error.last_line()
                ),
                false,
            )),
            Err(check) => Err(SplitPushError::new(
                format!(
                    "could not confirm whether origin/{branch} was created. {} The remote result \
                     could not be verified: {}",
                    push_error.last_line(),
                    check.last_line()
                ),
                true,
            )),
        },
        (local, remote) => {
            let check = match (local, remote) {
                (Err(local), Err(remote)) => format!(
                    "the local commit could not be read: {}; origin could not be read: {}",
                    local.last_line(),
                    remote.last_line()
                ),
                (Err(local), _) => {
                    format!("the local commit could not be read: {}", local.last_line())
                }
                (_, Err(remote)) => format!("origin could not be read: {}", remote.last_line()),
                _ => unreachable!(),
            };
            Err(SplitPushError::new(
                format!(
                    "could not confirm whether origin/{branch} was created. {} The result could \
                     not be verified because {check}",
                    push_error.last_line()
                ),
                true,
            ))
        }
    }
}

impl Repo {
    pub fn open(root: impl AsRef<Path>, cfg: &Config) -> Result<Self> {
        let root =
            std::fs::canonicalize(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf());
        // A linked worktree has a `.git` file rather than a directory, and a
        // bare-ish layout can have neither, so ask git instead of guessing.
        let inside = proc::run_str(
            &["git", "rev-parse", "--is-inside-work-tree"],
            &ExecOpts::new().cwd(&root).check(false).timeout_secs(30),
        )
        .unwrap_or_default();
        if inside.trim() != "true" {
            bail!("not a git repository: {}", root.display());
        }
        let repo = Self {
            root,
            style: cfg.style.clone(),
            branch_prefix: cfg.loop_cfg.branch_prefix.clone(),
            state_store: cfg.loop_cfg.state_store,
            followups: cfg.loop_cfg.followups,
            drafts: cfg.loop_cfg.drafts,
            viewer: OnceLock::new(),
            checkpoints: Mutex::new(BTreeMap::new()),
            writes: WriteStats::default(),
        };
        repo.self_exclude();
        Ok(repo)
    }

    /// Keep spar's own scratch directories out of the target repo's
    /// `git status`.
    ///
    /// Written to `.git/info/exclude`, never to a tracked `.gitignore`: this is
    /// somebody else's repository and spar has no business committing to it.
    /// Best effort and silent on failure, because a read-only git directory is
    /// not a reason to abandon a run.
    fn self_exclude(&self) {
        let git_dir = self.git_try(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
        let git_dir = git_dir.trim();
        if git_dir.is_empty() {
            return;
        }
        let path = Path::new(git_dir).join("info").join("exclude");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();

        let wanted = [format!("/{WORKTREE_DIR}/"), format!("/{STATE_DIR}/")];
        let missing: Vec<&String> = wanted
            .iter()
            .filter(|line| !existing.lines().any(|l| l.trim() == line.as_str()))
            .collect();
        if missing.is_empty() {
            return;
        }

        use std::io::Write;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut block = String::new();
        if !existing.is_empty() && !existing.ends_with('\n') {
            block.push('\n');
        }
        block.push_str("\n# added by spar: its worktrees and run state\n");
        for line in missing {
            block.push_str(line);
            block.push('\n');
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(block.as_bytes());
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn write_summary(&self) -> WriteSummary {
        WriteSummary {
            attempted: self.writes.attempted.load(Ordering::Relaxed),
            failed: self.writes.failed.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn record_write<T, E>(
        &self,
        result: std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.record_write_outcome(result.is_err());
        result
    }

    pub(crate) fn record_failed_write<T, E>(
        &self,
        result: std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if result.is_err() {
            self.record_write_outcome(true);
        }
        result
    }

    fn record_write_outcome(&self, failed: bool) {
        self.writes.attempted.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.writes.failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    // -- gates ------------------------------------------------------------

    /// Scrub, then verify. A leak here reaches GitHub, so it is a hard error
    /// rather than a warning: silent partial compliance is how a style rule
    /// erodes over a long run.
    pub fn clean(&self, text: &str) -> Result<String> {
        let out = style::scrub(text, &self.style);
        let bad = style::violations(&out, &self.style);
        if !bad.is_empty() {
            bail!(
                "style gate could not clean text ({}): {}",
                bad.join(", "),
                style::clip(&out, 300)
            );
        }
        Ok(out)
    }

    /// Clean, and hold to a length budget. For anything a model wrote.
    pub fn clean_body(&self, text: &str) -> Result<String> {
        self.clean(&style::body(text, &self.style))
    }

    /// The same, with an issue's far larger budget and its exemption for code.
    pub fn clean_issue_body(&self, text: &str) -> Result<String> {
        self.clean(&style::issue_body(text, &self.style))
    }

    /// The single transform every outbound title goes through.
    ///
    /// Scrub first, clip second, and never the other way round. Clipping first
    /// lets the scrub lengthen the result past the budget (an em dash becomes
    /// two characters), so a second pass would clip again and produce a
    /// different string. That broke follow-up deduplication silently: the
    /// lookup searched for one title while GitHub had stored another, no match
    /// was ever found, and a fresh duplicate issue was filed every review
    /// round. Doing it in this order makes the transform idempotent, which the
    /// tests assert.
    pub fn clean_title(&self, text: &str) -> Result<String> {
        Ok(style::title(&self.clean(text)?, &self.style))
    }

    pub(crate) fn clean_nonempty_title_for_write(&self, text: &str) -> Result<String> {
        let title = self.record_failed_write(self.clean_title(text))?;
        if title.trim().is_empty() {
            return self.record_failed_write(Err(spar_err!(
                "nothing left of the title after cleaning it"
            )));
        }
        Ok(title)
    }

    pub(crate) fn clean_followup_title(&self, text: &str) -> Result<String> {
        if self.followups == Followups::Issues {
            self.clean_nonempty_title_for_write(text)
        } else {
            self.clean_title(text)
        }
    }

    // -- git --------------------------------------------------------------

    fn git_opts(&self, cwd: Option<&Path>, check: bool) -> ExecOpts {
        ExecOpts::new()
            .cwd(cwd.unwrap_or(&self.root))
            .check(check)
            .timeout_secs(600)
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        self.git_at(None, args)
    }

    pub fn git_at(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let argv = git_without_maintenance_argv(args);
        proc::run(&argv, &self.git_opts(cwd, true))
    }

    /// Run a parent-side Git operation without inherited background helpers.
    ///
    /// Editing calls are untrusted. Once one returns, status, staging, and
    /// committing happen in this process, so an inherited fsmonitor or automatic
    /// maintenance command must not become a way to execute outside its sandbox.
    fn git_at_without_automation(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let argv = git_without_automation_argv(args);
        proc::run(
            &argv,
            &self.git_opts(Some(cwd), true).stop_descendants(true),
        )
    }

    fn git_try_without_automation(&self, args: &[&str]) -> Result<bool> {
        let argv = git_without_automation_argv(args);
        proc::exec(&argv, &self.git_opts(None, false).stop_descendants(true))
            .map(|output| output.ok())
    }

    /// Run git, tolerating failure. Returns whatever landed on stdout.
    pub fn git_try(&self, args: &[&str]) -> String {
        self.git_try_at(None, args)
    }

    pub fn git_try_at(&self, cwd: Option<&Path>, args: &[&str]) -> String {
        let argv = git_without_maintenance_argv(args);
        proc::run(&argv, &self.git_opts(cwd, false)).unwrap_or_default()
    }

    /// The base branch the remote actually points at, rather than assuming
    /// `main`. Falls back to the configured value when there is no origin.
    pub fn default_branch(&self, configured: &str) -> String {
        let refname = self.git_try(&["symbolic-ref", "refs/remotes/origin/HEAD"]);
        match refname.trim().rsplit('/').next() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => configured.to_string(),
        }
    }

    // -- branch naming and ownership --------------------------------------
    //
    // Branch names default to `issue-N`, which is exactly what a person would
    // name a branch by hand. Ownership therefore cannot be inferred from the
    // name, so every branch spar creates is recorded and cleanup only ever
    // touches what is in that record.

    pub fn branch_for_issue(&self, issue: i64) -> String {
        format!("{}issue-{issue}", self.branch_prefix)
    }

    pub fn branch_for_pr(&self, number: i64) -> String {
        format!("{}pr-{number}", self.branch_prefix)
    }

    /// One part of a split, numbered from 1 within its parent.
    ///
    /// Its own namespace rather than `issue-N`, because the parts of a split
    /// pull request have no issue of their own and would otherwise collide with
    /// the branch of the issue that happens to share the parent's number.
    ///
    /// The name a part is tried on first. `worktree_for_split` may end up on a
    /// suffixed one, because this name is not free forever.
    pub fn branch_for_split(&self, parent: i64, index: usize) -> String {
        format!("{}{}", self.branch_prefix, split_slot(parent, index, 1))
    }

    fn ledger_path(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("branches.json")
    }

    pub fn known_branches(&self) -> BTreeMap<String, BranchRecord> {
        std::fs::read_to_string(self.ledger_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn record_branch(&self, branch: &str, kind: &str, number: i64) {
        let mut data = self.known_branches();
        data.insert(
            branch.to_string(),
            BranchRecord {
                kind: kind.to_string(),
                number,
            },
        );
        if let Err(e) = write_json_atomic(&self.ledger_path(), &data) {
            logdim!("could not record branch {branch}: {e}");
        }
    }

    pub fn forget_branch(&self, branch: &str) {
        let mut data = self.known_branches();
        if data.remove(branch).is_none() {
            return;
        }
        if let Err(e) = write_json_atomic(&self.ledger_path(), &data) {
            logdim!("could not update the branch record: {e}");
        }
    }

    // -- worktrees --------------------------------------------------------

    fn worktree_path(&self, name: &str) -> PathBuf {
        self.root.join(WORKTREE_DIR).join(name)
    }

    /// Isolate an issue so a failed run cannot poison the next one's base.
    pub fn worktree_add(&self, issue: i64, base: &str) -> Result<(PathBuf, String)> {
        let branch = self.branch_for_issue(issue);
        let path = self.worktree_path(&format!("issue-{issue}"));

        self.refuse_issue_branch_rebuild(issue, base)?;
        self.refuse_dirty_worktree(&path, &format!("worktree for issue #{issue}"))?;

        if !self.branch_deletion_is_safe(&branch)? {
            bail!(
                "the existing branch {branch} has a tip or reflog-only commit that no surviving \
                 ref preserves. Rebuilding it would delete recovery history. Inspect the branch \
                 before retrying."
            );
        }

        if !self.remove_worktree_at(&path)? {
            bail!(
                "the existing worktree for issue #{issue} could not be removed safely. Its \
                 branch was kept."
            );
        }
        if !self.delete_branch_if_safe(&branch)? {
            bail!(
                "the existing branch {branch} changed or remained checked out while its \
                 worktree was being rebuilt. It was kept."
            );
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| spar_err!("could not create {}: {e}", parent.display()))?;
        }

        let path_str = path.display().to_string();
        let remote_start = format!("origin/{base}");
        let created = self
            .git(&["worktree", "add", "-b", &branch, &path_str, &remote_start])
            .or_else(|_| self.git(&["worktree", "add", "-b", &branch, &path_str, base]));

        // Recorded on both paths: an unrecorded branch is one cleanup will
        // never remove, and the fallback creates a branch just the same.
        created.map_err(|e| {
            spar_err!(
                "could not create a worktree for issue #{issue}. {}\nIs `{base}` a real branch, \
                 and does `origin` exist?",
                e.last_line()
            )
        })?;
        self.record_branch(&branch, "issue", issue);
        Ok((path, branch))
    }

    /// Refuse to reset the local or remote branch assigned to an issue when it
    /// carries work that no pull request preserves.
    ///
    /// Both linked-worktree mode and shared-checkout mode rebuild the same
    /// branch name. Keeping the guard here prevents either path from silently
    /// replacing recovery commits left by an earlier run.
    pub(crate) fn refuse_issue_branch_rebuild(&self, issue: i64, base: &str) -> Result<()> {
        let branch = self.branch_for_issue(issue);
        let base_remote_ref = format!("refs/heads/{base}");
        let base_tracking_ref = format!("refs/remotes/origin/{base}");
        let base_refspec = format!("+{base_remote_ref}:{base_tracking_ref}");
        self.git(&["fetch", "--no-tags", "origin", &base_refspec])
            .map_err(|e| {
                spar_err!(
                    "could not refresh origin/{base} before checking issue #{issue}: {}",
                    e.last_line()
                )
            })?;

        if let Some(remote_ref) = self.refresh_issue_remote_ref(&branch)? {
            let ahead = self.commit_count_checked(&self.root, &remote_ref, base)?;
            if ahead > 0 && !self.pull_request_holds(&branch, &remote_ref, base) {
                bail!(
                    "origin/{branch} already has {ahead} commit(s) that are not on {base}, and no \
                     pull request accounts for them. Rebuilding it would force push over that \
                     work.\nOpen a pull request for the branch and run `spar resume <pr>` to continue \
                     it, or delete it with `git push origin --delete {branch}` if the remote \
                     branch is no longer needed."
                );
            }
        }

        let local_ref = format!("refs/heads/{branch}");
        if self.exact_ref_exists_checked(&self.root, &local_ref)? {
            let ahead = self.commit_count_checked(&self.root, &local_ref, base)?;
            let recorded_pr = self
                .known_branches()
                .get(&branch)
                .is_some_and(|record| record.kind == "pr");
            let preserved = ahead == 0
                || if recorded_pr {
                    self.local_branch_is_preserved(&branch)?
                } else {
                    self.pull_request_holds(&branch, &local_ref, base)
                };
            if !preserved {
                let listed = self
                    .commit_lines(&self.root, &local_ref, base)
                    .iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!(
                    "the local branch {branch} has {ahead} commit(s) that are not on {base}, and \
                     no pull request preserves them. Rebuilding it would delete the only copy.\n\
                     {listed}\nPush it and run `spar resume <pr>` on the pull request to continue \
                     it, or delete it with `git branch -D {branch}` if it is stale."
                );
            }
        }
        Ok(())
    }

    fn refresh_issue_remote_ref(&self, branch: &str) -> Result<Option<String>> {
        let live_ref = format!("refs/heads/{branch}");
        let tracking_ref = format!("refs/remotes/origin/{branch}");
        let listed = self
            .git(&["ls-remote", "--heads", "origin", &live_ref])
            .map_err(|e| {
                spar_err!(
                    "could not verify whether origin/{branch} still exists: {}",
                    e.last_line()
                )
            })?;

        if remote_head_oid(&listed, &live_ref)?.is_some() {
            let refspec = format!("+{live_ref}:{tracking_ref}");
            self.git(&["fetch", "--no-tags", "origin", &refspec])
                .map_err(|e| {
                    spar_err!(
                        "origin/{branch} exists but its tracking ref could not be refreshed: {}",
                        e.last_line()
                    )
                })?;
            if !self.exact_ref_exists_checked(&self.root, &tracking_ref)? {
                bail!("origin/{branch} was fetched but its tracking ref is missing");
            }
            return Ok(Some(tracking_ref));
        }

        if !self.exact_ref_exists_checked(&self.root, &tracking_ref)? {
            return Ok(None);
        }
        let expected = self
            .git_at(Some(&self.root), &["rev-parse", "--verify", &tracking_ref])?
            .trim()
            .to_string();
        self.git_at_without_automation(&self.root, &["update-ref", "-d", &tracking_ref, &expected])
            .map_err(|e| {
                spar_err!(
                    "could not discard stale origin/{branch} tracking ref safely: {}",
                    e.last_line()
                )
            })?;
        if self.exact_ref_exists_checked(&self.root, &tracking_ref)? {
            bail!(
                "origin/{branch} changed while its stale tracking ref was being removed. It was \
                 kept."
            );
        }
        Ok(None)
    }

    /// Whether a pull request from `branch` already holds every commit `refname`
    /// has beyond `base`.
    ///
    /// GitHub serves `refs/pull/N/head` for as long as the repository lives, so
    /// commits that reached a pull request outlive the branch they were pushed
    /// from. A matching branch name does not establish that on its own: an
    /// issue worked twice reuses the name, and the merged pull request from the
    /// first round says nothing about where the second round's commits are.
    fn pull_request_holds(&self, branch: &str, refname: &str, base: &str) -> bool {
        self.prs_for_branch(branch)
            .iter()
            .any(|pr| self.pr_head_holds(pr.number, refname, base))
    }

    fn pr_head_holds(&self, number: i64, refname: &str, base: &str) -> bool {
        let head = format!("refs/spar/pr-head/{number}");
        let refspec = format!("+refs/pull/{number}/head:{head}");
        if self.git(&["fetch", "origin", &refspec]).is_err() {
            return false;
        }
        let held = self.commits_held_by(refname, base, &head);
        self.git_try(&["update-ref", "-d", &head]);
        held
    }

    pub(crate) fn is_ancestor_checked(&self, cwd: &Path, older: &str, newer: &str) -> Result<bool> {
        let argv = vec![
            "git".to_string(),
            "merge-base".to_string(),
            "--is-ancestor".to_string(),
            older.to_string(),
            newer.to_string(),
        ];
        let out = proc::exec(&argv, &self.git_opts(Some(cwd), false))?;
        match out.code {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(spar_err!("{}", proc::failure_message(&argv, &out))),
        }
    }

    fn pr_head_contains_checked(&self, number: i64, branch_ref: &str) -> Result<bool> {
        let head = format!("refs/spar/pr-head/{number}");
        let refspec = format!("+refs/pull/{number}/head:{head}");
        self.git(&["fetch", "origin", &refspec]).map_err(|e| {
            spar_err!(
                "could not verify the immutable head of PR #{number}: {}",
                e.last_line()
            )
        })?;
        let held = self.is_ancestor_checked(&self.root, branch_ref, &head);
        self.git_try(&["update-ref", "-d", &head]);
        held
    }

    fn branch_prs_checked(&self, branch: &str) -> Result<Vec<PrRef>> {
        let text = self.gh(&[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,url,title",
        ])?;
        serde_json::from_str(text.trim())
            .map_err(|e| spar_err!("could not read pull requests for {branch}: {e}"))
    }

    fn branch_is_preserved_checked(&self, branch: &str, record: &BranchRecord) -> Result<bool> {
        let branch_ref = format!("refs/heads/{branch}");
        if record.kind == "pr" {
            return self.pr_head_contains_checked(record.number, &branch_ref);
        }
        let prs = self.branch_prs_checked(branch)?;
        if prs.is_empty() {
            return Ok(false);
        }
        for pr in prs {
            if self.pr_head_contains_checked(pr.number, &branch_ref)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn branch_deletion_is_safe(&self, branch: &str) -> Result<bool> {
        let local_ref = format!("refs/heads/{branch}");
        if !self.exact_ref_exists_checked(&self.root, &local_ref)? {
            return Ok(true);
        }
        let oid = self
            .git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref])?
            .trim()
            .to_string();
        let mut durable_tip = commit_has_shared_ref_except(&self.root, &oid, Some(&local_ref))?;
        if !durable_tip {
            let remote_ref = format!("refs/heads/{branch}");
            let remote = self.git(&["ls-remote", "--heads", "origin", &remote_ref])?;
            durable_tip = remote.lines().any(|line| {
                line.split_whitespace()
                    .next()
                    .is_some_and(|remote_oid| remote_oid == oid)
            });
        }
        if !durable_tip {
            if let Some(record) = self.known_branches().get(branch) {
                durable_tip = self.branch_is_preserved_checked(branch, record)?;
            }
        }
        if !durable_tip {
            return Ok(false);
        }
        ref_reflog_is_preserved(&self.root, &local_ref, &oid)
    }

    /// Delete a branch only if its exact current tip and reflog are still safe.
    /// The expected old value makes a concurrent ref update fail instead of
    /// deleting work that appeared after the preservation check.
    fn delete_branch_if_safe(&self, branch: &str) -> Result<bool> {
        let local_ref = format!("refs/heads/{branch}");
        if !self.exact_ref_exists_checked(&self.root, &local_ref)? {
            return Ok(true);
        }
        let expected = self
            .git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref])?
            .trim()
            .to_string();
        if !self.branch_deletion_is_safe(branch)? {
            return Ok(false);
        }
        let checked_out = self
            .git_at(Some(&self.root), &["worktree", "list", "--porcelain"])?
            .lines()
            .any(|line| line == format!("branch {local_ref}"));
        if checked_out {
            return Ok(false);
        }
        self.git_at_without_automation(&self.root, &["update-ref", "-d", &local_ref, &expected])?;
        Ok(!self.exact_ref_exists_checked(&self.root, &local_ref)?)
    }

    fn review_ref_deletion_is_safe(&self, number: i64) -> Result<bool> {
        let local_ref = review_ref(number);
        if !self.exact_ref_exists_checked(&self.root, &local_ref)? {
            return Ok(true);
        }
        let oid = self
            .git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref])?
            .trim()
            .to_string();
        if !self.pr_head_contains_checked(number, &local_ref)? {
            return Ok(false);
        }
        ref_reflog_is_preserved(&self.root, &local_ref, &oid)
    }

    fn delete_review_ref_if_safe(&self, number: i64) -> Result<bool> {
        let local_ref = review_ref(number);
        if !self.exact_ref_exists_checked(&self.root, &local_ref)? {
            return Ok(true);
        }
        let expected = self
            .git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref])?
            .trim()
            .to_string();
        if !self.review_ref_deletion_is_safe(number)? {
            return Ok(false);
        }
        self.git_at_without_automation(&self.root, &["update-ref", "-d", &local_ref, &expected])?;
        Ok(!self.exact_ref_exists_checked(&self.root, &local_ref)?)
    }

    /// Whether `other` already contains every commit `branch` has beyond
    /// `base`. False when either ref fails to resolve, so a ref that is not
    /// there cannot vouch for anything.
    pub fn commits_held_by(&self, branch: &str, base: &str, other: &str) -> bool {
        let range = format!("{}..{branch}", self.base_ref(&self.root, base));
        self.git_try(&["rev-list", "--count", &range, "--not", other])
            .trim()
            == "0"
    }

    pub fn worktree_remove(&self, issue: i64) -> bool {
        let path = self.worktree_path(&format!("issue-{issue}"));
        match self.remove_worktree_at(&path) {
            Ok(removed) => removed,
            Err(error) => {
                logdim!(
                    "kept {} because removal did not reach a confirmed quiet point: {}",
                    path.display(),
                    error.last_line()
                );
                false
            }
        }
    }

    /// Verify both the common Git directory and the worktree top level.
    ///
    /// A stale worktree entry is not ownership proof. An unrelated repository
    /// can later occupy the same path and must survive cleanup.
    fn worktree_belongs_to_repo(&self, path: &Path) -> Result<bool> {
        let wanted = std::fs::canonicalize(path)
            .map_err(|e| spar_err!("could not resolve {}: {e}", path.display()))?;
        // Every SPAR worktree path is built from the canonical repository root.
        // A different canonical path therefore means the final component or
        // one of its parents is a symlink. Passing that alias to `git worktree
        // remove` can delete the worktree at its real target.
        if wanted != path {
            return Ok(false);
        }
        let resolve = |cwd: &Path, value: &str| -> Result<PathBuf> {
            let raw = PathBuf::from(value.trim());
            let joined = if raw.is_absolute() {
                raw
            } else {
                cwd.join(raw)
            };
            std::fs::canonicalize(&joined)
                .map_err(|e| spar_err!("could not resolve {}: {e}", joined.display()))
        };
        let expected =
            self.git_at_without_automation(&self.root, &["rev-parse", "--git-common-dir"])?;
        let actual = self.git_at_without_automation(path, &["rev-parse", "--git-common-dir"])?;
        let top = self.git_at_without_automation(path, &["rev-parse", "--show-toplevel"])?;
        let expected = resolve(&self.root, &expected)?;
        let actual = resolve(path, &actual)?;
        let top = resolve(path, &top)?;
        Ok(expected == actual && top == wanted)
    }

    /// Remove only a worktree that belongs to this repository.
    ///
    /// The path sits under a predictable directory, but that does not establish
    /// ownership. A clean independent repository at the same path must survive
    /// even when `git worktree remove` rejects it.
    fn remove_worktree_at_with_force(&self, path: &Path, force: bool) -> Result<bool> {
        let existed = path.exists();
        if path.exists() {
            match self.worktree_belongs_to_repo(path) {
                Ok(true) => {}
                Ok(false) => {
                    logdim!(
                        "kept {} because it is not a worktree owned by this repository",
                        path.display()
                    );
                    return Ok(false);
                }
                Err(e) => {
                    logdim!(
                        "kept {} because its worktree ownership could not be verified: {}",
                        path.display(),
                        e.last_line()
                    );
                    return Ok(false);
                }
            }
            if !force {
                match self.has_recoverable_work(path) {
                    Ok(true) => {
                        logdim!(
                            "kept {} because it contains recoverable files or repository state",
                            path.display()
                        );
                        return Ok(false);
                    }
                    Err(e) => {
                        logdim!(
                            "kept {} because its recoverable state could not be checked: {}",
                            path.display(),
                            e.last_line()
                        );
                        return Ok(false);
                    }
                    Ok(false) => {}
                }
            }
        }
        let path_str = path.display().to_string();
        let command_ok = if force {
            self.git_try_without_automation(&["worktree", "remove", "--force", &path_str])?
        } else {
            self.git_try_without_automation(&["worktree", "remove", &path_str])?
        };
        Ok((command_ok || !existed) && !path.exists())
    }

    fn remove_worktree_at(&self, path: &Path) -> Result<bool> {
        self.remove_worktree_at_with_force(path, false)
    }

    /// Force removal is reserved for the explicit `clean --all` path.
    fn remove_worktree_at_force(&self, path: &Path) -> bool {
        match self.remove_worktree_at_with_force(path, true) {
            Ok(removed) => removed,
            Err(error) => {
                logdim!(
                    "kept {} because removal did not reach a confirmed quiet point: {}",
                    path.display(),
                    error.last_line()
                );
                false
            }
        }
    }

    /// Remove a worktree only after a caller has verified it is unchanged.
    ///
    /// There is deliberately no force fallback, so Git can still refuse a
    /// removal if tracked or non-ignored work appears after the final check.
    fn remove_worktree_at_checked(&self, path: &Path) -> Result<bool> {
        if path.exists() && !self.worktree_belongs_to_repo(path)? {
            bail!(
                "{} is not a worktree owned by this repository, so it was kept",
                path.display()
            );
        }
        if path.exists() && self.has_recoverable_work(path)? {
            bail!(
                "the verified worktree at {} contains recoverable files or repository state. It \
                 was kept.",
                path.display()
            );
        }
        let path_str = path.display().to_string();
        self.git_at_without_automation(&self.root, &["worktree", "remove", &path_str])
            .map_err(|e| {
                e.with_message(format!(
                    "could not remove the verified worktree at {}: {}. It was kept.",
                    path.display(),
                    e.last_line()
                ))
            })?;
        Ok(!path.exists())
    }

    fn refuse_dirty_worktree(&self, path: &Path, label: &str) -> Result<()> {
        if !path.is_dir() {
            return Ok(());
        }
        let has_files = std::fs::read_dir(path)
            .map_err(|e| spar_err!("could not inspect {}: {e}", path.display()))?
            .next()
            .is_some();
        let owned = self.worktree_belongs_to_repo(path).map_err(|e| {
            spar_err!(
                "could not verify whether the existing {label} at {} belongs to this repository, \
                 so it was kept: {}",
                path.display(),
                e.last_line()
            )
        })?;
        if !owned {
            if has_files {
                bail!(
                    "the existing {label} at {} is not a worktree owned by \
                     this repository. Refusing to remove it.",
                    path.display()
                );
            }
            return Ok(());
        }
        if !path.join(".git").exists() {
            if has_files {
                bail!(
                    "the existing {label} at {} is not a readable Git worktree and is not empty. \
                     Refusing to remove it.",
                    path.display()
                );
            }
            return Ok(());
        }
        let dirty = self.has_recoverable_work(path).map_err(|e| {
            spar_err!(
                "could not verify whether the existing {label} at {} is clean, so it was kept: \
                 {}",
                path.display(),
                e.last_line()
            )
        })?;
        if dirty {
            bail!(
                "the existing {label} contains uncommitted changes or ignored files at {}. \
                 Rebuilding it would delete those files.\nCommit or recover them before running this \
                 command again, or use `spar clean --all` if they are not needed.",
                path.display()
            );
        }
        Ok(())
    }

    /// Check an existing PR branch out into an isolated worktree.
    pub fn worktree_for_pr(&self, pr: &PrView) -> Result<(PathBuf, String)> {
        let head = pr.head_ref_name.clone();
        if head.trim().is_empty() {
            bail!("PR #{} has no head branch to check out", pr.number);
        }
        let path = self.worktree_path(&format!("pr-{}", pr.number));
        let local = self.branch_for_pr(pr.number);

        self.git(&["fetch", "origin", &head]).map_err(|e| {
            spar_err!(
                "could not fetch the branch behind PR #{}: {}",
                pr.number,
                e.last_line()
            )
        })?;
        let start = format!("origin/{head}");
        let start_ref = format!("refs/remotes/origin/{head}");
        let local_ref = format!("refs/heads/{local}");
        if self.exact_ref_exists_checked(&self.root, &local_ref)? {
            let unpushed = self.commits_not_in_checked(&self.root, &local_ref, &start_ref)?;
            if unpushed > 0 {
                bail!(
                    "the existing worktree for PR #{} has {unpushed} local commit(s) that are not \
                     on {start}. Rebuilding it would delete their branch.\nInspect the worktree at \
                     {} and push or recover those commits before running this command again.",
                    pr.number,
                    path.display()
                );
            }
        }
        self.refuse_dirty_worktree(&path, &format!("worktree for PR #{}", pr.number))?;
        if !self.branch_deletion_is_safe(&local)? {
            bail!(
                "the existing branch {local} has a tip or reflog-only commit that no surviving \
                 ref preserves. Rebuilding it would delete recovery history. Inspect the branch \
                 before retrying."
            );
        }
        if !self.remove_worktree_at(&path)? {
            bail!(
                "the existing worktree for PR #{} could not be removed safely. Its branch was \
                 kept.",
                pr.number
            );
        }
        if !self.delete_branch_if_safe(&local)? {
            bail!(
                "the existing branch {local} changed or remained checked out while the PR \
                 worktree was being rebuilt. It was kept."
            );
        }

        let path_str = path.display().to_string();
        self.git(&["worktree", "add", "-B", &local, &path_str, &start])?;
        self.record_branch(&local, "pr", pr.number);
        Ok((path, head))
    }

    /// Check a pull request's head out read only, detached, with no branch.
    ///
    /// Fetches `refs/pull/N/head`, which GitHub serves for every pull request
    /// including one from a fork whose branch is not in this repository at all.
    /// That is what makes reviewing an outside contribution possible when
    /// pushing to it is not.
    ///
    /// Detached on purpose. Review only mode has nothing to push, and a branch
    /// would only invite something to try.
    pub fn worktree_for_pr_head(&self, number: i64) -> Result<PathBuf> {
        let path = self.worktree_path(&format!("review-{number}"));
        let local_ref = review_ref(number);
        let refspec = format!("+refs/pull/{number}/head:{local_ref}");

        self.refuse_review_worktree_changes(number)?;

        self.git(&["fetch", "origin", &refspec]).map_err(|e| {
            spar_err!(
                "could not fetch the head of PR #{number}. {}\nGitHub serves refs/pull/N/head for \
                 every pull request, so this usually means the number is wrong or `origin` does \
                 not point at the repository the PR is on.",
                e.last_line()
            )
        })?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| spar_err!("could not create {}: {e}", parent.display()))?;
        }
        if !self.remove_worktree_at(&path)? {
            bail!(
                "the existing review worktree for PR #{number} could not be removed safely. Its \
                 reference was kept."
            );
        }
        let path_str = path.display().to_string();
        self.git(&["worktree", "add", "--detach", &path_str, &local_ref])?;
        Ok(path)
    }

    fn refuse_review_worktree_changes(&self, number: i64) -> Result<()> {
        let path = self.worktree_path(&format!("review-{number}"));
        if !path.is_dir() {
            return Ok(());
        }
        let local_ref = review_ref(number);
        if !self.worktree_belongs_to_repo(&path)? {
            return Ok(());
        }
        if !self.exact_ref_exists_checked(&self.root, &local_ref)? {
            bail!(
                "the existing review worktree for PR #{number} has no recorded head at \
                 {local_ref}. Refusing to rebuild {}.",
                path.display()
            );
        }
        let worktree_head = self.head_oid_checked(&path)?;
        let recorded_head = self
            .git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref])?
            .trim()
            .to_string();
        if worktree_head != recorded_head {
            bail!(
                "the existing review worktree for PR #{number} has a local commit that is not on \
                 {local_ref}. Rebuilding it would delete the only checkout of that work. Inspect \
                 {} before retrying.",
                path.display()
            );
        }
        self.refuse_dirty_worktree(&path, &format!("review worktree for PR #{number}"))?;
        Ok(())
    }

    /// A worktree for one part of a split, on a new branch off `start`.
    ///
    /// `start` is the base branch for independent parts and the previous part's
    /// branch for stacked ones, which is the only difference between the two
    /// shapes at this level.
    ///
    /// The branch is whatever name was free, which is why it is returned rather
    /// than derived by the caller. Splitting the same pull request a second
    /// time would otherwise target the branch behind the first run's pull
    /// request. Split pushes are create-only and would refuse that target, but
    /// a repeated split still needs distinct branches rather than a name that
    /// can never be created.
    pub fn worktree_for_split(
        &self,
        parent: i64,
        index: usize,
        start: &str,
    ) -> Result<(PathBuf, String)> {
        let slot = self.free_split_slot(parent, index)?;
        let branch = format!("{}{slot}", self.branch_prefix);
        let path = self.worktree_path(&slot);

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| spar_err!("could not create {}: {e}", dir.display()))?;
        }
        self.refuse_dirty_worktree(&path, &format!("worktree for part {index} of PR #{parent}"))?;
        // The name is free, so there is no branch to delete. A directory can
        // still be in the way, left by a worktree that was pruned from git's
        // records without being removed from disk.
        if !self.remove_worktree_at(&path)? {
            bail!(
                "the existing worktree for part {index} of PR #{parent} could not be removed \
                 safely. No branch was created."
            );
        }

        let path_str = path.display().to_string();
        self.git(&["worktree", "add", "-b", &branch, &path_str, start])
            .map_err(|e| {
                spar_err!(
                    "could not create a worktree for part {index} of #{parent}. {}",
                    e.last_line()
                )
            })?;
        // Recorded before anything else can fail. An unrecorded branch is one
        // `prune_branches` will never remove.
        self.record_branch(&branch, "split", parent);
        Ok((path, branch))
    }

    /// The first part branch nothing is already sitting on.
    ///
    /// Origin as well as local, because a part's branch outlives the local one:
    /// a second split of the same pull request finds its own earlier branches
    /// deleted here but alive on origin, where the pull requests that reviewed
    /// them still point at them.
    fn free_split_slot(&self, parent: i64, index: usize) -> Result<String> {
        for attempt in 1..=SPLIT_SLOTS {
            let slot = split_slot(parent, index, attempt);
            let branch = format!("{}{slot}", self.branch_prefix);
            self.git_try(&["fetch", "origin", &branch]);
            if !self.rev_exists(&self.root, &branch)
                && !self.rev_exists(&self.root, &format!("origin/{branch}"))
            {
                return Ok(slot);
            }
        }
        bail!(
            "part {index} of #{parent} has no free branch name: {} and {SPLIT_SLOTS} suffixed \
             names are all taken. Inspect the existing branches and child pull requests. Finish \
             recording the earlier split, or remove every retained local worktree and branch, \
             child pull request, and remote split branch before starting over.",
            self.branch_for_split(parent, index)
        )
    }

    /// Whether a previous attempt pushed any branch for this split.
    ///
    /// The parent comment is the normal retry marker. A branch is the fallback
    /// when that comment or the pull request creation failed after the push.
    /// Reading origin directly makes the guard survive a fresh clone.
    pub fn has_remote_split_branch(&self, parent: i64) -> Result<bool> {
        let pattern = format!("refs/heads/{}split-{parent}-*", self.branch_prefix);
        Ok(!self
            .git(&["ls-remote", "--heads", "origin", &pattern])?
            .trim()
            .is_empty())
    }

    /// Throw one part away: its worktree, its branch, and its record.
    ///
    /// For a part that would not stand on its own. Nothing has been pushed at
    /// that point, so this leaves no trace anywhere but the log. Takes what
    /// `worktree_for_split` returned, since the name it settled on is not
    /// derivable from the parent and the index.
    pub fn release_split_worktree(&self, dir: &Path, branch: &str) {
        match self.branch_deletion_is_safe(branch) {
            Ok(true) => {}
            Ok(false) => {
                logdim!(
                    "kept {branch} and {} because no surviving ref preserves its tip",
                    dir.display()
                );
                return;
            }
            Err(error) => {
                logdim!(
                    "kept {branch} and {} because preservation could not be verified: {}",
                    dir.display(),
                    error.last_line()
                );
                return;
            }
        }
        match self.remove_worktree_at(dir) {
            Ok(true) => match self.delete_branch_if_safe(branch) {
                Ok(true) => self.forget_branch(branch),
                Ok(false) => {
                    logdim!("kept {branch} because its tip or reflog changed before deletion")
                }
                Err(error) => logdim!(
                    "kept {branch} because deletion safety could not be rechecked: {}",
                    error.last_line()
                ),
            },
            Ok(false) => {}
            Err(error) => logdim!(
                "kept {branch} and {} because removal did not reach a confirmed quiet point: {}",
                dir.display(),
                error.last_line()
            ),
        }
    }

    /// Discard one exact mechanical slice that the split workflow just made.
    ///
    /// Unlike ordinary release, this intentionally removes an unpushed commit.
    /// The caller supplies the exact disposable tip, and every file, worktree,
    /// ownership, and ref check must still match before anything is removed.
    pub fn discard_split_worktree(&self, dir: &Path, branch: &str, disposable_head: &str) -> bool {
        let record = self.known_branches().get(branch).cloned();
        if record.is_none_or(|record| record.kind != "split") {
            logdim!("kept {branch} because no split branch record proves ownership");
            return false;
        }
        let local_ref = format!("refs/heads/{branch}");
        let expected = match self.git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref]) {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                logdim!(
                    "kept {branch} because its tip could not be checked: {}",
                    error.last_line()
                );
                return false;
            }
        };
        if expected != disposable_head {
            logdim!("kept {branch} because it moved beyond the disposable slice");
            return false;
        }
        match ref_reflog_is_preserved(&self.root, &local_ref, disposable_head) {
            Ok(true) => {}
            Ok(false) => {
                logdim!(
                    "kept {branch} because its reflog contains work outside the disposable slice"
                );
                return false;
            }
            Err(error) => {
                logdim!(
                    "kept {branch} because its reflog could not be checked: {}",
                    error.last_line()
                );
                return false;
            }
        }
        match self.head_oid_checked(dir) {
            Ok(head) if head == disposable_head => {}
            Ok(_) => {
                logdim!(
                    "kept {branch} and {} because the worktree moved beyond the disposable slice",
                    dir.display()
                );
                return false;
            }
            Err(error) => {
                logdim!(
                    "kept {branch} and {} because its head could not be checked: {}",
                    dir.display(),
                    error.last_line()
                );
                return false;
            }
        }
        match self.remove_worktree_at_checked(dir) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                logdim!(
                    "kept {branch} and {} because the disposable slice could not be verified: {}",
                    dir.display(),
                    error.last_line()
                );
                return false;
            }
        }
        if let Err(error) =
            self.git_at_without_automation(&self.root, &["update-ref", "-d", &local_ref, &expected])
        {
            logdim!(
                "kept {branch} because its exact disposable tip could not be deleted: {}",
                error.last_line()
            );
            return false;
        }
        match self.exact_ref_exists_checked(&self.root, &local_ref) {
            Ok(false) => {
                self.forget_branch(branch);
                true
            }
            Ok(true) => {
                logdim!("kept {branch} because its ref still exists after deletion");
                false
            }
            Err(error) => {
                logdim!(
                    "kept the branch record for {branch} because deletion could not be verified: {}",
                    error.last_line()
                );
                false
            }
        }
    }

    pub fn release_review_worktree(&self, number: i64) {
        let path = self.worktree_path(&format!("review-{number}"));
        match self.review_ref_deletion_is_safe(number) {
            Ok(true) => {}
            Ok(false) => {
                logdim!(
                    "kept {} because no surviving ref preserves its review history",
                    path.display()
                );
                return;
            }
            Err(error) => {
                logdim!(
                    "kept {} because review history could not be verified: {}",
                    path.display(),
                    error.last_line()
                );
                return;
            }
        }
        match self.remove_worktree_at(&path) {
            Ok(true) => match self.delete_review_ref_if_safe(number) {
                Ok(true) => {}
                Ok(false) => logdim!(
                    "kept {} because its review history changed before deletion",
                    review_ref(number)
                ),
                Err(error) => logdim!(
                    "kept {} because deletion safety could not be rechecked: {}",
                    review_ref(number),
                    error.last_line()
                ),
            },
            Ok(false) => {}
            Err(error) => logdim!(
                "kept {} because removal did not reach a confirmed quiet point: {}",
                path.display(),
                error.last_line()
            ),
        }
    }

    /// Release a read-only review checkout only when every observed part of
    /// its Git state still matches the checkpoint captured before the calls.
    pub(crate) fn release_review_worktree_checked(
        &self,
        number: i64,
        checkpoint: &WorktreeCheckpoint,
    ) -> Result<()> {
        let path = self.worktree_path(&format!("review-{number}"));
        self.require_unchanged_worktree(
            &path,
            checkpoint,
            &format!("review worktree for PR #{number}"),
        )?;
        if !self.review_ref_deletion_is_safe(number)? {
            bail!(
                "the review reference for PR #{number} has reflog-only recovery history. The \
                 worktree and reference were kept."
            );
        }
        if !self.remove_worktree_at_checked(&path)? {
            bail!(
                "the verified review worktree at {} could not be removed, so its reference was \
                 kept",
                path.display()
            );
        }
        if !self.delete_review_ref_if_safe(number)? {
            bail!(
                "the review reference for PR #{number} changed before deletion. The reference was \
                 kept."
            );
        }
        Ok(())
    }

    pub fn release_pr_worktree(&self, number: i64) -> bool {
        let path = self.worktree_path(&format!("pr-{number}"));
        let local = self.branch_for_pr(number);
        match self.branch_deletion_is_safe(&local) {
            Ok(true) => {}
            Ok(false) => {
                logdim!(
                    "kept {local} and {} because no surviving ref preserves its tip",
                    path.display()
                );
                return false;
            }
            Err(error) => {
                logdim!(
                    "kept {local} and {} because preservation could not be verified: {}",
                    path.display(),
                    error.last_line()
                );
                return false;
            }
        }
        match self.remove_worktree_at(&path) {
            Ok(true) => match self.delete_branch_if_safe(&local) {
                Ok(true) => {
                    self.forget_branch(&local);
                    true
                }
                Ok(false) => {
                    logdim!("kept {local} because its tip or reflog changed before deletion");
                    false
                }
                Err(error) => {
                    logdim!(
                        "kept {local} because deletion safety could not be rechecked: {}",
                        error.last_line()
                    );
                    false
                }
            },
            Ok(false) => false,
            Err(error) => {
                logdim!(
                    "kept {local} and {} because removal did not reach a confirmed quiet point: {}",
                    path.display(),
                    error.last_line()
                );
                false
            }
        }
    }

    // -- branch state -----------------------------------------------------

    /// What to diff against: the remote tracking branch when it resolves, the
    /// local branch when it does not.
    ///
    /// This is not a nicety. Every "did the agent do anything" check hangs off
    /// this ref, and `git log` against a ref that does not exist fails silently
    /// and reads as "no commits". A checkout whose `origin/main` was never
    /// fetched would report every implementation as abandoned and throw the
    /// work away.
    pub fn base_ref(&self, cwd: &Path, base: &str) -> String {
        let remote = format!("origin/{base}");
        if self.rev_exists(cwd, &remote) {
            return remote;
        }
        if self.rev_exists(cwd, base) {
            logdim!("origin/{base} does not resolve, comparing against local {base}");
            return base.to_string();
        }
        logdim!("neither origin/{base} nor {base} resolves; results will be unreliable");
        remote
    }

    fn rev_exists(&self, cwd: &Path, refname: &str) -> bool {
        let spec = format!("{refname}^{{commit}}");
        !self
            .git_try_at(Some(cwd), &["rev-parse", "--verify", "--quiet", &spec])
            .trim()
            .is_empty()
    }

    pub fn has_changes(&self, cwd: &Path, base: &str) -> bool {
        let range = format!("{}..HEAD", self.base_ref(cwd, base));
        !self
            .git_try_at(Some(cwd), &["log", &range, "--oneline"])
            .trim()
            .is_empty()
    }

    fn exact_ref_exists_checked(&self, cwd: &Path, refname: &str) -> Result<bool> {
        let found = self.git_at(Some(cwd), &["for-each-ref", "--format=%(refname)", refname])?;
        Ok(found.lines().any(|line| line.trim() == refname))
    }

    fn commits_not_in_checked(&self, cwd: &Path, tip: &str, published: &str) -> Result<usize> {
        let count = self.git_at(Some(cwd), &["rev-list", "--count", tip, "--not", published])?;
        count.trim().parse::<usize>().map_err(|e| {
            spar_err!(
                "git returned an invalid commit count for {tip} outside {published}: {:?} ({e})",
                count.trim()
            )
        })
    }

    pub(crate) fn base_ref_checked(&self, cwd: &Path, base: &str) -> Result<String> {
        let remote = format!("refs/remotes/origin/{base}");
        if self.exact_ref_exists_checked(cwd, &remote)? {
            return Ok(remote);
        }
        let local = format!("refs/heads/{base}");
        if self.exact_ref_exists_checked(cwd, &local)? {
            return Ok(local);
        }
        bail!("neither origin/{base} nor local branch {base} resolves")
    }

    pub(crate) fn commit_count_checked(
        &self,
        cwd: &Path,
        refname: &str,
        base: &str,
    ) -> Result<usize> {
        let range = format!("{}..{refname}", self.base_ref_checked(cwd, base)?);
        let count = self.git_at(Some(cwd), &["rev-list", "--count", &range])?;
        count.trim().parse::<usize>().map_err(|e| {
            spar_err!(
                "git returned an invalid commit count for {range}: {:?} ({e})",
                count.trim()
            )
        })
    }

    pub(crate) fn has_changes_checked(&self, cwd: &Path, base: &str) -> Result<bool> {
        Ok(self.commit_count_checked(cwd, "HEAD", base)? > 0)
    }

    pub(crate) fn head_oid_checked(&self, cwd: &Path) -> Result<String> {
        let head = self.git_at(Some(cwd), &["rev-parse", "--verify", "HEAD^{commit}"])?;
        let head = head.trim().to_string();
        if head.is_empty() {
            bail!("git returned an empty HEAD for {}", cwd.display());
        }
        Ok(head)
    }

    /// Whether a recorded SPAR branch's exact tip is retained by a pull request.
    ///
    /// Pull request head refs remain available after close or merge, so this is
    /// stronger than requiring an open pull request or a live remote branch.
    pub(crate) fn current_branch_is_preserved(&self, cwd: &Path) -> Result<bool> {
        let branch = self.git_at(Some(cwd), &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        self.local_branch_is_preserved(branch.trim())
    }

    /// Whether a recorded local branch's exact tip is retained by its pull
    /// request head, regardless of which branch is currently checked out.
    pub(crate) fn local_branch_is_preserved(&self, branch: &str) -> Result<bool> {
        let known = self.known_branches();
        let Some(record) = known.get(branch) else {
            return Ok(false);
        };
        self.branch_is_preserved_checked(branch, record)
    }

    /// Whether tracked, staged, or non-ignored untracked files are uncommitted.
    pub(crate) fn has_uncommitted_changes(&self, cwd: &Path) -> Result<bool> {
        has_uncommitted_work(cwd)
    }

    /// Whether removing a worktree would delete any local file Git does not
    /// reproduce from its commits, including ignored untracked files.
    fn has_recoverable_work(&self, cwd: &Path) -> Result<bool> {
        repository_has_recoverable_work(cwd, true)
    }

    /// Record ignored artifacts that existed before an editing call.
    pub(crate) fn worktree_baseline(&self, cwd: &Path) -> Result<WorktreeBaseline> {
        let attributes = attribute_state(cwd)?;
        Ok(WorktreeBaseline {
            attributes,
            ignored_untracked: ignored_untracked_state(cwd)?,
            git_state: safe_git_state(cwd)?,
        })
    }

    /// Capture the Git state of a checkout intended to remain read only while
    /// external commands inspect it.
    pub(crate) fn worktree_checkpoint(&self, cwd: &Path) -> Result<WorktreeCheckpoint> {
        let attributes = attribute_state(cwd)?;
        Ok(WorktreeCheckpoint {
            path: std::fs::canonicalize(cwd)
                .map_err(|e| spar_err!("could not resolve {}: {e}", cwd.display()))?,
            attributes,
            git_state: safe_git_state(cwd)?,
            ignored_untracked: ignored_untracked_state(cwd)?,
        })
    }

    /// Require a read-only checkout to match a previously captured checkpoint.
    /// Any probe failure is an error because deletion cannot be proven safe.
    pub(crate) fn require_unchanged_worktree(
        &self,
        cwd: &Path,
        checkpoint: &WorktreeCheckpoint,
        label: &str,
    ) -> Result<()> {
        let resolved = std::fs::canonicalize(cwd).map_err(|e| {
            crate::error::SparError::uncertain_write(format!(
                "could not resolve the {label} at {} after inspection: {e}. It was kept.",
                cwd.display()
            ))
        })?;
        if resolved != checkpoint.path {
            return Err(uncertain_worktree_change(
                cwd,
                format!(
                    "the {label} moved from {} to {} during inspection. It was kept.",
                    checkpoint.path.display(),
                    resolved.display()
                ),
            ));
        }
        let attributes = attribute_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify attribute files in the {label} at {}: {}. It was kept.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        if attributes != checkpoint.attributes {
            return Err(uncertain_worktree_change(
                cwd,
                format!(
                    "attribute files in the {label} at {} changed during inspection. It was \
                     kept for recovery.",
                    cwd.display()
                ),
            ));
        }
        let git_state = git_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify the Git state of the {label} at {}: {}. It was kept.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        let ignored = ignored_untracked_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify untracked files in the {label} at {}: {}. It was kept.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        if git_state != checkpoint.git_state
            || checkpoint
                .ignored_untracked
                .changed_beyond_generated(&ignored)
        {
            return Err(uncertain_worktree_change(
                cwd,
                format!(
                    "the {label} at {} changed during a read-only inspection. It was kept for \
                     recovery.",
                    cwd.display()
                ),
            ));
        }
        Ok(())
    }

    /// Refuse to discard ignored files that appeared during a call.
    ///
    /// Call this when the call reported success but produced no commit-worthy
    /// status. Existing ignored files are harmless because they are present in
    /// `baseline`; only new paths stop cleanup, and recognized build and cache
    /// output is not one of them. Running the project's tests is what a call is
    /// asked to do, and whatever wrote that output writes it again.
    pub(crate) fn refuse_new_ignored_files(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<()> {
        self.check_new_ignored_files(cwd, baseline).map(drop)
    }

    /// The generated paths the check let through, for one report per attempt
    /// rather than one per check.
    fn allow_generated_ignored_files(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<Vec<PathBuf>> {
        self.check_new_ignored_files(cwd, baseline)
    }

    fn check_new_ignored_files(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<Vec<PathBuf>> {
        self.refuse_changed_attributes(cwd, baseline)?;
        let after = ignored_untracked_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify untracked files in {} after editing: {}. The worktree was \
                     kept for recovery.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        let changed = baseline.ignored_untracked.changed_paths(&after);
        if changed.is_empty() {
            return Ok(Vec::new());
        }
        let (generated, changed): (Vec<_>, Vec<_>) = changed
            .into_iter()
            .partition(|path| after.is_ignored(path) && is_generated_artifact(path));
        if changed.is_empty() {
            return Ok(generated);
        }
        let mut listed = changed
            .iter()
            .take(5)
            .map(|path| format!("{:?}", path.as_os_str()))
            .collect::<Vec<_>>()
            .join(", ");
        if changed.len() > 5 {
            listed.push_str(&format!(", and {} more", changed.len() - 5));
        }
        Err(uncertain_worktree_change(
            cwd,
            format!(
                "the editing call created or changed untracked or ignored file(s) in {} that \
                 cannot be represented by a managed commit: {listed}. The worktree was kept for \
                 recovery.",
                cwd.display()
            ),
        ))
    }

    /// Existing untracked files belong to the checkout owner, even when a call
    /// also produces a valid tracked change. Refuse their modification or
    /// deletion before accepting the tracked result. Rebuilt output is not that:
    /// it is ignored on both sides and under a known build or cache directory,
    /// which is where the command the call was asked to run puts it.
    pub(crate) fn refuse_changed_existing_untracked(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<()> {
        self.check_changed_existing_untracked(cwd, baseline)
            .map(drop)
    }

    fn allow_changed_generated_artifacts(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<Vec<PathBuf>> {
        self.check_changed_existing_untracked(cwd, baseline)
    }

    fn check_changed_existing_untracked(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<Vec<PathBuf>> {
        self.refuse_changed_attributes(cwd, baseline)?;
        let after = ignored_untracked_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify existing untracked files in {} after editing: {}. The \
                     worktree was kept for recovery.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        let changed = baseline.ignored_untracked.changed_existing_paths(&after);
        if changed.is_empty() {
            return Ok(Vec::new());
        }
        let (generated, changed): (Vec<_>, Vec<_>) = changed.into_iter().partition(|path| {
            baseline.ignored_untracked.is_ignored(path)
                && after.is_ignored(path)
                && is_generated_artifact(path)
        });
        if changed.is_empty() {
            return Ok(generated);
        }
        let mut listed = changed
            .iter()
            .take(5)
            .map(|path| format!("{:?}", path.as_os_str()))
            .collect::<Vec<_>>()
            .join(", ");
        if changed.len() > 5 {
            listed.push_str(&format!(", and {} more", changed.len() - 5));
        }
        Err(uncertain_worktree_change(
            cwd,
            format!(
                "the editing call changed or deleted existing untracked file(s) in {}: \
                 {listed}. The worktree was kept for recovery.",
                cwd.display()
            ),
        ))
    }

    /// Refuse a byte or mode change that the index did not represent.
    ///
    /// Clean filters can normalize a working file back to its existing blob,
    /// and index flags can hide a change from porcelain status. Comparing the
    /// actual tracked files on both sides keeps those bytes from being treated
    /// as disposable just because Git has no diff for them.
    pub(crate) fn refuse_unrepresented_tracked_changes(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<()> {
        self.refuse_changed_attributes(cwd, baseline)?;
        let after = safe_git_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify tracked files in {} after editing: {}. The worktree was \
                     kept for recovery.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        let mut changed = Vec::new();
        let before_filter_untracked = ignored_untracked_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not record untracked files before verifying transformed content in {}: \
                     {}. The worktree was kept for recovery.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        let mut filter_was_run = false;
        let mut filter_problem = None;
        let mut repositories: BTreeSet<PathBuf> =
            baseline.git_state.repositories.keys().cloned().collect();
        repositories.extend(after.repositories.keys().cloned());
        'repositories: for repository_path in repositories {
            let before_repository = baseline.git_state.repositories.get(&repository_path);
            let after_repository = after.repositories.get(&repository_path);
            if before_repository.is_none() || after_repository.is_none() {
                changed.push(repository_path.clone());
                continue;
            }
            if before_repository.map(|repository| &repository.gitlinks)
                != after_repository.map(|repository| &repository.gitlinks)
            {
                changed.push(repository_path.join("<gitlinks>"));
            }
            let mut paths = BTreeSet::new();
            if let Some(repository) = before_repository {
                paths.extend(repository.tracked.keys().cloned());
            }
            if let Some(repository) = after_repository {
                paths.extend(repository.tracked.keys().cloned());
            }
            for path in paths {
                let before = before_repository.and_then(|repository| repository.tracked.get(&path));
                let current = after_repository.and_then(|repository| repository.tracked.get(&path));
                let worktree_changed =
                    before.map(|entry| &entry.worktree) != current.map(|entry| &entry.worktree);
                let index_changed = before.map(|entry| (&entry.index_mode, &entry.index_oid))
                    != current.map(|entry| (&entry.index_mode, &entry.index_oid));
                if !worktree_changed {
                    continue;
                }
                if !index_changed {
                    changed.push(repository_path.join(&path));
                    continue;
                }
                let before_worktree = before.and_then(|entry| entry.worktree.as_ref());
                let current_worktree = current.and_then(|entry| entry.worktree.as_ref());
                let Some(current_entry) = current else {
                    continue;
                };
                let Some(current_worktree) = current_worktree else {
                    continue;
                };
                let content_changed =
                    before_worktree.map(|file| file.content) != Some(current_worktree.content);
                let mode_changed = before_worktree.map(|file| file.mode.as_str())
                    != Some(current_worktree.mode.as_str());
                let repository = cwd.join(&repository_path);
                let represented_content = if content_changed {
                    filter_was_run = true;
                    let result =
                        filtered_index_content(&repository, &path, &current_entry.index_oid);
                    self.refuse_changed_attributes(cwd, baseline)?;
                    match result {
                        Ok(expected) => expected == current_worktree.content,
                        Err(error) => {
                            filter_problem = Some(format!(
                                "could not verify transformed content for {:?}: {}",
                                repository_path.join(&path),
                                error.last_line()
                            ));
                            false
                        }
                    }
                } else {
                    true
                };
                let represented_mode =
                    !mode_changed || current_worktree.mode == current_entry.index_mode;
                if !represented_content || !represented_mode {
                    changed.push(repository_path.join(&path));
                }
                if filter_problem.is_some() {
                    break 'repositories;
                }
            }
        }
        if filter_was_run {
            self.refuse_changed_attributes(cwd, baseline)?;
            let verified = safe_git_state(cwd).map_err(|e| {
                uncertain_worktree_change(
                    cwd,
                    format!(
                        "could not recheck tracked files after verifying transformed content in \
                         {}: {}. The worktree was kept for recovery.",
                        cwd.display(),
                        e.last_line()
                    ),
                )
            })?;
            let verified_untracked = ignored_untracked_state(cwd).map_err(|e| {
                uncertain_worktree_change(
                    cwd,
                    format!(
                        "could not recheck untracked files after verifying transformed content \
                         in {}: {}. The worktree was kept for recovery.",
                        cwd.display(),
                        e.last_line()
                    ),
                )
            })?;
            if verified != after
                || before_filter_untracked.changed_beyond_generated(&verified_untracked)
            {
                return Err(uncertain_worktree_change(
                    cwd,
                    "a content filter changed the worktree while SPAR verified the managed \
                     commit. The worktree was kept for recovery.",
                ));
            }
            self.refuse_changed_existing_untracked(cwd, baseline)?;
        }
        if let Some(problem) = filter_problem {
            return Err(uncertain_worktree_change(
                cwd,
                format!("{problem}. The worktree was kept for recovery."),
            ));
        }
        if changed.is_empty() {
            return Ok(());
        }
        let mut listed = changed
            .iter()
            .take(5)
            .map(|path| format!("{:?}", path.as_os_str()))
            .collect::<Vec<_>>()
            .join(", ");
        if changed.len() > 5 {
            listed.push_str(&format!(", and {} more", changed.len() - 5));
        }
        Err(uncertain_worktree_change(
            cwd,
            format!(
                "the editing call changed tracked working-file bytes, modes, repositories, or \
                 gitlinks outside an accepted commit: {listed}. The worktree was kept for \
                 recovery."
            ),
        ))
    }

    pub(crate) fn refuse_changed_attributes(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
    ) -> Result<()> {
        let after = attribute_state(cwd).map_err(|e| {
            uncertain_worktree_change(
                cwd,
                format!(
                    "could not verify attribute files in {} after editing: {}. The worktree was \
                     kept for recovery.",
                    cwd.display(),
                    e.last_line()
                ),
            )
        })?;
        if after == baseline.attributes {
            return Ok(());
        }
        Err(uncertain_worktree_change(
            cwd,
            format!(
                "the editing call changed a .gitattributes file in {}. It was kept, but SPAR \
                 refused to run a Git operation that could select a new external filter.",
                cwd.display()
            ),
        ))
    }

    /// Commit a successful editing call from the trusted harness process.
    ///
    /// Editing sandboxes only need the working tree. They never need writable
    /// access to the repository's object database, refs, config, or hooks.
    pub(crate) fn commit_pending_changes(
        &self,
        cwd: &Path,
        baseline: &WorktreeBaseline,
        preferred_subject: &str,
        fallback_subject: &str,
    ) -> Result<bool> {
        let mut artifacts = GeneratedArtifacts::default();
        self.refuse_changed_attributes(cwd, baseline)?;
        artifacts.changed(self.allow_changed_generated_artifacts(cwd, baseline)?);
        refuse_unsafe_index_flags(cwd)?;
        if !self.has_uncommitted_changes(cwd)? {
            artifacts.left(self.allow_generated_ignored_files(cwd, baseline)?);
            artifacts.report(cwd);
            return Ok(false);
        }
        self.stage_managed_changes(cwd, baseline).map_err(|e| {
            e.with_message(format!(
                "could not stage changes in {}: {}",
                cwd.display(),
                e.last_line()
            ))
        })?;
        // Ignored paths remain untracked after staging. Only known generated
        // output may remain beside an otherwise complete managed commit.
        artifacts.left(self.allow_generated_ignored_files(cwd, baseline)?);
        let changed_gitlinks = changed_staged_gitlinks(cwd)?;
        if !changed_gitlinks.is_empty() {
            let listed = changed_gitlinks
                .iter()
                .take(5)
                .map(|path| format!("{:?}", path.as_os_str()))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "the editing call added or changed a gitlink at {listed}. It was staged but not \
                 committed because the referenced repository objects might exist only inside \
                 this worktree. The worktree was kept for recovery."
            );
        }
        let mut subject = self.clean_title(preferred_subject)?;
        if subject.trim().is_empty() {
            subject = self.clean_title(fallback_subject)?;
        }
        self.commit_staged_changes(cwd, &subject).map_err(|e| {
            e.with_message(format!(
                "could not commit changes in {}: {}. The staged files were kept.",
                cwd.display(),
                e.last_line()
            ))
        })?;
        if has_tracked_or_staged_work(cwd)? {
            bail!(
                "the commit in {} left additional uncommitted files. They were kept for \
                 recovery.",
                cwd.display()
            );
        }
        artifacts.changed(self.allow_changed_generated_artifacts(cwd, baseline)?);
        artifacts.left(self.allow_generated_ignored_files(cwd, baseline)?);
        artifacts.report(cwd);
        Ok(true)
    }

    fn stage_managed_changes(&self, cwd: &Path, baseline: &WorktreeBaseline) -> Result<()> {
        let after = ignored_untracked_state(cwd)?;
        self.git_at_without_automation(cwd, &["add", "-u"])?;
        let paths = baseline.ignored_untracked.new_ordinary_paths(&after);
        if paths.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for path in paths {
            input.extend(os_str_bytes(path.as_os_str())?);
            input.push(0);
        }
        let argv = git_without_automation_argv(&[
            "--literal-pathspecs",
            "add",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]);
        proc::run_with_input_bytes(
            &argv,
            &self.git_opts(Some(cwd), true).stop_descendants(true),
            &input,
        )?;
        Ok(())
    }

    /// Commit an index prepared by the parent without signing, hooks, or
    /// inherited repository automation.
    pub(crate) fn commit_staged_changes(&self, cwd: &Path, subject: &str) -> Result<()> {
        self.git_at_without_automation(cwd, &["commit", "--no-verify", "-m", subject])
            .map(|_| ())
    }

    /// How many commits `refname` carries that the base does not.
    ///
    /// Counted from the commits themselves rather than from `commit_subjects`,
    /// which drops a commit whose message is empty. The guards in
    /// `worktree_add` decide whether to delete a branch on this number, and an
    /// empty message must not read as an empty branch.
    pub fn commit_count(&self, cwd: &Path, refname: &str, base: &str) -> usize {
        let range = format!("{}..{refname}", self.base_ref(cwd, base));
        self.git_try_at(Some(cwd), &["rev-list", "--count", &range])
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// One `hash subject` line per commit `refname` carries that the base does
    /// not, oldest first. For showing a person what is on a branch, so the
    /// hash keeps a commit with no message from listing as nothing.
    pub fn commit_lines(&self, cwd: &Path, refname: &str, base: &str) -> Vec<String> {
        let range = format!("{}..{refname}", self.base_ref(cwd, base));
        self.git_try_at(Some(cwd), &["log", &range, "--reverse", "--format=%h %s"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The commits `later` carries that `earlier` does not, oldest first, when
    /// `earlier` is genuinely behind it.
    ///
    /// `None` when it is not an ancestor, which is not the same as nothing
    /// having landed. `rewrite_commits_if_needed` rewrites hashes from the first
    /// offending commit spar made onward, so a head recorded part way through a
    /// round can still be a readable object and no longer be on the branch. `git log` answers that
    /// with every commit on the branch, so without the check the one caller
    /// would report the whole branch as unread, which is the widest possible
    /// wrong answer.
    ///
    /// No `base_ref` resolution, unlike its neighbours: these are commits rather
    /// than branch names, and putting a sha through it logs a fallback line
    /// every time.
    pub fn commits_since(&self, cwd: &Path, earlier: &str, later: &str) -> Option<Vec<String>> {
        let ancestor = self
            .git_at(Some(cwd), &["merge-base", "--is-ancestor", earlier, later])
            .is_ok();
        if !ancestor {
            return None;
        }
        let range = format!("{earlier}..{later}");
        Some(
            self.git_try_at(Some(cwd), &["log", &range, "--reverse", "--format=%h %s"])
                .lines()
                .map(str::to_string)
                .collect(),
        )
    }

    /// The subjects of the commits `refname` carries that the base does not,
    /// oldest first.
    pub fn commit_subjects(&self, cwd: &Path, refname: &str, base: &str) -> Vec<String> {
        let range = format!("{}..{refname}", self.base_ref(cwd, base));
        self.git_try_at(Some(cwd), &["log", &range, "--reverse", "--format=%s"])
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The paths this checkout changes relative to the base, sorted.
    ///
    /// A three dot range, matching `diff_stat`: what the branch did, not what
    /// the base has done since.
    ///
    /// `--no-renames` because a rename reported as its destination alone leaves
    /// the source out of the list, and a part carrying only the destination
    /// would be a copy. As a deletion and an addition it is two paths, which a
    /// part can carry together or leave to the leftover report.
    ///
    /// `-z` because without it git writes paths for display: anything
    /// non-ASCII comes back escaped and wrapped in quotes, and that string is
    /// not a path. A part built from one carries a pathspec matching no file,
    /// so the file never reaches the slice while every list still says the part
    /// took it. It also keeps a path with a space at either end intact.
    pub fn changed_files(&self, cwd: &Path, base: &str) -> Vec<String> {
        let range = format!("{}...HEAD", self.base_ref(cwd, base));
        self.git_try_at(
            Some(cwd),
            &["diff", "--name-only", "--no-renames", "-z", &range],
        )
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
    }

    /// Where `refname` left the base: the commit its own change is measured
    /// from, and the one a slice of that change has to be taken against.
    pub fn merge_base(&self, cwd: &Path, base: &str, refname: &str) -> Result<String> {
        let base_ref = self.base_ref(cwd, base);
        let out = self
            .git_at(Some(cwd), &["merge-base", &base_ref, refname])
            .map_err(|e| {
                spar_err!(
                    "could not find where {refname} and {base_ref} diverged. {}",
                    e.last_line()
                )
            })?;
        let sha = out.trim().to_string();
        if sha.is_empty() {
            bail!("{refname} and {base_ref} share no history");
        }
        Ok(sha)
    }

    pub fn diff_stat(&self, cwd: &Path, base: &str) -> String {
        let range = format!("{}...HEAD", self.base_ref(cwd, base));
        let full = self.git_try_at(Some(cwd), &["diff", &range, "--shortstat"]);
        full.trim().to_string()
    }

    /// Scrub commit messages that slipped past the prompt.
    ///
    /// `git filter-branch` calls back into this same binary, so there is no
    /// interpreter to find and no second copy of the rules to drift.
    ///
    /// `floor` is the head spar found when it started, and nothing at or below
    /// it is rewritten. The style gate is for what spar writes, and a person's
    /// commit message is not that: rewriting one changes a published SHA, which
    /// diverges the branch under somebody's own checkout and is exactly the
    /// history rewrite this design promises never to do. A `None` floor means
    /// every commit above the base is spar's own, which is true of a branch
    /// spar created in this invocation and of nothing else.
    pub fn rewrite_commits_if_needed(
        &self,
        cwd: &Path,
        base: &str,
        floor: Option<&str>,
    ) -> Result<()> {
        let base_ref = self.base_ref(cwd, base);
        let floor = floor.filter(|f| !f.trim().is_empty()).unwrap_or(&base_ref);
        self.report_commits_left_alone(cwd, &base_ref, floor);

        let range = format!("{floor}..HEAD");
        let raw = self.git_try_at(Some(cwd), &["log", &range, "--format=%H%x00%B%x1e"]);

        let offenders = raw
            .split('\x1e')
            .filter_map(|entry| entry.split_once('\0'))
            .filter(|(_, body)| !style::violations(body, &self.style).is_empty())
            .count();
        if offenders == 0 {
            return Ok(());
        }
        logdim!("{offenders} commit message(s) violated style rules, rewriting");

        let exe = self_binary()?;
        let filter = format!("{} scrub-filter", sh_quote(&exe.display().to_string()));

        let argv: Vec<String> = [
            "git",
            "filter-branch",
            "-f",
            "--msg-filter",
            &filter,
            &range,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let opts = ExecOpts::new()
            .cwd(cwd)
            .check(false)
            .timeout_secs(600)
            .env("FILTER_BRANCH_SQUELCH_WARNING", "1")
            .env("SPAR_BAN_EM_DASH", bool_env(self.style.ban_em_dash))
            .env(
                "SPAR_BAN_AI_ATTRIBUTION",
                bool_env(self.style.ban_ai_attribution),
            );
        let _ = proc::run(&argv, &opts);

        let after = self.git_try_at(Some(cwd), &["log", &range, "--format=%B"]);
        if !style::violations(&after, &self.style).is_empty() {
            bail!(
                "commit messages still violate style rules after a rewrite in {}.",
                cwd.display()
            );
        }
        Ok(())
    }

    /// Say out loud when a message spar will not touch breaks the style rules.
    ///
    /// Silence here would read as "there was nothing to scrub", and the point
    /// of the floor is that there was something and spar chose to leave it.
    fn report_commits_left_alone(&self, cwd: &Path, base_ref: &str, floor: &str) {
        if floor == base_ref {
            return;
        }
        let raw = self.git_try_at(
            Some(cwd),
            &[
                "log",
                &format!("{base_ref}..{floor}"),
                "--format=%H%x00%B%x1e",
            ],
        );
        let left: Vec<&str> = raw
            .split('\x1e')
            .filter_map(|entry| entry.split_once('\0'))
            .filter(|(_, body)| !style::violations(body, &self.style).is_empty())
            .map(|(hash, _)| &hash.trim()[..hash.trim().len().min(8)])
            .collect();
        if left.is_empty() {
            return;
        }
        logdim!(
            "{} commit message(s) already on the branch break the style rules and were left \
             alone: {}. spar does not rewrite commits it did not write.",
            left.len(),
            left.join(", ")
        );
    }

    /// Push by explicit refspec from HEAD.
    ///
    /// A resumed PR is checked out under a local name (`pr-N`) that does not
    /// match its remote branch, so pushing by branch name would resolve the
    /// wrong local ref or fail outright.
    pub fn push(&self, cwd: &Path, branch: &str) -> Result<()> {
        let refspec = format!("HEAD:{branch}");
        let pushed = self
            .git_at(
                Some(cwd),
                &["push", "--force-with-lease", "origin", &refspec],
            )
            .map(|_| ())
            .map_err(|e| {
                spar_err!(
                    "could not push to origin/{branch}. {}\nCheck push access and whether the \
                     branch moved under you.",
                    e.last_line()
                )
            });
        self.record_write(pushed)
    }

    /// Create one remote branch for a split without ever moving an existing ref.
    ///
    /// `worktree_for_split` chooses a name that is free locally and on origin,
    /// but another writer can still take it before the push. An empty expected
    /// value in the lease makes this an atomic create: it creates an absent ref,
    /// accepts an identical ref as a no-op, and never moves an existing ref.
    /// The shared `push` method cannot be used because its lease permits
    /// updating a ref fetched earlier.
    pub fn push_split_branch(
        &self,
        cwd: &Path,
        branch: &str,
    ) -> std::result::Result<(), SplitPushError> {
        let remote_ref = format!("refs/heads/{branch}");
        let lease = format!("--force-with-lease={remote_ref}:");
        let refspec = format!("HEAD:{remote_ref}");
        let result = match self.git_at(Some(cwd), &["push", &lease, "origin", &refspec]) {
            Ok(_) => Ok(()),
            Err(push_error) => {
                let local = self.git_at(Some(cwd), &["rev-parse", "HEAD"]);
                let remote = self.git(&["ls-remote", "--heads", "origin", &remote_ref]);
                reconcile_failed_split_push(branch, push_error, local, remote)
            }
        };
        self.record_write(result)
    }

    // -- gh ---------------------------------------------------------------

    pub fn gh(&self, args: &[&str]) -> Result<String> {
        self.gh_at(None, args)
    }

    pub fn gh_at(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut argv = vec!["gh".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        proc::run(
            &argv,
            &ExecOpts::new()
                .cwd(cwd.unwrap_or(&self.root))
                .timeout_secs(300),
        )
    }

    /// Run gh with something on its stdin.
    ///
    /// A tracker body is far too long to pass on argv, and `--body-file -` is
    /// how gh takes one. `proc::exec` already wires the pipe, so this is a
    /// sibling of `gh_at` rather than anything new.
    pub fn gh_stdin(&self, args: &[&str], stdin: &str) -> Result<String> {
        let mut argv = vec!["gh".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        proc::run(
            &argv,
            &ExecOpts::new()
                .cwd(&self.root)
                .timeout_secs(300)
                .stdin(stdin),
        )
    }

    pub fn gh_try(&self, args: &[&str]) -> String {
        let mut argv = vec!["gh".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        proc::run(
            &argv,
            &ExecOpts::new()
                .cwd(&self.root)
                .check(false)
                .timeout_secs(300),
        )
        .unwrap_or_default()
    }

    /// The login `gh` is authenticated as.
    ///
    /// A hard error, never a degradation. Everything spar wrote has to be
    /// excluded from what it answers, and custody cannot be read from git
    /// authorship, so this is the only thing that tells spar's own comments
    /// from somebody else's. Without it the failure is not "answers a bit too
    /// much", it is a thread where spar answers itself until somebody notices.
    ///
    /// Not cached on disk: `gh auth switch` between runs would make a stored
    /// answer wrong in exactly the way that produces that thread.
    pub fn viewer_login(&self) -> Result<&str> {
        if let Some(login) = self.viewer.get() {
            return Ok(login);
        }
        let rest = self.gh_try(&["api", "user", "--jq", ".login"]);
        let login = if !rest.trim().is_empty() {
            rest.trim().to_string()
        } else {
            // A token that cannot read /user can still answer for itself in
            // GraphQL, which is the case on some Enterprise installs.
            self.gh(&[
                "api",
                "graphql",
                "-f",
                "query={ viewer { login } }",
                "--jq",
                ".data.viewer.login",
            ])
            .map_err(|e| {
                spar_err!(
                    "could not find out who `gh` is authenticated as, so spar cannot tell its \
                     own comments from anybody else's. {}\nRun `gh auth status`.",
                    e.last_line()
                )
            })?
            .trim()
            .to_string()
        };
        if login.is_empty() {
            bail!("`gh` reported an empty login. Run `gh auth status`.");
        }
        Ok(self.viewer.get_or_init(|| login))
    }

    /// One issue as it stands, open or closed.
    ///
    /// `fetch_issues` reads a queue to work: it drops a closed issue and fails
    /// when nothing survives. Both are wrong for reading one issue back, where
    /// closed is an answer and the empty case cannot arise.
    pub fn read_issue(&self, number: i64) -> Result<Issue> {
        let text = self
            .gh(&[
                "issue",
                "view",
                &number.to_string(),
                "--json",
                "number,title,body,labels,state,url",
            ])
            .map_err(|e| spar_err!("could not read issue #{number}: {}", e.last_line()))?;
        serde_json::from_str(&text)
            .map_err(|e| spar_err!("unexpected shape for issue #{number}: {e}"))
    }

    pub fn fetch_issues(&self, numbers: &[i64]) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        for number in numbers {
            let text = self
                .gh(&[
                    "issue",
                    "view",
                    &number.to_string(),
                    "--json",
                    "number,title,body,labels,state,url",
                ])
                .map_err(|e| spar_err!("could not read issue #{number}: {}", e.last_line()))?;
            let issue: Issue = serde_json::from_str(&text)
                .map_err(|e| spar_err!("unexpected shape for issue #{number}: {e}"))?;
            if issue.is_closed() {
                crate::log!("issue #{number} is closed, skipping");
                continue;
            }
            issues.push(issue);
        }
        if issues.is_empty() {
            bail!("no open issues to work on");
        }
        Ok(issues)
    }

    /// Open items, lowest numbered first, from `min_number` upward.
    ///
    /// The floor exists because a long lived repository accumulates a tail of
    /// old issues nobody is going to get to, and taking the lowest numbered
    /// open items means walking straight into them.
    fn open_numbers(&self, kind: &str, limit: usize, min_number: i64) -> Result<Vec<i64>> {
        #[derive(Deserialize)]
        struct Row {
            number: i64,
        }
        let text = self.gh(&[
            kind,
            "list",
            "--state",
            "open",
            "--limit",
            &FETCH_CEILING.to_string(),
            "--json",
            "number",
        ])?;
        let rows: Vec<Row> = serde_json::from_str(text.trim()).unwrap_or_default();
        let mut numbers: Vec<i64> = rows.into_iter().map(|r| r.number).collect();
        numbers.sort_unstable();

        let noun = if kind == "issue" { "issues" } else { "PRs" };
        let found = numbers.len();
        if min_number > 0 {
            numbers.retain(|n| *n >= min_number);
            let skipped = found - numbers.len();
            if skipped > 0 {
                crate::log!("{skipped} open {noun} below #{min_number} skipped");
            }
        }
        if found >= FETCH_CEILING {
            crate::log!(
                "more than {FETCH_CEILING} open {noun}; only the first {FETCH_CEILING} were \
                 considered."
            );
        }
        if numbers.len() > limit {
            crate::log!(
                "{} open {noun}, taking the {limit} lowest numbered. Raise --limit or name them \
                 explicitly.",
                numbers.len()
            );
            numbers.truncate(limit);
        }
        Ok(numbers)
    }

    /// Open issues, lowest numbered first. `gh issue list` excludes PRs.
    pub fn list_open_issues(&self, limit: usize, min_number: i64) -> Result<Vec<i64>> {
        self.open_numbers("issue", limit, min_number)
    }

    pub fn list_open_prs(&self, limit: usize, min_number: i64) -> Result<Vec<i64>> {
        self.open_numbers("pr", limit, min_number)
    }

    pub fn pr_for_branch(&self, branch: &str) -> Option<PrRef> {
        self.branch_prs(branch, "open").into_iter().next()
    }

    /// The open pull request for a branch, preserving a failed lookup as an
    /// error when the caller is deciding whether a write already landed.
    pub fn try_pr_for_branch(&self, branch: &str, base: &str) -> Result<Option<PrRef>> {
        let text = self.gh(&[
            "pr",
            "list",
            "--head",
            branch,
            "--base",
            base,
            "--state",
            "open",
            "--json",
            "number,url,title,baseRefName",
        ])?;
        pr_for_base(&text, branch, base)
    }

    /// Every pull request opened from this branch, merged and closed ones
    /// included, because a commit is preserved by whichever one carries it and
    /// that is rarely the newest.
    fn prs_for_branch(&self, branch: &str) -> Vec<PrRef> {
        self.branch_prs(branch, "all")
    }

    fn branch_prs(&self, branch: &str, state: &str) -> Vec<PrRef> {
        let text = self.gh_try(&[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            state,
            "--json",
            "number,url,title",
        ]);
        serde_json::from_str::<Vec<PrRef>>(text.trim()).unwrap_or_default()
    }

    /// Whether a number names an issue or a pull request.
    ///
    /// `gh issue view` happily returns a pull request when handed its number,
    /// so it cannot be used to tell them apart. The issues API carries both and
    /// marks a pull request with a `pull_request` key, which is definitive.
    pub fn item_kind(&self, number: i64) -> Result<ItemKind> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{number}");
        let text = self
            .gh(&[
                "api",
                &path,
                "--jq",
                "if .pull_request then \"pr\" else \"issue\" end",
            ])
            .map_err(|e| {
                spar_err!(
                    "no issue or pull request #{number} in this repository. {}",
                    e.last_line()
                )
            })?;
        match text.trim() {
            "pr" => Ok(ItemKind::Pr),
            "issue" => Ok(ItemKind::Issue),
            other => Err(spar_err!(
                "could not tell whether #{number} is an issue or a pull request (got {other:?})"
            )),
        }
    }

    /// An open pull request that would close this issue, whoever opened it.
    ///
    /// spar's own branch naming is checked first because it is exact and cheap.
    /// Falling back to GitHub's own issue linkage is what lets spar pick up a
    /// pull request a person started on a branch named anything at all.
    pub fn open_pr_for_issue(&self, issue: i64) -> Option<PrRef> {
        if let Some(pr) = self.pr_for_branch(&self.branch_for_issue(issue)) {
            return Some(pr);
        }
        let text = self.gh_try(&[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &FETCH_CEILING.to_string(),
            "--json",
            "number,url,title,closingIssuesReferences",
        ]);
        find_linked_pr(&text, issue)
    }

    pub fn pr_view(&self, number: i64) -> Result<PrView> {
        let text = self.gh(&[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "number,url,title,headRefName,baseRefName,state,closingIssuesReferences,isCrossRepository",
        ])?;
        serde_json::from_str(&text).map_err(|e| spar_err!("unexpected shape for PR #{number}: {e}"))
    }

    fn try_pr_state(&self, number: i64) -> Result<String> {
        let text = self.gh(&["pr", "view", &number.to_string(), "--json", "state"])?;
        serde_json::from_str::<Value>(text.trim())
            .map_err(|e| spar_err!("unexpected shape for PR #{number}: {e}"))?
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| spar_err!("PR #{number} did not include a state"))
    }

    pub fn pr_state(&self, number: i64) -> String {
        self.try_pr_state(number).unwrap_or_default()
    }

    /// The commit currently exposed as a pull request's head.
    pub fn pr_head_oid(&self, number: i64) -> Result<String> {
        let text = self.gh(&["pr", "view", &number.to_string(), "--json", "headRefOid"])?;
        let oid = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("headRefOid")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|oid| !oid.is_empty())
                    .map(str::to_string)
            })
            .ok_or_else(|| spar_err!("could not read the head commit for PR #{number}"))?;
        Ok(oid)
    }

    pub fn create_pr(
        &self,
        cwd: &Path,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrRef> {
        let title = self.record_failed_write(self.clean_title(title))?;
        let body = self.record_failed_write(self.clean(body))?;
        let mut argv = vec![
            "pr", "create", "--base", base, "--head", branch, "--title", &title, "--body", &body,
        ];
        if self.drafts != Drafts::Never {
            argv.push("--draft");
        }
        let created = self.gh_at(Some(cwd), &argv);
        let found = self.try_pr_for_branch(branch, base);
        self.record_write(reconcile_pr_creation(branch, created, found))
    }

    pub fn comment_pr(&self, number: i64, body: &str) -> Result<()> {
        let body = self.record_failed_write(self.clean(body))?;
        let comments = self.record_failed_write(self.try_issue_comments(number))?;
        if has_exact_comment(&comments, &body) {
            return Ok(());
        }
        let posted = self.gh(&["pr", "comment", &number.to_string(), "--body", &body]);
        let result = match posted {
            Ok(_) => Ok(()),
            Err(post_error) => {
                reconcile_comment_post(number, &body, post_error, self.try_issue_comments(number))
            }
        };
        self.record_write(result)
    }

    pub fn comment_issue(&self, number: i64, body: &str) -> Result<()> {
        let body = self.record_failed_write(self.clean(body))?;
        let comments = self.record_failed_write(self.try_issue_comments(number))?;
        if has_exact_comment(&comments, &body) {
            return Ok(());
        }
        let posted = self.gh(&["issue", "comment", &number.to_string(), "--body", &body]);
        let result = match posted {
            Ok(_) => Ok(()),
            Err(post_error) => {
                reconcile_comment_post(number, &body, post_error, self.try_issue_comments(number))
            }
        };
        self.record_write(result)
    }

    /// Comment, then close as not planned.
    ///
    /// Only ever called when both agents independently declined the issue: one
    /// agent's opinion is not enough to close somebody's report.
    pub fn close_issue(&self, number: i64, body: &str) -> Result<()> {
        self.comment_issue(number, body)?;
        let n = number.to_string();
        let closed = match self.gh(&["issue", "close", &n, "--reason", "not planned"]) {
            Ok(_) => Ok(()),
            // Older gh builds do not take --reason.
            Err(_) => self.gh(&["issue", "close", &n]).map(|_| ()).map_err(|e| {
                spar_err!(
                    "commented on #{number} but could not close it: {}",
                    e.last_line()
                )
            }),
        };
        self.record_write(closed)
    }

    /// Replace an issue body, refusing unless it is still byte for byte what
    /// the caller read and validating only the fragment spar inserted.
    ///
    /// The only place spar rewrites text somebody else wrote, so the check is
    /// the whole point: an edit computed from a body that has since moved would
    /// silently delete whatever moved it. The caller decides whether another
    /// attempt is safe for its workflow.
    ///
    /// Deliberately not through `clean_issue_body`. The body is mostly a
    /// person's own prose, and the scrub would rewrite their punctuation while
    /// the length budget could truncate the end of a long report. `inserted` is
    /// the only text here spar is answerable for, so it still passes through the
    /// style gate. The full body travels over stdin because a tracker can be far
    /// too long for one argument.
    pub fn edit_issue_body(
        &self,
        number: i64,
        expected: &str,
        body: &str,
        inserted: &str,
    ) -> Result<()> {
        let cleaned = self.record_failed_write(self.clean(inserted))?;
        if cleaned.trim() != inserted.trim() {
            return self.record_failed_write(Err(spar_err!(
                "the style gate rewrote {inserted:?} to {cleaned:?}, so it is not being inserted"
            )));
        }
        let current = self.record_failed_write(self.issue_body(number))?;
        if current != expected {
            return self.record_failed_write(Err(spar_err!(
                "the body of #{number} changed since it was read, so it was left alone rather \
                 than written over."
            )));
        }
        let edited = self.gh_stdin(
            &["issue", "edit", &number.to_string(), "--body-file", "-"],
            body,
        );
        let result = match edited {
            Ok(_) => Ok(()),
            Err(edit_error) => {
                reconcile_issue_edit(number, body, edit_error, self.issue_body(number))
            }
        };
        self.record_write(result)
    }

    /// One issue's body, exactly as GitHub holds it.
    pub fn issue_body(&self, number: i64) -> Result<String> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            body: Option<String>,
        }
        let text = self.gh(&["issue", "view", &number.to_string(), "--json", "body"])?;
        let row: Row = serde_json::from_str(text.trim())
            .map_err(|e| spar_err!("unexpected shape for issue #{number}: {e}"))?;
        Ok(row.body.unwrap_or_default())
    }

    /// Every open issue with its title and body, in one call.
    ///
    /// For a screen that has to say something about each of twenty items before
    /// anything expensive happens. One call rather than one per issue.
    pub fn open_issue_rows(&self) -> Vec<Issue> {
        let text = self.gh_try(&[
            "issue",
            "list",
            "--state",
            "open",
            "--limit",
            &FETCH_CEILING.to_string(),
            "--json",
            "number,title,body,labels,state,url",
        ]);
        serde_json::from_str::<Vec<Issue>>(text.trim()).unwrap_or_default()
    }

    /// Every open pull request with its size, in one call.
    pub fn open_pr_rows(&self) -> Vec<PrRow> {
        let text = self.gh_try(&[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &FETCH_CEILING.to_string(),
            "--json",
            "number,title,changedFiles,additions,deletions",
        ]);
        serde_json::from_str::<Vec<PrRow>>(text.trim()).unwrap_or_default()
    }

    pub fn create_issue(&self, title: &str, body: &str) -> Result<String> {
        self.create_issue_apart_from(title, body, None)
    }

    pub fn create_issue_apart_from(
        &self,
        title: &str,
        body: &str,
        apart_from: Option<i64>,
    ) -> Result<String> {
        let title = self.record_failed_write(self.clean_title(title))?;
        let body = self.record_failed_write(self.clean_issue_body(body))?;
        let created = self.gh(&["issue", "create", "--title", &title, "--body", &body]);
        let result = match created {
            Ok(url) if issue_url_has_number(&url) => Ok(url.trim().to_string()),
            created => {
                let found = self.try_exact_issue_apart_from(&title, &body, apart_from);
                reconcile_issue_creation(&title, created, found)
            }
        };
        self.record_write(result)
    }
}

/// An issue that already covers what spar was about to file.
#[derive(Debug, Clone)]
pub struct ExistingIssue {
    pub number: i64,
    pub url: String,
    pub title: String,
    pub body: String,
    pub open: bool,
}

impl Repo {
    pub(crate) fn try_exact_issue_apart_from(
        &self,
        title: &str,
        body: &str,
        apart_from: Option<i64>,
    ) -> Result<Option<ExistingIssue>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            number: i64,
            #[serde(default)]
            title: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            state: String,
        }

        let text = self.gh(&[
            "issue",
            "list",
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "number,title,url,body,state",
        ])?;
        let rows = serde_json::from_str::<Vec<Row>>(text.trim())
            .map_err(|e| spar_err!("unexpected issue list while verifying {title:?}: {e}"))?;
        Ok(rows
            .into_iter()
            .filter(|row| Some(row.number) != apart_from)
            .find(|row| row.title == title && row.body.as_deref().unwrap_or_default() == body)
            .map(|row| ExistingIssue {
                number: row.number,
                url: row.url,
                title: row.title,
                body: row.body.unwrap_or_default(),
                open: row.state.eq_ignore_ascii_case("open"),
            }))
    }

    /// An issue that already describes this defect, however it was worded.
    ///
    /// Exact title matching let duplicates through: two agents, or two runs a
    /// week apart, never word one defect identically. A real run filed two
    /// duplicates that way, and each had to be closed by hand afterwards.
    /// Titles alone are too thin to match on, so this compares titles and
    /// bodies together.
    pub fn find_similar_issue(&self, title: &str, body: &str) -> Option<ExistingIssue> {
        self.find_similar_issue_apart_from(title, body, None)
    }

    /// The same search, with one issue that cannot be its own duplicate.
    ///
    /// A tracker's body quotes every item in its checklist, so searching for an
    /// item's words matches the tracker before it matches anything else. That
    /// would link an item to the issue it is written in.
    pub fn find_similar_issue_apart_from(
        &self,
        title: &str,
        body: &str,
        apart_from: Option<i64>,
    ) -> Option<ExistingIssue> {
        self.try_find_similar_issue_apart_from(title, body, apart_from)
            .ok()
            .flatten()
    }

    /// The same search, preserving lookup failure for a caller about to write.
    pub fn try_find_similar_issue_apart_from(
        &self,
        title: &str,
        body: &str,
        apart_from: Option<i64>,
    ) -> Result<Option<ExistingIssue>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            number: i64,
            #[serde(default)]
            title: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            body: String,
            #[serde(default)]
            state: String,
        }
        if title.trim().is_empty() {
            return Ok(None);
        }
        // Search on the title's own words: GitHub's index is the cheap way to
        // narrow the field before comparing properly.
        let query: String = title
            .chars()
            .filter(|c| !matches!(c, '"' | '\'' | '\n' | '\r'))
            .take(120)
            .collect();
        let text = self.gh(&[
            "issue",
            "list",
            "--state",
            "all",
            "--limit",
            "100",
            "--search",
            query.trim(),
            "--json",
            "number,title,url,body,state",
        ])?;
        let rows: Vec<Row> = serde_json::from_str(text.trim())
            .map_err(|e| spar_err!("unexpected issue search for {title:?}: {e}"))?;
        let wanted = format!("{title} {body}");

        Ok(rows
            .into_iter()
            .filter(|row| Some(row.number) != apart_from)
            .find(|row| {
                let theirs = format!("{} {}", row.title, row.body);
                row.title.trim().eq_ignore_ascii_case(title.trim())
                    || textsim::same_subject(&wanted, &theirs)
            })
            .map(|row| ExistingIssue {
                number: row.number,
                url: row.url,
                title: row.title,
                open: row.state.eq_ignore_ascii_case("open"),
                body: row.body,
            }))
    }

    /// Avoid filing a duplicate when a follow-up already exists.
    pub fn find_issue_by_title(&self, title: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Row {
            title: String,
            url: String,
        }
        let needle = title.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        // Quotes and newlines would be read as search syntax rather than text.
        let query: String = title
            .chars()
            .filter(|c| !matches!(c, '"' | '\'' | '\n' | '\r'))
            .take(120)
            .collect();
        let text = self.gh_try(&[
            "issue",
            "list",
            "--state",
            "all",
            "--limit",
            "100",
            "--search",
            query.trim(),
            "--json",
            "number,title,url",
        ]);
        serde_json::from_str::<Vec<Row>>(text.trim())
            .ok()?
            .into_iter()
            .find(|row| row.title.trim().to_lowercase() == needle)
            .map(|row| row.url)
    }

    /// Squash merge, tolerating cleanup failures after a successful merge.
    ///
    /// Take a pull request out of draft, once the review has converged.
    ///
    /// Best effort for the remaining workflow. A failure does not discard the
    /// review or stop later independent work, but the final write summary
    /// reports it and the command returns non-zero.
    pub fn mark_ready(&self, number: i64) -> bool {
        match self.record_write(self.gh(&["pr", "ready", &number.to_string()])) {
            Ok(_) => true,
            Err(e) => {
                logdim!(
                    "PR #{number} is approved but could not be taken out of draft: {}",
                    e.last_line()
                );
                false
            }
        }
    }

    /// `gh pr merge --delete-branch` exits non-zero when it cannot delete the
    /// local branch, which happens *after* the merge has already landed.
    /// Treating that as a failure reports work as lost when it is not.
    pub fn merge_pr(&self, number: i64) -> Result<()> {
        let n = number.to_string();
        let merged = match self.gh(&merge_pr_args(&n, None, true)) {
            Ok(_) => Ok(()),
            Err(e) => {
                if self.pr_state(number) == "MERGED" {
                    logdim!(
                        "PR #{number} merged; branch cleanup did not finish: {}",
                        e.last_line()
                    );
                    Ok(())
                } else {
                    Err(spar_err!("could not merge PR #{number}. {}", e.last_line()))
                }
            }
        };
        self.record_write(merged)
    }

    /// Squash merge only if the pull request still exposes the reviewed head.
    pub fn merge_pr_at_head(
        &self,
        number: i64,
        expected_head: &str,
        delete_branch: bool,
    ) -> Result<()> {
        let n = number.to_string();
        let merged = match self.gh(&merge_pr_args(&n, Some(expected_head), delete_branch)) {
            Ok(_) => Ok(()),
            Err(e) => {
                if self.pr_state(number) == "MERGED" {
                    logdim!(
                        "PR #{number} merged; branch cleanup did not finish: {}",
                        e.last_line()
                    );
                    Ok(())
                } else {
                    Err(spar_err!("could not merge PR #{number}. {}", e.last_line()))
                }
            }
        };
        self.record_write(merged)
    }

    // -- follow-ups -------------------------------------------------------

    /// The queue of follow-ups recorded locally rather than filed, which
    /// `spar followup` works.
    pub fn followups_path(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("followups.md")
    }

    /// What `spar followup` already dealt with, kept beside the queue.
    ///
    /// Two jobs. It is what stops `append_local_followup` re-recording a
    /// follow-up whose entry has since left the queue, which would otherwise
    /// turn the file into a ring buffer of things already filed. And it keeps
    /// the text of an entry a screening pass ruled stale, so a wrong verdict
    /// costs a re-read rather than the only copy of a real defect.
    pub fn worked_followups_path(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("followups.done.md")
    }

    /// What `spar checkin` has already answered on one pull request or issue.
    pub fn checkin_state_path(&self, number: i64) -> PathBuf {
        self.root
            .join(STATE_DIR)
            .join("state")
            .join(format!("checkin-{number}.json"))
    }

    /// Append a follow-up to a local note instead of the tracker.
    ///
    /// Deduplicated on the title, matching the issue path. The body arrives
    /// with its provenance already stamped by the caller, so nothing is added
    /// here.
    ///
    /// A write that did not happen is reported as such rather than as a
    /// duplicate: the caller settles the point on the strength of this answer,
    /// and settling it on a failed write is how a real defect is lost.
    ///
    /// Both files are checked, because `spar followup` removes an entry from
    /// the queue once it has filed it. Checking only the queue would let the
    /// next run that rediscovers the same defect append it again, on top of the
    /// issue that now exists for it.
    pub fn append_local_followup(&self, title: &str, body: &str) -> Followup {
        let path = self.followups_path();
        let heading = format!("## {}", title.trim());
        for seen in [&path, &self.worked_followups_path()] {
            if let Ok(existing) = std::fs::read_to_string(seen) {
                if existing.contains(&heading) {
                    logdim!("follow-up already noted: {title}");
                    return Followup::Covered(format!("note: {}", title.trim()));
                }
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        // The caller already stamped the provenance into the body. Adding
        // "From #N." here as well printed it twice, in two different wordings.
        //
        // The marker above the heading is what makes the entry boundary
        // unambiguous to the parser, since the body's own sections are written
        // at the same heading level as the title.
        let entry = format!("{FOLLOWUP_MARKER}\n{heading}\n\n{}\n\n", body.trim());
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(mut file) => match file.write_all(entry.as_bytes()) {
                Ok(()) => Followup::Recorded(format!("note: {}", title.trim())),
                Err(e) => {
                    logdim!("could not write {}: {e}", path.display());
                    Followup::Failed
                }
            },
            Err(e) => {
                logdim!("could not write {}: {e}", path.display());
                Followup::Failed
            }
        }
    }

    /// Record what `spar followup` did with an entry, and why.
    ///
    /// Best effort: an archive that could not be written is not a reason to
    /// stop, since the entry has already been filed or ruled on.
    pub fn archive_followup(&self, title: &str, body: &str, verdict: &str) {
        let path = self.worked_followups_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        let entry = format!(
            "{FOLLOWUP_MARKER}\n## {}\n\n{verdict}\n\n{}\n\n",
            title.trim(),
            body.trim()
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(entry.as_bytes());
        }
    }

    // -- resumable state --------------------------------------------------
    //
    // Custody cannot be read from GitHub authorship: every agent commits and
    // comments as the same git identity, so `author` is always the human who
    // ran spar. State is kept on disk by default and can additionally travel in
    // a PR comment, which is what lets a run be resumed from another machine.

    /// Where a comment spar produced but did not post is kept.
    pub fn pending_comment_path(&self, number: i64) -> PathBuf {
        self.root
            .join(STATE_DIR)
            .join("reviews")
            .join(format!("pr-{number}.md"))
    }

    /// Keep a comment spar decided not to post.
    ///
    /// A dry run that prints and forgets means agreeing with what you read
    /// costs a second full review. Saving it makes the whole point of reading
    /// it first: look, edit if you like, then post what you already paid for.
    pub fn save_pending_comment(&self, number: i64, text: &str) -> Result<PathBuf> {
        let path = self.pending_comment_path(number);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| spar_err!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text)
            .map_err(|e| spar_err!("could not write {}: {e}", path.display()))?;
        Ok(path)
    }

    pub fn read_pending_comment(&self, number: i64) -> Option<String> {
        std::fs::read_to_string(self.pending_comment_path(number)).ok()
    }

    pub fn state_path(&self, number: i64) -> PathBuf {
        self.root
            .join(STATE_DIR)
            .join("state")
            .join(format!("pr-{number}.json"))
    }

    fn read_local_state(&self, number: i64) -> Option<PersistedState> {
        let path = self.state_path(number);
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&text) {
            Ok(state) => Some(state),
            Err(_) => {
                logdim!("could not read {}, starting fresh", path.display());
                None
            }
        }
    }

    pub fn read_state(&self, pr: &PrView) -> Option<PersistedState> {
        if let Some(local) = self.read_local_state(pr.number) {
            return Some(local);
        }
        if self.state_store.writes_pr() {
            return self.read_pr_state(pr.number);
        }
        None
    }

    pub(crate) fn read_state_for_head(
        &self,
        pr: &PrView,
        actual_head: &str,
    ) -> Option<PersistedState> {
        let local = self
            .state_store
            .writes_local()
            .then(|| self.read_local_state(pr.number))
            .flatten();
        let remote = self
            .state_store
            .writes_pr()
            .then(|| self.read_pr_state(pr.number))
            .flatten();
        let candidates: Vec<PersistedState> = [local, remote].into_iter().flatten().collect();
        if let Some(checkpoint) = candidates.iter().map(|state| state.checkpoint).max() {
            self.remember_checkpoint(pr.number, checkpoint);
        }
        choose_state_for_head(candidates, actual_head)
    }

    fn read_pr_state(&self, number: i64) -> Option<PersistedState> {
        self.try_read_pr_state(number).ok().flatten()
    }

    fn try_read_pr_state(&self, number: i64) -> Result<Option<PersistedState>> {
        for (_, body) in self.try_state_comments(number)?.into_iter().rev() {
            if let Some(state) = parse_state_comment(&body) {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }

    pub fn write_state(&self, number: i64, state: &PersistedState) -> Result<()> {
        let remote_state = if self.state_store.writes_pr() {
            self.try_read_pr_state(number)
        } else {
            Ok(None)
        };
        self.write_state_after_remote_read(number, state, remote_state)
    }

    fn write_state_after_remote_read(
        &self,
        number: i64,
        state: &PersistedState,
        remote_state: Result<Option<PersistedState>>,
    ) -> Result<()> {
        let remote_checkpoint = if self.state_store.writes_pr() {
            self.record_failed_write(remote_state)?
                .map(|saved| saved.checkpoint)
                .unwrap_or_default()
        } else {
            0
        };
        let local_checkpoint = self
            .state_store
            .writes_local()
            .then(|| self.read_local_state(number))
            .flatten()
            .map(|saved| saved.checkpoint)
            .unwrap_or_default();
        let mut stamped = state.clone();
        stamped.checkpoint = state
            .checkpoint
            .max(local_checkpoint)
            .max(remote_checkpoint)
            .max(self.remembered_checkpoint(number))
            .saturating_add(1);
        self.remember_checkpoint(number, stamped.checkpoint);
        if self.state_store.writes_local() {
            write_json_atomic(&self.state_path(number), &stamped)?;
        }
        if self.state_store.writes_pr() {
            self.write_pr_state(number, &stamped)?;
        }
        Ok(())
    }

    fn remembered_checkpoint(&self, number: i64) -> u64 {
        self.checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&number)
            .copied()
            .unwrap_or_default()
    }

    fn remember_checkpoint(&self, number: i64, checkpoint: u64) {
        let mut checkpoints = self
            .checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = checkpoints.entry(number).or_default();
        *saved = (*saved).max(checkpoint);
    }

    fn write_pr_state(&self, number: i64, state: &PersistedState) -> Result<()> {
        // Not run through clean(): this is structured data, and scrubbing would
        // corrupt refutation text stored in the ledger. It sits inside an
        // unclosed HTML comment so GitHub renders it as nothing.
        let serialized = self.record_failed_write(serde_json::to_string_pretty(state))?;
        let body = format!("{STATE_MARKER}\n{}\n-->", serialized);
        let comment_id = self.record_failed_write(self.try_state_comment_id(number))?;
        if let Some(id) = comment_id {
            let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
            let field = format!("body={body}");
            let written = self
                .gh(&["api", "-X", "PATCH", &path, "-f", &field, "--silent"])
                .map(|_| ());
            return self.record_write(written);
        }
        let written = self
            .gh(&["pr", "comment", &number.to_string(), "--body", &body])
            .map(|_| ());
        self.record_write(written)
    }

    /// Top level comments. Works for issues and pull requests alike, because
    /// GitHub serves both from the issues endpoint.
    ///
    /// Nothing when they cannot be read, which suits a reader that is going to
    /// go on regardless. A caller deciding whether it has already written here
    /// wants `try_issue_comments`, since for that one no comments and no answer
    /// are opposite answers.
    pub fn issue_comments(&self, number: i64) -> Vec<Value> {
        self.try_issue_comments(number).unwrap_or_default()
    }

    pub fn try_issue_comments(&self, number: i64) -> Result<Vec<Value>> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{number}/comments");
        try_parse_comment_pages(&self.gh(&["api", "--paginate", &path])?)
    }

    fn try_state_comments(&self, number: i64) -> Result<Vec<(i64, String)>> {
        Ok(self
            .try_issue_comments(number)?
            .into_iter()
            .filter_map(|c| {
                let body = c.get("body").and_then(Value::as_str)?.to_string();
                if !body.contains("spar:state") {
                    return None;
                }
                let id = c.get("id").and_then(Value::as_i64)?;
                Some((id, body))
            })
            .collect())
    }

    fn try_state_comment_id(&self, number: i64) -> Result<Option<i64>> {
        Ok(self.try_state_comments(number)?.last().map(|(id, _)| *id))
    }

    /// Drop state once the PR is finished and there is nothing to resume.
    pub fn clear_state(&self, number: i64) {
        let path = self.state_path(number);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }

    // -- housekeeping -----------------------------------------------------

    /// Remove state files whose PR is merged or closed.
    pub fn prune_state(&self) -> Vec<String> {
        let base = self.root.join(STATE_DIR).join("state");
        let Ok(entries) = std::fs::read_dir(&base) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.starts_with("pr-") && n.ends_with(".json"))
            .collect();
        names.sort();

        let mut removed = Vec::new();
        for name in names {
            let Ok(number) = name[3..name.len() - 5].parse::<i64>() else {
                continue;
            };
            if is_finished(&self.pr_state(number)) {
                let _ = std::fs::remove_file(base.join(&name));
                removed.push(format!("state {name}"));
            }
        }
        removed
    }

    /// Delete state comments from PRs that are finished.
    ///
    /// Open PRs are left alone: their state may still be live.
    pub fn prune_pr_state(&self, numbers: Option<Vec<i64>>) -> Vec<String> {
        #[derive(Deserialize)]
        struct Row {
            number: i64,
        }
        let numbers = match numbers {
            Some(numbers) => numbers,
            None => {
                let listed: Result<Vec<i64>> = (|| {
                    let text = self.gh(&[
                        "pr", "list", "--state", "all", "--limit", "200", "--json", "number",
                    ])?;
                    let rows = serde_json::from_str::<Vec<Row>>(text.trim())
                        .map_err(|e| spar_err!("unexpected pull request list: {e}"))?;
                    Ok(rows.into_iter().map(|row| row.number).collect())
                })();
                match self.record_failed_write(listed) {
                    Ok(numbers) => numbers,
                    Err(e) => {
                        logdim!("could not inspect pull requests for state cleanup: {e}");
                        return Vec::new();
                    }
                }
            }
        };

        let mut removed = Vec::new();
        for number in numbers {
            let state = match self.record_failed_write(self.try_pr_state(number)) {
                Ok(state) => state,
                Err(e) => {
                    logdim!("could not inspect PR #{number} for state cleanup: {e}");
                    continue;
                }
            };
            if !is_finished(&state) {
                continue;
            }
            let comments = match self.record_failed_write(self.try_state_comments(number)) {
                Ok(comments) => comments,
                Err(e) => {
                    logdim!("could not inspect state comments on PR #{number}: {e}");
                    continue;
                }
            };
            for (id, _) in comments {
                let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
                let deleted = self
                    .gh(&["api", "-X", "DELETE", &path, "--silent"])
                    .map(|_| ());
                match self.record_write(deleted) {
                    Ok(()) => removed.push(format!("state comment on PR #{number}")),
                    Err(e) => logdim!("could not remove state comment on PR #{number}: {e}"),
                }
            }
        }
        removed
    }

    /// Drop worktrees whose PR is finished, then the branches they left behind.
    ///
    /// With auto_merge off, which is the default, a run ends at "approved", so
    /// nothing would ever clean these up on its own and they accumulate one per
    /// run. A stranded worktree also holds its branch checked out, which makes
    /// a later `gh pr merge --delete-branch` fail to clean up.
    pub fn prune_worktrees(&self, force_all: bool) -> Vec<String> {
        let base = self.root.join(WORKTREE_DIR);
        let mut removed = Vec::new();
        let known = self.known_branches();

        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect();
            names.sort();

            for name in names {
                // A review worktree is detached and owns no branch, so it is
                // tied to the pull request only by its directory name.
                if let Some(rest) = name.strip_prefix("review-") {
                    let number: i64 = rest.parse().unwrap_or(-1);
                    if !(force_all || is_finished(&self.pr_state(number))) {
                        continue;
                    }
                    let path = base.join(&name);
                    if force_all {
                        let owned = self.worktree_belongs_to_repo(&path).and_then(|belongs| {
                            if !belongs {
                                return Ok(false);
                            }
                            let local_ref = review_ref(number);
                            if !self.exact_ref_exists_checked(&self.root, &local_ref)? {
                                return Ok(false);
                            }
                            let head = self.head_oid_checked(&path)?;
                            let recorded = self
                                .git_at(Some(&self.root), &["rev-parse", "--verify", &local_ref])?
                                .trim()
                                .to_string();
                            Ok(head == recorded)
                        });
                        match owned {
                            Ok(true) => {}
                            Ok(false) => {
                                logdim!(
                                    "kept {} because no matching SPAR review reference proves \
                                     ownership",
                                    path.display()
                                );
                                continue;
                            }
                            Err(e) => {
                                logdim!(
                                    "kept {} because review ownership could not be verified: {}",
                                    path.display(),
                                    e.last_line()
                                );
                                continue;
                            }
                        }
                    } else {
                        if let Err(e) = self.refuse_review_worktree_changes(number) {
                            logdim!(
                                "kept {} because its review state could not be verified as \
                                 disposable: {}",
                                path.display(),
                                e.last_line()
                            );
                            continue;
                        }
                    }
                    if force_all {
                        if self.remove_worktree_at_force(&path) {
                            self.git_try(&["update-ref", "-d", &review_ref(number)]);
                        }
                    } else {
                        self.release_review_worktree(number);
                    }
                    if !path.exists() {
                        removed.push(name);
                    }
                    continue;
                }
                let branch = format!("{}{name}", self.branch_prefix);
                if !(force_all || self.worktree_is_done(&branch)) {
                    continue;
                }
                if !known.contains_key(&branch) {
                    logdim!("kept {branch} because it has no branch record");
                    continue;
                }
                let path = base.join(&name);
                if !force_all {
                    match self.has_recoverable_work(&path) {
                        Ok(true) => {
                            logdim!(
                                "kept {} because it contains uncommitted changes or ignored files",
                                path.display()
                            );
                            continue;
                        }
                        Err(e) => {
                            logdim!(
                                "kept {} because its Git state could not be checked: {}",
                                path.display(),
                                e.last_line()
                            );
                            continue;
                        }
                        Ok(false) => {}
                    }
                    match self.branch_deletion_is_safe(&branch) {
                        Ok(true) => {}
                        Ok(false) => {
                            logdim!(
                                "kept {branch} because no surviving ref preserves its tip or \
                                 reflog-only commits"
                            );
                            continue;
                        }
                        Err(e) => {
                            logdim!(
                                "kept {branch} because preservation could not be verified: {}",
                                e.last_line()
                            );
                            continue;
                        }
                    }
                }
                let removed_worktree = if force_all {
                    self.remove_worktree_at_force(&path)
                } else {
                    match self.remove_worktree_at(&path) {
                        Ok(removed) => removed,
                        Err(error) => {
                            logdim!(
                                "kept {branch} and {} because removal did not reach a confirmed \
                                 quiet point: {}",
                                path.display(),
                                error.last_line()
                            );
                            false
                        }
                    }
                };
                if !removed_worktree {
                    continue;
                }
                if force_all {
                    self.git_try(&["branch", "-D", &branch]);
                    self.forget_branch(&branch);
                } else {
                    match self.delete_branch_if_safe(&branch) {
                        Ok(true) => self.forget_branch(&branch),
                        Ok(false) => logdim!(
                            "kept {branch} because its tip or reflog changed before deletion"
                        ),
                        Err(error) => logdim!(
                            "kept {branch} because deletion safety could not be rechecked: {}",
                            error.last_line()
                        ),
                    }
                }
                removed.push(name);
            }
        }
        removed.extend(self.prune_branches(force_all));
        removed
    }

    /// Delete leftover branches spar created whose worktree is already gone.
    ///
    /// Deletion is driven by the ledger of branches spar actually created, not
    /// by a name pattern. Names default to `issue-N`, which is exactly what a
    /// person would call a branch themselves, so a name alone can never
    /// establish ownership. This is the data loss guard.
    pub fn prune_branches(&self, force_all: bool) -> Vec<String> {
        let known = self.known_branches();
        let branches: Vec<String> = known.keys().cloned().collect();
        if branches.is_empty() {
            return Vec::new();
        }

        let checked_out: Vec<String> = self
            .git_try(&["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("branch refs/heads/").map(str::to_string))
            .collect();

        // %(refname:short) is ambiguous when a tag shares the branch name (it
        // yields "heads/..."), so take the full ref and strip it here.
        let existing: Vec<String> = self
            .git_try(&["for-each-ref", "refs/heads/", "--format=%(refname)"])
            .lines()
            .filter_map(|l| l.trim().strip_prefix("refs/heads/").map(str::to_string))
            .collect();

        let mut removed = Vec::new();
        for branch in branches {
            if !existing.contains(&branch) {
                self.forget_branch(&branch); // already gone, drop the record
                continue;
            }
            if checked_out.contains(&branch) {
                continue;
            }
            if !(force_all || self.worktree_is_done(&branch)) {
                continue;
            }
            if !force_all {
                let Some(_record) = known.get(&branch) else {
                    continue;
                };
                match self.branch_deletion_is_safe(&branch) {
                    Ok(true) => {}
                    Ok(false) => {
                        logdim!(
                            "kept {branch} because no surviving ref preserves its tip or \
                             reflog-only commits"
                        );
                        continue;
                    }
                    Err(e) => {
                        logdim!(
                            "kept {branch} because preservation could not be verified: {}",
                            e.last_line()
                        );
                        continue;
                    }
                }
            }
            let deleted = if force_all {
                self.git(&["branch", "-D", &branch]).map(|_| true)
            } else {
                self.delete_branch_if_safe(&branch)
            };
            match deleted {
                Ok(true) => {
                    self.forget_branch(&branch);
                    removed.push(format!("branch {branch}"));
                }
                Ok(false) => {
                    logdim!("kept {branch} because its tip or reflog changed before deletion");
                }
                Err(e) => {
                    // A branch that silently survives pruning looks like a spar
                    // bug, so the name and git's own reason have to be said.
                    logdim!("could not delete {branch}: {}", e.last_line());
                }
            }
        }
        removed
    }

    /// True when the PR behind this branch is merged or closed.
    fn worktree_is_done(&self, branch: &str) -> bool {
        #[derive(Deserialize)]
        struct Row {
            state: String,
        }
        let entry = branch
            .strip_prefix(self.branch_prefix.as_str())
            .unwrap_or(branch);
        if let Some(rest) = entry.strip_prefix("pr-") {
            return is_finished(&self.pr_state(rest.parse().unwrap_or(-1)));
        }
        // A split part is the same shape as an issue branch: one branch, whose
        // pull requests say whether it is finished. Without it here, a part
        // branch is one nothing but `clean --all` would ever remove.
        if entry.starts_with("issue-") || entry.starts_with("split-") {
            let text = self.gh_try(&[
                "pr", "list", "--head", branch, "--state", "all", "--json", "state",
            ]);
            let rows: Vec<Row> = serde_json::from_str(text.trim()).unwrap_or_default();
            return !rows.is_empty() && rows.iter().all(|r| is_finished(&r.state));
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Read attribute files without asking Git to inspect working-tree content.
///
/// A newly written attribute can select a clean or smudge filter. It must be
/// detected before a post-call status, diff, or add command has a chance to run
/// that filter in the parent process.
pub(crate) fn attribute_state(cwd: &Path) -> Result<AttributeState> {
    let root = std::fs::canonicalize(cwd)
        .map_err(|e| spar_err!("could not resolve {}: {e}", cwd.display()))?;
    let mut files = BTreeMap::new();
    let mut visited = BTreeSet::new();
    collect_attribute_files(&root, &root, Path::new(""), &mut visited, &mut files)?;
    Ok(AttributeState { files })
}

fn collect_attribute_files(
    root: &Path,
    repository: &Path,
    prefix: &Path,
    visited: &mut BTreeSet<PathBuf>,
    files: &mut BTreeMap<PathBuf, [u8; 32]>,
) -> Result<()> {
    let canonical = std::fs::canonicalize(repository)
        .map_err(|e| spar_err!("could not resolve {}: {e}", repository.display()))?;
    if !visited.insert(canonical) {
        bail!("submodule recursion revisited {}", repository.display());
    }
    let entries = index_entries(repository)?;
    let mut paths: BTreeSet<PathBuf> = entries
        .iter()
        .filter(|entry| entry.path.file_name() == Some(OsStr::new(".gitattributes")))
        .map(|entry| entry.path.clone())
        .collect();
    let untracked = run_git_bytes(
        repository,
        &[
            "ls-files",
            "--others",
            "-z",
            "--",
            ".gitattributes",
            ":(glob)**/.gitattributes",
        ],
    )?;
    if !untracked.is_empty() && !untracked.ends_with(&[0]) {
        bail!(
            "git returned an unterminated attribute-file listing for {}",
            repository.display()
        );
    }
    for raw in untracked
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        paths.insert(safe_git_path(raw, "attribute")?);
    }
    for path in paths {
        let from_root = prefix.join(&path);
        let state = attribute_file_fingerprint(&root.join(&from_root))?;
        files.insert(from_root, state);
    }
    for entry in entries.into_iter().filter(|entry| entry.mode == "160000") {
        let Some(submodule) = initialized_submodule(repository, &entry.path)? else {
            continue;
        };
        collect_attribute_files(root, &submodule, &prefix.join(&entry.path), visited, files)?;
    }
    Ok(())
}

/// Leave a visible, untracked reason ordinary cleanup can detect on a later
/// run even when Git status normalizes the original working-file change away.
pub(crate) fn uncertain_worktree_change(
    cwd: &Path,
    message: impl Into<String>,
) -> crate::error::SparError {
    let message = message.into();
    let marker = write_recovery_marker(cwd, &message);
    let note = match marker {
        Ok(path) => format!(" Recovery marker: {}.", path.display()),
        Err(e) => format!(
            " A recovery marker could not be written: {}.",
            e.last_line()
        ),
    };
    crate::error::SparError::uncertain_write(format!("{message}{note}"))
}

fn write_recovery_marker(cwd: &Path, detail: &str) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    for _ in 0..1000 {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = cwd.join(format!(
            ".spar-recovery-needed-{}-{serial}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(detail.as_bytes())
                    .and_then(|_| file.write_all(b"\n"))
                    .map_err(|e| spar_err!("could not write {}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(spar_err!(
                    "could not create a recovery marker in {}: {e}",
                    cwd.display()
                ))
            }
        }
    }
    bail!(
        "could not choose a free recovery marker name in {}",
        cwd.display()
    )
}

/// Build a Git command that cannot launch automatic repository maintenance.
///
/// A fetch may otherwise prune missing linked worktree registrations. SPAR
/// must only remove registrations it has proven it owns.
fn git_without_maintenance_argv(args: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        "maintenance.auto=false".to_string(),
        "-c".to_string(),
        "gc.auto=0".to_string(),
    ];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    argv
}

fn git_without_automation_argv(args: &[&str]) -> Vec<String> {
    let mut argv = git_without_maintenance_argv(&[]);
    argv.extend([
        "-c".to_string(),
        "core.fsmonitor=".to_string(),
        "-c".to_string(),
        "commit.gpgsign=false".to_string(),
        "-c".to_string(),
        "core.hooksPath=/dev/null".to_string(),
    ]);
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    argv
}

/// Snapshot every untracked file, including ignored files, without changing
/// path bytes.
///
/// Without an exclude option, Git lists both ordinary and ignored untracked
/// entries. Metadata fingerprints make overwriting an existing path observable
/// without hashing a potentially multi-gigabyte build tree on every call.
pub(crate) fn ignored_untracked_state(cwd: &Path) -> Result<IgnoredState> {
    let root = std::fs::canonicalize(cwd)
        .map_err(|e| spar_err!("could not resolve {}: {e}", cwd.display()))?;
    let mut files = BTreeMap::new();
    let mut ignored = BTreeSet::new();
    let mut visited = BTreeSet::new();
    collect_untracked_files(
        &root,
        &root,
        Path::new(""),
        &mut visited,
        &mut files,
        &mut ignored,
    )?;
    Ok(IgnoredState { files, ignored })
}

fn collect_untracked_files(
    root: &Path,
    repository: &Path,
    prefix: &Path,
    visited: &mut BTreeSet<PathBuf>,
    files: &mut BTreeMap<PathBuf, UntrackedFile>,
    ignored: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let canonical = std::fs::canonicalize(repository)
        .map_err(|e| spar_err!("could not resolve {}: {e}", repository.display()))?;
    if !visited.insert(canonical.clone()) {
        bail!("submodule recursion revisited {}", canonical.display());
    }
    let listed = run_git_bytes(repository, &["ls-files", "--others", "-z"])?;
    if !listed.is_empty() && !listed.ends_with(&[0]) {
        bail!(
            "git returned an unterminated untracked-file list for {}",
            repository.display()
        );
    }

    for raw in listed
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let (relative, nested) = untracked_record(raw, "untracked")?;
        let from_root = prefix.join(&relative);
        let absolute = root.join(&from_root);
        let fingerprint = if nested {
            nested_repository_fingerprint(&absolute)?
        } else {
            ignored_file_fingerprint(&absolute)?
        };
        if files.insert(from_root.clone(), fingerprint).is_some() {
            bail!(
                "git returned the untracked path more than once: {:?}",
                from_root
            );
        }
    }

    let ignored_listed = run_git_bytes(
        repository,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    if !ignored_listed.is_empty() && !ignored_listed.ends_with(&[0]) {
        bail!(
            "git returned an unterminated ignored-file list for {}",
            repository.display()
        );
    }
    for raw in ignored_listed
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let (relative, _) = untracked_record(raw, "ignored")?;
        let from_root = prefix.join(relative);
        if !files.contains_key(&from_root) {
            bail!(
                "git classified an unlisted path as ignored: {:?}",
                from_root
            );
        }
        if !ignored.insert(from_root.clone()) {
            bail!(
                "git returned the ignored path more than once: {:?}",
                from_root
            );
        }
    }

    for link in gitlinks(repository)? {
        let Some(submodule) = initialized_submodule(repository, &link.path)? else {
            continue;
        };
        collect_untracked_files(
            root,
            &submodule,
            &prefix.join(&link.path),
            visited,
            files,
            ignored,
        )?;
    }
    Ok(())
}

fn run_git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let argv = git_without_automation_argv(args);
    proc::run_bytes(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .stop_descendants(true),
    )
}

fn run_git_text(cwd: &Path, args: &[&str]) -> Result<String> {
    let argv = git_without_automation_argv(args);
    proc::run(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .stop_descendants(true),
    )
}

fn filtered_index_content(cwd: &Path, path: &Path, oid: &str) -> Result<[u8; 32]> {
    let path = path.to_str().ok_or_else(|| {
        spar_err!(
            "cannot verify filtered content for a non-UTF-8 path in {}",
            cwd.display()
        )
    })?;
    let path_arg = format!("--path={path}");
    let bytes = run_git_bytes(cwd, &["cat-file", "--filters", &path_arg, oid])?;
    Ok(Sha256::digest(bytes).into())
}

fn safe_git_path(raw: &[u8], kind: &str) -> Result<PathBuf> {
    let relative = path_from_git_bytes(raw)?;
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("git returned an unsafe {kind} path: {:?}", relative);
    }
    Ok(relative)
}

/// Split one `ls-files --others` record into its path and whether Git reported
/// a nested repository rather than a single file.
///
/// Git never lists the contents of a repository inside the working tree, so a
/// checkout parked there, such as another of SPAR's own worktrees, arrives as
/// one record for the directory itself ending in a separator. Git writes that
/// separator on every platform. Trimming it keeps the recorded path equal to
/// the same path seen any other way.
fn untracked_record(raw: &[u8], kind: &str) -> Result<(PathBuf, bool)> {
    let nested = raw.last() == Some(&b'/');
    let trimmed = if nested { &raw[..raw.len() - 1] } else { raw };
    if trimmed.is_empty() {
        bail!("git returned an empty {kind} path");
    }
    Ok((safe_git_path(trimmed, kind)?, nested))
}

fn index_entries(cwd: &Path) -> Result<Vec<IndexEntry>> {
    let listed = run_git_bytes(cwd, &["ls-files", "--stage", "-z"])?;
    if !listed.is_empty() && !listed.ends_with(&[0]) {
        bail!(
            "git returned an unterminated index listing for {}",
            cwd.display()
        );
    }
    let mut entries = Vec::new();
    for record in listed
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            bail!(
                "git returned a malformed index record for {}",
                cwd.display()
            );
        };
        let header = &record[..tab];
        let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.len() != 3 {
            bail!(
                "git returned a malformed index header for {}",
                cwd.display()
            );
        }
        if fields[2] != b"0" {
            continue;
        }
        let mode = std::str::from_utf8(fields[0])
            .map_err(|_| spar_err!("git returned a non-UTF-8 index mode"))?
            .to_string();
        let oid = std::str::from_utf8(fields[1])
            .map_err(|_| spar_err!("git returned a non-UTF-8 object id"))?
            .to_string();
        entries.push(IndexEntry {
            path: safe_git_path(&record[tab + 1..], "index")?,
            mode,
            oid,
        });
    }
    Ok(entries)
}

fn attributes_may_be_modified(cwd: &Path) -> Result<bool> {
    let untracked = run_git_bytes(
        cwd,
        &[
            "ls-files",
            "--others",
            "-z",
            "--",
            ".gitattributes",
            ":(glob)**/.gitattributes",
        ],
    )?;
    if !untracked.is_empty() {
        return Ok(true);
    }

    let index = index_entries(cwd)?
        .into_iter()
        .filter(|entry| entry.path.file_name() == Some(OsStr::new(".gitattributes")))
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect::<BTreeMap<_, _>>();
    let head = tree_entries(cwd, "HEAD")?
        .into_iter()
        .filter(|entry| entry.path.file_name() == Some(OsStr::new(".gitattributes")))
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect::<BTreeMap<_, _>>();
    if index != head {
        return Ok(true);
    }

    let effective = check_attributes(cwd, index.keys().cloned())?;
    let config = CheckoutConfig::read(cwd)?;
    for (path, (_mode, oid)) in index {
        let Some(worktree) = tracked_worktree_file(&cwd.join(&path), oid.len())? else {
            return Ok(true);
        };
        let attributes = effective
            .get(&path)
            .ok_or_else(|| spar_err!("git omitted attributes for {}", cwd.join(&path).display()))?;
        if allows_expected_crlf(&config, attributes)? {
            if worktree.mode == "120000" {
                return Ok(true);
            }
            let (normalized, every_lf_was_crlf) =
                normalized_git_blob_oid(&cwd.join(&path), oid.len())?;
            if !every_lf_was_crlf || normalized != oid {
                return Ok(true);
            }
        } else if worktree.raw_oid != oid {
            return Ok(true);
        }
    }
    Ok(false)
}

fn gitlinks(cwd: &Path) -> Result<Vec<Gitlink>> {
    Ok(index_entries(cwd)?
        .into_iter()
        .filter(|entry| entry.mode == "160000")
        .map(|entry| Gitlink {
            path: entry.path,
            oid: entry.oid,
        })
        .collect())
}

fn tracked_entries(cwd: &Path) -> Result<BTreeMap<PathBuf, TrackedEntry>> {
    let mut tracked = BTreeMap::new();
    for entry in index_entries(cwd)? {
        if entry.mode == "160000" {
            continue;
        }
        let worktree = tracked_worktree_file(&cwd.join(&entry.path), entry.oid.len())?;
        tracked.insert(
            entry.path,
            TrackedEntry {
                index_mode: entry.mode,
                index_oid: entry.oid,
                worktree,
            },
        );
    }
    Ok(tracked)
}

fn tracked_worktree_file(path: &Path, oid_len: usize) -> Result<Option<WorktreeFile>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(spar_err!(
                "could not inspect tracked file {}: {e}",
                path.display()
            ))
        }
    };
    let mut fingerprint = Sha256::new();
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path)
            .map_err(|e| spar_err!("could not read tracked symlink {}: {e}", path.display()))?;
        let bytes = os_str_bytes(target.as_os_str())?;
        fingerprint.update(b"symlink\0");
        fingerprint.update(&bytes);
        let content = Sha256::digest(&bytes).into();
        return Ok(Some(WorktreeFile {
            mode: "120000".to_string(),
            #[cfg(unix)]
            permissions: 0,
            raw_oid: git_blob_oid(oid_len, &bytes)?,
            fingerprint: fingerprint.finalize().into(),
            content,
        }));
    }
    if !metadata.is_file() {
        bail!("tracked path {} is not a file or symlink", path.display());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|e| spar_err!("could not read tracked file {}: {e}", path.display()))?;
    let before = file
        .metadata()
        .map_err(|e| spar_err!("could not inspect tracked file {}: {e}", path.display()))?;
    let mode = tracked_file_mode(&before);
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::MetadataExt;
        before.mode() & 0o7777
    };
    fingerprint.update(b"file\0");
    fingerprint.update(mode.as_bytes());
    #[cfg(unix)]
    fingerprint.update(permissions.to_le_bytes());
    fingerprint.update(before.len().to_le_bytes());
    let mut content = Sha256::new();
    let header = format!("blob {}\0", before.len());
    let mut object = ObjectHasher::new(oid_len, header.as_bytes())?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| spar_err!("could not read tracked file {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        fingerprint.update(&buf[..read]);
        content.update(&buf[..read]);
        object.update(&buf[..read]);
    }
    let after = file
        .metadata()
        .map_err(|e| spar_err!("could not recheck tracked file {}: {e}", path.display()))?;
    if before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.permissions() != after.permissions()
    {
        bail!(
            "tracked file {} changed while it was being inspected",
            path.display()
        );
    }
    let current = std::fs::symlink_metadata(path)
        .map_err(|e| spar_err!("could not recheck tracked file {}: {e}", path.display()))?;
    if !same_file(&after, &current) {
        bail!(
            "tracked file {} was replaced while it was being inspected",
            path.display()
        );
    }
    Ok(Some(WorktreeFile {
        mode,
        #[cfg(unix)]
        permissions,
        raw_oid: object.finish(),
        fingerprint: fingerprint.finalize().into(),
        content: content.finalize().into(),
    }))
}

fn attribute_file_fingerprint(path: &Path) -> Result<[u8; 32]> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| spar_err!("could not inspect attribute file {}: {e}", path.display()))?;
    let mut digest = Sha256::new();
    if metadata.file_type().is_symlink() {
        digest.update(b"symlink\0");
        let target = std::fs::read_link(path)
            .map_err(|e| spar_err!("could not read attribute symlink {}: {e}", path.display()))?;
        digest.update(os_str_bytes(target.as_os_str())?);
        return Ok(digest.finalize().into());
    }
    if !metadata.is_file() {
        bail!("attribute path {} is not a file or symlink", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|e| spar_err!("could not read attribute file {}: {e}", path.display()))?;
    let before = file
        .metadata()
        .map_err(|e| spar_err!("could not inspect attribute file {}: {e}", path.display()))?;
    digest.update(b"file\0");
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| spar_err!("could not read attribute file {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buf[..read]);
    }
    let after = file
        .metadata()
        .map_err(|e| spar_err!("could not recheck attribute file {}: {e}", path.display()))?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|e| spar_err!("could not recheck attribute file {}: {e}", path.display()))?;
    if before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || !same_file(&after, &current)
    {
        bail!(
            "attribute file {} changed while it was being inspected",
            path.display()
        );
    }
    Ok(digest.finalize().into())
}

enum ObjectHasher {
    Sha1(Sha1),
    Sha256(Sha256),
}

impl ObjectHasher {
    fn new(oid_len: usize, header: &[u8]) -> Result<Self> {
        let mut hasher = match oid_len {
            40 => Self::Sha1(<Sha1 as sha1::Digest>::new()),
            64 => Self::Sha256(Sha256::new()),
            _ => bail!("git returned an object id with an unsupported length: {oid_len}"),
        };
        hasher.update(header);
        Ok(hasher)
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hasher) => sha1::Digest::update(hasher, bytes),
            Self::Sha256(hasher) => hasher.update(bytes),
        }
    }

    fn finish(self) -> String {
        let bytes = match self {
            Self::Sha1(hasher) => sha1::Digest::finalize(hasher).to_vec(),
            Self::Sha256(hasher) => hasher.finalize().to_vec(),
        };
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

fn git_blob_oid(oid_len: usize, bytes: &[u8]) -> Result<String> {
    let header = format!("blob {}\0", bytes.len());
    let mut hasher = ObjectHasher::new(oid_len, header.as_bytes())?;
    hasher.update(bytes);
    Ok(hasher.finish())
}

fn normalized_git_blob_oid(path: &Path, oid_len: usize) -> Result<(String, bool)> {
    let mut first = open_regular_file(path)?;
    let first_before = first
        .metadata()
        .map_err(|e| spar_err!("could not inspect tracked file {}: {e}", path.display()))?;
    let mut raw_len = 0u64;
    let mut crlf_pairs = 0u64;
    let mut previous_was_cr = false;
    let mut every_lf_was_crlf = true;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = first
            .read(&mut buf)
            .map_err(|e| spar_err!("could not read tracked file {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        raw_len = raw_len
            .checked_add(read as u64)
            .ok_or_else(|| spar_err!("tracked file {} is too large", path.display()))?;
        for byte in &buf[..read] {
            if *byte == b'\n' {
                if previous_was_cr {
                    crlf_pairs += 1;
                } else {
                    every_lf_was_crlf = false;
                }
            }
            previous_was_cr = *byte == b'\r';
        }
    }
    let first_after = first
        .metadata()
        .map_err(|e| spar_err!("could not recheck tracked file {}: {e}", path.display()))?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|e| spar_err!("could not recheck tracked file {}: {e}", path.display()))?;
    if raw_len != first_before.len()
        || !stable_file_metadata(&first_before, &first_after)
        || !stable_file_metadata(&first_after, &current)
    {
        bail!(
            "tracked file {} changed while line endings were inspected",
            path.display()
        );
    }

    let normalized_len = raw_len
        .checked_sub(crlf_pairs)
        .ok_or_else(|| spar_err!("could not normalize tracked file {}", path.display()))?;
    let header = format!("blob {normalized_len}\0");
    let mut object = ObjectHasher::new(oid_len, header.as_bytes())?;
    let mut second = open_regular_file(path)?;
    let second_before = second
        .metadata()
        .map_err(|e| spar_err!("could not inspect tracked file {}: {e}", path.display()))?;
    if !stable_file_metadata(&first_after, &second_before) {
        bail!(
            "tracked file {} changed between line-ending checks",
            path.display()
        );
    }
    let mut pending_cr = false;
    loop {
        let read = second
            .read(&mut buf)
            .map_err(|e| spar_err!("could not read tracked file {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        for byte in &buf[..read] {
            if pending_cr {
                if *byte == b'\n' {
                    object.update(b"\n");
                    pending_cr = false;
                    continue;
                }
                object.update(b"\r");
                pending_cr = false;
            }
            if *byte == b'\r' {
                pending_cr = true;
            } else {
                object.update(std::slice::from_ref(byte));
            }
        }
    }
    if pending_cr {
        object.update(b"\r");
    }
    let second_after = second
        .metadata()
        .map_err(|e| spar_err!("could not recheck tracked file {}: {e}", path.display()))?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|e| spar_err!("could not recheck tracked file {}: {e}", path.display()))?;
    if !stable_file_metadata(&second_before, &second_after)
        || !stable_file_metadata(&second_after, &current)
    {
        bail!(
            "tracked file {} changed while line endings were hashed",
            path.display()
        );
    }
    Ok((object.finish(), every_lf_was_crlf))
}

fn open_regular_file(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|e| spar_err!("could not read tracked file {}: {e}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|e| spar_err!("could not inspect tracked file {}: {e}", path.display()))?;
    if !metadata.is_file() {
        bail!("tracked path {} is not a regular file", path.display());
    }
    Ok(file)
}

fn stable_file_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    if !same_file(left, right)
        || left.len() != right.len()
        || left.modified().ok() != right.modified().ok()
        || left.permissions() != right.permissions()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.ctime() == right.ctime() && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        left.created().ok() == right.created().ok()
    }
}

/// How many paths one `check-attr` call is asked about.
///
/// `check-attr` answers with three NUL fields per attribute per path, so the
/// output is several times the input. A whole monorepo in one call is megabytes
/// in both directions, and the 30 second timeout below is a timeout on the
/// batch. Batching keeps each call small enough to be honest about.
const ATTRIBUTE_BATCH: usize = 1000;

fn check_attributes(
    cwd: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<BTreeMap<PathBuf, BTreeMap<String, String>>> {
    let paths = paths.into_iter().collect::<BTreeSet<_>>();
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ordered = paths.into_iter().collect::<Vec<_>>();
    let mut values: BTreeMap<PathBuf, BTreeMap<String, String>> = BTreeMap::new();
    for batch in ordered.chunks(ATTRIBUTE_BATCH) {
        values.extend(check_attribute_batch(cwd, batch)?);
    }
    Ok(values)
}

fn check_attribute_batch(
    cwd: &Path,
    batch: &[PathBuf],
) -> Result<BTreeMap<PathBuf, BTreeMap<String, String>>> {
    const NAMES: [&str; 6] = [
        "filter",
        "working-tree-encoding",
        "ident",
        "text",
        "eol",
        "crlf",
    ];
    let paths = batch.iter().cloned().collect::<BTreeSet<_>>();
    let mut input = String::new();
    for path in &paths {
        let path = path.to_str().ok_or_else(|| {
            spar_err!(
                "cannot inspect attributes for a non-UTF-8 path in {}",
                cwd.display()
            )
        })?;
        input.push_str(path);
        input.push('\0');
    }
    let argv = git_without_automation_argv(&[
        "check-attr",
        "-z",
        "--cached",
        "--stdin",
        "filter",
        "working-tree-encoding",
        "ident",
        "text",
        "eol",
        "crlf",
    ]);
    let output = proc::run_bytes(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .stdin(input)
            .stop_descendants(true),
    )?;
    if !output.is_empty() && !output.ends_with(&[0]) {
        bail!(
            "git returned an unterminated attribute result for {}",
            cwd.display()
        );
    }
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != paths.len() * NAMES.len() * 3 {
        bail!(
            "git returned an unexpected attribute result for {}",
            cwd.display()
        );
    }
    let mut values: BTreeMap<PathBuf, BTreeMap<String, String>> = BTreeMap::new();
    for record in fields.chunks_exact(3) {
        let path = safe_git_path(record[0], "attribute")?;
        if !paths.contains(&path) {
            bail!(
                "git returned attributes for the wrong path in {}",
                cwd.display()
            );
        }
        let name = std::str::from_utf8(record[1])
            .map_err(|_| spar_err!("git returned a non-UTF-8 attribute name"))?;
        let value = std::str::from_utf8(record[2])
            .map_err(|_| spar_err!("git returned a non-UTF-8 attribute value"))?;
        values
            .entry(path)
            .or_default()
            .insert(name.to_string(), value.to_string());
    }
    if paths.iter().any(|path| {
        values
            .get(path)
            .is_none_or(|attributes| attributes.len() != NAMES.len())
    }) {
        bail!(
            "git omitted an attribute result for a tracked path in {}",
            cwd.display()
        );
    }
    Ok(values)
}

fn attribute_is_active(value: Option<&String>) -> bool {
    !matches!(
        value.map(String::as_str),
        None | Some("unspecified") | Some("unset")
    )
}

fn path_has_external_transform(values: &BTreeMap<String, String>) -> bool {
    attribute_is_active(values.get("filter"))
        || attribute_is_active(values.get("working-tree-encoding"))
}

/// The checkout settings a per-file line-ending decision depends on.
///
/// These are constant for a worktree, but the checks that read them run once
/// per tracked file, and each read was its own `git config` process. A
/// thousand-file repository with no `.gitattributes` takes every one of those
/// branches, which is half a minute of process spawning per scan, and the sweep
/// before a run scans every finished worktree more than once. Read them here,
/// once, and hand them down.
struct CheckoutConfig {
    autocrlf: Option<String>,
    eol: Option<String>,
    symlinks: Option<bool>,
}

impl CheckoutConfig {
    fn read(cwd: &Path) -> Result<Self> {
        Ok(Self {
            autocrlf: git_config_value(cwd, "core.autocrlf")?,
            eol: git_config_value(cwd, "core.eol")?,
            symlinks: git_config_bool(cwd, "core.symlinks")?,
        })
    }

    fn autocrlf_is_true(&self) -> bool {
        self.autocrlf.as_ref().is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            )
        })
    }

    fn eol_is(&self, wanted: &str) -> bool {
        self.eol
            .as_ref()
            .is_some_and(|value| value.eq_ignore_ascii_case(wanted))
    }
}

fn path_has_ambiguous_transform(
    config: &CheckoutConfig,
    values: &BTreeMap<String, String>,
) -> Result<bool> {
    if path_has_external_transform(values)
        || attribute_is_active(values.get("ident"))
        || attribute_is_active(values.get("crlf"))
    {
        return Ok(true);
    }
    let text = values.get("text").map(String::as_str);
    let eol = values.get("eol").map(String::as_str);
    if text == Some("auto") {
        return Ok(true);
    }
    if !matches!(text, Some("set") | Some("unset") | Some("unspecified"))
        || !matches!(
            eol,
            Some("lf") | Some("crlf") | Some("unset") | Some("unspecified")
        )
    {
        return Ok(true);
    }
    if text == Some("unspecified") && matches!(eol, Some("unspecified") | Some("unset")) {
        return Ok(config.autocrlf_is_true());
    }
    Ok(false)
}

fn allows_expected_crlf(
    config: &CheckoutConfig,
    values: &BTreeMap<String, String>,
) -> Result<bool> {
    if path_has_external_transform(values)
        || attribute_is_active(values.get("ident"))
        || attribute_is_active(values.get("crlf"))
    {
        return Ok(false);
    }
    let text = values.get("text").map(String::as_str);
    let eol = values.get("eol").map(String::as_str);
    if matches!(text, Some("unset") | Some("auto")) || eol == Some("lf") {
        return Ok(false);
    }
    if eol == Some("crlf") {
        return Ok(true);
    }
    if text != Some("set") {
        return Ok(false);
    }
    if let Some(autocrlf) = config.autocrlf.as_deref() {
        match autocrlf.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => return Ok(true),
            "input" => return Ok(false),
            _ => {}
        }
    }
    if config.eol_is("crlf") {
        return Ok(true);
    }
    #[cfg(windows)]
    if config.eol.is_none() || config.eol_is("native") {
        return Ok(true);
    }
    Ok(false)
}

fn git_config_value(cwd: &Path, key: &str) -> Result<Option<String>> {
    let argv = git_without_automation_argv(&["config", "--get", key]);
    let output = proc::exec(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .check(false)
            .stop_descendants(true),
    )?;
    match output.code {
        0 => Ok(Some(output.stdout.trim().to_string())),
        1 => Ok(None),
        _ => bail!(
            "could not read Git configuration in {}: {}",
            cwd.display(),
            output.stderr.trim()
        ),
    }
}

fn git_config_bool(cwd: &Path, key: &str) -> Result<Option<bool>> {
    let argv = git_without_automation_argv(&["config", "--type=bool", "--get", key]);
    let output = proc::exec(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .check(false)
            .stop_descendants(true),
    )?;
    match output.code {
        0 if output.stdout.trim() == "true" => Ok(Some(true)),
        0 if output.stdout.trim() == "false" => Ok(Some(false)),
        0 => bail!(
            "git returned an invalid boolean for {key} in {}",
            cwd.display()
        ),
        1 => Ok(None),
        _ => bail!(
            "could not read Git configuration in {}: {}",
            cwd.display(),
            output.stderr.trim()
        ),
    }
}

#[cfg(unix)]
fn tracked_file_mode(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        "100644".to_string()
    } else {
        "100755".to_string()
    }
}

#[cfg(not(unix))]
fn tracked_file_mode(_metadata: &std::fs::Metadata) -> String {
    "100644".to_string()
}

fn tree_entries(cwd: &Path, treeish: &str) -> Result<Vec<IndexEntry>> {
    let listed = run_git_bytes(cwd, &["ls-tree", "-r", "-z", treeish])?;
    if !listed.is_empty() && !listed.ends_with(&[0]) {
        bail!(
            "git returned an unterminated tree listing for {}",
            cwd.display()
        );
    }
    let mut entries = Vec::new();
    for record in listed
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            bail!("git returned a malformed tree record for {}", cwd.display());
        };
        let fields = record[..tab]
            .split(|byte| *byte == b' ')
            .collect::<Vec<_>>();
        if fields.len() != 3 {
            bail!("git returned a malformed tree header for {}", cwd.display());
        }
        let mode = std::str::from_utf8(fields[0])
            .map_err(|_| spar_err!("git returned a non-UTF-8 tree mode"))?
            .to_string();
        let oid = std::str::from_utf8(fields[2])
            .map_err(|_| spar_err!("git returned a non-UTF-8 object id"))?
            .to_string();
        entries.push(IndexEntry {
            path: safe_git_path(&record[tab + 1..], "tree")?,
            mode,
            oid,
        });
    }
    Ok(entries)
}

fn head_gitlinks(cwd: &Path) -> Result<BTreeMap<PathBuf, String>> {
    Ok(tree_entries(cwd, "HEAD")?
        .into_iter()
        .filter(|entry| entry.mode == "160000")
        .map(|entry| (entry.path, entry.oid))
        .collect())
}

fn changed_staged_gitlinks(cwd: &Path) -> Result<Vec<PathBuf>> {
    let head = head_gitlinks(cwd)?;
    let index: BTreeMap<PathBuf, String> = gitlinks(cwd)?
        .into_iter()
        .map(|link| (link.path, link.oid))
        .collect();
    let mut paths: BTreeSet<PathBuf> = head.keys().cloned().collect();
    paths.extend(index.keys().cloned());
    Ok(paths
        .into_iter()
        .filter(|path| head.get(path) != index.get(path))
        .collect())
}

fn initialized_submodule(parent: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let path = parent.join(relative);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(spar_err!("could not inspect {}: {e}", path.display())),
    };
    if !metadata.is_dir() {
        bail!("the gitlink at {} is not a directory", path.display());
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| spar_err!("could not resolve {}: {e}", path.display()))?;
    if canonical != path {
        bail!(
            "the gitlink at {} resolves through a symlink",
            path.display()
        );
    }
    if !path.join(".git").exists() {
        let empty = std::fs::read_dir(&path)
            .map_err(|e| spar_err!("could not inspect {}: {e}", path.display()))?
            .next()
            .is_none();
        if empty {
            return Ok(None);
        }
        bail!(
            "the uninitialized gitlink at {} contains local files",
            path.display()
        );
    }
    let inside = run_git_text(&path, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        bail!("the gitlink at {} is not a worktree", path.display());
    }
    let top = run_git_text(&path, &["rev-parse", "--show-toplevel"])?;
    let top = std::fs::canonicalize(top.trim()).map_err(|e| {
        spar_err!(
            "could not resolve the gitlink top level at {}: {e}",
            path.display()
        )
    })?;
    if top != canonical {
        bail!(
            "the gitlink at {} belongs to a different worktree",
            path.display()
        );
    }
    Ok(Some(canonical))
}

fn unexpected_nested_git_entry(cwd: &Path) -> Result<Option<PathBuf>> {
    let root = std::fs::canonicalize(cwd)
        .map_err(|e| spar_err!("could not resolve {}: {e}", cwd.display()))?;
    let mut allowed = BTreeSet::from([root.join(".git")]);
    let mut repositories = vec![root.clone()];
    let mut visited = BTreeSet::new();
    while let Some(repository) = repositories.pop() {
        let canonical = std::fs::canonicalize(&repository)
            .map_err(|e| spar_err!("could not resolve {}: {e}", repository.display()))?;
        if !visited.insert(canonical.clone()) {
            bail!("submodule recursion revisited {}", canonical.display());
        }
        for link in gitlinks(&canonical)? {
            let Some(submodule) = initialized_submodule(&canonical, &link.path)? else {
                continue;
            };
            allowed.insert(submodule.join(".git"));
            repositories.push(submodule);
        }
    }

    let scan_root = root.clone();
    let mut directories = vec![root];
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|e| spar_err!("could not inspect {}: {e}", directory.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| spar_err!("could not inspect {}: {e}", directory.display()))?;
            let path = entry.path();
            if directory == scan_root && entry.file_name() == OsStr::new(WORKTREE_DIR) {
                continue;
            }
            if entry.file_name() == OsStr::new(".git") {
                if !allowed.contains(&path) {
                    return Ok(Some(path));
                }
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|e| spar_err!("could not inspect {}: {e}", path.display()))?;
            if kind.is_dir() {
                directories.push(path);
            }
        }
    }
    Ok(None)
}

pub(crate) fn git_state(cwd: &Path) -> Result<GitState> {
    if let Some(path) = unexpected_nested_git_entry(cwd)? {
        bail!(
            "the worktree contains an untracked Git entry at {}. It was kept because its \
             repository objects are not represented by the outer index.",
            path.display()
        );
    }
    let root = std::fs::canonicalize(cwd)
        .map_err(|e| spar_err!("could not resolve {}: {e}", cwd.display()))?;
    let mut repositories = BTreeMap::new();
    let mut visited = BTreeSet::new();
    collect_git_state(&root, Path::new(""), &mut visited, &mut repositories)?;
    Ok(GitState { repositories })
}

fn collect_git_state(
    repository: &Path,
    prefix: &Path,
    visited: &mut BTreeSet<PathBuf>,
    repositories: &mut BTreeMap<PathBuf, RepositoryState>,
) -> Result<()> {
    let canonical = std::fs::canonicalize(repository)
        .map_err(|e| spar_err!("could not resolve {}: {e}", repository.display()))?;
    if !visited.insert(canonical.clone()) {
        bail!("submodule recursion revisited {}", canonical.display());
    }
    let head = run_git_text(repository, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let head = head.trim().to_string();
    if head.is_empty() {
        bail!("git returned an empty head for {}", repository.display());
    }
    let unsafe_index_flags = unsafe_index_flags(repository)?;
    let tracked = tracked_entries(repository)?;
    let gitlinks = gitlinks(repository)?;
    if repositories
        .insert(
            prefix.to_path_buf(),
            RepositoryState {
                head,
                unsafe_index_flags,
                tracked,
                gitlinks: gitlinks
                    .iter()
                    .map(|link| (link.path.clone(), link.oid.clone()))
                    .collect(),
            },
        )
        .is_some()
    {
        bail!("Git state contains duplicate repository path {:?}", prefix);
    }

    for link in gitlinks {
        let Some(submodule) = initialized_submodule(repository, &link.path)? else {
            continue;
        };
        collect_git_state(&submodule, &prefix.join(&link.path), visited, repositories)?;
    }
    Ok(())
}

fn unsafe_index_flags(cwd: &Path) -> Result<Vec<u8>> {
    let listed = run_git_bytes(cwd, &["ls-files", "-v", "-z"])?;
    if !listed.is_empty() && !listed.ends_with(&[0]) {
        bail!(
            "git returned an unterminated index-flag listing for {}",
            cwd.display()
        );
    }
    let mut unsafe_records = Vec::new();
    for record in listed
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if record.len() < 3 || record[1] != b' ' {
            bail!(
                "git returned a malformed index-flag record for {}",
                cwd.display()
            );
        }
        if record[0] != b'H' {
            unsafe_records.extend_from_slice(record);
            unsafe_records.push(0);
        }
    }
    Ok(unsafe_records)
}

pub(crate) fn refuse_unsafe_index_flags(cwd: &Path) -> Result<()> {
    safe_git_state(cwd).map(|_| ())
}

pub(crate) fn safe_git_state(cwd: &Path) -> Result<GitState> {
    let state = git_state(cwd)?;
    if let Some((path, _repository)) = state
        .repositories
        .iter()
        .find(|(_, repository)| !repository.unsafe_index_flags.is_empty())
    {
        let label = if path.as_os_str().is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(path)
        };
        bail!(
            "the index at {} has assume-unchanged, skip-worktree, or another nonstandard flag. \
             SPAR cannot prove the working files are unchanged, so it was kept.",
            label.display()
        );
    }
    Ok(state)
}

fn repository_has_recoverable_work(cwd: &Path, include_ignored: bool) -> Result<bool> {
    if include_ignored && unexpected_nested_git_entry(cwd)?.is_some() {
        return Ok(true);
    }
    let mut visited = BTreeSet::new();
    repository_has_recoverable_work_inner(cwd, include_ignored, &mut visited)
}

fn has_recoverable_worktree_admin_state(cwd: &Path) -> Result<bool> {
    let git_dir = run_git_text(cwd, &["rev-parse", "--git-dir"])?;
    let git_dir = PathBuf::from(git_dir.trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        cwd.join(git_dir)
    };
    let git_dir = std::fs::canonicalize(&git_dir)
        .map_err(|e| spar_err!("could not resolve {}: {e}", git_dir.display()))?;
    match std::fs::symlink_metadata(git_dir.join("config.worktree")) {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(spar_err!(
                "could not inspect per-worktree configuration in {}: {error}",
                git_dir.display()
            ))
        }
    }

    let orig_head = git_dir.join("ORIG_HEAD");
    match std::fs::symlink_metadata(&orig_head) {
        Ok(metadata) if metadata.is_file() => {
            let oid = std::fs::read_to_string(&orig_head)
                .map_err(|e| spar_err!("could not read {}: {e}", orig_head.display()))?;
            let Some(commit) = resolve_optional_commit(cwd, oid.trim())? else {
                return Ok(true);
            };
            if !commit_has_shared_ref(cwd, &commit)? {
                return Ok(true);
            }
        }
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(spar_err!(
                "could not inspect {}: {error}",
                orig_head.display()
            ))
        }
    }

    let edit_message = git_dir.join("COMMIT_EDITMSG");
    match std::fs::symlink_metadata(&edit_message) {
        Ok(metadata) if metadata.is_file() => {
            let draft = std::fs::read(&edit_message)
                .map_err(|e| spar_err!("could not read {}: {e}", edit_message.display()))?;
            if draft != head_commit_message(cwd)? {
                return Ok(true);
            }
        }
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(spar_err!(
                "could not inspect {}: {error}",
                edit_message.display()
            ))
        }
    }

    if reflogs_have_unpreserved_commits(cwd, &git_dir.join("logs"))? {
        return Ok(true);
    }

    let local_refs = run_git_bytes(
        cwd,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/worktree",
            "refs/bisect",
            "refs/rewritten",
        ],
    )?;
    if !local_refs.is_empty() {
        return Ok(true);
    }

    for entry in std::fs::read_dir(&git_dir)
        .map_err(|e| spar_err!("could not inspect {}: {e}", git_dir.display()))?
    {
        let entry = entry.map_err(|e| spar_err!("could not inspect {}: {e}", git_dir.display()))?;
        let known = matches!(
            entry.file_name().to_str(),
            Some(
                "HEAD"
                    | "ORIG_HEAD"
                    | "COMMIT_EDITMSG"
                    | "commondir"
                    | "gitdir"
                    | "index"
                    | "logs"
                    | "refs"
            )
        );
        if !known {
            return Ok(true);
        }
    }

    let head = run_git_text(cwd, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !commit_has_shared_ref(cwd, head.trim())? {
        return Ok(true);
    }
    Ok(false)
}

fn head_commit_message(cwd: &Path) -> Result<Vec<u8>> {
    let commit = run_git_bytes(cwd, &["cat-file", "commit", "HEAD"])?;
    let Some(split) = commit.windows(2).position(|bytes| bytes == b"\n\n") else {
        bail!(
            "git returned a commit without a message separator in {}",
            cwd.display()
        );
    };
    Ok(commit[split + 2..].to_vec())
}

fn reflogs_have_unpreserved_commits(cwd: &Path, logs: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(logs) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(spar_err!("could not inspect {}: {error}", logs.display())),
    };
    if !metadata.is_dir() {
        return Ok(true);
    }
    let mut files = Vec::new();
    let mut directories = vec![logs.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)
            .map_err(|e| spar_err!("could not inspect {}: {e}", directory.display()))?
        {
            let entry =
                entry.map_err(|e| spar_err!("could not inspect {}: {e}", directory.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| spar_err!("could not inspect {}: {e}", path.display()))?;
            if kind.is_dir() {
                directories.push(path);
            } else if kind.is_file() {
                files.push(path);
            } else {
                return Ok(true);
            }
        }
    }

    let mut commits = BTreeSet::new();
    for path in files {
        if !collect_reflog_commits(cwd, &path, &mut commits)? {
            return Ok(true);
        }
    }
    for commit in commits {
        if !commit_has_shared_ref(cwd, &commit)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Every commit named by one ref's common reflog must survive deletion of that
/// ref. Ancestors of a durable current tip survive with the tip; divergent
/// entries need another shared ref of their own.
fn ref_reflog_is_preserved(cwd: &Path, refname: &str, durable_tip: &str) -> Result<bool> {
    let common = common_git_dir(cwd)?;
    let reflog = common.join("logs").join(refname);
    let metadata = match std::fs::symlink_metadata(&reflog) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(spar_err!("could not inspect {}: {error}", reflog.display())),
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    let mut commits = BTreeSet::new();
    if !collect_reflog_commits(cwd, &reflog, &mut commits)? {
        return Ok(false);
    }
    for commit in commits {
        if is_ancestor(cwd, &commit, durable_tip)?
            || commit_has_shared_ref_except(cwd, &commit, Some(refname))?
        {
            continue;
        }
        return Ok(false);
    }
    Ok(true)
}

fn collect_reflog_commits(cwd: &Path, path: &Path, commits: &mut BTreeSet<String>) -> Result<bool> {
    let file = std::fs::File::open(path)
        .map_err(|e| spar_err!("could not read {}: {e}", path.display()))?;
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|e| spar_err!("could not read {}: {e}", path.display()))?;
        let mut fields = line.splitn(3, ' ');
        let Some(old) = fields.next() else {
            return Ok(false);
        };
        let Some(new) = fields.next() else {
            return Ok(false);
        };
        if fields.next().is_none() {
            return Ok(false);
        }
        for oid in [old, new] {
            if oid.bytes().all(|byte| byte == b'0') {
                continue;
            }
            let Some(commit) = resolve_optional_commit(cwd, oid)? else {
                return Ok(false);
            };
            commits.insert(commit);
        }
    }
    Ok(true)
}

fn common_git_dir(cwd: &Path) -> Result<PathBuf> {
    let raw = run_git_text(cwd, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(raw.trim());
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    std::fs::canonicalize(&path).map_err(|e| spar_err!("could not resolve {}: {e}", path.display()))
}

fn is_ancestor(cwd: &Path, older: &str, newer: &str) -> Result<bool> {
    let argv = git_without_automation_argv(&["merge-base", "--is-ancestor", older, newer]);
    let output = proc::exec(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .check(false)
            .stop_descendants(true),
    )?;
    match output.code {
        0 => Ok(true),
        1 => Ok(false),
        _ => bail!("{}", proc::failure_message(&argv, &output)),
    }
}

fn resolve_optional_commit(cwd: &Path, oid: &str) -> Result<Option<String>> {
    let commit = format!("{oid}^{{commit}}");
    let argv = git_without_automation_argv(&["rev-parse", "--quiet", "--verify", &commit]);
    let output = proc::exec(
        &argv,
        &ExecOpts::new()
            .cwd(cwd)
            .timeout_secs(30)
            .check(false)
            .stop_descendants(true),
    )?;
    if output.code != 0 {
        return Ok(None);
    }
    let oid = output.stdout.trim();
    if oid.is_empty() {
        return Ok(None);
    }
    Ok(Some(oid.to_string()))
}

fn commit_has_shared_ref(cwd: &Path, oid: &str) -> Result<bool> {
    commit_has_shared_ref_except(cwd, oid, None)
}

fn commit_has_shared_ref_except(cwd: &Path, oid: &str, exclude: Option<&str>) -> Result<bool> {
    let contains = format!("--contains={oid}");
    let shared = run_git_bytes(cwd, &["for-each-ref", "--format=%(refname)", &contains])?;
    Ok(shared.split(|byte| *byte == b'\n').any(|record| {
        !record.is_empty()
            && !record.starts_with(b"refs/worktree/")
            && !record.starts_with(b"refs/bisect/")
            && !record.starts_with(b"refs/rewritten/")
            && exclude.is_none_or(|excluded| record != excluded.as_bytes())
    }))
}

/// Whether removing the worktree would take away an untracked file somebody
/// might want back.
///
/// Ordinary untracked files always count. So does an ignored file outside the
/// known build and cache directories, because an ignored path is only a path
/// Git was told not to track, which is where a local `.env` lives as readily as
/// compiler output.
///
/// Recognized build and cache output does not. A managed commit already leaves
/// it out rather than treating it as work, and the command that wrote it writes
/// it again. Counting it kept every worktree whose tests or build had run,
/// which is nearly all of them, so a merged pull request still left its
/// checkout behind. A repository nested in that output is somebody else's
/// history and counts whatever it sits under.
fn has_untracked_work_worth_keeping(cwd: &Path) -> Result<bool> {
    let ordinary = untracked_listing(cwd, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if !ordinary.is_empty() {
        return Ok(true);
    }
    let listed = untracked_listing(
        cwd,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    for raw in listed {
        let (path, nested) = untracked_record(&raw, "ignored")?;
        if nested || !is_generated_artifact(&path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn untracked_listing(cwd: &Path, args: &[&str]) -> Result<Vec<Vec<u8>>> {
    let listed = run_git_bytes(cwd, args)?;
    if !listed.is_empty() && !listed.ends_with(&[0]) {
        bail!(
            "git returned an unterminated untracked-file list for {}",
            cwd.display()
        );
    }
    Ok(listed
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
        .map(|raw| raw.to_vec())
        .collect())
}

fn repository_has_recoverable_work_inner(
    cwd: &Path,
    include_ignored: bool,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<bool> {
    let canonical = std::fs::canonicalize(cwd)
        .map_err(|e| spar_err!("could not resolve {}: {e}", cwd.display()))?;
    if !visited.insert(canonical.clone()) {
        bail!("submodule recursion revisited {}", canonical.display());
    }
    if include_ignored && has_untracked_work_worth_keeping(cwd)? {
        return Ok(true);
    }
    if !unsafe_index_flags(cwd)?.is_empty() {
        return Ok(true);
    }
    if attributes_may_be_modified(cwd)? {
        return Ok(true);
    }
    if include_ignored && has_recoverable_worktree_admin_state(cwd)? {
        return Ok(true);
    }
    if include_ignored {
        let index = index_entries(cwd)?
            .into_iter()
            .map(|entry| (entry.path, (entry.mode, entry.oid)))
            .collect::<BTreeMap<_, _>>();
        let head = tree_entries(cwd, "HEAD")?
            .into_iter()
            .map(|entry| (entry.path, (entry.mode, entry.oid)))
            .collect::<BTreeMap<_, _>>();
        if index != head || !run_git_bytes(cwd, &["ls-files", "--unmerged", "-z"])?.is_empty() {
            return Ok(true);
        }
        let tracked = tracked_entries(cwd)?;
        let effective = check_attributes(cwd, tracked.keys().cloned())?;
        let config = CheckoutConfig::read(cwd)?;
        for (path, entry) in tracked {
            let Some(worktree) = entry.worktree else {
                return Ok(true);
            };
            let attributes = effective.get(&path).ok_or_else(|| {
                spar_err!("git omitted attributes for {}", cwd.join(&path).display())
            })?;
            if path_has_ambiguous_transform(&config, attributes)? {
                return Ok(true);
            }
            let symlink_file = entry.index_mode == "120000"
                && worktree.mode == "100644"
                && worktree.raw_oid == entry.index_oid
                && config.symlinks == Some(false);
            if worktree.mode != entry.index_mode && !symlink_file {
                return Ok(true);
            }
            if entry.index_mode == "120000" {
                if worktree.raw_oid != entry.index_oid {
                    return Ok(true);
                }
                continue;
            }
            #[cfg(unix)]
            {
                let expected = if entry.index_mode == "100755" {
                    0o755
                } else {
                    0o644
                };
                if worktree.permissions != expected {
                    return Ok(true);
                }
            }
            if allows_expected_crlf(&config, attributes)? {
                let (normalized, every_lf_was_crlf) =
                    normalized_git_blob_oid(&cwd.join(&path), entry.index_oid.len())?;
                if !every_lf_was_crlf || normalized != entry.index_oid {
                    return Ok(true);
                }
            } else if worktree.raw_oid != entry.index_oid {
                return Ok(true);
            }
        }
    } else {
        let args = ["status", "--porcelain=v1", "-z", "--untracked-files=all"];
        if !run_git_bytes(cwd, &args)?.is_empty() {
            return Ok(true);
        }
    }
    for link in gitlinks(cwd)? {
        let Some(submodule) = initialized_submodule(cwd, &link.path)? else {
            continue;
        };
        // Git stores a linked worktree's initialized submodule objects under
        // that worktree's administrative directory. Ordinary removal cannot
        // prove a local submodule commit exists anywhere else, even when both
        // working trees look clean.
        if include_ignored {
            return Ok(true);
        }
        let head = run_git_text(&submodule, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        if head.trim() != link.oid {
            return Ok(true);
        }
        if repository_has_recoverable_work_inner(&submodule, include_ignored, visited)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn has_uncommitted_work(cwd: &Path) -> Result<bool> {
    repository_has_recoverable_work(cwd, false)
}

fn has_tracked_or_staged_work(cwd: &Path) -> Result<bool> {
    let args = ["status", "--porcelain=v1", "-z", "--untracked-files=no"];
    Ok(!run_git_bytes(cwd, &args)?.is_empty())
}

#[cfg(unix)]
fn path_from_git_bytes(raw: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(raw: &[u8]) -> Result<PathBuf> {
    String::from_utf8(raw.to_vec())
        .map(PathBuf::from)
        .map_err(|_| spar_err!("git returned a non-UTF-8 ignored path"))
}

/// Fingerprint the directory of a nested repository without reading inside it.
///
/// The files under it are that repository's, not this one's. They are recorded
/// against its own baseline whenever SPAR works there, and a run of its own may
/// legitimately add or remove entries while this call is in flight, so the
/// volatile directory fields stay out of the fingerprint. Identity and type
/// remain, which is what makes deleting the checkout, or replacing it with a
/// file, observable from the outer worktree.
fn nested_repository_fingerprint(path: &Path) -> Result<UntrackedFile> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        spar_err!(
            "could not inspect the nested repository at {}: {e}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "git reported {} as a nested repository, but it is not a directory",
            path.display()
        );
    }
    if std::fs::symlink_metadata(path.join(".git")).is_err() {
        bail!(
            "git reported {} as a nested repository, but it has no Git entry",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(UntrackedFile {
            kind: 3,
            len: 0,
            modified: None,
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            symlink_target: None,
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            change_seconds: 0,
            change_nanoseconds: 0,
        })
    }
    #[cfg(not(unix))]
    {
        Ok(UntrackedFile {
            kind: 3,
            len: 0,
            modified: None,
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            symlink_target: None,
        })
    }
}

fn ignored_file_fingerprint(path: &Path) -> Result<UntrackedFile> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| spar_err!("could not inspect untracked file {}: {e}", path.display()))?;
    let kind = if metadata.file_type().is_symlink() {
        2
    } else if metadata.is_file() {
        1
    } else {
        bail!(
            "untracked path {} is not a regular file or symlink",
            path.display()
        );
    };
    let symlink_target = if kind == 2 {
        let target = std::fs::read_link(path)
            .map_err(|e| spar_err!("could not read untracked symlink {}: {e}", path.display()))?;
        Some(os_str_bytes(target.as_os_str())?)
    } else {
        None
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(UntrackedFile {
            kind,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            symlink_target,
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            change_seconds: metadata.ctime(),
            change_nanoseconds: metadata.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(UntrackedFile {
            kind,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            readonly: metadata.permissions().readonly(),
            symlink_target,
        })
    }
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(value.as_bytes().to_vec())
}

#[cfg(not(unix))]
fn os_str_bytes(value: &OsStr) -> Result<Vec<u8>> {
    value
        .to_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| spar_err!("a filesystem path is not UTF-8"))
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    right.is_file() && left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    right.is_file() && left.len() == right.len() && left.permissions() == right.permissions()
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct BranchRecord {
    pub kind: String,
    pub number: i64,
}

/// Where a pull request's fetched head is parked. Under `refs/spar/` rather
/// than `refs/heads/` so it can never be mistaken for a branch, or pushed.
pub fn review_ref(number: i64) -> String {
    format!("refs/spar/pr-{number}")
}

pub fn is_finished(state: &str) -> bool {
    matches!(state.trim().to_uppercase().as_str(), "MERGED" | "CLOSED")
}

/// Write text through a temporary file and rename, so a kill cannot leave a
/// truncated file behind.
///
/// The follow-up queue is the one file spar rewrites in place rather than
/// appends to, and a truncated queue is lost work: what it held was never
/// written anywhere else.
pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| spar_err!("could not create {}: {e}", parent.display()))?;
    }
    // The extension defaults to `json` so `clear_state`, which removes a
    // leftover `pr-N.json.tmp` by name, keeps finding the one this wrote.
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    std::fs::write(&tmp, text).map_err(|e| spar_err!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| spar_err!("could not replace {}: {e}", path.display()))?;
    Ok(())
}

/// Write JSON through a temporary file and rename, so a kill cannot leave a
/// truncated state file behind.
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    write_text_atomic(path, &serde_json::to_string_pretty(value)?)
}

/// Among the open pull requests gh listed, the first that would close `issue`.
///
/// Separated from the gh call so the real payload shape can be tested. GitHub
/// returns far more per linked issue than the number, and silently failing to
/// parse it would look exactly like "no pull request exists", which is the
/// answer that makes spar implement over the top of somebody's work.
pub fn find_linked_pr(json: &str, issue: i64) -> Option<PrRef> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Row {
        number: i64,
        #[serde(default)]
        url: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        closing_issues_references: Vec<IssueRef>,
    }

    serde_json::from_str::<Vec<Row>>(json.trim())
        .ok()?
        .into_iter()
        .find(|row| {
            row.closing_issues_references
                .iter()
                .any(|linked| linked.number == issue)
        })
        .map(|row| PrRef {
            number: row.number,
            url: row.url,
            title: row.title,
        })
}

/// Flatten whatever `gh api --paginate` printed into a list of comments.
///
/// Current gh merges array pages into one array. Older builds concatenated one
/// document per page. A streaming parser reads either, and unlike splitting the
/// text on a bracket pair it cannot be fooled by a comment body that happens to
/// contain one, which would otherwise make a resume silently start over.
fn try_parse_comment_pages(text: &str) -> Result<Vec<Value>> {
    if text.trim().is_empty() {
        return Err(spar_err!("GitHub returned no comment data"));
    }
    let mut out = Vec::new();
    for value in serde_json::Deserializer::from_str(text.trim()).into_iter::<Value>() {
        match value.map_err(|e| spar_err!("unexpected comment pages: {e}"))? {
            Value::Array(items) => out.extend(items),
            _ => return Err(spar_err!("unexpected non-array comment page")),
        }
    }
    Ok(out)
}

pub fn parse_comment_pages(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for value in serde_json::Deserializer::from_str(text.trim()).into_iter::<Value>() {
        match value {
            Ok(Value::Array(items)) => out.extend(items),
            Ok(other) => out.push(other),
            Err(_) => break,
        }
    }
    out
}

/// Extract the payload from a state comment. The marker is followed by JSON and
/// terminated with `-->`.
pub fn parse_state_comment(body: &str) -> Option<PersistedState> {
    let marker = body.find(STATE_MARKER)?;
    let start = body[marker..].find('{')? + marker;
    let end = body.rfind('}')?;
    if end <= start {
        return None;
    }
    match serde_json::from_str(&body[start..=end]) {
        Ok(state) => Some(state),
        Err(_) => {
            logdim!("found a spar state comment but could not parse it");
            None
        }
    }
}

fn choose_state_for_head(
    candidates: Vec<PersistedState>,
    actual_head: &str,
) -> Option<PersistedState> {
    let matching: Vec<PersistedState> = candidates
        .iter()
        .filter(|state| state.pr_head == actual_head)
        .cloned()
        .collect();
    if !matching.is_empty() {
        return newest_state(matching);
    }
    newest_state(candidates)
}

fn newest_state(candidates: Vec<PersistedState>) -> Option<PersistedState> {
    candidates.into_iter().reduce(|best, candidate| {
        if (candidate.checkpoint, candidate.round) > (best.checkpoint, best.round) {
            candidate
        } else {
            // The local candidate is supplied first. Keeping the first exact
            // tie recovers correctly from a local write followed by a failed
            // pull request state update, including legacy states with no
            // checkpoint field.
            best
        }
    })
}

/// Where this binary lives, so `git filter-branch` can call back into it.
///
/// `SPAR_SELF_BIN` overrides the answer. That matters for the integration
/// tests, whose `current_exe` is the test harness rather than spar, and for
/// anyone who ships spar behind a wrapper script.
pub fn self_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SPAR_SELF_BIN") {
        let path = PathBuf::from(path);
        if proc::is_executable(&path) {
            return Ok(path);
        }
        bail!(
            "SPAR_SELF_BIN is set to {}, which is not executable",
            path.display()
        );
    }
    std::env::current_exe()
        .map_err(|e| spar_err!("could not locate the spar binary for a commit rewrite: {e}"))
}

fn bool_env(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

/// Wrap a string for a POSIX shell. `git filter-branch` takes its filter as a
/// shell command, and an install path with a space in it is not exotic.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Style rules for the `scrub-filter` subcommand, which runs in a child process
/// spawned by git and so cannot see the parent's config.
pub fn style_from_env() -> Style {
    let flag = |key: &str| !matches!(std::env::var(key).as_deref(), Ok("0"));
    Style {
        ban_em_dash: flag("SPAR_BAN_EM_DASH"),
        ban_ai_attribution: flag("SPAR_BAN_AI_ATTRIBUTION"),
        ..Style::permissive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StateStore;
    use crate::model::{Dispute, Finding, Ledger, PersistedState, Severity, Status};
    use std::process::Command;

    fn repo_for_titles() -> Repo {
        Repo {
            root: PathBuf::from("/nonexistent"),
            style: Style::default(),
            branch_prefix: String::new(),
            state_store: StateStore::Local,
            followups: crate::config::Followups::Issues,
            drafts: Drafts::Never,
            viewer: OnceLock::new(),
            checkpoints: Mutex::new(BTreeMap::new()),
            writes: WriteStats::default(),
        }
    }

    #[test]
    fn write_results_accumulate_for_the_run() {
        let repo = repo_for_titles();

        let _: std::result::Result<(), ()> = repo.record_write(Ok(()));
        let _: std::result::Result<(), ()> = repo.record_write(Err(()));

        assert_eq!(
            WriteSummary {
                attempted: 2,
                failed: 1,
            },
            repo.write_summary()
        );
    }

    #[test]
    fn only_failed_write_preflights_join_the_summary() {
        let repo = repo_for_titles();

        let _: std::result::Result<(), ()> = repo.record_failed_write(Ok(()));
        let _: std::result::Result<(), ()> = repo.record_failed_write(Err(()));

        assert_eq!(
            WriteSummary {
                attempted: 1,
                failed: 1,
            },
            repo.write_summary()
        );
    }

    #[test]
    fn a_nonempty_write_title_that_cleans_to_empty_is_one_failed_preflight() {
        let repo = repo_for_titles();

        assert!(repo.clean_nonempty_title_for_write("\u{1F916}").is_err());
        assert_eq!(
            WriteSummary {
                attempted: 1,
                failed: 1,
            },
            repo.write_summary()
        );
    }

    #[test]
    fn a_local_followup_title_failure_is_not_a_remote_write_failure() {
        let mut repo = repo_for_titles();
        repo.followups = Followups::Local;

        assert_eq!("", repo.clean_followup_title("\u{1F916}").unwrap());
        assert_eq!(WriteSummary::default(), repo.write_summary());
    }

    #[test]
    fn a_failed_remote_state_read_stops_before_state_mutation() {
        let root = std::env::temp_dir().join(format!(
            "spar-state-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let _fixture = ReviewFixture { root: root.clone() };
        let mut repo = repo_for_titles();
        repo.root = root;
        repo.state_store = StateStore::Both;
        let state = PersistedState {
            version: 1,
            checkpoint: 4,
            round: 2,
            next_actor: "a".into(),
            status: Status::Pending,
            pr_head: "abc123".into(),
            ledger: Ledger::new(),
            filed: Vec::new(),
            open_findings: Vec::new(),
            disputes: Vec::new(),
            noted: Vec::new(),
        };

        let error = repo
            .write_state_after_remote_read(
                7,
                &state,
                Err(crate::error::SparError::new("state comments unavailable")),
            )
            .unwrap_err();

        assert!(error.to_string().contains("state comments unavailable"));
        assert!(!repo.state_path(7).exists());
        assert_eq!(0, repo.remembered_checkpoint(7));
        assert_eq!(
            WriteSummary {
                attempted: 1,
                failed: 1,
            },
            repo.write_summary()
        );
    }

    #[test]
    fn only_known_build_and_cache_directories_are_generated_artifacts() {
        assert!(is_generated_artifact(Path::new("target/debug/artifact")));
        assert!(is_generated_artifact(Path::new("dist/cli/index.js")));
        assert!(is_generated_artifact(Path::new(
            "package/node_modules/dependency/file.js"
        )));
        assert!(!is_generated_artifact(Path::new(
            "distribution/required-package.js"
        )));
        assert!(!is_generated_artifact(Path::new(
            "generated/required-fixture.txt"
        )));
        assert!(!is_generated_artifact(Path::new("local.env")));
    }

    struct ReviewFixture {
        root: PathBuf,
    }

    impl Drop for ReviewFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn test_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn review_fixture(
        tag: &str,
        number: i64,
    ) -> (ReviewFixture, Repo, PathBuf, WorktreeCheckpoint) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("spar-repo-test-{tag}-{}-{id}", std::process::id()));
        let origin = root.join("origin.git");
        let work = root.join("work");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        test_git(&origin, &["init", "--bare", "-b", "main"]);
        test_git(&work, &["init", "-b", "main"]);
        test_git(&work, &["config", "user.email", "spar@example.invalid"]);
        test_git(&work, &["config", "user.name", "spar test"]);
        test_git(&work, &["config", "commit.gpgsign", "false"]);
        test_git(&work, &["config", "filter.drop.clean", "sed '/^secret:/d'"]);
        test_git(&work, &["config", "filter.drop.smudge", "cat"]);
        std::fs::write(work.join("README.md"), "seed\n").unwrap();
        std::fs::write(work.join("data.txt"), "old\n").unwrap();
        std::fs::write(work.join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(work.join(".gitattributes"), "* text\n").unwrap();
        test_git(&work, &["add", "."]);
        test_git(&work, &["commit", "-m", "seed"]);
        test_git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        test_git(&work, &["push", "-u", "origin", "main"]);
        test_git(
            &work,
            &["push", "origin", &format!("HEAD:refs/pull/{number}/head")],
        );
        let cfg = crate::config::parse(
            "[agents.a]\ncommand = [\"true\"]\n[agents.b]\ncommand = [\"true\"]\n",
        )
        .unwrap();
        let repo = Repo::open(&work, &cfg).unwrap();
        let path = repo.worktree_for_pr_head(number).unwrap();
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();
        (ReviewFixture { root }, repo, path, checkpoint)
    }

    #[test]
    fn an_unchanged_review_worktree_is_released_after_a_checked_read() {
        let (_fixture, repo, path, checkpoint) = review_fixture("checked-release", 901);

        repo.release_review_worktree_checked(901, &checkpoint)
            .unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn a_branch_reflog_only_commit_prevents_ordinary_deletion() {
        let (_fixture, repo, _review, _checkpoint) = review_fixture("branch-reflog", 920);
        let (path, branch) = repo.worktree_for_split(45, 1, "main").unwrap();
        std::fs::write(path.join("recovery.txt"), "keep me\n").unwrap();
        test_git(&path, &["add", "recovery.txt"]);
        test_git(&path, &["commit", "-m", "recovery commit"]);
        let recovery = test_git(&path, &["rev-parse", "HEAD"]);
        test_git(&path, &["reset", "--hard", "main"]);

        assert!(!repo.branch_deletion_is_safe(&branch).unwrap());
        test_git(
            &path,
            &["cat-file", "-e", &format!("{}^{{commit}}", recovery.trim())],
        );
    }

    #[test]
    fn a_review_ref_reflog_only_commit_prevents_deletion() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("review-ref-reflog", 921);
        let local_ref = review_ref(921);
        let original = test_git(&path, &["rev-parse", &local_ref]);
        let tree = test_git(&path, &["rev-parse", "HEAD^{tree}"]);
        let recovery = test_git(
            &path,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                original.trim(),
                "-m",
                "review ref recovery",
            ],
        );
        test_git(
            &path,
            &["update-ref", "--create-reflog", &local_ref, recovery.trim()],
        );
        test_git(
            &path,
            &["update-ref", &local_ref, original.trim(), recovery.trim()],
        );

        assert!(!repo.review_ref_deletion_is_safe(921).unwrap());
        assert_eq!(original, test_git(&path, &["rev-parse", &local_ref]));
        test_git(
            &path,
            &["cat-file", "-e", &format!("{}^{{commit}}", recovery.trim())],
        );
    }

    #[test]
    fn an_unpublished_commit_message_draft_is_recoverable() {
        let (_fixture, _repo, path, _checkpoint) = review_fixture("commit-draft", 922);
        let raw = PathBuf::from(test_git(&path, &["rev-parse", "--git-dir"]).trim());
        let git_dir = if raw.is_absolute() {
            raw
        } else {
            path.join(raw)
        };
        std::fs::write(git_dir.join("COMMIT_EDITMSG"), "unique recovery draft\n").unwrap();

        assert!(repository_has_recoverable_work(&path, true).unwrap());
        assert_eq!(
            "unique recovery draft\n",
            std::fs::read_to_string(git_dir.join("COMMIT_EDITMSG")).unwrap()
        );
    }

    #[test]
    fn a_changed_review_worktree_is_retained_after_a_checked_read() {
        let (_fixture, repo, path, checkpoint) = review_fixture("checked-dirty", 902);
        std::fs::write(path.join("README.md"), "recover me\n").unwrap();

        let error = repo
            .release_review_worktree_checked(902, &checkpoint)
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("kept for recovery"), "{error}");
        assert_eq!(
            "recover me\n",
            std::fs::read_to_string(path.join("README.md")).unwrap()
        );
        repo.release_review_worktree(902);
    }

    #[test]
    fn a_review_commit_is_retained_after_a_checked_read() {
        let (_fixture, repo, path, checkpoint) = review_fixture("checked-commit", 903);
        std::fs::write(path.join("review-note.txt"), "recover me\n").unwrap();
        test_git(&path, &["add", "review-note.txt"]);
        test_git(&path, &["commit", "-m", "local review recovery"]);
        let head = test_git(&path, &["rev-parse", "HEAD"]);

        let error = repo
            .release_review_worktree_checked(903, &checkpoint)
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!(head, test_git(&path, &["rev-parse", "HEAD"]));
        assert_eq!(
            "recover me\n",
            std::fs::read_to_string(path.join("review-note.txt")).unwrap()
        );
        repo.release_review_worktree(903);
    }

    #[test]
    fn an_ignored_review_file_is_retained_after_a_checked_read() {
        let (_fixture, repo, path, checkpoint) = review_fixture("checked-ignored", 904);
        std::fs::create_dir_all(path.join("generated")).unwrap();
        std::fs::write(path.join("generated/recovery.txt"), "recover me\n").unwrap();

        let error = repo
            .release_review_worktree_checked(904, &checkpoint)
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!(
            "recover me\n",
            std::fs::read_to_string(path.join("generated/recovery.txt")).unwrap()
        );
        repo.release_review_worktree(904);
    }

    #[test]
    fn a_preexisting_ignored_review_file_change_is_retained() {
        let (_fixture, repo, path, _initial) = review_fixture("changed-existing-ignored", 905);
        std::fs::create_dir_all(path.join("generated")).unwrap();
        let ignored = path.join("generated/recovery.txt");
        std::fs::write(&ignored, "before\n").unwrap();
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();
        std::fs::write(&ignored, "after!\n").unwrap();

        let error = repo
            .release_review_worktree_checked(905, &checkpoint)
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!("after!\n", std::fs::read_to_string(&ignored).unwrap());
        repo.release_review_worktree(905);
    }

    #[test]
    fn a_preexisting_ignored_review_file_prevents_checked_removal() {
        let (_fixture, repo, path, _initial) = review_fixture("existing-ignored", 906);
        std::fs::create_dir_all(path.join("generated")).unwrap();
        let ignored = path.join("generated/recovery.txt");
        std::fs::write(&ignored, "keep me\n").unwrap();
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();

        let error = repo
            .release_review_worktree_checked(906, &checkpoint)
            .unwrap_err();

        assert!(error.to_string().contains("recoverable"), "{error}");
        assert_eq!("keep me\n", std::fs::read_to_string(&ignored).unwrap());
    }

    #[test]
    fn overwriting_a_preexisting_untracked_file_is_detected() {
        let (_fixture, repo, path, _initial) = review_fixture("changed-untracked", 907);
        let untracked = path.join("notes.txt");
        std::fs::write(&untracked, "before\n").unwrap();
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();
        std::fs::write(&untracked, "after!\n").unwrap();

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!("after!\n", std::fs::read_to_string(&untracked).unwrap());
    }

    #[test]
    fn an_assume_unchanged_edit_is_detected() {
        let (_fixture, repo, path, checkpoint) = review_fixture("assume-unchanged", 908);
        test_git(&path, &["update-index", "--assume-unchanged", "README.md"]);
        std::fs::write(path.join("README.md"), "hidden\n").unwrap();

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!(
            "hidden\n",
            std::fs::read_to_string(path.join("README.md")).unwrap()
        );
    }

    #[test]
    fn a_normalized_text_edit_is_detected_even_when_status_is_clean() {
        let (_fixture, repo, path, checkpoint) = review_fixture("normalized-text", 909);
        std::fs::write(path.join("README.md"), b"seed\r\n").unwrap();
        test_git(&path, &["add", "README.md"]);
        assert!(test_git(&path, &["status", "--porcelain"]).is_empty());

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!(
            b"seed\r\n",
            std::fs::read(path.join("README.md")).unwrap().as_slice()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_mode_edit_is_detected_when_filemode_is_disabled() {
        use std::os::unix::fs::PermissionsExt;

        let (_fixture, repo, path, checkpoint) = review_fixture("hidden-mode", 910);
        test_git(&path, &["config", "core.filemode", "false"]);
        let readme = path.join("README.md");
        let mut permissions = std::fs::metadata(&readme).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&readme, permissions).unwrap();
        assert!(test_git(&path, &["status", "--porcelain"]).is_empty());

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!(
            0o755,
            std::fs::metadata(&readme).unwrap().permissions().mode() & 0o777
        );
    }

    #[test]
    fn a_lossy_filter_cannot_hide_raw_bytes_from_a_managed_commit() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("lossy-filter", 911);
        std::fs::write(path.join(".gitattributes"), "* text\n*.txt filter=drop\n").unwrap();
        test_git(&path, &["add", ".gitattributes"]);
        test_git(&path, &["commit", "-m", "select data filter"]);
        let baseline = repo.worktree_baseline(&path).unwrap();
        std::fs::write(path.join("data.txt"), "secret: recover me\nnew\n").unwrap();

        assert!(repo
            .commit_pending_changes(&path, &baseline, "change data", "change data")
            .unwrap());
        let error = repo
            .refuse_unrepresented_tracked_changes(&path, &baseline)
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert_eq!(
            "secret: recover me\nnew\n",
            std::fs::read_to_string(path.join("data.txt")).unwrap()
        );
        assert_eq!("new\n", test_git(&path, &["show", "HEAD:data.txt"]));
    }

    #[test]
    fn a_baseline_ordinary_untracked_file_is_not_staged_by_a_managed_commit() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("baseline-untracked", 927);
        std::fs::create_dir_all(path.join("target")).unwrap();
        let untracked = path.join("target/user.yaml");
        std::fs::write(&untracked, "user data\n").unwrap();
        let baseline = repo.worktree_baseline(&path).unwrap();
        std::fs::write(path.join("README.md"), "tracked change\n").unwrap();

        assert!(repo
            .commit_pending_changes(&path, &baseline, "change readme", "change readme")
            .unwrap());

        assert_eq!("user data\n", std::fs::read_to_string(&untracked).unwrap());
        assert_eq!(
            "?? target/user.yaml\n",
            test_git(&path, &["status", "--short", "--untracked-files=all"])
        );
        assert!(test_git(
            &path,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                "HEAD",
                "--",
                "target/user.yaml"
            ]
        )
        .is_empty());
    }

    #[test]
    fn changing_a_baseline_ordinary_untracked_file_stops_a_managed_commit() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("changed-untracked", 929);
        std::fs::create_dir_all(path.join("target")).unwrap();
        let untracked = path.join("target/user.yaml");
        std::fs::write(&untracked, "before\n").unwrap();
        let baseline = repo.worktree_baseline(&path).unwrap();
        let before = test_git(&path, &["rev-parse", "HEAD"]);
        std::fs::write(&untracked, "after\n").unwrap();
        std::fs::write(path.join("README.md"), "tracked change\n").unwrap();

        let error = repo
            .commit_pending_changes(&path, &baseline, "change readme", "change readme")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("target/user.yaml"), "{error}");
        assert_eq!(before, test_git(&path, &["rev-parse", "HEAD"]));
        assert!(test_git(&path, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!("after\n", std::fs::read_to_string(&untracked).unwrap());
    }

    #[test]
    fn a_new_ordinary_untracked_file_is_staged_by_a_managed_commit() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("new-untracked", 928);
        let baseline = repo.worktree_baseline(&path).unwrap();
        std::fs::create_dir_all(path.join("target")).unwrap();
        std::fs::write(path.join("target/new.txt"), "new file\n").unwrap();

        assert!(repo
            .commit_pending_changes(&path, &baseline, "add file", "add file")
            .unwrap());

        assert_eq!(
            "new file\n",
            test_git(&path, &["show", "HEAD:target/new.txt"])
        );
        assert!(test_git(&path, &["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn deleting_existing_ignored_work_stops_a_managed_commit() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("deleted-ignored", 912);
        std::fs::create_dir_all(path.join("generated")).unwrap();
        let ignored = path.join("generated/keep.txt");
        std::fs::write(&ignored, "user data\n").unwrap();
        let baseline = repo.worktree_baseline(&path).unwrap();
        let before = test_git(&path, &["rev-parse", "HEAD"]);
        std::fs::write(path.join("README.md"), "tracked change\n").unwrap();
        std::fs::remove_file(&ignored).unwrap();

        let error = repo
            .commit_pending_changes(&path, &baseline, "change readme", "change readme")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("existing untracked"), "{error}");
        assert_eq!(before, test_git(&path, &["rev-parse", "HEAD"]));
        assert_eq!(
            "tracked change\n",
            std::fs::read_to_string(path.join("README.md")).unwrap()
        );
    }

    #[test]
    fn new_ignored_work_stops_a_managed_commit_with_tracked_changes() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("mixed-ignored", 926);
        let baseline = repo.worktree_baseline(&path).unwrap();
        let before = test_git(&path, &["rev-parse", "HEAD"]);
        std::fs::write(path.join("README.md"), "tracked change\n").unwrap();
        std::fs::create_dir_all(path.join("generated")).unwrap();
        let ignored = path.join("generated/recovery.txt");
        std::fs::write(&ignored, "keep me\n").unwrap();

        let error = repo
            .commit_pending_changes(&path, &baseline, "change readme", "change readme")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("recovery.txt"), "{error}");
        assert_eq!(before, test_git(&path, &["rev-parse", "HEAD"]));
        assert_eq!("keep me\n", std::fs::read_to_string(&ignored).unwrap());
        assert!(test_git(&path, &["status", "--porcelain"])
            .lines()
            .any(|line| line == "M  README.md"));
    }

    #[test]
    fn an_lf_override_of_an_expected_crlf_checkout_is_recoverable() {
        let (_fixture, _repo, path, _checkpoint) = review_fixture("lf-override", 913);
        test_git(&path, &["config", "core.autocrlf", "true"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        assert_eq!(
            test_git(&path, &["hash-object", "README.md"]).trim(),
            test_git(&path, &["rev-parse", "HEAD:README.md"]).trim()
        );

        assert!(repository_has_recoverable_work(&path, true).unwrap());
        assert_eq!(
            "seed\n",
            std::fs::read_to_string(path.join("README.md")).unwrap()
        );
    }

    #[test]
    fn autocrlf_input_overrides_a_crlf_core_eol() {
        let (_fixture, _repo, path, _checkpoint) = review_fixture("autocrlf-input", 923);
        test_git(&path, &["config", "core.autocrlf", "input"]);
        test_git(&path, &["config", "core.eol", "crlf"]);
        std::fs::write(path.join("README.md"), b"seed\r\n").unwrap();
        assert_eq!(
            test_git(&path, &["hash-object", "README.md"]).trim(),
            test_git(&path, &["rev-parse", "HEAD:README.md"]).trim()
        );

        assert!(repository_has_recoverable_work(&path, true).unwrap());
        assert_eq!(
            b"seed\r\n",
            std::fs::read(path.join("README.md")).unwrap().as_slice()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_permission_change_is_recoverable() {
        use std::os::unix::fs::PermissionsExt;

        let (_fixture, repo, path, checkpoint) = review_fixture("permission-change", 924);
        let readme = path.join("README.md");
        let mut permissions = std::fs::metadata(&readme).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&readme, permissions).unwrap();
        assert!(test_git(&path, &["status", "--porcelain"]).is_empty());

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(repository_has_recoverable_work(&path, true).unwrap());
        assert_eq!(
            0o600,
            std::fs::metadata(&readme).unwrap().permissions().mode() & 0o777
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_managed_commit_skips_signing_and_hooks() {
        use std::os::unix::fs::PermissionsExt;

        let (fixture, repo, path, _checkpoint) = review_fixture("managed-commit", 925);
        let common = common_git_dir(&path).unwrap();
        let hook = common.join("hooks/pre-commit");
        let marker = fixture.root.join("hook-ran");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf ran > {}\nexit 1\n",
                sh_quote(marker.to_str().unwrap())
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        test_git(&path, &["config", "commit.gpgsign", "true"]);
        test_git(&path, &["config", "gpg.program", "/usr/bin/false"]);
        std::fs::write(path.join("managed.txt"), "managed\n").unwrap();
        test_git(&path, &["add", "managed.txt"]);

        repo.commit_staged_changes(&path, "record managed change")
            .unwrap();

        assert!(!marker.exists());
        assert_eq!("managed\n", test_git(&path, &["show", "HEAD:managed.txt"]));
    }

    #[test]
    fn an_auto_text_checkout_is_retained_when_representation_is_ambiguous() {
        let (_fixture, _repo, path, _checkpoint) = review_fixture("auto-text", 914);
        std::fs::write(
            path.join(".gitattributes"),
            ".gitattributes -text\n.gitignore -text\ndata.txt -text\nREADME.md text=auto\n",
        )
        .unwrap();
        test_git(&path, &["add", ".gitattributes"]);
        test_git(&path, &["commit", "-m", "select automatic text"]);
        test_git(&path, &["config", "core.autocrlf", "true"]);

        assert!(repository_has_recoverable_work(&path, true).unwrap());
    }

    #[test]
    fn an_ident_checkout_is_retained_even_when_raw_bytes_match_the_index() {
        let (_fixture, _repo, path, _checkpoint) = review_fixture("ident", 915);
        std::fs::write(
            path.join(".gitattributes"),
            ".gitattributes -text\n.gitignore -text\ndata.txt -text\nREADME.md -text ident\n",
        )
        .unwrap();
        test_git(&path, &["add", ".gitattributes"]);
        test_git(&path, &["commit", "-m", "select ident expansion"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();

        assert!(repository_has_recoverable_work(&path, true).unwrap());
    }

    /// Add ignore rules the way SPAR does, without a tracked change the
    /// worktree would then be kept for.
    fn exclude_paths(repo: &Repo, lines: &[&str]) {
        use std::io::Write;
        let path = repo.root().join(".git").join("info").join("exclude");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn rebuilt_output_does_not_stop_a_call_that_committed_nothing() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("rebuilt-output", 939);
        exclude_paths(&repo, &["dist/"]);
        std::fs::create_dir_all(path.join("dist")).unwrap();
        std::fs::write(path.join("dist/index.js"), "first build\n").unwrap();
        let baseline = repo.worktree_baseline(&path).unwrap();
        std::fs::write(path.join("dist/index.js"), "second build\n").unwrap();
        std::fs::write(path.join("dist/extra.js"), "more output\n").unwrap();

        repo.refuse_new_ignored_files(&path, &baseline).unwrap();
        repo.refuse_changed_existing_untracked(&path, &baseline)
            .unwrap();
    }

    #[test]
    fn a_new_ignored_file_outside_build_output_still_stops_a_call() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("new-ignored-local", 940);
        exclude_paths(&repo, &["dist/", ".env.local"]);
        std::fs::create_dir_all(path.join("dist")).unwrap();
        let baseline = repo.worktree_baseline(&path).unwrap();
        std::fs::write(path.join("dist/index.js"), "a build\n").unwrap();
        std::fs::write(path.join(".env.local"), "TOKEN=x\n").unwrap();

        let error = repo.refuse_new_ignored_files(&path, &baseline).unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains(".env.local"), "{error}");
    }

    #[test]
    fn a_read_only_inspection_may_rebuild_generated_output() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("inspect-build", 937);
        exclude_paths(&repo, &["dist/"]);
        std::fs::create_dir_all(path.join("dist")).unwrap();
        std::fs::write(path.join("dist/index.js"), "first build\n").unwrap();
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();
        std::fs::write(path.join("dist/index.js"), "second build\n").unwrap();
        std::fs::write(path.join("dist/extra.js"), "more output\n").unwrap();

        repo.require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap();
    }

    #[test]
    fn a_read_only_inspection_may_not_change_an_ignored_file_elsewhere() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("inspect-local", 938);
        exclude_paths(&repo, &["dist/", ".env.local"]);
        std::fs::write(path.join(".env.local"), "TOKEN=before\n").unwrap();
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();
        std::fs::write(path.join(".env.local"), "TOKEN=after\n").unwrap();

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
    }

    #[test]
    fn build_output_alone_does_not_keep_a_worktree() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("build-output", 933);
        exclude_paths(&repo, &["target/", "dist/"]);
        std::fs::create_dir_all(path.join("target/debug")).unwrap();
        std::fs::write(path.join("target/debug/artifact"), "compiler output\n").unwrap();
        std::fs::create_dir_all(path.join("dist/cli")).unwrap();
        std::fs::write(path.join("dist/cli/index.js"), "typescript output\n").unwrap();

        assert!(!repository_has_recoverable_work(&path, true).unwrap());
        repo.release_review_worktree(933);

        assert!(!path.exists());
    }

    #[test]
    fn an_ignored_file_outside_build_output_keeps_a_worktree() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("ignored-local", 934);
        exclude_paths(&repo, &["target/", ".env.local"]);
        std::fs::create_dir_all(path.join("target/debug")).unwrap();
        std::fs::write(path.join("target/debug/artifact"), "compiler output\n").unwrap();
        std::fs::write(path.join(".env.local"), "TOKEN=keep me\n").unwrap();

        assert!(repository_has_recoverable_work(&path, true).unwrap());
        repo.release_review_worktree(934);

        assert_eq!(
            "TOKEN=keep me\n",
            std::fs::read_to_string(path.join(".env.local")).unwrap()
        );
    }

    #[test]
    fn a_repository_nested_in_build_output_keeps_a_worktree() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("nested-in-build", 935);
        exclude_paths(&repo, &["node_modules/"]);
        let nested = path.join("node_modules/local-dep");
        std::fs::create_dir_all(&nested).unwrap();
        test_git(&nested, &["init"]);
        std::fs::write(nested.join("work.txt"), "uncommitted\n").unwrap();

        assert!(repository_has_recoverable_work(&path, true).unwrap());
        repo.release_review_worktree(935);

        assert!(nested.join(".git").exists());
    }

    #[test]
    fn an_ordinary_untracked_file_keeps_a_worktree() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("ordinary-untracked", 936);
        std::fs::write(path.join("notes.md"), "somebody's notes\n").unwrap();

        assert!(repository_has_recoverable_work(&path, true).unwrap());
        repo.release_review_worktree(936);

        assert_eq!(
            "somebody's notes\n",
            std::fs::read_to_string(path.join("notes.md")).unwrap()
        );
    }

    #[test]
    fn a_legacy_crlf_checkout_is_retained_conservatively() {
        let (_fixture, _repo, path, _checkpoint) = review_fixture("legacy-crlf", 916);
        std::fs::write(
            path.join(".gitattributes"),
            ".gitattributes -text\n.gitignore -text\ndata.txt -text\nREADME.md crlf\n",
        )
        .unwrap();
        test_git(&path, &["add", ".gitattributes"]);
        test_git(&path, &["commit", "-m", "select legacy line endings"]);

        assert!(repository_has_recoverable_work(&path, true).unwrap());
    }

    #[test]
    fn a_nested_git_entry_inside_a_tracked_directory_is_recoverable() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("nested-git", 917);
        let nested = path.join("tracked");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("seed.txt"), "seed\n").unwrap();
        test_git(&path, &["add", "tracked/seed.txt"]);
        test_git(&path, &["commit", "-m", "add tracked directory"]);
        let checkpoint = repo.worktree_checkpoint(&path).unwrap();
        test_git(&nested, &["init"]);

        let error = repo
            .require_unchanged_worktree(&path, &checkpoint, "review worktree")
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("Git entry"), "{error}");
        assert!(nested.join(".git").exists());
    }

    #[test]
    fn a_resident_worktree_is_snapshotted_as_one_ignored_entry() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("resident-snapshot", 930);

        let state = ignored_untracked_state(repo.root()).unwrap();

        let relative = path.strip_prefix(repo.root()).unwrap();
        assert!(
            state.files.contains_key(relative),
            "{:?}",
            state.files.keys().collect::<Vec<_>>()
        );
        assert!(state.is_ignored(relative));
    }

    #[test]
    fn work_inside_a_resident_worktree_leaves_the_outer_baseline_alone() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("resident-churn", 931);
        let baseline = repo.worktree_baseline(repo.root()).unwrap();
        std::fs::write(path.join("scratch.txt"), "another run's work\n").unwrap();
        std::fs::write(path.join("README.md"), "another run's edit\n").unwrap();

        repo.refuse_new_ignored_files(repo.root(), &baseline)
            .unwrap();
        repo.refuse_changed_existing_untracked(repo.root(), &baseline)
            .unwrap();
    }

    #[test]
    fn deleting_a_resident_worktree_during_a_call_is_refused() {
        let (_fixture, repo, path, _checkpoint) = review_fixture("resident-deleted", 932);
        let baseline = repo.worktree_baseline(repo.root()).unwrap();
        std::fs::remove_dir_all(&path).unwrap();

        let error = repo
            .refuse_new_ignored_files(repo.root(), &baseline)
            .unwrap_err();

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("review-932"), "{error}");
    }

    #[test]
    fn a_nested_repository_record_is_read_as_a_plain_path() {
        let (path, nested) = untracked_record(b"vendor/checkout/", "untracked").unwrap();
        assert_eq!(Path::new("vendor/checkout"), path);
        assert!(nested);

        let (path, nested) = untracked_record(b"vendor/notes.txt", "untracked").unwrap();
        assert_eq!(Path::new("vendor/notes.txt"), path);
        assert!(!nested);

        assert!(untracked_record(b"/", "untracked").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_git_path_is_preserved_without_loss() {
        use std::os::unix::ffi::OsStrExt;

        let path = path_from_git_bytes(&[b'f', 0xff]).unwrap();

        assert_eq!(&[b'f', 0xff], path.as_os_str().as_bytes());
    }

    #[test]
    fn guarded_merge_pins_the_reviewed_head() {
        let args = merge_pr_args("36", Some("abc123"), true);
        assert_eq!(
            vec![
                "pr",
                "merge",
                "36",
                "--squash",
                "--delete-branch",
                "--match-head-commit",
                "abc123"
            ],
            args
        );
    }

    #[test]
    fn an_ambiguous_create_is_success_when_the_pull_request_exists() {
        let pr = PrRef {
            number: 7,
            url: "https://example.test/pull/7".into(),
            title: "part one".into(),
        };
        let result = reconcile_pr_creation(
            "split-34-1",
            Err(crate::error::SparError::new("connection lost")),
            Ok(Some(pr)),
        )
        .unwrap();
        assert_eq!(7, result.number);
    }

    #[test]
    fn a_failed_create_keeps_its_original_error_when_no_pr_exists() {
        let error = reconcile_pr_creation(
            "split-34-1",
            Err(crate::error::SparError::new("permission denied")),
            Ok(None),
        )
        .unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{error}");
    }

    #[test]
    fn a_pull_request_against_the_wrong_base_does_not_reconcile_creation() {
        let text = r#"[{"number":7,"url":"https://example.test/pull/7","title":"part one","baseRefName":"main"}]"#;
        assert!(pr_for_base(text, "split-34-2", "split-34-1")
            .unwrap()
            .is_none());
        let found = pr_for_base(text, "split-34-2", "main").unwrap().unwrap();
        assert_eq!(7, found.number);
    }

    #[test]
    fn an_ambiguous_comment_is_success_when_the_exact_body_exists() {
        let result = reconcile_comment_post(
            34,
            "the summary",
            crate::error::SparError::new("connection lost"),
            Ok(vec![serde_json::json!({"body": "the summary"})]),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn an_ambiguous_comment_preserves_failure_when_only_other_text_exists() {
        let error = reconcile_comment_post(
            34,
            "the summary",
            crate::error::SparError::new("connection lost"),
            Ok(vec![serde_json::json!({"body": "<!-- spar:split -->"})]),
        )
        .unwrap_err();
        assert_eq!("connection lost", error.to_string());
    }

    #[test]
    fn an_ambiguous_comment_reports_an_unverifiable_lookup() {
        let error = reconcile_comment_post(
            34,
            "the summary",
            crate::error::SparError::new("connection lost"),
            Err(crate::error::SparError::new("comments unavailable")),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("could not be verified"),
            "{error}"
        );
        assert!(
            error.to_string().contains("comments unavailable"),
            "{error}"
        );
        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(!error.worth_retrying());
    }

    #[test]
    fn an_ambiguous_issue_edit_is_success_when_the_wanted_body_exists() {
        let result = reconcile_issue_edit(
            34,
            "wanted body",
            crate::error::SparError::new("connection lost"),
            Ok("wanted body".to_string()),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn an_ambiguous_issue_edit_reports_an_unverifiable_lookup() {
        let error = reconcile_issue_edit(
            34,
            "wanted body",
            crate::error::SparError::new("connection lost"),
            Err(crate::error::SparError::new("issue unavailable")),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("could not be verified"),
            "{error}"
        );
        assert!(error.to_string().contains("issue unavailable"), "{error}");
        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(!error.worth_retrying());
    }

    #[test]
    fn an_ambiguous_issue_creation_recovers_the_exact_issue() {
        let found = ExistingIssue {
            number: 101,
            url: "https://example.test/issues/101".into(),
            title: "child".into(),
            body: "body".into(),
            open: true,
        };
        let url = reconcile_issue_creation(
            "child",
            Err(crate::error::SparError::new("connection lost")),
            Ok(Some(found)),
        )
        .unwrap();
        assert_eq!("https://example.test/issues/101", url);
    }

    #[test]
    fn a_failed_issue_creation_keeps_its_error_when_no_issue_exists() {
        let error = reconcile_issue_creation(
            "child",
            Err(crate::error::SparError::new("permission denied")),
            Ok(None),
        )
        .unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{error}");
    }

    #[test]
    fn an_unverifiable_issue_creation_is_marked_uncertain() {
        let error = reconcile_issue_creation(
            "child",
            Err(crate::error::SparError::new("connection lost")),
            Err(crate::error::SparError::new("issues unavailable")),
        )
        .unwrap_err();
        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(!error.worth_retrying());
    }

    #[test]
    fn an_ambiguous_split_push_is_success_when_origin_has_local_head() {
        let result = reconcile_failed_split_push(
            "split-34-1",
            crate::error::SparError::new("connection lost"),
            Ok("abc123\n".into()),
            Ok("abc123\trefs/heads/split-34-1\n".into()),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn a_split_push_collision_is_definite_and_never_overwrites() {
        let error = reconcile_failed_split_push(
            "split-34-1",
            crate::error::SparError::new("lease rejected"),
            Ok("abc123\n".into()),
            Ok("def456\trefs/heads/split-34-1\n".into()),
        )
        .unwrap_err();
        assert!(!error.retain_worktree());
        assert!(
            error.to_string().contains("Nothing was overwritten"),
            "{error}"
        );
    }

    #[test]
    fn an_unreadable_split_push_result_keeps_the_worktree() {
        let error = reconcile_failed_split_push(
            "split-34-1",
            crate::error::SparError::new("connection lost"),
            Ok("abc123\n".into()),
            Err(crate::error::SparError::new("origin unavailable")),
        )
        .unwrap_err();
        assert!(error.retain_worktree());
        assert!(error.to_string().contains("could not confirm"), "{error}");
    }

    /// Follow-up deduplication compares a title it computed against the title
    /// GitHub stored. If those two transforms can disagree, the check never
    /// matches and every review round files another copy of the same issue.
    #[test]
    fn clean_title_is_idempotent_even_when_the_scrub_lengthens_it() {
        let repo = repo_for_titles();
        for raw in [
            "Retry loop spins \u{2014} Retry-After parses to zero",
            "plain title",
            "  spread   over\nlines  ",
            "\u{1F916} Generated with something",
            &format!("a \u{2014} {}", "very long title ".repeat(20)),
            &"x".repeat(300),
            &format!("{} \u{2014} end", "y".repeat(88)),
            // Exactly the budget, with two spaceless dashes. The scrub turns
            // each "a\u{2014}b" into "a, b", so clip-then-scrub lands one
            // character over budget per dash and a second pass clips again,
            // producing a different string. Scrub-then-clip cannot.
            &{
                let tail = "a\u{2014}b c\u{2014}d";
                let pad = Style::default().max_title_chars - tail.chars().count();
                format!("{}{tail}", "w".repeat(pad))
            },
        ] {
            let once = repo.clean_title(raw).unwrap();
            let twice = repo.clean_title(&once).unwrap();
            assert_eq!(once, twice, "not idempotent for {raw:?}");
            assert!(
                once.chars().count() <= repo.style.max_title_chars,
                "over budget: {once:?}"
            );
            assert!(style::violations(&once, &repo.style).is_empty(), "{once:?}");
        }
    }

    #[test]
    fn a_title_with_an_em_dash_survives_as_readable_text() {
        let repo = repo_for_titles();
        assert_eq!(
            "Retry loop spins, Retry-After parses to zero",
            repo.clean_title("Retry loop spins \u{2014} Retry-After parses to zero")
                .unwrap()
        );
    }

    #[test]
    fn sh_quote_survives_a_quote() {
        assert_eq!(r"'a'\''b'", sh_quote("a'b"));
    }

    #[test]
    fn sh_quote_wraps_a_space() {
        assert_eq!(
            "'/Applications/My App/spar'",
            sh_quote("/Applications/My App/spar")
        );
    }

    #[test]
    fn finished_states_are_recognised_case_insensitively() {
        assert!(is_finished("MERGED"));
        assert!(is_finished("closed"));
        assert!(!is_finished("OPEN"));
        assert!(!is_finished(""));
    }

    fn state() -> PersistedState {
        PersistedState {
            version: 1,
            checkpoint: 0,
            round: 4,
            next_actor: "codex".into(),
            status: Status::Pending,
            pr_head: "abc123".into(),
            ledger: Ledger::new(),
            filed: vec![],
            open_findings: vec![Finding {
                severity: Severity::Blocking,
                title: "Unchecked error".into(),
                detail: "the failure is discarded".into(),
                file: "src/a.rs:12".into(),
                ..Finding::default()
            }],
            disputes: vec![Dispute {
                title: "Retry limit".into(),
                file: "src/net.rs".into(),
                reasoning: "the caller already bounds it".into(),
            }],
            noted: vec![Finding {
                severity: Severity::NonBlocking,
                title: "Timeout is fixed".into(),
                file: "src/config.rs".into(),
                ..Finding::default()
            }],
        }
    }

    #[test]
    fn a_state_comment_round_trips() {
        let body = format!(
            "{STATE_MARKER}\n{}\n-->",
            serde_json::to_string(&state()).unwrap()
        );
        let back = parse_state_comment(&body).unwrap();
        assert_eq!(4, back.round);
        assert_eq!("codex", back.next_actor);
        assert_eq!("abc123", back.pr_head);
        assert_eq!("Unchecked error", back.open_findings[0].title);
        assert_eq!("src/net.rs", back.disputes[0].file);
        assert_eq!("Timeout is fixed", back.noted[0].title);
    }

    #[test]
    fn old_state_without_new_lists_still_parses() {
        let body = format!(
            "{STATE_MARKER}\n{{\"version\":1,\"round\":2,\"next_actor\":\"b\",\
             \"status\":\"pending\",\"ledger\":{{}},\"filed\":[]}}\n-->"
        );
        let back = parse_state_comment(&body).expect("old state");
        assert!(back.open_findings.is_empty());
        assert!(back.disputes.is_empty());
        assert!(back.noted.is_empty());
        assert!(back.pr_head.is_empty());
        assert_eq!(0, back.checkpoint);
    }

    #[test]
    fn matching_remote_state_beats_a_newer_stale_local_checkpoint() {
        let mut local = state();
        local.pr_head = "old".into();
        local.round = 9;
        let mut remote = state();
        remote.pr_head = "current".into();
        remote.round = 4;

        let chosen = choose_state_for_head(vec![local, remote], "current").unwrap();
        assert_eq!("current", chosen.pr_head);
        assert_eq!(4, chosen.round);
    }

    #[test]
    fn checkpoint_order_breaks_same_round_ties() {
        let mut local = state();
        local.pr_head = "current".into();
        local.round = 4;
        local.checkpoint = 8;
        let mut remote = local.clone();
        remote.checkpoint = 7;
        remote.open_findings.clear();

        let chosen = choose_state_for_head(vec![local], "current").unwrap();
        assert_eq!(8, chosen.checkpoint);

        let mut local = state();
        local.pr_head = "current".into();
        local.round = 4;
        local.checkpoint = 8;
        let chosen = choose_state_for_head(vec![remote, local], "current").unwrap();
        assert_eq!(8, chosen.checkpoint);
    }

    #[test]
    fn legacy_same_round_tie_keeps_the_local_checkpoint() {
        let mut local = state();
        local.pr_head = "current".into();
        local.round = 4;
        local.open_findings.push(Finding {
            title: "local checkpoint".into(),
            ..Finding::default()
        });
        let mut remote = state();
        remote.pr_head = "current".into();
        remote.round = 4;

        let chosen = choose_state_for_head(vec![local, remote], "current").unwrap();
        assert_eq!(
            "local checkpoint",
            chosen.open_findings.last().unwrap().title
        );
    }

    /// It must render as nothing, so PRs are not littered with machine state.
    #[test]
    fn the_state_block_is_an_html_comment() {
        let body = format!(
            "{STATE_MARKER}\n{}\n-->",
            serde_json::to_string(&state()).unwrap()
        );
        assert!(body.starts_with("<!--"));
        assert!(body.trim_end().ends_with("-->"));
        assert!(!body[..body.find('{').unwrap()].contains("-->"));
    }

    #[test]
    fn an_unrelated_json_block_is_not_state() {
        assert!(parse_state_comment("here is a snippet\n```json\n{\"round\": 99}\n```").is_none());
    }

    #[test]
    fn a_malformed_state_comment_is_none_not_a_panic() {
        assert!(parse_state_comment(&format!("{STATE_MARKER}\n{{not json\n-->")).is_none());
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("spar-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state").join("pr-7.json");
        write_json_atomic(&path, &state()).unwrap();
        let files: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        assert_eq!(vec!["pr-7.json".to_string()], files);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_overwrites_rather_than_accumulating() {
        let dir = std::env::temp_dir().join(format!("spar-overwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("pr-7.json");
        for round in 1..4 {
            let mut s = state();
            s.round = round;
            write_json_atomic(&path, &s).unwrap();
        }
        let back: PersistedState =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(3, back.round);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn style_from_env_defaults_to_enforcing() {
        std::env::remove_var("SPAR_BAN_EM_DASH");
        std::env::remove_var("SPAR_BAN_AI_ATTRIBUTION");
        let style = style_from_env();
        assert!(style.ban_em_dash && style.ban_ai_attribution);
        assert!(
            !style.terse,
            "the commit filter must not truncate a commit message"
        );
    }
}

#[cfg(test)]
mod comment_page_tests {
    use super::*;

    #[test]
    fn a_single_merged_array_is_read() {
        let pages = parse_comment_pages(r#"[{"id":1,"body":"a"},{"id":2,"body":"b"}]"#);
        assert_eq!(2, pages.len());
        assert_eq!(Some(2), pages[1]["id"].as_i64());
    }

    #[test]
    fn concatenated_pages_from_an_older_gh_are_read_too() {
        let pages = parse_comment_pages(r#"[{"id":1}][{"id":2}]"#);
        assert_eq!(2, pages.len());
    }

    /// A comment body containing a bracket pair used to split the payload into
    /// two invalid halves, so no state comment was found and a resume silently
    /// started from round one.
    #[test]
    fn a_comment_body_containing_a_bracket_pair_is_not_mistaken_for_a_page_break() {
        let text = r#"[{"id":1,"body":"see [the docs][ref] for why"},{"id":2,"body":"ok"}]"#;
        let pages = parse_comment_pages(text);
        assert_eq!(2, pages.len(), "{pages:?}");
        assert!(pages[0]["body"].as_str().unwrap().contains("[ref]"));
    }

    #[test]
    fn empty_output_is_no_comments_not_a_panic() {
        assert!(parse_comment_pages("").is_empty());
        assert!(parse_comment_pages("   ").is_empty());
        assert!(parse_comment_pages("[]").is_empty());
    }

    #[test]
    fn a_gh_error_message_on_stdout_yields_nothing_rather_than_garbage() {
        assert!(parse_comment_pages("gh: Not Found (HTTP 404)").is_empty());
    }

    #[test]
    fn a_write_postcheck_rejects_truncated_comment_pages() {
        let error = try_parse_comment_pages(r#"[{"body":"the summary"}]["#).unwrap_err();
        assert!(
            error.to_string().contains("unexpected comment pages"),
            "{error}"
        );
    }

    #[test]
    fn a_write_postcheck_rejects_empty_or_non_array_output() {
        assert!(try_parse_comment_pages("").is_err());
        assert!(try_parse_comment_pages(r#"{"body":"the summary"}"#).is_err());
        assert!(try_parse_comment_pages("[]").is_ok());
    }

    #[test]
    fn state_is_found_in_the_last_matching_comment() {
        let payload = |round: u32| {
            format!(
                "{STATE_MARKER}\n{{\"version\":1,\"round\":{round},\"next_actor\":\"a\",\"status\":\"pending\",\"ledger\":{{}},\"filed\":[]}}\n-->"
            )
        };
        let text = serde_json::to_string(&serde_json::json!([
            {"id": 1, "body": payload(1)},
            {"id": 2, "body": "looks good to me"},
            {"id": 3, "body": payload(5)},
        ]))
        .unwrap();
        let pages = parse_comment_pages(&text);
        let last = pages
            .iter()
            .rev()
            .find_map(|c| parse_state_comment(c["body"].as_str().unwrap_or("")))
            .unwrap();
        assert_eq!(5, last.round);
    }
}

#[cfg(test)]
mod linked_pr_tests {
    use super::*;

    /// The exact shape `gh pr list --json closingIssuesReferences` returns.
    /// It carries an id and a whole repository object per linked issue, and a
    /// parser that chokes on those reports "no pull request", which is the one
    /// answer that makes spar implement over the top of somebody's work.
    const REAL_PAYLOAD: &str = r#"[
      {"number":14252,"title":"fix: reject leading-dash branch names",
       "url":"https://github.com/cli/cli/pull/14252",
       "closingIssuesReferences":[{"id":"I_kwDO","number":14238,
         "repository":{"id":"MDEwOlJl","name":"cli","owner":{"id":"MDEy","login":"cli"}},
         "url":"https://github.com/cli/cli/issues/14238"}]},
      {"number":14217,"title":"another change",
       "url":"https://github.com/cli/cli/pull/14217",
       "closingIssuesReferences":[{"id":"I_kwDO","number":9761,
         "repository":{"id":"MDEwOlJl","name":"cli","owner":{"id":"MDEy","login":"cli"}},
         "url":"https://github.com/cli/cli/issues/9761"}]},
      {"number":14200,"title":"unlinked work",
       "url":"https://github.com/cli/cli/pull/14200","closingIssuesReferences":[]}
    ]"#;

    #[test]
    fn a_linked_pr_is_found_whatever_its_branch_is_called() {
        let pr = find_linked_pr(REAL_PAYLOAD, 14238).expect("should find it");
        assert_eq!(14252, pr.number);
        assert_eq!("https://github.com/cli/cli/pull/14252", pr.url);
    }

    #[test]
    fn the_right_pr_is_picked_out_of_several() {
        assert_eq!(14217, find_linked_pr(REAL_PAYLOAD, 9761).unwrap().number);
    }

    #[test]
    fn an_issue_nobody_is_working_on_finds_nothing() {
        assert!(find_linked_pr(REAL_PAYLOAD, 99999).is_none());
    }

    #[test]
    fn an_unlinked_pr_is_never_matched() {
        // 14200 closes nothing, so no issue number should ever return it.
        for issue in [14200, 0, 1] {
            if let Some(pr) = find_linked_pr(REAL_PAYLOAD, issue) {
                assert_ne!(14200, pr.number, "matched a PR that closes nothing");
            }
        }
    }

    #[test]
    fn empty_or_broken_output_is_none_rather_than_a_panic() {
        assert!(find_linked_pr("", 1).is_none());
        assert!(find_linked_pr("[]", 1).is_none());
        assert!(find_linked_pr("gh: Not Found (HTTP 404)", 1).is_none());
        assert!(find_linked_pr("[{\"number\":", 1).is_none());
    }

    /// A fork PR cannot be pushed to, so the flag has to survive parsing.
    #[test]
    fn pr_view_reads_the_cross_repository_flag() {
        let json = r#"{"number":7,"url":"u","title":"t","headRefName":"patch-1",
                       "baseRefName":"main","state":"OPEN",
                       "closingIssuesReferences":[],"isCrossRepository":true}"#;
        let pr: PrView = serde_json::from_str(json).unwrap();
        assert!(pr.is_cross_repository);
        assert!(pr.is_open());

        let same_repo = json.replace("\"isCrossRepository\":true", "\"isCrossRepository\":false");
        assert!(
            !serde_json::from_str::<PrView>(&same_repo)
                .unwrap()
                .is_cross_repository
        );
    }
}

#[cfg(test)]
mod min_number_tests {
    /// The floor is applied before the cap, which is the order that matters.
    /// spar takes the *lowest* numbered open items, so a repository with a tail
    /// of old issues would otherwise spend its whole run in the tail: the cap
    /// would be filled by the oldest items and the floor would never be
    /// reached. Filtering first is what makes the setting do anything.
    fn pick(open: &[i64], limit: usize, min_number: i64) -> Vec<i64> {
        let mut numbers: Vec<i64> = open.to_vec();
        numbers.sort_unstable();
        if min_number > 0 {
            numbers.retain(|n| *n >= min_number);
        }
        numbers.truncate(limit);
        numbers
    }

    #[test]
    fn the_floor_is_applied_before_the_cap_not_after() {
        let open = [12, 13, 14, 480, 481, 482];
        assert_eq!(vec![480, 481], pick(&open, 2, 480));
        // Capping first would have returned the two oldest and then filtered
        // them all away, leaving nothing.
        assert!(!pick(&open, 2, 480).is_empty());
    }

    #[test]
    fn no_floor_keeps_the_old_behaviour() {
        assert_eq!(vec![12, 13], pick(&[12, 13, 14, 480], 2, 0));
    }

    #[test]
    fn the_floor_is_inclusive() {
        assert_eq!(vec![480, 481], pick(&[479, 480, 481], 10, 480));
    }

    #[test]
    fn a_floor_above_everything_open_yields_nothing() {
        assert!(pick(&[1, 2, 3], 10, 9999).is_empty());
    }
}
