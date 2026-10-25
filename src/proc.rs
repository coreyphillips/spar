//! Running external commands.
//!
//! Two things here are load bearing. First, both output streams are captured on
//! their own threads: an agent CLI can emit megabytes of JSONL, and reading one
//! pipe to completion before the other deadlocks as soon as the unread pipe
//! fills. Second, a failure message shows *both* streams, because one agent CLI
//! reports fatal conditions on stdout with stderr empty, and another writes
//! routine chatter to stderr on every run.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Result, SparError};

/// How long one agent call may run before it is killed.
///
/// An hour, because thirty minutes was not enough: a review at the highest
/// effort setting on a large repository, running the test suite as it went,
/// ran past it. A timeout costs the whole call, so the default errs long and
/// `timeout` on an agent shortens it.
pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// Thirty minutes was not enough for a deep review of a large repository, and a
/// timeout costs the whole call. Checked at compile time rather than in a test,
/// since shortening it is a decision worth stopping the build for.
const _: () = assert!(DEFAULT_TIMEOUT_SECS >= 3600);

#[derive(Debug, Clone)]
pub struct ExecOpts {
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
    /// When false, a non-zero exit returns stdout instead of an error.
    pub check: bool,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    /// On Unix, put the command in its own process group, and stop every
    /// descendant when the direct child exits, times out, or is interrupted.
    /// That includes descendants that moved to a group or session of their
    /// own, which is where agent CLIs run their shells. Agent calls use this
    /// to establish a quiet point before their working tree is inspected or
    /// committed.
    pub stop_descendants: bool,
}

impl Default for ExecOpts {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            check: true,
            env: Vec::new(),
            stdin: None,
            stop_descendants: false,
        }
    }
}

impl ExecOpts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cwd(mut self, path: impl AsRef<Path>) -> Self {
        self.cwd = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn cwd_opt(mut self, path: Option<PathBuf>) -> Self {
        self.cwd = path;
        self
    }

    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    pub fn check(mut self, value: bool) -> Self {
        self.check = value;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn stdin(mut self, text: impl Into<String>) -> Self {
        self.stdin = Some(text.into());
        self
    }

    pub fn stop_descendants(mut self, value: bool) -> Self {
        self.stop_descendants = value;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: String,
    /// Exact stdout bytes for commands whose output carries filesystem paths.
    pub stdout_bytes: Vec<u8>,
    pub stderr: String,
    pub code: i32,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Run a command to completion, capturing both streams.
///
/// Only spawn failures and timeouts are errors here; a non-zero exit is a
/// normal result that the caller decides how to treat.
pub fn exec(argv: &[String], opts: &ExecOpts) -> Result<Output> {
    let input = opts.stdin.as_deref().map(str::as_bytes);
    exec_with_input(argv, opts, input)
}

/// Set once the process has been told to stop, by Ctrl-C or SIGTERM, when
/// `stop_on_signal` has been installed. Every command that is running checks it
/// and stops its child, and the idle sleep in `spar listen` checks it too.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Whether the process has been told to stop.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

#[cfg(unix)]
extern "C" fn note_signal(sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
    // Only calls that are safe inside a signal handler belong here. Restoring
    // the default disposition means a second signal ends the process at once,
    // for somebody who does not want to wait for the current call to stop.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
    }
}

/// Turn SIGINT and SIGTERM into a flag that running commands and idle sleeps
/// check, instead of the default action that ends the process where it stands.
///
/// Installed by `spar listen` alone. Every other command keeps the default, so
/// nothing about them changes. The default would leave an agent CLI running:
/// agent calls are started in their own process group, which is what keeps a
/// terminal's Ctrl-C from reaching them.
#[cfg(unix)]
pub fn stop_on_signal() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a zeroed sigaction is a valid starting point on every platform
        // libc supports, and the handler does nothing a signal handler may not.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = note_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // Reads on the output pipes restart rather than fail.
        action.sa_flags = libc::SA_RESTART;
        let failed = unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut()) != 0
        };
        if failed {
            return Err(SparError::new(format!(
                "could not watch for Ctrl-C: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn stop_on_signal() -> Result<()> {
    Ok(())
}

/// Sleep for `total`, waking early if the process is told to stop. True when
/// it woke early.
pub fn sleep_unless_interrupted(total: Duration) -> bool {
    sleep_until(total, &INTERRUPTED)
}

fn sleep_until(total: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_secs(1)));
    }
    true
}

fn exec_with_input(argv: &[String], opts: &ExecOpts, input: Option<&[u8]>) -> Result<Output> {
    exec_until(argv, opts, input, &INTERRUPTED)
}

