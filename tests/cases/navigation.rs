use super::*;

#[tokio::test]
async fn typescript_javascript_only_search_skips_rust_analyzer() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("web-only");
    write(
        &workspace,
        "src/telemetry.ts",
        "export function buildProductionTelemetryPipeline(events: string[]) { return events; }\n",
    );
    write(
        &workspace,
        "src/runtime.js",
        "export const runtimeReady = () => true;\n",
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .search(&workspace, "buildProductionTelemetryPipeline", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "typescript");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    fake.embed_fails.store(true, Ordering::SeqCst);
    let lexical_only = app
        .search(&workspace, "runtimeReady", Some(4))
        .await
        .unwrap();
    assert!(lexical_only.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "src/runtime.js"
            && hit.chunk.language == "javascript"
            && hit.retrieval_channels.contains(&"lexical".to_string())
    }));
    let warning = lexical_only.warning.as_deref().unwrap();
    assert!(warning.contains("Query embedding unavailable"));
    assert!(!warning.contains("LSP"));
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let error = app
        .definition(&workspace, "src/telemetry.ts", 1, 0)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support typescript source files")
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}

#[tokio::test]
async fn python_only_search_and_navigation_skip_rust_analyzer() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("python-only");
    write(
        &workspace,
        "src/revenue.py",
        "# Quarterly revenue summary implementation.\ndef compute_quarterly_revenue_summary(rows):\n    return [row for row in rows if row.get(\"amount\")]\n",
    );
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1);
    assert_eq!(indexed.parsed_files, 1);
    assert_eq!(indexed.embedded_chunks, 1);

    let report = app
        .search(&workspace, "compute_quarterly_revenue_summary", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "python");
    assert_eq!(report.results[0].chunk.relative_file_path, "src/revenue.py");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let definition_error = app
        .definition(&workspace, "src/revenue.py", 1, 0)
        .await
        .unwrap_err();
    assert!(
        definition_error
            .to_string()
            .contains("does not support python source files"),
        "unexpected error: {definition_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let references_error = app
        .references(&workspace, "src/revenue.py", 1, 0, true)
        .await
        .unwrap_err();
    assert!(
        references_error
            .to_string()
            .contains("does not support python source files"),
        "unexpected error: {references_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}

#[tokio::test]
async fn csharp_only_search_and_navigation_skip_rust_analyzer() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("csharp-only");
    write(
        &workspace,
        "src/RevenueService.cs",
        "namespace Billing;\npublic sealed class RevenueService { public decimal ComputeQuarterlyRevenue() => 42m; }\n",
    );
    let app = App::open(config).await.unwrap();
    assert_eq!(app.index(&workspace).await.unwrap().files, 1);
    let report = app
        .search(&workspace, "ComputeQuarterlyRevenue", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "csharp");
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    for error in [
        app.definition(&workspace, "src/RevenueService.cs", 2, 28)
            .await
            .unwrap_err(),
        app.references(&workspace, "src/RevenueService.cs", 2, 28, true)
            .await
            .unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains("does not support csharp source files"),
            "unexpected error: {error}"
        );
    }
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}
