---
name: adversarial-mutation-test
version: 0.33.0
description: Use to systematically find BUGS in and harden the test suite for a WHOLE repository (or a whole module of it). Two co-equal goals the name carries: ADVERSARIAL (treat spec/intent as the oracle and the code as suspect — derive expected behavior independently and hunt for inputs where the code is wrong) and MUTATION (prove tests cover the code). Mutation-drives a behavior-centric coverage ledger — for each behavior, break the line and check the whole suite: existing tests that kill the mutant are validated and logged (so existing coverage is audited and in scope), and only surviving mutants (real gaps) get a new discriminating test. An existing test that already kills mutants is left as-is; one meant to cover a behavior but that a mutant survives is strengthened in place (not duplicated); one broken on the unmutated baseline is fixed or its underlying code bug surfaced; a test is never edited to swallow a mutation. Designed for long campaigns that outlive the context window: progress lives in a durable gitignored scratch file so it survives compaction. A single change/PR/function is just a narrowed scope. Triggers on "test the whole repo", "harden the test suite", "mutation test the codebase", "audit the tests", "adversarial tests", "prove these tests cover the code", "exhaust the eventualities".
---

# Adversarial Mutation Testing (whole-repo, resumable)

Two co-equal goals, and the name carries both: **adversarial** = spec/intent is
the oracle and the code is suspect — hunt for actual BUGS; **mutation** = code
is the oracle — break a line, prove a test notices. Mutation alone is the common
failure: it pins whatever the code does and structurally CANNOT find a bug.
Every unit gets BOTH passes. Default scope is the whole repo; a single change /
PR / function is just a narrowed target.

Whole-repo runs are ultracode-shaped: fan out with the native Workflow tool
(`agent()` / `parallel()` / `pipeline()`, schema-forced returns,
`resumeFromRunId`, `budget`); a narrowed scope runs inline and serial.
"Orchestrator" below means the Workflow author when fanned out, the serial
driver when not.

## Durable progress — two roles, kept separate

- **Fan-out resume + convergence is owned natively**: re-invoke with
  `resumeFromRunId`; the orchestrator's own loop state (sliced worklist,
  seen-set) is the convergence signal. Never recover run state from agents
  editing a shared file.
- **Human/audit trail + serial-mode resume** is a gitignored
  `.mutation-test/PROGRESS.md` (add to `.git/info/exclude`): scope, harness
  commands, worklist with per-unit status, per-unit matrices. Serial mode marks
  exactly one unit `IN PROGRESS` and updates after each; fan-out mode has the
  orchestrator write it once from aggregated structured returns.

The coverage ledger is **behavior-centric**: one row per behavior — the mutation
and the test that killed it, existing or new — so existing tests earn credit
only by actually killing a mutant, and "covered" lines whose mutants survive are
exposed as gaps. A killed mutant proves the behavior is PINNED, never CORRECT: a
test can mirror the implementation and enshrine its bug, which is why the
adversarial pass judges covered behaviors too.

## Repo-wide campaign

1. **Survey**: enumerate testable units and existing tests; find gaps with
   coverage tooling. Zero- and weakly-covered units are highest value. Price
   each unit in BEHAVIOURS — a unit list without a size term cannot be grouped.
2. **Prioritize and group** by risk × coverage gap; each group ships as its own
   branch + PR (additive test-only PRs are independent and CI-safe). **A group
   is sized by the behaviours it holds, never by "a module / package / coverage
   area"** — a file is not indivisible, so a unit over one agent's budget is
   split across groups. Shards of one file are ordinary groups; their PRs append
   to the same test file, and that textual conflict is far cheaper than a group
   dying. An over-sized group does not degrade, it DIES: cost is behaviours ×
   mutants × a probe cycle each, and every test the group adds lengthens the
   suite its own later probes re-run, so the overrun is not the excess but a
   handoff plus a successor's re-read. When a unit cannot be priced cheaply,
   slice smaller — finishing early costs a clone, dying costs an agent.
3. **Learn the harness once** — build, test, and any artifact-regeneration step
   — BEFORE probing. Regeneration belongs INSIDE the probe's suite command:
   tests that execute a stale artifact test nothing.
4. **Run the per-unit loop**, committing each unit's tests as you finish; push
   and open each group's PR before starting the next group.
