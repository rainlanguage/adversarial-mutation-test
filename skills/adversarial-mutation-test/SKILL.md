---
name: adversarial-mutation-test
description: Use to systematically build or harden the test suite for a WHOLE repository (or a whole module of it). Mutation-drives a behavior-centric coverage ledger — for each behavior, break the line and check the whole suite: existing tests that kill the mutant are validated and logged (so existing coverage is audited and in scope), and only surviving mutants (real gaps) get a new discriminating test. An existing test that already kills mutants is left as-is; one meant to cover a behavior but that a mutant survives is strengthened in place (not duplicated); one broken on the unmutated baseline is fixed or its underlying code bug surfaced; a test is never edited to swallow a mutation. Designed for long campaigns that outlive the context window: progress lives in a durable gitignored scratch file so it survives compaction. A single change/PR/function is just a narrowed scope. Triggers on "test the whole repo", "harden the test suite", "mutation test the codebase", "audit the tests", "adversarial tests", "prove these tests cover the code", "exhaust the eventualities".
version: 0.10.0
---

# Adversarial Mutation Testing (whole-repo, resumable)

Build or harden a repository's tests so every test PROVABLY covers code. Default scope is the **whole repo** (or a whole module of it); a single change / PR / function is just a narrowed target. Core idea: every test makes an assertion that differs between correct and buggy code, validated by deliberately breaking the exact line it claims to cover and confirming the test fails. Any language / harness.

A whole-repo run is long and **will outlive the conversation's context window**. Treat conversation memory as unreliable: the durable scratch file (below) is the single source of truth, and committed git history is the durable record of the work itself.

## 0. Durable progress — set up FIRST (survives compaction)

Create a **gitignored scratch file** as the source of truth for the campaign:

- Path: `.mutation-test/PROGRESS.md` at repo root.
- Make git ignore it **without a tracked change**: append `.mutation-test/` to `.git/info/exclude` (preferred — no repo diff). Use `.gitignore` only if the team wants it shared.
- It holds: **scope**; the **harness commands** (build / test / regenerate-artifacts / coverage); the full **worklist** with per-unit status; and the per-unit **mutation matrices**.

Protocol (non-negotiable for a long run):
- **On every entry/iteration, READ `.mutation-test/PROGRESS.md` first** to recover state. If it's missing, you're starting fresh (do the survey). If it exists, resume from it — do not re-derive progress from the conversation.
- **Mark exactly one unit `IN PROGRESS`** with a one-line note of the precise sub-step, so a mid-unit interruption resumes exactly.
- **After every unit, update the file** (mark `DONE`, record its matrix + the commit hash) before moving on.
- Keep the file small and skimmable — it's read every iteration.

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

## Repo-wide campaign

1. **Survey & inventory.** Enumerate testable units (modules / contracts / files / public functions) and existing tests. Find the gaps with coverage tooling — `forge coverage`, `cargo llvm-cov`, `pytest --cov`, `go test -cover`, `nyc`, etc. Zero-coverage and weakly-covered units are highest value. Write the worklist into PROGRESS.md.
2. **Prioritize, chunk, and group.** Order by risk × coverage gap (security-critical / complex / untested first). Process **one unit at a time**, but organize the worklist into **logical groups** — a module / package / coverage area (a handful of related units) is one group, and **each group ships as its own branch + PR**. This keeps PRs small and reviewable and ships coverage incrementally instead of one giant branch.
3. **Learn the harness once.** Discover build/test commands and any **artifact-regeneration** step (compiled output, generated bindings, etched/deployed bytecode, golden files, snapshots) BEFORE mutating. Record them in PROGRESS.md. Re-running tests after editing source but without regenerating the artifact they execute tests **nothing**.
4. **Run the per-unit loop**, **committing each unit's tests** as you finish it (durable record; the scratch file only tracks meta-progress). Work each group on its own branch off the default. When a group's units are done and the suite is green, **push and open a PR for that group**, then start the next group on a fresh branch. Added tests are additive (no source/bytecode change), so per-group test PRs are independent and CI-safe — they review and merge on their own; don't accumulate everything on one branch. Record each group's branch + PR in PROGRESS.md.
5. **Resume.** On re-entry after compaction / a new turn / a restart: read PROGRESS.md, pick up the current group's `[WIP]` (or next `[TODO]`) unit, continue. If a group is finished but unshipped, push + open its PR first. Stop only when the worklist (or agreed scope) is exhausted; then report repo-wide coverage proven, the PRs opened, and gaps remaining.

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

