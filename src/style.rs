//! Two gates over every string spar sends to GitHub.
//!
//! **Style.** Models comply unreliably with negative instructions, especially
//! over a long run, so prompting is necessary but not sufficient. Every commit
//! message, PR body, and comment is scrubbed deterministically and then
//! re-verified. A leak is a hard error, not a warning.
//!
//! **Concision.** The reader of a PR is a human with other work. Model prose
//! defaults to three paragraphs where one sentence would do, and asking nicely
//! has the same reliability problem as asking for no em-dashes. So spar
//! composes every comment itself from structured fields and clips each field to
//! a budget, rather than forwarding whatever the model felt like writing.

use std::sync::LazyLock;

use regex::Regex;

/// Figure dash, en dash, em dash, horizontal bar. The Python original caught
/// only en and em; a model that reaches for U+2015 should not slip through.
const DASHES: &str = r"[\x{2012}-\x{2015}]";

static ATTRIBUTION_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?im)^\s*(?:",
        r"co-authored-by:\s*(?:claude|codex|openai|chatgpt|anthropic|gpt).*",
        r"|\x{1F916}?\s*generated with .*",
        r"|.*\bwritten by (?:claude|codex|chatgpt|an? ai)\b.*",
        r"|assisted[- ]by:.*",
        r")\s*$",
    ))
    .expect("attribution line pattern")
});

static ATTRIBUTION_INLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\b(?:",
        r"generated (?:with|by) (?:claude|codex|openai|chatgpt|ai)",
        r"|(?:written|authored|created) (?:with|by) (?:claude|codex|chatgpt|ai)",
        r"|with the help of (?:claude|codex|chatgpt|ai)",
        r"|using (?:claude code|codex|chatgpt)",
        r"|ai[- ]generated",
        r"|as an ai\b",
        r")",
    ))
    .expect("attribution inline pattern")
});

static DASH_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"[ \t]*{DASHES}[ \t]*")).expect("dash pattern"));

static ANY_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(DASHES).expect("dash class"));

static TRAILING_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)[ \t]+$").expect("trailing space pattern"));

static BLANK_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("blank run pattern"));

static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}#{1,6}\s+\S").expect("heading pattern"));

/// Headings that only announce that a body follows. Dropping them costs the
/// reader nothing and saves them a line.
static NOISE_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s{0,3}#{1,6}\s*(summary|description|overview|context|details?|background)\s*:?\s*$",
    )
    .expect("noise heading pattern")
});

/// Everything the two gates need to know. Mirrors the `[style]` config block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    pub ban_em_dash: bool,
    pub ban_ai_attribution: bool,
    /// Enforce the length budgets below. Off means model prose passes through
    /// at whatever length it arrived at.
    pub terse: bool,
    /// A finding's explanatory detail, as shown in the PR thread.
    pub max_detail_chars: usize,
    /// A one-line verdict or disposition summary.
    pub max_summary_chars: usize,
    /// A PR body.
    pub max_body_chars: usize,
    /// A filed issue's body.
    pub max_issue_body_chars: usize,
    /// A finding title, issue title, or PR title.
    pub max_title_chars: usize,
    /// How much of its own working spar narrates into a pull request thread.
    pub pr_comments: crate::config::PrComments,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            ban_em_dash: true,
            ban_ai_attribution: true,
            terse: true,
            max_detail_chars: 320,
            max_summary_chars: 200,
            max_body_chars: 900,
            max_issue_body_chars: 4000,
            max_title_chars: 90,
            pr_comments: crate::config::PrComments::Outcome,
        }
    }
}

