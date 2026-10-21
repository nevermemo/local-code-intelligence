//! `cargo xtask env`: where the development-tooling roles live on this
//! machine.
//!
//! Committed text names a machine value by role ("the work area", "the plans
//! directory") and points here for the value, so the repository never carries
//! an absolute path. Each role resolves, lowest to highest, from
//!
//! 1. a built-in default (the work area under the platform's local data
//!    directory, every other role derived from it or from the system temp
//!    directory),
//! 2. `.local/machine.toml` at the repository root (gitignored; a linked git
//!    worktree without its own file reads the main worktree's), then
//! 3. an `LCI_DEV_<KEY>` environment variable (an empty value is unset).
//!
//! Relative values resolve against the repository root. `--ensure` creates
//! the directories the roles live in; `--init` writes the machine file. The
//! resolver itself is pure ([`resolve`]) and never touches the file system.
//! See `docs/development/machine-values.md`.

use crate::acceptance::workspace_root;
use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Prefix of the environment variables that override one role each.
const ENV_PREFIX: &str = "LCI_DEV_";
/// The machine file, relative to the repository root.
const MACHINE_DIR: &str = ".local";
const MACHINE_FILE: &str = "machine.toml";
/// Directory name under the platform's local data directory that holds the
/// default work area.
const DEV_DIR: &str = "local-code-intelligence-dev";
/// Named outside the `lci-` prefix. `live_acceptance` now removes only its
/// own run root (`lci-live-run-*`, d430), so this is a second guard: the
/// scratch area can never look like a fixture leftover.
const SCRATCH_DIR: &str = "local-code-intelligence-scratch";
/// Stands for the work area in the commented lines `--init` writes. A value
/// still holding it was uncommented without being edited, so
/// [`parse_machine_file`] rejects it: left alone it would resolve to a path
/// literally named `<work_area>` inside the checkout.
const WORK_AREA_PLACEHOLDER: &str = "<work_area>";

#[derive(Args)]
pub struct EnvArgs {
    /// Role to resolve and print as a bare value (for scripts and hooks).
    /// Without it every role is listed.
    key: Option<String>,
    /// Print JSON: every role with its value, source and origin, plus the
    /// machine file and the environment variables used.
    #[arg(long)]
    json: bool,
    /// Create the missing directories the roles live in (all of them, or the
    /// one named KEY) and print what was created. Never creates a file.
    #[arg(long, conflicts_with = "init")]
    ensure: bool,
    /// Write `.local/machine.toml` for this machine from the built-in
    /// defaults. Refuses to overwrite without --force.
    #[arg(long, conflicts_with_all = ["key", "json", "ensure"])]
    init: bool,
    /// With --init, replace an existing machine file.
    #[arg(long, requires = "init")]
    force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Directory,
    File,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Directory => "directory",
            Kind::File => "file",
        }
    }
}

/// How a role's built-in default is computed.
#[derive(Clone, Copy)]
enum Derive {
    /// Under the platform's local data directory (`<data>/DEV_DIR/<name>`).
    LocalData(&'static str),
    /// Under the system temp directory.
    Temp(&'static str),
    /// Relative to the resolved work area.
    WorkArea(&'static str),
}

impl Derive {
    /// The path below the work area for a role that follows it, else `None`.
    fn work_area_relative(self) -> Option<&'static str> {
        match self {
            Derive::WorkArea(relative) => Some(relative),
            Derive::LocalData(_) | Derive::Temp(_) => None,
        }
    }
}

struct Role {
    key: &'static str,
    kind: Kind,
    derive: Derive,
}

impl Role {
    fn env_var(&self) -> String {
        format!("{ENV_PREFIX}{}", self.key.to_ascii_uppercase())
    }
}

/// The settable roles, in output order. `work_area` must stay first: the
/// derived defaults read it.
const ROLES: &[Role] = &[
    Role {
        key: "work_area",
        kind: Kind::Directory,
        derive: Derive::LocalData("work"),
    },
    Role {
        key: "experiment_root",
        kind: Kind::Directory,
        derive: Derive::WorkArea("experiments"),
    },
    Role {
        key: "scratch_area",
        kind: Kind::Directory,
        derive: Derive::Temp(SCRATCH_DIR),
    },
    Role {
        key: "plans_dir",
        kind: Kind::Directory,
        derive: Derive::WorkArea("plans"),
    },
    Role {
        key: "frozen_bin_dir",
        kind: Kind::Directory,
        derive: Derive::WorkArea("bin"),
    },
    Role {
        key: "evidence_dir",
        kind: Kind::Directory,
        derive: Derive::WorkArea("evidence"),
    },
    Role {
        key: "logs_dir",
        kind: Kind::Directory,
        derive: Derive::WorkArea("logs"),
    },
    Role {
        key: "checkpoint",
        kind: Kind::File,
        derive: Derive::WorkArea("run/checkpoint.md"),
    },
    Role {
        key: "dev_config",
        kind: Kind::File,
        derive: Derive::WorkArea("cfg/dev.toml"),
    },
];

/// Roles derived from the checkout, listed beside the settable ones but not
/// overridable: the acceptance harness computes them the same way.
const FIXED_KEYS: &[&str] = &["repo_root", "test_results"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Env,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::File => "file",
            Source::Env => "env",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: &'static str,
    kind: Kind,
    pub value: PathBuf,
    pub source: Source,
    /// Where the value came from: the variable name, the machine file's path,
    /// or for a default how it is derived.
    pub from: String,
}

/// What was found at the machine file's location.
#[derive(Clone, Debug)]
pub struct FileReport {
    pub path: PathBuf,
    pub present: bool,
    pub from_main_worktree: bool,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub entries: Vec<Entry>,
    pub file: FileReport,
    /// `LCI_DEV_*` variables that set a role.
    pub env_used: Vec<String>,
    /// `LCI_DEV_*` variables that name no role (likely typos).
    pub env_unknown: Vec<String>,
}

impl Resolved {
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.key == key)
    }
}

/// A machine file that was read.
#[derive(Clone, Debug)]
pub struct MachineFile {
    pub path: PathBuf,
    pub text: String,
    pub from_main_worktree: bool,
}

/// The machine file's own location in this checkout and the file found for
/// it, which may be the main worktree's.
#[derive(Clone, Debug)]
pub struct Located {
    pub own_path: PathBuf,
    pub file: Option<MachineFile>,
}

