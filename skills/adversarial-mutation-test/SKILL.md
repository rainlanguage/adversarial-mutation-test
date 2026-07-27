---
name: adversarial-mutation-test
description: Use to systematically find BUGS in and harden the test suite for a WHOLE repository (or a whole module of it). Two co-equal goals the name carries: ADVERSARIAL (treat spec/intent as the oracle and the code as suspect — derive expected behavior independently and hunt for inputs where the code is wrong) and MUTATION (prove tests cover the code). Mutation-drives a behavior-centric coverage ledger — for each behavior, break the line and check the whole suite: existing tests that kill the mutant are validated and logged (so existing coverage is audited and in scope), and only surviving mutants (real gaps) get a new discriminating test. An existing test that already kills mutants is left as-is; one meant to cover a behavior but that a mutant survives is strengthened in place (not duplicated); one broken on the unmutated baseline is fixed or its underlying code bug surfaced; a test is never edited to swallow a mutation. Designed for long campaigns that outlive the context window: progress lives in a durable gitignored scratch file so it survives compaction. A single change/PR/function is just a narrowed scope. Triggers on "test the whole repo", "harden the test suite", "mutation test the codebase", "audit the tests", "adversarial tests", "prove these tests cover the code", "exhaust the eventualities".
version: 0.30.0
---

# Adversarial Mutation Testing (whole-repo, resumable)

This has TWO co-equal goals, and the name carries both: **adversarial** = hunt for actual BUGS (places the code does the wrong thing), and **mutation** = prove the tests cover the code. Default scope is the **whole repo** (or a whole module of it); a single change / PR / function is just a narrowed target. Any language / harness.

- **Mutation** treats the current code as the oracle: break a line, check that a test notices → finds **test gaps**. Every test makes an assertion that differs between correct and buggy code, validated by breaking the exact line it claims to cover and confirming the test fails.
- **Adversarial** treats the *spec/intent* as the oracle and the **code as suspect**: independently derive what the code *should* do, then actively try to make it do something wrong → finds **bugs**.

Doing only the mutation half is the common failure: it pins whatever the code currently does and structurally CANNOT find a bug, then rationalizes "no bugs found" as fine. A run that adds tests but never tries to break the code has done half the job. Every unit gets BOTH passes (see the adversarial pass below).

A whole-repo run is long and **will outlive the conversation's context window**. Treat conversation memory as unreliable; committed git history (tests, filed issues) is the durable record of the work itself.

**Run this in ultracode — native Workflow orchestration.** This skill is written for ultracode: the whole-repo campaign is a fan-out driven by the native Workflow tool — `agent()` / `parallel()` / `pipeline()`, schema-forced structured returns, the run journal (`resumeFromRunId`), and `budget`. Let those primitives own concurrency, resume, and convergence — and do NOT hand-roll them — while each worker takes its own **fresh clone** for isolation (see Parallelizing; worktrees are wrong for this skill). The **orchestrator** (the agent authoring the Workflow) owns the survey-slice, the loop-until-dry convergence, and the final synthesis; subagents do the per-unit work and return validated findings. A narrowed scope (one change / PR / function) can run inline and serial without a Workflow; everywhere below, "orchestrator" means the Workflow script when fanned out, and the serial driver when not.

## 0. Durable progress — two roles, kept separate

A long run has two distinct durability needs; don't collapse them into one mechanism:

- **Run-time resume + convergence (fan-out) is owned NATIVELY.** Re-invoke the Workflow with `resumeFromRunId` and the unchanged prefix of `agent()` calls replays from cache — completed units/batches are never re-run. The orchestrator's own loop state (the sliced worklist, round counter, seen-set) is the convergence signal. Do NOT recover run state by having agents read/write a shared progress file — that is the agents-edit-shared-state bug (see Parallelizing).
- **Human/audit trail + serial-mode resume is a gitignored scratch file** `.mutation-test/PROGRESS.md` at repo root. It is the human-readable narrative and, in the non-Workflow serial mode (a narrowed scope run inline), the resume source. It is **never** the run-time convergence signal for a fan-out.

Set up the scratch file first:
- Make git ignore it **without a tracked change**: append `.mutation-test/` to `.git/info/exclude` (preferred — no repo diff). Use `.gitignore` only if the team wants it shared.
- It holds: **scope**; the **harness commands** (build / test / regenerate-artifacts / coverage); the full **worklist** with per-unit status; and the per-unit **mutation matrices**.

