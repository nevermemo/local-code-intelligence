//! `[run_profile]` and the run-profile hash: the part of a resolved
//! configuration that can change what a search returns, in a canonical form
//! with a stable hash.
//!
//! The profile is the resolved [`Config`] (as JSON, with the sections that are
//! left out of its serialization while they equal the default put back) minus
//! a deny-list of dotted keys: the machine-set keys of
//! [`layers::machine_keys`](super::layers::machine_keys) plus
//! [`EXCLUDED_KEYS`] (timeouts and timing, throughput, file paths, per-machine
//! security, service identity). Everything else is in, so a key added to
//! [`Config`] is part of the profile until someone excludes it by name.

use super::{Config, layers::machine_keys};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `[run_profile]`: the home of the switches that name a run's configuration
/// (cycle 1: every new behaviour is a key here whose default keeps today's
/// behaviour). An unknown key is an error.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileConfig {
    /// `[run_profile.traverse]`: switches of the traversal.
    pub traverse: RunProfileTraverse,
    /// `[run_profile.walk]`: how a traversal counts evidence.
    pub walk: WalkProfile,
    /// `[run_profile.scip]`: how a traversal treats the SCIP index.
    pub scip: ScipProfile,
    /// `[run_profile.lsp]`: how `lsp.live` gets its language servers.
    pub lsp: LspProfile,
}

/// What a traversal does with the SCIP index (`[run_profile.scip] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScipMode {
    /// The index is required and read; a missing or unreadable file is an
    /// error. Today's behaviour.
    #[default]
    Authoritative,
    /// The index is a cache: a missing or unreadable file gives an empty SCIP
    /// graph and a note; a readable one is loaded as under `authoritative`.
    Cache,
    /// The index path is never touched; the SCIP graph and every map built
    /// from the index are empty.
    Off,
}

impl ScipMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Cache => "cache",
            Self::Off => "off",
        }
    }
}

impl std::str::FromStr for ScipMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "authoritative" => Ok(Self::Authoritative),
            "cache" => Ok(Self::Cache),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "mode must be \"authoritative\", \"cache\" or \"off\" (got {other:?})"
            )),
        }
    }
}

/// `[run_profile.scip]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScipProfile {
    /// `"authoritative"` (default, the index is required), `"cache"` (read it
    /// when it is there) or `"off"` (never read it).
    pub mode: ScipMode,
}

/// `[run_profile.traverse]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileTraverse {
    /// Name each map's freshness in a traverse answer: `maps`, an array of
    /// `{id, observed, stale, lag}` with one entry for every map of the call
    /// that states it. `false` (the default): the answer has no `maps` key and
    /// is as without the key. `true` counts the tree's source files that are
    /// newer than the index file or changed since the process first saw its
    /// stamp (a read of the tree's source files once per call) and adds the
    /// result to every response. See the README "Map freshness" for what
    /// `observed`, `stale` and `lag` mean.
    pub map_freshness: bool,
}

/// How a traversal groups the evidence that reaches a node
/// (`[run_profile.walk] groups`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WalkGroups {
    /// One group per map (the maps derived from one shared graph are one
    /// group together). Today's behaviour.
    #[default]
    Tool,
    /// One group per claim: the same `(source node, destination node, claim
    /// family)` is one claim, whichever maps assert it.
    Claim,
}

impl WalkGroups {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Claim => "claim",
        }
    }
}

impl std::str::FromStr for WalkGroups {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "tool" => Ok(Self::Tool),
            "claim" => Ok(Self::Claim),
            other => Err(format!(
                "groups must be \"tool\" or \"claim\" (got {other:?})"
            )),
        }
    }
}

/// `[run_profile.walk]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalkProfile {
    /// What counts as one piece of evidence: `"tool"` (default, one per map)
    /// or `"claim"`. `"claim"` cannot be combined with `[traverse] fusion =
    /// "rrf"`.
    pub groups: WalkGroups,
    /// What a group made only of `mentions` edges (text matches) adds to the
    /// evidence count, `0.0..=1.0`; 1.0 (default) counts it like any other.
    pub grep_corroboration: f64,
}

