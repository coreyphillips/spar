# spar

Two coding agents alternate implementing and reviewing GitHub issues until a PR
converges. Neither agent reviews its own most recent edit.

Roles are not fixed. Whoever holds the PR may implement, review, fix, or file
follow-ups, and then hands custody to the other. A reviewer that finds a problem
can either fix it directly and hand back for review of its own fix, or return
notes for the author to address.

It also reviews without touching: `spar review <pr>` puts two independent
reviewers on a pull request you cannot push to, including one from a fork, and
reports what they agree on and what they do not.

Ships pairing **Claude Code** with **OpenAI Codex**. Any two CLIs work.

## Why alternate instead of assigning roles

A single model reviewing its own work grades its own homework. Two models with
different training and different failure modes catch different things, and the
disagreements are the useful part. In an early run, Codex flagged that Python's
`\d` matches Unicode digits, so `"١h"` parsed as a valid duration. The Claude
agent had missed it. On a separate point the Claude agent pushed back and
refused the change with a reason, which is what kept the code from drifting.

Refutation is a first-class outcome. An agent that accepts every review comment
to get approved produces worse code, not better.

## Install

Needs `git`, `gh` (authenticated), and two agent CLIs. Rust 1.85 or newer to
build (`rustup update` if yours is older).

```bash
cargo install spar-cli
spar doctor
```

The package is named `spar-cli` because `spar` is taken on crates.io by an
unrelated config parser. The binary it installs is `spar`, which is what you
type.

For whatever is on main rather than the last release:

```bash
cargo install --git https://github.com/coreyphillips/spar spar-cli
```

From a clone:

```bash
git clone https://github.com/coreyphillips/spar && cd spar
cargo install --path .
```

`spar doctor` checks every prerequisite, resolves each configured agent, and
tells you which is missing. Presets are compiled into the binary, so there is
nothing to copy alongside it.

### After upgrading

`spar init` will not touch a config that already exists, so a release that adds a
setting would otherwise be invisible. `doctor` lists every setting your file does
not mention, with its default, and `init --update` appends them as comments
without changing a line of what is already there:

```bash
spar doctor                 # says what is new since you wrote your config
spar init --update          # appends those settings, commented out
```

## Trying it without letting it do anything

Every command that writes has a way to not write. Worth knowing before the first
run on a repository you care about:

```bash
spar triage 42              # judges the issue, writes plan.json, touches nothing
spar review 108 --dry-run   # full two agent review, printed, nothing posted
spar run 42 --no-close-skipped   # will not close an issue it declines
```

`--dry-run` is on `review` only, because `review` is the command whose whole
output is a comment. There is no `--dry-run` on `run`, since `run` writes code:
`triage` is its read-only half.

For something persistent rather than per invocation, these live in `spar.toml`:

```toml
[loop]
followups   = "local"   # the default. Records to .spar/followups.md, never the
                        # tracker. "none" drops them entirely.

[style]
pr_comments = "none"    # never comment on a pull request, in any mode.
                        # The standing equivalent of --dry-run.
```

## Quick start

```bash
cd ~/projects/thing
spar init      # detects your installed CLIs, writes spar.toml
spar doctor    # confirms it works
spar run 478   # triage issue 478, and open a PR if it is worth doing
```

## Use

Point it at a number and it works out the rest. Issues and pull requests share
one number sequence per repository, so spar sorts them itself: `spar run 42` does
the right thing whether 42 is an untouched issue, an issue that already has a
pull request open, or a pull request. Omit the numbers and `run` takes every open
issue while `resume` takes every open PR.

```bash
spar run                    # triage every open issue, then work them in order
spar run 42 51 60           # or name the ones you care about
spar triage                 # triage only, write plan.json, touch nothing
spar run --limit 50         # raise the cap on how many open issues to take
spar run --min-number 480   # ignore anything numbered below #480
spar run 42 --auto-merge    # merge when no blocking findings remain
spar run 42 --max-rounds 5  # allow more review rounds before escalating
spar run 42                 # if 42 already has an open PR, continues that
spar run 101                # 101 is a PR? it reviews it, no triage
spar run 42 101             # mixed is fine
spar run 42 --first codex   # the other agent implements
spar run 42 --base develop  # base branch, if origin/HEAD is not what you want
spar run 42 --plan-out /tmp/plan.json      # where the triage plan is written
spar run 42 --no-close-skipped   # comment on a declined issue but leave it open
spar run 42 --close-skipped      # close it (the default, so rarely needed)
spar run 42 --absorb 1      # work the follow-ups this run files, in this run

spar review 108             # review a PR without touching it, fork or not
spar review 108 --dry-run   # print the review instead of posting it
spar review                 # review every open PR

spar resume                 # continue the loop on every open PR
spar resume 108             # or name them
spar resume 108 --next codex     # override whose turn it is

spar run 42 --quiet         # suppress progress; warnings and errors still print
```

