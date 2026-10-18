//! Q1 answer units for nodes whose id names no impl.
//!
//! `ScipMap::with_default_ids` ids carry no container (`sym:rust:app::x::a/method`),
//! and a result card gives only the definition site, so the node id alone cannot
//! say that two methods belong to one `impl` block. When the bench knows the
//! evaluated tree's root, this module reads the Rust source at the site with the
//! repository's declaration parser (`lci_core::node::rust_declarations`) and
//! names the enclosing impl block instead: one unit per (file, impl block), the
//! block identified by the span its members cover. Each file is read and parsed
//! at most once per run.

use lci_core::node::rust_declarations;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Where a definition site sits in its source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Placement {
    /// Inside an impl block; the string names (file, block span).
    Impl(String),
    /// Parsed, and not inside any impl block (a free function, a trait
    /// default method, a type, or a line no declaration covers).
    Elsewhere,
    /// The file could not be read or parsed.
    Unreadable,
}

/// One declaration of a file: its lines and the impl block it belongs to.
struct Member {
    start: u32,
    end: u32,
    block: Option<usize>,
}

/// The declarations of one parsed file and the span of each impl block.
struct FileUnits {
    members: Vec<Member>,
    /// (first member line, last member line) of each impl block.
    blocks: Vec<(u32, u32)>,
}

type BlockIdentity = (Vec<String>, Vec<String>);

impl FileUnits {
    fn parse(source: &str) -> Option<FileUnits> {
        let declarations = rust_declarations(source).ok()?;
        let mut members = Vec::new();
        let mut blocks: Vec<(u32, u32)> = Vec::new();
        // The block the previous declaration belonged to, with its identity
        // (inline modules and container chain up to the impl header):
        // consecutive members of one impl share it, and any declaration
        // outside that impl in between starts a new block, so two blocks with
        // one header stay apart when something separates them.
        // The declaration parser gives no impl start line, so a new header
        // between two members of the same identity (consecutive blocks with
        // one header, such as a cfg pair) is found in the source text.
        let lines: Vec<&str> = source.lines().collect();
        let mut current: Option<(BlockIdentity, usize)> = None;
        for declaration in &declarations {
            let impl_end = declaration
                .container
                .iter()
                .rposition(|header| is_impl_header(header));
            let Some(last) = impl_end else {
                current = None;
                members.push(Member {
                    start: declaration.start_line,
                    end: declaration.end_line,
                    block: None,
                });
                continue;
            };
            let identity = (
                declaration.inline_modules.clone(),
                declaration.container[..=last].to_vec(),
            );
            let block = match &current {
                Some((open, block))
                    if *open == identity
                        && !impl_header_between(
                            &lines,
                            blocks[*block].1,
                            declaration.start_line,
                        ) =>
                {
                    blocks[*block].1 = blocks[*block].1.max(declaration.end_line);
                    *block
                }
                _ => {
                    blocks.push((declaration.start_line, declaration.end_line));
                    blocks.len() - 1
                }
            };
            current = Some((identity, block));
            members.push(Member {
                start: declaration.start_line,
                end: declaration.end_line,
                block: Some(block),
            });
        }
        Some(FileUnits { members, blocks })
    }

    fn place(&self, path: &str, line: u32) -> Placement {
        let member = self
            .members
            .iter()
            .filter(|m| m.start <= line && line <= m.end)
            .min_by_key(|m| m.end - m.start);
        match member.and_then(|m| m.block) {
            Some(block) => {
                let (start, end) = self.blocks[block];
                Placement::Impl(format!("impl@{path}:{start}-{end}"))
            }
            None => Placement::Elsewhere,
        }
    }
}

/// Whether an impl header begins on a line after `prev_end` and up to
/// `next_start` (both one-based; the next member's own line counts because a
/// header may share it, `impl A { fn a() {} }`; the previous member's last line
/// does not). Leading attributes and whole-line comments are skipped.
fn impl_header_between(lines: &[&str], prev_end: u32, next_start: u32) -> bool {
    let from = prev_end as usize;
    let to = (next_start as usize).min(lines.len());
    lines
        .get(from..to)
        .is_some_and(|gap| gap.iter().any(|line| starts_impl_header(line)))
}

