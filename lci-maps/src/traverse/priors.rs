//! Learned hop priors (slice S6f): the fitted replacement for the guessed
//! [`kind_prior`] table.
//!
//! A [`PriorTable`] holds, per question type (the gold set's name: `impact`,
//! `flow`, `where-to`, `history`), a prior for each (map, edge kind) pair.
//! [`PriorTable::lookup`] falls back to the guess for a type the table does
//! not hold, and, on a [normalised](PriorTable::normalised) table, to the guess
//! scaled onto the fitted scale for a pair missing from a type it holds. A
//! prior in a file must be finite and not negative. The table is plain JSON
//! with sorted keys, so the same fit always writes the same bytes.
//!
//! [`PriorFit`] fits a table from traces. A `hop` or `relax` event is one
//! scored edge. It is a positive when it is the edge that finally parents its
//! `dst` and `dst` lies on the parent chain from a seed to a node the caller
//! calls gold; every other edge (a route a later `relax` replaced, a branch
//! that led nowhere) is a negative. The prior of a pair is
//! `(positives + m * guess) / (n + m)`: the guess weighs as `m` observations.
//! These are base rates, on a smaller scale than the guess when most hops are
//! negatives; a type with no positive hop at all is left out (its fit would
//! only shrink every pair toward 0), so it keeps the guess.

use super::{frontier::kind_prior, trace::hash16};
use crate::EdgeKind;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, io,
    path::Path,
};

/// The table file format version.
pub const VERSION: u32 = 1;

const ALL_KINDS: [EdgeKind; 14] = [
    EdgeKind::QueryMatch,
    EdgeKind::Prior,
    EdgeKind::CallsOrRefs,
    EdgeKind::Contains,
    EdgeKind::Imports,
    EdgeKind::Implements,
    EdgeKind::Tests,
    EdgeKind::ReadsKey,
    EdgeKind::Mentions,
    EdgeKind::Touched,
    EdgeKind::Cochange,
    EdgeKind::CompileErrorAt,
    EdgeKind::Near,
    EdgeKind::DefinedIn,
];

/// The edge kind with this stable name (`EdgeKind::as_str`).
pub fn kind_from_name(name: &str) -> Option<EdgeKind> {
    ALL_KINDS.into_iter().find(|kind| kind.as_str() == name)
}

/// Per question type: map id -> edge kind -> prior.
type Priors = BTreeMap<String, BTreeMap<String, BTreeMap<EdgeKind, f64>>>;

#[derive(Debug, Clone, PartialEq)]
pub struct PriorTable {
    pub version: u32,
    /// Item ids kept out of the fit; an evaluation uses only these.
    pub holdout: BTreeSet<String>,
    pub types: Priors,
    /// Per question type, the factor that puts a guessed prior on the scale of
    /// the type's normalised priors; set by [`PriorTable::normalised`], empty
    /// otherwise.
    fallback_scale: BTreeMap<String, f64>,
}

