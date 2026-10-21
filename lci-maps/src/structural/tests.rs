use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

/// A bound no loaded machine reaches (see `live_grep`'s tests).
const PATIENT: Duration = Duration::from_secs(3600);

fn tree(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let full: PathBuf = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    dir
}

fn paths(files: &[&str]) -> Vec<String> {
    files.iter().map(|file| (*file).to_owned()).collect()
}

fn scan(dir: &TempDir, files: &[&str], name: &str, language: Option<&str>) -> Scan {
    scan_with(dir, files, name, language, &StructuralOptions::default())
}

fn scan_with(
    dir: &TempDir,
    files: &[&str],
    name: &str,
    language: Option<&str>,
    options: &StructuralOptions,
) -> Scan {
    scan_files(
        dir.path(),
        &paths(files),
        name,
        language,
        options,
        Instant::now() + PATIENT,
    )
}

/// (role, line, other) of every hit, in order.
fn rows(scan: &Scan) -> Vec<(Role, u32, Option<&str>)> {
    scan.hits
        .iter()
        .map(|hit| (hit.role, hit.line, hit.other.as_deref()))
        .collect()
}

#[test]
fn every_pattern_of_the_table_compiles_against_its_grammar() {
    for identifier in LANGUAGES {
        let lang = Lang::for_identifier(identifier)
            .unwrap_or_else(|| panic!("no grammar for the table's language {identifier}"));
        let set = Compiled::build(&lang, "Sample");
        assert!(
            set.dropped.is_empty(),
            "{identifier}: dropped {:?}",
            set.dropped
        );
        for role in [Role::Header, Role::Call] {
            let table = patterns(identifier, role);
            let built = match role {
                Role::Header => set.header.len(),
                Role::Call => set.call.len(),
            };
            assert_eq!(built, table.len(), "{identifier} {role:?}");
        }
    }
    // Every language of the table is one the chunker has a grammar for.
    assert_eq!(LANGUAGES.len(), 11);
}

#[test]
fn a_template_that_does_not_compile_is_an_error_and_a_clean_one_is_not() {
    let rust = Lang::for_identifier("rust").unwrap();
    let impl_for = &patterns("rust", Role::Header)[0];
    assert!(compile(&rust, impl_for, "Shape").is_ok());
    // Two statements are not one pattern.
    let two = Template {
        text: "fn a() {} fn b() {}",
        selector: None,
    };
    assert!(compile(&rust, &two, "x").is_err());
    assert_eq!(
        instantiate("impl NAME for $T { $$$ }", "Shape"),
        "impl Shape for $T { $$$ }"
    );
}

#[test]
fn scan_finds_the_implementing_types_of_a_trait_and_not_other_traits() {
    let dir = tree(&[(
        "src/shapes.rs",
        "trait Shape {\n    fn area(&self) -> f64;\n}\nstruct Circle;\nstruct Wrapper<T>(T);\n\
         impl Shape for Circle {\n    fn area(&self) -> f64 { 1.0 }\n}\n\
         impl<T> Shape for Wrapper<T> {\n    fn area(&self) -> f64 { 2.0 }\n}\n\
         impl Other for Circle {\n    fn other(&self) {}\n}\n",
    )]);
    let found = scan(&dir, &["src/shapes.rs"], "Shape", Some("rust"));
    assert_eq!(
        rows(&found),
        [
            (Role::Header, 5, Some("Circle")),
            (Role::Header, 8, Some("Wrapper")),
        ]
    );
    assert_eq!(found.candidates, 1);
    assert_eq!(found.files_parsed, found.candidates);
    assert!(found.dropped.is_empty());
    // The seed may be the type: the other party is the trait.
    let found = scan(&dir, &["src/shapes.rs"], "Circle", Some("rust"));
    assert_eq!(
        rows(&found),
        [
            (Role::Header, 5, Some("Shape")),
            (Role::Header, 11, Some("Other")),
        ]
    );
}

#[test]
fn scan_finds_call_sites_and_not_comments_strings_or_definitions() {
    let dir = tree(&[(
        "src/calls.rs",
        "// draw() is called below\n\
         fn quiet() {\n    let text = \"draw()\";\n}\n\
         fn draw() {}\n\
         fn loud(shape: Shape) {\n    shape.draw();\n    draw();\n    self::draw();\n}\n",
    )]);
    let found = scan(&dir, &["src/calls.rs"], "draw", Some("rust"));
    assert_eq!(
        rows(&found),
        [
            (Role::Call, 6, None),
            (Role::Call, 7, None),
            (Role::Call, 8, None),
        ]
    );
    // None of the matches is the comment (line 0), the string (2) or the
    // definition (4).
    assert!(found.hits.iter().all(|hit| ![0, 2, 4].contains(&hit.line)));
}

