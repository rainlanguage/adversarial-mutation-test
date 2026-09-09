# Adversarial Mutation Testing — Claude Code plugin

A [Claude Code](https://claude.com/claude-code) skill that builds or hardens a
repository's test suite so every test **provably** covers code. The core idea:
every test makes an assertion that differs between correct and buggy code,
validated by deliberately breaking the exact line it claims to cover and
confirming the test fails. Surviving mutants (lines a mutation can break with no
test noticing) are real coverage gaps and get a new discriminating test;
existing tests that already kill their mutant are credited and logged.

Language- and harness-agnostic. Designed for long, whole-repo campaigns that
outlive the conversation context window: progress lives in a durable gitignored
scratch file so it survives compaction, and each logical group of tests ships as
its own branch + PR.

## Install

```
/plugin marketplace add rainlanguage/adversarial-mutation-test
/plugin install adversarial-mutation-test@rainlanguage-skills
```

Then reload (or restart Claude Code) and invoke it:

```
/adversarial-mutation-test:adversarial-mutation-test
```

You can pass a scope as an argument, e.g.:

```
/adversarial-mutation-test:adversarial-mutation-test harden the whole repo, with fresh clones
/adversarial-mutation-test:adversarial-mutation-test the src/lib module
/adversarial-mutation-test:adversarial-mutation-test this PR
```

The skill also auto-triggers on requests like "harden the test suite", "mutation
test the codebase", "prove these tests cover the code", or "exhaust the
eventualities".

## What it does

- **Surveys** the repo, inventories testable units and existing tests, and finds
  the gaps (coverage tooling + mutation probing).
- **Groups** the work by the behaviours each group contains — not by module —
  and ships each group as its own branch + PR.
- Runs a per-unit loop: enumerate behaviors → baseline green → break _every_
  behavior with one targeted mutation against the **pre-existing** suite, before
  writing anything → credit by name each existing test that catches one → then
  work the survivors, which are the worklist, with new or strengthened tests.
- **Never edits a test to pass under a mutation** (that would encode the bug);
  strengthens weak tests in place, fixes legitimately broken baseline tests, and
  surfaces real code bugs.
- For large repos, fans out **one fresh clone per worker** so groups can be
  hardened in parallel.

## Method (the per-unit loop)

1. Enumerate behaviors (each guard, comparison, computation, side-effect,
   early-return, error path).
2. Baseline the existing suite green.
3. Probe _every_ enumerated behavior with one targeted mutation, against the
   **pre-existing** suite and before writing any test of your own — each
   mutation made live in whatever the tests actually execute (regenerate any
   built/cached/generated/etched artifact first — stale artifacts are the #1 way
   mutation testing lies to you), restoring the source after each probe.
4. A test fails → behavior covered, credited to that named test. No test fails →
   the mutant survived → a real gap.
5. Once that pass is complete, the survivors are the worklist: add or strengthen
   a discriminating test until it fails under the mutation. Writing one before
   the pass finishes forfeits the attribution — recovering it costs a second
   clone at the base commit and a second full mutation pass.
6. Record the result.

See
[`skills/adversarial-mutation-test/SKILL.md`](skills/adversarial-mutation-test/SKILL.md)
for the full method.

## The probe harness (`mutation-probe`)

The mutate → run → score → restore machinery is a tested Rust bin shipped by
this repo's nix flake — campaigns author mutants declaratively and never
hand-roll the harness (hand-rolls kept faking matrices: zero-match mutants
scored as survived, crashed suites scored at all, imperfect restores poisoning
later probes).

```sh
nix run github:rainlanguage/adversarial-mutation-test#mutation-probe -- mutants.toml
```

`mutation-probe --help` is the complete manual. The short form: the mutants file
names the suite command as argv (artifact regeneration included — the probe runs
exactly that per verdict), a proof-of-run regex reading the suite's own
pass/fail tally, and the mutants as exact-string `(file, target, replacement)`
triples that must match exactly once.

```toml
[suite]
root = "."
command = ["nix", "develop", "-c", "cargo", "test"]
proof = '(\d+) passed; (\d+) failed'
fail-pattern = 'test (\S+) \.\.\. FAILED'   # optional: names the killer
                                             # forge: '\[FAIL.*\] (\S+)' — see below
timeout-secs = 1800                          # optional

[[mutants]]
name = "M01 guard inverted"
file = "src/lib.rs"
target = "if !ok {"
replacement = "if ok {"
```

Verdicts: `KILLED` (failing tally, or non-zero exit with proof of a run) /
`SURVIVED` (ran green: a real gap) / `NO-RUN` (no proof the suite ran — crash,
compile error, timeout — never scored as survived) / `HARNESS-ERROR` (target not
matched exactly once). A red, silent, or zero-test baseline aborts before any
probe; writes are atomic and every restore is verified byte-exact; a hung
suite's whole process group is killed at `timeout-secs`. Exit 0 only when every
probed mutant is killed; 1 on any non-kill; 2 when the pass cannot be trusted.
`--only <substring>` re-runs a subset while strengthening a killer — repeatable,
the selections union, and each value must match something; `--json <path>`
writes the machine-readable report.

For forge suites use the greedy `fail-pattern = '\[FAIL.*\] (\S+)'`, which
anchors on a line's last `]`. The natural-looking `'\[FAIL[^\]]*\] (\S+)'` stops
at the first nested `]` — forge puts brackets inside `[FAIL: ...]` reasons
(custom errors pretty-print integers as `65534 [6.553e4]`; fuzz counterexamples
embed `args=[...]`) — so the capture never reaches the test name and attribution
drops on exactly the most informative failures. The probe warns whenever a
mutant is killed but the configured fail-pattern captured no killer, so that gap
is loud rather than a silently empty `killedBy`.

