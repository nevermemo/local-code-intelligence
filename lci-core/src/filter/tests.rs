use super::source_role::normalize_path;
use super::*;
use std::mem::discriminant;
use std::str::FromStr;

// -- SourceRole parsing / display ------------------------------------

#[test]
fn source_role_round_trips_all_variants() {
    for role in SourceRole::ALL {
        let name = role.as_str();
        let parsed = SourceRole::from_str(name).unwrap();
        assert_eq!(parsed, role);
        assert_eq!(parsed.to_string(), name);
    }
}

#[test]
fn source_role_unknown_errors_clearly() {
    let err = SourceRole::from_str("bogus").unwrap_err();
    assert!(matches!(err, SourceRoleError::Unknown { ref value } if value == "bogus"));
    assert!(err.to_string().contains("bogus"));
}

// -- classify_source_role ---------------------------------------------

/// Every path is named in the failure message. Groups are by expected role;
/// the comment on each group says what the paths show.
#[test]
fn classify_source_role_by_path() {
    let cases: &[(SourceRole, &[&str])] = &[
        (
            SourceRole::Generated,
            &[
                // directory
                "generated/foo.rs",
                "gen/bar.py",
                // file-name markers
                "src/foo.generated.rs",
                "src/foo_generated.py",
                "src/foo.g.rs",
                "src/Telemetry.generated.cs",
                "src/schema.g.h",
                "src/schema.generated.cc",
                "src/schema_generated.hh",
                // generated beats test, and test beats benchmark below
                "generated/foo_test.py",
                "generated/tests.rs",
            ],
        ),
        (
            SourceRole::Test,
            &[
                // directory
                "tests/foo.rs",
                "test/bar.py",
                "__tests__/baz.js",
                // file name
                "src/foo_test.py",
                "src/test_foo.py",
                "src/foo.test.ts",
                "src/foo.spec.tsx",
                "src/foo.test.js",
                "src/foo.spec.jsx",
                "src/TelemetryTests.cs",
                "src/TelemetryTest.cs",
                // test beats benchmark
                "tests/foo_bench.rs",
                "benches/tests.rs",
                // colocated Go, Java, C and C++ tests
                "settings/loader_test.go",
                "src/main/java/com/acme/ConfigLoaderTest.java",
                "src/main/java/com/acme/ConfigLoaderTests.java",
                "src/main/java/com/acme/ConfigLoaderTestCase.java",
                "src/main/java/com/acme/ConfigLoaderIT.java",
                "src/main/java/com/acme/Parser2IT.java",
                "native/config_test.c",
                "native/config_tests.c",
                "native/test_config.c",
                "cpp/config_unittest.cpp",
                "cpp/config_test.cpp",
                "cpp/test_config.hpp",
                // the other registered C and C++ extensions
                "native/config_test.h",
                "native/test_config.h",
                "cpp/config_test.cc",
                "cpp/config_unittest.cc",
                "cpp/config_tests.cxx",
                "cpp/test_config.cxx",
                "cpp/config_test.hh",
                "cpp/test_config.hxx",
                // Rust test files by name
                "tests.rs",
                "lci-core/src/filter/tests.rs",
                "src/evaluate/tests.rs",
                "src/test_config.rs",
                "src/config_test.rs",
                "src/config_tests.rs",
                "lci-core/src/app/search_tests.rs",
                // a bare test.rs is source, but a test directory still counts
                "tests/test.rs",
                "src/test/test.rs",
                // Rust tests directories at any depth
                "tests/it.rs",
                "xtask/tests/cli.rs",
                "crates/foo/tests/common/mod.rs",
                "crates/foo/src/store/tests/basic.rs",
                // Gradle and Android test source sets
                "app/src/androidTest/java/com/acme/LoaderInstrumented.java",
                "src/integrationTest/java/com/acme/LoaderFlow.java",
                "src/testFixtures/java/com/acme/Fakes.java",
                // layout directories above the JVM source root
                "src/test/java/com/example/ConfigLoaderSpec.java",
            ],
        ),
        (
            SourceRole::Benchmark,
            &[
                "benches/foo.rs",
                "bench/bar.py",
                "benchmark/baz.rs",
                "benchmarks/qux.rs",
                "src/foo_bench.rs",
                "src/bench_foo.py",
                "src/foo.bench.ts",
                "benches/search.rs",
                "crates/foo/benches/search.rs",
            ],
        ),
        (
            SourceRole::Example,
            &[
                "examples/foo.rs",
                "example/bar.py",
                "samples/baz.js",
                "examples/demo.rs",
                "crates/foo/examples/demo.rs",
                "examples/src/main/java/com/acme/Demo.java",
            ],
        ),
        (
            SourceRole::Documentation,
            &["docs/guide.md", "doc/api.md", "documentation/overview.md"],
        ),
        (
            SourceRole::Configuration,
            &[
                "config/app.toml",
                "scripts/build.sh",
                "tools/lint.py",
                "build/Makefile",
                // configuration data keeps its role in a config directory
                "internal/config/defaults.yaml",
            ],
        ),
        (
            SourceRole::Source,
            &[
                // the default
                "src/lib.rs",
                "main.py",
                "app.ts",
                "Service.cs",
                // `.g.` must not match as part of a longer token
                "src/foo.gx.rs",
                "src/generate.rs",
                "src/flag.rs",
                // a config file under src is source
                "src/config.rs",
                "src/config.py",
                // look-alikes of the test-name rules
                "contest.go",
                "testing/helpers.go",
                "src/main/java/com/acme/Latest.java",
                "src/main/java/com/acme/Kit.java",
                "src/main/java/com/acme/GIT.java",
                "src/main/java/com/acme/TestDataFactory.java",
                "native/latest.c",
                "native/attest_util.c",
                "native/latest.h",
                "native/attest_util.h",
                "cpp/contest.cpp",
                "cpp/contest.cc",
                "cpp/latest.hh",
                "cpp/attest.hxx",
                "cpp/config.cxx",
                "include/config.h",
                "include/config.hpp",
                "src/attest.rs",
                "src/attests.rs",
                "src/latest.rs",
                "src/contest.rs",
                "src/contest_utils.rs",
                "src/testing.rs",
                "src/tester.rs",
                "src/testsuite.rs",
                "src/protest.rs",
                "src/lib.rs",
                "contest/solution.rs",
                "latest/release.rs",
                // `xtask/src/test.rs` implements `cargo xtask test`: a bare
                // test.rs carries no test-module convention
                "xtask/src/test.rs",
                "src/test.rs",
                // an inline `#[cfg(test)]` module cannot be told from the path
                "lci-core/src/filter/source_role.rs",
                // production code under config directories
                "config/loader.go",
                "internal/config/loader.go",
                "src/config/index.ts",
                "app/configs/settings.py",
                "configuration/Options.cs",
                "config/defaults.h",
                "src/config/loader.cc",
                // JVM package names are not repository layout
                "src/main/java/com/example/ConfigLoader.java",
                "src/main/java/com/acme/docs/DocRenderer.java",
                "src/main/kotlin/org/acme/config/Settings.kt",
                "src/main/java/com/acme/tools/Formatter.java",
            ],
        ),
    ];
    for (role, paths) in cases {
        for path in *paths {
            assert_eq!(classify_source_role(path), *role, "{path}");
        }
    }
}

