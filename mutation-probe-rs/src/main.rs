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
// run or could not be trusted (config unreadable, baseline not green, restore failed,
// another probe owns the tree).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Proof-of-run regex over the suite's combined stdout+stderr. Needs two capture
    /// groups: passed count, failed count. Multiple matches sum (cargo prints one result
    /// line per test binary). No match anywhere = the suite did not provably run.
    proof: String,
    /// Optional: one capture group extracting a failing test's name, for `killedBy`.
    #[serde(rename = "fail-pattern")]
    fail_pattern: Option<String>,
    /// Per-run wall clock limit. A hung suite is NO-RUN, not a hung campaign.
    #[serde(rename = "timeout-secs", default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    1800
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
                        re.captures_iter(&output)
                            .filter_map(|c| c.get(1))
                            .map(|m| m.as_str().to_string())
                            .take(5)
                            .collect()
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

// ------------------------------------------------------------------- lock ----

// A probe outlives the agent that launched it. An agent that dies mid-pass (token
// guard, compaction, kill) leaves mutation-probe running until its remaining mutants
// are done: in one campaign two such orphans mutated and restored one file for two
// hours inside clones nobody owned, and a successor — told the reports "do not exist",
// which is also what an unfinished pass looks like — was dispatched to re-run them
// against those same clones. Two probes on one tree interleave: one's restore lands as
// the other's mutant, and every verdict after that attaches to the wrong code. That is
// the failure a per-worker clone exists to prevent, and a clone cannot prevent it — a
// clone isolates workers from each other, never a worker from its own orphans.
//
// So the tree carries the claim: `<root>/.mutation-test/probe.lock`, created
// exclusively before the baseline, rewritten with the mutant currently on disk, and
// removed on every exit this process controls. Keyed by root because the contended
// resource is the tree, not the config or the report: two passes with different
// mutants files still mutate one working copy. Derived from root, so a successor
// holding nothing but the clone path can read who owns it, how far it got, and whether
// it died with a mutant applied — instead of inferring liveness from an absent report.

const LOCK_DIR: &str = ".mutation-test";
const LOCK_FILE: &str = "probe.lock";

fn lock_path(root: &Path) -> PathBuf {
    root.join(LOCK_DIR).join(LOCK_FILE)
}

/// The owning run, as the next reader of the tree needs it.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LockRecord {
    pid: u32,
    /// Liveness of a pid is only checkable on the machine that recorded it.
    host: String,
    #[serde(rename = "started-unix")]
    started_unix: u64,
    root: String,
    config: String,
    command: Vec<String>,
    mutants: usize,
    /// Serialized as a table, so it stays last.
    phase: Phase,
}

