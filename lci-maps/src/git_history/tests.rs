//! `git.history` on small throwaway repositories (the `git` CLI is the
//! map's only dependency, so it is the fixture's too).

use super::*;
use crate::{
    Registry, SnapshotRef,
    audit::audit_edge_sources,
    kit_c3::check_edge_invariants,
    scratch::Scratch,
    traverse::{
        merged::{MergedAdjacency, PreMerged},
        source::OnTheFly,
    },
};
use lci_core::{
    chunk::Chunk,
    node::{SymbolKey, SymbolKind},
};

struct Repo {
    dir: Scratch,
    days: u32,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            dir: Scratch::new("git-history").unwrap(),
            days: 0,
        };
        repo.git(&["init", "-q", "-b", "main"], None);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str], date: Option<&str>) -> String {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(self.path())
            .args([
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=tester@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args);
        if let Some(date) = date {
            command
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
        let out = command.output().expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Commits the working tree; returns the new hash.
    fn commit_all(&mut self, message: &str) -> String {
        self.days += 1;
        let date = format!("2026-01-{:02}T12:00:00+00:00", self.days);
        self.git(&["add", "-A"], None);
        self.git(&["commit", "-q", "-m", message], Some(&date));
        self.git(&["rev-parse", "HEAD"], None)
    }

    /// Writes `files` (content made unique by the commit number) and commits.
    fn commit(&mut self, files: &[&str], message: &str) -> String {
        for name in files {
            self.write(name, &format!("// {name} at commit {}\n", self.days + 1));
        }
        self.commit_all(message)
    }
}

fn file(path: &str) -> NodeId {
    NodeId::file(path).unwrap()
}

fn commit_id(sha: &str) -> NodeId {
    NodeId::Commit { sha: sha.into() }
}

fn dsts(edges: &[Edge]) -> Vec<NodeId> {
    edges.iter().map(|e| e.dst.clone()).collect()
}

/// a.rs and b.rs added, both edited, a.rs and c.rs edited, a.rs renamed to
/// a2.rs, a2.rs and b.rs edited. Returns the five hashes, oldest first.
fn lineage() -> (Repo, Vec<String>) {
    let mut repo = Repo::new();
    let mut shas = vec![repo.commit(&["a.rs", "b.rs"], "add a and b")];
    shas.push(repo.commit(
        &["a.rs", "b.rs"],
        "touch both\n\nA longer body that must not reach the subject.",
    ));
    shas.push(repo.commit(&["a.rs", "c.rs"], "a and c"));
    repo.git(&["mv", "a.rs", "a2.rs"], None);
    shas.push(repo.commit_all("rename a"));
    shas.push(repo.commit(&["a2.rs", "b.rs"], "edit a2 and b"));
    (repo, shas)
}

#[test]
fn a_file_gets_its_commits_newest_first_across_a_rename() {
    let (repo, shas) = lineage();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    assert_eq!(map.head(), shas[4]);
    let edges = map.edges_blocking(&file("a2.rs")).unwrap();
    let touched: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Touched)
        .collect();
    let want: Vec<NodeId> = shas.iter().rev().map(|s| commit_id(s)).collect();
    assert_eq!(
        touched.iter().map(|e| e.dst.clone()).collect::<Vec<_>>(),
        want
    );
    // Newest first is the confidence order: 100, 99, ...
    assert_eq!(
        touched.iter().map(|e| e.conf).collect::<Vec<_>>(),
        [100, 99, 98, 97, 96]
    );
    assert!(touched.iter().all(|e| e.witness == WitnessClass::Exact
        && e.map == GIT_HISTORY_ID
        && e.src == file("a2.rs")));
    // The cost is the call's: git processes were run.
    assert!(edges[0].cost.external_calls >= 1 && edges[0].cost.elapsed_ms > 0.0);
}

#[test]
fn the_cap_keeps_the_newest_commits() {
    let (repo, shas) = lineage();
    let config = GitHistoryConfig {
        file_commits: 2,
        ..GitHistoryConfig::default()
    };
    let map = GitHistoryMap::open_with(repo.path(), config, HashMap::new()).unwrap();
    let edges = map.edges_blocking(&file("a2.rs")).unwrap();
    let touched: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Touched)
        .map(|e| e.dst.clone())
        .collect();
    assert_eq!(touched, [commit_id(&shas[4]), commit_id(&shas[3])]);
}