`--quiet` works before or after the subcommand.

With no numbers given, spar takes the 20 lowest numbered open items. It says
which ones it picked, and says so explicitly when there were more than the cap
rather than quietly truncating. Raise it with `--limit`.

Because it takes the **lowest** numbered items, a repository that has been going
a while walks straight into a tail of old issues nobody is going to reach.
`min_number` puts a floor under that:

```bash
spar run --min-number 480      # or min_number = 480 in spar.toml
```

The floor is applied before the cap, which is the part that matters: filtering
afterwards would fill the cap with the oldest items and then discard them all,
leaving nothing. So `--min-number 480 --limit 20` gives you the twenty lowest
numbered items **at or above** #480, and spar says how many it skipped. A number
you name explicitly is always honoured, floor or not, because naming it is the
point.

Works on any GitHub repo you have cloned with push access. It follows `gh`
conventions: the repo comes from the checkout you point at, and the base branch
is detected from `origin/HEAD` rather than assumed to be `main`.

```bash
spar run --repo ~/projects/thing 42     # --repo belongs to the subcommand
```

## How a run goes

**Triage.** Both agents independently judge every issue: is it worth doing, how
complex, what does it depend on, how risky. They run at the same time, and
neither sees the other's answer. Then reconcile mechanically:

- Both say do, it is scheduled.
- Both say skip, the shared reasoning is posted and the issue is closed as not
  planned. Turn that off with `close_skipped = false`.
- They disagree, that issue is parked for you and the run continues. One agent
  never overrules the other.

Scheduled issues are ordered by dependency, then cheapest first, so blockers
clear early and risky work inherits a healthier base. The plan is written to
`plan.json`.

**Per issue.** An isolated git worktree, so a failed issue cannot poison the
next one's base. The first agent implements and opens a PR. Custody passes to
the other, which reviews with full repo context rather than a bare diff. Each
finding is labelled `blocking`, `non-blocking`, or `nit`.

**Rounds.** `max_rounds` is a budget for one invocation, not a lifetime cap on
the PR. A run that escalates after three rounds and is then resumed gets three
more, because a person looked at it and chose to continue. Round numbers keep
counting up (4, 5, 6) so the refutation ledger and the PR history stay coherent,
and the escalation comment says both how many rounds this run spent and how many
the PR has seen in total.

**Follow-ups.** A review that finds something real but out of scope files it as
its own issue. Before filing, spar looks for an issue that already describes the
same defect, comparing titles and bodies rather than matching strings, because
two agents never word one defect the same way. If it finds one it adds whatever
the new pass learned as a comment there instead of opening a second issue, and
says nothing if the new pass learned nothing.

By default those follow-ups wait for the next run. `absorb_new_issues` folds them
back into the current one instead, a wave at a time, each wave triaged like any
other issue so both agents still have to agree it is worth doing. It is off by
default because it multiplies what a run costs.

**Convergence.** Only `blocking` findings gate the merge. Everything else is
filed as a follow-up issue and the PR proceeds. This matters: a competent
reviewer can always find something, so "no objections remaining" is not a
stopping condition, but "no blocking objections" is.

## Resuming work someone else started

`spar resume <pr>` picks up any open PR, whether or not spar created it. Human
authored, agent authored, half finished, does not matter: if it has a branch and
a diff, the loop can take custody of it.

`spar run <issue>` does the same thing when that issue already has work open. It
checks its own branch naming first, then falls back to GitHub's issue linkage, so
a pull request somebody else started on a branch called anything at all is found
and continued rather than duplicated. Running spar again on work in progress is
always a resume, never a restart.

If a branch carries pushed commits that no open pull request accounts for, spar
refuses rather than force pushing over them, and says how to recover.

## Reviewing what you cannot push to

A pull request from a fork cannot be resumed: spar has to push its fixes back to
the PR's branch, and that branch is not in your repository. Reviewing it is still
the useful thing, and for an outside contribution it is usually the only thing
you wanted, so `spar review` does that and `run` falls into it automatically when
it meets a fork.

