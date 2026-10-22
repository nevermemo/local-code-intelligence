//! Layered resolution of the service configuration.
//!
//! A setting takes its value from the highest of these layers that sets it:
//!
//! 1. a command-line flag (`serve --port`),
//! 2. an `LCI_<KEY>` environment variable,
//! 3. the local overlay file,
//! 4. the configuration file (`--config`, else `LCI_CONFIG`),
//! 5. the built-in default ([`Config::default`]).
//!
//! Resolution is per key, not per file: an overlay that sets `data_dir`
//! leaves the file's `embedding_url` alone. Only the machine set -- values
//! that describe this machine rather than the service's behaviour (paths,
//! the port, the model endpoints and names, the language servers) -- may come
//! from the overlay or the environment. Every other key there is an error,
//! so a local file can never change ranking. The configuration file keeps
//! its full schema and `deny_unknown_fields`.
//!
//! With no overlay file and no `LCI_*` variable the result is exactly what
//! [`Config::load`] returns for the same file: the file layer parses the file
//! with the same call, the other layers apply an empty patch, and
//! [`Config::validate`] runs once, on the resolved value.
//!
//! The environment is passed in as a snapshot ([`ResolveSpec::env`]), never
//! read inside the resolver, so every rule here is testable without touching
//! the process environment.
//!
//! [`render_overlay`] is the other direction: it writes a [`LocalOverrides`]
//! (with comments) as the text of an overlay file, for `lci init`.

use super::Config;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::ErrorKind,
    path::{Path, PathBuf},
};

/// Names the configuration file when `--config` is absent.
pub const CONFIG_ENV: &str = "LCI_CONFIG";
/// Names the local overlay file, replacing the default location.
pub const LOCAL_CONFIG_ENV: &str = "LCI_LOCAL_CONFIG";
/// Prefix of every environment variable this module reads.
pub const ENV_PREFIX: &str = "LCI_";

/// [`FileLayer::named_by`] when the file came from `--config`.
pub const NAMED_BY_FLAG: &str = "--config";
/// [`LocalLayer::named_by`] when the overlay sits beside the configuration file.
pub const NAMED_BY_SIBLING: &str = "config-file-sibling";
/// [`LocalLayer::named_by`] when the overlay is the user-level file.
pub const NAMED_BY_USER_DIR: &str = "user-config-dir";

/// Directory, under the platform configuration directory, of the user-level
/// overlay.
const USER_CONFIG_SUBDIR: &str = "local-code-intelligence";
/// File name of the user-level overlay.
const USER_OVERLAY_FILE: &str = "config.local.toml";
/// Suffix that turns a configuration file's stem into its overlay's name.
const OVERLAY_SUFFIX: &str = ".local.toml";

/// The machine-set keys that are plain top-level values, each with the
/// environment variable that overrides it.
const SCALAR_KEYS: [(&str, &str); 8] = [
    ("data_dir", "LCI_DATA_DIR"),
    ("port", "LCI_PORT"),
    ("embedding_url", "LCI_EMBEDDING_URL"),
    ("embedding_model", "LCI_EMBEDDING_MODEL"),
    ("reranker_url", "LCI_RERANKER_URL"),
    ("reranker_model", "LCI_RERANKER_MODEL"),
    ("ripgrep_path", "LCI_RIPGREP_PATH"),
    ("rust_analyzer_path", "LCI_RUST_ANALYZER_PATH"),
];

/// The language-server sections of the machine set, each with the upper-case
/// name its environment variables use (`LCI_<NAME>_PATH`, `LCI_<NAME>_DISABLED`).
const LSP_SECTIONS: [(&str, &str); 7] = [
    ("csharp", "CSHARP"),
    ("typescript", "TYPESCRIPT"),
    ("python", "PYTHON"),
    ("go", "GO"),
    ("java", "JAVA"),
    ("clangd", "CLANGD"),
    ("markdown", "MARKDOWN"),
];

/// The keys of a language-server section, all of which the machine set takes.
const LSP_FIELDS: [&str; 3] = ["path", "args", "disabled"];

/// Where a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The built-in default.
    Default,
    /// The configuration file.
    File,
    /// The local overlay.
    Local,
    /// An `LCI_*` environment variable.
    Env,
    /// A command-line flag.
    Flag,
}

impl Source {
    /// The lower-case name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
            Self::Local => "local",
            Self::Env => "env",
            Self::Flag => "flag",
        }
    }
}

/// The layer a value came from and where in that layer: the file path for
/// `file` and `local`, the variable name for `env`, the flag for `flag`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Provenance {
    pub source: Source,
    pub from: Option<String>,
}

impl Provenance {
    fn of(source: Source, from: impl Into<String>) -> Self {
        Self {
            source,
            from: Some(from.into()),
        }
    }

    fn default_value() -> Self {
        Self {
            source: Source::Default,
            from: None,
        }
    }
}

/// Command-line flags that set configuration values.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    /// `serve --port`.
    pub port: Option<u16>,
    /// `--no-local-config`: ignore the local overlay.
    pub no_local_config: bool,
}

/// Everything resolution reads from outside the files it opens.
#[derive(Debug, Clone)]
pub struct ResolveSpec {
    /// The `--config` path.
    pub config_path: Option<PathBuf>,
    /// The `LCI_*` variables of the environment (see [`snapshot_env`]).
    pub env: BTreeMap<String, String>,
    /// The platform's per-user configuration directory, where the user-level
    /// overlay lives; `None` when the platform has none.
    pub user_config_dir: Option<PathBuf>,
    pub flags: Flags,
}

impl ResolveSpec {
    /// The spec for this process: its `LCI_*` variables and the platform's
    /// configuration directory.
    pub fn from_process(config_path: Option<PathBuf>, flags: Flags) -> Result<Self> {
        Ok(Self {
            config_path,
            env: snapshot_env()?,
            user_config_dir: dirs::config_dir(),
            flags,
        })
    }
}

/// The `LCI_*` variables of the process environment. On Windows, where
/// variable names are case-insensitive, the names are upper-cased. A variable
/// this module reads whose value is not valid Unicode is an error; any other
/// is kept lossily.
pub fn snapshot_env() -> Result<BTreeMap<String, String>> {
    let recognized = recognized_env_names();
    let mut env = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        let key = if cfg!(windows) {
            name.to_ascii_uppercase()
        } else {
            name.to_owned()
        };
        if !key.starts_with(ENV_PREFIX) {
            continue;
        }
        let value = match value.into_string() {
            Ok(value) => value,
            Err(raw) if recognized.contains(&key) => {
                bail!("{key} is not valid Unicode: {raw:?}")
            }
            Err(raw) => raw.to_string_lossy().into_owned(),
        };
        env.insert(key, value);
    }
    Ok(env)
}

/// Every environment variable this module reads.
fn recognized_env_names() -> Vec<String> {
    let mut names = vec![CONFIG_ENV.to_owned(), LOCAL_CONFIG_ENV.to_owned()];
    names.extend(SCALAR_KEYS.iter().map(|(_, var)| (*var).to_owned()));
    for (_, upper) in LSP_SECTIONS {
        names.push(format!("{ENV_PREFIX}{upper}_PATH"));
        names.push(format!("{ENV_PREFIX}{upper}_DISABLED"));
    }
    names
}

/// The dotted machine-set keys, in the order reports list them.
pub fn machine_keys() -> Vec<String> {
    let mut keys: Vec<String> = SCALAR_KEYS
        .iter()
        .map(|(key, _)| (*key).to_owned())
        .collect();
    for (section, _) in LSP_SECTIONS {
        for field in LSP_FIELDS {
            keys.push(format!("{section}.{field}"));
        }
    }
    keys
}

/// The environment variable that overrides a machine-set key; `None` for keys
/// with no variable (`<section>.args`).
pub fn env_var_for_key(key: &str) -> Option<String> {
    if let Some((_, var)) = SCALAR_KEYS.iter().find(|(name, _)| *name == key) {
        return Some((*var).to_owned());
    }
    let (section, field) = key.split_once('.')?;
    let (_, upper) = LSP_SECTIONS.iter().find(|(name, _)| *name == section)?;
    match field {
        "path" | "disabled" => Some(format!(
            "{ENV_PREFIX}{upper}_{}",
            field.to_ascii_uppercase()
        )),
        _ => None,
    }
}

