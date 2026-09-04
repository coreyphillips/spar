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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::config::Config;
use crate::error::Result;
use crate::model::{Issue, ItemKind};
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
        r"^(?P<indent>[ \t]*)(?:[-*+]|[0-9]{1,9}[.)])(?P<gap>[ \t]+)\[(?P<state>[ xX])\](?P<rest>[ \t].*|)$",
    )
    .expect("task item pattern")
});

/// The line read as a task list item, or `None` if it is not one.
///
/// More than four columns after the marker put the checkbox in an indented code
/// block inside the item: markdown starts the content one column after the
/// marker, and everything past that is code. `-     [ ] example` is somebody
/// showing the syntax, which is the fence case again.
fn item_of(line: &str) -> Option<regex::Captures<'_>> {
    let caps = ITEM.captures(line)?;
    (indent_width(&caps["gap"]) <= 4).then_some(caps)
}

/// Any list line, checkbox or not, so that a task nested under a plain bullet
/// is still read as nested rather than as indented code. The marker and the
/// gap after it are captured because they decide where the item's content
/// starts, and that is what four spaces are measured against.
static LIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<indent>[ \t]*)(?P<marker>[-*+]|[0-9]{1,9}[.)])(?P<gap>[ \t]*)(?P<rest>.*)$")
        .expect("list pattern")
});

/// The raw HTML blocks GitHub renders exactly as written, so a checkbox inside
/// one is text somebody is showing rather than a box anybody can tick. Same
/// shape as a fence, different clothes again.
static HTML_VERBATIM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[ \t]*<(?:pre|script|style|textarea)\b").expect("html open pattern")
});

static HTML_CLOSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</(?:pre|script|style|textarea)>").expect("html close pattern")
});

/// A line that starts a block level HTML tag. Markdown inside one of these is
/// not parsed either: `<div>` then a checkbox on the next line renders as the
/// literal text `- [ ] thing`. The block runs to the next blank line rather
/// than to a closing tag, which is what makes the `<details>` a tracker is
/// often written in still work: the blank line after `</summary>` ends it and
/// the checklist below is a checklist.
static HTML_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^[ \t]*</?(?:address|article|aside|base|basefont|blockquote|body|caption|center",
        r"|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form",
        r"|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem",
        r"|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot",
        r"|th|thead|title|tr|track|ul)\b"
    ))
    .expect("html block pattern")
});

/// Which kind of raw HTML block is open, since the two end differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Html {
    /// `<pre>` and friends, which run to their closing tag.
    Verbatim,
    /// Any other block tag, which runs to the next blank line.
    Block,
}

/// A fence, opening or closing. Info string included so that only a bare fence
/// can close one.
static FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*(?P<fence>`{3,}|~{3,})(?P<info>.*)$").expect("fence pattern")
});

/// The three things GitHub turns into an issue link without a url: `#123`,
/// `owner/repo#123`, and `GH-123`. All at the start of the text or after
/// something that is neither a word nor a path separator, so that a fragment
/// left in the middle of an address is not read as a number.
static HASH_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^\w/])(?:(?P<slug>[\w.-]+/[\w.-]+)#|#|GH-)(?P<number>[0-9]{1,9})\b")
        .expect("hash pattern")
});

/// A link, of any shape. Blanked before the bare numbers are read, because the
/// `#8` in `https://example.com/guide/#8` is a fragment in somebody's address
/// and not a reference to issue 8. The links that do name an issue are read by
/// `URL_REF` from the text with them still in it.
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>)\]]*").expect("link pattern"));

/// An HTML comment, closed or running to the end of the text. GitHub renders
/// none of it, so a number parked in one is a note to a person.
static COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?(?:-->|$)").expect("comment pattern"));

/// A link to an issue or a pull request, on any host. Which repository it
/// belongs to is decided later, against this one's name.
///
/// Pull requests are here because an item is very often written down as the
/// change that closes it, and because `resolve` already answers for one: a
/// merged pull request ticks the box, an open one is held. Reading only
/// `/issues/` would file a second issue for work already in flight.
static URL_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<url>https?://[^\s)\]]*?/(?:issues|pull)/(?P<number>[0-9]{1,9}))\b")
        .expect("url pattern")
});

