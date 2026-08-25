//! Render every comment spar can post, so you can see what lands on GitHub
//! before you spend a token.
//!
//!     cargo run --example preview
//!     cargo run --example preview -- --loose    # with the concision gate off
//!
//! The model output below is deliberately as verbose as a real model gets. What
//! prints is what a reviewer would actually read.

use spar::model::{
    Dispute, Finding, IssueRun, Judged, NextAction, ResponseDoc, Review, Severity, SkippedItem,
    Standing, Verdict,
};
use spar::review::{
    disposition_comment, outcome_comment, pr_body, review_comment, skip_comment, Ending,
};
use spar::review_only::verdict_comment;
use spar::style::Style;

fn finding(severity: &str, title: &str, detail: &str, file: &str, in_scope: bool) -> Finding {
    Finding {
        severity: Severity::parse_lenient(severity).expect("severity"),
        title: title.into(),
        detail: detail.into(),
        file: file.into(),
        in_scope,
    }
}

fn rule(label: &str) {
    println!("\n\x1b[1m{label}\x1b[0m\n{}", "-".repeat(72));
}

fn main() {
    let loose = std::env::args().any(|a| a == "--loose");
    let style = if loose {
        Style {
            terse: false,
            ..Style::default()
        }
    } else {
        Style::default()
    };
    if loose {
        println!("(concision gate OFF: this is what a model would post unedited)");
    }

    rule("A clean review");
    println!(
        "{}",
        review_comment(
            "codex",
            1,
            &Review {
                verdict: Verdict::Approve,
                next_action: NextAction::Merge,
                summary: "I reviewed the changes on this branch carefully and I am happy to \
                          report that the retry path is correct, the backoff calculation is \
                          sound, and the new test covers the 429 case that the issue described. \
                          I have no objections to this change landing as it stands."
                    .into(),
                findings: vec![],
            },
            &style
        )
    );

    rule("A review with real work in it");
    println!(
        "{}",
        review_comment(
            "codex",
            2,
            &Review {
                verdict: Verdict::ChangesRequested,
                next_action: NextAction::HandBack,
                summary: "There is one genuine defect here that should block, along with a \
                          couple of improvements that I do not think need to gate this \
                          particular pull request, and one pre-existing problem I noticed \
                          while reading the surrounding code."
                    .into(),
                findings: vec![
                    finding(
                        "blocking",
                        "Retry loop never terminates when max_attempts is unset",
                        "I confirmed this by running the 429 test with max_attempts left at its \
                         default of None: the loop spins forever because the guard on line 91 \
                         compares against Some(0) rather than checking for None first. This is \
                         not a theoretical concern, the test hangs and I had to kill it.",
                        "src/net.rs:88",
                        true,
                    ),
                    finding(
                        "non-blocking",
                        "The request timeout is hard coded to thirty seconds",
                        "It would be better if this were configurable, since a slow upstream \
                         will now fail rather than wait, but the previous code had the same \
                         limitation so this is not a regression introduced by the change.",
                        "src/net.rs:44",
                        true,
                    ),
                    finding(
                        "nit",
                        "Log line says \"retrying\" without saying how many attempts remain",
                        "Purely a readability point for whoever is reading the logs at 3am.",
                        "src/net.rs:102",
                        true,
                    ),
                    finding(
                        "blocking",
                        "Config loader swallows a parse error",
                        "Unrelated to this PR, but load_config discards the error from serde and \
                         returns Default::default(), so a typo in the config file is silently \
                         ignored.",
                        "src/config.rs:210",
                        false,
                    ),
                ],
            },
            &style
        )
    );

    rule("Answering that review");
    println!(
        "{}",
        disposition_comment(
            "claude",
            &ResponseDoc {
                summary: "One of the two blocking points was right and I have fixed it. I do not \
                          agree with the other and have explained why below rather than changing \
                          working code to make the review go away."
                    .into(),
                dispositions: vec![],
            },
            &["Retry loop never terminates when max_attempts is unset".to_string()],
            &[
                "Config loader swallows a parse error. The caller already validates the file \
               against the schema before load_config is reached, so the discarded error is \
               unreachable in practice."
                    .to_string()
            ],
            &["https://github.com/you/thing/issues/512".to_string()],
            &style
        )
        .unwrap_or_default()
    );

    rule("The pull request body");
    println!(
        "{}",
        pr_body(
            478,
            "Retry a 429 with exponential backoff instead of failing the request.",
            &style
        )
    );

    rule("What a whole run leaves on the PR (the default, one comment)");
    let mut ended = IssueRun::new(482, "t");
    ended.disputes = vec![Dispute {
        title: "Config loader swallows a parse error".into(),
        reasoning: "the caller validates against the schema before load_config is reached".into(),
    }];
    ended.filed = vec![
        "https://github.com/you/thing/issues/485".into(),
        "https://github.com/you/thing/issues/486".into(),
    ];
    println!(
        "{}",
        outcome_comment(
            &ended,
            &spar::model::Ledger::new(),
            &Ending::OutOfRounds,
            &style
        )
        .unwrap_or_default()
    );
    println!("\n  (and a clean run that filed nothing posts no comment at all)");

    rule("A review of somebody else's pull request (spar review)");
    let judged = |standing, severity, title: &str, detail: &str, file: &str, by: &str| Judged {
        finding: finding(severity, title, detail, file, true),
        raised_by: by.to_string(),
        standing,
        counterpoint: None,
        defence: None,
    };
    let mut disputed = judged(
        Standing::Disputed,
        "blocking",
        "Config loader swallows a parse error",
        "load_config discards the error from serde and returns a default.",
        "src/config.rs:210",
        "claude",
    );
    disputed.counterpoint =
        Some("the caller validates against the schema before load_config is reached".into());
    println!(
        "{}",
        verdict_comment(
            &[
                judged(
                    Standing::Corroborated,
                    "blocking",
                    "Retry loop never terminates when max_attempts is unset",
                    "Both reviewers reproduced this: the guard on line 91 compares against \
                     Some(0) rather than checking for None, so the 429 test hangs.",
                    "src/net.rs:88",
                    "claude and codex",
                ),
                judged(
                    Standing::Confirmed,
                    "non-blocking",
                    "The request timeout is hard coded",
                    "Not a regression, the previous code had the same limitation.",
                    "src/net.rs:44",
                    "codex",
                ),
                judged(
                    Standing::Unverified,
                    "nit",
                    "Log line does not say how many attempts remain",
                    "Readability for whoever reads the logs at 3am.",
                    "src/net.rs:102",
                    "claude",
                ),
                disputed,
                judged(
                    Standing::Withdrawn,
                    "blocking",
                    "Off by one in the backoff",
                    "Withdrawn after the other reviewer pointed at the test that covers it.",
                    "src/net.rs:70",
                    "codex",
                ),
            ],
            &style
        )
    );

    rule("An issue both reviewers declined");
    println!(
        "{}",
        skip_comment(
            &SkippedItem {
                issue: 91,
                title: "Add a dark mode".into(),
                reasons: [
                    (
                        "claude".to_string(),
                        "This was already implemented in 1.4 and shipped behind the theme \
                         setting, so there is nothing left to do here."
                            .to_string()
                    ),
                    (
                        "codex".to_string(),
                        "Duplicate of #62, which is still open and has the full discussion."
                            .to_string()
                    ),
                ]
                .into_iter()
                .collect(),
            },
            &style
        )
    );
    println!();
}