/// A `.md` file is documentation in any directory; lookalike extensions are
/// not, and code in the same directories keeps its role.
#[test]
fn markdown_files_are_documentation_regardless_of_directory() {
    for path in [
        "README.md",
        "docs/guide.md",
        "src/notes.md",
        "tests/README.md",
        "src/tests/cases.md",
        "benches/results.md",
        "examples/walkthrough.md",
        "generated/api.md",
        "src/api.generated.md",
        "scripts/usage.md",
        "tools/lint/README.md",
        "config/schema.md",
        "build/notes.md",
        r"docs\sub\page.md",
        ".agents/skills/x/SKILL.md",
        // The extension folds case (N94); the directory rules do not decide.
        "tests/README.MD",
        "src/Guide.Md",
    ] {
        assert_eq!(
            classify_source_role(path),
            SourceRole::Documentation,
            "{path}"
        );
    }
    for (path, role) in [
        // Not a registered extension: the directory rules still decide.
        ("tests/notes.markdown", SourceRole::Test),
        ("examples/page.mdx", SourceRole::Example),
        ("tests/md", SourceRole::Test),
        ("src/md", SourceRole::Source),
        ("src/.md", SourceRole::Source),
        ("src/readme.md.txt", SourceRole::Source),
        // Code in the same directories keeps its role.
        ("tests/cases.rs", SourceRole::Test),
        ("scripts/build.py", SourceRole::Configuration),
        ("src/lib.rs", SourceRole::Source),
    ] {
        assert_eq!(classify_source_role(path), role, "{path}");
    }
}

