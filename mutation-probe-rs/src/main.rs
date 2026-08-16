// mutation-probe — the skill's probe harness as a tested tool.
//
// The adversarial-mutation-test skill needs, per PR/campaign, a harness that applies
// exact-string mutants and scores the suite's reaction. Hand-rolling that harness each
// time re-risks the same integrity bugs every time: a zero-match "mutation" that mutates
// nothing scoring as "survived", a suite that never ran (crash, E2BIG, wrong dir) scoring
// as anything at all, a red baseline silently probing garbage, a restore that leaves the
// tree mutated. Each of those has faked a matrix in a real incident. This bin owns the
// machinery once, tested; the adversarial half — deriving WHICH mutants would prove the
// suite discriminates — stays with the agent, in the mutants file.
//
// Verdicts:
//   KILLED        — the suite ran (proof line matched) and failed.
//   SURVIVED      — the suite ran and passed: a real coverage gap.
//   NO-RUN        — the suite produced no proof of running (crash / compile error /
//                   timeout). Unscorable, never "survived".
//   HARNESS-ERROR — the mutant itself is invalid (target not found exactly once, or the
//                   file changed under us). The harness is wrong, not the suite.
//
// Exit codes: 0 = baseline green and every probed mutant KILLED; 1 = the pass ran and
// something was not killed (survivor / no-run / harness-error); 2 = the pass could not
// run or could not be trusted (config unreadable, baseline not green, restore failed).

use std::collections::BTreeMap;
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- config ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    suite: SuiteConfig,
    #[serde(default)]
    mutants: Vec<MutantConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteConfig {
    /// Target repo root the suite runs in, resolved relative to the mutants file.
    root: String,
    /// The suite as argv — no shell. Artifact regeneration (build.sh, codegen) must be
    /// part of this command: the probe runs exactly one command per verdict, and a suite
    /// that tests stale artifacts is the #1 way a mutation matrix lies.
    command: Vec<String>,
    /// Optional: name a shipped harness (`forge`, `cargo`) and its known-good `proof`
    /// and `fail-pattern` are used. Both scrape a harness's OUTPUT FORMAT, which is a
    /// property of the harness and not of any repo — so authoring them per campaign
    /// re-derives the same two mistakes every time (see HARNESSES).
    #[serde(default)]
    harness: Option<String>,
    /// Proof-of-run regex over the suite's combined stdout+stderr. Needs two capture
    /// groups: passed count, failed count. Multiple matches sum (cargo prints one result
    /// line per test binary). No match anywhere = the suite did not provably run.
    /// Required unless `harness` supplies it; given here it overrides the harness.
    #[serde(default)]
    proof: Option<String>,
    /// Optional: one capture group extracting a failing test's name, for `killedBy`.
    /// Given here it overrides the harness's.
    #[serde(rename = "fail-pattern", default)]
    fail_pattern: Option<String>,
    /// Per-run wall clock limit. A hung suite is NO-RUN, not a hung campaign.
    #[serde(rename = "timeout-secs", default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    1800
}

// -------------------------------------------------------------- harnesses ----

/// A harness's own output format, scraped once here instead of per campaign.
struct Harness {
    name: &'static str,
    proof: &'static str,
    fail_pattern: &'static str,
}

/// Known-good patterns, each pinned by a test to REAL captured output of that harness
/// (`fixtures/`), green and red.
///
/// `fail-pattern` is the field campaigns get wrong, in two ways that look nothing alike:
///
///  1. TOO WIDE. `'\] (test\w+)\('` against forge matches `[PASS] testFoo(` exactly as
///     readily as `[FAIL: …] testFoo(`, so every mutant is "killed by" whichever passing
///     tests happen to be printed — a killer column that is not evidence of anything.
///     `fail_pattern_defect` now catches this class at baseline.
///  2. TOO NARROW, AND SILENT. `'\[FAIL.*?\] (test\w+)\('` fixes (1) and then names
///     nobody, because `.` does not match `\n` and forge puts multi-line assertion
///     messages inside the brackets. The verdict stays correct (it is read from the
///     tally, never from this pattern) while the killer column empties out.
///
/// `(?s)` is NOT the fix for (2): it lets a FAIL entry with no `] name(` shape of its
/// own — forge's invariant failures print the name on a later line, after a `[Sequence]`
/// block — run on into the NEXT entry and name that test instead. Trading a blank cell
/// for a wrong one is the worse half of the trade, so these patterns stay line-anchored.
const HARNESSES: &[Harness] = &[
    Harness {
        name: "forge",
        // Per test CONTRACT, summed; the trailing per-run line ("3 tests passed, 6
        // failed") is comma-shaped and deliberately not matched twice.
        proof: r"(?m)^Suite result: \w+\. (\d+) passed; (\d+) failed;",
        // A failing entry's name is preceded by `] ` (single-line message), by `] ` at
        // the start of a continuation line (multi-line message), by the tail of a
        // continuation line, or by one space (invariant). A `[PASS]`/`[SKIP]` line
        // reaches none of those: the alternation admits a leading `[` only for `[FAIL`.
        fail_pattern: r"(?m)^(?:(?:(?:\[FAIL|[^\[\n])[^\n]*?)?\] | )(\w+)\([^\n]*\) \((?:gas|runs):",
    },
    Harness {
        name: "cargo",
        // One line per test binary and per doctest run; they sum.
        proof: r"(?m)^test result: \w+\. (\d+) passed; (\d+) failed;",
        // `(.+)` not `(\S+)`: a doctest's name has spaces in it
        // ("src/lib.rs - two (line 2)").
        fail_pattern: r"(?m)^test (.+) \.\.\. FAILED$",
    },
];

fn harness_named(name: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.name == name)
}