#[test]
fn commit_nodes_carry_hash_first_line_subject_and_date() {
    let (repo, shas) = lineage();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    // Not yet seen through an edge: read from git.
    let node = map.commit_node(&shas[1]).unwrap();
    assert_eq!(node.sha, shas[1]);
    assert_eq!(node.subject, "touch both");
    assert!(
        node.date.starts_with("2026-01-02T12:00:00"),
        "{}",
        node.date
    );
    assert_eq!(node.node(), commit_id(&shas[1]));
    assert!(node.line().starts_with(&shas[1][..7]));
    assert!(node.line().ends_with("touch both"));
    // Seen through an edge: the same node.
    map.edges_blocking(&file("b.rs")).unwrap();
    assert_eq!(map.commit_node(&shas[1]), Some(node));
    assert_eq!(map.commit_node("not a hash"), None);
    assert_eq!(map.commit_node(&"0".repeat(40)), None);
}

#[test]
fn a_commit_gets_the_files_it_touched_that_exist_at_head() {
    let (repo, shas) = lineage();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    // a.rs is gone at HEAD (renamed): only b.rs is left of commit 2.
    let edges = map.edges_blocking(&commit_id(&shas[1])).unwrap();
    assert_eq!(dsts(&edges), [file("b.rs")]);
    assert!(
        edges.iter().all(|e| e.kind == EdgeKind::Touched
            && e.witness == WitnessClass::Exact
            && e.conf == 100)
    );
    let edges = map.edges_blocking(&commit_id(&shas[4])).unwrap();
    let mut got = dsts(&edges);
    got.sort();
    assert_eq!(got, [file("a2.rs"), file("b.rs")]);
    // The first commit has no parent: its files are still found (--root).
    let edges = map.edges_blocking(&commit_id(&shas[0])).unwrap();
    assert_eq!(dsts(&edges), [file("b.rs")]);
}

#[test]
fn a_big_commit_names_each_of_its_files_with_less_confidence() {
    let mut repo = Repo::new();
    let names: Vec<String> = (0..16).map(|i| format!("f{i:02}.rs")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let sha = repo.commit(&refs, "many");
    let map = GitHistoryMap::open(repo.path()).unwrap();
    let edges = map.edges_blocking(&commit_id(&sha)).unwrap();
    assert_eq!(edges.len(), 16);
    assert!(edges.iter().all(|e| e.conf == 50), "{edges:?}");
}

/// x and y change together three times, x and z once, and twice x changes in
/// a commit with 35 other files.
fn cochange_repo() -> Repo {
    let mut repo = Repo::new();
    repo.commit(&["x.rs", "y.rs"], "x and y 1");
    repo.commit(&["x.rs", "y.rs"], "x and y 2");
    repo.commit(&["x.rs", "z.rs"], "x and z");
    repo.commit(&["x.rs", "y.rs"], "x and y 3");
    let bulk: Vec<String> = (0..35).map(|i| format!("bulk/f{i:02}.rs")).collect();
    for round in 0..2 {
        let mut files: Vec<&str> = bulk.iter().map(String::as_str).collect();
        files.push("x.rs");
        repo.commit(&files, &format!("reformat {round}"));
    }
    repo
}

#[test]
fn co_change_counts_pairs_over_the_window_and_skips_big_commits() {
    let repo = cochange_repo();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    let edges = map.edges_blocking(&file("x.rs")).unwrap();
    let co: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Cochange)
        .collect();
    // y three times; z once is below the minimum; the reformat commits are
    // above the file limit, so the bulk files do not count (twice each).
    assert_eq!(dsts_of(&co), [file("y.rs")]);
    assert_eq!(co[0].witness, WitnessClass::Heuristic);
    // 90 * 3 / (4 seed commits + 1)
    assert_eq!(co[0].conf, 54);
    // y's own co-change with x: 3 of its 3 commits.
    let back = map.edges_blocking(&file("y.rs")).unwrap();
    let back: Vec<_> = back
        .iter()
        .filter(|e| e.kind == EdgeKind::Cochange)
        .collect();
    assert_eq!(dsts_of(&back), [file("x.rs")]);
    assert_eq!(back[0].conf, 67); // 90 * 3 / 4
}

