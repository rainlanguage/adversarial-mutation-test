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

#[test]
fn only_is_a_substring_filter_so_a_prefix_selects_every_match() {
    // `--only` matches by SUBSTRING, deliberately: names carry prose, and typing a
    // whole one exactly is hostile, while over-selection is fail-safe — a wider
    // match only adds verdicts, and exit 0 still demands that every one of them be
    // KILLED. This pins both halves against a silent switch to exact matching (which
    // would select nothing here and abort) or to first-match-wins (which would
    // report one mutant and exit 0, laundering the survivor out of the pass).
    let dir = unique_dir("substring");
    let code = "GUARD on\nCAP 10\nMODE strict\n";
    toy(&dir, code);
    let config = write_config(
        &dir,
        r#"
[[mutants]]
name = "M07 guard off"
file = "code.txt"
target = "GUARD on"
replacement = "GUARD off"

[[mutants]]
name = "M070 cap unchecked"
file = "code.txt"
target = "CAP 10"
replacement = "CAP 99"
"#,
    );
    let (exit, out, report) = run(&config, &["--only", "M07"]);
    let mutants = report["mutants"].as_array().unwrap();
    assert_eq!(
        mutants.len(),
        2,
        "M07 is a substring of both names, so both are probed; output:\n{out}"
    );
    assert_eq!(mutants[0]["verdict"].as_str(), Some("KILLED"));
    assert_eq!(mutants[1]["verdict"].as_str(), Some("SURVIVED"));
    assert_eq!(
        exit, 1,
        "the extra match's survival still fails the pass; output:\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
