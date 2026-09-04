//! The local follow-up queue, and working it.
//!
//! `.spar/followups.md` is what `followups = "local"` writes instead of filing
//! on the tracker. Until now nothing read it back, so a review's out of scope
//! findings accumulated in a file with no way to act on them. `spar followup`
//! reads it, asks one agent whether each entry is still true of the code as it
//! is now, files the survivors, and hands them to the same pipeline `spar run`
//! uses, which means both agents still triage each one before anything is
//! implemented.
//!
//! Most of the care here is in the parser, for one reason. An entry is written
//! as `## <title>`, and `review::issue_report` writes that entry's own sections
//! as `## Problem`, `## Reproduction`, `## Impact` and `## Expected behavior`,
//! at the same heading level. A real file had 25 `##` lines and 5 entries, so a
//! naive split would file twenty issues, four of them titled "Impact".

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::agent::Agent;
use crate::config::{Config, Followups};
use crate::error::Result;
use crate::model::{Finding, ScreenResponse, ScreenVerdict, Screened};
use crate::repo::{Repo, FOLLOWUP_MARKER};
use crate::{log, logwarn, schema, spar_err};

static BLANK_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("blank run pattern"));

// ---------------------------------------------------------------------------
// The file format
// ---------------------------------------------------------------------------

/// One entry as it sits in the note file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The `## ` heading text, marker excluded.
    pub title: String,
    /// Everything under the heading up to the next entry, trimmed. Keeps the
    /// report sections and the `Found while working on #N.` line.
    pub body: String,
    /// Byte range in the source covering the whole entry, marker included.
    ///
    /// The rewrite removes spans out of the original text rather than
    /// re-rendering what stays, which is what makes a hand edited file safe.
    pub span: Range<usize>,
}

/// The headings `review::issue_report` writes inside an entry.
///
/// Taken from `Finding::report_sections` rather than written out again. A fifth
/// section added there and forgotten here would split every entry that used it
/// in two, and the second half would be filed as its own issue.
fn report_headings() -> Vec<&'static str> {
    Finding {
        problem: Some("x".into()),
        reproduction: Some("x".into()),
        impact: Some("x".into()),
        expected: Some("x".into()),
        ..Finding::default()
    }
    .report_sections()
    .into_iter()
    .map(|(heading, _)| heading)
    .collect()
}

/// Whether a `## ` heading names a section of a bug report rather than an
/// entry's title.
///
/// Compares the whole heading, so `## Reproduction steps are missing` is a
/// title and not a section. The extra names are what a model writing
/// `new_issue_body` free hand produces, which the schema does not constrain.
fn is_section_heading(text: &str) -> bool {
    let got = text.trim().trim_end_matches(':').trim().to_lowercase();
    report_headings().iter().any(|h| h.to_lowercase() == got)
        || matches!(
            got.as_str(),
            "expected behaviour"
                | "expected"
                | "actual result"
                | "actual results"
                | "actual behavior"
                | "actual behaviour"
                | "steps to reproduce"
                | "summary"
        )
}

/// Every line with the byte offset it starts at.
fn lines_with_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut at = 0usize;
    text.split_inclusive('\n').map(move |line| {
        let start = at;
        at += line.len();
        (start, line.trim_end_matches(['\n', '\r']))
    })
}

