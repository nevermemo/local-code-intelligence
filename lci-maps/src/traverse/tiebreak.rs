//! A fitted tie-break (slice Q5): the order of results that tie.
//!
//! Hop values are products of a few discrete priors, so many results share
//! one `ranked_potential`; without a tie-break they go by discovery order,
//! which carries no information. A [`TieBreak`] is a declarative weighted sum
//! of named per-result features. [`TraversalResult::ranked`] orders results
//! by `ranked_potential` first (unchanged), then by the sum (higher first),
//! then by discovery order. It applies under `boost` fusion only; `rrf` keeps
//! its own order.
//!
//! The expression only orders results that are tied at the same
//! `ranked_potential`. It cannot lift a result over a higher-ranked one. The
//! score bound ([`super::stop`]) reasons about `ranked_potential` alone, but
//! with a tie-break it is strict: a state that can only tie the `k`-th rank
//! could be the one the expression prefers for a last slot, so the walk ends
//! on the bound only when nothing in the frontier, and no result below the top
//! `k`, can reach the `k`-th rank. Where the `k`-th rank is shared, the tied
//! results the walk found fill the last slots by the expression.
//!
//! The features of a result are [`Facts::features`]; a name absent from the
//! file has weight 0:
//!
//! - `depth`, `n_maps` (independent maps that reached the node), `maps_listed`
//!   (all maps that did), `log_fanout` (`ln(1 + edges the discovering
//!   expansion offered)`) and `overlap` (the share of the question's tokens in
//!   the node id, as the rules scorer uses it);
//! - one-hot `witness:<class>`, `via:<map id>` and `node:<kind prefix>`
//!   (`sym`, `file`, `chunk`, `commit`, `test`, `cfg`, `doc`, `str`) of the
//!   result.
//!
//! A state holds neither the last hop's edge kind nor the reranker frontier's
//! rating of the node (the rating is already in `potential`), so neither is a
//! feature.
//!
//! The file is JSON, keys sorted: `{"features": {"<name>": <weight>, ...},
//! "fitted_on": "<note>", "holdout": ["<item id>", ...], "version": 1}`.
//! `cargo xtask maps fit-tiebreak` writes one; `holdout` lists the gold items
//! kept out of the fit. A handle keeps the hash of the file's bytes.

use super::{State, frontier::node_overlap, trace::hash16};
use crate::edge::WitnessClass;
use lci_core::node::NodeKind;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

/// The file format version.
pub const VERSION: u32 = 1;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// What a result's features are computed from: the fields of a result card.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub depth: u32,
    pub n_maps: usize,
    pub maps_listed: usize,
    pub fanout: u32,
    pub overlap: f64,
    pub witness: Option<String>,
    pub via: Option<String>,
    /// The node kind's prefix (`sym`, `file`, ...).
    pub node_kind: String,
}

impl Facts {
    /// The facts of a result `state`, for a question with these `query`
    /// tokens (`crate::walk::question_tokens`).
    pub fn of(state: &State, query: &[String]) -> Self {
        Self {
            depth: state.depth,
            n_maps: state.n_maps(),
            maps_listed: state.maps.len(),
            fanout: state.fanout,
            overlap: node_overlap(query, &state.node),
            witness: state.witness.map(|w| w.as_str().to_owned()),
            via: state.via.map(str::to_owned),
            node_kind: state.node.kind().prefix().to_owned(),
        }
    }

    /// The facts a result card of the CLI's response states: its own fields
    /// plus the three [`Facts::card_fields`]. None when a field is missing
    /// (a response from before the tie-break existed).
    pub fn from_card(card: &Value) -> Option<Self> {
        let text = |key: &str| card[key].as_str().map(str::to_owned);
        Some(Self {
            depth: u32::try_from(card["depth"].as_u64()?).ok()?,
            n_maps: usize::try_from(card["n_maps"].as_u64()?).ok()?,
            maps_listed: card["maps"].as_array()?.len(),
            fanout: u32::try_from(card["fanout"].as_u64()?).ok()?,
            overlap: card["overlap"].as_f64()?,
            witness: text("witness"),
            via: text("via"),
            node_kind: text("node_kind")?,
        })
    }

