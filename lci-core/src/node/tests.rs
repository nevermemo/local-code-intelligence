//! The S1 rows of `docs/spike/multimap/specs/node-id.md`, section 6.

use super::*;
use crate::{chunk, manifest::CachedFile};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// A small deterministic generator (xorshift64*), so the property test
/// needs no new dependency and every failure reproduces.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// A non-empty string over an alphabet that is mostly separators.
    fn text(&mut self) -> String {
        const ALPHABET: &[char] = &[
            'a', 'b', 'Z', '0', '9', '_', '-', '.', '/', ':', '@', '#', '~', '%', ' ', '\n',
            '\u{1f}', '\u{7f}', 'é', '中', '\u{feff}', '\\', '+', '<', '>',
        ];
        let len = 1 + self.below(8);
        (0..len)
            .map(|_| ALPHABET[self.below(ALPHABET.len())])
            .collect()
    }

    fn segment_without(&mut self, banned: char) -> String {
        let text: String = self.text().chars().filter(|&c| c != banned).collect();
        if text.is_empty() { "x".into() } else { text }
    }

    fn path(&mut self) -> String {
        self.segment_without('\0')
    }

    fn hex(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| char::from_digit(self.below(16) as u32, 16).unwrap())
            .collect()
    }

    fn segments(&mut self, max: usize) -> Vec<String> {
        (0..self.below(max + 1)).map(|_| self.text()).collect()
    }

    fn id(&mut self, kind: NodeKind) -> NodeId {
        match kind {
            NodeKind::File => NodeId::File { path: self.path() },
            NodeKind::Chunk => NodeId::Chunk {
                path: self.path(),
                content_hash: self.text(),
                ordinal: self.below(4) as u32,
            },
            NodeKind::Symbol => NodeId::Symbol(SymbolKey {
                language: self.text(),
                module_path: self.segments(3),
                container: (0..self.below(3))
                    .map(|_| format!("impl {}", self.text()))
                    .collect(),
                name: self.text(),
                kind: [
                    SymbolKind::Function,
                    SymbolKind::Method,
                    SymbolKind::TypeAlias,
                ][self.below(3)],
                disambiguator: self.below(3) as u16,
            }),
            NodeKind::Commit => {
                let len = [40, 64][self.below(2)];
                NodeId::Commit { sha: self.hex(len) }
            }
            NodeKind::Test => NodeId::Test {
                runner: [TestRunner::Cargo, TestRunner::Pytest, TestRunner::Jest][self.below(3)],
                name: self.text(),
            },
            NodeKind::ConfigKey => NodeId::ConfigKey {
                namespace: [ConfigNamespace::Toml, ConfigNamespace::Env][self.below(2)],
                key: self.text(),
            },
            NodeKind::DocSection => NodeId::DocSection {
                path: self.path(),
                heading_path: self.segments(3),
                ordinal: self.below(3) as u32,
            },
            NodeKind::StringLiteral => NodeId::StringLiteral {
                text_hash: self.hex(64),
            },
        }
    }
}

#[test]
fn every_kind_round_trips_and_formatting_is_injective() {
    let mut generator = Gen(0x9e37_79b9_7f4a_7c15);
    let mut ids = HashSet::new();
    let mut strings = HashSet::new();
    for kind in NodeKind::ALL {
        for _ in 0..3000 {
            let id = generator.id(kind);
            id.validate().unwrap();
            let text = id.to_string();
            assert_eq!(id.kind(), kind);
            assert!(text.starts_with(&format!("{}:", kind.prefix())), "{text}");
            assert!(!text.chars().any(char::is_control), "{text:?}");
            let parsed: NodeId = text.parse().unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(parsed, id, "{text}");
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<NodeId>(&json).unwrap(), id);
            ids.insert(id);
            strings.insert(text);
        }
    }
    // Distinct ids never share a string.
    assert_eq!(ids.len(), strings.len());
    assert!(
        ids.len() > 20_000,
        "the generator repeats too much: {}",
        ids.len()
    );
}

