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

/// `[run_profile]`. No keys yet: the section is the home of the switches that
/// later name a run's configuration. An unknown key is an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileConfig {}

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
            "0218b08ee81dd57abdcd6d97278e1750a6ec213b19467899e9560522f0582523",
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
            "run_profile",
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
    fn the_default_profile_hash_is_pinned() {
        // Changes whenever a default, a Config key or the deny-list changes:
        // update it with the reason in the commit.
        assert_eq!(
            Config::default().run_profile().hash,
            "a489865da28049e15168145e9d5af53792b4fc388e35658ca5b558f6d3e12bef"
        );
    }
}
