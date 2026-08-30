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
}

impl Default for ExecOpts {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            check: true,
            env: Vec::new(),
            stdin: None,
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
}

#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: String,
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
    let program = argv
        .first()
        .ok_or_else(|| SparError::new("cannot run an empty command"))?;

    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if opts.stdin.is_some() {
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

    let mut child = command
        .spawn()
        .map_err(|e| SparError::new(format!("could not run `{}`: {e}", abbreviate(argv))))?;

    if let Some(text) = &opts.stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(text.as_bytes());
        }
        // Dropping the handle closes the pipe, which the child needs in order
        // to see EOF and exit.
    }

    let out_reader = Reader::spawn(child.stdout.take().expect("stdout piped"));
    let err_reader = Reader::spawn(child.stderr.take().expect("stderr piped"));

    let deadline = Instant::now() + opts.timeout;
    let mut poll = Duration::from_millis(5);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(poll);
                // Back off so a long agent run is not a busy loop, but stay
                // responsive for the many fast git and gh calls.
                poll = (poll * 2).min(Duration::from_millis(100));
            }
        }
    };

    // Never join the readers.
    //
    // An agent CLI runs shell commands, and any grandchild that inherited the
    // pipe keeps its write end open after its parent exits. `read_to_end` on
    // such a pipe never returns, so joining would hang past the deadline and
    // the timeout would bound nothing at all. Instead, wait for the readers to
    // finish or for the output to stop arriving, then take what did.
    let stdout = out_reader.collect(DRAIN_GRACE);
    let stderr = err_reader.collect(DRAIN_GRACE);

    if timed_out {
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
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.and_then(|s| s.code()).unwrap_or(-1),
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
            stderr: err.into(),
            code,
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stdin_reaches_the_child_byte_for_byte() {
        let body = "line one  \n\nline three\n";
        let out = run_str(&["/bin/sh", "-c", "cat"], &ExecOpts::new().stdin(body))
            .expect("the child reads stdin");
        assert_eq!(body, out);
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