#[test]
fn the_spec_examples_are_canonical() {
    let examples = [
        "file:src/app/search.rs",
        "chunk:src/app/search.rs@9f2c#0",
        "sym:rust:app::search::impl App::fuse_hits/method",
        "commit:7f9c636c0000000000000000000000000000abcd",
        "test:cargo:app::search::tests::unit_weights_match_plain_rrf",
        "cfg:toml:fusion.max_chunks_per_file",
        "cfg:env:LCI_CANDIDATE_DUMP",
        "doc:AGENTS.md#hard-rules",
        "str:41ab000000000000000000000000000000000000000000000000000000000000",
    ];
    for example in examples {
        let id: NodeId = example.parse().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(id.to_string(), example);
    }
    let NodeId::Symbol(key) = "sym:rust:app::search::impl App::fuse_hits/method"
        .parse()
        .unwrap()
    else {
        panic!("not a symbol");
    };
    assert_eq!(key.module_path, ["app", "search"]);
    assert_eq!(key.container, ["impl App"]);
    assert_eq!(key.name, "fuse_hits");
    assert_eq!(key.kind, SymbolKind::Method);
}

#[test]
fn strings_that_are_not_canonical_are_refused() {
    for bad in [
        "",
        "file",
        "file:",
        "nope:a.rs",
        "file:%41.rs",
        "file:a%2.rs",
        "file:a%zz.rs",
        "file:a@b.rs",
        "chunk:a.rs@h#01",
        "chunk:a.rs@h#+1",
        "chunk:a.rs@h",
        "chunk:a.rs@#0",
        "sym:rust:impl X::m::f/fn",
        "sym:rust:f/function",
        "sym:rust:f/fn~0",
        "doc:a.md#x~0",
        "commit:ABCDEF0000000000000000000000000000000000",
        "commit:abc",
        "str:41ab",
        "test:nose:x",
        "cfg:toml:",
    ] {
        assert!(bad.parse::<NodeId>().is_err(), "{bad:?} parsed");
    }
}

const FOUR_FUNCTIONS: &str = "fn one() -> u32 {\n    1\n}\n\nfn two() -> u32 {\n    2\n}\n\nfn three() -> u32 {\n    3\n}\n\nfn four() -> u32 {\n    4\n}\n";

fn ids_and_keys(path: &str, source: &str) -> Vec<(NodeId, (String, u32, String))> {
    let chunks = chunk::chunks_for_path(path, source).unwrap();
    chunk_ids(&chunks)
        .into_iter()
        .zip(chunks.iter().map(legacy_fusion_key))
        .collect()
}

#[test]
fn blank_lines_above_a_chunk_keep_every_chunk_id_and_move_the_legacy_keys_below() {
    let before = ids_and_keys("src/lib.rs", FOUR_FUNCTIONS);
    assert_eq!(before.len(), 4, "one chunk per function");
    // Ten blank lines only: a comment or an attribute would attach to the
    // next declaration's chunk as a prefix and change its text.
    let edited = FOUR_FUNCTIONS.replacen("fn three", "\n\n\n\n\n\n\n\n\n\nfn three", 1);
    let after = ids_and_keys("src/lib.rs", &edited);
    assert_eq!(after.len(), 4);
    for (index, ((id_before, key_before), (id_after, key_after))) in
        before.iter().zip(&after).enumerate()
    {
        assert_eq!(id_before, id_after, "chunk {index}");
        // Today's fusion key moves for the chunks below the edit.
        assert_eq!(key_before == key_after, index < 2, "chunk {index}");
    }
    // Symbols are line-independent too.
    let symbols = |source: &str| {
        rust_declarations(source)
            .unwrap()
            .iter()
            .map(|declaration| declaration.node_id("src/lib.rs"))
            .collect::<Vec<_>>()
    };
    assert_eq!(symbols(FOUR_FUNCTIONS), symbols(&edited));
}