/// Split a note file into entries.
///
/// Two rules, in order:
///
/// 1. A `<!-- spar:followup -->` line always starts an entry. spar writes one
///    above every entry it appends, so a file written from this release on is
///    read exactly rather than guessed at, and the first `## ` line after the
///    marker is that entry's title rather than a boundary.
/// 2. Otherwise a `## ` line starts an entry unless it names one of the
///    sections a bug report is written in, or there is nothing open for it to
///    be a section of.
///
/// Rule 2 is generous about what counts as a section, on purpose, because the
/// two ways to be wrong are not the same size. Mistaking a section for a title
/// splits one follow-up into four and files an issue called "Impact" carrying a
/// fragment: visible, embarrassing, and on somebody else's tracker. Mistaking a
/// title for a section merges two follow-ups into one issue that carries both:
/// fat, and recoverable by reading it. Nothing is lost.
///
/// A heading inside a fenced code block is never a boundary. A body is free to
/// carry a snippet, `style::issue_body` deliberately protects fenced blocks
/// from the length budget, and a snippet containing a `## ` line would
/// otherwise split the entry that quotes it.
///
/// Rejected as a third signal: "an entry follows a `Found while working on #N.`
/// line". Provenance is not guaranteed, a hand written entry has none, and a
/// parser that leans on prose is one an edit breaks.
pub fn parse(text: &str) -> Vec<Entry> {
    // Entry start, and where its `## ` title line starts if it has one.
    let mut opens: Vec<(usize, Option<usize>)> = Vec::new();
    let mut open = false;
    let mut awaiting_title = false;
    let mut fenced = false;

    for (offset, line) in lines_with_offsets(text) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if trimmed.starts_with(FOLLOWUP_MARKER) {
            opens.push((offset, None));
            open = true;
            awaiting_title = true;
            continue;
        }
        let Some(heading) = trimmed.strip_prefix("## ") else {
            continue;
        };
        if awaiting_title {
            if let Some(last) = opens.last_mut() {
                last.1 = Some(offset);
            }
            awaiting_title = false;
            continue;
        }
        // Nothing is open, so this cannot be a section of anything: a file
        // whose first heading is `## Problem` holds one entry called Problem.
        if open && is_section_heading(heading) {
            continue;
        }
        opens.push((offset, Some(offset)));
        open = true;
    }

    let mut out = Vec::with_capacity(opens.len());
    for (i, (start, title_at)) in opens.iter().enumerate() {
        let end = opens.get(i + 1).map(|(s, _)| *s).unwrap_or(text.len());
        let (title, body_from) = match title_at {
            Some(at) => {
                let line_end = text[*at..end].find('\n').map(|n| at + n + 1).unwrap_or(end);
                let heading = text[*at..line_end]
                    .trim()
                    .trim_start_matches("## ")
                    .trim()
                    .to_string();
                (heading, line_end)
            }
            // A marker with no heading under it. Keep the entry rather than
            // dropping it: the body is still somebody's finding.
            None => (String::new(), *start),
        };
        out.push(Entry {
            title,
            body: text[body_from..end].trim().to_string(),
            span: *start..end,
        });
    }
    out
}

/// The text with these entries removed, blank lines closed up at the seams.
///
/// Removal is by byte span out of the text as it was parsed, never by
/// re-rendering what stays. Anything in the file spar did not write, a preamble,
/// an entry in a shape spar does not recognise, an edit somebody made to a body,
/// comes back out exactly as it went in.
///
/// Spans are sorted and merged first, so a caller may pass them in any order and
/// may pass the same one twice. That is what makes it correct to rewrite the
/// file once per entry against the *original* text: splicing a splice would
/// shift every later offset.
pub fn without(text: &str, removed: &[Entry]) -> String {
    let mut spans: Vec<Range<usize>> = removed.iter().map(|e| e.span.clone()).collect();
    spans.sort_by_key(|s| s.start);

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for span in spans {
        // Already covered by an earlier span. Skipping rather than slicing
        // backwards, because a panic here would take the queue with it.
        if span.start < cursor {
            cursor = cursor.max(span.end);
            continue;
        }
        out.push_str(&text[cursor..span.start]);
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);

    let joined = BLANK_RUN.replace_all(out.trim_end(), "\n\n").to_string();
    if joined.trim().is_empty() {
        String::new()
    } else {
        format!("{joined}\n")
    }
}

// ---------------------------------------------------------------------------
// Screening
// ---------------------------------------------------------------------------

const SCREEN_PROMPT: &str = "\
Below are follow-ups recorded against this repository while other work was going
on. Each was a real finding when it was written. Time has passed and the code has
moved: some are already fixed, some describe behaviour that no longer exists, and
some were never worth the interruption.

Read the code in your working directory before judging each one. Do not modify
anything. The current checkout is what \"now\" means. Judge against it, not
against what the entry says the code used to do.

For each entry decide:
- verdict: still_relevant, already_fixed, not_worth_it, or duplicate.
  - still_relevant: the defect is still there. It becomes a GitHub issue.
  - already_fixed: go and look. Name the function or the change that fixed it,
    so somebody reading this can check you.
  - not_worth_it: real, still there, and not worth a maintainer's queue.
  - duplicate: something else already covers it. An open issue goes in
    duplicate_of_issue, another entry in this list goes in duplicate_of_entry
    by its number here. They are separate fields because an entry is not an
    issue: an entry you point at may itself be dropped, and this one is then
    kept rather than lost with it.
- reason: one sentence. For anything but still_relevant this is the only record
  of why the entry was dropped, so give the reason rather than the verdict
  restated.
