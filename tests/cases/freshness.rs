//! Freshness harness (K2a-1): every traverse map and the three search
//! channels asked about the same edit, and the answer compared with the tree.
//!
//! One tree holds C# files (served by the documents fake language server, so
//! `lsp.live` and `lsp.symbols` have a server) and a small Rust sidecar (a
//! trait with an impl, a config struct and its example TOML, the data that
//! `scip.implements` and `config.key` read). Generation `g` appends the probe
//! `ProbeG<g>` to each of them, with no line moving; the line-shift case puts
//! one blank line on top of `Calculator.cs` instead. The SCIP index is written
//! outside the tree, by a scanner of the tree, and is rewritten only when a
//! case says so (a longer file each time: the stamp is `(mtime, length)` and a
//! Windows mtime may not move).
//!
//! No sleep: a case edits, searches (which is what the product does before it
//! walks: the hybrid search refreshes a stale index under `on-search`), asks
//! every map once, and compares. The one wait is the product's own
//! asynchronous window under `[run_profile.lsp] sync = "watch"`: after an edit
//! the case waits, bounded, until the watcher has told the session (K8a).
//! Lag is a generation distance: a map's lag at generation `g` is `g` minus
//! the last generation it was fresh at.
//!
//! What counts as seeing the edit: the map reached a node that names the
//! probe or the chunk that holds the probe in the tree as it is now; a channel
//! is asked for code that holds the probe. The two live maps (`grep.live`,
//! `lsp.live`) are asked alone about the probe's own symbol (with a SCIP file
//! written for the tree as it is, as an input of that call), and saw the edit
//! when they offer an edge from it: the probe's name is in the tree only from
//! its generation on, so an answer made from older bytes or an older server
//! has none. Each generation is compared with the one before it. A map that
//! answers nothing is never labelled stale: it is a gap with no known data
//! behind it. The verdicts:
//!
//! * `Fresh`: it saw the edit.
//! * `StaleLabelled`: it did not, and the answer says so: with
//!   `[run_profile.traverse] map_freshness` on (K2a-2) its `maps[]` entry says
//!   `stale` with a `lag` equal to the number of files its data is behind the
//!   tree by (the label counts files, each once; the verdict's own lag counts
//!   generations; the gap rule accepts any lag at least that). The harness runs with the key on and checks, with a
//!   second world, that the same maps are plain gaps with it off, so the label
//!   and not the data is what changed.
//! * `Gap`: it did not and the answer is silent or says "not stale" (the live
//!   maps and the search channels). `EXPECTED_GAPS`,
//!   `EXPECTED_LABELLED` and `VERIFIED_ON_READ` list today's outcomes and are
//!   asserted in both directions. `VERIFIED_ON_READ` holds the maps that are
//!   labelled only with `[run_profile.freshness] verify_on_read` (K2b) on, on
//!   top of `map_freshness`; with it off they are gaps again.
//!
//! `git.history` and `exact.pickaxe` read committed history: an uncommitted
//! edit is not in it (correct by definition), and a commit is.
use super::*;
use local_code_intelligence::{
    app::{
        traverse::{self, AppSeeds, BudgetArgs, Call, Dirs, SteerArgs},
        verify_scans_on_this_thread,
    },
    config::{IndexFreshness, LspPlaces},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

// ---- the tree -------------------------------------------------------------

/// `Calculator.cs` at generation `generation`: `Combine` plus one probe method
/// per generation, each on its own line before the closing brace (no earlier
/// line moves). `shifted` puts one blank line on top (every line moves).
fn calculator(generation: u32, shifted: bool) -> String {
    let mut text = String::new();
    if shifted {
        text.push('\n');
    }
    text.push_str("namespace Acceptance;\npublic sealed class Calculator\n{\n");
    text.push_str("    public int Combine(int a, int b) => a + b;\n");
    for g in 1..=generation {
        text.push_str(&format!(
            "    public int ProbeG{g}() => Combine({g}, {g});\n"
        ));
    }
    text.push_str("}\n");
    text
}

/// `CallSite.cs`: `Run` calls `Combine` and every probe so far.
fn call_site(generation: u32) -> String {
    let probes: String = (1..=generation)
        .map(|g| format!(" + value.ProbeG{g}()"))
        .collect();
    format!(
        "namespace Acceptance;\npublic static class CallSite\n{{\n    public static int Run(Calculator value) => value.Combine(1, 2){probes};\n}}\n"
    )
}

/// The Rust sidecar: a trait, its impl for `Foo`, and a reader of the config
/// field `rate`; every generation adds a type that implements the trait and a
/// second reader.
fn shapes(generation: u32) -> String {
    let mut text = String::from(
        "pub struct Foo;\npub trait Speak { fn say(&self); }\nimpl Speak for Foo { fn say(&self) {} }\npub fn read_rate(c: &Config) -> u32 { c.rate }\n",
    );
    for g in 1..=generation {
        text.push_str(&format!(
            "pub struct ProbeG{g};\nimpl Speak for ProbeG{g} {{ fn say(&self) {{}} }}\npub fn ProbeG{g}_rate(c: &Config) -> u32 {{ c.rate }}\n"
        ));
    }
    text
}

const CONFIG_RS: &str = "pub struct Config {\n    pub rate: u32,\n}\n";

pub(super) fn write_tree(root: &Path, generation: u32, shifted: bool) {
    write(root, "src/Calculator.cs", &calculator(generation, shifted));
    write(root, "src/CallSite.cs", &call_site(generation));
    write(root, "src/shapes.rs", &shapes(generation));
    write(root, "lci-core/src/config.rs", CONFIG_RS);
    write(root, "config.example.toml", "rate = 1\n");
}

/// The files of [`write_tree`] that the language-server sync forwards (the
/// TOML example is no source file).
const SYNCED_SOURCES: [&str; 4] = [
    "src/Calculator.cs",
    "src/CallSite.cs",
    "src/shapes.rs",
    "lci-core/src/config.rs",
];

/// The content hash of every source file of the tree, by relative path: the
/// set the product counts for `map_freshness` (the chunk scan with prose off,
/// the files an adapter indexes), so `files_behind` counts what the label
/// counts, not every file on disk.
fn tree_hashes(root: &Path) -> BTreeMap<String, String> {
    lci_core::chunk::scan_incremental_with(root, &Default::default(), false)
        .unwrap()
        .files
        .into_iter()
        .map(|(path, file)| (path, file.content_hash))
        .collect()
}

pub(super) const COMBINE: &str = "sym:rust:Calculator.cs::Combine/fn";
pub(super) const RUN: &str = "sym:rust:CallSite.cs::Run/fn";
pub(super) const SPEAK: &str = "sym:rust:shapes::Speak/trait";
const RATE_KEY: &str = "cfg:toml:rate";

/// A function found on a line of the tree: `(zero-based line, name, column,
/// declares, line length)`.
type Found = (u64, String, u64, bool, u64);

/// The functions and calls of single-line C# members: an identifier followed
/// by `(` declares a function when a type-like word precedes it and `=>` or
/// `{` follows, else it is a call. What an indexer would write into SCIP.
fn scan_functions(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut words: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                words.push((start, i));
            } else {
                i += 1;
            }
        }
        for (at, &(start, end)) in words.iter().enumerate() {
            if !line[end..].starts_with('(') {
                continue;
            }
            let after = &line[end..];
            let declares = at > 0 && {
                let (previous_start, previous_end) = words[at - 1];
                !["return", "new", "await", "throw", "else", "case"]
                    .contains(&&line[previous_start..previous_end])
                    && line[previous_end..start]
                        .chars()
                        .all(|c| " \t<>[]?*&,".contains(c))
                    && (after.contains('{') || after.contains("=>"))
            };
            out.push((
                number as u64,
                line[start..end].to_owned(),
                start as u64,
                declares,
                line.len() as u64,
            ));
        }
    }
    out
}

type ScipSymbols = (
    Vec<scip::types::Occurrence>,
    Vec<scip::types::SymbolInformation>,
);

/// One SCIP occurrence; `body` makes it a definition.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn occurrence(path: &str, range: [usize; 3], body: Option<[usize; 4]>) -> scip::types::Occurrence {
    let mut occurrence = scip::types::Occurrence::default();
    occurrence.range = range.iter().map(|n| *n as i32).collect();
    occurrence.symbol = format!("rust-analyzer cargo demo 0.1.0 {path}");
    if let Some(body) = body {
        occurrence.symbol_roles = 1;
        occurrence.enclosing_range = body.iter().map(|n| *n as i32).collect();
    }
    occurrence
}

#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn information(
    path: &str,
    kind: scip::types::symbol_information::Kind,
) -> scip::types::SymbolInformation {
    let mut info = scip::types::SymbolInformation::default();
    info.symbol = format!("rust-analyzer cargo demo 0.1.0 {path}");
    info.kind = kind.into();
    info
}

/// What an indexer would write for the sidecar's lines: definitions with their
/// kinds, the references of the impl header, and the reads of `Config#rate`.
fn scan_rust(text: &str, module: &str) -> ScipSymbols {
    use scip::types::symbol_information::Kind;
    let (mut occurrences, mut infos) = (Vec::new(), Vec::new());
    let name_at = |line: &str, needle: &str| {
        line.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"))
    };
    for (n, line) in text.lines().enumerate() {
        let whole = [n, 0, n, line.len()];
        if let Some(rest) = line.strip_prefix("pub struct ") {
            let name = rest.trim_end_matches(';');
            let at = name_at(line, name);
            let path = format!("{module}/{name}#");
            occurrences.push(occurrence(&path, [n, at, at + name.len()], Some(whole)));
            infos.push(information(&path, Kind::Struct));
        } else if let Some(rest) = line.strip_prefix("pub trait ") {
            let name = rest.split(' ').next().unwrap();
            let at = name_at(line, name);
            let path = format!("{module}/{name}#");
            occurrences.push(occurrence(&path, [n, at, at + name.len()], Some(whole)));
            infos.push(information(&path, Kind::Trait));
            let method = "say";
            let from = name_at(line, "fn say");
            let at = from + 3;
            let path = format!("{module}/{name}#{method}().");
            occurrences.push(occurrence(
                &path,
                [n, at, at + method.len()],
                Some([n, from, n, line.rfind(';').unwrap() + 1]),
            ));
            infos.push(information(&path, Kind::Method));
        } else if let Some(rest) = line.strip_prefix("impl ") {
            let (tr, rest) = rest.split_once(" for ").unwrap();
            let ty = rest.split(' ').next().unwrap();
            let (tr_at, ty_at) = (name_at(line, tr), name_at(line, &format!("for {ty}")) + 4);
            occurrences.push(occurrence(
                &format!("{module}/{tr}#"),
                [n, tr_at, tr_at + tr.len()],
                None,
            ));
            occurrences.push(occurrence(
                &format!("{module}/{ty}#"),
                [n, ty_at, ty_at + ty.len()],
                None,
            ));
            let from = name_at(line, "fn say");
            let at = from + 3;
            let path = format!("{module}/impl#[{ty}][{tr}]say().");
            occurrences.push(occurrence(
                &path,
                [n, at, at + 3],
                Some([n, from, n, line.rfind("{}").unwrap() + 2]),
            ));
            infos.push(information(&path, Kind::Method));
        } else if let Some(rest) = line.strip_prefix("pub fn ") {
            let name = rest.split('(').next().unwrap();
            let at = name_at(line, name);
            let path = format!("{module}/{name}().");
            occurrences.push(occurrence(&path, [n, at, at + name.len()], Some(whole)));
            infos.push(information(&path, Kind::Function));
            let config = name_at(line, "Config");
            occurrences.push(occurrence("config/Config#", [n, config, config + 6], None));
            let rate = line.rfind("rate").unwrap();
            occurrences.push(occurrence("config/Config#rate.", [n, rate, rate + 4], None));
        }
    }
    (occurrences, infos)
}

