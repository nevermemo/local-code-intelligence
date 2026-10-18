//! `cargo xtask sim-structured`: the structured-config measurement. It runs
//! the simulated agent of `sim` (grep, read, follow names; no model, no gold
//! in the loop) over the questions of the structured gold files, scores each
//! trace against its family (`score`), and compares two such runs (`analyze`).
//! Preregistration: `docs/spike/multimap/structured-grep-prereg.md`.
//!
//! * `run`: one arm x one policy x one stratum, written to
//!   `<out-dir>/<arm>-<grep-skip>-<policy>/<stratum>/`: `questions/<id>.json`
//!   (the question, its family, the trace, the score), `summary.json` (the
//!   settings, the score rows, the aggregates, the scan totals; no times, so
//!   two identical runs give identical files) and `meta.json` (the hash of
//!   this binary, `git describe`, UTC times; no host name, no paths).
//! * `compare`: paired bootstrap of B against A over run dirs.
//! * `check-probes`: the probe-term fix of the preregistration, checked.
//!
//! Arms: `a` and `b` are the in-process scan; the grep-skip switch
//! (`none` or `lockfiles`) is the only thing that differs between them, and
//! one binary runs both. `r` replaces the scan with ripgrep (`sim::rg`).
//! The agent is given the question text verbatim. Nothing of the gold family
//! reaches the loop (no stop-at-gold, no opening call).
//!
//! `run` refuses to start when the gold could be searched: a `--gold-dir`
//! inside any `--tree`, a tree holding `evaluations/structured/*.json`, or a
//! tree with a copy of a gold file at its root or in `evaluations/`. A clean
//! export of the tree is the way out; `meta.json` records `gold_outside_trees`.
//!
//! Every arm of a measurement must use the same `--max-scan-bytes`: skipped
//! lockfiles do not count against the cap, so a binding cap would confound
//! the arms. A scan that stops on the cap or the deadline voids the run: the
//! files are written and the command fails.

mod analyze;
mod gold;
mod score;

use super::follow::Policy;
use super::reject_run_dir_in_tree;
use super::rg::{ARGS_BEFORE_NEEDLE, RgCommand};
use super::sim_loop::{SimConfig, run_question};
use super::terms::{Backend, ScanTotals, Searcher, pick_probe_terms, scan_options};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use lci_core::lexical::query_terms;
use lci_maps::live_grep::LOCKFILES;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The bytes one scan walk may read before it stops: far above a tree, so the
/// cap never binds (a binding cap confounds the arms, see the module docs).
const DEFAULT_MAX_SCAN_BYTES: u64 = 4 << 30;
/// The seed of the bootstrap when `compare` is not given one.
const DEFAULT_BOOTSTRAP_SEED: u64 = 20_261_018;

