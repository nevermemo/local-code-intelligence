//! The extraction hook ([`extract_with_adapter`]) must not change today's
//! chunk output: on the fixture trees, its chunks are byte-identical to
//! [`chunks_with_adapter`]'s, in the same order, and its declarations equal
//! the S1 extractor's own parse. Since merge condition M2 both are one
//! function (`extract`), so the chunk comparison is with itself; the record
//! of the every-language tree (`golden/every_language_chunks.json`) is what
//! pins the chunk output.

use super::*;
use crate::node::rust_declarations;

/// One small file per registered language, with the constructs the
/// chunkers treat specially (containers, prefixes, nested declarations).
fn write_multilanguage_tree(root: &Path) {
    let files = [
        (
            "src/lib.rs",
            "//! Crate docs.\n\nuse std::fmt;\n\n/// A point.\n#[derive(Debug)]\npub struct Point { x: i32 }\n\nimpl fmt::Display for Point {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n        write!(f, \"{}\", self.x)\n    }\n}\n\nmod inner {\n    pub fn helper() -> u8 { 1 }\n}\n\npub trait Shape { fn area(&self) -> f64; }\n\nmacro_rules! twice { ($e:expr) => { $e * 2 }; }\n",
        ),
        (
            "web/app.ts",
            "// Header comment\nexport interface User { name: string }\n\nexport class Store {\n  private users: User[] = [];\n  add(user: User): void { this.users.push(user); }\n}\n\nexport function greet(user: User): string {\n  return `hi ${user.name}`;\n}\n",
        ),
        (
            "web/view.tsx",
            "export function View(props: { name: string }) {\n  return <div>{props.name}</div>;\n}\n",
        ),
        (
            "web/util.js",
            "function add(a, b) { return a + b; }\nmodule.exports = { add };\n",
        ),
        (
            "web/button.jsx",
            "export const Button = () => <button>ok</button>;\n",
        ),
        (
            "tools/run.py",
            "\"\"\"Module docs.\"\"\"\nimport os\n\n\nclass Runner:\n    \"\"\"Runs things.\"\"\"\n\n    def run(self, name):\n        return os.path.join(name, 'x')\n\n\n@staticmethod\ndef main():\n    Runner().run('a')\n",
        ),
        (
            "svc/Calc.cs",
            "namespace Acceptance;\n// A calculator.\npublic sealed class Calculator {\n    public int Add(int a, int b) => a + b;\n    public int Sub(int a, int b) => a - b;\n}\n",
        ),
        (
            "svc/main.go",
            "package main\n\nimport \"fmt\"\n\n// Greeter greets.\ntype Greeter struct{ Name string }\n\nfunc (g Greeter) Greet() string { return fmt.Sprintf(\"hi %s\", g.Name) }\n\nfunc main() { fmt.Println(Greeter{\"a\"}.Greet()) }\n",
        ),
        (
            "svc/App.java",
            "package demo;\n\n/** App. */\npublic class App {\n    @Override\n    public String toString() { return \"app\"; }\n    static int twice(int x) { return 2 * x; }\n}\n",
        ),
        (
            "native/math.c",
            "#include <stdio.h>\n\n/* adds */\nint add(int a, int b) { return a + b; }\n\nstruct pair { int a; int b; };\n",
        ),
        (
            "native/shape.cpp",
            "#include <string>\nnamespace geo {\nclass Shape {\npublic:\n  virtual double area() const = 0;\n};\ndouble twice(double x) { return 2 * x; }\n}\n",
        ),
        ("native/shape.hpp", "#pragma once\nint area();\n"),
        (
            "native/math.h",
            "#ifndef MATH_H\n#define MATH_H\n\n/* adds */\nint add(int a, int b);\n\nstruct pair { int a; int b; };\n\n#endif\n",
        ),
        (
            "native/shape.cc",
            "namespace geo {\n// Perimeter of a square.\ndouble perimeter(double side) { return 4 * side; }\n}\n",
        ),
        (
            "native/shape.cxx",
            "namespace geo {\nclass Circle {\npublic:\n  double radius() const { return 1.0; }\n};\n}\n",
        ),
        (
            "native/shape.hh",
            "#pragma once\ndouble perimeter(double side);\n",
        ),
        (
            "native/shape.hxx",
            "#pragma once\nnamespace geo {\nstruct Square { double side; };\n}\n",
        ),
    ];
    for (name, content) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

/// Compares the hook with today's chunker on every source file under
/// `root`; returns how many files and chunks were compared. With
/// `compare_chunker` false only the hook runs (and its declarations are
/// checked): the two are one function, so a second, equally expensive pass
/// over a large tree would compare it with itself.
fn assert_hook_matches_chunker(root: &Path, compare_chunker: bool) -> (usize, usize) {
    let mut files = 0;
    let mut chunks = 0;
    for file in sources(root).unwrap().files {
        let path = &file.relative_path;
        let hooked = extract_with_adapter(path, &file.source, file.adapter).unwrap();
        if compare_chunker {
            let today = chunks_with_adapter(path, &file.source, file.adapter).unwrap();
            // JSON text: byte identity of every field and of the order, with a
            // readable difference on failure.
            assert_eq!(
                serde_json::to_string(&hooked.chunks).unwrap(),
                serde_json::to_string(&today).unwrap(),
                "{path}: the hook's chunks differ from chunks_with_adapter's"
            );
        }
        let declarations =
            (file.adapter.identifier() == "rust").then(|| rust_declarations(&file.source).unwrap());
        assert_eq!(
            hooked.declarations, declarations,
            "{path}: the hook's declarations differ from the S1 extractor's"
        );
        files += 1;
        chunks += hooked.chunks.len();
    }
    (files, chunks)
}

#[test]
fn hook_chunks_are_byte_identical_on_a_tree_of_every_language() {
    let temp = tempfile::tempdir().unwrap();
    write_multilanguage_tree(temp.path());
    let (files, chunks) = assert_hook_matches_chunker(temp.path(), true);
    // One file per registered adapter, and every adapter is some file's:
    // a language added later without a file here fails this test.
    assert_eq!(files, language::ADAPTERS.len());
    // (`ADAPTERS` is a `const`, so adapters are compared by identity
    // fields, not by address.)
    let identity =
        |adapter: &language::LanguageAdapter| (adapter.identifier(), adapter.cache_version());
    let mut registered: Vec<_> = language::ADAPTERS.iter().map(identity).collect();
    let mut walked: Vec<_> = sources(temp.path())
        .unwrap()
        .files
        .iter()
        .map(|file| identity(file.adapter))
        .collect();
    registered.sort();
    walked.sort();
    assert_eq!(
        walked, registered,
        "one fixture file per registered adapter"
    );
    assert!(chunks >= 24, "{chunks} chunks");
    let declared = extract_for_path(
        "src/lib.rs",
        &std::fs::read_to_string(temp.path().join("src/lib.rs")).unwrap(),
    )
    .unwrap()
    .declarations
    .unwrap();
    assert!(
        declared
            .iter()
            .any(|d| d.name == "fmt" && d.container == ["impl fmt::Display for Point"])
    );
    let python = extract_for_path("tools/run.py", "def f():\n    pass\n").unwrap();
    assert!(python.declarations.is_none());
}

#[test]
fn hook_chunks_are_byte_identical_on_this_repository() {
    // The workspace root (lci-core's parent): every tracked Rust and Python
    // file of this repository, walked as a scan walks it. The chunker runs
    // the test-only uncovered-bytes check, so a new file that exposes a
    // chunker coverage gap fails here with "uncovered byte" even though the
    // hook and the chunker still agree: fix the chunker, not this test. Only
    // the hook runs here: the chunker is the same function, and the fixture
    // tree compares the two.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let (files, chunks) = assert_hook_matches_chunker(root, false);
    assert!(files >= 100, "{files} files");
    assert!(chunks >= 2000, "{chunks} chunks");
}

#[test]
fn hook_keeps_the_scan_order_of_chunks() {
    // A scan concatenates per-file chunk lists in path order and sorts by
    // (path, start_line); the hook's lists, concatenated the same way, give
    // the same sequence.
    let temp = tempfile::tempdir().unwrap();
    write_multilanguage_tree(temp.path());
    let scanned = scan(temp.path()).unwrap();
    assert!(!scanned.is_empty());
    let mut hooked: Vec<Chunk> = Vec::new();
    for file in sources(temp.path()).unwrap().files {
        hooked.extend(
            extract_with_adapter(&file.relative_path, &file.source, file.adapter)
                .unwrap()
                .chunks,
        );
    }
    hooked.sort_by(|a, b| {
        (&a.relative_file_path, a.start_line).cmp(&(&b.relative_file_path, b.start_line))
    });
    assert_eq!(
        serde_json::to_string(&hooked).unwrap(),
        serde_json::to_string(&scanned).unwrap()
    );
}

/// The chunks of the every-language tree, recorded from `chunks_with_adapter`
/// while it still had its own body (merge condition M2). Once the chunker is
/// the hook, comparing the two compares a function with itself; this record
/// is what keeps today's chunk output pinned. Change it only with an
/// intended chunking change, and say so in the commit.
const EVERY_LANGUAGE_CHUNKS: &str = include_str!("golden/every_language_chunks.json");

#[test]
fn chunks_of_a_tree_of_every_language_match_the_record() {
    let temp = tempfile::tempdir().unwrap();
    write_multilanguage_tree(temp.path());
    let mut today = Vec::new();
    let mut hooked = Vec::new();
    for file in sources(temp.path()).unwrap().files {
        let path = &file.relative_path;
        today.extend(chunks_with_adapter(path, &file.source, file.adapter).unwrap());
        hooked.extend(
            extract_with_adapter(path, &file.source, file.adapter)
                .unwrap()
                .chunks,
        );
    }
    let recorded: Vec<Chunk> = serde_json::from_str(EVERY_LANGUAGE_CHUNKS).unwrap();
    assert!(recorded.len() >= 24, "{} recorded chunks", recorded.len());
    // Pretty JSON, one field per line: byte identity of every field and of
    // the order, with a readable difference on failure.
    let text = |chunks: &[Chunk]| serde_json::to_string_pretty(chunks).unwrap();
    assert_eq!(text(&today), text(&recorded), "chunks_with_adapter");
    assert_eq!(text(&hooked), text(&recorded), "extract_with_adapter");
}