#[test]
fn typescript_extends_and_python_bases_match_with_the_expando_char() {
    let dir = tree(&[
        (
            "web/a.ts",
            "const $el = document.body;\nclass Base {}\nexport class A extends Base {\n  run() { return $el; }\n}\n\
             class B implements Base {}\n",
        ),
        (
            "py/a.py",
            "class Base:\n    pass\n\nclass A(Base):\n    pass\n\nclass C(Mixin, Base):\n    pass\n",
        ),
    ]);
    let ts = scan(&dir, &["web/a.ts"], "Base", Some("typescript"));
    assert_eq!(
        rows(&ts),
        [(Role::Header, 2, Some("A")), (Role::Header, 5, Some("B")),]
    );
    let py = scan(&dir, &["py/a.py"], "Base", Some("python"));
    assert_eq!(
        rows(&py),
        [(Role::Header, 3, Some("A")), (Role::Header, 6, Some("C")),]
    );
    // A call in plain JavaScript, with a `$`-bearing identifier beside it.
    let js_dir = tree(&[(
        "a.js",
        "const $el = 1;\nfunction f() { return g($el); }\nconst h = () => g(2);\n",
    )]);
    let js = scan(&js_dir, &["a.js"], "g", Some("javascript"));
    assert_eq!(rows(&js), [(Role::Call, 1, None), (Role::Call, 2, None)]);
}

#[test]
fn a_file_the_candidates_do_not_name_is_never_read() {
    let dir = tree(&[
        ("a.rs", "fn a() { draw(); }\n"),
        ("b.rs", "fn b() { draw(); }\n"),
    ]);
    let found = scan(&dir, &["a.rs"], "draw", None);
    assert_eq!((found.candidates, found.files_parsed), (1, 1));
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].path, "a.rs");
    // A listed file that does not exist is passed over, not counted as parsed.
    let found = scan(&dir, &["gone.rs", "b.rs"], "draw", None);
    assert_eq!((found.candidates, found.files_parsed), (2, 1));
}

#[test]
fn the_candidate_list_is_cut_at_max_candidates_in_order() {
    let dir = tree(&[
        ("a.rs", "fn a() { draw(); }\n"),
        ("b.rs", "fn b() { draw(); }\n"),
        ("c.rs", "fn c() { draw(); }\n"),
    ]);
    let options = StructuralOptions {
        max_candidates: 2,
        ..StructuralOptions::default()
    };
    let found = scan_with(&dir, &["a.rs", "b.rs", "c.rs"], "draw", None, &options);
    assert_eq!((found.candidates, found.files_parsed), (2, 2));
    let hit_paths: Vec<_> = found.hits.iter().map(|hit| hit.path.as_str()).collect();
    assert_eq!(hit_paths, ["a.rs", "b.rs"]);
}

#[test]
fn a_file_over_max_bytes_is_skipped_and_counted() {
    let big = format!("fn big() {{ draw(); }}\n{}", "// padding\n".repeat(100));
    let dir = tree(&[("big.rs", &big), ("small.rs", "fn s() { draw(); }\n")]);
    let options = StructuralOptions {
        max_file_bytes: 200,
        ..StructuralOptions::default()
    };
    let found = scan_with(&dir, &["big.rs", "small.rs"], "draw", None, &options);
    assert_eq!(found.skipped_large, 1);
    assert_eq!(found.files_parsed, 1);
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].path, "small.rs");
}

#[test]
fn the_deadline_stops_the_scan_with_a_partial_status() {
    let dir = tree(&[("a.rs", "fn a() { draw(); }\n")]);
    let found = scan_files(
        dir.path(),
        &paths(&["a.rs"]),
        "draw",
        None,
        &StructuralOptions::default(),
        Instant::now() - Duration::from_secs(1),
    );
    assert_eq!(found.stopped, Some("deadline"));
    assert_eq!((found.files_parsed, found.hits.len()), (0, 0));
}

#[test]
fn the_hit_cap_stops_the_scan() {
    let dir = tree(&[(
        "a.rs",
        "fn a() {\n    draw();\n    draw();\n    draw();\n}\n",
    )]);
    let options = StructuralOptions {
        cap: 2,
        ..StructuralOptions::default()
    };
    let found = scan_with(&dir, &["a.rs"], "draw", None, &options);
    assert_eq!(found.hits.len(), 2);
    assert_eq!(found.stopped, Some("match cap"));
}

#[test]
fn a_name_that_is_not_an_identifier_makes_no_scan() {
    let dir = tree(&[("a.rs", "fn a() {}\n")]);
    for name in ["", "a b", "a::b", "$el"] {
        let found = scan(&dir, &["a.rs"], name, None);
        assert_eq!((found.files_parsed, found.hits.len()), (0, 0), "{name:?}");
    }
}

#[test]
fn scan_gives_call_hits_only_for_a_language_without_header_patterns() {
    let dir = tree(&[(
        "main.go",
        "package main\n\ntype Shape interface {\n\tDraw()\n}\n\ntype Circle struct{}\n\nfunc (c Circle) Draw() {}\n\n\
         func run(s Shape) {\n\ts.Draw()\n\tDraw()\n}\n",
    )]);
    assert!(patterns("go", Role::Header).is_empty());
    let found = scan(&dir, &["main.go"], "Draw", Some("go"));
    assert!(!found.hits.is_empty());
    assert!(found.hits.iter().all(|hit| hit.role == Role::Call));
    assert_eq!(
        found.hits.iter().map(|hit| hit.line).collect::<Vec<_>>(),
        [11, 12]
    );
}

