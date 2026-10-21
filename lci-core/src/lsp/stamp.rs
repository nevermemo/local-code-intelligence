//! The tree stamp of a workspace for one language server: what the barrier of
//! a kept session compares to know whether the tree changed since the session
//! last started or was last used (see `Manager::tree_barrier`).
//!
//! The stamp is built from the index's file list, with no directory walk (a
//! walk of a large repository costs seconds). For one server it stats:
//!
//! - every indexed file of the server's languages;
//! - every directory that holds one of those files, and all its ancestors up
//!   to the workspace root: a file added, removed or renamed in such a
//!   directory changes the directory's modification time (NTFS, ext4, APFS);
//! - the server's project manifests ([`crate::lsp::adapter::LspAdapter::stamp_manifests`],
//!   for rust-analyzer `Cargo.toml`, `Cargo.lock` and `rust-analyzer.toml`)
//!   in each of those directories.
//!
//! The stamp is the counts and an order-independent hash of every entry's
//! (path, modification time, length). Nothing is read.
//!
//! Residual, by construction: a new file in a directory that holds none of
//! the server's indexed files and is not an ancestor of one (for
//! rust-analyzer such a file matters once a `mod` line or a manifest changes,
//! and both are stamped); a project manifest of another server that is not on
//! its list; and an edit that keeps a file's modification time and length (a
//! deliberate `set_modified`, or a filesystem whose clock is coarser than the
//! edit interval).

use crate::language;
use anyhow::Result;
use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::Path,
    time::UNIX_EPOCH,
};

/// The stamp of one server's files in one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStamp {
    /// Indexed files of the server's languages and manifests that were stat'd.
    pub files: u64,
    /// Directories that were stat'd.
    pub dirs: u64,
    /// The wrapping sum of a hash of every entry's (kind, path, modification
    /// time, length): the same for any order.
    pub hash: u64,
}

/// What a barrier needs to know of the workspace's index: its file list.
#[async_trait::async_trait]
pub trait IndexedFiles: Send + Sync {
    /// The workspace-relative, `/`-separated paths the workspace's current
    /// index snapshot lists, or `None` when there is no snapshot (the barrier
    /// then restarts the session: no sharing without a file list).
    async fn indexed_files(
        &self,
        workspace: &crate::workspace::Workspace,
    ) -> Option<std::sync::Arc<Vec<String>>>;
}

/// One entry to stat: its kind (`0` an indexed file, `1` a directory, `2` a
/// project manifest, which counts only when it exists) and relative path.
type Task = (u8, String);

/// Stats `task` and returns its contribution to the hash, or `None` for a
/// manifest that does not exist.
fn stat(root: &Path, (kind, path): &Task) -> Option<u64> {
    let metadata = std::fs::metadata(root.join(path)).ok();
    if *kind == 2 && metadata.is_none() {
        return None;
    }
    let modified = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    let len = match (&metadata, *kind) {
        (Some(metadata), 0 | 2) => metadata.len(),
        _ => 0,
    };
    let mut hasher = DefaultHasher::new();
    (kind, path, metadata.is_some(), modified, len).hash(&mut hasher);
    Some(hasher.finish())
}

/// Runs the stats of `tasks`, on a few threads when there are many (a stat
/// costs about 0.1 ms on Windows), and totals them.
fn run(root: &Path, tasks: &[Task]) -> TreeStamp {
    let part = |chunk: &[Task]| {
        let mut stamp = TreeStamp {
            files: 0,
            dirs: 0,
            hash: 0,
        };
        for task in chunk {
            if let Some(hash) = stat(root, task) {
                stamp.hash = stamp.hash.wrapping_add(hash);
                if task.0 == 1 {
                    stamp.dirs += 1;
                } else {
                    stamp.files += 1;
                }
            }
        }
        stamp
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(8);
    if tasks.len() < 512 || threads < 2 {
        return part(tasks);
    }
    let size = tasks.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .chunks(size)
            .map(|chunk| scope.spawn(move || part(chunk)))
            .collect();
        handles.into_iter().fold(
            TreeStamp {
                files: 0,
                dirs: 0,
                hash: 0,
            },
            |total, handle| {
                let stamp = handle.join().expect("a stat thread does not panic");
                TreeStamp {
                    files: total.files + stamp.files,
                    dirs: total.dirs + stamp.dirs,
                    hash: total.hash.wrapping_add(stamp.hash),
                }
            },
        )
    })
}

/// The stamp of the indexed files `indexed` of a workspace at `root`, for a
/// server of `languages` (identifiers of [`language::for_path`]) with the
/// project manifest names `manifests`. A file or directory that is gone is
/// stamped as absent.
pub fn indexed_stamp(
    root: &Path,
    indexed: &[String],
    languages: &[&str],
    manifests: &[&str],
) -> TreeStamp {
    let mut tasks: Vec<Task> = Vec::new();
    let mut dirs: HashSet<&str> = HashSet::new();
    for relative in indexed {
        let Some(adapter) = language::for_path(Path::new(relative)) else {
            continue;
        };
        if !languages.contains(&adapter.identifier()) {
            continue;
        }
        tasks.push((0, relative.clone()));
        let mut directory = relative.as_str();
        while let Some(slash) = directory.rfind('/') {
            directory = &relative[..slash];
            if !dirs.insert(directory) {
                break;
            }
        }
    }
    dirs.insert("");
    for directory in dirs {
        tasks.push((1, directory.to_owned()));
        for manifest in manifests {
            let path = if directory.is_empty() {
                (*manifest).to_owned()
            } else {
                format!("{directory}/{manifest}")
            };
            tasks.push((2, path));
        }
    }
    run(root, &tasks)
}

