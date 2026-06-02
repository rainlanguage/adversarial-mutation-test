# Adversarial Mutation Testing — Claude Code plugin

A [Claude Code](https://claude.com/claude-code) skill that builds or hardens a
repository's test suite so every test **provably** covers code. The core idea:
every test makes an assertion that differs between correct and buggy code,
validated by deliberately breaking the exact line it claims to cover and
confirming the test fails. Surviving mutants (lines a mutation can break with no
test noticing) are real coverage gaps and get a new discriminating test; existing
tests that already kill their mutant are credited and logged.

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

The skill also auto-triggers on requests like "harden the test suite",
"mutation test the codebase", "prove these tests cover the code", or "exhaust
the eventualities".

## What it does

- **Surveys** the repo, inventories testable units and existing tests, and finds
  the gaps (coverage tooling + mutation probing).
- **Groups** the work into logical modules, each shipped as its own branch + PR.
- Runs a per-unit loop: enumerate behaviors → baseline → break each behavior with
  one targeted mutation → run the whole suite → credit the existing test that
  catches it, or add/strengthen a test for a surviving mutant.
- **Never edits a test to pass under a mutation** (that would encode the bug);
  strengthens weak tests in place, fixes legitimately broken baseline tests, and
  surfaces real code bugs.
- For large repos, fans out **one fresh clone per worker** so groups can be
  hardened in parallel.

## Method (the per-unit loop)

1. Enumerate behaviors (each guard, comparison, computation, side-effect,
   early-return, error path).
2. Baseline the existing suite green.
3. Probe each behavior with one targeted mutation, made live in whatever the
   tests actually execute (regenerate any built/cached/generated/etched artifact
   first — stale artifacts are the #1 way mutation testing lies to you).
4. A test fails → behavior covered. No test fails → a real gap → add or
   strengthen a discriminating test until it fails under the mutation.
5. Restore the mutation; record the result.

See [`skills/adversarial-mutation-test/SKILL.md`](skills/adversarial-mutation-test/SKILL.md)
for the full method.

## License

[DecentraLicense 1.0](LICENSE) (`LicenseRef-DCL-1.0`).
