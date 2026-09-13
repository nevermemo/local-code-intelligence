use super::*;
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

#[test]
fn classifies_generated_by_directory() {
    assert_eq!(
        classify_source_role("generated/foo.rs"),
        SourceRole::Generated
    );
    assert_eq!(classify_source_role("gen/bar.py"), SourceRole::Generated);
}

#[test]
fn classifies_generated_by_file_name_markers() {
    assert_eq!(
        classify_source_role("src/foo.generated.rs"),
        SourceRole::Generated
    );
    assert_eq!(
        classify_source_role("src/foo_generated.py"),
        SourceRole::Generated
    );
    assert_eq!(classify_source_role("src/foo.g.rs"), SourceRole::Generated);
}

#[test]
fn generated_g_marker_is_unambiguous() {
    // `.g.` must not match as part of a longer token.
    assert_eq!(classify_source_role("src/foo.gx.rs"), SourceRole::Source);
    assert_eq!(classify_source_role("src/generate.rs"), SourceRole::Source);
    assert_eq!(classify_source_role("src/flag.rs"), SourceRole::Source);
}

#[test]
fn classifies_test_by_directory() {
    assert_eq!(classify_source_role("tests/foo.rs"), SourceRole::Test);
    assert_eq!(classify_source_role("test/bar.py"), SourceRole::Test);
    assert_eq!(classify_source_role("__tests__/baz.js"), SourceRole::Test);
}

#[test]
fn classifies_test_by_file_name() {
    assert_eq!(classify_source_role("src/foo_test.py"), SourceRole::Test);
    assert_eq!(classify_source_role("src/test_foo.py"), SourceRole::Test);
    assert_eq!(classify_source_role("src/foo.test.ts"), SourceRole::Test);
    assert_eq!(classify_source_role("src/foo.spec.tsx"), SourceRole::Test);
    assert_eq!(classify_source_role("src/foo.test.js"), SourceRole::Test);
    assert_eq!(classify_source_role("src/foo.spec.jsx"), SourceRole::Test);
}

#[test]
fn classifies_benchmark_by_directory() {
    assert_eq!(
        classify_source_role("benches/foo.rs"),
        SourceRole::Benchmark
    );
    assert_eq!(classify_source_role("bench/bar.py"), SourceRole::Benchmark);
    assert_eq!(
        classify_source_role("benchmark/baz.rs"),
        SourceRole::Benchmark
    );
    assert_eq!(
        classify_source_role("benchmarks/qux.rs"),
        SourceRole::Benchmark
    );
}

#[test]
fn classifies_benchmark_by_file_name() {
    assert_eq!(
        classify_source_role("src/foo_bench.rs"),
        SourceRole::Benchmark
    );
    assert_eq!(
        classify_source_role("src/bench_foo.py"),
        SourceRole::Benchmark
    );
    assert_eq!(
        classify_source_role("src/foo.bench.ts"),
        SourceRole::Benchmark
    );
}

#[test]
fn classifies_example_by_directory() {
    assert_eq!(classify_source_role("examples/foo.rs"), SourceRole::Example);
    assert_eq!(classify_source_role("example/bar.py"), SourceRole::Example);
    assert_eq!(classify_source_role("samples/baz.js"), SourceRole::Example);
}

#[test]
fn classifies_documentation_by_directory() {
    assert_eq!(
        classify_source_role("docs/guide.md"),
        SourceRole::Documentation
    );
    assert_eq!(
        classify_source_role("doc/api.md"),
        SourceRole::Documentation
    );
    assert_eq!(
        classify_source_role("documentation/overview.md"),
        SourceRole::Documentation
    );
}

#[test]
fn classifies_configuration_by_directory() {
    assert_eq!(
        classify_source_role("config/app.toml"),
        SourceRole::Configuration
    );
    assert_eq!(
        classify_source_role("scripts/build.sh"),
        SourceRole::Configuration
    );
    assert_eq!(
        classify_source_role("tools/lint.py"),
        SourceRole::Configuration
    );
    assert_eq!(
        classify_source_role("build/Makefile"),
        SourceRole::Configuration
    );
}

#[test]
fn defaults_to_source() {
    assert_eq!(classify_source_role("src/lib.rs"), SourceRole::Source);
    assert_eq!(classify_source_role("main.py"), SourceRole::Source);
    assert_eq!(classify_source_role("app.ts"), SourceRole::Source);
}

#[test]
fn precedence_generated_over_test() {
    // A file in a `generated` directory that also looks like a test
    // should be classified as generated.
    assert_eq!(
        classify_source_role("generated/foo_test.py"),
        SourceRole::Generated
    );
}

#[test]
fn precedence_test_over_benchmark() {
    // A file in a `tests` directory with a benchmark-looking name
    // should be classified as test.
    assert_eq!(classify_source_role("tests/foo_bench.rs"), SourceRole::Test);
}