impl Default for WalkProfile {
    fn default() -> Self {
        Self {
            groups: WalkGroups::Tool,
            grep_corroboration: 1.0,
        }
    }
}

/// Whose language-server sessions `lsp.live` asks (`[run_profile.lsp] sessions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspSessions {
    /// Every traverse call starts its own servers and stops them when its
    /// walk ends. Today's behaviour.
    #[default]
    PerCall,
    /// The calls of one process share that process's language servers (the
    /// ones navigation and the search channel use): a server started by one
    /// call is reused by the next and stops with the idle timeout,
    /// `release_workspace` or service shutdown.
    Shared,
}

impl LspSessions {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PerCall => "per_call",
            Self::Shared => "shared",
        }
    }
}

impl std::str::FromStr for LspSessions {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "per_call" => Ok(Self::PerCall),
            "shared" => Ok(Self::Shared),
            other => Err(format!(
                "sessions must be \"per_call\" or \"shared\" (got {other:?})"
            )),
        }
    }
}

/// `[run_profile.lsp]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspProfile {
    /// `"per_call"` (default): each traverse call starts and stops its own
    /// language servers. `"shared"`: the calls of a process reuse one set (see
    /// [`LspSessions`]). The set lives in a manager that the caller supplies:
    /// the MCP and REST server's own, or one per process for `lci traverse`.
    /// The recipes walk no `lsp.live` and run the same under both values.
    pub sessions: LspSessions,
}

/// Dotted keys left out of the profile besides [`machine_keys`]. A key naming
/// a section (`system_one`) leaves out everything under it.
///
/// Some of these change answers (the traverse priors and tie-break files, the
/// language-server switches and budget, `rust_analyzer_trusted`, the embedding
/// identity) but are per machine or per file, so they are stamped beside the
/// profile hash instead of inside it.
pub const EXCLUDED_KEYS: &[&str] = &[
    "embedding_api_key_env",
    "reranker_api_key_env",
    "embedding_timeout_seconds",
    "reranker_timeout_seconds",
    "readiness_timeout_seconds",
    "lsp_timeout_seconds",
    "lsp_search_budget_ms",
    "lsp_idle_timeout_seconds",
    "lsp_stuck_seconds",
    "watch_poll_milliseconds",
    "watch_debounce_milliseconds",
    "index.stale_check_interval_seconds",
    "index.wait_for_existing_job_seconds",
    "traverse.deadline_ms",
    "embedding_batch_size",
    "traverse.scip_index",
    "traverse.priors",
    "traverse.tie_break",
    "rust_analyzer_trusted",
    "system_one",
];

/// Every dotted key the profile leaves out: [`machine_keys`] then
/// [`EXCLUDED_KEYS`].
pub fn excluded_keys() -> Vec<String> {
    let mut keys = machine_keys();
    keys.extend(EXCLUDED_KEYS.iter().map(|key| (*key).to_owned()));
    keys
}

/// The result-affecting profile of one resolved configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct RunProfile {
    /// The profile as a JSON value.
    pub value: Value,
    /// `value` as compact JSON with every object's keys sorted. Floats are
    /// the shortest round-trip `f64`; an `f32` setting is widened first, so
    /// `[traverse] tau = 0.8` reads `0.800000011920929`. Deterministic, not
    /// pretty.
    pub canonical: String,
    /// Lowercase hex SHA-256 of `canonical`.
    pub hash: String,
    /// The dotted path of every leaf of `value`, sorted. An array, a scalar,
    /// `null` and an empty object are leaves.
    pub keys: Vec<String>,
}

impl Config {
    /// The resolved result-affecting profile (see the module comment).
    pub fn run_profile(&self) -> RunProfile {
        let mut value = self.profile_projection();
        for key in excluded_keys() {
            remove_dotted(&mut value, &key);
        }
        let canonical = canonical_json(&value);
        RunProfile {
            hash: crate::workspace::hash(&canonical),
            keys: leaf_paths(&value),
            canonical,
            value,
        }
    }