/// Where the run is, and — the part a successor cannot get anywhere else — whether a
/// mutant is currently written to the tree.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Phase {
    /// No mutant has been applied yet: the tree is as the run found it.
    Baseline,
    /// This mutant is on disk. A run killed here leaves the tree mutated; the field
    /// is NOT cleared after the restore, so it reads as "the last mutant this run
    /// wrote", and a dead run always costs the successor one tree check. Over-
    /// reporting dirt is recoverable; under-reporting it is the incident.
    Probing {
        mutant: String,
        file: String,
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Alive,
    Dead,
    /// Not checkable from here (another host, an implausible pid, an unknown errno).
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
enum LockDecision {
    Abort(String),
    Reclaim(String),
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = vec![0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into a buffer we own.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "unknown-host".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
fn hostname() -> String {
    "unknown-host".to_string()
}

/// Is the recorded owner still running?
///
/// A pid only means anything on the host that issued it, so a foreign host is Unknown
/// rather than assumed dead — and Unknown aborts. Pid reuse can report a stranger as
/// the owner; that direction costs one manual `rm`, while the other direction is two
/// probes on one tree.
#[cfg(unix)]
fn process_liveness(record_host: &str, our_host: &str, pid: u32) -> Liveness {
    if record_host != our_host {
        return Liveness::Unknown;
    }
    if pid == 0 || pid > i32::MAX as u32 {
        return Liveness::Unknown;
    }
    // SAFETY: signal 0 delivers nothing; it only asks whether the pid exists.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return Liveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        // Running, owned by another user.
        Some(libc::EPERM) => Liveness::Alive,
        Some(libc::ESRCH) => Liveness::Dead,
        _ => Liveness::Unknown,
    }
}

#[cfg(not(unix))]
fn process_liveness(_record_host: &str, _our_host: &str, _pid: u32) -> Liveness {
    Liveness::Unknown
}

/// PURE: what a probe does about a lock it found.
///
/// Only one case reclaims: an owner this host can prove is gone, which never applied a
/// mutant, so the tree it leaves behind is the tree it was given. Everything else is a
/// question about code state that a probe must not answer by overwriting the evidence.
fn lock_decision(found: &LockRecord, liveness: Liveness, now: u64) -> LockDecision {
    let age = now.saturating_sub(found.started_unix);
    let who = format!("pid {} on {} (started {age}s ago)", found.pid, found.host);
    match (liveness, &found.phase) {
        (Liveness::Alive, phase) => LockDecision::Abort(format!(
            "another mutation-probe owns this tree: {who}, {}. Two probes on one tree \
             interleave mutant and restore, so every verdict lands on the wrong code — \
             wait for it, or give this pass its own clone.",
            describe_phase(phase)
        )),
        (Liveness::Unknown, phase) => LockDecision::Abort(format!(
            "a mutation-probe lock is held by {who}, {} — this host cannot check whether \
             that process is still running. Confirm it is gone, verify the tree, then \
             delete the lock.",
            describe_phase(phase)
        )),
        (
            Liveness::Dead,
            Phase::Probing {
                mutant,
                file,
                index,
            },
        ) => LockDecision::Abort(format!(
            "a mutation-probe ({who}) died at mutant {index} with {mutant:?} written to \
             {file}. The tree may still hold that mutant, and probing mutated source \
             scores every verdict against code nobody wrote. Verify and restore the tree \
             (`git status --porcelain`, `git checkout -- {file}`), then delete the lock."
        )),
        (Liveness::Dead, Phase::Baseline) => LockDecision::Reclaim(format!(
            "reclaiming a stale lock: {who} died before applying any mutant, so the tree \
             it held is unmutated"
        )),
    }
}

fn describe_phase(phase: &Phase) -> String {
    match phase {
        Phase::Baseline => "running the baseline".to_string(),
        Phase::Probing {
            mutant,
            file,
            index,
        } => format!("at mutant {index}, {mutant:?} applied to {file}"),
    }
}

/// Set ONLY by an acquire that won the race, so an abort onto someone else's lock can
/// never delete it on the way out.
static HELD_LOCK: OnceLock<PathBuf> = OnceLock::new();

fn serialize_lock(record: &LockRecord) -> String {
    toml::to_string(record).expect("the lock record serializes")
}

/// Take the tree, or say who has it. The link below is the exclusion itself — the
/// check and the claim are one syscall, so two probes starting together cannot both
/// win, and the loser reads a record that is already complete.
fn acquire_lock(path: &Path, record: &LockRecord) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create lock directory {}: {e}", dir.display()))?;
    }
    if let Err(e) = create_lock_exclusively(path, record) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("cannot write lock {}: {e}", path.display()));
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read existing lock {}: {e}", path.display()))?;
        let found: LockRecord = toml::from_str(&raw).map_err(|e| {
            format!(
                "lock {} exists but does not parse ({e}) — a probe wrote it and something \
                 truncated it. Verify the tree, then delete the lock.",
                path.display()
            )
        })?;
        let liveness = process_liveness(&found.host, &record.host, found.pid);
        match lock_decision(&found, liveness, now_unix()) {
            LockDecision::Abort(reason) => {
                return Err(format!("{reason}\nlock: {}", path.display()));
            }
            LockDecision::Reclaim(note) => {
                eprintln!("warning: {note}");
                std::fs::remove_file(path)
                    .map_err(|e| format!("cannot remove stale lock {}: {e}", path.display()))?;
                // A second probe can have reclaimed the same stale lock in that gap;
                // it now owns the tree and this pass does not.
                create_lock_exclusively(path, record).map_err(|e| {
                    format!(
                        "another probe claimed {} while this one was reclaiming a stale \
                         lock: {e}",
                        path.display()
                    )
                })?;
            }
        }
    }
    let _ = HELD_LOCK.set(path.to_path_buf());
    Ok(())
}

