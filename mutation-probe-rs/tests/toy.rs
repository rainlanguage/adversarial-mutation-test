// End-to-end probe runs against a toy repo with a deliberately weak suite: one behavior
// covered (killed), one uncovered (survived), one mutant that crashes the suite before
// its summary (no-run), one whose target does not exist (harness-error). The toy suite
// is plain `sh`, so the whole matrix runs hermetically inside `cargo test`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The suite: crashes pre-summary on BOOM, checks GUARD, never checks CAP.
const CHECK_SH: &str = r#"
p=0; f=0
if grep -q "BOOM" code.txt; then exit 7; fi
if grep -q "GUARD on" code.txt; then echo "guard_test ... ok"; p=$((p+1)); else echo "guard_test ... FAILED"; f=$((f+1)); fi
echo "cap_test ... ok"; p=$((p+1))
echo "$p passed | $f failed"
[ "$f" -eq 0 ] || exit 1
"#;

fn toy(dir: &Path, code: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("code.txt"), code).unwrap();
    std::fs::write(dir.join("check.sh"), CHECK_SH).unwrap();
}

fn unique_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mutation-probe-toy-{tag}-{}", std::process::id()))
}

fn write_config(dir: &Path, mutants_toml: &str) -> PathBuf {
    let config = format!(
        r#"
[suite]
root = "."
command = ["sh", "check.sh"]
proof = '(\d+) passed \| (\d+) failed'
fail-pattern = '(\S+) \.\.\. FAILED'
timeout-secs = 60
{mutants_toml}
"#
    );
    let path = dir.join("mutants.toml");
    std::fs::write(&path, config).unwrap();
    path
}

fn run(config: &Path, extra: &[&str]) -> (i32, String, serde_json::Value) {
    let json_path = config.with_extension("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_mutation-probe"))
        .arg(config)
        .arg("--json")
        .arg(&json_path)
        .args(extra)
        .output()
        .unwrap();
    let report = std::fs::read_to_string(&json_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        report,
    )
}

const ALL_FOUR: &str = r#"
[[mutants]]
name = "M-kill guard off"
file = "code.txt"
target = "GUARD on"
replacement = "GUARD off"

[[mutants]]
name = "M-survive cap unchecked"
file = "code.txt"
target = "CAP 10"
replacement = "CAP 99"

[[mutants]]
name = "M-norun crash the suite"
file = "code.txt"
target = "MODE strict"
replacement = "BOOM"

[[mutants]]
name = "M-zero no such target"
file = "code.txt"
target = "ABSENT TEXT"
replacement = "whatever"
"#;