    /// The whole configuration as JSON, with every section that
    /// serialization leaves out while it is the default put back.
    fn profile_projection(&self) -> Value {
        let mut value = serde_json::to_value(self)
            .expect("a Config serializes to JSON: every map key is a string");
        if let Value::Object(map) = &mut value {
            // Every section Config skips when serializing must be listed
            // here, or its keys never reach the profile.
            for (key, section) in [
                ("traverse", section_value(&self.traverse)),
                ("docs_lane", section_value(&self.docs_lane)),
                ("recipes", section_value(&self.recipes)),
                ("compose", section_value(&self.compose)),
                ("rescorer", section_value(&self.rescorer)),
                ("search_profiles", section_value(&self.search_profiles)),
                ("run_profile", section_value(&self.run_profile)),
            ] {
                map.insert(key.to_owned(), section);
            }
        }
        value
    }
}

fn section_value(section: &impl Serialize) -> Value {
    serde_json::to_value(section).expect("a config section serializes to JSON")
}

/// Remove the leaf or section at a dotted key, then every parent object that
/// the removal left empty. A key that is not there is ignored.
fn remove_dotted(value: &mut Value, key: &str) {
    // Returns whether the key was there; a parent emptied by the removal is
    // pruned, an already empty one is kept.
    fn remove(map: &mut Map<String, Value>, parts: &[&str]) -> bool {
        let Some((head, rest)) = parts.split_first() else {
            return false;
        };
        if rest.is_empty() {
            return map.remove(*head).is_some();
        }
        let mut removed = false;
        if let Some(Value::Object(child)) = map.get_mut(*head) {
            removed = remove(child, rest);
            if removed && child.is_empty() {
                map.remove(*head);
            }
        }
        removed
    }
    if let Value::Object(map) = value {
        let parts: Vec<&str> = key.split('.').collect();
        remove(map, &parts);
    }
}