impl Default for PriorTable {
    fn default() -> Self {
        Self {
            version: VERSION,
            holdout: BTreeSet::new(),
            types: Priors::new(),
            fallback_scale: BTreeMap::new(),
        }
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl PriorTable {
    /// The prior of a hop of `kind` offered by `map` for `question_type`; the
    /// guessed [`kind_prior`] when the table has no such type. For a pair a
    /// held type lacks, the guess on the type's scale on a normalised table:
    /// `min(1, kind_prior(kind) * c)`, `c` being the type's normalised priors
    /// summed over the sum of the guesses for the same entries (see
    /// [`PriorTable::normalised`]); the plain guess on any other table.
    pub fn lookup(&self, question_type: &str, map: &str, kind: EdgeKind) -> f64 {
        let Some(maps) = self.types.get(question_type) else {
            return kind_prior(kind);
        };
        maps.get(map)
            .and_then(|kinds| kinds.get(&kind))
            .copied()
            .unwrap_or_else(|| match self.fallback_scale.get(question_type) {
                Some(scale) => (kind_prior(kind) * scale).min(1.0),
                None => kind_prior(kind),
            })
    }

    /// The table the engine prices hops with: per question type, every prior
    /// divided by that type's largest, so the best hop kind is 1 and the rest
    /// keep their ratios (fitted base rates are far below the guess, whose
    /// scale the gate and beam arithmetic assumes). A type whose largest
    /// prior is not positive is left as it is.
    ///
    /// A guess is on the guess scale, not the fitted one (a fitted call at
    /// 0.088 against a guessed 1.0), so a pair missing from a held type would
    /// outrank every fitted pair. Each scaled type records
    /// `c = (sum of its normalised priors) / (sum of the guesses for the kinds
    /// of the same entries)`, and [`PriorTable::lookup`] prices a missing pair
    /// at `min(1, guess * c)`. A type the table lacks keeps the plain guess.
    pub fn normalised(&self) -> Self {
        let mut table = self.clone();
        for (question_type, maps) in &mut table.types {
            let top = maps
                .values()
                .flat_map(BTreeMap::values)
                .copied()
                .fold(0.0, f64::max);
            if top > 0.0 && top.is_finite() {
                maps.values_mut()
                    .flat_map(BTreeMap::values_mut)
                    .for_each(|prior| *prior /= top);
                let fitted: f64 = maps.values().flat_map(BTreeMap::values).sum();
                let guessed: f64 = maps
                    .values()
                    .flat_map(BTreeMap::keys)
                    .map(|kind| kind_prior(*kind))
                    .sum();
                if guessed > 0.0 {
                    table
                        .fallback_scale
                        .insert(question_type.clone(), fitted / guessed);
                }
            }
        }
        table
    }

    /// Pretty JSON with every key sorted, ending in a newline.
    pub fn to_json_string(&self) -> String {
        let mut types = Map::new();
        for (question_type, maps) in &self.types {
            let mut by_map = Map::new();
            for (map, kinds) in maps {
                let mut by_kind = Map::new();
                let mut named: Vec<(&str, f64)> =
                    kinds.iter().map(|(k, p)| (k.as_str(), *p)).collect();
                named.sort_by(|a, b| a.0.cmp(b.0));
                for (name, prior) in named {
                    by_kind.insert(name.to_owned(), Value::from(prior));
                }
                by_map.insert(map.clone(), Value::Object(by_kind));
            }
            types.insert(question_type.clone(), Value::Object(by_map));
        }
        let mut doc = Map::new();
        doc.insert(
            "holdout".into(),
            Value::Array(self.holdout.iter().cloned().map(Value::from).collect()),
        );
        doc.insert("priors".into(), Value::Object(types));
        doc.insert("version".into(), Value::from(self.version));
        // `Value`'s Display is infallible and `to_string_pretty` cannot fail on a `Value`.
        let mut text = serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default();
        text.push('\n');
        text
    }

    pub fn from_json_str(text: &str) -> io::Result<Self> {
        let doc: Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
        let version = doc["version"].as_u64().unwrap_or(0);
        if version != u64::from(VERSION) {
            return Err(invalid(format!(
                "prior table version {version}, expected {VERSION}"
            )));
        }
        let object = |value: &Value, what: &str| -> io::Result<Map<String, Value>> {
            value
                .as_object()
                .cloned()
                .ok_or_else(|| invalid(format!("{what} must be an object")))
        };
        let mut table = Self::default();
        for id in doc["holdout"].as_array().into_iter().flatten() {
            let id = id
                .as_str()
                .ok_or_else(|| invalid("holdout ids are strings"))?;
            table.holdout.insert(id.to_owned());
        }
        for (question_type, maps) in object(&doc["priors"], "priors")? {
            for (map, kinds) in object(&maps, "a question type")? {
                for (name, prior) in object(&kinds, "a map")? {
                    let kind = kind_from_name(&name)
                        .ok_or_else(|| invalid(format!("unknown edge kind {name}")))?;
                    let prior = prior
                        .as_f64()
                        .ok_or_else(|| invalid("a prior must be a number"))?;
                    if !prior.is_finite() || prior < 0.0 {
                        return Err(invalid(format!(
                            "the prior of {question_type}/{map}/{name} must be finite and not negative"
                        )));
                    }
                    table
                        .types
                        .entry(question_type.clone())
                        .or_default()
                        .entry(map.clone())
                        .or_default()
                        .insert(kind, prior);
                }
            }
        }
        Ok(table)
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_json_str(&fs::read_to_string(path)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.to_json_string())
    }
}

/// The hop priors of one engine run: the guessed [`kind_prior`], or a fitted
/// table read for one question type (normalised, see
/// [`PriorTable::normalised`]). It remembers the hash of the file's bytes,
/// which a continuation handle keeps so a resume refuses a changed table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HopPriors {
    learned: Option<Learned>,
}

#[derive(Debug, Clone, PartialEq)]
struct Learned {
    table: PriorTable,
    question_type: String,
    hash: String,
}

/// The guessed priors, for a caller that holds none of its own.
pub static GUESSED: HopPriors = HopPriors { learned: None };

impl HopPriors {
    /// The table in the file at `path`, for `question_type`.
    pub fn open(path: impl AsRef<Path>, question_type: &str) -> io::Result<Self> {
        Self::from_text(&fs::read_to_string(path)?, question_type)
    }

