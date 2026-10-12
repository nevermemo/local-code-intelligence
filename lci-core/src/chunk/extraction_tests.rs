//! The extraction hook ([`extract_with_adapter`]) must not change today's
//! chunk output: on the fixture trees, its chunks are byte-identical to
//! [`chunks_with_adapter`]'s, in the same order, and its declarations equal
//! the S1 extractor's own parse.

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
    ];
    for (name, content) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

/// Compares the hook with today's chunker on every source file under
/// `root`; returns how many files and chunks were compared.
fn assert_hook_matches_chunker(root: &Path) -> (usize, usize) {
    let mut files = 0;
    let mut chunks = 0;
    for file in sources(root).unwrap().files {
        let path = &file.relative_path;
        let today = chunks_with_adapter(path, &file.source, file.adapter).unwrap();
        let hooked = extract_with_adapter(path, &file.source, file.adapter).unwrap();
        assert_eq!(
            serde_json::to_vec(&hooked.chunks).unwrap(),
            serde_json::to_vec(&today).unwrap(),
            "{path}: the hook's chunks differ from chunks_with_adapter's"
        );
        let declarations =
            (file.adapter.identifier() == "rust").then(|| rust_declarations(&file.source).unwrap());
        assert_eq!(
            hooked.declarations, declarations,
            "{path}: the hook's declarations differ from the S1 extractor's"
        );
        files += 1;
        chunks += today.len();
    }
    (files, chunks)
}

#[test]
fn hook_chunks_are_byte_identical_on_a_tree_of_every_language() {
    let temp = tempfile::tempdir().unwrap();
    write_multilanguage_tree(temp.path());
    let (files, chunks) = assert_hook_matches_chunker(temp.path());
    // Every registered extension is in the tree, so a language the walker
    // skipped would show up here.
    assert_eq!(files, 12);
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
    // file of this repository, walked as a scan walks it.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let (files, chunks) = assert_hook_matches_chunker(root);
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
        serde_json::to_vec(&hooked).unwrap(),
        serde_json::to_vec(&scanned).unwrap()
    );
}
