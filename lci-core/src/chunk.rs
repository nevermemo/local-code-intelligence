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
/// Recursion frames of `ranges`, not syntactic levels: a nested Rust `mod`
/// costs two (`mod_item`, `declaration_list`), so about 64 nested modules.
const MAX_RANGE_DEPTH: usize = 128;
pub const DOCUMENT_FORMAT_VERSION: &str = "rust-chunks-v1";

#[derive(Debug)]
pub struct Scan {
    pub chunks: Vec<Chunk>,
    pub files: HashMap<String, crate::manifest::CachedFile>,
    pub fingerprint: String,
    pub parsed_files: usize,
    pub unchanged_files: usize,
    pub removed_files: usize,
    /// Repository-relative paths of files that were not valid UTF-8 and were
    /// indexed from a lossy decoding (invalid bytes become U+FFFD).
    pub lossy_files: Vec<String>,
    /// Files with a registered source extension that were left out because
    /// their content is a known non-source format (for example a Qt Linguist
    /// catalog saved as `.ts`). They are neither parsed, chunked, hashed nor
    /// recorded in the manifest, so a file that stops being source disappears
    /// from the next snapshot like a deleted one, and the staleness
    /// fingerprint ignores them.
    pub skipped_files: Vec<SkippedFile>,
}

/// A file with a source extension that is not indexed as source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    /// Repository-relative path with `/` separators.
    pub relative_path: String,
    pub reason: language::NonSource,
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
fn ranges(
    adapter: &language::LanguageAdapter,
    node: Node<'_>,
    out: &mut Vec<(usize, usize)>,
    depth: usize,
) {
    // Keep pathological nesting whole rather than exhausting a blocking worker's
    // stack; such a range can exceed MAX_DECLARATION_BYTES, which the embedder
    // already handles by halving rejected inputs.
    if depth >= MAX_RANGE_DEPTH {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
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
    // Prefix children (comments, attributes) attach to the declaration after
    // them. A run after the last declaration has none to attach to; when it is
    // longer than the target its children are packed like any other child
    // instead of staying whole in the chunk before it.
    let trailing_prefix_start = children
        .iter()
        .rposition(|child| !adapter.is_prefix(child.kind()))
        .map_or(0, |index| index + 1);
    let split_trailing_prefix = children
        .get(trailing_prefix_start)
        .zip(children.last())
        .is_some_and(|(first, last)| last.end_byte() - first.start_byte() > TARGET_BYTES);
    let mut start = node.start_byte();
    let mut end = start;
    let mut pending_prefix = None;
    let mut last_split_range = None;
    for (index, child) in children.into_iter().enumerate() {
        if container
            && adapter.is_prefix(child.kind())
            && (!split_trailing_prefix || index < trailing_prefix_start)
        {
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
            ranges(adapter, child, out, depth + 1);
            if let Some(range) = out.get_mut(first) {
                range.0 = if attach_header { start } else { child_start };
            }
            last_split_range = out.len().checked_sub(1).filter(|&index| index >= first);
            start = child.end_byte();
            end = start;
        } else if container || child.end_byte() - start > TARGET_BYTES {
            // The chunk reaches the next child's start, so the bytes between two
            // children (an operator, a comma) stay in a chunk: after a pending
            // run, in it; right after a split child, in that child's last chunk.
            if end > start {
                out.push((start, child_start));
            } else if let Some(index) = last_split_range.take()
                && let Some((_, previous_end)) = out.get_mut(index)
                && *previous_end == start
            {
                *previous_end = child_start;
            }
            last_split_range = None;
            start = child_start;
            end = child.end_byte();
        } else {
            last_split_range = None;
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
    ranges(adapter, tree.root_node(), &mut spans, 0);
    let mut chunks = Vec::new();
    for &(start, end) in &spans {
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
    #[cfg(test)]
    tests::assert_uncovered_bytes_are_expected(adapter, source, &spans);
    Ok(chunks)
}

struct SourceFile {
    relative_path: String,
    source: String,
    content_hash: String,
    adapter: &'static language::LanguageAdapter,
    lossy: bool,
}

/// Decodes source bytes as UTF-8, falling back to a lossy decoding (invalid
/// sequences become U+FFFD) rather than failing. Legacy-encoded files --
/// e.g. Windows-1252 C sources -- are common in real repositories, and one
/// of them must not make the whole workspace unindexable or unsearchable.
/// Line structure is preserved, so line ranges stay valid. One leading byte
/// order mark (U+FEFF) is dropped: it is not text, `trim()` keeps it, and it
/// would otherwise sit in the first chunk's code and in the file's hash.
fn decode_source(bytes: Vec<u8>) -> (String, bool) {
    let (mut source, lossy) = match String::from_utf8(bytes) {
        Ok(source) => (source, false),
        Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
    };
    if source.starts_with('\u{feff}') {
        source.drain(..'\u{feff}'.len_utf8());
    }
    (source, lossy)
}

struct Sources {
    files: Vec<SourceFile>,
    skipped: Vec<SkippedFile>,
}

fn sources(root: &Path) -> Result<Sources> {
    sources_with_read(root, |path| std::fs::read(path))
}

fn sources_with_read(
    root: &Path,
    mut read: impl FnMut(&Path) -> std::io::Result<Vec<u8>>,
) -> Result<Sources> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
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
        let path = entry.path().strip_prefix(root)?;
        let relative = path
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<Vec<_>>>();
        let Some(relative) = relative else {
            tracing::warn!(path = ?path, "skipping source file: non-UTF-8 file path");
            continue;
        };
        let relative = relative.join("/");
        let bytes = match read(entry.path()) {
            Ok(bytes) => bytes,
            // Removed between the walk and the read (a branch switch during a
            // run): not an error. Any other failure aborts the pass, so the
            // previous index is kept rather than silently losing the file.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %relative, %error, "skipping source file removed during the scan");
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("read {relative}")),
        };
        if let Some(reason) = adapter.non_source(&bytes) {
            skipped.push(SkippedFile {
                relative_path: relative,
                reason,
            });
            continue;
        }
        let (source, lossy) = decode_source(bytes);
        let source_hash = hash(&source);
        files.push(SourceFile {
            relative_path: relative,
            source,
            content_hash: source_hash,
            adapter,
            lossy,
        });
    }
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    skipped.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(Sources { files, skipped })
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
        .files
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
    let Sources {
        files: source_files,
        skipped: skipped_files,
    } = sources(root)?;
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
    let lossy_files = source_files
        .iter()
        .filter(|file| file.lossy)
        .map(|file| file.relative_path.clone())
        .collect();
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
        lossy_files,
        skipped_files,
    })
}