- title: the entry's title, which becomes the issue title. Copy it across unless
  it is wrong or says nothing.

Say still_relevant when you are unsure. What survives is triaged by both agents
afterwards and can still be declined there. What you drop here is dropped.

Entries:
";

/// The queue as the prompt carries it, and what would not fit.
struct Rendered {
    text: String,
    /// Left in the file for a later run, because the queue did not fit in one
    /// prompt.
    deferred: usize,
}

/// Render the queue under one budget.
///
/// The same shape as `triage::render` and for the same reason, which is worth
/// saying rather than sharing: whole entries wait rather than every entry losing
/// its tail, because a verdict here *deletes* the entry, so judging one on part
/// of what it says is worse than not having reached it yet.
///
/// Unlike an issue body there is no per entry cut at all. Every entry already
/// went through `style::issue_body` on the way in, and unlike an issue the entry
/// is all there is.
fn render(entries: &[Entry], cfg: &Config) -> Rendered {
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut deferred = 0usize;

    for (i, entry) in entries.iter().enumerate() {
        if deferred > 0 {
            deferred += 1;
            continue;
        }
        let block = format!("{}. {}\n{}", i + 1, entry.title, entry.body);
        let len = block.chars().count();
        // The first entry goes in whatever its size. A queue of one that does
        // not fit is a command that does nothing, forever.
        if !parts.is_empty() && total + len > cfg.loop_cfg.max_triage_chars {
            deferred += 1;
            continue;
        }
        total += len;
        parts.push(block);
    }

    Rendered {
        text: parts.join("\n\n"),
        deferred,
    }
}

/// One agent's verdict on every entry that fits in one prompt.
///
/// One call for the whole queue rather than one per entry. A repo aware call per
/// entry is the dominant cost, and `duplicate` is a judgement across entries as
/// well as against the tracker: an agent shown one entry at a time cannot say
/// that this is the same defect as the one above it.
pub fn screen(
    agent: &Agent,
    cfg: &Config,
    repo: &Repo,
    entries: &[Entry],
) -> Result<Vec<ScreenVerdict>> {
    let rendered = render(entries, cfg);
    if rendered.deferred > 0 {
        logwarn!(
            "the queue did not fit in one screening prompt, so {} entry(s) were left in the file \
             for a later run",
            rendered.deferred
        );
    }
    let prompt = format!("{SCREEN_PROMPT}{}", rendered.text);
    let effort = cfg.effort_for_round(&agent.spec, 1);
    let answer: ScreenResponse =
        agent.ask_json(&prompt, &schema::screen(), repo.root(), effort.as_deref())?;
    Ok(answer.entries)
}

// ---------------------------------------------------------------------------
// Working the queue
// ---------------------------------------------------------------------------

/// Where a run of `spar followup` stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Print the verdicts and touch nothing.
    ScreenOnly,
    /// File the issues and stop, leaving them for a later `spar run`.
    FileOnly,
    /// File them and work them.
    Work,
}

/// What was filed, and what was left behind.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Issues to work, in file order.
    pub issues: Vec<i64>,
    /// Left in the file: not reached, not screened, or could not be filed.
    pub held: usize,
}

