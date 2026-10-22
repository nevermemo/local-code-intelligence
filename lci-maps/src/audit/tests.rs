//! The C4 auditor passes `sym.defs` over a synthetic edit script (and, in
//! the slow suite, over a replay of this repository's commits) and rejects
//! a known-bad map at the step each one breaks: a bundle that holds a path
//! (move), a bundle key without the adapter version (adapter bump), and an
//! index that keeps a deleted file's declarations (delete).

use super::*;
use lci_core::{
    bundle::{BundleKey, Declarations, StoredDeclaration},
    chunk::Extraction,
    node::{NodeId, rust_module_path},
};
use std::{collections::BTreeSet, ffi::OsStr, path::PathBuf};

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

const A: &str = "pub fn alpha() -> u8 {\n    1\n}\n\npub struct Engine;\n\nimpl Engine {\n    pub fn run(&self) -> u8 {\n        alpha()\n    }\n}\n";

/// Puts the scratch tree into one state of the script.
type Edit = Box<dyn Fn(&Path)>;

/// The synthetic edit script: every kind of change the spec lists, in an
/// order where each known-bad map is first caught by its own step.
fn script() -> Vec<(Step, Edit)> {
    let step = |label: &str, bump_adapter: bool| Step {
        label: label.into(),
        bump_adapter,
    };
    vec![
        (
            step("initial", false),
            Box::new(|root: &Path| {
                write(root, "src/lib.rs", "pub mod a;\npub mod b;\n");
                write(root, "src/a.rs", A);
                write(
                    root,
                    "src/b.rs",
                    "pub fn beta() {}\n\npub trait Shape {\n    fn area(&self) -> f64;\n}\n",
                );
                write(root, "src/gone.rs", "pub const GONE: u8 = 0;\n");
                write(root, "tools/run.py", "def run():\n    pass\n");
            }),
        ),
        (
            step("edit inside a function", false),
            Box::new(|root: &Path| write(root, "src/a.rs", &A.replace("    1\n", "    2\n"))),
        ),
        (
            step("insert lines above", false),
            Box::new(|root: &Path| {
                write(
                    root,
                    "src/a.rs",
                    &format!("\n\n\n{}", A.replace("    1\n", "    2\n")),
                );
            }),
        ),
        (
            step("duplicate a chunk", false),
            Box::new(|root: &Path| {
                let text = std::fs::read_to_string(root.join("src/a.rs")).unwrap();
                write(
                    root,
                    "src/a.rs",
                    &format!("{text}\npub fn dup() {{}}\n\npub fn dup() {{}}\n"),
                );
            }),
        ),
        (
            step("add a file", false),
            Box::new(|root: &Path| write(root, "src/c.rs", "pub enum Mode { On, Off }\n")),
        ),
        (
            step("delete a file", false),
            Box::new(|root: &Path| std::fs::remove_file(root.join("src/gone.rs")).unwrap()),
        ),
        (
            step("move a file", false),
            Box::new(|root: &Path| {
                std::fs::create_dir_all(root.join("src/shapes")).unwrap();
                std::fs::rename(root.join("src/b.rs"), root.join("src/shapes/b.rs")).unwrap();
            }),
        ),
        (step("adapter bump", true), Box::new(|_: &Path| {})),
        (
            step("edit after the bump", false),
            Box::new(|root: &Path| write(root, "src/c.rs", "pub enum Mode { On, Off, Auto }\n")),
        ),
    ]
}

fn run_script(subject: &mut dyn Audited) -> Result<Vec<StepReport>, ContractFailure> {
    let root = Scratch::new("c4-script").unwrap();
    let script = script();
    let steps: Vec<Step> = script.iter().map(|(step, _)| step.clone()).collect();
    audit(subject, root.path(), &steps, |index| {
        (script[index].1)(root.path());
        Ok(())
    })
}

#[test]
fn sym_defs_is_equal_to_a_full_build_after_every_scripted_step() {
    let mut subject = SymDefsAudit::new().unwrap();
    let report = run_script(&mut subject).unwrap();
    assert_eq!(report.len(), 9);
    assert!(report.iter().all(|step| step.entries > 0));
    // (reused, extracted) per step: the move and the delete extract
    // nothing; the bump extracts every Rust file again.
    let labels: Vec<&str> = report.iter().map(|step| step.label.as_str()).collect();
    let syncs: Vec<_> = labels.iter().zip(&subject.syncs).collect();
    assert_eq!(
        syncs,
        [
            (&"initial", &(0, 4)),
            (&"edit inside a function", &(3, 1)),
            (&"insert lines above", &(3, 1)),
            (&"duplicate a chunk", &(3, 1)),
            (&"add a file", &(4, 1)),
            (&"delete a file", &(4, 0)),
            (&"move a file", &(4, 0)),
            (&"adapter bump", &(0, 4)),
            (&"edit after the bump", &(3, 1)),
        ]
    );
}