Protocol:
- **Serial mode (no Workflow):** on re-entry after compaction, READ PROGRESS.md to recover; **mark exactly one unit `IN PROGRESS`** with a one-line note of the precise sub-step so a mid-unit interruption resumes exactly; after each unit update it (`DONE`, matrix, commit hash). Keep it small and skimmable.
- **Fan-out mode:** the orchestrator writes PROGRESS.md (and the committed audit record) ONCE, from the aggregated structured returns of its agents — subagents never edit it; resume is `resumeFromRunId`, not parsing the file. **So every "record … in PROGRESS.md" / "resume there next iteration" instruction in the steps below is the *serial-mode* action**; under a Workflow the executing agent instead RETURNS that (its matrix, bug candidates, branch/PR, unprobed list) as its schema-validated result, and the orchestrator records it once.

Suggested format:
```
# Adversarial Mutation Test — PROGRESS
Scope: <whole repo | module X>
Harness: build=<cmd> | test=<cmd> | regen=<cmd or "none"> | coverage=<cmd>

## Groups & worklist  (one branch + PR per group)
### math-libs — branch <name> — PR #123 (merged)
- [DONE] LibFoo — validated; 2 existing confirmed, 1 gap filled
- [DONE] LibBar — validated; 1 gap filled
### arb — branch <name> — PR not opened yet
- [WIP]  GenericPoolArb.exchange — probing branch 2
- [TODO] RouteProcessorArb
...

## Coverage ledger  (unit.behavior -> mutation -> killer; SURVIVED = gap)
LibFoo.guard: negate      -> existing:testGuardRejects ✓   (existing test validated)
LibFoo.cap:   off-by-one  -> SURVIVED -> added:testCapBoundary ✓
LibFoo.emit:  drop emit    -> SURVIVED -> GAP (todo)
```

The ledger is **behavior-centric**: each behavior gets a row recording the mutation and the test that killed it — *existing or new*. So existing tests enter scope and the tracker by being **mutation-validated** (they earn a row only by actually killing a mutant), and weak existing tests are exposed: a line `coverage` calls "covered" whose mutation **survives** had only incidental coverage and is still a gap.

**Coverage is a separate axis from correctness.** A killed mutant proves the behavior is PINNED (a test and the code agree), never that it is CORRECT — the test may simply mirror the implementation and enshrine a bug. This ledger answers "is it tested?"; the adversarial pass answers "is it right?", and it judges EVERY behavior including the ones marked covered here. Never read a `covered ✓` row as validation.

## Repo-wide campaign

1. **Survey & inventory.** Enumerate testable units (modules / contracts / files / public functions) and existing tests. Find the gaps with coverage tooling — `forge coverage`, `cargo llvm-cov`, `pytest --cov`, `go test -cover`, `nyc`, etc. Zero-coverage and weakly-covered units are highest value. Write the worklist into PROGRESS.md.
2. **Prioritize, chunk, and group.** Order by risk × coverage gap (security-critical / complex / untested first). Process **one unit at a time**, but organize the worklist into **logical groups** — a module / package / coverage area (a handful of related units) is one group, and **each group ships as its own branch + PR**. This keeps PRs small and reviewable and ships coverage incrementally instead of one giant branch.
3. **Learn the harness once.** Discover build/test commands and any **artifact-regeneration** step (compiled output, generated bindings, etched/deployed bytecode, golden files, snapshots) BEFORE mutating. Record them in PROGRESS.md. Re-running tests after editing source but without regenerating the artifact they execute tests **nothing**.
4. **Run the per-unit loop**, **committing each unit's tests** as you finish it (durable record; the scratch file only tracks meta-progress). Work each group on its own branch off the default. When a group's units are done and the suite is green, **push and open a PR for that group**, then start the next group on a fresh branch. Added tests are additive (no source/bytecode change), so per-group test PRs are independent and CI-safe — they review and merge on their own; don't accumulate everything on one branch. Record each group's branch + PR in PROGRESS.md.
5. **Resume.** Serial mode: on re-entry, read PROGRESS.md and pick up the current group's `[WIP]` (or next `[TODO]`) unit. Fan-out mode: re-invoke the Workflow with `resumeFromRunId` — the journal replays completed agents from cache and the orchestrator's sliced-list state resumes convergence; the audit record is regenerated from the aggregated returns, not parsed to decide what to redo. Either way: never re-do a completed unit; if a group is finished but unshipped, push + open its PR first; stop only when the worklist (or agreed scope) is exhausted, then report repo-wide coverage proven, the PRs opened, and gaps remaining.