#[test]
fn bare_name_keeps_the_last_identifier_of_a_written_type() {
    for (text, expected) in [
        ("Circle", Some("Circle")),
        ("Wrapper<T>", Some("Wrapper")),
        ("a::b::Wrapper<T, U>", Some("Wrapper")),
        ("&mut Wrapper<T>", Some("Wrapper")),
        ("pkg.Base", Some("Base")),
        ("", None),
        ("<T>", None),
    ] {
        assert_eq!(bare_name(text).as_deref(), expected, "{text:?}");
    }
}

#[test]
fn extensions_of_a_language_come_from_the_adapter_table() {
    assert_eq!(extensions_of("rust"), ["rs"]);
    assert!(extensions_of("cpp").contains(&"cpp") && extensions_of("cpp").contains(&"hpp"));
    assert!(extensions_of("markdown").is_empty());
    assert!(all_extensions().contains(&"py"));
}

#[tokio::test]
async fn the_listed_double_names_its_files_whatever_the_name() {
    let listed = Listed(paths(&["a.rs", "b.rs"]));
    let files = listed
        .files_with_word(Path::new("."), "anything", &["rs"])
        .await
        .unwrap();
    assert_eq!(files, ["a.rs", "b.rs"]);
}

#[test]
fn go_calls_match_whatever_the_arguments() {
    let dir = tree(&[(
        "main.go",
        "package main\n\nfunc run() {\n\ts.Draw()\n\ts.Draw(x)\n\ts.Draw(1)\n\ts.Draw(f(x))\n\ts.Draw(x, y)\n\tDraw(x)\n\t// s.Draw(x)\n}\n",
    )]);
    let found = scan(&dir, &["main.go"], "Draw", Some("go"));
    assert_eq!(
        found.hits.iter().map(|hit| hit.line).collect::<Vec<_>>(),
        [3, 4, 5, 6, 7, 8]
    );
    assert!(found.dropped.is_empty());
}

#[test]
fn java_csharp_and_cpp_headers_name_the_other_party() {
    let dir = tree(&[
        (
            "A.java",
            "class A extends Base {\n}\nclass B implements Base {\n}\nclass C {\n}\n",
        ),
        ("A.cs", "class A : Base\n{\n}\nclass D\n{\n}\n"),
        (
            "a.cpp",
            "class A : public Base { int x; };\nstruct B : Base {};\nclass Other {};\n",
        ),
    ]);
    let java = scan(&dir, &["A.java"], "Base", Some("java"));
    assert_eq!(
        rows(&java),
        [(Role::Header, 0, Some("A")), (Role::Header, 2, Some("B"))]
    );
    let csharp = scan(&dir, &["A.cs"], "Base", Some("csharp"));
    assert_eq!(rows(&csharp), [(Role::Header, 0, Some("A"))]);
    // The base clause cannot hold its class: the class around it names the
    // other party.
    let cpp = scan(&dir, &["a.cpp"], "Base", Some("cpp"));
    assert_eq!(
        rows(&cpp),
        [(Role::Header, 0, Some("A")), (Role::Header, 1, Some("B"))]
    );
    assert!(java.dropped.is_empty() && csharp.dropped.is_empty() && cpp.dropped.is_empty());
}

#[test]
fn c_calls_and_javascript_classes_match() {
    let dir = tree(&[
        (
            "a.c",
            "void f(void) {\n    draw(1);\n    /* draw(2); */\n}\n",
        ),
        ("a.js", "class A extends Base {}\nclass B {}\n"),
    ]);
    let c = scan(&dir, &["a.c"], "draw", Some("c"));
    assert_eq!(rows(&c), [(Role::Call, 1, None)]);
    let js = scan(&dir, &["a.js"], "Base", Some("javascript"));
    assert_eq!(rows(&js), [(Role::Header, 0, Some("A"))]);
}

#[test]
fn a_hit_carries_the_live_blob_of_its_file() {
    let dir = tree(&[
        ("a.rs", "fn a() { draw(); }\n"),
        ("b.rs", "fn b() { draw(); draw(); }\n"),
    ]);
    let found = scan(&dir, &["a.rs", "b.rs"], "draw", None);
    assert_eq!(found.hits.len(), 3);
    let blobs: Vec<&str> = found.hits.iter().map(|hit| hit.blob.as_str()).collect();
    assert!(
        blobs
            .iter()
            .all(|blob| blob.starts_with("live:") && blob.len() == 5 + 32)
    );
    // One blob per file's bytes: the two hits of `b.rs` share it, `a.rs` has its own.
    assert_eq!(blobs[1], blobs[2]);
    assert_ne!(blobs[0], blobs[1]);
}