#[derive(Subcommand)]
pub enum StructuredCommand {
    /// Run the simulated agent over the questions of one stratum and score
    /// them. Fails (after writing the run dir) when any scan stopped early.
    Run(RunArgs),
    /// Paired comparison of B against A (and optional R as context) over run
    /// dirs written by `run`.
    Compare(CompareArgs),
    /// Check that every question that names its key probes that key's last
    /// segment as a term (the probe-term fix of the preregistration).
    CheckProbes(ProbeArgs),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ArmArg {
    /// The in-process scan, first arm of the comparison.
    A,
    /// The in-process scan, second arm of the comparison.
    B,
    /// An external ripgrep behind the same agent (context only).
    R,
}

impl ArmArg {
    fn slug(self) -> &'static str {
        match self {
            ArmArg::A => "a",
            ArmArg::B => "b",
            ArmArg::R => "r",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum GrepSkip {
    /// Skip no file by name.
    #[value(name = "none")]
    Off,
    /// Skip the lockfiles of `lci_maps::live_grep::LOCKFILES`.
    Lockfiles,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum StratumArg {
    Config,
    Dependency,
}

impl StratumArg {
    fn name(self) -> &'static str {
        match self {
            StratumArg::Config => "config",
            StratumArg::Dependency => "dependency",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SplitArg {
    Dev,
    Heldout,
    All,
}

impl SplitArg {
    fn name(self) -> &'static str {
        match self {
            SplitArg::Dev => "dev",
            SplitArg::Heldout => "heldout",
            SplitArg::All => "all",
        }
    }

    fn keeps(self, split: &str) -> bool {
        self == SplitArg::All || self.name() == split
    }
}

#[derive(Args)]
pub struct RunArgs {
    /// Directory of the structured gold files, `<repo>.json` (private; no
    /// default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// A checked-out tree, `<repo>=<dir>`; repeat per repository.
    #[arg(long = "tree", required = true)]
    trees: Vec<String>,
    /// The arm: `a` and `b` scan in process, `r` runs ripgrep.
    #[arg(long, value_enum)]
    arm: ArmArg,
    /// Files the scan skips by name (arms a and b; required there; refused
    /// with `r`).
    #[arg(long, value_enum)]
    grep_skip: Option<GrepSkip>,
    /// The ripgrep executable (arm r only; use a frozen copy). Its version
    /// and SHA-256 are recorded.
    #[arg(long)]
    rg: Option<PathBuf>,
    /// Frontier order: `greedy` or `bfs`.
    #[arg(long, value_enum, default_value = "greedy")]
    policy: Policy,
    /// The stratum of the families to run.
    #[arg(long, value_enum)]
    stratum: StratumArg,
    /// The split of the families to run. Dev is a shakedown only.
    #[arg(long, value_enum, default_value = "all")]
    split: SplitArg,
    /// Tool calls allowed per question.
    #[arg(long, default_value_t = 25)]
    max_calls: u32,
    /// Tokens of tool output allowed per question (characters / 4).
    #[arg(long, default_value_t = 20_000)]
    max_tokens: u64,
    /// Query terms probed (the longest ones), one call each. 8 is the
    /// registered value: with 6, short package names lose the tie.
    #[arg(long, default_value_t = 8)]
    probe_terms: usize,
    /// Files read after the first search.
    #[arg(long, default_value_t = 3)]
    read_files: usize,
    /// Bytes a scan may read before it stops (the run is then void). The
    /// same for every arm of a measurement; recorded.
    #[arg(long, default_value_t = DEFAULT_MAX_SCAN_BYTES)]
    max_scan_bytes: u64,
    /// Where the run dir is written. Refused inside any `--tree`.
    #[arg(long)]
    out_dir: PathBuf,
    /// Run only the first N questions.
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Args)]
pub struct CompareArgs {
    /// One paired comparison, `<label>=<A run dir>,<B run dir>`; repeat per
    /// policy and stratum. The label is passed to the analysis verbatim.
    #[arg(long = "pair", required = true)]
    pairs: Vec<String>,
    /// A run shown as context only (the R arm), `<label>=<run dir>`.
    #[arg(long = "context")]
    context: Vec<String>,
    /// The split compared (`heldout` decides; `dev` is a shakedown).
    #[arg(long, value_enum, default_value = "heldout")]
    split: SplitArg,
    /// Seed of the bootstrap's SplitMix64; recorded in the output.
    #[arg(long, default_value_t = DEFAULT_BOOTSTRAP_SEED)]
    seed: u64,
    /// Where `compare.json` and `compare.md` are written.
    #[arg(long)]
    out_dir: PathBuf,
}

#[derive(Args)]
pub struct ProbeArgs {
    /// Directory of the structured gold files.
    #[arg(long)]
    gold_dir: PathBuf,
    /// The `--probe-terms` the runs will use.
    #[arg(long, default_value_t = 8)]
    probe_terms: usize,
}

pub fn run(which: StructuredCommand) -> Result<()> {
    match which {
        StructuredCommand::Run(args) => run_questions(&args),
        StructuredCommand::Compare(args) => compare_runs(&args),
        StructuredCommand::CheckProbes(args) => check_probes(&args),
    }
}

// ------------------------------------------------------------------ matcher

/// What the run searches with, checked and identified.
enum Matcher {
    Live {
        skip: GrepSkip,
        max_scan_bytes: u64,
    },
    Rg {
        command: RgCommand,
        identity: RgIdentity,
    },
}

/// Which `rg` ran: the first line of `--version` and the hash of the file.
struct RgIdentity {
    version: String,
    sha256: String,
}

impl Matcher {
    /// The skip-list part of the run dir's name.
    fn label(&self) -> &'static str {
        match self {
            Matcher::Live {
                skip: GrepSkip::Off,
                ..
            } => "none",
            Matcher::Live {
                skip: GrepSkip::Lockfiles,
                ..
            } => "lockfiles",
            Matcher::Rg { .. } => "rg",
        }
    }

    fn backend(&self) -> Backend {
        match self {
            Matcher::Live {
                skip,
                max_scan_bytes,
            } => {
                let mut options = scan_options();
                options.max_scan_bytes = *max_scan_bytes;
                options.skip_files = match skip {
                    GrepSkip::Off => Vec::new(),
                    GrepSkip::Lockfiles => LOCKFILES.iter().map(|s| (*s).to_owned()).collect(),
                };
                Backend::Live(options)
            }
            Matcher::Rg { command, .. } => Backend::Rg(command.clone()),
        }
    }

    fn describe(&self) -> Value {
        match self {
            Matcher::Live { .. } => {
                let Backend::Live(options) = self.backend() else {
                    unreachable!("a live matcher builds a live backend")
                };
                json!({
                    "kind": "live_grep",
                    "skip_files": options.skip_files,
                    "skip_dirs": options.skip_dirs,
                    "max_file_bytes": options.max_file_bytes,
                    "max_scan_bytes": options.max_scan_bytes,
                    "min_name_len": options.min_name_len,
                    "max_matches": "unlimited",
                })
            }
            Matcher::Rg { identity, .. } => {
                let mut argv: Vec<&str> = ARGS_BEFORE_NEEDLE.to_vec();
                argv.extend(["<needle>", "."]);
                json!({
                    "kind": "rg",
                    "version": identity.version,
                    "sha256": identity.sha256,
                    "argv": argv,
                })
            }
        }
    }
}

/// Checks the arm's switches against each other and identifies `rg`.
fn resolve_matcher(args: &RunArgs) -> Result<Matcher> {
    match args.arm {
        ArmArg::R => {
            if args.grep_skip.is_some() {
                bail!("--grep-skip does not apply to --arm r: rg has no skip list");
            }
            let Some(exe) = &args.rg else {
                bail!("--arm r needs --rg <executable>");
            };
            if !exe.is_file() {
                bail!("--rg {} is not a file", exe.display());
            }
            Ok(Matcher::Rg {
                command: RgCommand::new(exe.clone()),
                identity: rg_identity(exe)?,
            })
        }
        ArmArg::A | ArmArg::B => {
            if args.rg.is_some() {
                bail!("--rg only applies to --arm r");
            }
            let Some(skip) = args.grep_skip else {
                bail!("--arm a and --arm b need --grep-skip none|lockfiles");
            };
            Ok(Matcher::Live {
                skip,
                max_scan_bytes: args.max_scan_bytes,
            })
        }
    }
}

fn rg_identity(exe: &Path) -> Result<RgIdentity> {
    let bytes = std::fs::read(exe).with_context(|| format!("read {}", exe.display()))?;
    let output = Command::new(exe)
        .arg("--version")
        .output()
        .context("run rg --version")?;
    if !output.status.success() {
        bail!("rg --version failed ({})", output.status);
    }
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    Ok(RgIdentity {
        version,
        sha256: hex(&Sha256::digest(&bytes)),
    })
}

// --------------------------------------------------------------------- run

/// One question to run, with what it belongs to.
struct Picked<'a> {
    repo: &'a gold::Repo,
    family: &'a gold::Family,
    question: &'a gold::Question,
}

/// The questions of the stratum and split, in file order, cut to `--limit`.
fn select<'a>(repos: &'a [gold::Repo], args: &RunArgs) -> Result<Vec<Picked<'a>>> {
    let mut picked = Vec::new();
    for repo in repos {
        let families: HashMap<&str, &gold::Family> =
            repo.families.iter().map(|f| (f.id.as_str(), f)).collect();
        for question in &repo.questions {
            let Some(&family) = families.get(question.family.as_str()) else {
                bail!(
                    "question {} names the unknown family {}",
                    question.id,
                    question.family
                );
            };
            if family.stratum == args.stratum.name() && args.split.keeps(&family.split) {
                picked.push(Picked {
                    repo,
                    family,
                    question,
                });
            }
        }
    }
    if let Some(limit) = args.limit {
        picked.truncate(limit);
    }
    Ok(picked)
}