/// Writes the SCIP index of the tree as an indexer run now would: every
/// function with its exact range, its body and its kind. Returns the byte
/// length of the file (the stamp changes with it).
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
pub(super) fn write_scip(root: &Path, index: &Path) -> u64 {
    use scip::types::{Document, symbol_information::Kind};
    let mut message = scip::types::Index::default();
    for file in ["src/Calculator.cs", "src/CallSite.cs"] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let mut document = Document::default();
        document.relative_path = file.to_owned();
        for (line, name, column, declares, length) in scan_functions(&text) {
            let (line, column, length) = (line as usize, column as usize, length as usize);
            let path = format!("m/{name}().");
            let body = declares.then_some([line, 0, line, length]);
            document
                .occurrences
                .push(occurrence(&path, [line, column, column + name.len()], body));
            if declares {
                document.symbols.push(information(&path, Kind::Function));
            }
        }
        message.documents.push(document);
    }
    let text = std::fs::read_to_string(root.join("src/shapes.rs")).unwrap();
    let mut shapes = Document::default();
    shapes.relative_path = "src/shapes.rs".to_owned();
    (shapes.occurrences, shapes.symbols) = scan_rust(&text, "shapes");
    message.documents.push(shapes);
    let mut config = Document::default();
    config.relative_path = "lci-core/src/config.rs".to_owned();
    config.occurrences = vec![
        occurrence("config/Config#", [0, 11, 17], Some([0, 0, 2, 1])),
        occurrence("config/Config#rate.", [1, 8, 12], Some([1, 4, 1, 17])),
    ];
    config.symbols = vec![
        information("config/Config#", Kind::Struct),
        information("config/Config#rate.", Kind::Field),
    ];
    message.documents.push(config);
    std::fs::create_dir_all(index.parent().unwrap()).unwrap();
    scip::write_message_to_file(index, message).unwrap();
    std::fs::metadata(index).unwrap().len()
}

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=freshness",
            "-c",
            "user.email=freshness@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---- what a call showed ----------------------------------------------------

type Reached = BTreeMap<String, BTreeSet<String>>;
type Edges = BTreeMap<String, BTreeSet<(String, String)>>;

/// What one traversal call showed, per map: every edge the trace records (a
/// first route, a better route, a route to a node already reached), and the
/// nodes of the cards that name the map.
struct Walk {
    response: Value,
    reached: Reached,
    edges: Edges,
}

/// A map that ran out of the traversal's time answered nothing, which is not a
/// finding about its data.
fn assert_no_timeout(answer: &Value) {
    let notes = answer["notes"].to_string();
    assert!(!notes.contains("deadline passed"), "{notes}");
}

/// `base` asked another question, with other start nodes and maps: every other
/// field is `base`'s. One place builds these calls (no struct update from a
/// reused `Call`), so a field added to `Call` is named here once.
fn asked<'a>(
    base: &Call<'a>,
    question: &'a str,
    seeds: &'a [String],
    maps: Option<&'a [String]>,
) -> Call<'a> {
    Call {
        scip_index: base.scip_index,
        scip_root: base.scip_root,
        question,
        seeds,
        seed_search: base.seed_search,
        top_k: base.top_k,
        maps,
        budget: base.budget,
        steer: base.steer,
        handle: base.handle,
        lsp: base.lsp,
        lsp_sessions: None,
    }
}

fn read_walk(report: &traverse::Report) -> Walk {
    let mut reached = Reached::new();
    let mut edges = Edges::new();
    let trace = std::fs::read_to_string(report.trace.as_ref().expect("a trace")).unwrap();
    for line in trace.lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        if !matches!(event["ev"].as_str(), Some("hop" | "relax" | "alt_path")) {
            continue;
        }
        let (src, dst) = (
            event["src"].as_str().unwrap().to_owned(),
            event["dst"].as_str().unwrap().to_owned(),
        );
        for map in event["maps"].as_array().unwrap() {
            let map = map.as_str().unwrap().to_owned();
            reached.entry(map.clone()).or_default().insert(dst.clone());
            edges
                .entry(map)
                .or_default()
                .insert((src.clone(), dst.clone()));
        }
    }
    for card in report.response["results"].as_array().into_iter().flatten() {
        for map in card["maps"].as_array().into_iter().flatten() {
            reached
                .entry(map.as_str().unwrap().to_owned())
                .or_default()
                .insert(card["node"].as_str().unwrap().to_owned());
        }
    }
    Walk {
        response: report.response.clone(),
        reached,
        edges,
    }
}

// ---- the world -------------------------------------------------------------

/// The maps of the walk call (`probe.compile` runs cargo and stays out; the
/// seed maps and the two git maps have calls of their own).
const WALK_MAPS: [&str; 10] = [
    "structure.contains",
    "graph.scip",
    "grep.live",
    "config.key",
    "search.lexical",
    "search.vector",
    "scip.impact",
    "scip.implements",
    "lsp.live",
    "exact.closure",
];
/// The maps that read the working tree when they answer.
const LIVE_MAPS: [&str; 2] = ["grep.live", "lsp.live"];
const SEED_MAPS: [&str; 2] = ["seeds.hybrid", "seeds.scip_names"];
/// Every map the append cases ask about, in the order the tables list them.
const SCOPE: [&str; 15] = [
    "seeds.hybrid",
    "seeds.scip_names",
    "graph.scip",
    "structure.contains",
    "grep.live",
    "config.key",
    "search.lexical",
    "search.vector",
    "scip.impact",
    "scip.implements",
    "lsp.live",
    "exact.closure",
    "vec.chunk.code",
    "lexical",
    "lsp.symbols",
];

/// Everything one round of questions showed.
struct Observed {
    index_action: String,
    stale_before_search: bool,
    stale_after_search: bool,
    reached: Reached,
    edges: Edges,
    /// The walk call's answer (cards with their sites).
    walk: Value,
    /// The search report of the call that came first (as JSON).
    search: Value,
    /// For each live map (`grep.live`, `lsp.live`): how many edges it offers
    /// from the probe's own symbol, asked alone about it. The probe's name is
    /// in the tree only from its generation on, so an answer made from older
    /// bytes or an older server has none. The SCIP file of these calls is an
    /// input (a new one per observation), not the file under test.
    probes: BTreeMap<&'static str, usize>,
    /// The maps that asked about the probe can answer nothing for a good
    /// reason (an old index or session does not know it): these answered the
    /// question about `Combine`, which every generation knows.
    answered: BTreeSet<&'static str>,
}

/// The tree as it is now, for the checks that compare an answer with it.
struct Truth {
    generation: u32,
    /// `ProbeG<g>`, or `Combine` before the first edit.
    probe: String,
    /// The chunks of a fresh index that hold the probe.
    chunks: BTreeSet<String>,
    /// The one-based line each function (by name) is declared on now.
    lines: BTreeMap<String, u32>,
}

struct World {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    scip: PathBuf,
    scip_length: u64,
    app: App,
    fake: Arc<FakeModels>,
    models_task: tokio::task::JoinHandle<()>,
    /// The state directory of the application's language-server session.
    session_state: PathBuf,
    /// The settings of `lsp.live`, whose server has a state directory of its
    /// own.
    walk_config: Config,
    generation: u32,
    shifted: bool,
    last_fresh: BTreeMap<&'static str, u32>,
    /// `[run_profile.traverse] map_freshness` for the calls of this world.
    labels: bool,
    /// `[run_profile.freshness] verify_on_read` for the calls and the searches
    /// of this world.
    verify: bool,
    /// `[run_profile.lsp] sync = "watch"` for the application's language
    /// servers (`lsp.symbols`).
    sync: bool,
    /// `[run_profile.lsp] places` for the calls of this world (K10a): `scip`,
    /// or `outline`, which places `lsp.live` by the server's own outline.
    places: LspPlaces,
    /// Observations made so far (names the SCIP file of each probe call).
    probe_runs: std::cell::Cell<u32>,
    /// The source files of the tree (path and content hash) as they were when
    /// the SCIP file was last written: a map built from the file is behind by
    /// the files that differ from this.
    scip_tree: BTreeMap<String, String>,
}

impl World {
    /// With the map labels (`map_freshness`), `verify_on_read` and the
    /// language-server sync on.
    async fn start(freshness: IndexFreshness) -> World {
        World::start_with(freshness, true, true, true).await
    }

    async fn start_with(
        freshness: IndexFreshness,
        labels: bool,
        verify: bool,
        sync: bool,
    ) -> World {
        World::start_gated(freshness, labels, verify, sync, false).await
    }

    /// [`World::start_with`] with `[run_profile.freshness] stat_gate` as given.
    async fn start_gated(
        freshness: IndexFreshness,
        labels: bool,
        verify: bool,
        sync: bool,
        stat_gate: bool,
    ) -> World {
        let (temp, mut config, fake, models_task) = fixture().await;
        config.run_profile.freshness.verify_on_read = verify;
        config.run_profile.freshness.stat_gate = stat_gate;
        config.run_profile.lsp.sync = if sync {
            local_code_intelligence::config::LspSync::Watch
        } else {
            local_code_intelligence::config::LspSync::Off
        };
        config.traverse.enabled = true;
        // The exact cards would otherwise start the walk from the probes.
        config.traverse.exact_seeds = false;
        // The deadline only bounds a call: a language server that starts slowly
        // on a loaded machine must not turn `lsp.live` into an empty answer.
        config.traverse.deadline_ms = 60_000;
        // The version probe of `lsp.live` is bounded by this (5 s failed under load).
        config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
        config.index.freshness = freshness;
        config.index.stale_check_interval_seconds = 0;
        let session_state = temp.path().join("lsp-session");
        let walk_config =
            with_fake_documents_server(config.clone(), &temp.path().join("lsp-walk"), &[], 10);
        let config = with_fake_documents_server(config, &session_state, &[], 10);
        let tree = temp.path().join("tree");
        write_tree(&tree, 0, false);
        git(&tree, &["init", "-q"]);
        git(&tree, &["add", "-A"]);
        git(&tree, &["commit", "-q", "-m", "baseline"]);
        let workspace = dunce::canonicalize(&tree).unwrap();
        let scip = temp.path().join("scip/index.scip");
        // The tree is written before the SCIP file, never after: the product
        // takes the files not newer than the index file as the index's tree, so
        // a reordering would inflate its lag from the first call.
        let scip_length = write_scip(&workspace, &scip);
        let app = App::open(config).await.unwrap();
        app.index(&workspace).await.unwrap();
        let scip_tree = tree_hashes(&workspace);
        World {
            _temp: temp,
            workspace,
            scip,
            scip_length,
            app,
            fake,
            models_task,
            session_state,
            walk_config,
            generation: 0,
            shifted: false,
            last_fresh: BTreeMap::new(),
            labels,
            verify,
            sync,
            places: LspPlaces::Scip,
            probe_runs: std::cell::Cell::new(0),
            scip_tree,
        }
    }

    /// How many files of the tree differ from the tree the SCIP file was
    /// written for (changed, added or removed): what a label of the SCIP-built
    /// maps has to cover.
    fn files_behind(&self) -> u32 {
        let now = tree_hashes(&self.workspace);
        let changed = now
            .iter()
            .filter(|(path, text)| self.scip_tree.get(*path) != Some(*text))
            .count();
        let removed = self
            .scip_tree
            .keys()
            .filter(|path| !now.contains_key(*path))
            .count();
        u32::try_from(changed + removed).unwrap()
    }

    /// Generation `g + 1`: the probes are written to the tree. Nothing else
    /// moves: not the index, not the SCIP file, not the language server
    /// (unless the sync is on, which tells it).
    async fn edit(&mut self) {
        self.generation += 1;
        self.write_generation(self.shifted).await;
    }

    /// One blank line on top of `Calculator.cs`.
    async fn shift(&mut self) {
        self.shifted = true;
        self.write_generation(true).await;
    }

    /// The application's workspace id (its language-server sessions' key).
    fn workspace_id(&self) -> String {
        lci_core::workspace::Workspace::resolve(&self.workspace, &self.app.config.data_dir)
            .unwrap()
            .id
    }

    /// Writes the tree at the current generation. With the sync on, the
    /// session started by the first observation is told about it before this
    /// returns: a bounded wait on the watcher's own forwarding (the product's
    /// asynchronous window), never a fixed sleep. The first wait makes sure
    /// the sync's first walk is over, so an edit is a change of a known file.
    async fn write_generation(&self, shifted: bool) {
        let servers = self.app.language_servers();
        let id = self.workspace_id();
        // A session exists after the first observation (or a symbol call);
        // before it there is none to tell: it walks the disk when it starts.
        let told = self.sync && (self.probe_runs.get() > 0 || servers.sync_status(&id).is_some());
        if told {
            assert!(
                servers.synced(&id, &[], LOADED_BUDGET).await,
                "the sync of the session runs: {:?}",
                servers.sync_status(&id)
            );
        }
        write_tree(&self.workspace, self.generation, shifted);
        if told {
            assert!(
                servers.synced(&id, &SYNCED_SOURCES, LOADED_BUDGET).await,
                "the session was told about the edit: {:?}",
                servers.sync_status(&id)
            );
        }
    }

