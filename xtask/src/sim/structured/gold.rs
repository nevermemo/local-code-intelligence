//! The structured-config gold as typed data: one `<repo>.json` per repository
//! under a gold dir, in the shape `gold validate-structured` checks (its module
//! comment is the schema). The loader keeps what the simulator and the scorer
//! read -- the families with their assignment, reader and documentation spans,
//! and the questions -- and refuses what would make a run meaningless: a seed
//! other than the preregistered one, a path that is not a repo-relative `/`
//! path, a question whose family or type is unknown, a reader question of a
//! config family whose scope names no component file. It checks nothing against
//! a tree and nothing about counts or the split; that is the validator's job.
//! Fields the validator reads and this does not (`sha256`, `value`,
//! `behaviour`, ...) are ignored.

use super::score::scope_file;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The seed every gold file must record (preregistration section 4).
pub(crate) const SEED: u64 = 20_261_018;
/// The strata a family may belong to; `config` when the field is absent.
pub(crate) const STRATA: [&str; 2] = ["config", "dependency"];
pub(crate) const SPLITS: [&str; 2] = ["dev", "heldout"];
pub(crate) const SETTING_LOCATION: &str = "setting_location";
pub(crate) const BEHAVIOUR_TO_SETTING: &str = "behaviour_to_setting";
pub(crate) const SETTING_TO_READER: &str = "setting_to_reader";
const TYPES: [&str; 3] = [SETTING_LOCATION, BEHAVIOUR_TO_SETTING, SETTING_TO_READER];

/// Lines `line..=end_line` (1-based, inclusive) of a repo-relative file.
/// `kind` labels an assignment span (`assignment`, `default`, `example`,
/// `fixture`); `how` a reader span (`literal`, `serde`, `ast`, `constant`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Span {
    pub path: String,
    pub line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
}

fn config() -> String {
    "config".to_owned()
}

/// One key family: where the key is set, what reads it, what documents it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Family {
    pub id: String,
    /// `config` (the default) or `dependency`.
    #[serde(default = "config")]
    pub stratum: String,
    /// `dev` or `heldout`.
    pub split: String,
    pub key: String,
    pub assignments: Vec<Span>,
    #[serde(default)]
    pub readers: Vec<Span>,
    /// `component:<file>[::<function>]` for a config family, else a label.
    #[serde(default)]
    pub readers_scope: String,
    #[serde(default)]
    pub docs: Vec<Span>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Question {
    pub id: String,
    pub family: String,
    /// The gold's `type` field.
    #[serde(rename = "type")]
    pub qtype: String,
    pub text: String,
    /// Whether the text spells the key (`false` for behaviour questions).
    #[serde(default)]
    pub names_key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Repo {
    /// The repository name; the file stem when the file records none.
    #[serde(default)]
    pub repo: String,
    pub families: Vec<Family>,
    pub questions: Vec<Question>,
}

impl Repo {
    pub(crate) fn family(&self, id: &str) -> Option<&Family> {
        self.families.iter().find(|f| f.id == id)
    }
}

/// Why `path` is not a repo-relative `/` path, if it is not. The rule of the
/// validator's `unsafe_path`, which is private to that module.
fn unsafe_path(path: &str) -> Option<&'static str> {
    let bytes = path.as_bytes();
    if path.is_empty() {
        Some("empty path")
    } else if path.contains('\\') {
        Some("backslash in path")
    } else if path.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        Some("path is absolute")
    } else if path.split('/').any(|c| c.is_empty() || c == "..") {
        Some("path has an empty or `..` component")
    } else {
        None
    }
}

fn check_spans(what: &str, spans: &[Span]) -> Result<()> {
    for span in spans {
        if let Some(why) = unsafe_path(&span.path) {
            bail!("{what}: {why}: {}", span.path);
        }
        if span.line < 1 || span.end_line < span.line {
            bail!(
                "{what}: bad line range {}-{} in {}",
                span.line,
                span.end_line,
                span.path
            );
        }
    }
    Ok(())
}