#[test]
fn identical_chunks_take_ordinals_in_source_order_and_shift_when_one_is_inserted_above() {
    let body = "fn same() -> u32 {\n    7\n}\n";
    let other = "fn other() -> u32 {\n    8\n}\n";
    let source = format!("{other}\n{body}\n{body}");
    let ids = ids_and_keys("src/dup.rs", &source);
    let ordinals = |ids: &[(NodeId, (String, u32, String))]| {
        ids.iter()
            .map(|(id, (_, line, _))| match id {
                NodeId::Chunk { ordinal, .. } => (*line, *ordinal),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(ordinals(&ids), [(1, 0), (5, 0), (9, 1)]);
    // A third copy above both shifts them to 1 and 2 (documented and
    // accepted: the later duplicates get new ids).
    let shifted = ids_and_keys("src/dup.rs", &format!("{body}\n{source}"));
    assert_eq!(ordinals(&shifted), [(1, 0), (5, 0), (9, 1), (13, 2)]);
    // Ordinals count per file: the same text in another file starts at 0.
    let other_file = ids_and_keys("src/other.rs", &source);
    assert_eq!(ordinals(&other_file), [(1, 0), (5, 0), (9, 1)]);
    let mut both: Vec<Chunk> = chunk::chunks_for_path("src/dup.rs", &source).unwrap();
    both.extend(chunk::chunks_for_path("src/other.rs", &source).unwrap());
    let ids = chunk_ids(&both);
    assert_eq!(ids[3..], chunk_ids(&both[3..])[..]);
}

#[test]
fn chunk_ids_do_not_depend_on_the_order_chunks_are_given_in() {
    let source = format!("{FOUR_FUNCTIONS}\nfn one() -> u32 {{\n    1\n}}\n");
    let chunks = chunk::chunks_for_path("src/lib.rs", &source).unwrap();
    let forward = chunk_ids(&chunks);
    let mut generator = Gen(7);
    for _ in 0..20 {
        let mut order: Vec<usize> = (0..chunks.len()).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, generator.below(i + 1));
        }
        let shuffled: Vec<Chunk> = order.iter().map(|&i| chunks[i].clone()).collect();
        let ids = chunk_ids(&shuffled);
        for (position, &index) in order.iter().enumerate() {
            assert_eq!(ids[position], forward[index]);
        }
    }
}

#[test]
fn an_impl_header_tells_two_methods_of_one_type_apart() {
    let source = "trait A { fn f(&self); }\ntrait B { fn f(&self); }\nstruct X;\n\
                  impl A for X { fn f(&self) {} }\nimpl B for X { fn f(&self) {} }\n";
    let declarations = rust_declarations(source).unwrap();
    let methods: Vec<&Declaration> = declarations
        .iter()
        .filter(|d| d.kind == SymbolKind::Method && d.container[0].starts_with("impl"))
        .collect();
    assert_eq!(methods.len(), 2);
    let ids: Vec<NodeId> = methods.iter().map(|d| d.node_id("src/x.rs")).collect();
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ids[0].to_string(), "sym:rust:x::impl A for X::f/method");
    assert_eq!(ids[1].to_string(), "sym:rust:x::impl B for X::f/method");
    for declaration in &methods {
        assert_eq!(declaration.disambiguator, 0, "no collision with the header");
    }
    // A deliberately reduced key without the impl header collides.
    let reduced: HashSet<String> = methods
        .iter()
        .map(|d| {
            let mut key = d.key("src/x.rs");
            key.container.clear();
            NodeId::Symbol(key).to_string()
        })
        .collect();
    assert_eq!(reduced.len(), 1);
    // The trait methods have their own container.
    let trait_methods: Vec<String> = declarations
        .iter()
        .filter(|d| d.container.first().is_some_and(|c| c.starts_with("trait")))
        .map(|d| d.node_id("src/x.rs").to_string())
        .collect();
    assert_eq!(
        trait_methods,
        [
            "sym:rust:x::trait A::f/method",
            "sym:rust:x::trait B::f/method"
        ]
    );
}

