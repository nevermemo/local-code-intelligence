//! The tree stamp of a workspace for one language server: what the barrier of
//! a kept session compares to know whether the tree changed since the session
//! last started or was last used (see `Manager::tree_barrier`).
//!
//! The stamp is built from the index's file list and one directory read per
//! stamped directory, with no recursive walk (a walk of a large repository
//! costs seconds). For one server it covers:
//!
//! - every indexed file of the server's languages (modification time and
//!   length);
//! - every directory that holds one of those files, and all its ancestors up
//!   to the workspace root: a file added, removed or renamed in such a
//!   directory changes the directory's modification time (NTFS, ext4, APFS),
//!   and the directory's child names are hashed too;
//! - one level of children of each of those directories: the modification
//!   time of every child directory (so a file created in an existing,
//!   unindexed `src/generated/`, which a `#[path]` attribute can name, moves
//!   it), except version-control and build-output directories (`.git`,
//!   `target`, `node_modules`, `obj`, `bin`, `build`, `dist`, `out`, virtual
//!   environments), whose times move with
//!   every git command or build;
//! - the server's project configuration files
//!   ([`crate::lsp::adapter::LspAdapter::stamp_manifests`]: for
//!   rust-analyzer `Cargo.toml`, `Cargo.lock` and `rust-analyzer.toml`, for
//!   the TypeScript server `tsconfig.json`, `tsconfig.*.json`, `package.json`
//!   and so on) among the children of each of those directories
//!   (modification time and length).
//!
//! The stamp is the counts and an order-independent hash of all of that.
//! Nothing is read but directories and metadata. Only `NotFound` counts as
//! "absent" (and is stamped as such); any other failure to read a directory or
//! stat an entry fails the stamp, and the barrier then restarts the session
//! and records nothing.
//!
//! Residuals, by construction: a new file two or more levels below a stamped
//! directory, in a directory that holds none of the server's indexed files; a
//! configuration file of the server that is not on its list; and an edit that
//! keeps a file's modification time and length (a deliberate `set_modified`,
//! or a filesystem whose clock is coarser than the edit interval).

use crate::language;
use anyhow::{Context, Result};
use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    io::{self, ErrorKind},
    path::Path,
    time::UNIX_EPOCH,
};

/// The stamp of one server's files in one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStamp {
    /// Indexed files of the server's languages and configuration files that
    /// were stat'd.
    pub files: u64,
    /// Directories that were read.
    pub dirs: u64,
    /// The wrapping sum of a hash of every entry: the same for any order.
    pub hash: u64,
}

impl TreeStamp {
    const EMPTY: Self = Self {
        files: 0,
        dirs: 0,
        hash: 0,
    };

    fn add(&mut self, hash: u64) {
        self.hash = self.hash.wrapping_add(hash);
    }

    fn merge(self, other: Self) -> Self {
        Self {
            files: self.files + other.files,
            dirs: self.dirs + other.dirs,
            hash: self.hash.wrapping_add(other.hash),
        }
    }
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

/// Child directories whose modification time moves with every git command or
/// build, and which hold no source a server reads through a `#[path]`-like
/// reference: left out of the one-level child scan.
const SKIPPED_CHILDREN: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "obj",
    "bin",
    "build",
    "dist",
    "out",
    "__pycache__",
    ".venv",
    "venv",
];

/// One entry to stamp: an indexed file or a directory, by relative path.
enum Task {
    File(String),
    Directory(String),
}

fn hash_of(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// The modification time as a hashable value; fails when the platform has none.
fn time_of(metadata: &std::fs::Metadata) -> io::Result<(bool, u128)> {
    let modified = metadata.modified()?;
    Ok(match modified.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => (false, elapsed.as_nanos()),
        Err(before) => (true, before.duration().as_nanos()),
    })
}

/// Metadata of `path`: `None` when it does not exist, an error for any other
/// failure.
fn metadata_of(path: &Path) -> io::Result<Option<std::fs::Metadata>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether the file name `name` is one of the configuration file patterns
/// (`pattern` is a name, or has one `*`), ignoring ASCII case (Windows and
/// macOS volumes are case-insensitive).
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.is_char_boundary(prefix.len())
                && name.is_char_boundary(name.len() - suffix.len())
                && name[..prefix.len()].eq_ignore_ascii_case(prefix)
                && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        }
        None => name.eq_ignore_ascii_case(pattern),
    }
}

/// Stamps an indexed file.
fn stamp_file(root: &Path, path: &str, stamp: &mut TreeStamp) -> io::Result<()> {
    let metadata = metadata_of(&root.join(path))?;
    let (present, time, len) = match &metadata {
        Some(metadata) => (true, time_of(metadata)?, metadata.len()),
        None => (false, (false, 0), 0),
    };
    stamp.add(hash_of((0u8, path, present, time, len)));
    stamp.files += 1;
    Ok(())
}