5. **Resume**: serial via PROGRESS.md; fan-out via `resumeFromRunId`. Never
   re-do a completed unit; stop only when the worklist is exhausted.

## The per-unit loop

1. **Enumerate behaviors**: every conditional, comparison, computation,
   side-effect, filter, early-return, and error/skip path — happy path, each
   branch, boundaries, interactions, "must NOT happen" cases.
2. **Baseline green, then probe the pre-existing suite in full.** A test failing
   on unmutated code is legitimately broken: fix a wrong/outdated/flaky test, or
   surface the real code bug its failure reveals. Never mask a baseline failure
   by matching the assertion to buggy behavior. Then, with **none of your own
   tests written**, probe the whole enumerated list against that suite and
   finish the pass before writing anything: every kill credits a **named
   pre-existing** test, and the survivors are step 4's worklist. Writing early
   forfeits that attribution and cannot be recovered in place — recovery costs a
   second clone at the base commit, a second full pass, and a diff of the two
   matrices, which `rain.sol.codegen` paid across 95 mutants to recover 14
   killed / 81 survived.
3. **Probe with the bundled tool.** Author one targeted mutation per behavior
   (catalog below) in a mutants file, then:

   ```sh
   nix run github:rainlanguage/adversarial-mutation-test#mutation-probe -- mutants.toml
   ```

   `mutation-probe --help` is the manual (file format, verdicts, exit codes).
   The bin enforces probe integrity — green non-empty baseline, proof the suite
   actually ran, exactly-once targets, byte-exact restore — so a crashed suite
   or a no-op mutant can never fake a result. Yours to uphold: **commit before
   the first probe** (the auditable recovery point), and **keep targets out of
   test code** — a target in the oracle co-mutates the expectation and voids the
   probe.
4. **Act on verdicts — step 2's survivor set is the worklist.** KILLED =
   covered: credit the killing test in the ledger. SURVIVED = a real gap: if an
   existing test purports to cover the behavior, strengthen it in place until it
   kills the mutant; otherwise add a new test. Either way the test must be
   **discriminating** — a different observable value under correct vs wrong code
   (exact values over bare reverts) — passing on baseline and failing under the
   mutation; re-probe (`--only`) until killed. Never delete a test, never weaken
   one, and never edit one to pass under a mutation — that encodes the injected
   bug.
5. **Loop until dry.** Record the matrix, then re-survey the unit for unprobed
   behaviors and keep probing until a full pass adds no new gap. Size and
   per-probe cost change pacing, never scope; an unfinished unit records its
   exact unprobed list rather than being declared done.

## Adversarial correctness pass (per unit — the half that finds bugs)

0. **Ingest the intent oracle first**: spec, NatSpec, interface contracts,
   domain invariants. Do not substitute an intent the code happens to satisfy
   ("dust retained = conservative" is the classic self-own). Findings are wider
   than exploits: orphaned funds, reverts-that-should-succeed, wrong-but-not-
   stolen values.
1. **Derive expected behavior independently** of the code, from the oracle.
2. **Enumerate invariants**: conservation, monotonicity, bounds, rounding
   DIRECTION, idempotence, access control, isolation, ordering independence,
   reentrancy, "must never happen".
3. **Try to falsify each** against REAL dependencies — extreme/zero/max values,
   odd decimals, dust edges, reordered/interleaved/reentrant sequences, hostile
   counterparties — never the suite's always-succeed mocks. A falsifying case
   failing on unmutated code is a candidate bug.
4. **A surviving mutant gets the adversarial question first**: is the current
   output even correct? If yes, add the test; if no, that's a bug — do not
   enshrine it. **A killed mutant is still in scope**: a test whose expected
   value looks derived from the code (same formula, same magic constant) is a
   red flag — re-derive from spec; a mismatch means the test enshrines a bug.
5. **Surface every candidate for TRIAGE — never adjudicate yourself.** Neutral
   framing: behavior, where, why it might or might not be intended, cheap repro.
   Ambiguous → flag, never drop. Refutation reasoning attaches as a NOTE, never
   gates a finding; the only discardable item is a provably-broken repro you
   wrote yourself, and its behavior question stays on the list.

Re-verify each candidate's repro against current code before filing; the repro
belongs on the finding, never merged green into the suite.