/// A language-server section of the overlay (or of the environment). Every
/// field is optional: only what is set overrides the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspOverride {
    pub path: Option<String>,
    pub args: Option<Vec<String>>,
    pub disabled: Option<bool>,
}

/// The machine set: the values the local overlay and the environment may
/// override. Every field is optional; a patch with nothing set changes
/// nothing. Adding a machine-specific key later means adding a field here, a
/// line in `SCALAR_KEYS` (or `LSP_SECTIONS`), its assignment in `apply`, and
/// its variable in `from_env`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalOverrides {
    pub data_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub embedding_url: Option<String>,
    pub embedding_model: Option<String>,
    pub reranker_url: Option<String>,
    pub reranker_model: Option<String>,
    pub ripgrep_path: Option<String>,
    pub rust_analyzer_path: Option<String>,
    pub csharp: Option<LspOverride>,
    pub typescript: Option<LspOverride>,
    pub python: Option<LspOverride>,
    pub go: Option<LspOverride>,
    pub java: Option<LspOverride>,
    pub clangd: Option<LspOverride>,
    pub markdown: Option<LspOverride>,
}

impl LocalOverrides {
    /// Parses the text of an overlay file. A key outside the machine set is an
    /// error that names it; a value of the wrong type is an error with its
    /// location.
    pub fn parse(text: &str) -> Result<Self> {
        let table: toml::Table = text.parse()?;
        check_machine_keys(&table)?;
        Ok(toml::from_str(text)?)
    }

    /// The patch the environment describes. An empty variable counts as
    /// unset; a malformed value is an error naming the variable.
    pub fn from_env(env: &BTreeMap<String, String>) -> Result<Self> {
        let text = |name: &str| env_value(env, name).map(str::to_owned);
        let mut patch = Self {
            data_dir: env_value(env, "LCI_DATA_DIR").map(PathBuf::from),
            port: env_value(env, "LCI_PORT")
                .map(|value| parse_port("LCI_PORT", value))
                .transpose()?,
            embedding_url: text("LCI_EMBEDDING_URL"),
            embedding_model: text("LCI_EMBEDDING_MODEL"),
            reranker_url: text("LCI_RERANKER_URL"),
            reranker_model: text("LCI_RERANKER_MODEL"),
            ripgrep_path: text("LCI_RIPGREP_PATH"),
            rust_analyzer_path: text("LCI_RUST_ANALYZER_PATH"),
            ..Self::default()
        };
        for (section, upper) in LSP_SECTIONS {
            let path = text(&format!("{ENV_PREFIX}{upper}_PATH"));
            let disabled_name = format!("{ENV_PREFIX}{upper}_DISABLED");
            let disabled = env_value(env, &disabled_name)
                .map(|value| parse_flag(&disabled_name, value))
                .transpose()?;
            if path.is_some() || disabled.is_some() {
                *patch.lsp_mut(section) = Some(LspOverride {
                    path,
                    args: None,
                    disabled,
                });
            }
        }
        Ok(patch)
    }

    fn lsp(&self, section: &str) -> Option<&LspOverride> {
        match section {
            "csharp" => self.csharp.as_ref(),
            "typescript" => self.typescript.as_ref(),
            "python" => self.python.as_ref(),
            "go" => self.go.as_ref(),
            "java" => self.java.as_ref(),
            "clangd" => self.clangd.as_ref(),
            "markdown" => self.markdown.as_ref(),
            _ => None,
        }
    }

    /// The slot of language-server section `section` (`csharp`, `typescript`,
    /// `python`, `go`, `java`, `clangd` or `markdown`).
    ///
    /// # Panics
    ///
    /// When `section` is not one of those.
    pub fn lsp_mut(&mut self, section: &str) -> &mut Option<LspOverride> {
        match section {
            "csharp" => &mut self.csharp,
            "typescript" => &mut self.typescript,
            "python" => &mut self.python,
            "go" => &mut self.go,
            "java" => &mut self.java,
            "clangd" => &mut self.clangd,
            "markdown" => &mut self.markdown,
            _ => unreachable!("{section} is not a language-server section"),
        }
    }

    /// Writes every set field into `config` and returns the dotted keys it
    /// wrote.
    pub fn apply(&self, config: &mut Config) -> Vec<String> {
        let mut set = Vec::new();
        if let Some(value) = &self.data_dir {
            config.data_dir = value.clone();
            set.push("data_dir".to_owned());
        }
        if let Some(value) = self.port {
            config.port = value;
            set.push("port".to_owned());
        }
        for (key, value, target) in [
            (
                "embedding_url",
                &self.embedding_url,
                &mut config.embedding_url,
            ),
            (
                "embedding_model",
                &self.embedding_model,
                &mut config.embedding_model,
            ),
            ("reranker_url", &self.reranker_url, &mut config.reranker_url),
            (
                "reranker_model",
                &self.reranker_model,
                &mut config.reranker_model,
            ),
            ("ripgrep_path", &self.ripgrep_path, &mut config.ripgrep_path),
            (
                "rust_analyzer_path",
                &self.rust_analyzer_path,
                &mut config.rust_analyzer_path,
            ),
        ] {
            if let Some(value) = value {
                target.clone_from(value);
                set.push(key.to_owned());
            }
        }
        for (section, _) in LSP_SECTIONS {
            let Some(patch) = self.lsp(section) else {
                continue;
            };
            let (path, args, disabled) = match section {
                "csharp" => (
                    &mut config.csharp.path,
                    &mut config.csharp.args,
                    &mut config.csharp.disabled,
                ),
                "typescript" => (
                    &mut config.typescript.path,
                    &mut config.typescript.args,
                    &mut config.typescript.disabled,
                ),
                "python" => (
                    &mut config.python.path,
                    &mut config.python.args,
                    &mut config.python.disabled,
                ),
                "go" => (
                    &mut config.go.path,
                    &mut config.go.args,
                    &mut config.go.disabled,
                ),
                "java" => (
                    &mut config.java.path,
                    &mut config.java.args,
                    &mut config.java.disabled,
                ),
                "clangd" => (
                    &mut config.clangd.path,
                    &mut config.clangd.args,
                    &mut config.clangd.disabled,
                ),
                _ => (
                    &mut config.markdown.path,
                    &mut config.markdown.args,
                    &mut config.markdown.disabled,
                ),
            };
            if let Some(value) = &patch.path {
                *path = Some(value.clone());
                set.push(format!("{section}.path"));
            }
            if let Some(value) = &patch.args {
                args.clone_from(value);
                set.push(format!("{section}.args"));
            }
            if let Some(value) = patch.disabled {
                *disabled = value;
                set.push(format!("{section}.disabled"));
            }
        }
        set
    }
}

/// A non-empty environment value.
fn env_value<'a>(env: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn parse_port(name: &str, value: &str) -> Result<u16> {
    value
        .trim()
        .parse()
        .map_err(|_| anyhow!("{name} must be a port number from 0 to 65535, got {value:?}"))
}

fn parse_flag(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("{name} must be true, false, 1 or 0, got {value:?}"),
    }
}

/// Rejects a top-level key (or a language-server field) outside the machine
/// set, naming it.
fn check_machine_keys(table: &toml::Table) -> Result<()> {
    for (key, value) in table {
        if SCALAR_KEYS.iter().any(|(name, _)| *name == key.as_str()) {
            continue;
        }
        if LSP_SECTIONS.iter().any(|(name, _)| *name == key.as_str()) {
            // A section that is not a table is a type error; serde reports it
            // with its location.
            if let Some(section) = value.as_table() {
                for field in section.keys() {
                    if !LSP_FIELDS.contains(&field.as_str()) {
                        bail!(
                            "unknown setting `{key}.{field}`; [{key}] accepts only {}",
                            LSP_FIELDS.join(", ")
                        );
                    }
                }
            }
            continue;
        }
        return Err(rejected_key(key));
    }
    Ok(())
}

fn rejected_key(key: &str) -> anyhow::Error {
    if is_config_key(key) {
        anyhow!("`{key}` is not a machine-specific setting; set it in the configuration file")
    } else {
        let scalars: Vec<&str> = SCALAR_KEYS.iter().map(|(name, _)| *name).collect();
        let sections: Vec<String> = LSP_SECTIONS
            .iter()
            .map(|(name, _)| format!("[{name}]"))
            .collect();
        anyhow!(
            "unknown setting `{key}`; the local configuration accepts only machine-specific settings ({}, {}); every other setting belongs in the configuration file",
            scalars.join(", "),
            sections.join(", ")
        )
    }
}