/// Stamps a directory: its own modification time, its child names, one level
/// of child directories' modification times and the configuration files
/// among its children.
fn stamp_directory(
    root: &Path,
    path: &str,
    manifests: &[&str],
    stamp: &mut TreeStamp,
) -> io::Result<()> {
    let full = if path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(path)
    };
    stamp.dirs += 1;
    let Some(metadata) = metadata_of(&full)? else {
        stamp.add(hash_of((1u8, path, false)));
        return Ok(());
    };
    stamp.add(hash_of((1u8, path, true, time_of(&metadata)?)));
    let entries = match std::fs::read_dir(&full) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            stamp.add(hash_of((1u8, path, false)));
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    // An entry that vanishes between the listing and its stat is skipped: the
    // directory's own time and names moved with it.
    let gone = |error: io::Error| (error.kind() != ErrorKind::NotFound).then_some(error);
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => match gone(error) {
                Some(error) => return Err(error),
                None => continue,
            },
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = match entry.file_type() {
            Ok(kind) => kind.is_dir(),
            Err(error) => match gone(error) {
                Some(error) => return Err(error),
                None => continue,
            },
        };
        stamp.add(hash_of((3u8, path, &name, is_dir)));
        let wanted = if is_dir {
            !SKIPPED_CHILDREN.contains(&name.as_str())
        } else {
            manifests.iter().any(|pattern| matches(pattern, &name))
        };
        if !wanted {
            continue;
        }
        // Not `entry.metadata()`: on Windows that is the directory listing's
        // cached copy, which can lag a change to the entry for a while.
        let metadata = match std::fs::metadata(full.join(&name)) {
            Ok(metadata) => metadata,
            Err(error) => match gone(error) {
                Some(error) => return Err(error),
                None => continue,
            },
        };
        let time = time_of(&metadata)?;
        if is_dir {
            stamp.add(hash_of((4u8, path, &name, time)));
        } else {
            stamp.add(hash_of((2u8, path, &name, time, metadata.len())));
            stamp.files += 1;
        }
    }
    Ok(())
}

/// Runs `tasks`, on a few threads when there are many (a stat costs about
/// 0.1 ms on Windows), and totals them.
fn run(root: &Path, tasks: &[Task], manifests: &[&str]) -> io::Result<TreeStamp> {
    let part = |chunk: &[Task]| -> io::Result<TreeStamp> {
        let mut stamp = TreeStamp::EMPTY;
        for task in chunk {
            match task {
                Task::File(path) => stamp_file(root, path, &mut stamp),
                Task::Directory(path) => stamp_directory(root, path, manifests, &mut stamp),
            }
            .map_err(|error| {
                let path = match task {
                    Task::File(path) | Task::Directory(path) => path,
                };
                io::Error::new(error.kind(), format!("{path}: {error}"))
            })?;
        }
        Ok(stamp)
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
        handles
            .into_iter()
            .try_fold(TreeStamp::EMPTY, |total, handle| {
                Ok(total.merge(handle.join().expect("a stamp thread does not panic")?))
            })
    })
}

/// The stamp of the indexed files `indexed` of a workspace at `root`, for a
/// server of `languages` (identifiers of [`language::for_path`]) with the
/// configuration file patterns `manifests`. A file or directory that is gone
/// is stamped as absent; any other failure to stat or read is an error.
pub fn indexed_stamp(
    root: &Path,
    indexed: &[String],
    languages: &[&str],
    manifests: &[&str],
) -> io::Result<TreeStamp> {
    let mut tasks: Vec<Task> = Vec::new();
    let mut dirs: HashSet<&str> = HashSet::new();
    for relative in indexed {
        let Some(adapter) = language::for_path(Path::new(relative)) else {
            continue;
        };
        if !languages.contains(&adapter.identifier()) {
            continue;
        }
        tasks.push(Task::File(relative.clone()));
        let mut directory = relative.as_str();
        while let Some(slash) = directory.rfind('/') {
            directory = &relative[..slash];
            if !dirs.insert(directory) {
                break;
            }
        }
    }
    dirs.insert("");
    tasks.extend(
        dirs.into_iter()
            .map(|directory| Task::Directory(directory.to_owned())),
    );
    run(root, &tasks, manifests)
}

