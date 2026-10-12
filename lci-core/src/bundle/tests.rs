use super::*;

fn key(content_hash: &str) -> BundleKey {
    BundleKey {
        content_hash: content_hash.into(),
        language: "rust".into(),
        adapter_version: "rust-chunks-v3".into(),
        producer: "test@1".into(),
    }
}

fn store() -> (tempfile::TempDir, BundleStore) {
    let temp = tempfile::tempdir().unwrap();
    let store = BundleStore::new(temp.path(), &"ab".repeat(32));
    (temp, store)
}

#[test]
fn a_missing_bundle_is_none_and_a_stored_one_round_trips() {
    let (_temp, store) = store();
    assert_eq!(store.get::<Vec<String>>(&key("h1")).unwrap(), None);
    store
        .put(&key("h1"), &vec!["a".to_owned(), "b".to_owned()])
        .unwrap();
    assert_eq!(
        store.get::<Vec<String>>(&key("h1")).unwrap(),
        Some(vec!["a".to_owned(), "b".to_owned()])
    );
    // Replacing keeps one file with the new payload and no temporary file.
    store.put(&key("h1"), &vec!["c".to_owned()]).unwrap();
    assert_eq!(
        store.get::<Vec<String>>(&key("h1")).unwrap(),
        Some(vec!["c".to_owned()])
    );
    let shard = store.path_of(&key("h1")).parent().unwrap().to_owned();
    assert_eq!(std::fs::read_dir(shard).unwrap().count(), 1);
}

#[test]
fn every_key_field_separates_bundles() {
    let (_temp, store) = store();
    let base = key("h1");
    store.put(&base, &1u32).unwrap();
    let variants = [
        BundleKey {
            content_hash: "h2".into(),
            ..base.clone()
        },
        BundleKey {
            language: "python".into(),
            ..base.clone()
        },
        BundleKey {
            adapter_version: "rust-chunks-v4".into(),
            ..base.clone()
        },
        BundleKey {
            producer: "test@2".into(),
            ..base.clone()
        },
    ];
    for variant in &variants {
        assert_ne!(store.path_of(variant), store.path_of(&base), "{variant:?}");
        assert_eq!(store.get::<u32>(variant).unwrap(), None, "{variant:?}");
    }
    // The field separator keeps `("a", "bc")` and `("ab", "c")` apart.
    let shifted = BundleKey {
        content_hash: "h1r".into(),
        language: "ust".into(),
        ..base.clone()
    };
    assert_ne!(store.path_of(&shifted), store.path_of(&base));
}

#[test]
fn a_bundle_of_another_key_format_or_shape_reads_as_missing() {
    let (_temp, store) = store();
    let wanted = key("h1");
    let path = store.path_of(&wanted);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A digest collision: the file holds another key.
    let other = serde_json::json!({"format": FORMAT, "key": key("h2"), "payload": 7});
    std::fs::write(&path, serde_json::to_vec(&other).unwrap()).unwrap();
    assert_eq!(store.get::<u32>(&wanted).unwrap(), None);
    // Another format.
    let old = serde_json::json!({"format": FORMAT + 1, "key": wanted, "payload": 7});
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(store.get::<u32>(&wanted).unwrap(), None);
    // Not JSON, or another payload shape.
    std::fs::write(&path, b"{truncated").unwrap();
    assert_eq!(store.get::<u32>(&wanted).unwrap(), None);
    let shape = serde_json::json!({"format": FORMAT, "key": wanted, "payload": "seven"});
    std::fs::write(&path, serde_json::to_vec(&shape).unwrap()).unwrap();
    assert_eq!(store.get::<u32>(&wanted).unwrap(), None);
    // The right file reads.
    let good = serde_json::json!({"format": FORMAT, "key": wanted, "payload": 7});
    std::fs::write(&path, serde_json::to_vec(&good).unwrap()).unwrap();
    assert_eq!(store.get::<u32>(&wanted).unwrap(), Some(7));
}

#[test]
fn a_bundle_that_cannot_be_read_is_an_error() {
    let (_temp, store) = store();
    let wanted = key("h1");
    // A directory where the file should be.
    std::fs::create_dir_all(store.path_of(&wanted)).unwrap();
    assert!(store.get::<u32>(&wanted).is_err());
}

