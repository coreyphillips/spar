//! A tracking issue's checklist, read as work.
//!
//! Triage already declines a tracker and holds it open, which is right and
//! leaves it standing still: every run judges it again, comments again, and
//! none of the work written down in it moves. This reads the `- [ ]` lines,
//! gives each one an issue, and writes the number back beside the item so the
//! body itself is the record. No state file: the checkbox is the state, where a
//! person can read it and correct it by hand.
//!
//! The trigger is the checklist and never a judgement about what the parts
//! might be. A tracker with no task list is commented on and held exactly as
//! before. Writing `- [ ]` lines is something somebody does on purpose, which
//! makes this opt in per issue as well as per repository.
//!
//! Everything else spar writes is additive: a comment, a new issue, a commit on
//! its own branch. This rewrites text a person wrote, in place, in the issue
//! most likely to be the shared plan for a piece of work. So the surgery is
//! line local, every other line is proved byte identical before the write, the
//! body is re-read immediately before each one, and `spar triage` prints the
//! whole thing without writing any of it.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::Config;
use crate::error::Result;
use crate::model::ItemKind;
use crate::repo::Repo;
use crate::review;
use crate::style;
use crate::{bail, log, logdim, logwarn, spar_err};

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A markdown task list item. Indentation and the marker are captured rather
/// than normalised, because nothing here rebuilds a line it did not have to.
static ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<indent>[ \t]*)(?:[-*+]|[0-9]{1,9}[.)])[ \t]+\[(?P<state>[ xX])\](?P<rest>[ \t].*|)$",
    )
    .expect("task item pattern")
});

/// A fence, opening or closing. Info string included so that only a bare fence
/// can close one.
static FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*(?P<fence>`{3,}|~{3,})(?P<info>.*)$").expect("fence pattern")
});

/// `#123`, at the start of the text or after something that is not a word.
static HASH_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^\w])#(?P<number>[0-9]{1,9})\b").expect("hash pattern"));

/// A link to an issue, on any host. Which repository it belongs to is decided
/// later, against this one's name.
static URL_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<url>https?://[^\s)\]]*?/issues/(?P<number>[0-9]{1,9}))\b")
        .expect("url pattern")
});

/// An issue an item's text already names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub number: i64,
    /// Set when the reference was written as a link. A bare `#123` can only
    /// mean this repository; a link can name anybody's.
    pub url: Option<String>,
}

impl Reference {
    /// The issue number, when the reference is one this repository can act on.
    ///
    /// A link to another repository's issue is not adoptable: taking the number
    /// out of it would point the item at whatever happens to carry that number
    /// here, which is the wrong link failure with no fuzziness to blame.
    pub fn local(&self, slug: &str) -> Option<i64> {
        match &self.url {
            None => Some(self.number),
            Some(url) => {
                let owned = !slug.trim().is_empty() && url.contains(&format!("/{slug}/issues/"));
                owned.then_some(self.number)
            }
        }
    }
}

/// One task list item, as it stands in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 1-based, so a log line names something a person can go and look at.
    pub line: usize,
    /// The line exactly as it stands, without its terminator. This is the
    /// handle: the line is found again by content on re-read, never by index,
    /// because an edit that lands mid-run moves indexes and does not move text.
    pub raw: String,
    /// The item's own text, after the checkbox.
    pub text: String,
    pub checked: bool,
    pub reference: Option<Reference>,
}