/// Keeps the module path in the bundle (lesson L2's failure).
#[derive(Clone)]
struct PathInBundle;

impl Producer for PathInBundle {
    type Payload = Vec<StoredDeclaration>;

    fn id(&self) -> &str {
        "sym.defs.path-in-bundle@1"
    }

    fn applies(&self, language: &str) -> bool {
        Declarations.applies(language)
    }

    fn produce(&self, path: &str, extraction: &Extraction) -> Self::Payload {
        let mut stored = Declarations.produce(path, extraction);
        for declaration in &mut stored {
            let mut modules = rust_module_path(path);
            modules.append(&mut declaration.inline_modules);
            declaration.inline_modules = modules;
        }
        stored
    }
}

/// Keys bundles without the adapter version.
#[derive(Clone)]
struct KeyWithoutAdapterVersion;

impl Producer for KeyWithoutAdapterVersion {
    type Payload = Vec<StoredDeclaration>;

    fn id(&self) -> &str {
        "sym.defs.no-adapter-key@1"
    }

    fn applies(&self, language: &str) -> bool {
        Declarations.applies(language)
    }

    fn produce(&self, path: &str, extraction: &Extraction) -> Self::Payload {
        Declarations.produce(path, extraction)
    }

    fn key(&self, file: &CachedFile) -> BundleKey {
        BundleKey {
            content_hash: file.content_hash.clone(),
            language: file.language.clone(),
            adapter_version: String::new(),
            producer: self.id().to_owned(),
        }
    }
}

/// Keeps the declarations of files that disappeared.
struct KeepsDeletedFiles {
    inner: SymDefsAudit,
    previous: Vec<String>,
}

fn head_path(line: &str) -> Option<String> {
    let head = line.split('\t').nth(1)?;
    head.parse::<NodeId>().ok()?.path().map(str::to_owned)
}

impl Audited for KeepsDeletedFiles {
    fn incremental(&mut self, root: &Path) -> Result<Vec<String>, String> {
        let mut lines = self.inner.incremental(root)?;
        let present: BTreeSet<String> = lines.iter().filter_map(|l| head_path(l)).collect();
        lines.extend(
            self.previous
                .iter()
                .filter(|line| head_path(line).is_some_and(|path| !present.contains(&path)))
                .cloned(),
        );
        lines.sort();
        self.previous = lines.clone();
        Ok(lines)
    }

    fn full(&self, root: &Path) -> Result<Vec<String>, String> {
        self.inner.full(root)
    }

    fn bump_adapter(&mut self) -> Result<(), String> {
        self.inner.bump_adapter()
    }
}

#[test]
fn c4_rejects_each_known_bad_map_at_the_step_it_breaks() {
    let expect = |result: Result<Vec<StepReport>, ContractFailure>, step: &str| {
        let failure = result.expect_err("a known-bad map must fail C4");
        assert_eq!(failure.contract, "C4", "{failure:?}");
        assert!(
            failure
                .detail
                .starts_with(&format!("step {step}: incremental differs")),
            "{failure:?}"
        );
    };
    expect(
        run_script(&mut SymDefsAudit::with_producer(PathInBundle).unwrap()),
        "move a file",
    );
    expect(
        run_script(&mut SymDefsAudit::with_producer(KeyWithoutAdapterVersion).unwrap()),
        "adapter bump",
    );
    expect(
        run_script(&mut KeepsDeletedFiles {
            inner: SymDefsAudit::new().unwrap(),
            previous: Vec::new(),
        }),
        "delete a file",
    );
}

#[test]
fn the_first_difference_names_the_entry_and_its_side() {
    let lines = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        first_difference(&lines(&["a", "b"]), &lines(&["a", "b"])),
        None
    );
    assert_eq!(
        first_difference(&lines(&["a", "c"]), &lines(&["a", "b", "c"])).as_deref(),
        Some("\"b\" (full only)")
    );
    assert_eq!(
        first_difference(&lines(&["a", "b", "c"]), &lines(&["a", "c"])).as_deref(),
        Some("\"b\" (incremental only)")
    );
    assert_eq!(
        first_difference(&lines(&["a", "b"]), &lines(&["a"])).as_deref(),
        Some("\"b\" (incremental only)")
    );
}

/// Removes the worktree when the replay ends, also on a panic.
struct Worktree<'r> {
    repository: &'r Path,
    path: PathBuf,
}