## The per-unit loop

Drive it by mutation so existing tests are **credited** and you only add tests for genuine gaps. **Never edit or delete existing tests — only add.** Every behavior is judged against the *whole* suite (existing + anything you add).

1. **Enumerate behaviors.** Each conditional, comparison, computation, side-effect, filter, early-return, and error/skip path is a separate thing a test can claim to cover. Include happy path, every branch, boundaries, interactions, and "should NOT happen" cases.
2. **Baseline.** Run the existing suite on the *unmutated* code. If green, proceed. If a test fails here (not under a mutation), it's legitimately broken — stop and diagnose: fix a genuinely-wrong/outdated/flaky test, or if the failure reveals a real code bug, surface it. Never mask a baseline failure by editing the assertion to match buggy behavior, and don't start mutating on top of a red baseline.
3. **Probe each behavior with a mutation.** Apply ONE targeted mutation that breaks exactly that behavior (catalog below); **make it live in what the tests execute** (regenerate any built/cached/generated/etched artifact, and sanity-check the mutation actually changed behavior — stale artifacts are the #1 failure mode); run the **whole** suite.
   - **A test fails → behavior already covered.** Note which test (often a pre-existing one). No new test needed.
   - **No test fails → the mutant *survived* → a real coverage gap.**
   - **Restore** source via VCS before the next probe. Never leave or commit a mutation.
4. **Kill each surviving mutant.** Make a test catch it:
   - If an existing test **purports** to cover that behavior (by name/intent/setup) but the mutant survived, the test is inadequate → **strengthen it in place** — tighten its assertions until it fails under the mutation. (Making a test *fail* under a mutation it should catch is the goal — the opposite of editing one to *pass* under a mutation.)
   - If **no** existing test targets the behavior → **add a new** test.
   Either way the test must be **discriminating** — its assertion yields a *different observable value* under correct vs. wrong code (not "it runs" or a bare "it reverts"; prefer exact values / events / amounts) — and must **pass** on the unmutated baseline and **fail** under the mutation. Re-apply, regenerate, confirm both, then restore.
5. **Record the matrix** in PROGRESS.md: each behavior → covered by (existing test / new test) / still-gap.
6. **Loop until dry — a single pass is NOT done.** One prioritized pass over a handful of high-value behaviors leaves the long tail (and the subtle bugs) uncovered. Re-survey the unit for behaviors you have not yet probed — large units have dozens: every public/external entrypoint, each branch and boundary, every revert/skip/early-return path, each accounting step, event, and cross-feature interaction — and keep probing until a **full pass adds no new gap**. Unit size and per-probe cost (e.g. regenerating etched artifacts) are NEVER reasons to stop early or to sample; they only change *pacing*. "Already heavily hardened" is a hypothesis to disprove by mutation, not a reason to skip. If a unit is genuinely too large to finish in one sitting, do NOT declare it done — record in PROGRESS.md the exact list of behaviors still UNPROBED and resume there next iteration.

## Adversarial correctness pass (find BUGS, not just gaps)

Run this PER UNIT alongside the mutation loop — it is the half that finds bugs. The mutation loop asks "is this behavior tested?"; this asks "is this behavior CORRECT?".

0. **Ingest the authoritative intent oracle FIRST.** The spec, NatSpec, interface contracts, and domain invariants are the oracle for what the code SHOULD do — start there and treat the code as suspect. Do NOT substitute a convenient intent the code happens to satisfy (the classic self-own: observing "dust is retained by the contract" and calling it "conservative, safe" when it's really an orphaned-funds bug). Widen the bar beyond "exploitable": accounting/UX/correctness divergences (orphaned funds, reverts-that-should-succeed, wrong-but-not-stolen values) are findings too.
1. **Derive the intended behavior independently** of the code: from the authoritative oracle above, plus type/unit constraints and domain invariants. Write the expected value/property from intent — do NOT read it off the code's current output.
2. **Enumerate invariants and properties** the unit must uphold: conservation (no value created/destroyed; balances reconcile), monotonicity, bounds (no overflow/underflow/precision loss), rounding DIRECTION (does rounding ever favor the wrong party?), idempotence, access control (only authorized callers), isolation (no cross-owner/namespace/account leakage), ordering independence, reentrancy safety, and "this must NEVER happen" cases.
3. **Try to FALSIFY each** — adversarially, against REAL dependencies. Construct the inputs, sequences, boundaries, and hostile counterparties most likely to violate the property: extreme/zero/max values, non-default decimals, dust/rounding edges, repeated/reordered/interleaved operations, reentrant callbacks, unexpected token behaviors, self-referential parties. Exercise the **real** tokens/contracts/dependencies, not the suite's always-succeed mocks — a mock that returns true for everything structurally hides conservation/decimal/solvency divergences. A single falsifying case that FAILS on the unmutated baseline against a real oracle is a **candidate bug**.
4. **When a mutant SURVIVES, ask the adversarial question first.** A surviving mutant means no test pins the behavior — before reflexively pinning the *current* output, check the current output is actually CORRECT per step 1. If it is, add the test (mutation gap). If it is NOT, you've found a bug: do not enshrine it in a test.
4b. **A KILLED mutant means PINNED, not CORRECT — covered behaviors are still in adversarial scope.** A test killing a mutant only proves the test and the code agree; they can be co-wrong. Tests routinely **mirror the implementation** — assert the code's own output, hardcode the value the code happens to produce, or recompute the "expected" with the SAME formula the code uses — so a green, mutant-killing test faithfully **enshrines whatever the code does, bug included**. Therefore run the correctness check (steps 1–3) on well-covered behaviors too, NOT only on surviving mutants. Treat as a red flag any existing test whose expected value looks **derived from the code** (same magic constants, same formula, "assert x == <the thing the code returns>"): re-derive the expected value INDEPENDENTLY from the spec; if it differs from what the test asserts, the test is enshrining a bug — surface it for triage, do not trust the green check. "An existing test covers it" is never a reason to skip correctness review.
5. **Surface every candidate for TRIAGE — do NOT adjudicate it yourself.** You are not the arbiter of bug-vs-intended; the code owner is. For each observation write a neutral triage item — the behavior, where, why it might or might not be intended, and a cheap repro if available — and hand it to the owner / a triage list. **Ambiguous → flag it, never drop it.** Refutation reasoning is attached as a NOTE, never used as a GATE: auto-refute pipelines (refute-by-default skeptics, majority-vote, "not exploitable so discard") silently bury real-but-subtle and non-exploitable findings (orphaned dust, reverts-that-should-succeed) — that is the failure mode this whole pass exists to avoid. The only thing you may discard is a provably-broken repro you wrote yourself (the test asserted the wrong thing); even then keep the underlying behavior question on the list. Never weaken an assertion to make the code look correct. "I judged it fine" is not a disposition you get to make.

Record bug candidates separately from coverage in PROGRESS.md (e.g. a `## SUSPECTED BUGS` section): unit, the violated property, the repro, verify status (real / refuted / needs-input). "No bugs found" is only credible after this pass actually ran.

The output of this pass is a **triaged finding with a verified repro**, not a passing test merged into the suite: a bug-repro merged green either enshrines the bug or rots red, so it belongs ON the finding (the mutation pass produces the green coverage PRs; the adversarial pass produces filed issues + their repros). And **re-verify a candidate yourself before filing it** — re-run the repro against current code, because a sub-agent's scratch repro is often gone or asserted the wrong thing; file only what you reproduce. File it per **Findings → issues** below — including the label gate, which is what makes the finding visible to anything outside the issue itself.

## Findings → issues (the adversarial pass's output)

Findings are tracked as **GitHub issues** — the durable product record, and the half of the run that is not a PR. The orchestrator files them after synthesis and after re-verifying each repro, not per-agent mid-run.

- **Every filed finding carries the `audit` label. It is not optional.** `audit` is the org-wide handle for "this repo has an outstanding finding": the `rain-org-health` scan counts a repo's backlog with `gh search issues --owner <org> --label audit --state open`. A finding filed without it is **invisible** — the graph reports the repo as having zero outstanding findings, so as far as every consumer downstream of the issue is concerned the finding does not exist. This is not hypothetical: `rain.solmem`'s adversarial pass filed three real findings (#50, #54, #55) with no label, and the dashboard read `openAuditIssues: 0` until they were hand-labelled days later.
- **Create the label set FIRST (mandatory, before the first `gh issue create`).** `gh issue create --label <name>` **hard-fails when the label does not exist in that repo**, and a repo being scanned for the first time usually has neither label. So, once per repo, list what exists and create every missing label before filing anything:
  ```sh
  gh label list -R <org>/<repo> --limit 100
  # for each missing name in: audit adversarial
  gh label create audit       -R <org>/<repo> --color 5319E7 --description "Audit finding"
  gh label create adversarial -R <org>/<repo> --color A371F7 --description "Adversarial mutation-test finding"
  ```
- **Never recover from a label error by filing the issue unlabelled.** Dropping the label turns a loud failure into a silent one: the issue exists, reads fine, and is counted by nothing. If a label genuinely cannot be created (no permission), STOP and tell the user rather than filing unlabelled.
- **Issue shape:** Title = the violated behavior in one line (what is wrong and where — not "investigate X"); Labels = **`audit`** (required — the countable one) **plus `adversarial`** (provenance, so this skill's findings stay distinguishable from the audit skill's while both stay countable); Body = the unit, the intent oracle the expected behavior was derived from, the violated property, the verified repro, and the neutral triage framing of step 5 above — why it might or might not be intended. You are surfacing a candidate, not adjudicating it.
- **Verify the labels landed.** After filing, re-list (`gh issue list -R <org>/<repo> --label audit --state open`) and confirm every issue you just created is returned. An issue created while its label was missing is silently label-less, so a `gh issue create` that printed a URL is not proof; if any are missing, add the labels now (`gh issue edit <n> --add-label audit,adversarial`).
- **Record the filed issue numbers** in `summary.filed` of the committed scan record (see below) and in PROGRESS.md, so the run's own record and the org health graph tell the same story.

## Mutation catalog (break ONE behavior)

- **Conditionals:** negate (`x`→`!x`), force `true`/`false`, swap branches.
- **Comparisons:** `<`↔`<=`, `>`↔`>=`, `==`↔`!=`, swap operands.
- **Arithmetic / off-by-one:** `+1`→`-1`/`+0`, `*`↔`/`, drop a term.
- **Returns / outputs:** return early, empty/zero/default, a constant.
- **Side-effects:** delete a write/emit/update; or move it across a guard so it runs in the wrong cases.
- **Constants / identifiers:** change a literal, swap an error/event, use the wrong variable.
- **Filters / scopes:** remove a predicate (ownership / namespace / key) — validates isolation tests.

Pick the mutation that maps to exactly one behavior so the failing-test set is diagnostic.

## Parallelizing across groups (the native fan-out)

Groups are independent (separate branch, additive test-only PR), so fan them out with the native Workflow: one `agent()` per group, collected with `parallel(thunks)` (a barrier — you want every group's ledger together to aggregate) or `pipeline(groups, …)` for per-item flow. Each worker runs its group's per-unit loop, then commits, pushes, and opens its own PR. Let the runtime own the mechanics:

- **Isolation is a fresh CLONE per worker — not a worktree, not a shared checkout.** Each worker mutates source AND commits / restores / pushes concurrently, so it needs TOTAL isolation of two things. (a) **Build state:** `forge` writes bytecode to `out/` + an incremental `cache/`, soldeer writes `dependencies/`, and any regen step (`build.sh`) rewrites generated sources — if that untracked build state is shared, one worker's mutated/stale artifact becomes what ANOTHER worker's test executes, so a mutant reads as "killed"/"survived" for the wrong reason and the campaign **silently lies** (the #1 failure mode). (b) **Git state:** refs, hooks, config, index. A `git worktree` happens to dir-isolate (a) but **shares one `.git`**, so concurrent ref-updates / commits / checkouts — and the repo's pre-commit hooks firing on every worker's commit — contend, and a bad op's blast radius reaches the real source checkout. A **`git clone` per worker isolates BOTH**, with nothing shared except the immutable global fork/compiler cache under `~/.foundry` (which you WANT shared). So each worker `git clone`s the repo, provisions it independently (install deps + build), runs its loop, commits, pushes, opens its PR. **Do NOT use `isolation:'worktree'` for this skill — its shared `.git` is exactly the problem.** (The org-wide sweep clones each DIFFERENT repo — same primitive, uniformly.)
- **Each worker builds/regenerates inside its own clone** — keep the install / build / regenerate-artifact step so the mutated source is what the tests actually execute (stale artifacts are the #1 way mutation lies). Parallelism is **across** groups; **within** a group the `mutate → regenerate → test → restore` cycle stays serial.
- **Don't hand-manage concurrency.** The runtime auto-caps at `min(16, cpu-2)` and backstops total agents at 1000 (a runaway guard, NEVER a coverage cap). Emit one agent per group/batch and let the scheduler throttle.
- **Effort per stage.** Probing (mutate → regenerate → test → restore, observe pass/fail) is mechanical — run probe agents at `effort:'low'`. Killing a survivor with a discriminating test, and the whole adversarial correctness pass + the final synthesis, are hard reasoning — `effort:'high'`/`'xhigh'`. Don't run the bug-finding half at probe altitude.
- **Failed vs empty is the native `null` contract — don't re-implement it.** An agent that dies after the runtime's retries returns `null`; `.filter(Boolean)` drops those, and a `null` is your re-dispatch signal. An agent that ran and returned a schema-valid empty result genuinely found nothing. So "failed to provision" vs "found nothing" is distinguished for free — no sentinel bookkeeping. `agent()` retries only terminal **API** errors, so a clone / install / build failure INSIDE a worker is the worker's OWN bash to retry (have its prompt retry a few times, cleaning the partial checkout between attempts). A group whose agent came back `null` is a GAP — re-dispatch it **promptly, in parallel with the rest**, never parked "until the end"; the run isn't complete until every group either produced results or is verifiably empty.
- **Clean up the per-worker clones when the run ends.** Each clone is throwaway scratch infrastructure — a full provisioned checkout (deps + build artifacts, often hundreds of MB). When the run finishes or is abandoned, delete the clones it created — but FIRST check each for unpushed commits / uncommitted work and preserve anything of value (push it, or capture it in the issue/PR). Reusing a clone by resetting it to a new branch is NOT cleanup; never delete a checkout you didn't create for this run.

### Survey → slice → loop-until-dry (orchestrator-owned)

For a unit too big for one agent's context, the **orchestrator** slices the worklist — do NOT have agents self-select from a shared file.

- **Survey returns a validated list, not a count.** Run the survey as `agent(prompt, {schema})` forcing an array of behaviour/unit items; the Workflow validates at the tool layer, so the orchestrator gets a real array to `.slice()` with zero parsing. Force every downstream probe / kill / adversarial agent to a schema too, so the coverage ledger and triage list assemble from validated returns, never parsed prose.
- **Partition in orchestrator code** into disjoint batches (~5–8 items) and dispatch one agent per batch with its EXPLICIT items in the prompt. Every behaviour is assigned exactly once; coverage is deterministic and exhaustive by construction.
- **Converge with the native loop-until-dry, in orchestrator code.** Collect each round's structured returns (via `parallel()`, so you have the whole round), add newly-surfaced behaviours to a **seen-set**, re-slice the new ones, and fan out again — repeat until a round adds nothing new. Dedup against the seen-set, NOT against confirmed gaps. Convergence depends on orchestrator-controlled state (the sliced list is exhausted), **never** on agents faithfully editing shared state.
  - **The failure mode this avoids (learned the hard way):** do NOT have chunk-agents read a shared `[TODO]` checklist, pick "the next few", probe them, and mark them `[HUNTED]` across iterations with the loop terminating on a `dry` flag. Sub-agents reliably skip the bookkeeping — so the checklist never updates, every chunk re-reads the same `[TODO]`s and re-does the same work, the `dry` flag never flips, and the loop burns its whole budget on duplicate work (≈180 agents, zero net progress, large wasted compute, in one real run).
- **Cross-restart resume is `resumeFromRunId`** — re-invoking the Workflow replays the journaled `agent()` calls from cache, so completed batches aren't re-run and the orchestrator's loop state reconstructs natively; you don't hand-persist the sliced list.
- **Never cap coverage with a fixed agent count** — run as many batches as the (possibly growing) list requires; the 1000-agent cap is only the runaway backstop. Scale rounds to `budget.remaining()` when the user set a token target.

### Dispatcher duties (YOU, the orchestrator authoring the Workflow — not the sub-agents)

Fanning work out does NOT delegate the review; you own the synthesis, at high effort, AFTER the Workflow returns. The sub-agents wrote the rules above; these are for you.

- **A worker's conclusion is an INPUT, not a verdict.** "No candidates", "no bugs", "all green" from a worker is a claim to audit, never to relay. You have not found "no bugs" until you have reviewed what the worker actually did and saw.
- **Put the prose where you can act on it — make NOTES a schema field.** The buried findings live in the prose, so force each worker's structured return to carry it (e.g. `{candidates, notes, suppressionFlags}`). Then every "discarded", "not exploitable", "by design", "benign", "safe", "conservative", "expected", "no recovery path", "only a UX issue", "refuted", "out of scope" lands in a validated field you iterate **deterministically** — surfacing the buried behavior is orchestrator code, not a hope that you read free prose.
- **Union, never vote.** Collect verify agents with `parallel(thunks).filter(Boolean)` and UNION every surfaced candidate into the triage list; a skeptic's refutation attaches as a NOTE, never gates or out-votes the finding. Your report inherits the weakest worker — one surfaced item survives even if three others "refuted" it. Do not average findings away.
- **Cross-check a worker's "clean" claim against the oracle yourself.** Re-derive the intended behavior for an area a worker reports clean and confirm it actually exercised the risky cases (conservation, rounding direction, decimals, isolation, access) against the spec — "clean" is a claim to verify, not accept (it is how subtle accounting findings get buried under "conservative, safe").
- **Re-read suspicious reasoning for errors.** Workers analyze things backwards (e.g. claiming "pull before push" for code that pushes before pulling). If a worker's safety argument hinges on an ordering/sign/rounding claim, verify the claim against the code before accepting it.

## Leave a committed scan record (org-wide health tracking)

At the END of a run, commit a **minimal, machine-readable** record so an org-wide health-check can tell which repos were scanned recently from which are stale — and against **which release**. This is the inverse of `PROGRESS.md`: that is gitignored local working state; this is a small **committed** file that travels with the repo.

- **Predictable path, same in every repo** so a health-check can fetch it uniformly (`gh api` / raw URL): **`audit/mutation-test-scans.json`**. (Create the `audit/` directory if the repo has none — the scan record is an audit artifact and belongs with audit outputs. It is committed, unlike the gitignored `.mutation-test/` scratch dir.)
- **Append one entry per run** (keep history; the health-check reads the newest `timestamp` for recency):
  ```json
  {
    "timestamp": "2026-06-06T19:40:00Z",          // UTC, when the run finished
    "commit": "08d547f…",                          // the exact SHA scanned
    "publishedTag": "v1.2.3",                       // the published/release version AT that commit, or null if unreleased
    "commitsAheadOfTag": 0,                         // how far the scanned commit is past that tag
    "scope": "whole repo",                          // or the module scoped
    "tool": "adversarial-mutation-test", "skillVersion": "0.28.0",
    "summary": { "behaviours": 600, "candidates": 89, "confirmed": 30, "filed": ["#2651","#2660"] }
  }
  ```
- **Record what was CHECKED — including the published tag.** Staleness is "which *release* was last audited," not just "when." Resolve the published version at the scanned commit: the release tag (`git describe --tags --abbrev=0`), and/or the version in the package manifest (`soldeer.toml` / `Cargo.toml` / `package.json`). If the scanned commit is ahead of the last release, record both the tag and `commitsAheadOfTag`.
- **Land it on the default branch** — include the record commit in the findings PR, or a tiny dedicated PR; a record that never leaves a local branch is invisible to the org health-check. (Commit it even if the run found nothing — "scanned, clean, on date X" is exactly the signal a health-check needs.)
- Minimal is fine: `timestamp` + `commit` + `publishedTag` are the must-haves; the `summary` is nice-to-have.

## Principles

- **Never edit a test to pass under a MUTATION.** A test failing under a mutation is SUCCESS — it caught the injected bug; reverting the mutation restores green. Changing a test's assertion to swallow a mutation encodes the bug. The only test you adjust *mid-mutation* is a *new* one you just wrote that failed to discriminate (didn't fail under its own target mutation) — strengthen it.
- **Strengthen weak tests; don't duplicate them.** If a mutant survives and an existing test *purports* to cover that behavior, it's inadequate — tighten it in place until it kills the mutant, rather than leaving it and adding a redundant parallel test. Add a *new* test only when no existing test targets the behavior. Don't gratuitously rewrite tests that already do their job.
- **Fix legitimately broken tests; surface real bugs.** A test failing on the *unmutated* baseline is broken: fix it if its assertion is wrong/outdated/flaky, or if the failure exposes a real code bug, report the bug — never mask a baseline failure by editing the assertion to match buggy code. Distinguish **"the test is wrong"** (fix it) from **"the code is wrong"** (report it).
- **Discriminating assertions** — "got 3, expected 1" beats "it reverted".
- **One mutation, one behavior** — isolation makes the failing set identify the covered line.
- **Confirm the mutation is live** — stale artifacts are the #1 way this lies to you.
- **Harden the probe harness itself — a lying harness fakes the whole campaign.** Four integrity rules, each from a real incident where the matrix was silently wrong:
  - **Commit (or pin) the suite BEFORE the first probe.** Restore-via-VCS restores committed state; when the new tests share a file with the mutated source (e.g. a Rust in-file `#[cfg(test)]` module) and are uncommitted, the first restore WIPES them — every later probe runs testless and reports universal survival.
  - **Assert the baseline count before probing.** The harness must run the clean tree first and abort unless it sees the expected `N passed`; "0 tests ran" must be a loud failure, never a silent "everything survived".
  - **Classify probe outcomes from the test harness's own result line, not by grepping for "error".** `cargo test` prints `error: test failed` on every KILL — a naive error-grep reclassifies kills as compile failures. Harness-ran (result line present) → killed/survived from the failing-test list; result line absent → the mutation was invalid.
  - **Scope automated mutations away from the oracle.** A whole-file sed can rewrite the test module's expected values in lockstep with the code (same literal in both) — the mutant then "survives" against a co-mutated oracle. Restrict the mutation to the code region (address range above `#[cfg(test)]`, exclude test dirs), and treat a survival whose diff touched test code as void.
- **Durable state, not conversation memory** — committed tests/issues are the durable record of WORK; PROGRESS.md is the authoritative HUMAN/audit narrative; under a Workflow, run-time resume and convergence are owned natively (the run journal / `resumeFromRunId` + cached agent results + orchestrator loop state), not by PROGRESS.md.
- **Always restore** mutations via VCS; verify a clean tree before committing tests.
- **Comments describe behavior, not the mutation process.**
- **Scale to scope** — a fix → a handful of mutations; a whole repo → a chunked, tracked, resumable campaign with a warm toolchain, and for very large repos a parallel fan-out (one author + mutation-check pass per module).
- **The dispatcher owns the synthesis — review sub-agent work, don't relay it.** When you fan out to sub-agents, their conclusions are inputs you must audit, not verdicts you forward. Read their NOTES (not just the structured result) for buried suppressions — "discarded / by design / not exploitable / benign / safe" are items to pull up, not accept. Verify any safety argument that hinges on an ordering/sign/rounding claim against the actual code. Relaying a worker's "no bugs found" without reviewing what it observed is the same rubber-stamp as a bare "Reviewed" on a PR.
- **A passing test is not a correct behavior — coverage ≠ correctness.** A test killing a mutant only proves the test and the code agree, and tests routinely mirror the implementation (assert the code's own output / recompute with the same formula), so a green test can faithfully enshrine a bug. Run the correctness check on covered behaviors too; treat a test whose expected values look derived from the code as a red flag and re-derive them independently from the spec. "An existing test covers it" is never validation, and never a reason to skip adversarial scrutiny.
- **Question the oracle — adversarial, not just mutation.** Mutation testing makes the code the oracle and can only find test gaps; it CANNOT find a bug, because it pins whatever the code does. The "adversarial" half makes the *spec/intent* the oracle and the code suspect: derive the expected value/property independently and hunt for inputs where the code violates it. A run that only adds passing tests and reports "no bugs" did half the job — say so honestly rather than calling the absence "expected". Every exact-value assertion is a chance to check the value against intent, not just against the code's output.
- **Exhaust, don't sample — loop until dry.** Probe *every* behavior of a unit, then re-survey for the ones you missed; stop only when a full pass surfaces no new gap. A single pass over the high-value subset is a coverage *sample*, not coverage. A large or expensive-to-probe unit (e.g. one whose tests run etched/regenerated bytecode) gets paced and resumed — never truncated, and never declared done on the strength of "it looked well-tested already".
- **Every filed finding is labelled `audit` — create the labels before filing, never file unlabelled.** The org health graph counts a repo's outstanding findings by `--label audit`, so an unlabelled finding is invisible and effectively does not exist (`rain.solmem` filed three and the graph read zero). `gh issue create --label` hard-fails on a label the repo lacks, so create `audit` (+ `adversarial` for provenance) up front, and re-list after filing to confirm they stuck. A label that cannot be created is a STOP-and-report, never a reason to file bare.
- **End with a committed scan record.** Every run closes by appending an entry to a committed `audit/mutation-test-scans.json` (timestamp, scanned commit, published tag, scope, summary) and landing it on the default branch — even a clean run — so org-wide health tracking can distinguish recently-scanned repos from stale ones and know which release was audited.