## Mutation catalog (break ONE behavior)

- **Conditionals:** negate (`x`→`!x`), force `true`/`false`, swap branches.
- **Comparisons:** `<`↔`<=`, `>`↔`>=`, `==`↔`!=`, swap operands.
- **Arithmetic / off-by-one:** `+1`→`-1`/`+0`, `*`↔`/`, drop a term.
- **Returns / outputs:** return early, empty/zero/default, a constant.
- **Side-effects:** delete a write/emit/update; or move it across a guard so it runs in the wrong cases.
- **Constants / identifiers:** change a literal, swap an error/event, use the wrong variable.
- **Filters / scopes:** remove a predicate (ownership / namespace / key) — validates isolation tests.

Pick the mutation that maps to exactly one behavior so the failing-test set is diagnostic.

## Parallelizing across groups (optional, for large repos)

Groups are independent (separate branch, additive test-only PR), so fan out **one sub-agent per group, each in its own fresh clone**. A fresh clone per worker is required, not a nicety: mutation testing mutates shared source, so workers cannot share a checkout. Each worker clones the repo, provisions it (install deps + build), runs its group's per-unit loop, then commits, pushes, and opens its own PR.

- Parallelism is **across** groups; **within** a group the `mutate → regenerate → test → restore` cycle stays serial. Concurrency is bounded (~cores).
- An orchestrator (e.g. a Workflow) assigns groups, spawns a clone-per-group agent, and aggregates the repo-wide ledger. Opt in when the repo is large enough to justify the per-worker clone + build cost.

## Principles

- **Never edit a test to pass under a MUTATION.** A test failing under a mutation is SUCCESS — it caught the injected bug; reverting the mutation restores green. Changing a test's assertion to swallow a mutation encodes the bug. The only test you adjust *mid-mutation* is a *new* one you just wrote that failed to discriminate (didn't fail under its own target mutation) — strengthen it.
- **Strengthen weak tests; don't duplicate them.** If a mutant survives and an existing test *purports* to cover that behavior, it's inadequate — tighten it in place until it kills the mutant, rather than leaving it and adding a redundant parallel test. Add a *new* test only when no existing test targets the behavior. Don't gratuitously rewrite tests that already do their job.
- **Fix legitimately broken tests; surface real bugs.** A test failing on the *unmutated* baseline is broken: fix it if its assertion is wrong/outdated/flaky, or if the failure exposes a real code bug, report the bug — never mask a baseline failure by editing the assertion to match buggy code. Distinguish **"the test is wrong"** (fix it) from **"the code is wrong"** (report it).
- **Discriminating assertions** — "got 3, expected 1" beats "it reverted".
- **One mutation, one behavior** — isolation makes the failing set identify the covered line.
- **Confirm the mutation is live** — stale artifacts are the #1 way this lies to you.
- **Durable state, not conversation memory** — PROGRESS.md is authoritative; commit tests per unit.
- **Always restore** mutations via VCS; verify a clean tree before committing tests.
- **Comments describe behavior, not the mutation process.**
- **Scale to scope** — a fix → a handful of mutations; a whole repo → a chunked, tracked, resumable campaign with a warm toolchain, and for very large repos a parallel fan-out (one author + mutation-check pass per module).
