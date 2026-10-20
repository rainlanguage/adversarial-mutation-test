// mutation-gen — the deterministic floor of a PR's mutant set.
//
// Hand-written mutants differ run to run, so a local pre-run and a reviewer never probe
// the same set and cannot converge. This bin enumerates mutants mechanically from
// `git diff <base>...HEAD`: every changed line crossed with each catalog operator that
// applies to it, plus deleting each added file and each added list entry. The output is
// a mutation-probe config, and the same diff always yields byte-identical output.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

// -------------------------------------------------------------- operators ----

/// Swapped wherever they occur with no operator character on either side, so `==`
/// inside `===` or `<=` inside `<=>` is left alone.
const TOKEN_SWAPS: &[(&str, &str)] = &[
    ("===", "!=="),
    ("!==", "==="),
    ("==", "!="),
    ("!=", "=="),
    ("<=", "<"),
    (">=", ">"),
    ("&&", "||"),
    ("||", "&&"),
    ("+=", "-="),
    ("-=", "+="),
];

/// Swapped only between spaces: unspaced `<`/`>` are generics as often as comparisons,
/// and unspaced `-`/`*`/`/` are as often unary, pointers or paths.
const SPACED_SWAPS: &[(&str, &str)] = &[
    ("<", "<="),
    (">", ">="),
    ("+", "-"),
    ("-", "+"),
    ("*", "/"),
    ("/", "*"),
];

const OPERATOR_CHARS: &str = "<>=!&|+-*/%^";

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_operator(c: Option<char>) -> bool {
    c.is_some_and(|c| OPERATOR_CHARS.contains(c))
}

/// One edit to one line: the line's new text, plus where and what for the mutant name.
struct LineMutation {
    col: usize,
    label: String,
    line: String,
}

/// PURE: a comment line carries no behaviour, so every mutant of it is equivalent.
fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    if ["//", "/*", "* ", "<!--", "#!/"]
        .iter()
        .any(|p| t.starts_with(p))
    {
        return true;
    }
    t.strip_prefix('#')
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn splice(line: &str, at: usize, len: usize, with: &str) -> String {
    format!("{}{}{}", &line[..at], with, &line[at + len..])
}

/// PURE: every catalog mutation of one changed line, in column order per operator.
fn line_mutations(line: &str) -> Vec<LineMutation> {
    let mut out = Vec::new();
    if is_comment(line) {
        return out;
    }
    let before = |at: usize| line[..at].chars().next_back();
    let after = |at: usize| line[at..].chars().next();

    for (from, to) in TOKEN_SWAPS {
        for (at, _) in line.match_indices(from) {
            if !is_operator(before(at)) && !is_operator(after(at + from.len())) {
                out.push(LineMutation {
                    col: at + 1,
                    label: format!("{from} -> {to}"),
                    line: splice(line, at, from.len(), to),
                });
            }
        }
    }
    for (from, to) in SPACED_SWAPS {
        for (at, _) in line.match_indices(from) {
            if before(at) == Some(' ') && after(at + from.len()) == Some(' ') {
                out.push(LineMutation {
                    col: at + 1,
                    label: format!("{from} -> {to}"),
                    line: splice(line, at, from.len(), to),
                });
            }
        }
    }
    for (at, _) in line.match_indices('!') {
        if after(at + 1) != Some('=') && !before(at).is_some_and(is_ident) {
            out.push(LineMutation {
                col: at + 1,
                label: "drop !".to_string(),
                line: splice(line, at, 1, ""),
            });
        }
    }
    for (from, to) in [("true", "false"), ("false", "true")] {
        for (at, _) in line.match_indices(from) {
            if !before(at).is_some_and(is_ident) && !after(at + from.len()).is_some_and(is_ident) {
                out.push(LineMutation {
                    col: at + 1,
                    label: format!("{from} -> {to}"),
                    line: splice(line, at, from.len(), to),
                });
            }
        }
    }
    out.extend(literal_mutations(line));
    out.extend(entry_deletions(line));
    if line.chars().any(|c| c.is_alphanumeric()) {
        let label = if line.trim_end().ends_with(',') {
            "delete-entry"
        } else {
            "delete-line"
        };
        out.push(LineMutation {
            col: 1,
            label: label.to_string(),
            line: String::new(),
        });
    }
    out
}