pub fn scan(root: &Path) -> Result<Vec<Chunk>> {
    Ok(scan_incremental(root, &HashMap::new())?.chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only check that runs on every chunking call of this crate's unit
    /// tests: a byte that is in no chunk must be whitespace, a separator the
    /// chunker drops on purpose (`{`, `}`, `;`), a leading byte order mark, or a
    /// comment that no chunk carries as a prefix.
    pub(super) fn assert_uncovered_bytes_are_expected(
        adapter: &language::LanguageAdapter,
        source: &str,
        spans: &[(usize, usize)],
    ) {
        let mut covered = vec![false; source.len()];
        for &(start, end) in spans {
            let raw = &source[start..end];
            let code = raw.trim();
            if code.is_empty() || code.chars().all(|c| c.is_whitespace() || "{};".contains(c)) {
                continue;
            }
            let begin = start + raw.len() - raw.trim_start().len();
            covered[begin..begin + code.len()].fill(true);
        }

        let mut parser = Parser::new();
        parser.set_language(&adapter.grammar()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut cursor = tree.walk();
        let mut stack = vec![tree.root_node()];
        let mut comments = Vec::new();
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "line_comment" | "block_comment" | "comment") {
                comments.push((node.kind(), node.start_byte(), node.end_byte()));
            }
            stack.extend(node.children(&mut cursor));
        }
        for (kind, comment_start, comment_end) in comments {
            let attached = spans.iter().any(|&(start, end)| {
                let raw = &source[start..end];
                let code = raw.trim();
                if code.is_empty() || code.chars().all(|c| c.is_whitespace() || "{};".contains(c)) {
                    return false;
                }
                let begin = start + raw.len() - raw.trim_start().len();
                adapter.is_prefix(kind)
                    && begin >= comment_end
                    && source[comment_end..begin].chars().all(char::is_whitespace)
            });
            if attached {
                covered[comment_start..comment_end].fill(true);
            }
        }

        for (offset, character) in source.char_indices() {
            let end = offset + character.len_utf8();
            if covered[offset..end].iter().all(|&byte| byte)
                || character.is_whitespace()
                || "{};".contains(character)
                || (offset == 0 && character == '\u{feff}')
            {
                continue;
            }
            panic!(
                "unexpected uncovered byte at {offset} ({character:?}) in {} source bytes",
                source.len()
            );
        }
    }

    fn compact(source: &str) -> String {
        source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn assert_chunks_cover(chunks: &[Chunk], expected: &str) {
        let packed: String = chunks.iter().map(|chunk| compact(&chunk.code)).collect();
        let expected = compact(expected);
        assert!(
            packed.contains(&expected),
            "chunks do not cover {expected:?}:\n{chunks:#?}"
        );
    }

    fn assert_non_whitespace_coverage(source: &str, expected: &str) {
        let chunks = rust_chunks("lib.rs", source).unwrap();
        assert_chunks_cover(&chunks, expected);
    }

    #[test]
    fn ordinary_nested_modules_keep_exact_chunks() {
        let depth = 32;
        let source = format!(
            "{}fn leaf() {{}}\n{}fn sibling() {{}}\n",
            "mod m {\n".repeat(depth),
            "}\n".repeat(depth)
        );
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        let mut expected: Vec<_> = (1..=depth as u32)
            .map(|line| (line, line, "mod m"))
            .collect();
        expected.push((32, 34, "{\nfn leaf() {}\n}"));
        expected.push((66, 66, "fn sibling() {}"));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
                .collect::<Vec<_>>(),
            expected
        );
        for chunk in chunks {
            assert_eq!(chunk.relative_file_path, "lib.rs");
            assert_eq!(chunk.language, "rust");
            assert_eq!(chunk.content_hash, hash(&chunk.code));
        }
    }

    #[test]
    fn deeply_nested_modules_on_default_blocking_stack() {
        const CHILD_DEPTH: &str = "LCI_TEST_NESTING_DEPTH";
        if let Ok(depth) = std::env::var(CHILD_DEPTH) {
            let depth: usize = depth.parse().unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::task::spawn_blocking(move || {
                    let source = format!(
                        "{}fn leaf() {{}}\n{}fn sibling() {{}}\n",
                        "mod m {\n".repeat(depth),
                        "}\n".repeat(depth)
                    );
                    let chunks = rust_chunks("deep.rs", &source).unwrap();
                    assert_chunks_cover(&chunks, "fn leaf() {}");
                    assert_chunks_cover(&chunks, "fn sibling() {}");
                    assert_eq!(
                        chunks
                            .iter()
                            .map(|chunk| chunk.code.matches("mod m").count())
                            .sum::<usize>(),
                        depth
                    );
                    for chunk in chunks {
                        assert_eq!(chunk.content_hash, hash(&chunk.code));
                        assert!(source.contains(&chunk.code));
                    }
                })
                .await
                .unwrap();
            });
            return;
        }

        // Isolate stack overflow in a child so the regression cannot abort
        // the rest of the test suite.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "chunk::tests::deeply_nested_modules_on_default_blocking_stack",
                "--nocapture",
            ])
            .env(CHILD_DEPTH, "32768")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "default-stack chunking failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // A renamed test would make the child run nothing and pass.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child ran no test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

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
    fn leading_bom_does_not_change_rust_typescript_or_csharp_chunks() {
        let fixtures = [
            ("lib.rs", "fn rust_value() { let _ = 1; }\n"),
            ("types.ts", "export const typescriptValue = 1;\n"),
            ("Service.cs", "public sealed class CsharpService {}\n"),
        ];
        let without_bom = tempfile::tempdir().unwrap();
        let with_bom = tempfile::tempdir().unwrap();
        for (path, source) in fixtures {
            std::fs::write(without_bom.path().join(path), source).unwrap();
            std::fs::write(with_bom.path().join(path), format!("\u{feff}{source}")).unwrap();
        }

        let without_bom = scan_incremental(without_bom.path(), &HashMap::new()).unwrap();
        let with_bom = scan_incremental(with_bom.path(), &HashMap::new()).unwrap();
        let details = |chunks: &[Chunk]| {
            chunks
                .iter()
                .map(|chunk| {
                    (
                        chunk.relative_file_path.clone(),
                        chunk.language.clone(),
                        chunk.start_line,
                        chunk.end_line,
                        chunk.code.clone(),
                        chunk.content_hash.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            details(&with_bom.chunks),
            details(&without_bom.chunks),
            "chunks with BOM: {:#?}\nchunks without BOM: {:#?}",
            with_bom.chunks,
            without_bom.chunks
        );
        assert!(with_bom.chunks.iter().all(|chunk| chunk.code != "\u{feff}"));
        let differing_source_hashes: Vec<_> = fixtures
            .iter()
            .filter_map(|(path, _)| {
                let with_bom_hash = &with_bom.files[*path].content_hash;
                let without_bom_hash = &without_bom.files[*path].content_hash;
                (with_bom_hash != without_bom_hash).then_some((
                    *path,
                    with_bom_hash,
                    without_bom_hash,
                ))
            })
            .collect();
        assert!(
            differing_source_hashes.is_empty(),
            "decoded source hashes differ by language: {differing_source_hashes:#?}"
        );
    }

    #[test]
    fn decode_source_drops_one_leading_byte_order_mark_and_keeps_the_lossy_flag() {
        assert_eq!(
            decode_source(b"\xEF\xBB\xBFfn a() {}\n".to_vec()),
            ("fn a() {}\n".to_owned(), false)
        );
        assert_eq!(
            decode_source(b"fn a() {}\n".to_vec()),
            ("fn a() {}\n".to_owned(), false)
        );
        assert_eq!(
            decode_source(b"\xEF\xBB\xBF".to_vec()),
            (String::new(), false)
        );
        // Only the first mark is the byte order mark; a second one is content.
        assert_eq!(
            decode_source("\u{feff}\u{feff}x".as_bytes().to_vec()),
            ("\u{feff}x".to_owned(), false)
        );
        // A mark in the middle is content too.
        assert_eq!(
            decode_source("a\u{feff}b".as_bytes().to_vec()),
            ("a\u{feff}b".to_owned(), false)
        );
        // Invalid UTF-8 after the mark: the file is lossy, and the mark still goes.
        assert_eq!(
            decode_source(b"\xEF\xBB\xBFlet a = \xFF;\n".to_vec()),
            ("let a = \u{fffd};\n".to_owned(), true)
        );
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

    // Linux only: APFS refuses to create a file whose name is not valid UTF-8.
    #[test]
    #[cfg(target_os = "linux")]
    fn scan_skips_non_utf8_paths_and_indexes_other_files() {
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::tempdir().unwrap();
        let invalid = std::ffi::OsStr::from_bytes(b"invalid-\xff.rs");
        std::fs::write(temp.path().join(invalid), "fn invisible() {}\n").unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();

        let result = scan_incremental(temp.path(), &HashMap::new());
        assert!(
            result.is_ok(),
            "one non-UTF-8 path failed the scan: {result:?}"
        );
        let scan = result.unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.chunks.len(), 1);
        assert_eq!(scan.chunks[0].relative_file_path, "lib.rs");
        assert!(scan.chunks[0].code.contains("visible"));
        assert_eq!(fingerprint(temp.path()).unwrap(), scan.fingerprint);
        let repeated = scan_incremental(temp.path(), &scan.files).unwrap();
        assert_eq!(repeated.unchanged_files, 1);
        assert_eq!(repeated.parsed_files, 0);
    }

    #[test]
    fn scan_skips_a_file_removed_between_discovery_and_read() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("vanishing.rs");
        std::fs::write(&victim, "fn vanishing() {}\n").unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();
        let previous = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let mut removed = false;

        let result = sources_with_read(temp.path(), |path| {
            if path == victim {
                std::fs::remove_file(path).unwrap();
                removed = true;
            }
            std::fs::read(path)
        });
        assert!(removed, "the walker must discover the file before removal");
        assert!(result.is_ok(), "one disappearing file failed the scan");
        let sources = result.unwrap();
        assert_eq!(sources.files.len(), 1);
        assert_eq!(sources.files[0].relative_path, "lib.rs");
        assert!(sources.files[0].source.contains("visible"));
        let scan = scan_incremental(temp.path(), &previous.files).unwrap();
        assert_eq!(scan.removed_files, 1);
        assert_eq!(scan.unchanged_files, 1);
        assert_eq!(scan.chunks.len(), 1);
        assert!(scan.chunks[0].code.contains("visible"));
        assert_eq!(fingerprint(temp.path()).unwrap(), scan.fingerprint);
    }

    #[test]
    fn scan_fails_on_a_read_error_other_than_a_removed_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();

        let result = sources_with_read(temp.path(), |_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });

        let Err(error) = result else {
            panic!("an unreadable file must abort the scan, not be skipped");
        };
        assert!(format!("{error:#}").contains("read lib.rs"), "{error:#}");
    }

    #[test]
    fn non_utf8_files_are_indexed_lossily_instead_of_failing_the_scan() {
        let temp = tempfile::tempdir().unwrap();
        // Windows-1252 "café" (0xE9 is not valid UTF-8 on its own).
        std::fs::write(
            temp.path().join("legacy.c"),
            b"/* caf\xe9 */\nint legacy_value(void) { return 7; }\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();

        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        assert_eq!(scan.lossy_files, vec!["legacy.c".to_owned()]);
        let legacy = scan
            .chunks
            .iter()
            .find(|chunk| chunk.relative_file_path == "legacy.c")
            .expect("the legacy file is still indexed");
        assert!(legacy.code.contains("caf\u{FFFD}"), "{}", legacy.code);
        assert!(legacy.code.contains("legacy_value"));
        // The staleness fingerprint no longer fails on the same file.
        assert_eq!(fingerprint(temp.path()).unwrap(), scan.fingerprint);
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

    const QT_LINGUIST_CATALOG: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE TS>\n<TS version=\"2.1\" language=\"de\">\n<context>\n    <name>SendCoinsDialog</name>\n    <message>\n        <source>Send coins</source>\n        <translation>Muenzen senden</translation>\n    </message>\n</context>\n</TS>\n";

    #[test]
    fn qt_linguist_files_are_skipped_while_typescript_is_indexed() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("locale")).unwrap();
        std::fs::write(
            temp.path().join("send.ts"),
            "export function sendCoins() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("locale/app_de.ts"), QT_LINGUIST_CATALOG).unwrap();

        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        assert_eq!(
            scan.files.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["send.ts"]
        );
        assert_eq!(scan.files["send.ts"].language, "typescript");
        assert!(
            scan.chunks
                .iter()
                .all(|chunk| chunk.language == "typescript"
                    && chunk.relative_file_path == "send.ts"
                    && !chunk.code.contains("Muenzen"))
        );
        assert_eq!(scan.parsed_files, 1);
        assert_eq!(
            scan.skipped_files,
            vec![SkippedFile {
                relative_path: "locale/app_de.ts".into(),
                reason: language::NonSource::QtLinguist,
            }]
        );
        assert!(scan.lossy_files.is_empty());
    }

    #[test]
    fn skipped_files_do_not_change_the_staleness_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("send.ts"), "export const a = 1;\n").unwrap();
        let without = fingerprint(temp.path()).unwrap();
        std::fs::write(temp.path().join("app_de.ts"), QT_LINGUIST_CATALOG).unwrap();
        assert_eq!(fingerprint(temp.path()).unwrap(), without);
        std::fs::write(
            temp.path().join("app_de.ts"),
            QT_LINGUIST_CATALOG.replace("Muenzen", "Coins"),
        )
        .unwrap();
        assert_eq!(fingerprint(temp.path()).unwrap(), without);
        // Once the file turns out to be TypeScript the fingerprint moves.
        std::fs::write(temp.path().join("app_de.ts"), "export const b = 2;\n").unwrap();
        assert_ne!(fingerprint(temp.path()).unwrap(), without);
    }

    #[test]
    fn a_file_reclassified_as_qt_linguist_drops_its_cached_chunks() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("send.ts"), "export const send = 1;\n").unwrap();
        std::fs::write(
            temp.path().join("catalog.ts"),
            "export function translatorOnly() { return 1; }\n",
        )
        .unwrap();
        let first = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        assert!(first.skipped_files.is_empty());
        assert!(
            first
                .chunks
                .iter()
                .any(|chunk| chunk.relative_file_path == "catalog.ts")
        );

        std::fs::write(temp.path().join("catalog.ts"), QT_LINGUIST_CATALOG).unwrap();
        let second = scan_incremental(temp.path(), &first.files).unwrap();
        assert_eq!(second.removed_files, 1);
        assert_eq!(second.unchanged_files, 1);
        assert_eq!(second.parsed_files, 0);
        assert!(!second.files.contains_key("catalog.ts"));
        assert!(
            second
                .chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "send.ts")
        );
        assert_eq!(second.skipped_files.len(), 1);
        assert_ne!(second.fingerprint, first.fingerprint);

        // And back: the same path is parsed again as TypeScript.
        std::fs::write(
            temp.path().join("catalog.ts"),
            "export function translatorOnly() { return 1; }\n",
        )
        .unwrap();
        let third = scan_incremental(temp.path(), &second.files).unwrap();
        assert_eq!(third.parsed_files, 1);
        assert!(third.skipped_files.is_empty());
        assert_eq!(third.files["catalog.ts"].language, "typescript");
    }

    #[test]
    fn qt_linguist_edge_cases_keep_real_typescript_indexed() {
        let temp = tempfile::tempdir().unwrap();
        let bom_catalog = format!("\u{feff}{QT_LINGUIST_CATALOG}");
        let blank_then_catalog = format!("\n\n  \t{QT_LINGUIST_CATALOG}");
        for (name, content) in [
            ("bom.ts", bom_catalog.as_str()),
            ("blank_then_catalog.ts", blank_then_catalog.as_str()),
            (
                "root_only.ts",
                "<TS version=\"2.1\" language=\"fr\">\n</TS>\n",
            ),
            (
                "comment.ts",
                "// <TS version=\"2.1\"> is the Qt Linguist root element\nexport const note = 1;\n",
            ),
            (
                "jsdoc.ts",
                "/** Reads <?xml prologs. */\nexport function readProlog() {}\n",
            ),
            ("assertion.ts", "<TS>value;\n"),
            (
                "generic_arrow.ts",
                "<TS extends object>(item: TS) => item;\n",
            ),
            ("bom_typescript.ts", "\u{feff}export const bomValue = 1;\n"),
            ("empty.ts", ""),
            ("blank.ts", "\n  \n"),
        ] {
            std::fs::write(temp.path().join(name), content).unwrap();
        }
        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let skipped: Vec<_> = scan
            .skipped_files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect();
        assert_eq!(skipped, ["blank_then_catalog.ts", "bom.ts", "root_only.ts"]);
        let mut indexed: Vec<_> = scan.files.keys().map(String::as_str).collect();
        indexed.sort_unstable();
        assert_eq!(
            indexed,
            [
                "assertion.ts",
                "blank.ts",
                "bom_typescript.ts",
                "comment.ts",
                "empty.ts",
                "generic_arrow.ts",
                "jsdoc.ts"
            ]
        );
        // Empty and blank files are indexed (as zero chunks), not skipped.
        assert!(
            scan.chunks
                .iter()
                .all(|chunk| !matches!(chunk.relative_file_path.as_str(), "empty.ts" | "blank.ts"))
        );
        assert!(
            scan.chunks
                .iter()
                .any(|chunk| chunk.relative_file_path == "comment.ts"
                    && chunk.code.contains("export const note"))
        );
    }

    #[test]
    fn utf16_qt_linguist_catalogs_are_skipped_instead_of_decoded_lossily() {
        fn utf16(text: &str, big_endian: bool) -> Vec<u8> {
            std::iter::once(0xFEFF_u16)
                .chain(text.encode_utf16())
                .flat_map(|unit| {
                    if big_endian {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    }
                })
                .collect()
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("app_de_le.ts"),
            utf16(QT_LINGUIST_CATALOG, false),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("app_de_be.ts"),
            utf16(QT_LINGUIST_CATALOG, true),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("send.ts"),
            "export function sendCoins() { return 1; }\n",
        )
        .unwrap();

        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let skipped: Vec<_> = scan
            .skipped_files
            .iter()
            .map(|file| (file.relative_path.as_str(), file.reason))
            .collect();
        assert_eq!(
            skipped,
            [
                ("app_de_be.ts", language::NonSource::QtLinguist),
                ("app_de_le.ts", language::NonSource::QtLinguist)
            ]
        );
        // Skipped before decoding: neither catalog is reported as lossy.
        assert!(scan.lossy_files.is_empty());
        assert_eq!(scan.files.keys().collect::<Vec<_>>(), ["send.ts"]);
        assert!(
            scan.chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "send.ts")
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
    fn oversized_trailing_comment_run_splits_at_syntax_boundaries() {
        let body = "    let value = 42;\n".repeat(2000);
        assert!(body.len() > MAX_DECLARATION_BYTES);
        let comment = format!("// {}\n", "x".repeat(56));
        assert_eq!(comment.len(), 60);
        let trailing = comment.repeat(2 * TARGET_BYTES / comment.len());
        assert_eq!(trailing.len(), 2 * TARGET_BYTES);
        for declaration in [
            format!("fn large() {{\n{body}}}\n"),
            "fn small() {}\n".into(),
            String::new(),
        ] {
            let source = format!("impl Example {{\n{declaration}{trailing}}}\n");
            let chunks = rust_chunks("lib.rs", &source).unwrap();
            assert!(
                chunks.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES),
                "chunk lengths exceed target {TARGET_BYTES}: {:?}",
                chunks
                    .iter()
                    .map(|chunk| chunk.code.len())
                    .collect::<Vec<_>>()
            );
            assert_chunks_cover(&chunks, &trailing);
            assert_chunks_cover(&chunks, &declaration);
            for chunk in chunks {
                assert!(source.contains(&chunk.code));
                assert_eq!(chunk.content_hash, hash(&chunk.code));
                let lines: Vec<_> = source.lines().collect();
                assert!(
                    lines[(chunk.start_line - 1) as usize..chunk.end_line as usize]
                        .join("\n")
                        .contains(&chunk.code)
                );
            }
        }
    }

    #[test]
    fn short_trailing_comments_keep_existing_boundaries() {
        let source = "impl Example {\nfn small() {}\n// tail\n}\n";
        let chunks = rust_chunks("lib.rs", source).unwrap();
        let boundaries: Vec<_> = chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
            .collect();
        assert_eq!(
            boundaries,
            [
                (1, 1, "impl Example"),
                (1, 4, "{\nfn small() {}\n// tail\n}")
            ]
        );
    }

    #[test]
    fn oversized_trailing_comments_split_for_every_adapter() {
        for (path, declaration, marker) in [
            ("lib.rs", "fn small() {}\n", "//"),
            ("small.ts", "function small() {}\n", "//"),
            ("small.tsx", "function small() {}\n", "//"),
            ("small.js", "function small() {}\n", "//"),
            ("small.jsx", "function small() {}\n", "//"),
            ("small.py", "def small():\n    pass\n", "#"),
            ("Small.cs", "class Small {}\n", "//"),
            ("small.go", "package small\nfunc small() {}\n", "//"),
            ("Small.java", "class Small {}\n", "//"),
            ("small.c", "void small() {}\n", "//"),
            ("small.cpp", "void small() {}\n", "//"),
            ("small.hpp", "void small() {}\n", "//"),
        ] {
            let comment = format!("{marker} tail\n");
            let trailing = comment.repeat(2 * TARGET_BYTES / comment.len() + 1);
            let source = format!("{declaration}{trailing}");
            let chunks = chunks_for_path(path, &source).unwrap();
            assert!(
                chunks.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES),
                "{path} has an oversized trailing chunk"
            );
            assert_chunks_cover(&chunks, &trailing);
        }
    }

    #[test]
    fn indivisible_trailing_comment_remains_whole() {
        let comment = format!("/* {} */", "é".repeat(TARGET_BYTES));
        let source = format!("impl Example {{\nfn small() {{}}\n{comment}\n}}\n");
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(chunks.iter().any(|chunk| chunk.code.contains(&comment)));
    }

    #[test]
    fn leading_comments_stay_attached_when_a_long_trailing_run_splits() {
        let comment = format!("// {}\n", "x".repeat(56));
        let trailing = comment.repeat(2 * TARGET_BYTES / comment.len());
        let source = format!(
            "impl Example {{\n    /// Documented.\n    fn documented() {{}}\n{trailing}}}\n"
        );
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(
            chunks.iter().any(|chunk| {
                chunk.code.contains("/// Documented.") && chunk.code.contains("fn documented")
            }),
            "the doc comment left its function: {:?}",
            chunks
                .iter()
                .map(|chunk| &chunk.code[..chunk.code.len().min(40)])
                .collect::<Vec<_>>()
        );
        assert!(chunks.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES));
        assert_chunks_cover(&chunks, &trailing);
    }

    #[test]
    fn trailing_comment_run_at_the_target_is_not_split_by_the_closing_brace() {
        let comment = format!("// {}\n", "x".repeat(TARGET_BYTES / 2 - 4));
        assert_eq!(2 * comment.len(), TARGET_BYTES);
        let source = format!("impl Example {{\nfn small() {{}}\n{comment}{comment}}}\n");
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(
            chunks.iter().any(|chunk| chunk
                .code
                .contains(&format!("{comment}{}", comment.trim_end()))),
            "a run of exactly the target was split: {:?}",
            chunks
                .iter()
                .map(|chunk| chunk.code.len())
                .collect::<Vec<_>>()
        );
    }

    /// The traced example of N22: an oversized call whose two arguments are
    /// joined by `+`. The operator lies between the two split children and used
    /// to be in no chunk.
    #[test]
    fn oversized_expression_keeps_the_traced_operator() {
        let left = "a".repeat(3500);
        let right = "b".repeat(3500);
        let expression = format!("emit(\"{left}\" + \"{right}\")");
        let statements = format!("    {expression};\n").repeat(4);
        let source = format!("fn main() {{\n{statements}}}\n");

        assert_non_whitespace_coverage(&source, &expression);
    }

    #[test]
    fn oversized_containers_cover_non_whitespace_bytes_across_inputs() {
        let left = "a".repeat(3500);
        let middle = "b".repeat(3500);
        let right = "c".repeat(3500);
        let expressions = [
            format!("emit([\"{left}\", \"{middle}\"])"),
            format!("emit((\"{left}\", \"{middle}\"))"),
            format!("emit(\"{left}\" + \"{middle}\" + \"{right}\")"),
        ];

        for expression in expressions {
            let statements = format!("    {expression};\n").repeat(4);
            let source = format!("fn main() {{\n{statements}}}\n");
            assert_non_whitespace_coverage(&source, &expression);
        }
    }

    #[test]
    fn split_children_keep_separators_before_following_small_siblings() {
        let large = "a".repeat(6500);
        for expression in [
            format!("consume(\"{large}\", short_name)"),
            format!("[\"{large}\", short_name]"),
            format!("(\"{large}\", short_name)"),
        ] {
            let statements = format!("    {expression};\n").repeat(4);
            let source = format!("fn main() {{\n{statements}}}\n");
            assert_non_whitespace_coverage(&source, &expression);
        }

        let mut properties: String = (0..1200)
            .map(|index| format!("  field_{index}: string,\n"))
            .collect();
        properties.push_str(&format!("  oversized: \"{large}\",\n  short: string,\n"));
        let source = format!("type Fields = {{\n{properties}}};\n");
        let chunks = chunks_for_path("fields.ts", &source).unwrap();
        assert_chunks_cover(&chunks, &format!("\"{large}\", short: string"));
        assert!(
            chunks
                .iter()
                .all(|chunk| !matches!(chunk.code.trim(), "," | ";")),
            "a chunk contains only separators:\n{chunks:#?}"
        );
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

    #[test]
    fn chunks_c_declarations_with_leading_comments_attached() {
        let source = r#"#include <stdio.h>

// Add returns the sum of two numbers.
int add(int a, int b) {
    return a + b;
}

// Counter tracks a running total.
struct Counter {
    int total;
};

// increment adds n to the counter's total.
void increment(struct Counter *c, int n) {
    c->total += n;
}

#define MAX_RETRIES 3

int default_timeout = 30;
"#;
        let chunks = chunks_for_path("service.c", source).unwrap();

        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.language == "c"));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "service.c")
        );

        let add = chunks
            .iter()
            .find(|chunk| chunk.code.contains("int add"))
            .unwrap();
        assert!(
            add.code
                .starts_with("// Add returns the sum of two numbers.\nint add")
        );

        let counter = chunks
            .iter()
            .find(|chunk| chunk.code.contains("struct Counter"))
            .unwrap();
        assert!(
            counter
                .code
                .starts_with("// Counter tracks a running total.\nstruct Counter")
        );

        let increment = chunks
            .iter()
            .find(|chunk| chunk.code.contains("void increment"))
            .unwrap();
        assert!(
            increment
                .code
                .starts_with("// increment adds n to the counter's total.\nvoid increment")
        );

        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("#define MAX_RETRIES 3"))
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("int default_timeout = 30;"))
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("int add"))
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
    fn oversized_c_function_splits_at_statements_without_losing_them() {
        let statements: Vec<_> = (0..2000)
            .map(|i| format!("\tint value{i} = {i};\n\t(void)value{i};\n"))
            .collect();
        let source = format!(
            "int large(void) {{\n{}\treturn 0;\n}}\n",
            statements.concat()
        );
        let chunks = chunks_for_path("large.c", &source).unwrap();
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
    fn c_parser_recovery_keeps_following_declarations_searchable() {
        let source = "int broken( {\nint healthy_value(void) { return 42; }\n";
        let chunks = chunks_for_path("recovered.c", source).unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("healthy_value")),
            "valid declaration after syntax damage was lost: {chunks:#?}"
        );
    }

    #[test]
    fn chunks_cpp_namespaces_classes_members_templates_and_leading_comments() {
        let source = r#"namespace telemetry {

// Processes production telemetry events.
class Processor {
public:
    // Normalizes a single event value.
    std::string normalize(std::string value) {
        return value;
    }

private:
    int count_;
};

// add returns the sum of two values of any numeric type.
template<typename T>
T add(T a, T b) {
    return a + b;
}

}
"#;
        let chunks = chunks_for_path("processor.cpp", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "cpp"));
        for expected in [
            "class Processor",
            "std::string normalize",
            "int count_;",
            "T add(T a, T b)",
        ] {
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(expected)),
                "missing {expected}: {chunks:#?}"
            );
        }

        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class Processor"))
            .unwrap();
        assert!(
            class
                .code
                .contains("// Processes production telemetry events.")
        );

        let template_fn = chunks
            .iter()
            .find(|chunk| chunk.code.contains("T add(T a, T b)"))
            .unwrap();
        assert!(template_fn.code.contains("template<typename T>"));
        assert!(
            template_fn
                .code
                .contains("// add returns the sum of two values of any numeric type.")
        );

        for chunk in &chunks {
            assert!(source.contains(&chunk.code));
            assert_eq!(chunk.content_hash, hash(&chunk.code));
            assert!(chunk.start_line >= 1);
            assert!(chunk.end_line >= chunk.start_line);
        }
    }

    #[test]
    fn oversized_cpp_method_splits_at_statements_and_keeps_its_header() {
        let statements = (0..1200)
            .map(|i| format!("        total += {i};\n"))
            .collect::<String>();
        let source = format!(
            "class LargeCalculator {{\npublic:\n    int calculate() {{\n        int total = 0;\n{statements}        return total;\n    }}\n}};\n"
        );
        let chunks = chunks_for_path("LargeCalculator.cpp", &source).unwrap();
        assert!(
            chunks.len() > 2,
            "oversized member was not split: {chunks:#?}"
        );
        let header = chunks
            .iter()
            .find(|chunk| chunk.code.contains("int calculate"))
            .unwrap();
        assert!(header.code.contains("int calculate"));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("return total;"))
        );
        assert!(chunks.iter().all(|chunk| source.contains(&chunk.code)));
    }

    #[test]
    fn cpp_parser_recovery_keeps_following_declarations_searchable() {
        let source = "class Broken { void run( { }\nint healthy_value() { return 42; }\n";
        let chunks = chunks_for_path("recovered.cpp", source).unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("healthy_value")),
            "valid declaration after syntax damage was lost: {chunks:#?}"
        );
    }

    #[test]
    fn chunks_hpp_extension_as_cpp() {
        let source = "class Widget {\npublic:\n    int size() const { return 0; }\n};\n";
        let chunks = chunks_for_path("widget.hpp", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "cpp"));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("class Widget"))
        );
    }
}