fn harness_names() -> String {
    HARNESSES
        .iter()
        .map(|h| h.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// PURE: the proof and fail patterns a suite config resolves to.
///
/// An explicit pattern wins over the harness's: a repo whose suite wraps the harness
/// (a build.sh that reformats output) must still be able to say so, and silently
/// ignoring what the config asked for would be its own lie.
fn resolve_patterns(cfg: &SuiteConfig) -> Result<(String, Option<String>), String> {
    let harness = match cfg.harness.as_deref() {
        None => None,
        Some(name) => Some(harness_named(name).ok_or_else(|| {
            format!(
                "suite.harness {name:?} is not one this build ships (have: {}) — \
                 drop it and write suite.proof yourself, or add the harness",
                harness_names()
            )
        })?),
    };
    let proof = cfg
        .proof
        .clone()
        .or_else(|| harness.map(|h| h.proof.to_string()))
        .ok_or("suite.proof is required unless suite.harness supplies it")?;
    let fail_pattern = cfg
        .fail_pattern
        .clone()
        .or_else(|| harness.map(|h| h.fail_pattern.to_string()));
    Ok((proof, fail_pattern))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutantConfig {
    name: String,
    /// File the mutant applies to, relative to `suite.root`.
    file: String,
    /// Must occur EXACTLY once in the file, or the mutant is a HARNESS-ERROR.
    target: String,
    replacement: String,
}

// --------------------------------------------------------------- verdicts ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "SCREAMING-KEBAB-CASE")]
enum Verdict {
    Killed {
        #[serde(skip_serializing_if = "Vec::is_empty")]
        killed_by: Vec<String>,
    },
    Survived,
    NoRun {
        detail: String,
    },
    HarnessError {
        detail: String,
    },
}

impl Verdict {
    fn label(&self) -> &'static str {
        match self {
            Verdict::Killed { .. } => "KILLED",
            Verdict::Survived => "SURVIVED",
            Verdict::NoRun { .. } => "NO-RUN",
            Verdict::HarnessError { .. } => "HARNESS-ERROR",
        }
    }
}

/// What one suite invocation reported, before it means anything for a mutant.
#[derive(Debug, PartialEq, Eq)]
enum SuiteOutcome {
    /// Proof line(s) matched: the suite ran and this is its own tally.
    Ran {
        passed: u64,
        failed: u64,
        exit_ok: bool,
        output: String,
    },
    /// No proof anywhere in the output: crash, compile error, wrong dir.
    NoProof { output: String },
    /// Wall-clock limit hit; the child was killed.
    TimedOut { secs: u64 },
}

/// PURE: score one suite run against the proof regex.
///
/// Summing across matches is what makes one `proof` work for multi-binary harnesses
/// (cargo prints a result line per test binary and per doctest run); proof-of-run is
/// "at least one match", so a partial crash after one binary still counts as ran —
/// its failures are in the tally.
fn classify_suite(output: &str, exit_ok: bool, proof: &regex::Regex) -> SuiteOutcome {
    let mut passed: u64 = 0;
    let mut failed: u64 = 0;
    let mut matched = false;
    for cap in proof.captures_iter(output) {
        let p = cap.get(1).and_then(|m| m.as_str().parse::<u64>().ok());
        let f = cap.get(2).and_then(|m| m.as_str().parse::<u64>().ok());
        if let (Some(p), Some(f)) = (p, f) {
            matched = true;
            passed += p;
            failed += f;
        }
    }
    if !matched {
        return SuiteOutcome::NoProof {
            output: output.to_string(),
        };
    }
    SuiteOutcome::Ran {
        passed,
        failed,
        exit_ok,
        output: output.to_string(),
    }
}

/// How many killers `killedBy` carries. The matrix wants "which test caught this",
/// not a transcript of the whole failing suite.
const KILLED_BY_CAP: usize = 5;

/// PURE: the distinct names a fail-pattern captures from suite output, first seen first.
///
/// DISTINCT because harnesses repeat themselves: forge prints every failing test twice,
/// once inline and again under `Failing tests:`, so an undeduped list spends its cap on
/// two names printed twice and drops the rest.
fn captured_names(output: &str, fail_pattern: &regex::Regex) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for cap in fail_pattern.captures_iter(output) {
        if let Some(m) = cap.get(1) {
            if !names.iter().any(|n| n == m.as_str()) {
                names.push(m.as_str().to_string());
            }
        }
    }
    names
}

/// PURE: a mutant's verdict from its suite outcome.
///
/// KILLED on failed > 0 OR a non-zero exit with proof present: a harness that proves it
/// ran and then exits non-zero is declaring failure even when its tally line predates
/// the failure (deno prints the tally, then exits 1). SURVIVED requires the suite to
/// have both passed its own tally and exited zero.
fn mutant_verdict(outcome: SuiteOutcome, fail_pattern: Option<&regex::Regex>) -> Verdict {
    match outcome {
        SuiteOutcome::TimedOut { secs } => Verdict::NoRun {
            detail: format!("suite timed out after {secs}s"),
        },
        SuiteOutcome::NoProof { output } => Verdict::NoRun {
            detail: format!(
                "no proof-of-run in suite output; tail: {}",
                tail(&output, 400)
            ),
        },
        SuiteOutcome::Ran {
            failed,
            exit_ok,
            output,
            ..
        } => {
            if failed > 0 || !exit_ok {
                let killed_by = fail_pattern
                    .map(|re| {
                        let mut names = captured_names(&output, re);
                        names.truncate(KILLED_BY_CAP);
                        names
                    })
                    .unwrap_or_default();
                Verdict::Killed { killed_by }
            } else {
                Verdict::Survived
            }
        }
    }
}

/// PURE: why a baseline run blocks the pass, or None if it is sound.
///
/// A red baseline probes garbage; a zero-test baseline is the "0 tests ran" incident —
/// every later probe would run testless and report universal survival. Both abort.
fn baseline_defect(outcome: &SuiteOutcome) -> Option<String> {
    match outcome {
        SuiteOutcome::TimedOut { secs } => Some(format!("baseline suite timed out after {secs}s")),
        SuiteOutcome::NoProof { output } => Some(format!(
            "baseline produced no proof-of-run; tail: {}",
            tail(output, 400)
        )),
        SuiteOutcome::Ran {
            passed,
            failed,
            exit_ok,
            ..
        } => {
            if *failed > 0 {
                Some(format!(
                    "baseline is RED ({failed} failed) — fix the suite before probing"
                ))
            } else if !*exit_ok {
                Some(
                    "baseline is RED (green tally but non-zero exit) — fix the suite before probing"
                        .to_string(),
                )
            } else if *passed == 0 {
                Some("baseline ran 0 tests — nothing can kill anything".to_string())
            } else {
                None
            }
        }
    }
}

/// PURE: why a fail-pattern cannot be trusted, or None if it is sound.
///
/// The baseline already runs, and by the time this is asked `baseline_defect` has
/// established that ZERO tests failed there. So anything the fail-pattern captures out
/// of the baseline's own output is the name of a PASSING test — the pattern matches
/// result lines it must not, and every `killedBy` it goes on to produce credits tests
/// that cannot possibly have killed anything.
///
/// This is not a near-miss worth a warning. `[PASS] testFoo(` and `[FAIL: …] testFoo(`
/// differ by a few characters, and a matrix built on the wide pattern reads exactly like
/// a correct one — a pure-constant assertion appeared as the killer of a guard mutant in
/// the incident this check exists for. Abort, the way a red baseline aborts.
fn fail_pattern_defect(baseline_output: &str, fail_pattern: &regex::Regex) -> Option<String> {
    let names = captured_names(baseline_output, fail_pattern);
    if names.is_empty() {
        return None;
    }
    let shown = names.len().min(KILLED_BY_CAP);
    Some(format!(
        "suite.fail-pattern matches the GREEN baseline's own output: it captured {} \
         name(s) where nothing failed, so it matches PASSING result lines and every \
         killedBy it produced would be wrong. Captured: {}{}",
        names.len(),
        names[..shown].join(", "),
        if names.len() > shown { ", ..." } else { "" }
    ))
}

/// PURE: exit code from the pass's verdicts (baseline defects exit earlier, as 2).
fn exit_code(verdicts: &[Verdict]) -> i32 {
    if verdicts.iter().all(|v| matches!(v, Verdict::Killed { .. })) {
        0
    } else {
        1
    }
}

fn tail(s: &str, n: usize) -> String {
    let cleaned = s.replace('\n', " ");
    let chars: Vec<char> = cleaned.chars().collect();
    if chars.len() <= n {
        cleaned
    } else {
        chars[chars.len() - n..].iter().collect()
    }
}

// ---------------------------------------------------------------- running ----

/// Per-stream capture cap. The TAIL is kept: tallies and failure lists live at the
/// end of suite output, and a mutant that makes the suite log in a loop must cost
/// memory O(cap), not O(output).
const CAPTURE_CAP: usize = 4 * 1024 * 1024;

/// Write via temp-file + rename in the target's own directory: rename is atomic on a
/// same-filesystem move, so the target is always either its old or its new content —
/// never truncated by a failed write.
fn write_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("mutation-probe.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("renaming over {}: {e}", path.display()))
}

/// Drain a pipe keeping at most the last `cap` bytes.
fn drain_capped(mut r: impl Read, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > cap.saturating_mul(2) {
                    buf.drain(..buf.len() - cap);
                }
            }
            Err(_) => break,
        }
    }
    if buf.len() > cap {
        buf.drain(..buf.len() - cap);
    }
    buf
}