/// Publish the record, then claim the path by linking to it: `link` fails with
/// AlreadyExists if the lock is taken, and the lock only ever appears with its full
/// contents. Creating it empty and filling it afterwards would let a losing probe read
/// a half-written record and report a corrupt lock over a tree that is merely busy.
fn create_lock_exclusively(path: &Path, record: &LockRecord) -> std::io::Result<()> {
    let staged = path.with_extension(format!("staged-{}", std::process::id()));
    std::fs::write(&staged, serialize_lock(record))?;
    let claimed = std::fs::hard_link(&staged, path);
    let _ = std::fs::remove_file(&staged);
    claimed
}

/// Republish the record mid-pass. Called BEFORE the mutant reaches the tree, so the
/// lock never lags the disk.
fn update_lock(record: &LockRecord) -> Result<(), String> {
    let Some(path) = HELD_LOCK.get() else {
        return Ok(());
    };
    write_atomic(path, &serialize_lock(record))
}

/// Drop the claim. A lock is removed at exit rather than left as a completion record:
/// presence has to mean "a run owns this tree", or every later probe pays to decide
/// whether it does.
fn release_lock() {
    if let Some(path) = HELD_LOCK.get() {
        let _ = std::fs::remove_file(path);
    }
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

/// Every abort funnels here, so this is also where the tree's lock is given back —
/// `std::process::exit` runs no destructor, and a lock outliving its aborted run makes
/// the next probe pay for a run that is not there.
fn fail(msg: &str) -> ! {
    release_lock();
    eprintln!("error: {msg}");
    std::process::exit(2);
}

/// The manual lives here (and in the repo README), NOT in the skill text: skill prose
/// is a recurring per-invocation context cost, while --help is read on demand.
const HELP: &str = r#"mutation-probe — apply exact-string mutants, prove the suite ran, score honestly.

USAGE
    mutation-probe <mutants.toml> [--json <path>] [--only <substring>]

    --json <path>       also write the machine-readable report
    --only <substring>  probe EVERY mutant whose name contains the substring, so a
                        short value ("M07") also selects longer names ("M070").
                        Deliberately not exact matching: names carry prose, and
                        over-selection is fail-safe — the extra mutants are probed
                        and scored, exit 0 still demands all of them KILLED, and a
                        value matching nothing is an error, never a silent no-op.
    --help, -h          this manual

MUTANTS FILE (TOML)
    [suite]
    root = "."                    # repo the suite runs in, relative to this file
    command = ["sh", "check.sh"]  # argv, no shell. Include any artifact regeneration
                                  # here (wrapper script is fine): the probe runs
                                  # exactly this per verdict, and a suite that tests
                                  # stale artifacts is the #1 way a matrix lies.
    proof = '(\d+) passed; (\d+) failed'
                                  # 2 capture groups: passed, failed — read from the
                                  # suite's own tally. Multiple matches SUM (cargo
                                  # prints one line per test binary). No match =
                                  # the suite did not provably run.
    fail-pattern = 'test (\S+) \.\.\. FAILED'   # optional: 1 group naming a killer
    timeout-secs = 1800           # optional; the suite's process group is killed

    [[mutants]]
    name = "M01 guard inverted"
    file = "src/lib.rs"           # relative to root
    target = "if !ok {"           # must occur EXACTLY once in the file
    replacement = "if ok {"

VERDICTS
    KILLED         suite ran and failed (failing tally, or non-zero exit with proof
                   present — the tally is trusted over a lying wrapper exit code,
                   and vice versa)
    SURVIVED       suite ran green: a real coverage gap
    NO-RUN         no proof the suite ran (crash / compile error / timeout) —
                   unscorable, never "survived"
    HARNESS-ERROR  the mutant is invalid: target not found exactly once

INTEGRITY (enforced)
    A red, silent, or zero-test baseline aborts before any probe. Writes are
    atomic (temp + rename): no failure mode leaves a file truncated. Every
    restore is verified byte-exact, and each file is re-checked pristine before
    the next mutant. Suite output is capped per stream (oldest bytes dropped).
    One probe per tree, enforced by the lock below.

THE LOCK (<root>/.mutation-test/probe.lock)
    Created before the baseline, removed on exit. It records the owning pid and
    host, the start time, the root, the config, the suite command, the mutant
    count, and the mutant currently written to the tree — so a probe that
    outlives the agent that launched it is readable from the clone alone, and an
    unfinished pass is no longer indistinguishable from one that never started.

    Starting a probe against a locked tree:
      owner alive         abort, naming the owning pid and its current mutant
      owner dead, at the  reclaimed automatically: nothing was ever mutated
        baseline
      owner dead, past    abort — the tree may still hold that mutant. Verify
        the baseline      and restore it, then delete the lock and re-run
      other host          abort: this host cannot check that pid

    Deleting the lock is the override, and it is deliberately manual: the thing
    it is protecting is whether the source on disk is the source you think.

EXIT CODES
    0  baseline green and every probed mutant KILLED
    1  the pass ran; something SURVIVED, was NO-RUN, or was a HARNESS-ERROR
    2  the pass could not run or be trusted (config error, red baseline,
       restore failure, another probe owns the tree)
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
    let proof = regex::Regex::new(&cfg.suite.proof)
        .unwrap_or_else(|e| fail(&format!("suite.proof is not a valid regex: {e}")));
    if proof.captures_len() < 3 {
        fail("suite.proof needs two capture groups: (passed) and (failed)");
    }
    let fail_pattern = cfg.suite.fail_pattern.as_deref().map(|p| {
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

    // Claim the tree before reading a single byte of it: everything below assumes the
    // only writer is this process.
    let lock = lock_path(&root);
    let mut record = LockRecord {
        pid: std::process::id(),
        host: hostname(),
        started_unix: now_unix(),
        root: std::fs::canonicalize(&root)
            .unwrap_or_else(|_| root.clone())
            .display()
            .to_string(),
        config: std::fs::canonicalize(&config_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&config_path))
            .display()
            .to_string(),
        command: cfg.suite.command.clone(),
        mutants: selected.len(),
        phase: Phase::Baseline,
    };
    acquire_lock(&lock, &record).unwrap_or_else(|e| fail(&e));
    println!("lock: {} (pid {})", lock.display(), record.pid);

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
    let (base_passed, base_failed) = match &baseline {
        SuiteOutcome::Ran { passed, failed, .. } => (*passed, *failed),
        _ => unreachable!("baseline_defect rejects non-Ran outcomes"),
    };
    println!("baseline: green ({base_passed} passed)");

    let mut reports: Vec<MutantReport> = Vec::new();
    for (i, m) in selected.iter().enumerate() {
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
            // Publish the mutant to the lock BEFORE it reaches the tree. A run killed
            // between these two writes is the case the successor must not misread, and
            // the only safe direction is for the lock to be early rather than late.
            record.phase = Phase::Probing {
                mutant: m.name.clone(),
                file: m.file.clone(),
                index: i + 1,
            };
            update_lock(&record).unwrap_or_else(|e| fail(&format!("cannot update lock: {e}")));
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
    release_lock();
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

    fn lock_record(phase: Phase) -> LockRecord {
        LockRecord {
            pid: 4242,
            host: "box".to_string(),
            started_unix: 1_000,
            root: "/clones/repo".to_string(),
            config: "/clones/repo/mutants.toml".to_string(),
            command: vec!["cargo".to_string(), "test".to_string()],
            mutants: 95,
            phase,
        }
    }

    fn probing() -> Phase {
        Phase::Probing {
            mutant: "M26 guard inverted".to_string(),
            file: "src/lib/LibCodeGen.sol".to_string(),
            index: 26,
        }
    }

    #[test]
    fn a_live_owner_is_never_reclaimed() {
        // The incident: an orphan probe still mutating a clone while a successor was
        // dispatched to re-run the same pass against it.
        for phase in [Phase::Baseline, probing()] {
            let found = lock_record(phase);
            match lock_decision(&found, Liveness::Alive, 8_200) {
                LockDecision::Abort(reason) => {
                    assert!(reason.contains("4242"), "names the owning pid: {reason}");
                    assert!(reason.contains("7200s ago"), "names its age: {reason}");
                }
                other => panic!("a live owner must abort, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_dead_owner_that_had_a_mutant_applied_is_never_reclaimed() {
        // The tree may still hold that mutant; probing it scores every verdict against
        // source nobody wrote.
        match lock_decision(&lock_record(probing()), Liveness::Dead, 1_100) {
            LockDecision::Abort(reason) => {
                assert!(
                    reason.contains("M26 guard inverted") && reason.contains("LibCodeGen.sol"),
                    "names the mutant still on disk and its file: {reason}"
                );
            }
            other => panic!("a dead owner mid-mutation must abort, got {other:?}"),
        }
    }

    #[test]
    fn a_dead_owner_still_at_the_baseline_is_reclaimed() {
        // Nothing was ever applied, so the tree is the tree this pass was given —
        // the one case where overwriting the lock destroys no evidence.
        match lock_decision(&lock_record(Phase::Baseline), Liveness::Dead, 1_100) {
            LockDecision::Reclaim(note) => assert!(note.contains("4242"), "{note}"),
            other => panic!("a stale baseline lock must be reclaimed, got {other:?}"),
        }
    }

    #[test]
    fn an_owner_this_host_cannot_check_is_never_reclaimed() {
        match lock_decision(&lock_record(Phase::Baseline), Liveness::Unknown, 1_100) {
            LockDecision::Abort(reason) => assert!(reason.contains("cannot check"), "{reason}"),
            other => panic!("an uncheckable owner must abort, got {other:?}"),
        }
    }

    #[test]
    fn liveness_is_only_readable_on_the_recording_host() {
        let here = hostname();
        assert_eq!(
            process_liveness(&here, &here, std::process::id()),
            Liveness::Alive,
            "this process is running"
        );
        assert_eq!(
            process_liveness("some-other-box", &here, std::process::id()),
            Liveness::Unknown,
            "a pid from another host says nothing here, even when it exists locally"
        );
        assert_eq!(
            process_liveness(&here, &here, 2_000_000_000),
            Liveness::Dead,
            "a pid past any allocator's range is gone"
        );
        assert_eq!(
            process_liveness(&here, &here, 0),
            Liveness::Unknown,
            "pid 0 is not a process this can check"
        );
    }

    #[test]
    fn a_lock_cannot_be_claimed_twice() {
        let dir = std::env::temp_dir().join(format!("mp-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOCK_FILE);
        let owner = lock_record(Phase::Baseline);

        create_lock_exclusively(&path, &owner).expect("the first claim wins");
        let second = create_lock_exclusively(&path, &lock_record(probing()))
            .expect_err("the second claim must not win");
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            serialize_lock(&owner),
            "and must not overwrite the owner's record"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "neither claim leaves a staged file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_round_trips_through_toml() {
        // A successor reads this file with nothing but the clone path, so the write
        // and the read have to agree without it.
        for phase in [Phase::Baseline, probing()] {
            let record = lock_record(phase);
            let text = serialize_lock(&record);
            let parsed: LockRecord = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("the lock must parse: {e}\n{text}"));
            assert_eq!(parsed, record);
        }
        assert!(
            serialize_lock(&lock_record(probing())).contains("LibCodeGen.sol"),
            "the applied mutant's file is readable in the file itself"
        );
    }

    #[test]
    fn tail_keeps_the_end_and_flattens_newlines() {
        assert_eq!(tail("abc\ndef", 4), " def");
        assert_eq!(tail("ab", 4), "ab");
    }
}
