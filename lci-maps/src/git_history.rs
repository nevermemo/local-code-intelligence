//! The `git.history` map (multimap wave W0, part b): commit nodes from the
//! tree's git history, read up to the HEAD the map was opened at and no
//! further.
//!
//! Edges (every one is a fact of the object database except co-change):
//!
//! * file (or chunk, or symbol) -> the commits that touched the file, newest
//!   first, at most [`GitHistoryConfig::file_commits`], kind `touched`,
//!   witness `Exact`. The log follows renames (`git log --follow`), merge
//!   commits are left out (they touch nothing of their own). The confidence of
//!   the rank-`r` commit is `100 - r`, so that the cap, which keeps a prefix of
//!   the confidence order, keeps the newest. A symbol is mapped to its file
//!   (the caller supplies the table, [`symbol_files`] builds it from the SCIP
//!   graph) and its commits are the file's commits at 80% confidence: the
//!   history is the file's, not the function's.
//! * commit -> the files it touched that still exist at HEAD, kind `touched`,
//!   witness `Exact`. A commit that touched many files says less about each:
//!   the confidence is 100 up to 8 files and falls as `800 / files` after,
//!   never below 10.
//! * file -> files that changed together with it, kind `cochange`, witness
//!   `Heuristic`, only from a file or chunk node (a symbol is not a clean seed:
//!   the earlier spike found co-change needs clean seeds, and a vector
//!   neighbour is not a co-edit edge). Counted over the newest
//!   [`GitHistoryConfig::cochange_window`] commits, leaving out commits that
//!   touched more than [`GitHistoryConfig::cochange_max_files`] files, and
//!   only pairs seen at least [`GitHistoryConfig::cochange_min_count`] times.
//!   The confidence is `90 * count / (seed commits + 1)`, so it grows with the
//!   count and never reaches an exact edge.
//!
//! A commit node's identity is `commit:<40 hex digits>`, the same string the
//! `history` gold set's `commit` answers hold. Its subject (first line only)
//! and date are [`CommitNode`]s, kept by the map as edges are made
//! ([`GitHistoryMap::commit_node`]).
//!
//! The churn prior is the number of non-merge commits that touched a file
//! over the history read ([`GitHistoryMap::churn`]): the standing control for
//! any history question (rank files by how often they change).
//!
//! The map is off unless a traversal asks for it by id; it costs a few `git`
//! processes per expansion (measured, see [`MapCard`]) and nothing else.
//!
//! The work runs on a blocking thread, which outlives a caller that stopped
//! waiting, so the call's deadline ([`Budget::deadline`]) goes into the worker:
//! no `git` process is started after it, the one running when it passes is
//! killed (at most the time left, never more than
//! [`GitHistoryConfig::git_timeout`]), and the expansion ends with
//! [`DEADLINE_NOTE`] as its recorded failure and no edges. A log read cut short
//! this way is not remembered.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, Need, SnapshotView,
    SourceLabel,
    edge::{CONF_MAX, Edge, Expanded, MapCard, QuestionType, WitnessClass, cap_edges},
    probe_map::run_limited,
    scip_graph::ScipMap,
};
use lci_core::node::{NodeId, NodeKind};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

pub const GIT_HISTORY_ID: MapId = "git.history";

/// The note of a call that ran out of the traversal's time (the deadline of
/// its [`Budget`]).
pub const DEADLINE_NOTE: &str = "git.history: the traversal deadline passed";

/// The p50 of one expansion on the lci repository (about 100 commits),
/// measured in W0b; the card reports the map's own running p50 once it has
/// answered, and this before.
pub const PRIOR_EXPAND_MS: f64 = 60.0;

/// A commit: its hash, the first line of its message and its committer date
/// (ISO 8601, as `git log --format=%cI` prints it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitNode {
    pub sha: String,
    pub subject: String,
    pub date: String,
}