#[test]
fn retain_deletes_only_unkept_bundles_of_this_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = BundleStore::new(temp.path(), &"ab".repeat(32));
    let neighbour = BundleStore::new(temp.path(), &"cd".repeat(32));
    for name in ["h1", "h2", "h3"] {
        store.put(&key(name), &name).unwrap();
        neighbour.put(&key(name), &name).unwrap();
    }
    let stray = store.root().join("notes.txt");
    std::fs::write(&stray, "not a bundle").unwrap();
    let keep: HashSet<BundleKey> = [key("h2")].into();
    assert_eq!(store.retain(&keep).unwrap(), 2);
    assert_eq!(store.get::<String>(&key("h1")).unwrap(), None);
    assert_eq!(
        store.get::<String>(&key("h2")).unwrap().as_deref(),
        Some("h2")
    );
    assert_eq!(store.get::<String>(&key("h3")).unwrap(), None);
    assert!(stray.exists());
    for name in ["h1", "h2", "h3"] {
        assert!(neighbour.get::<String>(&key(name)).unwrap().is_some());
    }
    // A store that never wrote anything has nothing to delete.
    let empty = BundleStore::new(temp.path(), &"ef".repeat(32));
    assert_eq!(empty.retain(&keep).unwrap(), 0);
}

#[test]
fn retain_deletes_temporary_files_of_killed_writers_only_once_they_are_old() {
    let (_temp, store) = store();
    store.put(&key("h1"), &1u32).unwrap();
    let shard = store.path_of(&key("h1")).parent().unwrap().to_owned();
    let old = shard.join(".0123.999.0.tmp");
    let fresh = shard.join(".0123.999.1.tmp");
    std::fs::write(&old, "partial").unwrap();
    std::fs::write(&fresh, "partial").unwrap();
    let two_hours_ago = std::time::SystemTime::now() - 2 * STALE_TEMPORARY;
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();
    let keep: HashSet<BundleKey> = [key("h1")].into();
    assert_eq!(store.retain(&keep).unwrap(), 1);
    assert!(!old.exists());
    assert!(fresh.exists(), "a write in progress keeps its file");
    assert_eq!(store.get::<u32>(&key("h1")).unwrap(), Some(1));
}

#[test]
fn the_store_lives_under_the_data_dir_and_its_paths_stay_short() {
    let temp = tempfile::tempdir().unwrap();
    let store = BundleStore::new(temp.path(), &"ab".repeat(32));
    let path = store.path_of(&key(&"f".repeat(64)));
    let relative = path.strip_prefix(temp.path()).unwrap();
    // bundles/<16>/<2>/<30>.json: 7 + 1 + 16 + 1 + 2 + 1 + 30 + 5 characters,
    // so a data_dir under the 150-character limit stays under 260 on Windows.
    assert_eq!(relative.to_str().unwrap().len(), 63);
    assert!(relative.starts_with(Path::new("bundles").join("abababababababab")));
}

mod synced {
    use super::super::*;
    use crate::{chunk::scan_incremental, manifest::CachedFile};
    use std::collections::HashMap;

