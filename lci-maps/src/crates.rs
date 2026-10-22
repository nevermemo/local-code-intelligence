//! Crate names for Rust module paths (decision N-9, as the owner settled it
//! on 2026-10-12: "crate name fusion").
//!
//! The S1 module path of a file in a crate that is not at the repository
//! root starts with the crate's directory (`lci-core::chunk`). The
//! canonical form uses the Cargo crate name instead, as Rust code writes it
//! (`lci_core::chunk`): `[package].name` from that directory's
//! `Cargo.toml`, with `-` turned into `_`. The directory form stays an
//! alias: [`CrateNames::canonical`] maps it to the canonical id, so both
//! name one node and never two. The root crate's files keep their S1 path
//! (no prefix). Names are read when bundles are joined, never stored in a
//! bundle, so a bundle stays path- and crate-free (lesson L2).

use lci_core::{manifest::CachedFile, node::NodeId};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

/// Crate directory (the module path's first segment) to crate name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateNames {
    by_directory: BTreeMap<String, String>,
}

/// `[package].name` of a `Cargo.toml`, as Rust code names the crate.
/// `None` without a `[package]` table or a literal name (for example
/// `name.workspace = true`).
pub fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        let value = value.trim();
        let name = value
            .strip_prefix('"')
            .and_then(|rest| rest.split_once('"'))
            .map(|(name, _)| name)?;
        if name.is_empty() {
            return None;
        }
        return Some(name.replace('-', "_"));
    }
    None
}

/// The crate directory of a Rust file, as `rust_module_path` puts it first:
/// the components before the last `src`, when there are any.
fn crate_directory(path: &str) -> Option<String> {
    let components: Vec<&str> = path.split('/').collect();
    let src = components[..components.len().saturating_sub(1)]
        .iter()
        .rposition(|c| *c == "src")?;
    (src > 0).then(|| components[..src].join("/"))
}

impl CrateNames {
    /// Reads the crate name of every crate directory that holds a Rust file
    /// of `files`, from `<root>/<directory>/Cargo.toml`. A directory whose
    /// manifest is missing or names no package keeps the directory form.
    pub fn read(root: &Path, files: &HashMap<String, CachedFile>) -> Self {
        Self::read_directories(
            root,
            files
                .iter()
                .filter(|(_, file)| file.language == "rust")
                .map(|(path, _)| path.as_str()),
        )
    }

    /// [`CrateNames::read`] for the Rust files in `paths` only (a live walk
    /// that knows one file, not the snapshot's file list): the same
    /// `Cargo.toml` lookup. The one difference is that a name another,
    /// unlisted crate also uses, or that is another crate's directory, cannot
    /// be seen and is not dropped. Paths that are not Rust files are ignored.
    pub fn read_paths(root: &Path, paths: &[&str]) -> Self {
        Self::read_directories(
            root,
            paths
                .iter()
                .copied()
                .filter(|path| lci_core::path::has_extension_ignore_case(path, "rs")),
        )
    }