/// The refusals of the loader, over a parsed repository.
fn check(repo: &Repo, name: &str) -> Result<()> {
    let mut ids = BTreeSet::new();
    for family in &repo.families {
        let what = format!("{name}: family {}", family.id);
        if !ids.insert(family.id.as_str()) {
            bail!("{what} appears twice");
        }
        if !STRATA.contains(&family.stratum.as_str()) {
            bail!("{what}: unknown stratum {:?}", family.stratum);
        }
        if !SPLITS.contains(&family.split.as_str()) {
            bail!("{what}: unknown split {:?}", family.split);
        }
        if family.assignments.is_empty() {
            bail!("{what}: no assignment spans");
        }
        check_spans(&what, &family.assignments)?;
        check_spans(&what, &family.readers)?;
        check_spans(&what, &family.docs)?;
    }
    let mut question_ids = BTreeSet::new();
    for question in &repo.questions {
        let what = format!("{name}: question {}", question.id);
        if !question_ids.insert(question.id.as_str()) {
            bail!("{what} appears twice");
        }
        if !TYPES.contains(&question.qtype.as_str()) {
            bail!("{what}: unknown type {:?}", question.qtype);
        }
        let Some(family) = repo.family(&question.family) else {
            bail!("{what}: names unknown family {}", question.family);
        };
        if question.qtype == SETTING_TO_READER && family.readers.is_empty() {
            bail!("{what}: asks for readers but family {} has none", family.id);
        }
        // Reader precision is scored over the windows read in the scope file;
        // a scope that names none would score every such question 0.
        if question.qtype == SETTING_TO_READER
            && family.stratum == "config"
            && scope_file(&family.readers_scope).is_none()
        {
            bail!(
                "{what}: asks for readers but family {} has the readers_scope {:?}, which names no `component:<file>` to score against",
                family.id,
                family.readers_scope
            );
        }
    }
    Ok(())
}

/// One gold file. `name` stands in for the repository when the file records
/// none (the file stem).
pub(crate) fn parse_repo(value: &Value, name: &str) -> Result<Repo> {
    match value.get("seed").and_then(Value::as_u64) {
        Some(SEED) => {}
        other => bail!("{name}: gold seed is {other:?}, expected {SEED}"),
    }
    let mut repo: Repo = serde_json::from_value(value.clone())
        .with_context(|| format!("{name}: not a structured gold file"))?;
    if repo.repo.is_empty() {
        repo.repo = name.to_owned();
    }
    check(&repo, name)?;
    Ok(repo)
}