    /// The table in `text` (the content of a table file).
    pub fn from_text(text: &str, question_type: &str) -> io::Result<Self> {
        Ok(Self {
            learned: Some(Learned {
                table: PriorTable::from_json_str(text)?.normalised(),
                question_type: question_type.to_owned(),
                hash: hash16(text.as_bytes()),
            }),
        })
    }

    /// The prior of a hop of `kind` offered by `map`.
    pub fn prior(&self, map: &str, kind: EdgeKind) -> f64 {
        match &self.learned {
            Some(learned) => learned.table.lookup(&learned.question_type, map, kind),
            None => kind_prior(kind),
        }
    }

    /// The question type and the content hash of the table; none when the
    /// priors are the guess.
    pub fn fingerprint(&self) -> Option<(&str, &str)> {
        self.learned
            .as_ref()
            .map(|learned| (learned.question_type.as_str(), learned.hash.as_str()))
    }

    /// The fingerprint as the `start` and `resume` events show it.
    pub(super) fn json(&self) -> Option<Value> {
        self.fingerprint()
            .map(|(question_type, hash)| json!({"question_type": question_type, "hash": hash}))
    }
}

/// How many hops of a (map, kind) pair were seen and how many were positive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub n: u64,
    pub positives: u64,
}

/// One scored edge of a trace, labelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledHop {
    pub map: String,
    pub kind: EdgeKind,
    pub positive: bool,
}

/// Labels the `hop` and `relax` events of a trace (see the module docs).
/// `seeds` are the start nodes; `is_gold` says whether a node is an answer.
/// An edge with a kind name the engine does not know is not counted.
pub fn label_hops(
    events: &[Value],
    seeds: &[String],
    mut is_gold: impl FnMut(&str) -> bool,
) -> Vec<LabeledHop> {
    struct Edge<'a> {
        src: &'a str,
        dst: &'a str,
        map: &'a str,
        kind: Option<EdgeKind>,
    }
    let mut edges: Vec<Edge> = Vec::new();
    let mut parent_edge: HashMap<&str, usize> = HashMap::new();
    for event in events {
        if !matches!(event["ev"].as_str(), Some("hop" | "relax")) {
            continue;
        }
        let (Some(src), Some(dst)) = (event["src"].as_str(), event["dst"].as_str()) else {
            continue;
        };
        parent_edge.insert(dst, edges.len());
        edges.push(Edge {
            src,
            dst,
            map: event["map"].as_str().unwrap_or_default(),
            kind: event["kind"].as_str().and_then(kind_from_name),
        });
    }
    let seeds: HashSet<&str> = seeds.iter().map(String::as_str).collect();
    let nodes: BTreeSet<&str> = edges
        .iter()
        .map(|e| e.dst)
        .chain(seeds.iter().copied())
        .collect();
    let mut positive = vec![false; edges.len()];
    for node in nodes.into_iter().filter(|node| is_gold(node)) {
        // The chain up to a node with no parent; kept only when it is a seed.
        let (mut chain, mut at, mut seen) = (Vec::new(), node, HashSet::from([node]));
        while let Some(&index) = parent_edge.get(at) {
            chain.push(index);
            at = edges[index].src;
            if !seen.insert(at) {
                break;
            }
        }
        if seeds.contains(at) {
            for index in chain {
                positive[index] = true;
            }
        }
    }
    edges
        .iter()
        .zip(positive)
        .filter_map(|(edge, positive)| {
            Some(LabeledHop {
                map: edge.map.to_owned(),
                kind: edge.kind?,
                positive,
            })
        })
        .collect()
}

