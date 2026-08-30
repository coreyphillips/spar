//! End to end against a real git repository.
//!
//! Everything here runs on git alone: no network, no gh, no model. These cover
//! the mechanisms that unit tests cannot reach, in particular the commit
//! message rewrite, which shells out to `git filter-branch` and calls back into
//! this same binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use spar::config::{self, Config, Followups};
use spar::model::{Followup, IssueRun};
use spar::repo::Repo;
use spar::review::{drop_uncommitted, file_followup, park, snapshot, undo_edits};

const SPAR_BIN: &str = env!("CARGO_BIN_EXE_spar");

const TWO_AGENTS: &str = "\
[agents.a]
command = [\"true\"]

[agents.b]
command = [\"true\"]
";

struct Fixture {
    dir: PathBuf,
    work: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn unique(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("spar-it-{tag}-{}-{nanos}-{n}", std::process::id()))
}

/// A work tree with an `origin` behind it, so `origin/main..HEAD` resolves.
fn repo(tag: &str) -> Fixture {
    let dir = unique(tag);
    let origin = dir.join("origin.git");
    let work = dir.join("work");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&work).unwrap();

    git(&origin, &["init", "--bare", "-b", "main"]);
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.email", "spar@example.invalid"]);
    git(&work, &["config", "user.name", "spar test"]);
    git(&work, &["config", "commit.gpgsign", "false"]);

    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "seed"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&work, &["push", "-u", "origin", "main"]);

    Fixture { dir, work }
}

fn cfg() -> Config {
    config::parse(TWO_AGENTS).unwrap()
}

fn commit(work: &Path, file: &str, body: &str, message: &str) {
    std::fs::write(work.join(file), body).unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-m", message]);
}

fn messages(work: &Path) -> String {
    git(work, &["log", "origin/main..HEAD", "--format=%B"])
}

// ---------------------------------------------------------------------------
// Opening a repository
// ---------------------------------------------------------------------------

#[test]
fn a_real_repository_opens() {
    let fx = repo("open");
    assert!(Repo::open(&fx.work, &cfg()).is_ok());
}