/// Where the issue an item names lives, as the item spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `#123` or `GH-123`, which can only mean this repository.
    Here,
    /// `owner/repo#123`. The host is left out of the shorthand, so it is this
    /// repository whenever the path matches, wherever this one is served from.
    Repo(String),
    /// A link, host and all, which can name anybody's.
    Url(String),
}

/// An issue an item's text already names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub number: i64,
    pub origin: Origin,
}

impl Reference {
    /// The issue number, when the reference is one this repository can act on.
    ///
    /// Another repository's issue is not adoptable: taking the number out of it
    /// would point the item at whatever happens to carry that number here,
    /// which is the wrong link failure with no fuzziness to blame.
    ///
    /// `home` is this repository's own address, host and all, so that another
    /// host serving the same `owner/repo` path is somebody else's.
    pub fn local(&self, home: &str) -> Option<i64> {
        match &self.origin {
            Origin::Here => Some(self.number),
            Origin::Repo(slug) => (slug == &owner_repo(home)).then_some(self.number),
            Origin::Url(url) => {
                let home = locator(home).trim_end_matches('/');
                let url = locator(url);
                let owned = !home.is_empty()
                    && (url.starts_with(&format!("{home}/issues/"))
                        || url.starts_with(&format!("{home}/pull/")));
                owned.then_some(self.number)
            }
        }
    }

    /// The reference as it would be quoted back to somebody, for a log line.
    pub fn names(&self) -> String {
        match &self.origin {
            Origin::Here => format!("#{}", self.number),
            Origin::Repo(slug) => format!("{slug}#{}", self.number),
            Origin::Url(url) => url.clone(),
        }
    }
}

/// The last two segments of an address, which are the owner and repository a
/// `owner/repo#1` shorthand is measured against. Empty when there is no address
/// to read, which matches no shorthand rather than guessing at one.
fn owner_repo(home: &str) -> String {
    let path: Vec<&str> = locator(home).trim_matches('/').split('/').collect();
    match path.len() {
        0..=2 => String::new(),
        n => format!("{}/{}", path[n - 2], path[n - 1]),
    }
}

