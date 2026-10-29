//! Other repositories a run may read, from `[[projects]]`.
//!
//! Each project gets a bare mirror that spar owns, inside the primary
//! repository's git directory, and every agent call gets a fresh detached
//! checkout of the mirror's `origin/<base>`. The user's own clone is only ever
//! asked for its origin URL: spar never fetches into it, checks it out, or
//! adds a worktree to it, so it is exactly as it was after a run.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::config::{github_slug, ProjectAccess, ProjectCfg, ProjectSource};
use crate::error::{Result, SparError};
use crate::proc::{self, ExecOpts};
use crate::{bail, logdim, logwarn, spar_err};

const GIT_TIMEOUT: u64 = 600;

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut argv = vec!["git", "-c", "maintenance.auto=false", "-c", "gc.auto=0"];
    argv.extend_from_slice(args);
    proc::run_str(&argv, &ExecOpts::new().cwd(cwd).timeout_secs(GIT_TIMEOUT))
}

fn git_try(cwd: &Path, args: &[&str]) -> Option<String> {
    git(cwd, args).ok().map(|out| out.trim().to_string())
}

/// The top of the working tree `path` is in, or None when it is in none.
fn toplevel(path: &Path) -> Option<PathBuf> {
    let top = git_try(path, &["rev-parse", "--show-toplevel"])?;
    std::fs::canonicalize(&top).ok()
}

/// The origin URL of a clone, made absolute when it is a relative path.
fn origin_url(clone: &Path) -> Option<String> {
    let url = git_try(clone, &["remote", "get-url", "origin"])?;
    let looks_local = !url.contains("://") && !url.contains(':');
    if looks_local && Path::new(&url).is_relative() {
        return Some(clone.join(&url).display().to_string());
    }
    Some(url)
}

/// One configured project, resolved against the primary repository.
pub struct Project {
    pub name: String,
    pub access: ProjectAccess,
    /// `owner/name` when it is on GitHub.
    pub slug: Option<String>,
    /// The user's clone, for a `path` project.
    pub path: Option<PathBuf>,
    /// The bare repository spar fetches into and checks out from.
    mirror: PathBuf,
    /// The base branch, fetched once per run. The error is kept as text so
    /// every call after a failed fetch reports the same reason.
    base: OnceLock<std::result::Result<String, String>>,
    /// `git worktree add` on one repository is not safe to run twice at once.
    adding: Mutex<()>,
}

impl Project {
    /// Where the project's history comes from, for `doctor` and errors.
    pub fn location(&self) -> String {
        match (&self.path, &self.slug) {
            (Some(path), _) => path.display().to_string(),
            (None, Some(slug)) => {
                format!("github.com/{slug} (cloned to {})", self.mirror.display())
            }
            (None, None) => self.mirror.display().to_string(),
        }
    }

    /// The URL the mirror fetches from.
    fn remote(&self) -> Result<String> {
        match &self.path {
            Some(path) => origin_url(path).ok_or_else(|| {
                spar_err!(
                    "project '{}': {} has no origin remote to fetch from",
                    self.name,
                    path.display()
                )
            }),
            None => bail!("project '{}' is fetched by its own origin", self.name),
        }
    }

    /// Make the mirror if it is not there, fetch it, and return the base.
    fn fetch(&self) -> Result<String> {
        let fetched = self
            .base
            .get_or_init(|| self.fetch_now().map_err(|e| e.message().to_string()));
        fetched.clone().map_err(SparError::new)
    }

    fn fetch_now(&self) -> Result<String> {
        if !self.mirror.join("HEAD").exists() {
            self.create_mirror()?;
        }
        if self.path.is_some() {
            // The user's origin can move, so the mirror follows it each run.
            git(
                &self.mirror,
                &["remote", "set-url", "origin", &self.remote()?],
            )?;
        }
        git(&self.mirror, &["worktree", "prune"])?;
        git(&self.mirror, &["fetch", "--prune", "--quiet", "origin"]).map_err(|e| {
            spar_err!(
                "project '{}': could not fetch: {}",
                self.name,
                e.last_line()
            )
        })?;
        // A failure here leaves whatever origin/HEAD the last run found.
        let _ = git(&self.mirror, &["remote", "set-head", "origin", "--auto"]);
        let head = git_try(&self.mirror, &["symbolic-ref", "refs/remotes/origin/HEAD"])
            .unwrap_or_default();
        match head.strip_prefix("refs/remotes/origin/") {
            Some(base) if !base.is_empty() => Ok(base.to_string()),
            _ => bail!(
                "project '{}': could not tell its base branch from origin/HEAD",
                self.name
            ),
        }
    }