/// Whether `key` is a top-level key of the configuration file.
fn is_config_key(key: &str) -> bool {
    // `search_profiles` is left out of a serialized default (it is empty).
    key == "search_profiles"
        || serde_json::to_value(Config::default())
            .ok()
            .and_then(|value| value.as_object().map(|object| object.contains_key(key)))
            .unwrap_or(false)
}

/// Where the local overlay is, and how that was decided.
///
/// `LCI_LOCAL_CONFIG` wins. Otherwise, beside the configuration file in use
/// as `<stem>.local.toml`. With no configuration file, the user-level
/// `local-code-intelligence/config.local.toml` under `user_config_dir`.
/// Nothing is read or created here.
pub fn overlay_location(
    config_file: Option<&Path>,
    local_config_env: Option<&str>,
    user_config_dir: Option<&Path>,
) -> Option<(PathBuf, &'static str)> {
    if let Some(path) = local_config_env {
        return Some((PathBuf::from(path), LOCAL_CONFIG_ENV));
    }
    if let Some(file) = config_file {
        let mut name = file.file_stem().unwrap_or_default().to_os_string();
        name.push(OVERLAY_SUFFIX);
        return Some((file.with_file_name(name), NAMED_BY_SIBLING));
    }
    user_config_dir.map(|dir| {
        (
            dir.join(USER_CONFIG_SUBDIR).join(USER_OVERLAY_FILE),
            NAMED_BY_USER_DIR,
        )
    })
}

/// A comment written with the setting it is about (see [`render_overlay`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayNote {
    /// The dotted machine-set key the note belongs to (`port`,
    /// `typescript.path`). A language-server section name (`typescript`) stands
    /// for the whole section. An empty or unrecognised key makes a general
    /// note, written before every setting.
    pub key: String,
    pub text: String,
}

impl OverlayNote {
    pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
        }
    }
}

/// The text of an overlay file: `header` and the notes as comments, and every
/// field `overrides` sets as a TOML setting, in [`machine_keys`] order with
/// the top-level keys first and the language-server tables after them. A note
/// is written just above the setting it belongs to, or on its own when that
/// key is not set. [`LocalOverrides::parse`] reads the result back as
/// `overrides`: comment text is flattened to one line (a control character
/// becomes a space), so a note built from data a probe received cannot add a
/// setting.
pub fn render_overlay(header: &str, overrides: &LocalOverrides, notes: &[OverlayNote]) -> String {
    let known = machine_keys();
    let mut groups: Vec<Vec<String>> = Vec::new();

    let mut intro = vec![comment_line(header)];
    intro.extend(
        notes
            .iter()
            .filter(|note| !known.contains(&note.key) && !is_lsp_section(&note.key))
            .map(|note| comment_line(&note.text)),
    );
    groups.push(intro);

    for (key, _) in SCALAR_KEYS {
        let mut group: Vec<String> = notes
            .iter()
            .filter(|note| note.key == key)
            .map(|note| comment_line(&note.text))
            .collect();
        if let Some(value) = scalar_value(overrides, key) {
            group.push(format!("{key} = {value}"));
        }
        if !group.is_empty() {
            groups.push(group);
        }
    }
    for (section, _) in LSP_SECTIONS {
        let prefix = format!("{section}.");
        let mut group: Vec<String> = notes
            .iter()
            .filter(|note| note.key == section || note.key.starts_with(&prefix))
            .map(|note| comment_line(&note.text))
            .collect();
        if let Some(patch) = overrides.lsp(section) {
            let mut fields = Vec::new();
            if let Some(path) = &patch.path {
                fields.push(format!("path = {}", toml_string(path)));
            }
            if let Some(args) = &patch.args {
                let items: Vec<String> = args.iter().map(|arg| toml_string(arg)).collect();
                fields.push(format!("args = [{}]", items.join(", ")));
            }
            if let Some(disabled) = patch.disabled {
                fields.push(format!("disabled = {disabled}"));
            }
            if !fields.is_empty() {
                group.push(format!("[{section}]"));
                group.extend(fields);
            }
        }
        if !group.is_empty() {
            groups.push(group);
        }
    }

    let mut text = groups
        .iter()
        .map(|group| group.join("\n"))
        .collect::<Vec<_>>()
        .join("\n\n");
    text.push('\n');
    text
}

fn is_lsp_section(key: &str) -> bool {
    let section = key.split_once('.').map_or(key, |(section, _)| section);
    LSP_SECTIONS.iter().any(|(name, _)| *name == section)
}

/// The TOML text of a top-level machine-set value, or `None` when unset.
fn scalar_value(overrides: &LocalOverrides, key: &str) -> Option<String> {
    let text = |value: &Option<String>| value.as_deref().map(toml_string);
    match key {
        "data_dir" => overrides
            .data_dir
            .as_ref()
            .map(|path| toml_string(&path.to_string_lossy())),
        "port" => overrides.port.map(|port| port.to_string()),
        "embedding_url" => text(&overrides.embedding_url),
        "embedding_model" => text(&overrides.embedding_model),
        "reranker_url" => text(&overrides.reranker_url),
        "reranker_model" => text(&overrides.reranker_model),
        "ripgrep_path" => text(&overrides.ripgrep_path),
        "rust_analyzer_path" => text(&overrides.rust_analyzer_path),
        _ => None,
    }
}

/// A TOML string for `value`: a literal string (`'...'`, so a Windows path
/// keeps its backslashes) unless it holds a quote or a control character, then
/// an escaped basic string.
fn toml_string(value: &str) -> String {
    if value.chars().any(|c| c == '\'' || c.is_control()) {
        toml::Value::String(value.to_owned()).to_string()
    } else {
        format!("'{value}'")
    }
}

/// One comment line holding `text` flattened to a single line.
fn comment_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.is_empty() {
        "#".to_owned()
    } else {
        format!("# {flat}")
    }
}

/// The overlay's patch, or `None` when no file is there.
fn read_overlay(path: &Path) -> Result<Option<LocalOverrides>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read local overlay {}", path.display()));
        }
    };
    LocalOverrides::parse(&text)
        .with_context(|| format!("local overlay {}", path.display()))
        .map(Some)
}

/// The configuration file layer.
#[derive(Debug, Clone, Serialize)]
pub struct FileLayer {
    /// The file read; `None` when the built-in defaults stand alone.
    pub path: Option<PathBuf>,
    /// `--config` or `LCI_CONFIG`.
    pub named_by: Option<&'static str>,
}

/// Whether the overlay file was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalState {
    /// The file exists and was applied.
    Present,
    /// No file at the location (or no location: the platform has no
    /// configuration directory).
    Absent,
    /// `--no-local-config`: the location was not consulted.
    Disabled,
}

impl LocalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Disabled => "disabled",
        }
    }
}

/// The local overlay layer.
#[derive(Debug, Clone, Serialize)]
pub struct LocalLayer {
    /// The overlay location, whether or not a file is there.
    pub path: Option<PathBuf>,
    pub state: LocalState,
    /// `config-file-sibling`, `user-config-dir` or `LCI_LOCAL_CONFIG`.
    pub named_by: Option<&'static str>,
}

/// What each layer contributed.
#[derive(Debug, Clone, Serialize)]
pub struct Layers {
    pub file: FileLayer,
    pub local: LocalLayer,
    /// The machine-set environment variables that were set and non-empty.
    pub env: Vec<String>,
    /// The value flags that were given.
    pub flags: Vec<String>,
}

/// One resolved leaf value with its provenance.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Dotted key: `port`, `typescript.path`, `index.freshness`. A
    /// `[search_profiles.<name>]` table is one entry, `search_profiles`.
    pub key: String,
    pub value: Value,
    pub provenance: Provenance,
    /// Whether the key is in the machine set.
    pub machine: bool,
}

/// The outcome of layering.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub config: Config,
    /// Provenance of every machine-set key.
    pub sources: BTreeMap<String, Provenance>,
    pub layers: Layers,
    /// The configuration file's parsed text, to tell a key the file set from
    /// a default.
    file_table: Option<toml::Table>,
}