/// A url reduced to what identifies it. The scheme and a leading `www.` are
/// two spellings of the same place, and matching on the rest from the front
/// keeps `elsewhere.example/me/mine/issues/7` out of `me/mine`.
fn locator(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.strip_prefix("www.").unwrap_or(rest)
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
/// Deliberately dull about what it will not treat as an item: anything the
/// reader of the issue does not see as a checkbox is not one. That is a fenced
/// block, an indented code block, an HTML comment, anything inside a block
/// level HTML tag, and anything that is not a task list line. Nested items are
/// ordinary items, since every edit here is line local.
pub fn parse(body: &str) -> Vec<Item> {
    let mut out = Vec::new();
    // The glyph and length a close has to match, and the column it opened in,
    // since a fence four columns further in is code rather than the close.
    let mut fence: Option<(char, usize, usize)> = None;
    let mut comment = false;
    let mut html: Option<Html> = None;
    // Where the content of each open list item starts, outermost first. Four
    // spaces mean code, but four spaces from where: the margin outside a list,
    // and the innermost item's own content column inside one.
    let mut open: Vec<usize> = Vec::new();

    for (index, raw) in split_keep(body).into_iter().enumerate() {
        let line = without_eol(raw);
        // A comment is markdown a person wrote for the next person, often the
        // items they decided against. GitHub renders none of it.
        if comment {
            comment = !line.contains("-->");
            continue;
        }
        if let Some(kind) = html {
            let ends = match kind {
                Html::Verbatim => HTML_CLOSE.is_match(line),
                Html::Block => line.trim().is_empty(),
            };
            if ends {
                html = None;
            }
            continue;
        }
        let indent = indent_width(line);
        if let Some((glyph, len, column)) = fence {
            // Only the same glyph, at least as long, with nothing after it, and
            // not indented so far past the opening one that it is code.
            if let Some(caps) = FENCE.captures(line) {
                let marker = &caps["fence"];
                let closes = marker.starts_with(glyph)
                    && marker.len() >= len
                    && caps["info"].trim().is_empty()
                    && indent < column + 4;
                if closes {
                    fence = None;
                }
            }
            continue;
        }
        // A blank line closes nothing: a list survives one, and both markdown
        // and the person writing it expect the item after it to still be in.
        if !line.trim().is_empty() {
            while open.last().is_some_and(|col| indent < *col) {
                open.pop();
            }
        }
        // Four spaces past wherever the content of this line belongs is a code
        // block, which is the fence case wearing different clothes. `- outer`
        // then six spaces is an example inside that item, not a nested task.
        let margin = open.last().copied().unwrap_or(0);
        if !line.trim().is_empty() && indent >= margin + 4 {
            continue;
        }
        // After the code check, so that four spaces in are a fence a person is
        // showing rather than one they are opening.
        if let Some(caps) = FENCE.captures(line) {
            let marker = &caps["fence"];
            fence = Some((
                marker.chars().next().expect("a fence"),
                marker.len(),
                indent,
            ));
            continue;
        }
        if let Some(column) = content_column(line) {
            open.push(column);
        }
        if HTML_VERBATIM.is_match(line) {
            html = (!HTML_CLOSE.is_match(line)).then_some(Html::Verbatim);
            continue;
        }
        if HTML_BLOCK.is_match(line) {
            html = Some(Html::Block);
            continue;
        }
        comment = opens_comment(line);
        let Some(caps) = item_of(line) else {
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

/// Where a list line's content starts, in columns, or `None` if it is not one.
///
/// Markdown puts the content one column after the marker when the gap is
/// nothing or wider than four, and at the gap otherwise.
fn content_column(line: &str) -> Option<usize> {
    let caps = LIST.captures(line)?;
    let gap = indent_width(&caps["gap"]);
    if gap == 0 && !caps["rest"].is_empty() {
        return None;
    }
    let gap = match (1..=4).contains(&gap) && !caps["rest"].trim().is_empty() {
        true => gap,
        false => 1,
    };
    Some(indent_width(&caps["indent"]) + caps["marker"].len() + gap)
}

/// The first issue this text names, by link or by number.
///
/// Read from the text with comments, code spans and link labels blanked out. A
/// `#12` in backticks is somebody writing about a number rather than pointing
/// at one, and a `#7` in a link's label captions the destination: without this,
/// `[other/widgets #7](https://github.com/other/widgets/issues/7)` becomes a
/// bare local #7 and the foreign repository check never sees it.
///
/// The bare numbers are read from a copy with the links blanked too, so that
/// only `URL_REF` speaks for what is inside an address. Both copies are the
/// same length as the original, so the two offsets can still be compared.
fn reference_in(text: &str) -> Option<Reference> {
    let text = &readable(text);
    let url = URL_REF.captures(text);
    let outside = blank(text, &LINK);
    let hash = HASH_REF.captures(&outside);
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
        origin: match caps.name("slug") {
            Some(slug) => Origin::Repo(slug.as_str().to_string()),
            None => Origin::Here,
        },
    }
}

fn as_url(caps: regex::Captures) -> Reference {
    Reference {
        number: caps["number"].parse().unwrap_or_default(),
        origin: Origin::Url(caps["url"].to_string()),
    }
}

/// The text with every match of `what` replaced by spaces, byte for byte so
/// that offsets into it still line up with the original.
fn blank(text: &str, what: &Regex) -> String {
    let mut out = text.as_bytes().to_vec();
    for found in what.find_iter(text) {
        out[found.range()].fill(b' ');
    }
    // Whole matches are blanked, so no character is left half replaced.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// The text with comments, code spans and inline link labels replaced by
/// spaces, byte for byte so that offsets into it still line up with the
/// original. A link's destination is left standing, because that is the part
/// that names an issue.
fn readable(text: &str) -> String {
    let text = blank(text, &COMMENT);
    let text = text.as_str();
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'`' => {
                let start = at;
                while at < bytes.len() && bytes[at] == b'`' {
                    at += 1;
                }
                if let Some(end) = backtick_run(bytes, at, at - start) {
                    out[start..end].fill(b' ');
                    at = end;
                }
            }
            b'[' => match label_end(bytes, at) {
                Some(end) => {
                    out[at..end].fill(b' ');
                    at = end;
                }
                None => at += 1,
            },
            _ => at += 1,
        }
    }
    // Only whole regions delimited by ASCII are blanked, so this holds.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// The end of the next run of exactly `len` backticks, which is what closes a
/// code span opened by one that long.
fn backtick_run(bytes: &[u8], from: usize, len: usize) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at] == b'`' {
            at += 1;
        }
        if at - start == len {
            return Some(at);
        }
    }
    None
}

