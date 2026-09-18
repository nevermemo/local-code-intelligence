use crate::{language, workspace::hash};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::{Node, Parser};

const TARGET_BYTES: usize = 6000;
const MAX_DECLARATION_BYTES: usize = 24000;
pub const DOCUMENT_FORMAT_VERSION: &str = "rust-chunks-v1";

#[derive(Debug)]
pub struct Scan {
    pub chunks: Vec<Chunk>,
    pub files: HashMap<String, crate::manifest::CachedFile>,
    pub fingerprint: String,
    pub parsed_files: usize,
    pub unchanged_files: usize,
    pub removed_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub relative_file_path: String,
    pub language: String,
    pub start_line: u32,
    pub end_line: u32,
    pub code: String,
    pub content_hash: String,
}

// Recurse through oversized declarations to syntax boundaries (including match arms).
// A large leaf remains whole: no arbitrary text windows or broken UTF-8.
fn ranges(adapter: &language::LanguageAdapter, node: Node<'_>, out: &mut Vec<(usize, usize)>) {
    let container = adapter.is_container(node.kind());
    let boundary = adapter.is_boundary(node.kind());
    let declaration = adapter.is_declaration(node.kind());
    if !container
        && !boundary
        && node.end_byte() - node.start_byte()
            <= if declaration {
                MAX_DECLARATION_BYTES
            } else {
                TARGET_BYTES
            }
    {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    if children.is_empty() {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    let mut start = node.start_byte();
    let mut end = start;
    let mut pending_prefix = None;
    for child in children {
        if container && adapter.is_prefix(child.kind()) {
            pending_prefix.get_or_insert(child.start_byte());
            continue;
        }
        let child_start = pending_prefix.take().unwrap_or(child.start_byte());
        let child_limit = if adapter.is_declaration(child.kind()) {
            MAX_DECLARATION_BYTES
        } else {
            TARGET_BYTES
        };
        let split = child.end_byte() - child.start_byte() > child_limit
            || (adapter.is_container(child.kind()) && child.kind() != "source_file")
            || adapter.is_boundary(child.kind());
        if split {
            let attach_header = adapter.attaches_header_to_child(node.kind(), child.kind());
            if child_start > start && !attach_header {
                out.push((start, child_start));
            }
            let first = out.len();
            ranges(adapter, child, out);
            if let Some(range) = out.get_mut(first) {
                range.0 = if attach_header { start } else { child_start };
            }
            start = child.end_byte();
            end = start;
        } else if container || child.end_byte() - start > TARGET_BYTES {
            if end > start {
                out.push((start, end));
            }
            start = child_start;
            end = child.end_byte();
        } else {
            end = child.end_byte();
        }
    }
    if node.end_byte() > start {
        out.push((start, node.end_byte()));
    }
}

pub fn rust_chunks(path: &str, source: &str) -> Result<Vec<Chunk>> {
    let adapter = language::for_extension("rs").expect("Rust adapter must be registered");
    chunks_with_adapter(path, source, adapter)
}

pub fn chunks_for_path(path: &str, source: &str) -> Result<Vec<Chunk>> {
    let Some(adapter) = language::for_path(Path::new(path)) else {
        bail!("unsupported source extension: {path}");
    };
    chunks_with_adapter(path, source, adapter)
}

fn chunks_with_adapter(
    path: &str,
    source: &str,
    adapter: &language::LanguageAdapter,
) -> Result<Vec<Chunk>> {
    let mut parser = Parser::new();
    parser.set_language(&adapter.grammar())?;
    let tree = parser
        .parse(source, None)
        .context("Tree-sitter parse cancelled")?;
    let mut spans = Vec::new();
    ranges(adapter, tree.root_node(), &mut spans);
    let mut chunks = Vec::new();
    for (start, end) in spans {
        let raw = &source[start..end];
        let code = raw.trim();
        if code.is_empty() || code.chars().all(|c| c.is_whitespace() || "{};".contains(c)) {
            continue;
        }
        let begin = start + raw.len() - raw.trim_start().len();
        let finish = begin + code.len();
        chunks.push(Chunk {
            relative_file_path: path.into(),
            language: adapter.identifier().into(),
            start_line: source[..begin].bytes().filter(|&b| b == b'\n').count() as u32 + 1,
            end_line: source[..finish].bytes().filter(|&b| b == b'\n').count() as u32 + 1,
            code: code.into(),
            content_hash: hash(code),
        });
    }
    Ok(chunks)
}

struct SourceFile {
    relative_path: String,
    source: String,
    content_hash: String,
    adapter: &'static language::LanguageAdapter,
}

fn sources(root: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .add_custom_ignore_filename(".lciignore")
        .filter_entry(|e| e.file_name() != ".git");
    for entry in walker.build() {
        let entry = entry.context("workspace traversal failed; previous index retained")?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(adapter) = language::for_path(entry.path()) else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("non-UTF-8 file path")?
            .replace('\\', "/");
        let source =
            std::fs::read_to_string(entry.path()).with_context(|| format!("read {relative}"))?;
        let source_hash = hash(&source);
        files.push(SourceFile {
            relative_path: relative,
            source,
            content_hash: source_hash,
            adapter,
        });
    }
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

fn fingerprint_material(file: &SourceFile) -> String {
    format!(
        "{}\0{}\0{}\0{}\n",
        file.relative_path,
        file.content_hash,
        file.adapter.identifier(),
        file.adapter.cache_version()
    )
}

pub fn fingerprint(root: &Path) -> Result<String> {
    let material = sources(root)?
        .iter()
        .map(fingerprint_material)
        .collect::<String>();
    Ok(hash(&material))
}

pub fn scan_incremental(
    root: &Path,
    previous: &HashMap<String, crate::manifest::CachedFile>,
) -> Result<Scan> {
    let mut chunks = Vec::new();
    let mut files = HashMap::new();
    let mut parsed_files = 0;
    let mut unchanged_files = 0;
    let source_files = sources(root)?;
    let present: HashSet<_> = source_files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect();
    let removed_files = previous
        .keys()
        .filter(|p| !present.contains(p.as_str()))
        .count();
    let fingerprint_material = source_files
        .iter()
        .map(fingerprint_material)
        .collect::<String>();
    for file in source_files {
        let file_chunks = match previous.get(&file.relative_path) {
            Some(cached)
                if cached.content_hash == file.content_hash
                    && cached.language == file.adapter.identifier()
                    && cached.adapter_version == file.adapter.cache_version() =>
            {
                unchanged_files += 1;
                cached.chunks.clone()
            }
            _ => {
                parsed_files += 1;
                chunks_with_adapter(&file.relative_path, &file.source, file.adapter)?
            }
        };
        chunks.extend(file_chunks.clone());
        files.insert(
            file.relative_path,
            crate::manifest::CachedFile {
                language: file.adapter.identifier().into(),
                adapter_version: file.adapter.cache_version().into(),
                content_hash: file.content_hash,
                chunks: file_chunks,
            },
        );
    }
    chunks.sort_by(|a, b| {
        (&a.relative_file_path, a.start_line).cmp(&(&b.relative_file_path, b.start_line))
    });
    Ok(Scan {
        chunks,
        files,
        fingerprint: hash(&fingerprint_material),
        parsed_files,
        unchanged_files,
        removed_files,
    })
}

pub fn scan(root: &Path) -> Result<Vec<Chunk>> {
    Ok(scan_incremental(root, &HashMap::new())?.chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_have_exact_source_lines_and_stable_hashes() {
        let source =
            "// café\nfn first() { println!(\"hello\"); }\n\nimpl A {\nfn second() {}\n}\n";
        let chunks = rust_chunks("lib.rs", source).unwrap();
        assert!(chunks.iter().any(|c| c.code.contains("fn second")));
        for c in chunks {
            assert!(source.contains(&c.code));
            assert_eq!(c.content_hash, hash(&c.code));
            assert!(c.end_line >= c.start_line);
        }
    }

    #[test]
    fn rust_boundaries_remain_stable() {
        assert_eq!(DOCUMENT_FORMAT_VERSION, "rust-chunks-v1");
        let source =
            "// café\nfn first() { println!(\"hello\"); }\n\nimpl A {\nfn second() {}\n}\n";
        let chunks = rust_chunks("lib.rs", source).unwrap();
        let boundaries: Vec<_> = chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
            .collect();
        assert_eq!(
            boundaries,
            vec![
                (1, 2, "// café\nfn first() { println!(\"hello\"); }"),
                (4, 4, "impl A"),
                (4, 6, "{\nfn second() {}\n}"),
            ]
        );
    }

    #[test]
    fn chunks_typescript_declarations_prefixes_and_top_level_code() {
        let source = r#"import { metric } from "./metric";
// Streams telemetry.
export function* streamTelemetry() { yield metric; }

/** Runtime service. */
@sealed
export class TelemetryService {
  // Records a sample.
  @trace
  record(): number { return metric; }
}

export interface TelemetrySink {
  write(value: number): void;
}
export type TelemetryId = string;
export enum TelemetryMode { Live, Replay }
export const collectTelemetry = (value: number) => value + 1;
startTelemetry();
"#;
        let chunks = chunks_for_path("telemetry.ts", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "typescript"));
        for expected in [
            "import { metric }",
            "export function* streamTelemetry",
            "export class TelemetryService",
            "record(): number",
            "export interface TelemetrySink",
            "write(value: number): void",
            "export type TelemetryId",
            "export enum TelemetryMode",
            "export const collectTelemetry",
            "startTelemetry();",
        ] {
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(expected)),
                "missing {expected}: {chunks:#?}"
            );
        }
        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class TelemetryService"))
            .unwrap();
        assert!(class.code.starts_with("/** Runtime service. */"));
        assert!(class.code.contains("@sealed"));
        assert!(class.code.contains("@trace"));
        let arrow = chunks
            .iter()
            .find(|chunk| chunk.code.contains("collectTelemetry"))
            .unwrap();
        assert!(arrow.code.starts_with("export const collectTelemetry"));
    }

    #[test]
    fn chunks_class_methods_and_interface_signatures_at_member_boundaries() {
        let source = r#"/** Coordinates telemetry. */
export class TelemetryCoordinator {
  // Starts collection.
  start(): void {}

  // Stops collection.
  stop(): void {}
}

export interface TelemetryLifecycle {
  start(): void;
  stop(): void;
}
"#;
        let chunks = chunks_for_path("lifecycle.ts", source).unwrap();
        let start_method = chunks
            .iter()
            .find(|chunk| chunk.code.contains("start(): void {}"))
            .unwrap();
        let stop_method = chunks
            .iter()
            .find(|chunk| chunk.code.contains("stop(): void {}"))
            .unwrap();
        assert!(
            start_method
                .code
                .starts_with("/** Coordinates telemetry. */")
        );
        assert!(
            start_method
                .code
                .contains("export class TelemetryCoordinator")
        );
        assert!(start_method.code.contains("// Starts collection."));
        assert!(!start_method.code.contains("stop(): void {}"));
        assert!(stop_method.code.contains("// Stops collection."));

        let start_signature = chunks
            .iter()
            .find(|chunk| chunk.code.contains("export interface TelemetryLifecycle"))
            .unwrap();
        let stop_signature = chunks
            .iter()
            .find(|chunk| {
                chunk.code.contains("stop(): void;") && !chunk.code.contains("stop(): void {}")
            })
            .unwrap();
        assert!(start_signature.code.contains("start(): void"));
        assert!(!start_signature.code.contains("stop(): void"));
        assert!(!stop_signature.code.contains("start(): void"));
    }

    #[test]
    fn chunks_tsx_and_javascript_variants_with_correct_languages() {
        let cases = [
            (
                "panel.tsx",
                "tsx",
                "// UI\nexport const TelemetryPanel = () => <section>Live</section>;\n",
                "export const TelemetryPanel",
            ),
            (
                "worker.js",
                "javascript",
                "export function collect() { return 1; }\nclass Worker { run() {} }\nboot();\n",
                "class Worker",
            ),
            (
                "view.jsx",
                "jsx",
                "export const View = () => <main>Ready</main>;\nrender(<View />);\n",
                "render(<View />);",
            ),
        ];
        for (path, language, source, expected) in cases {
            let chunks = chunks_for_path(path, source).unwrap();
            assert!(chunks.iter().all(|chunk| chunk.language == language));
            assert!(chunks.iter().any(|chunk| chunk.code.contains(expected)));
            assert!(chunks.iter().all(|chunk| source.contains(&chunk.code)));
        }
    }

    #[test]
    fn oversized_exported_typescript_class_splits_at_members_with_header_attached() {
        let methods = (0..120)
            .map(|index| {
                format!(
                    "  // member {index}\n  method_{index}() {{ return \"{}\"; }}\n",
                    "value".repeat(50)
                )
            })
            .collect::<String>();
        let source =
            format!("/** Large service. */\n@sealed\nexport class LargeService {{\n{methods}}}\n");
        assert!(source.len() > MAX_DECLARATION_BYTES);
        let chunks = chunks_for_path("large.ts", &source).unwrap();
        assert!(chunks.len() > 1);
        assert!(chunks[0].code.starts_with("/** Large service. */"));
        assert!(chunks[0].code.contains("export class LargeService"));
        for index in 0..120 {
            let method = format!("method_{index}");
            let chunk = chunks
                .iter()
                .find(|chunk| chunk.code.contains(&method))
                .unwrap_or_else(|| panic!("missing {method}"));
            assert!(chunk.code.contains(&format!("// member {index}")));
        }
    }
    #[test]
    fn respects_ignore_without_git_repository() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(temp.path().join("ignored")).unwrap();
        std::fs::write(temp.path().join("ignored/a.rs"), "fn hidden() {} ").unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {} ").unwrap();
        let chunks = scan(temp.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].relative_file_path, "lib.rs");
    }

    #[test]
    fn respects_project_local_lciignore() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible.rs"), "fn visible() {}\n").unwrap();
        std::fs::create_dir_all(temp.path().join(".agents/skills/example")).unwrap();
        std::fs::write(
            temp.path().join(".agents/skills/example/template.rs"),
            "fn skill_template() {}\n",
        )
        .unwrap();
        std::fs::write(temp.path().join(".lciignore"), ".agents/skills/**\n").unwrap();

        let chunks = scan(temp.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].relative_file_path, "visible.rs");
    }

    #[test]
    fn scans_exactly_the_registered_lowercase_extensions() {
        let temp = tempfile::tempdir().unwrap();
        for (name, source) in [
            ("lib.rs", "fn rust_item() {}\n"),
            ("types.ts", "export type Item = string;\n"),
            ("panel.tsx", "export const Panel = () => <main />;\n"),
            ("runtime.js", "export function run() {}\n"),
            ("view.jsx", "export const View = () => <main />;\n"),
            ("Service.cs", "public sealed class Service {}\n"),
            ("ignored.RS", "fn uppercase() {}\n"),
            ("ignored.CS", "public sealed class Ignored {}\n"),
            ("ignored.mts", "export const ignored = 1;\n"),
            ("ignored.py", "ignored = True\n"),
        ] {
            std::fs::write(temp.path().join(name), source).unwrap();
        }
        let chunks = scan(temp.path()).unwrap();
        let files: HashSet<_> = chunks
            .iter()
            .map(|chunk| chunk.relative_file_path.as_str())
            .collect();
        assert_eq!(
            files,
            HashSet::from([
                "lib.rs",
                "types.ts",
                "panel.tsx",
                "runtime.js",
                "view.jsx",
                "ignored.py",
                "Service.cs"
            ])
        );
    }

    #[test]
    fn reparses_only_the_language_with_incompatible_cache_metadata() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn stable() {}\n").unwrap();
        std::fs::write(
            temp.path().join("panel.ts"),
            "export const panel = () => 1;\n",
        )
        .unwrap();
        let first = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let mut cached = first.files;
        cached.get_mut("panel.ts").unwrap().adapter_version = "old-typescript".into();
        let second = scan_incremental(temp.path(), &cached).unwrap();
        assert_eq!(second.parsed_files, 1);
        assert_eq!(second.unchanged_files, 1);
        assert_eq!(second.files["lib.rs"].language, "rust");
        assert_eq!(second.files["panel.ts"].language, "typescript");
    }

    #[test]
    fn keeps_docs_attributes_and_medium_function_together() {
        let body = "    let number = 42;\n".repeat(400);
        let source = format!(
            "/// Translate expressions.\n#[allow(unused)]\nfn translator() {{\n{body}}}\nfn next() {{}}\n"
        );
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].code.starts_with("/// Translate"));
        assert!(chunks[0].code.contains("#[allow(unused)]"));
        assert!(chunks[0].code.contains(&body));
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 404);
    }

    #[test]
    fn oversized_function_splits_at_syntax_without_losing_statements() {
        let statements: Vec<_> = (0..2000)
            .map(|i| format!("    let value_{i} = {i};\n"))
            .collect();
        let source = format!("fn large() {{\n{}}}\n", statements.concat());
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(chunks.len() > 1);
        for statement in statements {
            assert!(chunks.iter().any(|c| c.code.contains(statement.trim())));
        }
        for c in chunks {
            let lines: Vec<_> = source.lines().collect();
            assert!(
                lines[(c.start_line - 1) as usize..c.end_line as usize]
                    .join("\n")
                    .contains(&c.code)
            );
        }
    }

    #[test]
    fn chunks_python_declarations_statements_and_decorators() {
        let source = r#"# module comment
import os
from os import path

TOP = 1

# leading comment
@decorator
def sync_func(a, b=2):
    """docstring"""
    x = a + b
    return x

async def async_func():
    await something()

@class_decorator
class Base:
    """class doc"""
    class_attr = 1

    @method_decorator
    def method(self):
        return self

    async def amethod(self):
        await thing()

if TOP:
    print("top")
"#;
        let chunks = chunks_for_path("service.py", source).unwrap();

        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.language == "python"));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "service.py")
        );

        let sync = chunks
            .iter()
            .find(|chunk| chunk.code.contains("def sync_func"))
            .unwrap();
        assert!(
            sync.code
                .starts_with("# leading comment\n@decorator\ndef sync_func")
        );
        assert_eq!(sync.start_line, 7);
        assert_eq!(sync.end_line, 12);

        let async_function = chunks
            .iter()
            .find(|chunk| chunk.code.contains("async def async_func"))
            .unwrap();
        assert_eq!(async_function.start_line, 14);
        assert_eq!(async_function.end_line, 15);

        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class Base"))
            .unwrap();
        assert!(class.code.starts_with("@class_decorator\nclass Base"));
        assert!(class.code.contains("@method_decorator\n    def method"));
        assert!(class.code.contains("async def amethod"));

        assert!(chunks.iter().any(|chunk| chunk.code == "TOP = 1"));
        assert!(chunks.iter().any(|chunk| chunk.code.contains("if TOP:")));
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("def sync_func"))
                .count(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("class Base"))
                .count(),
            1
        );

        let lines: Vec<_> = source.lines().collect();
        for chunk in chunks {
            assert!(
                lines[(chunk.start_line - 1) as usize..chunk.end_line as usize]
                    .join("\n")
                    .contains(&chunk.code),
                "invalid range for {:#?}",
                chunk
            );
        }
    }

    #[test]
    fn oversized_decorated_python_function_keeps_header_and_statements() {
        let statements: Vec<_> = (0..1600)
            .map(|i| format!("    value_{i} = calculate({i})\n"))
            .collect();
        let source = format!(
            "# explanation\n@trace\nasync def produce():\n{}",
            statements.concat()
        );

        let chunks = chunks_for_path("worker.py", &source).unwrap();
        assert!(chunks.len() > 1);
        assert!(
            chunks[0]
                .code
                .starts_with("# explanation\n@trace\nasync def produce():")
        );
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("async def produce"))
                .count(),
            1
        );
        for statement in statements {
            assert!(
                chunks
                    .iter()
                    .any(|chunk| chunk.code.contains(statement.trim())),
                "missing {statement}"
            );
        }
    }

    #[test]
    fn chunks_csharp_namespaces_types_members_attributes_and_top_level_statements() {
        let source = r#"using System;

Console.WriteLine("startup");

namespace Telemetry;

/// <summary>Processes production telemetry.</summary>
[Obsolete]
public sealed class TelemetryProcessor
{
    public event EventHandler? Completed;
    public string Name { get; init; } = "production";

    public TelemetryProcessor(string name) { Name = name; }

    public string Normalize(string value)
    {
        string Trimmed() => value.Trim();
        return Trimmed().ToLowerInvariant();
    }
}

public interface ITelemetrySink
{
    void Write(string value);
}
"#;
        let chunks = chunks_for_path("TelemetryProcessor.cs", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "csharp"));
        for expected in [
            "using System",
            "Console.WriteLine",
            "class TelemetryProcessor",
            "event EventHandler",
            "string Name",
            "TelemetryProcessor(string name)",
            "string Normalize",
            "string Trimmed()",
            "interface ITelemetrySink",
            "void Write",
        ] {
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(expected)),
                "missing {expected}: {chunks:#?}"
            );
        }
        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class TelemetryProcessor"))
            .unwrap();
        assert!(class.code.contains("/// <summary>"));
        assert!(class.code.contains("[Obsolete]"));
        for chunk in &chunks {
            assert!(source.contains(&chunk.code));
            assert_eq!(chunk.content_hash, hash(&chunk.code));
            assert!(chunk.start_line >= 1);
            assert!(chunk.end_line >= chunk.start_line);
        }
    }

    #[test]
    fn oversized_csharp_method_splits_at_statements_and_keeps_its_header() {
        let statements = (0..1200)
            .map(|i| format!("        total += {i};\n"))
            .collect::<String>();
        let source = format!(
            "public class LargeCalculator\n{{\n    [Obsolete]\n    public int Calculate()\n    {{\n        var total = 0;\n{statements}        return total;\n    }}\n}}\n"
        );
        let chunks = chunks_for_path("LargeCalculator.cs", &source).unwrap();
        assert!(
            chunks.len() > 2,
            "oversized member was not split: {chunks:#?}"
        );
        let header = chunks
            .iter()
            .find(|chunk| chunk.code.contains("public int Calculate"))
            .unwrap();
        assert!(header.code.contains("[Obsolete]"));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("return total;"))
        );
        assert!(chunks.iter().all(|chunk| source.contains(&chunk.code)));
    }

    #[test]
    fn csharp_parser_recovery_keeps_following_declarations_searchable() {
        let source =
            "public class Broken { void Run( { }\npublic record HealthyRecord(int Value);\n";
        let chunks = chunks_for_path("Recovered.cs", source).unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("HealthyRecord")),
            "valid declaration after syntax damage was lost: {chunks:#?}"
        );
    }

    #[test]
    fn chunks_go_declarations_with_leading_comments_attached() {
        let source = r#"package example

// Add returns the sum of two numbers.
func Add(a, b int) int {
	return a + b
}

// Counter tracks a running total.
type Counter struct {
	Total int
}

// Increment adds n to the counter's total.
func (c *Counter) Increment(n int) {
	c.Total += n
}

const MaxRetries = 3

var DefaultTimeout = 30
"#;
        let chunks = chunks_for_path("service.go", source).unwrap();

        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.language == "go"));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "service.go")
        );

        let add = chunks
            .iter()
            .find(|chunk| chunk.code.contains("func Add"))
            .unwrap();
        assert!(
            add.code
                .starts_with("// Add returns the sum of two numbers.\nfunc Add")
        );

        let counter = chunks
            .iter()
            .find(|chunk| chunk.code.contains("type Counter struct"))
            .unwrap();
        assert!(
            counter
                .code
                .starts_with("// Counter tracks a running total.\ntype Counter")
        );

        let increment = chunks
            .iter()
            .find(|chunk| chunk.code.contains("func (c *Counter) Increment"))
            .unwrap();
        assert!(increment.code.starts_with(
            "// Increment adds n to the counter's total.\nfunc (c *Counter) Increment"
        ));

        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("const MaxRetries = 3"))
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("var DefaultTimeout = 30"))
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("func Add"))
                .count(),
            1
        );

        let lines: Vec<_> = source.lines().collect();
        for chunk in chunks {
            assert!(
                lines[(chunk.start_line - 1) as usize..chunk.end_line as usize]
                    .join("\n")
                    .contains(&chunk.code),
                "invalid range for {:#?}",
                chunk
            );
        }
    }

    #[test]
    fn oversized_go_function_splits_at_statements_without_losing_them() {
        let statements: Vec<_> = (0..2000)
            .map(|i| format!("\tvalue{i} := {i}\n\t_ = value{i}\n"))
            .collect();
        let source = format!(
            "package example\n\nfunc large() {{\n{}}}\n",
            statements.concat()
        );
        let chunks = chunks_for_path("large.go", &source).unwrap();
        assert!(chunks.len() > 1);
        for statement in statements {
            for line in statement.lines().filter(|line| !line.trim().is_empty()) {
                assert!(
                    chunks.iter().any(|c| c.code.contains(line.trim())),
                    "missing statement: {line}"
                );
            }
        }
        for c in chunks {
            let lines: Vec<_> = source.lines().collect();
            assert!(
                lines[(c.start_line - 1) as usize..c.end_line as usize]
                    .join("\n")
                    .contains(&c.code)
            );
        }
    }

    #[test]
    fn chunks_java_packages_types_members_annotations_and_javadoc() {
        let source = r#"package com.example.telemetry;

import java.util.List;

/**
 * Processes production telemetry.
 */
@Deprecated
public class TelemetryProcessor {
    private String name = "production";

    public TelemetryProcessor(String name) {
        this.name = name;
    }

    public String normalize(String value) {
        String trimmed = value.trim();
        return trimmed.toLowerCase();
    }
}

interface TelemetrySink {
    void write(String value);
}
"#;
        let chunks = chunks_for_path("TelemetryProcessor.java", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "java"));
        for expected in [
            "import java.util.List",
            "class TelemetryProcessor",
            "private String name",
            "TelemetryProcessor(String name)",
            "String normalize",
            "interface TelemetrySink",
            "void write",
        ] {
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(expected)),
                "missing {expected}: {chunks:#?}"
            );
        }
        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class TelemetryProcessor"))
            .unwrap();
        assert!(class.code.contains("/**"));
        assert!(class.code.contains("@Deprecated"));
        for chunk in &chunks {
            assert!(source.contains(&chunk.code));
            assert_eq!(chunk.content_hash, hash(&chunk.code));
            assert!(chunk.start_line >= 1);
            assert!(chunk.end_line >= chunk.start_line);
        }
    }

    #[test]
    fn oversized_java_method_splits_at_statements_and_keeps_its_header() {
        let statements = (0..1200)
            .map(|i| format!("        total += {i};\n"))
            .collect::<String>();
        let source = format!(
            "public class LargeCalculator {{\n    @Deprecated\n    public int calculate() {{\n        int total = 0;\n{statements}        return total;\n    }}\n}}\n"
        );
        let chunks = chunks_for_path("LargeCalculator.java", &source).unwrap();
        assert!(
            chunks.len() > 2,
            "oversized member was not split: {chunks:#?}"
        );
        let header = chunks
            .iter()
            .find(|chunk| chunk.code.contains("public int calculate"))
            .unwrap();
        assert!(header.code.contains("@Deprecated"));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("return total;"))
        );
        assert!(chunks.iter().all(|chunk| source.contains(&chunk.code)));
    }

    #[test]
    fn java_parser_recovery_keeps_following_declarations_searchable() {
        let source =
            "public class Broken { void run( { }\npublic record HealthyRecord(int value) {}\n";
        let chunks = chunks_for_path("Recovered.java", source).unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("HealthyRecord")),
            "valid declaration after syntax damage was lost: {chunks:#?}"
        );
    }
}