## Findings → issues

- Every finding is filed as a GitHub issue carrying the **`audit` label plus
  `adversarial`** — `audit` is what the org health scan counts; an unlabelled
  finding is invisible and effectively does not exist.
- **Create missing labels before the first `gh issue create`** (it hard-fails on
  absent labels), and never recover from a label error by filing unlabelled —
  STOP and report instead.
- **Verify after filing**: re-list by `--label audit` and confirm every filed
  issue appears; add labels to any that slipped through.
- Title = the violated behavior; body = unit, intent oracle, violated property,
  verified repro, neutral triage framing. Record filed numbers in the scan
  record and PROGRESS.md.

## Mutation catalog (break ONE behavior)

- **Conditionals:** negate, force true/false, swap branches.
- **Comparisons:** `<`↔`<=`, `>`↔`>=`, `==`↔`!=`, swap operands.
- **Arithmetic / off-by-one:** `+1`→`-1`/`+0`, `*`↔`/`, drop a term.
- **Returns / outputs:** early return, empty/zero/default, a constant.
- **Side-effects:** delete a write/emit/update, or move it across a guard.
- **Constants / identifiers:** change a literal, swap an error/event/variable.
- **Filters / scopes:** remove a predicate (ownership / namespace / key).

One mutation, one behavior — the failing-test set stays diagnostic.

## Parallelizing across groups (the native fan-out)

- **One fresh CLONE per worker — never a worktree, never a shared checkout.**
  Workers mutate source and build state concurrently; shared untracked build
  output makes one worker's stale artifact another worker's test subject (the
  matrix silently lies), and a worktree's shared `.git` contends on every
  commit/restore. A clone isolates both; each worker provisions, probes,
  commits, pushes, and opens its own PR.
- **Orchestrator slices; agents never self-select.** Survey returns a
  schema-validated list carrying each item's behaviour count — an identity-only
  list is size-blind, and slicing it cannot honour the sizing rule above.
  Partition on the BEHAVIOUR axis into explicit disjoint batches in orchestrator
  code (a 95-behaviour unit is a dozen batches, not one), treating an estimated
  count as a floor; converge loop-until-dry against a seen-set. Agents editing a
  shared TODO list is the known failure mode (duplicate work, no convergence).
- A `null` agent return is a re-dispatch signal, promptly and in parallel — a
  group is either productive or verifiably empty before the run is complete.
  Probe agents run at low effort; kill/adversarial/synthesis at high.
- **Clean up clones at run end** — after checking each for unpushed work worth
  preserving. Never delete a checkout you didn't create.

## Dispatcher duties (the orchestrator's own review)

- A worker's "no bugs / all green / discarded / by design / not exploitable" is
  an INPUT to audit, never a verdict to relay. Force NOTES into the structured
  return schema and iterate the suppressions deterministically.
- **Union, never vote**: every surfaced candidate enters triage; a skeptic's
  refutation is a note. Cross-check "clean" claims against the oracle yourself,
  and verify any safety argument hinging on an ordering/sign/rounding claim
  against the actual code — workers analyze these backwards.

## Committed scan record

Close every run — including a clean one — by appending an entry to a committed
`audit/mutation-test-scans.json` and landing it on the default branch:
timestamp, scanned commit, published tag (+ commits ahead), scope, tool + skill
version, summary with filed issue numbers. The org health check reads the newest
entry for "which release was last audited"; the JSON template lives in this
repo's README.

## Principles

- Strengthen weak tests in place; add only where nothing targets the behavior;
  never gratuitously rewrite tests that do their job.
- Discriminating assertions: "got 3, expected 1" beats "it reverted".
- Confirm the mutation is live — regeneration inside the suite command; stale
  artifacts are the #1 way a matrix lies.
- Probe the pre-existing suite before writing a test — attribution exists only
  while the suite is untouched.
- Coverage ≠ correctness: green, mutant-killing tests can enshrine a bug;
  re-derive expected values from spec.
- Exhaust, don't sample — loop until a full pass adds nothing; pace, never
  truncate.
- Durable state over conversation memory: committed tests and issues are the
  record of work; PROGRESS.md is the narrative; the Workflow journal owns
  fan-out resume.
- Comments describe behavior, not the mutation process.