    /// The SCIP file as an indexer would write it for the tree now. Its length
    /// changes, so the stamp does.
    /// (The tree is already written: the SCIP file must be the newer of the
    /// two, see `start`.)
    fn regenerate_scip(&mut self) {
        let length = write_scip(&self.workspace, &self.scip);
        assert_ne!(length, self.scip_length, "the stamp must move");
        self.scip_length = length;
        self.scip_tree = tree_hashes(&self.workspace);
    }

    fn truth(&self) -> Truth {
        let probe = if self.generation == 0 {
            "Combine".to_owned()
        } else {
            format!("ProbeG{}", self.generation)
        };
        let chunks = lci_core::chunk::scan(&self.workspace).unwrap();
        let ids = lci_core::node::chunk_ids(&chunks);
        let held = chunks
            .iter()
            .zip(&ids)
            .filter(|(chunk, _)| chunk.code.contains(&probe))
            .map(|(_, id)| id.to_string())
            .collect();
        let mut lines = BTreeMap::new();
        for file in ["src/Calculator.cs", "src/CallSite.cs"] {
            let text = std::fs::read_to_string(self.workspace.join(file)).unwrap();
            for (line, name, _, declares, _) in scan_functions(&text) {
                if declares {
                    lines.insert(name, line as u32 + 1);
                }
            }
        }
        Truth {
            generation: self.generation,
            probe,
            chunks: held,
            lines,
        }
    }

    async fn run(&self, call: &Call<'_>) -> traverse::Report {
        let mut profile = self.app.config.run_profile.clone();
        profile.traverse.map_freshness = self.labels;
        profile.freshness.verify_on_read = self.verify;
        profile.lsp.places = self.places;
        let report = traverse::run(
            &self.app.config.traverse,
            &profile,
            &Dirs::under(&self.app.config.data_dir),
            call,
        )
        .await
        .unwrap();
        // A map that ran out of time answered nothing: not a finding.
        assert_no_timeout(&report.response);
        report
    }

    /// Asks everything once, in the order the product does: the hybrid search
    /// first (it refreshes a stale index under `on-search`), then the walk.
    async fn observe(&self) -> Observed {
        let truth = self.truth();
        let (stale_before_search, search) = (
            self.app.status(&self.workspace).await.unwrap().stale,
            self.app
                .search(&self.workspace, &truth.probe, Some(10))
                .await
                .unwrap(),
        );
        let stale_after_search = self.app.status(&self.workspace).await.unwrap().stale;
        let search = serde_json::to_value(&search).unwrap();
        let mut reached = Reached::new();
        for hit in search["results"].as_array().unwrap() {
            let code = hit["code"].as_str().unwrap().to_owned();
            for (channel, rank) in [
                ("vec.chunk.code", "semantic_rank"),
                ("lexical", "lexical_rank"),
            ] {
                if !hit[rank].is_null() {
                    reached
                        .entry(channel.to_owned())
                        .or_default()
                        .insert(code.clone());
                }
            }
        }
        // The App-wide language-server session, asked the way the symbol
        // search asks it (the channel's own `<term>#` queries match nothing
        // in the fake, which matches substrings of the name).
        let mut answered = BTreeSet::new();
        let symbols = self
            .app
            .symbols(&self.workspace, &truth.probe)
            .await
            .unwrap();
        // The probe is not in an old session, which is what staleness is; a
        // session that lists nothing for `Combine` either is broken.
        let baseline = self.app.symbols(&self.workspace, "Combine").await.unwrap();
        if !baseline.results.is_empty() {
            answered.insert("lsp.symbols");
        }
        for found in &symbols.results {
            reached
                .entry("lsp.symbols".to_owned())
                .or_default()
                .insert(found.name.clone().unwrap_or_default());
        }

        let seed_search = AppSeeds {
            app: &self.app,
            workspace: &self.workspace,
            profile: None,
        };
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let seeds = [
            COMBINE.to_owned(),
            RUN.to_owned(),
            RATE_KEY.to_owned(),
            SPEAK.to_owned(),
        ];
        let maps = WALK_MAPS.map(str::to_owned);
        let base = Call {
            scip_index: &self.scip,
            scip_root: Some(&self.workspace),
            question: "`Combine`",
            seeds: &seeds,
            seed_search: Some(&seed_search),
            top_k: 40,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: Some(&self.walk_config),
            lsp_sessions: None,
        };
        let mut walk = read_walk(&self.run(&base).await);
        let edges = walk.edges;
        for (map, nodes) in walk.reached {
            reached.entry(map).or_default().extend(nodes);
        }
        // The start nodes of the seed maps, each map alone.
        for seed_map in SEED_MAPS {
            let only = [seed_map.to_owned()];
            let call = asked(&base, &truth.probe, &[], Some(&only));
            let report = self.run(&call).await;
            // The label of a seed map is in the answer of the call that ran it.
            let label = report.response["maps"]
                .as_array()
                .and_then(|maps| maps.iter().find(|entry| entry["id"] == seed_map));
            if let (Some(entry), Some(maps)) = (
                label,
                walk.response.get_mut("maps").and_then(Value::as_array_mut),
            ) && !maps.iter().any(|known| known["id"] == seed_map)
            {
                maps.push(entry.clone());
            }
            // The same for a seed map: an old index does not know the probe,
            // but it knows `Combine`.
            let known = asked(&call, "`Combine`", &[], Some(&only));
            let baseline = self.run(&known).await;
            if baseline.response["seeds"]
                .as_array()
                .is_some_and(|seeds| !seeds.is_empty())
            {
                answered.insert(seed_map);
            }
            let nodes = report.response["seeds"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|seed| seed.as_str().unwrap().to_owned());
            reached
                .entry(seed_map.to_owned())
                .or_default()
                .extend(nodes);
        }
        // The live maps asked about the probe itself. A SCIP file written for
        // the tree as it is now gives the probe's symbol a place for `lsp.live`
        // to ask at; it is an input of this call, not the file under test.
        // Its own path per observation: a rewrite at one path inside one mtime
        // tick would be read from the product's cached parse.
        let run = self.probe_runs.get();
        self.probe_runs.set(run + 1);
        let probe_scip = self
            .scip
            .with_file_name(format!("probe-g{}-{run}.scip", truth.generation));
        write_scip(&self.workspace, &probe_scip);
        let mut probes = BTreeMap::new();
        for live_map in LIVE_MAPS {
            probes.insert(live_map, self.probe_edges(&probe_scip, live_map).await);
        }
        Observed {
            answered,
            probes,
            index_action: search["index"]["action"].as_str().unwrap().to_owned(),
            stale_before_search,
            stale_after_search,
            reached,
            edges,
            walk: walk.response,
            search,
        }
    }

    /// How many edges `map` offers from the probe's own symbol, asked alone
    /// about it with the SCIP file `scip_index`.
    async fn probe_edges(&self, scip_index: &Path, map: &str) -> usize {
        let truth = self.truth();
        let seed = if truth.generation == 0 {
            COMBINE.to_owned()
        } else {
            format!("sym:rust:Calculator.cs::ProbeG{}/fn", truth.generation)
        };
        let seed_search = AppSeeds {
            app: &self.app,
            workspace: &self.workspace,
            profile: None,
        };
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let only = [map.to_owned()];
        let call = Call {
            scip_index,
            scip_root: Some(&self.workspace),
            question: &truth.probe,
            seeds: std::slice::from_ref(&seed),
            seed_search: Some(&seed_search),
            top_k: 40,
            maps: Some(&only),
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: Some(&self.walk_config),
            lsp_sessions: None,
        };
        let found = read_walk(&self.run(&call).await);
        found
            .edges
            .get(map)
            .into_iter()
            .flatten()
            .filter(|(src, _)| *src == seed)
            .count()
    }

    /// Control for the probe signal of `lsp.live`: asked with the SCIP file
    /// written before the edit, which has no place for the probe's symbol, it
    /// offers nothing from it, so the edge count depends on the seed having a
    /// place and not on the call merely running.
    async fn assert_lsp_probe_needs_a_place(&self) {
        assert!(self.generation >= 1);
        assert_eq!(
            self.probe_edges(&self.scip, "lsp.live").await,
            0,
            "an index with no place for the probe gives lsp.live nothing to ask at"
        );
    }

    /// The commits `git.history` reaches from `Combine` and the commits
    /// `exact.pickaxe` finds for the probe's creation.
    async fn history(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let truth = self.truth();
        let seed_search = AppSeeds {
            app: &self.app,
            workspace: &self.workspace,
            profile: None,
        };
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let seeds = [COMBINE.to_owned()];
        let maps = ["git.history".to_owned(), "exact.pickaxe".to_owned()];
        let question = format!("why `{}`", truth.probe);
        let call = Call {
            scip_index: &self.scip,
            scip_root: Some(&self.workspace),
            question: &question,
            seeds: &seeds,
            seed_search: Some(&seed_search),
            top_k: 40,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: Some(&self.walk_config),
            lsp_sessions: None,
        };
        let walk = read_walk(&self.run(&call).await);
        let commits = |map: &str| -> BTreeSet<String> {
            walk.reached
                .get(map)
                .into_iter()
                .flatten()
                .filter(|node| node.starts_with("commit:"))
                .cloned()
                .collect()
        };
        (commits("git.history"), commits("exact.pickaxe"))
    }

    async fn finish(self) {
        self.app.shutdown_language_servers().await;
        self.models_task.abort();
        drop(self.fake);
    }
}

// ---- verdicts --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Case {
    OnSearch,
    Manual,
    LineShift,
    ScipRewritten,
}

/// What the silent answer is behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    /// A SCIP index older than the tree (the stamp has not moved).
    OldIndex,
    /// An index snapshot older than the tree.
    OldSnapshot,
    /// A language-server session that was never told.
    OldSession,
    /// The place of a symbol is the index's, not the tree's.
    SiteDrift,
    /// An edge the tree still has is gone because the places moved.
    LostEdge,
    /// A gap with no known data behind it.
    Unexplained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Fresh,
    /// Stale and says so in `maps[]`, with this lag in files (K2a-2).
    StaleLabelled {
        lag: u32,
    },
    /// Stale and silent.
    Gap {
        why: Why,
        lag: u32,
    },
    /// The map gave nothing before the edit either: nothing to compare.
    Unexercised,
}

/// The verdict for a map that did or did not see the edit. `label` is the lag
/// the answer reports for the map, if it reports a stale one; it counts when
/// it is at least `needed`, the number of files the map's data is behind the
/// tree by (the label counts files, `lag` is the generation distance). A map
/// that answered nothing (`answered` false) is not stale, it is broken: a gap
/// with no known data behind it, whatever its label says.
fn classify(
    sees: bool,
    answered: bool,
    label: Option<u32>,
    needed: u32,
    lag: u32,
    why: Why,
) -> Verdict {
    if sees {
        Verdict::Fresh
    } else if !answered {
        Verdict::Gap {
            why: Why::Unexplained,
            lag,
        }
    } else if let Some(reported) = label.filter(|reported| *reported >= needed) {
        Verdict::StaleLabelled { lag: reported }
    } else {
        Verdict::Gap { why, lag }
    }
}

/// The lag an entry list (`maps[]` of a traverse answer, `channels` of a
/// search report: `{id, observed, stale, lag}`) gives `map`, when its entry says
/// `stale` and gives a lag.
fn entry_lag(entries: &Value, map: &str) -> Option<u32> {
    entries
        .as_array()?
        .iter()
        .find(|entry| entry["id"] == map && entry["stale"] == true)
        .and_then(|entry| u32::try_from(entry["lag"].as_u64()?).ok())
}

/// The lag the answers label `map` with as stale: for the three search
/// channels the `freshness.channels` of the search report
/// (`[run_profile.freshness] verify_on_read`), for every other map its `maps[]`
/// entry in the traverse answer (`[run_profile.traverse] map_freshness`).
fn labelled_lag(now: &Observed, map: &str) -> Option<u32> {
    if CHANNELS.contains(&map) {
        entry_lag(&now.search["freshness"]["channels"], map)
    } else {
        entry_lag(&now.walk["maps"], map)
    }
}

