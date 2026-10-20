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

#[test]
fn output_omits_unset_fields_and_reports_counts() {
    let dir = repo("fields");
    let out = gen(&dir, &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains("delete = false"), "{stdout}");
    let v: toml::Table = toml::from_str(&stdout).unwrap();
    let file = v["mutants"][6].as_table().unwrap();
    assert_eq!(file["name"].as_str(), Some("G0007 extra.txt delete-file"));
    assert!(!file.contains_key("target") && !file.contains_key("replacement"));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "8 mutants\n");

    let shards = gen(&dir, &["--shard-size", "3", "--out-dir", "shards"]);
    assert_eq!(
        String::from_utf8_lossy(&shards.stdout),
        "shards/shard-001.toml\t3\nshards/shard-002.toml\t3\nshards/shard-003.toml\t2\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&shards.stderr),
        "8 mutants in 3 shards\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shard_flags_are_validated() {
    let dir = repo("shardflags");
    let zero = gen(&dir, &["--shard-size", "0", "--out-dir", "z"]);
    assert_eq!(zero.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&zero.stderr).contains("--shard-size needs a positive integer"));
    let alone = gen(&dir, &["--shard-size", "3"]);
    assert_eq!(alone.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&alone.stderr).contains("--shard-size and --out-dir go together")
    );
    let one = gen(&dir, &["--shard-size", "1", "--out-dir", "ones"]);
    assert!(
        one.status.success(),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );
    assert!(dir.join("ones/shard-008.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg(unix)]
fn modes_encodings_and_deletions_pick_what_is_mutated() {
    let dir = repo("modes");
    std::fs::write(dir.join("run.sh"), "go = 1\n").unwrap();
    std::fs::set_permissions(
        dir.join("run.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    git(&dir, &["add", "run.sh"]);
    std::os::unix::fs::symlink("code.txt", dir.join("link")).unwrap();
    std::fs::write(dir.join("bin.dat"), b"\xff = 1\n").unwrap();
    git(&dir, &["rm", "-q", "check.sh"]);
    git(&dir, &["add", "link", "bin.dat"]);
    git(&dir, &["commit", "-qm", "modes"]);
    let out = gen(&dir, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.contains("# skipped: bin.dat (not UTF-8)\n# skipped: link (mode 120000)\n\n"),
        "{out}"
    );
    assert!(!out.contains("# skipped: check.sh"), "{out}");
    let names = names(&out);
    assert!(
        names.contains(&"G0009 run.sh delete-file".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"G0010 run.sh:1:6 1 -> 2".to_string()),
        "{names:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn text_files_git_would_diff_as_binary_are_still_mutated() {
    let dir = repo("binary");
    std::fs::write(dir.join(".gitattributes"), "attr.txt -diff\n").unwrap();
    std::fs::write(dir.join("attr.txt"), "n = 1\n").unwrap();
    std::fs::write(dir.join("nul.txt"), "a\0b\nn = 1\n").unwrap();
    git(&dir, &["add", ".gitattributes", "attr.txt", "nul.txt"]);
    git(&dir, &["commit", "-qm", "binary to git"]);
    let out = gen(&dir, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let names = names(&String::from_utf8(out.stdout).unwrap());
    assert!(
        names.iter().any(|n| n.ends_with(" attr.txt:1:5 1 -> 2")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with(" nul.txt:2:5 1 -> 2")),
        "{names:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn staged_changes_are_refused_and_unrelated_edits_are_ignored() {
    let dir = repo("staged");
    std::fs::write(dir.join("code.txt"), "staged\n").unwrap();
    git(&dir, &["add", "code.txt"]);
    let staged = gen(&dir, &[]);
    assert_eq!(staged.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&staged.stderr).contains("code.txt differs from HEAD"));
    git(&dir, &["reset", "-q", "--hard"]);

    std::fs::write(dir.join("check.sh"), "unrelated\n").unwrap();
    let unrelated = gen(&dir, &[]);
    assert!(
        unrelated.status.success(),
        "{}",
        String::from_utf8_lossy(&unrelated.stderr)
    );
    assert_eq!(
        names(&String::from_utf8(unrelated.stdout).unwrap()),
        EXPECTED
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn paths_that_parse_as_revisions_stay_paths() {
    let dir = repo("literal");
    git(&dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("(literal)code.txt"), "decoy\n").unwrap();
    git(&dir, &["add", "(literal)code.txt"]);
    git(&dir, &["commit", "-qm", "decoy"]);
    git(&dir, &["checkout", "-q", "branch"]);
    git(&dir, &["merge", "-q", "--no-edit", "main"]);
    let all = gen(&dir, &[]);
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    assert_eq!(names(&String::from_utf8(all.stdout).unwrap()), EXPECTED);
    let narrowed = gen(&dir, &["--", ":(literal)code.txt"]);
    assert!(
        narrowed.status.success(),
        "{}",
        String::from_utf8_lossy(&narrowed.stderr)
    );
    assert_eq!(
        names(&String::from_utf8(narrowed.stdout).unwrap()),
        EXPECTED[..6]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Files whose added lines move under each diff knob that user or repo config can turn.
fn knob_repo() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutation-gen-knobs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();
    write(".gitattributes", "tc.txt diff=pre\n");
    write("pre.sh", "echo pre; cat \"$1\"\n");
    write("indent.txt", "1\n2\na\n\nb\n3\n4\n");
    write("alg.txt", "b\na\nx\na\nx\nx\nc\n");
    write("gap.txt", "a\nb\nc\n");
    write("old.txt", "r1\nr2\nr3\nr4\nr5\n");
    write("tc.txt", "t\n");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "branch"]);
    write("indent.txt", "1\n2\na\n\nb\na\n\nb\n3\n4\n");
    write("alg.txt", "b\na\nx\nx\na\nx\na\n");
    write("gap.txt", "A\nb\nC\n");
    std::fs::remove_file(dir.join("old.txt")).unwrap();
    write("new.txt", "r1\nr2\nr3\nr4\nR5\n");
    write("tc.txt", "t\nu\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "change"]);
    write(
        "order.txt",
        "tc.txt\nnew.txt\ngap.txt\nalg.txt\nindent.txt\n",
    );
    std::fs::write(dir.join("suite.toml"), SUITE).unwrap();
    dir
}

#[test]
fn repo_config_cannot_move_the_set() {
    let dir = knob_repo();
    let plain = gen(&dir, &[]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain = String::from_utf8(plain.stdout).unwrap();
    let names = names(&plain);
    for want in [
        "alg.txt:5:1 delete-line",
        "alg.txt:7:1 delete-line",
        "gap.txt:1:1 delete-line",
        "gap.txt:3:1 delete-line",
        "indent.txt:5:1 delete-line",
        "new.txt delete-file",
        "tc.txt:2:1 delete-line",
    ] {
        assert!(names.iter().any(|n| n.ends_with(want)), "{want}: {names:?}");
    }
    assert!(!names.iter().any(|n| n.contains("gap.txt:2:")), "{names:?}");
    for (key, value) in [
        ("color.diff", "always"),
        ("diff.external", "true"),
        ("diff.pre.textconv", "sh pre.sh"),
        ("diff.renames", "true"),
        ("diff.interHunkContext", "3"),
        ("diff.algorithm", "histogram"),
        ("diff.indentHeuristic", "false"),
        ("diff.orderFile", "order.txt"),
    ] {
        git(&dir, &["config", key, value]);
        let out = gen(&dir, &[]);
        git(&dir, &["config", "--unset", key]);
        assert!(
            out.status.success(),
            "{key}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            plain,
            "{key}={value}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