/// PURE: each standalone decimal integer literal n becomes n+1 and, when n > 0, n-1.
/// A run preceded by `.` is a fraction or a tuple index, and a multi-digit run with a
/// leading zero may be octal, so both are skipped.
fn literal_mutations(line: &str) -> Vec<LineMutation> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let prev = line[..start].chars().next_back();
        let next = line[i..].chars().next();
        let digits = &line[start..i];
        if prev.is_some_and(|c| is_ident(c) || c == '.')
            || next.is_some_and(is_ident)
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            continue;
        }
        let Ok(n) = digits.parse::<u128>() else {
            continue;
        };
        let mut push = |m: u128| {
            out.push(LineMutation {
                col: start + 1,
                label: format!("{n} -> {m}"),
                line: splice(line, start, digits.len(), &m.to_string()),
            })
        };
        if let Some(up) = n.checked_add(1) {
            push(up);
        }
        if n > 0 {
            push(n - 1);
        }
    }
    out
}

/// PURE: each entry of a single-line `[...]` list with at least two entries, deleted
/// with its separator. `"` strings are skipped over so a comma inside one is not a
/// separator; one-entry brackets are skipped because they are index expressions as
/// often as lists.
fn entry_deletions(line: &str) -> Vec<LineMutation> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    // Per open bracket: its byte offset, and the offsets of its top-level commas.
    let mut stack: Vec<(u8, usize, Vec<usize>)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            open @ (b'[' | b'(' | b'{') => stack.push((open, i, Vec::new())),
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.2.push(i);
                }
            }
            close @ (b']' | b')' | b'}') => {
                let Some((open, at, commas)) = stack.pop() else {
                    i += 1;
                    continue;
                };
                if open == b'[' && close == b']' {
                    out.extend(list_entries(line, at + 1, i, &commas));
                }
            }
            _ => {}
        }
        i += 1;
    }
    out.sort_by_key(|m| m.col);
    out
}