fn starts_impl_header(line: &str) -> bool {
    let mut rest = line.trim_start();
    while rest.starts_with("#[") {
        let mut depth = 0usize;
        let mut end = None;
        for (index, ch) in rest.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(end) => rest = rest[end..].trim_start(),
            None => return false,
        }
    }
    if rest.starts_with("//") || rest.starts_with("/*") || rest.starts_with('*') {
        return false;
    }
    is_impl_header(rest)
}

pub(super) fn is_impl_header(header: &str) -> bool {
    let header = header.strip_prefix("unsafe ").unwrap_or(header);
    header.starts_with("impl ") || header.starts_with("impl<")
}

/// Resolves definition sites to impl blocks under one source root, caching the
/// parse of every file.
pub(super) struct ImplResolver {
    root: PathBuf,
    files: HashMap<String, Option<FileUnits>>,
}

impl ImplResolver {
    pub(super) fn new(root: PathBuf) -> Self {
        ImplResolver {
            root,
            files: HashMap::new(),
        }
    }

    pub(super) fn place(&mut self, path: &str, line: u64) -> Placement {
        if !path.ends_with(".rs") {
            return Placement::Elsewhere;
        }
        let root = &self.root;
        let units = self
            .files
            .entry(path.to_owned())
            .or_insert_with(|| read_units(root, path));
        match (units, u32::try_from(line)) {
            (Some(units), Ok(line)) => units.place(path, line),
            _ => Placement::Unreadable,
        }
    }

    /// How many distinct files were read (or tried) so far.
    #[cfg(test)]
    pub(super) fn files_seen(&self) -> usize {
        self.files.len()
    }
}

/// The file at `root/path`, parsed; a path that leaves the root is refused.
fn read_units(root: &Path, path: &str) -> Option<FileUnits> {
    let relative = Path::new(path);
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let source = std::fs::read_to_string(root.join(relative)).ok()?;
    FileUnits::parse(&source)
}

/// Groups Q1 method nodes by impl block, and counts what it could not place.
pub(super) struct ImplGrouper {
    resolver: Option<ImplResolver>,
    /// Method or function nodes with no impl in their id that stayed their own
    /// unit because the run has no source root.
    no_root: usize,
    /// The same, because the file could not be read or parsed.
    unreadable: usize,
}

impl ImplGrouper {
    pub(super) fn new(root: Option<&Path>) -> Self {
        ImplGrouper {
            resolver: root.map(|root| ImplResolver::new(root.to_path_buf())),
            no_root: 0,
            unreadable: 0,
        }
    }

    /// The unit of the node defined at `path:line`, or `None` when it stays
    /// its own unit.
    pub(super) fn unit(&mut self, path: &str, line: u64) -> Option<String> {
        let Some(resolver) = &mut self.resolver else {
            self.no_root += 1;
            return None;
        };
        match resolver.place(path, line) {
            Placement::Impl(unit) => Some(unit),
            Placement::Elsewhere => None,
            Placement::Unreadable => {
                self.unreadable += 1;
                None
            }
        }
    }