#[test]
fn rust_declarations_cover_items_modules_and_collisions() {
    let source = "mod inner {\n    pub struct S;\n    impl S {\n        pub const N: u32 = 1;\n        \
                  pub fn new() -> Self { fn hidden() {} S }\n    }\n}\n\
                  #[cfg(unix)]\nfn twin() {}\n#[cfg(not(unix))]\nfn twin() {}\n\
                  enum E { A }\nunion U { a: u32 }\nstatic S: u8 = 0;\ntype T = u8;\n\
                  macro_rules! m { () => {} }\nmod outer;\n\
                  impl<T: Clone>   Into<u8>\n    for Vec<T> where T: Copy { fn into(self) -> u8 { 0 } }\n";
    let ids: Vec<String> = rust_declarations(source)
        .unwrap()
        .iter()
        .map(|d| d.node_id("lci-core/src/node.rs").to_string())
        .collect();
    assert_eq!(
        ids,
        [
            "sym:rust:lci-core::node::inner/mod",
            "sym:rust:lci-core::node::inner::S/struct",
            "sym:rust:lci-core::node::inner::impl S::N/const",
            "sym:rust:lci-core::node::inner::impl S::new/method",
            "sym:rust:lci-core::node::twin/fn",
            "sym:rust:lci-core::node::twin/fn~1",
            "sym:rust:lci-core::node::E/enum",
            "sym:rust:lci-core::node::U/union",
            "sym:rust:lci-core::node::S/static",
            "sym:rust:lci-core::node::T/type",
            "sym:rust:lci-core::node::m/macro",
            "sym:rust:lci-core::node::outer/mod",
            "sym:rust:lci-core::node::impl<T%3A Clone> Into<u8> for Vec<T> where T%3A Copy::into/method",
        ]
    );
    for id in &ids {
        assert_eq!(&id.parse::<NodeId>().unwrap().to_string(), id);
    }
}

#[test]
fn module_paths_follow_the_file_path() {
    for (path, expected) in [
        ("src/app/search.rs", &["app", "search"][..]),
        ("src/lib.rs", &[]),
        ("src/main.rs", &["main"]),
        ("src/app/lib.rs", &["app", "lib"]),
        ("xtask/src/main.rs", &["xtask", "main"]),
        ("src/app/mod.rs", &["app"]),
        ("lci-core/src/chunk.rs", &["lci-core", "chunk"]),
        ("lci-core/src/lib.rs", &["lci-core"]),
        ("tests/cases/mcp.rs", &["tests", "cases", "mcp"]),
        ("xtask/src/acceptance/mod.rs", &["xtask", "acceptance"]),
    ] {
        assert_eq!(rust_module_path(path), expected, "{path}");
    }
}

#[test]
fn declarations_hold_no_path_and_a_moved_file_joins_its_new_path() {
    // The path-free half of the "no paths in bundles" row; the bundle store
    // itself is S3.
    let declarations = rust_declarations(FOUR_FUNCTIONS).unwrap();
    let debug = format!("{declarations:?}");
    assert!(!debug.contains("src"), "{debug}");
    let old: Vec<String> = declarations
        .iter()
        .map(|d| d.node_id("src/old.rs").to_string())
        .collect();
    let new: Vec<String> = declarations
        .iter()
        .map(|d| d.node_id("src/new.rs").to_string())
        .collect();
    assert_eq!(old[0], "sym:rust:old::one/fn");
    assert_eq!(new[0], "sym:rust:new::one/fn");
}

#[test]
fn every_legacy_key_maps_to_one_chunk_id_except_same_line_twins() {
    // `fn a() {}` twice on one line: two chunks with one hash and one start
    // line, which today's fusion merges into one entry (decision N-5).
    let source = format!("{FOUR_FUNCTIONS}fn a() {{}} fn a() {{}}\n");
    let pairs = ids_and_keys("src/lib.rs", &source);
    let mut by_key = HashMap::<(String, u32, String), BTreeSet<NodeId>>::new();
    for (id, key) in &pairs {
        by_key.entry(key.clone()).or_default().insert(id.clone());
    }
    let mut twins: Vec<_> = by_key
        .iter()
        .filter(|(_, ids)| ids.len() != 1)
        .map(|((path, line, _), ids)| (path.as_str(), *line, ids.len()))
        .collect();
    twins.sort();
    assert_eq!(twins, [("src/lib.rs", 16, 2)]);
    let distinct: HashSet<&NodeId> = pairs.iter().map(|(id, _)| id).collect();
    assert_eq!(
        distinct.len(),
        pairs.len(),
        "NodeIds are unique per snapshot"
    );
}