    fn read_directories<'p>(root: &Path, paths: impl Iterator<Item = &'p str>) -> Self {
        let mut named = BTreeMap::new();
        let mut directories = BTreeSet::new();
        for path in paths {
            let Some(directory) = crate_directory(path) else {
                continue;
            };
            if !directories.insert(directory.clone()) {
                continue;
            }
            let manifest = root.join(&directory).join("Cargo.toml");
            if let Some(name) = std::fs::read_to_string(manifest)
                .ok()
                .as_deref()
                .and_then(package_name)
            {
                named.insert(directory, name);
            }
        }
        Self::unambiguous(named, &directories)
    }

    /// From explicit pairs (tests); every pair's directory counts as a
    /// crate directory.
    pub fn from_pairs<'p>(pairs: impl IntoIterator<Item = (&'p str, &'p str)>) -> Self {
        let named: BTreeMap<String, String> = pairs
            .into_iter()
            .map(|(directory, name)| (directory.to_owned(), name.to_owned()))
            .collect();
        let directories = named.keys().cloned().collect();
        Self::unambiguous(named, &directories)
    }

    /// Keeps only the names that cannot merge two crates or chain: a name
    /// that another crate also uses, or that is itself some crate's
    /// directory, is dropped, and those crates keep the directory form.
    /// With no name equal to a directory, [`CrateNames::canonical`] is
    /// idempotent and maps distinct crates to distinct first segments.
    fn unambiguous(named: BTreeMap<String, String>, directories: &BTreeSet<String>) -> Self {
        let mut uses: BTreeMap<&str, usize> = BTreeMap::new();
        for name in named.values() {
            *uses.entry(name.as_str()).or_default() += 1;
        }
        let by_directory = named
            .iter()
            .filter(|(directory, name)| {
                uses[name.as_str()] == 1
                    && (!directories.contains(name.as_str()) || directory == name)
            })
            .map(|(directory, name)| (directory.clone(), name.clone()))
            .collect();
        Self { by_directory }
    }

    /// The canonical id of `id`: a Rust symbol whose module path starts with
    /// a known crate directory gets the crate name there; any other id,
    /// canonical ids included, is returned as it is.
    pub fn canonical(&self, id: NodeId) -> NodeId {
        match id {
            NodeId::Symbol(mut key) if key.language == "rust" => {
                if let Some(name) = key
                    .module_path
                    .first()
                    .and_then(|first| self.by_directory.get(first))
                {
                    key.module_path[0] = name.clone();
                }
                NodeId::Symbol(key)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_name_is_read_with_dashes_as_underscores() {
        assert_eq!(
            package_name("[package]\nname = \"lci-core\"\nversion = \"0.1.0\"\n").as_deref(),
            Some("lci_core")
        );
        // Another table first, comments and spacing.
        assert_eq!(
            package_name("[workspace]\nmembers = []\nname = \"no\"\n\n[package]\n# x\n  name=\"a-b-c\" # crate\n").as_deref(),
            Some("a_b_c")
        );
        // A name in another table only, an inherited name, no manifest text.
        assert_eq!(package_name("[[bin]]\nname = \"tool\"\n"), None);
        assert_eq!(package_name("[package]\nname.workspace = true\n"), None);
        assert_eq!(package_name(""), None);
    }

    #[test]
    fn names_that_would_merge_or_chain_crates_are_dropped_and_canonical_is_idempotent() {
        let id = |text: &str| -> NodeId { text.parse().unwrap() };
        // foo's name is bar's directory: foo keeps its directory form, so
        // foo and bar never share a namespace and bar's name does not chain.
        let crates = CrateNames::from_pairs([("foo", "bar"), ("bar", "baz")]);
        assert_eq!(
            crates.canonical(id("sym:rust:foo::f/fn")),
            id("sym:rust:foo::f/fn")
        );
        assert_eq!(
            crates.canonical(id("sym:rust:bar::f/fn")),
            id("sym:rust:baz::f/fn")
        );
        // Two crates with one package name keep their directory forms.
        let crates =
            CrateNames::from_pairs([("a", "same"), ("b", "same"), ("lci-core", "lci_core")]);
        assert_eq!(
            crates.canonical(id("sym:rust:a::f/fn")),
            id("sym:rust:a::f/fn")
        );
        assert_eq!(
            crates.canonical(id("sym:rust:b::f/fn")),
            id("sym:rust:b::f/fn")
        );
        // Idempotent on every id, canonical or alias.
        for text in [
            "sym:rust:lci-core::chunk::f/fn",
            "sym:rust:lci_core::chunk::f/fn",
            "sym:rust:a::f/fn",
            "sym:rust:same::f/fn",
        ] {
            let once = crates.canonical(id(text));
            assert_eq!(crates.canonical(once.clone()), once, "{text}");
        }
        // A directory whose own name is its directory is fine.
        let crates = CrateNames::from_pairs([("tools", "tools")]);
        assert_eq!(
            crates.canonical(id("sym:rust:tools::f/fn")),
            id("sym:rust:tools::f/fn")
        );
    }

    #[test]
    fn read_paths_reads_the_manifest_of_the_files_crate_only() {
        let tree = tempfile::tempdir().unwrap();
        for (directory, name) in [("lci-core", "lci-core"), ("other", "other-crate")] {
            let directory = tree.path().join(directory);
            std::fs::create_dir_all(directory.join("src")).unwrap();
            std::fs::write(
                directory.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\n"),
            )
            .unwrap();
        }
        let id = |text: &str| -> NodeId { text.parse().unwrap() };
        let crates = CrateNames::read_paths(tree.path(), &["lci-core/src/a.rs", "README.md"]);
        assert_eq!(
            crates.canonical(id("sym:rust:lci-core::a::f/fn")),
            id("sym:rust:lci_core::a::f/fn")
        );
        // The unlisted crate keeps its directory form; a root file has none.
        assert_eq!(
            crates.canonical(id("sym:rust:other::f/fn")),
            id("sym:rust:other::f/fn")
        );
        assert_eq!(
            CrateNames::read_paths(tree.path(), &["src/lib.rs"]),
            CrateNames::default()
        );
    }

    #[test]
    fn the_crate_directory_is_what_the_module_path_puts_first() {
        assert_eq!(
            crate_directory("lci-core/src/chunk.rs").as_deref(),
            Some("lci-core")
        );
        assert_eq!(
            crate_directory("crates/a/src/x/y.rs").as_deref(),
            Some("crates/a")
        );
        assert_eq!(crate_directory("src/app/search.rs"), None);
        assert_eq!(crate_directory("tests/cases/mcp.rs"), None);
        // A file named like the directory is not one.
        assert_eq!(crate_directory("tools/src"), None);
    }
}