/// `exec_with_input`, stopping the child as soon as `stop` is set. The flag is
/// a parameter so the kill path can be tested without touching the process
/// wide one that every other test's commands check.
fn exec_until(
    argv: &[String],
    opts: &ExecOpts,
    input: Option<&[u8]>,
    stop: &AtomicBool,
) -> Result<Output> {
    if stop.load(Ordering::SeqCst) {
        return Err(SparError::interrupted(format!(
            "interrupted before `{}` started",
            abbreviate(argv)
        )));
    }
    let program = argv
        .first()
        .ok_or_else(|| SparError::new("cannot run an empty command"))?;

    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        // Agent CLIs happily block forever waiting on an inherited terminal.
        command.stdin(Stdio::null());
    }

    if let Some(dir) = &opts.cwd {
        command.current_dir(dir);
    }
    for (key, value) in &opts.env {
        command.env(key, value);
    }

    #[cfg(unix)]
    let token = opts.stop_descendants.then(call_token);
    #[cfg(unix)]
    if let Some(token) = &token {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let outer = std::env::var(CALLS_VAR).ok();
        command.env(CALLS_VAR, calls_value(outer.as_deref(), token));
    }

    let mut child = command
        .spawn()
        .map_err(|e| SparError::new(format!("could not run `{}`: {e}", abbreviate(argv))))?;

    #[cfg(unix)]
    let mut tree = token.map(|token| Descendants::new(child.id(), token));
    #[cfg(not(unix))]
    let tree: Option<Descendants> = None;

    // Readers first, writer second, and the writer on its own thread.
    //
    // A child that writes as it reads, `git check-attr --stdin` being the one
    // that bit, blocks on a full stdout pipe long before it has drained an
    // input larger than the pipe buffer. Writing stdin here on the calling
    // thread would then block against a child blocked on output nobody is
    // reading yet, and because that happens before the poll loop below, the
    // timeout would never fire. Neither side can stall now: output is draining
    // from the moment the child starts.
    let out_reader = Reader::spawn(child.stdout.take().expect("stdout piped"));
    let err_reader = Reader::spawn(child.stderr.take().expect("stderr piped"));

    if let Some(bytes) = input {
        if let Some(mut pipe) = child.stdin.take() {
            let owned = bytes.to_vec();
            // Detached: a child that exits early leaves the write erroring with
            // a broken pipe, which is its answer and not a failure of ours.
            // Dropping the handle at the end of the thread closes the pipe,
            // which the child needs in order to see EOF and exit.
            std::thread::spawn(move || {
                let _ = pipe.write_all(&owned);
            });
        }
    }

    let deadline = Instant::now() + opts.timeout;
    let mut poll = Duration::from_millis(5);
    let mut timed_out = false;
    let mut stopped = false;
    let mut quiet_error = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if stop.load(Ordering::SeqCst) {
                    if let Err(error) = stop_and_reap(&mut child, tree.as_ref()) {
                        quiet_error = Some(error);
                    }
                    stopped = true;
                    break None;
                }
                if Instant::now() >= deadline {
                    if let Err(error) = stop_and_reap(&mut child, tree.as_ref()) {
                        quiet_error = Some(error);
                    }
                    timed_out = true;
                    break None;
                }
                #[cfg(unix)]
                if let Some(tree) = tree.as_mut() {
                    tree.sample_if_due();
                }
                std::thread::sleep(poll);
                // Back off so a long agent run is not a busy loop, but stay
                // responsive for the many fast git and gh calls.
                poll = (poll * 2).min(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(poll_failure(error, argv, || {
                    stop_and_reap(&mut child, tree.as_ref())
                }));
            }
        }
    };

    // A stopped or timed out call was swept while its child was still alive.
    #[cfg(unix)]
    if let (Some(_), Some(tree)) = (status, &tree) {
        if let Err(error) = tree.stop(false) {
            quiet_error = Some(error);
        }
    }

    // Never join the readers.
    //
    // An agent CLI runs shell commands, and any grandchild that inherited the
    // pipe keeps its write end open after its parent exits. `read_to_end` on
    // such a pipe never returns, so joining would hang past the deadline and
    // the timeout would bound nothing at all. Instead, wait for the readers to
    // finish or for the output to stop arriving, then take what did.
    let stdout = out_reader.collect(DRAIN_GRACE);
    let stderr = err_reader.collect(DRAIN_GRACE);

    if stopped {
        let cleanup = quiet_error
            .map(|error| format!(" Stopping it also failed: {error}."))
            .unwrap_or_default();
        return Err(SparError::interrupted(format!(
            "interrupted: {}.{cleanup}",
            abbreviate(argv)
        )));
    }
    if let Some(error) = quiet_error {
        return Err(quiet_point_failure(error, argv));
    }

    if timed_out {
        #[cfg(not(unix))]
        if opts.stop_descendants {
            return Err(SparError::uncertain_write(format!(
                "timed out after {}s: {}\nThe direct process was stopped, but remaining child \
                 processes could not be stopped on this platform. The worktree must be inspected \
                 before retrying.",
                opts.timeout.as_secs(),
                abbreviate(argv)
            )));
        }
        return Err(SparError::timed_out(format!(
            "timed out after {}s: {}\nRaise `timeout` on this agent in spar.toml if the model \
             legitimately needs longer. Not retried: asking again would wait exactly as long a \
             second time.",
            opts.timeout.as_secs(),
            abbreviate(argv)
        )));
    }

    Ok(Output {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stdout_bytes: stdout,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.and_then(|s| s.code()).unwrap_or(-1),
    })
}

fn stop_and_reap(
    child: &mut std::process::Child,
    tree: Option<&Descendants>,
) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = tree;

    #[cfg(unix)]
    if let Some(tree) = tree {
        if let Err(tree_error) = tree.stop(true) {
            if child.kill().is_ok() {
                let _ = child.wait();
            }
            return Err(tree_error);
        }
        return child.wait().map(|_| ());
    }

    match child.kill() {
        Ok(()) => child.wait().map(|_| ()),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(kill_error),
            Err(poll_error) => Err(std::io::Error::new(
                poll_error.kind(),
                format!("could not stop the direct process: {kill_error}; {poll_error}"),
            )),
        },
    }
}

fn poll_failure(
    error: std::io::Error,
    argv: &[String],
    stop_and_reap: impl FnOnce() -> std::io::Result<()>,
) -> SparError {
    let cleanup = stop_and_reap()
        .err()
        .map(|cleanup| format!(" Cleanup also failed: {cleanup}."))
        .unwrap_or_default();
    SparError::uncertain_write(format!(
        "could not confirm whether `{}` stopped: {error}. SPAR attempted to stop and reap the \
         direct process and its descendants.{cleanup} The worktree must be inspected before \
         retrying.",
        abbreviate(argv),
    ))
}

fn quiet_point_failure(error: std::io::Error, argv: &[String]) -> SparError {
    SparError::uncertain_write(format!(
        "could not establish a quiet point after `{}`: {error}. The worktree must be inspected \
         before retrying.",
        abbreviate(argv)
    ))
}

/// The variable every process below a call inherits, whatever process group
/// or session it moves to.
///
/// The call's own process group is not enough to find them. Agent CLIs start
/// each shell in a new group and session, so a signal to the call's group
/// reaches the CLI and none of the test suites it started, and those keep
/// running under init once the call is over. The value is a list, so a spar
/// started inside an agent call marks its own calls without unmarking the one
/// it runs in.
///
/// macOS does not show the environment of Apple's own binaries, `/bin/sh` and
/// `/bin/zsh` among them, to another process. There the variable finds the
/// test runner a shell started and not the shell, and the session and the
/// samples below find the shell.
#[cfg(unix)]
const CALLS_VAR: &str = "SPAR_CALLS";

/// How often a running call's descendants are noted down. One that leaves the
/// tree by outliving its parent is still recognised later if a sample saw it
/// while it was in it. A call shorter than this is never sampled at all.
#[cfg(unix)]
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// How many times a sweep looks again for processes that started while it was
/// freezing the ones it had already found.
#[cfg(unix)]
const FREEZE_ROUNDS: usize = 8;

#[cfg(unix)]
static NEXT_CALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A token no other call has used, in this spar or in an earlier one that had
/// the same pid.
#[cfg(unix)]
fn call_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}.{nanos}.{}",
        std::process::id(),
        NEXT_CALL.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(unix)]