impl Style {
    /// Style rules only, no length budgets. Used for text spar composed itself
    /// and has already sized.
    pub fn permissive() -> Self {
        Self {
            terse: false,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Style gate
// ---------------------------------------------------------------------------

/// Remove banned style artifacts. Idempotent: scrubbing scrubbed text is a
/// no-op, which matters because text passes through here more than once.
pub fn scrub(text: &str, style: &Style) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = text.to_string();

    if style.ban_ai_attribution {
        out = ATTRIBUTION_LINE.replace_all(&out, "").into_owned();
        out = ATTRIBUTION_INLINE.replace_all(&out, "").into_owned();
        out = out.replace('\u{1F916}', "");
    }

    if style.ban_em_dash {
        // "a - b" becomes "a, b". Bounded to spaces and tabs so a dash at the
        // end of a line joins two lines with a comma instead of swallowing the
        // paragraph break after it.
        out = DASH_RUN.replace_all(&out, ", ").into_owned();
    }

    out = TRAILING_SPACE.replace_all(&out, "").into_owned();
    out = BLANK_RUN.replace_all(&out, "\n\n").into_owned();
    out.trim().to_string()
}

/// Anything the scrub should have caught. Used as a post-check, so that a
/// pattern the scrub cannot fix becomes a loud failure rather than a leak.
pub fn violations(text: &str, style: &Style) -> Vec<String> {
    let mut bad = Vec::new();
    if style.ban_em_dash && ANY_DASH.is_match(text) {
        bad.push("em/en dash present".to_string());
    }
    if style.ban_ai_attribution
        && (ATTRIBUTION_LINE.is_match(text) || ATTRIBUTION_INLINE.is_match(text))
    {
        bad.push("AI attribution present".to_string());
    }
    bad
}

// ---------------------------------------------------------------------------
// Concision gate
// ---------------------------------------------------------------------------

/// Collapse to a single line of single-spaced words.
///
/// For a field that is displayed inline, such as a finding title or a one-line
/// verdict. A model that returns a paragraph there would otherwise break the
/// layout of everything around it.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `max` characters, preferring a sentence boundary.
///
/// Cutting mid-sentence and marking it with an ellipsis is a last resort: a
/// clipped finding still has to be actionable, and the first sentence of a
/// review comment almost always is.
pub fn clip(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if max == 0 {
        return trimmed.to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max {
        return trimmed.to_string();
    }

    let window = &chars[..max];

    // The last sentence end inside the budget, if it keeps enough of the text
    // to still be worth reading.
    let mut sentence_end = None;
    for (i, c) in window.iter().enumerate() {
        // Look at the real next character, not `window`'s. A period landing on
        // the last budget character has text after it; treating the end of the
        // window as the end of a sentence cuts mid-path with no ellipsis, so
        // "src/repo.rs:412" is silently served as "src/repo." and reads as
        // finished prose.
        if matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) {
            sentence_end = Some(i + 1);
        }
    }
    if let Some(cut) = sentence_end {
        if cut * 2 >= max {
            return window[..cut]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string();
        }
    }

    // Otherwise the last word boundary, marked so the reader knows there is
    // more where this came from. The mark is inside the budget, never added to
    // it: a caller that asked for at most N characters gets at most N.
    const MARK: &str = "...";
    if max <= MARK.len() {
        return window.iter().collect::<String>().trim_end().to_string();
    }
    let budget = max - MARK.len();
    let mut end = budget;
    while end > 0 && !window[end - 1].is_whitespace() {
        end -= 1;
    }
    if end == 0 {
        end = budget;
    }
    let mut out: String = window[..end]
        .iter()
        .collect::<String>()
        .trim_end()
        .to_string();
    out.push_str(MARK);
    out
}

/// Drop a heading whose section holds nothing, and the bare "## Summary" style
/// heading that only announces the body underneath it.
pub fn strip_empty_sections(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep: Vec<&str> = Vec::with_capacity(lines.len());

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if HEADING.is_match(line) {
            // Everything up to the next heading is this section's body.
            let mut j = i + 1;
            while j < lines.len() && !HEADING.is_match(lines[j]) {
                j += 1;
            }
            let body_is_empty = lines[i + 1..j].iter().all(|l| l.trim().is_empty());
            let only_heading = lines.iter().filter(|l| HEADING.is_match(l)).count() == 1;

            if body_is_empty {
                i = j; // drop the heading and the blank lines under it
                continue;
            }
            if only_heading && NOISE_HEADING.is_match(line) {
                i += 1; // drop the label, keep the body
                continue;
            }
        }
        keep.push(line);
        i += 1;
    }

    let joined = keep.join("\n");
    BLANK_RUN.replace_all(&joined, "\n\n").trim().to_string()
}

/// The full outbound treatment for a block of model prose: strip structural
/// noise, then clip to a budget.
pub fn tighten(text: &str, max: usize, style: &Style) -> String {
    if !style.terse {
        return text.trim().to_string();
    }
    clip(&strip_empty_sections(text), max)
}

/// A filed issue's body.
///
/// An issue is a work item. Somebody picks it up cold, possibly months later,
/// with none of the context the pull request thread had, so the rules that keep
/// a comment short are the wrong rules here. Two things follow.
///
/// A fenced code block is never truncated and never counts against the budget.
/// A snippet cut in half is worse than useless: it is broken markdown and a
/// misleading fragment of code. Steps to reproduce, a stack trace, the offending
/// function: those are the reason the issue is worth filing at all.
///
/// And when prose does have to be dropped, whole blocks go from the end rather
/// than a sentence being cut mid-word. What survives is complete.
pub fn issue_body(text: &str, style: &Style) -> String {
    if !style.terse {
        return text.trim().to_string();
    }
    let cleaned = strip_empty_sections(text);
    let blocks = split_blocks(&cleaned);

    // A runaway model pasting an entire file is still worth stopping, so code
    // is exempt from the prose budget but not from a far looser ceiling.
    let ceiling = style.max_issue_body_chars.saturating_mul(4);

    let mut kept: Vec<&Block> = Vec::new();
    let mut prose = 0usize;
    let mut total = 0usize;
    for block in &blocks {
        let len = block.text.chars().count();
        let over_prose = !block.code && prose + len > style.max_issue_body_chars;
        let over_ceiling = total + len > ceiling;
        if (over_prose || over_ceiling) && !kept.is_empty() {
            break;
        }
        if !block.code {
            prose += len;
        }
        total += len;
        kept.push(block);
    }

    kept.iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string()
}

struct Block {
    text: String,
    code: bool,
}

/// Split into paragraphs, keeping every fenced code block whole however many
/// blank lines it contains.
fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;
    let mut fence_block = false;