    fn create_mirror(&self) -> Result<()> {
        if let Some(parent) = self.mirror.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| spar_err!("could not create {}: {e}", parent.display()))?;
        }
        let mirror = self.mirror.display().to_string();
        match (&self.path, &self.slug) {
            (Some(_), _) => {
                git(Path::new("/"), &["init", "--quiet", "--bare", &mirror])?;
                git(&self.mirror, &["remote", "add", "origin", &self.remote()?])?;
            }
            (None, Some(slug)) => {
                // gh rather than git so a private repository uses the same
                // login every other call spar makes to GitHub does.
                proc::run_str(
                    &[
                        "gh", "repo", "clone", slug, &mirror, "--", "--bare", "--quiet",
                    ],
                    &ExecOpts::new().timeout_secs(GIT_TIMEOUT),
                )
                .map_err(|e| {
                    spar_err!(
                        "project '{}': could not clone {slug}: {}",
                        self.name,
                        e.last_line()
                    )
                })?;
            }
            (None, None) => bail!("project '{}' has nowhere to fetch from", self.name),
        }
        // A bare clone maps branches onto refs/heads. Remote-tracking refs keep
        // origin/<base> meaning what it means in any other checkout.
        git(
            &self.mirror,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )?;
        Ok(())
    }

    fn checkout(&self) -> Result<Checkout> {
        let base = self.fetch()?;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = loop {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "spar-project-{}-{}-{serial}",
                self.name,
                std::process::id()
            ));
            if !path.exists() {
                break path;
            }
        };
        let path_str = path.display().to_string();
        let start = format!("refs/remotes/origin/{base}");
        {
            let _guard = self.adding.lock().unwrap_or_else(|e| e.into_inner());
            git(
                &self.mirror,
                &["worktree", "add", "--quiet", "--detach", &path_str, &start],
            )
            .map_err(|e| {
                spar_err!(
                    "project '{}': could not check out origin/{base}: {}",
                    self.name,
                    e.last_line()
                )
            })?;
        }
        // Canonical, so the path in the prompt is the one tools report back.
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let head = git(&path, &["rev-parse", "HEAD"])?.trim().to_string();
        Ok(Checkout {
            name: self.name.clone(),
            access: self.access,
            slug: self.slug.clone(),
            base,
            path,
            mirror: self.mirror.clone(),
            head,
        })
    }
}

/// Every configured project for one run.
pub struct Projects {
    list: Vec<Project>,
    keep: bool,
}