/// Every `*.json` in `dir`, sorted by file name, as typed repositories.
pub(crate) fn load_dir(dir: &Path) -> Result<Vec<Repo>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read gold dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        bail!("no <repo>.json files in {}", dir.display());
    }
    let mut repos: Vec<Repo> = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let repo = parse_repo(&value, stem)?;
        if repos.iter().any(|r| r.repo == repo.repo) {
            bail!("two gold files record the repository {}", repo.repo);
        }
        repos.push(repo);
    }
    Ok(repos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lci-sim-structured-gold-{}-{name}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha() -> String {
        "0".repeat(64)
    }

    /// A config-stratum file `r` with one family and its three questions.
    /// `stratum` is absent, as in the committed config gold.
    fn config_file() -> Value {
        json!({
            "version": 1, "repo": "r", "revision": "a".repeat(40), "seed": SEED,
            "families": [{
                "id": "r.listen", "split": "heldout", "format": "toml", "key": "s.listen",
                "assignments": [{
                    "path": "cfg/a.toml", "line": 2, "end_line": 2, "sha256": sha(),
                    "value": "1", "kind": "assignment",
                }],
                "readers": [{
                    "path": "src/a.rs", "line": 4, "end_line": 6, "sha256": sha(), "how": "serde",
                }],
                "readers_scope": "component:src/a.rs::serve",
                "excluded_readers": [], "behaviour": "Chooses the address.",
                "docs": [{"path": "src/doc.rs", "line": 9, "end_line": 12, "sha256": sha()}],
            }],
            "questions": [
                {"id": "r.listen.q1", "family": "r.listen", "type": "setting_location",
                 "text": "Where is `s.listen` set?", "names_key": true},
                {"id": "r.listen.q2", "family": "r.listen", "type": "behaviour_to_setting",
                 "text": "Which setting chooses the address?"},
                {"id": "r.listen.q3", "family": "r.listen", "type": "setting_to_reader",
                 "text": "What reads `s.listen`?", "names_key": true},
            ],
        })
    }

    /// A dependency-stratum file `d`.
    fn dependency_file() -> Value {
        json!({
            "version": 1, "repo": "d", "revision": "a".repeat(40), "seed": SEED,
            "families": [{
                "id": "d.dep.futures", "stratum": "dependency", "split": "dev",
                "format": "toml", "key": "futures",
                "assignments": [{
                    "path": "Cargo.toml", "line": 17, "end_line": 17, "sha256": sha(),
                    "value": "\"0.3\"", "kind": "assignment",
                }],
                "readers": [], "readers_scope": "none:package-manager",
                "excluded_readers": [], "behaviour": "Declares futures.", "docs": [],
            }],
            "questions": [
                {"id": "d.dep.futures.q1", "family": "d.dep.futures", "type": "setting_location",
                 "text": "Where is `futures` declared?", "names_key": true},
            ],
        })
    }

    fn write(dir: &Path, name: &str, value: &Value) {
        std::fs::write(dir.join(name), serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn both_strata_load_in_file_order() {
        let dir = temp_dir("both");
        write(&dir, "r.json", &config_file());
        write(&dir, "d.json", &dependency_file());
        std::fs::write(dir.join("NOTES.md"), "not json").unwrap();
        let repos = load_dir(&dir).unwrap();
        // d.json sorts before r.json.
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].repo, "d");
        assert_eq!(repos[1].repo, "r");

        let dep = &repos[0].families[0];
        assert_eq!(dep.stratum, "dependency");
        assert_eq!(dep.split, "dev");
        assert!(dep.readers.is_empty());
        assert_eq!(dep.readers_scope, "none:package-manager");
        assert_eq!(dep.assignments[0].kind.as_deref(), Some("assignment"));
        assert_eq!(repos[0].questions.len(), 1);

        let cfg = &repos[1];
        let family = cfg.family("r.listen").unwrap();
        // No `stratum` in the file: the default is config.
        assert_eq!(family.stratum, "config");
        assert_eq!(family.key, "s.listen");
        assert_eq!(family.assignments[0].line, 2);
        assert_eq!((family.readers[0].line, family.readers[0].end_line), (4, 6));
        assert_eq!(family.readers[0].how.as_deref(), Some("serde"));
        assert_eq!(family.docs[0].path, "src/doc.rs");
        assert_eq!(cfg.questions.len(), 3);
        assert_eq!(cfg.questions[1].qtype, "behaviour_to_setting");
        // `names_key` defaults to false when the file omits it.
        assert!(!cfg.questions[1].names_key);
        assert!(cfg.questions[0].names_key);
        assert!(cfg.family("nope").is_none());
    }

    #[test]
    fn the_typed_gold_serializes_with_the_gold_field_names() {
        let repo = parse_repo(&config_file(), "r").unwrap();
        let family = serde_json::to_value(repo.family("r.listen").unwrap()).unwrap();
        assert_eq!(family["stratum"], "config");
        assert_eq!(family["readers"][0]["how"], "serde");
        assert_eq!(family["assignments"][0]["kind"], "assignment");
        // An absent label is left out, not written as null.
        assert!(family["assignments"][0].get("how").is_none());
        let question = serde_json::to_value(&repo.questions[2]).unwrap();
        assert_eq!(question["type"], "setting_to_reader");
        assert_eq!(
            serde_json::to_value(&repo.questions[0]).unwrap()["names_key"],
            true
        );
        let back: Repo = serde_json::from_value(serde_json::to_value(&repo).unwrap()).unwrap();
        assert_eq!(back, repo);
    }

    #[test]
    fn another_seed_is_refused() {
        let mut file = config_file();
        file["seed"] = json!(7);
        let err = parse_repo(&file, "r").unwrap_err().to_string();
        assert!(err.contains("seed"), "{err}");
        file.as_object_mut().unwrap().remove("seed");
        assert!(parse_repo(&file, "r").is_err());
        let dir = temp_dir("seed");
        let mut file = config_file();
        file["seed"] = json!(SEED + 1);
        write(&dir, "r.json", &file);
        assert!(load_dir(&dir).is_err());
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in ["/etc/x", "C:/x.toml", "a\\b.toml", "../x.toml", "a//b", ""] {
            let mut file = config_file();
            file["families"][0]["assignments"][0]["path"] = json!(bad);
            let err = parse_repo(&file, "r").unwrap_err().to_string();
            assert!(err.contains("path"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_question_must_name_a_known_family_and_type() {
        let mut file = config_file();
        file["questions"][0]["family"] = json!("r.other");
        assert!(parse_repo(&file, "r").is_err());

        let mut file = config_file();
        file["questions"][0]["type"] = json!("who_knows");
        assert!(parse_repo(&file, "r").is_err());

        // A reader question needs readers.
        let mut file = config_file();
        file["families"][0]["readers"] = json!([]);
        assert!(parse_repo(&file, "r").is_err());

        // A family needs an assignment, a known split and a known stratum.
        let mut file = config_file();
        file["families"][0]["assignments"] = json!([]);
        assert!(parse_repo(&file, "r").is_err());
        let mut file = config_file();
        file["families"][0]["split"] = json!("pending");
        assert!(parse_repo(&file, "r").is_err());
        let mut file = config_file();
        file["families"][0]["stratum"] = json!("other");
        assert!(parse_repo(&file, "r").is_err());
    }

    #[test]
    fn a_config_reader_question_needs_a_scope_that_names_a_component_file() {
        for bad in ["repo", "component:", "none:package-manager", ""] {
            let mut file = config_file();
            file["families"][0]["readers_scope"] = json!(bad);
            let err = parse_repo(&file, "r").unwrap_err().to_string();
            assert!(
                err.contains("readers_scope") && err.contains("r.listen.q3"),
                "{bad}: {err}"
            );
        }
        // A scope with a component file, with or without a function, loads.
        for good in [
            "component:src/a.rs",
            "component:src/a.rs::serve (handoff to B)",
        ] {
            let mut file = config_file();
            file["families"][0]["readers_scope"] = json!(good);
            parse_repo(&file, "r").unwrap();
        }
        // Without a reader question the scope is not read.
        let mut file = config_file();
        file["families"][0]["readers_scope"] = json!("repo");
        file["questions"].as_array_mut().unwrap().pop();
        parse_repo(&file, "r").unwrap();
        // The dependency stratum has no reader questions to score.
        parse_repo(&dependency_file(), "d").unwrap();
    }

    #[test]
    fn the_committed_gold_loads_and_every_reader_scope_names_a_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../evaluations/structured");
        if !dir.is_dir() {
            return;
        }
        let repos = load_dir(&dir).unwrap();
        let mut readers = 0;
        for repo in &repos {
            for q in repo
                .questions
                .iter()
                .filter(|q| q.qtype == SETTING_TO_READER)
            {
                let family = repo.family(&q.family).unwrap();
                assert!(
                    scope_file(&family.readers_scope).is_some(),
                    "{}: {:?}",
                    q.id,
                    family.readers_scope
                );
                readers += 1;
            }
        }
        assert!(readers > 0);
    }

    #[test]
    fn duplicate_repositories_and_empty_dirs_are_refused() {
        let dir = temp_dir("dup");
        write(&dir, "one.json", &config_file());
        write(&dir, "two.json", &config_file());
        assert!(load_dir(&dir).is_err());
        let empty = temp_dir("empty");
        assert!(load_dir(&empty).is_err());
    }
}
