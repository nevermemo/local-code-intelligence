//! Where a language server starts (`[run_profile.lsp] roots`).
//!
//! Under `roots = "workspace"` every server starts at the workspace path.
//! Under `roots = "manifest"` a server starts at the directory of the topmost
//! project manifest its adapter names
//! ([`crate::lsp::adapter::LspAdapter::stamp_manifests`]: `go.mod`,
//! `pyproject.toml`, `tsconfig.json`, `*.sln`, ...) among the directories that
//! hold the indexed files of the server's languages and their ancestors up to
//! the workspace. The index's file list holds source files only (a manifest
//! has no source extension), so the manifests are found the way the tree stamp
//! finds them ([`crate::lsp::stamp`]): one directory read per such directory,
//! no recursive walk.
//!
//! A root is used only when it is unambiguous and loses nothing:
//!
//! - exactly one directory holds the topmost (shallowest) manifest, else the
//!   workspace ([`RootReason::SeveralRoots`]: one session per root is not
//!   built, a monorepo of sibling modules keeps today's root);
//! - every indexed file of the server's languages lies below it, else the
//!   workspace ([`RootReason::LeavesFilesOut`]).
//!
//! [`discover`] is the pure rule over a list of relative paths; [`for_server`]
//! adds the directory reads and the second condition.

use super::stamp;
use crate::language;
use std::{
    collections::BTreeSet,
    fmt, io,
    path::{Path, PathBuf},
};

/// Why a server starts where it does, for the roster of `index_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootReason {
    /// `roots = "workspace"`: the key says the workspace.
    Workspace,
    /// `roots = "manifest"` and the index has no file list: the workspace.
    NoFileList,
    /// The server is not one the roster works a root out for (disabled,
    /// unconfigured or serving no language of the workspace): the workspace.
    NotServed,
    /// No manifest among the directories of the server's files: the
    /// workspace.
    NoManifest,
    /// This many sibling directories hold a topmost manifest: the workspace.
    SeveralRoots(usize),
    /// The topmost manifest's directory would leave this many indexed files of
    /// the server's languages outside the root: the workspace.
    LeavesFilesOut(usize),
    /// A directory could not be read: the workspace.
    Unreadable,
    /// The directory of this topmost manifest (the workspace itself when the
    /// manifest is at its top).
    Manifest(String),
}

impl fmt::Display for RootReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace => formatter.write_str("roots_workspace"),
            Self::NoFileList => formatter.write_str("no_file_list"),
            Self::NoManifest => formatter.write_str("no_manifest"),
            Self::NotServed => formatter.write_str("not_served"),
            Self::SeveralRoots(count) => write!(formatter, "several_roots:{count}"),
            Self::LeavesFilesOut(count) => write!(formatter, "files_outside_root:{count}"),
            Self::Unreadable => formatter.write_str("unreadable"),
            Self::Manifest(name) => write!(formatter, "manifest:{name}"),
        }
    }
}

/// The directory one server starts at, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// The absolute directory (the workspace path or a directory below it).
    pub path: PathBuf,
    /// `.` for the workspace, else the `/`-separated path below it.
    pub relative: String,
    pub reason: RootReason,
}

impl Root {
    /// The workspace itself, for `reason`.
    pub fn workspace(workspace: &Path, reason: RootReason) -> Self {
        Self {
            path: workspace.to_path_buf(),
            relative: ".".to_owned(),
            reason,
        }
    }

    fn below(workspace: &Path, relative: &str, reason: RootReason) -> Self {
        if relative.is_empty() {
            return Self::workspace(workspace, reason);
        }
        Self {
            path: relative
                .split('/')
                .fold(workspace.to_path_buf(), |path, part| path.join(part)),
            relative: relative.to_owned(),
            reason,
        }
    }
}

/// The topmost manifest among the workspace-relative, `/`-separated `files`:
/// the directory of the one with the fewest path components, when exactly one
/// directory holds such a manifest. `manifests` are file name patterns (a name,
/// or one with a `*`), as the tree stamp matches them, ignoring ASCII case.
/// With two or more such directories (siblings), or none, the workspace.
/// Among several manifests of the chosen directory the one whose pattern comes
/// first in `manifests` names the reason. Reads nothing from disk.
pub fn discover(workspace: &Path, manifests: &[&str], files: &[String]) -> Root {
    // (depth, directory, pattern rank, file name)
    let mut found: Vec<(usize, &str, usize, &str)> = Vec::new();
    for file in files {
        let (directory, name) = file.rsplit_once('/').unwrap_or(("", file));
        let Some(rank) = manifests
            .iter()
            .position(|pattern| stamp::matches(pattern, name))
        else {
            continue;
        };
        let depth = if directory.is_empty() {
            0
        } else {
            directory.split('/').count()
        };
        found.push((depth, directory, rank, name));
    }
    let Some(top) = found.iter().map(|entry| entry.0).min() else {
        return Root::workspace(workspace, RootReason::NoManifest);
    };
    let topmost: Vec<_> = found.iter().filter(|entry| entry.0 == top).collect();
    let directories: BTreeSet<&str> = topmost.iter().map(|entry| entry.1).collect();
    if directories.len() > 1 {
        return Root::workspace(workspace, RootReason::SeveralRoots(directories.len()));
    }
    let (_, directory, _, name) = topmost
        .iter()
        .min_by_key(|entry| (entry.2, entry.3))
        .copied()
        .expect("a topmost manifest exists");
    Root::below(
        workspace,
        directory,
        RootReason::Manifest((*name).to_owned()),
    )
}