    let flush = |lines: &mut Vec<&str>, code: bool, out: &mut Vec<Block>| {
        let joined = lines.join("\n");
        if !joined.trim().is_empty() {
            out.push(Block {
                text: joined.trim_end().to_string(),
                code,
            });
        }
        lines.clear();
    };

    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence {
            if in_fence {
                current.push(line);
                in_fence = false;
                flush(&mut current, true, &mut blocks);
                fence_block = false;
                continue;
            }
            // A fence starts here, so whatever came before is its own block.
            flush(&mut current, false, &mut blocks);
            in_fence = true;
            fence_block = true;
            current.push(line);
            continue;
        }
        if in_fence {
            current.push(line);
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut current, false, &mut blocks);
        } else {
            current.push(line);
        }
    }
    // An unterminated fence is still kept whole rather than split.
    flush(&mut current, fence_block, &mut blocks);
    blocks
}

/// A finding title, issue title, or PR title: always one line, always short.
pub fn title(text: &str, style: &Style) -> String {
    let flat = one_line(text);
    if style.terse {
        clip(&flat, style.max_title_chars)
    } else {
        flat
    }
}

/// Capitalise the first letter, so a model's fragment reads as a sentence when
/// spar sets it after one of its own.
pub fn sentence(text: &str, style: &Style) -> String {
    let one = summary(text, style);
    let mut chars = one.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => one,
    }
}

/// A one-sentence verdict or disposition reason.
pub fn summary(text: &str, style: &Style) -> String {
    let flat = one_line(text);
    if style.terse {
        clip(&flat, style.max_summary_chars)
    } else {
        flat
    }
}

/// A finding's explanation, as it appears in the PR thread. Kept on one line so
/// a bullet stays a bullet.
pub fn detail(text: &str, style: &Style) -> String {
    let flat = one_line(text);
    if style.terse {
        clip(&flat, style.max_detail_chars)
    } else {
        flat
    }
}