/// Every task list item in the body, in order.
///
/// Deliberately dull about what it will not treat as an item: anything inside a
/// fenced code block, and anything that is not a task list line. Nested and
/// indented items are ordinary items, since every edit here is line local.
pub fn parse(body: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for (index, raw) in split_keep(body).into_iter().enumerate() {
        let line = without_eol(raw);
        if let Some(caps) = FENCE.captures(line) {
            let marker = &caps["fence"];
            let (glyph, len) = (marker.chars().next().expect("a fence"), marker.len());
            fence = match fence {
                None => Some((glyph, len)),
                // Only the same glyph, at least as long, with nothing after it.
                Some((open, open_len))
                    if glyph == open && len >= open_len && caps["info"].trim().is_empty() =>
                {
                    None
                }
                open => open,
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let Some(caps) = ITEM.captures(line) else {
            continue;
        };
        let text = caps["rest"].trim().to_string();
        out.push(Item {
            line: index + 1,
            raw: line.to_string(),
            reference: reference_in(&text),
            text,
            checked: &caps["state"] != " ",
        });
    }
    out
}

/// The first issue this text names, by link or by number.
fn reference_in(text: &str) -> Option<Reference> {
    let url = URL_REF.captures(text);
    let hash = HASH_REF.captures(text);
    let at = |caps: &Option<regex::Captures>| {
        caps.as_ref()
            .map(|c| c.get(0).expect("the whole match").start())
    };
    // Whichever comes first, so a link is not shadowed by a `#` later in the
    // same line.
    match (at(&url), at(&hash)) {
        (Some(u), Some(h)) if h < u => hash.map(as_hash),
        (Some(_), _) => url.map(as_url),
        (None, Some(_)) => hash.map(as_hash),
        (None, None) => None,
    }
}

fn as_hash(caps: regex::Captures) -> Reference {
    Reference {
        number: caps["number"].parse().unwrap_or_default(),
        url: None,
    }
}

fn as_url(caps: regex::Captures) -> Reference {
    Reference {
        number: caps["number"].parse().unwrap_or_default(),
        url: Some(caps["url"].to_string()),
    }
}

/// Lines with their terminators kept, so concatenating them is the original
/// string. CRLF survives, and so does a body that does not end in a newline.
fn split_keep(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (at, c) in text.char_indices() {
        if c == '\n' {
            out.push(&text[start..=at]);
            start = at + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn without_eol(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => line,
    }
}

fn eol_of(line: &str) -> &str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

// ---------------------------------------------------------------------------
// Line surgery
// ---------------------------------------------------------------------------

/// The one edit a write may make to one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Tick the box, and only ever in that direction. A checked box beside an
    /// open issue is left alone: unchecking is destroying somebody's record on
    /// a heuristic, and every state that produces it (the work landed in
    /// another pull request, the issue was reopened for one detail) is one where
    /// the person is right and the heuristic is wrong.
    Tick,
    /// Append a reference to the item's text.
    Reference(String),
}

impl Change {
    /// What spar is adding, for the style gate. The rest of the body is
    /// somebody else's writing and is not spar's to clean.
    pub fn inserted(&self) -> &str {
        match self {
            Change::Tick => "x",
            Change::Reference(reference) => reference,
        }
    }
}

/// The body with one line changed, or an error saying why it will not be.
///
/// The line is found by its exact content, and every other line comes through
/// byte identical. That is proved here rather than assumed: this is the only
/// place spar rewrites something a person wrote, and a parser that mishandles a
/// nested list does not produce a bad comment, it produces a mangled plan.
pub fn rewrite(body: &str, raw: &str, change: &Change) -> Result<String> {
    let lines = split_keep(body);
    // If the split is not lossless nothing below it is safe.
    if lines.concat() != body {
        bail!("could not split the body into lines without changing it");
    }

    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| without_eol(line) == raw)
        .map(|(at, _)| at)
        .collect();
    match hits.len() {
        0 => bail!("that line is no longer in the body"),
        1 => {}
        n => bail!("{n} lines read exactly alike, so the edit could go to either"),
    }
    let at = hits[0];
    let replaced = changed(raw, change)?;

    let mut out = String::with_capacity(body.len() + replaced.len());
    for (index, line) in lines.iter().enumerate() {
        if index == at {
            out.push_str(&replaced);
            out.push_str(eol_of(line));
        } else {
            out.push_str(line);
        }
    }

    // Byte identity, on the result rather than on the plan for it.
    let after = split_keep(&out);
    if after.len() != lines.len() {
        bail!(
            "the edit changed the line count from {} to {}",
            lines.len(),
            after.len()
        );
    }
    for (index, (before, now)) in lines.iter().zip(&after).enumerate() {
        if index != at && before != now {
            bail!(
                "the edit would have changed line {}, which it must not",
                index + 1
            );
        }
    }
    Ok(out)
}

/// One line, changed. Nothing is reflowed, normalised, or re-emitted from a
/// parsed model: the untouched parts of the line are copied through as bytes.
fn changed(line: &str, change: &Change) -> Result<String> {
    let caps = ITEM
        .captures(line)
        .ok_or_else(|| spar_err!("that line is no longer a checklist item"))?;

    match change {
        Change::Tick => {
            let at = caps.name("state").expect("a state").start();
            if &line[at..at + 1] != " " {
                bail!("that box is already ticked");
            }
            Ok(format!("{}x{}", &line[..at], &line[at + 1..]))
        }
        Change::Reference(reference) => {
            let rest = caps.name("rest").expect("a rest");
            let text = rest.as_str();
            // Trailing whitespace is a markdown hard break, so the reference
            // goes before it rather than after.
            let body = text.trim_end_matches([' ', '\t']);
            if body.trim().is_empty() {
                bail!("that item has no text to attach {reference} to");
            }
            Ok(format!(
                "{}{body} {reference}{}",
                &line[..rest.start()],
                &text[body.len()..]
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Deciding
// ---------------------------------------------------------------------------

/// What an item is, before anything is asked of the network.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// It names something in this repository. What that something turns out to
    /// be is the network's answer, not the parser's.
    Names(i64),
    /// It names nothing, so it needs one.
    Needs,
    /// Left alone, with the reason.
    Hold(String),
    /// Past `max_tracker_children`.
    Over,
}

/// What spar would do with one unchecked item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The line already names an open issue here. Nothing is written.
    Adopt(i64),
    /// What it names is finished, so the box is ticked. An item that carried its
    /// reference before spar touched it is not in doubt.
    Tick(i64),
    /// An issue covers it and nothing linked them. The link is written; the box
    /// is not ticked this run whatever state that issue is in, because the
    /// match is fuzzy and a wrong adoption that also ticks the box is spar
    /// asserting work is done that nobody did.
    Link {
        number: i64,
        title: String,
        open: bool,
    },
    /// Nothing covers it. One is filed, then linked.
    File,
    /// Left alone, with the reason.
    Hold(String),
    /// Past `max_tracker_children`, and said out loud rather than dropped.
    Over,
}

impl Action {
    /// The edit this writes to the item's line.
    ///
    /// `File` has none yet, because its issue does not exist until it is filed;
    /// the caller writes the reference the moment it does.
    ///
    /// A `Link` writes the reference and never the tick, whatever state the
    /// issue it matched is in. The match is fuzzy, and a wrong adoption that
    /// also ticks the box is spar asserting work is done that nobody did.
    /// Holding the tick for one run puts the link in front of a person first,
    /// and by the next run the item carries its own reference and is in no more
    /// doubt than one somebody wrote by hand.
    pub fn change(&self) -> Option<Change> {
        match self {
            Action::Tick(_) => Some(Change::Tick),
            Action::Link { number, .. } => Some(Change::Reference(format!("#{number}"))),
            Action::Adopt(_) | Action::File | Action::Hold(_) | Action::Over => None,
        }
    }
}

/// One item and what is to become of it.
#[derive(Debug, Clone)]
pub struct Step {
    pub item: Item,
    pub action: Action,
}

/// Every unchecked item and its shape, without touching the network.
///
/// Checked items are absent by construction: spar checks a box and never
/// unchecks one, so there is nothing to decide about them.
fn shape(body: &str, slug: &str, max: usize) -> Vec<(Item, Shape)> {
    let items = parse(body);
    // A line that appears twice cannot be rewritten unambiguously, and there is
    // no reading of two identical items that makes filing two issues right.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for item in &items {
        *seen.entry(item.raw.as_str()).or_default() += 1;
    }

    let mut out = Vec::new();
    let mut taken = 0usize;
    for item in &items {
        if item.checked {
            continue;
        }
        let shape = if seen.get(item.raw.as_str()).copied().unwrap_or(0) > 1 {
            Shape::Hold("another item is written identically, so a link could go to either".into())
        } else if item.text.is_empty() {
            Shape::Hold("the item has no text".into())
        } else if taken >= max {
            Shape::Over
        } else {
            match &item.reference {
                Some(reference) => match reference.local(slug) {
                    Some(number) => {
                        taken += 1;
                        Shape::Names(number)
                    }
                    None => Shape::Hold(format!(
                        "it names an issue in another repository: {}",
                        reference.url.clone().unwrap_or_default()
                    )),
                },
                None => {
                    taken += 1;
                    Shape::Needs
                }
            }
        };
        out.push((item.clone(), shape));
    }
    out
}

/// What spar would do with each unchecked item, deciding but never writing.
///
/// Reads only: the state of an issue an item already names, and the similarity
/// search for one that does not. `spar triage` prints exactly this and the
/// acting path applies it, so the preview and the run cannot drift apart.
pub fn plan(repo: &Repo, cfg: &Config, tracker: i64, body: &str, slug: &str) -> Vec<Step> {
    shape(body, slug, cfg.loop_cfg.max_tracker_children)
        .into_iter()
        .map(|(item, shape)| {
            let action = match shape {
                Shape::Hold(why) => Action::Hold(why),
                Shape::Over => Action::Over,
                Shape::Names(number) => resolve(repo, number),
                // `file_as_issue` runs this search itself, so doing it here
                // looks redundant and is not: it maps a closed match to
                // `AlreadyClosed` and returns no url, which is right when
                // filing a follow-up and wrong here, where a closed match is a
                // done item that wants a link. Doing it first is also what lets
                // the log say "linked #40, filed nothing".
                Shape::Needs => match search(repo, tracker, &item.text) {
                    Some(found) => Action::Link {
                        number: found.number,
                        title: found.title,
                        open: found.open,
                    },
                    None => Action::File,
                },
            };
            Step { item, action }
        })
        .collect()
}

/// What to do about an item that already names something.
///
/// What it names is established before anything is read from it. Issues and
/// pull requests share one number sequence, `gh issue view` answers happily for
/// either, and adopting a pull request as a child would hand the run something
/// with no issue behind it.
fn resolve(repo: &Repo, number: i64) -> Action {
    match repo.item_kind(number) {
        Ok(ItemKind::Issue) => match repo.read_issue(number) {
            Ok(issue) if issue.is_closed() => Action::Tick(number),
            Ok(_) => Action::Adopt(number),
            Err(e) => Action::Hold(format!("could not read #{number}: {}", e.first_line())),
        },
        // A merged pull request is the work landing, which is what the tick is
        // for. An open one is somebody's work in progress and not spar's to
        // take up, and one closed unmerged is not a finished item at all.
        Ok(ItemKind::Pr) => match repo.pr_state(number).to_uppercase().as_str() {
            "MERGED" => Action::Tick(number),
            "" => Action::Hold(format!(
                "#{number} is a pull request in an unreadable state"
            )),
            state => Action::Hold(format!(
                "#{number} is a pull request, {}",
                state.to_lowercase()
            )),
        },
        Err(e) => Action::Hold(format!("could not read #{number}: {}", e.first_line())),
    }
}

fn search(repo: &Repo, tracker: i64, text: &str) -> Option<crate::repo::ExistingIssue> {
    let title = repo.clean_title(text).ok()?;
    repo.find_similar_issue(&title, &child_body(text, tracker))
}

/// What a child issue says, when spar has to file one. The item's own words,
/// and where they came from.
fn child_body(text: &str, tracker: i64) -> String {
    format!("{text}\n\nFrom the checklist in #{tracker}.")
}

// ---------------------------------------------------------------------------
// Acting
// ---------------------------------------------------------------------------

/// Work the checklist in one tracker, and hand back the children to work.
///
/// The children are ordinary issues from here on: they go through two agent
/// triage like anything else, which is what makes deterministic extraction
/// safe. An item that is stale or already fixed is declined there rather than
/// judged here.
pub fn decompose(cfg: &Config, repo: &Repo, tracker: i64) -> Vec<i64> {
    let Some((body, slug)) = read(repo, tracker) else {
        return Vec::new();
    };
    let steps = plan(repo, cfg, tracker, &body, &slug);
    if steps.is_empty() {
        logdim!("#{tracker} has no unchecked checklist items, so there is nothing to extract");
        return Vec::new();
    }
    log!("#{tracker}: {} unchecked checklist item(s)", steps.len());
    report_overflow(cfg, tracker, &steps);
    apply(repo, tracker, &steps)
}

fn apply(repo: &Repo, tracker: i64, steps: &[Step]) -> Vec<i64> {
    let mut children = Vec::new();
    for step in steps {
        let what = style::clip(&style::one_line(&step.item.text), 80);
        match &step.action {
            Action::Hold(why) => logdim!("  left '{what}' alone: {why}"),
            // Already named by `report_overflow`, in one line rather than one
            // line each.
            Action::Over => {}
            Action::Adopt(number) => {
                log!("  '{what}' is already #{number}");
                children.push(*number);
            }
            Action::Tick(number) => {
                let Some(change) = step.action.change() else {
                    continue;
                };
                if write(repo, tracker, &step.item.raw, &change) {
                    log!("  ticked '{what}' off, #{number} is finished");
                }
            }
            // Both titles, always. The match is fuzzy, and a wrong adoption
            // puts a false claim in somebody's plan.
            Action::Link {
                number,
                title,
                open,
            } => {
                log!("  linking '{what}' to #{number} '{title}', filed nothing");
                let Some(change) = step.action.change() else {
                    continue;
                };
                if write(repo, tracker, &step.item.raw, &change) && *open {
                    children.push(*number);
                }
            }
            Action::File => {
                let Ok(title) = repo.clean_title(&step.item.text) else {
                    logdim!("  could not clean a title out of '{what}'");
                    continue;
                };
                match review::file_as_issue(repo, &title, &child_body(&step.item.text, tracker)) {
                    Ok(filed) => {
                        let number = filed.issue();
                        log!("  {} for '{what}'", filed.note());
                        // The link is written the moment the issue exists, not
                        // once at the end over the whole checklist. The window
                        // is then one item wide and falls on the side of filing
                        // twice rather than losing a link, which the similarity
                        // search catches next run like any other duplicate.
                        if write(
                            repo,
                            tracker,
                            &step.item.raw,
                            &Change::Reference(format!("#{number}")),
                        ) && filed.number().is_some()
                        {
                            children.push(number);
                        }
                    }
                    Err(e) => logdim!("  could not file an issue for '{what}': {e}"),
                }
            }
        }
    }
    children
}

/// Re-read, rewrite one line, write back.
///
/// The body is read again here rather than reused from the copy this run
/// parsed. A run is long, and somebody editing the tracker while it goes must
/// not lose that edit: if the line has moved or changed, this is a skip with a
/// log line, never a write.
fn write(repo: &Repo, tracker: i64, raw: &str, change: &Change) -> bool {
    let body = match repo.read_issue(tracker) {
        Ok(issue) => issue.body_text().to_string(),
        Err(e) => {
            logdim!("  could not re-read #{tracker}: {}", e.first_line());
            return false;
        }
    };
    let updated = match rewrite(&body, raw, change) {
        Ok(updated) => updated,
        Err(e) => {
            logdim!("  not editing #{tracker}: {}", e.first_line());
            return false;
        }
    };
    match repo.edit_issue_body(tracker, &updated, change.inserted()) {
        Ok(()) => true,
        Err(e) => {
            logdim!("  could not edit #{tracker}: {}", e.first_line());
            false
        }
    }
}

fn report_overflow(cfg: &Config, tracker: i64, steps: &[Step]) {
    let left: Vec<String> = steps
        .iter()
        .filter(|s| s.action == Action::Over)
        .map(|s| format!("'{}'", style::clip(&style::one_line(&s.item.text), 60)))
        .collect();
    if left.is_empty() {
        return;
    }
    logwarn!(
        "#{tracker} has more unchecked items than max_tracker_children ({}), so {} were left for \
         a later run: {}",
        cfg.loop_cfg.max_tracker_children,
        left.len(),
        left.join(", ")
    );
}

fn read(repo: &Repo, tracker: i64) -> Option<(String, String)> {
    // From the API, never from what triage rendered: `body_for_prompt`
    // shortens past `max_issue_chars`, and a tracker is precisely the long
    // issue that trips it. Parsing a shortened body drops the last items and
    // looks identical to a tracker that had fewer.
    match repo.read_issue(tracker) {
        Ok(issue) => Some((
            issue.body_text().to_string(),
            repo.name_with_owner().unwrap_or_default(),
        )),
        Err(e) => {
            logdim!("could not read #{tracker}: {}", e.first_line());
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The read only half
// ---------------------------------------------------------------------------

/// Print the decomposition `spar run` would perform, and write none of it.
///
/// `spar triage` is the command you reach for to look before leaping, so a
/// preview that files issues and rewrites somebody's issue body is exactly the
/// trap that comment names. This is where the first few real trackers should be
/// checked.
pub fn preview(cfg: &Config, repo: &Repo, tracker: i64) {
    let Some((body, slug)) = read(repo, tracker) else {
        return;
    };
    let steps = plan(repo, cfg, tracker, &body, &slug);
    if steps.is_empty() {
        return;
    }
    println!("\n#{tracker}, if decompose_trackers let it act on the checklist:");

    let mut projected = body.clone();
    for step in &steps {
        let what = style::clip(&style::one_line(&step.item.text), 80);
        match &step.action {
            Action::Adopt(number) => println!("  keep  '{what}' is already #{number}"),
            Action::Tick(number) => println!("  tick  '{what}', #{number} is finished"),
            Action::Link {
                number,
                title,
                open,
            } => {
                let state = if *open { "open" } else { "closed" };
                println!("  link  '{what}' to #{number} '{title}' ({state}), filing nothing");
            }
            Action::File => println!("  file  '{what}'"),
            Action::Over => println!("  over  '{what}' is past max_tracker_children"),
            Action::Hold(why) => println!("  hold  '{what}': {why}"),
        }
        // The same mapping the acting path uses, so what is printed here and
        // what a run would write cannot drift. Only `File` is the preview's own,
        // since the issue it would link to does not exist yet.
        let change = match &step.action {
            Action::File => Some(Change::Reference(FILED.to_string())),
            other => other.change(),
        };
        let Some(change) = change else { continue };
        match rewrite(&projected, &step.item.raw, &change) {
            Ok(next) => projected = next,
            Err(e) => println!("        the line will not be rewritten: {e}"),
        }
    }

    let diff = diff(&body, &projected);
    if diff.is_empty() {
        println!("  nothing would be written to the body");
    } else {
        println!("  and the body it would write:");
        for line in diff {
            println!("    {line}");
        }
    }
}

/// Stands in for a number that does not exist yet, so the preview can show the
/// line an item would get without pretending to know which issue it will be.
const FILED: &str = "#(the issue it files)";

/// The changed lines, old then new. Line local edits only, so the two bodies
/// always have the same number of lines and nothing has to be aligned.
fn diff(before: &str, after: &str) -> Vec<String> {
    split_keep(before)
        .into_iter()
        .zip(split_keep(after))
        .filter(|(old, new)| old != new)
        .flat_map(|(old, new)| {
            [
                format!("- {}", without_eol(old)),
                format!("+ {}", without_eol(new)),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(body: &str) -> Vec<String> {
        parse(body).into_iter().map(|i| i.text).collect()
    }

    #[test]
    fn the_ordinary_checklist_is_read_as_items() {
        let items = parse("Some prose.\n\n- [ ] first\n- [x] second\n");
        assert_eq!(2, items.len());
        assert_eq!("first", items[0].text);
        assert!(!items[0].checked);
        assert!(items[1].checked);
        assert_eq!(3, items[0].line);
    }

    /// Nested and indented items are ordinary items: every edit here is line
    /// local, so the shape of the list around one does not matter.
    #[test]
    fn indented_and_nested_items_are_items() {
        let body = "- [ ] parent\n  - [ ] child\n\t- [ ] tabbed\n    * [ ] deeper\n1. [ ] ordered\n2) [ ] also ordered\n";
        assert_eq!(
            vec![
                "parent",
                "child",
                "tabbed",
                "deeper",
                "ordered",
                "also ordered"
            ],
            texts(body)
        );
    }

    /// The failure that would file issues for somebody's example markdown.
    #[test]
    fn something_that_looks_like_an_item_inside_a_fence_is_not_one() {
        let body = "\
- [ ] real

```markdown
- [ ] not real
```

~~~
- [ ] also not real
~~~

- [ ] real again
";
        assert_eq!(vec!["real", "real again"], texts(body));
    }

    /// A fence closes on its own glyph only, so a stray one of the other kind
    /// inside does not end the block early.
    #[test]
    fn a_fence_is_closed_only_by_its_own_kind() {
        let body = "~~~\n```\n- [ ] not real\n```\n~~~\n- [ ] real\n";
        assert_eq!(vec!["real"], texts(body));
    }

    #[test]
    fn a_windows_body_is_read_the_same_way() {
        let items = parse("intro\r\n\r\n- [ ] first\r\n- [x] second\r\n");
        assert_eq!(2, items.len());
        assert_eq!("first", items[0].text);
        assert!(items[1].checked);
        assert_eq!(
            "- [ ] first", items[0].raw,
            "the terminator is not part of the handle"
        );
    }

    #[test]
    fn a_reference_is_read_from_a_number_or_a_link() {
        let items = parse(
            "- [ ] one #12\n\
             - [ ] two https://github.com/o/r/issues/34\n\
             - [ ] [three](https://github.com/o/r/issues/56)\n\
             - [ ] four\n",
        );
        assert_eq!(Some(12), items[0].reference.as_ref().map(|r| r.number));
        assert_eq!(Some(34), items[1].reference.as_ref().map(|r| r.number));
        assert_eq!(Some(56), items[2].reference.as_ref().map(|r| r.number));
        assert_eq!(None, items[3].reference);
    }

    /// An item whose whole text is a link to somewhere else is not a reference
    /// to anything spar can act on.
    #[test]
    fn an_item_that_is_a_link_to_something_else_names_no_issue() {
        let items = parse("- [ ] [the docs](https://example.com/guide)\n");
        assert_eq!(None, items[0].reference);
        assert_eq!("[the docs](https://example.com/guide)", items[0].text);
    }

    /// Taking the number out of another repository's link would point the item
    /// at whatever happens to carry that number here.
    #[test]
    fn a_link_to_another_repository_is_not_adoptable() {
        let items = parse("- [ ] see https://github.com/other/thing/issues/7\n");
        let reference = items[0].reference.as_ref().expect("a reference");
        assert_eq!(None, reference.local("me/mine"));
        assert_eq!(Some(7), reference.local("other/thing"));
    }

    /// A bare number can only mean this repository, so it needs no slug.
    #[test]
    fn a_bare_number_resolves_wherever_it_is_read() {
        let items = parse("- [ ] work #7\n");
        assert_eq!(Some(7), items[0].reference.as_ref().unwrap().local(""));
    }

    // -- the line surgery -------------------------------------------------

    #[test]
    fn a_reference_is_appended_to_its_own_line_and_nowhere_else() {
        let body = "intro\n\n- [ ] first\n- [ ] second\n\nmore prose\n";
        let out =
            rewrite(body, "- [ ] first", &Change::Reference("#40".into())).expect("a rewrite");
        assert_eq!(
            "intro\n\n- [ ] first #40\n- [ ] second\n\nmore prose\n",
            out
        );
    }

    /// Two trailing spaces are a markdown hard break, and `style::scrub` would
    /// eat them. The reference goes before them.
    #[test]
    fn a_hard_break_survives_the_edit() {
        let out = rewrite(
            "- [ ] first  \nnext\n",
            "- [ ] first  ",
            &Change::Reference("#4".into()),
        )
        .expect("a rewrite");
        assert_eq!("- [ ] first #4  \nnext\n", out);
    }

    #[test]
    fn every_other_line_comes_through_byte_identical() {
        let body = "# Plan\r\n\r\n  trailing spaces here   \r\n- [ ] one\r\n\r\n\r\n\r\nlots of blank lines above\r\n";
        let out = rewrite(body, "- [ ] one", &Change::Reference("#9".into())).expect("a rewrite");
        let (before, after): (Vec<&str>, Vec<&str>) =
            (body.lines().collect(), out.lines().collect());
        assert_eq!(before.len(), after.len());
        for (i, (a, b)) in before.iter().zip(&after).enumerate() {
            if i == 3 {
                assert_eq!("- [ ] one #9", *b);
            } else {
                assert_eq!(a, b, "line {} changed", i + 1);
            }
        }
        assert!(out.contains("trailing spaces here   \r\n"));
        assert!(out.contains("\r\n\r\n\r\n\r\n"));
    }

    #[test]
    fn a_body_with_no_final_newline_keeps_not_having_one() {
        let out = rewrite("- [ ] only", "- [ ] only", &Change::Tick).expect("a rewrite");
        assert_eq!("- [x] only", out);
    }

    #[test]
    fn ticking_changes_the_box_and_leaves_the_text() {
        let out = rewrite("  - [ ] deep #3\n", "  - [ ] deep #3", &Change::Tick).expect("a tick");
        assert_eq!("  - [x] deep #3\n", out);
    }

    /// spar checks a box and never unchecks one, so there is no change that
    /// could and nothing to do to one that is already ticked.
    #[test]
    fn a_ticked_box_is_never_written_again() {
        assert!(rewrite("- [x] done\n", "- [x] done", &Change::Tick).is_err());
        assert!(!matches!(Change::Tick, Change::Reference(_)));
    }

    #[test]
    fn a_line_that_is_gone_or_ambiguous_is_a_refusal_not_a_guess() {
        assert!(rewrite("- [ ] a\n", "- [ ] b", &Change::Tick).is_err());
        let twice = "- [ ] same\n- [ ] same\n";
        assert!(rewrite(twice, "- [ ] same", &Change::Tick).is_err());
    }

    #[test]
    fn an_item_with_no_text_gets_no_reference() {
        assert!(rewrite("- [ ]\n", "- [ ]", &Change::Reference("#1".into())).is_err());
    }

    // -- shaping ----------------------------------------------------------

    fn shapes(body: &str, max: usize) -> Vec<Shape> {
        shape(body, "me/mine", max)
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    }

    #[test]
    fn a_checked_item_is_never_reconsidered() {
        assert!(shapes("- [x] done\n", 5).is_empty());
    }

    #[test]
    fn an_item_that_names_an_issue_is_kept_apart_from_one_that_does_not() {
        assert_eq!(
            vec![Shape::Names(12), Shape::Needs],
            shapes("- [ ] one #12\n- [ ] two\n", 5)
        );
    }

    /// A cap, not a target, and what it left is named out loud rather than
    /// quietly dropped.
    #[test]
    fn the_cap_stops_at_the_cap() {
        let body = "- [ ] a\n- [ ] b\n- [ ] c\n- [ ] d\n";
        assert_eq!(
            vec![Shape::Needs, Shape::Needs, Shape::Over, Shape::Over],
            shapes(body, 2)
        );
    }

    /// A checked item does not spend the budget, since nothing is done to it.
    #[test]
    fn the_cap_counts_only_what_it_acts_on() {
        let body = "- [x] a\n- [x] b\n- [ ] c\n";
        assert_eq!(vec![Shape::Needs], shapes(body, 1));
    }

    #[test]
    fn two_identical_items_are_left_alone() {
        let out = shapes("- [ ] same\n- [ ] same\n", 5);
        assert!(matches!(out[0], Shape::Hold(_)), "{out:?}");
        assert!(matches!(out[1], Shape::Hold(_)), "{out:?}");
    }

    #[test]
    fn an_item_naming_another_repository_is_held_rather_than_adopted() {
        let out = shapes("- [ ] see https://github.com/other/thing/issues/7\n", 5);
        assert!(matches!(out[0], Shape::Hold(_)), "{out:?}");
    }

    /// The whole thing on one body of the shape a person actually writes: what
    /// each item is taken for, and exactly what comes out the other side.
    #[test]
    fn a_realistic_tracker_keeps_every_line_it_was_not_asked_to_change() {
        let body = "\
Context somebody wrote, with a hard break here:
and the rest of it.

## Parts

- [x] already done
- [ ] parse the checklist
- [ ] write the link back #40
  - [ ] and prove it first

```markdown
- [ ] an example, not an item
```

That is all.
";
        let shapes: Vec<Shape> = shape(body, "me/mine", 5)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            vec![Shape::Needs, Shape::Names(40), Shape::Needs],
            shapes,
            "the ticked item, the fenced one and the prose are all left out"
        );

        let out = rewrite(
            body,
            "- [ ] parse the checklist",
            &Change::Reference("#41".into()),
        )
        .expect("a link");
        let out = rewrite(
            &out,
            "  - [ ] and prove it first",
            &Change::Reference("#42".into()),
        )
        .expect("a nested link");
        let out = rewrite(&out, "- [ ] write the link back #40", &Change::Tick).expect("a tick");

        assert_eq!(
            "\
Context somebody wrote, with a hard break here:
and the rest of it.

## Parts

- [x] already done
- [ ] parse the checklist #41
- [x] write the link back #40
  - [ ] and prove it first #42

```markdown
- [ ] an example, not an item
```

That is all.
",
            out
        );
    }

    // -- what each decision writes ----------------------------------------

    /// The match is fuzzy, so a wrong adoption that also ticked the box would
    /// be spar asserting work is done that nobody did. The link goes in this
    /// run and the tick waits for the next, by which time the item carries its
    /// own reference and a person has had the chance to see it.
    #[test]
    fn an_item_linked_by_similarity_is_not_ticked_in_the_same_run() {
        for open in [true, false] {
            let action = Action::Link {
                number: 7,
                title: "something close enough".into(),
                open,
            };
            assert_eq!(Some(Change::Reference("#7".into())), action.change());
        }
    }

    /// An item that carried its reference before spar touched it is in no such
    /// doubt, so it is ticked on the spot.
    #[test]
    fn an_item_that_already_named_its_issue_is_ticked_when_that_issue_closes() {
        assert_eq!(Some(Change::Tick), Action::Tick(7).change());
    }

    /// Adopting is reading, not writing: the line already says what it says.
    #[test]
    fn nothing_is_written_for_an_item_that_is_already_linked_and_open() {
        assert_eq!(None, Action::Adopt(7).change());
        assert_eq!(None, Action::Over.change());
        assert_eq!(None, Action::Hold("any reason".into()).change());
    }

    // -- the preview ------------------------------------------------------

    #[test]
    fn the_diff_shows_only_the_lines_that_change() {
        let before = "- [ ] one\n- [ ] two\n";
        let after =
            rewrite(before, "- [ ] two", &Change::Reference("#8".into())).expect("a rewrite");
        assert_eq!(
            vec!["- - [ ] two".to_string(), "+ - [ ] two #8".to_string()],
            diff(before, &after)
        );
    }
}
