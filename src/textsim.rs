//! Deciding whether two pieces of prose make the same point.
//!
//! Everything spar deduplicates used to compare strings exactly. Two agents
//! describing one defect, or two runs a week apart describing it again, never
//! phrase it identically, so exact matching let duplicates straight through: a
//! follow-up filed twice as two issues, and an issue closed with two comments
//! saying the same thing in different words.
//!
//! This is deliberately shallow. No stemming, no embeddings, nothing that needs
//! a model. It compares the significant words two texts share, which is enough
//! to catch a rewording and cheap enough to run on every finding.
//!
//! The thresholds lean toward calling things the same, because the two errors
//! are not symmetric. Treating one defect as two files a duplicate, which is
//! the complaint. Treating two defects as one still records the second, as a
//! comment on the first issue rather than an issue of its own, so nothing is
//! lost and a person can split them.

use std::collections::BTreeSet;

/// Words too common to say anything about what a text is about.
const NOISE: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "which", "when", "then", "than", "from", "into",
    "但", "are", "was", "were", "has", "have", "had", "not", "but", "its", "it's", "their", "they",
    "there", "here", "same", "still", "also", "only", "any", "all", "can", "will", "would",
    "should", "could", "does", "did", "done", "being", "been", "because", "while", "after",
    "before", "since", "each", "every", "some", "such", "them", "these", "those", "what", "where",
    "who", "why", "how", "you", "your", "our", "one", "two", "new", "now", "may", "might", "must",
    "issue", "issues", "bug", "fix", "fixes", "fixed", "change", "changes", "changed",
];

/// Significant words, lowercased. Punctuation goes, short words go, and words
/// that appear in every bug report go.
pub fn tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 2)
        .filter(|word| !NOISE.contains(word))
        .map(str::to_string)
        .collect()
}

/// How much of the smaller text's vocabulary the larger one already contains,
/// from 0.0 to 1.0.
///
/// Containment rather than Jaccard on purpose: a one line title and a paragraph
/// describing the same defect should read as the same point, and Jaccard
/// punishes them for differing in length.
pub fn containment(a: &str, b: &str) -> f64 {
    let (left, right) = (tokens(a), tokens(b));
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let shared = left.intersection(&right).count() as f64;
    let smaller = left.len().min(right.len()) as f64;
    shared / smaller
}

/// Words the two texts share. Used to insist on real overlap rather than one
/// lucky word.
pub fn shared(a: &str, b: &str) -> usize {
    tokens(a).intersection(&tokens(b)).count()
}

/// Whether two texts make the same point.
///
/// Needs both a high proportion of shared vocabulary and enough shared words
/// for that proportion to mean anything: two three-word titles sharing one word
/// are not the same point, however good the ratio looks.
pub fn same_point(a: &str, b: &str) -> bool {
    let a_trim = a.trim();
    let b_trim = b.trim();
    if a_trim.is_empty() || b_trim.is_empty() {
        return a_trim == b_trim;
    }
    if a_trim.eq_ignore_ascii_case(b_trim) {
        return true;
    }
    containment(a, b) >= 0.6 && shared(a, b) >= 3
}

/// Issue and pull request numbers a text cites.
pub fn references(text: &str) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    let bytes: Vec<char> = text.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c != '#' {
            continue;
        }
        let digits: String = bytes[i + 1..]
            .iter()
            .take_while(|d| d.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<u64>() {
            out.insert(n);
        }
    }
    out
}

/// Whether two reviewers gave the same reason for declining an issue.
///
/// Looser than [`same_point`], because a reason is one sentence and two models
/// write it with almost no words in common. On the run that prompted this, one
/// wrote "Same root cause and same fix as #485" and the other "This is another
/// manifestation of #485's unrecorded same-peer reset": two shared words, and
/// the reader saw the same point twice. Citing the same issue is the signal
/// that survives the rewording.
pub fn same_reason(a: &str, b: &str) -> bool {
    if same_point(a, b) {
        return true;
    }
    let cited: BTreeSet<u64> = references(a)
        .intersection(&references(b))
        .copied()
        .collect();
    !cited.is_empty() && containment(a, b) >= 0.15
}