/// [`indexed_stamp`] for each of `servers` (languages, configuration file
/// patterns), in order, as one blocking call; the first failure fails all.
pub fn indexed_stamps(
    root: &Path,
    indexed: &[String],
    servers: &[(Vec<&'static str>, &'static [&'static str])],
) -> Result<Vec<TreeStamp>> {
    servers
        .iter()
        .map(|(languages, manifests)| {
            indexed_stamp(root, indexed, languages, manifests)
                .with_context(|| format!("stamping {}", root.display()))
        })
        .collect()
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
        indexed_stamp(root, &indexed, &["rust"], &["Cargo.toml", "Cargo.lock"]).unwrap()
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
        write(root, "docs/deep/er/old.md", "# old\n");
        write(root, ".git/HEAD", "ref\n");
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

        // New entries inside the skipped children (a build, a git command)
        // and two levels down do not count either.
        std::fs::create_dir_all(root.join("target/release")).unwrap();
        write(root, ".git/index.lock", "");
        write(root, "docs/deep/er/new.md", "# new\n");
        assert_eq!(
            stamp(root, &indexed),
            before,
            "a skipped child, and a directory two levels below a stamped one"
        );
    }

    /// Sol's case: `src/lib.rs` (indexed) names `#[path = "generated/api.rs"]`
    /// and the file is created in the existing, unindexed `src/generated/`.
    #[test]
    fn a_new_file_in_an_unindexed_child_directory_of_a_stamped_one_changes_the_stamp() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "Cargo.toml", "[package]\n");
        write(
            root,
            "src/lib.rs",
            "#[path = \"generated/api.rs\"]\nmod api;\n",
        );
        write(root, "src/generated/README", "generated\n");
        age(root);
        let indexed = ["src/lib.rs"];
        let before = stamp(root, &indexed);
        assert_eq!(stamp(root, &indexed), before);
        write(root, "src/generated/api.rs", "pub fn f() {}\n");
        assert_ne!(stamp(root, &indexed), before);
    }

    #[test]
    fn a_configuration_file_matched_by_name_in_a_stamped_directory_counts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "app/main.ts", "export const a = 1;\n");
        write(root, "app/tsconfig.base.json", "{}\n");
        write(root, "app/other.json", "{}\n");
        write(root, "tsconfig.json", "{}\n");
        age(root);
        let patterns = ["tsconfig.json", "tsconfig.*.json", "package.json"];
        let stamp = |root: &Path| {
            indexed_stamp(
                root,
                &["app/main.ts".to_owned()],
                &["typescript"],
                &patterns,
            )
            .unwrap()
        };
        let before = stamp(root);
        assert_eq!(
            before.files, 3,
            "main.ts, tsconfig.base.json, tsconfig.json"
        );

        // An unlisted JSON file edited in place: no change.
        write(root, "app/other.json", "{ \"x\": 1 }\n");
        assert_eq!(stamp(root), before);
        // A pattern match (`tsconfig.*.json`) in a stamped directory.
        write(
            root,
            "app/tsconfig.base.json",
            "{ \"compilerOptions\": {} }\n",
        );
        let after = stamp(root);
        assert_ne!(after, before);
        age(root);
        let aged = stamp(root);
        // The one at the root, an ancestor.
        write(root, "tsconfig.json", "{ \"extends\": \"x\" }\n");
        assert_ne!(stamp(root), aged);
    }

    #[test]
    fn name_patterns_have_at_most_one_star_and_ignore_case() {
        assert!(matches("tsconfig.json", "tsconfig.json"));
        assert!(matches("tsconfig.json", "TSConfig.JSON"));
        assert!(!matches("tsconfig.json", "tsconfig.base.json"));
        assert!(matches("tsconfig.*.json", "tsconfig.base.json"));
        assert!(!matches("tsconfig.*.json", "tsconfig.json"));
        assert!(!matches("tsconfig.*.json", "tsconfig..jso"));
        assert!(matches("*.sln", "App.sln"));
        assert!(!matches("tsconfig.*.json", "é"));
    }

    #[test]
    fn only_not_found_counts_as_absent_any_other_stat_error_fails_the_stamp() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(root, "src/lib.rs", "pub fn a() {}\n");
        age(root);
        let run = |indexed: &[&str]| {
            let indexed: Vec<String> = indexed.iter().map(|s| (*s).to_owned()).collect();
            indexed_stamp(root, &indexed, &["rust"], &[])
        };
        // A file the index lists that is gone is stamped as absent.
        let present = run(&["src/lib.rs"]).unwrap();
        let gone = run(&["src/lib.rs", "src/gone.rs"]).unwrap();
        assert_ne!(present, gone);
        assert_eq!(gone, run(&["src/lib.rs", "src/gone.rs"]).unwrap());
        // A path the filesystem refuses is an error, not an absent file (an
        // embedded NUL is `InvalidInput` on every platform).
        let refused = run(&["src/lib.rs", "src/bad\0name.rs"]).unwrap_err();
        assert_ne!(refused.kind(), ErrorKind::NotFound, "{refused}");
        let servers = [(vec!["rust"], &[][..])];
        assert!(
            indexed_stamps(
                root,
                &["src/lib.rs".to_owned(), "src/bad\0name.rs".to_owned()],
                &servers
            )
            .is_err()
        );
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