/// Read the queue, screen it, file what still holds, and take the filed entries
/// out of the file.
pub fn run(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    path: &Path,
    limit: usize,
    mode: Mode,
) -> Result<Outcome> {
    let mut outcome = Outcome::default();

    // Before any network call, so an empty queue is a purely local no-op.
    let Ok(original) = std::fs::read_to_string(path) else {
        log!("no follow-ups recorded in {}", path.display());
        if repo.followups != Followups::Local {
            log!(
                "followups = \"{}\" is configured, so nothing is written to that file.",
                repo.followups
            );
        }
        return Ok(outcome);
    };
    if original.trim().is_empty() {
        log!("{} is there and empty", path.display());
        return Ok(outcome);
    }

    let entries = parse(&original);
    if entries.is_empty() {
        // A parser problem must not be reported as an empty queue.
        log!(
            "{} has no `## ` headings, so there is nothing to work. An entry is a `## Title` line \
             and the text under it.",
            path.display()
        );
        return Ok(outcome);
    }

    let taken: Vec<Entry> = entries.iter().take(limit).cloned().collect();
    outcome.held += entries.len() - taken.len();
    if outcome.held > 0 {
        log!(
            "{} follow-up(s) recorded, taking the first {limit}. Raise --limit for the rest.",
            entries.len()
        );
    }

    let agent = crate::agent::find(agents, &cfg.first_implementor)?;
    // An `already_fixed` verdict is uninterpretable without knowing what was
    // being judged.
    log!(
        "screening {} follow-up(s) with {} against {} at {}",
        taken.len(),
        agent.name(),
        repo.git_try(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        repo.git_try(&["rev-parse", "--short", "HEAD"]).trim(),
    );

    // A screen that did not happen is not a screen that found nothing: nothing
    // is filed and the file is not touched.
    let verdicts = screen(agent, cfg, repo, &taken)?;

    if mode == Mode::ScreenOnly {
        print_verdicts(&taken, &verdicts);
        return Ok(outcome);
    }

    let mut disposed: Vec<Entry> = Vec::new();
    // What each entry became, so a sibling that says it duplicates one can be
    // answered after the batch rather than guessed at during it. An entry
    // pointing at one that was itself dropped, or failed to file, is kept:
    // archiving it as covered by an issue number that is really an entry
    // number sends a reader to an unrelated issue and loses both copies.
    let mut filed_as: BTreeMap<i64, i64> = BTreeMap::new();
    let mut deferred: Vec<usize> = Vec::new();

    for pass in 0..2 {
        for (i, entry) in taken.iter().enumerate() {
            let number = i as i64 + 1;
            if pass == 0 && disposed.iter().any(|d| d.title == entry.title) {
                continue;
            }
            if pass == 1 && !deferred.contains(&i) {
                continue;
            }
            let Some(verdict) = verdicts.iter().find(|v| v.entry == number) else {
                // Never read as "drop it". Filing something nobody looked at
                // defeats the point of the screen; leaving it costs one line.
                logwarn!(
                    "no verdict for '{}', leaving it in the file",
                    first_line(&entry.title)
                );
                outcome.held += 1;
                continue;
            };

            // An entry that says it duplicates a sibling waits for the sibling to
            // be dealt with, because what happened to that one decides this one.
            if pass == 0
                && verdict.verdict == Screened::Duplicate
                && verdict.duplicate_of_entry.is_some()
            {
                deferred.push(i);
                continue;
            }
            let covered_by = verdict
                .duplicate_of_entry
                .and_then(|target| filed_as.get(&target).copied())
                .or(verdict.duplicate_of_issue);
            if verdict.verdict == Screened::Duplicate
                && verdict.duplicate_of_entry.is_some()
                && covered_by.is_none()
            {
                logwarn!(
                    "'{}' was called a duplicate of entry {}, which was not filed, so it stays in \
                 the file",
                    first_line(&entry.title),
                    verdict.duplicate_of_entry.unwrap_or_default()
                );
                outcome.held += 1;
                continue;
            }

            // "Duplicate of nothing in particular" is unfalsifiable, and
            // `find_similar_issue` on the filing path is exactly the check for it.
            let files = verdict.verdict == Screened::StillRelevant
                || (verdict.verdict == Screened::Duplicate && !verdict.names_a_duplicate());

            if files {
                let retitled = !verdict.title.trim().is_empty()
                    && !verdict
                        .title
                        .trim()
                        .eq_ignore_ascii_case(entry.title.trim());
                let title = if verdict.title.trim().is_empty() {
                    entry.title.as_str()
                } else {
                    verdict.title.as_str()
                };
                match crate::review::file_as_issue(repo, title, &entry.body) {
                    Ok(filed) => {
                        log!("  {}", filed.describe(title));
                        if let Some(n) = filed.number() {
                            outcome.issues.push(n);
                            filed_as.insert(number, n);
                        }
                        // Archived under the title it was written with, so the next
                        // run that rediscovers the same defect in the original
                        // wording finds it here rather than appending it again. The
                        // new title goes in the verdict, and gets a heading of its
                        // own, so either wording is recognised.
                        repo.archive_followup(
                            &entry.title,
                            &entry.body,
                            &format!(
                                "Filed: {}{}",
                                filed.note(),
                                if retitled {
                                    format!(", retitled to '{}'", title.trim())
                                } else {
                                    String::new()
                                }
                            ),
                        );
                        if retitled {
                            repo.archive_followup(
                                title,
                                &format!("The same entry as '{}'.", entry.title.trim()),
                                &format!("Filed: {}", filed.note()),
                            );
                        }
                    }
                    Err(e) => {
                        logwarn!("could not file '{}': {e}", first_line(title));
                        outcome.held += 1;
                        continue;
                    }
                }
            } else {
                let why = dropped_note(verdict, covered_by);
                log!("  dropped '{}': {why}", first_line(&entry.title));
                repo.archive_followup(&entry.title, &entry.body, &format!("Dropped: {why}"));
            }

            disposed.push(entry.clone());
            // Written after the entry is dealt with, never before. The window is
            // one entry wide and it always falls on the side of filing twice rather
            // than losing one: an entry filed and not yet removed is found again
            // next run and matched to the issue that was just created, while an
            // entry removed before it was filed is gone.
            crate::repo::write_text_atomic(path, &without(&original, &disposed)).map_err(|e| {
                spar_err!(
                "{e}\n{} follow-up(s) were already dealt with. Remove them from {} by hand before \
                 running this again, or they will be filed twice.",
                disposed.len(),
                path.display()
            )
            })?;
        }
    }

    let filed = outcome.issues.len();
    println!(
        "\nfollowups: {} screened, {filed} filed{}",
        taken.len(),
        summarise(&taken, &verdicts)
    );
    if outcome.held > 0 {
        println!("{} entry(s) left in {}", outcome.held, path.display());
    }
    if !disposed.is_empty() {
        println!(
            "what was dealt with is in {}",
            repo.worked_followups_path().display()
        );
    }
    Ok(outcome)
}

fn first_line(text: &str) -> String {
    crate::style::clip(text.trim().lines().next().unwrap_or("").trim(), 80)
}

fn dropped_note(v: &ScreenVerdict, covered_by: Option<i64>) -> String {
    let reason = v.reason.trim();
    match (v.verdict, covered_by) {
        (Screened::Duplicate, Some(n)) if reason.is_empty() => format!("#{n} already covers it"),
        (Screened::Duplicate, Some(n)) => format!("#{n} already covers it. {reason}"),
        (_, _) if reason.is_empty() => v.verdict.to_string(),
        _ => format!("{}. {reason}", v.verdict),
    }
}

/// The tail of the summary line, naming each verdict that dropped something.
fn summarise(taken: &[Entry], verdicts: &[ScreenVerdict]) -> String {
    let mut counts: Vec<(Screened, usize)> = Vec::new();
    for v in verdicts {
        if v.entry < 1 || v.entry as usize > taken.len() {
            continue;
        }
        match counts.iter_mut().find(|(k, _)| *k == v.verdict) {
            Some((_, n)) => *n += 1,
            None => counts.push((v.verdict, 1)),
        }
    }
    counts.retain(|(k, _)| *k != Screened::StillRelevant);
    if counts.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = counts
        .iter()
        .map(|(k, n)| format!("{n} {}", k.as_str().replace('_', " ")))
        .collect();
    format!(", {}", listed.join(", "))
}

/// What `--screen-only` prints. `println!`, not `log!`: it is the whole output
/// of the command, and an entry dropped with no visible record of why is
/// exactly the failure the archive exists for.
fn print_verdicts(taken: &[Entry], verdicts: &[ScreenVerdict]) {
    println!();
    for (i, entry) in taken.iter().enumerate() {
        let number = i as i64 + 1;
        match verdicts.iter().find(|v| v.entry == number) {
            Some(v) => println!(
                "  {:<14} {}\n                 {}",
                v.verdict.as_str(),
                first_line(&entry.title),
                v.reason.trim()
            ),
            None => println!("  {:<14} {}", "no verdict", first_line(&entry.title)),
        }
    }
    let filed = verdicts
        .iter()
        .filter(|v| v.verdict == Screened::StillRelevant)
        .count();
    println!(
        "\n{filed} of {} would be filed. Nothing was written.",
        taken.len()
    );
}

/// The issue numbers a set of outcomes produced, deduplicated and in order.
pub fn wave(outcome: &Outcome) -> Vec<i64> {
    outcome
        .issues
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two entries in the shape a real file has: a title, a lead paragraph,
    /// the four report sections, and the provenance line.
    const REAL: &str = "\
## Backend headers never drive commitment CPFP retries

The production ChainWatcher advances monitors and emits block.

## Problem

Configured chain backends route accepted headers through handleNewBlock.

## Reproduction

1. Configure a node with a watcher backend.
2. Deliver height 101.

## Impact

Nodes do not retry stuck commitment packages on new blocks.

## Expected behavior

Run the pass exactly once for each accepted backend header.

Found while working on #589.

## Overlapping scans can move a recorded spend height backward

## Problem

checkOutputSpend applies its result with no arbitration against a later scan.

## Impact

A stale verdict can overwrite a newer one.

Found while working on #590.
";

    /// The whole reason this module exists. A follow-up's own sections are
    /// written at the same heading level as its title, so a naive split on
    /// `## ` files six issues here, four of them called things like "Impact".
    #[test]
    fn an_entry_and_its_sections_are_not_confused_for_each_other() {
        let entries = parse(REAL);
        assert_eq!(
            2,
            entries.len(),
            "{:#?}",
            entries.iter().map(|e| &e.title).collect::<Vec<_>>()
        );
        assert!(entries[0].title.starts_with("Backend headers"));
        assert!(entries[1].title.starts_with("Overlapping scans"));
        // The sections stay with the entry they belong to.
        assert!(
            entries[0].body.contains("## Reproduction"),
            "{}",
            entries[0].body
        );
        assert!(entries[0].body.contains("Found while working on #589."));
    }

    /// The heading heuristic alone cannot see a title that collides with a
    /// section name. The marker is what fixes that going forward, and spar
    /// writes one above every entry it appends.
    #[test]
    fn a_marker_makes_the_boundary_exact() {
        let text = format!(
            "{FOLLOWUP_MARKER}\n## Impact\n\nThe first one.\n\n\
             {FOLLOWUP_MARKER}\n## Problem\n\nThe second one.\n"
        );
        let entries = parse(&text);
        assert_eq!(2, entries.len());
        assert_eq!("Impact", entries[0].title);
        assert_eq!("Problem", entries[1].title);
    }

    /// Keeping the queue by hand is a documented use, and it must not require
    /// knowing spar's marker.
    #[test]
    fn a_hand_written_file_with_no_markers_still_parses() {
        let text =
            "## One thing\n\nprose\n\n## Another thing\n\nmore prose\n\n## A third\n\nyet more\n";
        let entries = parse(text);
        assert_eq!(3, entries.len());
        assert_eq!("Another thing", entries[1].title);
    }

    /// A body is free to carry a snippet, and `style::issue_body` deliberately
    /// protects fenced blocks from the length budget, so a snippet containing a
    /// `## ` line would otherwise split the entry that quotes it.
    #[test]
    fn a_heading_inside_a_fenced_block_does_not_start_an_entry() {
        let text = "## Real title\n\n```md\n## Problem\n## Not a title either\n```\n\nprose\n";
        let entries = parse(text);
        assert_eq!(1, entries.len(), "{:?}", entries);
        assert_eq!("Real title", entries[0].title);
    }

    /// `is_section_heading` compares the whole heading. A prefix match would
    /// swallow an entry whose title happens to open with a section word.
    #[test]
    fn an_entry_whose_title_opens_with_a_section_word_is_still_a_title() {
        let text =
            "## First\n\nprose\n\n## Reproduction steps are missing from the docs\n\nprose\n";
        assert_eq!(2, parse(text).len());
    }

    /// A fifth section added to a bug report and forgotten here would split
    /// every entry that used it, and the second half would be filed on its own.
    #[test]
    fn the_section_list_covers_every_heading_a_report_writes() {
        for heading in report_headings() {
            assert!(
                is_section_heading(heading),
                "`## {heading}` would be read as the start of a new follow-up"
            );
        }
    }

    /// A rewrite that re-renders what stays silently deletes whatever a person
    /// added: a note at the top, an edit inside a body, a trailing reminder.
    #[test]
    fn text_the_parser_does_not_own_survives_a_rewrite() {
        let text = "A note I keep at the top.\n\n\
                    ## One\n\nfirst\n\n\
                    ## Two\n\nsecond\n\n\
                    ## Three\n\nthird\n";
        let entries = parse(text);
        assert_eq!(3, entries.len());
        let out = without(text, &[entries[1].clone()]);
        assert!(out.starts_with("A note I keep at the top."), "{out}");
        assert!(out.contains("## One"), "{out}");
        assert!(!out.contains("## Two"), "{out}");
        assert!(out.contains("## Three"), "{out}");
        assert!(out.contains("third"), "{out}");
    }

    /// The guard against splicing a splice. Spans index the text as it was
    /// parsed, so removing one entry would shift every later offset, and the
    /// run rewrites the file once per entry.
    #[test]
    fn removing_entries_one_at_a_time_matches_removing_them_at_once() {
        let entries = parse(REAL);
        let all_at_once = without(REAL, &entries);

        let mut done = Vec::new();
        let mut last = String::new();
        for entry in &entries {
            done.push(entry.clone());
            last = without(REAL, &done);
        }
        assert_eq!(all_at_once, last);
        assert!(last.is_empty(), "{last:?}");
    }

    /// A caller that recorded the same entry twice, or recorded them out of
    /// order, must not corrupt the file or panic on a backwards slice.
    #[test]
    fn without_tolerates_a_repeated_or_unordered_span() {
        let entries = parse(REAL);
        let once = without(REAL, &[entries[0].clone()]);
        let twice = without(REAL, &[entries[0].clone(), entries[0].clone()]);
        assert_eq!(once, twice);

        let forwards = without(REAL, &[entries[0].clone(), entries[1].clone()]);
        let backwards = without(REAL, &[entries[1].clone(), entries[0].clone()]);
        assert_eq!(forwards, backwards);
    }

    /// An emptied queue is an empty file, not a pile of blank lines that reads
    /// as content to the next thing that opens it.
    #[test]
    fn removing_every_entry_leaves_an_empty_file() {
        let entries = parse(REAL);
        assert_eq!("", without(REAL, &entries));
    }

    /// The link back to the work that found a defect cannot be re-derived, so
    /// the body has to carry it through unchanged.
    #[test]
    fn an_entry_keeps_the_provenance_it_was_written_with() {
        let entries = parse(REAL);
        assert!(entries[1].body.ends_with("Found while working on #590."));
    }

    /// A file edited on Windows must parse the same as one edited anywhere else.
    #[test]
    fn crlf_line_endings_parse_the_same_as_lf() {
        let lf = "## One\n\nfirst\n\n## Two\n\nsecond\n";
        let crlf = lf.replace('\n', "\r\n");
        let a = parse(lf);
        let b = parse(&crlf);
        assert_eq!(a.len(), b.len());
        assert_eq!(a[1].title, b[1].title);
    }

    /// A file whose first heading is a section name holds one entry called
    /// that, rather than nothing at all.
    #[test]
    fn a_file_that_opens_with_a_section_name_still_holds_an_entry() {
        let entries = parse("## Problem\n\nsomething is wrong\n");
        assert_eq!(1, entries.len());
        assert_eq!("Problem", entries[0].title);
    }

    fn verdict(entry: i64, v: Screened, dup: Option<i64>) -> ScreenVerdict {
        ScreenVerdict {
            entry,
            verdict: v,
            title: String::new(),
            reason: "because".into(),
            duplicate_of_issue: dup,
            duplicate_of_entry: None,
        }
    }

    fn duplicate_of_entry(entry: i64, target: i64) -> ScreenVerdict {
        ScreenVerdict {
            entry,
            verdict: Screened::Duplicate,
            title: String::new(),
            reason: "same as the one above".into(),
            duplicate_of_issue: None,
            duplicate_of_entry: Some(target),
        }
    }

    /// "Duplicate of nothing in particular" is unfalsifiable, and the search on
    /// the filing path is exactly the check for it. Dropping the entry on that
    /// verdict would delete a real defect on no evidence.
    #[test]
    fn a_duplicate_verdict_with_nothing_to_point_at_would_still_be_filed() {
        let with_number = verdict(1, Screened::Duplicate, Some(412));
        let without_number = verdict(1, Screened::Duplicate, None);
        let files = |v: &ScreenVerdict| {
            v.verdict == Screened::StillRelevant
                || (v.verdict == Screened::Duplicate && !v.names_a_duplicate())
        };
        assert!(!files(&with_number));
        assert!(files(&without_number));
    }

    /// A short answer must never read as "drop the rest". The entries the
    /// screen did not rule on stay in the file.
    #[test]
    fn an_entry_with_no_verdict_is_not_disposed_of() {
        let entries = parse(REAL);
        let verdicts = [verdict(1, Screened::AlreadyFixed, None)];
        let unruled: Vec<usize> = (1..=entries.len())
            .filter(|n| !verdicts.iter().any(|v| v.entry == *n as i64))
            .collect();
        assert_eq!(vec![2], unruled);
    }

    /// An out of range index used as a slice index panics, and a verdict for an
    /// entry that does not exist must not shift the ones that do.
    #[test]
    fn a_verdict_naming_an_entry_that_does_not_exist_is_ignored() {
        let entries = parse(REAL);
        let verdicts = [verdict(9, Screened::AlreadyFixed, None)];
        assert_eq!("", summarise(&entries, &verdicts));
    }

    /// The summary is the only place a dropped entry is accounted for, and it
    /// has to name what happened rather than only how many.
    #[test]
    fn the_summary_names_each_verdict_that_dropped_something() {
        let entries = parse(REAL);
        let verdicts = vec![
            verdict(1, Screened::AlreadyFixed, None),
            verdict(2, Screened::StillRelevant, None),
        ];
        let out = summarise(&entries, &verdicts);
        assert!(out.contains("1 already fixed"), "{out}");
        assert!(!out.contains("still relevant"), "{out}");
    }

    /// The reason is the only record of why an entry left the queue, so it has
    /// to survive into the log line and the archive.
    #[test]
    fn a_dropped_entry_carries_its_reason_and_the_issue_it_duplicates() {
        let v = verdict(1, Screened::Duplicate, Some(412));
        let note = dropped_note(&v, v.duplicate_of_issue);
        assert!(note.contains("#412"), "{note}");
        assert!(note.contains("because"), "{note}");
    }

    /// An entry number is not an issue number, and one field could not say
    /// which it was.
    ///
    /// Entry 4 duplicating entry 2 was archived as covered by issue #2, an
    /// unrelated issue, and if entry 2 was then ruled already fixed or failed
    /// to file, neither copy was filed at all.
    #[test]
    fn a_duplicate_of_a_sibling_is_kept_apart_from_a_duplicate_of_an_issue() {
        let sibling = duplicate_of_entry(4, 2);
        assert!(sibling.names_a_duplicate(), "it points at something");
        assert_eq!(None, sibling.duplicate_of_issue);

        // Unresolved, it names nothing to send a reader to.
        let unresolved = dropped_note(&sibling, None);
        assert!(!unresolved.contains("#2"), "{unresolved}");

        // Resolved after the batch, it names the issue that entry became.
        let resolved = dropped_note(&sibling, Some(512));
        assert!(resolved.contains("#512"), "{resolved}");
    }
}

#[cfg(test)]
mod real_file {
    use super::*;

    /// The queue one real run left on a real repository, captured verbatim.
    /// Five follow-ups, twenty-five `## ` lines. This is the file the heuristic
    /// is measured against rather than guessed at.
    const CORPUS: &str = include_str!("../tests/fixtures/local_followups.md");

    #[test]
    fn the_real_queue_parses_as_five_follow_ups_not_twenty_five() {
        let entries = parse(CORPUS);
        assert_eq!(
            5,
            entries.len(),
            "{:#?}",
            entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>()
        );
        for entry in &entries {
            assert!(
                !is_section_heading(&entry.title),
                "a section was filed as a follow-up: {}",
                entry.title
            );
            assert!(!entry.body.trim().is_empty(), "{} has no body", entry.title);
        }
    }

    /// Every entry in that file ends with the line that says which issue it
    /// came out of, and an issue filed from it is the only place that link
    /// survives.
    #[test]
    fn every_entry_in_the_real_queue_keeps_its_provenance() {
        for entry in parse(CORPUS) {
            assert!(
                entry.body.contains("Found while working on #"),
                "{} lost its provenance",
                entry.title
            );
        }
    }

    /// Removing them one at a time, always against the original text, has to
    /// end where removing them all at once does, and has to end empty.
    #[test]
    fn the_real_queue_drains_to_nothing_one_entry_at_a_time() {
        let entries = parse(CORPUS);
        let mut done = Vec::new();
        let mut text = CORPUS.to_string();
        for entry in &entries {
            done.push(entry.clone());
            text = without(CORPUS, &done);
        }
        assert_eq!("", text);
        assert_eq!(without(CORPUS, &entries), text);
    }

    /// Taking the middle one out leaves the other four intact, including the
    /// one a person would notice first if the seams were wrong.
    #[test]
    fn draining_one_entry_leaves_the_rest_byte_for_byte() {
        let entries = parse(CORPUS);
        let out = without(CORPUS, &[entries[2].clone()]);
        let left = parse(&out);
        assert_eq!(4, left.len());
        for (before, after) in [(0, 0), (1, 1), (3, 2), (4, 3)] {
            assert_eq!(entries[before].title, left[after].title);
            assert_eq!(entries[before].body, left[after].body);
        }
    }
}