/// The platform facts the built-in defaults need, injected so the resolver is
/// deterministic in tests.
pub struct Platform {
    pub data_local_dir: Option<PathBuf>,
    pub temp_dir: PathBuf,
}

impl Platform {
    fn current() -> Self {
        Self {
            data_local_dir: dirs::data_local_dir(),
            temp_dir: std::env::temp_dir(),
        }
    }
}

pub struct Inputs<'a> {
    pub repo_root: &'a Path,
    pub located: &'a Located,
    /// The `LCI_DEV_*` variables, names upper-cased ([`snapshot_env`]).
    pub env: &'a BTreeMap<String, String>,
    pub platform: &'a Platform,
}

/// Collapses repeated and mixed separators and `.` components (a value written
/// with `/` on Windows prints with `\`). `..` is kept as written.
fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

/// A value as a path: absolute as written, relative against the repository
/// root.
fn resolve_path(raw: &str, root: &Path) -> PathBuf {
    let path = Path::new(raw.trim());
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&root.join(path))
    }
}

fn valid_keys() -> String {
    FIXED_KEYS
        .iter()
        .copied()
        .chain(ROLES.iter().map(|role| role.key))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `LCI_DEV_*` variables of this process, names upper-cased (Windows
/// treats them case-insensitively).
pub fn snapshot_env() -> Result<BTreeMap<String, String>> {
    let mut snapshot = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        let name = name.to_string_lossy().to_ascii_uppercase();
        if !name.starts_with(ENV_PREFIX) {
            continue;
        }
        let value = value
            .into_string()
            .map_err(|_| anyhow!("{name} is not valid unicode"))?;
        snapshot.insert(name, value);
    }
    Ok(snapshot)
}

/// The machine file's roles: every key must name a settable role and hold a
/// non-empty string that is not an unedited `<work_area>` placeholder.
fn parse_machine_file(file: &MachineFile) -> Result<BTreeMap<&'static str, String>> {
    let shown = file.path.display();
    let table: toml::Table =
        toml::from_str(&file.text).with_context(|| format!("parse {shown}"))?;
    let mut values = BTreeMap::new();
    for (key, value) in table {
        let Some(role) = ROLES.iter().find(|role| role.key == key) else {
            if FIXED_KEYS.contains(&key.as_str()) {
                bail!("{shown}: `{key}` is derived from the checkout and cannot be set");
            }
            bail!(
                "{shown}: unknown key `{key}`; valid keys: {}",
                settable_keys()
            );
        };
        let Some(text) = value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            bail!("{shown}: `{key}` must be a non-empty path string");
        };
        if text.contains(WORK_AREA_PLACEHOLDER) {
            bail!(
                "{shown}: `{key}` still holds the `{WORK_AREA_PLACEHOLDER}` placeholder: \
                 replace it with a real path or keep the line commented"
            );
        }
        values.insert(role.key, text.to_owned());
    }
    Ok(values)
}

fn settable_keys() -> String {
    ROLES
        .iter()
        .map(|role| role.key)
        .collect::<Vec<_>>()
        .join(", ")
}

fn default_for(
    role: &Role,
    work_area: Option<&Path>,
    platform: &Platform,
) -> Result<(PathBuf, String)> {
    Ok(match role.derive {
        Derive::LocalData(name) => {
            let base = platform.data_local_dir.as_ref().ok_or_else(|| {
                anyhow!(
                    "no platform data directory is known on this system; set {key} in \
                     {MACHINE_DIR}/{MACHINE_FILE} or {var}",
                    key = role.key,
                    var = role.env_var()
                )
            })?;
            (
                normalize(&base.join(DEV_DIR).join(name)),
                "platform local data directory".to_owned(),
            )
        }
        Derive::Temp(name) => (
            normalize(&platform.temp_dir.join(name)),
            "system temp directory".to_owned(),
        ),
        Derive::WorkArea(relative) => {
            let work_area = work_area.ok_or_else(|| anyhow!("work_area is not resolved yet"))?;
            (
                normalize(&work_area.join(relative)),
                format!("work_area/{relative}"),
            )
        }
    })
}

/// Resolves every role. Pure: reads nothing from the process or the file
/// system.
pub fn resolve(inputs: &Inputs<'_>) -> Result<Resolved> {
    let root = inputs.repo_root;
    let from_file = match &inputs.located.file {
        Some(file) => parse_machine_file(file)?,
        None => BTreeMap::new(),
    };
    let file_origin = inputs
        .located
        .file
        .as_ref()
        .map(|file| file.path.display().to_string())
        .unwrap_or_default();

    let mut entries = vec![
        Entry {
            key: "repo_root",
            kind: Kind::Directory,
            value: normalize(root),
            source: Source::Default,
            from: "the checkout".to_owned(),
        },
        Entry {
            key: "test_results",
            kind: Kind::Directory,
            value: normalize(&root.join("test-results")),
            source: Source::Default,
            from: "repo_root/test-results".to_owned(),
        },
    ];
    let mut env_used = Vec::new();
    let mut work_area: Option<PathBuf> = None;
    for role in ROLES {
        let var = role.env_var();
        let env_value = inputs
            .env
            .get(&var)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty());
        let (value, source, from) = if let Some(raw) = env_value {
            env_used.push(var.clone());
            (resolve_path(raw, root), Source::Env, var)
        } else if let Some(raw) = from_file.get(role.key) {
            (resolve_path(raw, root), Source::File, file_origin.clone())
        } else {
            let (value, from) = default_for(role, work_area.as_deref(), inputs.platform)?;
            (value, Source::Default, from)
        };
        if role.key == "work_area" {
            work_area = Some(value.clone());
        }
        entries.push(Entry {
            key: role.key,
            kind: role.kind,
            value,
            source,
            from,
        });
    }

    let known: Vec<String> = ROLES.iter().map(Role::env_var).collect();
    let env_unknown = inputs
        .env
        .keys()
        .filter(|name| !known.contains(name))
        .cloned()
        .collect();
    Ok(Resolved {
        entries,
        file: FileReport {
            path: inputs
                .located
                .file
                .as_ref()
                .map_or_else(|| inputs.located.own_path.clone(), |file| file.path.clone()),
            present: inputs.located.file.is_some(),
            from_main_worktree: inputs
                .located
                .file
                .as_ref()
                .is_some_and(|file| file.from_main_worktree),
        },
        env_used,
        env_unknown,
    })
}