/// A source file's role does not depend on the case of its extension (N94):
/// `lib_test.RS` is a Rust test file, `api.generated.TSX` is generated. The
/// stem and the directory names stay case-sensitive.
#[test]
fn a_mixed_case_extension_has_the_role_of_the_lowercase_one() {
    for (upper, lower) in [
        ("src/lib_test.RS", "src/lib_test.rs"),
        ("src/tests.Rs", "src/tests.rs"),
        ("tools/Check.Py", "tools/check.py"),
        ("src/test_util.PY", "src/test_util.py"),
        ("web/app.test.TS", "web/app.test.ts"),
        ("web/api.generated.TSX", "web/api.generated.tsx"),
        ("bench/Run.GO", "bench/run.go"),
        ("src/widget_test.CPP", "src/widget_test.cpp"),
        ("src/parser_bench.RS", "src/parser_bench.rs"),
        ("config/Types.CS", "config/types.cs"),
        ("src/Docs/Guide.MD", "src/Docs/Guide.md"),
    ] {
        assert_eq!(
            classify_source_role(upper),
            classify_source_role(lower),
            "{upper}"
        );
    }
    assert_eq!(classify_source_role("src/lib_test.RS"), SourceRole::Test);
    assert_eq!(
        classify_source_role("web/api.generated.TSX"),
        SourceRole::Generated
    );
    // Only the extension folds: a capitalised stem marker is still not one.
    assert_eq!(classify_source_role("src/lib_TEST.RS"), SourceRole::Source);
}

#[test]
fn markdown_is_a_registered_language_filter_value() {
    let filter = EffectiveFilter::from_request(&FilterRequest {
        languages: Some(vec!["markdown".into()]),
        source_roles: Some(vec!["documentation".into()]),
        ..Default::default()
    })
    .unwrap();
    assert!(filter.matches("markdown", "tests/README.md", SourceRole::Documentation));
    assert!(!filter.matches("rust", "src/lib.rs", SourceRole::Source));
    // The extension is not a language identifier.
    assert!(matches!(
        EffectiveFilter::from_request(&FilterRequest {
            languages: Some(vec!["md".into()]),
            ..Default::default()
        }),
        Err(FilterError::UnknownLanguage { .. })
    ));
}

#[test]
#[cfg(windows)]
fn backslash_separators_are_normalized() {
    assert_eq!(classify_source_role(r"tests\foo.rs"), SourceRole::Test);
    assert_eq!(classify_source_role(r"src\lib.rs"), SourceRole::Source);
    assert_eq!(
        classify_source_role(r"src\evaluate\tests.rs"),
        SourceRole::Test
    );
}

#[cfg(unix)]
#[test]
fn unix_backslashes_are_filename_characters_in_filters_and_source_roles() {
    let include = EffectiveFilter::from_request(&FilterRequest {
        include_paths: Some(vec!["a/b.rs".into()]),
        ..Default::default()
    })
    .unwrap();
    assert!(!include.matches("rust", r"a\b.rs", SourceRole::Source));

    let exclude = EffectiveFilter::from_request(&FilterRequest {
        exclude_paths: Some(vec!["a/b.rs".into()]),
        ..Default::default()
    })
    .unwrap();
    assert!(exclude.matches("rust", r"a\b.rs", SourceRole::Source));

    let root_file = EffectiveFilter::from_request(&FilterRequest {
        include_paths: Some(vec!["*.rs".into()]),
        ..Default::default()
    })
    .unwrap();
    assert!(root_file.matches("rust", r"a\b.rs", SourceRole::Source));
    assert_eq!(classify_source_role(r"tests\lib.rs"), SourceRole::Source);
}

// -- GlobPattern -------------------------------------------------------

/// `(pattern, path, expected)`; the pattern and path are named in the failure.
#[test]
fn glob_matching() {
    let cases = [
        // `*` does not cross a separator
        ("src/*.rs", "src/lib.rs", true),
        ("src/*.rs", "src/deep/lib.rs", false),
        // `**` crosses directories
        ("src/**/*.rs", "src/lib.rs", true),
        ("src/**/*.rs", "src/deep/lib.rs", true),
        ("src/**/*.rs", "src/a/b/c/lib.rs", true),
        // `?` matches one non-separator
        ("src/?.rs", "src/a.rs", true),
        ("src/?.rs", "src/ab.rs", false),
        ("src/?.rs", "src/a/b.rs", false),
        // case-sensitive
        ("src/*.rs", "src/LIB.RS", false),
        // a colon is fine in a relative pattern
        ("a:b", "a:b", true),
        // dot components are removed; doubled separators collapse
        ("./src/**", "src/main.rs", true),
        ("src//*.rs", "src/a.rs", true),
    ];
    for (pattern, path, expected) in cases {
        let glob = GlobPattern::compile(pattern).unwrap();
        assert_eq!(glob.matches(path), expected, "{pattern} against {path}");
    }
    assert_eq!(normalize_path("src/./lib.rs"), "src/lib.rs");
}