/// The manifests (patterns `manifests`) that sit in the directories of the
/// indexed files of `languages` and in their ancestors up to the workspace
/// `root`: one directory read per such directory, no recursive walk. Paths are
/// workspace-relative and `/`-separated. A directory that is gone is skipped;
/// any other failure to read one is an error.
pub fn manifests_near(
    root: &Path,
    indexed: &[String],
    languages: &[&str],
    manifests: &[&str],
) -> io::Result<Vec<String>> {
    let mut directories: BTreeSet<&str> = BTreeSet::new();
    directories.insert("");
    for relative in served(indexed, languages) {
        let mut directory = relative;
        while let Some(slash) = directory.rfind('/') {
            directory = &relative[..slash];
            if !directories.insert(directory) {
                break;
            }
        }
    }
    let mut found = Vec::new();
    for directory in directories {
        let full = directory
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(root.to_path_buf(), |path, part| path.join(part));
        let entries = match std::fs::read_dir(&full) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !manifests
                .iter()
                .any(|pattern| stamp::matches(pattern, &name))
            {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => continue,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            found.push(if directory.is_empty() {
                name
            } else {
                format!("{directory}/{name}")
            });
        }
    }
    Ok(found)
}

/// The indexed files of `languages` (identifiers of [`language::for_path`]).
fn served<'a>(indexed: &'a [String], languages: &'a [&str]) -> impl Iterator<Item = &'a str> {
    indexed.iter().map(String::as_str).filter(move |relative| {
        language::for_path(Path::new(relative))
            .is_some_and(|adapter| languages.contains(&adapter.identifier()))
    })
}

