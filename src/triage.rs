//! Both agents judge every issue independently, then the two verdicts are
//! reconciled mechanically.
//!
//! Nothing here lets one agent overrule the other. Both say do, it is
//! scheduled. Both say skip, it is skipped and the shared reasoning is posted.
//! They disagree, it is parked for a person, because a disagreement between two
//! competent reviewers is information, not noise to be averaged away.

use std::collections::BTreeMap;

use crate::agent::Agent;
use crate::config::Config;
use crate::error::Result;
use crate::model::{
    Complexity, ContestedItem, Issue, Plan, PlanItem, Risk, SkippedItem, TriageResponse,
    TriageVerdict,
};
use crate::repo::Repo;
use crate::{log, logwarn, schema, spar_err};

const TRIAGE_PROMPT: &str = "\
You are triaging GitHub issues for the repository in your working directory.
Read the codebase as needed before judging. Do not modify anything.

Each issue below is its number, title, URL, and body as filed. The discussion
since it was filed is not included. Where a body leaves the judgement genuinely
unclear, read that one issue's thread before deciding; read the ones that need
it rather than all of them, because the queue is long and most will not. If you
cannot reach the network, judge on what is here.

For each issue decide:
- worth_doing: is this a real, valid, actionable issue worth a PR? Say false for
  duplicates, stale requests, things already fixed, vague reports with nothing
  reproducible, or changes that would make the codebase worse.
- complexity: s, m, or l.
- depends_on: issue numbers from this same list that should land first.
- risk: how likely a change here is to break something.

Judge independently. Be willing to say an issue is not worth doing. Your reason
is posted on the issue when the other reviewer agrees with you, so write one
sentence a maintainer would be happy to have their name on.

Issues:
";

/// Every issue as the prompt carries it, and what would not fit.
struct Rendered {
    text: String,
    /// Left for a later run, because the queue did not fit in one prompt.
    deferred: Vec<i64>,
    /// Included, but with the tail of the body left off.
    shortened: Vec<i64>,
}

/// Render the queue, under two budgets that do different jobs.
///
/// One issue is shortened only past `max_issue_chars`, which nothing a person
/// wrote reaches. The queue as a whole is bounded by `max_triage_chars`,
/// because triage reads every open issue at once and the queue is the only
/// unbounded thing here.
///
/// Past that, whole issues are left for the next run rather than every issue
/// losing its tail. A triage verdict is posted on the issue and can close it,
/// so judging one on part of what it says is worse than not having reached it
/// yet. Everything from the first issue that does not fit is deferred together,
/// so what was read is always a prefix of the queue rather than whichever
/// issues happened to be small.
fn render(issues: &[Issue], cfg: &Config) -> Rendered {
    let mut parts: Vec<String> = Vec::new();
    let mut deferred = Vec::new();
    let mut shortened = Vec::new();
    let mut total = 0usize;

    for issue in issues {
        if !deferred.is_empty() {
            deferred.push(issue.number);
            continue;
        }
        let (body, cut) = issue.body_for_prompt(cfg.loop_cfg.max_issue_chars);
        // The URL is what makes the comments reachable to an agent that can
        // reach them, without spar fetching every thread in the queue on the
        // chance one of them matters.
        let entry = format!("#{}: {}\n{}\n{body}", issue.number, issue.title, issue.url);
        let len = entry.chars().count();
        // The first issue goes in whatever its size. A queue of one that does
        // not fit is a run that does nothing, forever.
        if !parts.is_empty() && total + len > cfg.loop_cfg.max_triage_chars {
            deferred.push(issue.number);
            continue;
        }
        if cut {
            shortened.push(issue.number);
        }
        total += len;
        parts.push(entry);
    }

    Rendered {
        text: parts.join("\n\n"),
        deferred,
        shortened,
    }
}

