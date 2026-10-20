// mutation-gen end to end against a toy git repo: a base commit plus a branch that edits
// one file and adds another. The generated set is pinned exactly, regenerating it is
// byte-identical, shards partition it, and mutation-probe scores it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CHECK_SH: &str = r#"
p=0; f=0
if grep -q "LIMIT = 3" code.txt; then echo "limit_test ... ok"; p=$((p+1)); else echo "limit_test ... FAILED"; f=$((f+1)); fi
if [ -f extra.txt ]; then echo "extra_test ... ok"; p=$((p+1)); else echo "extra_test ... FAILED"; f=$((f+1)); fi
echo "$p passed | $f failed"
[ "$f" -eq 0 ] || exit 1
"#;

const SUITE: &str = r#"[suite]
root = "."
command = ["sh", "check.sh"]
proof = '(\d+) passed \| (\d+) failed'
fail-pattern = '(\S+) \.\.\. FAILED'
timeout-secs = 60
"#;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A repo at `branch` HEAD whose diff from `main` is the fixture every test reads.
fn repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutation-gen-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("check.sh"), CHECK_SH).unwrap();
    std::fs::write(dir.join("code.txt"), "keep\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "branch"]);
    std::fs::write(
        dir.join("code.txt"),
        "keep\nLIMIT = 3\n# a < b\nxs = [a, b]\n",
    )
    .unwrap();
    std::fs::write(dir.join("extra.txt"), "present\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "change"]);
    std::fs::write(dir.join("suite.toml"), SUITE).unwrap();
    dir
}

fn gen(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mutation-gen"))
        .args(["--base", "main", "--suite", "suite.toml"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn names(config: &str) -> Vec<String> {
    let v: toml::Table = toml::from_str(config).unwrap();
    v["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect()
}

const EXPECTED: &[&str] = &[
    "G0001 code.txt:2:9 3 -> 4",
    "G0002 code.txt:2:9 3 -> 2",
    "G0003 code.txt:2:1 delete-line",
    "G0004 code.txt:4:7 delete-entry",
    "G0005 code.txt:4:10 delete-entry",
    "G0006 code.txt:4:1 delete-line",
    "G0007 extra.txt delete-file",
    "G0008 extra.txt:1:1 delete-line",
];

#[test]
fn the_diff_fixes_the_set_and_regenerating_is_byte_identical() {
    let dir = repo("set");
    let first = gen(&dir, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out = String::from_utf8(first.stdout).unwrap();
    assert_eq!(names(&out), EXPECTED);
    assert!(out.contains("replacement = \"xs = [b]\""), "{out}");
    assert!(out.contains("delete = true"), "{out}");
    assert_eq!(
        gen(&dir, &[]).stdout,
        out.as_bytes(),
        "same diff, same bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_probe_scores_the_generated_set() {
    let dir = repo("probe");
    let out = gen(&dir, &[]);
    std::fs::write(dir.join("mutants.toml"), &out.stdout).unwrap();
    let json = dir.join("report.json");
    let probe = Command::new(env!("CARGO_BIN_EXE_mutation-probe"))
        .arg(dir.join("mutants.toml"))
        .arg("--json")
        .arg(&json)
        .output()
        .unwrap();
    assert_eq!(
        probe.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&probe.stdout)
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let verdicts: Vec<&str> = report["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["verdict"].as_str().unwrap())
        .collect();
    assert_eq!(
        verdicts,
        ["KILLED", "KILLED", "KILLED", "SURVIVED", "SURVIVED", "SURVIVED", "KILLED", "SURVIVED"]
    );
    assert_eq!(
        report["mutants"][6]["killed_by"][0].as_str(),
        Some("extra_test")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("extra.txt")).unwrap(),
        "present\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shards_partition_the_set_in_order() {
    let dir = repo("shards");
    let out = gen(&dir, &["--shard-size", "3", "--out-dir", "shards"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut all = Vec::new();
    for (k, size) in [(1, 3), (2, 3), (3, 2)] {
        let shard = std::fs::read_to_string(dir.join(format!("shards/shard-00{k}.toml"))).unwrap();
        assert!(
            shard.contains(&format!("# shard {k}/3: {size} mutants")),
            "{shard}"
        );
        assert!(
            shard.contains("command = [\"sh\", \"check.sh\"]"),
            "every shard is a full config"
        );
        all.extend(names(&shard));
    }
    assert_eq!(all, EXPECTED);
    assert!(!dir.join("shards/shard-004.toml").exists());
    let again = gen(&dir, &["--shard-size", "3", "--out-dir", "shards"]);
    assert_eq!(
        again.status.code(),
        Some(2),
        "a non-empty out-dir would mix stale shards in"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pathspecs_narrow_the_diff() {
    let dir = repo("pathspec");
    let out = gen(&dir, &["--", ".", ":(exclude)extra.txt"]);
    let out = String::from_utf8(out.stdout).unwrap();
    assert_eq!(names(&out), EXPECTED[..6]);
    assert!(out.contains("# pathspec: . :(exclude)extra.txt"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_working_tree_off_head_or_a_bad_suite_file_is_refused() {
    let dir = repo("refuse");
    std::fs::write(dir.join("code.txt"), "dirty\n").unwrap();
    let dirty = gen(&dir, &[]);
    assert_eq!(dirty.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&dirty.stderr).contains("code.txt differs from HEAD"));
    git(&dir, &["checkout", "-q", "code.txt"]);

    std::fs::write(
        dir.join("suite.toml"),
        format!("{SUITE}\n[[mutants]]\nname = \"x\"\n"),
    )
    .unwrap();
    let extra = gen(&dir, &[]);
    assert_eq!(extra.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&extra.stderr).contains("only the [suite] table"));

    std::fs::write(dir.join("suite.toml"), SUITE).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let nested = Command::new(env!("CARGO_BIN_EXE_mutation-gen"))
        .args(["--base", "main", "--suite", "../suite.toml"])
        .current_dir(dir.join("sub"))
        .output()
        .unwrap();
    assert_eq!(nested.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&nested.stderr).contains("top level"));
    let _ = std::fs::remove_dir_all(&dir);
}