/// The hop counts of a set of traces, per question type.
#[derive(Debug, Clone, Default)]
pub struct PriorFit {
    counts: BTreeMap<String, BTreeMap<(String, EdgeKind), Count>>,
}

impl PriorFit {
    pub fn add(&mut self, question_type: &str, hops: &[LabeledHop]) {
        let counts = self.counts.entry(question_type.to_owned()).or_default();
        for hop in hops {
            let count = counts.entry((hop.map.clone(), hop.kind)).or_default();
            count.n += 1;
            count.positives += u64::from(hop.positive);
        }
    }

    pub fn counts(&self) -> &BTreeMap<String, BTreeMap<(String, EdgeKind), Count>> {
        &self.counts
    }

    /// `(positives + m * guess) / (n + m)` per pair; a type without a
    /// positive hop is left out.
    pub fn fit(&self, smoothing: f64, holdout: BTreeSet<String>) -> PriorTable {
        let mut table = PriorTable {
            holdout,
            ..PriorTable::default()
        };
        for (question_type, pairs) in &self.counts {
            if pairs.values().all(|count| count.positives == 0) {
                continue;
            }
            for ((map, kind), count) in pairs {
                let prior = (count.positives as f64 + smoothing * kind_prior(*kind))
                    / (count.n as f64 + smoothing);
                table
                    .types
                    .entry(question_type.clone())
                    .or_default()
                    .entry(map.clone())
                    .or_default()
                    .insert(*kind, prior);
            }
        }
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hop(ev: &str, src: &str, dst: &str, kind: &str) -> Value {
        json!({"ev": ev, "src": src, "dst": dst, "kind": kind, "map": "graph.scip"})
    }

    fn labels(events: &[Value], gold: &str) -> Vec<(String, bool)> {
        let hops = label_hops(events, &["s".to_owned()], |node| node == gold);
        hops.into_iter()
            .map(|h| (h.kind.as_str().to_owned(), h.positive))
            .collect()
    }

    #[test]
    fn a_hop_is_positive_only_on_the_final_chain_from_a_seed_to_gold() {
        let events = [
            hop("hop", "s", "a", "calls_or_refs"),
            // The first route to g, later replaced by the one through a.
            hop("hop", "s", "g", "contains"),
            hop("relax", "a", "g", "calls_or_refs"),
            hop("hop", "s", "b", "contains"),
            hop("hop", "b", "c", "contains"),
            // Not a hop: ignored.
            json!({"ev": "alt_path", "src": "b", "dst": "g", "kind": "contains"}),
        ];
        let got = labels(&events, "g");
        let want = [
            ("calls_or_refs", true),
            ("contains", false),
            ("calls_or_refs", true),
            ("contains", false),
            ("contains", false),
        ];
        assert_eq!(
            got,
            want.map(|(k, p)| (k.to_owned(), p)),
            "positives are s->a and a->g"
        );
        // A chain that does not start at a seed counts for nothing.
        let orphan = [hop("hop", "x", "g", "calls_or_refs")];
        assert_eq!(labels(&orphan, "g"), [("calls_or_refs".to_owned(), false)]);
    }

    /// Item `i` reaches its gold node `g` by a call in 7 of 10 items and by
    /// containment in 2 of 5; every other hop leads to a dead end `d`.
    fn planted_traces() -> PriorFit {
        let mut fit = PriorFit::default();
        for i in 0..100 {
            let call = if i % 10 < 7 { "g" } else { "d" };
            let contain = if i % 5 < 2 { "g" } else { "d" };
            let events = [
                hop("hop", "s", &format!("{call}1"), "calls_or_refs"),
                hop("hop", "s", &format!("{contain}2"), "contains"),
            ];
            let seeds = ["s".to_owned()];
            let hops = label_hops(&events, &seeds, |node| node.starts_with('g'));
            fit.add("flow", &hops);
        }
        fit
    }

    #[test]
    fn fitting_traces_recovers_the_planted_priors() {
        let table = planted_traces().fit(1.0, BTreeSet::new());
        let calls = table.lookup("flow", "graph.scip", EdgeKind::CallsOrRefs);
        let contains = table.lookup("flow", "graph.scip", EdgeKind::Contains);
        // (70 + 1 * 1.0) / 101 and (40 + 1 * 0.5) / 101.
        assert!((calls - 0.70).abs() < 0.01, "{calls}");
        assert!((contains - 0.40).abs() < 0.01, "{contains}");
    }

    #[test]
    fn unknown_types_and_pairs_fall_back_to_the_guess_and_a_type_without_positives_is_left_out() {
        let mut fit = planted_traces();
        let dead = [hop("hop", "s", "d", "calls_or_refs")];
        fit.add("history", &label_hops(&dead, &["s".to_owned()], |_| false));
        let table = fit.fit(1.0, BTreeSet::new());
        for (question_type, map, kind) in [
            ("impact", "graph.scip", EdgeKind::CallsOrRefs),
            ("history", "graph.scip", EdgeKind::CallsOrRefs),
            ("flow", "other.map", EdgeKind::CallsOrRefs),
            ("flow", "graph.scip", EdgeKind::Tests),
        ] {
            assert_eq!(table.lookup(question_type, map, kind), kind_prior(kind));
        }
    }

    #[test]
    fn the_same_traces_write_the_same_bytes_and_the_file_loads_back() {
        let holdout: BTreeSet<String> = ["b", "a"].map(String::from).into();
        let table = planted_traces().fit(1.0, holdout);
        let text = table.to_json_string();
        assert_eq!(
            text,
            planted_traces()
                .fit(1.0, table.holdout.clone())
                .to_json_string()
        );
        assert_eq!(PriorTable::from_json_str(&text).unwrap(), table);
        // Keys are sorted whatever the insertion order.
        assert!(text.find("calls_or_refs").unwrap() < text.find("contains").unwrap());
        assert!(text.find("\"a\"").unwrap() < text.find("\"b\"").unwrap());
        assert!(PriorTable::from_json_str("{\"version\": 9, \"priors\": {}}").is_err());
    }

    #[test]
    fn normalising_scales_each_type_by_its_largest_prior_and_leaves_the_rest() {
        let mut table = PriorTable::default();
        for (question_type, map, kind, prior) in [
            ("impact", "graph.scip", EdgeKind::DefinedIn, 0.13),
            ("impact", "graph.scip", EdgeKind::CallsOrRefs, 0.013),
            ("impact", "scip.impact", EdgeKind::CallsOrRefs, 0.065),
            ("flow", "graph.scip", EdgeKind::CallsOrRefs, 0.4),
            ("dead", "graph.scip", EdgeKind::CallsOrRefs, 0.0),
        ] {
            table
                .types
                .entry(question_type.into())
                .or_default()
                .entry(map.into())
                .or_default()
                .insert(kind, prior);
        }
        let got = table.normalised();
        let at = |question_type: &str, map: &str, kind| got.lookup(question_type, map, kind);
        assert_eq!(at("impact", "graph.scip", EdgeKind::DefinedIn), 1.0);
        assert!((at("impact", "graph.scip", EdgeKind::CallsOrRefs) - 0.1).abs() < 1e-12);
        assert!((at("impact", "scip.impact", EdgeKind::CallsOrRefs) - 0.5).abs() < 1e-12);
        assert_eq!(at("flow", "graph.scip", EdgeKind::CallsOrRefs), 1.0);
        assert_eq!(at("dead", "graph.scip", EdgeKind::CallsOrRefs), 0.0);
        // The file keeps the raw values and the plain guess for an absent pair.
        assert_eq!(
            table.lookup("impact", "graph.scip", EdgeKind::DefinedIn),
            0.13
        );
        assert_eq!(
            table.lookup("impact", "graph.scip", EdgeKind::Tests),
            kind_prior(EdgeKind::Tests)
        );
        // A type the table lacks keeps the plain guess when normalised too, and
        // so does a type left unscaled (no positive prior).
        assert_eq!(
            at("history", "graph.scip", EdgeKind::Tests),
            kind_prior(EdgeKind::Tests)
        );
        assert_eq!(
            at("dead", "graph.scip", EdgeKind::Tests),
            kind_prior(EdgeKind::Tests)
        );
    }

    #[test]
    fn a_pair_missing_from_a_held_type_falls_back_to_the_guess_on_the_fitted_scale() {
        // Fitted calls at 0.088 and containment at 0.044: normalised 1 and 0.5.
        let mut table = PriorTable::default();
        for (map, kind, prior) in [
            ("graph.scip", EdgeKind::CallsOrRefs, 0.088),
            ("graph.scip", EdgeKind::Contains, 0.044),
            ("scip.impact", EdgeKind::CallsOrRefs, 0.022),
        ] {
            table
                .types
                .entry("flow".into())
                .or_default()
                .entry(map.into())
                .or_default()
                .insert(kind, prior);
        }
        let got = table.normalised();
        // c = (1 + 0.5 + 0.25) / (1.0 + 0.5 + 1.0), the guesses of the three
        // entries' kinds.
        let scale = 1.75 / 2.5;
        let expected = kind_prior(EdgeKind::Tests) * scale;
        let tests = got.lookup("flow", "graph.scip", EdgeKind::Tests);
        assert!((tests - expected).abs() < 1e-12, "{tests}");
        // Under the plain guess, which was far above these fitted base rates.
        assert!(tests < kind_prior(EdgeKind::Tests));
        // A map the type does not hold falls back the same way, and never past 1.
        assert_eq!(got.lookup("flow", "other.map", EdgeKind::Tests), tests);
        assert!(got.lookup("flow", "other.map", EdgeKind::CallsOrRefs) <= 1.0);
        // A type absent from the table keeps the plain guess.
        assert_eq!(
            got.lookup("impact", "graph.scip", EdgeKind::Tests),
            kind_prior(EdgeKind::Tests)
        );
        // Fitted pairs are not touched.
        assert_eq!(got.lookup("flow", "graph.scip", EdgeKind::CallsOrRefs), 1.0);
    }

    #[test]
    fn a_prior_that_is_negative_or_not_finite_is_rejected_on_load() {
        let table = |prior: &str| {
            format!(
                r#"{{"version": 1, "priors": {{"flow": {{"graph.scip": {{"calls_or_refs": {prior}}}}}}}}}"#
            )
        };
        assert!(PriorTable::from_json_str(&table("0.5")).is_ok());
        assert!(PriorTable::from_json_str(&table("0")).is_ok());
        for bad in ["-0.1", "-1e-9", "1e999", "\"NaN\"", "null"] {
            assert!(PriorTable::from_json_str(&table(bad)).is_err(), "{bad}");
        }
        let error = PriorTable::from_json_str(&table("-0.1")).unwrap_err();
        assert!(error.to_string().contains("flow/graph.scip/calls_or_refs"));
        assert!(HopPriors::from_text(&table("-0.1"), "flow").is_err());
    }
}