    /// The facts a card does not already carry, as card fields.
    pub fn card_fields(&self) -> [(&'static str, Value); 3] {
        [
            ("node_kind", json!(self.node_kind)),
            ("overlap", json!(self.overlap)),
            ("fanout", json!(self.fanout)),
        ]
    }

    /// The non-zero features, by name.
    pub fn features(&self) -> Vec<(String, f64)> {
        let mut features = vec![
            ("depth".to_owned(), f64::from(self.depth)),
            ("n_maps".to_owned(), self.n_maps as f64),
            ("maps_listed".to_owned(), self.maps_listed as f64),
            ("log_fanout".to_owned(), f64::from(self.fanout).ln_1p()),
            ("overlap".to_owned(), self.overlap),
            (format!("node:{}", self.node_kind), 1.0),
        ];
        features.extend(self.witness.iter().map(|w| (format!("witness:{w}"), 1.0)));
        features.extend(self.via.iter().map(|m| (format!("via:{m}"), 1.0)));
        features.retain(|(_, value)| *value != 0.0);
        features
    }
}

/// Whether `name` is a feature [`Facts::features`] can produce.
fn is_feature(name: &str) -> bool {
    match name.split_once(':') {
        None => ["depth", "n_maps", "maps_listed", "log_fanout", "overlap"].contains(&name),
        Some(("witness", class)) => WitnessClass::parse(class).is_some(),
        Some(("via", map)) => !map.is_empty(),
        Some(("node", kind)) => NodeKind::ALL.iter().any(|k| k.prefix() == kind),
        Some(_) => false,
    }
}

/// A fitted tie-break; see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct TieBreak {
    pub features: BTreeMap<String, f64>,
    /// Item ids kept out of the fit; an evaluation uses only these.
    pub holdout: BTreeSet<String>,
    /// A note on what the fit used (no paths).
    pub fitted_on: String,
    hash: String,
}

impl TieBreak {
    /// A tie-break of these weights; its hash is that of the file text it
    /// writes. Weights must be finite and named by a known feature.
    pub fn new(
        features: BTreeMap<String, f64>,
        holdout: BTreeSet<String>,
        fitted_on: String,
    ) -> io::Result<Self> {
        for (name, weight) in &features {
            if !is_feature(name) {
                return Err(invalid(format!("unknown tie-break feature {name:?}")));
            }
            if !weight.is_finite() {
                return Err(invalid(format!("the weight of {name} is not a number")));
            }
        }
        let mut tie_break = Self {
            features,
            holdout,
            fitted_on,
            hash: String::new(),
        };
        tie_break.hash = hash16(tie_break.to_json_string().as_bytes());
        Ok(tie_break)
    }

    /// The tie-break in the file at `path`.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_text(&fs::read_to_string(path)?)
    }

    /// The tie-break in `text` (the content of a file); its hash is that of
    /// the bytes.
    pub fn from_text(text: &str) -> io::Result<Self> {
        let doc: Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
        let version = doc["version"].as_u64().unwrap_or(0);
        if version != u64::from(VERSION) {
            return Err(invalid(format!(
                "tie-break version {version}, expected {VERSION}"
            )));
        }
        let features = doc["features"]
            .as_object()
            .ok_or_else(|| invalid("features must be an object"))?
            .iter()
            .map(|(name, weight)| {
                let weight = weight
                    .as_f64()
                    .ok_or_else(|| invalid(format!("the weight of {name} is not a number")))?;
                Ok((name.clone(), weight))
            })
            .collect::<io::Result<_>>()?;
        let holdout = doc["holdout"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|id| {
                id.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("holdout ids are strings"))
            })
            .collect::<io::Result<_>>()?;
        let fitted_on = doc["fitted_on"].as_str().unwrap_or_default().to_owned();
        let mut tie_break = Self::new(features, holdout, fitted_on)?;
        tie_break.hash = hash16(text.as_bytes());
        Ok(tie_break)
    }

    /// The content hash a handle keeps.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The weighted sum of the result's features.
    pub fn score(&self, facts: &Facts) -> f64 {
        facts
            .features()
            .iter()
            .map(|(name, value)| self.features.get(name).map_or(0.0, |w| w * value))
            .sum()
    }

    /// Pretty JSON with every key sorted, ending in a newline.
    pub fn to_json_string(&self) -> String {
        let mut doc = Map::new();
        doc.insert(
            "features".into(),
            Value::Object(
                self.features
                    .iter()
                    .map(|(name, weight)| (name.clone(), Value::from(*weight)))
                    .collect(),
            ),
        );
        doc.insert("fitted_on".into(), Value::from(self.fitted_on.clone()));
        doc.insert(
            "holdout".into(),
            Value::Array(self.holdout.iter().cloned().map(Value::from).collect()),
        );
        doc.insert("version".into(), Value::from(VERSION));
        // `to_string_pretty` cannot fail on a `Value`.
        let mut text = serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default();
        text.push('\n');
        text
    }

    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.to_json_string())
    }
}