impl Projects {
    /// Resolve and check `[[projects]]` against the primary repository.
    pub fn resolve(cfgs: &[ProjectCfg], primary: &Path, keep_worktrees: bool) -> Result<Self> {
        let common = git(
            primary,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let store = PathBuf::from(common.trim()).join("spar-projects");
        let primary_top = toplevel(primary);
        let primary_slug = origin_url(primary).as_deref().and_then(github_slug);
        let mut list = Vec::new();
        for cfg in cfgs {
            let name = &cfg.name;
            let (path, slug) = match &cfg.source {
                ProjectSource::Path(raw) => {
                    let Some(top) = raw.is_dir().then(|| toplevel(raw)).flatten() else {
                        bail!(
                            "project '{name}': {} is not a git repository",
                            raw.display()
                        );
                    };
                    let slug = origin_url(&top).as_deref().and_then(github_slug);
                    let same_dir = primary_top.as_ref() == Some(&top);
                    let same_origin = slug.is_some() && slug == primary_slug;
                    if same_dir || same_origin {
                        bail!(
                            "project '{name}' is the repository spar is running in: list only \
                             other repositories under [[projects]]"
                        );
                    }
                    (Some(top), slug)
                }
                ProjectSource::Repo(slug) => {
                    if primary_slug
                        .as_deref()
                        .is_some_and(|own| own.eq_ignore_ascii_case(slug))
                    {
                        bail!(
                            "project '{name}' is the repository spar is running in: list only \
                             other repositories under [[projects]]"
                        );
                    }
                    (None, Some(slug.clone()))
                }
            };
            let dir: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            list.push(Project {
                name: name.clone(),
                access: cfg.access,
                slug,
                path,
                mirror: store.join(format!("{dir}.git")),
                base: OnceLock::new(),
                adding: Mutex::new(()),
            });
        }
        Ok(Self {
            list,
            keep: keep_worktrees,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Project> {
        self.list.iter()
    }

    /// A fresh checkout of every project, for one call.
    pub fn checkouts(&self) -> Result<Checkouts> {
        let mut made = Checkouts {
            list: Vec::new(),
            keep: self.keep,
        };
        for project in &self.list {
            // Dropping `made` on an error removes the ones already made.
            made.list.push(project.checkout()?);
        }
        Ok(made)
    }
}

/// One project's checkout for one call.
pub struct Checkout {
    pub name: String,
    pub access: ProjectAccess,
    pub slug: Option<String>,
    pub base: String,
    pub path: PathBuf,
    mirror: PathBuf,
    head: String,
}

impl Checkout {
    /// What changed since the checkout was made, or None.
    ///
    /// Ignored files are left out, as they are for a read-only call in the
    /// primary checkout: building or testing a project is reading it.
    fn changed(&self) -> Option<String> {
        let head = git_try(&self.path, &["rev-parse", "HEAD"]);
        if head.as_deref() != Some(self.head.as_str()) {
            return Some(format!("HEAD moved in {}", self.path.display()));
        }
        match git_try(
            &self.path,
            &["status", "--porcelain", "--untracked-files=all"],
        ) {
            Some(status) if status.is_empty() => None,
            Some(_) => Some(format!("files changed in {}", self.path.display())),
            None => Some(format!(
                "could not read the state of {}",
                self.path.display()
            )),
        }
    }
}

/// The checkouts one call was given. Removed when dropped.
pub struct Checkouts {
    list: Vec<Checkout>,
    keep: bool,
}

impl Checkouts {
    pub fn dirs(&self) -> Vec<String> {
        self.list
            .iter()
            .map(|c| c.path.display().to_string())
            .collect()
    }

    /// The prompt section naming each project. Empty when there are none.
    pub fn prompt(&self) -> String {
        if self.list.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "Other projects:\nYou can read these other repositories. Each is a fresh checkout of \
             its origin base branch made for this call. Do not change anything in them: a call \
             that does has its answer discarded. Changes to them are made through each \
             project's own issues and pull requests, never from here.\n",
        );
        for c in &self.list {
            let github = c.slug.as_deref().unwrap_or("not on GitHub");
            out.push_str(&format!(
                "- {}: {} (GitHub {github}, origin/{}, access {})\n",
                c.name,
                c.path.display(),
                c.base,
                c.access
            ));
        }
        out
    }

    /// The first change any checkout had, as a reason to discard the answer.
    pub fn changed(&self) -> Option<String> {
        self.list.iter().find_map(|c| {
            c.changed()
                .map(|why| format!("the call changed project '{}': {why}", c.name))
        })
    }
}

impl Drop for Checkouts {
    fn drop(&mut self) {
        for c in &self.list {
            if self.keep {
                logdim!(
                    "kept the checkout of project '{}' at {}",
                    c.name,
                    c.path.display()
                );
                continue;
            }
            if c.changed().is_some() {
                logwarn!(
                    "kept the checkout of project '{}' at {} because a call changed it",
                    c.name,
                    c.path.display()
                );
                continue;
            }
            let path = c.path.display().to_string();
            if let Err(e) = git(&c.mirror, &["worktree", "remove", "--force", &path]) {
                logdim!("could not remove {path}: {}", e.last_line());
            }
        }
    }
}