    fn write(root: &Path, name: &str, content: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    const SEARCH: &str = "pub struct Engine;\n\nimpl Engine {\n    pub fn run(&self) -> u8 { 1 }\n}\n\npub fn helper() {}\n";

    fn tree(root: &Path) {
        write(root, "src/app/search.rs", SEARCH);
        write(root, "src/lib.rs", "pub fn version() -> u8 { 1 }\n");
        write(root, "tools/run.py", "def run():\n    pass\n");
    }

    /// Every bundle file under the store, with its bytes.
    fn bundle_files(store: &BundleStore) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(store.root()).unwrap() {
            for file in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                out.push((path.clone(), std::fs::read(path).unwrap()));
            }
        }
        out.sort();
        out
    }

    fn symbols(
        synced: &Synced<Vec<StoredDeclaration>>,
        files: &HashMap<String, CachedFile>,
    ) -> Vec<String> {
        let mut out: Vec<String> = synced
            .payloads
            .iter()
            .inspect(|(path, _)| assert!(files.contains_key(*path)))
            .flat_map(|(path, stored)| {
                stored
                    .iter()
                    .map(move |d| d.declaration().unwrap().node_id(path).to_string())
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_moved_file_reuses_its_bundle_unchanged_and_joins_the_new_path() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = workspace.path();
        tree(root);
        let store = BundleStore::new(data.path(), &"ab".repeat(32));

        let first = scan_incremental(root, &HashMap::new()).unwrap();
        let synced = sync(&store, &Declarations, root, &first.files).unwrap();
        // Two Rust files extracted; the Python file has no producer and no
        // bundle.
        assert_eq!((synced.reused, synced.extracted), (0, 2));
        assert_eq!(
            symbols(&synced, &first.files),
            [
                "sym:rust:app::search::Engine/struct",
                "sym:rust:app::search::helper/fn",
                "sym:rust:app::search::impl Engine::run/method",
                "sym:rust:version/fn",
            ]
        );
        let before = bundle_files(&store);
        assert_eq!(before.len(), 2);
        // No bundle holds a path or a module path derived from one.
        for (_, bytes) in &before {
            let text = String::from_utf8(bytes.clone()).unwrap();
            for needle in ["search.rs", "src/", "app", "lib.rs"] {
                assert!(!text.contains(needle), "{needle} in {text}");
            }
        }

        // Move the file and rescan: same content, new path.
        std::fs::create_dir_all(root.join("src/engine")).unwrap();
        std::fs::rename(
            root.join("src/app/search.rs"),
            root.join("src/engine/core.rs"),
        )
        .unwrap();
        let second = scan_incremental(root, &first.files).unwrap();
        let moved = sync(&store, &Declarations, root, &second.files).unwrap();
        assert_eq!((moved.reused, moved.extracted), (2, 0));
        assert_eq!(moved.keys, synced.keys, "the moved file keeps its key");
        // The bundle files are byte-identical: nothing was rewritten.
        assert_eq!(bundle_files(&store), before);
        assert_eq!(
            symbols(&moved, &second.files),
            [
                "sym:rust:engine::core::Engine/struct",
                "sym:rust:engine::core::helper/fn",
                "sym:rust:engine::core::impl Engine::run/method",
                "sym:rust:version/fn",
            ]
        );
        // The joined ids equal those of a fresh extraction at the new path.
        let fresh = BundleStore::new(data.path(), &"cd".repeat(32));
        let full = sync(&fresh, &Declarations, root, &second.files).unwrap();
        assert_eq!((full.reused, full.extracted), (0, 2));
        assert_eq!(
            symbols(&full, &second.files),
            symbols(&moved, &second.files)
        );
        // Reuse never reads the file: with the tree gone, the sync of the
        // same scan still succeeds from the bundles alone.
        std::fs::remove_dir_all(root.join("src")).unwrap();
        let unread = sync(&store, &Declarations, root, &second.files).unwrap();
        assert_eq!((unread.reused, unread.extracted), (2, 0));
    }

    #[test]
    fn a_copied_file_is_extracted_once_and_joined_at_both_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = workspace.path();
        write(root, "src/a.rs", "pub fn shared() {}\n");
        write(root, "src/b.rs", "pub fn shared() {}\n");
        let store = BundleStore::new(data.path(), &"ab".repeat(32));
        let scan = scan_incremental(root, &HashMap::new()).unwrap();
        let synced = sync(&store, &Declarations, root, &scan.files).unwrap();
        assert_eq!((synced.reused, synced.extracted), (1, 1));
        assert_eq!(synced.keys.len(), 1);
        assert_eq!(
            symbols(&synced, &scan.files),
            ["sym:rust:a::shared/fn", "sym:rust:b::shared/fn"]
        );
    }

    #[test]
    fn a_file_that_changed_after_its_scan_fails_the_sync() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = workspace.path();
        tree(root);
        let store = BundleStore::new(data.path(), &"ab".repeat(32));
        let scan = scan_incremental(root, &HashMap::new()).unwrap();
        write(root, "src/app/search.rs", "pub fn other() {}\n");
        let error = sync(&store, &Declarations, root, &scan.files).unwrap_err();
        assert!(
            format!("{error:#}").contains("changed since it was scanned"),
            "{error:#}"
        );
        // A file that disappeared fails too, with the path named.
        std::fs::remove_file(root.join("src/app/search.rs")).unwrap();
        let error = sync(&store, &Declarations, root, &scan.files).unwrap_err();
        assert!(
            format!("{error:#}").contains("src/app/search.rs"),
            "{error:#}"
        );
    }

    #[test]
    fn a_file_scanned_by_another_adapter_version_is_not_extracted() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = workspace.path();
        tree(root);
        let store = BundleStore::new(data.path(), &"ab".repeat(32));
        let mut scan = scan_incremental(root, &HashMap::new()).unwrap();
        scan.files.get_mut("src/lib.rs").unwrap().adapter_version = "rust-chunks-v0".into();
        let error = sync(&store, &Declarations, root, &scan.files).unwrap_err();
        assert!(format!("{error:#}").contains("rust-chunks-v0"), "{error:#}");
    }
}

#[test]
fn put_repairs_an_invalid_bundle_in_place() {
    let (_temp, store) = store();
    let path = store.path_of(&key("h1"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{truncated").unwrap();
    assert_eq!(store.get::<u32>(&key("h1")).unwrap(), None);
    store.put(&key("h1"), &5u32).unwrap();
    assert_eq!(store.get::<u32>(&key("h1")).unwrap(), Some(5));
    let shard = path.parent().unwrap();
    assert_eq!(
        std::fs::read_dir(shard).unwrap().count(),
        1,
        "no temporary file left"
    );
}