## Scan record template

Campaigns close by appending one entry per run to a committed
`audit/mutation-test-scans.json` on the default branch (see SKILL.md). Valid
JSON, no comments:

```json
{
  "timestamp": "2026-08-12T19:40:00Z",
  "commit": "08d547fdeadbeefc0ffee1122334455667788990",
  "testsAfterCommit": "1f9be22cafebabe0ddf00d998877665544332211",
  "publishedTag": "v1.2.3",
  "commitsAheadOfTag": 0,
  "scope": "whole repo",
  "tool": "adversarial-mutation-test",
  "skillVersion": "0.34.0",
  "summary": {
    "behaviours": 600,
    "candidates": 89,
    "confirmed": 30,
    "testsBefore": 41,
    "testsAfter": 84,
    "filed": ["#2651", "#2660"]
  }
}
```

`timestamp` is UTC at run end; `commit` the exact SHA scanned;
`testsAfterCommit` the exact SHA the run's own output landed at; `publishedTag`
the release at `commit` (null if unreleased) with `commitsAheadOfTag` its
distance. All five are must-haves; `summary` is nice-to-have.

A record spans two trees, and every number in it is measured at one of them:
`commit` is the tree the scan ran against, which every _before_ number
(`testsBefore`, baseline counts) holds at; `testsAfterCommit` is the tree with
the run's coverage PRs merged, which every _after_ number (`testsAfter`, and
anything else measured post-landing) holds at. Both are full 40-character SHAs —
a short prefix is a weaker anchor and grows ambiguous as history grows.

`testsAfterCommit` is never null and never omitted: a run that landed nothing
sets it **equal to `commit`**. "Nothing landed" and "nobody recorded where it
landed" have to stay distinguishable, so an absent field means the record is
malformed rather than that the run was clean. This is the field that makes an
after-state count checkable at all — `rain.sol.codegen` committed
`testsAfter: 102`, a count that occurs at no commit in the range the record
covers, and nothing could catch it because the record named no tree to check it
against.

## License

[DecentraLicense 1.0](LICENSE) (`LicenseRef-DCL-1.0`).