/// Run the suite once: piped output drained on threads (a full pipe would deadlock a
/// chatty suite), wall clock enforced by poll + kill.
///
/// The suite runs in its OWN PROCESS GROUP, and timeout kills the group: killing only
/// the direct child (`sh`, `nix develop`) leaves descendants holding the output pipes,
/// and the reader joins below would block forever on a suite that hung past its wrapper.
fn run_suite(
    cfg: &SuiteConfig,
    root: &std::path::Path,
    proof: &regex::Regex,
) -> Result<SuiteOutcome, String> {
    let (program, args) = cfg.command.split_first().ok_or("suite.command is empty")?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot spawn suite {program:?}: {e}"))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let out_thread = std::thread::spawn(move || drain_capped(stdout, CAPTURE_CAP));
    let err_thread = std::thread::spawn(move || drain_capped(stderr, CAPTURE_CAP));

    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    #[cfg(unix)]
                    // SAFETY: plain syscall on the pgid this process created above.
                    unsafe {
                        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("waiting on suite: {e}")),
        }
    };
    let out = out_thread.join().unwrap_or_default();
    let err = err_thread.join().unwrap_or_default();

    let Some(status) = status else {
        return Ok(SuiteOutcome::TimedOut {
            secs: cfg.timeout_secs,
        });
    };
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    Ok(classify_suite(&combined, status.success(), proof))
}

// ----------------------------------------------------------------- report ----

#[derive(Serialize)]
struct Report {
    baseline: BaselineReport,
    mutants: Vec<MutantReport>,
    summary: Summary,
}

#[derive(Serialize)]
struct BaselineReport {
    passed: u64,
    failed: u64,
}

#[derive(Serialize)]
struct MutantReport {
    name: String,
    file: String,
    #[serde(flatten)]
    verdict: Verdict,
}

#[derive(Serialize, Default)]
struct Summary {
    killed: usize,
    survived: usize,
    no_run: usize,
    harness_error: usize,
}

// ------------------------------------------------------------------- main ----

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

/// The manual lives here (and in the repo README), NOT in the skill text: skill prose
/// is a recurring per-invocation context cost, while --help is read on demand.
const HELP: &str = r#"mutation-probe — apply exact-string mutants, prove the suite ran, score honestly.