/// The end of an inline link's label, including nested and escaped brackets,
/// when a destination follows.
fn label_end(bytes: &[u8], open: usize) -> Option<usize> {
    if bytes.get(open) != Some(&b'[') || escaped(bytes, open) {
        return None;
    }

    let mut depth = 1usize;
    let mut at = open + 1;
    while at < bytes.len() {
        if escaped(bytes, at) {
            at += 1;
            continue;
        }
        match bytes[at] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return (bytes.get(at + 1) == Some(&b'(')).then_some(at + 1);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

fn escaped(bytes: &[u8], at: usize) -> bool {
    let mut slashes = 0usize;
    let mut cursor = at;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
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

/// Leading whitespace in columns, a tab counting as the four spaces markdown
/// gives it when it decides what is indented far enough to be code.
fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Whether the line leaves an HTML comment open behind it.
fn opens_comment(line: &str) -> bool {
    match line.rfind("<!--") {
        Some(at) => !line[at + 4..].contains("-->"),
        None => false,
    }
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
    let caps = item_of(line).ok_or_else(|| spar_err!("that line is no longer a checklist item"))?;

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
fn shape(body: &str, home: &str, max: usize) -> Vec<(Item, Shape)> {
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
                Some(reference) => match reference.local(home) {
                    Some(number) => {
                        taken += 1;
                        Shape::Names(number)
                    }
                    None => Shape::Hold(format!(
                        "it names an issue in another repository: {}",
                        reference.names()
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
pub fn plan(repo: &Repo, cfg: &Config, tracker: i64, body: &str, home: &str) -> Vec<Step> {
    shape(body, home, cfg.loop_cfg.max_tracker_children)
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
            Ok(issue) => issue_action(&issue),
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

/// What an item's own issue says about the box beside it.
///
/// A tick says the work landed. An issue closed as not planned is the
/// opposite: somebody, or both agents under `close_skipped`, decided against
/// it, and "spar decided not to do this" is indistinguishable from "this was
/// done" once the box is ticked. spar never unchecks one, so the correction
/// would be somebody else's job.
fn issue_action(issue: &Issue) -> Action {
    let number = issue.number;
    if issue.closed_as_not_planned() {
        return Action::Hold(format!(
            "#{number} was closed as not planned, so the item is not done"
        ));
    }
    if issue.is_closed() {
        return Action::Tick(number);
    }
    Action::Adopt(number)
}

/// An issue that already covers this item, the tracker itself apart.
///
/// The tracker quotes every item in its own checklist, so it is the closest
/// match for each one of them. Adopting it would link an item to the issue it
/// is written in, and the run would then see the tracker as already handled and
/// work nothing.
fn search(repo: &Repo, tracker: i64, text: &str) -> Option<crate::repo::ExistingIssue> {
    let title = repo.clean_title(text).ok()?;
    repo.find_similar_issue_apart_from(&title, &child_body(text, tracker), Some(tracker))
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
    let Some((body, slug)) = read_for_write(repo, tracker) else {
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
                let Ok(title) = repo.clean_nonempty_title_for_write(&step.item.text) else {
                    logdim!("  could not clean a title out of '{what}'");
                    continue;
                };
                // Asked again, against the tracker as it stands, because filing
                // is the one step that leaves something behind. An item
                // somebody deleted while the run was working must not still get
                // an issue for work the tracker no longer asks for.
                if !still_asked_for(repo, tracker, &step.item.raw) {
                    logdim!("  '{what}' is no longer in #{tracker}, so nothing was filed for it");
                    continue;
                }
                match review::file_as_issue_apart_from(
                    repo,
                    &title,
                    &child_body(&step.item.text, tracker),
                    Some(tracker),
                ) {
                    Ok(filed) => {
                        let number = filed.issue();
                        log!("  {} for '{what}'", filed.note());
                        // The link is written the moment the issue exists, not
                        // once at the end over the whole checklist. The window
                        // is then one item wide and falls on the side of filing
                        // twice rather than losing a link, which the similarity
                        // search catches next run like any other duplicate.
                        let linked = write(
                            repo,
                            tracker,
                            &step.item.raw,
                            &Change::Reference(format!("#{number}")),
                        );
                        match linked {
                            true if filed.number().is_some() => children.push(number),
                            true => {}
                            false => logwarn!(
                                "  '{what}' went to #{number}, but #{tracker} does not link to it"
                            ),
                        }
                    }
                    Err(e) => logdim!("  could not file an issue for '{what}': {e}"),
                }
            }
        }
    }
    unique_children(children)
}

fn unique_children(mut children: Vec<i64>) -> Vec<i64> {
    let mut seen = BTreeSet::new();
    children.retain(|number| seen.insert(*number));
    children
}

/// Whether the tracker still carries this exact line, once and only once, and
/// still reads it as a checklist item.
///
/// Both halves, because an edit that lands mid-run can leave the bytes exactly
/// as they were and still change what they mean. Fencing the line, or opening
/// a comment above it, is a person saying not this one, and matching the raw
/// text alone would file for it and rewrite it inside the fence.
fn still_an_item(body: &str, raw: &str) -> bool {
    let lines = split_keep(body)
        .into_iter()
        .filter(|line| without_eol(line) == raw)
        .count();
    lines == 1 && parse(body).iter().filter(|item| item.raw == raw).count() == 1
}

/// The same question, asked of the tracker as it stands.
///
/// Unreadable counts as no: this gates filing, and an issue filed against a
/// tracker that cannot be read is one nothing will link.
fn still_asked_for(repo: &Repo, tracker: i64, raw: &str) -> bool {
    match repo.record_failed_write(repo.read_issue(tracker)) {
        Ok(issue) => still_an_item(issue.body_text(), raw),
        Err(e) => {
            logdim!("  could not re-read #{tracker}: {}", e.first_line());
            false
        }
    }
}

/// Re-read, rewrite one line, write back.
///
/// The body is read again here rather than reused from the copy this run
/// parsed. A run is long, and somebody editing the tracker while it goes must
/// not lose that edit: if the line has moved or changed, or the markdown around
/// it has stopped making it an item, this is a skip with a log line, never a
/// write.
fn write(repo: &Repo, tracker: i64, raw: &str, change: &Change) -> bool {
    let body = match repo.record_failed_write(repo.read_issue(tracker)) {
        Ok(issue) => issue.body_text().to_string(),
        Err(e) => {
            logdim!("  could not re-read #{tracker}: {}", e.first_line());
            return false;
        }
    };
    if !still_an_item(&body, raw) {
        logdim!("  not editing #{tracker}: that line is no longer a checklist item in it");
        return false;
    }
    let updated = match rewrite(&body, raw, change) {
        Ok(updated) => updated,
        Err(e) => {
            logdim!("  not editing #{tracker}: {}", e.first_line());
            return false;
        }
    };
    match repo.edit_issue_body(tracker, &body, &updated, change.inserted()) {
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
        Ok(issue) => Some((issue.body_text().to_string(), home_of(&issue.url))),
        Err(e) => {
            logdim!("could not read #{tracker}: {}", e.first_line());
            None
        }
    }
}

fn read_for_write(repo: &Repo, tracker: i64) -> Option<(String, String)> {
    match repo.record_failed_write(repo.read_issue(tracker)) {
        Ok(issue) => Some((issue.body_text().to_string(), home_of(&issue.url))),
        Err(e) => {
            logdim!("could not read #{tracker}: {}", e.first_line());
            None
        }
    }
}

/// This repository's address, taken from the tracker's own url by dropping the
/// `/issues/29` off the end.
///
/// Read rather than assembled from `owner/repo`, because the host is half the
/// answer: a link to `elsewhere.example/me/mine/issues/7` shares the path and
/// is not this repository's issue. Empty when there is no url to read, which
/// holds every linked item rather than guessing at one.
fn home_of(url: &str) -> String {
    match url.rfind("/issues/") {
        Some(at) => url[..at].to_string(),
        None => String::new(),
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

    /// Where the tests' own repository lives, as `read` would work it out.
    const HOME: &str = "https://github.com/me/mine";

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

    /// Four spaces outside a list is a code block on GitHub, and filing an
    /// issue for somebody's example is the failure the fence check exists to
    /// prevent.
    #[test]
    fn an_indented_code_block_is_not_a_checklist() {
        let body = "\
Write the parts like this:

    - [ ] an example, not an item

- [ ] real
  - [ ] nested
- plain bullet
    - [ ] nested under a bullet
";
        assert_eq!(vec!["real", "nested", "nested under a bullet"], texts(body));
    }

    /// Four spaces is measured from the enclosing item's content, not from the
    /// margin: under `- outer` the content starts in column 2, so six spaces
    /// are an example inside that item and two are a nested task.
    #[test]
    fn code_indented_inside_a_list_item_is_still_code() {
        let body = "\
- outer

      - [ ] an example, not an item

  - [ ] nested
- plain
    - [ ] nested under a bullet
        - [ ] and under that one
";
        assert_eq!(
            vec!["nested", "nested under a bullet", "and under that one"],
            texts(body)
        );
    }

    /// GitHub renders the inside of these verbatim, so a checkbox in one is
    /// text somebody is showing.
    #[test]
    fn an_item_inside_raw_html_is_not_one() {
        let body = "\
- [ ] real

<pre>
- [ ] not real
</pre>

<textarea>
- [ ] also not real
</textarea>

- [ ] real again
";
        assert_eq!(vec!["real", "real again"], texts(body));
    }

    /// Markdown inside a block tag is not markdown: GitHub prints the checkbox
    /// as the text it is. The block ends at a blank line and not at the closing
    /// tag, which is what keeps the `<details>` a tracker is often written in
    /// working.
    #[test]
    fn an_item_inside_a_block_tag_is_not_one() {
        let body = "\
<div>
- [ ] not real
</div>

<details>
<summary>the parts</summary>

- [ ] real
</details>
";
        assert_eq!(vec!["real"], texts(body));
    }

    /// More than four columns after the marker put the checkbox in an indented
    /// code block inside the item, which is how somebody writes down the syntax
    /// itself.
    #[test]
    fn a_checkbox_pushed_past_its_own_content_column_is_code() {
        assert_eq!(Vec::<String>::new(), texts("-     [ ] an example\n"));
        assert_eq!(vec!["real"], texts("-    [ ] real\n"));
    }

    /// A fence four columns in is a fence somebody is showing, so it neither
    /// opens a block nor closes the one it sits in.
    #[test]
    fn a_fence_indented_into_code_neither_opens_nor_closes() {
        let body = "```\n- [ ] not real\n    ```\n- [ ] still not real\n";
        assert_eq!(Vec::<String>::new(), texts(body));

        let body = "Like this:\n\n    ```\n- [ ] real\n";
        assert_eq!(vec!["real"], texts(body));
    }

    /// The items somebody decided against are often kept in a comment. GitHub
    /// renders none of it, so neither does this.
    #[test]
    fn an_item_inside_an_html_comment_is_not_one() {
        let body = "\
- [ ] real

<!--
- [ ] not real
-->

- [ ] real again
<!-- - [ ] on one line, closed -->
- [ ] last
";
        assert_eq!(vec!["real", "real again", "last"], texts(body));
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

    /// An item is very often written down as the change that closes it.
    /// `resolve` answers for a pull request already, so reading only `/issues/`
    /// would file a second issue for work in flight.
    #[test]
    fn a_link_to_a_pull_request_is_a_reference_too() {
        let items = parse(
            "- [ ] one https://github.com/me/mine/pull/42\n\
             - [ ] two https://github.com/me/mine/pull/43/files\n\
             - [ ] three https://github.com/other/thing/pull/44\n",
        );
        assert_eq!(Some(42), items[0].reference.as_ref().unwrap().local(HOME));
        assert_eq!(Some(43), items[1].reference.as_ref().unwrap().local(HOME));
        assert_eq!(None, items[2].reference.as_ref().unwrap().local(HOME));
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
        assert_eq!(None, reference.local(HOME));
        assert_eq!(Some(7), reference.local("https://github.com/other/thing"));
    }

    /// A bare number can only mean this repository, so it needs no slug.
    #[test]
    fn a_bare_number_resolves_wherever_it_is_read() {
        let items = parse("- [ ] work #7\n");
        assert_eq!(Some(7), items[0].reference.as_ref().unwrap().local(""));
    }

    /// The path is half the answer. Another host serving `me/mine` is somebody
    /// else's, and adopting it would tick a local issue nobody named.
    #[test]
    fn a_link_to_the_same_path_on_another_host_is_not_this_repository() {
        for url in [
            "https://gitlab.example/me/mine/issues/7",
            "https://github.com/mirror/me/mine/issues/7",
        ] {
            let items = parse(&format!("- [ ] see {url}\n"));
            let reference = items[0].reference.as_ref().expect("a reference");
            assert_eq!(None, reference.local(HOME), "{url}");
        }
    }

    /// http and https to the same issue are the same issue.
    #[test]
    fn the_scheme_is_not_what_makes_a_link_somebody_elses() {
        let items = parse("- [ ] see http://github.com/me/mine/issues/7\n");
        assert_eq!(Some(7), items[0].reference.as_ref().unwrap().local(HOME));
    }

    /// An issue url with the tail taken off is the address every other link is
    /// measured against.
    #[test]
    fn home_is_read_off_the_trackers_own_url() {
        assert_eq!(HOME, home_of("https://github.com/me/mine/issues/29"));
        assert_eq!("", home_of(""));
    }

    /// A number in backticks is somebody writing about it, and the reference
    /// they meant is the one outside.
    #[test]
    fn a_number_in_a_code_span_names_nothing() {
        let items = parse(
            "- [ ] Handle the literal `#12`, tracked in #34\n\
             - [ ] Only ``a #12 in a double span``\n",
        );
        assert_eq!(Some(34), items[0].reference.as_ref().map(|r| r.number));
        assert_eq!(None, items[1].reference);
    }

    /// A comment is not rendered, so a number left in one is a note to a person
    /// and never the issue the item is about. Ticking the box because that
    /// issue happens to be closed would call somebody's work done.
    #[test]
    fn a_number_in_a_comment_names_nothing() {
        let items = parse(
            "- [ ] ship it <!-- old note: #7 -->\n\
             - [ ] and this one <!-- #7 --> #8\n",
        );
        assert_eq!(None, items[0].reference);
        assert_eq!(Some(8), items[1].reference.as_ref().map(|r| r.number));
    }

    /// The `#8` in an address is a fragment of it. Only a link that names an
    /// issue by path is read as one.
    #[test]
    fn a_fragment_in_a_link_is_not_an_issue_number() {
        let items = parse(
            "- [ ] update [docs](https://example.com/guide/#8)\n\
             - [ ] see https://example.com/guide#9 and #10\n",
        );
        assert_eq!(None, items[0].reference);
        assert_eq!(Some(10), items[1].reference.as_ref().map(|r| r.number));
    }

    /// GitHub links both of these without a url, so an item that carries one is
    /// an item that already names its issue. Reading neither filed a second
    /// issue for work the tracker had already written down.
    #[test]
    fn the_shorthands_github_links_are_references_too() {
        let items = parse(
            "- [ ] one me/mine#12\n\
             - [ ] two other/thing#13\n\
             - [ ] three GH-14\n",
        );
        assert_eq!(Some(12), items[0].reference.as_ref().unwrap().local(HOME));
        let foreign = items[1].reference.as_ref().expect("a reference");
        assert_eq!(None, foreign.local(HOME), "somebody else's repository");
        assert_eq!("other/thing#13", foreign.names());
        assert_eq!(Some(14), items[2].reference.as_ref().unwrap().local(HOME));
    }

    /// The shorthand leaves the host out, so it means this repository wherever
    /// this repository is served from. The path still has to be this one's.
    #[test]
    fn a_shorthand_is_read_against_this_repositorys_path() {
        let items = parse("- [ ] work me/mine#7\n");
        let reference = items[0].reference.as_ref().expect("a reference");
        assert_eq!(Some(7), reference.local("https://ghe.example/me/mine"));
        assert_eq!(None, reference.local("https://github.com/me/other"));
        assert_eq!(None, reference.local(""), "no address to measure against");
    }

    /// A link's label captions its destination. Reading the label first turned
    /// another repository's issue into a bare local number, which is exactly
    /// the adoption the foreign repository check exists to refuse.
    #[test]
    fn a_link_is_read_from_its_destination_and_not_its_label() {
        let items = parse(
            "- [ ] [other/widgets #7](https://github.com/other/widgets/issues/7)\n\
             - [ ] [me/mine #7](https://github.com/me/mine/issues/7)\n",
        );
        let foreign = items[0].reference.as_ref().expect("a reference");
        assert!(
            matches!(foreign.origin, Origin::Url(_)),
            "the destination, not the label"
        );
        assert_eq!(None, foreign.local(HOME));
        assert_eq!(Some(7), items[1].reference.as_ref().unwrap().local(HOME));
    }

    /// Nested brackets and escaped closing brackets are both valid inside a
    /// link label. Stopping at either one exposes the label's local-looking
    /// number and hides the foreign destination.
    #[test]
    fn complex_link_labels_still_read_the_destination() {
        let items = parse(
            "- [ ] [see [#7]](https://github.com/other/widgets/issues/8)\n\
             - [ ] [see \\] #7](https://github.com/other/widgets/issues/8)\n",
        );
        for item in items {
            let reference = item.reference.expect("the destination");
            assert_eq!(8, reference.number);
            assert!(matches!(reference.origin, Origin::Url(_)));
            assert_eq!(None, reference.local(HOME));
        }
    }

    #[test]
    fn one_child_referenced_by_several_items_is_worked_once() {
        assert_eq!(vec![8, 9], unique_children(vec![8, 8, 9, 8]));
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
        shape(body, HOME, max).into_iter().map(|(_, s)| s).collect()
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

    /// A tick means the work landed, and a decline is not that.
    ///
    /// With `close_skipped = true`, the default, an item whose issue both
    /// agents declined is closed as not planned. Ticking it writes "this was
    /// done" into somebody's checklist, and since spar never unchecks a box,
    /// correcting it is left to them.
    #[test]
    fn an_item_closed_as_not_planned_is_not_ticked() {
        let issue = |state: &str, reason: Option<&str>| Issue {
            number: 12,
            title: "migrate the cache to v2".into(),
            body: None,
            state: state.into(),
            state_reason: reason.map(str::to_string),
            url: String::new(),
            labels: Vec::new(),
        };

        assert!(matches!(
            issue_action(&issue("CLOSED", Some("not_planned"))),
            Action::Hold(_)
        ));
        assert_eq!(
            Action::Tick(12),
            issue_action(&issue("CLOSED", Some("completed")))
        );
        assert_eq!(
            Action::Tick(12),
            issue_action(&issue("CLOSED", None)),
            "an issue closed before GitHub recorded a reason still ticks"
        );
        assert_eq!(Action::Adopt(12), issue_action(&issue("OPEN", None)));
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
        let shapes: Vec<Shape> = shape(body, HOME, 5).into_iter().map(|(_, s)| s).collect();
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

    // -- the guard before a write -----------------------------------------

    /// The bytes of the line are not the whole of what it means. Somebody
    /// fencing an item mid-run is saying not this one, and the raw text is
    /// still there to match.
    #[test]
    fn a_line_that_stopped_being_an_item_is_not_written_to() {
        let raw = "- [ ] ship it";
        assert!(still_an_item("intro\n\n- [ ] ship it\n", raw));
        assert!(!still_an_item("```\n- [ ] ship it\n```\n", raw));
        assert!(!still_an_item("<!--\n- [ ] ship it\n-->\n", raw));
        assert!(!still_an_item("- [ ] something else\n", raw));
        assert!(
            !still_an_item("- [ ] ship it\n- [ ] ship it\n", raw),
            "two alike is a line the edit could go to either of"
        );
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