/// SHA-256 of `bytes` with CRLF line ends read as LF (a copy checked out on
/// Windows is still a copy).
fn lf_sha256(bytes: &[u8]) -> String {
    let mut text = Vec::with_capacity(bytes.len());
    for (i, &byte) in bytes.iter().enumerate() {
        if byte != b'\r' || bytes.get(i + 1) != Some(&b'\n') {
            text.push(byte);
        }
    }
    hex(&Sha256::digest(&text))
}

/// The `*.json` files directly in `dir`; none when it is not a directory.
fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
}

/// The gold must not be searchable text: its keys and question texts would
/// then be found by the agent it is meant to score. Refuses a gold dir inside
/// any tree, a tree that holds `evaluations/structured/*.json` or
/// `evaluations/structured/dependency/*.json` (the layout of a working tree of
/// this repository: use a clean export), and a tree with a
/// byte-for-byte copy (line ends aside) of a gold file at its root, in
/// `evaluations/` or in `evaluations/structured/`.
fn check_gold_outside_trees(gold_dir: &Path, trees: &BTreeMap<String, PathBuf>) -> Result<()> {
    let mut gold_hashes: BTreeMap<String, PathBuf> = BTreeMap::new();
    for file in json_files(gold_dir) {
        let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
        gold_hashes.insert(lf_sha256(&bytes), file);
    }
    for (repo, tree) in trees {
        if reject_run_dir_in_tree(tree, gold_dir).is_err() {
            bail!(
                "--gold-dir is inside --tree {repo}: its files would be searched as source; keep the gold outside every tree"
            );
        }
        let structured = tree.join("evaluations").join("structured");
        if !json_files(&structured).is_empty() {
            bail!(
                "--tree {repo} holds evaluations/structured/*.json: the gold's keys and questions would be searched text; use a clean export of the tree"
            );
        }
        // The dependency gold lives one level down in a working tree; any json
        // there is refused, so a copy of a gold file is too.
        if !json_files(&structured.join("dependency")).is_empty() {
            bail!(
                "--tree {repo} holds evaluations/structured/dependency/*.json: the gold's keys and questions would be searched text; use a clean export of the tree"
            );
        }
        for dir in [tree.to_path_buf(), tree.join("evaluations")] {
            for file in json_files(&dir) {
                let Ok(bytes) = std::fs::read(&file) else {
                    continue;
                };
                if let Some(gold) = gold_hashes.get(&lf_sha256(&bytes)) {
                    bail!(
                        "--tree {repo} holds {}, a copy of the gold file {}; use a clean export of the tree",
                        file.strip_prefix(tree).unwrap_or(&file).display(),
                        gold.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }
    }
    Ok(())
}

/// `--tree <repo>=<dir>` arguments to a map (as `gold structured` reads them).
fn parse_trees(specs: &[String]) -> Result<BTreeMap<String, PathBuf>> {
    let mut trees = BTreeMap::new();
    for spec in specs {
        let Some((repo, dir)) = spec
            .split_once('=')
            .filter(|(r, d)| !r.is_empty() && !d.is_empty())
        else {
            bail!("--tree must look like <repo>=<dir>, got {spec:?}");
        };
        if trees.insert(repo.to_owned(), PathBuf::from(dir)).is_some() {
            bail!("--tree {repo} is given twice");
        }
    }
    Ok(trees)
}

fn run_questions(args: &RunArgs) -> Result<()> {
    let matcher = resolve_matcher(args)?;
    let trees = parse_trees(&args.trees)?;
    let dir = args
        .out_dir
        .join(format!(
            "{}-{}-{}",
            args.arm.slug(),
            matcher.label(),
            args.policy.name()
        ))
        .join(args.stratum.name());
    for tree in trees.values() {
        reject_run_dir_in_tree(tree, &dir)?;
    }
    check_gold_outside_trees(&args.gold_dir, &trees)?;
    if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
        bail!(
            "the run dir {} is not empty; use a fresh --out-dir (a rerun must not mix files)",
            dir.display()
        );
    }

    let repos = gold::load_dir(&args.gold_dir)?;
    let picked = select(&repos, args)?;
    if picked.is_empty() {
        bail!(
            "no question of stratum {} and split {} in the gold files",
            args.stratum.name(),
            args.split.name()
        );
    }
    let mut used: Vec<&str> = picked.iter().map(|p| p.repo.repo.as_str()).collect();
    used.dedup();
    for repo in &used {
        match trees.get(*repo) {
            Some(tree) if tree.is_dir() => {}
            Some(_) => bail!("--tree {repo} is not a directory"),
            None => bail!("no --tree <repo>=<dir> for the repository {repo}"),
        }
    }

    let started = utc_now();
    let cfg = SimConfig {
        policy: args.policy,
        max_calls: args.max_calls,
        max_tokens: args.max_tokens,
        probe_terms: args.probe_terms,
        read_files: args.read_files,
        stop_at_gold: false,
        ..SimConfig::default()
    };
    std::fs::create_dir_all(dir.join("questions"))
        .with_context(|| format!("create {}", dir.display()))?;

    let mut rows: Vec<score::ScoreRow> = Vec::new();
    let mut totals = ScanTotals::default();
    let mut incomplete: u64 = 0;
    let mut by_repo = serde_json::Map::new();
    for repo in &repos {
        let mine: Vec<&Picked> = picked.iter().filter(|p| p.repo.repo == repo.repo).collect();
        if mine.is_empty() {
            continue;
        }
        // One searcher per repository: its root is the tree, and its caches
        // only save work.
        let mut searcher = Searcher::with(&trees[&repo.repo], matcher.backend());
        for (done, pick) in mine.iter().enumerate() {
            let (question, family) = (pick.question, pick.family);
            let before = searcher.incomplete_scans;
            let trace = run_question(&mut searcher, &cfg, &question.text, None, None);
            if let Some(why) = &searcher.failure {
                bail!("question {}: {why}", question.id);
            }
            let result = score::score_question(question, family, &trace);
            let record = json!({
                "question": {
                    "id": question.id, "type": question.qtype, "text": question.text,
                    "names_key": question.names_key,
                },
                "family": {
                    "id": family.id, "key": family.key, "stratum": family.stratum,
                    "split": family.split, "readers_scope": family.readers_scope,
                },
                "repo": repo.repo,
                "split": family.split,
                "incomplete_scans": searcher.incomplete_scans - before,
                "trace": trace.to_json(),
                "score": serde_json::to_value(&result)?,
            });
            write_json(
                &dir.join("questions")
                    .join(format!("{}.json", file_stem(&question.id))),
                &record,
            )?;
            rows.push(score::ScoreRow {
                question_id: question.id.clone(),
                family: family.id.clone(),
                repo: repo.repo.clone(),
                stratum: family.stratum.clone(),
                split: family.split.clone(),
                qtype: question.qtype.clone(),
                score: result,
            });
            if (done + 1) % 10 == 0 {
                eprintln!(
                    "sim-structured: {} {}/{} done",
                    repo.repo,
                    done + 1,
                    mine.len()
                );
            }
        }
        totals.add(&searcher.totals);
        incomplete += u64::from(searcher.incomplete_scans);
        by_repo.insert(
            repo.repo.clone(),
            json!({
                "questions": mine.len(),
                "incomplete_scans": searcher.incomplete_scans,
                "scan_totals": totals_json(&searcher.totals),
            }),
        );
    }

    let summary = json!({
        "version": 1,
        "arm": args.arm.slug(),
        "grep_skip": matcher.label(),
        "policy": args.policy.name(),
        "stratum": args.stratum.name(),
        "split_filter": args.split.name(),
        "limit": args.limit,
        "config": cfg.to_json(),
        "matcher": matcher.describe(),
        "trees": used,
        "questions": rows.len(),
        "incomplete_scans": incomplete,
        "scan_totals": totals_json(&totals),
        "by_repo": by_repo,
        "rows": serde_json::to_value(&rows)?,
        "aggregates": score::aggregate(&rows),
    });
    write_json(&dir.join("summary.json"), &summary)?;

    let gold_sha256: BTreeMap<&str, Option<String>> = used
        .iter()
        .map(|repo| {
            let file = args.gold_dir.join(format!("{repo}.json"));
            (
                *repo,
                std::fs::read(file).ok().map(|b| hex(&Sha256::digest(&b))),
            )
        })
        .collect();
    let exe = std::env::current_exe().context("locate this executable")?;
    let meta = json!({
        "command": "sim-structured run",
        "xtask_sha256": hex(&Sha256::digest(
            std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?
        )),
        "git_describe": git_describe(),
        "started_utc": started,
        "finished_utc": utc_now(),
        "gold_sha256": gold_sha256,
        // Checked before the run: no tree holds the gold (see
        // `check_gold_outside_trees`); a run that fails the check writes nothing.
        "gold_outside_trees": true,
        "tool": matcher.describe(),
    });
    write_json(&dir.join("meta.json"), &meta)?;

    eprintln!(
        "sim-structured {}-{}-{} {}: {} questions, {} files read, {} skipped by name, {} incomplete scans\nrun dir: {}",
        args.arm.slug(),
        matcher.label(),
        args.policy.name(),
        args.stratum.name(),
        rows.len(),
        totals.files_read,
        totals.skipped_by_name,
        incomplete,
        dir.display()
    );
    if incomplete > 0 {
        bail!(
            "{incomplete} scans stopped on a deadline or the byte cap: the run may not repeat and is void (raise --max-scan-bytes for every arm)"
        );
    }
    Ok(())
}

fn totals_json(t: &ScanTotals) -> Value {
    json!({
        "scans": t.scans,
        "skipped_by_name": t.skipped_by_name,
        "files_read": t.files_read,
        "bytes_read": t.bytes_read,
        "stopped": t.stopped,
        "rg_stdout_bytes": t.rg_stdout_bytes,
        "rg_stdout_lines": t.rg_stdout_lines,
        "rg_ignored_lines": t.rg_ignored_lines,
    })
}

/// A question id as a file name: letters, digits and `._-` kept.
fn file_stem(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `git describe` of the checkout this binary was built from; none when git
/// or that checkout is not there.
fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            env!("CARGO_MANIFEST_DIR"),
            "describe",
            "--always",
            "--dirty",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    utc_iso(secs)
}

/// Seconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ` (civil date from
/// days, proleptic Gregorian).
fn utc_iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ----------------------------------------------------------------- compare

fn compare_runs(args: &CompareArgs) -> Result<()> {
    let mut pairs = Vec::new();
    for spec in &args.pairs {
        let (label, dirs) = split_label(spec, "--pair")?;
        let Some((a, b)) = dirs
            .split_once(',')
            .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        else {
            bail!("--pair must look like <label>=<A dir>,<B dir>, got {spec:?}");
        };
        pairs.push((
            label,
            analyze::load_run(Path::new(a)).with_context(|| format!("load A run for {spec:?}"))?,
            analyze::load_run(Path::new(b)).with_context(|| format!("load B run for {spec:?}"))?,
        ));
    }
    let mut context = Vec::new();
    for spec in &args.context {
        let (label, dir) = split_label(spec, "--context")?;
        let run = analyze::load_run(Path::new(dir))
            .with_context(|| format!("load context run for {spec:?}"))?;
        context.push((label, run));
    }
    let context = (!context.is_empty()).then_some(context.as_slice());
    let comparison = analyze::compare(&pairs, context, args.split.name(), args.seed)?;
    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create {}", args.out_dir.display()))?;
    write_json(
        &args.out_dir.join("compare.json"),
        &serde_json::to_value(&comparison)?,
    )?;
    let markdown = comparison.to_markdown();
    std::fs::write(args.out_dir.join("compare.md"), &markdown)
        .with_context(|| "write compare.md")?;
    println!("{markdown}");
    Ok(())
}

/// `label=rest` to `(label, rest)`.
fn split_label<'a>(spec: &'a str, flag: &str) -> Result<(String, &'a str)> {
    match spec.split_once('=') {
        Some((label, rest)) if !label.is_empty() && !rest.is_empty() => {
            Ok((label.to_owned(), rest))
        }
        _ => bail!("{flag} must start with <label>=, got {spec:?}"),
    }
}

// ------------------------------------------------------------ check-probes

/// For each question that names its key: the last segment of the family key,
/// lower-cased as `query_terms` lower-cases, must be among the terms the
/// agent probes. A question that does not name its key has nothing to check.
fn probe_failures(repos: &[gold::Repo], probe_terms: usize) -> (usize, usize, Vec<String>) {
    let (mut checked, mut skipped, mut failures) = (0, 0, Vec::new());
    for repo in repos {
        let keys: HashMap<&str, &str> = repo
            .families
            .iter()
            .map(|f| (f.id.as_str(), f.key.as_str()))
            .collect();
        for question in &repo.questions {
            let Some(&key) = keys.get(question.family.as_str()) else {
                failures.push(format!(
                    "{}: names the unknown family {}",
                    question.id, question.family
                ));
                continue;
            };
            if !question.names_key {
                skipped += 1;
                continue;
            }
            checked += 1;
            let segment = key.rsplit('.').next().unwrap_or(key).to_lowercase();
            let picked = pick_probe_terms(&query_terms(&question.text), probe_terms);
            if !picked.contains(&segment) {
                failures.push(format!(
                    "{}: key segment `{segment}` is not among the {probe_terms} probed terms {picked:?}",
                    question.id
                ));
            }
        }
    }
    (checked, skipped, failures)
}

fn check_probes(args: &ProbeArgs) -> Result<()> {
    let repos = gold::load_dir(&args.gold_dir)?;
    let (checked, skipped, failures) = probe_failures(&repos, args.probe_terms);
    for failure in &failures {
        eprintln!("FAIL {failure}");
    }
    eprintln!(
        "check-probes: {checked} questions checked, {skipped} do not name their key, {} failures (--probe-terms {})",
        failures.len(),
        args.probe_terms
    );
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "check-probes: {} question(s) would not probe their key",
            failures.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CONFIG: &str = "[s]\nlisten = 1\nother = 2\n";
    const MAIN: &str = "fn main() {\n    let a = cfg.listen;\n}\n";

    fn temp(tag: &str, what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xtask-sim-structured-{what}-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha(text: &str) -> String {
        hex(&Sha256::digest(text.as_bytes()))
    }

    /// One config family `r.f0` (key `s.listen`) with its three questions;
    /// `q1` is the text of the first.
    fn gold_json(q1: &str) -> Value {
        let span = |path: &str, line: u32, text: &str| json!({"path": path, "line": line, "end_line": line, "sha256": sha(text)});
        let mut assignment = span("cfg/a.toml", 2, CONFIG);
        assignment["kind"] = json!("assignment");
        assignment["value"] = json!("1");
        let mut reader = span("src/main.rs", 2, MAIN);
        reader["how"] = json!("literal");
        let questions = [
            ("q1", "setting_location", q1, true),
            (
                "q2",
                "behaviour_to_setting",
                "Which setting chooses the network address it serves on?",
                false,
            ),
            ("q3", "setting_to_reader", "What reads `s.listen`?", true),
        ];
        let questions: Vec<Value> = questions
            .iter()
            .map(|(n, ty, text, names)| {
                json!({
                    "id": format!("r.f0.{n}"), "family": "r.f0", "type": ty,
                    "text": text, "names_key": names,
                })
            })
            .collect();
        let revision = "a".repeat(40);
        json!({
            "version": 1, "repo": "r", "revision": revision, "seed": 20_261_018,
            "families": [{
                "id": "r.f0", "stratum": "config", "split": "dev", "format": "toml",
                "key": "s.listen", "assignments": [assignment], "readers": [reader],
                "readers_scope": "component:src/main.rs::main", "excluded_readers": [],
                "behaviour": "Chooses the network address it serves on.", "docs": [],
            }],
            "questions": questions,
        })
    }

    /// A gold dir, a tree (a config file, a source file that reads it and a
    /// lockfile that does not hold the needle) and an out dir, all apart.
    fn fixture(tag: &str, q1: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (gold, tree, out) = (temp(tag, "gold"), temp(tag, "tree"), temp(tag, "out"));
        fs::write(
            gold.join("r.json"),
            serde_json::to_string_pretty(&gold_json(q1)).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(tree.join("cfg")).unwrap();
        fs::create_dir_all(tree.join("src")).unwrap();
        fs::write(tree.join("cfg/a.toml"), CONFIG).unwrap();
        fs::write(tree.join("src/main.rs"), MAIN).unwrap();
        // No name the agent can follow from the other files, so A never lists it either.
        fs::write(tree.join("Cargo.lock"), "lockonly_token = 1\n").unwrap();
        (gold, tree, out)
    }

    fn cleanup(dirs: &[&Path]) {
        for dir in dirs {
            let _ = fs::remove_dir_all(dir);
        }
    }

    fn run_args(
        gold: &Path,
        tree: &Path,
        out: &Path,
        arm: ArmArg,
        skip: Option<GrepSkip>,
    ) -> RunArgs {
        RunArgs {
            gold_dir: gold.to_path_buf(),
            trees: vec![format!("r={}", tree.display())],
            arm,
            grep_skip: skip,
            rg: None,
            policy: Policy::Greedy,
            stratum: StratumArg::Config,
            split: SplitArg::All,
            max_calls: 25,
            max_tokens: 20_000,
            probe_terms: 8,
            read_files: 3,
            max_scan_bytes: DEFAULT_MAX_SCAN_BYTES,
            out_dir: out.to_path_buf(),
            limit: None,
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The `questions/*.json` of a run dir, by file name.
    fn question_files(dir: &Path) -> Vec<(String, String)> {
        let mut files: Vec<(String, String)> = fs::read_dir(dir.join("questions"))
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    read(&path),
                )
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn two_identical_runs_write_equal_results() {
        let (gold, tree, out) = fixture("twice", "Where is `s.listen` set?");
        for sub in ["one", "two"] {
            let args = run_args(&gold, &tree, &out.join(sub), ArmArg::A, Some(GrepSkip::Off));
            run_questions(&args).unwrap();
        }
        let dir = |sub: &str| out.join(sub).join("a-none-greedy").join("config");
        let (one, two) = (dir("one"), dir("two"));
        let questions = question_files(&one);
        assert_eq!(questions.len(), 3);
        assert_eq!(questions, question_files(&two));
        assert_eq!(
            read(&one.join("summary.json")),
            read(&two.join("summary.json"))
        );
        // Neither the results nor the meta name a path of this machine.
        for text in [
            read(&one.join("summary.json")),
            read(&one.join("meta.json")),
        ]
        .iter()
        .chain(questions.iter().map(|(_, t)| t))
        {
            assert!(!text.contains("xtask-sim-structured"), "{text}");
        }
        let meta: Value = serde_json::from_str(&read(&one.join("meta.json"))).unwrap();
        assert_eq!(meta["xtask_sha256"].as_str().map(str::len), Some(64));
        assert!(meta["started_utc"].as_str().unwrap().ends_with('Z'));
        let summary: Value = serde_json::from_str(&read(&one.join("summary.json"))).unwrap();
        assert_eq!(summary["trees"], json!(["r"]));
        assert_eq!(summary["rows"].as_array().map(Vec::len), Some(3));
        assert_eq!(summary["incomplete_scans"], 0);
        assert_eq!(summary["config"]["probe_terms"], 8);
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn skipping_lockfiles_that_do_not_hold_the_needle_changes_only_the_skip_count() {
        let (gold, tree, out) = fixture("lockfiles", "Where is `s.listen` set?");
        let a = run_args(&gold, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        let b = run_args(&gold, &tree, &out, ArmArg::B, Some(GrepSkip::Lockfiles));
        run_questions(&a).unwrap();
        run_questions(&b).unwrap();
        let (da, db) = (
            out.join("a-none-greedy").join("config"),
            out.join("b-lockfiles-greedy").join("config"),
        );
        assert_eq!(question_files(&da), question_files(&db));
        let (sa, sb): (Value, Value) = (
            serde_json::from_str(&read(&da.join("summary.json"))).unwrap(),
            serde_json::from_str(&read(&db.join("summary.json"))).unwrap(),
        );
        assert_eq!(sa["rows"], sb["rows"]);
        assert_eq!(sa["aggregates"], sb["aggregates"]);
        let (ta, tb) = (&sa["scan_totals"], &sb["scan_totals"]);
        assert_eq!(ta["skipped_by_name"], 0);
        let skipped = tb["skipped_by_name"].as_u64().unwrap();
        assert!(skipped > 0);
        // What B skipped by name, A read.
        assert_eq!(
            ta["files_read"].as_u64().unwrap(),
            tb["files_read"].as_u64().unwrap() + skipped
        );
        assert!(ta["bytes_read"].as_u64() > tb["bytes_read"].as_u64());
        assert_eq!(sa["matcher"]["skip_files"], json!([]));
        assert!(!sb["matcher"]["skip_files"].as_array().unwrap().is_empty());
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn a_run_dir_inside_a_tree_is_refused_and_nothing_is_written() {
        let (gold, tree, out) = fixture("in-tree", "Where is `s.listen` set?");
        let inside = tree.join("results");
        let args = run_args(&gold, &tree, &inside, ArmArg::A, Some(GrepSkip::Off));
        let err = run_questions(&args).unwrap_err().to_string();
        assert!(err.contains("inside --tree"), "{err}");
        assert!(!inside.exists());
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn a_gold_dir_inside_a_tree_is_refused_and_nothing_is_written() {
        let (gold, tree, out) = fixture("gold-in-tree", "Where is `s.listen` set?");
        let inside = tree.join("private-gold");
        fs::create_dir_all(&inside).unwrap();
        fs::copy(gold.join("r.json"), inside.join("r.json")).unwrap();
        let args = run_args(&inside, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        let err = run_questions(&args).unwrap_err().to_string();
        assert!(err.contains("--gold-dir is inside --tree r"), "{err}");
        assert!(!out.join("a-none-greedy").exists());
        // The tree itself as the gold dir is inside it too.
        let args = run_args(&tree, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        assert!(run_questions(&args).is_err());
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn a_tree_that_holds_the_gold_or_a_copy_of_it_is_refused() {
        let (gold, tree, out) = fixture("gold-copy", "Where is `s.listen` set?");
        let run = || {
            run_questions(&run_args(
                &gold,
                &tree,
                &out,
                ArmArg::A,
                Some(GrepSkip::Off),
            ))
            .map_err(|e| e.to_string())
        };
        // Another json file at the root or in evaluations/ is no copy.
        fs::write(tree.join("package.json"), "{\"name\": \"x\"}\n").unwrap();
        fs::create_dir_all(tree.join("evaluations")).unwrap();
        fs::write(tree.join("evaluations/other.json"), "{}\n").unwrap();
        run().unwrap();
        let meta: Value = serde_json::from_str(&read(
            &out.join("a-none-greedy").join("config").join("meta.json"),
        ))
        .unwrap();
        assert_eq!(meta["gold_outside_trees"], true);
        fs::remove_dir_all(out.join("a-none-greedy")).unwrap();

        // A copy with Windows line ends, anywhere the check looks.
        let text = read(&gold.join("r.json"));
        let crlf = text.replace('\n', "\r\n");
        assert_ne!(text, crlf);
        for place in ["copy.json", "evaluations/copy.json"] {
            fs::write(tree.join(place), &crlf).unwrap();
            let err = run().unwrap_err();
            assert!(
                err.contains("a copy of the gold file r.json") && err.contains("copy.json"),
                "{place}: {err}"
            );
            assert!(!out.join("a-none-greedy").exists());
            fs::remove_file(tree.join(place)).unwrap();
        }

        // The working tree layout: evaluations/structured/*.json, whatever it holds.
        fs::create_dir_all(tree.join("evaluations/structured")).unwrap();
        fs::write(tree.join("evaluations/structured/anything.json"), "{}\n").unwrap();
        let err = run().unwrap_err();
        assert!(err.contains("holds evaluations/structured/*.json"), "{err}");
        assert!(!out.join("a-none-greedy").exists());
        // A notes file there is not json.
        fs::remove_file(tree.join("evaluations/structured/anything.json")).unwrap();
        fs::write(tree.join("evaluations/structured/NOTES.md"), "notes\n").unwrap();
        run().unwrap();

        // Only the dependency json remaining is refused too, and a notes file
        // there is not json.
        let dependency = tree.join("evaluations/structured/dependency");
        fs::create_dir_all(&dependency).unwrap();
        fs::write(dependency.join("NOTES.md"), "notes\n").unwrap();
        fs::remove_dir_all(out.join("a-none-greedy")).unwrap();
        run().unwrap();
        fs::remove_dir_all(out.join("a-none-greedy")).unwrap();
        fs::write(dependency.join("vite.json"), "{}\n").unwrap();
        let err = run().unwrap_err();
        assert!(
            err.contains("holds evaluations/structured/dependency/*.json"),
            "{err}"
        );
        assert!(!out.join("a-none-greedy").exists());
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn a_run_dir_that_already_holds_files_is_refused() {
        let (gold, tree, out) = fixture("not-empty", "Where is `s.listen` set?");
        let args = run_args(&gold, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        run_questions(&args).unwrap();
        let err = run_questions(&args).unwrap_err().to_string();
        assert!(err.contains("not empty"), "{err}");
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn the_arm_and_its_switches_must_agree() {
        let (gold, tree, out) = fixture("switches", "Where is `s.listen` set?");
        let refused = |args: RunArgs, needle: &str| {
            let err = run_questions(&args).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
            assert!(!args.out_dir.join("r-rg-greedy").exists());
        };
        // r with an rg that is not there.
        let mut r = run_args(&gold, &tree, &out, ArmArg::R, None);
        r.rg = Some(out.join("no-such-rg"));
        refused(r, "not a file");
        // r without --rg, and with --grep-skip.
        refused(run_args(&gold, &tree, &out, ArmArg::R, None), "needs --rg");
        let mut r = run_args(&gold, &tree, &out, ArmArg::R, Some(GrepSkip::Off));
        r.rg = Some(out.join("no-such-rg"));
        refused(r, "does not apply to --arm r");
        // a without --grep-skip, and with --rg.
        refused(
            run_args(&gold, &tree, &out, ArmArg::A, None),
            "need --grep-skip",
        );
        let mut a = run_args(&gold, &tree, &out, ArmArg::B, Some(GrepSkip::Lockfiles));
        a.rg = Some(out.join("rg"));
        refused(a, "only applies to --arm r");
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn a_scan_that_stops_voids_the_run_but_leaves_its_files() {
        let (gold, tree, out) = fixture("cap", "Where is `s.listen` set?");
        let mut args = run_args(&gold, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        args.max_scan_bytes = 1;
        let err = run_questions(&args).unwrap_err().to_string();
        assert!(err.contains("void"), "{err}");
        let summary: Value = serde_json::from_str(&read(
            &out.join("a-none-greedy")
                .join("config")
                .join("summary.json"),
        ))
        .unwrap();
        assert!(summary["incomplete_scans"].as_u64().unwrap() > 0);
        assert_eq!(summary["matcher"]["max_scan_bytes"], 1);
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn a_missing_tree_or_stratum_is_reported() {
        let (gold, tree, out) = fixture("missing", "Where is `s.listen` set?");
        let mut args = run_args(&gold, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        args.trees = vec!["other=somewhere".into()];
        let err = run_questions(&args).unwrap_err().to_string();
        assert!(
            err.contains("no --tree <repo>=<dir> for the repository r"),
            "{err}"
        );
        let mut args = run_args(&gold, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        args.stratum = StratumArg::Dependency;
        let err = run_questions(&args).unwrap_err().to_string();
        assert!(err.contains("no question of stratum dependency"), "{err}");
        let mut args = run_args(&gold, &tree, &out, ArmArg::A, Some(GrepSkip::Off));
        args.split = SplitArg::Heldout;
        assert!(run_questions(&args).is_err());
        cleanup(&[&gold, &tree, &out]);
    }

    #[test]
    fn check_probes_passes_a_probed_key_and_fails_a_crowded_out_one() {
        let (gold, tree, out) = fixture("probes-ok", "Where is `s.listen` set?");
        let ok = ProbeArgs {
            gold_dir: gold.clone(),
            probe_terms: 8,
        };
        check_probes(&ok).unwrap();
        // A long word crowds the key out when only one term is probed; the
        // question that does not name its key is not checked.
        let (gold2, tree2, out2) = fixture(
            "probes-bad",
            "Where in the configuration files is `s.listen` set?",
        );
        let repos = gold::load_dir(&gold2).unwrap();
        let (checked, skipped, failures) = probe_failures(&repos, 1);
        assert_eq!((checked, skipped), (2, 1));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("r.f0.q1") && failures[0].contains("`listen`"));
        let bad = ProbeArgs {
            gold_dir: gold2.clone(),
            probe_terms: 1,
        };
        assert!(check_probes(&bad).is_err());
        check_probes(&ProbeArgs {
            gold_dir: gold2.clone(),
            probe_terms: 8,
        })
        .unwrap();
        cleanup(&[&gold, &tree, &out, &gold2, &tree2, &out2]);
    }

    #[test]
    fn trees_labels_and_times_are_parsed() {
        let trees = parse_trees(&["vite=/x/vite".into(), "lci=y".into()]).unwrap();
        assert_eq!(trees["vite"], PathBuf::from("/x/vite"));
        assert!(parse_trees(&["novalue".into()]).is_err());
        assert!(parse_trees(&["=x".into()]).is_err());
        assert!(parse_trees(&["a=x".into(), "a=y".into()]).is_err());
        assert_eq!(
            split_label("greedy=a,b", "--pair").unwrap(),
            ("greedy".to_owned(), "a,b")
        );
        assert!(split_label("=a", "--pair").is_err());
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_iso(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(utc_iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(file_stem("vite.a/b q1"), "vite.a_b_q1");
    }
}