USAGE
    mutation-probe <mutants.toml> [--json <path>] [--only <substring>]

    --json <path>       also write the machine-readable report
    --only <substring>  probe only mutants whose name contains the substring
                        (substring match — a broad value selects several)
    --help, -h          this manual

MUTANTS FILE (TOML)
    [suite]
    root = "."                    # repo the suite runs in, relative to this file
    command = ["sh", "check.sh"]  # argv, no shell. Include any artifact regeneration
                                  # here (wrapper script is fine): the probe runs
                                  # exactly this per verdict, and a suite that tests
                                  # stale artifacts is the #1 way a matrix lies.
    harness = "forge"             # prefer this to hand-written patterns: it supplies
                                  # a known-good proof + fail-pattern for that
                                  # harness's output format (see HARNESSES below).
    timeout-secs = 1800           # optional; the suite's process group is killed

    [[mutants]]
    name = "M01 guard inverted"
    file = "src/lib.rs"           # relative to root
    target = "if !ok {"           # must occur EXACTLY once in the file
    replacement = "if ok {"

HARNESSES
    harness = "forge" | "cargo"   Supplies proof and fail-pattern. Each shipped
                                  pattern is pinned by a test to real captured
                                  output of that harness, green and red.

    Without a harness, write the two patterns yourself:

    proof = '(\d+) passed; (\d+) failed'
                                  # 2 capture groups: passed, failed — read from the
                                  # suite's own tally. Multiple matches SUM (cargo
                                  # prints one line per test binary). No match =
                                  # the suite did not provably run. REQUIRED unless
                                  # harness supplies it.
    fail-pattern = '(?m)^test (.+) \.\.\. FAILED$'   # optional: 1 group, the killer

    Either given explicitly overrides the harness's. Both are worth avoiding: a
    fail-pattern is easy to get wrong in two opposite directions, and neither
    shows up as a wrong VERDICT — only as a wrong or empty killer column.
      too wide   also matches PASSING result lines, so mutants are credited to
                 tests that cannot kill them. The probe aborts on this: at the
                 green baseline the pattern must capture NOTHING.
      too narrow matches nothing (e.g. `.` does not cross the newlines in a
                 multi-line assertion message), and says nothing. The probe
                 prints "killer NOT NAMED" per kill instead of shipping a blank.

VERDICTS
    KILLED         suite ran and failed (failing tally, or non-zero exit with proof
                   present — the tally is trusted over a lying wrapper exit code,
                   and vice versa)
    SURVIVED       suite ran green: a real coverage gap
    NO-RUN         no proof the suite ran (crash / compile error / timeout) —
                   unscorable, never "survived"
    HARNESS-ERROR  the mutant is invalid: target not found exactly once

INTEGRITY (enforced)
    A red, silent, or zero-test baseline aborts before any probe, and so does a
    fail-pattern that matches that baseline's own output. Writes are
    atomic (temp + rename): no failure mode leaves a file truncated. Every
    restore is verified byte-exact, and each file is re-checked pristine before
    the next mutant. Suite output is capped per stream (oldest bytes dropped).

EXIT CODES
    0  baseline green and every probed mutant KILLED
    1  the pass ran; something SURVIVED, was NO-RUN, or was a HARNESS-ERROR
    2  the pass could not run or be trusted (config error, red baseline,
       fail-pattern that matches the baseline, restore failure)