/// Compact JSON with every object's keys in sorted order, whatever order the
/// `Map` keeps them in.
fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (key, child)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(child, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The sorted dotted path of every leaf of a JSON object.
fn leaf_paths(value: &Value) -> Vec<String> {
    fn walk(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(child, &path, out);
                }
            }
            _ => out.push(prefix.to_owned()),
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{IndexFreshness, McpResponse, SearchProfile, TraverseFusion, TraversePolicy},
        provider::SystemOneConfig,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn base_toml() -> String {
        format!("data_dir = '{}'\n", std::env::temp_dir().display())
    }

    /// A configuration with every optional key set and no value at its
    /// default, so every leaf of the projection exists.
    #[allow(clippy::field_reassign_with_default)]
    fn full_config() -> Config {
        let mut config = Config::default();
        config.embedding_url = "http://example.test/v1".into();
        config.embedding_model = "m".into();
        config.reranker_url = "http://example.test/rerank".into();
        config.reranker_model = "r".into();
        config.embedding_api_key_env = Some("EMB_KEY".into());
        config.reranker_api_key_env = Some("RR_KEY".into());
        config.data_dir = PathBuf::from("/elsewhere");
        config.port = 9001;
        config.default_top_k = 9;
        config.embedding_batch_size = 3;
        config.embedding_timeout_seconds = 7;
        config.reranker_timeout_seconds = 8;
        config.readiness_timeout_seconds = 9;
        config.ripgrep_path = "rg2".into();
        config.rust_analyzer_path = "ra2".into();
        config.rust_analyzer_trusted = true;
        config.lsp_timeout_seconds = 11;
        config.lsp_search_budget_ms = 12;
        config.lsp_idle_timeout_seconds = 13;
        config.lsp_stuck_seconds = 14;
        config.watch_poll_milliseconds = 15;
        config.watch_debounce_milliseconds = 16;
        config.csharp.path = Some("cs".into());
        config.typescript.path = Some("ts".into());
        config.python.path = Some("py".into());
        config.go.path = Some("go".into());
        config.java.path = Some("java".into());
        config.clangd.path = Some("clangd".into());
        config.index.freshness = IndexFreshness::Manual;
        config.index.stale_check_interval_seconds = 17;
        config.index.wait_for_existing_job_seconds = 18;
        config.index.prose = true;
        config.channels.lsp = false;
        config.fusion.lexical_weight = 2.0;
        config.docs_lane.top_k = 3;
        config.search_profiles.insert(
            "fast".into(),
            SearchProfile {
                reranker: Some(false),
                response: Some(McpResponse::Compact),
                compact_max_hit_lines: Some(5),
                ..SearchProfile::default()
            },
        );
        config.traverse.enabled = true;
        config.traverse.scip_index = "other.scip".into();
        config.traverse.deadline_ms = 19;
        config.traverse.max_depth = 3;
        config.run_profile.traverse.map_freshness = true;
        config.traverse.fusion = TraverseFusion::Rrf;
        config.traverse.max_model_calls = Some(4);
        config.traverse.priors = Some("priors.json".into());
        config.traverse.question_type = Some("flow".into());
        config.traverse.tie_break = Some("tie.json".into());
        config.traverse.policies.insert(
            "p".into(),
            TraversePolicy {
                walk: Some(true),
                ..TraversePolicy::default()
            },
        );
        config.recipes.enabled = true;
        config.compose.enabled = true;
        config.system_one = Some(SystemOneConfig::default());
        config.rescorer = crate::config::Rescorer::SystemOne;
        config.run_profile.walk.groups = crate::config::WalkGroups::Claim;
        config.run_profile.walk.grep_corroboration = 0.5;
        config.run_profile.scip.mode = crate::config::profile::ScipMode::Cache;
        config.run_profile.lsp.sessions = LspSessions::Shared;
        config
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn the_same_profile_hashes_the_same_whatever_the_machine_values() {
        let a = Config::default().run_profile();
        let mut other = Config::default();
        other.data_dir = PathBuf::from("/elsewhere");
        other.port = 9001;
        other.embedding_url = "http://example.test/v1".into();
        other.embedding_model = "another".into();
        other.ripgrep_path = "rg2".into();
        other.csharp.path = Some("cs".into());
        other.typescript.args = vec!["--x".into()];
        other.embedding_api_key_env = Some("EMB_KEY".into());
        other.embedding_timeout_seconds = 7;
        other.embedding_batch_size = 3;
        other.lsp_search_budget_ms = 12;
        other.rust_analyzer_trusted = true;
        other.index.stale_check_interval_seconds = 17;
        other.traverse.deadline_ms = 19;
        other.traverse.scip_index = "other.scip".into();
        other.traverse.priors = Some("priors.json".into());
        other.system_one = Some(SystemOneConfig::default());
        let b = other.run_profile();
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.canonical, b.canonical);
        assert_eq!(a.hash.len(), 64);
        assert!(
            a.hash
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn a_result_affecting_key_changes_the_hash() {
        let base = Config::default().run_profile().hash;
        let mut reranker_off = Config::default();
        reranker_off.channels.reranker = false;
        assert_ne!(reranker_off.run_profile().hash, base);
        let mut shallow = Config::default();
        shallow.traverse.max_depth = 3;
        assert_ne!(shallow.run_profile().hash, base);
        assert_ne!(shallow.run_profile().hash, reranker_off.run_profile().hash);
    }

    #[test]
    fn walk_groups_and_grep_corroboration_change_the_profile_hash() {
        let base = Config::default().run_profile();
        assert!(
            base.canonical
                .contains(
                    r#""run_profile":{"lsp":{"sessions":"per_call"},"scip":{"mode":"authoritative"},"traverse":{"map_freshness":false},"walk":{"grep_corroboration":1.0,"groups":"tool"}}"#
                )
        );
        let mut claim = Config::default();
        claim.run_profile.walk.groups = WalkGroups::Claim;
        let mut discount = Config::default();
        discount.run_profile.walk.grep_corroboration = 0.5;
        let hashes = [
            base.hash.clone(),
            claim.run_profile().hash,
            discount.run_profile().hash,
        ];
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Both are leaves of the profile, never excluded.
        for key in [
            "run_profile.walk.groups",
            "run_profile.walk.grep_corroboration",
        ] {
            assert!(base.keys.iter().any(|k| k == key), "{key}");
        }
    }

    #[test]
    fn scip_mode_changes_the_profile_hash() {
        let base = Config::default().run_profile();
        assert!(
            base.canonical
                .contains(r#""scip":{"mode":"authoritative"}"#)
        );
        let hashes: Vec<String> = [ScipMode::Authoritative, ScipMode::Cache, ScipMode::Off]
            .into_iter()
            .map(|mode| {
                let mut config = Config::default();
                config.run_profile.scip.mode = mode;
                config.run_profile().hash
            })
            .collect();
        assert_eq!(hashes[0], base.hash);
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[0], hashes[2]);
        assert_ne!(hashes[1], hashes[2]);
        assert!(base.keys.iter().any(|key| key == "run_profile.scip.mode"));
        // The file reads the key.
        let read: Config = toml::from_str(&format!(
            "{}[run_profile.scip]
mode = \"off\"
",
            base_toml()
        ))
        .unwrap();
        read.validate().unwrap();
        assert_eq!(read.run_profile.scip.mode, ScipMode::Off);
        assert_eq!("cache".parse::<ScipMode>(), Ok(ScipMode::Cache));
    }

    #[test]
    fn scip_mode_refuses_an_unknown_value() {
        for bad in [
            "mode = \"Off\"",
            "mode = \"\"",
            "mode = \"on\"",
            "mode = true",
            "nope = 1",
        ] {
            let text = format!(
                "{}[run_profile.scip]
{bad}
",
                base_toml()
            );
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        let error = toml::from_str::<Config>(&format!(
            "{}[run_profile.scip]
mode = \"sometimes\"
",
            base_toml()
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("sometimes"), "{error}");
        assert!("sometimes".parse::<ScipMode>().is_err());
    }

    #[test]
    fn canonical_json_has_sorted_keys_and_no_whitespace() {
        let mixed = json!({"b": 1, "a": [{"d": 1, "c": "x y"}, 2.5], "e": {}});
        assert_eq!(
            canonical_json(&mixed),
            r#"{"a":[{"c":"x y","d":1},2.5],"b":1,"e":{}}"#
        );
        let profile = Config::default().run_profile();
        assert!(profile.canonical.starts_with(
            r#"{"channels":{"lexical":true,"lsp":true,"reranker":true,"semantic":true},"compose":{"enabled":false},"default_top_k":8,"#
        ));
        assert!(!profile.canonical.contains('\n'));
        assert!(profile.canonical.contains(r#""tau":0.800000011920929"#));
        assert_eq!(
            serde_json::from_str::<Value>(&profile.canonical).unwrap(),
            profile.value
        );
        let mut sorted = profile.keys.clone();
        sorted.sort();
        assert_eq!(profile.keys, sorted);
    }

    #[test]
    fn every_config_leaf_is_in_the_profile_or_excluded_by_name() {
        let config = full_config();
        let all = leaf_paths(&config.profile_projection());
        let profile = config.run_profile();
        let excluded = excluded_keys();
        // An exclusion names a leaf or a section, and exists in a full
        // configuration: a renamed or removed key must be dropped from the
        // deny-list.
        for key in &excluded {
            assert!(
                all.iter()
                    .any(|leaf| leaf == key || leaf.starts_with(&format!("{key}."))),
                "the deny-list names {key}, which no Config leaf has"
            );
        }
        let is_excluded = |leaf: &String| {
            excluded
                .iter()
                .any(|key| leaf == key || leaf.starts_with(&format!("{key}.")))
        };
        // Every leaf is exactly one of: in the profile, excluded.
        for leaf in &all {
            assert_ne!(
                profile.keys.contains(leaf),
                is_excluded(leaf),
                "{leaf} must be in the profile or excluded, not both or neither"
            );
        }
        for key in &profile.keys {
            assert!(all.contains(key), "{key} is not a Config leaf");
        }
        // A new key changes this hash, so adding one is a conscious step:
        // classify it (profile or deny-list) and set it in `full_config`.
        assert_eq!(
            crate::workspace::hash(&profile.keys.join("\n")),
            "7ba4de4746690a60f09e958f21522e8e0d7d748ed7837da51b1b40223cf7a218",
            "the profile key set changed"
        );
        // Spot checks of the classification.
        for key in [
            "default_top_k",
            "rrf_k",
            "channels.reranker",
            "fusion.max_chunks_per_file",
            "docs_lane.top_k",
            "search_profiles.fast.reranker",
            "traverse.max_depth",
            "traverse.policies.p.walk",
            "recipes.enabled",
            "rescorer",
            "run_profile.walk.groups",
            "run_profile.walk.grep_corroboration",
            "run_profile.traverse.map_freshness",
            "run_profile.scip.mode",
            "run_profile.lsp.sessions",
        ] {
            assert!(profile.keys.iter().any(|k| k == key), "{key} must be in");
        }
        for key in ["port", "data_dir", "traverse.deadline_ms", "system_one.url"] {
            assert!(!profile.keys.iter().any(|k| k == key), "{key} must be out");
        }
    }

    #[test]
    fn an_unknown_run_profile_key_is_rejected_by_name() {
        let error = toml::from_str::<Config>(&format!("{}[run_profile]\nfoo = 1\n", base_toml()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("foo"), "{error}");
        let empty: Config = toml::from_str(&format!("{}[run_profile]\n", base_toml())).unwrap();
        empty.validate().unwrap();
        assert_eq!(empty.run_profile, RunProfileConfig::default());
        assert_eq!(
            empty.run_profile().hash,
            Config::default().run_profile().hash
        );
    }

    #[test]
    fn map_freshness_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.traverse.map_freshness);
        let text = format!(
            "{}[run_profile.traverse]\nmap_freshness = true\n",
            base_toml()
        );
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.traverse.map_freshness);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["traverse"]["map_freshness"],
            true
        );
        // It changes what a traverse answer holds, so it changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["traverse"]["map_freshness"], false);
        assert_eq!(on.value["run_profile"]["traverse"]["map_freshness"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.traverse.map_freshness")
        );
        assert_ne!(off.hash, on.hash);
        for bad in [
            "[run_profile.traverse]\nmap_freshness = \"yes\"\n",
            "[run_profile.traverse]\nfoo = 1\n",
            "[run_profile]\nmap_freshness = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn lsp_sessions_are_per_call_by_default_and_change_the_profile_hash() {
        let off = Config::default();
        assert_eq!(off.run_profile.lsp.sessions, LspSessions::PerCall);
        let text = format!("{}[run_profile.lsp]\nsessions = \"shared\"\n", base_toml());
        let shared: Config = toml::from_str(&text).unwrap();
        shared.validate().unwrap();
        assert_eq!(shared.run_profile.lsp.sessions, LspSessions::Shared);
        assert_eq!(
            serde_json::to_value(&shared).unwrap()["run_profile"]["lsp"]["sessions"],
            "shared"
        );
        let (off, shared) = (off.run_profile(), shared.run_profile());
        assert_eq!(off.value["run_profile"]["lsp"]["sessions"], "per_call");
        assert_eq!(shared.value["run_profile"]["lsp"]["sessions"], "shared");
        assert!(off.keys.iter().any(|key| key == "run_profile.lsp.sessions"));
        assert_ne!(off.hash, shared.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nsessions = \"per_call\"\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.lsp]\nsessions = \"always\"\n",
            "[run_profile.lsp]\nsessions = true\n",
            "[run_profile.lsp]\nfoo = 1\n",
            "[run_profile]\nsessions = \"shared\"\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        assert_eq!(LspSessions::Shared.as_str(), "shared");
        assert_eq!("per_call".parse::<LspSessions>(), Ok(LspSessions::PerCall));
        assert!("per-call".parse::<LspSessions>().is_err());
    }

    #[test]
    fn the_default_profile_hash_is_pinned() {
        // Changes whenever a default, a Config key or the deny-list changes:
        // update it with the reason in the commit.
        // K23a: a489865d... before `[run_profile.walk]` (`groups`,
        // `grep_corroboration`) gave the section its two leaves.
        // K2a-2: c10fe368... before `[run_profile.traverse] map_freshness`.
        // K20a: 854c4164... before `[run_profile.scip] mode` added its leaf.
        // K7b-1: e4cab053... before `[run_profile.lsp] sessions`.
        assert_eq!(
            Config::default().run_profile().hash,
            "7c5a1ce880f62c278a0948d3c60edcdb3d7dd6cae2e869428a2514384298f0c3"
        );
    }
}