/// An issue or PR body. Multi-line is fine here; bloat is not.
pub fn body(text: &str, style: &Style) -> String {
    tighten(text, style.max_body_chars, style)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Style {
        Style::default()
    }

    // -- style gate ------------------------------------------------------

    #[test]
    fn em_dash_removed() {
        let out = scrub(
            "Fix the parser, it was broken \u{2014} badly \u{2014} on empty input.",
            &s(),
        );
        assert!(!out.contains('\u{2014}'));
        assert!(violations(&out, &s()).is_empty());
    }

    #[test]
    fn en_dash_removed() {
        assert!(!scrub("range 1 \u{2013} 5", &s()).contains('\u{2013}'));
    }

    #[test]
    fn horizontal_bar_removed() {
        assert!(violations(&scrub("a \u{2015} b", &s()), &s()).is_empty());
    }

    #[test]
    fn coauthor_trailer_stripped() {
        let out = scrub(
            "Add retry logic\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>\n",
            &s(),
        );
        assert!(!out.contains("Co-Authored-By"));
        assert!(out.contains("Add retry logic"));
    }

    #[test]
    fn generated_with_footer_stripped() {
        let out = scrub(
            "Fix bug\n\n\u{1F916} Generated with [Claude Code](https://claude.com)\n",
            &s(),
        );
        assert!(violations(&out, &s()).is_empty(), "{out}");
        assert!(out.contains("Fix bug"));
    }

    #[test]
    fn inline_attribution_stripped() {
        let out = scrub("This patch was written by Claude to fix the leak.", &s());
        assert!(violations(&out, &s()).is_empty(), "{out}");
    }

    #[test]
    fn scrub_is_idempotent() {
        let once = scrub("A \u{2014} B\n\nCo-Authored-By: Codex <x@y.z>", &s());
        assert_eq!(once, scrub(&once, &s()));
    }

    #[test]
    fn violations_detected_before_scrub() {
        assert!(!violations("a \u{2014} b", &s()).is_empty());
        assert!(!violations("Co-Authored-By: Claude <a@b.c>", &s()).is_empty());
    }

    #[test]
    fn legitimate_prose_survives() {
        let out = scrub("Refactor the AI-facing endpoint handler for clarity.", &s());
        assert!(out.contains("endpoint handler"), "{out}");
    }

    #[test]
    fn disabled_rules_are_respected() {
        let off = Style {
            ban_em_dash: false,
            ban_ai_attribution: false,
            ..s()
        };
        let text = "a \u{2014} b\nCo-Authored-By: Claude <x@y.z>";
        assert!(scrub(text, &off).contains('\u{2014}'));
        assert!(violations(text, &off).is_empty());
    }

    #[test]
    fn dash_at_end_of_line_does_not_swallow_the_paragraph_break() {
        let out = scrub("first line \u{2014}\n\nsecond paragraph", &s());
        assert!(out.contains("\n\n"), "{out:?}");
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!("", scrub("", &s()));
    }

    // -- concision gate --------------------------------------------------

    #[test]
    fn one_line_flattens() {
        assert_eq!("a b c", one_line("  a\n\n b\t c  "));
    }

    #[test]
    fn clip_leaves_short_text_alone() {
        assert_eq!("short", clip("short", 40));
    }

    #[test]
    fn clip_prefers_a_sentence_boundary() {
        let text = "The loop never terminates. It also leaks a file descriptor on every pass.";
        assert_eq!("The loop never terminates.", clip(text, 40));
    }

    #[test]
    fn clip_falls_back_to_a_word_boundary() {
        let out = clip("supercalifragilistic wording that runs on and on", 25);
        assert!(out.ends_with("..."), "{out}");
        assert!(out.chars().count() <= 25, "{out}");
        assert!(!out.contains("wording that runs"), "{out}");
    }

    #[test]
    fn clip_never_exceeds_the_budget() {
        for max in 1..60 {
            let out = clip("one two three four five six seven eight nine ten.", max);
            assert!(out.chars().count() <= max, "max={max} out={out:?}");
        }
    }

    #[test]
    fn clip_handles_multibyte_text() {
        let out = clip(&"\u{1f600}".repeat(50), 10);
        assert!(out.chars().count() <= 10, "{out}");
    }

    /// The sentence-end scan used to look at the last character of the *budget*
    /// rather than of the *text*, so a period landing exactly on the boundary
    /// read as the end of a sentence. The result came back with no ellipsis, so
    /// a truncated file path looked like finished prose.
    #[test]
    fn a_period_on_the_budget_boundary_is_not_a_sentence_end() {
        assert_ne!(
            "Version 1.",
            clip("Version 1.4 of the parser mishandles input", 10)
        );
        assert_ne!(
            "Panic in src/style.",
            clip("Panic in src/style.rs when the budget lands mid word", 19)
        );
    }

    #[test]
    fn an_unmarked_clip_really_did_end_a_sentence() {
        // The only way to come back without an ellipsis is to stop where the
        // author stopped.
        for max in 4..80 {
            let text = "First sentence here. Second one follows it. Third trails off";
            let out = clip(text, max);
            if out.len() < text.len() && !out.ends_with("...") {
                assert!(
                    out.ends_with('.') || out.ends_with('!') || out.ends_with('?'),
                    "max={max} out={out:?}"
                );
                let next = text[out.len()..].chars().next();
                assert!(
                    next.is_none_or(|c| c.is_whitespace()),
                    "max={max} cut mid-token before {next:?}: {out:?}"
                );
            }
        }
    }

    #[test]
    fn clip_ignores_a_decimal_point_as_a_sentence_end() {
        let text = "Version 1.4 of the parser mishandles empty input badly and loops.";
        assert_ne!("Version 1.", clip(text, 30));
    }

    #[test]
    fn empty_sections_are_dropped() {
        let out = strip_empty_sections("## Context\n\n## Proposal\n\nDo the thing.\n");
        assert!(!out.contains("Context"), "{out}");
        assert!(out.contains("Do the thing."), "{out}");
    }

    #[test]
    fn a_lone_label_heading_is_dropped() {
        assert_eq!(
            "The retry never fires.",
            strip_empty_sections("## Summary\n\nThe retry never fires.")
        );
    }

    #[test]
    fn real_headings_survive_when_there_are_several() {
        let text = "## Summary\n\nA thing.\n\n## Repro\n\nRun it.";
        let out = strip_empty_sections(text);
        assert!(
            out.contains("## Summary") && out.contains("## Repro"),
            "{out}"
        );
    }

    #[test]
    fn terse_off_leaves_length_alone() {
        let loose = Style {
            terse: false,
            ..s()
        };
        let long = "word ".repeat(400);
        assert_eq!(long.trim(), detail(&long, &loose));
    }

    #[test]
    fn detail_is_capped_and_single_line() {
        let out = detail(
            &format!("first line\nsecond line\n{}", "filler ".repeat(200)),
            &s(),
        );
        assert!(!out.contains('\n'));
        assert!(out.chars().count() <= s().max_detail_chars);
    }

    #[test]
    fn title_is_capped_and_single_line() {
        let out = title(
            "a very\nlong\ttitle that keeps going ".repeat(20).as_str(),
            &s(),
        );
        assert!(!out.contains('\n'));
        assert!(out.chars().count() <= s().max_title_chars);
    }

    #[test]
    fn body_keeps_structure_but_bounds_length() {
        let text = format!(
            "## Summary\n\nreal content here.\n\n{}",
            "more prose. ".repeat(300)
        );
        let out = body(&text, &s());
        assert!(
            out.chars().count() <= s().max_body_chars,
            "{}",
            out.chars().count()
        );
        assert!(out.contains("real content here"), "{out}");
    }
}