impl Drop for Worktree<'_> {
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(self.repository)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .status();
    }
}

fn git<S: AsRef<OsStr> + std::fmt::Debug>(directory: &Path, args: &[S]) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .map_err(|error| format!("git {args:?}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// The slow suite (decision K-3): replays commits of this repository into a
/// scratch worktree, with one delete (3ba1422 removes `src/app.rs`), one
/// move (0729d9a moves 18 files into `lci-core/src`), one adapter bump and
/// the spike's base commit. Needs the full history (not a shallow clone).
/// Run: `cargo test -p lci-maps --lib -- --ignored c4_commit_replay --nocapture`.
#[test]
#[ignore = "slow suite: contract C4 commit replay (decision K-3); needs the full git history"]
fn c4_commit_replay() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let commits = [
        ("3ba1422^", "before the delete", false),
        ("3ba1422", "delete src/app.rs (3ba1422)", false),
        ("0729d9a^", "before the move", false),
        (
            "0729d9a",
            "move 18 files into lci-core/src (0729d9a)",
            false,
        ),
        ("0729d9a", "adapter bump", true),
        ("b87b16a", "spike base (b87b16a)", false),
    ];
    for (commit, _, _) in &commits {
        git(
            repository,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )
        .unwrap_or_else(|error| panic!("commit {commit} is missing (shallow clone?): {error}"));
    }
    let scratch = Scratch::new("c4-replay").unwrap();
    let tree = scratch.path().join("tree");
    // Entries of worktrees whose directory is gone (a killed run) first.
    git(repository, &["worktree", "prune"]).unwrap();
    git(
        repository,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            tree.as_os_str(),
            OsStr::new(commits[0].0),
        ],
    )
    .unwrap();
    let _worktree = Worktree {
        repository,
        path: tree.clone(),
    };
    let steps: Vec<Step> = commits
        .iter()
        .map(|(_, label, bump_adapter)| Step {
            label: (*label).into(),
            bump_adapter: *bump_adapter,
        })
        .collect();
    let mut subject = SymDefsAudit::new().unwrap();
    let started = Instant::now();
    let report = audit(&mut subject, &tree, &steps, |index| {
        git(&tree, &["checkout", "-q", "--detach", commits[index].0])
    })
    .unwrap();
    for (step, sync) in report.iter().zip(&subject.syncs) {
        println!(
            "{:<45} {:>6} declarations  (reused, extracted) = {:?}  {:.1} s",
            step.label,
            step.entries,
            sync,
            step.elapsed.as_secs_f64()
        );
    }
    println!("total {:.1} s", started.elapsed().as_secs_f64());
    // 0729d9a moves 18 files and edits `src/lib.rs` and adds
    // `lci-core/src/lib.rs`: exactly those two are extracted, every moved
    // file's bundle is reused.
    let moved = &subject.syncs[3];
    assert_eq!(moved.1, 2, "{moved:?}");
}

/// A source that answers every node with one fixed call.
struct FixedCall(crate::Cost);

#[crate::async_trait]
impl EdgeSource for FixedCall {
    async fn edges(&self, _node: &NodeId, _budget: Budget) -> Expansion {
        Expansion {
            edges: Vec::new(),
            calls: vec![crate::traverse::source::ExpandCall {
                map: "g",
                n_edges: 0,
                cost: self.0,
                failure: None,
                ms: 0.0,
                arbitration: None,
            }],
        }
    }

    fn card(&self, _map: crate::MapId) -> crate::edge::MapCard {
        crate::edge::MapCard::default()
    }

    fn set_enabled(&mut self, _id: &str, _on: bool) -> bool {
        false
    }
}

#[tokio::test]
async fn s5_sources_that_charge_different_costs_differ() {
    let node = NodeId::file("src/a.rs").unwrap();
    let cost = |est_tokens, external_calls, elapsed_ms| crate::Cost {
        est_tokens,
        external_calls,
        elapsed_ms,
        ..crate::Cost::default()
    };
    let audit = |left: crate::Cost, right: crate::Cost| {
        let nodes = [node.clone()];
        async move {
            audit_edge_sources(
                &FixedCall(left),
                &FixedCall(right),
                &nodes,
                Duration::from_secs(1),
            )
            .await
        }
    };
    // The time a call took and the external calls a cache saved are not
    // differences; the tokens charged are.
    assert!(audit(cost(5, 1, 3.0), cost(5, 2, 9.0)).await.is_ok());
    let failure = audit(cost(5, 1, 3.0), cost(6, 1, 3.0)).await.unwrap_err();
    assert_eq!(failure.contract, "S5");
    assert!(failure.detail.contains("tokens="), "{}", failure.detail);
}
