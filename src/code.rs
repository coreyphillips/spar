//! Turn a project request into an issue before handing it to the normal run.

use serde::Deserialize;

use crate::agent::{self, Agent};
use crate::config::{Call, Config};
use crate::error::Result;
use crate::repo::Repo;
use crate::{bail, log, schema};

const PROMPT: &str = "\
Draft one detailed GitHub issue for the project in your working directory,
using the request below. Read the README, relevant source, tests, and project
instructions first so the issue fits this repository and its conventions.

Preserve the user's intent and constraints. Describe the problem or motivation,
current behavior, requested behavior, scope, concrete acceptance criteria, and
a test plan. Include relevant file paths and implementation guidance when the
code supports them. For a bug, include supplied or verified reproduction steps
and expected versus actual behavior. Do not invent reproductions, observed
results, existing APIs, or requirements. Label assumptions and open questions
explicitly. Keep the issue focused on the request, without unrelated work.
Use Markdown headings and checkboxes for acceptance criteria.

This is issue drafting only. Do not implement the request, modify files,
commit, push, or create issues, comments, or pull requests. Spar will file the
returned title and body and run its normal triage and implementation workflow.
If you need scratch space, use the system temporary directory.

User request:
";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub title: String,
    pub body: String,
}

pub fn validate_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("spar code needs a nonempty project prompt");
    }
    Ok(())
}

/// The read-only call carries the configured instructions and uses the same
/// style gates as issue creation, so the preview shows what will be filed.
pub fn draft(agents: &[Agent], cfg: &Config, repo: &Repo, request: &str) -> Result<Draft> {
    validate_prompt(request)?;
    let agent = agent::find(agents, &cfg.first_implementor)?;
    log!("drafting an issue from the project prompt");
    let answer: Draft = agent.ask_json(
        &format!("{PROMPT}{}", request.trim()),
        &schema::code_issue(),
        repo.root(),
        &agent.effort(cfg, Call::Code),
    )?;
    let title = repo.clean_title(&answer.title)?;
    let body = repo.clean_issue_body(&answer.body)?;
    if title.trim().is_empty() || body.trim().is_empty() {
        bail!("issue draft must have a nonempty title and body after style cleanup");
    }
    Ok(Draft { title, body })
}