#[cfg(test)]
mod sentence_tests {
    use super::*;

    #[test]
    fn a_fragment_reads_as_a_sentence() {
        assert_eq!(
            "The caller already validates it.",
            sentence("the caller already validates it.", &Style::default())
        );
    }

    #[test]
    fn an_already_capitalised_one_is_untouched() {
        assert_eq!(
            "Already fine.",
            sentence("Already fine.", &Style::default())
        );
    }

    #[test]
    fn empty_stays_empty_rather_than_panicking() {
        assert_eq!("", sentence("   ", &Style::default()));
    }

    #[test]
    fn a_multibyte_first_character_does_not_panic() {
        assert_eq!("Ärger", sentence("ärger", &Style::default()));
    }
}

#[cfg(test)]
mod issue_body_tests {
    use super::*;

    fn s() -> Style {
        Style::default()
    }

    fn fences(text: &str) -> usize {
        text.lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count()
    }

    /// The whole point. A snippet cut in half is broken markdown and a
    /// misleading fragment of the code somebody is being asked to fix.
    #[test]
    fn a_code_block_is_never_truncated() {
        let code = (0..400)
            .map(|n| format!("    line_{n}();"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("It spins forever.\n\n```rust\n{code}\n```\n\nThat is the loop.");
        let out = issue_body(&text, &s());

        assert!(
            out.contains("line_0();") && out.contains("line_399();"),
            "the block was cut"
        );
        assert_eq!(0, fences(&out) % 2, "left an unclosed fence:\n{out}");
    }

    /// Why there are two functions rather than one budget. The comment path
    /// cuts wherever the character count runs out, which on a snippet means an
    /// unclosed fence and a misleading half of the code.
    #[test]
    fn the_comment_budget_would_have_mangled_the_same_snippet() {
        let code = (0..400)
            .map(|n| format!("    line_{n}();"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("It spins forever.\n\n```rust\n{code}\n```");

        let as_comment = body(&text, &s());
        assert_ne!(0, fences(&as_comment) % 2, "a comment cuts the fence open");

        let as_issue = issue_body(&text, &s());
        assert_eq!(0, fences(&as_issue) % 2, "an issue keeps it closed");
    }

    /// Code is exempt from the budget, so a long snippet cannot squeeze out the
    /// prose that explains it.
    #[test]
    fn a_long_snippet_does_not_evict_the_explanation() {
        let code = "x();\n".repeat(1500);
        let text = format!(
            "Reproduction:\n\n1. Call connect twice.\n2. Watch the retry count.\n\n```\n{code}```\n\nSuggested fix: bound the loop."
        );
        let out = issue_body(&text, &s());
        assert!(out.contains("Call connect twice"), "{out}");
        assert!(out.contains("Suggested fix"), "the tail survived");
    }

    #[test]
    fn steps_to_reproduce_survive_intact() {
        let text = "The retry never fires.\n\n1. Start the daemon.\n2. Kill the peer.\n3. Observe connectedToElectrum stays true.\n\nsrc/electrum/index.ts:289 is where the guard is.";
        assert_eq!(text, issue_body(text, &s()));
    }

    #[test]
    fn a_body_within_budget_is_untouched() {
        let text = "One paragraph.\n\nAnd another.";
        assert_eq!(text, issue_body(text, &s()));
    }

    /// When prose does have to go, whole blocks go from the end. Nothing is cut
    /// mid-sentence and nothing gains an ellipsis.
    #[test]
    fn overlong_prose_drops_whole_blocks_from_the_end() {
        let para = |n: usize| format!("Paragraph {n}. {}", "filler words here. ".repeat(30));
        let text = (0..20).map(para).collect::<Vec<_>>().join("\n\n");
        let out = issue_body(&text, &s());

        assert!(out.starts_with("Paragraph 0."), "{out}");
        assert!(!out.contains("..."), "no mid-sentence cut: {out}");
        assert!(
            out.trim_end().ends_with('.'),
            "ends on a complete block: {out}"
        );
        assert!(
            out.chars().count() <= s().max_issue_body_chars + 400,
            "{}",
            out.chars().count()
        );
    }

    /// An issue gets far more room than a pull request comment, because it is
    /// read cold by somebody with none of the context.
    #[test]
    fn an_issue_gets_much_more_room_than_a_comment() {
        let text = "word ".repeat(500);
        assert!(issue_body(&text, &s()).len() > body(&text, &s()).len() * 2);
    }

    #[test]
    fn a_single_block_over_budget_is_kept_rather_than_mangled() {
        let text = format!("```\n{}\n```", "y();\n".repeat(3000));
        let out = issue_body(&text, &s());
        assert!(!out.is_empty());
        assert_eq!(0, fences(&out) % 2, "{}", &out[..80.min(out.len())]);
    }

    #[test]
    fn an_unterminated_fence_is_still_kept_whole() {
        let text = "Here is the code:\n\n```rust\nfn broken() {\n    loop {}";
        let out = issue_body(text, &s());
        assert!(out.contains("fn broken()"), "{out}");
    }

    #[test]
    fn terse_off_leaves_an_issue_body_completely_alone() {
        let loose = Style {
            terse: false,
            ..s()
        };
        let text = "a".repeat(50_000);
        assert_eq!(text, issue_body(&text, &loose));
    }

    #[test]
    fn issue_body_is_idempotent() {
        let text = format!(
            "Explanation.\n\n```\n{}\n```\n\n{}",
            "z();\n".repeat(50),
            "more prose. ".repeat(600)
        );
        let once = issue_body(&text, &s());
        assert_eq!(once, issue_body(&once, &s()));
    }
}