// Scan-level rows: determinism, incremental equals full, locality.

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn scan_ids(scan: &chunk::Scan) -> BTreeSet<NodeId> {
    let ids = manifest_chunk_ids(scan.files.values());
    let set: BTreeSet<NodeId> = ids.iter().cloned().collect();
    assert_eq!(set.len(), ids.len(), "NodeIds are unique per snapshot");
    // The scan's flat, sorted chunk list gives the same ids.
    assert_eq!(
        chunk_ids(&scan.chunks).into_iter().collect::<BTreeSet<_>>(),
        set
    );
    set
}

fn full(root: &Path) -> BTreeSet<NodeId> {
    scan_ids(&chunk::scan_incremental(root, &HashMap::new()).unwrap())
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.rs", FOUR_FUNCTIONS);
    write(dir.path(), "src/b.rs", "fn b() -> u32 {\n    2\n}\n");
    write(
        dir.path(),
        "lib/c.py",
        "def c():\n    return 3\n\n\ndef d():\n    return 4\n",
    );
    write(
        dir.path(),
        "web/d.ts",
        "export function d(): number {\n  return 4;\n}\n",
    );
    dir
}

#[test]
fn the_same_tree_gives_the_same_ids_from_an_empty_or_a_full_previous_manifest() {
    let dir = fixture();
    let first = chunk::scan_incremental(dir.path(), &HashMap::new()).unwrap();
    let again = chunk::scan_incremental(dir.path(), &first.files).unwrap();
    assert_eq!(again.parsed_files, 0, "everything reused");
    assert_eq!(scan_ids(&first), scan_ids(&again));
    assert_eq!(scan_ids(&first), full(dir.path()));
}

#[test]
fn incremental_scans_give_the_ids_of_a_full_scan_through_an_edit_script() {
    let dir = fixture();
    let root = dir.path();
    let mut files = chunk::scan_incremental(root, &HashMap::new())
        .unwrap()
        .files;
    type Step = Box<dyn Fn(&Path, &mut HashMap<String, CachedFile>)>;
    let steps: Vec<(&str, Step)> = vec![
        (
            "edit inside a chunk",
            Box::new(|root, _| write(root, "src/b.rs", "fn b() -> u32 {\n    22\n}\n")),
        ),
        (
            "insert above",
            Box::new(|root, _| {
                write(root, "src/a.rs", &format!("\n\n\n{FOUR_FUNCTIONS}"));
            }),
        ),
        (
            "rename",
            Box::new(|root, _| {
                std::fs::rename(root.join("lib/c.py"), root.join("lib/renamed.py")).unwrap();
            }),
        ),
        (
            "duplicate chunk",
            Box::new(|root, _| {
                write(
                    root,
                    "src/a.rs",
                    &format!("fn two() -> u32 {{\n    2\n}}\n\n\n\n{FOUR_FUNCTIONS}"),
                );
            }),
        ),
        (
            "adapter version bump",
            Box::new(|_, files| {
                // As if the previous manifest came from an older adapter.
                files.get_mut("web/d.ts").unwrap().adapter_version = "typescript-old".into();
            }),
        ),
        (
            "delete",
            Box::new(|root, _| std::fs::remove_file(root.join("src/b.rs")).unwrap()),
        ),
    ];
    for (name, step) in steps {
        step(root, &mut files);
        let incremental = chunk::scan_incremental(root, &files).unwrap();
        assert!(
            incremental.parsed_files <= 1,
            "{name}: only the touched file"
        );
        assert_eq!(scan_ids(&incremental), full(root), "{name}");
        files = incremental.files;
    }
}