"#;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<String> = None;
    let mut json_path: Option<String> = None;
    let mut only: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--json" => {
                json_path = Some(args.next().unwrap_or_else(|| fail("--json needs a path")))
            }
            "--only" => {
                only = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--only needs a substring")),
                )
            }
            // A misspelled flag must not silently become the config path.
            other if other.starts_with('-') => fail(&format!("unknown flag {other:?} (--help)")),
            _ if config_path.is_none() => config_path = Some(a),
            other => fail(&format!("unexpected argument {other:?}")),
        }
    }
    let config_path = config_path.unwrap_or_else(|| {
        fail("usage: mutation-probe <mutants.toml> [--json <path>] [--only <substring>] (--help for the manual)")
    });

    let raw = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|e| fail(&format!("cannot read {config_path}: {e}")));
    let cfg: Config = toml::from_str(&raw).unwrap_or_else(|e| fail(&format!("{config_path}: {e}")));

    // Validate regexes at load, loudly: a proof with fewer than two capture groups can
    // never prove a run, which would score every mutant NO-RUN and look like a broken
    // suite instead of a broken config.
    let (proof_src, fail_pattern_src) = resolve_patterns(&cfg.suite).unwrap_or_else(|e| fail(&e));
    let proof = regex::Regex::new(&proof_src)
        .unwrap_or_else(|e| fail(&format!("suite.proof is not a valid regex: {e}")));
    if proof.captures_len() < 3 {
        fail("suite.proof needs two capture groups: (passed) and (failed)");
    }
    let fail_pattern = fail_pattern_src.as_deref().map(|p| {
        let re = regex::Regex::new(p)
            .unwrap_or_else(|e| fail(&format!("suite.fail-pattern is not a valid regex: {e}")));
        if re.captures_len() < 2 {
            fail("suite.fail-pattern needs one capture group: the failing test's name");
        }
        re
    });

    let config_dir = std::path::Path::new(&config_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let root = config_dir.join(&cfg.suite.root);
    if !root.is_dir() {
        fail(&format!("suite.root {} is not a directory", root.display()));
    }

    let selected: Vec<&MutantConfig> = cfg
        .mutants
        .iter()
        .filter(|m| only.as_deref().is_none_or(|o| m.name.contains(o)))
        .collect();
    if selected.is_empty() {
        fail(match only {
            Some(_) => "--only matched no mutant",
            None => "no [[mutants]] in the config",
        });
    }

    // Original bytes of every file the pass touches, read once up front. Also the
    // restore oracle: after every probe the file must byte-match this.
    let mut originals: BTreeMap<&str, String> = BTreeMap::new();
    for m in &selected {
        if !originals.contains_key(m.file.as_str()) {
            let content = std::fs::read_to_string(root.join(&m.file))
                .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", m.file)));
            originals.insert(m.file.as_str(), content);
        }
    }

    // Baseline: the suite must prove itself green on the unmutated tree first.
    println!("baseline: running suite ...");
    let baseline = run_suite(&cfg.suite, &root, &proof).unwrap_or_else(|e| fail(&e));
    if let Some(defect) = baseline_defect(&baseline) {
        fail(&defect);
    }
    let (base_passed, base_failed, base_output) = match &baseline {
        SuiteOutcome::Ran {
            passed,
            failed,
            output,
            ..
        } => (*passed, *failed, output),
        _ => unreachable!("baseline_defect rejects non-Ran outcomes"),
    };
    // The green baseline is also the oracle for the fail-pattern: nothing failed, so
    // anything it matches here it would go on to misreport as a killer.
    if let Some(re) = fail_pattern.as_ref() {
        if let Some(defect) = fail_pattern_defect(base_output, re) {
            fail(&defect);
        }
    }
    println!("baseline: green ({base_passed} passed)");

    let mut reports: Vec<MutantReport> = Vec::new();
    for m in &selected {
        let original = &originals[m.file.as_str()];
        let path = root.join(&m.file);
        let verdict = 'v: {
            // The tree must still be pristine — a suite that mutates the tree, or a
            // failed earlier restore, invalidates every occurrence count taken at load.
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| fail(&format!("cannot re-read {}: {e}", m.file)));
            if on_disk != *original {
                fail(&format!(
                    "{} changed on disk mid-pass — tree is not pristine, aborting",
                    m.file
                ));
            }
            let occurrences = original.matches(m.target.as_str()).count();
            if occurrences != 1 {
                break 'v Verdict::HarnessError {
                    detail: format!(
                        "target occurs {occurrences}x (need exactly 1) — mutates nothing"
                    ),
                };
            }
            let mutated = original.replacen(m.target.as_str(), &m.replacement, 1);
            // Atomic (temp + rename) in both directions: a plain fs::write truncates
            // first, so a failure mid-write (ENOSPC, EIO) would leave the file
            // truncated with nothing to restore from on disk.
            write_atomic(&path, &mutated)
                .unwrap_or_else(|e| fail(&format!("cannot write mutant to {}: {e}", m.file)));
            let outcome = run_suite(&cfg.suite, &root, &proof);
            // Restore before anything can early-return, then verify byte-exact: a tree
            // left mutated poisons every later probe and the working copy itself.
            write_atomic(&path, original).unwrap_or_else(|e| {
                fail(&format!(
                    "RESTORE FAILED for {}: {e} — tree is dirty",
                    m.file
                ))
            });
            let restored = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| fail(&format!("cannot verify restore of {}: {e}", m.file)));
            if restored != *original {
                fail(&format!(
                    "restore of {} is not byte-exact — tree is dirty",
                    m.file
                ));
            }
            let outcome = outcome.unwrap_or_else(|e| fail(&e));
            mutant_verdict(outcome, fail_pattern.as_ref())
        };
        let line = match &verdict {
            Verdict::Killed { killed_by } if !killed_by.is_empty() => {
                format!("{} — killed by: {}", verdict.label(), killed_by.join(", "))
            }
            // The verdict is sound (it comes from the tally) but the killer column is
            // blank, which is the OTHER way a fail-pattern fails: too narrow, and
            // silent about it. The baseline check cannot see this one — a pattern that
            // matches nothing matches nothing at baseline too — so say it here rather
            // than let a matrix ship with no killers in it.
            Verdict::Killed { killed_by } if killed_by.is_empty() && fail_pattern.is_some() => {
                format!(
                    "{} — killer NOT NAMED: suite.fail-pattern matched nothing in this \
                     mutant's failing output",
                    verdict.label()
                )
            }
            Verdict::NoRun { detail } | Verdict::HarnessError { detail } => {
                format!("{} — {}", verdict.label(), detail)
            }
            _ => verdict.label().to_string(),
        };
        println!("{}: {line}", m.name);
        reports.push(MutantReport {
            name: m.name.clone(),
            file: m.file.clone(),
            verdict,
        });
    }

    let mut summary = Summary::default();
    for r in &reports {
        match r.verdict {
            Verdict::Killed { .. } => summary.killed += 1,
            Verdict::Survived => summary.survived += 1,
            Verdict::NoRun { .. } => summary.no_run += 1,
            Verdict::HarnessError { .. } => summary.harness_error += 1,
        }
    }
    println!(
        "\n== {}/{} killed; survived: {}; no-run: {}; harness errors: {}",
        summary.killed,
        reports.len(),
        summary.survived,
        summary.no_run,
        summary.harness_error
    );

    let verdicts: Vec<Verdict> = reports.iter().map(|r| r.verdict.clone()).collect();
    let report = Report {
        baseline: BaselineReport {
            passed: base_passed,
            failed: base_failed,
        },
        mutants: reports,
        summary,
    };
    if let Some(p) = json_path {
        let json = serde_json::to_string_pretty(&report).expect("report serializes");
        std::fs::write(&p, json).unwrap_or_else(|e| fail(&format!("cannot write {p}: {e}")));
    }
    std::process::exit(exit_code(&verdicts));
}