impl Resolved {
    /// Every leaf value of the resolved configuration with where it came
    /// from: the machine-set keys first, in [`machine_keys`] order, then the
    /// rest alphabetically. A key outside the machine set comes from the
    /// file when the file names it, else from the default.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut leaves = BTreeMap::new();
        flatten("", &serde_json::to_value(&self.config)?, &mut leaves);
        let mut entries = Vec::new();
        for key in machine_keys() {
            let value = leaves.remove(&key).unwrap_or(Value::Null);
            let provenance = self
                .sources
                .get(&key)
                .cloned()
                .unwrap_or_else(Provenance::default_value);
            entries.push(Entry {
                key,
                value,
                provenance,
                machine: true,
            });
        }
        for (key, value) in leaves {
            let from_file = match (&self.layers.file.path, &self.file_table) {
                (Some(path), Some(table)) if table_has(table, &key) => Some(path),
                _ => None,
            };
            let provenance = match from_file {
                Some(path) => Provenance::of(Source::File, path.display().to_string()),
                None => Provenance::default_value(),
            };
            entries.push(Entry {
                key,
                value,
                provenance,
                machine: false,
            });
        }
        Ok(entries)
    }
}

/// Splits `value` into dotted leaves; a `search_profiles` table is one leaf.
fn flatten(prefix: &str, value: &Value, leaves: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) if prefix != "search_profiles" => {
            for (key, value) in map {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&key, value, leaves);
            }
        }
        other => {
            leaves.insert(prefix.to_owned(), other.clone());
        }
    }
}

/// Whether `table` names the dotted `key`, or its first segment for a table
/// that is reported as one entry.
fn table_has(table: &toml::Table, key: &str) -> bool {
    let mut current = table;
    let mut segments = key.split('.').peekable();
    while let Some(segment) = segments.next() {
        let Some(value) = current.get(segment) else {
            return false;
        };
        if segments.peek().is_none() || segment == "search_profiles" {
            return true;
        }
        let Some(next) = value.as_table() else {
            return false;
        };
        current = next;
    }
    false
}

/// Layers the configuration without validating it, so a caller can report an
/// invalid result (`lci config`). An error here is a problem reading or
/// parsing a layer: an unreadable or malformed file, a key outside the
/// machine set in the overlay, a malformed environment value.
pub fn resolve_unvalidated(spec: &ResolveSpec) -> Result<Resolved> {
    let env = &spec.env;

    let (file_path, file_named_by) = if let Some(path) = &spec.config_path {
        (Some(path.clone()), Some(NAMED_BY_FLAG))
    } else if let Some(value) = env_value(env, CONFIG_ENV) {
        (Some(PathBuf::from(value)), Some(CONFIG_ENV))
    } else {
        (None, None)
    };
    // The same two steps as `Config::load`; its validation waits for the
    // other layers.
    let (mut config, file_table) = match &file_path {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| {
                if file_named_by == Some(CONFIG_ENV) {
                    format!("read configuration named by {CONFIG_ENV}")
                } else {
                    "read configuration".to_owned()
                }
            })?;
            let config: Config = toml::from_str(&text)?;
            (config, text.parse::<toml::Table>().ok())
        }
        None => (Config::default(), None),
    };

    let mut sources = BTreeMap::new();
    for key in machine_keys() {
        let provenance = match (&file_path, &file_table) {
            (Some(path), Some(table)) if table_has(table, &key) => {
                Provenance::of(Source::File, path.display().to_string())
            }
            _ => Provenance::default_value(),
        };
        sources.insert(key, provenance);
    }

    let location = overlay_location(
        file_path.as_deref(),
        env_value(env, LOCAL_CONFIG_ENV),
        spec.user_config_dir.as_deref(),
    );
    let mut local = LocalLayer {
        path: location.as_ref().map(|(path, _)| path.clone()),
        state: LocalState::Absent,
        named_by: location.as_ref().map(|(_, named_by)| *named_by),
    };
    if spec.flags.no_local_config {
        local.state = LocalState::Disabled;
    } else if let Some((path, _)) = &location
        && let Some(overrides) = read_overlay(path)?
    {
        local.state = LocalState::Present;
        for key in overrides.apply(&mut config) {
            sources.insert(
                key,
                Provenance::of(Source::Local, path.display().to_string()),
            );
        }
    }

    for key in LocalOverrides::from_env(env)?.apply(&mut config) {
        let provenance = match env_var_for_key(&key) {
            Some(var) => Provenance::of(Source::Env, var),
            None => Provenance::default_value(),
        };
        sources.insert(key, provenance);
    }
    let mut env_names: Vec<String> = recognized_env_names()
        .into_iter()
        .filter(|name| name != CONFIG_ENV && name != LOCAL_CONFIG_ENV)
        .filter(|name| env_value(env, name).is_some())
        .collect();
    env_names.sort();

    let mut flags = Vec::new();
    if let Some(port) = spec.flags.port {
        config.port = port;
        sources.insert("port".to_owned(), Provenance::of(Source::Flag, "--port"));
        flags.push("--port".to_owned());
    }

    config.trim_model_settings();

    Ok(Resolved {
        config,
        sources,
        layers: Layers {
            file: FileLayer {
                path: file_path,
                named_by: file_named_by,
            },
            local,
            env: env_names,
            flags,
        },
        file_table,
    })
}

/// Layers the configuration and validates the result once.
pub fn resolve(spec: &ResolveSpec) -> Result<Resolved> {
    let resolved = resolve_unvalidated(spec)?;
    resolved.config.validate()?;
    Ok(resolved)
}