#[test]
fn an_edit_changes_only_ids_that_name_the_edited_file() {
    let dir = fixture();
    let before = full(dir.path());
    write(
        dir.path(),
        "src/a.rs",
        &FOUR_FUNCTIONS.replace("    3\n", "    33\n"),
    );
    let after = full(dir.path());
    let changed: BTreeSet<&str> = before
        .symmetric_difference(&after)
        .map(|id| id.path().unwrap())
        .collect();
    assert_eq!(changed, BTreeSet::from(["src/a.rs"]));
    assert_eq!(
        before.symmetric_difference(&after).count(),
        2,
        "one chunk out, one in"
    );
}

#[test]
fn an_impl_header_is_the_same_whatever_its_whitespace_and_comments() {
    let header = |source: &str| {
        let declarations = rust_declarations(source).unwrap();
        let method = declarations
            .iter()
            .find(|d| d.kind == SymbolKind::Method)
            .unwrap();
        let id = method.node_id("src/x.rs");
        assert_eq!(id.to_string().parse::<NodeId>().unwrap(), id);
        id.validate().unwrap();
        method.container[0].clone()
    };
    let canonical = "impl<T, U> Foo<T, U> for Bar<'a> where T: Clone + Send";
    for source in [
        "impl<T, U> Foo<T, U> for Bar<'a> where T: Clone + Send { fn f() {} }",
        "impl<T,U>Foo<T,U>for Bar<'a>where T:Clone+Send{fn f(){}}",
        "impl < T , U >\n    Foo<T, U> /* note */ for Bar<'a>\nwhere\n    T: Clone + Send,\n{ fn f() {} }"
            .replace(",\n{", "\n{")
            .as_str(),
    ] {
        assert_eq!(header(source), canonical, "{source}");
    }
    // A header written with no space at all still gets one after `impl`.
    assert_eq!(header("impl<T>Foo<T>{fn f(){}}"), "impl<T> Foo<T>");
    assert_eq!(header("impl!Send for X{fn f(){}}"), "impl !Send for X");
    assert_eq!(
        header("impl Tr for &'a mut [u8]{fn f(){}}"),
        "impl Tr for &'a mut [u8]"
    );
}

#[test]
fn ord_agrees_with_eq_even_for_invalid_ids() {
    let none = NodeId::DocSection {
        path: "a.md".into(),
        heading_path: vec![],
        ordinal: 0,
    };
    let empty = NodeId::DocSection {
        path: "a.md".into(),
        heading_path: vec![String::new()],
        ordinal: 0,
    };
    assert_eq!(none.to_string(), empty.to_string());
    assert_ne!(none, empty);
    assert_ne!(none.cmp(&empty), std::cmp::Ordering::Equal);
    assert_eq!(BTreeSet::from([none, empty]).len(), 2);
}

#[test]
fn every_symbol_kind_name_round_trips_and_the_outline_kinds_parse() {
    let kinds = [
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Struct,
        SymbolKind::Enum,
        SymbolKind::Union,
        SymbolKind::Trait,
        SymbolKind::Const,
        SymbolKind::Static,
        SymbolKind::TypeAlias,
        SymbolKind::Module,
        SymbolKind::Macro,
        SymbolKind::Field,
        SymbolKind::Variable,
        SymbolKind::Interface,
        SymbolKind::Class,
    ];
    for kind in kinds {
        let id = NodeId::Symbol(SymbolKey {
            language: "rust".into(),
            module_path: vec!["a".into()],
            container: Vec::new(),
            name: "x".into(),
            kind,
            disambiguator: 0,
        });
        let parsed: NodeId = id.to_string().parse().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed, id, "{id}");
    }
    for (text, kind) in [
        ("sym:csharp:Shapes.cs::Circle/class", SymbolKind::Class),
        (
            "sym:csharp:Shapes.cs::IShape/interface",
            SymbolKind::Interface,
        ),
        ("sym:typescript:a::x/variable", SymbolKind::Variable),
        ("sym:rust:a::f/field", SymbolKind::Field),
    ] {
        match text.parse::<NodeId>() {
            Ok(NodeId::Symbol(key)) => assert_eq!(key.kind, kind, "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
    assert!(
        "sym:csharp:Shapes.cs::P/property"
            .parse::<NodeId>()
            .is_err()
    );
}