impl CommitNode {
    pub fn node(&self) -> NodeId {
        NodeId::Commit {
            sha: self.sha.clone(),
        }
    }

    /// `abcdef0 2026-01-02 subject`, the line an agent reads.
    pub fn line(&self) -> String {
        let short: String = self.sha.chars().take(7).collect();
        let day: String = self.date.chars().take(10).collect();
        format!("{short} {day} {}", self.subject)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHistoryConfig {
    /// Most commits one file edge list holds (at most [`EXPAND_LIMIT`]).
    pub file_commits: usize,
    /// How many of the newest commits co-change counts over.
    pub cochange_window: usize,
    /// How many of the newest commits the churn prior counts over.
    pub log_limit: usize,
    /// A commit that touched more files than this is not counted for
    /// co-change.
    pub cochange_max_files: usize,
    /// The fewest co-changes that make an edge.
    pub cochange_min_count: u32,
    /// Longest one `git` process may run.
    pub git_timeout: Duration,
}

impl Default for GitHistoryConfig {
    fn default() -> Self {
        Self {
            file_commits: 20,
            cochange_window: 500,
            log_limit: 5000,
            cochange_max_files: 30,
            cochange_min_count: 2,
            git_timeout: Duration::from_secs(30),
        }
    }
}

/// The confidence of a symbol's history, as a percentage of the file's.
const SYMBOL_PCT: u32 = 80;
/// The largest co-change confidence.
const COCHANGE_CONF_CAP: u32 = 90;
/// A commit up to this many files is a full-confidence source of file edges.
const SMALL_COMMIT_FILES: u32 = 8;

/// Symbol node -> repository-relative file (`/` separated), for every node of
/// the SCIP graph.
pub fn symbol_files(scip: &ScipMap) -> HashMap<NodeId, String> {
    let graph = scip.graph();
    (0..graph.nodes.len())
        .map(|i| {
            (
                scip.node_id(i).clone(),
                graph.nodes[i].file.replace('\\', "/"),
            )
        })
        .collect()
}

#[derive(Debug)]
struct LogEntry {
    files: Vec<String>,
}

/// The newest commits with the files each touched.
#[derive(Debug, Default)]
struct RepoLog {
    entries: Vec<LogEntry>,
    churn: HashMap<String, u32>,
}

#[derive(Debug, Default)]
struct Tally {
    calls: u32,
    bytes: u64,
}

struct Inner {
    root: PathBuf,
    head: String,
    config: GitHistoryConfig,
    symbol_files: HashMap<NodeId, String>,
    log: OnceLock<Result<RepoLog, String>>,
    head_files: OnceLock<Result<HashSet<String>, String>>,
    commits: Mutex<HashMap<String, CommitNode>>,
    timings: Mutex<Vec<f64>>,
    last_error: Mutex<Option<String>>,
    /// Git processes started by expansions and log reads (not by `open`).
    runs: AtomicU32,
}

pub struct GitHistoryMap {
    descriptor: MapDescriptor,
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl GitHistoryMap {
    /// Opens the history of the repository that holds `root` at its current
    /// HEAD. An error when `root` is not in a git repository or has no commit.
    pub fn open(root: &Path) -> Result<Self, String> {
        Self::open_with(root, GitHistoryConfig::default(), HashMap::new())
    }

    /// As [`Self::open`], with the config and the symbol -> file table.
    pub fn open_with(
        root: &Path,
        config: GitHistoryConfig,
        symbol_files: HashMap<NodeId, String>,
    ) -> Result<Self, String> {
        let mut tally = Tally::default();
        let head = git_text(
            root,
            config.git_timeout,
            &mut tally,
            &["rev-parse", "--verify", "HEAD^{commit}"],
        )
        .map_err(|e| {
            format!("{} is not in a git repository with a commit: {e}", {
                root.display()
            })
        })?;
        let head = head.trim().to_string();
        let inner = Inner {
            root: root.to_path_buf(),
            head,
            config,
            symbol_files,
            log: OnceLock::new(),
            head_files: OnceLock::new(),
            commits: Mutex::new(HashMap::new()),
            timings: Mutex::new(Vec::new()),
            last_error: Mutex::new(None),
            runs: AtomicU32::new(0),
        };
        Ok(Self {
            descriptor: MapDescriptor {
                id: GIT_HISTORY_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![
                    NodeKind::File,
                    NodeKind::Chunk,
                    NodeKind::Symbol,
                    NodeKind::Commit,
                ],
                emits: vec![NodeKind::Commit, NodeKind::File],
                freshness: FreshnessModel::PerCommit,
                source_labels: vec![SourceLabel::Git, SourceLabel::Heuristic],
                needs: vec![Need::Git],
            },
            inner: Arc::new(inner),
        })
    }

    /// The commit the history is read up to.
    pub fn head(&self) -> &str {
        &self.inner.head
    }

    /// The commit node for `sha` (a full hash): from the edges made so far, else
    /// read from git. `None` when `sha` is not a commit.
    pub fn commit_node(&self, sha: &str) -> Option<CommitNode> {
        self.inner.commit_node(sha)
    }

    /// How many non-merge commits touched `path` over the history read (the
    /// newest [`GitHistoryConfig::log_limit`] commits up to HEAD); 0 when the
    /// log cannot be read.
    pub fn churn(&self, path: &str) -> u32 {
        self.inner
            .log(None)
            .ok()
            .and_then(|log| log.churn.get(path).copied())
            .unwrap_or(0)
    }

    /// The `n` most touched files, most first, ties by path.
    pub fn churn_ranked(&self, n: usize) -> Vec<(String, u32)> {
        let Ok(log) = self.inner.log(None) else {
            return Vec::new();
        };
        let mut all: Vec<(String, u32)> = log.churn.iter().map(|(p, c)| (p.clone(), *c)).collect();
        all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(n);
        all
    }

    /// How many git processes expansions and log reads have started.
    #[cfg(test)]
    pub(crate) fn git_runs(&self) -> u32 {
        self.inner.runs.load(Ordering::Relaxed)
    }

    /// The error of the last expansion that failed, if any (an expansion that
    /// fails returns no edges: the trait has no way to say more).
    pub fn last_error(&self) -> Option<String> {
        lock(&self.inner.last_error).clone()
    }

    /// The edges leaving `node`, synchronously (what [`Map::expand`] runs on a
    /// blocking thread).
    pub fn edges_blocking(&self, node: &NodeId) -> Result<Vec<Edge>, String> {
        self.inner
            .edges_for(node, None)
            .map(|expanded| expanded.edges)
    }
}

impl Inner {
    /// One git process, kept to `deadline` when there is one: not started
    /// after it, and killed when it passes.
    fn git(
        &self,
        deadline: Option<Instant>,
        tally: &mut Tally,
        args: &[&str],
    ) -> Result<(bool, Vec<u8>), String> {
        let started = tally.calls;
        let ran = git_run(&self.root, self.config.git_timeout, deadline, tally, args);
        self.runs
            .fetch_add(tally.calls - started, Ordering::Relaxed);
        ran
    }

    /// The log of the history read. A read cut short by the deadline is not
    /// remembered: the next call, with time, reads it again.
    fn log(&self, deadline: Option<Instant>) -> Result<&RepoLog, String> {
        if self.log.get().is_none() {
            let read = self.read_log(deadline);
            if read.as_ref().is_err_and(|e| e == DEADLINE_NOTE) {
                return Err(DEADLINE_NOTE.to_string());
            }
            let _ = self.log.set(read);
        }
        self.log
            .get()
            .expect("set above")
            .as_ref()
            .map_err(Clone::clone)
    }

    fn read_log(&self, deadline: Option<Instant>) -> Result<RepoLog, String> {
        let mut tally = Tally::default();
        let limit = self.config.log_limit.max(self.config.cochange_window);
        let limit = limit.to_string();
        let (ok, out) = self.git(
            deadline,
            &mut tally,
            &[
                "log",
                &self.head,
                "-n",
                &limit,
                "--no-merges",
                "--no-renames",
                "--relative",
                "--name-only",
                "-z",
                "--format=%x01%H",
            ],
        )?;
        if !ok {
            return Err("git log failed".into());
        }
        let entries = parse_name_log(&out);
        let mut churn: HashMap<String, u32> = HashMap::new();
        for entry in entries.iter().take(self.config.log_limit) {
            for file in &entry.files {
                *churn.entry(file.clone()).or_default() += 1;
            }
        }
        Ok(RepoLog { entries, churn })
    }

    /// The files at HEAD; cut short by the deadline, it is not remembered.
    fn head_files(
        &self,
        deadline: Option<Instant>,
        tally: &mut Tally,
    ) -> Result<&HashSet<String>, String> {
        if self.head_files.get().is_none() {
            let read = self
                .git(
                    deadline,
                    tally,
                    &["ls-tree", "-r", "-z", "--name-only", &self.head],
                )
                .and_then(|(ok, out)| {
                    if !ok {
                        return Err("git ls-tree failed".to_string());
                    }
                    Ok(String::from_utf8_lossy(&out)
                        .split('\0')
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                        .collect::<HashSet<String>>())
                });
            if read.as_ref().is_err_and(|e| e == DEADLINE_NOTE) {
                return Err(DEADLINE_NOTE.to_string());
            }
            let _ = self.head_files.set(read);
        }
        self.head_files
            .get()
            .expect("set above")
            .as_ref()
            .map_err(Clone::clone)
    }

    fn commit_node(&self, sha: &str) -> Option<CommitNode> {
        if let Some(found) = lock(&self.commits).get(sha) {
            return Some(found.clone());
        }
        if !is_full_sha(sha) {
            return None;
        }
        let mut tally = Tally::default();
        let (ok, out) = self
            .git(
                None,
                &mut tally,
                &["log", "-n", "1", "--format=%x1e%H%x1f%cI%x1f%B", sha],
            )
            .ok()?;
        if !ok {
            return None;
        }
        let node = parse_commit_log(&out).into_iter().next()?;
        self.remember(std::slice::from_ref(&node));
        Some(node)
    }

    fn remember(&self, nodes: &[CommitNode]) {
        let mut known = lock(&self.commits);
        for node in nodes {
            known.insert(node.sha.clone(), node.clone());
        }
    }

    /// The repository path of a node that has one, and the percentage its
    /// history edges keep.
    fn path_of(&self, node: &NodeId) -> Option<(String, u32)> {
        match node {
            NodeId::File { path } | NodeId::Chunk { path, .. } => {
                Some((path.replace('\\', "/"), 100))
            }
            NodeId::Symbol(_) => self.symbol_files.get(node).map(|p| (p.clone(), SYMBOL_PCT)),
            _ => None,
        }
    }

    /// The edges leaving `node`; with a `deadline`, no git process starts
    /// after it and the one running then is killed, and the answer is an
    /// error ([`DEADLINE_NOTE`]) instead of a part of the edges.
    fn edges_for(&self, node: &NodeId, deadline: Option<Instant>) -> Result<Expanded, String> {
        let started = Instant::now();
        let mut tally = Tally::default();
        let mut edges = match node {
            NodeId::Commit { sha } => self.commit_edges(node, sha, deadline, &mut tally)?,
            _ => match self.path_of(node) {
                Some((path, pct)) => self.file_edges(
                    node,
                    &path,
                    pct,
                    matches!(node, NodeId::File { .. } | NodeId::Chunk { .. }),
                    deadline,
                    &mut tally,
                )?,
                None => Vec::new(),
            },
        };
        cap_edges(&mut edges, EXPAND_LIMIT);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let cost = Cost {
            elapsed_ms,
            external_calls: tally.calls,
            nodes_touched: edges.len() as u64,
            bytes_read: tally.bytes,
            est_tokens: 0,
        };
        let mut timings = lock(&self.timings);
        if timings.len() >= 256 {
            timings.remove(0);
        }
        timings.push(elapsed_ms);
        Ok(Expanded::new(edges, cost))
    }

    fn file_edges(
        &self,
        node: &NodeId,
        path: &str,
        pct: u32,
        cochange: bool,
        deadline: Option<Instant>,
        tally: &mut Tally,
    ) -> Result<Vec<Edge>, String> {
        let mut edges = Vec::new();
        let cap = self.config.file_commits.clamp(1, EXPAND_LIMIT).to_string();
        let (ok, out) = self.git(
            deadline,
            tally,
            &[
                "log",
                &self.head,
                "--follow",
                "--no-merges",
                "--relative",
                "-n",
                &cap,
                "--format=%x1e%H%x1f%cI%x1f%B",
                "--",
                path,
            ],
        )?;
        if ok {
            let commits = parse_commit_log(&out);
            for (rank, commit) in commits.iter().enumerate() {
                let base = 100u32.saturating_sub(rank as u32).max(1);
                edges.push(Edge {
                    src: node.clone(),
                    dst: commit.node(),
                    kind: EdgeKind::Touched,
                    map: GIT_HISTORY_ID,
                    witness: WitnessClass::Exact,
                    conf: scale(base, pct),
                    site: None,
                });
            }
            self.remember(&commits);
        }
        if cochange {
            edges.extend(self.cochange_edges(node, path, deadline, tally)?);
        }
        Ok(edges)
    }

    fn cochange_edges(
        &self,
        node: &NodeId,
        path: &str,
        deadline: Option<Instant>,
        tally: &mut Tally,
    ) -> Result<Vec<Edge>, String> {
        let log = self.log(deadline)?;
        let head_files = self.head_files(deadline, tally)?;
        let window = log.entries.iter().take(self.config.cochange_window);
        let mut seed_commits = 0u32;
        let mut together: HashMap<&str, u32> = HashMap::new();
        for entry in window {
            if entry.files.len() > self.config.cochange_max_files
                || !entry.files.iter().any(|f| f == path)
            {
                continue;
            }
            seed_commits += 1;
            for file in entry.files.iter().filter(|f| f.as_str() != path) {
                *together.entry(file).or_default() += 1;
            }
        }
        let mut edges = Vec::new();
        for (file, count) in together {
            if count < self.config.cochange_min_count || !head_files.contains(file) {
                continue;
            }
            let Ok(dst) = NodeId::file(file) else {
                continue;
            };
            let conf = (COCHANGE_CONF_CAP * count / (seed_commits + 1)).clamp(1, COCHANGE_CONF_CAP);
            edges.push(Edge {
                src: node.clone(),
                dst,
                kind: EdgeKind::Cochange,
                map: GIT_HISTORY_ID,
                witness: WitnessClass::Heuristic,
                conf: conf as u8,
                site: None,
            });
        }
        Ok(edges)
    }

    fn commit_edges(
        &self,
        node: &NodeId,
        sha: &str,
        deadline: Option<Instant>,
        tally: &mut Tally,
    ) -> Result<Vec<Edge>, String> {
        // History only up to HEAD: a commit that is not an ancestor of it is
        // not part of this map's history.
        let (ancestor, _) = self.git(
            deadline,
            tally,
            &["merge-base", "--is-ancestor", sha, &self.head],
        )?;
        if !ancestor {
            return Ok(Vec::new());
        }
        let (ok, out) = self.git(
            deadline,
            tally,
            &[
                "diff-tree",
                "-r",
                "--root",
                "--no-commit-id",
                "--no-renames",
                "--relative",
                "--name-only",
                "-z",
                sha,
            ],
        )?;
        if !ok {
            return Ok(Vec::new());
        }
        let head_files = self.head_files(deadline, tally)?;
        let files: Vec<String> = String::from_utf8_lossy(&out)
            .split('\0')
            .filter(|p| !p.is_empty() && head_files.contains(*p))
            .map(str::to_string)
            .collect();
        let touched = u32::try_from(files.len()).unwrap_or(u32::MAX);
        let conf = if touched <= SMALL_COMMIT_FILES {
            CONF_MAX
        } else {
            ((100 * SMALL_COMMIT_FILES) / touched).clamp(10, 100) as u8
        };
        Ok(files
            .into_iter()
            .filter_map(|file| NodeId::file(file).ok())
            .map(|dst| Edge {
                src: node.clone(),
                dst,
                kind: EdgeKind::Touched,
                map: GIT_HISTORY_ID,
                witness: WitnessClass::Exact,
                conf,
                site: None,
            })
            .collect())
    }
}

fn scale(conf: u32, pct: u32) -> u8 {
    ((conf * pct).div_ceil(100)).clamp(1, u32::from(CONF_MAX)) as u8
}

fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.quotepath=false",
            "-c",
            "log.showSignature=false",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// Runs git; the flag is whether it exited with status 0. With a `deadline`
/// the process is not started once it has passed, and gets no more time than
/// is left of it ([`DEADLINE_NOTE`] when that is what ended it).
fn git_run(
    root: &Path,
    timeout: Duration,
    deadline: Option<Instant>,
    tally: &mut Tally,
    args: &[&str],
) -> Result<(bool, Vec<u8>), String> {
    let (limit, by_deadline) = match deadline {
        None => (timeout, false),
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(DEADLINE_NOTE.to_string());
            }
            (left.min(timeout), left < timeout)
        }
    };
    let mut command = git_command(root);
    command.args(args);
    let ran = run_limited(command, limit).map_err(|e| format!("cannot run git: {e}"))?;
    tally.calls += 1;
    tally.bytes += ran.stdout.len() as u64;
    if ran.timed_out && by_deadline {
        return Err(DEADLINE_NOTE.to_string());
    }
    if ran.timed_out {
        return Err(format!(
            "git {} timed out after {} s",
            args.first().copied().unwrap_or(""),
            timeout.as_secs()
        ));
    }
    Ok((ran.status.is_some_and(|s| s.success()), ran.stdout))
}

fn git_text(
    root: &Path,
    timeout: Duration,
    tally: &mut Tally,
    args: &[&str],
) -> Result<String, String> {
    let (ok, out) = git_run(root, timeout, None, tally, args)?;
    if !ok {
        return Err("git failed".into());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `git log -z --name-only --format=%x01%H`: one record per commit.
fn parse_name_log(out: &[u8]) -> Vec<LogEntry> {
    String::from_utf8_lossy(out)
        .split('\u{1}')
        .skip(1)
        .map(|record| {
            let mut parts = record.split('\0');
            let _sha = parts.next();
            LogEntry {
                files: parts
                    .map(|p| p.trim_start_matches('\n'))
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect(),
            }
        })
        .collect()
}

/// `git log --format=%x1e%H%x1f%cI%x1f%B`: hash, date and the message, whose
/// first line is the subject.
fn parse_commit_log(out: &[u8]) -> Vec<CommitNode> {
    String::from_utf8_lossy(out)
        .split('\u{1e}')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut parts = record.splitn(3, '\u{1f}');
            let sha = parts.next()?.trim();
            let date = parts.next()?.trim();
            let message = parts.next().unwrap_or("");
            if !is_full_sha(sha) {
                return None;
            }
            let subject = message
                .lines()
                .next()
                .unwrap_or("")
                .trim_end_matches('\r')
                .trim()
                .to_string();
            Some(CommitNode {
                sha: sha.to_string(),
                subject,
                date: date.to_string(),
            })
        })
        .collect()
}

#[crate::async_trait]
impl Map for GitHistoryMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let deadline = request.budget.deadline;
        if started >= deadline {
            let warning = DEADLINE_NOTE.to_string();
            *lock(&self.inner.last_error) = Some(warning.clone());
            return MapResponse::empty(&self.descriptor, view, MapStatus::Failed { warning });
        }
        let inner = self.inner.clone();
        let seeds: Vec<NodeId> = request.seeds.iter().map(|(n, _)| n.clone()).collect();
        let answer = tokio::task::spawn_blocking(move || {
            let mut edges = Vec::new();
            let mut tally_cost = Cost::default();
            for seed in &seeds {
                let found = inner.edges_for(seed, Some(deadline))?;
                tally_cost.external_calls += found.cost.external_calls;
                tally_cost.bytes_read += found.cost.bytes_read;
                edges.extend(found.edges);
            }
            Ok::<_, String>((edges, tally_cost))
        })
        .await;
        let (mut edges, mut cost) = match answer {
            Ok(Ok(done)) => done,
            Ok(Err(warning)) => {
                *lock(&self.inner.last_error) = Some(warning.clone());
                return MapResponse::empty(&self.descriptor, view, MapStatus::Failed { warning });
            }
            Err(_) => {
                return MapResponse::empty(
                    &self.descriptor,
                    view,
                    MapStatus::Failed {
                        warning: "git history worker panicked".into(),
                    },
                );
            }
        };
        cap_edges(&mut edges, usize::MAX);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        cost.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        cost.nodes_touched = edges.len() as u64;
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.cost = cost;
        response.hits = edges
            .into_iter()
            .zip(1..)
            .map(|(edge, rank)| {
                let detail = match (&edge.dst, edge.kind) {
                    (NodeId::Commit { sha }, _) => self.inner.commit_node(sha).map(|c| c.line()),
                    (NodeId::File { path }, EdgeKind::Cochange) => {
                        Some(format!("churn {}", self.churn(path)))
                    }
                    _ => None,
                };
                MapHit {
                    node: edge.dst,
                    rank,
                    score: Some(f32::from(edge.conf) / f32::from(CONF_MAX)),
                    evidence: vec![Evidence {
                        via: Some(edge.src),
                        edge: edge.kind,
                        label: if edge.witness == WitnessClass::Heuristic {
                            SourceLabel::Heuristic
                        } else {
                            SourceLabel::Git
                        },
                        detail,
                    }],
                }
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        // Every expansion runs git: with no call left it does not run.
        if budget.max_external_calls == 0 {
            return Expanded::default();
        }
        // The caller stops waiting at the deadline, but the blocking worker
        // would not: it gets the deadline and stops there.
        let deadline = budget.deadline;
        if Instant::now() >= deadline {
            *lock(&self.inner.last_error) = Some(DEADLINE_NOTE.to_string());
            return Expanded::default();
        }
        let inner = self.inner.clone();
        let node = node.clone();
        match tokio::task::spawn_blocking(move || inner.edges_for(&node, Some(deadline))).await {
            Ok(Ok(expanded)) => expanded,
            Ok(Err(error)) => {
                *lock(&self.inner.last_error) = Some(error);
                Expanded::default()
            }
            Err(_) => {
                *lock(&self.inner.last_error) = Some("git history worker panicked".into());
                Expanded::default()
            }
        }
    }

    fn card(&self) -> MapCard {
        let p50 = {
            let mut seen = lock(&self.inner.timings).clone();
            if seen.is_empty() {
                None
            } else {
                seen.sort_by(f64::total_cmp);
                Some(seen[seen.len() / 2])
            }
        };
        MapCard {
            expand_p50_ms: Some(p50.unwrap_or(PRIOR_EXPAND_MS)),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: None,
            dependent: false,
            good_for: vec![QuestionType::History],
        }
    }
}

#[cfg(test)]
mod tests;