    /// The warnings for the report: empty when every candidate node was placed.
    pub(super) fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.no_root > 0 {
            warnings.push(format!(
                "Q1 impl grouping off: no source root (pass --tree); {} method or function \
                 nodes whose id names no impl each keep their own answer slot",
                self.no_root
            ));
        }
        if self.unreadable > 0 {
            warnings.push(format!(
                "Q1 impl grouping: {} nodes sit in a file under --tree that could not be read \
                 or parsed; they keep their own answer slot",
                self.unreadable
            ));
        }
        warnings
    }

    #[cfg(test)]
    pub(super) fn files_seen(&self) -> usize {
        self.resolver.as_ref().map_or(0, ImplResolver::files_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "\
pub struct Foo;
pub struct Bar;

impl Foo {
    pub fn a(&self) {}

    pub fn b(&self) {}
}

impl Tr for Bar {
    fn a(&self) {}
}

pub fn free() {}

impl<T> Tr for Wrap<T> {
    fn a(&self) {}
    fn b(&self) {}
}

unsafe impl Send for Foo {
    fn z() {}
}

trait Tr {
    fn d(&self) {}
}
";

    fn units() -> FileUnits {
        FileUnits::parse(SOURCE).expect("parse")
    }

    #[test]
    fn members_of_one_block_share_a_unit() {
        let units = units();
        let a = units.place("src/x.rs", 5);
        let b = units.place("src/x.rs", 7);
        assert!(matches!(a, Placement::Impl(_)));
        assert_eq!(a, b);
        assert_ne!(a, units.place("src/x.rs", 11));
    }

    #[test]
    fn generic_and_unsafe_blocks_are_blocks() {
        let units = units();
        assert_eq!(units.place("p.rs", 17), units.place("p.rs", 18));
        assert!(matches!(units.place("p.rs", 17), Placement::Impl(_)));
        assert!(matches!(units.place("p.rs", 22), Placement::Impl(_)));
        assert_ne!(units.place("p.rs", 17), units.place("p.rs", 22));
    }

    #[test]
    fn free_functions_types_and_trait_defaults_are_elsewhere() {
        let units = units();
        assert_eq!(units.place("p.rs", 14), Placement::Elsewhere);
        assert_eq!(units.place("p.rs", 1), Placement::Elsewhere);
        assert_eq!(units.place("p.rs", 26), Placement::Elsewhere);
        assert_eq!(units.place("p.rs", 3), Placement::Elsewhere);
    }

    #[test]
    fn consecutive_cfg_gated_blocks_with_one_header_are_two_units() {
        let source = "\
#[cfg(windows)]
impl Tr for Foo {
    fn a(&self) {}
}
#[cfg(unix)]
impl Tr for Foo {
    fn a(&self) {}
}
";
        let units = FileUnits::parse(source).expect("parse");
        let first = units.place("p.rs", 3);
        let second = units.place("p.rs", 7);
        assert!(matches!(first, Placement::Impl(_)));
        assert!(matches!(second, Placement::Impl(_)));
        assert_ne!(first, second);

        // Single-line blocks, the header sharing the member's line.
        let compact = "\
#[cfg(windows)]
impl Tr for Foo { fn a(&self) {} }
#[cfg(unix)]
impl Tr for Foo { fn a(&self) {} }
";
        let units = FileUnits::parse(compact).expect("parse");
        assert_ne!(units.place("p.rs", 2), units.place("p.rs", 4));

        // An attribute sharing the header's line.
        let inline_attr = "\
impl Tr for Foo { fn a(&self) {} }
#[cfg(unix)] impl Tr for Foo { fn a(&self) {} }
";
        let units = FileUnits::parse(inline_attr).expect("parse");
        assert_ne!(units.place("p.rs", 1), units.place("p.rs", 2));
    }

    #[test]
    fn blank_lines_comments_and_attributes_between_members_keep_one_unit() {
        let source = "\
impl Foo {
    fn a(&self) {}

    // impl Foo { not a header }
    /// impl Foo docs
    /* impl Foo */

    #[cfg(unix)]
    fn b(&self) {}

    #[inline]
    fn c(&self) {}
}
";
        let units = FileUnits::parse(source).expect("parse");
        let a = units.place("p.rs", 2);
        assert!(matches!(a, Placement::Impl(_)));
        assert_eq!(a, units.place("p.rs", 9));
        assert_eq!(a, units.place("p.rs", 12));
    }

    #[test]
    fn a_path_outside_the_root_is_unreadable() {
        let mut resolver = ImplResolver::new(std::env::temp_dir());
        assert_eq!(resolver.place("../x.rs", 1), Placement::Unreadable);
        assert_eq!(resolver.place("missing-file.rs", 1), Placement::Unreadable);
        assert_eq!(resolver.place("notes.md", 1), Placement::Elsewhere);
    }
}