#[test]
fn weak_suite_scores_all_four_verdicts_and_restores() {
    let dir = unique_dir("four");
    let code = "GUARD on\nCAP 10\nMODE strict\n";
    toy(&dir, code);
    let config = write_config(&dir, ALL_FOUR);

    let (exit, out, report) = run(&config, &[]);
    assert_eq!(exit, 1, "survivors must exit 1; output:\n{out}");

    let verdicts: Vec<(&str, &str)> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["verdict"].as_str().unwrap()))
        .collect();
    assert_eq!(
        verdicts,
        vec![
            ("M-kill guard off", "KILLED"),
            ("M-survive cap unchecked", "SURVIVED"),
            ("M-norun crash the suite", "NO-RUN"),
            ("M-zero no such target", "HARNESS-ERROR"),
        ]
    );
    assert_eq!(
        report["mutants"][0]["killed_by"][0].as_str(),
        Some("guard_test"),
        "fail-pattern names the killer"
    );
    assert_eq!(report["baseline"]["passed"].as_u64(), Some(2));
    assert_eq!(
        std::fs::read_to_string(dir.join("code.txt")).unwrap(),
        code,
        "the tree must be restored byte-exact after the pass"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn all_killed_exits_zero() {
    let dir = unique_dir("killed");
    toy(&dir, "GUARD on\nCAP 10\nMODE strict\n");
    let config = write_config(
        &dir,
        r#"
[[mutants]]
name = "M-kill guard off"
file = "code.txt"
target = "GUARD on"
replacement = "GUARD off"
"#,
    );
    let (exit, out, report) = run(&config, &[]);
    assert_eq!(exit, 0, "an all-killed pass exits 0; output:\n{out}");
    assert_eq!(report["summary"]["killed"].as_u64(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn red_baseline_aborts_without_probing() {
    let dir = unique_dir("red");
    toy(&dir, "GUARD off\nCAP 10\nMODE strict\n");
    let config = write_config(&dir, ALL_FOUR);
    let (exit, out, report) = run(&config, &[]);
    assert_eq!(exit, 2, "a red baseline is an abort; output:\n{out}");
    assert!(
        out.contains("RED"),
        "the abort names the red baseline:\n{out}"
    );
    assert_eq!(
        report,
        serde_json::Value::Null,
        "no report on an aborted pass"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crashed_baseline_aborts_as_no_proof() {
    let dir = unique_dir("crash");
    toy(&dir, "GUARD on\nCAP 10\nBOOM\n");
    let config = write_config(&dir, ALL_FOUR);
    let (exit, out, _) = run(&config, &[]);
    assert_eq!(exit, 2);
    assert!(
        out.contains("no proof-of-run"),
        "a baseline that cannot prove it ran must say so:\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hung_suite_times_out_as_no_run_and_restores() {
    // The mutant makes the suite sleep far past timeout-secs; sh's CHILD (sleep)
    // holds the output pipes, so this also proves the process-group kill — with a
    // child-only kill the probe would hang on the pipe readers, not finish.
    let dir = unique_dir("timeout");
    let code = "GUARD on\nCAP 10\nMODE strict\n";
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("code.txt"), code).unwrap();
    std::fs::write(
        dir.join("check.sh"),
        "if grep -q SLOW code.txt; then sleep 600; fi\n".to_string() + CHECK_SH,
    )
    .unwrap();
    let config = r#"
[suite]
root = "."
command = ["sh", "check.sh"]
proof = '(\d+) passed \| (\d+) failed'
timeout-secs = 2

[[mutants]]
name = "M-hang the suite sleeps forever"
file = "code.txt"
target = "MODE strict"
replacement = "SLOW"
"#;
    let config_path = dir.join("mutants.toml");
    std::fs::write(&config_path, config).unwrap();
    let started = std::time::Instant::now();
    let (exit, out, report) = run(&config_path, &[]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the probe must not hang on the hung suite's pipes"
    );
    assert_eq!(exit, 1, "a NO-RUN is a non-kill; output:\n{out}");
    assert_eq!(report["mutants"][0]["verdict"].as_str(), Some("NO-RUN"));
    assert!(
        report["mutants"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("timed out"),
        "the detail names the timeout"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("code.txt")).unwrap(),
        code,
        "restored despite the timeout"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failing_tally_with_zero_exit_still_kills() {
    // A wrapper that swallows the suite's exit code must not launder a failure the
    // suite's own tally reports — end-to-end twin of the unit test.
    let dir = unique_dir("liar");
    let code = "GUARD on\nCAP 10\nMODE strict\n";
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("code.txt"), code).unwrap();
    // Same suite, but the final failure-propagating line is replaced with exit 0.
    let swallowing = CHECK_SH.replace("[ \"$f\" -eq 0 ] || exit 1", "exit 0");
    assert_ne!(
        swallowing, CHECK_SH,
        "the exit-propagation line must exist to be swallowed"
    );
    std::fs::write(dir.join("check.sh"), swallowing).unwrap();
    let config = write_config(
        &dir,
        r#"
[[mutants]]
name = "M-kill guard off"
file = "code.txt"
target = "GUARD on"
replacement = "GUARD off"
"#,
    );
    let (exit, out, report) = run(&config, &[]);
    assert_eq!(exit, 0, "killed via the tally alone; output:\n{out}");
    assert_eq!(report["mutants"][0]["verdict"].as_str(), Some("KILLED"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_filter_narrows_the_pass() {
    let dir = unique_dir("only");
    let code = "GUARD on\nCAP 10\nMODE strict\n";
    toy(&dir, code);
    let config = write_config(&dir, ALL_FOUR);
    let (exit, out, report) = run(&config, &["--only", "M-kill"]);
    assert_eq!(exit, 0, "the killed mutant alone exits 0; output:\n{out}");
    assert_eq!(report["mutants"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------- two-phase (narrow) probing ----
//
// A second toy, laid out so every branch of the derivation is a real repo shape:
//
//   src/guard.txt  -> test/guard.t.sh   exists, and its tests kill the guard mutant
//   src/mode.txt   -> test/mode.t.sh    exists, but the killer lives in guard.t.sh
//   src/cap.txt    -> test/cap.t.sh     ABSENT: tests do not mirror this source
//   src/flag.txt   -> test/flag.t.sh    exists but selects NO tests; its killer is
//                                       in guard.t.sh, so a probe that trusted the
//                                       empty selection would score a false gap
//
// The suite writes every invocation's selection to runs.log, so the tests can assert
// which runs actually happened rather than inferring it from the verdict.

const NARROW_CHECK_SH: &str = r#"
sel="$1"
echo "${sel:-FULL}" >> runs.log
p=0; f=0
guard=0; cap=0; mode=0
case "$sel" in
  "") guard=1; cap=1; mode=1 ;;
  test/guard.t.sh) guard=1 ;;
  test/cap.t.sh) cap=1 ;;
  test/mode.t.sh) mode=1 ;;
esac
if [ "$guard" = 1 ]; then
  if grep -q "GUARD on" src/guard.txt; then echo "guard_test ... ok"; p=$((p+1)); else echo "guard_test ... FAILED"; f=$((f+1)); fi
  if grep -q "MODE strict" src/mode.txt; then echo "mode_cross_test ... ok"; p=$((p+1)); else echo "mode_cross_test ... FAILED"; f=$((f+1)); fi
  if grep -q "FLAG on" src/flag.txt; then echo "flag_cross_test ... ok"; p=$((p+1)); else echo "flag_cross_test ... FAILED"; f=$((f+1)); fi
fi
if [ "$cap" = 1 ]; then echo "cap_test ... ok"; p=$((p+1)); fi
if [ "$mode" = 1 ]; then echo "mode_smoke_test ... ok"; p=$((p+1)); fi
echo "$p passed | $f failed"
[ "$f" -eq 0 ] || exit 1
"#;

const NARROW_MUTANTS: &str = r#"
[[mutants]]
name = "M-narrow guard off"
file = "src/guard.txt"
target = "GUARD on"
replacement = "GUARD off"

[[mutants]]
name = "M-escalate mode loose"
file = "src/mode.txt"
target = "MODE strict"
replacement = "MODE loose"

[[mutants]]
name = "M-nomirror cap raised"
file = "src/cap.txt"
target = "CAP 10"
replacement = "CAP 99"

[[mutants]]
name = "M-emptysel flag off"
file = "src/flag.txt"
target = "FLAG on"
replacement = "FLAG off"
"#;

/// The toy repo plus a `[suite.narrow]` config; returns the mutants file's path.
fn narrow_toy(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("test")).unwrap();
    std::fs::write(dir.join("src/guard.txt"), "GUARD on\n").unwrap();
    std::fs::write(dir.join("src/mode.txt"), "MODE strict\n").unwrap();
    std::fs::write(dir.join("src/cap.txt"), "CAP 10\n").unwrap();
    std::fs::write(dir.join("src/flag.txt"), "FLAG on\n").unwrap();
    // test/cap.t.sh is deliberately NOT created.
    for t in ["test/guard.t.sh", "test/mode.t.sh", "test/flag.t.sh"] {
        std::fs::write(dir.join(t), "# selected by name; the suite dispatches\n").unwrap();
    }
    std::fs::write(dir.join("check.sh"), NARROW_CHECK_SH).unwrap();
    let config = format!(
        r#"
[suite]
root = "."
command = ["sh", "check.sh"]
proof = '(\d+) passed \| (\d+) failed'
fail-pattern = '(\S+) \.\.\. FAILED'
timeout-secs = 60

[suite.narrow]
from = '^src/(.*)\.txt$'
to = 'test/$1.t.sh'
command = ["sh", "check.sh", "{{}}"]
{NARROW_MUTANTS}
"#
    );
    let path = dir.join("mutants.toml");
    std::fs::write(&path, config).unwrap();
    path
}

/// Every suite invocation, in order, as the toy recorded them ("FULL" = whole suite).
fn runs(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("runs.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_narrow_kill_settles_without_ever_running_the_whole_suite() {
    let dir = unique_dir("narrow-kill");
    let config = narrow_toy(&dir);
    let (exit, out, report) = run(&config, &["--only", "M-narrow guard"]);
    assert_eq!(exit, 0, "a killed mutant exits 0; output:\n{out}");

    let m = &report["mutants"][0];
    assert_eq!(m["verdict"].as_str(), Some("KILLED"));
    assert_eq!(m["phase"].as_str(), Some("narrow"));
    assert_eq!(m["selection"].as_str(), Some("test/guard.t.sh"));
    assert_eq!(m["killed_by"][0].as_str(), Some("guard_test"));
    assert_eq!(report["summary"]["narrow_settled"].as_u64(), Some(1));
    assert_eq!(
        report["baseline"]["narrow"]["test/guard.t.sh"].as_u64(),
        Some(3),
        "the selection's own baseline count is recorded, so a selection that \
         silently matched nothing would be visible as a small wrong number"
    );

    // The saving is the claim, so assert it directly rather than trusting `phase`.
    assert_eq!(
        runs(&dir),
        vec!["FULL", "test/guard.t.sh", "test/guard.t.sh"],
        "one full baseline, one narrow baseline, one narrow probe — the whole \
         suite never ran for the mutant itself"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_apparent_survival_escalates_to_the_whole_suite_and_is_killed_there() {
    // The reason the whole suite was mandatory: a mutant killed by a test you did not
    // predict. test/mode.t.sh passes under the mutant; the killer is in guard.t.sh.
    let dir = unique_dir("narrow-escalate");
    let config = narrow_toy(&dir);
    let (exit, out, report) = run(&config, &["--only", "M-escalate"]);
    assert_eq!(
        exit, 0,
        "the escalated run finds the killer, so the pass is all-killed; output:\n{out}"
    );

    let m = &report["mutants"][0];
    assert_eq!(
        m["verdict"].as_str(),
        Some("KILLED"),
        "narrowing must not turn an unpredicted killer into a false gap"
    );
    assert_eq!(m["phase"].as_str(), Some("full"));
    assert_eq!(
        m["selection"].as_str(),
        Some("test/mode.t.sh"),
        "a selection with phase=full reads as 'narrow ran and was escalated'"
    );
    assert_eq!(m["killed_by"][0].as_str(), Some("mode_cross_test"));
    assert_eq!(report["summary"]["narrow_settled"].as_u64(), Some(0));
    assert_eq!(
        runs(&dir),
        vec!["FULL", "test/mode.t.sh", "test/mode.t.sh", "FULL"],
        "the apparent survival is re-taken against the whole suite BEFORE it is recorded"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_with_no_mirrored_test_falls_back_to_the_whole_suite() {
    let dir = unique_dir("narrow-nomirror");
    let config = narrow_toy(&dir);
    let (exit, out, report) = run(&config, &["--only", "M-nomirror"]);
    assert_eq!(exit, 1, "a genuine survivor exits 1; output:\n{out}");

    let m = &report["mutants"][0];
    assert_eq!(m["verdict"].as_str(), Some("SURVIVED"));
    assert_eq!(m["phase"].as_str(), Some("full"));
    assert!(
        m["selection"].is_null(),
        "no selection was derivable, so none is claimed"
    );
    assert!(
        out.contains("test/cap.t.sh does not exist"),
        "the fallback names its reason rather than narrowing silently:\n{out}"
    );
    assert_eq!(
        runs(&dir),
        vec!["FULL", "FULL"],
        "no narrow run at all — the whole suite decided this one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_selection_that_runs_no_tests_is_refused_at_its_own_baseline() {
    // test/flag.t.sh EXISTS, so the path check passes, but selecting it runs nothing.
    // Such a selection can kill nothing: every mutant would clear the narrow phase and
    // escalate anyway, so probing against it is pure overhead and the report would name
    // a selection that proved nothing. The selection's own baseline catches it first.
    let dir = unique_dir("narrow-empty");
    let config = narrow_toy(&dir);
    let (exit, out, report) = run(&config, &["--only", "M-emptysel"]);
    assert_eq!(exit, 0, "the whole suite kills it; output:\n{out}");

    let m = &report["mutants"][0];
    assert_eq!(m["verdict"].as_str(), Some("KILLED"));
    assert_eq!(m["phase"].as_str(), Some("full"));
    assert!(m["selection"].is_null());
    assert!(
        report["baseline"]["narrow"].is_null(),
        "a refused selection is not recorded as a usable narrow baseline"
    );
    assert!(
        out.contains("0 tests"),
        "the refusal names the empty selection:\n{out}"
    );
    assert_eq!(
        runs(&dir),
        vec!["FULL", "test/flag.t.sh", "FULL"],
        "the selection is tried ONCE at baseline, then abandoned for the whole suite"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_narrow_baseline_serves_every_mutant_on_the_same_file() {
    let dir = unique_dir("narrow-shared-baseline");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("test")).unwrap();
    std::fs::write(dir.join("src/guard.txt"), "GUARD on\n").unwrap();
    std::fs::write(dir.join("src/mode.txt"), "MODE strict\n").unwrap();
    std::fs::write(dir.join("src/cap.txt"), "CAP 10\n").unwrap();
    std::fs::write(dir.join("src/flag.txt"), "FLAG on\n").unwrap();
    std::fs::write(dir.join("test/guard.t.sh"), "#\n").unwrap();
    std::fs::write(dir.join("check.sh"), NARROW_CHECK_SH).unwrap();
    let config = r#"
[suite]
root = "."
command = ["sh", "check.sh"]
proof = '(\d+) passed \| (\d+) failed'
timeout-secs = 60

[suite.narrow]
from = '^src/(.*)\.txt$'
to = 'test/$1.t.sh'
command = ["sh", "check.sh", "{}"]

[[mutants]]
name = "M-a guard off"
file = "src/guard.txt"
target = "GUARD on"
replacement = "GUARD off"

[[mutants]]
name = "M-b guard absent"
file = "src/guard.txt"
target = "GUARD"
replacement = "SENTRY"
"#;
    let path = dir.join("mutants.toml");
    std::fs::write(&path, config).unwrap();

    let (exit, out, report) = run(&path, &[]);
    assert_eq!(exit, 0, "both mutants are killed narrowly; output:\n{out}");
    assert_eq!(report["summary"]["narrow_settled"].as_u64(), Some(2));
    assert_eq!(
        runs(&dir),
        vec![
            "FULL",
            "test/guard.t.sh",
            "test/guard.t.sh",
            "test/guard.t.sh"
        ],
        "the selection is baselined once per file, not once per mutant"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_narrow_command_that_ignores_the_selection_is_a_config_error() {
    let dir = unique_dir("narrow-noplaceholder");
    let config = narrow_toy(&dir);
    let broken = std::fs::read_to_string(&config).unwrap().replace(
        r#"command = ["sh", "check.sh", "{}"]"#,
        r#"command = ["sh", "check.sh"]"#,
    );
    std::fs::write(&config, broken).unwrap();
    let (exit, out, _) = run(&config, &[]);
    assert_eq!(
        exit, 2,
        "a config that cannot narrow must not run; output:\n{out}"
    );
    assert!(
        out.contains("placeholder"),
        "the error names the missing selection slot:\n{out}"
    );
    assert!(
        runs(&dir).is_empty(),
        "the config is rejected before any suite runs"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