/// The search channels the harness asks about.
const CHANNELS: [&str; 3] = ["vec.chunk.code", "lexical", "lsp.symbols"];

/// The `maps[]` entry of `map` in a traverse answer.
fn map_entry<'a>(answer: &'a Value, map: &str) -> &'a Value {
    answer["maps"]
        .as_array()
        .and_then(|maps| maps.iter().find(|entry| entry["id"] == map))
        .unwrap_or_else(|| panic!("no maps[] entry for {map} in {answer}"))
}

/// The `freshness.channels` entry of `channel` in a search report.
fn channel_entry<'a>(search: &'a Value, channel: &str) -> &'a Value {
    search["freshness"]["channels"]
        .as_array()
        .and_then(|channels| channels.iter().find(|entry| entry["id"] == channel))
        .unwrap_or_else(|| panic!("no channel {channel} in {}", search["freshness"]))
}

/// No hit of the search report says its file changed (the snapshot was
/// refreshed by the search): `false`, or `null` for a hit the snapshot did not
/// place.
fn assert_hits_unchanged(search: &Value) {
    let hits = search["freshness"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), search["results"].as_array().unwrap().len());
    assert!(!hits.is_empty());
    for hit in hits {
        assert_ne!(hit["file_changed"], true, "{hit}");
    }
}

/// The cards of a list whose `site.path` is `path`.
fn cards_at<'a>(cards: &'a Value, path: &str) -> Vec<&'a Value> {
    cards
        .as_array()
        .into_iter()
        .flatten()
        .filter(|card| card["site"]["path"] == path)
        .collect()
}

/// The maps whose cards a comparison with the SCIP index's tree answers for:
/// the maps built from the index, and the live maps that place by it.
const INDEX_MAPS: [&str; 9] = [
    "graph.scip",
    "structure.contains",
    "seeds.scip_names",
    "exact.closure",
    "config.key",
    "scip.impact",
    "scip.implements",
    "grep.live",
    "lsp.live",
];

/// With `verify_on_read`, every result card has `file_changed`, and a card
/// that a reference applies to (its maps are built from the index or placed
/// by it, or, unless `index_maps_only`, read the snapshot) says `true` for the
/// source files in `changed` and `false` for the other source files; a file
/// that is not a source file (the TOML example) is `null`.
fn assert_file_changed(answer: &Value, changed: &[&str], index_maps_only: bool) {
    let mut checked = 0;
    for card in answer["results"].as_array().expect("results") {
        assert!(card.get("file_changed").is_some(), "{card}");
        let Some(path) = card["site"]["path"].as_str() else {
            continue;
        };
        let named = |maps: &[&str]| {
            let listed = card["maps"].as_array().into_iter().flatten();
            listed
                .chain(card.get("via"))
                .filter_map(Value::as_str)
                .any(|map| maps.contains(&map))
        };
        let applies = named(&INDEX_MAPS)
            || (!index_maps_only && named(&["seeds.hybrid", "search.lexical", "search.vector"]));
        if !applies {
            continue;
        }
        let want = if path.ends_with(".toml") {
            Value::Null
        } else {
            json!(changed.contains(&path))
        };
        assert_eq!(card["file_changed"], want, "{card}");
        checked += 1;
    }
    assert!(checked > 0, "no card to check in {answer}");
}

/// The data a silent gap of this map is behind.
fn why_of(map: &str) -> Why {
    match map {
        "graph.scip" | "structure.contains" | "seeds.scip_names" | "scip.impact"
        | "scip.implements" | "config.key" | "exact.closure" => Why::OldIndex,
        "seeds.hybrid" | "search.lexical" | "search.vector" | "vec.chunk.code" | "lexical" => {
            Why::OldSnapshot
        }
        "lsp.symbols" => Why::OldSession,
        _ => Why::Unexplained,
    }
}

impl World {
    /// Whether `map` saw the edit of generation `now.generation`, for every
    /// map of the scope, with the lag bookkeeping.
    fn verdicts(
        &mut self,
        before: &Observed,
        now: &Observed,
        truth: &Truth,
    ) -> BTreeMap<&'static str, Verdict> {
        let mut out = BTreeMap::new();
        let needed = self.files_behind();
        for map in SCOPE {
            let empty = BTreeSet::new();
            let was = before.reached.get(map).unwrap_or(&empty);
            let is = now.reached.get(map).unwrap_or(&empty);
            // A live map saw the edit when it offers an edge from the probe's
            // own symbol, which names nothing in an older tree; the others
            // when they reached a node that names the probe or the chunk that
            // holds it.
            let sees = match now.probes.get(map) {
                Some(from_probe) => *from_probe > 0,
                None => is
                    .iter()
                    .any(|node| node.contains(&truth.probe) || truth.chunks.contains(node)),
            };
            let verdict = if was.is_empty() && !before.answered.contains(map) {
                Verdict::Unexercised
            } else {
                let lag = truth.generation - self.last_fresh.get(map).copied().unwrap_or(0);
                classify(
                    sees,
                    !is.is_empty() || now.answered.contains(map),
                    labelled_lag(now, map),
                    needed,
                    lag,
                    why_of(map),
                )
            };
            if verdict == Verdict::Fresh {
                self.last_fresh.insert(map, truth.generation);
            }
            out.insert(map, verdict);
        }
        out
    }
}

/// The gaps with `[run_profile.traverse] map_freshness`, `[run_profile.freshness]
/// verify_on_read` and `[run_profile.lsp] sync` on: stale and silent. A map
/// that becomes fresh (K14d) or that gets a label that covers its lag must be
/// struck from this list in the same commit, and a new gap fails the case
/// that finds it.
const EXPECTED_GAPS: &[(Case, &str, Why)] = &[];

/// The maps that miss the edit only with `[run_profile.lsp] sync` off (K8a).
/// None any more (N104): the language-server session of `lsp.symbols` walked
/// the tree before the edit and, with the sync off, is told nothing after it,
/// but navigation and the search channel pass the tree barrier, which stops a
/// session whose tree changed so the request starts one that reads the disk.
/// The sync no longer decides whether `lsp.symbols` sees the edit. The list
/// and its use stay: a map that is behind only without the sync is listed here
/// again.
const SYNCED: &[(Case, &str, Why)] = &[];

/// The maps that miss the edit and say so only with `verify_on_read` on (K2b):
/// with it off they are gaps (the same data, no label).
const VERIFIED_ON_READ: &[(Case, &str, Why)] = &[
    // Under `manual` the snapshot is old: the lag is the manifest's against
    // the tree.
    (Case::Manual, "seeds.hybrid", Why::OldSnapshot),
    (Case::Manual, "search.lexical", Why::OldSnapshot),
    (Case::Manual, "search.vector", Why::OldSnapshot),
    // The channels of the search report say it too (`freshness.channels`).
    (Case::Manual, "vec.chunk.code", Why::OldSnapshot),
    (Case::Manual, "lexical", Why::OldSnapshot),
    // One blank line moved: the two live maps ask or attribute at the index's
    // places; their label carries the index's lag.
    (Case::LineShift, "grep.live", Why::LostEdge),
    (Case::LineShift, "lsp.live", Why::LostEdge),
];

/// The entries of [`VERIFIED_ON_READ`] that `[run_profile.lsp] places =
/// "outline"` (K10a) changes, with what they become: `lsp.live` asks the server
/// at the places of the server's own outline of the file, not at the index's, so
/// one blank line on top of a file loses no edge (no `LostEdge`). Measured: the
/// nodes it reaches are also reached through the index, whose card sites keep
/// the index's lines, so the map is still stale and labelled, now as
/// `SiteDrift` (with the other keys as the entry was: a gap while
/// `verify_on_read` is off).
const PLACED_BY_OUTLINE: &[(Case, &str, Why)] = &[(Case::LineShift, "lsp.live", Why::SiteDrift)];

/// The maps that miss the edit but say so in `maps[]` with a lag that covers
/// the files they are behind by (with the key off they are gaps: the same
/// data, no label).
const EXPECTED_LABELLED: &[(Case, &str, Why)] = &[
    // The SCIP file is the one written at generation 0.
    (Case::OnSearch, "seeds.scip_names", Why::OldIndex),
    (Case::OnSearch, "graph.scip", Why::OldIndex),
    (Case::OnSearch, "structure.contains", Why::OldIndex),
    (Case::OnSearch, "config.key", Why::OldIndex),
    (Case::OnSearch, "scip.impact", Why::OldIndex),
    (Case::OnSearch, "scip.implements", Why::OldIndex),
    (Case::OnSearch, "exact.closure", Why::OldIndex),
    (Case::Manual, "seeds.scip_names", Why::OldIndex),
    (Case::Manual, "graph.scip", Why::OldIndex),
    (Case::Manual, "structure.contains", Why::OldIndex),
    (Case::Manual, "config.key", Why::OldIndex),
    (Case::Manual, "scip.impact", Why::OldIndex),
    (Case::Manual, "scip.implements", Why::OldIndex),
    (Case::Manual, "exact.closure", Why::OldIndex),
    // One blank line moved: the card sites are the index's. `Why` is what the
    // map is without the label (a gap).
    (Case::LineShift, "graph.scip", Why::SiteDrift),
    (Case::LineShift, "structure.contains", Why::SiteDrift),
    (Case::LineShift, "scip.impact", Why::SiteDrift),
    (Case::LineShift, "exact.closure", Why::SiteDrift),
];