#[test]
fn src_config_files_remain_source() {
    assert_eq!(classify_source_role("src/config.rs"), SourceRole::Source);
    assert_eq!(classify_source_role("src/config.py"), SourceRole::Source);
}

#[test]
fn contest_and_latest_are_not_test_or_generated() {
    // `contest` and `latest` are not in the test or generated
    // component lists, so they should fall through to source.
    assert_eq!(
        classify_source_role("contest/solution.rs"),
        SourceRole::Source
    );
    assert_eq!(
        classify_source_role("latest/release.rs"),
        SourceRole::Source
    );
}

#[test]
fn backslash_separators_are_normalized() {
    assert_eq!(classify_source_role(r"tests\foo.rs"), SourceRole::Test);
    assert_eq!(classify_source_role(r"src\lib.rs"), SourceRole::Source);
}

// -- GlobPattern -------------------------------------------------------

#[test]
fn glob_single_star_does_not_cross_separator() {
    let g = GlobPattern::compile("src/*.rs").unwrap();
    assert!(g.matches("src/lib.rs"));
    assert!(!g.matches("src/deep/lib.rs"));
}

#[test]
fn glob_double_star_crosses_directories() {
    let g = GlobPattern::compile("src/**/*.rs").unwrap();
    assert!(g.matches("src/lib.rs"));
    assert!(g.matches("src/deep/lib.rs"));
    assert!(g.matches("src/a/b/c/lib.rs"));
}

#[test]
fn glob_question_mark_matches_one_non_separator() {
    let g = GlobPattern::compile("src/?.rs").unwrap();
    assert!(g.matches("src/a.rs"));
    assert!(!g.matches("src/ab.rs"));
    assert!(!g.matches("src/a/b.rs"));
}

#[test]
fn glob_is_case_sensitive() {
    let g = GlobPattern::compile("src/*.rs").unwrap();
    assert!(!g.matches("src/LIB.RS"));
}

#[test]
fn glob_rejects_empty_pattern() {
    assert!(matches!(
        GlobPattern::compile(""),
        Err(FilterError::EmptyPattern)
    ));
    assert!(matches!(
        GlobPattern::compile("   "),
        Err(FilterError::EmptyPattern)
    ));
}

#[test]
fn glob_rejects_backslash() {
    assert!(matches!(
        GlobPattern::compile(r"src\lib.rs"),
        Err(FilterError::BackslashInPattern { .. })
    ));
}

#[test]
fn glob_rejects_absolute_unix() {
    assert!(matches!(
        GlobPattern::compile("/etc/passwd"),
        Err(FilterError::AbsolutePath { .. })
    ));
}

#[test]
fn glob_rejects_absolute_windows() {
    assert!(matches!(
        GlobPattern::compile("C:/Users/foo"),
        Err(FilterError::AbsolutePath { .. })
    ));
}

#[test]
fn glob_rejects_unc() {
    assert!(matches!(
        GlobPattern::compile("//server/share"),
        Err(FilterError::AbsolutePath { .. })
    ));
}

#[test]
fn glob_rejects_traversal() {
    assert!(matches!(
        GlobPattern::compile("../etc/passwd"),
        Err(FilterError::Traversal { .. })
    ));
    assert!(matches!(
        GlobPattern::compile("src/../../etc"),
        Err(FilterError::Traversal { .. })
    ));
}

#[test]
fn glob_rejects_malformed_pattern() {
    // An unbalanced bracket is not a valid glob.
    assert!(matches!(
        GlobPattern::compile("src/[a.rs"),
        Err(FilterError::MalformedPattern { .. })
    ));
}

// -- EffectiveFilter validation ----------------------------------------

#[test]
fn rejects_explicitly_empty_languages() {
    let req = FilterRequest {
        languages: Some(vec![]),
        ..Default::default()
    };
    assert!(matches!(
        EffectiveFilter::from_request(&req),
        Err(FilterError::EmptyLanguages)
    ));
}

#[test]
fn rejects_explicitly_empty_source_roles() {
    let req = FilterRequest {
        source_roles: Some(vec![]),
        ..Default::default()
    };
    assert!(matches!(
        EffectiveFilter::from_request(&req),
        Err(FilterError::EmptySourceRoles)
    ));
}

#[test]
fn rejects_unknown_language() {
    let req = FilterRequest {
        languages: Some(vec!["cobol".into()]),
        ..Default::default()
    };
    let error = EffectiveFilter::from_request(&req).unwrap_err();
    assert_eq!(
        error,
        FilterError::UnknownLanguage {
            value: "cobol".into()
        }
    );
}

#[test]
fn rejects_unknown_source_role() {
    let req = FilterRequest {
        source_roles: Some(vec!["bogus".into()]),
        ..Default::default()
    };
    let error = EffectiveFilter::from_request(&req).unwrap_err();
    assert_eq!(
        error,
        FilterError::UnknownSourceRole {
            value: "bogus".into()
        }
    );
}