#[test]
fn a_directory_that_is_not_a_repository_is_refused() {
    let dir = unique("notarepo");
    std::fs::create_dir_all(&dir).unwrap();
    let err = Repo::open(&dir, &cfg()).unwrap_err().to_string();
    assert!(err.contains("not a git repository"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_default_branch_comes_from_origin_head_not_an_assumption() {
    let fx = repo("defaultbranch");
    git(
        &fx.work,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert_eq!("main", repo.default_branch("some-other-guess"));
}

#[test]
fn a_repository_with_no_origin_head_falls_back_to_the_configured_branch() {
    let fx = repo("nohead");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert_eq!("trunk", repo.default_branch("trunk"));
}

// ---------------------------------------------------------------------------
// The style gate over commit messages
// ---------------------------------------------------------------------------

/// The plumbing subcommand git calls. Verified directly, because a bug here is
/// invisible until it has already rewritten somebody's history.
#[test]
fn the_scrub_filter_subcommand_cleans_stdin() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(SPAR_BIN)
        .arg("scrub-filter")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Add retry logic \xE2\x80\x94 with backoff\n\nCo-Authored-By: Claude <x@y.z>\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(out.status.success());
    assert!(text.contains("Add retry logic"), "{text}");
    assert!(!text.contains('\u{2014}'), "{text}");
    assert!(!text.contains("Co-Authored-By"), "{text}");
}

#[test]
fn an_offending_commit_message_is_rewritten_in_place() {
    let fx = repo("rewrite");
    std::env::set_var("SPAR_SELF_BIN", SPAR_BIN);
    commit(
        &fx.work,
        "a.txt",
        "one\n",
        "Add the parser \u{2014} finally\n\nCo-Authored-By: Claude Opus <noreply@anthropic.com>",
    );

    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.rewrite_commits_if_needed(&fx.work, "main").unwrap();

    let log = messages(&fx.work);
    assert!(!log.contains('\u{2014}'), "{log}");
    assert!(!log.contains("Co-Authored-By"), "{log}");
    assert!(
        log.contains("Add the parser"),
        "the message must survive, not vanish: {log}"
    );
}

#[test]
fn a_clean_history_is_left_completely_alone() {
    let fx = repo("clean");
    std::env::set_var("SPAR_SELF_BIN", SPAR_BIN);
    commit(&fx.work, "a.txt", "one\n", "Add the parser");
    let before = git(&fx.work, &["rev-parse", "HEAD"]);

    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.rewrite_commits_if_needed(&fx.work, "main").unwrap();

    assert_eq!(
        before,
        git(&fx.work, &["rev-parse", "HEAD"]),
        "no rewrite, no new sha"
    );
}

#[test]
fn only_the_offending_commit_of_several_loses_its_dash() {
    let fx = repo("multi");
    std::env::set_var("SPAR_SELF_BIN", SPAR_BIN);
    commit(&fx.work, "a.txt", "one\n", "First commit, perfectly fine");
    commit(
        &fx.work,
        "b.txt",
        "two\n",
        "Second commit \u{2013} not fine",
    );
    commit(&fx.work, "c.txt", "three\n", "Third commit, also fine");

    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.rewrite_commits_if_needed(&fx.work, "main").unwrap();

    let log = messages(&fx.work);
    assert!(!log.contains('\u{2013}'), "{log}");
    for text in ["First commit", "Second commit", "Third commit"] {
        assert!(log.contains(text), "{text} was lost:\n{log}");
    }
    assert_eq!(
        3,
        git(&fx.work, &["rev-list", "origin/main..HEAD"])
            .lines()
            .count()
    );
}

#[test]
fn the_tree_is_unchanged_by_a_message_rewrite() {
    let fx = repo("tree");
    std::env::set_var("SPAR_SELF_BIN", SPAR_BIN);
    commit(
        &fx.work,
        "a.txt",
        "content that must survive\n",
        "Work \u{2014} done",
    );
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.rewrite_commits_if_needed(&fx.work, "main").unwrap();
    assert_eq!(
        "content that must survive\n",
        std::fs::read_to_string(fx.work.join("a.txt")).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Change detection
// ---------------------------------------------------------------------------

#[test]
fn an_untouched_branch_has_no_changes() {
    let fx = repo("nochanges");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert!(!repo.has_changes(&fx.work, "main"));
}

#[test]
fn a_committed_branch_has_changes_and_a_diffstat() {
    let fx = repo("changes");
    commit(&fx.work, "a.txt", "one\n", "Add a file");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert!(repo.has_changes(&fx.work, "main"));
    let stat = repo.diff_stat(&fx.work, "main");
    assert!(stat.contains("1 file changed"), "{stat}");
    assert!(
        stat.lines().count() == 1,
        "the PR body wants one line, not a file list: {stat}"
    );
}

// ---------------------------------------------------------------------------
// Custody follows the commit that landed
// ---------------------------------------------------------------------------

/// The review prompt says not to write, and this is what makes that true. A
/// commit made while reviewing would be a commit its own author reviews next
/// round, which is the one thing the alternating loop exists to prevent.
#[test]
fn a_commit_made_while_reviewing_is_rolled_back() {
    let fx = repo("reviewcommit");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let before = snapshot(&repo, &fx.work);

    commit(
        &fx.work,
        "README.md",
        "reviewer was here\n",
        "Sneak a fix in",
    );
    let during = snapshot(&repo, &fx.work);
    assert!(during.landed_over(&before), "the fixture committed nothing");

    let after = undo_edits(&repo, &fx.work, &before);
    assert_eq!(before, after);
    assert_eq!(
        "seed\n",
        std::fs::read_to_string(fx.work.join("README.md")).unwrap()
    );
}

/// The review prompt asks for a scratch file when a claim needs running to
/// check it. Counting one as a mutation would roll back every review that did
/// as it was told, and throw away the scratch file with it.
#[test]
fn a_scratch_file_written_while_reviewing_is_not_a_mutation() {
    let fx = repo("scratch");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let before = snapshot(&repo, &fx.work);

    std::fs::write(fx.work.join("check.sh"), "echo hi\n").unwrap();
    assert_eq!(before, snapshot(&repo, &fx.work));

    // And the rollback a real mutation triggers leaves it where it is.
    std::fs::write(fx.work.join("README.md"), "edited\n").unwrap();
    git(&fx.work, &["commit", "-m", "Sneak a fix in", "README.md"]);
    undo_edits(&repo, &fx.work, &before);
    assert!(fx.work.join("check.sh").exists());
}

/// An edit left in the working tree is never pushed, so it must not be carried
/// into the round that follows either.
#[test]
fn an_uncommitted_edit_made_while_reviewing_is_rolled_back() {
    let fx = repo("reviewdirty");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let before = snapshot(&repo, &fx.work);

    std::fs::write(fx.work.join("README.md"), "reviewer was here\n").unwrap();
    let during = snapshot(&repo, &fx.work);
    assert!(during.dirty);
    assert!(!during.landed_over(&before));

    assert_eq!(before, undo_edits(&repo, &fx.work, &before));
}

/// A fix left in the working tree is code the next review reads and the pull
/// request does not have, so it goes. What the call did commit stays.
#[test]
fn a_fix_left_uncommitted_does_not_reach_the_next_round() {
    let fx = repo("dirtyfix");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    commit(&fx.work, "a.txt", "one\n", "Fix the finding");
    let committed = snapshot(&repo, &fx.work);

    std::fs::write(fx.work.join("a.txt"), "two\n").unwrap();
    std::fs::write(fx.work.join("check.sh"), "echo hi\n").unwrap();

    assert_eq!(committed, drop_uncommitted(&repo, &fx.work));
    assert_eq!(
        "one\n",
        std::fs::read_to_string(fx.work.join("a.txt")).unwrap()
    );
    assert!(
        fx.work.join("check.sh").exists(),
        "scratch files are not ours"
    );
}

/// A shared checkout is the user's own, and nothing here can tell an agent's
/// leftovers from an edit somebody made while a call was running, so what the
/// rollback throws away is recoverable. The stash stack is left alone: it
/// belongs to whoever is working in the repository.
#[test]
fn what_the_rollback_throws_away_is_saved_first() {
    let fx = repo("parked");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let before = snapshot(&repo, &fx.work);
    std::fs::write(fx.work.join("README.md"), "somebody was editing this\n").unwrap();

    // The rollback parks the same way; doing it here is how the test gets hold
    // of the handle the log prints.
    let parked = park(&repo, &fx.work).expect("a dirty tree has something to save");
    assert_eq!(before, undo_edits(&repo, &fx.work, &before));
    assert_eq!(
        "seed\n",
        std::fs::read_to_string(fx.work.join("README.md")).unwrap()
    );
    assert!(git(&fx.work, &["stash", "list"]).trim().is_empty());

    git(&fx.work, &["stash", "apply", &parked]);
    assert_eq!(
        "somebody was editing this\n",
        std::fs::read_to_string(fx.work.join("README.md")).unwrap()
    );
}

/// The `fix_myself` bug, at the level the loop reads. A call that returns
/// successfully having committed nothing leaves the head with whoever wrote it,
/// and `landed_over` is what says so.
#[test]
fn a_call_that_returns_without_committing_leaves_the_head_alone() {
    let fx = repo("nofix");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let before = snapshot(&repo, &fx.work);
    assert!(!snapshot(&repo, &fx.work).landed_over(&before));

    commit(&fx.work, "a.txt", "one\n", "Fix the finding");
    assert!(snapshot(&repo, &fx.work).landed_over(&before));
}

// ---------------------------------------------------------------------------
// Worktrees and the branch ledger
// ---------------------------------------------------------------------------

#[test]
fn a_worktree_is_created_on_the_base_branch_and_recorded() {
    let fx = repo("worktree");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let (path, branch) = repo.worktree_add(42, "main").unwrap();

    assert!(path.is_dir(), "{}", path.display());
    assert_eq!("issue-42", branch);
    assert!(
        path.join("README.md").is_file(),
        "the worktree has the base branch content"
    );
    assert!(
        repo.known_branches().contains_key("issue-42"),
        "an unrecorded branch is never cleaned"
    );

    repo.worktree_remove(42);
    assert!(!path.is_dir());
}

#[test]
fn a_branch_prefix_namespaces_the_branch() {
    let fx = repo("prefix");
    let mut c = cfg();
    c.loop_cfg.branch_prefix = "spar/".into();
    let repo = Repo::open(&fx.work, &c).unwrap();
    assert_eq!("spar/issue-7", repo.branch_for_issue(7));
    assert_eq!("spar/pr-7", repo.branch_for_pr(7));

    let (_, branch) = repo.worktree_add(7, "main").unwrap();
    assert_eq!("spar/issue-7", branch);
    repo.worktree_remove(7);
}

/// The data loss guard. `issue-9` here belongs to a person, not to spar, and
/// even `--all` must not remove it. Ownership comes from the ledger, never from
/// the name, because `issue-9` is exactly what somebody would call a branch.
#[test]
fn a_branch_spar_did_not_create_survives_even_a_forced_clean() {
    let fx = repo("dataloss");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    git(&fx.work, &["branch", "issue-9"]);
    git(&fx.work, &["branch", "my-own-work"]);

    let removed = repo.prune_worktrees(true);

    assert!(removed.is_empty(), "{removed:?}");
    let branches = git(
        &fx.work,
        &["for-each-ref", "refs/heads/", "--format=%(refname:short)"],
    );
    assert!(branches.contains("issue-9"), "{branches}");
    assert!(branches.contains("my-own-work"), "{branches}");
}

#[test]
fn a_recorded_branch_is_removed_by_a_forced_clean() {
    let fx = repo("forced");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let (path, branch) = repo.worktree_add(11, "main").unwrap();
    assert!(path.is_dir());

    let removed = repo.prune_worktrees(true);

    assert!(!removed.is_empty(), "nothing was cleaned");
    assert!(!path.is_dir(), "the worktree directory survived");
    let branches = git(
        &fx.work,
        &["for-each-ref", "refs/heads/", "--format=%(refname:short)"],
    );
    assert!(!branches.contains(&branch), "{branches}");
    assert!(
        !repo.known_branches().contains_key(&branch),
        "the record outlived the branch"
    );
}

#[test]
fn a_recorded_branch_already_deleted_by_hand_is_forgotten() {
    let fx = repo("forget");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.record_branch("issue-99", "issue", 99);
    repo.prune_worktrees(false);
    assert!(!repo.known_branches().contains_key("issue-99"));
}

// ---------------------------------------------------------------------------
// Splitting
// ---------------------------------------------------------------------------

/// A part branch has to go through `record_branch`, or `prune_branches` can
/// never clean it up: ownership comes from the ledger, never from the name.
#[test]
fn a_split_part_gets_its_own_branch_off_the_base_and_is_recorded() {
    let fx = repo("split-part");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    commit(&fx.work, "a.rs", "one\n", "on main");
    git(&fx.work, &["push", "origin", "main"]);

    let (path, branch) = repo.worktree_for_split(12, 1, "origin/main").unwrap();

    assert_eq!("split-12-1", branch);
    assert!(path.is_dir(), "{}", path.display());
    assert!(path.join("a.rs").is_file(), "it starts from the base");
    assert!(repo.known_branches().contains_key("split-12-1"));

    // Its own namespace, so it cannot collide with the branch of the issue that
    // happens to share the parent's number.
    assert_ne!(repo.branch_for_issue(12), branch);
    assert_ne!(repo.branch_for_pr(12), branch);
}

#[test]
fn a_branch_prefix_namespaces_a_part_branch_too() {
    let fx = repo("split-prefix");
    let mut c = cfg();
    c.loop_cfg.branch_prefix = "spar/".into();
    let repo = Repo::open(&fx.work, &c).unwrap();
    assert_eq!("spar/split-12-2", repo.branch_for_split(12, 2));

    let (_, branch) = repo.worktree_for_split(12, 2, "main").unwrap();
    assert_eq!("spar/split-12-2", branch);
    repo.release_split_worktree(12, 2);
}

/// A part that will not stand on its own is dropped, and nothing was pushed at
/// that point, so it has to leave no trace anywhere but the log.
#[test]
fn a_dropped_part_takes_its_branch_and_its_record_with_it() {
    let fx = repo("split-drop");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let (path, branch) = repo.worktree_for_split(12, 1, "main").unwrap();

    repo.release_split_worktree(12, 1);

    assert!(!path.is_dir(), "the worktree survived");
    let branches = git(
        &fx.work,
        &["for-each-ref", "refs/heads/", "--format=%(refname:short)"],
    );
    assert!(!branches.contains(&branch), "{branches}");
    assert!(!repo.known_branches().contains_key(&branch));
}

#[test]
fn a_part_branch_is_swept_by_clean_all_like_any_other() {
    let fx = repo("split-clean");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let (path, branch) = repo.worktree_for_split(12, 1, "main").unwrap();

    let removed = repo.prune_worktrees(true);

    assert!(!removed.is_empty(), "nothing was cleaned");
    assert!(!path.is_dir());
    assert!(!repo.known_branches().contains_key(&branch));
}

/// The mechanical heart of a pull request split, and the case it gets wrong:
/// a file the change deleted is not there to check out, so it has to be removed
/// rather than fetched. Everything the slice does not name stays as the base
/// had it.
#[test]
fn a_slice_carries_only_its_own_files_deletions_included() {
    let fx = repo("split-slice");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    for (name, body) in [("a.rs", "one\n"), ("b.rs", "two\n"), ("c.rs", "three\n")] {
        std::fs::write(fx.work.join(name), body).unwrap();
    }
    git(&fx.work, &["add", "-A"]);
    git(&fx.work, &["commit", "-m", "base"]);
    git(&fx.work, &["push", "origin", "main"]);

    // What the pull request did: edited a.rs and b.rs, added new.rs, deleted c.rs.
    git(&fx.work, &["checkout", "-b", "theirs"]);
    std::fs::write(fx.work.join("a.rs"), "one, changed\n").unwrap();
    std::fs::write(fx.work.join("b.rs"), "two, changed\n").unwrap();
    std::fs::write(fx.work.join("new.rs"), "added\n").unwrap();
    std::fs::remove_file(fx.work.join("c.rs")).unwrap();
    git(&fx.work, &["add", "-A"]);
    git(&fx.work, &["commit", "-m", "theirs"]);

    // What the proposing agent is shown, read the way the split reads it: from
    // the checked out head, against the base.
    assert_eq!(
        vec!["a.rs", "b.rs", "c.rs", "new.rs"],
        repo.changed_files(&fx.work, "main")
    );
    assert!(repo.path_exists_at(&fx.work, "theirs", "a.rs"));
    assert!(
        !repo.path_exists_at(&fx.work, "theirs", "c.rs"),
        "a file the change deleted is not at its head"
    );
    git(&fx.work, &["checkout", "main"]);

    let (dir, _) = repo.worktree_for_split(12, 1, "main").unwrap();
    let slice: Vec<String> = ["a.rs", "c.rs"].iter().map(|s| s.to_string()).collect();
    assert!(spar::split::apply_slice(&repo, &dir, "theirs", &slice).unwrap());
    git(&dir, &["commit", "-m", "part one"]);

    assert_eq!(
        "one, changed\n",
        std::fs::read_to_string(dir.join("a.rs")).unwrap()
    );
    assert!(
        !dir.join("c.rs").exists(),
        "the deletion did not carry over"
    );
    assert_eq!(
        "two\n",
        std::fs::read_to_string(dir.join("b.rs")).unwrap(),
        "a file no part named was changed anyway"
    );
    assert!(
        !dir.join("new.rs").exists(),
        "a file no part named was added"
    );
}

/// A slice that changes nothing is a part with no content, and committing it
/// would fail on an empty tree.
#[test]
fn a_slice_that_changes_nothing_says_so_rather_than_committing() {
    let fx = repo("split-empty");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    git(&fx.work, &["branch", "theirs"]);

    let (dir, _) = repo.worktree_for_split(12, 1, "main").unwrap();
    let slice = vec!["README.md".to_string()];
    assert!(!spar::split::apply_slice(&repo, &dir, "theirs", &slice).unwrap());
}

/// **spar never rewrites the branch behind somebody's pull request.** Splitting
/// a pull request touches code somebody else wrote, and the only thing that
/// makes it safe is that it is purely additive. This is that invariant against
/// a real repository: the base, the pull request's own branch, and an unrelated
/// branch all stay exactly where they were.
#[test]
fn splitting_never_moves_the_branch_behind_the_pull_request() {
    let fx = repo("split-additive");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    // The pull request's branch, with work on it that is not on main.
    git(&fx.work, &["checkout", "-b", "their-feature"]);
    commit(&fx.work, "theirs.rs", "their work\n", "their commit");
    git(&fx.work, &["push", "-u", "origin", "their-feature"]);
    git(&fx.work, &["checkout", "main"]);
    let before = git(&fx.work, &["rev-parse", "their-feature"]);
    let main_before = git(&fx.work, &["rev-parse", "main"]);

    // A part is built and committed on its own branch off the base.
    let (dir, branch) = repo.worktree_for_split(12, 1, "main").unwrap();
    std::fs::write(dir.join("part.rs"), "the slice\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-m", "part one"]);

    assert_eq!(before, git(&fx.work, &["rev-parse", "their-feature"]));
    assert_eq!(main_before, git(&fx.work, &["rev-parse", "main"]));
    assert_ne!(before, git(&fx.work, &["rev-parse", &branch]));

    // And the push path refuses any name but the one it created.
    assert!(spar::split::additive(&branch, "their-feature", "").is_ok());
    for other in ["their-feature", "main", "pr-12", "issue-12"] {
        assert!(
            spar::split::additive(other, "their-feature", "").is_err(),
            "{other} was allowed"
        );
    }
}

// ---------------------------------------------------------------------------
// Local state and follow-ups
// ---------------------------------------------------------------------------

#[test]
fn a_local_followup_is_written_once_and_only_once() {
    let fx = repo("followup");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    // The caller stamps provenance into the body, exactly as file_followup does.
    let body = |n: i64| format!("why it matters\n\nFound while working on #{n}.");

    assert!(matches!(
        repo.append_local_followup("Retry is unbounded", &body(42)),
        Followup::Recorded(_)
    ));
    assert!(
        matches!(
            repo.append_local_followup("Retry is unbounded", &body(43)),
            Followup::Covered(_)
        ),
        "a repeat across rounds must not file twice, and is covered rather than failed"
    );
    assert!(matches!(
        repo.append_local_followup("A different thing", &body(44)),
        Followup::Recorded(_)
    ));

    let notes = std::fs::read_to_string(fx.work.join(".spar").join("followups.md")).unwrap();
    assert_eq!(1, notes.matches("## Retry is unbounded").count(), "{notes}");
    assert!(notes.contains("## A different thing"), "{notes}");
    // Once, in one wording. It used to appear twice: "From #42." added here and
    // "Found while working on #42." already in the body the caller passed in.
    assert_eq!(
        1,
        notes.matches("#42").count(),
        "provenance is written once: {notes}"
    );
    assert!(!notes.contains("From #42."), "{notes}");
}

/// The writer and the reader live in different modules, and only this crosses
/// them. A marker or a heading shape that changed on one side and not the other
/// would file every section of every follow-up as its own issue.
#[test]
fn a_followup_file_written_by_spar_is_read_back_by_the_parser() {
    let fx = repo("followup-roundtrip");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    // Bodies in the shape `issue_report` produces: sections at the same
    // heading level as the entry title, then the provenance line.
    let body = |n: i64| {
        format!(
            "## Problem\n\nThe guard is inverted.\n\n## Impact\n\nCallers see a stale              value.\n\nFound while working on #{n}."
        )
    };
    for (title, number) in [
        ("Retry is unbounded", 42),
        ("Headers are restored only for the initiating instance", 43),
        ("A stale verdict overwrites a newer one", 44),
    ] {
        assert!(matches!(
            repo.append_local_followup(title, &body(number)),
            Followup::Recorded(_)
        ));
    }

    let text = std::fs::read_to_string(repo.followups_path()).unwrap();
    let entries = spar::followups::parse(&text);
    assert_eq!(
        3,
        entries.len(),
        "{:?}",
        entries.iter().map(|e| &e.title).collect::<Vec<_>>()
    );
    assert_eq!("Retry is unbounded", entries[0].title);
    assert!(
        entries[0].body.contains("## Problem"),
        "{}",
        entries[0].body
    );
    assert!(entries[2].body.ends_with("Found while working on #44."));
}

/// The queue is rewritten in place, so what survives a removal has to be
/// readable by the same parser that produced the spans.
#[test]
fn a_rewritten_followup_file_still_reads_back() {
    let fx = repo("followup-rewrite");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    for title in ["One", "Two", "Three"] {
        repo.append_local_followup(title, "why it matters\n\nFound while working on #7.");
    }

    let path = repo.followups_path();
    let text = std::fs::read_to_string(&path).unwrap();
    let entries = spar::followups::parse(&text);
    let left = spar::followups::without(&text, &[entries[1].clone()]);
    spar::repo::write_text_atomic(&path, &left).unwrap();

    let back = spar::followups::parse(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(2, back.len());
    assert_eq!("One", back[0].title);
    assert_eq!("Three", back[1].title);
    assert!(
        !path.with_extension("md.tmp").exists(),
        "the temp file was left behind"
    );
}

/// `spar followup` removes an entry once it has filed it. Without the archive,
/// the dedup in `append_local_followup` would have nothing left to match and
/// the next run that rediscovered the same defect would record it again, on top
/// of the issue that now exists for it.
#[test]
fn a_followup_already_dealt_with_is_not_recorded_again() {
    let fx = repo("followup-archive");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let body = "why it matters\n\nFound while working on #7.";

    assert!(matches!(
        repo.append_local_followup("Retry is unbounded", body),
        Followup::Recorded(_)
    ));
    let text = std::fs::read_to_string(repo.followups_path()).unwrap();
    let entries = spar::followups::parse(&text);
    repo.archive_followup(&entries[0].title, &entries[0].body, "Filed: #512");
    spar::repo::write_text_atomic(
        &repo.followups_path(),
        &spar::followups::without(&text, &entries),
    )
    .unwrap();

    assert!(
        matches!(
            repo.append_local_followup("Retry is unbounded", body),
            Followup::Covered(_)
        ),
        "it was already filed, and recording it again puts it back in the queue forever"
    );
    assert_eq!(
        "",
        std::fs::read_to_string(repo.followups_path()).unwrap(),
        "the queue is drained and must stay drained"
    );
}

/// A follow-up that could not be written has to say so. The caller settles the
/// point on this answer and the ledger outlives the run, so a write failure
/// reported as success suppresses a real defect on every later round too.
#[test]
fn a_local_note_that_cannot_be_written_reports_failure() {
    let fx = repo("followup-write-fails");
    let mut cfg = cfg();
    cfg.loop_cfg.followups = Followups::Local;
    let repo = Repo::open(&fx.work, &cfg).unwrap();
    let state = IssueRun::new(7, "t");

    // A directory where the queue file goes: the append cannot open it.
    std::fs::create_dir_all(repo.followups_path()).unwrap();

    assert_eq!(
        Followup::Failed,
        repo.append_local_followup("Retry is unbounded", "why it matters")
    );
    assert_eq!(
        Followup::Failed,
        file_followup(
            &repo,
            "Retry is unbounded",
            "why it matters",
            7,
            &cfg,
            &state
        )
    );
}

/// `origin` here is a bare repository on disk, so there is no tracker to reach.
/// The point is not filed and nothing covers it, which is a retry rather than a
/// settled point.
#[test]
fn a_tracker_that_cannot_be_reached_reports_failure() {
    let fx = repo("followup-tracker-fails");
    let mut cfg = cfg();
    cfg.loop_cfg.followups = Followups::Issues;
    let repo = Repo::open(&fx.work, &cfg).unwrap();
    let state = IssueRun::new(7, "t");

    assert_eq!(
        Followup::Failed,
        file_followup(
            &repo,
            "Retry is unbounded",
            "why it matters",
            7,
            &cfg,
            &state
        )
    );
    assert!(!repo.followups_path().exists(), "nothing was written");
}

/// Turning follow-ups off is a decision, not a failure. Retrying it every round
/// would spend the budget on a write that will never happen.
#[test]
fn follow_ups_turned_off_are_dropped_rather_than_failed() {
    let fx = repo("followup-off");
    let mut cfg = cfg();
    cfg.loop_cfg.followups = Followups::None;
    let repo = Repo::open(&fx.work, &cfg).unwrap();
    let state = IssueRun::new(7, "t");

    let outcome = file_followup(&repo, "Retry is unbounded", "why", 7, &cfg, &state);
    assert!(matches!(outcome, Followup::Dropped(_)), "{outcome:?}");
    assert_eq!(None, outcome.url());
    assert!(!repo.followups_path().exists());
}

/// The cap is the backstop against a run that will not stop finding things.
/// Reaching it drops the point deliberately, and says which.
#[test]
fn the_followup_cap_drops_rather_than_fails() {
    let fx = repo("followup-cap");
    let mut cfg = cfg();
    cfg.loop_cfg.followups = Followups::Local;
    cfg.loop_cfg.max_followups = 2;
    let repo = Repo::open(&fx.work, &cfg).unwrap();
    let mut state = IssueRun::new(7, "t");
    state.filed = vec!["note: one".into(), "note: two".into()];

    let outcome = file_followup(&repo, "Retry is unbounded", "why", 7, &cfg, &state);
    assert!(matches!(outcome, Followup::Dropped(_)), "{outcome:?}");
    assert!(!repo.followups_path().exists());
}

#[test]
fn state_round_trips_through_the_local_store() {
    use spar::model::{Ledger, LedgerEntry, PersistedState, Status};

    let fx = repo("state");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    let mut ledger = Ledger::new();
    ledger.insert(
        "abc123".into(),
        LedgerEntry {
            title: "Unbounded loop".into(),
            file: "src/x.rs".into(),
            reasoning: "the caller already bounds it".into(),
            round: 1,
            reraised: 0,
            outcome: Default::default(),
        },
    );
    let state = PersistedState {
        version: 1,
        round: 2,
        next_actor: "b".into(),
        status: Status::Pending,
        ledger,
        filed: vec!["https://example.invalid/1".into()],
    };
    repo.write_state(7, &state).unwrap();

    let text = std::fs::read_to_string(repo.state_path(7)).unwrap();
    let back: PersistedState = serde_json::from_str(&text).unwrap();
    assert_eq!(2, back.round);
    assert_eq!("b", back.next_actor);
    assert!(back.ledger.contains_key("abc123"));
    assert_eq!(
        "the caller already bounds it",
        back.ledger["abc123"].reasoning
    );

    repo.clear_state(7);
    assert!(!repo.state_path(7).exists());
}

#[test]
fn clearing_state_that_was_never_written_is_not_an_error() {
    let fx = repo("clearstate");
    Repo::open(&fx.work, &cfg()).unwrap().clear_state(1234);
}

// ---------------------------------------------------------------------------
// The binary itself
// ---------------------------------------------------------------------------

fn spar(args: &[&str], cwd: &Path) -> (bool, String, String) {
    let out = Command::new(SPAR_BIN)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// An empty queue is a local no-op, and it has to say which file it looked in.
/// Reaching gh to find that out would make the common case cost a round trip.
#[test]
fn followup_says_so_when_there_is_nothing_recorded() {
    let fx = repo("followup-empty");
    let config = fx.dir.join("spar.toml");
    std::fs::write(&config, TWO_AGENTS).unwrap();

    let (ok, _, err) = spar(
        &[
            "followup",
            "--config",
            config.to_str().unwrap(),
            "--repo",
            fx.work.to_str().unwrap(),
        ],
        &fx.dir,
    );
    assert!(ok, "an empty queue is not a failure: {err}");
    assert!(err.contains("no follow-ups recorded"), "{err}");
    assert!(err.contains("followups.md"), "{err}");
}

/// A file that is there and empty is a different thing from one that is not
/// there, and a file with no headings is a third: the last is a parser problem
/// and reporting it as an empty queue would hide it.
#[test]
fn followup_tells_an_empty_queue_from_one_it_could_not_read() {
    let fx = repo("followup-shapes");
    let config = fx.dir.join("spar.toml");
    std::fs::write(&config, TWO_AGENTS).unwrap();
    let notes = fx.work.join(".spar");
    std::fs::create_dir_all(&notes).unwrap();
    let path = notes.join("followups.md");

    let run = || {
        spar(
            &[
                "followup",
                "--config",
                config.to_str().unwrap(),
                "--repo",
                fx.work.to_str().unwrap(),
            ],
            &fx.dir,
        )
    };

    std::fs::write(&path, "\n  \n").unwrap();
    let (ok, _, err) = run();
    assert!(ok, "{err}");
    assert!(err.contains("there and empty"), "{err}");

    std::fs::write(&path, "just some prose nobody put a heading on\n").unwrap();
    let (ok, _, err) = run();
    assert!(ok, "{err}");
    assert!(err.contains("no `## ` headings"), "{err}");
    assert_eq!(
        "just some prose nobody put a heading on\n",
        std::fs::read_to_string(&path).unwrap(),
        "a file it could not read must not be rewritten"
    );
}

/// The watermark is what makes the "leave a thread spar disagreed with open"
/// decision terminate: without it, an unresolved thread reads as unanswered
/// forever and spar re-argues every point it lost, once per run.
#[test]
fn a_checkin_watermark_round_trips_and_prunes_with_the_rest() {
    use spar::model::Answered;

    let fx = repo("checkin-state");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    let mut seen = Answered {
        version: 1,
        ..Answered::default()
    };
    seen.seen
        .insert("thread:PRRT_kwABC".into(), "PRRC_kw9".into());
    seen.seen
        .insert("comment:5455795654".into(), "5455795654".into());

    let path = repo.checkin_state_path(108);
    spar::repo::write_json_atomic(&path, &seen).unwrap();

    let back: Answered = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        Some(&"PRRC_kw9".to_string()),
        back.seen.get("thread:PRRT_kwABC")
    );
    assert_eq!(2, back.seen.len());

    // It lives beside the resume state so housekeeping reaches it, and it is
    // named apart so `review::persist` cannot overwrite a map it knows nothing
    // about.
    assert_eq!(
        repo.state_path(108).parent(),
        path.parent(),
        "checkin state has to sit where prune_state looks"
    );
    assert_ne!(repo.state_path(108), path);
    assert!(!path.with_extension("json.tmp").exists());
}

#[test]
fn version_and_help_work_without_any_configuration() {
    let dir = unique("help");
    std::fs::create_dir_all(&dir).unwrap();
    let (ok, out, _) = spar(&["--version"], &dir);
    assert!(ok);
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");

    let (ok, out, _) = spar(&["--help"], &dir);
    assert!(ok);
    for word in [
        "run", "triage", "resume", "followup", "checkin", "init", "clean", "doctor",
    ] {
        assert!(out.contains(word), "{word} missing from help:\n{out}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_config_says_what_to_do_about_it() {
    let dir = unique("noconfig");
    std::fs::create_dir_all(&dir).unwrap();
    let (ok, _, err) = spar(&["run", "1", "--config", "nope.toml"], &dir);
    assert!(!ok);
    assert!(err.contains("nope.toml"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_writes_a_config_that_loads_back() {
    let dir = unique("init");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("spar.toml");

    let (_, stdout, _) = spar(&["init", "--out", out.to_str().unwrap()], &dir);
    if !out.exists() {
        // Fewer than two agent CLIs on this machine, which is a legitimate
        // outcome and has to be explained rather than crash.
        assert!(stdout.contains("missing"), "{stdout}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let cfg = config::load(Some(&out)).unwrap();
    assert_eq!(2, cfg.agents.len());
    assert!(
        !cfg.loop_cfg.auto_merge,
        "a generated config must not auto merge"
    );
    assert!(cfg.has_agent(&cfg.first_implementor));

    // A second run must refuse rather than clobber.
    let (ok, _, err) = spar(&["init", "--out", out.to_str().unwrap()], &dir);
    assert!(!ok);
    assert!(err.contains("already exists"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quiet_init_still_explains_why_it_refused() {
    let dir = unique("quietinit");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("spar.toml");
    std::fs::write(&out, "").unwrap();

    let (ok, _, err) = spar(&["--quiet", "init", "--out", out.to_str().unwrap()], &dir);
    assert!(!ok, "an existing file must not be overwritten");
    assert!(
        err.contains("already exists"),
        "--quiet must not swallow a refusal: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_reports_a_missing_agent_binary_without_crashing() {
    let dir = unique("doctor");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("spar.toml");
    std::fs::write(
        &config,
        "[agents.alpha]\ncommand = [\"definitely-not-installed-xyz\"]\n\
         [agents.beta]\ncommand = [\"also-not-installed-xyz\"]\n",
    )
    .unwrap();

    let (ok, out, _) = spar(&["doctor", "--config", config.to_str().unwrap()], &dir);
    assert!(!ok, "doctor must exit non-zero when something is missing");
    assert!(out.contains("FAIL  alpha"), "{out}");
    assert!(out.contains("FAIL  beta"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The correlation warning is the one thing doctor says that no other command
/// says, so an unrelated probe failure must not swallow it.
#[test]
fn doctor_warns_when_both_agents_are_the_same_binary_and_model() {
    let dir = unique("doctorcorr");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("spar.toml");
    std::fs::write(
        &config,
        "[agents.alpha]\ncommand = [\"/bin/echo\"]\nmodel = \"fable\"\n\
         [agents.beta]\ncommand = [\"/bin/echo\"]\nmodel = \"fable\"\n",
    )
    .unwrap();

    let (_, out, _) = spar(&["doctor", "--config", config.to_str().unwrap()], &dir);
    assert!(out.contains("WARNING"), "{out}");
    assert!(out.contains("alpha") && out.contains("beta"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_stays_quiet_when_the_two_agents_are_genuinely_different() {
    let dir = unique("doctorok");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("spar.toml");
    std::fs::write(
        &config,
        "[agents.alpha]\ncommand = [\"/bin/echo\"]\nmodel = \"fable\"\n\
         [agents.beta]\ncommand = [\"/bin/cat\"]\nmodel = \"fable\"\n",
    )
    .unwrap();

    let (_, out, _) = spar(&["doctor", "--config", config.to_str().unwrap()], &dir);
    assert!(!out.contains("WARNING"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clean_on_a_repository_with_nothing_to_clean_says_so() {
    let fx = repo("cleannothing");
    std::fs::write(fx.work.join("spar.toml"), TWO_AGENTS).unwrap();
    let (ok, out, _) = spar(&["clean", "--repo", fx.work.to_str().unwrap()], &fx.work);
    assert!(ok);
    assert!(out.contains("nothing to clean"), "{out}");
}

// ---------------------------------------------------------------------------
// Staying out of the way
// ---------------------------------------------------------------------------

/// spar's scratch directories live inside the target repo. Leaving them
/// untracked-but-visible makes `git status` noisy in somebody else's project,
/// and writing to a tracked .gitignore would be spar editing their repo.
#[test]
fn spar_excludes_its_own_scratch_directories_from_git_status() {
    let fx = repo("exclude");
    let _ = Repo::open(&fx.work, &cfg()).unwrap();

    let exclude =
        std::fs::read_to_string(fx.work.join(".git").join("info").join("exclude")).unwrap();
    assert!(exclude.contains("/.spar-worktrees/"), "{exclude}");
    assert!(exclude.contains("/.spar/"), "{exclude}");

    // And the repo really is clean once spar has written its state there.
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.append_local_followup("A note", "body");
    repo.record_branch("issue-1", "issue", 1);
    assert_eq!("", git(&fx.work, &["status", "--porcelain"]).trim());
}

#[test]
fn the_exclude_entries_are_written_once_not_on_every_open() {
    let fx = repo("excludeonce");
    for _ in 0..3 {
        let _ = Repo::open(&fx.work, &cfg()).unwrap();
    }
    let exclude =
        std::fs::read_to_string(fx.work.join(".git").join("info").join("exclude")).unwrap();
    assert_eq!(1, exclude.matches("/.spar-worktrees/").count(), "{exclude}");
    assert_eq!(1, exclude.matches("added by spar").count(), "{exclude}");
}

#[test]
fn a_users_own_exclude_entries_are_preserved() {
    let fx = repo("excludekeep");
    let path = fx.work.join(".git").join("info").join("exclude");
    std::fs::write(&path, "# mine\n/scratch/\n").unwrap();

    let _ = Repo::open(&fx.work, &cfg()).unwrap();

    let exclude = std::fs::read_to_string(&path).unwrap();
    assert!(exclude.contains("/scratch/"), "{exclude}");
    assert!(exclude.contains("/.spar/"), "{exclude}");
}

// ---------------------------------------------------------------------------
// The base ref every "did anything happen" check hangs off
// ---------------------------------------------------------------------------

#[test]
fn the_remote_tracking_branch_is_preferred_when_it_resolves() {
    let fx = repo("baseref");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert_eq!("origin/main", repo.base_ref(&fx.work, "main"));
}

/// Without the fallback, a checkout whose origin was never fetched reports
/// every implementation as "no commits" and throws the agent's work away.
#[test]
fn a_missing_remote_ref_falls_back_to_the_local_branch() {
    let fx = repo("baserefmissing");
    git(&fx.work, &["remote", "remove", "origin"]);
    git(&fx.work, &["update-ref", "-d", "refs/remotes/origin/main"]);
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert_eq!("main", repo.base_ref(&fx.work, "main"));
}

#[test]
fn work_on_a_branch_is_still_detected_with_no_remote_ref() {
    let fx = repo("nochangesnoremote");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    git(&fx.work, &["checkout", "-q", "-b", "feature"]);
    commit(
        &fx.work,
        "a.txt",
        "one\n",
        "Real work that must not be discarded",
    );
    git(&fx.work, &["update-ref", "-d", "refs/remotes/origin/main"]);

    assert!(
        repo.has_changes(&fx.work, "main"),
        "an unfetched origin must not read as an abandoned issue"
    );
    assert!(repo.diff_stat(&fx.work, "main").contains("1 file changed"));
}

#[test]
fn a_branch_with_no_commits_still_reports_no_changes() {
    let fx = repo("trulynochanges");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    git(&fx.work, &["checkout", "-q", "-b", "feature"]);
    assert!(!repo.has_changes(&fx.work, "main"));
}

// ---------------------------------------------------------------------------
// Not colonising ordinary directory names
// ---------------------------------------------------------------------------

/// `presets/` is a perfectly ordinary directory name in somebody else's
/// project. Searching it for agent presets made a stray `presets/claude.toml`
/// shadow the built in one, and `spar init` then reported Claude Code as
/// missing while it sat on PATH.
#[test]
fn an_unrelated_presets_directory_in_the_repo_is_ignored() {
    let dir = unique("shadow");
    std::fs::create_dir_all(dir.join("presets")).unwrap();
    std::fs::write(
        dir.join("presets").join("claude.toml"),
        "name = \"my sampler preset\"\ntemperature = 0.8\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("presets").join("codex.toml"),
        "name = \"another unrelated thing\"\n",
    )
    .unwrap();

    let (_, out, err) = spar(&["init", "--out", "spar.toml"], &dir);
    assert!(
        !out.contains("no command") && !err.contains("no command"),
        "the built in preset must win:\n{out}\n{err}"
    );
    // Whatever is installed on this machine, the shadowing file must not have
    // turned a resolvable CLI into a missing one.
    assert!(!err.contains("unknown preset"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The documented override location still works.
#[test]
fn a_preset_override_under_the_spar_directory_is_honoured() {
    let dir = unique("override");
    let presets = dir.join(".spar").join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    std::fs::write(
        presets.join("claude.toml"),
        "command = [\"/bin/echo\", \"{prompt}\"]\noutput = \"text\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("spar.toml"),
        "[agents.claude]\npreset = \"claude\"\n[agents.other]\ncommand = [\"/bin/cat\"]\n",
    )
    .unwrap();

    let (_, out, _) = spar(&["doctor", "--config", "spar.toml"], &dir);
    // Deliberately not the exit code. `doctor` also checks gh authentication,
    // which is an unrelated prerequisite that no CI runner has, and asserting
    // on it would make this a test about the environment rather than about
    // which binary the preset resolved to.
    assert!(
        out.contains("/bin/echo"),
        "the override was not used:\n{out}"
    );
    assert!(!out.contains("unknown preset"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Never rebuilding over work that already exists
// ---------------------------------------------------------------------------

/// The data loss case behind `spar run 42` on an issue that has already been
/// worked. Deleting the local branch and rebuilding from the base leaves the
/// remote tracking ref intact, so `--force-with-lease` is satisfied and the
/// push quietly replaces the previous round's commits.
#[test]
fn a_remote_branch_with_unaccounted_work_is_not_rebuilt() {
    let fx = repo("noclobber");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    // A previous session's work, pushed.
    git(&fx.work, &["checkout", "-q", "-b", "issue-42"]);
    commit(
        &fx.work,
        "feature.txt",
        "round one\n",
        "Implement the feature",
    );
    git(&fx.work, &["push", "-q", "-u", "origin", "issue-42"]);
    git(&fx.work, &["checkout", "-q", "main"]);
    let before = git(&fx.work, &["rev-parse", "origin/issue-42"]);

    let err = repo.worktree_add(42, "main").unwrap_err().to_string();

    assert!(err.contains("issue-42"), "{err}");
    assert!(
        err.contains("force push"),
        "the reason has to be in the message: {err}"
    );
    assert!(
        err.contains("spar resume"),
        "the remedy has to be in the message: {err}"
    );
    assert_eq!(
        before,
        git(&fx.work, &["rev-parse", "origin/issue-42"]),
        "the previous work must still be on origin"
    );
}

#[test]
fn a_branch_that_matches_the_base_is_not_treated_as_work() {
    let fx = repo("noclobber-equal");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    // A branch pushed with nothing on it beyond the base.
    git(&fx.work, &["push", "-q", "origin", "main:issue-43"]);

    let (path, branch) = repo
        .worktree_add(43, "main")
        .expect("an empty branch carries no work to lose");
    assert_eq!("issue-43", branch);
    assert!(path.is_dir());
    repo.worktree_remove(43);
}

/// The other half of the same case. An implement call that fails after the
/// agent has committed leaves the work unpushed, so there is no remote branch
/// for the guard above to find and nothing but the reflog if this one deletes
/// the local branch.
#[test]
fn a_local_branch_with_unpushed_work_is_not_rebuilt() {
    let fx = repo("noclobber-local");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    git(&fx.work, &["checkout", "-q", "-b", "issue-45"]);
    commit(
        &fx.work,
        "feature.txt",
        "round one\n",
        "Implement the feature",
    );
    let before = git(&fx.work, &["rev-parse", "issue-45"]);
    git(&fx.work, &["checkout", "-q", "main"]);

    let err = repo.worktree_add(45, "main").unwrap_err().to_string();

    assert!(
        err.contains("Implement the feature"),
        "the message has to say what is sitting there: {err}"
    );
    assert!(
        err.contains("git branch -D issue-45"),
        "the remedy has to be in the message: {err}"
    );
    assert_eq!(
        before,
        git(&fx.work, &["rev-parse", "issue-45"]),
        "the unpushed work must still be reachable"
    );
}

#[test]
fn a_local_branch_that_matches_the_base_is_not_treated_as_work() {
    let fx = repo("noclobber-local-equal");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    git(&fx.work, &["branch", "issue-46", "main"]);

    let (path, branch) = repo
        .worktree_add(46, "main")
        .expect("an empty branch carries no work to lose");
    assert_eq!("issue-46", branch);
    assert!(path.is_dir());
    repo.worktree_remove(46);
}

/// `git commit --allow-empty-message` is legal and leaves a commit with no
/// subject line, which is exactly what a listing of subjects drops. The guards
/// count commits instead, or a branch whose work happens to be unnamed reads as
/// an empty branch and gets rebuilt over.
#[test]
fn a_commit_with_no_message_is_still_work() {
    let fx = repo("noclobber-blank");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    git(&fx.work, &["checkout", "-q", "-b", "issue-47"]);
    std::fs::write(fx.work.join("feature.txt"), "round one\n").unwrap();
    git(&fx.work, &["add", "."]);
    git(
        &fx.work,
        &["commit", "-q", "--allow-empty-message", "-m", ""],
    );
    let before = git(&fx.work, &["rev-parse", "issue-47"]);
    git(&fx.work, &["checkout", "-q", "main"]);

    let err = repo.worktree_add(47, "main").unwrap_err().to_string();

    assert!(err.contains("1 commit(s)"), "{err}");
    assert_eq!(
        before,
        git(&fx.work, &["rev-parse", "issue-47"]),
        "the unpushed work must still be reachable"
    );
}

#[test]
fn a_pushed_commit_with_no_message_is_still_work() {
    let fx = repo("noclobber-blank-remote");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    git(&fx.work, &["checkout", "-q", "-b", "issue-48"]);
    std::fs::write(fx.work.join("feature.txt"), "round one\n").unwrap();
    git(&fx.work, &["add", "."]);
    git(
        &fx.work,
        &["commit", "-q", "--allow-empty-message", "-m", ""],
    );
    git(&fx.work, &["push", "-q", "-u", "origin", "issue-48"]);
    git(&fx.work, &["checkout", "-q", "main"]);
    let before = git(&fx.work, &["rev-parse", "origin/issue-48"]);

    let err = repo.worktree_add(48, "main").unwrap_err().to_string();

    assert!(err.contains("force push"), "{err}");
    assert_eq!(
        before,
        git(&fx.work, &["rev-parse", "origin/issue-48"]),
        "the previous work must still be on origin"
    );
}

/// What lets the local guard rebuild a branch: a pull request already holds its
/// commits. Reusing a branch name for a second round puts new commits under the
/// old number, and the head that pull request preserved holds the round it was
/// opened from and nothing after it.
#[test]
fn a_pull_request_head_holds_only_what_it_was_opened_from() {
    let fx = repo("pr-head-holds");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let pr_head = "refs/spar/test-pr-head";

    git(&fx.work, &["checkout", "-q", "-b", "issue-49"]);
    commit(
        &fx.work,
        "feature.txt",
        "round one\n",
        "First round, merged",
    );
    // What GitHub still serves at refs/pull/N/head after the branch is gone.
    git(&fx.work, &["update-ref", pr_head, "issue-49"]);

    assert!(
        repo.commits_held_by("issue-49", "main", pr_head),
        "the pull request holds the commits it was opened from"
    );

    commit(
        &fx.work,
        "feature.txt",
        "round two\n",
        "Second round, unpushed",
    );

    assert!(
        !repo.commits_held_by("issue-49", "main", pr_head),
        "a commit made after the pull request is not on its head"
    );
    assert!(
        !repo.commits_held_by("issue-49", "main", "refs/spar/does-not-exist"),
        "a ref that does not resolve cannot vouch for anything"
    );
}

#[test]
fn a_fresh_issue_is_unaffected_by_the_guard() {
    let fx = repo("noclobber-fresh");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let (path, branch) = repo.worktree_add(44, "main").unwrap();
    assert_eq!("issue-44", branch);
    assert!(path.join("README.md").is_file());
    repo.worktree_remove(44);
}

// ---------------------------------------------------------------------------
// Reviewing what cannot be pushed to
// ---------------------------------------------------------------------------

/// A pull request from a fork has no branch in the upstream repository, which
/// is why it cannot be resumed. GitHub still serves `refs/pull/N/head` for it,
/// and that is what makes reviewing an outside contribution possible at all.
/// Verified against real GitHub as well: for cli/cli PR 14252, the head branch
/// is absent from the upstream repo while refs/pull/14252/head resolves.
#[test]
fn a_pull_request_head_is_checked_out_without_any_branch() {
    let fx = repo("prhead");
    let origin = fx.dir.join("origin.git");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    // Somebody else's commit, published only as a pull request head.
    git(&fx.work, &["checkout", "-q", "-b", "contributor-work"]);
    commit(
        &fx.work,
        "contributed.txt",
        "their change\n",
        "Their contribution",
    );
    let head = git(&fx.work, &["rev-parse", "HEAD"]).trim().to_string();
    git(
        &fx.work,
        &["push", "-q", "origin", "HEAD:refs/pull/42/head"],
    );
    git(&fx.work, &["checkout", "-q", "main"]);
    git(&fx.work, &["branch", "-D", "-q", "contributor-work"]);

    // The branch is nowhere in the repository: only the pull request ref is.
    let branches = git(
        &fx.work,
        &["for-each-ref", "refs/heads/", "--format=%(refname)"],
    );
    assert!(!branches.contains("contributor-work"), "{branches}");
    let remote_refs = git(&fx.work, &["ls-remote", origin.to_str().unwrap()]);
    assert!(remote_refs.contains("refs/pull/42/head"), "{remote_refs}");

    let path = repo
        .worktree_for_pr_head(42)
        .expect("the head must be reachable");

    assert!(path.is_dir(), "{}", path.display());
    assert_eq!(
        "their change\n",
        std::fs::read_to_string(path.join("contributed.txt")).unwrap()
    );
    assert_eq!(head, git(&path, &["rev-parse", "HEAD"]).trim());

    // Detached on purpose: there is nothing here to push, so there should be
    // nothing that looks pushable.
    assert_eq!(
        "HEAD",
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "the review checkout must not be on a branch"
    );
    let after = git(
        &fx.work,
        &["for-each-ref", "refs/heads/", "--format=%(refname)"],
    );
    assert!(
        !after.contains("pr-42"),
        "no branch should have been created: {after}"
    );

    repo.release_review_worktree(42);
    assert!(!path.is_dir(), "the worktree should be gone");
    let refs = git(
        &fx.work,
        &["for-each-ref", "refs/spar/", "--format=%(refname)"],
    );
    assert_eq!("", refs.trim(), "the parked ref should be gone too: {refs}");
}

#[test]
fn a_missing_pull_request_head_says_what_went_wrong() {
    let fx = repo("prheadmissing");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    let err = repo.worktree_for_pr_head(999).unwrap_err().to_string();
    assert!(err.contains("999"), "{err}");
    assert!(
        err.contains("refs/pull"),
        "the message should explain the mechanism: {err}"
    );
}

#[test]
fn review_worktrees_are_swept_by_clean_all() {
    let fx = repo("prheadclean");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    git(&fx.work, &["push", "-q", "origin", "HEAD:refs/pull/7/head"]);
    let path = repo.worktree_for_pr_head(7).unwrap();
    assert!(path.is_dir());

    let removed = repo.prune_worktrees(true);

    assert!(removed.iter().any(|r| r == "review-7"), "{removed:?}");
    assert!(!path.is_dir());
}

// ---------------------------------------------------------------------------
// The documentation is part of the product
// ---------------------------------------------------------------------------

const EXAMPLE_CONFIG: &str = include_str!("../spar.example.toml");

/// spar.example.toml is the file people copy. If it stops parsing, the first
/// thing a new user does fails, and nothing else in the suite would notice.
#[test]
fn the_example_config_parses() {
    let cfg = spar::config::parse(EXAMPLE_CONFIG)
        .unwrap_or_else(|e| panic!("spar.example.toml does not parse: {e}"));
    assert_eq!(2, cfg.agents.len());
    assert!(cfg.has_agent(&cfg.first_implementor));
}

/// It also has to keep saying what the code actually does. `deny_unknown_fields`
/// catches a key the example has and the code dropped; this catches the other
/// direction, a key the code gained and nobody documented. Both matter: an
/// undocumented option is one an error message can tell you to set and you
/// cannot find.
#[test]
fn every_config_option_is_documented_in_the_example() {
    fn keys_of<T: serde::Serialize>(value: &T) -> Vec<String> {
        let text = toml::to_string(value).expect("serializable");
        text.lines()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k.trim().to_string()))
            .filter(|k| !k.is_empty())
            .collect()
    }

    let mut expected: Vec<String> = Vec::new();
    expected.extend(keys_of(&spar::config::LoopCfg::default()));
    expected.extend(keys_of(&spar::config::StyleCfg::default()));
    expected.extend(keys_of(&spar::config::EffortSchedule {
        round_1: Some("high".into()),
        rest: Some("low".into()),
    }));
    // Per-agent options, which a preset normally supplies but a user may set.
    expected.extend(
        [
            "model",
            "effort",
            "output",
            "timeout",
            "search_paths",
            "system_via",
            "message_path",
            "message_match",
            "command",
            "preset",
            "models",
            "efforts",
            "options_note",
        ]
        .map(String::from),
    );

    // Documented means findable by somebody reading the file. A commented out
    // option counts, since the example shows options that are off by default
    // without turning them on, and a sub-table counts as documenting its name.
    let documented = |key: &str| -> bool {
        EXAMPLE_CONFIG.lines().any(|line| {
            let bare = line.trim_start().trim_start_matches('#').trim_start();
            bare.starts_with(&format!("{key} "))
                || bare.starts_with(&format!("{key}="))
                || bare.starts_with(&format!("[{key}]"))
                || bare.contains(&format!(".{key}]"))
        })
    };
    let missing: Vec<&String> = expected.iter().filter(|key| !documented(key)).collect();

    assert!(
        missing.is_empty(),
        "spar.example.toml does not mention: {missing:?}\n\
         Every option the parser accepts has to be findable by somebody reading \
         the example config."
    );
}

/// `spar init` writes a config too, and it has to parse for the same reason.
#[test]
fn the_generated_config_is_not_stale_against_the_example() {
    // Both must at least agree on the options they do mention.
    for key in ["max_rounds", "auto_merge", "first_implementor", "worktrees"] {
        assert!(
            EXAMPLE_CONFIG.contains(key),
            "{key} missing from spar.example.toml"
        );
    }
}

/// `spar init` writes the file most people will ever read. It has to name the
/// values that go in `model` and `effort`, because "..." is exactly the
/// guesswork the command exists to remove.
#[test]
fn the_generated_config_names_the_options_rather_than_guessing() {
    let dir = unique("inithints");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("spar.toml");
    let (_, stdout, _) = spar(&["init", "--out", out.to_str().unwrap()], &dir);

    assert!(
        !stdout.contains("BROKEN"),
        "a preset failed to build:\n{stdout}"
    );
    if !out.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // fewer than two agent CLIs here, covered by another test
    }
    let text = std::fs::read_to_string(&out).unwrap();

    assert!(!text.contains("\"...\""), "no placeholder values:\n{text}");
    for key in [
        "keep_worktrees",
        "parallel_triage",
        "file_nits",
        "branch_prefix",
        "state_store",
        "max_title_chars",
        "max_detail_chars",
    ] {
        assert!(
            text.contains(key),
            "{key} is not offered in the generated config:\n{text}"
        );
    }
    // And it still has to load.
    let cfg = spar::config::load(Some(&out)).unwrap();
    assert_eq!(2, cfg.agents.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every built in preset has to survive being turned into an agent. A preset
/// that does not is reported as BROKEN rather than silently skipped, because
/// skipping it made a malformed preset look like an uninstalled CLI.
#[test]
fn no_builtin_preset_is_broken() {
    for name in spar::config::available_presets() {
        let raw = spar::config::load_preset(&name)
            .unwrap_or_else(|e| panic!("preset {name} does not load: {e}"));
        let table = raw.as_table().cloned().expect("a table");
        let spec: Result<spar::config::AgentSpec, _> = toml::Value::Table(table).try_into();
        spec.unwrap_or_else(|e| panic!("preset {name} does not build: {e}"));
    }
}

/// The hint lists are advisory. Validating against them would refuse a model
/// that works the moment a CLI adds one.
#[test]
fn a_model_outside_the_hint_list_is_still_accepted() {
    let text = "[agents.a]\npreset = \"claude\"\nmodel = \"some-model-nobody-listed\"\n\
                effort = \"invented-effort\"\n[agents.b]\ncommand = [\"x\"]\n";
    let cfg = spar::config::parse(text).expect("hints must not be a whitelist");
    assert_eq!(
        Some("some-model-nobody-listed"),
        cfg.spec("a").unwrap().model.as_deref()
    );
}

// ---------------------------------------------------------------------------
// Upgrading a config that already exists
// ---------------------------------------------------------------------------

/// `spar init` will not touch an existing config, so without this a release
/// that adds a setting is invisible to anyone already using spar.
#[test]
fn init_update_appends_new_settings_without_touching_the_old_ones() {
    let dir = unique("initupdate");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("spar.toml");
    let original = "# my own comment, kept\n[agents.a]\ncommand = [\"/bin/echo\"]\n\
                    [agents.b]\ncommand = [\"/bin/cat\"]\n\n[loop]\nmax_rounds = 7\n";
    std::fs::write(&out, original).unwrap();

    let (ok, stdout, _) = spar(&["init", "--out", out.to_str().unwrap(), "--update"], &dir);
    assert!(ok, "{stdout}");

    let after = std::fs::read_to_string(&out).unwrap();
    assert!(
        after.starts_with(original),
        "existing content was rewritten:\n{after}"
    );
    assert!(after.contains("# my own comment, kept"));
    assert!(
        after.contains("max_rounds = 7"),
        "a set value must survive untouched"
    );
    // The appended settings are commented, so nothing takes effect by surprise.
    assert!(after.contains("# followups ="), "{after}");
    assert!(after.contains("# pr_comments ="), "{after}");

    // Still parses, and the value the user set still wins.
    let cfg = spar::config::load(Some(&out)).unwrap();
    assert_eq!(7, cfg.loop_cfg.max_rounds);

    // Idempotent: a second run has nothing to add.
    let (ok, stdout, _) = spar(&["init", "--out", out.to_str().unwrap(), "--update"], &dir);
    assert!(ok);
    assert!(
        stdout.contains("already mentions every setting"),
        "{stdout}"
    );
    assert_eq!(
        after,
        std::fs::read_to_string(&out).unwrap(),
        "second run changed the file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_update_refuses_to_lengthen_a_broken_config() {
    let dir = unique("initupdatebroken");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("spar.toml");
    std::fs::write(&out, "[agents.a]\nthis is not toml\n").unwrap();
    let before = std::fs::read_to_string(&out).unwrap();

    let (ok, _, err) = spar(&["init", "--out", out.to_str().unwrap(), "--update"], &dir);
    assert!(!ok);
    assert!(err.contains("does not parse"), "{err}");
    assert_eq!(
        before,
        std::fs::read_to_string(&out).unwrap(),
        "it wrote anyway"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_names_the_settings_a_config_has_never_heard_of() {
    let dir = unique("doctorunset");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("spar.toml");
    std::fs::write(
        &out,
        "[agents.a]\ncommand = [\"/bin/echo\"]\n[agents.b]\ncommand = [\"/bin/cat\"]\n",
    )
    .unwrap();

    let (_, stdout, _) = spar(&["doctor", "--config", out.to_str().unwrap()], &dir);
    assert!(stdout.contains("does not mention"), "{stdout}");
    assert!(stdout.contains("pr_comments"), "{stdout}");
    assert!(
        stdout.contains("init --update"),
        "it should say how to fix it:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The example config is the one file that should already mention everything.
#[test]
fn the_example_config_mentions_every_setting() {
    let unset = spar::config::unmentioned_options(EXAMPLE_CONFIG);
    let names: Vec<String> = unset.iter().map(|o| o.key.clone()).collect();
    assert!(
        names.is_empty(),
        "spar.example.toml never mentions: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Agreeing with a dry run without paying for it twice
// ---------------------------------------------------------------------------

#[test]
fn a_saved_review_round_trips_through_the_repo() {
    let fx = repo("pending");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    assert!(
        repo.read_pending_comment(366).is_none(),
        "nothing saved yet"
    );

    let review = "Two independent reviews.\n\nneeds changing before merge\n- Something real.";
    let path = repo.save_pending_comment(366, review).unwrap();
    assert!(path.ends_with("pr-366.md"), "{}", path.display());
    assert_eq!(Some(review.to_string()), repo.read_pending_comment(366));

    // It lives under .spar, which spar keeps out of the target repo's status.
    assert_eq!("", git(&fx.work, &["status", "--porcelain"]).trim());
}

#[test]
fn post_says_so_when_there_is_nothing_saved() {
    let fx = repo("postmissing");
    std::fs::write(fx.work.join("spar.toml"), TWO_AGENTS).unwrap();
    let (ok, _, err) = spar(
        &[
            "post",
            "366",
            "--repo",
            fx.work.to_str().unwrap(),
            "--config",
            "spar.toml",
        ],
        &fx.work,
    );
    assert!(!ok);
    assert!(err.contains("no saved review"), "{err}");
    assert!(
        err.contains("--dry-run"),
        "it should say how to produce one: {err}"
    );
}

/// The point of the feature: read it, then post exactly what you read, without
/// running the agents again.
#[test]
fn post_dry_run_prints_the_saved_review_without_touching_github() {
    let fx = repo("postdry");
    std::fs::write(fx.work.join("spar.toml"), TWO_AGENTS).unwrap();
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    repo.save_pending_comment(366, "needs changing before merge\n- A real defect.")
        .unwrap();

    let (ok, out, _) = spar(
        &[
            "post",
            "366",
            "--repo",
            fx.work.to_str().unwrap(),
            "--config",
            "spar.toml",
            "--dry-run",
        ],
        &fx.work,
    );
    assert!(ok, "{out}");
    assert!(out.contains("A real defect."), "{out}");
}

/// An edited file is still spar's output, so it goes through the same gate.
#[test]
fn posting_a_file_that_breaks_the_style_rules_is_refused() {
    let fx = repo("poststyle");
    std::fs::write(fx.work.join("spar.toml"), TWO_AGENTS).unwrap();
    let edited = fx.work.join("edited.md");
    std::fs::write(&edited, "I rewrote this \u{2014} with an em dash.").unwrap();

    let (_, _, err) = spar(
        &[
            "post",
            "366",
            "--repo",
            fx.work.to_str().unwrap(),
            "--config",
            "spar.toml",
            "--file",
            edited.to_str().unwrap(),
        ],
        &fx.work,
    );
    // gh will fail here for lack of a GitHub remote, but the em dash must not
    // be what reaches it: the scrub runs first and turns it into a comma.
    assert!(
        !err.contains('\u{2014}'),
        "an em dash reached the API call: {err}"
    );
}

#[test]
fn post_refuses_a_file_for_several_pull_requests() {
    let fx = repo("postmany");
    std::fs::write(fx.work.join("spar.toml"), TWO_AGENTS).unwrap();
    let (ok, _, err) = spar(
        &[
            "post",
            "1",
            "2",
            "--repo",
            fx.work.to_str().unwrap(),
            "--config",
            "spar.toml",
            "--file",
            "x.md",
        ],
        &fx.work,
    );
    assert!(!ok);
    assert!(err.contains("one pull request"), "{err}");
}

// ---------------------------------------------------------------------------
// Work whose author never got to describe it
// ---------------------------------------------------------------------------

/// An implement call that fails on its structured answer leaves commits and no
/// description of them. The commit messages are the only account there is, so
/// the pull request body is written from those rather than the branch being
/// left with nothing pointing at it.
#[test]
fn a_pr_body_is_assembled_from_the_commits_when_the_report_never_came() {
    let fx = repo("frombranch");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    commit(&fx.work, "parser.rs", "one\n", "Add the parser");
    commit(&fx.work, "empty.rs", "two\n", "Cover the empty input case");

    let work = spar::review::from_commits(&repo, &fx.work, "main");
    assert!(
        !work.not_worth_doing,
        "there is work, so it was not declined"
    );
    assert_eq!(
        vec!["Add the parser", "Cover the empty input case"],
        work.changes,
        "oldest first, as the branch reads"
    );

    let body = spar::review::pr_body(42, &work, &repo.style);
    assert!(body.contains("Closes #42"), "{body}");
    assert!(body.contains("- Add the parser"), "{body}");
    assert!(body.contains("- Cover the empty input case"), "{body}");
    assert!(
        body.contains("failed after these commits were made"),
        "a reviewer has to know this body is not the author's own: {body}"
    );
}

#[test]
fn a_branch_with_no_commits_of_its_own_lists_nothing() {
    let fx = repo("frombranch-empty");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();
    assert!(spar::review::from_commits(&repo, &fx.work, "main")
        .changes
        .is_empty());
}