fn list_entries(line: &str, start: usize, end: usize, commas: &[usize]) -> Vec<LineMutation> {
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut from = start;
    for &c in commas.iter().chain(std::iter::once(&end)) {
        if !line[from..c].trim().is_empty() {
            bounds.push((from, c));
        }
        from = c + 1;
    }
    if bounds.len() < 2 {
        return Vec::new();
    }
    let lead = |s: usize, e: usize| s + (line[s..e].len() - line[s..e].trim_start().len());
    (0..bounds.len())
        .map(|k| {
            // Not last: from this entry's text through the next entry's text start.
            // Last: from the previous entry's end (its comma) through this entry's end.
            let (cut_from, cut_to) = if k + 1 < bounds.len() {
                (
                    lead(bounds[k].0, bounds[k].1),
                    lead(bounds[k + 1].0, bounds[k + 1].1),
                )
            } else {
                (
                    bounds[k - 1].1,
                    bounds[k].0 + line[bounds[k].0..bounds[k].1].trim_end().len(),
                )
            };
            LineMutation {
                col: lead(bounds[k].0, bounds[k].1) + 1,
                label: "delete-entry".to_string(),
                line: splice(line, cut_from, cut_to - cut_from, ""),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- targets ----

/// PURE: whether `needle` occurs exactly once in `hay`, overlapping occurrences counted.
/// The probe counts non-overlapping matches and applies the first, so overlapping
/// uniqueness is what guarantees the one it applies is this one.
fn occurs_once(hay: &str, needle: &str) -> bool {
    let Some(first) = hay.find(needle) else {
        return false;
    };
    let mut rest = hay[first..].chars();
    rest.next();
    !rest.as_str().contains(needle)
}

/// PURE: the smallest window of whole lines around line `i` (grown downward, then
/// upward, alternately) whose text occurs once in the file, as (first, last) indices.
fn unique_window(content: &str, lines: &[&str], i: usize) -> (usize, usize) {
    let (mut lo, mut hi) = (i, i);
    let mut down = true;
    while !occurs_once(content, &lines[lo..=hi].join("\n")) {
        if (down && hi + 1 < lines.len()) || lo == 0 {
            hi += 1;
        } else {
            lo -= 1;
        }
        down = !down;
    }
    (lo, hi)
}

// ---------------------------------------------------------------- mutants ----

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
struct Mutant {
    name: String,
    file: String,
    target: Option<String>,
    replacement: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    delete: bool,
}

/// One changed file as the diff describes it, with its working-tree content.
struct Changed {
    path: String,
    added: bool,
    lines: Vec<usize>,
    content: String,
}

/// PURE: the mutants of one changed file, unnumbered (`name` holds only the locator).
fn file_mutants(c: &Changed) -> Vec<Mutant> {
    let mut out = Vec::new();
    if c.added {
        out.push(Mutant {
            name: format!("{} delete-file", c.path),
            file: c.path.clone(),
            target: None,
            replacement: None,
            delete: true,
        });
    }
    let lines: Vec<&str> = c.content.split('\n').collect();
    for &n in &c.lines {
        let Some(&line) = lines.get(n - 1) else {
            continue;
        };
        // The window is only searched for once the line has a mutation.
        let mut window: Option<(usize, String)> = None;
        for m in line_mutations(line) {
            let (lo, target) = window.get_or_insert_with(|| {
                let (lo, hi) = unique_window(&c.content, &lines, n - 1);
                (lo, lines[lo..=hi].join("\n"))
            });
            let mut edited: Vec<&str> = target.split('\n').collect();
            edited[n - 1 - *lo] = &m.line;
            let replacement = edited.join("\n");
            let target = target.clone();
            if out.iter().any(|o: &Mutant| {
                o.target.as_deref() == Some(&target)
                    && o.replacement.as_deref() == Some(&replacement)
            }) {
                continue;
            }
            out.push(Mutant {
                name: format!("{}:{}:{} {}", c.path, n, m.col, m.label),
                file: c.path.clone(),
                target: Some(target),
                replacement: Some(replacement),
                delete: false,
            });
        }
    }
    out
}

/// PURE: prefix every mutant with a zero-padded ordinal, `G0001` up.
fn number(mut mutants: Vec<Mutant>) -> Vec<Mutant> {
    let width = mutants.len().to_string().len().max(4);
    for (k, m) in mutants.iter_mut().enumerate() {
        m.name = format!("G{:0width$} {}", k + 1, m.name);
    }
    mutants
}

/// PURE: added line numbers from `git diff -U0` hunk headers (`@@ -a,b +c,d @@`).
/// A header that does not parse is an error: skipping it would silently drop lines.
fn added_lines(patch: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for h in patch.lines().filter(|l| l.starts_with("@@ ")) {
        let malformed = || format!("malformed hunk header {h:?}");
        let plus = h
            .split(' ')
            .find_map(|w| w.strip_prefix('+'))
            .ok_or_else(malformed)?;
        let (start, count) = plus.split_once(',').unwrap_or((plus, "1"));
        let (Ok(start), Ok(count)) = (start.parse::<usize>(), count.parse::<usize>()) else {
            return Err(malformed());
        };
        out.extend(start..start + count);
    }
    Ok(out)
}

/// PURE: (status, new mode, path) entries from `git diff --raw -z --no-renames`, where
/// every entry is one meta field then one path field. A meta field that does not parse
/// is an error: skipping it would silently drop a file.
fn raw_entries(raw: &[u8]) -> Result<Vec<(String, String, String)>, String> {
    let mut fields = raw.split(|&b| b == 0);
    let mut out = Vec::new();
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let meta = String::from_utf8_lossy(meta);
        let words: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, _, status] = words[..] else {
            return Err(format!("malformed raw diff entry {meta:?}"));
        };
        out.push((
            status.to_string(),
            mode.to_string(),
            String::from_utf8_lossy(path).into_owned(),
        ));
    }
    Ok(out)
}

// -------------------------------------------------------------------- git ----

/// Flags that pin every diff knob user or repo config could otherwise turn, so the set
/// of added lines cannot differ between machines.
const DIFF_FLAGS: &[&str] = &[
    "--no-color",
    "--text",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--diff-algorithm=myers",
    "--indent-heuristic",
    "--inter-hunk-context=0",
];

fn git(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn git_line(args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git(args)?).trim().to_string())
}

struct Generated {
    base: String,
    head: String,
    mutants: Vec<Mutant>,
    skipped: Vec<String>,
}

fn generate(base: &str, pathspecs: &[String]) -> Result<Generated, String> {
    if !git_line(&["rev-parse", "--show-prefix"])?.is_empty() {
        return Err(
            "run from the repository's top level: generated paths are relative to it".into(),
        );
    }
    let head = git_line(&["rev-parse", "HEAD^{commit}"])?;
    let mb = git_line(&["merge-base", base, &head])?;
    let mut raw_args: Vec<&str> = vec!["diff", "--raw", "-z"];
    raw_args.extend(DIFF_FLAGS);
    raw_args.extend([mb.as_str(), head.as_str(), "--"]);
    raw_args.extend(pathspecs.iter().map(String::as_str));
    let mut entries = raw_entries(&git(&raw_args)?)?;
    entries.sort_by(|a, b| a.2.cmp(&b.2));

    let mut mutants = Vec::new();
    let mut skipped = Vec::new();
    for (status, mode, path) in entries {
        if !matches!(status.as_str(), "A" | "M" | "T") {
            continue;
        }
        if mode != "100644" && mode != "100755" {
            skipped.push(format!("{path} (mode {mode})"));
            continue;
        }
        let literal = format!(":(literal){path}");
        if !Command::new("git")
            .args(["diff", "--quiet", "HEAD", "--", &literal])
            .status()
            .map_err(|e| format!("cannot run git: {e}"))?
            .success()
        {
            return Err(format!(
                "{path} differs from HEAD in the working tree; commit or stash first"
            ));
        }
        let Ok(content) = String::from_utf8(
            std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?,
        ) else {
            skipped.push(format!("{path} (not UTF-8)"));
            continue;
        };
        let mut patch_args: Vec<&str> = vec!["diff", "-U0"];
        patch_args.extend(DIFF_FLAGS);
        // Against the working tree, which matches HEAD for this path (checked above).
        patch_args.extend([mb.as_str(), "--", literal.as_str()]);
        let patch = String::from_utf8_lossy(&git(&patch_args)?).into_owned();
        mutants.extend(file_mutants(&Changed {
            added: status == "A",
            lines: added_lines(&patch)?,
            content,
            path,
        }));
    }
    Ok(Generated {
        base: mb,
        head,
        mutants: number(mutants),
        skipped,
    })
}

// ----------------------------------------------------------------- output ----

#[derive(Serialize)]
struct MutantsTable<'a> {
    mutants: &'a [Mutant],
}