fn calls_value(outer: Option<&str>, token: &str) -> String {
    match outer {
        Some(outer) if !outer.is_empty() => format!("{outer}:{token}"),
        _ => token.to_string(),
    }
}

/// Whether a NUL separated environment block marks its process as one of the
/// call's.
#[cfg(unix)]
fn carries(environment: &[u8], token: &str) -> bool {
    let prefix = format!("{CALLS_VAR}=");
    environment
        .split(|&b| b == 0)
        .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .any(|value| value.split(|&b| b == b':').any(|t| t == token.as_bytes()))
}

/// The processes one call started.
#[cfg(unix)]
struct Descendants {
    root: u32,
    token: String,
    /// When the direct child started, in the process table's own units.
    /// Nothing that started earlier can descend from it, which spares reading
    /// the environment of every other process on the machine.
    floor: Option<u64>,
    /// What the last sample found, with when each started, which tells one
    /// from a stranger that was later given its pid.
    seen: std::collections::BTreeMap<libc::pid_t, u64>,
    /// Sessions the last sample found a descendant in, other than spar's own,
    /// with when each one's leader started if it was still running. A session
    /// outlives its leader, and the start tells the leader from a stranger
    /// later given its pid, which would lead a session of its own.
    sessions: std::collections::BTreeMap<libc::pid_t, Option<u64>>,
    sampled: Instant,
}

#[cfg(not(unix))]
struct Descendants;

#[cfg(unix)]
impl Descendants {
    fn new(root: u32, token: String) -> Self {
        let floor = libc::pid_t::try_from(root)
            .ok()
            .and_then(table::entry)
            .map(|entry| entry.start);
        Self {
            root,
            token,
            floor,
            seen: Default::default(),
            sessions: Default::default(),
            sampled: Instant::now(),
        }
    }

    /// Note down what is below the call now, once `SAMPLE_EVERY` has passed
    /// since the last look. A failed sample leaves the last one standing.
    fn sample_if_due(&mut self) {
        if self.sampled.elapsed() < SAMPLE_EVERY {
            return;
        }
        self.sampled = Instant::now();
        let (Ok(root), Ok(procs)) = (libc::pid_t::try_from(self.root), table::list()) else {
            return;
        };
        // The child has not been reaped, so its pid is still its own even if
        // it has just exited.
        let found = self.find(&procs, root, true, true, false);
        let my_session = unsafe { libc::getsid(0) };
        let start_of = |pid: libc::pid_t| procs.iter().find(|p| p.pid == pid).map(|p| p.start);
        let found: Vec<&Entry> = procs.iter().filter(|p| found.contains(&p.pid)).collect();
        self.seen = found.iter().map(|p| (p.pid, p.start)).collect();
        self.sessions = found
            .iter()
            .filter(|p| p.sid != my_session)
            .map(|p| (p.sid, start_of(p.sid)))
            .collect();
    }