// ------------------------------------------------------------------ tests ----

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> regex::Regex {
        regex::Regex::new(r"(\d+) passed \| (\d+) failed").unwrap()
    }

    #[test]
    fn no_proof_line_is_never_scorable() {
        let out = classify_suite("panicked before any tests ran", true, &proof());
        assert!(matches!(out, SuiteOutcome::NoProof { .. }));
        // ...and becomes NO-RUN, not SURVIVED, whatever the exit code claimed.
        assert!(matches!(mutant_verdict(out, None), Verdict::NoRun { .. }));
    }

    #[test]
    fn proof_matches_sum_across_binaries() {
        let out = classify_suite(
            "test result: 3 passed | 0 failed\nlater: 4 passed | 2 failed",
            false,
            &proof(),
        );
        match out {
            SuiteOutcome::Ran { passed, failed, .. } => {
                assert_eq!(passed, 7);
                assert_eq!(failed, 2);
            }
            other => panic!("expected Ran, got {other:?}"),
        }
    }

    #[test]
    fn failed_tests_kill() {
        let out = classify_suite("5 passed | 1 failed", false, &proof());
        assert!(matches!(mutant_verdict(out, None), Verdict::Killed { .. }));
    }

    #[test]
    fn failing_tally_kills_even_when_the_exit_code_lies() {
        // A wrapper that swallows the suite's exit code must not launder a failure:
        // the tally is the suite's own word, and it says something failed.
        let out = classify_suite("5 passed | 1 failed", true, &proof());
        assert!(matches!(mutant_verdict(out, None), Verdict::Killed { .. }));
    }

    #[test]
    fn nonzero_exit_with_proof_kills_even_at_zero_failed_tally() {
        // deno prints its tally and then exits 1 on a failing permission/sanitizer step;
        // the suite declared failure, so the mutant is caught.
        let out = classify_suite("5 passed | 0 failed", false, &proof());
        assert!(matches!(mutant_verdict(out, None), Verdict::Killed { .. }));
    }

    #[test]
    fn clean_pass_survives() {
        let out = classify_suite("5 passed | 0 failed", true, &proof());
        assert_eq!(mutant_verdict(out, None), Verdict::Survived);
    }

    #[test]
    fn timeout_is_no_run() {
        assert!(matches!(
            mutant_verdict(SuiteOutcome::TimedOut { secs: 9 }, None),
            Verdict::NoRun { .. }
        ));
    }

    #[test]
    fn killed_by_extracts_failing_test_names() {
        let fp = regex::Regex::new(r"(?m)^(\S+) \.\.\. FAILED$").unwrap();
        let out = classify_suite(
            "guard_test ... FAILED\n1 passed | 1 failed",
            false,
            &proof(),
        );
        match mutant_verdict(out, Some(&fp)) {
            Verdict::Killed { killed_by } => assert_eq!(killed_by, vec!["guard_test"]),
            other => panic!("expected Killed, got {other:?}"),
        }
    }

    #[test]
    fn baseline_red_and_empty_and_silent_all_block() {
        let red = classify_suite("3 passed | 1 failed", false, &proof());
        assert!(baseline_defect(&red).unwrap().contains("RED"));
        let empty = classify_suite("0 passed | 0 failed", true, &proof());
        assert!(baseline_defect(&empty).unwrap().contains("0 tests"));
        let silent = classify_suite("", true, &proof());
        assert!(baseline_defect(&silent).unwrap().contains("no proof"));
        let nonzero_exit = classify_suite("3 passed | 0 failed", false, &proof());
        assert!(baseline_defect(&nonzero_exit).is_some());
        let green = classify_suite("3 passed | 0 failed", true, &proof());
        assert!(baseline_defect(&green).is_none());
    }

    #[test]
    fn exit_zero_only_when_everything_killed() {
        let killed = Verdict::Killed { killed_by: vec![] };
        assert_eq!(exit_code(&[killed.clone(), killed.clone()]), 0);
        assert_eq!(exit_code(&[killed.clone(), Verdict::Survived]), 1);
        assert_eq!(
            exit_code(&[
                killed.clone(),
                Verdict::NoRun {
                    detail: String::new()
                }
            ]),
            1
        );
        assert_eq!(
            exit_code(&[
                killed,
                Verdict::HarnessError {
                    detail: String::new()
                }
            ]),
            1
        );
    }

    #[test]
    fn config_parses_with_defaults_and_rejects_unknown_keys() {
        let cfg: Config = toml::from_str(
            r#"
            [suite]
            root = "."
            command = ["sh", "check.sh"]
            proof = '(\d+) passed \| (\d+) failed'

            [[mutants]]
            name = "M01"
            file = "code.txt"
            target = "a"
            replacement = "b"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.suite.timeout_secs, 1800);
        assert_eq!(cfg.mutants.len(), 1);

        let unknown: Result<Config, _> = toml::from_str(
            r#"
            [suite]
            root = "."
            command = ["sh"]
            proof = "x"
            typo-field = 1
            "#,
        );
        assert!(
            unknown.is_err(),
            "an unknown key must be a loud config error"
        );
    }

    #[test]
    fn red_baseline_names_the_actual_defect() {
        let tally_red = classify_suite("3 passed | 1 failed", false, &proof());
        assert!(baseline_defect(&tally_red).unwrap().contains("1 failed"));
        let exit_red = classify_suite("3 passed | 0 failed", false, &proof());
        assert!(baseline_defect(&exit_red)
            .unwrap()
            .contains("green tally but non-zero exit"));
    }

    #[test]
    fn capped_drain_keeps_the_tail() {
        let data: Vec<u8> = (0..100_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let out = drain_capped(std::io::Cursor::new(data.clone()), 1024);
        assert_eq!(out.len(), 1024);
        assert_eq!(
            out[..],
            data[data.len() - 1024..],
            "the TAIL survives the cap"
        );
        let small = drain_capped(std::io::Cursor::new(b"abc".to_vec()), 1024);
        assert_eq!(small, b"abc");
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("mp-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("f.txt");
        std::fs::write(&target, "old").unwrap();
        write_atomic(&target, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(
            !target.with_extension("mutation-probe.tmp").exists(),
            "the temp file is consumed by the rename"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_keeps_the_end_and_flattens_newlines() {
        assert_eq!(tail("abc\ndef", 4), " def");
        assert_eq!(tail("ab", 4), "ab");
    }

    // ------------------------------------------------- shipped harnesses ----
    //
    // Real captured output, verbatim, from a throwaway project holding the shapes a
    // shipped pattern has to survive: a PASS line, a single-line [FAIL: …], a
    // multi-line assertEq message (generated Solidity source — the shape that broke
    // the pattern in the incident), a fuzz counterexample with `]` inside the
    // message, a custom-error revert, a snake_case test name, an invariant failure
    // (name on a later line after a [Sequence] block), and forge's `Failing tests:`
    // recap, which prints every failure a second time.
    //
    //   forge-1.7.1-*         forge 1.7.1, `forge test --offline [--fuzz-seed 1]`
    //   forge-1.0.0-nightly-* forge 1.0.0-nightly, same, `--color never`
    //   cargo-1.95.0-red      cargo 1.95.0, `cargo test --no-fail-fast --color never`
    //   cargo-1.95.0-green    the same, filtered to the passing test
    //
    // Two forge versions because a pattern that only reads the newest build is not a
    // known-good pattern for the org's repos.
    const FORGE_GREEN: &str = include_str!("../fixtures/forge-1.7.1-green.txt");
    const FORGE_RED: &str = include_str!("../fixtures/forge-1.7.1-red.txt");
    const FORGE_OLD_RED: &str = include_str!("../fixtures/forge-1.0.0-nightly-red.txt");
    const CARGO_GREEN: &str = include_str!("../fixtures/cargo-1.95.0-green.txt");
    const CARGO_RED: &str = include_str!("../fixtures/cargo-1.95.0-red.txt");

    fn shipped(name: &str) -> (regex::Regex, regex::Regex) {
        let h = harness_named(name).expect("shipped harness");
        (
            regex::Regex::new(h.proof).expect("proof compiles"),
            regex::Regex::new(h.fail_pattern).expect("fail-pattern compiles"),
        )
    }

    /// Every failure in both red forge fixtures, in the order forge first prints them.
    const FORGE_KILLERS: &[&str] = &[
        "testGeneratedSourceMatchesSnapshot", // multi-line assertEq message
        "testSingleLineFailure",
        "testCustomErrorRevertIsUncaught",
        "testFuzz_BoundedIsAlwaysSmall", // counterexample with `]` inside the message
        "testPlainRevert",
        "test_snake_case_name_fails", // snake_case, not just `test\w+`
        "invariant_NeverIncrements",  // name on its own line, after a [Sequence] block
    ];

    #[test]
    fn shipped_forge_proof_reads_forges_own_tally() {
        let (proof, _) = shipped("forge");
        // "4 tests passed, 7 failed" per forge's own run summary, reached by summing
        // the per-contract `Suite result:` lines — and NOT double-counted off that
        // summary line, which is comma-shaped.
        for (fixture, label) in [(FORGE_RED, "1.7.1"), (FORGE_OLD_RED, "1.0.0-nightly")] {
            match classify_suite(fixture, false, &proof) {
                SuiteOutcome::Ran { passed, failed, .. } => {
                    assert_eq!((passed, failed), (4, 7), "forge {label}");
                }
                other => panic!("forge {label}: expected Ran, got {other:?}"),
            }
        }
        match classify_suite(FORGE_GREEN, true, &proof) {
            SuiteOutcome::Ran { passed, failed, .. } => assert_eq!((passed, failed), (2, 0)),
            other => panic!("expected Ran, got {other:?}"),
        }
    }

    #[test]
    fn shipped_forge_fail_pattern_names_every_failure_and_no_passing_test() {
        let (_, fp) = shipped("forge");
        for (fixture, label) in [(FORGE_RED, "1.7.1"), (FORGE_OLD_RED, "1.0.0-nightly")] {
            assert_eq!(
                captured_names(fixture, &fp),
                FORGE_KILLERS,
                "forge {label}: every failing test, once each — forge prints them twice, \
                 and the run's own summary says 7"
            );
        }
        // …and the green run, which is nothing but [PASS] lines, yields none.
        assert!(captured_names(FORGE_GREEN, &fp).is_empty());
        assert!(fail_pattern_defect(FORGE_GREEN, &fp).is_none());
    }

    #[test]
    fn shipped_cargo_patterns_read_real_cargo_output() {
        let (proof, fp) = shipped("cargo");
        match classify_suite(CARGO_RED, false, &proof) {
            SuiteOutcome::Ran { passed, failed, .. } => {
                // lib 1+1, integration 1+1, doctest 1+0 — the tallies sum across
                // every target, which is why one proof works for a cargo workspace.
                assert_eq!((passed, failed), (3, 2));
            }
            other => panic!("expected Ran, got {other:?}"),
        }
        assert_eq!(
            captured_names(CARGO_RED, &fp),
            vec!["tests::unit_fails_multiline", "integration_fails"]
        );
        match classify_suite(CARGO_GREEN, true, &proof) {
            SuiteOutcome::Ran { passed, failed, .. } => assert_eq!((passed, failed), (1, 0)),
            other => panic!("expected Ran, got {other:?}"),
        }
        assert!(fail_pattern_defect(CARGO_GREEN, &fp).is_none());
    }

    #[test]
    fn the_incidents_wide_pattern_is_caught_by_the_green_baseline() {
        // `'\] (test\w+)\('` — the pattern the campaign actually shipped. It matches
        // `[PASS] testFoo(` as readily as `[FAIL: …] testFoo(`, and the green
        // baseline is where that is provable: nothing failed, so anything captured
        // here is a passing test.
        let wide = regex::Regex::new(r"\] (test\w+)\(").unwrap();
        let defect = fail_pattern_defect(FORGE_GREEN, &wide).expect("must be rejected");
        assert!(defect.contains("testHeadGenesisIsNotZero"), "{defect}");
        assert!(defect.contains("testAppliedIsIdempotent"), "{defect}");
    }

    #[test]
    fn a_sound_fail_pattern_captures_nothing_at_a_green_baseline() {
        let fp = regex::Regex::new(r"(?m)^(\S+) \.\.\. FAILED$").unwrap();
        assert!(fail_pattern_defect("guard_test ... ok\n1 passed | 0 failed", &fp).is_none());
    }

    #[test]
    fn the_s_flag_fixes_multiline_messages_and_then_misattributes_invariants() {
        // Issue #13 proposed `(?s)` as the one-line fix, flagged as unverified. Both
        // halves of that, against real forge output:
        //
        // It IS the reason the narrow pattern names nobody — `.` stops at the
        // newlines inside a multi-line assertEq message, so `.*?` never reaches `]`.
        let narrow = regex::Regex::new(r"\[FAIL.*?\] (test\w+)\(").unwrap();
        assert!(!captured_names(FORGE_RED, &narrow)
            .iter()
            .any(|n| n == "testGeneratedSourceMatchesSnapshot"));
        let dotall = regex::Regex::new(r"(?s)\[FAIL.*?\] (test\w+)\(").unwrap();
        assert!(captured_names(FORGE_RED, &dotall)
            .iter()
            .any(|n| n == "testGeneratedSourceMatchesSnapshot"));

        // And it trades that blank for a WRONG cell, which is the worse half. A forge
        // invariant failure has no `] name(` of its own — the name is on a later line,
        // after a [Sequence] block — so `(?s)` runs on past the end of that entry and
        // stops at the next `] name(` in the output. When the next one belongs to a
        // [PASS] line, the pattern names a test that PASSED under the mutant: defect 1
        // again, arrived at from the opposite direction.
        let dotall_any = regex::Regex::new(r"(?s)\[FAIL.*?\] (\w+)\(").unwrap();
        for (fixture, label) in [(FORGE_RED, "1.7.1"), (FORGE_OLD_RED, "1.0.0-nightly")] {
            let names = captured_names(fixture, &dotall_any);
            assert!(
                !names.iter().any(|n| n == "invariant_NeverIncrements"),
                "forge {label}: (?s) drops the invariant: {names:?}"
            );
            assert!(
                names.iter().any(|n| n == "testCounterStartsAtZero"),
                "forge {label}: (?s) names a PASSING test: {names:?}"
            );
        }
        // The shipped pattern is line-anchored instead: all seven failures, no passers.
        let (_, fp) = shipped("forge");
        assert_eq!(captured_names(FORGE_OLD_RED, &fp), FORGE_KILLERS);
    }

    #[test]
    fn killed_by_is_distinct_and_capped() {
        let fp = regex::Regex::new(r"(?m)^(\S+) \.\.\. FAILED$").unwrap();
        let mut out = String::new();
        for i in 0..8 {
            // Each name printed twice, the way forge repeats its failures.
            out.push_str(&format!("t{i} ... FAILED\nt{i} ... FAILED\n"));
        }
        out.push_str("0 passed | 8 failed");
        let outcome = classify_suite(&out, false, &proof());
        match mutant_verdict(outcome, Some(&fp)) {
            Verdict::Killed { killed_by } => {
                assert_eq!(killed_by, vec!["t0", "t1", "t2", "t3", "t4"]);
            }
            other => panic!("expected Killed, got {other:?}"),
        }
    }

    fn suite_config(toml_body: &str) -> SuiteConfig {
        let cfg: Config = toml::from_str(&format!(
            "[suite]\nroot = \".\"\ncommand = [\"sh\"]\n{toml_body}\n"
        ))
        .expect("config parses");
        cfg.suite
    }

    #[test]
    fn a_named_harness_supplies_both_patterns() {
        let (proof, fp) = resolve_patterns(&suite_config(r#"harness = "forge""#)).unwrap();
        let h = harness_named("forge").unwrap();
        assert_eq!(proof, h.proof);
        assert_eq!(fp.as_deref(), Some(h.fail_pattern));
    }

    #[test]
    fn explicit_patterns_override_the_harness() {
        // A suite that wraps or reformats its harness's output must still be able to
        // say so; silently using the harness's pattern would be its own wrong matrix.
        let cfg = suite_config(
            "harness = \"forge\"\nproof = '(\\d+) ok (\\d+) bad'\nfail-pattern = 'X(\\w+)'",
        );
        let (proof, fp) = resolve_patterns(&cfg).unwrap();
        assert_eq!(proof, r"(\d+) ok (\d+) bad");
        assert_eq!(fp.as_deref(), Some(r"X(\w+)"));
    }

    #[test]
    fn an_unknown_harness_names_the_ones_that_exist() {
        let err = resolve_patterns(&suite_config(r#"harness = "jest""#)).unwrap_err();
        assert!(err.contains("jest"), "{err}");
        assert!(err.contains("forge") && err.contains("cargo"), "{err}");
    }

    #[test]
    fn without_a_harness_proof_is_still_required() {
        let err = resolve_patterns(&suite_config("")).unwrap_err();
        assert!(err.contains("suite.proof"), "{err}");
        // …and a hand-written proof alone is enough, with no killer attribution.
        let (proof, fp) = resolve_patterns(&suite_config(r"proof = '(\d+)/(\d+)'")).unwrap();
        assert_eq!(proof, r"(\d+)/(\d+)");
        assert_eq!(fp, None);
    }

    #[test]
    fn every_shipped_pattern_compiles_with_the_groups_the_probe_reads() {
        for h in HARNESSES {
            let proof = regex::Regex::new(h.proof)
                .unwrap_or_else(|e| panic!("{}: proof does not compile: {e}", h.name));
            assert!(
                proof.captures_len() >= 3,
                "{}: proof needs (passed) and (failed)",
                h.name
            );
            let fp = regex::Regex::new(h.fail_pattern)
                .unwrap_or_else(|e| panic!("{}: fail-pattern does not compile: {e}", h.name));
            assert_eq!(
                fp.captures_len(),
                2,
                "{}: fail-pattern needs exactly one group — the probe reads group 1, so a \
                 second group would be silently ignored",
                h.name
            );
        }
    }
}