/// [`indexed_stamp`] for each of `servers` (languages, manifest names), in
/// order, as one blocking call.
pub fn indexed_stamps(
    root: &Path,
    indexed: &[String],
    servers: &[(Vec<&'static str>, &'static [&'static str])],
) -> Result<Vec<TreeStamp>> {
    Ok(servers
        .iter()
        .map(|(languages, manifests)| indexed_stamp(root, indexed, languages, manifests))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Sets the modification time of a file or a directory.
    fn set_modified(path: &Path, time: SystemTime) {
        let mut options = std::fs::OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS opens a directory.
            options.write(true).custom_flags(0x0200_0000);
        }
        #[cfg(not(windows))]
        options.read(true);
        options.open(path).unwrap().set_modified(time).unwrap();
    }

    /// Gives every file and directory under `root` (and `root`) an old
    /// modification time, so a later change cannot share its clock tick.
    fn age(root: &Path) {
        let old = SystemTime::now() - Duration::from_secs(3600);
        for entry in walkdir_all(root) {
            set_modified(&entry, old);
        }
    }

    fn walkdir_all(root: &Path) -> Vec<std::path::PathBuf> {
        let mut found = vec![root.to_path_buf()];
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(walkdir_all(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    fn stamp(root: &Path, indexed: &[&str]) -> TreeStamp {
        let indexed: Vec<String> = indexed.iter().map(|s| (*s).to_owned()).collect();
        indexed_stamp(root, &indexed, &["rust"], &["Cargo.toml", "Cargo.lock"])
    }

    #[test]
    fn a_stamp_changes_with_an_edit_a_new_file_beside_an_indexed_one_or_a_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "Cargo.toml", "[package]\n");
        write(root, "src/lib.rs", "pub fn a() {}\n");
        write(root, "src/deep/mod.rs", "pub fn d() {}\n");
        age(root);
        let indexed = ["src/lib.rs", "src/deep/mod.rs"];
        let before = stamp(root, &indexed);
        assert_eq!(
            (before.files, before.dirs),
            (3, 3),
            "two files, Cargo.toml; src, src/deep, root"
        );
        assert_eq!(stamp(root, &indexed), before, "nothing changed");

        // A new file in a directory that holds an indexed file: the
        // directory's modification time moves.
        write(root, "src/more.rs", "pub fn b() {}\n");
        let added = stamp(root, &indexed);
        assert_ne!(added, before);
        age(root);
        let aged = stamp(root, &indexed);

        // An edit of an indexed file (a new length).
        write(root, "src/lib.rs", "pub fn a() { let _x = 1; }\n");
        let edited = stamp(root, &indexed);
        assert_ne!(edited, aged);
        age(root);
        let aged = stamp(root, &indexed);

        // A manifest edit.
        write(root, "Cargo.toml", "[package]\nname = \"x\"\n");
        let manifest = stamp(root, &indexed);
        assert_ne!(manifest, aged);
        age(root);
        let aged = stamp(root, &indexed);

        // A removed indexed file.
        std::fs::remove_file(root.join("src/deep/mod.rs")).unwrap();
        assert_ne!(stamp(root, &indexed), aged);
    }

    #[test]
    fn other_languages_unlisted_files_and_unrelated_directories_do_not_count() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/lib.rs", "pub fn a() {}\n");
        write(root, "target/debug/gen.rs", "pub fn g() {}\n");
        write(root, "docs/notes.md", "# notes\n");
        age(root);
        // `target/debug/gen.rs` is not in the index (ignored); the markdown
        // file and the C# file are of other languages.
        let indexed = ["src/lib.rs", "docs/notes.md", "src/Program.cs"];
        let before = stamp(root, &indexed);
        assert_eq!(before.files, 1);

        // A change inside an unrelated directory, to a file already there.
        write(root, "target/debug/gen.rs", "pub fn g() { let _x = 1; }\n");
        write(root, "docs/notes.md", "# more notes\n");
        assert_eq!(stamp(root, &indexed), before);
    }

    #[test]
    fn the_hash_does_not_depend_on_the_order_of_the_indexed_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "a/one.rs", "1\n");
        write(root, "b/two.rs", "22\n");
        write(root, "c/three.rs", "333\n");
        let forward = stamp(root, &["a/one.rs", "b/two.rs", "c/three.rs"]);
        let backward = stamp(root, &["c/three.rs", "b/two.rs", "a/one.rs"]);
        assert_eq!(forward, backward);
        assert_ne!(forward, stamp(root, &["a/one.rs", "b/two.rs"]));
    }
}