/// The root of a server of `languages` with the manifest patterns `manifests`
/// in the workspace at `workspace` whose index lists `indexed`: [`discover`]
/// over the manifests [`manifests_near`] finds, then the workspace instead if
/// the chosen directory would leave a file of the server's languages outside
/// it. Blocking (directory reads).
pub fn for_server(
    workspace: &Path,
    manifests: &[&str],
    languages: &[&str],
    indexed: &[String],
) -> Root {
    if manifests.is_empty() {
        return Root::workspace(workspace, RootReason::NoManifest);
    }
    let near = match manifests_near(workspace, indexed, languages, manifests) {
        Ok(near) => near,
        Err(error) => {
            tracing::warn!(
                workspace = %workspace.display(),
                error = %error,
                "root discovery failed: the server starts at the workspace"
            );
            return Root::workspace(workspace, RootReason::Unreadable);
        }
    };
    let root = discover(workspace, manifests, &near);
    if root.relative != "." {
        let prefix = format!("{}/", root.relative);
        let outside = served(indexed, languages)
            .filter(|relative| !relative.starts_with(&prefix))
            .count();
        if outside > 0 {
            return Root::workspace(workspace, RootReason::LeavesFilesOut(outside));
        }
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn write(root: &Path, relative: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x\n").unwrap();
    }

    #[test]
    fn the_topmost_manifest_directory_is_the_root() {
        let workspace = Path::new("/ws");
        let files = list(&[
            "services/api/main.go",
            "services/api/go.mod",
            "services/api/tools/go.mod",
            "services/api/tools/gen/go.sum",
        ]);
        let root = discover(workspace, &["go.mod", "go.sum", "go.work"], &files);
        assert_eq!(root.relative, "services/api");
        assert_eq!(root.path, workspace.join("services").join("api"));
        assert_eq!(root.reason, RootReason::Manifest("go.mod".into()));
        // A manifest at the top is the workspace, whatever lies below it.
        let top = discover(
            workspace,
            &["go.mod"],
            &list(&["go.mod", "services/api/go.mod"]),
        );
        assert_eq!(top.relative, ".");
        assert_eq!(top.path, workspace);
        assert_eq!(top.reason, RootReason::Manifest("go.mod".into()));
        // Several manifests of the chosen directory: the first pattern names it.
        let both = discover(
            workspace,
            &["go.mod", "go.sum", "go.work"],
            &list(&["m/go.work", "m/go.sum", "m/go.mod"]),
        );
        assert_eq!(both.reason, RootReason::Manifest("go.mod".into()));
        // The shallowest wins over deeper manifests, whatever the list order.
        let shallow = discover(
            workspace,
            &["go.mod"],
            &list(&["a/b/c/go.mod", "x/go.mod", "a/b/c/d/go.mod"]),
        );
        assert_eq!(shallow.relative, "x");
    }

    #[test]
    fn siblings_at_the_same_depth_fall_back_to_the_workspace_and_say_so() {
        let workspace = Path::new("/ws");
        let files = list(&["services/api/go.mod", "services/web/go.mod", "x/y/z/go.mod"]);
        let root = discover(workspace, &["go.mod"], &files);
        assert_eq!(root.path, workspace);
        assert_eq!(root.relative, ".");
        assert_eq!(root.reason, RootReason::SeveralRoots(2));
        assert_eq!(root.reason.to_string(), "several_roots:2");
        // A shallower manifest is no sibling of the deeper ones: it is the root.
        let mut with_shallower = files.clone();
        with_shallower.push("x/go.mod".to_owned());
        let shallower = discover(workspace, &["go.mod"], &with_shallower);
        assert_eq!(shallower.relative, "x");
        assert_eq!(shallower.reason, RootReason::Manifest("go.mod".into()));
    }

    #[test]
    fn no_manifest_is_the_workspace() {
        let workspace = Path::new("/ws");
        let root = discover(workspace, &["go.mod"], &list(&["a/main.go", "b/util.go"]));
        assert_eq!(root.path, workspace);
        assert_eq!(root.reason, RootReason::NoManifest);
        assert_eq!(discover(workspace, &["go.mod"], &[]).relative, ".");
        assert_eq!(discover(workspace, &[], &list(&["go.mod"])).relative, ".");
        // An adapter that names no manifest has none; it is not a missing list.
        let nameless = for_server(workspace, &[], &["go"], &list(&["a/main.go"]));
        assert_eq!(nameless.reason, RootReason::NoManifest);
        assert_eq!(RootReason::NotServed.to_string(), "not_served");
    }

    #[test]
    fn a_glob_manifest_matches_by_file_name() {
        let workspace = Path::new("/ws");
        let files = list(&["sub/Calculator.cs", "sub/App.SLN", "other/notes.sln.txt"]);
        let root = discover(workspace, &["*.sln", "*.csproj"], &files);
        assert_eq!(root.relative, "sub");
        assert_eq!(root.reason, RootReason::Manifest("App.SLN".into()));
        // A directory named like a manifest holds none.
        let none = discover(workspace, &["*.sln"], &list(&["x.sln/inner.cs"]));
        assert_eq!(none.reason, RootReason::NoManifest);
    }

    #[test]
    fn manifests_are_read_from_the_directories_of_the_servers_files_only() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        write(ws, "services/api/main.go");
        write(ws, "services/api/go.mod");
        write(ws, "services/api/internal/db/db.go");
        write(ws, "other/go.mod");
        write(ws, "py/pyproject.toml");
        write(ws, "py/app.py");
        std::fs::create_dir_all(ws.join("services/api/go.sum")).unwrap();
        let indexed = list(&[
            "services/api/main.go",
            "services/api/internal/db/db.go",
            "py/app.py",
            "gone/dir/missing.go",
        ]);
        let mut near = manifests_near(ws, &indexed, &["go"], &["go.mod", "go.sum"]).unwrap();
        near.sort();
        // `other/` holds no Go file, `py/` no Go manifest, the `go.sum`
        // directory is not a file, and a vanished directory is skipped.
        assert_eq!(near, ["services/api/go.mod"]);
        let root = for_server(ws, &["go.mod", "go.sum"], &["go"], &indexed[..2]);
        assert_eq!(root.relative, "services/api");
        assert_eq!(root.path, ws.join("services").join("api"));
        assert_eq!(root.reason, RootReason::Manifest("go.mod".into()));
    }

    #[test]
    fn a_root_that_would_leave_files_outside_is_the_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        write(ws, "services/api/main.go");
        write(ws, "services/api/go.mod");
        write(ws, "scripts/gen.go");
        let indexed = list(&["services/api/main.go", "scripts/gen.go"]);
        let root = for_server(ws, &["go.mod"], &["go"], &indexed);
        assert_eq!(root.path, ws);
        assert_eq!(root.reason, RootReason::LeavesFilesOut(1));
        assert_eq!(root.reason.to_string(), "files_outside_root:1");
        // Without the stray file the same manifest is the root.
        let root = for_server(ws, &["go.mod"], &["go"], &indexed[..1]);
        assert_eq!(root.relative, "services/api");
        // An adapter that names no manifest keeps the workspace.
        let root = for_server(ws, &[], &["go"], &indexed[..1]);
        assert_eq!(root.relative, ".");
    }
}