fn dsts_of(edges: &[&Edge]) -> Vec<NodeId> {
    edges.iter().map(|e| e.dst.clone()).collect()
}

#[test]
fn the_window_limits_what_co_change_counts() {
    let repo = cochange_repo();
    // Only the newest two commits are in the window: the reformats.
    let config = GitHistoryConfig {
        cochange_window: 2,
        ..GitHistoryConfig::default()
    };
    let map = GitHistoryMap::open_with(repo.path(), config, HashMap::new()).unwrap();
    let edges = map.edges_blocking(&file("x.rs")).unwrap();
    assert!(edges.iter().all(|e| e.kind != EdgeKind::Cochange));
}

#[test]
fn a_symbol_gets_its_files_commits_at_lower_confidence_and_no_co_change() {
    let repo = cochange_repo();
    let symbol = NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec!["x".into()],
        container: Vec::new(),
        name: "run".into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    });
    let table = HashMap::from([(symbol.clone(), "x.rs".to_string())]);
    let map = GitHistoryMap::open_with(repo.path(), GitHistoryConfig::default(), table).unwrap();
    let edges = map.edges_blocking(&symbol).unwrap();
    assert!(!edges.is_empty());
    assert!(
        edges
            .iter()
            .all(|e| e.kind == EdgeKind::Touched && e.src == symbol)
    );
    assert_eq!(edges[0].conf, 80);
    assert!(edges.windows(2).all(|w| w[0].conf >= w[1].conf));
    // A symbol the table does not know has no history.
    let other = NodeId::Symbol(SymbolKey {
        name: "other".into(),
        ..match &symbol {
            NodeId::Symbol(key) => key.clone(),
            _ => unreachable!(),
        }
    });
    assert!(map.edges_blocking(&other).unwrap().is_empty());
}

#[test]
fn history_stops_at_the_head_the_map_was_opened_at() {
    let mut repo = Repo::new();
    let first = repo.commit(&["a.rs"], "first");
    let map = GitHistoryMap::open(repo.path()).unwrap();
    let later = repo.commit(&["a.rs"], "later");
    let edges = map.edges_blocking(&file("a.rs")).unwrap();
    assert_eq!(dsts(&edges), [commit_id(&first)]);
    // A commit after HEAD is not an ancestor of it: no edges.
    assert!(map.edges_blocking(&commit_id(&later)).unwrap().is_empty());
    // And one on another branch.
    repo.git(&["checkout", "-q", "-b", "side"], None);
    let side = repo.commit(&["s.rs"], "side");
    repo.git(&["checkout", "-q", "main"], None);
    let map = GitHistoryMap::open(repo.path()).unwrap();
    assert!(map.edges_blocking(&commit_id(&side)).unwrap().is_empty());
}

#[test]
fn churn_is_the_touch_count_over_the_history_read() {
    let (repo, _) = lineage();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    assert_eq!(map.churn("b.rs"), 3);
    assert_eq!(map.churn("a2.rs"), 2); // added by the rename, edited once
    assert_eq!(map.churn("a.rs"), 4); // the rename deletes it (no rename detection in the churn log)
    assert_eq!(map.churn("nowhere.rs"), 0);
    let top = map.churn_ranked(2);
    assert_eq!(top[0], ("a.rs".to_string(), 4));
    assert_eq!(top[1], ("b.rs".to_string(), 3));
}