/// Strip the provenance line spar stamps onto every follow-up it files.
///
/// Without this, every issue from one run shares "Found while working on #482"
/// and so looks a little like every other, which is exactly the wrong thumb on
/// the scale when the question is whether two of them are the same defect.
pub fn strip_provenance(text: &str) -> String {
    const STAMPS: [&str; 2] = ["found while working on #", "from #"];
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cut_to = 0usize;
    let chars: Vec<char> = text.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0usize;
    while i < chars.len() {
        let matched = STAMPS.iter().find(|stamp| {
            let s: Vec<char> = stamp.chars().collect();
            i + s.len() <= lower_chars.len() && lower_chars[i..i + s.len()] == s[..]
        });
        match matched {
            Some(stamp) => {
                // Swallow the phrase and the issue number after it.
                let mut j = i + stamp.chars().count();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '.' {
                    j += 1;
                }
                out.extend(&chars[cut_to..i]);
                cut_to = j;
                i = j;
            }
            None => i += 1,
        }
    }
    out.extend(&chars[cut_to..]);
    out
}

/// Whether two issues describe the same defect, compared on title and body
/// together.
///
/// The threshold is measured, not guessed. Against the ten follow-ups one real
/// run filed, where two pairs were confirmed duplicates by their own closing
/// comments, the duplicates scored 0.50 and 0.46 while the closest genuinely
/// distinct pair scored 0.35. 0.40 sits in that gap with room on both sides,
/// and the test holds it there.
const SAME_SUBJECT: f64 = 0.40;

pub fn same_subject(a: &str, b: &str) -> bool {
    let (a, b) = (strip_provenance(a), strip_provenance(b));
    containment(&a, &b) >= SAME_SUBJECT && shared(&a, &b) >= 5
}

/// How much of a title has to be shared before a body can be read as covering
/// it.
///
/// Lower than `SAME_SUBJECT`, because two people naming one defect share fewer
/// words in a title than in a whole issue, and the point of the test is not to
/// be strict but to require the subject to appear at all.
const SAME_TITLE: f64 = 0.34;

/// Whether an existing issue already describes this one.
///
/// The title is weighed on its own, and that is the whole change from
/// `same_subject`. `containment` normalises by the smaller token set, so a long
/// umbrella issue, a "known issues" list, or a tracker has a large enough
/// vocabulary that any short finding about the same module scores as covered,
/// and the finding is then dropped without anything being written. Requiring
/// the titles to overlap costs a genuine duplicate nothing: two people naming
/// one defect name the same thing.
pub fn covers(new_title: &str, new_body: &str, old_title: &str, old_body: &str) -> bool {
    let mine = format!("{new_title} {new_body}");
    let theirs = format!("{old_title} {old_body}");
    same_subject(&mine, &theirs) && titles_overlap(new_title, old_title)
}

fn titles_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (strip_provenance(a), strip_provenance(b));
    if a.trim().eq_ignore_ascii_case(b.trim()) {
        return true;
    }
    containment(&a, &b) >= SAME_TITLE && shared(&a, &b) >= 2
}

/// Whether `candidate` says anything `existing` does not.
///
/// The question behind "should this be a comment on the issue that already
/// exists, or nothing at all". Repeating what an issue already says is the same
/// noise as filing it twice.
pub fn adds_information(candidate: &str, existing: &str) -> bool {
    let new = tokens(candidate);
    if new.is_empty() {
        return false;
    }
    let known = tokens(existing);
    let unknown = new.difference(&known).count() as f64;
    unknown / new.len() as f64 >= 0.3
}

/// Collapse texts that make the same point, keeping the fullest wording of each.
pub fn dedupe(texts: impl IntoIterator<Item = String>) -> Vec<String> {
    dedupe_by(texts, same_point)
}