    /// Freeze the call's process group and every process found below it, then
    /// kill all of them.
    ///
    /// Freezing first is what lets the search end. A stopped process cannot
    /// start another, so each round finds only what started before its parent
    /// was frozen. `root_alive` is false once the child has been reaped, since
    /// its pid may then belong to somebody else.
    fn stop(&self, root_alive: bool) -> std::io::Result<()> {
        let root = libc::pid_t::try_from(self.root).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the child process id does not fit a platform process-group id",
            )
        })?;
        let mut frozen = std::collections::BTreeSet::new();
        let mut root_group = true;
        let searched = self.freeze(root, root_alive, &mut frozen, &mut root_group);

        // Whatever the search managed, everything it froze is killed: a
        // stopped process left behind would hold its files open for good. The
        // group goes first and once, because a second signal to it finds only
        // the zombies the first one made.
        let mut killed = Ok(());
        if root_group {
            killed = signal_group(root, libc::SIGKILL);
        }
        for pid in &frozen {
            killed = killed.and(signal(*pid, libc::SIGKILL));
        }
        searched.and(killed)
    }

    /// Every process it could not stop is left out of the kill and reported
    /// once the rest are stopped, rather than ending the search: one setuid
    /// descendant must not leave its siblings running.
    fn freeze(
        &self,
        root: libc::pid_t,
        root_alive: bool,
        frozen: &mut std::collections::BTreeSet<libc::pid_t>,
        root_group: &mut bool,
    ) -> std::io::Result<()> {
        let mut refused: std::collections::BTreeSet<libc::pid_t> = Default::default();
        let mut failure = None;
        if root_alive {
            if let Err(error) = signal_group(root, libc::SIGSTOP) {
                failure.get_or_insert(error);
            }
        }
        // A fork under way when its parent was frozen can finish after the
        // next listing, so a round that froze something is always followed by
        // one more look.
        let mut busy = false;
        for _ in 0..FREEZE_ROUNDS {
            let procs = table::list()?;
            // A reaped child's pid is free again, and a process holding it now
            // leads a group that has nothing to do with this call.
            if !root_alive && procs.iter().any(|p| p.pid == root) {
                *root_group = false;
            }
            let found = self.find(&procs, root, root_alive, *root_group, true);
            let fresh: Vec<libc::pid_t> = found
                .iter()
                .filter(|&&pid| !frozen.contains(&pid) && !refused.contains(&pid))
                .copied()
                .collect();
            if fresh.is_empty() {
                if !busy {
                    return failure.map_or(Ok(()), Err);
                }
                busy = false;
                continue;
            }
            busy = true;
            for pid in fresh {
                match signal(pid, libc::SIGSTOP) {
                    Ok(()) => {
                        frozen.insert(pid);
                    }
                    Err(error) => {
                        refused.insert(pid);
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        Err(failure.unwrap_or_else(|| {
            std::io::Error::other(
                "processes below the call kept starting while they were being stopped",
            )
        }))
    }

    /// Everything in `procs` that belongs to the call.
    ///
    /// It starts from the child while it is alive, the child's process group,
    /// whatever the last sample saw that is still the same process, and, with
    /// `marks`, whatever carries the call's token. From there it spreads to
    /// children, to the members of any group one of them leads, and to the
    /// members of any session one of them is in other than spar's own. A
    /// process cannot join a session it did not inherit, so a session that
    /// holds a descendant and is not spar's was created by a descendant, and
    /// everything in it descends from that one.
    fn find(
        &self,
        procs: &[Entry],
        root: libc::pid_t,
        root_alive: bool,
        root_group: bool,
        marks: bool,
    ) -> std::collections::BTreeSet<libc::pid_t> {
        let me = unsafe { libc::getpid() };
        let uid = unsafe { libc::geteuid() };
        let my_session = unsafe { libc::getsid(0) };
        let by_pid: std::collections::HashMap<libc::pid_t, &Entry> =
            procs.iter().map(|p| (p.pid, p)).collect();
        // The session a sample recorded, unless its id now belongs to a
        // stranger that made a session of its own.
        let recorded = |sid: libc::pid_t| match self.sessions.get(&sid) {
            None => false,
            Some(leader) => by_pid
                .get(&sid)
                .is_none_or(|now| *leader == Some(now.start)),
        };
        let marked = |p: &Entry| {
            marks
                && p.uid == uid
                && self.floor.is_none_or(|floor| p.start >= floor)
                && table::environment(p.pid).is_some_and(|env| carries(&env, &self.token))
        };
        let mut ours: std::collections::BTreeSet<libc::pid_t> = procs
            .iter()
            .filter(|p| p.pid != me)
            .filter(|p| {
                (root_alive && p.pid == root)
                    || (root_group && p.pgid == root)
                    || self.seen.get(&p.pid) == Some(&p.start)
                    || recorded(p.sid)
                    || marked(p)
            })
            .map(|p| p.pid)
            .collect();
        loop {
            let sessions: std::collections::BTreeSet<libc::pid_t> = procs
                .iter()
                .filter(|p| ours.contains(&p.pid) && p.sid != my_session)
                .map(|p| p.sid)
                .collect();
            let before = ours.len();
            for p in procs {
                if p.pid != me
                    && (ours.contains(&p.ppid)
                        || ours.contains(&p.pgid)
                        || sessions.contains(&p.sid))
                {
                    ours.insert(p.pid);
                }
            }
            if ours.len() == before {
                return ours;
            }
        }
    }
}

/// Send `sig` to a process, or to a process group when `target` is negative.
/// One that has already gone is not an error. Never this process, its group,
/// or the broadcast targets, whatever a search turned up.
#[cfg(unix)]
fn signal(target: libc::pid_t, sig: libc::c_int) -> std::io::Result<()> {
    let me = unsafe { libc::getpid() };
    let my_group = unsafe { libc::getpgrp() };
    if target == 0 || target == -1 || target == me || target == -my_group {
        return Ok(());
    }
    if unsafe { libc::kill(target, sig) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

/// Signal a process group. macOS refuses with EPERM, not ESRCH, when every
/// member left is a zombie, so a refusal counts only if a member still visible
/// in the process table refuses on its own.
#[cfg(unix)]
fn signal_group(group: libc::pid_t, sig: libc::c_int) -> std::io::Result<()> {
    match signal(-group, sig) {
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => table::list()?
            .iter()
            .filter(|p| p.pgid == group)
            .try_for_each(|p| signal(p.pid, sig)),
        other => other,
    }
}

/// One row of the process table.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    pid: libc::pid_t,
    ppid: libc::pid_t,
    pgid: libc::pid_t,
    sid: libc::pid_t,
    uid: libc::uid_t,
    /// When it started, comparable only with another start from the same table.
    start: u64,
}

#[cfg(target_os = "macos")]
mod table {
    use super::Entry;

    pub(super) fn list() -> std::io::Result<Vec<Entry>> {
        // Asked with no buffer, the kernel estimates how many there are.
        let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let mut room = usize::try_from(estimate).unwrap_or(0) + 256;
        loop {
            let mut pids: Vec<libc::pid_t> = vec![0; room];
            let bytes = libc::c_int::try_from(room * std::mem::size_of::<libc::pid_t>())
                .map_err(std::io::Error::other)?;
            let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            let Ok(listed) = usize::try_from(listed) else {
                return Err(std::io::Error::last_os_error());
            };
            // A full buffer may have cut the list short.
            if listed >= room {
                room *= 2;
                continue;
            }
            pids.truncate(listed);
            return Ok(pids.into_iter().filter_map(entry).collect());
        }
    }

    /// None for a process that has gone, and for one of another user's, which
    /// the kernel will not describe.
    pub(super) fn entry(pid: libc::pid_t) -> Option<Entry> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if filled != size {
            return None;
        }
        // SAFETY: the kernel filled every byte it was given, and zeroed is a
        // valid value of this plain C struct in any case.
        let info = unsafe { info.assume_init() };
        let sid = unsafe { libc::getsid(pid) };
        if sid < 0 {
            return None;
        }
        Some(Entry {
            pid,
            ppid: libc::pid_t::try_from(info.pbi_ppid).ok()?,
            pgid: libc::pid_t::try_from(info.pbi_pgid).ok()?,
            sid,
            uid: info.pbi_uid,
            start: info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
        })
    }

    pub(super) fn environment(pid: libc::pid_t) -> Option<Vec<u8>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // Asked with no buffer, the kernel says how much room the answer needs.
        let sized = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if sized != 0 || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        let read = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if read != 0 {
            return None;
        }
        buf.truncate(size);
        super::procargs_environment(&buf).map(<[u8]>::to_vec)
    }
}

#[cfg(target_os = "linux")]
mod table {
    use super::Entry;
    use std::os::unix::fs::MetadataExt;

    pub(super) fn list() -> std::io::Result<Vec<Entry>> {
        let mut procs = Vec::new();
        for item in std::fs::read_dir("/proc")? {
            let Ok(item) = item else { continue };
            // Only the numbered entries are processes.
            let Some(pid) = item.file_name().to_str().and_then(|name| name.parse().ok()) else {
                continue;
            };
            procs.extend(entry(pid));
        }
        Ok(procs)
    }

    /// None for a process that exited after the listing.
    pub(super) fn entry(pid: libc::pid_t) -> Option<Entry> {
        let dir = format!("/proc/{pid}");
        let uid = std::fs::metadata(&dir).ok()?.uid();
        let stat = std::fs::read(format!("{dir}/stat")).ok()?;
        super::parse_stat(pid, &String::from_utf8_lossy(&stat), uid)
    }

    pub(super) fn environment(pid: libc::pid_t) -> Option<Vec<u8>> {
        std::fs::read(format!("/proc/{pid}/environ")).ok()
    }
}

/// No process table is read here, so a sweep stops the call's own process
/// group and nothing that left it.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
mod table {
    use super::Entry;

    pub(super) fn list() -> std::io::Result<Vec<Entry>> {
        Ok(Vec::new())
    }

    pub(super) fn entry(_: libc::pid_t) -> Option<Entry> {
        None
    }

    pub(super) fn environment(_: libc::pid_t) -> Option<Vec<u8>> {
        None
    }
}

/// The environment in a macOS `KERN_PROCARGS2` answer: an argument count, the
/// executable path and its padding, the arguments, then the environment up to
/// an empty string.
#[cfg(any(target_os = "macos", test))]
fn procargs_environment(buf: &[u8]) -> Option<&[u8]> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &buf[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    let rest = &rest[path_end..];
    let args_start = rest.iter().position(|&b| b != 0)?;
    let mut rest = &rest[args_start..];
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0)?;
        rest = &rest[end + 1..];
    }
    let mut end = 0;
    for entry in rest.split(|&b| b == 0) {
        if entry.is_empty() {
            break;
        }
        end += entry.len() + 1;
    }
    Some(&rest[..end.min(rest.len())])
}