```bash
spar review 108             # any PR, fork or not
spar review 108 --dry-run   # see it before it is posted
```

### Agreeing with a dry run

A dry run keeps what it produced, so liking it does not mean paying for a second
review:

```bash
spar review 108 --dry-run   # prints it, and saves it under .spar/reviews/
spar post 108               # posts exactly that, agents not run again
```

`spar post 108 --dry-run` shows what would go up. The saved file is plain
markdown, so editing it before posting is the expected thing rather than a
trick: strike the finding you disagree with, then post. Anything you post still
goes through the style gate, so an edit that reintroduces an em dash is caught
rather than published. `spar post 108 --file notes.md` posts something else
entirely.

The same saving happens when `pr_comments = "none"` suppresses a comment, so
nothing spar spent money producing is thrown away.

The loop is different here, and deliberately so. The custody loop converges
because the diff changes between rounds. In review only mode nothing changes, so
more rounds would just re-litigate the same unchanged code. It is three passes:

1. **Independent review.** Both agents review at the same time, neither seeing
   the other. A finding both reach on their own is the strongest signal there is.
2. **Cross-adjudication.** Each reads the other's remaining findings, goes to the
   code, and rules on them. A point one model raised and the other examined and
   rejected is usually a pattern match, and saying so is worth more to you than
   forwarding both.
3. **Rebuttal.** Anything rejected goes back to whoever raised it, to withdraw or
   to substantiate with the line, the input, the failing case.

What comes out is sorted by how well it is attested, so a maintainer can see at a
glance which findings carry two independent signatures:

```
Two independent reviews.

needs changing before merge
- Retry loop never terminates when max_attempts is unset (src/net.rs:88) [both].
  Both reviewers reproduced this: the guard on line 91 compares against Some(0).

the reviewers disagree, your call
- Config loader swallows a parse error (src/config.rs:210). Objection: the
  caller validates against the schema first. Answer: the validation runs after.
```

Nothing is committed, pushed, merged, or closed. `--max-rounds` picks how far it
goes: 1 is two independent reviews with no cross-checking, 2 adds it, 3 adds the
rebuttal.

This is also the cheapest way to adopt spar. No agent writes a feature from
scratch; it only reviews what already exists.

Resuming needs state that GitHub cannot provide. Every agent commits and
comments as the same git identity, so `author` is always the human who ran spar,
and authorship reveals nothing about who acted last. So spar keeps its own
state: round number, whose turn is next, and the full disputed ledger. That
lives in `.spar/state/` by default, and can also travel on the PR as a hidden
comment (`state_store = "pr"`) if a run might be picked up from another machine.

A PR with no spar state starts fresh, with the agent that did not implement
taking the first review.

## Keeping it readable

The reader of a PR is a person with other work. Model prose defaults to three
paragraphs where one sentence would do, and asking nicely for brevity has the
same reliability problem as asking for no em-dashes.

So spar does not forward what a model wrote. It asks for structured findings,
composes every comment itself, and clips each field to a budget. It also never
narrates its own working: no agent names, no round numbers, no counts of things
listed on the next line. A reader wants to know about the code, not about the
tool. A review with work to do gives the detail only for what blocks, and lists
the rest by title:

```
One real problem, the rest are follow-ups.

blocking
- Retry loop never terminates (src/net.rs:88). Confirmed by running the 429 test
  with max_attempts unset: it spins.

non-blocking
- Timeout is not configurable (src/net.rs)
- Error message does not name the host (src/net.rs)
```

spar is also quiet while it works. The agents never read the PR thread, they get
findings through their prompts, so nothing in the loop depends on any of it being
posted. A run leaves **one** comment, and only when it has something to say:

```
Not signed off: the last round of fixes was pushed but has not been reviewed.

Raised and refuted:
- Config loader swallows a parse error. The caller validates against the schema
  before load_config is reached.

Filed separately: #485, #486
```

A run that converged cleanly and filed nothing posts nothing at all. The absence
of objections is the message, and the diff already records what was fixed. What
survives is what is unrecoverable elsewhere: what is still unresolved, what was
argued down and why, and where the follow-ups went.

Set `pr_comments = "rounds"` for a comment per review and per response, which is
an audit trail at the cost of a thread nobody wants to read, or `"none"` to keep
GitHub out of it entirely.

**A filed issue is written as a bug report.** The agents are asked for the parts
of one separately, and spar assembles them under headings, skipping any that do
not apply:

```markdown
## Problem
What is wrong, with the specifics: the function, the call it does not make.

## Reproduction
Numbered steps, then an Actual result list. If part of what happens is correct
and only part is the defect, which.

## Impact
What an operator or a user can do, or loses, because of this.

## Expected behavior
Requirements specific enough to implement and to test.

Found while working on #400.
```

A finding that stays in the pull request thread carries none of that and is just
its one line, so nothing sprouts empty headings for the sake of a format.

**A filed issue is not a comment, and is not held to a comment's budget.** A
comment is read with the diff in front of you; an issue is picked up cold, months
later, by somebody who was not there. So issues get `max_issue_body_chars`,
several times a comment's allowance, and the agents are asked for what a person
picking one up actually needs: what goes wrong, how to reproduce it, the file and
line, and what would fix it.

Fenced code blocks in an issue are **never truncated and never count against the
budget at all**. A snippet cut in half is broken markdown and a misleading
fragment of the code somebody is being asked to fix. When prose does have to be
dropped, whole blocks go from the end, so what survives is complete.

The lengths in `[style]` are **safety valves, not editors**. They are sized so
real content is never touched, and when one does fire it finishes the sentence in
progress rather than stopping mid-thought. Cutting substance was a mistake worth
naming: a reader who cannot act on a finding has been given nothing, and the
characters saved bought nothing. Brevity is asked for in the prompts, which is
free, and enforced only on shape. Set `terse = false` to remove even the valves. To see exactly what spar would post, before spending a
token on it:

```bash
cargo run --example preview            # every comment type, gate on
cargo run --example preview -- --loose # the same input with the gate off
```

## Failure modes it handles

Cross-model review loops break in specific ways. These are handled explicitly.

**The nitpick spiral.** Round 6 findings are worse than round 1 findings and a
naive loop cannot tell. Severity gating means only real defects block.

**Re-litigation.** If A refutes a point and B raises it again next round, the
loop never ends. Refuted points are hashed into a ledger carried across rounds
and injected into the next reviewer's prompt as settled. Re-raising needs new
evidence, and a second re-raise escalates to you.

**Approval drift.** Optimizing for "get approved" pressures the author into
accepting wrong review comments. Refutation is explicitly blessed in the prompt,
disputes are surfaced in the summary, and the merge gate is
blocking-findings-empty rather than reviewer-satisfied.

**Merge authority.** Two models agreeing is not the same as being right, and
neither carries the consequences. `--auto-merge` is off by default; the terminal
state is "approved, ready for a human to merge". Branch protection on the target
repo should be considered required, not optional, if you turn it on.

**Style rules models forget.** Prompting alone is unreliable over a long run.
Every commit message, PR body, and comment is scrubbed deterministically and
then re-verified. A leak is a hard error, not a warning.

**Two agents that are secretly one.** Config keys are arbitrary, so `alpha` and
`beta` can both be Claude on the same model. spar compares the resolved binary
(by inode, so a symlink does not fool it) and the configured model, and warns
loudly. An approval from a model reviewing itself looks identical to a real one,
which is worse than no review at all.

## Configuration

`spar init` writes a config from the CLIs you actually have:

```
  missing  aider
  found    claude     /Users/you/.local/bin/claude
  found    codex      /Applications/ChatGPT.app/Contents/Resources/codex
  missing  gemini

wrote spar.toml
```

```toml
[agents.claude]
preset = "claude"
model  = "fable"        # omit to use the CLI's own default
effort = "high"

[agents.codex]
preset = "codex"
model  = "gpt-5.6-sol"
effort = "ultra"

[loop]
max_rounds        = 3       # then escalate rather than loop
auto_merge        = false
first_implementor = "claude"
close_skipped     = true
worktrees         = true

[loop.effort_schedule]
round_1 = "ultra"           # the deep pass
rest    = "high"            # later rounds only see a small delta

[style]
ban_em_dash        = true
ban_ai_attribution = true
terse              = true
```

See [`spar.example.toml`](spar.example.toml) for every option with its
reasoning.

## Pairing other agents

An agent is a command template plus an output adapter, not a class. Supporting a
new CLI is a preset file, not a code change. Presets ship for claude, codex,
gemini, and aider; any two can be paired, and agent names are arbitrary.

To wire up something with no preset, declare it inline:

```toml
[agents.custom]
command = ["mytool", ["-m", "{model}"], "--prompt", "{prompt}"]
output  = "text"            # text | jsonl | json
```

Placeholders: `{prompt}` `{system}` `{model}` `{effort}` `{cwd}` `{schema}`
`{schema_file}`.