/// PURE: one complete mutation-probe config: provenance comments, the suite file's text
/// verbatim, then the mutants.
fn render(
    g: &Generated,
    pathspecs: &[String],
    suite: &str,
    shard: Option<(usize, usize)>,
    mutants: &[Mutant],
) -> String {
    let mut out = format!(
        "# mutation-gen {}: {} mutants from `git diff {}...{}`\n# pathspec: {}\n",
        env!("CARGO_PKG_VERSION"),
        g.mutants.len(),
        g.base,
        g.head,
        if pathspecs.is_empty() {
            "(whole tree)".to_string()
        } else {
            pathspecs.join(" ")
        },
    );
    if let Some((k, n)) = shard {
        out.push_str(&format!("# shard {k}/{n}: {} mutants\n", mutants.len()));
    }
    for s in &g.skipped {
        out.push_str(&format!("# skipped: {s}\n"));
    }
    out.push('\n');
    out.push_str(suite.trim_end());
    out.push_str("\n\n");
    out.push_str(&toml::to_string(&MutantsTable { mutants }).expect("mutants serialize"));
    out
}

const HELP: &str = r#"mutation-gen — enumerate a PR's mutants mechanically from its diff.

USAGE
    mutation-gen --base <rev> --suite <suite.toml> [--shard-size <k> --out-dir <dir>]
                 [-- <pathspec>...]

    Run from the repository's top level, on a working tree whose changed files match
    HEAD. Mutants come from `git diff <base>...HEAD` restricted to the pathspecs
    (git's own syntax, e.g. `. ':(exclude)test/**'`). The same diff, pathspecs and
    suite file always produce byte-identical output.

    --base <rev>        the PR's base (e.g. origin/main); diffed from its merge-base
    --suite <file>      a TOML file holding only the [suite] table, copied verbatim;
                        its `root` must resolve to the repository top level, which
                        every generated `file` is relative to
    --shard-size <k>    split the set into consecutive shards of at most k mutants,
                        written as <dir>/shard-NNN.toml, each a complete config
    --out-dir <dir>     where shards go; must not exist or be empty
    (no --shard-size)   the whole set goes to stdout

MUTANTS
    Per changed (added-side) line, skipping comment lines:
      ===/!==, ==/!=, <= -> <, >= -> >, &&/||, +=/-=      (no operator char adjacent)
      ' < ' -> ' <= ', ' > ' -> ' >= ', ' + '/' - ', ' * '/' / '     (spaced only)
      drop a prefix `!`; true/false; integer literal n -> n+1, n-1
      delete each entry of a one-line [a, b, ...] list (two or more entries)
      delete the line (named delete-entry when it ends in `,`: a multi-line entry)
    Per added file: delete the file (mutation-probe `delete = true`).
    Targets are the smallest window of whole lines around the change that occurs
    exactly once in the file. Identical edits are emitted once. Names are
    `G<ordinal> <path>:<line>:<col> <operator>`; non-UTF-8 files, symlinks and
    submodules are listed as skipped in the header.
"#;

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut base = None;
    let mut suite_path = None;
    let mut shard_size: Option<usize> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut pathspecs: Vec<String> = Vec::new();
    while let Some(a) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .unwrap_or_else(|| fail(&format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--base" => base = Some(value("--base")),
            "--suite" => suite_path = Some(value("--suite")),
            "--shard-size" => {
                shard_size = Some(
                    value("--shard-size")
                        .parse()
                        .ok()
                        .filter(|&k| k > 0)
                        .unwrap_or_else(|| fail("--shard-size needs a positive integer")),
                )
            }
            "--out-dir" => out_dir = Some(PathBuf::from(value("--out-dir"))),
            "--" => pathspecs.extend(args.by_ref()),
            other => fail(&format!("unexpected argument {other:?} (--help)")),
        }
    }
    let base = base.unwrap_or_else(|| fail("--base is required (--help)"));
    let suite_path = suite_path.unwrap_or_else(|| fail("--suite is required (--help)"));
    if shard_size.is_some() != out_dir.is_some() {
        fail("--shard-size and --out-dir go together");
    }

    let suite = std::fs::read_to_string(&suite_path)
        .unwrap_or_else(|e| fail(&format!("cannot read {suite_path}: {e}")));
    let table: toml::Table =
        toml::from_str(&suite).unwrap_or_else(|e| fail(&format!("{suite_path}: {e}")));
    if !table.get("suite").is_some_and(toml::Value::is_table) || table.len() != 1 {
        fail(&format!("{suite_path} must hold only the [suite] table"));
    }

    let g = generate(&base, &pathspecs).unwrap_or_else(|e| fail(&e));
    let (Some(k), Some(dir)) = (shard_size, out_dir) else {
        print!("{}", render(&g, &pathspecs, &suite, None, &g.mutants));
        eprintln!("{} mutants", g.mutants.len());
        return;
    };
    write_shards(&g, &pathspecs, &suite, k, &dir).unwrap_or_else(|e| fail(&e));
}

fn write_shards(
    g: &Generated,
    pathspecs: &[String],
    suite: &str,
    k: usize,
    dir: &Path,
) -> Result<(), String> {
    if dir.exists()
        && std::fs::read_dir(dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .next()
            .is_some()
    {
        return Err(format!(
            "{} is not empty; stale shards would mix sets",
            dir.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let shards: Vec<&[Mutant]> = g.mutants.chunks(k).collect();
    let width = shards.len().to_string().len().max(3);
    for (i, chunk) in shards.iter().enumerate() {
        let path = dir.join(format!("shard-{:0width$}.toml", i + 1));
        std::fs::write(
            &path,
            render(g, pathspecs, suite, Some((i + 1, shards.len())), chunk),
        )
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!("{}\t{}", path.display(), chunk.len());
    }
    eprintln!("{} mutants in {} shards", g.mutants.len(), shards.len());
    Ok(())
}

// ------------------------------------------------------------------ tests ----

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(line: &str) -> Vec<(usize, String, String)> {
        line_mutations(line)
            .into_iter()
            .map(|m| (m.col, m.label, m.line))
            .collect()
    }

    fn l(col: usize, label: &str, line: &str) -> (usize, String, String) {
        (col, label.to_string(), line.to_string())
    }

    #[test]
    fn comparison_line_gets_each_applicable_operator() {
        assert_eq!(
            labels("if a <= b && !c {"),
            vec![
                l(6, "<= -> <", "if a < b && !c {"),
                l(11, "&& -> ||", "if a <= b || !c {"),
                l(14, "drop !", "if a <= b && c {"),
                l(1, "delete-line", ""),
            ]
        );
    }

    #[test]
    fn operator_inside_a_longer_operator_is_not_split() {
        let got = labels("x === y");
        assert_eq!(
            got,
            vec![l(3, "=== -> !==", "x !== y"), l(1, "delete-line", "")]
        );
        assert!(labels("a <=> b").iter().all(|m| m.1 == "delete-line"));
        assert!(labels("a << b").iter().all(|m| m.1 == "delete-line"));
    }

    #[test]
    fn unspaced_angle_and_arithmetic_are_left_alone() {
        assert_eq!(labels("Vec<u8>"), vec![l(1, "delete-line", "")]);
        assert_eq!(
            labels("a < b - c"),
            vec![
                l(3, "< -> <=", "a <= b - c"),
                l(7, "- -> +", "a < b + c"),
                l(1, "delete-line", ""),
            ]
        );
        assert_eq!(
            labels("x-1"),
            vec![
                l(3, "1 -> 2", "x-2"),
                l(3, "1 -> 0", "x-0"),
                l(1, "delete-line", "")
            ]
        );
    }

    #[test]
    fn macro_bang_and_not_equal_are_not_negations() {
        assert_eq!(
            labels("println!(a != b)"),
            vec![
                l(12, "!= -> ==", "println!(a == b)"),
                l(1, "delete-line", "")
            ]
        );
    }

    #[test]
    fn booleans_swap_only_as_whole_words() {
        assert_eq!(
            labels("ok = true"),
            vec![l(6, "true -> false", "ok = false"), l(1, "delete-line", "")]
        );
        assert_eq!(labels("untrue_x"), vec![l(1, "delete-line", "")]);
    }

    #[test]
    fn literals_step_both_ways_and_skip_identifiers_fractions_and_octal() {
        assert_eq!(
            labels("n = 0 + 10"),
            vec![
                l(7, "+ -> -", "n = 0 - 10"),
                l(5, "0 -> 1", "n = 1 + 10"),
                l(9, "10 -> 11", "n = 0 + 11"),
                l(9, "10 -> 9", "n = 0 + 9"),
                l(1, "delete-line", ""),
            ]
        );
        assert_eq!(labels("u8 x.0 007 0x10"), vec![l(1, "delete-line", "")]);
        assert_eq!(
            labels("1.5"),
            vec![
                l(1, "1 -> 2", "2.5"),
                l(1, "1 -> 0", "0.5"),
                l(1, "delete-line", "")
            ]
        );
    }

    #[test]
    fn inline_list_entries_are_deleted_one_at_a_time() {
        let got: Vec<String> = entry_deletions(r#"xs = ["a,b", c, d]"#)
            .into_iter()
            .map(|m| m.line)
            .collect();
        assert_eq!(
            got,
            vec![r#"xs = [c, d]"#, r#"xs = ["a,b", d]"#, r#"xs = ["a,b", c]"#]
        );
        assert!(entry_deletions("a[i]").is_empty());
        assert!(entry_deletions("[]").is_empty());
        let trailing: Vec<String> = entry_deletions("[a, b,]")
            .into_iter()
            .map(|m| m.line)
            .collect();
        assert_eq!(trailing, vec!["[b,]", "[a,]"]);
        let nested: Vec<String> = entry_deletions("[f(a, b), [c, d]]")
            .into_iter()
            .map(|m| m.line)
            .collect();
        assert_eq!(
            nested,
            vec!["[[c, d]]", "[f(a, b)]", "[f(a, b), [d]]", "[f(a, b), [c]]"]
        );
    }

    #[test]
    fn trailing_comma_line_deletion_is_an_entry_and_comments_get_nothing() {
        assert_eq!(labels("    Foo,").last().unwrap().1, "delete-entry");
        for c in [
            "// a < b",
            "  # x = 1",
            "#",
            " * 1 == 2",
            "/* true */",
            "<!-- 1 -->",
            "#!/bin/sh",
        ] {
            assert!(labels(c).is_empty(), "{c:?}");
        }
        assert!(!labels("#[test]").is_empty());
        assert!(labels("  }").is_empty());
    }

    #[test]
    fn occurs_once_counts_overlaps() {
        assert!(occurs_once("abc", "b"));
        assert!(!occurs_once("aaa", "aa"));
        assert!(!occurs_once("abc", "x"));
        assert!(!occurs_once("b b", "b"));
    }

    #[test]
    fn window_grows_until_unique() {
        let content = "x\nif a < b {\ny\n}\nif a < b {\nz\n}\n";
        let lines: Vec<&str> = content.split('\n').collect();
        assert_eq!(unique_window(content, &lines, 1), (1, 2));
        assert_eq!(unique_window(content, &lines, 4), (4, 5));
        assert_eq!(unique_window(content, &lines, 0), (0, 0));
    }

    #[test]
    fn hunk_headers_give_added_lines() {
        let patch = "diff --git a/f b/f\n@@ -1,0 +1,2 @@\n+a\n+b\n@@ -9 +10 @@\n-x\n+y\n@@ -20,3 +21,0 @@\n-q\n";
        assert_eq!(added_lines(patch), Ok(vec![1, 2, 10]));
        for bad in ["@@ -1 @@", "@@ -1 +x,2 @@", "@@ -1 +3,y @@"] {
            assert_eq!(
                added_lines(bad),
                Err(format!("malformed hunk header {bad:?}"))
            );
        }
    }

    #[test]
    fn every_token_swap_replaces_with_its_partner() {
        for (line, want) in [
            ("a === b", "a !== b"),
            ("a !== b", "a === b"),
            ("a == b", "a != b"),
            ("a != b", "a == b"),
            ("a <= b", "a < b"),
            ("a >= b", "a > b"),
            ("a && b", "a || b"),
            ("a || b", "a && b"),
            ("a += b", "a -= b"),
            ("a -= b", "a += b"),
            ("a < b", "a <= b"),
            ("a > b", "a >= b"),
            ("a + b", "a - b"),
            ("a - b", "a + b"),
            ("a * b", "a / b"),
            ("a / b", "a * b"),
            ("a = true", "a = false"),
            ("a = false", "a = true"),
        ] {
            let got = labels(line);
            assert_eq!(got.len(), 2, "{line:?}: {got:?}");
            assert_eq!(got[0].2, want, "{line:?}");
        }
    }

    #[test]
    fn bang_and_booleans_touching_identifiers_are_not_operators() {
        assert_eq!(
            labels("a !== b"),
            vec![l(3, "!== -> ===", "a === b"), l(1, "delete-line", "")]
        );
        assert_eq!(labels("istrue"), vec![l(1, "delete-line", "")]);
        assert_eq!(labels("true_x"), vec![l(1, "delete-line", "")]);
    }

    #[test]
    fn code_after_a_block_comment_close_is_mutated() {
        assert_eq!(
            labels("*/ x = 1"),
            vec![
                l(8, "1 -> 2", "*/ x = 2"),
                l(8, "1 -> 0", "*/ x = 0"),
                l(1, "delete-line", "")
            ]
        );
    }

    #[test]
    fn two_digit_leading_zero_is_octal_and_non_ascii_is_stepped_over() {
        assert_eq!(labels("x = 07"), vec![l(1, "delete-line", "")]);
        assert_eq!(
            literal_mutations("é1")
                .into_iter()
                .map(|m| (m.col, m.line))
                .collect::<Vec<_>>(),
            vec![(3, "é2".to_string()), (3, "é0".to_string())]
        );
    }

    #[test]
    fn list_strings_brackets_and_escapes_delimit_entries() {
        let lines =
            |s: &str| -> Vec<String> { entry_deletions(s).into_iter().map(|m| m.line).collect() };
        assert_eq!(lines(r#"["", a]"#), vec!["[a]", r#"[""]"#]);
        assert_eq!(lines(r#"["a\"b", c]"#), vec!["[c]", r#"["a\"b"]"#]);
        assert_eq!(lines(r#"["a\\", b]"#), vec!["[b]", r#"["a\\"]"#]);
        assert!(lines(r#"["a, b"#).is_empty());
        assert_eq!(lines("][a, b]"), vec!["][b]", "][a]"]);
        assert!(lines("[a, b)").is_empty());
        assert!(lines("(a, b]").is_empty());
    }

    #[test]
    fn window_alternates_down_then_up_and_stops_at_the_last_line() {
        let content = "p\na\nb\nq\na\nb\nr";
        let lines: Vec<&str> = content.split('\n').collect();
        assert_eq!(unique_window(content, &lines, 1), (0, 2));
        let content = "a\nb\na\nc";
        let lines: Vec<&str> = content.split('\n').collect();
        assert_eq!(unique_window(content, &lines, 2), (2, 3));
        let content = "a\nb\na";
        let lines: Vec<&str> = content.split('\n').collect();
        assert_eq!(unique_window(content, &lines, 2), (1, 2));
    }

    #[test]
    fn identical_edits_of_one_line_are_emitted_once() {
        let c = Changed {
            path: "f".into(),
            added: false,
            lines: vec![1],
            content: "xs = [a, a]\n".into(),
        };
        let names: Vec<String> = file_mutants(&c).into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["f:1:7 delete-entry", "f:1:1 delete-line"]);
    }

    #[test]
    fn raw_entries_parse_status_mode_and_path() {
        let raw = b":000000 100644 0000000 abcdef0 A\0new file.rs\0:100644 120000 1 2 T\0link\0";
        let entry = |s: &str, m: &str, p: &str| (s.to_string(), m.to_string(), p.to_string());
        assert_eq!(
            raw_entries(raw),
            Ok(vec![
                entry("A", "100644", "new file.rs"),
                entry("T", "120000", "link"),
            ])
        );
        assert_eq!(raw_entries(b""), Ok(vec![]));
        assert_eq!(
            raw_entries(b":100644 100644 1 2\0f\0"),
            Err(r#"malformed raw diff entry ":100644 100644 1 2""#.to_string())
        );
    }

    #[test]
    fn file_mutants_delete_added_files_and_dedupe_identical_edits() {
        let c = Changed {
            path: "f".into(),
            added: true,
            lines: vec![1, 2],
            content: "a == b\na == b\n".into(),
        };
        let ms = file_mutants(&c);
        assert!(ms[0].delete && ms[0].name == "f delete-file");
        for m in &ms[1..] {
            let t = m.target.as_deref().unwrap();
            assert!(occurs_once(&c.content, t), "{t:?}");
            assert_eq!(c.content.matches(t).count(), 1);
        }
        let mut pairs: Vec<_> = ms.iter().map(|m| (&m.target, &m.replacement)).collect();
        pairs.dedup();
        assert_eq!(pairs.len(), ms.len());
        assert_eq!(number(ms)[0].name, "G0001 f delete-file");
    }
}