/// Collapse texts under a caller supplied notion of sameness.
pub fn dedupe_by(
    texts: impl IntoIterator<Item = String>,
    same: impl Fn(&str, &str) -> bool,
) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for text in texts {
        if text.trim().is_empty() {
            continue;
        }
        match kept.iter_mut().find(|seen| same(seen, &text)) {
            // The longer wording is usually the one carrying the evidence.
            Some(seen) => {
                if text.len() > seen.len() {
                    *seen = text;
                }
            }
            None => kept.push(text),
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two comments that landed on beignet#489, verbatim. spar posted both
    /// because they are not the same string, and a reader saw the same sentence
    /// twice.
    const REAL_A: &str = "Duplicate of #487, which reports the same refused-teardown state \
                          contradiction (connectedToElectrum false while the retained peer still \
                          serves) and is fixed by the same change.";
    const REAL_B: &str =
        "This is a duplicate of #487, which covers the same refused-teardown state mismatch.";

    #[test]
    fn the_two_comments_from_the_real_issue_are_one_point() {
        assert!(
            same_point(REAL_A, REAL_B),
            "{}",
            containment(REAL_A, REAL_B)
        );
    }

    #[test]
    fn deduping_them_keeps_the_one_carrying_the_evidence() {
        let out = dedupe([REAL_B.to_string(), REAL_A.to_string()]);
        assert_eq!(1, out.len());
        assert!(out[0].contains("connectedToElectrum"), "{:?}", out[0]);
    }

    /// Two titles for one defect, from a real run. They share three words, and
    /// no honest lexical threshold calls that a match. This is exactly why
    /// issues are compared on their bodies as well: see `same_subject`.
    #[test]
    fn titles_alone_are_too_thin_to_match_a_reworded_defect() {
        let a = "Failed switch reports a live peer as disconnected";
        let b = "A refused teardown marks the wallet disconnected while the peer is still live";
        assert!(!same_point(a, b), "{}", containment(a, b));
    }

    #[test]
    fn genuinely_different_defects_stay_apart() {
        for (a, b) in [
            (
                "Retry loop never terminates when max_attempts is unset",
                "Headers are restored only for the instance that reset the client",
            ),
            (
                "Subscription errors permanently clear restore debt",
                "attemptConnect's doc comment no longer describes what it does",
            ),
            ("Log wording", "Unbounded allocation on empty input"),
        ] {
            assert!(
                !same_point(a, b),
                "merged two different defects:\n  {a}\n  {b}"
            );
        }
    }

    /// A high ratio on two words is luck, not agreement.
    #[test]
    fn a_short_title_needs_real_overlap_not_a_lucky_word() {
        assert!(!same_point("Timeout handling", "Timeout value"));
    }

    #[test]
    fn identical_text_is_always_the_same_point() {
        assert!(same_point("Anything at all", "anything at all"));
        assert!(same_point("x", "x"));
    }

    #[test]
    fn empty_text_matches_only_empty_text() {
        assert!(same_point("", "  "));
        assert!(!same_point("", "something"));
    }

    #[test]
    fn new_evidence_counts_as_new_information() {
        let existing = "The retry loop never terminates when max_attempts is unset.";
        assert!(adds_information(
            "Reproduced on macOS with tokio 1.38: the guard on line 91 compares against Some(0).",
            existing
        ));
    }

    #[test]
    fn a_restatement_adds_nothing() {
        let existing = "The retry loop never terminates when max_attempts is unset.";
        assert!(!adds_information(
            "The retry loop never terminates if max_attempts is unset.",
            existing
        ));
    }

    #[test]
    fn dedupe_keeps_distinct_points_and_drops_blanks() {
        let out = dedupe([
            "Retry loop never terminates".to_string(),
            "   ".to_string(),
            "Headers are restored only for the initiating instance".to_string(),
        ]);
        assert_eq!(2, out.len());
    }

    #[test]
    fn tokens_ignore_punctuation_and_filler() {
        let t = tokens("The retry-loop, which never terminates!");
        assert!(t.contains("retry") && t.contains("loop") && t.contains("terminates"));
        assert!(!t.contains("the") && !t.contains("which"));
    }
}

#[cfg(test)]
mod real_corpus {
    use super::*;
    use std::collections::BTreeMap;

    /// The ten follow-ups one real run filed on a real repository, captured
    /// verbatim. Two pairs of them are confirmed duplicates by their own
    /// closing comments, which is what the thresholds here are measured
    /// against rather than guessed at.
    const CORPUS: &str = include_str!("../tests/fixtures/real_followups.json");

    fn issues() -> BTreeMap<u64, String> {
        let rows: Vec<serde_json::Value> = serde_json::from_str(CORPUS).expect("fixture");
        rows.into_iter()
            .map(|r| {
                let number = r["number"].as_u64().expect("number");
                let text = format!(
                    "{} {}",
                    r["title"].as_str().unwrap_or(""),
                    r["body"].as_str().unwrap_or("")
                );
                (number, text)
            })
            .collect()
    }

    /// #489 duplicates #487, and #490 duplicates #485. Both were closed saying
    /// so. spar filed them anyway, because it compared titles for exact
    /// equality.
    #[test]
    fn both_duplicates_that_were_actually_filed_are_caught() {
        let by = issues();
        for (dup, original) in [(489u64, 487u64), (490, 485)] {
            let score = containment(&by[&dup], &by[&original]);
            assert!(
                same_subject(&by[&dup], &by[&original]),
                "#{dup} vs #{original} scored {score:.3}"
            );
            // And with headroom above the bar, not scraping it.
            assert!(
                score >= 0.44,
                "#{dup} vs #{original} only scored {score:.3}"
            );
        }
    }

    /// The title carries its own weight, and the real duplicates survive it.
    ///
    /// Weighing the title separately is what stops a long issue absorbing every
    /// short finding in its module, and it has to cost a genuine duplicate
    /// nothing: two people naming one defect name the same thing.
    #[test]
    fn the_real_duplicates_survive_the_title_test() {
        let rows: Vec<serde_json::Value> = serde_json::from_str(CORPUS).expect("fixture");
        let by: BTreeMap<u64, (String, String)> = rows
            .into_iter()
            .map(|r| {
                (
                    r["number"].as_u64().expect("number"),
                    (
                        r["title"].as_str().unwrap_or("").to_string(),
                        r["body"].as_str().unwrap_or("").to_string(),
                    ),
                )
            })
            .collect();
        for (dup, original) in [(489u64, 487u64), (490, 485)] {
            let (dt, db) = &by[&dup];
            let (ot, ob) = &by[&original];
            assert!(covers(dt, db, ot, ob), "#{dup} vs #{original}");
        }
    }

    /// A long umbrella issue has a large enough vocabulary to contain any short
    /// finding about the same module, and `containment` normalises by the
    /// smaller token set, so it scored as covering all of them. The finding was
    /// then dropped with nothing written anywhere.
    #[test]
    fn a_long_umbrella_issue_does_not_absorb_every_finding_in_its_module() {
        let umbrella_title = "Known issues in the retry and cache layers";
        let umbrella_body = "\
            A running list. The retry loop needs a cap. The cache eviction is \
            wrong under load. The connection pool leaks sockets on failover. \
            The backoff calculation ignores Retry-After. Header parsing accepts \
            duplicate keys. The metrics counter double counts retries. Cache \
            keys collide across tenants. The socket timeout is hard coded.";
        let finding_title = "Cache keys collide across tenants";
        let finding_body = "Two tenants with the same object name share a cache entry.";

        assert!(
            same_subject(
                &format!("{finding_title} {finding_body}"),
                &format!("{umbrella_title} {umbrella_body}")
            ),
            "the old rule has to absorb it, or this proves nothing"
        );
        assert!(
            !covers(finding_title, finding_body, umbrella_title, umbrella_body),
            "a list that mentions the module absorbed a finding and wrote nothing"
        );

        // An issue actually about that defect still covers it, however the
        // wording differs.
        assert!(
            covers(
                finding_title,
                finding_body,
                "Cache keys collide between tenants",
                "Two tenants storing the same object name share one cache entry."
            ),
            "a real duplicate was filed twice"
        );
    }

    /// Everything else in that run is a genuinely separate defect, and merging
    /// any of them would be worse than the duplicate this is meant to prevent.
    #[test]
    fn no_two_distinct_defects_are_merged() {
        let by = issues();
        let dups = [(489u64, 487u64), (490, 485)];
        let numbers: Vec<u64> = by.keys().copied().collect();
        let mut worst = (0.0f64, 0u64, 0u64);

        for (i, a) in numbers.iter().enumerate() {
            for b in &numbers[i + 1..] {
                if dups.contains(&(*a, *b)) || dups.contains(&(*b, *a)) {
                    continue;
                }
                let score = containment(&by[a], &by[b]);
                if score > worst.0 {
                    worst = (score, *a, *b);
                }
                assert!(
                    !same_subject(&by[a], &by[b]),
                    "merged #{a} and #{b}, which are different defects (scored {score:.2})"
                );
            }
        }
        // Headroom matters as much as the verdict: a threshold with no gap
        // under it is luck rather than a threshold. Measured, the closest
        // distinct pair is 0.35 against a bar of 0.40.
        assert!(
            worst.0 <= 0.36,
            "#{} and #{} scored {:.3}, leaving no headroom under the threshold",
            worst.1,
            worst.2,
            worst.0
        );
    }

    /// Every follow-up from one run ends with the same provenance line. Left
    /// in, it makes unrelated issues look alike.
    #[test]
    fn the_provenance_line_does_not_count_toward_similarity() {
        let a = "Something entirely unrelated. Found while working on #482.";
        let b = "A different thing altogether. Found while working on #482.";
        assert!(
            !strip_provenance(a).contains("482"),
            "{:?}",
            strip_provenance(a)
        );
        assert!(!same_subject(a, b));
    }

    #[test]
    fn stripping_provenance_leaves_the_rest_intact() {
        assert_eq!(
            "The retry loop spins. ",
            strip_provenance("The retry loop spins. Found while working on #482.")
        );
    }

    /// The two comments that landed together on the real closed issue. Almost
    /// no words in common, but both cite the issue they duplicate.
    #[test]
    fn reasons_citing_the_same_issue_are_one_reason() {
        let a = "Same root cause and same fix as #485 (the client's own same-target reconnect \
                 clears the bookkeeping while stopPeerIfServerChanged reports clientReset:false, \
                 so no restore debt is recorded), so it is covered by the same change.";
        let b = "This is another manifestation of #485's unrecorded same-peer reset and is \
                 covered by restoring subscriptions there.";
        assert!(!same_point(a, b), "lexically they really are far apart");
        assert!(same_reason(a, b), "but they make the same point");

        let kept = dedupe_by([b.to_string(), a.to_string()], same_reason);
        assert_eq!(1, kept.len());
        assert!(
            kept[0].contains("root cause"),
            "the fuller wording survives"
        );
    }

    #[test]
    fn reasons_citing_different_issues_stay_apart() {
        assert!(!same_reason(
            "Duplicate of #485, same root cause.",
            "Superseded by #999, which takes a different approach entirely."
        ));
    }

    #[test]
    fn references_are_extracted_from_prose() {
        assert_eq!(
            vec![12u64, 487],
            references("Duplicate of #487, see also #12.")
                .into_iter()
                .collect::<Vec<_>>()
        );
        assert!(references("no numbers here").is_empty());
        assert!(references("# not a reference").is_empty());
    }
}