An argument group whose placeholder is unset is dropped whole, so omitting
`model` drops the `-m` flag rather than passing an empty string.

Include `{schema}` or `{schema_file}` and spar uses the CLI's native structured
output. This is worth doing rather than optional: without it spar asks for JSON
in the prompt and parses whatever comes back, and a long answer that hits the
model's output limit stops mid-object, which costs you that agent's whole review.
`{schema}` passes the schema as an argument, `{schema_file}` passes a path to it,
and a CLI that supports either is enough. When a structured answer is unusable
anyway, spar asks once more with the parser's complaint attached before giving
up on that agent.

For a CLI that emits an event stream rather than plain text, say where the
answer lives:

```toml
output       = "jsonl"
message_path = "item.text"

[agents.custom.message_match]
type        = "item.completed"
"item.type" = "agent_message"
```

Binaries are located by walking `SPAR_<NAME>_BIN`, then PATH, then the preset's
`search_paths`. Nothing is hardcoded, and a miss reports every location tried.

Every option a preset supplies can be overridden per agent, including `timeout`
(seconds one call may take before spar gives up), `search_paths`, and
`system_via`. [`spar.example.toml`](spar.example.toml) lists the full set.

Presets are embedded in the binary, and a file on disk of the same name wins
over the built-in copy. So when a CLI's flags drift you can fix it without
waiting for a release:

```bash
mkdir -p ~/.config/spar/presets
spar doctor   # see what it resolves today
$EDITOR ~/.config/spar/presets/codex.toml
```

`SPAR_PRESET_DIR` overrides the search, and `.spar/presets/` in the repo you are
working on is checked first, for an override that should travel with one
project. A bare `presets/` directory is deliberately *not* searched: it is an
ordinary directory name in plenty of projects, and shadowing a built-in preset
by accident produces a baffling failure.

## Housekeeping

```bash
spar clean          # drop worktrees, branches, and state whose PR is finished
spar clean --all    # drop every worktree and branch spar created, review ones too
spar clean --pr-state   # also delete state comments left on finished PRs
spar doctor         # check prerequisites and resolve each configured agent
spar doctor --config other.toml   # check a config before adopting it

spar init                     # write spar.toml from the CLIs you have
spar init --out other.toml    # somewhere else
spar init --force             # overwrite an existing config
```

`--close-skipped` and `--no-close-skipped` are offered only on `run`, since it is
the only command that triages and so the only one that can decline an issue.

Each issue gets its own git worktree so a failed run cannot poison the next
one's base. A worktree is released as soon as its run reaches a terminal
outcome, and kept only on `escalated` or `error`, where you may want to inspect
local state. Pass `--keep-worktrees` to hold on to them regardless, or
`--no-worktrees` to work directly in the main checkout.

Releasing matters for more than tidiness: a stranded worktree holds its branch
checked out, which makes a later `gh pr merge --delete-branch` fail to clean up.
Runs also sweep any finished worktrees on start.

Cleanup only ever touches branches spar recorded creating. Branch names default
to `issue-N`, which is exactly what a person would call a branch by hand, so the
name alone can never establish ownership: `spar clean --all` will not delete
your `issue-9`.

## Cost

Every review round is a repo-aware pass at the configured effort. A full ultra
review of a three-line round-3 delta is money on fire, which is what
`effort_schedule` exists to prevent. Both agents bill against their respective
subscriptions.

Rough shape per issue: two calls for triage, one to implement, then two per
review round. `spar review` is cheaper, four to six calls for a whole pull
request, because nothing is being rewritten between passes.

## Caveats

On macOS the Codex CLI usually lives inside `ChatGPT.app`, currently an alpha
build, so its path and flags will drift. That location is only one entry in the
preset's `search_paths`, not an assumption: PATH wins, `SPAR_CODEX_BIN`
overrides everything, and a miss reports every location tried rather than
degrading quietly.

Triage runs both agents concurrently in the repo root. They are told to read
only, but they are real agent CLIs with write permissions. Commit or stash
anything you care about before a run, which you would want to do anyway.

## Development

```bash
cargo test          # unit and integration tests, no network needed
cargo clippy --all-targets
cargo fmt
```

The integration tests build real git repositories in a temp directory and
exercise the worktree, branch-ledger, and commit-rewrite paths end to end. They
never touch the network and never call `gh`.

`cargo run --example preview` renders every comment spar can post, from
deliberately verbose model output. It is the fastest way to judge a change to
the concision gate.

## License

MIT.
