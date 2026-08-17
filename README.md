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
`--only <substring>` re-runs a subset while strengthening a killer;
`--json <path>` writes the machine-readable report.

## Scan ledger

Campaigns close by appending one record per run to a committed
`audit/mutation-test-scans.json` on the default branch (see SKILL.md). The file
is a wrapper object, not a bare array: `schemaVersion` says how to read it,
`records` holds the runs. Valid JSON, no comments:

```json
{
  "schemaVersion": 1,
  "records": [
    {
      "schemaVersion": 1,
      "timestamp": "2026-08-12T19:40:00Z",
      "commit": "08d547fdeadbeefc0ffee1122334455667788990",
      "testsAfterCommit": "1f9be22cafebabe0ddf00d998877665544332211",
      "publishedTag": "v1.2.3",
      "commitsAheadOfTag": 0,
      "scope": "whole repo",
      "tool": "adversarial-mutation-test",
      "skillVersion": "0.35.0",
      "summary": {
        "behaviours": 600,
        "candidates": 89,
        "confirmed": 30,
        "testsBefore": 41,
        "testsAfter": 84,
        "filed": ["#2651", "#2660"]
      }
    }
  ]
}
```

`timestamp` is UTC at run end; `commit` the exact SHA scanned;
`testsAfterCommit` the exact SHA the run's own output landed at; `publishedTag`
the release at `commit` (null if unreleased) with `commitsAheadOfTag` its
distance. All five are must-haves; `summary` is nice-to-have.

### Two trees, and which numbers hold at each

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

### Two versions, and neither is `skillVersion`

The top-level `schemaVersion` versions the envelope — the wrapper shape and the
read rules below. The per-record `schemaVersion` versions that one record's
field set. They are separate fields because the file provably holds records of
more than one shape (see Migration), so a single number could not honestly
describe both the file and everything in it. Neither of these is skillVersion,
which names the skill that wrote a record and is not a statement about its shape
— it has already failed to be one, the `0.30.0` summary and the current template
sharing only `filed`.

### Ordering, and what "newest" means

`records` is append-only. A record is a historical fact about a tree, so it is
never rewritten, reordered, or removed.

The newest record is the one **whose timestamp is greatest** — not the last
array element. Append order is the weakest of the candidates precisely because
it is what a PR queue scrambles: two campaigns can land in the opposite order to
the order they ran. Ties in `timestamp` break by array position, later wins.
Nothing else is the rule — not `commit`'s position in history, not file order.

**Newest is not current.** Every value in a record is frozen at run end and
describes the tree it names, so `commitsAheadOfTag` keeps reading `0` however
far the repo moves on afterwards — a stale record does not decay into looking
stale. The ledger answers "what was audited, and when", never "is that still
true": a reader asking whether a repo is audited at its current release must
compare the newest record's `commit` against the repo today. `rain.sol.codegen`
is the live case — its one record says `commitsAheadOfTag: 0` while sitting 35
commits and 5 published tags behind the default branch.

### Migration from a bare array

The ledger was a bare array before `schemaVersion` 1, and committed ones still
are. A campaign that opens a bare array **wraps it in place on that run's
append**: the array becomes `records`, the top-level `schemaVersion` is added,
and every record already in it is preserved exactly — no field back-filled, no
value corrected, nothing reordered. Back-filling `schemaVersion` or
`testsAfterCommit` into an existing record would assert a measurement no run
made.

So a record carrying no `schemaVersion` predates the wrapper: its must-haves may
be absent and its `summary` is not the current shape. Read it as a record of its
own `skillVersion`, and never assume the newest record's shape holds across the
file.

## License

[DecentraLicense 1.0](LICENSE) (`LicenseRef-DCL-1.0`).
