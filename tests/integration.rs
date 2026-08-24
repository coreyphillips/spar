//! End to end against a real git repository.
//!
//! Everything here runs on git alone: no network, no gh, no model. These cover
//! the mechanisms that unit tests cannot reach, in particular the commit
//! message rewrite, which shells out to `git filter-branch` and calls back into
//! this same binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use spar::config::{self, Config};
use spar::repo::Repo;

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
// Local state and follow-ups
// ---------------------------------------------------------------------------

#[test]
fn a_local_followup_is_written_once_and_only_once() {
    let fx = repo("followup");
    let repo = Repo::open(&fx.work, &cfg()).unwrap();

    assert!(repo
        .append_local_followup("Retry is unbounded", "body", 42)
        .is_some());
    assert!(
        repo.append_local_followup("Retry is unbounded", "body", 43)
            .is_none(),
        "a repeat across rounds must not file twice"
    );
    assert!(repo
        .append_local_followup("A different thing", "body", 44)
        .is_some());

    let notes = std::fs::read_to_string(fx.work.join(".spar").join("followups.md")).unwrap();
    assert_eq!(1, notes.matches("## Retry is unbounded").count(), "{notes}");
    assert!(notes.contains("## A different thing"), "{notes}");
    assert!(notes.contains("From #42"), "{notes}");
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

#[test]
fn version_and_help_work_without_any_configuration() {
    let dir = unique("help");
    std::fs::create_dir_all(&dir).unwrap();
    let (ok, out, _) = spar(&["--version"], &dir);
    assert!(ok);
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");

    let (ok, out, _) = spar(&["--help"], &dir);
    assert!(ok);
    for word in ["run", "triage", "resume", "init", "clean", "doctor"] {
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
    repo.append_local_followup("A note", "body", 1);
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