/// A Linux `/proc/<pid>/stat` line. The command name sits in parentheses and
/// may itself hold spaces and parentheses, so the fields are counted from the
/// last closing one.
#[cfg(all(unix, any(target_os = "linux", test)))]
fn parse_stat(pid: libc::pid_t, stat: &str, uid: libc::uid_t) -> Option<Entry> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(Entry {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        pgid: fields.get(2)?.parse().ok()?,
        sid: fields.get(3)?.parse().ok()?,
        uid,
        start: fields.get(19)?.parse().ok()?,
    })
}

/// How long to keep waiting for output after the child has exited, when a
/// surviving grandchild is holding the pipe open. Measured from the last byte
/// received, so a slow large read is never cut short.
const DRAIN_GRACE: Duration = Duration::from_secs(3);

/// A pipe drained on its own thread into a shared buffer.
///
/// Reading incrementally rather than with `read_to_end` is what lets the caller
/// take the output without joining, which is what keeps the timeout honest.
struct Reader {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl Reader {
    fn spawn<R: Read + Send + 'static>(mut pipe: R) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (buf_w, done_w) = (Arc::clone(&buf), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match pipe.read(&mut chunk) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf_w
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend_from_slice(&chunk[..n]),
                }
            }
            done_w.store(true, Ordering::Release);
        });
        Self { buf, done }
    }

    fn len(&self) -> usize {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Everything received once the reader finishes, or once `grace` passes
    /// with no new bytes.
    ///
    /// The poll starts fine grained and backs off. A run makes hundreds of git
    /// and gh calls whose pipes are already at EOF by the time the child is
    /// reaped, and a flat ten millisecond wait on each one is real wall clock
    /// spent on nothing.
    fn collect(&self, grace: Duration) -> Vec<u8> {
        let mut last_len = self.len();
        let mut quiet_since = Instant::now();
        let mut poll = Duration::from_micros(100);
        while !self.done.load(Ordering::Acquire) {
            let now_len = self.len();
            if now_len != last_len {
                last_len = now_len;
                quiet_since = Instant::now();
            } else if quiet_since.elapsed() >= grace {
                break;
            }
            std::thread::sleep(poll);
            poll = (poll * 2).min(Duration::from_millis(10));
        }
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Run a command and return stdout. With `check` set, a non-zero exit is an
/// error carrying both streams.
pub fn run(argv: &[String], opts: &ExecOpts) -> Result<String> {
    let out = exec(argv, opts)?;
    if opts.check && !out.ok() {
        return Err(SparError::call_failed(failure_message(argv, &out)));
    }
    Ok(out.stdout)
}

pub(crate) fn run_with_input_bytes(
    argv: &[String],
    opts: &ExecOpts,
    input: &[u8],
) -> Result<String> {
    let out = exec_with_input(argv, opts, Some(input))?;
    if opts.check && !out.ok() {
        return Err(SparError::call_failed(failure_message(argv, &out)));
    }
    Ok(out.stdout)
}

/// Run a command and return stdout without changing non-UTF-8 bytes.
pub fn run_bytes(argv: &[String], opts: &ExecOpts) -> Result<Vec<u8>> {
    let out = exec(argv, opts)?;
    if opts.check && !out.ok() {
        return Err(SparError::call_failed(failure_message(argv, &out)));
    }
    Ok(out.stdout_bytes)
}

/// Convenience for the many `run(&["git".into(), ...])` call sites.
pub fn run_str(argv: &[&str], opts: &ExecOpts) -> Result<String> {
    let owned: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    run(&owned, opts)
}

/// Prompts run to many kilobytes. Echoing them whole buries the actual error.
pub fn abbreviate(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let one_line = arg.split_whitespace().collect::<Vec<_>>().join(" ");
            if one_line.chars().count() <= 60 {
                one_line
            } else {
                let head: String = one_line.chars().take(57).collect();
                format!("{head}...")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Both streams, labelled. Preferring stderr is not enough: one agent CLI
/// writes chatter like "Reading additional input from stdin..." to stderr on
/// every run, which outranks the real reason and explains nothing, while
/// another reports fatal conditions on stdout with stderr empty.
pub fn failure_message(argv: &[String], out: &Output) -> String {
    let mut parts = vec![format!(
        "command failed ({}): {}",
        out.code,
        abbreviate(argv)
    )];
    for (label, stream) in [("stderr", &out.stderr), ("stdout", &out.stdout)] {
        let text = stream.trim();
        if !text.is_empty() {
            parts.push(format!("--- {label} ---\n{}", tail(text, 1500)));
        }
    }
    if parts.len() == 1 {
        parts.push("(no output on either stream)".to_string());
    }
    parts.join("\n")
}

/// Last `max` characters, on a character boundary.
fn tail(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let start = text
        .char_indices()
        .nth(count - max)
        .map(|(i, _)| i)
        .unwrap_or(0);
    &text[start..]
}

/// Whether a program is on PATH.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(program);
        is_executable(&candidate).then_some(candidate)
    })
}

pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Expand a leading `~` against $HOME. Nothing else: a config path is not a
/// shell word and should not behave like one.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(out: &str, err: &str, code: i32) -> Output {
        Output {
            stdout: out.into(),
            stdout_bytes: out.as_bytes().to_vec(),
            stderr: err.into(),
            code,
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_poll_failure_attempts_cleanup_and_is_uncertain() {
        let stopped = std::cell::Cell::new(false);
        let error = poll_failure(
            std::io::Error::other("poll failed"),
            &argv(&["editor"]),
            || {
                stopped.set(true);
                Ok(())
            },
        );

        assert!(stopped.get());
        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("poll failed"), "{error}");
    }

    #[test]
    fn a_stop_or_reap_failure_is_uncertain() {
        let error =
            quiet_point_failure(std::io::Error::other("could not reap"), &argv(&["editor"]));

        assert_eq!(crate::error::ErrorKind::UncertainWrite, error.kind());
        assert!(error.to_string().contains("could not reap"), "{error}");
    }

    #[test]
    fn stdin_reaches_the_child_byte_for_byte() {
        let body = "line one  \n\nline three\n";
        let out = run_str(&["/bin/sh", "-c", "cat"], &ExecOpts::new().stdin(body))
            .expect("the child reads stdin");
        assert_eq!(body, out);
    }

    #[test]
    fn byte_input_reaches_the_child_without_text_conversion() {
        let input = [b'f', 0xff, 0];
        let command = argv(&["/bin/sh", "-c", "cat"]);
        let out = exec_with_input(&command, &ExecOpts::new(), Some(&input))
            .expect("the child reads byte input");

        assert_eq!(input, out.stdout_bytes.as_slice());
    }

    #[test]
    fn stdout_used_when_stderr_is_empty() {
        let msg = failure_message(&argv(&["claude"]), &proc("You've hit your limit.", "", 1));
        assert!(msg.contains("hit your limit"), "{msg}");
    }

    #[test]
    fn stderr_shown_when_present() {
        let msg = failure_message(&argv(&["gh"]), &proc("noise", "real reason", 1));
        assert!(msg.contains("real reason"), "{msg}");
    }

    #[test]
    fn both_streams_are_shown_not_just_one() {
        let msg = failure_message(&argv(&["gh"]), &proc("on stdout", "on stderr", 1));
        assert!(
            msg.contains("on stdout") && msg.contains("on stderr"),
            "{msg}"
        );
    }

    #[test]
    fn says_something_when_both_are_empty() {
        assert!(failure_message(&argv(&["x"]), &proc("", "", 2)).contains("no output"));
    }

    #[test]
    fn long_arguments_are_abbreviated() {
        let long = "word ".repeat(500);
        let out = abbreviate(&argv(&["claude", "-p", &long]));
        assert!(out.len() < 200, "{}", out.len());
    }

    #[test]
    fn newlines_in_arguments_do_not_break_the_line() {
        assert!(!abbreviate(&argv(&["claude", "a\nb\nc"])).contains('\n'));
    }

    #[test]
    fn short_arguments_survive_intact() {
        assert_eq!(
            "gh pr merge 17",
            abbreviate(&argv(&["gh", "pr", "merge", "17"]))
        );
    }

    #[test]
    fn abbreviation_never_splits_a_character() {
        // A multi-byte argument longer than the cap must not panic on a slice
        // that lands mid-character.
        let wide = "\u{1f600}".repeat(200);
        let out = abbreviate(&argv(&[&wide]));
        assert!(out.ends_with("..."));
    }

    #[test]
    fn exit_code_is_reported() {
        let out = exec(
            &argv(&["sh", "-c", "exit 3"]),
            &ExecOpts::new().check(false),
        )
        .unwrap();
        assert_eq!(3, out.code);
    }

    #[test]
    fn check_false_returns_stdout_on_failure() {
        let text = run(
            &argv(&["sh", "-c", "echo partial; exit 1"]),
            &ExecOpts::new().check(false),
        )
        .unwrap();
        assert_eq!("partial\n", text);
    }

    #[test]
    fn check_true_fails_loudly() {
        let err = run(
            &argv(&["sh", "-c", "echo why >&2; exit 1"]),
            &ExecOpts::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("why"), "{err}");
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // Well past a pipe buffer on every platform spar runs on.
        let text = run(
            &argv(&["sh", "-c", "yes hello | head -c 400000"]),
            &ExecOpts::new().timeout_secs(60),
        )
        .unwrap();
        assert_eq!(400_000, text.len());
    }

    /// Input larger than a pipe buffer, from a child that answers as it reads.
    ///
    /// This is the shape `git check-attr --stdin` has on a repository of a few
    /// thousand files. Writing stdin before the readers started deadlocked the
    /// pair: the child blocked writing to a stdout pipe nobody was draining,
    /// spar blocked writing the rest of stdin, and the timeout never fired
    /// because the poll loop had not been reached.
    #[test]
    fn large_input_and_large_output_together_do_not_deadlock() {
        let line = "a".repeat(199);
        let input = std::iter::repeat_n(line.as_str(), 5_000)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(input.len() > 900_000, "the input has to outgrow the pipe");

        let start = Instant::now();
        // `cat` on both streams, so the child is writing while it is still
        // reading and neither pipe can be left unattended.
        let out = run(
            &argv(&["sh", "-c", "tee /dev/stderr"]),
            &ExecOpts::new().timeout_secs(60).stdin(input.clone()),
        )
        .unwrap();

        assert_eq!(input.len(), out.trim_end().len());
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the call did not return promptly: {:?}",
            start.elapsed()
        );
    }

    /// The reason the readers are never joined. A grandchild that inherited
    /// the pipe holds its write end open after its parent exits, so
    /// `read_to_end` would never return and the deadline would bound nothing.
    #[test]
    fn a_surviving_grandchild_holding_the_pipe_cannot_hang_the_timeout() {
        let start = Instant::now();
        let err = run(
            &argv(&["sh", "-c", "sleep 120 & echo parent-output; sleep 60"]),
            &ExecOpts::new().timeout_secs(1),
        )
        .unwrap_err();
        let elapsed = start.elapsed();

        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            elapsed < Duration::from_secs(20),
            "the timeout did not bound the call: {elapsed:?}"
        );
    }

    #[test]
    fn a_surviving_grandchild_does_not_hang_a_normal_exit_either() {
        let start = Instant::now();
        let out = run(
            &argv(&["sh", "-c", "sleep 120 & echo done"]),
            &ExecOpts::new().timeout_secs(60),
        )
        .unwrap();
        assert!(out.contains("done"), "{out:?}");
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "waited on a grandchild that will never exit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_process_group_cannot_edit_after_the_parent_exits() {
        let late = std::env::temp_dir().join(format!(
            "spar-late-child-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&late);
        let out = run(
            &argv(&[
                "/bin/sh",
                "-c",
                "(sleep 1; touch \"$1\") & echo done",
                "sh",
                late.to_str().unwrap(),
            ]),
            &ExecOpts::new().stop_descendants(true),
        )
        .unwrap();

        assert!(out.contains("done"), "{out:?}");
        std::thread::sleep(Duration::from_millis(1_200));
        assert!(!late.exists(), "a descendant edited after the quiet point");
    }

    /// A file a descendant would write too late, unique to the test.
    #[cfg(unix)]
    fn late_file(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("spar-late-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A descendant that leaves for a session of its own, the way an agent CLI
    /// starts every shell, and writes the file named by `$1` after `secs`.
    #[cfg(unix)]
    fn leaves_then_writes(secs: u32) -> String {
        format!(
            "perl -MPOSIX -e 'POSIX::setsid() or die; sleep {secs}; \
             open(my $f, \">\", $ARGV[0]) or die' \"$1\""
        )
    }

    #[cfg(unix)]
    fn run_shell(script: &str, late: &Path, opts: &ExecOpts) -> Result<String> {
        run(
            &argv(&["/bin/sh", "-c", script, "sh", late.to_str().unwrap()]),
            opts,
        )
    }

    /// A sample sees the descendant while its parent is alive, which is what
    /// finds it afterwards on macOS, where perl does not show its environment
    /// to anybody else.
    #[cfg(unix)]
    #[test]
    fn a_descendant_in_a_session_of_its_own_cannot_edit_after_the_parent_exits() {
        let late = late_file("session");
        let script = format!("{} & sleep 1.5; echo done", leaves_then_writes(3));
        let out = run_shell(&script, &late, &ExecOpts::new().stop_descendants(true)).unwrap();

        assert!(out.contains("done"), "{out:?}");
        std::thread::sleep(Duration::from_millis(2_500));
        assert!(!late.exists(), "a descendant edited after the quiet point");
    }

    /// A sample sees perl in its own session. perl then starts a shell that no
    /// sample sees and exits, so on macOS, where neither shows its
    /// environment, only the session ties that shell to the call.
    #[cfg(unix)]
    #[test]
    fn a_process_no_sample_saw_is_found_through_the_session_it_was_born_in() {
        let late = late_file("born");
        let perl = r#"perl -MPOSIX -e 'POSIX::setsid() or die; select(undef, undef, undef, 0.8); if (fork() == 0) { exec("/bin/sh", "-c", "sleep 2; : > \"\$0\"", $ARGV[0]) or die }' "$1""#;
        let script = format!("{perl} & sleep 0.9; echo done");
        let out = run_shell(&script, &late, &ExecOpts::new().stop_descendants(true)).unwrap();

        assert!(out.contains("done"), "{out:?}");
        std::thread::sleep(Duration::from_millis(2_500));
        assert!(
            !late.exists(),
            "a shell born in a descendant's session survived"
        );
    }

    /// Two calls can run at once, as both triage calls do, and one ending must
    /// not reach into the other.
    #[cfg(unix)]
    #[test]
    fn one_call_ending_leaves_another_calls_descendants_alone() {
        let proof = late_file("concurrent");
        let other = {
            let proof = proof.clone();
            std::thread::spawn(move || {
                let script = format!("{} & sleep 3; echo done", leaves_then_writes(2));
                run_shell(&script, &proof, &ExecOpts::new().stop_descendants(true))
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        run(
            &argv(&["/bin/sh", "-c", "sleep 1"]),
            &ExecOpts::new().stop_descendants(true),
        )
        .unwrap();

        let out = other.join().unwrap().unwrap();
        assert!(out.contains("done"), "{out:?}");
        assert!(
            proof.exists(),
            "one call's sweep stopped another call's descendant"
        );
        let _ = std::fs::remove_file(&proof);
    }

    /// What every one of the four timed out implementations did: the test
    /// suite an agent started kept running into the next issue's hour.
    #[cfg(unix)]
    #[test]
    fn a_timeout_stops_a_descendant_in_a_session_of_its_own() {
        let late = late_file("timeout");
        let script = format!("{} & sleep 30", leaves_then_writes(2));
        let err = run_shell(
            &script,
            &late,
            &ExecOpts::new().timeout_secs(1).stop_descendants(true),
        )
        .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err}");
        std::thread::sleep(Duration::from_millis(2_500));
        assert!(!late.exists(), "a descendant outlived the timeout");
    }

    /// Without the variable, a descendant is still found by its parent while
    /// the call it belongs to is alive.
    #[cfg(unix)]
    #[test]
    fn a_timeout_stops_a_descendant_that_dropped_its_environment() {
        let late = late_file("bare");
        let script = format!("env -i {} & sleep 30", leaves_then_writes(2));
        let err = run_shell(
            &script,
            &late,
            &ExecOpts::new().timeout_secs(1).stop_descendants(true),
        )
        .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err}");
        std::thread::sleep(Duration::from_millis(2_500));
        assert!(!late.exists(), "a descendant without the variable survived");
    }

    /// The parent exits before the first sample, so only the variable can
    /// find what it left behind.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_descendant_no_sample_saw_is_found_by_its_environment() {
        let late = late_file("marked");
        let script = format!("{} & sleep 0.1; echo done", leaves_then_writes(1));
        let out = run_shell(&script, &late, &ExecOpts::new().stop_descendants(true)).unwrap();

        assert!(out.contains("done"), "{out:?}");
        std::thread::sleep(Duration::from_millis(1_500));
        assert!(
            !late.exists(),
            "an unsampled descendant edited after the quiet point"
        );
    }

    /// A spar started inside an agent call adds its own token to the list, and
    /// what it starts still belongs to the outer call.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_descendant_of_a_nested_call_still_belongs_to_the_outer_one() {
        let late = late_file("nested");
        let script = format!(
            "SPAR_CALLS=\"$SPAR_CALLS:inner\" {} & sleep 0.1; echo done",
            leaves_then_writes(1)
        );
        let out = run_shell(&script, &late, &ExecOpts::new().stop_descendants(true)).unwrap();

        assert!(out.contains("done"), "{out:?}");
        std::thread::sleep(Duration::from_millis(1_500));
        assert!(
            !late.exists(),
            "a nested call's descendant edited after the quiet point"
        );
    }

    /// The marker is only worth anything if another process's environment can
    /// be read, and this binary is not one of Apple's.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_process_environment_can_be_read_back() {
        let env = table::environment(unsafe { libc::getpid() }).expect("readable");
        assert!(
            env.split(|&b| b == 0)
                .any(|entry| entry.starts_with(b"PATH=")),
            "{}",
            String::from_utf8_lossy(&env)
        );
    }

    /// The sweep goes by ancestry, group, and the inherited variable, never by
    /// start time alone.
    #[cfg(unix)]
    #[test]
    fn a_process_that_only_started_during_the_call_is_left_alone() {
        let stranger = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
            Command::new("sleep").arg("30").spawn()
        });
        run(
            &argv(&["/bin/sh", "-c", "sleep 1"]),
            &ExecOpts::new().stop_descendants(true),
        )
        .unwrap();

        let mut stranger = stranger.join().unwrap().unwrap();
        let alive = stranger.try_wait().unwrap().is_none();
        let _ = stranger.kill();
        let _ = stranger.wait();
        assert!(alive, "the sweep killed a process the call did not start");
    }

    /// A group that has just been killed holds only zombies, and macOS
    /// answers a signal to it with EPERM. Treated as a refusal, it turned a
    /// timeout into an uncertain write.
    #[cfg(unix)]
    #[test]
    fn a_group_of_zombies_is_not_a_refusal() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        // Exited and not yet reaped, so the group is still there.
        std::thread::sleep(Duration::from_millis(300));
        let result = signal_group(pid, libc::SIGKILL);
        let _ = child.wait();
        result.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_call_extends_the_list_rather_than_replacing_it() {
        assert_eq!("a", calls_value(None, "a"));
        assert_eq!("a", calls_value(Some(""), "a"));
        assert_eq!("outer:a", calls_value(Some("outer"), "a"));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_whole_token_in_the_variable_marks_a_process() {
        let env = b"HOME=/h\0SPAR_CALLS=1.2.3:9.8.7\0PATH=/bin\0";
        assert!(carries(env, "1.2.3"));
        assert!(carries(env, "9.8.7"));
        assert!(!carries(env, "1.2"));
        assert!(!carries(env, "2.3"));
        assert!(!carries(b"XSPAR_CALLS=1.2.3\0", "1.2.3"));
        assert!(!carries(b"OTHER=SPAR_CALLS=1.2.3\0", "1.2.3"));
    }

    #[test]
    fn the_environment_is_read_past_the_arguments_and_stops_at_the_apple_strings() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sh\0\0\0\0sh\0SPAR_CALLS=in-an-argument\0");
        buf.extend_from_slice(b"SPAR_CALLS=t\0HOME=/h\0\0executable_path=/bin/sh\0");

        assert_eq!(
            Some(&b"SPAR_CALLS=t\0HOME=/h\0"[..]),
            procargs_environment(&buf)
        );
        assert_eq!(None, procargs_environment(b"\x01\0"));
    }

    #[cfg(unix)]
    #[test]
    fn a_stat_line_is_read_past_a_command_name_with_spaces_and_parentheses() {
        let stat = "4242 (a) (b c) S 17 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 \
                    987654 0 0";
        let entry = parse_stat(4242, stat, 501).unwrap();

        assert_eq!(17, entry.ppid);
        assert_eq!(4242, entry.pgid);
        assert_eq!(4242, entry.sid);
        assert_eq!(501, entry.uid);
        assert_eq!(987_654, entry.start);
    }

    #[test]
    fn timeout_kills_and_explains() {
        let err = run(
            &argv(&["sh", "-c", "sleep 30"]),
            &ExecOpts::new().timeout_secs(1),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn missing_binary_names_the_command() {
        let err = exec(
            &argv(&["spar-definitely-not-a-real-binary"]),
            &ExecOpts::new(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("spar-definitely-not-a-real-binary"),
            "{err}"
        );
    }

    #[test]
    fn tilde_expands_against_home() {
        std::env::set_var("HOME", "/home/someone");
        assert_eq!(PathBuf::from("/home/someone/bin"), expand_tilde("~/bin"));
        assert_eq!(PathBuf::from("/absolute"), expand_tilde("/absolute"));
        assert_eq!(PathBuf::from("~notauser/x"), expand_tilde("~notauser/x"));
    }
}

#[cfg(test)]
mod timeout_kind_tests {
    use super::*;
    use crate::error::ErrorKind;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// A deadline is not a bad answer. Retrying one buys another wait of
    /// exactly the same length, which on a review at the highest effort
    /// setting turned a thirty minute failure into an hour of it.
    #[test]
    fn a_timeout_is_marked_as_one_and_is_not_worth_retrying() {
        let err = run(
            &["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            &ExecOpts::new().timeout_secs(1),
        )
        .unwrap_err();

        assert_eq!(ErrorKind::TimedOut, err.kind());
        assert!(!err.worth_retrying());
        assert!(err.to_string().contains("Not retried"), "{err}");
    }

    /// `spar listen` runs for hours. Ctrl-C has to stop the agent call in
    /// flight rather than leave it running in its own process group with
    /// nobody to read its answer.
    #[test]
    fn a_cancelled_command_is_stopped_and_says_so() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let err = exec_until(
            &argv(&["sh", "-c", "sleep 30"]),
            &ExecOpts::new().timeout_secs(30).stop_descendants(true),
            None,
            &stop,
        )
        .unwrap_err();

        assert_eq!(ErrorKind::Interrupted, err.kind());
        assert!(
            err.to_string().starts_with("interrupted: sh -c sleep 30"),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        assert!(!err.worth_retrying());
    }

    #[test]
    fn a_command_asked_for_after_the_stop_is_refused_without_running() {
        let stop = AtomicBool::new(true);
        let err = exec_until(
            &argv(&["sh", "-c", "exit 0"]),
            &ExecOpts::new(),
            None,
            &stop,
        )
        .unwrap_err();
        assert_eq!(ErrorKind::Interrupted, err.kind());
        assert!(err.to_string().contains("before"), "{err}");
    }

    #[test]
    fn a_sleep_ends_early_once_the_stop_is_set_and_on_time_otherwise() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert!(sleep_until(Duration::from_secs(30), &stop));
        assert!(started.elapsed() < Duration::from_secs(5));

        let calm = AtomicBool::new(false);
        let started = Instant::now();
        assert!(!sleep_until(Duration::from_millis(50), &calm));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    /// A non-zero exit is the CLI reporting that it could not answer, which is
    /// a different thing from an answer that arrived and could not be parsed.
    /// Only the second is what the retry exists for.
    #[test]
    fn a_non_zero_exit_is_the_call_failing_rather_than_the_answer() {
        let err = run(
            &["sh".to_string(), "-c".to_string(), "exit 1".to_string()],
            &ExecOpts::new(),
        )
        .unwrap_err();

        assert_eq!(ErrorKind::CallFailed, err.kind());
        // Not hopeless in the abstract, since it could have been transient.
        // Whether to spend a second call on it is `Agent`'s decision, and it
        // turns on whether there is a stand in to send the call to instead.
        assert!(err.worth_retrying());
    }
}
