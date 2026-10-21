//! N59 (a): an incremental scan equals a cold scan. A seeded loop applies a
//! random sequence of adds, edits, deletes, renames and `.lciignore` edits to a
//! temporary workspace. After every step the scan that reuses the previous
//! manifest (saved and loaded from disk, as the indexer does) must produce the
//! same chunks, files and fingerprint as a scan with an empty manifest, and
//! `load(save(m))` must equal `m`.

use super::{CachedFile, Manifest, VERSION, save, try_load};
use crate::{chunk, workspace::Workspace};
use std::{collections::HashMap, path::Path};

/// SplitMix64: a small deterministic generator, so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Paths share stems across languages, so a rename can change a file's
/// adapter while its content hash stays the same.
const PATHS: &[&str] = &[
    "lib.rs",
    "lib.py",
    "main.go",
    "app.ts",
    "src/lib.rs",
    "src/util.py",
    "src/deep/mod.rs",
    "src/deep/view.ts",
    "tools/gen.go",
    "notes.txt",
];

/// The same bytes reach files of different languages, and two files often hold
/// identical content: the cache must be keyed by path, language and adapter
/// version, not by content alone.
const CONTENTS: &[&[u8]] = &[
    b"",
    b"fn alpha() -> u32 {\n    1\n}\n",
    b"// leading comment\nfn beta(x: u32) -> u32 {\n    x + 2\n}\n\nstruct Gamma;\n",
    b"def alpha():\n    return 1\n\n\nclass Beta:\n    pass\n",
    b"package main\n\nfunc alpha() int {\n\treturn 1\n}\n",
    b"export function alpha(): number {\n  return 1;\n}\n",
    b"fn broken( {\n",
    b"\xEF\xBB\xBFfn with_bom() {}\n",
    b"fn lossy() { let _ = \"\xFF\xFE\"; }\n",
    b"<?xml version=\"1.0\"?>\n<TS version=\"2.1\">\n</TS>\n",
    b"impl Delta {\n    fn one(&self) {}\n    fn two(&self) {}\n}\n",
];

const IGNORE_LINES: &[&str] = &[
    "*.py",
    "src/",
    "deep/",
    "lib.rs",
    "!src/lib.rs",
    "*.ts",
    "tools",
    "# comment",
];

fn existing(root: &Path) -> Vec<&'static str> {
    PATHS
        .iter()
        .copied()
        .filter(|path| root.join(path).is_file())
        .collect()
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// One random edit; returns its description for the failure message.
fn mutate(rng: &mut Rng, root: &Path) -> String {
    let present = existing(root);
    match rng.below(6) {
        // Add or overwrite (an edit when the path exists).
        0 | 1 => {
            let path = *rng.pick(PATHS);
            let content = rng.below(CONTENTS.len());
            write(root, path, CONTENTS[content]);
            format!("write {path} <- content {content}")
        }
        2 if !present.is_empty() => {
            let path = *rng.pick(&present);
            std::fs::remove_file(root.join(path)).unwrap();
            format!("delete {path}")
        }
        3 if !present.is_empty() => {
            let from = *rng.pick(&present);
            let to = *rng.pick(PATHS);
            let target = root.join(to);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::rename(root.join(from), target).unwrap();
            format!("rename {from} -> {to}")
        }
        4 => {
            let directory = *rng.pick(&["", "src/"]);
            let file = root.join(directory).join(".lciignore");
            if rng.below(4) == 0 {
                let _ = std::fs::remove_file(&file);
                format!("remove {directory}.lciignore")
            } else {
                let lines: Vec<&str> = (0..=rng.below(3))
                    .map(|_| *rng.pick(IGNORE_LINES))
                    .collect();
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, lines.join("\n")).unwrap();
                format!("{directory}.lciignore <- {lines:?}")
            }
        }
        // Touch a file without changing it.
        _ => {
            if let Some(path) = present.first() {
                let bytes = std::fs::read(root.join(path)).unwrap();
                std::fs::write(root.join(path), bytes).unwrap();
                format!("rewrite {path} unchanged")
            } else {
                "nothing".into()
            }
        }
    }
}

fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

fn manifest(
    workspace: &Workspace,
    fingerprint: String,
    files: HashMap<String, CachedFile>,
) -> Manifest {
    Manifest {
        version: VERSION,
        workspace_id: workspace.id.clone(),
        workspace_path: workspace.path.clone(),
        fingerprint,
        files,
    }
}