fn numbers(items: &[i64]) -> String {
    items
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Ask both agents, then reconcile.
pub fn triage(agents: &[Agent], cfg: &Config, repo: &Repo, issues: &[Issue]) -> Result<Plan> {
    let rendered = render(issues, cfg);
    // Never silently. An agent cannot report a gap it was not told about, and
    // a verdict on part of an issue looks exactly like a verdict on all of it.
    if !rendered.shortened.is_empty() {
        logwarn!(
            "issue body shortened to fit the prompt: {}. Raise max_issue_chars if these matter.",
            numbers(&rendered.shortened)
        );
    }
    if !rendered.deferred.is_empty() {
        logwarn!(
            "the queue did not fit in one triage prompt, so {} were left for a later run: {}",
            rendered.deferred.len(),
            numbers(&rendered.deferred)
        );
    }
    let prompt = format!("{TRIAGE_PROMPT}{}", rendered.text);
    let schema = schema::triage();

    let answers = if cfg.loop_cfg.parallel_triage && agents.len() > 1 {
        ask_together(agents, cfg, repo, &prompt, &schema)
    } else {
        ask_in_turn(agents, cfg, repo, &prompt, &schema)
    };

    let mut verdicts: Vec<(String, BTreeMap<i64, TriageVerdict>)> = Vec::new();
    for (name, answer) in answers {
        let response = answer?;
        let mut by_issue = BTreeMap::new();
        for verdict in response.issues {
            by_issue.insert(verdict.issue, verdict);
        }
        verdicts.push((name, by_issue));
    }

    Ok(reconcile(issues, &verdicts))
}

type Answer = (String, Result<TriageResponse>);

fn ask_one(
    agent: &Agent,
    cfg: &Config,
    repo: &Repo,
    prompt: &str,
    schema: &serde_json::Value,
) -> Answer {
    let effort = cfg.effort_for_round(&agent.spec, 1);
    let out = agent.ask_json::<TriageResponse>(prompt, schema, repo.root(), effort.as_deref());
    (agent.name().to_string(), out)
}

/// Both agents at once. Triage only reads, so there is nothing to serialise,
/// and a full repo pass is the slowest step in a run.
fn ask_together(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    prompt: &str,
    schema: &serde_json::Value,
) -> Vec<Answer> {
    log!("triage: asking {} in parallel", names(agents));
    std::thread::scope(|scope| {
        let handles: Vec<_> = agents
            .iter()
            .map(|agent| scope.spawn(move || ask_one(agent, cfg, repo, prompt, schema)))
            .collect();
        handles
            .into_iter()
            .zip(agents)
            .map(|(handle, agent)| {
                handle.join().unwrap_or_else(|_| {
                    (
                        agent.name().to_string(),
                        Err(spar_err!("triage thread for '{}' panicked", agent.name())),
                    )
                })
            })
            .collect()
    })
}

fn ask_in_turn(
    agents: &[Agent],
    cfg: &Config,
    repo: &Repo,
    prompt: &str,
    schema: &serde_json::Value,
) -> Vec<Answer> {
    agents
        .iter()
        .map(|agent| {
            log!(
                "triage: asking {} ({})",
                agent.name(),
                agent.spec.describe()
            );
            ask_one(agent, cfg, repo, prompt, schema)
        })
        .collect()
}

fn names(agents: &[Agent]) -> String {
    agents
        .iter()
        .map(Agent::name)
        .collect::<Vec<_>>()
        .join(" and ")
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

fn reconcile(issues: &[Issue], verdicts: &[(String, BTreeMap<i64, TriageVerdict>)]) -> Plan {
    let mut agreed = Vec::new();
    let mut skipped = Vec::new();
    let mut contested = Vec::new();

    for issue in issues {
        let number = issue.number;
        let seen: Vec<(&String, Option<&TriageVerdict>)> = verdicts
            .iter()
            .map(|(name, map)| (name, map.get(&number)))
            .collect();

        if seen.iter().any(|(_, v)| v.is_none()) {
            let missing: Vec<&str> = seen
                .iter()
                .filter(|(_, v)| v.is_none())
                .map(|(n, _)| n.as_str())
                .collect();
            contested.push(ContestedItem {
                issue: number,
                title: issue.title.clone(),
                positions: BTreeMap::new(),
                reasons: BTreeMap::new(),
                note: Some(format!("no verdict from {}", missing.join(", "))),
            });
            continue;
        }

        let all: Vec<(&String, &TriageVerdict)> = seen
            .into_iter()
            .map(|(n, v)| (n, v.expect("checked")))
            .collect();

        if all.iter().all(|(_, v)| v.worth_doing) {
            let complexity = all
                .iter()
                .map(|(_, v)| v.complexity)
                .max_by_key(|c| c.rank())
                .unwrap_or(Complexity::M);
            let risk = all
                .iter()
                .map(|(_, v)| v.risk)
                .max_by_key(|r| r.rank())
                .unwrap_or(Risk::Med);
            let mut depends: Vec<i64> =
                all.iter().flat_map(|(_, v)| v.depends_on.clone()).collect();
            depends.sort_unstable();
            depends.dedup();
            agreed.push(PlanItem {
                issue: number,
                title: issue.title.clone(),
                complexity,
                risk,
                depends_on: depends,
                reason: all[0].1.reason.clone(),
            });
        } else if all.iter().all(|(_, v)| !v.worth_doing) {
            skipped.push(SkippedItem {
                issue: number,
                title: issue.title.clone(),
                reasons: all
                    .iter()
                    .map(|(n, v)| ((*n).clone(), v.reason.clone()))
                    .collect(),
                // Either agent is enough. Closing needs both to agree, so one
                // saying the issue is not finished is enough to withhold it.
                tracker: all.iter().any(|(_, v)| v.tracker),
            });
        } else {
            contested.push(ContestedItem {
                issue: number,
                title: issue.title.clone(),
                positions: all
                    .iter()
                    .map(|(n, v)| {
                        (
                            (*n).clone(),
                            if v.worth_doing { "do" } else { "skip" }.to_string(),
                        )
                    })
                    .collect(),
                reasons: all
                    .iter()
                    .map(|(n, v)| ((*n).clone(), v.reason.clone()))
                    .collect(),
                note: None,
            });
        }
    }

    Plan {
        order: order(agreed),
        skipped,
        contested,
    }
}

/// Topological by dependency, then cheapest first, so blockers clear early and
/// the large risky items inherit a healthier base.
pub fn order(items: Vec<PlanItem>) -> Vec<PlanItem> {
    let by_number: BTreeMap<i64, PlanItem> = items.iter().map(|i| (i.issue, i.clone())).collect();

    let mut entry: Vec<&PlanItem> = items.iter().collect();
    entry.sort_by_key(|i| (i.complexity.rank(), i.issue));

    let mut ordered = Vec::new();
    let mut done: Vec<i64> = Vec::new();
    let mut visiting: Vec<i64> = Vec::new();

    fn visit(
        number: i64,
        by_number: &BTreeMap<i64, PlanItem>,
        done: &mut Vec<i64>,
        visiting: &mut Vec<i64>,
        ordered: &mut Vec<PlanItem>,
    ) {
        if done.contains(&number) {
            return;
        }
        let Some(item) = by_number.get(&number) else {
            return; // a dependency outside this run's list
        };
        if visiting.contains(&number) {
            return; // dependency cycle, break it rather than hang
        }
        visiting.push(number);
        let mut deps = item.depends_on.clone();
        deps.sort_unstable();
        for dep in deps {
            visit(dep, by_number, done, visiting, ordered);
        }
        visiting.retain(|n| *n != number);
        done.push(number);
        ordered.push(item.clone());
    }

    for item in entry {
        visit(
            item.issue,
            &by_number,
            &mut done,
            &mut visiting,
            &mut ordered,
        );
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(max_issue: usize, max_total: usize) -> Config {
        let text = "[agents.a]\ncommand = [\"x\"]\n[agents.b]\ncommand = [\"y\"]\n";
        let mut cfg = crate::config::parse(text).expect("a config");
        cfg.loop_cfg.max_issue_chars = max_issue;
        cfg.loop_cfg.max_triage_chars = max_total;
        cfg
    }

    fn issue_of(number: i64, body: &str) -> Issue {
        let mut i: Issue = serde_json::from_value(serde_json::json!({
            "number": number, "title": "t", "state": "open", "url": "u"
        }))
        .expect("an issue");
        i.body = Some(body.to_string());
        i
    }

    /// The case that is every real queue. Nothing is cut, nothing is deferred,
    /// and every issue reaches the prompt entire.
    #[test]
    fn an_ordinary_queue_is_rendered_whole() {
        let issues = vec![issue_of(1, "first body"), issue_of(2, "second body")];
        let out = render(&issues, &cfg_with(60_000, 200_000));
        assert!(out.deferred.is_empty() && out.shortened.is_empty());
        assert!(out.text.contains("first body") && out.text.contains("second body"));
    }

    /// The body is what an agent judges on, and the link is how one that can
    /// reach the network reads the discussion spar does not fetch. Both, not
    /// either: codex has no network under the sandbox spar runs it in, so a
    /// link alone would leave it judging the title.
    #[test]
    fn every_issue_carries_its_link_as_well_as_its_body() {
        let mut issue = issue_of(1, "the body");
        issue.url = "https://github.com/o/r/issues/1".into();
        let out = render(&[issue], &cfg_with(60_000, 200_000));
        assert!(
            out.text.contains("https://github.com/o/r/issues/1"),
            "{}",
            out.text
        );
        assert!(out.text.contains("the body"), "{}", out.text);
    }

    /// A verdict is posted on the issue and can close it, so an issue judged on
    /// part of its body is worse than one not reached yet. Past the budget,
    /// whole issues wait rather than every issue losing its tail.
    #[test]
    fn a_queue_that_does_not_fit_defers_whole_issues() {
        let issues = vec![
            issue_of(1, &"a".repeat(80)),
            issue_of(2, &"b".repeat(80)),
            issue_of(3, &"c".repeat(80)),
        ];
        let out = render(&issues, &cfg_with(60_000, 120));
        assert_eq!(vec![2, 3], out.deferred);
        assert!(out.shortened.is_empty(), "no issue lost its tail");
        assert!(out.text.contains(&"a".repeat(80)));
        assert!(!out.text.contains(&"b".repeat(80)));
    }

    /// Everything from the first issue that does not fit is deferred together,
    /// so what was read is a prefix of the queue rather than whichever issues
    /// happened to be small enough to slot in.
    #[test]
    fn deferral_is_a_prefix_and_does_not_pick_the_small_ones() {
        let issues = vec![
            issue_of(1, &"a".repeat(80)),
            issue_of(2, &"b".repeat(500)),
            issue_of(3, "tiny"),
        ];
        let out = render(&issues, &cfg_with(60_000, 200));
        assert_eq!(vec![2, 3], out.deferred);
        assert!(
            !out.text.contains("tiny"),
            "a later small issue must not jump the queue"
        );
    }

    /// A queue of one that does not fit would be a run that does nothing,
    /// forever, so the first issue goes in whatever its size.
    #[test]
    fn the_first_issue_is_never_deferred() {
        let issues = vec![issue_of(1, &"a".repeat(500))];
        let out = render(&issues, &cfg_with(60_000, 10));
        assert!(out.deferred.is_empty());
        assert!(out.text.contains(&"a".repeat(500)));
    }

    /// Past the per issue budget the body is shortened and the issue is named,
    /// rather than the queue quietly carrying a fragment.
    #[test]
    fn an_oversized_body_is_shortened_and_reported() {
        let issues = vec![issue_of(7, &"word\n".repeat(400))];
        let out = render(&issues, &cfg_with(100, 200_000));
        assert_eq!(vec![7], out.shortened);
        assert!(out.text.contains("Shortened to fit"), "{}", out.text);
    }

    fn item(n: i64, complexity: &str, deps: &[i64]) -> PlanItem {
        PlanItem {
            issue: n,
            title: format!("i{n}"),
            complexity: Complexity::parse_lenient(complexity).unwrap(),
            risk: Risk::Low,
            depends_on: deps.to_vec(),
            reason: String::new(),
        }
    }

    fn numbers(items: Vec<PlanItem>) -> Vec<i64> {
        order(items).into_iter().map(|i| i.issue).collect()
    }

    #[test]
    fn a_dependency_precedes_its_dependent() {
        let out = numbers(vec![item(1, "s", &[2]), item(2, "l", &[])]);
        let (a, b) = (
            out.iter().position(|n| *n == 2).unwrap(),
            out.iter().position(|n| *n == 1).unwrap(),
        );
        assert!(a < b, "{out:?}");
    }

    #[test]
    fn cheapest_first_without_dependencies() {
        assert_eq!(
            vec![2, 3, 1],
            numbers(vec![
                item(1, "l", &[]),
                item(2, "s", &[]),
                item(3, "m", &[])
            ])
        );
    }

    #[test]
    fn a_cycle_does_not_hang() {
        assert_eq!(
            2,
            numbers(vec![item(1, "s", &[2]), item(2, "s", &[1])]).len()
        );
    }

    #[test]
    fn an_unknown_dependency_is_ignored() {
        assert_eq!(vec![1], numbers(vec![item(1, "s", &[99])]));
    }

    #[test]
    fn every_item_appears_exactly_once() {
        let items: Vec<PlanItem> = (1..=5).map(|n| item(n, "m", &[])).collect();
        let out = numbers(items);
        assert_eq!(vec![1, 2, 3, 4, 5], out);
    }

    #[test]
    fn a_dependency_chain_is_ordered_end_to_end() {
        let items: Vec<PlanItem> = (1..=5)
            .map(|n| {
                let deps: Vec<i64> = if n > 1 { vec![n - 1] } else { vec![] };
                PlanItem {
                    depends_on: deps,
                    ..item(n, "m", &[])
                }
            })
            .collect();
        assert_eq!(vec![1, 2, 3, 4, 5], numbers(items));
    }

    // -- reconciliation --------------------------------------------------

    fn issue(n: i64) -> Issue {
        Issue {
            number: n,
            title: format!("issue {n}"),
            body: Some("body".into()),
            state: "OPEN".into(),
            state_reason: None,
            url: String::new(),
            labels: vec![],
        }
    }

    fn verdict(n: i64, worth: bool, complexity: &str, risk: &str, deps: &[i64]) -> TriageVerdict {
        TriageVerdict {
            issue: n,
            worth_doing: worth,
            tracker: false,
            reason: format!("because {n}"),
            complexity: Complexity::parse_lenient(complexity).unwrap(),
            depends_on: deps.to_vec(),
            risk: Risk::parse_lenient(risk).unwrap(),
        }
    }

    fn pair(
        a: Vec<TriageVerdict>,
        b: Vec<TriageVerdict>,
    ) -> Vec<(String, BTreeMap<i64, TriageVerdict>)> {
        vec![
            (
                "claude".to_string(),
                a.into_iter().map(|v| (v.issue, v)).collect(),
            ),
            (
                "codex".to_string(),
                b.into_iter().map(|v| (v.issue, v)).collect(),
            ),
        ]
    }

    #[test]
    fn both_agreeing_to_do_schedules_it() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![verdict(1, true, "s", "low", &[])],
                vec![verdict(1, true, "s", "low", &[])],
            ),
        );
        assert_eq!(1, plan.order.len());
        assert!(plan.skipped.is_empty() && plan.contested.is_empty());
    }

    #[test]
    fn both_agreeing_to_skip_records_both_reasons() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![verdict(1, false, "s", "low", &[])],
                vec![verdict(1, false, "s", "low", &[])],
            ),
        );
        assert_eq!(1, plan.skipped.len());
        assert_eq!(2, plan.skipped[0].reasons.len());
        assert!(plan.skipped[0].reasons.contains_key("claude"));
        assert!(plan.skipped[0].reasons.contains_key("codex"));
    }

    /// One agent never overrules the other. A split goes to a person.
    #[test]
    fn a_disagreement_is_contested_not_averaged() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![verdict(1, true, "s", "low", &[])],
                vec![verdict(1, false, "s", "low", &[])],
            ),
        );
        assert!(plan.order.is_empty() && plan.skipped.is_empty());
        assert_eq!(1, plan.contested.len());
        assert_eq!(
            Some(&"do".to_string()),
            plan.contested[0].positions.get("claude")
        );
        assert_eq!(
            Some(&"skip".to_string()),
            plan.contested[0].positions.get("codex")
        );
    }

    #[test]
    fn a_missing_verdict_is_contested_and_says_who_was_silent() {
        let plan = reconcile(
            &[issue(1)],
            &pair(vec![verdict(1, true, "s", "low", &[])], vec![]),
        );
        assert_eq!(1, plan.contested.len());
        assert!(plan.contested[0].note.as_deref().unwrap().contains("codex"));
    }

    #[test]
    fn the_pessimistic_estimate_wins() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![verdict(1, true, "s", "low", &[])],
                vec![verdict(1, true, "l", "high", &[])],
            ),
        );
        assert_eq!(Complexity::L, plan.order[0].complexity);
        assert_eq!(Risk::High, plan.order[0].risk);
    }

    #[test]
    fn dependencies_from_both_agents_are_unioned() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![verdict(1, true, "s", "low", &[2, 3])],
                vec![verdict(1, true, "s", "low", &[3, 4])],
            ),
        );
        assert_eq!(vec![2, 3, 4], plan.order[0].depends_on);
    }

    // -- trackers ---------------------------------------------------------

    /// The failure this exists to stop. Both agents correctly declined an
    /// umbrella whose three subtasks were filed separately and still open, and
    /// spar closed it as "not planned". Declining to open a pull request for a
    /// tracker is right; closing it does not follow from that.
    #[test]
    fn an_umbrella_both_agents_declined_is_skipped_but_held_open() {
        let plan = reconcile(
            &[issue(1)],
            &pair(vec![tracker_verdict(1)], vec![tracker_verdict(1)]),
        );
        assert!(plan.order.is_empty());
        assert_eq!(1, plan.skipped.len());
        assert!(plan.skipped[0].tracker, "a tracker must not be closeable");
    }

    /// Closing already needs both agents to decline, on the principle that one
    /// agent's opinion is not enough to close somebody's report. One agent
    /// saying the issue is not finished is that same principle from the other
    /// side, so either is enough to withhold the close.
    #[test]
    fn one_agent_calling_it_a_tracker_is_enough_to_hold_it_open() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![tracker_verdict(1)],
                vec![verdict(1, false, "s", "low", &[])],
            ),
        );
        assert!(plan.skipped[0].tracker);
    }

    /// An ordinary decline stays closeable, which is the behaviour worth
    /// keeping: an issue filed twice is finished.
    #[test]
    fn an_ordinary_decline_is_not_held_open() {
        let plan = reconcile(
            &[issue(1)],
            &pair(
                vec![verdict(1, false, "s", "low", &[])],
                vec![verdict(1, false, "s", "low", &[])],
            ),
        );
        assert!(!plan.skipped[0].tracker);
    }

    fn tracker_verdict(n: i64) -> TriageVerdict {
        let mut v = verdict(n, false, "m", "low", &[]);
        v.tracker = true;
        v
    }
}