type Listed = Vec<(Case, &'static str, Why)>;

/// The gaps and the labelled maps a case must find. Every key on: as the lists
/// say. `verify_on_read` off: the verified maps are gaps. `map_freshness` off:
/// every labelled map is a gap (`verify_on_read` does nothing without it in a
/// traverse answer). `sync` off: the session's maps are gaps. `places = outline`:
/// the entries of [`PLACED_BY_OUTLINE`] have the reason it names.
fn expected(labels: bool, verify: bool, sync: bool, places: LspPlaces) -> (Listed, Listed) {
    let unsynced = SYNCED.iter().copied().filter(|_| !sync);
    let gaps = EXPECTED_GAPS.iter().copied().chain(unsynced);
    let labelled = EXPECTED_LABELLED.iter().copied();
    let verified = VERIFIED_ON_READ.iter().copied().map(move |entry| {
        let placed = PLACED_BY_OUTLINE
            .iter()
            .find(|(case, map, _)| (*case, *map) == (entry.0, entry.1));
        match placed {
            Some((case, map, why)) if places == LspPlaces::Outline => (*case, *map, *why),
            _ => entry,
        }
    });
    match (labels, verify) {
        (true, true) => (gaps.collect(), labelled.chain(verified).collect()),
        (true, false) => (gaps.chain(verified).collect(), labelled.collect()),
        (false, _) => (gaps.chain(verified).chain(labelled).collect(), Vec::new()),
    }
}

/// The gaps of `found` (a case's verdicts) against the list, both ways.
fn compare_gaps<'a>(
    expected: &[(Case, &'a str, Why)],
    labelled: &[(Case, &'a str, Why)],
    case: Case,
    found: &BTreeMap<&'a str, Verdict>,
) -> Result<(), String> {
    let of_case = |list: &[(Case, &'a str, Why)]| -> BTreeSet<(&'a str, Why)> {
        list.iter()
            .filter(|(c, _, _)| *c == case)
            .map(|(_, map, why)| (*map, *why))
            .collect()
    };
    let wanted = of_case(expected);
    let wanted_labelled: BTreeSet<&str> = of_case(labelled).iter().map(|(map, _)| *map).collect();
    let seen: BTreeSet<(&str, Why)> = found
        .iter()
        .filter_map(|(map, verdict)| match verdict {
            Verdict::Gap { why, .. } => Some((*map, *why)),
            _ => None,
        })
        .collect();
    let seen_labelled: BTreeSet<&str> = found
        .iter()
        .filter_map(|(map, verdict)| match verdict {
            Verdict::StaleLabelled { .. } => Some(*map),
            _ => None,
        })
        .collect();
    let mut problems = Vec::new();
    for entry in seen.difference(&wanted) {
        problems.push(format!("new gap, not on the list: {entry:?}"));
    }
    for entry in wanted.difference(&seen) {
        problems.push(format!("listed gap that is not one now: {entry:?}"));
    }
    for entry in seen_labelled.difference(&wanted_labelled) {
        problems.push(format!("labelled, not on the list: {entry:?}"));
    }
    for entry in wanted_labelled.difference(&seen_labelled) {
        problems.push(format!("listed as labelled, not one now: {entry:?}"));
    }
    for (map, verdict) in found {
        if *verdict == Verdict::Unexercised {
            problems.push(format!("{map} gave nothing before the edit"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("{case:?}: {}", problems.join("; ")))
    }
}

fn table(case: Case, verdicts: &BTreeMap<&str, Verdict>) -> String {
    let mut text = format!("{case:?}\n");
    for map in SCOPE {
        text.push_str(&format!("  {map:20} {:?}\n", verdicts[map]));
    }
    text
}

// ---- the line shift ----------------------------------------------------------

/// The maps whose answers carry places: a symbol's site is the SCIP index's
/// line, `lsp.live` asks the server at the index's places, and `grep.live`
/// attributes a line to the symbol whose index span holds it.
const PLACED_MAPS: [&str; 6] = [
    "graph.scip",
    "structure.contains",
    "grep.live",
    "scip.impact",
    "lsp.live",
    "exact.closure",
];

fn symbol_name(node: &str) -> Option<&str> {
    node.strip_prefix("sym:rust:")?
        .rsplit("::")
        .next()?
        .strip_suffix("/fn")
}

/// After a shift of `Calculator.cs`, with the index and the snapshot
/// consistent before it and the SCIP file not rewritten: a map is a gap when
/// an edge between two symbols that it offered before is gone (the places it
/// asked at moved), or when a card it reaches carries a line that is not
/// where the tree declares the symbol now.
fn shift_verdicts(
    before: &Observed,
    after: &Observed,
    truth: &Truth,
    needed: u32,
) -> BTreeMap<&'static str, Verdict> {
    let symbols = |edges: Option<&BTreeSet<(String, String)>>| -> BTreeSet<(String, String)> {
        edges
            .into_iter()
            .flatten()
            .filter(|(src, dst)| src.starts_with("sym:") && dst.starts_with("sym:"))
            .cloned()
            .collect()
    };
    let mut out = BTreeMap::new();
    // The line each card gives its node (the index's, for a symbol).
    let lines: BTreeMap<String, u64> = after.walk["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|card| {
            Some((
                card["node"].as_str()?.to_owned(),
                card["site"]["line"].as_u64()?,
            ))
        })
        .collect();
    for map in PLACED_MAPS {
        let was = symbols(before.edges.get(map));
        let is = symbols(after.edges.get(map));
        let exercised = !was.is_empty()
            || before
                .reached
                .get(map)
                .is_some_and(|nodes| !nodes.is_empty());
        let lost = was.difference(&is).count();
        let drifted = after.reached.get(map).into_iter().flatten().any(|node| {
            let declared = symbol_name(node).and_then(|name| truth.lines.get(name));
            matches!((declared, lines.get(node)), (Some(declared), Some(line)) if u64::from(*declared) != *line)
        });
        let answered = after
            .reached
            .get(map)
            .is_some_and(|nodes| !nodes.is_empty());
        let verdict = if !exercised {
            Verdict::Unexercised
        } else if lost > 0 {
            classify(
                false,
                answered,
                labelled_lag(after, map),
                needed,
                1,
                Why::LostEdge,
            )
        } else if drifted {
            classify(
                false,
                answered,
                labelled_lag(after, map),
                needed,
                1,
                Why::SiteDrift,
            )
        } else {
            Verdict::Fresh
        };
        out.insert(map, verdict);
    }
    out
}

fn shift_table(verdicts: &BTreeMap<&str, Verdict>) -> String {
    let mut text = String::from("LineShift\n");
    for map in PLACED_MAPS {
        text.push_str(&format!("  {map:20} {:?}\n", verdicts[map]));
    }
    text
}

fn problems_of(
    (labels, verify, sync, places): (bool, bool, bool, LspPlaces),
    case: Case,
    found: &BTreeMap<&str, Verdict>,
) -> Result<(), String> {
    let (gaps, labelled) = expected(labels, verify, sync, places);
    compare_gaps(&gaps, &labelled, case, found)
}

impl World {
    /// The lists of this world's keys against what a case found.
    fn problems(&self, case: Case, found: &BTreeMap<&str, Verdict>) -> Result<(), String> {
        problems_of(
            (self.labels, self.verify, self.sync, self.places),
            case,
            found,
        )
    }

    /// With the sync on, the session's own record shows it was told: a
    /// `workspace/didChangeWatchedFiles` that names `src/Calculator.cs` as
    /// changed (type 2) and that the fake applied (no note). Read after an
    /// observation, whose requests follow the notification. The tree barrier
    /// (N104) restarts the session at the first request after each edit and
    /// keeps it while the tree is unchanged, so the servers started are one
    /// per generation plus the first: the edit told the session, then the
    /// barrier replaced it.
    fn assert_session_told(&self) {
        assert!(self.sync);
        let events = events_of(&self.session_state);
        self.assert_restarted_once_per_edit(&events);
        let told: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "workspace/didChangeWatchedFiles")
            .collect();
        assert!(!told.is_empty(), "no didChangeWatchedFiles");
        for event in &told {
            assert!(event.get("note").is_none(), "{event}");
        }
        assert!(
            told.iter().any(|event| {
                event["changes"].as_array().is_some_and(|changes| {
                    changes.iter().any(|change| {
                        change["type"] == 2
                            && change["uri"]
                                .as_str()
                                .is_some_and(|uri| uri.ends_with("src/Calculator.cs"))
                    })
                })
            }),
            "{told:?}"
        );
    }
}

impl World {
    /// The language-server sessions of the application's manager are numbered
    /// 1 to `generation + 1`: a restart by the tree barrier after each edit
    /// and no other start (the unchanged tree after a rewritten SCIP file,
    /// which lives outside the tree, keeps the session).
    fn assert_restarted_once_per_edit(&self, events: &[Value]) {
        let spawns: BTreeSet<u64> = events
            .iter()
            .map(|event| event["spawn"].as_u64().unwrap())
            .collect();
        let wanted: BTreeSet<u64> = (1..=u64::from(self.generation) + 1).collect();
        assert_eq!(spawns, wanted, "a restart after each edit, no other start");
    }
}

// ---- the cases ---------------------------------------------------------------

/// The index and the snapshot follow an edit (`on-search`), and so does the
/// language-server session, which the sync tells; every SCIP-built map does
/// not. Two generations show the lag growing. (The two git maps have a test of
/// their own below: they read commits, not the index, so it runs beside this
/// one.)
#[tokio::test]
async fn on_search_every_map_sees_an_appended_symbol_or_is_on_the_gap_list() {
    let mut world = World::start(IndexFreshness::OnSearch).await;
    let mut before = world.observe().await;
    assert_eq!(before.index_action, "reused");
    assert!(!before.stale_before_search);

    for generation in 1..=2u32 {
        world.edit().await;
        let truth = world.truth();
        assert_eq!(truth.generation, generation);
        assert!(
            !truth.chunks.is_empty(),
            "the tree holds the probe in some chunk"
        );
        let now = world.observe().await;
        if generation == 1 {
            world.assert_lsp_probe_needs_a_place().await;
        }
        // The snapshot is stale, the search refreshes it, the walk sees the
        // refreshed one.
        assert!(now.stale_before_search, "the edit makes the snapshot stale");
        assert_eq!(now.index_action, "refreshed_incrementally");
        assert!(!now.stale_after_search);
        let verdicts = world.verdicts(&before, &now, &truth);
        let shown = table(Case::OnSearch, &verdicts);
        eprintln!("generation {generation}\n{shown}");
        world
            .problems(Case::OnSearch, &verdicts)
            .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
        // The lag is the number of generations since the map was fresh.
        for (map, verdict) in &verdicts {
            match verdict {
                Verdict::Gap { lag, .. } => assert_eq!(*lag, generation, "{map}"),
                // The label counts files, each once however often it is
                // edited: the three the probes are written to.
                Verdict::StaleLabelled { lag } => {
                    assert_eq!(*lag, world.files_behind(), "{map}");
                    assert_eq!(world.files_behind(), 3, "{map}");
                }
                Verdict::Fresh => assert_eq!(world.last_fresh[map], generation, "{map}"),
                other => panic!("{map}: {other:?}"),
            }
        }
        // The search refreshed the snapshot: the maps that read it are not
        // behind the tree, whatever the SCIP index is.
        for map in ["seeds.hybrid", "search.lexical", "search.vector"] {
            let entry = map_entry(&now.walk, map);
            assert_eq!(entry["observed"], "snapshot", "{entry}");
            assert_eq!(entry["stale"], false, "{entry}");
            assert_eq!(entry["lag"], 0, "{entry}");
        }
        // The search report says the same of its channels and its snapshot.
        let freshness = &now.search["freshness"];
        assert_eq!(freshness["snapshot"], json!({"stale": false, "lag": 0}));
        for map in ["vec.chunk.code", "lexical"] {
            let entry = channel_entry(&now.search, map);
            assert_eq!(entry["observed"], "snapshot", "{entry}");
            assert_eq!(entry["stale"], false, "{entry}");
            assert_eq!(entry["lag"], 0, "{entry}");
        }
        assert_hits_unchanged(&now.search);
        // The SCIP-built maps' cards cite files that changed.
        assert_file_changed(
            &now.walk,
            &["src/Calculator.cs", "src/CallSite.cs", "src/shapes.rs"],
            true,
        );
        world.assert_session_told();
        // The next generation is compared with this one.
        before = now;
    }
    world.finish().await;
}

/// The two git maps only read commits: an uncommitted edit is not history, in
/// either generation, and a commit is.
#[tokio::test]
async fn on_search_the_git_maps_see_a_commit_and_not_an_uncommitted_edit() {
    let mut world = World::start(IndexFreshness::OnSearch).await;
    let (history_before, pickaxe_before) = world.history().await;
    assert!(
        !history_before.is_empty(),
        "git.history reaches the baseline commit"
    );
    assert!(
        !pickaxe_before.is_empty(),
        "exact.pickaxe finds the baseline commit of Combine"
    );

    for generation in 1..=2u32 {
        world.edit().await;
        assert_eq!(world.truth().generation, generation);
        // The edit is not history yet.
        let (history, pickaxe) = world.history().await;
        assert_eq!(
            history, history_before,
            "an uncommitted edit is not history"
        );
        assert!(pickaxe.is_empty(), "{pickaxe:?}");
    }

    git(&world.workspace, &["add", "-A"]);
    git(&world.workspace, &["commit", "-q", "-m", "add the probes"]);
    let (history, pickaxe) = world.history().await;
    assert!(
        history.is_superset(&history_before) && history.len() > history_before.len(),
        "git.history reaches the new commit: {history:?}"
    );
    assert!(
        !pickaxe.is_empty(),
        "exact.pickaxe finds the commit that created the probe"
    );
    world.finish().await;
}

/// Under `manual` the index is never refreshed by a search: the search report
/// says `reused`, and every map that reads the snapshot answers from the old
/// one, besides the maps that were already gaps.
#[tokio::test]
async fn manual_mode_index_maps_answer_the_old_snapshot_and_the_search_report_says_reused() {
    let mut world = World::start(IndexFreshness::Manual).await;
    let before = world.observe().await;
    world.edit().await;
    let truth = world.truth();
    let now = world.observe().await;
    assert!(now.stale_before_search);
    assert_eq!(now.index_action, "reused");
    assert!(now.stale_after_search, "nothing refreshed the snapshot");
    let verdicts = world.verdicts(&before, &now, &truth);
    let shown = table(Case::Manual, &verdicts);
    eprintln!("{shown}");
    world
        .problems(Case::Manual, &verdicts)
        .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    world.assert_session_told();
    // The live maps still see the edit.
    for map in ["grep.live", "lsp.live"] {
        assert_eq!(verdicts[map], Verdict::Fresh, "{map}");
    }
    // With `verify_on_read` the maps that read the snapshot say how far it is
    // behind the tree: the three source files the probes were written to, as
    // the SCIP-built maps count them. The live maps place by the index, which
    // is as far behind, and say so.
    let behind = u64::from(world.files_behind());
    assert_eq!(behind, 3);
    for map in ["seeds.hybrid", "search.lexical", "search.vector"] {
        let entry = map_entry(&now.walk, map);
        assert_eq!(entry["observed"], "snapshot", "{entry}");
        assert_eq!(entry["stale"], true, "{entry}");
        assert_eq!(entry["lag"], behind, "{entry}");
        assert!(entry.get("places").is_none(), "{entry}");
    }
    for map in LIVE_MAPS {
        let entry = map_entry(&now.walk, map);
        assert_eq!(entry["observed"], "live", "{entry}");
        assert_eq!(entry["stale"], true, "{entry}");
        assert_eq!(entry["lag"], behind, "{entry}");
        assert_eq!(entry["places"], "index", "{entry}");
    }
    // Every card says whether its file changed: the three edited sources
    // did, the rest of the tree did not (the config example is not a source
    // file: nothing to compare).
    assert_file_changed(
        &now.walk,
        &["src/Calculator.cs", "src/CallSite.cs", "src/shapes.rs"],
        false,
    );
    // The search report: the snapshot is the three files behind, as its
    // channels are; a hit in an edited file says so, and the session's answer
    // is not compared.
    let freshness = &now.search["freshness"];
    assert_eq!(
        freshness["snapshot"],
        json!({"stale": true, "lag": behind}),
        "{freshness}"
    );
    for map in ["vec.chunk.code", "lexical"] {
        let entry = channel_entry(&now.search, map);
        assert_eq!(entry["observed"], "snapshot", "{entry}");
        assert_eq!(entry["stale"], true, "{entry}");
        assert_eq!(entry["lag"], behind, "{entry}");
    }
    let session = channel_entry(&now.search, "lsp.symbols");
    assert_eq!(session["observed"], "session", "{session}");
    assert!(
        session["stale"].is_null() && session["lag"].is_null(),
        "{session}"
    );
    assert!(session["note"].as_str().unwrap().contains("not compared"));
    let hits = freshness["hits"].as_array().unwrap();
    assert_eq!(hits.len(), now.search["results"].as_array().unwrap().len());
    let in_calculator: Vec<_> = hits
        .iter()
        .filter(|hit| hit["path"] == "src/Calculator.cs")
        .collect();
    assert!(!in_calculator.is_empty(), "{freshness}");
    assert!(in_calculator.iter().all(|hit| hit["file_changed"] == true));
    world.finish().await;
}

/// The walk reads the tree once, for its own labels. The search that finds its
/// start nodes discards the `freshness` block, so it must not scan the tree
/// again; a search of its own (the control) does.
#[tokio::test]
async fn the_search_that_seeds_a_walk_does_not_scan_the_tree_again() {
    // The counter counts on the thread that polls the search: on another
    // runtime flavour (or a spawned seed map) this test would pass without
    // checking anything.
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::CurrentThread
    );
    let world = World::start(IndexFreshness::OnSearch).await;
    let seed_search = AppSeeds {
        app: &world.app,
        workspace: &world.workspace,
        profile: None,
    };
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let maps = ["seeds.hybrid".to_owned()];
    let call = Call {
        scip_index: &world.scip,
        scip_root: Some(&world.workspace),
        question: "`Combine`",
        seeds: &[],
        seed_search: Some(&seed_search),
        top_k: 40,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: Some(&world.walk_config),
        lsp_sessions: None,
    };
    let before = verify_scans_on_this_thread();
    let report = world.run(&call).await;
    assert!(
        report.response["seeds"]
            .as_array()
            .is_some_and(|seeds| !seeds.is_empty()),
        "the seed search found nothing: {}",
        report.response
    );
    assert_eq!(
        verify_scans_on_this_thread(),
        before,
        "the seed search scanned the tree"
    );
    // The control: a search of its own does scan, and says so in its timings.
    let search = world
        .app
        .search(&world.workspace, "Combine", Some(5))
        .await
        .unwrap();
    assert_eq!(verify_scans_on_this_thread(), before + 1);
    assert!(search.timings.verify_ms > 0.0);
    world.finish().await;
}

/// One blank line on top of `Calculator.cs`, with the snapshot refreshed and
/// the SCIP file not rewritten: the places the maps use are the index's.
#[tokio::test]
async fn a_line_shift_moves_live_sites_and_the_gap_list_names_them() {
    let mut world = World::start(IndexFreshness::OnSearch).await;
    world.edit().await;
    world.regenerate_scip();
    let before = world.observe().await;
    world.shift().await;
    let truth = world.truth();
    let after = world.observe().await;
    assert_eq!(after.index_action, "refreshed_incrementally");
    // The tree declares `Combine` one line lower than the index does.
    assert_eq!(truth.lines["Combine"], 5);
    let verdicts = shift_verdicts(&before, &after, &truth, world.files_behind());
    let shown = shift_table(&verdicts);
    eprintln!("{shown}");
    world
        .problems(Case::LineShift, &verdicts)
        .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    // The cards the maps built from the index and placed by it cite: the file
    // that moved changed, the file that did not move did not.
    assert_eq!(world.files_behind(), 1);
    for map in LIVE_MAPS {
        let entry = map_entry(&after.walk, map);
        assert_eq!(entry["stale"], true, "{entry}");
        assert_eq!(entry["lag"], 1, "{entry}");
        assert_eq!(entry["places"], "index", "{entry}");
    }
    assert_file_changed(&after.walk, &["src/Calculator.cs"], true);
    let at = |path: &str| cards_at(&after.walk["results"], path);
    assert!(!at("src/Calculator.cs").is_empty(), "{}", after.walk);
    assert!(!at("src/CallSite.cs").is_empty(), "{}", after.walk);
    for card in at("src/Calculator.cs") {
        assert_eq!(card["file_changed"], true, "{card}");
    }
    for card in at("src/CallSite.cs") {
        assert_eq!(card["file_changed"], false, "{card}");
    }
    world.finish().await;
}

/// The same shift with `[run_profile.lsp] places = "outline"` (K10a): the walk
/// outlines `Calculator.cs` with the server before it asks, so `lsp.live` asks
/// at the places of the tree as it is, not at the index's, and loses no edge:
/// it is no longer the `LostEdge` gap of the default. It is still labelled
/// stale, as `SiteDrift`: the nodes it reaches are also reached through the
/// index, whose card sites are the index's lines (`PLACED_BY_OUTLINE`).
/// Without the slice the walk asks at the index's line and the edge is lost.
#[tokio::test]
async fn a_line_shift_loses_no_lsp_live_edge_under_places_outline() {
    let mut world = World::start(IndexFreshness::OnSearch).await;
    world.places = LspPlaces::Outline;
    world.edit().await;
    world.regenerate_scip();
    let before = world.observe().await;
    world.shift().await;
    let truth = world.truth();
    let after = world.observe().await;
    assert_eq!(truth.lines["Combine"], 5);
    let verdicts = shift_verdicts(&before, &after, &truth, world.files_behind());
    let shown = shift_table(&verdicts);
    eprintln!("{shown}");
    world
        .problems(Case::LineShift, &verdicts)
        .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    assert_eq!(
        verdicts["lsp.live"],
        Verdict::StaleLabelled { lag: 1 },
        "{shown}"
    );
    // Every symbol-to-symbol edge `lsp.live` offered before the shift is still
    // there, and the walk paid for the outline it asked at.
    let symbols = |observed: &Observed| -> BTreeSet<(String, String)> {
        observed
            .edges
            .get("lsp.live")
            .into_iter()
            .flatten()
            .filter(|(src, dst)| src.starts_with("sym:") && dst.starts_with("sym:"))
            .cloned()
            .collect()
    };
    assert!(
        !symbols(&before).is_empty(),
        "{:?}",
        before.edges.get("lsp.live")
    );
    assert!(symbols(&before).is_subset(&symbols(&after)));
    let walk_state = world._temp.path().join("lsp-walk");
    assert!(
        events_of(&walk_state)
            .iter()
            .any(|event| event["method"] == "textDocument/documentSymbol"),
        "the walk asked the server for an outline"
    );
    world.finish().await;
}

/// A rewritten SCIP file (a longer one: the stamp is `(mtime, length)`)
/// refreshes every map built from it, and only those (the language-server
/// session is fresh by the sync, which this world has on). The next edit
/// makes them lag again by one. The snapshot is refreshed by a search before
/// the rewrite, so the stamp reload is tested on its own, not as a side effect
/// of an index refresh (the stale verdicts of that search are the first
/// generation of the `on_search` case).
#[tokio::test]
async fn a_rewritten_scip_index_refreshes_the_scip_built_maps_by_its_stamp() {
    let mut world = World::start(IndexFreshness::OnSearch).await;
    let before = world.observe().await;
    world.edit().await;
    let truth = world.truth();
    let stale = world.observe().await;
    assert_eq!(stale.index_action, "refreshed_incrementally");

    world.regenerate_scip();
    let rewritten = world.observe().await;
    // The snapshot is already current: only the stamp can reload the maps.
    assert_eq!(rewritten.index_action, "reused");
    let verdicts = world.verdicts(&before, &rewritten, &truth);
    let shown = table(Case::ScipRewritten, &verdicts);
    eprintln!("{shown}");
    world
        .problems(Case::ScipRewritten, &verdicts)
        .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    for map in SCOPE.iter().filter(|map| why_of(map) == Why::OldIndex) {
        assert_eq!(verdicts[map], Verdict::Fresh, "{map} reloads by the stamp");
    }
    assert_eq!(
        verdicts["lsp.symbols"],
        Verdict::Fresh,
        "the barrier restarted the session after the edit"
    );
    world.assert_session_told();

    // The next edit: the SCIP-built maps are one generation behind.
    world.edit().await;
    let truth = world.truth();
    let again = world.observe().await;
    let verdicts = world.verdicts(&before, &again, &truth);
    let shown = table(Case::OnSearch, &verdicts);
    for map in SCOPE.iter().filter(|map| why_of(map) == Why::OldIndex) {
        let behind = if world.labels {
            Verdict::StaleLabelled {
                lag: world.files_behind(),
            }
        } else {
            Verdict::Gap {
                why: Why::OldIndex,
                lag: 1,
            }
        };
        assert_eq!(verdicts[map], behind, "{map}\n{shown}");
    }
    world.finish().await;
}

/// The number of lines the fake's store holds for `Calculator.cs` (its
/// `documents.json` keeps a line count and a hash per document, no text).
fn store_lines(state: &Path) -> u64 {
    let text = std::fs::read_to_string(state.join("documents.json")).unwrap();
    let store: Value = serde_json::from_str(&text).unwrap();
    store
        .as_object()
        .unwrap()
        .iter()
        .find(|(uri, _)| uri.ends_with("src/Calculator.cs"))
        .map(|(_, document)| document["lines"].as_u64().unwrap())
        .expect("Calculator.cs is in the store")
}

fn events_of(state: &Path) -> Vec<Value> {
    std::fs::read_to_string(state.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// With `[run_profile.lsp] sync` off (the default), the application's
/// language-server session walks the tree when it starts and is told nothing
/// after that, so an edit on disk is not pushed to it. Navigation passes the
/// tree barrier (N104): the first call after the edit stops that session, the
/// new one walks the disk and lists the edit (with a `restarted` note), and a
/// call on the unchanged tree keeps it. The fake answers from its store (K7a):
/// the store of the first server stays as it walked, the second one's holds
/// the edit.
#[tokio::test]
async fn with_sync_off_the_barrier_restarts_the_session_and_the_new_one_walks_the_disk() {
    let mut world = World::start_with(IndexFreshness::OnSearch, true, true, false).await;
    // The names the session lists in `Calculator.cs` (the fake walks the
    // sidecar's Rust file too, and answers from it).
    let names = |results: &[local_code_intelligence::lsp::Location]| -> Vec<String> {
        results
            .iter()
            .filter(|found| found.relative_file_path.as_deref() == Some("src/Calculator.cs"))
            .map(|found| found.name.clone().unwrap_or_default())
            .collect()
    };
    let first = world
        .app
        .symbols(&world.workspace, "Combine")
        .await
        .unwrap();
    assert_eq!(names(&first.results), ["Combine"]);
    assert!(first.notes.is_empty(), "{first:?}");

    world.edit().await;
    assert!(
        std::fs::read_to_string(world.workspace.join("src/Calculator.cs"))
            .unwrap()
            .contains("ProbeG1"),
        "the disk has the probe"
    );
    let found = world
        .app
        .symbols(&world.workspace, "ProbeG1")
        .await
        .unwrap();
    assert_eq!(names(&found.results), ["ProbeG1"], "{found:?}");
    // (rust-analyzer's path is missing in this fixture, so it has no server;
    // its failed-start session entry is also stopped and noted by the
    // barrier, which is why the notes are searched, not compared whole.)
    assert!(
        found
            .notes
            .contains(&"lsp: csharp-ls restarted: the tree changed since its last call".to_owned()),
        "{found:?}"
    );
    let events = events_of(&world.session_state);
    let methods: Vec<&str> = events
        .iter()
        .map(|e| e["method"].as_str().unwrap())
        .collect();
    for notification in [
        "textDocument/didOpen",
        "textDocument/didChange",
        "textDocument/didClose",
        "workspace/didChangeWatchedFiles",
    ] {
        assert!(
            !methods.contains(&notification),
            "{notification} in {methods:?}"
        );
    }
    world.assert_restarted_once_per_edit(&events);
    assert_eq!(
        store_lines(&world.session_state),
        calculator(1, false).lines().count() as u64,
        "the second server's store holds the text it walked"
    );

    // No sync ran.
    assert!(
        world
            .app
            .language_servers()
            .sync_status(&world.workspace_id())
            .is_none()
    );
    // The tree is unchanged: the second server is kept.
    let again = world
        .app
        .symbols(&world.workspace, "ProbeG1")
        .await
        .unwrap();
    assert_eq!(names(&again.results), ["ProbeG1"]);
    assert!(
        again.notes.iter().any(
            |note| note.starts_with("lsp: csharp-ls kept session; tree stamp unchanged since ")
        ),
        "{again:?}"
    );
    assert!(
        !again.notes.iter().any(|note| note.contains("restarted")),
        "{again:?}"
    );
    world.assert_restarted_once_per_edit(&events_of(&world.session_state));
    // A new session (after a release) walks the disk again and has it.
    world
        .app
        .release_language_servers(&world.workspace)
        .await
        .unwrap();
    let fresh = world
        .app
        .symbols(&world.workspace, "ProbeG1")
        .await
        .unwrap();
    assert_eq!(names(&fresh.results), ["ProbeG1"]);
    assert!(
        events_of(&world.session_state)
            .iter()
            .any(|event| event["spawn"] == 3)
    );
    assert_eq!(
        store_lines(&world.session_state),
        calculator(1, false).lines().count() as u64
    );
    world.finish().await;
}

/// With `[run_profile.lsp] sync = "watch"` the session is told about the
/// edit (the first server received the `workspace/didChangeWatchedFiles`),
/// and the tree barrier (N104) then replaces it: the second server lists the
/// probe, the fake's store holds the new text, and the sync walked the tree
/// once per session (at start), not per event.
#[tokio::test]
async fn with_sync_on_the_session_is_told_about_the_edit_and_the_barrier_replaces_it() {
    let mut world = World::start(IndexFreshness::OnSearch).await;
    let names = |results: &[local_code_intelligence::lsp::Location]| -> Vec<String> {
        results
            .iter()
            .filter(|found| found.relative_file_path.as_deref() == Some("src/Calculator.cs"))
            .map(|found| found.name.clone().unwrap_or_default())
            .collect()
    };
    let first = world
        .app
        .symbols(&world.workspace, "Combine")
        .await
        .unwrap();
    assert_eq!(names(&first.results), ["Combine"]);

    world.edit().await;
    let found = world
        .app
        .symbols(&world.workspace, "ProbeG1")
        .await
        .unwrap();
    assert_eq!(names(&found.results), ["ProbeG1"], "{found:?}");
    // (rust-analyzer's path is missing in this fixture, so it has no server;
    // its failed-start session entry is also stopped and noted by the
    // barrier, which is why the notes are searched, not compared whole.)
    assert!(
        found
            .notes
            .contains(&"lsp: csharp-ls restarted: the tree changed since its last call".to_owned()),
        "{found:?}"
    );
    world.assert_session_told();
    assert_eq!(
        store_lines(&world.session_state),
        calculator(1, false).lines().count() as u64,
        "the store holds the edited text"
    );
    // The replacement session started a new sync, whose start walk runs on
    // its own task after `start` returns: wait for it on the sync's own
    // progress signal (`synced` with no paths waits for the first walk), as
    // `write_generation` does, instead of reading the counters at once.
    let servers = world.app.language_servers();
    let id = world.workspace_id();
    assert!(
        servers.synced(&id, &[], LOADED_BUDGET).await,
        "the replacement session's sync walked the tree: {:?}",
        servers.sync_status(&id)
    );
    let status = servers
        .sync_status(&id)
        .expect("the session's workspace is synced");
    assert_eq!((status.walks, status.rescans), (1, 0), "{status:?}");
    // The replacement session started after the edit and walked the disk, so
    // its sync forwarded nothing; the forwarding to the first server is the
    // `workspace/didChangeWatchedFiles` event `assert_session_told` read.
    assert_eq!((status.batches, status.forwarded), (0, 0), "{status:?}");
    // Released with its last session.
    world
        .app
        .release_language_servers(&world.workspace)
        .await
        .unwrap();
    assert!(
        world
            .app
            .language_servers()
            .sync_status(&world.workspace_id())
            .is_none()
    );
    world.finish().await;
}

/// With the key off the answer has no `maps[]`, so the maps that are labelled
/// with it on are gaps: the same data, no label. Run on the case that finds
/// the most (`on-search`) here, and on the line shift in the test below.
#[tokio::test]
async fn without_the_key_the_labelled_maps_are_gaps_and_the_answer_has_no_maps() {
    let mut world = World::start_with(IndexFreshness::OnSearch, false, false, false).await;
    let before = world.observe().await;
    assert!(
        before.walk.get("maps").is_none(),
        "no maps[] with the key off"
    );
    world.edit().await;
    let truth = world.truth();
    let now = world.observe().await;
    assert!(now.walk.get("maps").is_none(), "no maps[] with the key off");
    assert!(now.search.get("freshness").is_none(), "no freshness block");
    let verdicts = world.verdicts(&before, &now, &truth);
    let shown = table(Case::OnSearch, &verdicts);
    eprintln!("key off\n{shown}");
    problems_of(
        (false, false, false, LspPlaces::Scip),
        Case::OnSearch,
        &verdicts,
    )
    .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    // Every map the key labels is a silent gap here (the lag is the number of
    // generations).
    for (_, map, why) in EXPECTED_LABELLED
        .iter()
        .filter(|(case, _, _)| *case == Case::OnSearch)
    {
        assert_eq!(verdicts[map], Verdict::Gap { why: *why, lag: 1 }, "{map}");
    }
    world.finish().await;
}

/// The line shift of the test above, with the key off.
#[tokio::test]
async fn without_the_key_the_labelled_maps_are_gaps_on_a_line_shift_too() {
    let mut world = World::start_with(IndexFreshness::OnSearch, false, false, false).await;
    world.edit().await;
    world.regenerate_scip();
    let before = world.observe().await;
    world.shift().await;
    let truth = world.truth();
    let after = world.observe().await;
    assert!(
        after.walk.get("maps").is_none(),
        "no maps[] with the key off"
    );
    let verdicts = shift_verdicts(&before, &after, &truth, world.files_behind());
    let shown = shift_table(&verdicts);
    eprintln!("key off\n{shown}");
    problems_of(
        (false, false, false, LspPlaces::Scip),
        Case::LineShift,
        &verdicts,
    )
    .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    world.finish().await;
}

/// With `map_freshness` on and `verify_on_read` off, the answers are those of
/// K2a-2: the snapshot maps have no label, the live maps say `live`, not stale,
/// lag 0, and no card says `file_changed`. So the maps of `VERIFIED_ON_READ`
/// are gaps, and it is the key that labels them. Run on the two cases that list
/// them (`manual` and the line shift).
#[tokio::test]
async fn with_verify_on_read_off_the_snapshot_maps_and_the_shifted_live_maps_are_gaps() {
    let live_as_before = |walk: &Value| {
        for map in LIVE_MAPS {
            assert_eq!(
                *map_entry(walk, map),
                json!({"id": map, "observed": "live", "stale": false, "lag": 0}),
            );
        }
        for card in walk["results"].as_array().unwrap() {
            assert!(card.get("file_changed").is_none(), "{card}");
        }
        for entry in walk["maps"].as_array().unwrap() {
            assert!(entry.get("places").is_none(), "{entry}");
        }
    };

    let mut world = World::start_with(IndexFreshness::Manual, true, false, false).await;
    let before = world.observe().await;
    world.edit().await;
    let truth = world.truth();
    let now = world.observe().await;
    let verdicts = world.verdicts(&before, &now, &truth);
    let shown = table(Case::Manual, &verdicts);
    eprintln!("verify off\n{shown}");
    world
        .problems(Case::Manual, &verdicts)
        .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    for map in ["seeds.hybrid", "search.lexical", "search.vector"] {
        assert_eq!(
            verdicts[map],
            Verdict::Gap {
                why: Why::OldSnapshot,
                lag: 1
            },
            "{map}"
        );
        let labelled = now.walk["maps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == map);
        assert!(!labelled, "{map} has no label with the key off");
    }
    live_as_before(&now.walk);
    // The search report has no freshness block, and its channels are gaps.
    assert!(now.search.get("freshness").is_none(), "{}", now.search);
    for map in ["vec.chunk.code", "lexical"] {
        assert_eq!(
            verdicts[map],
            Verdict::Gap {
                why: Why::OldSnapshot,
                lag: 1
            },
            "{map}"
        );
    }
    world.finish().await;

    let mut world = World::start_with(IndexFreshness::OnSearch, true, false, false).await;
    world.edit().await;
    world.regenerate_scip();
    let before = world.observe().await;
    world.shift().await;
    let truth = world.truth();
    let after = world.observe().await;
    let verdicts = shift_verdicts(&before, &after, &truth, world.files_behind());
    let shown = shift_table(&verdicts);
    eprintln!("verify off\n{shown}");
    world
        .problems(Case::LineShift, &verdicts)
        .unwrap_or_else(|problem| panic!("{problem}\n{shown}"));
    for map in LIVE_MAPS {
        assert_eq!(
            verdicts[map],
            Verdict::Gap {
                why: Why::LostEdge,
                lag: 1
            },
            "{map}"
        );
    }
    live_as_before(&after.walk);
    world.finish().await;
}

/// What an observation says about freshness, for a comparison between two
/// worlds: the traverse answer's `maps[]`, which file each card cites says
/// changed, and the search report's `freshness` block. Timings are left out.
fn freshness_signature(now: &Observed) -> Value {
    let mut cards: Vec<String> = now.walk["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|card| {
            let mut maps: Vec<&str> = card["maps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            maps.sort_unstable();
            Some(format!(
                "{} {:?} {}",
                card["site"]["path"].as_str()?,
                maps,
                card["file_changed"]
            ))
        })
        .collect();
    cards.sort();
    json!({
        "maps": now.walk["maps"],
        "cards": cards,
        "snapshot": now.search["freshness"]["snapshot"],
        "channels": now.search["freshness"]["channels"],
        "hits": now.search["freshness"]["hits"],
    })
}

/// `[run_profile.freshness] stat_gate` changes what a read of the tree costs,
/// not what the labels say: the same edits give the same `maps[]` lags and
/// `file_changed` sets, and the same search `freshness` block, as the world
/// without the gate, at every step (the edit, then the line shift). The edits
/// change file lengths, so the documented same-stamp gap is not in play (it has
/// its own test in `lci-core`). `timings.verify_ms` is logged for both worlds
/// (a number, not an assertion).
#[tokio::test]
async fn with_the_stat_gate_on_the_labels_equal_the_full_hash_path() {
    let mut signatures: Vec<Vec<Value>> = Vec::new();
    for stat_gate in [false, true] {
        let mut world =
            World::start_gated(IndexFreshness::OnSearch, true, true, false, stat_gate).await;
        let mut seen = vec![freshness_signature(&world.observe().await)];
        world.edit().await;
        seen.push(freshness_signature(&world.observe().await));
        world.shift().await;
        let after = world.observe().await;
        eprintln!(
            "stat_gate = {stat_gate}: verify_ms of the last search = {}",
            after.search["timings"]["verify_ms"]
        );
        assert!(after.search["timings"]["verify_ms"].is_number());
        seen.push(freshness_signature(&after));
        // The table is the gate's: filled with it on, never touched with it off.
        let kept = lci_core::chunk::stat_gate(&world.workspace)
            .lock()
            .unwrap()
            .len();
        assert_eq!(
            kept > 0,
            stat_gate,
            "{kept} entries with stat_gate = {stat_gate}"
        );
        signatures.push(seen);
        world.finish().await;
    }
    for (step, (off, on)) in signatures[0].iter().zip(&signatures[1]).enumerate() {
        assert_eq!(off, on, "step {step}: the gate changed a label");
    }
    // The edits are seen through the gate, not hidden by it.
    let edited = &signatures[1][1];
    assert!(
        edited["maps"].to_string().contains(r#""stale":true"#)
            && edited["cards"].to_string().contains("true"),
        "{edited}"
    );
}

/// The lists are compared with what a case finds in both directions: a gap or
/// a label that is not listed fails, and so does a listed one that is not one
/// any more (strike it when the map becomes fresh, or move it between the
/// lists when its label changes).
#[test]
fn the_gap_list_is_exact_in_both_directions() {
    // Every entry names a map of the scope, once per case, in one list.
    let mut seen = BTreeSet::new();
    for (case, map, _) in EXPECTED_GAPS
        .iter()
        .chain(SYNCED)
        .chain(EXPECTED_LABELLED)
        .chain(VERIFIED_ON_READ)
    {
        assert!(SCOPE.contains(map) || PLACED_MAPS.contains(map), "{map}");
        assert!(seen.insert((*case, *map)), "{case:?} {map} twice");
    }
    // `verify_on_read` moves exactly the verified maps between the lists:
    // labelled with both keys on, gaps with it off, and every labelled map is
    // a gap with `map_freshness` off.
    let (gaps, labelled) = expected(true, true, true, LspPlaces::Scip);
    for entry in VERIFIED_ON_READ {
        assert!(
            labelled.contains(entry) && !gaps.contains(entry),
            "{entry:?}"
        );
    }
    assert_eq!(gaps, EXPECTED_GAPS);
    // With every key on no gap is left: the language-server session, the last
    // one, is replaced by the tree barrier after an edit (N104; the sync tells
    // it too, which no longer decides).
    assert!(EXPECTED_GAPS.is_empty(), "{EXPECTED_GAPS:?}");
    // `sync` moves exactly the maps of `SYNCED`: gaps with it off, absent
    // (fresh) with it on, whatever the other keys are. The session's maps are
    // no longer among them (N104): the barrier makes them fresh either way.
    assert!(SYNCED.is_empty(), "{SYNCED:?}");
    for (labels, verify) in [(true, true), (true, false), (false, false)] {
        let (on, on_labelled) = expected(labels, verify, true, LspPlaces::Scip);
        let (off, off_labelled) = expected(labels, verify, false, LspPlaces::Scip);
        assert_eq!(on_labelled, off_labelled);
        let added: Listed = off
            .iter()
            .filter(|entry| !on.contains(entry))
            .copied()
            .collect();
        assert_eq!(added, SYNCED);
        assert!(on.iter().all(|entry| off.contains(entry)));
    }
    let (gaps, labelled) = expected(true, false, true, LspPlaces::Scip);
    for entry in VERIFIED_ON_READ {
        assert!(
            gaps.contains(entry) && !labelled.contains(entry),
            "{entry:?}"
        );
    }
    assert_eq!(labelled, EXPECTED_LABELLED);
    let (gaps, labelled) = expected(false, true, false, LspPlaces::Scip);
    assert!(labelled.is_empty());
    assert_eq!(
        gaps.len(),
        EXPECTED_GAPS.len() + SYNCED.len() + EXPECTED_LABELLED.len() + VERIFIED_ON_READ.len()
    );
    for (labels, verify) in [(true, true), (true, false), (false, false)] {
        for sync in [true, false] {
            let (gaps, labelled) = expected(labels, verify, sync, LspPlaces::Scip);
            lists_are_exact(&gaps, &labelled);
        }
    }
    // `places = outline` changes exactly the reason of the entries of
    // `PLACED_BY_OUTLINE` (on whichever list the other keys put them), and
    // nothing else moves.
    for (case, map, why) in PLACED_BY_OUTLINE {
        assert!(
            VERIFIED_ON_READ
                .iter()
                .any(|entry| (entry.0, entry.1) == (*case, *map) && entry.2 != *why),
            "{case:?} {map} is on the verified list with another reason"
        );
    }
    let placed = |list: &Listed| -> Listed {
        list.iter()
            .map(|&entry| {
                PLACED_BY_OUTLINE
                    .iter()
                    .find(|(case, map, _)| (*case, *map) == (entry.0, entry.1))
                    .map_or(entry, |&(case, map, why)| (case, map, why))
            })
            .collect()
    };
    for (labels, verify) in [(true, true), (true, false), (false, false)] {
        for sync in [true, false] {
            let (scip_gaps, scip_labelled) = expected(labels, verify, sync, LspPlaces::Scip);
            let (gaps, labelled) = expected(labels, verify, sync, LspPlaces::Outline);
            assert_eq!(gaps, placed(&scip_gaps), "{labels} {verify} {sync}");
            assert_eq!(labelled, placed(&scip_labelled), "{labels} {verify} {sync}");
            assert!(gaps != scip_gaps || labelled != scip_labelled);
            lists_are_exact(&gaps, &labelled);
        }
    }
    // A map that gave nothing before the edit cannot be called fresh or a gap.
    let (gaps, labelled) = expected(true, true, true, LspPlaces::Scip);
    let mut unexercised = verdicts_of(Case::OnSearch, &gaps, &labelled);
    unexercised.insert("grep.live", Verdict::Unexercised);
    assert!(compare_gaps(&gaps, &labelled, Case::OnSearch, &unexercised).is_err());
}

/// The verdicts a case would find if the lists were true.
fn verdicts_of(case: Case, gaps: &Listed, labelled: &Listed) -> BTreeMap<&'static str, Verdict> {
    SCOPE
        .iter()
        .chain(&PLACED_MAPS)
        .map(|map| {
            let in_case = |list: &Listed| {
                list.iter()
                    .find(|(c, m, _)| *c == case && m == map)
                    .copied()
            };
            let verdict = match (in_case(gaps), in_case(labelled)) {
                (Some((_, _, why)), _) => Verdict::Gap { why, lag: 1 },
                (None, Some(_)) => Verdict::StaleLabelled { lag: 3 },
                (None, None) => Verdict::Fresh,
            };
            (*map, verdict)
        })
        .collect()
}

/// Striking, labelling, silencing or re-explaining an entry of the lists fails
/// the comparison, for each case.
fn lists_are_exact(gaps: &Listed, labelled: &Listed) {
    for case in [
        Case::OnSearch,
        Case::Manual,
        Case::LineShift,
        Case::ScipRewritten,
    ] {
        let found = || verdicts_of(case, gaps, labelled);
        compare_gaps(gaps, labelled, case, &found()).unwrap();
        // The verdict a case finds for each entry the lists name.
        let Some(at) = gaps.iter().position(|(c, _, _)| *c == case) else {
            continue;
        };
        // Strike one entry: the gap the case finds is now a new gap.
        let struck: Vec<_> = gaps
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != at)
            .map(|(_, entry)| *entry)
            .collect();
        let error = compare_gaps(&struck, labelled, case, &found()).unwrap_err();
        assert!(error.contains("new gap"), "{error}");
        // A map that became fresh, still listed.
        let mut fresh = found();
        fresh.insert(gaps[at].1, Verdict::Fresh);
        let error = compare_gaps(gaps, labelled, case, &fresh).unwrap_err();
        assert!(error.contains("not one now"), "{error}");
        // A listed gap that the answer now labels.
        let mut now_labelled = found();
        now_labelled.insert(gaps[at].1, Verdict::StaleLabelled { lag: 3 });
        let error = compare_gaps(gaps, labelled, case, &now_labelled).unwrap_err();
        assert!(error.contains("labelled, not on the list"), "{error}");
        // A listed gap with the wrong reason is both.
        let mut wrong = found();
        wrong.insert(
            gaps[at].1,
            Verdict::Gap {
                why: Why::Unexplained,
                lag: 1,
            },
        );
        assert!(compare_gaps(gaps, labelled, case, &wrong).is_err());
        // A listed label that is a gap again (the label went silent, or it
        // does not cover the lag).
        if let Some((_, map, why)) = labelled.iter().find(|(c, _, _)| *c == case) {
            let mut silent = found();
            silent.insert(map, Verdict::Gap { why: *why, lag: 1 });
            let error = compare_gaps(gaps, labelled, case, &silent).unwrap_err();
            assert!(error.contains("new gap"), "{error}");
            assert!(error.contains("listed as labelled"), "{error}");
        }
    }
}

/// The classifier: a stale map is a gap unless the answer labels it stale with
/// a lag that covers the files it is behind by (`maps[]` of K2a-2, the
/// `freshness.channels` of a search report of K2b).
#[test]
fn a_stale_map_is_labelled_only_by_a_lag_that_covers_the_truth() {
    assert_eq!(
        classify(true, true, Some(5), 3, 2, Why::OldIndex),
        Verdict::Fresh
    );
    assert_eq!(
        classify(false, true, Some(3), 3, 2, Why::OldIndex),
        Verdict::StaleLabelled { lag: 3 }
    );
    // A map that answered nothing is not stale, whatever it says: a map that
    // failed to load would otherwise pass as labelled.
    assert_eq!(
        classify(false, false, Some(3), 3, 2, Why::OldIndex),
        Verdict::Gap {
            why: Why::Unexplained,
            lag: 2
        }
    );
    assert_eq!(
        classify(false, true, Some(1), 3, 2, Why::OldIndex),
        Verdict::Gap {
            why: Why::OldIndex,
            lag: 2
        }
    );
    assert_eq!(
        classify(false, true, None, 3, 2, Why::OldIndex),
        Verdict::Gap {
            why: Why::OldIndex,
            lag: 2
        }
    );
    let observed = |walk: Value, search: Value| Observed {
        index_action: String::new(),
        stale_before_search: false,
        stale_after_search: false,
        reached: Reached::new(),
        edges: Edges::new(),
        walk,
        search,
        probes: BTreeMap::new(),
        answered: BTreeSet::new(),
    };
    let answer = observed(
        json!({"maps": [
            {"id": "graph.scip", "observed": "snapshot", "stale": true, "lag": 3},
            {"id": "config.key", "observed": "snapshot", "stale": true, "lag": null},
            {"id": "exact.closure", "observed": "snapshot", "stale": false, "lag": 3},
            {"id": "grep.live", "observed": "live", "stale": false, "lag": 0},
            {"id": "lexical", "observed": "snapshot", "stale": true, "lag": 9},
        ]}),
        json!({"freshness": {"channels": [
            {"id": "vec.chunk.code", "observed": "snapshot", "stale": true, "lag": 3},
            {"id": "lexical", "observed": "snapshot", "stale": false, "lag": 0},
            {"id": "lsp.symbols", "observed": "session", "stale": null, "lag": null},
        ]}}),
    );
    assert_eq!(labelled_lag(&answer, "graph.scip"), Some(3));
    // A label with no lag, or one that says not stale, labels nothing.
    assert_eq!(labelled_lag(&answer, "config.key"), None);
    assert_eq!(labelled_lag(&answer, "exact.closure"), None);
    assert_eq!(labelled_lag(&answer, "grep.live"), None);
    // A search channel is read from the search report, not from `maps[]`; a
    // session answer that is not compared labels nothing.
    assert_eq!(labelled_lag(&answer, "vec.chunk.code"), Some(3));
    assert_eq!(labelled_lag(&answer, "lexical"), None);
    assert_eq!(labelled_lag(&answer, "lsp.symbols"), None);
    assert_eq!(
        labelled_lag(&observed(json!({}), json!({})), "graph.scip"),
        None,
        "no maps[] with the key off"
    );
    assert_eq!(
        labelled_lag(&observed(json!({}), json!({})), "vec.chunk.code"),
        None,
        "no freshness block with the key off"
    );
}