/// What the steps of one or more seeds reached, so the test can tell that the
/// whole run was not vacuous.
#[derive(Default)]
struct Reached {
    reused: usize,
    removed: usize,
    ignored_by_lciignore: usize,
    skipped: usize,
    lossy: usize,
    /// Scans that both reparsed and reused files: where a stale cache entry
    /// would show.
    mixed: usize,
}

impl Reached {
    fn add(&mut self, other: &Reached) {
        self.reused += other.reused;
        self.removed += other.removed;
        self.ignored_by_lciignore += other.ignored_by_lciignore;
        self.skipped += other.skipped;
        self.lossy += other.lossy;
        self.mixed += other.mixed;
    }
}

/// One seed: its own workspace and data directory, a random sequence of edits,
/// and the comparison after the steps that scan.
fn run_seed(seed: u64) -> Reached {
    const STEPS: usize = 14;
    let mut reached = Reached::default();
    let mut rng = Rng(seed);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let workspace = Workspace::resolve(&root, &data).unwrap();
    let mut history = Vec::new();
    for step in 0..STEPS {
        history.push(mutate(&mut rng, &root));
        // Several edits between scans as well as one.
        if rng.below(3) == 0 {
            continue;
        }
        let context = format!("seed {seed}, step {step}, edits {history:#?}");
        let previous = try_load(&data, &workspace).unwrap();
        let incremental = chunk::scan_incremental(&root, &previous.files).unwrap();
        let cold = chunk::scan_incremental(&root, &HashMap::new()).unwrap();

        // The fingerprint reads only the sources; the chunks and files
        // below are what the previous manifest can get wrong.
        assert_eq!(incremental.fingerprint, cold.fingerprint, "{context}");
        assert_eq!(
            incremental.fingerprint,
            chunk::fingerprint(&root).unwrap(),
            "{context}"
        );
        assert_eq!(json(&incremental.chunks), json(&cold.chunks), "{context}");
        assert_eq!(json(&incremental.files), json(&cold.files), "{context}");
        assert_eq!(incremental.lossy_files, cold.lossy_files, "{context}");
        assert_eq!(incremental.skipped_files, cold.skipped_files, "{context}");
        assert_eq!(
            incremental.parsed_files + incremental.unchanged_files,
            cold.files.len(),
            "{context}"
        );
        assert_eq!(
            incremental.removed_files,
            previous
                .files
                .keys()
                .filter(|path| !cold.files.contains_key(*path))
                .count(),
            "{context}"
        );

        reached.reused += incremental.unchanged_files;
        reached.mixed +=
            usize::from(incremental.parsed_files > 0 && incremental.unchanged_files > 0);
        reached.removed += incremental.removed_files;
        reached.skipped += cold.skipped_files.len();
        reached.lossy += cold.lossy_files.len();
        reached.ignored_by_lciignore += existing(&root)
            .iter()
            .filter(|path| !path.ends_with(".txt"))
            .filter(|path| !cold.files.contains_key(**path))
            .filter(|path| !cold.skipped_files.iter().any(|s| s.relative_path == **path))
            .count();

        let next = manifest(&workspace, incremental.fingerprint, incremental.files);
        save(&data, &next).unwrap();
        let loaded = try_load(&data, &workspace).unwrap();
        assert_eq!(json(&loaded), json(&next), "{context}");
    }
    reached
}

#[test]
fn incremental_scans_equal_cold_scans_and_the_manifest_round_trips() {
    use std::sync::atomic::{AtomicU64, Ordering};
    const SEEDS: u64 = 48;
    // The seeds share nothing (each has a temporary workspace of its own), so
    // a few threads take them in turn; a failed assertion of a seed is raised
    // again here with its own message, which names the seed.
    let next = AtomicU64::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    // Totals over the whole run, so the loop cannot pass by never reaching the
    // cases it is about.
    let total = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut reached = Reached::default();
                    loop {
                        let seed = next.fetch_add(1, Ordering::Relaxed);
                        if seed >= SEEDS {
                            break reached;
                        }
                        reached.add(&run_seed(seed));
                    }
                })
            })
            .collect();
        let mut total = Reached::default();
        for handle in handles {
            let reached = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            total.add(&reached);
        }
        total
    });
    assert!(
        total.reused > 0
            && total.mixed > 0
            && total.removed > 0
            && total.ignored_by_lciignore > 0
            && total.skipped > 0
            && total.lossy > 0,
        "reused {}, mixed {}, removed {}, ignored {}, skipped {}, lossy {}",
        total.reused,
        total.mixed,
        total.removed,
        total.ignored_by_lciignore,
        total.skipped,
        total.lossy
    );
}