/// Runs git in `dir` with the variables that redirect it removed, returning
/// its stdout on success.
fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// The main worktree's root when `root` is a linked git worktree: git's
/// common directory (`<main>/.git`) is then not the worktree's own git
/// directory, and its parent is the main worktree. `None` for the main
/// worktree itself, a plain directory, or a layout without a `.git`
/// directory (bare repository, separate git dir) and when git is missing.
fn main_worktree_root(root: &Path) -> Option<PathBuf> {
    let output = git_output(root, &["rev-parse", "--git-common-dir", "--git-dir"])?;
    let mut lines = output.lines();
    let absolute = |line: &str| {
        let path = Path::new(line.trim());
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        }
    };
    let common = absolute(lines.next()?);
    let git_dir = absolute(lines.next()?);
    if same_path(&common, &git_dir) || common.file_name()? != ".git" {
        return None;
    }
    let main = common.parent()?;
    if same_path(main, root) {
        return None;
    }
    Some(normalize(main))
}

fn read_machine_file(path: PathBuf, from_main_worktree: bool) -> Result<MachineFile> {
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read machine file {}", path.display()))?;
    Ok(MachineFile {
        path,
        text,
        from_main_worktree,
    })
}

/// Finds the machine file: this checkout's own, else (in a linked worktree)
/// the main worktree's, else none.
pub fn locate(root: &Path) -> Result<Located> {
    let own_path = root.join(MACHINE_DIR).join(MACHINE_FILE);
    if own_path.is_file() {
        let file = read_machine_file(own_path.clone(), false)?;
        return Ok(Located {
            own_path,
            file: Some(file),
        });
    }
    let file = match main_worktree_root(root) {
        Some(main) => {
            let candidate = main.join(MACHINE_DIR).join(MACHINE_FILE);
            if candidate.is_file() {
                Some(read_machine_file(candidate, true)?)
            } else {
                None
            }
        }
        None => None,
    };
    Ok(Located { own_path, file })
}

fn describe(entry: &Entry) -> String {
    format!("{}: {}", entry.source.as_str(), entry.from)
}