#[test]
fn a_directory_that_is_not_a_repository_is_an_error() {
    let dir = Scratch::new("no-git").unwrap();
    let error = GitHistoryMap::open(dir.path()).err().expect("an error");
    assert!(error.contains("git repository"), "{error}");
}

#[test]
fn edges_are_sorted_capped_deterministic_and_pass_the_edge_invariants() {
    let repo = cochange_repo();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    let node = file("x.rs");
    let first = map.edges_blocking(&node).unwrap();
    let mut second = map.edges_blocking(&node).unwrap();
    // Only the measured cost differs between two calls.
    for (a, b) in first.iter().zip(second.iter_mut()) {
        b.cost = a.cost;
    }
    assert_eq!(first, second);
    assert_eq!(
        check_edge_invariants(&node, map.describe(), &first, EXPAND_LIMIT),
        Ok(())
    );
    let card = map.card();
    assert_eq!(card.good_for, [QuestionType::History]);
    assert!(card.expand_p50_ms.unwrap() > 0.0);
    assert_eq!(card.coverage, [NodeKind::Commit, NodeKind::File]);
}

fn budget() -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(30),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

#[tokio::test]
async fn expand_and_query_agree_and_query_names_the_commit() {
    let (repo, shas) = lineage();
    let map = GitHistoryMap::open(repo.path()).unwrap();
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let node = file("b.rs");
    let edges = map.expand(&node, budget(), &view).await;
    assert_eq!(edges.len(), 3);
    assert_eq!(edges[0].dst, commit_id(&shas[4]));
    let seeds = [(node.clone(), 1.0)];
    let params = serde_json::Value::Null;
    let response = map
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: 3,
                budget: budget(),
                params: &params,
                as_of: None,
            },
            &view,
        )
        .await;
    assert_eq!(response.status, MapStatus::Ok);
    assert_eq!(response.hits.len(), 3);
    assert_eq!(response.hits[0].node, commit_id(&shas[4]));
    assert_eq!(response.hits[0].rank, 1);
    let detail = response.hits[0].evidence[0].detail.clone().unwrap();
    assert!(detail.ends_with("edit a2 and b"), "{detail}");
    assert_eq!(response.hits[0].evidence[0].label, SourceLabel::Git);
    assert_eq!(response.hits[0].evidence[0].edge, EdgeKind::Touched);
    // A node nothing here accepts has no edges.
    let none = map
        .expand(
            &NodeId::StringLiteral {
                text_hash: "a".repeat(64),
            },
            budget(),
            &view,
        )
        .await;
    assert!(none.is_empty());
}

fn chunk(path: &str) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line: 1,
        end_line: 1,
        code: String::new(),
        content_hash: format!("hash-{path}"),
    }
}

#[tokio::test]
async fn the_pre_merged_source_equals_the_on_the_fly_source_for_git_history() {
    let (repo, shas) = lineage();
    let mut registry = Registry::new(60.0);
    registry
        .register(
            Box::new(GitHistoryMap::open(repo.path()).unwrap()),
            1.0,
            EXPAND_LIMIT,
        )
        .unwrap();
    let chunks: Vec<Chunk> = ["a2.rs", "b.rs", "c.rs"].map(chunk).to_vec();
    let view = SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: shas[4].clone(),
        },
        chunks.iter().collect(),
    );
    let adjacency = MergedAdjacency::build(&registry, &view).await;
    let mut nodes: Vec<NodeId> = view.ids().to_vec();
    nodes.extend(["a2.rs", "b.rs", "c.rs"].map(file));
    nodes.extend(shas.iter().map(|s| commit_id(s)));
    let fly = OnTheFly::new(&registry, &view);
    let merged = PreMerged::new(&adjacency);
    let report = audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(30))
        .await
        .expect("the sources agree");
    assert_eq!(report.nodes, nodes.len());
    assert!(report.edges >= 15, "{report:?}");
}
