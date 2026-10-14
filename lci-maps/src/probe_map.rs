//! The `probe.compile` map (multimap wave W0, part b): the M1.3 compiler probe
//! ([`crate::probe`], the `compile_probe` example) as an ephemeral map.
//!
//! Given a function or method node (a unit of the SCIP graph) the map makes
//! a candidate patch to its definition, runs `cargo check --workspace
//! --all-targets --keep-going --message-format=json` on a scratch copy of the
//! tree, and emits one `compile_error_at` edge (witness `Compiler`) from the
//! node to each unit that holds an error site, or to the site's file when no
//! unit holds it (confidence 60 instead of 100).
//!
//! # The real tree is never touched
//!
//! The map reads the real tree (`git ls-files`, plain reads) and writes only
//! to the scratch directory: a checkout of the tree's files that persists
//! between calls so that the target directory stays warm (cargo's
//! fingerprints include the workspace path). A call syncs the scratch to the
//! tree by content (an unchanged file is not rewritten: its modification time
//! is what cargo reads), writes the patch, runs cargo and writes the original
//! content back, also when cargo fails or times out. The restored file keeps
//! its new modification time on purpose: giving it back its old one would let
//! cargo reuse the patched build.
//!
//! # Patches
//!
//! * [`PatchKind::Rename`] renames the function at its definition. Every
//!   reference then fails to resolve. Works for any function or method.
//! * [`PatchKind::Signature`] adds a parameter `__lci_probe: u8` (after the
//!   `self` parameter of a method). Every call then fails with a wrong
//!   argument count.
//! * [`ProbeMap::probe_diff`] applies a unified diff the caller made (a
//!   patch from an edge option).
//!
//! The default is `[Rename]`; one patch is one cargo run.
//!
//! # Cost (measured in M1.3, `docs/spike/multimap/v2/m1.3-results.md`)
//!
//! `cargo check` on a warm target directory: p50 4.2 s, p95 6.9 s. After
//! `cargo clean -p` of the workspace crates: p50 12.5 s, p95 16.7 s. From an
//! empty target directory: 661 s once. The map is off unless a traversal asks
//! for it by id, and every call has a hard timeout (the config's, and never
//! past the call's own deadline): a cargo that outlives it is killed with its
//! children and the sites parsed so far are returned as a partial answer
//! ([`ProbeMap::last_error`] says so). The card's p50 is the running p50 of the
//! map's own calls, [`PRIOR_EXPAND_MS`] before any.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, Observed, SnapshotView,
    SourceLabel,
    edge::{CONF_MAX, Edge, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    probe::parse_cargo_json,
    scip_graph::ScipMap,
};
use lci_core::node::{NodeId, NodeKind};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, PoisonError, mpsc},
    time::{Duration, Instant},
};

pub const PROBE_COMPILE_ID: MapId = "probe.compile";

/// The warm p50 of one probe call measured in M1.3 (4.20 s).
pub const PRIOR_EXPAND_MS: f64 = 4200.0;

/// The confidence of an edge to the unit that holds the error site.
const UNIT_CONF: u8 = CONF_MAX;
/// The confidence of an edge to a site's file when no unit holds the site.
const FILE_CONF: u8 = 60;
/// The parameter the signature patch adds.
const PROBE_PARAM: &str = "__lci_probe: u8";
/// What the rename patch appends to the name.
const RENAME_SUFFIX: &str = "_lci_probe";

// ------------------------------------------------------------ processes