#[cfg(test)]
mod properties;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A TOML literal string for `path`, so Windows backslashes stay literal.
    fn literal(path: &Path) -> String {
        format!("'{}'", path.display())
    }

    /// An absolute data directory under `dir`.
    fn data_dir_line(dir: &Path) -> String {
        format!("data_dir = {}\n", literal(&dir.join("data")))
    }

    /// A spec with no overlay location the machine could contribute: no user
    /// configuration directory, no environment.
    fn base_spec(config_path: Option<PathBuf>) -> ResolveSpec {
        ResolveSpec {
            config_path,
            env: BTreeMap::new(),
            user_config_dir: None,
            flags: Flags::default(),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn as_json(config: &Config) -> Value {
        serde_json::to_value(config).unwrap()
    }

    /// A file that sets every top-level key to a non-default value, every
    /// section, and two search profiles.
    fn full_file(dir: &Path) -> String {
        format!(
            "{}port = 9100\n\
             embedding_url = 'http://127.0.0.1:9101/v1'\n\
             embedding_model = 'embed-test'\n\
             reranker_url = 'http://127.0.0.1:9102/rerank'\n\
             reranker_model = 'rerank-test'\n\
             default_top_k = 5\n\
             embedding_batch_size = 4\n\
             embedding_query_template = 'q: {{query}}'\n\
             embedding_document_template = 'd: {{path}} {{code}}'\n\
             embedding_timeout_seconds = 30\n\
             reranker_timeout_seconds = 31\n\
             readiness_timeout_seconds = 3\n\
             ripgrep_path = 'rg-test'\n\
             semantic_candidate_count = 10\n\
             lexical_candidate_count = 11\n\
             rerank_candidate_count = 12\n\
             rerank_document_max_bytes = 2048\n\
             rrf_k = 30.0\n\
             watch_poll_milliseconds = 1000\n\
             watch_debounce_milliseconds = 500\n\
             rust_analyzer_path = 'ra-test'\n\
             lsp_timeout_seconds = 20\n\
             lsp_candidate_count = 9\n\
             lsp_search_budget_ms = 900\n\
             lsp_idle_timeout_seconds = 120\n\
             [csharp]\npath = 'cs-test'\nargs = ['-a']\n\
             [typescript]\npath = 'ts-test'\ndisabled = true\n\
             [python]\npath = 'py-test'\n\
             [go]\npath = 'go-test'\nargs = ['-b', '-c']\n\
             [java]\npath = 'java-test'\n\
             [clangd]\npath = 'clangd-test'\n\
             [index]\nfreshness = 'manual'\nstale_check_interval_seconds = 3\nwait_for_existing_job_seconds = 4\n\
             [channels]\nlsp = false\nreranker = false\n\
             [fusion]\nsemantic_weight = 2.0\nmax_chunks_per_file = 3\n\
             [search_profiles.fast]\nreranker = false\nrerank_candidate_count = 5\n\
             [search_profiles.deep]\nlsp = false\nrerank_candidate_count = 40\n",
            data_dir_line(dir)
        )
    }

    /// The default behaviour is the same as before the layers existed: with
    /// no overlay, no `LCI_*` variable and no flag, the resolved
    /// configuration equals `Config::load` of the same file, field for field.
    #[test]
    fn resolving_a_plain_file_equals_config_load_for_every_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let fixtures: Vec<(&str, String)> = vec![
            ("an empty file", String::new()),
            ("a data_dir only", data_dir_line(dir)),
            (
                "a file that predates [channels]",
                format!(
                    "{}rust_analyzer_path = \"rust-analyzer\"\nlsp_timeout_seconds = 60\nlsp_candidate_count = 40\n",
                    data_dir_line(dir)
                ),
            ),
            (
                "profiles beside a top-level count",
                format!(
                    "{}rerank_candidate_count = 10\n[search_profiles.fast]\nreranker = false\nrerank_candidate_count = 5\n",
                    data_dir_line(dir)
                ),
            ),
            ("every section", full_file(dir)),
        ];
        for (index, (name, text)) in fixtures.iter().enumerate() {
            let path = write(dir, &format!("fixture-{index}.toml"), text);
            let loaded = Config::load(Some(&path)).unwrap();
            let resolved = resolve(&base_spec(Some(path))).unwrap();
            assert_eq!(
                as_json(&resolved.config),
                as_json(&loaded),
                "resolve differs from Config::load for {name}"
            );
        }
        // No file at all is the built-in default, as `Config::load(None)`.
        let resolved = resolve(&base_spec(None)).unwrap();
        assert_eq!(
            as_json(&resolved.config),
            as_json(&Config::load(None).unwrap())
        );
        assert_eq!(resolved.config.port, 8768);
    }

    /// A failure in the file layer reads exactly as it did.
    #[test]
    fn a_file_layer_error_reads_as_config_load_reports_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let missing = dir.join("missing.toml");
        let relative = write(dir, "relative.toml", "data_dir = 'relative/data'\n");
        let unknown = write(dir, "unknown.toml", "not_a_field = 1\n");
        let malformed = write(dir, "malformed.toml", "data_dir = \n");
        let bad_count = write(dir, "count.toml", "rerank_candidate_count = 0\n");
        for path in [missing, relative, unknown, malformed, bad_count] {
            let loaded = Config::load(Some(&path)).unwrap_err();
            let resolved = resolve(&base_spec(Some(path.clone()))).unwrap_err();
            assert_eq!(
                format!("{resolved:#}"),
                format!("{loaded:#}"),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn with_nothing_set_every_value_is_a_default_or_comes_from_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            temp.path(),
            "config.toml",
            &format!("{}[typescript]\npath = 'ts'\n", data_dir_line(temp.path())),
        );
        let resolved = resolve(&base_spec(Some(path.clone()))).unwrap();
        assert_eq!(resolved.layers.file.named_by, Some("--config"));
        assert_eq!(resolved.layers.local.state, LocalState::Absent);
        assert_eq!(
            resolved.layers.local.path,
            Some(temp.path().join("config.local.toml"))
        );
        assert!(resolved.layers.env.is_empty());
        assert!(resolved.layers.flags.is_empty());
        let file = Provenance::of(Source::File, path.display().to_string());
        assert_eq!(resolved.sources["data_dir"], file);
        assert_eq!(resolved.sources["typescript.path"], file);
        assert_eq!(resolved.sources["port"], Provenance::default_value());
        assert_eq!(resolved.sources["csharp.path"], Provenance::default_value());
        for (key, provenance) in &resolved.sources {
            assert!(
                matches!(provenance.source, Source::File | Source::Default),
                "{key}"
            );
        }
        // An entry outside the machine set follows the same rule.
        let entries = resolved.entries().unwrap();
        let find = |key: &str| entries.iter().find(|entry| entry.key == key).unwrap();
        assert_eq!(find("default_top_k").provenance.source, Source::Default);
        assert!(!find("default_top_k").machine);
        assert!(find("port").machine);
    }

    #[test]
    fn an_overlay_next_to_the_file_overrides_only_the_keys_it_names() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(
            dir,
            "lci.toml",
            &format!(
                "{}embedding_url = 'http://127.0.0.1:9101/v1'\nrerank_candidate_count = 12\n[typescript]\npath = 'from-file'\nargs = ['--x']\n",
                data_dir_line(dir)
            ),
        );
        let overlay = write(
            dir,
            "lci.local.toml",
            &format!(
                "data_dir = {}\nport = 9200\n[typescript]\npath = 'from-overlay'\n",
                literal(&dir.join("overlay-data"))
            ),
        );
        let resolved = resolve(&base_spec(Some(path.clone()))).unwrap();
        let config = &resolved.config;
        assert_eq!(config.data_dir, dir.join("overlay-data"));
        assert_eq!(config.port, 9200);
        assert_eq!(config.typescript.path.as_deref(), Some("from-overlay"));
        // The file's other keys, and the file's args in the same section,
        // are untouched.
        assert_eq!(config.embedding_url, "http://127.0.0.1:9101/v1");
        assert_eq!(config.rerank_candidate_count, 12);
        assert_eq!(config.typescript.args, vec!["--x".to_owned()]);
        assert_eq!(resolved.layers.local.state, LocalState::Present);
        assert_eq!(resolved.layers.local.named_by, Some(NAMED_BY_SIBLING));
        let local = Provenance::of(Source::Local, overlay.display().to_string());
        let file = Provenance::of(Source::File, path.display().to_string());
        assert_eq!(resolved.sources["data_dir"], local);
        assert_eq!(resolved.sources["port"], local);
        assert_eq!(resolved.sources["typescript.path"], local);
        assert_eq!(resolved.sources["typescript.args"], file);
        assert_eq!(resolved.sources["embedding_url"], file);
        assert_eq!(
            resolved.sources["embedding_model"],
            Provenance::default_value()
        );
    }

    #[test]
    fn an_environment_variable_beats_the_overlay_and_a_flag_beats_the_variable() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        write(
            dir,
            "c.local.toml",
            &format!(
                "data_dir = {}\nport = 9200\nripgrep_path = 'rg-overlay'\n",
                literal(&dir.join("overlay-data"))
            ),
        );
        let env_dir = dir.join("env-data");
        let mut spec = base_spec(Some(path));
        spec.env = env(&[
            ("LCI_DATA_DIR", env_dir.to_str().unwrap()),
            ("LCI_PORT", "9300"),
        ]);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.data_dir, env_dir);
        assert_eq!(resolved.config.port, 9300);
        assert_eq!(resolved.config.ripgrep_path, "rg-overlay");
        assert_eq!(
            resolved.sources["data_dir"],
            Provenance::of(Source::Env, "LCI_DATA_DIR")
        );
        assert_eq!(
            resolved.sources["port"],
            Provenance::of(Source::Env, "LCI_PORT")
        );
        assert_eq!(resolved.sources["ripgrep_path"].source, Source::Local);
        assert_eq!(resolved.layers.env, ["LCI_DATA_DIR", "LCI_PORT"]);

        spec.flags.port = Some(9400);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.port, 9400);
        assert_eq!(
            resolved.sources["port"],
            Provenance::of(Source::Flag, "--port")
        );
        assert_eq!(resolved.layers.flags, ["--port"]);
        // The environment still wins the key the flag does not touch.
        assert_eq!(resolved.config.data_dir, env_dir);
    }

    #[test]
    fn no_local_config_ignores_a_present_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        write(dir, "c.local.toml", "port = 9200\n");
        let mut spec = base_spec(Some(path));
        spec.flags.no_local_config = true;
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.port, 8768);
        assert_eq!(resolved.layers.local.state, LocalState::Disabled);
        // The location it would have consulted is still reported.
        assert_eq!(resolved.layers.local.path, Some(dir.join("c.local.toml")));
    }

    #[test]
    fn a_key_outside_the_machine_set_is_rejected_by_name() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        for (overlay, key) in [
            ("rerank_candidate_count = 5\n", "rerank_candidate_count"),
            ("[channels]\nreranker = false\n", "channels"),
            (
                "[search_profiles.fast]\nreranker = false\n",
                "search_profiles",
            ),
            (
                "embedding_query_template = 'q: {query}'\n",
                "embedding_query_template",
            ),
            ("[index]\nfreshness = 'manual'\n", "index"),
        ] {
            write(dir, "c.local.toml", overlay);
            let error = format!("{:#}", resolve(&base_spec(Some(path.clone()))).unwrap_err());
            assert!(error.contains(&format!("`{key}`")), "{error}");
            assert!(error.contains("not a machine-specific setting"), "{error}");
            assert!(error.contains("configuration file"), "{error}");
            assert!(error.contains("c.local.toml"), "{error}");
        }
        // A key that is not a setting at all is named too, and the file it
        // belongs to still rejects it.
        write(dir, "c.local.toml", "data_dr = 'x'\n");
        let error = format!("{:#}", resolve(&base_spec(Some(path.clone()))).unwrap_err());
        assert!(
            error.contains("`data_dr`") && error.contains("unknown"),
            "{error}"
        );
        // A field of a language-server section outside path/args/disabled.
        write(dir, "c.local.toml", "[typescript]\ntimeout = 3\n");
        let error = format!("{:#}", resolve(&base_spec(Some(path.clone()))).unwrap_err());
        assert!(error.contains("`typescript.timeout`"), "{error}");
        // A machine key of the wrong type is a parse error, not a panic.
        write(dir, "c.local.toml", "port = 'high'\n");
        assert!(resolve(&base_spec(Some(path))).is_err());
    }

    #[test]
    fn every_machine_key_is_accepted_in_an_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        write(
            dir,
            "c.local.toml",
            &format!(
                "data_dir = {data}\nport = 9200\nembedding_url = 'http://127.0.0.1:9201/v1'\n\
                 embedding_model = 'e'\nreranker_url = 'http://127.0.0.1:9202/rerank'\n\
                 reranker_model = 'r'\nripgrep_path = '/x/rg'\nrust_analyzer_path = '/x/ra'\n\
                 [csharp]\npath = '/x/cs'\nargs = ['a']\ndisabled = false\n\
                 [typescript]\npath = '/x/ts'\n[python]\npath = '/x/py'\n[go]\npath = '/x/go'\n\
                 [java]\npath = '/x/java'\n[clangd]\npath = '/x/clangd'\ndisabled = true\n\
                 [markdown]\npath = '/x/marksman'\n",
                data = literal(&dir.join("d2"))
            ),
        );
        let resolved = resolve(&base_spec(Some(path))).unwrap();
        let config = &resolved.config;
        assert_eq!(config.embedding_model, "e");
        assert_eq!(config.reranker_model, "r");
        assert_eq!(config.ripgrep_path, "/x/rg");
        assert_eq!(config.rust_analyzer_path, "/x/ra");
        assert!(config.csharp.enabled() && config.typescript.enabled());
        assert!(config.python.enabled() && config.go.enabled() && config.java.enabled());
        assert_eq!(config.csharp.args, vec!["a".to_owned()]);
        assert!(config.clangd.path.is_some() && !config.clangd.enabled());
        assert!(config.markdown.enabled());
        for key in machine_keys() {
            if key.ends_with(".args") || key.ends_with(".disabled") {
                continue;
            }
            assert_eq!(resolved.sources[&key].source, Source::Local, "{key}");
        }
    }

    #[test]
    fn environment_values_parse_and_empty_ones_count_as_unset() {
        let parsed = LocalOverrides::from_env(&env(&[
            ("LCI_PORT", "9001"),
            ("LCI_CSHARP_PATH", ""),
            ("LCI_GO_PATH", "/x/gopls"),
            ("LCI_GO_DISABLED", "TRUE"),
            ("LCI_PYTHON_DISABLED", "0"),
            ("LCI_RIPGREP_PATH", ""),
            ("LCI_UNRELATED_VARIABLE", "ignored"),
        ]))
        .unwrap();
        assert_eq!(parsed.port, Some(9001));
        assert!(parsed.csharp.is_none());
        assert!(parsed.ripgrep_path.is_none());
        let go = parsed.go.unwrap();
        assert_eq!(go.path.as_deref(), Some("/x/gopls"));
        assert_eq!(go.disabled, Some(true));
        assert_eq!(parsed.python.unwrap().disabled, Some(false));

        // An empty LCI_CSHARP_PATH leaves the section as the file had it.
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            temp.path(),
            "c.toml",
            &format!(
                "{}[csharp]\npath = 'from-file'\n",
                data_dir_line(temp.path())
            ),
        );
        let mut spec = base_spec(Some(path));
        spec.env = env(&[("LCI_CSHARP_PATH", ""), ("LCI_CSHARP_DISABLED", "")]);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.csharp.path.as_deref(), Some("from-file"));
        assert_eq!(resolved.sources["csharp.path"].source, Source::File);
        assert!(resolved.layers.env.is_empty());
    }

    #[test]
    fn model_endpoints_and_names_are_trimmed_from_file_and_environment_layers() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let baseline_path = write(
            dir,
            "baseline.toml",
            &format!(
                "{}embedding_url = 'http://127.0.0.1:8766/v1'\n\
                 embedding_model = 'embed-test'\n\
                 reranker_url = 'http://127.0.0.1:8767/rerank'\n\
                 reranker_model = 'rerank-test'\n",
                data_dir_line(dir)
            ),
        );
        let baseline = resolve(&base_spec(Some(baseline_path))).unwrap();
        let baseline_identity = baseline.config.embedding_identity();

        let padded_path = write(
            dir,
            "padded.toml",
            &format!(
                "{}embedding_url = ' http://127.0.0.1:8766/v1 '\n\
                 embedding_model = ' embed-test '\n\
                 reranker_url = ' http://127.0.0.1:8767/rerank '\n\
                 reranker_model = ' rerank-test '\n",
                data_dir_line(dir)
            ),
        );
        let padded_file = resolve(&base_spec(Some(padded_path.clone()))).unwrap();
        assert_eq!(padded_file.config.embedding_identity(), baseline_identity);
        assert_eq!(padded_file.config.embedding_url, "http://127.0.0.1:8766/v1");
        assert_eq!(padded_file.config.embedding_model, "embed-test");
        assert_eq!(
            padded_file.config.reranker_url,
            "http://127.0.0.1:8767/rerank"
        );
        assert_eq!(padded_file.config.reranker_model, "rerank-test");
        assert_eq!(
            Config::load(Some(&padded_path))
                .unwrap()
                .embedding_identity(),
            baseline_identity
        );

        let mut by_env = base_spec(Some(write(dir, "defaults.toml", &data_dir_line(dir))));
        by_env.env = env(&[
            ("LCI_EMBEDDING_URL", " http://127.0.0.1:8766/v1 "),
            ("LCI_EMBEDDING_MODEL", " embed-test "),
            ("LCI_RERANKER_URL", " http://127.0.0.1:8767/rerank "),
            ("LCI_RERANKER_MODEL", " rerank-test "),
        ]);
        let padded_env = resolve(&by_env).unwrap();
        assert_eq!(padded_env.config.embedding_identity(), baseline_identity);
        assert_eq!(padded_env.config.embedding_url, "http://127.0.0.1:8766/v1");
        assert_eq!(padded_env.config.embedding_model, "embed-test");
        assert_eq!(
            padded_env.config.reranker_url,
            "http://127.0.0.1:8767/rerank"
        );
        assert_eq!(padded_env.config.reranker_model, "rerank-test");
    }

    #[test]
    fn a_malformed_environment_value_names_its_variable() {
        for (name, value) in [
            ("LCI_PORT", "abc"),
            ("LCI_PORT", "70000"),
            ("LCI_JAVA_DISABLED", "maybe"),
        ] {
            let error = LocalOverrides::from_env(&env(&[(name, value)])).unwrap_err();
            let error = error.to_string();
            assert!(error.contains(name) && error.contains(value), "{error}");
        }
    }

    #[test]
    fn a_reserved_port_is_rejected_from_the_file_the_overlay_and_the_environment() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        for reserved in [8765_u16, 8766, 8767] {
            let mut by_flag = base_spec(Some(path.clone()));
            by_flag.flags.port = Some(reserved);
            let mut by_env = base_spec(Some(path.clone()));
            let value = reserved.to_string();
            by_env.env = env(&[("LCI_PORT", value.as_str())]);
            for rejected in [by_flag, by_env] {
                let error = resolve(&rejected).unwrap_err().to_string();
                assert!(error.contains(&reserved.to_string()), "{error}");
            }
            let in_file = write(
                dir,
                "reserved.toml",
                &format!("{}port = {reserved}\n", data_dir_line(dir)),
            );
            assert!(resolve(&base_spec(Some(in_file))).is_err());
            write(dir, "c.local.toml", &format!("port = {reserved}\n"));
            assert!(resolve(&base_spec(Some(path.clone()))).is_err());
            fs::remove_file(dir.join("c.local.toml")).unwrap();
        }
        // The neighbours are fine.
        for allowed in [8764_u16, 8768, 8769] {
            let mut ok = base_spec(Some(path.clone()));
            ok.flags.port = Some(allowed);
            assert_eq!(resolve(&ok).unwrap().config.port, allowed);
        }
    }

    /// Port 8765 belongs to the generation model: a model URL naming it is
    /// rejected whichever layer supplies it, and a higher layer can repair it.
    #[test]
    fn a_model_url_on_port_8765_is_rejected_from_the_file_the_overlay_and_the_environment() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        let overlay = dir.join("c.local.toml");
        for (key, variable, url) in [
            (
                "embedding_url",
                "LCI_EMBEDDING_URL",
                "http://127.0.0.1:8765/v1",
            ),
            (
                "reranker_url",
                "LCI_RERANKER_URL",
                "http://models.example.com:8765/rerank",
            ),
        ] {
            let names_the_rule = |error: anyhow::Error| {
                let error = error.to_string();
                assert!(error.contains(&format!("`{key}`")), "{error}");
                assert!(error.contains("port 8765"), "{error}");
                assert!(error.contains("generation model"), "{error}");
            };

            // The configuration file.
            let in_file = write(
                dir,
                "reserved.toml",
                &format!("{}{key} = '{url}'\n", data_dir_line(dir)),
            );
            names_the_rule(resolve(&base_spec(Some(in_file))).unwrap_err());

            // The local overlay.
            write(dir, "c.local.toml", &format!("{key} = '{url}'\n"));
            names_the_rule(resolve(&base_spec(Some(path.clone()))).unwrap_err());
            let unvalidated = resolve_unvalidated(&base_spec(Some(path.clone()))).unwrap();
            assert_eq!(unvalidated.sources[key].source, Source::Local);
            assert!(unvalidated.config.validate().is_err());

            // The environment, which also beats a harmless overlay value.
            write(
                dir,
                "c.local.toml",
                &format!("{key} = 'http://127.0.0.1:9300/v1'\n"),
            );
            let mut by_env = base_spec(Some(path.clone()));
            by_env.env = env(&[(variable, url)]);
            names_the_rule(resolve(&by_env).unwrap_err());
            fs::remove_file(&overlay).unwrap();
            let mut by_env = base_spec(Some(path.clone()));
            by_env.env = env(&[(variable, url)]);
            names_the_rule(resolve(&by_env).unwrap_err());

            // A higher layer repairs a reserved value from a lower one.
            let in_file = write(
                dir,
                "reserved.toml",
                &format!("{}{key} = '{url}'\n", data_dir_line(dir)),
            );
            let mut repaired = base_spec(Some(in_file));
            repaired.env = env(&[(variable, "http://127.0.0.1:9301/v1")]);
            let resolved = resolve(&repaired).unwrap();
            assert_eq!(resolved.sources[key].source, Source::Env);

            // The neighbouring service ports and a URL with no explicit port
            // are fine from the overlay and the environment.
            for allowed in [
                "http://127.0.0.1:8766/v1",
                "http://127.0.0.1:8767/rerank",
                "http://localhost/v1",
            ] {
                write(dir, "c.local.toml", &format!("{key} = '{allowed}'\n"));
                assert!(resolve(&base_spec(Some(path.clone()))).is_ok(), "{allowed}");
                fs::remove_file(&overlay).unwrap();
                let mut ok = base_spec(Some(path.clone()));
                ok.env = env(&[(variable, allowed)]);
                assert!(resolve(&ok).is_ok(), "{allowed}");
            }
        }
    }

    /// Validation runs once, on the layered result: a file that is invalid on
    /// its own is fine when a higher layer repairs the key.
    #[test]
    fn validation_sees_the_resolved_value_not_the_file_alone() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", "data_dir = 'relative/data'\n");
        assert!(resolve(&base_spec(Some(path.clone()))).is_err());
        write(
            dir,
            "c.local.toml",
            &format!("data_dir = {}\n", literal(&dir.join("data"))),
        );
        assert!(resolve(&base_spec(Some(path.clone()))).is_ok());
        // `resolve_unvalidated` reports the broken value instead of failing.
        fs::remove_file(dir.join("c.local.toml")).unwrap();
        let unvalidated = resolve_unvalidated(&base_spec(Some(path))).unwrap();
        assert!(unvalidated.config.validate().is_err());
    }

    #[test]
    fn the_overlay_location_follows_the_configuration_file_in_use() {
        let user = Path::new("user-config");
        // Beside a file named by --config.
        assert_eq!(
            overlay_location(Some(Path::new("dir/lab.toml")), None, Some(user)),
            Some((PathBuf::from("dir/lab.local.toml"), NAMED_BY_SIBLING))
        );
        // A file with no extension still gets a stem-based sibling.
        assert_eq!(
            overlay_location(Some(Path::new("lab")), None, Some(user)),
            Some((PathBuf::from("lab.local.toml"), NAMED_BY_SIBLING))
        );
        // No file: the user-level overlay.
        assert_eq!(
            overlay_location(None, None, Some(user)),
            Some((
                user.join("local-code-intelligence")
                    .join("config.local.toml"),
                NAMED_BY_USER_DIR
            ))
        );
        // LCI_LOCAL_CONFIG replaces either.
        assert_eq!(
            overlay_location(
                Some(Path::new("dir/lab.toml")),
                Some("x/o.toml"),
                Some(user)
            ),
            Some((PathBuf::from("x/o.toml"), LOCAL_CONFIG_ENV))
        );
        assert_eq!(
            overlay_location(None, Some("x/o.toml"), Some(user)),
            Some((PathBuf::from("x/o.toml"), LOCAL_CONFIG_ENV))
        );
        // No file and no per-user directory: nowhere to look.
        assert_eq!(overlay_location(None, None, None), None);
    }

    #[test]
    fn the_user_level_overlay_applies_without_a_configuration_file() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let overlay = user
            .join("local-code-intelligence")
            .join("config.local.toml");
        fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        fs::write(
            &overlay,
            format!("data_dir = {}\n", literal(&temp.path().join("user-data"))),
        )
        .unwrap();
        let mut spec = base_spec(None);
        spec.user_config_dir = Some(user);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.data_dir, temp.path().join("user-data"));
        assert_eq!(resolved.layers.file.path, None);
        assert_eq!(resolved.layers.local.named_by, Some(NAMED_BY_USER_DIR));
        assert_eq!(resolved.sources["data_dir"].source, Source::Local);
        // A configuration file turns the user-level overlay off: it is not
        // consulted for a file that has its own sibling.
        let path = write(temp.path(), "c.toml", &data_dir_line(temp.path()));
        spec.config_path = Some(path);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.data_dir, temp.path().join("data"));
        assert_eq!(resolved.layers.local.state, LocalState::Absent);
    }

    #[test]
    fn lci_config_names_the_file_when_the_flag_is_absent_and_loses_to_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let named = write(
            dir,
            "named.toml",
            &format!("{}default_top_k = 3\n", data_dir_line(dir)),
        );
        let by_flag = write(
            dir,
            "flag.toml",
            &format!("{}default_top_k = 4\n", data_dir_line(dir)),
        );
        write(dir, "named.local.toml", "port = 9500\n");
        let mut spec = base_spec(None);
        spec.env = env(&[("LCI_CONFIG", named.to_str().unwrap())]);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.default_top_k, 3);
        assert_eq!(resolved.layers.file.named_by, Some("LCI_CONFIG"));
        // The overlay follows the file in use.
        assert_eq!(resolved.config.port, 9500);

        spec.config_path = Some(by_flag);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.default_top_k, 4);
        assert_eq!(resolved.layers.file.named_by, Some("--config"));
        assert_eq!(resolved.config.port, 8768);

        // A missing file is an error either way.
        let mut missing = base_spec(None);
        missing.env = env(&[("LCI_CONFIG", dir.join("nope.toml").to_str().unwrap())]);
        assert!(resolve(&missing).is_err());
    }

    #[test]
    fn lci_local_config_replaces_the_default_overlay_location() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        write(dir, "c.local.toml", "port = 9200\n");
        let elsewhere = write(dir, "elsewhere.toml", "port = 9300\n");
        let mut spec = base_spec(Some(path));
        spec.env = env(&[("LCI_LOCAL_CONFIG", elsewhere.to_str().unwrap())]);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.port, 9300);
        assert_eq!(resolved.layers.local.named_by, Some(LOCAL_CONFIG_ENV));
        // A path that is not there is just an absent overlay.
        spec.env = env(&[("LCI_LOCAL_CONFIG", dir.join("none.toml").to_str().unwrap())]);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.config.port, 8768);
        assert_eq!(resolved.layers.local.state, LocalState::Absent);
    }

    #[test]
    fn a_malformed_overlay_names_its_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        write(dir, "c.local.toml", "port = \n");
        let error = format!("{:#}", resolve(&base_spec(Some(path))).unwrap_err());
        assert!(error.contains("c.local.toml"), "{error}");
    }

    #[test]
    fn the_embedding_identity_follows_an_overlay_that_moves_the_embedder() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        let before = resolve(&base_spec(Some(path.clone())))
            .unwrap()
            .config
            .embedding_identity();
        write(dir, "c.local.toml", "embedding_model = 'other-embedder'\n");
        let after = resolve(&base_spec(Some(path)))
            .unwrap()
            .config
            .embedding_identity();
        assert_ne!(before, after);
        assert!(after.contains("other-embedder"));
    }

    #[test]
    fn entries_list_every_machine_key_first_and_then_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), "c.toml", &full_file(temp.path()));
        let resolved = resolve(&base_spec(Some(path.clone()))).unwrap();
        let entries = resolved.entries().unwrap();
        let machine = machine_keys();
        let leading: Vec<&str> = entries
            .iter()
            .take(machine.len())
            .map(|entry| entry.key.as_str())
            .collect();
        assert_eq!(
            leading,
            machine.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert!(entries[..machine.len()].iter().all(|entry| entry.machine));
        assert!(entries[machine.len()..].iter().all(|entry| !entry.machine));
        let find = |key: &str| entries.iter().find(|entry| entry.key == key).unwrap();
        assert_eq!(find("index.freshness").value, Value::from("manual"));
        assert_eq!(find("index.freshness").provenance.source, Source::File);
        assert_eq!(find("channels.semantic").provenance.source, Source::Default);
        assert_eq!(find("channels.lsp").provenance.source, Source::File);
        assert_eq!(find("search_profiles").provenance.source, Source::File);
        assert_eq!(find("typescript.path").value, Value::from("ts-test"));
        assert_eq!(find("python.args").value, Value::Array(Vec::new()));
        assert_eq!(find("python.args").provenance.source, Source::Default);
    }

    /// A patch that sets every machine key, with strings that need care in
    /// TOML: backslashes, a single quote, a double quote, a non-ASCII letter.
    fn full_overrides() -> LocalOverrides {
        let lsp = |path: &str, args: &[&str], disabled: Option<bool>| LspOverride {
            path: Some(path.to_owned()),
            args: Some(args.iter().map(|arg| (*arg).to_owned()).collect()),
            disabled,
        };
        LocalOverrides {
            data_dir: Some(PathBuf::from(r"D:\team's data\idx")),
            port: Some(9100),
            embedding_url: Some("http://127.0.0.1:9101/v1".to_owned()),
            embedding_model: Some(r#"embed "quoted" \ model"#.to_owned()),
            reranker_url: Some("http://127.0.0.1:9102/rerank".to_owned()),
            reranker_model: Some("rérank".to_owned()),
            ripgrep_path: Some(r"C:\tools\rg.exe".to_owned()),
            rust_analyzer_path: Some("/usr/local/bin/rust-analyzer".to_owned()),
            csharp: Some(lsp(
                r"C:\dotnet\csharp-ls.exe",
                &["--a", "b c"],
                Some(false),
            )),
            typescript: Some(lsp("/x/ts", &[], None)),
            python: Some(LspOverride {
                path: Some("/x/py".to_owned()),
                ..LspOverride::default()
            }),
            go: Some(LspOverride {
                disabled: Some(true),
                ..LspOverride::default()
            }),
            java: Some(lsp("/x/jdtls", &["-Xmx1g"], None)),
            clangd: Some(lsp(
                "/x/clangd",
                &["--compile-commands-dir=build"],
                Some(true),
            )),
            markdown: Some(lsp("/x/marksman", &[], None)),
        }
    }

    #[test]
    fn a_rendered_overlay_reads_back_as_the_patch_it_was_made_from() {
        let overrides = full_overrides();
        let text = render_overlay("generated for the test", &overrides, &[]);
        assert!(text.starts_with("# generated for the test\n"), "{text}");
        assert_eq!(LocalOverrides::parse(&text).unwrap(), overrides, "{text}");
        // Literal strings keep a Windows path readable; the quote forces the
        // escaped form only where it is needed.
        assert!(text.contains(r"ripgrep_path = 'C:\tools\rg.exe'"), "{text}");
        assert!(text.contains("port = 9100"), "{text}");

        // The empty patch is a header only.
        let text = render_overlay("nothing", &LocalOverrides::default(), &[]);
        assert_eq!(text, "# nothing\n");
        assert_eq!(
            LocalOverrides::parse(&text).unwrap(),
            LocalOverrides::default()
        );
    }

    #[test]
    fn notes_sit_with_their_setting_or_stand_alone_and_never_change_the_patch() {
        let overrides = LocalOverrides {
            ripgrep_path: Some("/x/rg".to_owned()),
            typescript: Some(LspOverride {
                path: Some("/x/ts".to_owned()),
                ..LspOverride::default()
            }),
            ..LocalOverrides::default()
        };
        let notes = [
            OverlayNote::new("", "general note"),
            OverlayNote::new("not-a-key", "unknown keys are general too"),
            OverlayNote::new("port", "port note"),
            OverlayNote::new("ripgrep_path", "rg note"),
            OverlayNote::new("csharp.path", "csharp-ls is not on PATH"),
            OverlayNote::new("typescript.path", "ts note"),
            OverlayNote::new("java", "java note"),
        ];
        let text = render_overlay("head", &overrides, &notes);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "# head",
                "# general note",
                "# unknown keys are general too",
                "",
                "# port note",
                "",
                "# rg note",
                "ripgrep_path = '/x/rg'",
                "",
                "# csharp-ls is not on PATH",
                "",
                "# ts note",
                "[typescript]",
                "path = '/x/ts'",
                "",
                "# java note",
            ]
        );
        assert_eq!(LocalOverrides::parse(&text).unwrap(), overrides);
    }

    /// A note may be built from what a service answered. A line break in it
    /// must not end the comment and start a setting.
    #[test]
    fn a_note_cannot_break_out_of_its_comment() {
        let hostile = "listed: evil\nport = 1\r\n[csharp]\npath = 'x'\u{0}\u{7f}\tend";
        let notes = [
            OverlayNote::new("embedding_url", hostile),
            OverlayNote::new("", hostile),
        ];
        let text = render_overlay(hostile, &LocalOverrides::default(), &notes);
        for line in text.lines().filter(|line| !line.is_empty()) {
            assert!(line.starts_with('#'), "{line:?} in {text}");
        }
        assert_eq!(
            LocalOverrides::parse(&text).unwrap(),
            LocalOverrides::default(),
            "{text}"
        );
    }

    #[test]
    fn a_rendered_overlay_applies_like_the_patch_and_names_the_keys_it_set() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = write(dir, "c.toml", &data_dir_line(dir));
        let overrides = LocalOverrides {
            port: Some(9150),
            go: Some(LspOverride {
                path: Some("/x/gopls".to_owned()),
                ..LspOverride::default()
            }),
            ..LocalOverrides::default()
        };
        write(
            dir,
            "c.local.toml",
            &render_overlay("test", &overrides, &[OverlayNote::new("port", "a note")]),
        );
        let resolved = resolve(&base_spec(Some(path))).unwrap();
        assert_eq!(resolved.config.port, 9150);
        assert_eq!(resolved.config.go.path.as_deref(), Some("/x/gopls"));
        assert_eq!(resolved.sources["port"].source, Source::Local);
        assert_eq!(resolved.sources["go.path"].source, Source::Local);
        let mut config = Config::default();
        assert_eq!(overrides.apply(&mut config), ["port", "go.path"]);
    }
}
