//! git and gh. Every outbound string passes through the style and concision
//! gates before it reaches GitHub.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use serde_json::Value;

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
}

fn merge_pr_args<'a>(number: &'a str, expected_head: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec!["pr", "merge", number, "--squash", "--delete-branch"];
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
        let mut argv = vec!["git".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        proc::run(&argv, &self.git_opts(cwd, true))
    }

    /// Run git, tolerating failure. Returns whatever landed on stdout.
    pub fn git_try(&self, args: &[&str]) -> String {
        self.git_try_at(None, args)
    }

    pub fn git_try_at(&self, cwd: Option<&Path>, args: &[&str]) -> String {
        let mut argv = vec!["git".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
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

        self.git_try(&["fetch", "origin", base]);

        // Never rebuild a branch that already carries work.
        //
        // `run_issue` sends an issue with an open pull request to the resume
        // path, so reaching here with a remote branch ahead of the base means
        // commits were pushed that no open PR accounts for. Rebuilding would
        // force push over them, and the lease is no protection: the remote
        // tracking ref survives the local branch being deleted, so it still
        // matches and the push succeeds.
        self.git_try(&["fetch", "origin", &branch]);
        let remote_branch = format!("origin/{branch}");
        if self.rev_exists(&self.root, &remote_branch) {
            let ahead = self.commit_count(&self.root, &remote_branch, base);
            if ahead > 0 {
                bail!(
                    "origin/{branch} already has {ahead} commit(s) that are not on {base}, and no \
                     open pull request accounts for them. Rebuilding it would force push over \
                     that work.\nOpen a pull request for the branch and run `spar resume <pr>` to \
                     continue it, or delete it with `git push origin --delete {branch}` if it is \
                     stale."
                );
            }
        }

        // The same guard, for commits that never reached the remote at all.
        //
        // An agent commits as it goes, so a run that dies after the commits and
        // before the push leaves the local branch holding the only copy. With
        // nothing on origin there is no remote branch to guard and no pull
        // request to find the work by, and `git branch -D` below would leave it
        // reachable from the reflog alone, which nothing would tell anyone to
        // look at.
        if self.rev_exists(&self.root, &branch) {
            let ahead = self.commit_count(&self.root, &branch, base);
            if ahead > 0 && !self.pull_request_holds(&branch, base) {
                let listed = self
                    .commit_lines(&self.root, &branch, base)
                    .iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!(
                    "the local branch {branch} has {ahead} commit(s) that are not on {base}, and \
                     nothing accounts for them: no branch on origin and no pull request that \
                     holds them. Rebuilding it would delete the only copy.\n{listed}\nPush it \
                     and run `spar \
                     resume <pr>` on the pull request to continue it, or delete it with `git \
                     branch -D {branch}` if it is stale."
                );
            }
        }

        self.worktree_remove(issue);
        self.git_try(&["branch", "-D", &branch]);

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

    /// Whether a pull request already holds every commit `branch` has beyond
    /// `base`.
    ///
    /// GitHub serves `refs/pull/N/head` for as long as the repository lives, so
    /// commits that reached a pull request outlive the branch they were pushed
    /// from. A matching branch name does not establish that on its own: an
    /// issue worked twice reuses the name, and the merged pull request from the
    /// first round says nothing about where the second round's commits are.
    fn pull_request_holds(&self, branch: &str, base: &str) -> bool {
        self.prs_for_branch(branch)
            .iter()
            .any(|pr| self.pr_head_holds(pr.number, branch, base))
    }

    fn pr_head_holds(&self, number: i64, branch: &str, base: &str) -> bool {
        let head = format!("refs/spar/pr-head/{number}");
        let refspec = format!("+refs/pull/{number}/head:{head}");
        if self.git(&["fetch", "origin", &refspec]).is_err() {
            return false;
        }
        let held = self.commits_held_by(branch, base, &head);
        self.git_try(&["update-ref", "-d", &head]);
        held
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

    pub fn worktree_remove(&self, issue: i64) {
        self.remove_worktree_at(&self.worktree_path(&format!("issue-{issue}")));
    }

    fn remove_worktree_at(&self, path: &Path) {
        let path_str = path.display().to_string();
        self.git_try(&["worktree", "remove", "--force", &path_str]);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
        self.git_try(&["worktree", "prune"]);
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
        self.remove_worktree_at(&path);
        self.git_try(&["branch", "-D", &local]);

        let path_str = path.display().to_string();
        let start = format!("origin/{head}");
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
        self.remove_worktree_at(&path);
        let path_str = path.display().to_string();
        self.git(&["worktree", "add", "--detach", &path_str, &local_ref])?;
        Ok(path)
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
        // The name is free, so there is no branch to delete. A directory can
        // still be in the way, left by a worktree that was pruned from git's
        // records without being removed from disk.
        self.remove_worktree_at(&path);

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
        self.remove_worktree_at(dir);
        self.git_try(&["branch", "-D", branch]);
        self.forget_branch(branch);
    }

    pub fn release_review_worktree(&self, number: i64) {
        self.remove_worktree_at(&self.worktree_path(&format!("review-{number}")));
        self.git_try(&["update-ref", "-d", &review_ref(number)]);
    }

    pub fn release_pr_worktree(&self, number: i64) {
        let path = self.worktree_path(&format!("pr-{number}"));
        self.remove_worktree_at(&path);
        let local = self.branch_for_pr(number);
        self.git_try(&["branch", "-D", &local]);
        self.forget_branch(&local);
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
    /// offending commit onward, so a head recorded before a round can still be a
    /// readable object and no longer be on the branch. `git log` answers that
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
    pub fn rewrite_commits_if_needed(&self, cwd: &Path, base: &str) -> Result<()> {
        let range = format!("{}..HEAD", self.base_ref(cwd, base));
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

    /// Push by explicit refspec from HEAD.
    ///
    /// A resumed PR is checked out under a local name (`pr-N`) that does not
    /// match its remote branch, so pushing by branch name would resolve the
    /// wrong local ref or fail outright.
    pub fn push(&self, cwd: &Path, branch: &str) -> Result<()> {
        let refspec = format!("HEAD:{branch}");
        self.git_at(
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
        })
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
        let pushed = self.git_at(Some(cwd), &["push", &lease, "origin", &refspec]);
        let Err(push_error) = pushed else {
            return Ok(());
        };
        let local = self.git_at(Some(cwd), &["rev-parse", "HEAD"]);
        let remote = self.git(&["ls-remote", "--heads", "origin", &remote_ref]);
        reconcile_failed_split_push(branch, push_error, local, remote)
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

    pub fn pr_state(&self, number: i64) -> String {
        let text = self.gh_try(&["pr", "view", &number.to_string(), "--json", "state"]);
        serde_json::from_str::<Value>(text.trim())
            .ok()
            .and_then(|v| v.get("state").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
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
        let title = self.clean_title(title)?;
        let body = self.clean(body)?;
        let mut argv = vec![
            "pr", "create", "--base", base, "--head", branch, "--title", &title, "--body", &body,
        ];
        if self.drafts != Drafts::Never {
            argv.push("--draft");
        }
        let created = self.gh_at(Some(cwd), &argv);
        let found = self.try_pr_for_branch(branch, base);
        reconcile_pr_creation(branch, created, found)
    }

    pub fn comment_pr(&self, number: i64, body: &str) -> Result<()> {
        let body = self.clean(body)?;
        if has_exact_comment(&self.try_issue_comments(number)?, &body) {
            return Ok(());
        }
        let posted = self.gh(&["pr", "comment", &number.to_string(), "--body", &body]);
        let Err(post_error) = posted else {
            return Ok(());
        };
        reconcile_comment_post(number, &body, post_error, self.try_issue_comments(number))
    }

    pub fn comment_issue(&self, number: i64, body: &str) -> Result<()> {
        let body = self.clean(body)?;
        if has_exact_comment(&self.try_issue_comments(number)?, &body) {
            return Ok(());
        }
        let posted = self.gh(&["issue", "comment", &number.to_string(), "--body", &body]);
        let Err(post_error) = posted else {
            return Ok(());
        };
        reconcile_comment_post(number, &body, post_error, self.try_issue_comments(number))
    }

    /// Comment, then close as not planned.
    ///
    /// Only ever called when both agents independently declined the issue: one
    /// agent's opinion is not enough to close somebody's report.
    pub fn close_issue(&self, number: i64, body: &str) -> Result<()> {
        self.comment_issue(number, body)?;
        let n = number.to_string();
        if self
            .gh(&["issue", "close", &n, "--reason", "not planned"])
            .is_ok()
        {
            return Ok(());
        }
        // Older gh builds do not take --reason.
        self.gh(&["issue", "close", &n]).map(|_| ()).map_err(|e| {
            spar_err!(
                "commented on #{number} but could not close it: {}",
                e.last_line()
            )
        })
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
        let cleaned = self.clean(inserted)?;
        if cleaned.trim() != inserted.trim() {
            bail!(
                "the style gate rewrote {inserted:?} to {cleaned:?}, so it is not being inserted"
            );
        }
        let current = self.issue_body(number)?;
        if current != expected {
            bail!(
                "the body of #{number} changed since it was read, so it was left alone rather \
                 than written over."
            );
        }
        let edited = self.gh_stdin(
            &["issue", "edit", &number.to_string(), "--body-file", "-"],
            body,
        );
        let Err(edit_error) = edited else {
            return Ok(());
        };
        reconcile_issue_edit(number, body, edit_error, self.issue_body(number))
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
        let title = self.clean_title(title)?;
        let body = self.clean_issue_body(body)?;
        let created = self.gh(&["issue", "create", "--title", &title, "--body", &body]);
        if created.as_ref().is_ok_and(|url| issue_url_has_number(url)) {
            return Ok(created.unwrap().trim().to_string());
        }
        let found = self.try_exact_issue_apart_from(&title, &body, apart_from);
        reconcile_issue_creation(&title, created, found)
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
    /// Best effort. A draft that stayed a draft is a cosmetic problem, and
    /// failing the run over it would throw away a review that has already
    /// finished and been posted.
    pub fn mark_ready(&self, number: i64) -> bool {
        match self.gh(&["pr", "ready", &number.to_string()]) {
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
        match self.gh(&merge_pr_args(&n, None)) {
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
        }
    }

    /// Squash merge only if the pull request still exposes the reviewed head.
    pub fn merge_pr_at_head(&self, number: i64, expected_head: &str) -> Result<()> {
        let n = number.to_string();
        match self.gh(&merge_pr_args(&n, Some(expected_head))) {
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
        }
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
        for body in self.state_comment_bodies(number).into_iter().rev() {
            if let Some(state) = parse_state_comment(&body) {
                return Some(state);
            }
        }
        None
    }

    pub fn write_state(&self, number: i64, state: &PersistedState) -> Result<()> {
        let local_checkpoint = self
            .state_store
            .writes_local()
            .then(|| self.read_local_state(number))
            .flatten()
            .map(|saved| saved.checkpoint)
            .unwrap_or_default();
        let remote_checkpoint = self
            .state_store
            .writes_pr()
            .then(|| self.read_pr_state(number))
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
        let body = format!(
            "{STATE_MARKER}\n{}\n-->",
            serde_json::to_string_pretty(state)?
        );
        if let Some(id) = self.state_comment_id(number) {
            let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
            let field = format!("body={body}");
            return self
                .gh(&["api", "-X", "PATCH", &path, "-f", &field, "--silent"])
                .map(|_| ());
        }
        self.gh(&["pr", "comment", &number.to_string(), "--body", &body])
            .map(|_| ())
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

    fn state_comments(&self, number: i64) -> Vec<(i64, String)> {
        self.issue_comments(number)
            .into_iter()
            .filter_map(|c| {
                let body = c.get("body").and_then(Value::as_str)?.to_string();
                if !body.contains("spar:state") {
                    return None;
                }
                let id = c.get("id").and_then(Value::as_i64)?;
                Some((id, body))
            })
            .collect()
    }

    fn state_comment_bodies(&self, number: i64) -> Vec<String> {
        self.state_comments(number)
            .into_iter()
            .map(|(_, b)| b)
            .collect()
    }

    fn state_comment_id(&self, number: i64) -> Option<i64> {
        self.state_comments(number).last().map(|(id, _)| *id)
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
        let numbers = numbers.unwrap_or_else(|| {
            let text = self.gh_try(&[
                "pr", "list", "--state", "all", "--limit", "200", "--json", "number",
            ]);
            serde_json::from_str::<Vec<Row>>(text.trim())
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.number)
                .collect()
        });

        let mut removed = Vec::new();
        for number in numbers {
            if !is_finished(&self.pr_state(number)) {
                continue;
            }
            for (id, _) in self.state_comments(number) {
                let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
                self.gh_try(&["api", "-X", "DELETE", &path, "--silent"]);
                removed.push(format!("state comment on PR #{number}"));
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
                    self.release_review_worktree(number);
                    removed.push(name);
                    continue;
                }
                let branch = format!("{}{name}", self.branch_prefix);
                if !(force_all || self.worktree_is_done(&branch)) {
                    continue;
                }
                self.remove_worktree_at(&base.join(&name));
                self.git_try(&["branch", "-D", &branch]);
                self.forget_branch(&branch);
                removed.push(name);
            }
        }
        if !removed.is_empty() {
            self.git_try(&["worktree", "prune"]);
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
        let branches: Vec<String> = self.known_branches().keys().cloned().collect();
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
            match self.git(&["branch", "-D", &branch]) {
                Ok(_) => {
                    self.forget_branch(&branch);
                    removed.push(format!("branch {branch}"));
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
    use crate::model::{Dispute, Finding, Ledger, Severity, Status};

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
        }
    }

    #[test]
    fn guarded_merge_pins_the_reviewed_head() {
        let args = merge_pr_args("36", Some("abc123"));
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