/// The listing: one `key = value  (source: origin)` line per role, then the
/// machine file and environment summary.
pub fn render_text(resolved: &Resolved) -> String {
    let width = resolved
        .entries
        .iter()
        .map(|entry| entry.key.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for entry in &resolved.entries {
        let _ = writeln!(
            out,
            "{:<width$} = {}  ({})",
            entry.key,
            entry.value.display(),
            describe(entry)
        );
    }
    let file = &resolved.file;
    let state = match (file.present, file.from_main_worktree) {
        (true, true) => "present, read from the main worktree",
        (true, false) => "present",
        (false, _) => "absent",
    };
    let _ = writeln!(out, "\nmachine file: {} ({state})", file.path.display());
    if !resolved.env_used.is_empty() {
        let _ = writeln!(out, "environment: {}", resolved.env_used.join(", "));
    }
    if !resolved.env_unknown.is_empty() {
        let _ = writeln!(
            out,
            "ignored (names no role): {}",
            resolved.env_unknown.join(", ")
        );
    }
    out
}

/// The `--json` object; `only` restricts `values` to one role.
pub fn render_json(resolved: &Resolved, only: Option<&str>) -> Value {
    let mut values = Map::new();
    for entry in &resolved.entries {
        if only.is_some_and(|key| key != entry.key) {
            continue;
        }
        values.insert(
            entry.key.to_owned(),
            json!({
                "value": entry.value.display().to_string(),
                "kind": entry.kind.as_str(),
                "source": entry.source.as_str(),
                "from": entry.from,
            }),
        );
    }
    json!({
        "values": values,
        "file": {
            "path": resolved.file.path.display().to_string(),
            "state": if resolved.file.present { "present" } else { "absent" },
            "from_main_worktree": resolved.file.from_main_worktree,
        },
        "env": {
            "used": resolved.env_used,
            "unknown": resolved.env_unknown,
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Exists,
    /// The path exists but is not a directory, or creating it failed.
    Failed,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Exists => "exists",
            Outcome::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Ensured {
    pub key: &'static str,
    /// The directory made sure of: the role's value for a directory role, the
    /// file's directory for a file role.
    pub dir: PathBuf,
    pub of_file: bool,
    pub outcome: Outcome,
    pub detail: Option<String>,
}

/// Makes sure the directory each selected role lives in exists. A directory
/// role is created itself; a file role (the checkpoint, the dev config) gets
/// its directory created and the file is never touched. `only` selects one
/// role; the fixed roles (`repo_root`, `test_results`) are not creatable here.
pub fn ensure_dirs(resolved: &Resolved, only: Option<&str>) -> Result<Vec<Ensured>> {
    if let Some(key) = only
        && !ROLES.iter().any(|role| role.key == key)
    {
        bail!(
            "`{key}` is derived from the checkout and is not created by --ensure; \
             roles: {}",
            settable_keys()
        );
    }
    let mut done = Vec::new();
    for role in ROLES {
        if only.is_some_and(|key| key != role.key) {
            continue;
        }
        let entry = resolved
            .get(role.key)
            .ok_or_else(|| anyhow!("role {} was not resolved", role.key))?;
        let of_file = role.kind == Kind::File;
        let dir = if of_file {
            entry
                .value
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| anyhow!("`{}` has no parent directory", role.key))?
        } else {
            entry.value.clone()
        };
        let (outcome, detail) = if dir.is_dir() {
            (Outcome::Exists, None)
        } else if dir.exists() {
            (
                Outcome::Failed,
                Some("exists and is not a directory".to_owned()),
            )
        } else {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => (Outcome::Created, None),
                Err(error) => (Outcome::Failed, Some(error.to_string())),
            }
        };
        done.push(Ensured {
            key: role.key,
            dir,
            of_file,
            outcome,
            detail,
        });
    }
    Ok(done)
}

pub fn render_ensured_text(done: &[Ensured]) -> String {
    let width = done.iter().map(|item| item.key.len()).max().unwrap_or(0);
    let mut out = String::new();
    for item in done {
        let _ = write!(
            out,
            "{:<7} {:<width$} {}",
            item.outcome.as_str(),
            item.key,
            item.dir.display()
        );
        if item.of_file {
            out.push_str("  (the file's directory; the file is never created)");
        }
        if let Some(detail) = &item.detail {
            let _ = write!(out, "  ({detail})");
        }
        out.push('\n');
    }
    out
}

pub fn render_ensured_json(done: &[Ensured]) -> Value {
    let items: Vec<Value> = done
        .iter()
        .map(|item| {
            json!({
                "key": item.key,
                "path": item.dir.display().to_string(),
                "status": item.outcome.as_str(),
                "directory_of_file_role": item.of_file,
                "detail": item.detail,
            })
        })
        .collect();
    json!({ "ensured": items })
}

/// A TOML string for `text`: a literal string (no escapes, so a Windows path
/// stays readable) unless it contains a character that cannot appear in one.
fn toml_string(text: &str) -> String {
    if text.chars().any(|ch| ch == '\'' || ch.is_control()) {
        toml::Value::String(text.to_owned()).to_string()
    } else {
        format!("'{text}'")
    }
}

/// The text `--init` writes: `work_area` and `scratch_area` set explicitly for
/// this machine, every role that follows the work area listed as a commented
/// line holding its default relative to `<work_area>` (never an absolute
/// value, which would go stale once the work area is edited).
fn init_text(defaults: &Resolved) -> String {
    let mut out = String::new();
    out.push_str(
        "# Development-tooling values for this machine, written by `cargo xtask env --init`.\n\
         # Gitignored (/.local/); never commit it. Roles and rules:\n\
         # docs/development/machine-values.md. A relative value resolves against the\n\
         # repository root; an LCI_DEV_<KEY> environment variable overrides a line here\n\
         # for one process.\n\n",
    );
    for key in ["work_area", "scratch_area"] {
        if let Some(entry) = defaults.get(key) {
            let _ = writeln!(
                out,
                "{key} = {}",
                toml_string(&entry.value.display().to_string())
            );
        }
    }
    let _ = write!(
        out,
        "\n# The roles below follow work_area (the path shown is relative to it) until a\n\
         # line is uncommented. To pin a role somewhere else, uncomment its line and\n\
         # replace {WORK_AREA_PLACEHOLDER} with a real path (a line that still holds it is an\n\
         # error); leave it commented to keep following work_area, wherever work_area\n\
         # points.\n"
    );
    for role in ROLES {
        if let Some(relative) = role.derive.work_area_relative() {
            let _ = writeln!(out, "# {} = '{WORK_AREA_PLACEHOLDER}/{relative}'", role.key);
        }
    }
    out
}

/// Writes `text` to `path`, creating its directory. Without `force` an
/// existing file is an error and is left untouched.
fn write_machine_file(path: &Path, text: &str, force: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if force {
        return std::fs::write(path, text).with_context(|| format!("write {}", path.display()));
    }
    let mut options = std::fs::OpenOptions::new();
    match options.write(true).create_new(true).open(path) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(text.as_bytes())
                .with_context(|| format!("write {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => bail!(
            "{} already exists; edit it, or pass --force to replace it",
            path.display()
        ),
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

/// `--init`: resolves the built-in defaults for `root` and writes them as
/// this checkout's own machine file.
fn init(root: &Path, platform: &Platform, force: bool) -> Result<String> {
    let target = root.join(MACHINE_DIR).join(MACHINE_FILE);
    // In a linked worktree without a file of its own, the main worktree's file
    // is the one in effect; writing this worktree's own file would silently
    // shadow those shared values, so it takes an explicit --force.
    let shadowed = locate(root)?
        .file
        .filter(|file| file.from_main_worktree)
        .map(|file| file.path);
    if let Some(main_file) = &shadowed
        && !force
    {
        bail!(
            "this worktree has no {MACHINE_DIR}/{MACHINE_FILE} of its own and reads the main \
             worktree's ({}); writing one here would shadow those shared values in this \
             worktree. Nothing was written. Pass --force to write this worktree's own file \
             anyway",
            main_file.display()
        );
    }
    let located = Located {
        own_path: target.clone(),
        file: None,
    };
    let defaults = resolve(&Inputs {
        repo_root: root,
        located: &located,
        env: &BTreeMap::new(),
        platform,
    })?;
    write_machine_file(&target, &init_text(&defaults), force)?;
    let mut message = format!(
        "wrote {}\nedit work_area (and uncomment a derived role to move it, replacing \
         {WORK_AREA_PLACEHOLDER} with a real path), then run `cargo xtask env --ensure` to \
         create the directories\n",
        target.display()
    );
    if let Some(main_file) = shadowed {
        let _ = writeln!(
            message,
            "note: in this worktree this file now shadows the main worktree's {}",
            main_file.display()
        );
    }
    Ok(message)
}

pub fn run(args: EnvArgs) -> Result<()> {
    let root = workspace_root();
    let platform = Platform::current();
    if args.init {
        print!("{}", init(&root, &platform, args.force)?);
        return Ok(());
    }
    if let Some(key) = &args.key
        && !FIXED_KEYS.contains(&key.as_str())
        && !ROLES.iter().any(|role| role.key == key)
    {
        bail!("unknown key `{key}`; valid keys: {}", valid_keys());
    }
    let located = locate(&root)?;
    let env = snapshot_env()?;
    let resolved = resolve(&Inputs {
        repo_root: &root,
        located: &located,
        env: &env,
        platform: &platform,
    })?;

    if args.ensure {
        let done = ensure_dirs(&resolved, args.key.as_deref())?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&render_ensured_json(&done))?
            );
        } else {
            print!("{}", render_ensured_text(&done));
        }
        let failed = done
            .iter()
            .filter(|item| item.outcome == Outcome::Failed)
            .count();
        if failed > 0 {
            bail!("{failed} directory(ies) could not be created");
        }
        return Ok(());
    }

    if args.json {
        let report = render_json(&resolved, args.key.as_deref());
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if let Some(key) = &args.key {
        let entry = resolved
            .get(key)
            .ok_or_else(|| anyhow!("role {key} was not resolved"))?;
        println!("{}", entry.value.display());
    } else {
        print!("{}", render_text(&resolved));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A directory under the system temp directory, removed on drop. Its name
    /// avoids the `lci-` prefix that the live-acceptance cleanup deletes.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "xtask-env-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A checkout root and platform that touch nothing on disk.
    struct Fake {
        root: PathBuf,
        platform: Platform,
    }

    impl Fake {
        fn new() -> Self {
            let base = std::env::temp_dir().join("xtask-env-fake");
            Self {
                root: base.join("repo"),
                platform: Platform {
                    data_local_dir: Some(base.join("data")),
                    temp_dir: base.join("temp"),
                },
            }
        }

        fn data(&self) -> PathBuf {
            self.platform.data_local_dir.clone().unwrap()
        }

        fn machine_file(&self, text: &str) -> Located {
            let path = self.root.join(".local").join("machine.toml");
            Located {
                own_path: path.clone(),
                file: Some(MachineFile {
                    path,
                    text: text.to_owned(),
                    from_main_worktree: false,
                }),
            }
        }

        fn no_file(&self) -> Located {
            Located {
                own_path: self.root.join(".local").join("machine.toml"),
                file: None,
            }
        }

        fn resolve(&self, located: &Located, env: &[(&str, &str)]) -> Result<Resolved> {
            let env = env
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            resolve(&Inputs {
                repo_root: &self.root,
                located,
                env: &env,
                platform: &self.platform,
            })
        }
    }

    /// A TOML literal string holding `path`.
    fn lit(path: &Path) -> String {
        format!("'{}'", path.display())
    }

    fn value(resolved: &Resolved, key: &str) -> PathBuf {
        resolved.get(key).unwrap().value.clone()
    }

    fn source(resolved: &Resolved, key: &str) -> Source {
        resolved.get(key).unwrap().source
    }

    #[test]
    fn defaults_derive_from_the_platform_and_the_work_area() {
        let fake = Fake::new();
        let resolved = fake.resolve(&fake.no_file(), &[]).unwrap();
        let work = fake.data().join("local-code-intelligence-dev").join("work");
        assert_eq!(value(&resolved, "repo_root"), fake.root);
        assert_eq!(
            value(&resolved, "test_results"),
            fake.root.join("test-results")
        );
        assert_eq!(value(&resolved, "work_area"), work);
        assert_eq!(
            value(&resolved, "experiment_root"),
            work.join("experiments")
        );
        assert_eq!(value(&resolved, "plans_dir"), work.join("plans"));
        assert_eq!(value(&resolved, "frozen_bin_dir"), work.join("bin"));
        assert_eq!(value(&resolved, "evidence_dir"), work.join("evidence"));
        assert_eq!(value(&resolved, "logs_dir"), work.join("logs"));
        assert_eq!(
            value(&resolved, "checkpoint"),
            work.join("run").join("checkpoint.md")
        );
        assert_eq!(
            value(&resolved, "dev_config"),
            work.join("cfg").join("dev.toml")
        );
        assert_eq!(
            value(&resolved, "scratch_area"),
            fake.platform
                .temp_dir
                .join("local-code-intelligence-scratch")
        );
        assert!(
            resolved
                .entries
                .iter()
                .all(|entry| entry.source == Source::Default)
        );
        assert!(!resolved.file.present);
        assert!(!resolved.file.from_main_worktree);
        assert!(resolved.env_used.is_empty() && resolved.env_unknown.is_empty());
    }

    #[test]
    fn every_role_is_listed_once_and_the_scratch_area_cannot_be_swept_by_live_acceptance() {
        let mut keys: Vec<&str> = ROLES.iter().map(|role| role.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ROLES.len());
        assert_eq!(ROLES[0].key, "work_area", "derived defaults read it first");
        assert!(!SCRATCH_DIR.starts_with("lci-"));
    }

    #[test]
    fn a_work_area_in_the_file_moves_every_derived_role_unless_it_is_set_itself() {
        let fake = Fake::new();
        let elsewhere = std::env::temp_dir().join("xtask-env-elsewhere");
        let pinned = std::env::temp_dir().join("xtask-env-pinned");
        let text = format!(
            "work_area = {}\nplans_dir = {}\n",
            lit(&elsewhere),
            lit(&pinned)
        );
        let resolved = fake.resolve(&fake.machine_file(&text), &[]).unwrap();
        assert_eq!(value(&resolved, "work_area"), elsewhere);
        assert_eq!(source(&resolved, "work_area"), Source::File);
        assert_eq!(value(&resolved, "plans_dir"), pinned);
        assert_eq!(source(&resolved, "plans_dir"), Source::File);
        for (key, relative) in [
            ("experiment_root", "experiments"),
            ("frozen_bin_dir", "bin"),
            ("evidence_dir", "evidence"),
            ("logs_dir", "logs"),
        ] {
            assert_eq!(value(&resolved, key), elsewhere.join(relative), "{key}");
            assert_eq!(source(&resolved, key), Source::Default, "{key}");
        }
        assert_eq!(
            value(&resolved, "checkpoint"),
            elsewhere.join("run").join("checkpoint.md")
        );
        assert!(resolved.file.present);
        assert!(
            resolved
                .get("work_area")
                .unwrap()
                .from
                .ends_with("machine.toml")
        );
    }

    #[test]
    fn the_environment_beats_the_file_and_an_empty_value_is_unset() {
        let fake = Fake::new();
        let from_file = std::env::temp_dir().join("xtask-env-file-wins-not");
        let from_env = std::env::temp_dir().join("xtask-env-env-wins");
        let text = format!(
            "work_area = {}\nlogs_dir = {}\n",
            lit(&from_file),
            lit(&from_file)
        );
        let resolved = fake
            .resolve(
                &fake.machine_file(&text),
                &[
                    ("LCI_DEV_WORK_AREA", from_env.to_str().unwrap()),
                    ("LCI_DEV_LOGS_DIR", ""),
                    ("LCI_DEV_PLANS_DIR", "   "),
                ],
            )
            .unwrap();
        assert_eq!(value(&resolved, "work_area"), from_env);
        assert_eq!(source(&resolved, "work_area"), Source::Env);
        assert_eq!(resolved.get("work_area").unwrap().from, "LCI_DEV_WORK_AREA");
        // The empty variable leaves the file's value; the blank one the default.
        assert_eq!(value(&resolved, "logs_dir"), from_file);
        assert_eq!(source(&resolved, "logs_dir"), Source::File);
        assert_eq!(value(&resolved, "plans_dir"), from_env.join("plans"));
        assert_eq!(source(&resolved, "plans_dir"), Source::Default);
        assert_eq!(resolved.env_used, ["LCI_DEV_WORK_AREA"]);
    }

    #[test]
    fn relative_values_resolve_against_the_repository_root() {
        let fake = Fake::new();
        let resolved = fake
            .resolve(
                &fake.machine_file("work_area = 'local/work'\n"),
                &[("LCI_DEV_LOGS_DIR", "out/logs")],
            )
            .unwrap();
        assert_eq!(value(&resolved, "work_area"), fake.root.join("local/work"));
        assert_eq!(value(&resolved, "logs_dir"), fake.root.join("out/logs"));
        assert_eq!(
            value(&resolved, "plans_dir"),
            fake.root.join("local/work/plans")
        );
    }

    #[test]
    fn an_unknown_key_is_an_error_naming_it_and_the_file() {
        let fake = Fake::new();
        let error = fake
            .resolve(&fake.machine_file("work_area = 'x'\nplan_dir = 'y'\n"), &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("`plan_dir`"), "{error}");
        assert!(error.contains("machine.toml"), "{error}");
        assert!(error.contains("plans_dir"), "lists the valid keys: {error}");
    }

    #[test]
    fn derived_roles_and_bad_values_are_rejected() {
        let fake = Fake::new();
        let error = fake
            .resolve(&fake.machine_file("test_results = 'x'\n"), &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("`test_results`") && error.contains("cannot be set"));
        for bad in ["work_area = 3\n", "work_area = ''\n", "work_area = '  '\n"] {
            let error = fake
                .resolve(&fake.machine_file(bad), &[])
                .unwrap_err()
                .to_string();
            assert!(error.contains("non-empty path string"), "{bad}: {error}");
        }
        let error = fake
            .resolve(&fake.machine_file("work_area = \n"), &[])
            .unwrap_err();
        assert!(format!("{error:#}").contains("machine.toml"));
    }

    #[test]
    fn a_missing_platform_data_directory_only_matters_without_an_explicit_work_area() {
        let mut fake = Fake::new();
        fake.platform.data_local_dir = None;
        let error = fake.resolve(&fake.no_file(), &[]).unwrap_err().to_string();
        assert!(error.contains("work_area") && error.contains("LCI_DEV_WORK_AREA"));
        let explicit = std::env::temp_dir().join("xtask-env-explicit");
        let resolved = fake
            .resolve(
                &fake.no_file(),
                &[("LCI_DEV_WORK_AREA", explicit.to_str().unwrap())],
            )
            .unwrap();
        assert_eq!(value(&resolved, "work_area"), explicit);
    }

    #[test]
    fn an_unknown_dev_variable_is_reported_not_applied() {
        let fake = Fake::new();
        let resolved = fake
            .resolve(&fake.no_file(), &[("LCI_DEV_WORKAREA", "/nowhere")])
            .unwrap();
        assert_eq!(resolved.env_unknown, ["LCI_DEV_WORKAREA"]);
        assert!(resolved.env_used.is_empty());
        assert_eq!(source(&resolved, "work_area"), Source::Default);
    }

    #[test]
    fn json_lists_every_role_with_its_source_and_the_file_state() {
        let fake = Fake::new();
        let resolved = fake.resolve(&fake.no_file(), &[]).unwrap();
        let report = render_json(&resolved, None);
        let values = report["values"].as_object().unwrap();
        assert_eq!(values.len(), FIXED_KEYS.len() + ROLES.len());
        for (key, entry) in values {
            assert_eq!(entry["source"], "default", "{key}");
            assert!(entry["value"].as_str().is_some_and(|text| !text.is_empty()));
        }
        assert_eq!(values["checkpoint"]["kind"], "file");
        assert_eq!(values["work_area"]["kind"], "directory");
        assert_eq!(report["file"]["state"], "absent");
        assert_eq!(report["file"]["from_main_worktree"], false);
        assert_eq!(report["env"]["used"], json!([]));

        let one = render_json(&resolved, Some("plans_dir"));
        assert_eq!(one["values"].as_object().unwrap().len(), 1);
        assert!(one["values"]["plans_dir"]["value"].is_string());
    }

    #[test]
    fn text_lists_one_line_per_role_with_its_origin() {
        let fake = Fake::new();
        let resolved = fake
            .resolve(&fake.no_file(), &[("LCI_DEV_LOGS_DIR", "out")])
            .unwrap();
        let text = render_text(&resolved);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("repo_root"));
        assert!(
            lines.iter().any(|line| line.starts_with("plans_dir")
                && line.ends_with("(default: work_area/plans)"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("logs_dir")
                    && line.ends_with("(env: LCI_DEV_LOGS_DIR)"))
        );
        assert!(text.contains("machine file:") && text.contains("(absent)"));
        assert!(text.contains("environment: LCI_DEV_LOGS_DIR"));
    }

    #[test]
    fn ensure_creates_missing_directories_and_never_a_file() {
        let tmp = TempDir::new();
        let fake = Fake::new();
        let work = tmp.path().join("work");
        let scratch = tmp.path().join("scratch");
        let text = format!(
            "work_area = {}\nscratch_area = {}\n",
            lit(&work),
            lit(&scratch)
        );
        let resolved = fake.resolve(&fake.machine_file(&text), &[]).unwrap();

        let first = ensure_dirs(&resolved, None).unwrap();
        assert!(first.iter().all(|item| item.outcome == Outcome::Created));
        for dir in [
            "experiments",
            "plans",
            "bin",
            "evidence",
            "logs",
            "run",
            "cfg",
        ] {
            assert!(work.join(dir).is_dir(), "{dir}");
        }
        assert!(scratch.is_dir());
        assert!(!work.join("run").join("checkpoint.md").exists());
        assert!(!work.join("cfg").join("dev.toml").exists());
        let checkpoint = first.iter().find(|item| item.key == "checkpoint").unwrap();
        assert!(checkpoint.of_file);
        assert_eq!(checkpoint.dir, work.join("run"));

        // A second run changes nothing and says so.
        let second = ensure_dirs(&resolved, None).unwrap();
        assert!(second.iter().all(|item| item.outcome == Outcome::Exists));
        let text = render_ensured_text(&first);
        assert!(
            text.lines()
                .any(|line| line.starts_with("created") && line.contains("plans_dir"))
        );
        assert_eq!(
            render_ensured_json(&second)["ensured"][0]["status"],
            "exists"
        );
    }

    #[test]
    fn ensure_with_a_key_touches_only_that_role() {
        let tmp = TempDir::new();
        let fake = Fake::new();
        let work = tmp.path().join("work");
        let text = format!("work_area = {}\n", lit(&work));
        let resolved = fake.resolve(&fake.machine_file(&text), &[]).unwrap();
        let done = ensure_dirs(&resolved, Some("plans_dir")).unwrap();
        assert_eq!(done.len(), 1);
        assert!(work.join("plans").is_dir());
        assert!(!work.join("logs").exists());
        // The work area itself came into being only as plans_dir's parent.
        assert!(work.is_dir());
        for fixed in ["repo_root", "test_results"] {
            let error = ensure_dirs(&resolved, Some(fixed)).unwrap_err().to_string();
            assert!(
                error.contains(fixed) && error.contains("--ensure"),
                "{error}"
            );
        }
    }

    #[test]
    fn ensure_reports_a_file_standing_where_a_directory_belongs() {
        let tmp = TempDir::new();
        let fake = Fake::new();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("plans"), "not a directory").unwrap();
        let scratch = tmp.path().join("scratch");
        let text = format!(
            "work_area = {}\nscratch_area = {}\n",
            lit(&work),
            lit(&scratch)
        );
        let resolved = fake.resolve(&fake.machine_file(&text), &[]).unwrap();
        let done = ensure_dirs(&resolved, None).unwrap();
        let plans = done.iter().find(|item| item.key == "plans_dir").unwrap();
        assert_eq!(plans.outcome, Outcome::Failed);
        assert!(plans.detail.as_deref().unwrap().contains("not a directory"));
        // The blocked path is left alone and the other roles are still made.
        assert_eq!(
            std::fs::read_to_string(work.join("plans")).unwrap(),
            "not a directory"
        );
        assert!(work.join("logs").is_dir());
    }

    #[test]
    fn init_text_pins_the_work_area_and_scratch_and_round_trips() {
        let fake = Fake::new();
        let defaults = fake.resolve(&fake.no_file(), &[]).unwrap();
        let text = init_text(&defaults);
        let table: toml::Table = toml::from_str(&text).unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["scratch_area", "work_area"]);
        // The derived roles are shown as comments, so they keep following the
        // work area. Each line is role-relative, built from the same default
        // the resolver uses.
        assert!(text.contains("# plans_dir = '<work_area>/plans'\n"));
        assert!(!text.contains("\nplans_dir"));
        for role in ROLES {
            match role.derive.work_area_relative() {
                Some(relative) => assert!(
                    text.contains(&format!("\n# {} = '<work_area>/{relative}'\n", role.key)),
                    "{}",
                    role.key
                ),
                None => assert!(
                    text.contains(&format!("\n{} = '", role.key)),
                    "{}",
                    role.key
                ),
            }
        }
        // No commented line carries a value computed at init time: it would
        // be wrong as soon as work_area is edited.
        let work_area = value(&defaults, "work_area").display().to_string();
        for line in text.lines().filter(|line| line.starts_with('#')) {
            assert!(!line.contains(&work_area), "{line}");
        }

        let located = fake.machine_file(&text);
        let again = fake.resolve(&located, &[]).unwrap();
        for entry in &defaults.entries {
            assert_eq!(value(&again, entry.key), entry.value, "{}", entry.key);
        }
        assert_eq!(source(&again, "work_area"), Source::File);
        assert_eq!(source(&again, "plans_dir"), Source::Default);
    }

    /// `text` with the leading `# ` removed from every commented role line
    /// (`# plans_dir = '...'`), leaving the prose comments alone.
    fn uncomment_role_lines(text: &str) -> String {
        text.lines()
            .map(|line| {
                let role_line = line.strip_prefix("# ").filter(|rest| {
                    ROLES
                        .iter()
                        .any(|role| rest.starts_with(&format!("{} = ", role.key)))
                });
                format!("{}\n", role_line.unwrap_or(line))
            })
            .collect()
    }

    #[test]
    fn an_uncommented_work_area_placeholder_is_an_error_naming_the_key_and_the_file() {
        let fake = Fake::new();
        let line = format!("plans_dir = '{WORK_AREA_PLACEHOLDER}/plans'\n");
        let error = fake
            .resolve(&fake.machine_file(&line), &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("`plans_dir`"), "{error}");
        assert!(error.contains("machine.toml"), "{error}");
        assert!(error.contains(WORK_AREA_PLACEHOLDER), "{error}");
        assert!(error.contains("commented"), "{error}");
        // The placeholder is rejected wherever it sits in a value and under
        // any key, while the same line with a real path is accepted.
        let error = fake
            .resolve(
                &fake.machine_file(&format!("work_area = 'x/{WORK_AREA_PLACEHOLDER}'\n")),
                &[],
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("`work_area`"), "{error}");
        let resolved = fake
            .resolve(&fake.machine_file("plans_dir = 'elsewhere/plans'\n"), &[])
            .unwrap();
        assert_eq!(
            value(&resolved, "plans_dir"),
            fake.root.join("elsewhere/plans")
        );
    }

    #[test]
    fn init_text_parses_untouched_and_is_rejected_once_its_lines_are_uncommented() {
        let fake = Fake::new();
        let defaults = fake.resolve(&fake.no_file(), &[]).unwrap();
        let text = init_text(&defaults);
        fake.resolve(&fake.machine_file(&text), &[]).unwrap();

        // Every commented line at once.
        let uncommented = uncomment_role_lines(&text);
        assert_ne!(uncommented, text);
        let error = fake
            .resolve(&fake.machine_file(&uncommented), &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains(WORK_AREA_PLACEHOLDER), "{error}");

        // Each commented line on its own, so every derived role is covered
        // and the error names that role.
        let mut checked = 0;
        for role in ROLES {
            let Some(relative) = role.derive.work_area_relative() else {
                continue;
            };
            let commented = format!("# {} = '{WORK_AREA_PLACEHOLDER}/{relative}'", role.key);
            assert!(text.contains(&commented), "{}", role.key);
            let one = text.replace(&commented, commented.trim_start_matches("# "));
            let error = fake
                .resolve(&fake.machine_file(&one), &[])
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("`{}`", role.key)), "{error}");
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[test]
    fn toml_strings_stay_literal_unless_they_cannot_be() {
        assert_eq!(toml_string("C:\\a\\b"), "'C:\\a\\b'");
        let quoted = toml_string("it's");
        let parsed: toml::Table = toml::from_str(&format!("k = {quoted}")).unwrap();
        assert_eq!(parsed["k"].as_str(), Some("it's"));
    }

    #[test]
    fn init_writes_once_and_replaces_only_with_force() {
        let tmp = TempDir::new();
        let platform = Platform {
            data_local_dir: Some(tmp.path().join("data")),
            temp_dir: tmp.path().join("temp"),
        };
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join(".local").join("machine.toml");

        let message = init(&root, &platform, false).unwrap();
        assert!(message.contains("--ensure"));
        assert!(message.contains(WORK_AREA_PLACEHOLDER), "{message}");
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.contains("work_area = "));

        std::fs::write(&target, "work_area = 'mine'\n").unwrap();
        let error = init(&root, &platform, false).unwrap_err().to_string();
        assert!(error.contains("already exists") && error.contains("--force"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "work_area = 'mine'\n"
        );

        init(&root, &platform, true).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), written);
    }

    #[test]
    fn locate_reads_the_checkouts_own_file_or_reports_it_absent() {
        let tmp = TempDir::new();
        let absent = locate(tmp.path()).unwrap();
        assert!(absent.file.is_none());
        assert_eq!(
            absent.own_path,
            tmp.path().join(".local").join("machine.toml")
        );

        std::fs::create_dir_all(tmp.path().join(".local")).unwrap();
        std::fs::write(absent.own_path.clone(), "work_area = 'w'\n").unwrap();
        let found = locate(tmp.path()).unwrap();
        let file = found.file.unwrap();
        assert!(!file.from_main_worktree);
        assert_eq!(file.path, absent.own_path);
        assert_eq!(file.text, "work_area = 'w'\n");
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git is required for the worktree tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A main worktree with one commit and a linked worktree beside it.
    fn repo_with_linked_worktree(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let main = tmp.path().join("main");
        let linked = tmp.path().join("linked");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "initial"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        (main, linked)
    }

    fn write_machine(root: &Path, text: &str) -> PathBuf {
        let path = root.join(".local").join("machine.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_linked_worktree_falls_back_to_the_main_worktrees_file() {
        let tmp = TempDir::new();
        let (main, linked) = repo_with_linked_worktree(&tmp);
        let main_file = write_machine(&main, "work_area = 'from-main'\n");

        let located = locate(&linked).unwrap();
        let file = located.file.expect("the main worktree's file is used");
        assert!(file.from_main_worktree);
        assert_eq!(
            std::fs::canonicalize(&file.path).unwrap(),
            std::fs::canonicalize(&main_file).unwrap()
        );
        assert_eq!(file.text, "work_area = 'from-main'\n");
        // The checkout's own location is still the linked worktree's, so
        // `--init` there writes its own file.
        assert_eq!(located.own_path, linked.join(".local").join("machine.toml"));

        // Resolution reports which file was used.
        let fake = Fake::new();
        let located = locate(&linked).unwrap();
        let resolved = fake.resolve(&located, &[]).unwrap();
        assert!(resolved.file.from_main_worktree && resolved.file.present);
        assert_eq!(value(&resolved, "work_area"), fake.root.join("from-main"));
        let report = render_json(&resolved, None);
        assert_eq!(report["file"]["from_main_worktree"], true);
        assert_eq!(report["file"]["state"], "present");
        assert!(render_text(&resolved).contains("read from the main worktree"));
    }

    #[test]
    fn a_linked_worktrees_own_file_wins_and_the_main_worktree_reads_its_own() {
        let tmp = TempDir::new();
        let (main, linked) = repo_with_linked_worktree(&tmp);
        write_machine(&main, "work_area = 'from-main'\n");
        let own = write_machine(&linked, "work_area = 'from-linked'\n");

        let file = locate(&linked).unwrap().file.unwrap();
        assert!(!file.from_main_worktree);
        assert_eq!(file.path, own);
        assert_eq!(file.text, "work_area = 'from-linked'\n");

        let file = locate(&main).unwrap().file.unwrap();
        assert!(!file.from_main_worktree);
        assert_eq!(file.text, "work_area = 'from-main'\n");
    }

    #[test]
    fn a_linked_worktree_without_any_file_reports_it_absent() {
        let tmp = TempDir::new();
        let (_main, linked) = repo_with_linked_worktree(&tmp);
        let located = locate(&linked).unwrap();
        assert!(located.file.is_none());
        assert_eq!(located.own_path, linked.join(".local").join("machine.toml"));
    }

    #[test]
    fn init_in_a_linked_worktree_does_not_shadow_the_main_worktrees_file_without_force() {
        let tmp = TempDir::new();
        let platform = Platform {
            data_local_dir: Some(tmp.path().join("data")),
            temp_dir: tmp.path().join("temp"),
        };
        let (main, linked) = repo_with_linked_worktree(&tmp);
        let main_file = write_machine(&main, "work_area = 'from-main'\n");
        let own = linked.join(".local").join("machine.toml");

        let error = init(&linked, &platform, false).unwrap_err().to_string();
        assert!(
            error.contains("main worktree")
                && error.contains("shadow")
                && error.contains("Nothing was written")
                && error.contains("--force"),
            "{error}"
        );
        assert!(!own.exists());

        let message = init(&linked, &platform, true).unwrap();
        assert!(message.contains("note: in this worktree this file now shadows"));
        assert!(own.is_file());
        // The shared file is untouched; this worktree now reads its own.
        assert_eq!(
            std::fs::read_to_string(&main_file).unwrap(),
            "work_area = 'from-main'\n"
        );
        let file = locate(&linked).unwrap().file.unwrap();
        assert!(!file.from_main_worktree);
        assert_eq!(file.path, own);
    }

    #[test]
    fn init_in_a_worktree_with_nothing_to_shadow_needs_no_force() {
        let tmp = TempDir::new();
        let platform = Platform {
            data_local_dir: Some(tmp.path().join("data")),
            temp_dir: tmp.path().join("temp"),
        };
        let (main, linked) = repo_with_linked_worktree(&tmp);
        let message = init(&linked, &platform, false).unwrap();
        assert!(!message.contains("shadows"));
        assert!(linked.join(".local").join("machine.toml").is_file());
        assert!(
            !main.join(".local").exists(),
            "the main worktree is not touched"
        );
    }

    #[test]
    fn the_repositorys_gitignore_covers_the_machine_file() {
        let tmp = TempDir::new();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), include_str!("../../.gitignore")).unwrap();
        let platform = Platform {
            data_local_dir: Some(tmp.path().join("data")),
            temp_dir: tmp.path().join("temp"),
        };
        init(&repo, &platform, false).unwrap();
        // `git check-ignore -q` exits 0 only when the path is ignored.
        git(&repo, &["check-ignore", "-q", ".local/machine.toml"]);
    }
}