#[test]
fn glob_rejects_invalid_patterns() {
    let empty = FilterError::EmptyPattern;
    let absolute = FilterError::AbsolutePath {
        pattern: String::new(),
    };
    let cases = [
        ("", &empty),
        ("   ", &empty),
        // made only of dot components
        (".", &empty),
        ("./", &empty),
        ("./.", &empty),
        (
            r"src\lib.rs",
            &FilterError::BackslashInPattern {
                pattern: String::new(),
            },
        ),
        // absolute Unix, Windows drive, and UNC paths
        ("/etc/passwd", &absolute),
        ("C:/projects/foo", &absolute),
        ("C:/x", &absolute),
        (r"C:\x", &absolute),
        ("//server/share", &absolute),
        (
            "../etc/passwd",
            &FilterError::Traversal {
                pattern: String::new(),
            },
        ),
        (
            "src/../../etc",
            &FilterError::Traversal {
                pattern: String::new(),
            },
        ),
        // an unbalanced bracket is not a valid glob
        (
            "src/[a.rs",
            &FilterError::MalformedPattern {
                pattern: String::new(),
            },
        ),
    ];
    for (pattern, kind) in cases {
        let error = GlobPattern::compile(pattern).unwrap_err();
        assert_eq!(discriminant(&error), discriminant(kind), "{pattern:?}");
    }
}

// -- EffectiveFilter validation ----------------------------------------

/// `Some(list)` of owned strings, for building requests.
fn strings(items: &[&str]) -> Option<Vec<String>> {
    Some(items.iter().map(|s| (*s).to_owned()).collect())
}

#[test]
fn effective_filter_rejects_invalid_requests() {
    let cases = [
        (
            "explicitly empty languages",
            FilterRequest {
                languages: strings(&[]),
                ..Default::default()
            },
            FilterError::EmptyLanguages,
        ),
        (
            "explicitly empty source roles",
            FilterRequest {
                source_roles: strings(&[]),
                ..Default::default()
            },
            FilterError::EmptySourceRoles,
        ),
        (
            "unknown language",
            FilterRequest {
                languages: strings(&["cobol"]),
                ..Default::default()
            },
            FilterError::UnknownLanguage {
                value: "cobol".into(),
            },
        ),
        (
            "an extension is not a language identifier",
            FilterRequest {
                languages: strings(&["rs"]),
                ..Default::default()
            },
            FilterError::UnknownLanguage { value: "rs".into() },
        ),
        (
            "unknown source role",
            FilterRequest {
                source_roles: strings(&["bogus"]),
                ..Default::default()
            },
            FilterError::UnknownSourceRole {
                value: "bogus".into(),
            },
        ),
    ];
    for (name, req, expected) in cases {
        assert_eq!(
            EffectiveFilter::from_request(&req).unwrap_err(),
            expected,
            "{name}"
        );
    }

    // Path-pattern errors carry the pattern; only the kind is pinned.
    let pattern_cases = [
        (
            "empty path pattern",
            FilterRequest {
                include_paths: strings(&[""]),
                ..Default::default()
            },
            FilterError::EmptyPattern,
        ),
        (
            "traversal in a path pattern",
            FilterRequest {
                exclude_paths: strings(&["../secret"]),
                ..Default::default()
            },
            FilterError::Traversal {
                pattern: String::new(),
            },
        ),
    ];
    for (name, req, kind) in pattern_cases {
        let error = EffectiveFilter::from_request(&req).unwrap_err();
        assert_eq!(discriminant(&error), discriminant(&kind), "{name}");
    }
}

#[test]
fn the_unknown_language_message_names_each_identifier_once() {
    let req = FilterRequest {
        languages: Some(vec!["cobol".into()]),
        ..Default::default()
    };
    let message = EffectiveFilter::from_request(&req).unwrap_err().to_string();
    let (_, listed) = message.split_once("registered identifiers: ").unwrap();
    let listed: Vec<&str> = listed.split(", ").collect();
    assert_eq!(
        listed.iter().filter(|name| **name == "cpp").count(),
        1,
        "{message}"
    );
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), listed.len(), "{message}");
}