#[test]
fn accepts_all_six_registered_languages() {
    for lang in ["rust", "typescript", "tsx", "javascript", "jsx", "python"] {
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
fn rejects_extension_as_language_identifier() {
    let req = FilterRequest {
        languages: Some(vec!["rs".into()]),
        ..Default::default()
    };
    assert!(matches!(
        EffectiveFilter::from_request(&req),
        Err(FilterError::UnknownLanguage { .. })
    ));
}

#[test]
fn deduplicates_languages_deterministically() {
    let req = FilterRequest {
        languages: Some(vec![
            "python".into(),
            "rust".into(),
            "python".into(),
            "rust".into(),
        ]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert_eq!(
        eff.languages.as_deref(),
        Some(vec!["python".to_owned(), "rust".to_owned()].as_slice())
    );
}

#[test]
fn deduplicates_source_roles_deterministically() {
    let req = FilterRequest {
        source_roles: Some(vec!["test".into(), "source".into(), "test".into()]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert_eq!(
        eff.source_roles.as_deref(),
        Some([SourceRole::Source, SourceRole::Test].as_slice())
    );
}

#[test]
fn deduplicates_path_patterns() {
    let req = FilterRequest {
        include_paths: Some(vec![
            "src/*.rs".into(),
            "src/*.rs".into(),
            "lib/*.rs".into(),
        ]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert_eq!(
        eff.report().include_paths,
        Some(vec!["lib/*.rs".to_owned(), "src/*.rs".to_owned()])
    );
}

#[test]
fn rejects_malformed_path_pattern() {
    let req = FilterRequest {
        include_paths: Some(vec!["".into()]),
        ..Default::default()
    };
    assert!(matches!(
        EffectiveFilter::from_request(&req),
        Err(FilterError::EmptyPattern)
    ));
}

#[test]
fn rejects_traversal_in_path_pattern() {
    let req = FilterRequest {
        exclude_paths: Some(vec!["../secret".into()]),
        ..Default::default()
    };
    assert!(matches!(
        EffectiveFilter::from_request(&req),
        Err(FilterError::Traversal { .. })
    ));
}

// -- EffectiveFilter matching ------------------------------------------

#[test]
fn no_filters_matches_everything() {
    let eff = EffectiveFilter::from_request(&FilterRequest::default()).unwrap();
    assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
    assert!(eff.matches("python", "tests/foo_test.py", SourceRole::Test));
}

#[test]
fn language_filter_restricts() {
    let req = FilterRequest {
        languages: Some(vec!["rust".into()]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
    assert!(!eff.matches("python", "src/lib.py", SourceRole::Source));
}

#[test]
fn source_role_filter_restricts() {
    let req = FilterRequest {
        source_roles: Some(vec!["test".into()]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert!(eff.matches("rust", "tests/foo.rs", SourceRole::Test));
    assert!(!eff.matches("rust", "src/lib.rs", SourceRole::Source));
}

#[test]
fn include_then_exclude_precedence() {
    let req = FilterRequest {
        include_paths: Some(vec!["src/**/*.rs".into()]),
        exclude_paths: Some(vec!["src/generated/**".into()]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    // Included and not excluded.
    assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
    // Included but excluded.
    assert!(!eff.matches("rust", "src/generated/foo.rs", SourceRole::Generated));
    // Not included.
    assert!(!eff.matches("rust", "lib/foo.rs", SourceRole::Source));
}

#[test]
fn exclude_only_filters() {
    let req = FilterRequest {
        exclude_paths: Some(vec!["**/*.generated.rs".into()]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
    assert!(!eff.matches("rust", "src/foo.generated.rs", SourceRole::Generated));
}

// -- Report serialization ----------------------------------------------

#[test]
fn report_represents_absent_filters_as_null() {
    let req = FilterRequest {
        languages: Some(vec!["rust".into()]),
        ..Default::default()
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    let report = eff.report();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["languages"], serde_json::json!(["rust"]));
    assert!(json["include_paths"].is_null());
    assert!(json["exclude_paths"].is_null());
    assert!(json["source_roles"].is_null());
}

#[test]
fn report_serializes_all_filters_when_present() {
    let req = FilterRequest {
        languages: Some(vec!["rust".into(), "python".into()]),
        include_paths: Some(vec!["src/**/*.rs".into()]),
        exclude_paths: Some(vec!["**/generated/**".into()]),
        source_roles: Some(vec!["source".into(), "test".into()]),
    };
    let eff = EffectiveFilter::from_request(&req).unwrap();
    let report = eff.report();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["languages"], serde_json::json!(["python", "rust"]));
    assert_eq!(json["include_paths"], serde_json::json!(["src/**/*.rs"]));
    assert_eq!(
        json["exclude_paths"],
        serde_json::json!(["**/generated/**"])
    );
    assert_eq!(json["source_roles"], serde_json::json!(["source", "test"]));
}