/// What a child process did.
pub(crate) struct Ran {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// Runs `command` with piped output; kills it (and its children) when it
/// outlives `timeout`. The output read so far is returned either way.
pub(crate) fn run_limited(mut command: Command, timeout: Duration) -> std::io::Result<Ran> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buffer);
            }
            let _ = sender.send(buffer);
        });
        receiver
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait()? {
            Some(status) => break (Some(status), false),
            None if started.elapsed() >= timeout => {
                kill_tree(&mut child);
                break (None, true);
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    // After a kill a grandchild that survived may still hold the pipes: wait
    // a moment, then take what there is.
    let grace = if timed_out {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(60)
    };
    let stdout = out.recv_timeout(grace).unwrap_or_default();
    let stderr = err.recv_timeout(grace).unwrap_or_default();
    Ok(Ran {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id().to_string();
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(windows))]
    let _ = Command::new("pkill")
        .args(["-KILL", "-P", &pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// What a cargo run returned.
#[derive(Debug, Clone, Default)]
pub struct CargoOutput {
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub success: Option<bool>,
}

/// Runs `cargo check` in a directory. A trait so that the tests can supply
/// canned cargo output.
pub trait CargoRunner: Send + Sync {
    fn check(&self, dir: &Path, target_dir: &Path, timeout: Duration) -> CargoOutput;
}

/// The real cargo.
#[derive(Debug, Clone)]
pub struct RealCargo {
    /// Pass `--offline`: the probe never reaches for the network.
    pub offline: bool,
}

impl Default for RealCargo {
    fn default() -> Self {
        Self { offline: true }
    }
}

impl CargoRunner for RealCargo {
    fn check(&self, dir: &Path, target_dir: &Path, timeout: Duration) -> CargoOutput {
        let mut command = Command::new("cargo");
        command.args([
            "check",
            "--workspace",
            "--all-targets",
            "--keep-going",
            "--message-format=json",
        ]);
        if self.offline {
            command.arg("--offline");
        }
        command
            .current_dir(dir)
            .env("CARGO_TARGET_DIR", target_dir)
            .env("CARGO_TERM_COLOR", "never");
        match run_limited(command, timeout) {
            Ok(ran) => CargoOutput {
                stdout: String::from_utf8_lossy(&ran.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
                timed_out: ran.timed_out,
                success: ran.status.map(|s| s.success()),
            },
            Err(error) => CargoOutput {
                stderr: format!("cannot run cargo: {error}"),
                ..CargoOutput::default()
            },
        }
    }
}

// -------------------------------------------------------------- patches

/// A fixed probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PatchKind {
    Rename,
    Signature,
}

impl PatchKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Signature => "signature",
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The byte offset of the 0-based `(line, col)` (col counts characters).
fn offset_of(source: &str, line: u32, col: u32) -> Option<usize> {
    let mut start = 0usize;
    for _ in 0..line {
        start += source[start..].find('\n')? + 1;
    }
    let rest = &source[start..];
    let in_line = rest.split('\n').next().unwrap_or("");
    let byte = in_line
        .char_indices()
        .nth(col as usize)
        .map(|(i, _)| i)
        .or((in_line.chars().count() == col as usize).then_some(in_line.len()))?;
    Some(start + byte)
}

/// Whether `param` (the text of one parameter) is a `self` parameter.
fn is_self_param(param: &str) -> bool {
    let mut rest = param.trim_start();
    if let Some(after) = rest.strip_prefix('&') {
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix('\'') {
            let end = after.find(|c: char| !is_ident(c)).unwrap_or(after.len());
            rest = after[end..].trim_start();
        }
    }
    if let Some(after) = rest.strip_prefix("mut")
        && after.starts_with(|c: char| c.is_whitespace())
    {
        rest = after.trim_start();
    }
    match rest.strip_prefix("self") {
        Some(after) => !after.starts_with(is_ident),
        None => false,
    }
}

/// The source with the patch of `kind` applied to the function whose name
/// `name` starts at the 0-based `(line, col)`.
pub fn make_patch(
    kind: PatchKind,
    source: &str,
    line: u32,
    col: u32,
    name: &str,
) -> Result<String, String> {
    let at = offset_of(source, line, col)
        .ok_or_else(|| format!("no position {}:{} in the file", line + 1, col + 1))?;
    let end = at + name.len();
    if source.get(at..end) != Some(name) {
        return Err(format!(
            "the definition is not at {}:{} (expected `{name}`)",
            line + 1,
            col + 1
        ));
    }
    match kind {
        PatchKind::Rename => Ok(format!(
            "{}{name}{RENAME_SUFFIX}{}",
            &source[..at],
            &source[end..]
        )),
        PatchKind::Signature => {
            let mut open = end;
            let bytes = source.as_bytes();
            // Skip generics, then expect the parameter list.
            while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                open += 1;
            }
            if bytes.get(open) == Some(&b'<') {
                let mut depth = 0i32;
                while open < bytes.len() {
                    match bytes[open] {
                        b'<' => depth += 1,
                        b'>' if open > 0 && bytes[open - 1] != b'-' => {
                            depth -= 1;
                            if depth == 0 {
                                open += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    open += 1;
                }
                while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                    open += 1;
                }
            }
            if bytes.get(open) != Some(&b'(') {
                return Err("no parameter list after the function name".into());
            }
            let params = open + 1;
            // The first parameter: up to the first top-level `,` or `)`.
            let mut depth = 0i32;
            let mut first_end = params;
            for (i, c) in source[params..].char_indices() {
                match c {
                    '(' | '[' | '{' | '<' => depth += 1,
                    ')' | ']' | '}' | '>' if depth > 0 => depth -= 1,
                    ',' | ')' if depth == 0 => {
                        first_end = params + i;
                        break;
                    }
                    _ => {}
                }
                first_end = params + i + c.len_utf8();
            }
            let first = &source[params..first_end];
            if first.trim().is_empty() {
                // `()` : the probe parameter is the only one.
                return Ok(format!(
                    "{}{PROBE_PARAM}{}",
                    &source[..params],
                    &source[params..]
                ));
            }
            if is_self_param(first) {
                let tail = first.trim_end().len();
                return Ok(format!(
                    "{}, {PROBE_PARAM}{}",
                    &source[..params + tail],
                    &source[params + tail..]
                ));
            }
            Ok(format!(
                "{}{PROBE_PARAM}, {}",
                &source[..params],
                &source[params..]
            ))
        }
    }
}

// ----------------------------------------------------------------- map

#[derive(Clone)]
pub struct ProbeConfig {
    /// The real tree. Read only.
    pub root: PathBuf,
    /// The scratch checkout the patches are made in (created; kept).
    pub scratch: PathBuf,
    /// Cargo's target directory (kept, so that it stays warm).
    pub target_dir: PathBuf,
    /// The patches an expansion tries, in order; one cargo run each.
    pub kinds: Vec<PatchKind>,
    /// Hard limit of one cargo run, also cut to the call's deadline.
    pub timeout: Duration,
}

/// `LCI_PROBE_SCRATCH_DIR` and `LCI_PROBE_TARGET_DIR` override the scratch
/// checkout and the target directory.
pub const ENV_SCRATCH: &str = "LCI_PROBE_SCRATCH_DIR";
pub const ENV_TARGET: &str = "LCI_PROBE_TARGET_DIR";

impl ProbeConfig {
    /// Defaults for the tree at `root`: a scratch checkout and a target
    /// directory under the system temp directory, keyed by the root's path.
    pub fn for_tree(root: &Path) -> Self {
        let key: String = Sha256::digest(root.to_string_lossy().as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let base = std::env::temp_dir().join("lci-probe").join(key);
        let pick =
            |var: &str, default: PathBuf| std::env::var_os(var).map_or(default, PathBuf::from);
        Self {
            root: root.to_path_buf(),
            scratch: pick(ENV_SCRATCH, base.join("tree")),
            target_dir: pick(ENV_TARGET, base.join("target")),
            kinds: vec![PatchKind::Rename],
            timeout: Duration::from_secs(60),
        }
    }
}

/// What one probe run found, for the tests and the demo.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeRun {
    pub patch: String,
    pub sites: usize,
    pub error_messages: usize,
    pub external_spans_skipped: usize,
    pub build_success: Option<bool>,
    pub timed_out: bool,
    pub sync_ms: f64,
    pub cargo_ms: f64,
    pub files_copied: usize,
}

struct Inner {
    scip: Arc<ScipMap>,
    /// Graph nodes by file (`/` separated).
    units: HashMap<String, Vec<usize>>,
    node_index: HashMap<NodeId, usize>,
    config: ProbeConfig,
    cargo: Box<dyn CargoRunner>,
    /// One probe at a time: they share the scratch checkout.
    gate: Mutex<()>,
    timings: Mutex<Vec<f64>>,
    last_error: Mutex<Option<String>>,
    runs: Mutex<Vec<ProbeRun>>,
}

pub struct ProbeMap {
    descriptor: MapDescriptor,
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProbeMap {
    pub fn new(scip: Arc<ScipMap>, config: ProbeConfig) -> Self {
        Self::with_cargo(scip, config, Box::new(RealCargo::default()))
    }

    pub fn with_cargo(
        scip: Arc<ScipMap>,
        config: ProbeConfig,
        cargo: Box<dyn CargoRunner>,
    ) -> Self {
        let graph = scip.graph();
        let mut units: HashMap<String, Vec<usize>> = HashMap::new();
        let mut node_index = HashMap::new();
        for (i, node) in graph.nodes.iter().enumerate() {
            units
                .entry(node.file.replace('\\', "/"))
                .or_default()
                .push(i);
            node_index.insert(scip.node_id(i).clone(), i);
        }
        Self {
            descriptor: MapDescriptor {
                id: PROBE_COMPILE_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol],
                emits: vec![NodeKind::Symbol, NodeKind::File],
                freshness: FreshnessModel::Live,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
            inner: Arc::new(Inner {
                scip,
                units,
                node_index,
                config,
                cargo,
                gate: Mutex::new(()),
                timings: Mutex::new(Vec::new()),
                last_error: Mutex::new(None),
                runs: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The error of the last probe that failed or ran out of time.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.inner.last_error).clone()
    }

    /// Every probe run so far.
    pub fn runs(&self) -> Vec<ProbeRun> {
        lock(&self.inner.runs).clone()
    }

    /// The edges of `node` for every configured patch, synchronously.
    pub fn edges_blocking(&self, node: &NodeId, timeout: Duration) -> Vec<Edge> {
        self.inner.edges_for(node, timeout)
    }

    /// The edges for a unified diff the caller made (paths relative to the
    /// tree root, `a/` and `b/` prefixes), applied in the scratch checkout
    /// with `git apply`.
    pub fn probe_diff(&self, node: &NodeId, diff: &str, timeout: Duration) -> Vec<Edge> {
        self.inner.probe(node, Patch::Diff(diff), timeout)
    }
}

enum Patch<'a> {
    Fixed(PatchKind),
    Diff(&'a str),
}

/// Writes the original content of the files a patch touched back, on drop.
struct Restore {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Restore {
    fn remember(&mut self, path: PathBuf) {
        let original = std::fs::read(&path).ok();
        self.files.push((path, original));
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, original) in self.files.drain(..) {
            let _ = match original {
                Some(bytes) => std::fs::write(&path, bytes),
                None => std::fs::remove_file(&path),
            };
        }
    }
}

impl Inner {
    fn edges_for(&self, node: &NodeId, timeout: Duration) -> Vec<Edge> {
        let started = Instant::now();
        let mut best: HashMap<NodeId, Edge> = HashMap::new();
        for kind in &self.config.kinds {
            let left = timeout.saturating_sub(started.elapsed());
            for edge in self.probe(node, Patch::Fixed(*kind), left) {
                match best.get(&edge.dst) {
                    Some(have) if have.conf >= edge.conf => {}
                    _ => {
                        best.insert(edge.dst.clone(), edge);
                    }
                }
            }
        }
        let mut edges: Vec<Edge> = best.into_values().collect();
        cap_edges(&mut edges, EXPAND_LIMIT);
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let mut timings = lock(&self.timings);
        if timings.len() >= 64 {
            timings.remove(0);
        }
        timings.push(elapsed);
        edges
    }

    fn fail(&self, message: String) -> Vec<Edge> {
        *lock(&self.last_error) = Some(message);
        Vec::new()
    }

    /// The innermost graph node whose lines hold the 0-based `line` of `file`.
    fn unit_at(&self, file: &str, line: u32) -> Option<usize> {
        let nodes = &self.scip.graph().nodes;
        self.units
            .get(file)?
            .iter()
            .copied()
            .filter(|&i| nodes[i].span.0 <= line && line <= nodes[i].span.1)
            .min_by_key(|&i| (nodes[i].span.1 - nodes[i].span.0, i))
    }

    fn probe(&self, node: &NodeId, patch: Patch<'_>, timeout: Duration) -> Vec<Edge> {
        let Some(&index) = self.node_index.get(node) else {
            return Vec::new();
        };
        let graph = self.scip.graph();
        let unit = &graph.nodes[index];
        let rel = unit.file.replace('\\', "/");
        let _gate = lock(&self.gate);
        let started = Instant::now();
        let scratch = &self.config.scratch;
        let (copied, sync_error) = match sync_scratch(&self.config.root, scratch) {
            Ok(copied) => (copied, None),
            Err(error) => (0, Some(error)),
        };
        if let Some(error) = sync_error {
            return self.fail(format!("cannot prepare the scratch checkout: {error}"));
        }
        let sync_ms = started.elapsed().as_secs_f64() * 1000.0;

        // Patch the scratch file; everything written is written back on drop.
        let mut restore = Restore { files: Vec::new() };
        let target_file = scratch.join(&rel);
        let label = match patch {
            Patch::Fixed(kind) => {
                let Ok(source) = std::fs::read_to_string(&target_file) else {
                    return self.fail(format!("cannot read {rel} in the scratch checkout"));
                };
                let name = unit.name.rsplit("::").next().unwrap_or(&unit.name);
                let patched = match make_patch(kind, &source, unit.line, unit.col, name) {
                    Ok(patched) => patched,
                    Err(error) => return self.fail(format!("{rel}: {error}")),
                };
                restore.remember(target_file.clone());
                if let Err(error) = std::fs::write(&target_file, patched) {
                    return self.fail(format!(
                        "cannot write {rel} in the scratch checkout: {error}"
                    ));
                }
                kind.as_str().to_string()
            }
            Patch::Diff(diff) => {
                for path in diff_paths(diff) {
                    restore.remember(scratch.join(path));
                }
                if let Err(error) = apply_diff(scratch, diff) {
                    return self.fail(format!("cannot apply the diff: {error}"));
                }
                "diff".to_string()
            }
        };

        let cargo_started = Instant::now();
        let output = self.cargo.check(
            scratch,
            &self.config.target_dir,
            timeout.min(self.config.timeout),
        );
        let cargo_ms = cargo_started.elapsed().as_secs_f64() * 1000.0;
        drop(restore);

        let root = scratch.to_string_lossy().into_owned();
        let parsed = parse_cargo_json(&output.stdout, &root);
        let mut edges = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        for site in &parsed.sites {
            let line0 = site.line.saturating_sub(1);
            let (dst, conf) = match self.unit_at(&site.file, line0) {
                Some(i) => (self.scip.node_id(i).clone(), UNIT_CONF),
                None => match NodeId::file(site.file.clone()) {
                    Ok(file) => (file, FILE_CONF),
                    Err(_) => continue,
                },
            };
            if dst == *node || !seen.insert(dst.clone()) {
                continue;
            }
            edges.push(Edge {
                src: node.clone(),
                dst,
                kind: EdgeKind::CompileErrorAt,
                map: PROBE_COMPILE_ID,
                witness: WitnessClass::Compiler,
                conf,
                site: Some(Site {
                    path: site.file.clone(),
                    blob: String::new(),
                    line: line0,
                    col: site.col.saturating_sub(1),
                }),
                cost: Cost::default(),
            });
        }
        cap_edges(&mut edges, EXPAND_LIMIT);
        let cost = Cost {
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            external_calls: 1,
            nodes_touched: edges.len() as u64,
            bytes_read: output.stdout.len() as u64,
            est_tokens: 0,
        };
        for edge in &mut edges {
            edge.cost = cost;
        }
        let mut problems = Vec::new();
        if output.timed_out {
            problems.push(format!(
                "cargo check ({label}) timed out after {} s; the sites parsed so far are returned",
                timeout.min(self.config.timeout).as_secs()
            ));
        } else if output.success.is_none() {
            problems.push(format!(
                "cargo check did not run: {}",
                output.stderr.lines().next().unwrap_or("no output")
            ));
        } else if parsed.build_success.is_none() {
            problems.push(format!(
                "cargo check produced no build result: {}",
                output.stderr.lines().next().unwrap_or("no message")
            ));
        }
        if let Some(problem) = problems.pop() {
            *lock(&self.last_error) = Some(problem);
        }
        lock(&self.runs).push(ProbeRun {
            patch: label,
            sites: parsed.sites.len(),
            error_messages: parsed.error_messages,
            external_spans_skipped: parsed.external_spans_skipped,
            build_success: parsed.build_success,
            timed_out: output.timed_out,
            sync_ms,
            cargo_ms,
            files_copied: copied,
        });
        edges
    }
}

// -------------------------------------------------------- scratch tree

/// The files of the tree: the tracked and the not-ignored untracked ones when
/// it is a git working tree, else every file outside `target`, `.git` and
/// hidden tool directories.
fn tree_files(root: &Path) -> Result<Vec<String>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.quotepath=false",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    if let Ok(ran) = run_limited(command, Duration::from_secs(60))
        && ran.status.is_some_and(|s| s.success())
    {
        let mut files: Vec<String> = String::from_utf8_lossy(&ran.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        // A deleted but still tracked file is listed; it is not on disk.
        files.retain(|p| root.join(p).is_file());
        files.sort();
        files.dedup();
        return Ok(files);
    }
    let mut files = Vec::new();
    walk(root, root, &mut files).map_err(|e| e.to_string())?;
    files.sort();
    Ok(files)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if name != "target" && name != ".git" {
                walk(root, &path, out)?;
            }
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Makes the files of `scratch` those of the tree at `root`, rewriting only
/// the files whose content differs (cargo reads modification times). Returns
/// how many files were written.
pub fn sync_scratch(root: &Path, scratch: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let wanted = tree_files(root)?;
    let mut copied = 0;
    for rel in &wanted {
        let (from, to) = (root.join(rel), scratch.join(rel));
        let content = std::fs::read(&from).map_err(|e| format!("{rel}: {e}"))?;
        if std::fs::read(&to).is_ok_and(|have| have == content) {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&to, content).map_err(|e| format!("{rel}: {e}"))?;
        copied += 1;
    }
    // Files the tree no longer has.
    let keep: HashSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut present = Vec::new();
    walk(scratch, scratch, &mut present).map_err(|e| e.to_string())?;
    for rel in present {
        if !keep.contains(rel.as_str()) {
            let _ = std::fs::remove_file(scratch.join(&rel));
            copied += 1;
        }
    }
    Ok(copied)
}

/// The paths a unified diff changes (`+++ b/path`, `--- a/path`).
fn diff_paths(diff: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line
            .strip_prefix("+++ ")
            .or_else(|| line.strip_prefix("--- "))
        else {
            continue;
        };
        let path = rest.split('\t').next().unwrap_or(rest).trim();
        let path = path
            .strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path);
        if path != "/dev/null" && !path.is_empty() && !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    }
    paths
}

fn apply_diff(scratch: &Path, diff: &str) -> Result<(), String> {
    let file = scratch.join(".lci-probe.diff");
    std::fs::write(&file, diff).map_err(|e| e.to_string())?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(scratch)
        .args(["apply", "--unidiff-zero", "--whitespace=nowarn"])
        .arg(&file);
    let ran = run_limited(command, Duration::from_secs(30)).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&file);
    let ran = ran?;
    if ran.status.is_some_and(|s| s.success()) {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&ran.stderr)
            .lines()
            .next()
            .unwrap_or("git apply failed")
            .to_string())
    }
}

#[crate::async_trait]
impl Map for ProbeMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let mut edges = Vec::new();
        for (seed, _) in request.seeds {
            let timeout = request
                .budget
                .deadline
                .saturating_duration_since(Instant::now());
            edges.extend(self.expand_blocking(seed.clone(), timeout).await);
        }
        cap_edges(&mut edges, usize::MAX);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.freshness.observed = Observed::LiveTree;
        if let Some(problem) = self.last_error() {
            response.status = MapStatus::Partial {
                note: None,
                warning: Some(problem),
            };
        }
        response.cost = Cost {
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            external_calls: edges.first().map_or(0, |e| e.cost.external_calls),
            nodes_touched: edges.len() as u64,
            bytes_read: 0,
            est_tokens: 0,
        };
        response.hits = edges
            .into_iter()
            .zip(1..)
            .map(|(edge, rank)| MapHit {
                node: edge.dst,
                rank,
                score: Some(f32::from(edge.conf) / f32::from(CONF_MAX)),
                evidence: vec![Evidence {
                    via: Some(edge.src),
                    edge: edge.kind,
                    label: SourceLabel::Scip,
                    detail: edge.site.map(|s| format!("{}:{}", s.path, s.line + 1)),
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Vec<Edge> {
        let timeout = budget.deadline.saturating_duration_since(Instant::now());
        self.expand_blocking(node.clone(), timeout).await
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
            good_for: vec![QuestionType::Impact],
        }
    }
}

impl ProbeMap {
    async fn expand_blocking(&self, node: NodeId, timeout: Duration) -> Vec<Edge> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.edges_for(&node, timeout))
            .await
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests;