#[test]
fn accepts_all_seven_registered_languages() {
    for lang in [
        "rust",
        "typescript",
        "tsx",
        "javascript",
        "jsx",
        "python",
        "csharp",
    ] {
        let req = FilterRequest {
            languages: Some(vec![lang.into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert_eq!(
            eff.languages.as_deref(),
            Some(vec![lang.to_owned()].as_slice())
        );
    }
}

#[test]
fn requests_are_deduplicated_deterministically() {
    let req = FilterRequest {
        languages: Some(vec![
            "python".into(),
            "rust".into(),
            "python".into(),
            "rust".into(),
        ]),
        source_roles: Some(vec!["test".into(), "source".into(), "test".into()]),
        include_paths: Some(vec![
            "src/*.rs".into(),
            "src/*.rs".into(),
            "lib/*.rs".into(),
        ]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert_eq!(
        eff.languages.as_deref(),
        Some(vec!["python".to_owned(), "rust".to_owned()].as_slice()),
        "languages"
    );
    assert_eq!(
        eff.source_roles.as_deref(),
        Some([SourceRole::Source, SourceRole::Test].as_slice()),
        "source roles"
    );
    assert_eq!(
        eff.report().include_paths,
        Some(vec!["lib/*.rs".to_owned(), "src/*.rs".to_owned()]),
        "path patterns"
    );
}

// -- EffectiveFilter matching ------------------------------------------

#[test]
fn effective_filter_matching() {
    // (name, request, [(language, path, role, expected)])
    let cases = [
        (
            "no filters match everything",
            FilterRequest::default(),
            vec![
                ("rust", "src/lib.rs", SourceRole::Source, true),
                ("python", "tests/foo_test.py", SourceRole::Test, true),
            ],
        ),
        (
            "language filter restricts",
            FilterRequest {
                languages: strings(&["rust"]),
                ..Default::default()
            },
            vec![
                ("rust", "src/lib.rs", SourceRole::Source, true),
                ("python", "src/lib.py", SourceRole::Source, false),
            ],
        ),
        (
            "source-role filter restricts",
            FilterRequest {
                source_roles: strings(&["test"]),
                ..Default::default()
            },
            vec![
                ("rust", "tests/foo.rs", SourceRole::Test, true),
                ("rust", "src/lib.rs", SourceRole::Source, false),
            ],
        ),
        (
            "include then exclude",
            FilterRequest {
                include_paths: strings(&["src/**/*.rs"]),
                exclude_paths: strings(&["src/generated/**"]),
                ..Default::default()
            },
            vec![
                // included and not excluded
                ("rust", "src/lib.rs", SourceRole::Source, true),
                // included but excluded
                ("rust", "src/generated/foo.rs", SourceRole::Generated, false),
                // not included
                ("rust", "lib/foo.rs", SourceRole::Source, false),
            ],
        ),
        (
            "exclude only",
            FilterRequest {
                exclude_paths: strings(&["**/*.generated.rs"]),
                ..Default::default()
            },
            vec![
                ("rust", "src/lib.rs", SourceRole::Source, true),
                ("rust", "src/foo.generated.rs", SourceRole::Generated, false),
            ],
        ),
    ];
    for (name, req, checks) in cases {
        let eff = EffectiveFilter::from_request(&req).unwrap();
        for (language, path, role, expected) in checks {
            assert_eq!(
                eff.matches(language, path, role),
                expected,
                "{name}: {language} {path}"
            );
        }
    }
}

// -- Report serialization ----------------------------------------------

#[test]
fn report_serializes_absent_filters_as_null_and_present_ones_sorted() {
    let req = FilterRequest {
        languages: Some(vec!["rust".into()]),
        ..Default::default()
    };
    let json = serde_json::to_value(EffectiveFilter::from_request(&req).unwrap().report()).unwrap();
    assert_eq!(json["languages"], serde_json::json!(["rust"]));
    assert!(json["include_paths"].is_null());
    assert!(json["exclude_paths"].is_null());
    assert!(json["source_roles"].is_null());

    let req = FilterRequest {
        languages: Some(vec!["rust".into(), "python".into()]),
        include_paths: Some(vec!["src/**/*.rs".into()]),
        exclude_paths: Some(vec!["**/generated/**".into()]),
        source_roles: Some(vec!["source".into(), "test".into()]),
    };
    let json = serde_json::to_value(EffectiveFilter::from_request(&req).unwrap().report()).unwrap();
    assert_eq!(json["languages"], serde_json::json!(["python", "rust"]));
    assert_eq!(json["include_paths"], serde_json::json!(["src/**/*.rs"]));
    assert_eq!(
        json["exclude_paths"],
        serde_json::json!(["**/generated/**"])
    );
    assert_eq!(json["source_roles"], serde_json::json!(["source", "test"]));
}
