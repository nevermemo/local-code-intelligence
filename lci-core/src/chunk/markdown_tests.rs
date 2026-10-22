//! Tests of the Markdown extractor (`markdown.rs`): headings, fences, front
//! matter, packing, exact text and line ranges, and the scan around it.

use super::*;

/// Test-only check run on every Markdown chunking call of this crate's unit
/// tests: chunks are in order and disjoint, each is the exact text of its
/// lines (original line endings inside, none after the last line), its hash is
/// the hash of its text, and every non-blank line of the source is in a chunk.
pub(super) fn assert_lines_covered(source: &str, chunks: &[Chunk]) {
    let mut lines: Vec<(usize, usize, bool)> = Vec::new();
    let mut offset = 0;
    for raw in source.split_inclusive('\n') {
        let text = raw.strip_suffix('\n').unwrap_or(raw);
        let text = text.strip_suffix('\r').unwrap_or(text);
        lines.push((offset, offset + text.len(), text.trim().is_empty()));
        offset += raw.len();
    }
    let mut covered = vec![false; lines.len()];
    let mut previous_end = 0;
    for chunk in chunks {
        assert!(chunk.start_line >= 1, "{chunk:?}");
        assert!(chunk.start_line <= chunk.end_line, "{chunk:?}");
        assert!(chunk.start_line > previous_end, "chunks overlap: {chunk:?}");
        previous_end = chunk.end_line;
        let (first, last) = (chunk.start_line as usize - 1, chunk.end_line as usize - 1);
        assert!(last < lines.len(), "{chunk:?}");
        assert_eq!(chunk.code, &source[lines[first].0..lines[last].1]);
        assert_eq!(chunk.content_hash, hash(&chunk.code));
        assert!(!lines[first].2 && !lines[last].2, "blank edge: {chunk:?}");
        covered[first..=last].fill(true);
    }
    for (index, &(_, _, blank)) in lines.iter().enumerate() {
        assert!(
            blank || covered[index],
            "line {} is in no chunk of {chunks:#?}",
            index + 1
        );
    }
}

fn split(source: &str) -> Vec<Chunk> {
    chunks_for_path("doc.md", source).unwrap()
}

fn ranges(source: &str) -> Vec<(u32, u32)> {
    split(source)
        .iter()
        .map(|chunk| (chunk.start_line, chunk.end_line))
        .collect()
}

fn texts(source: &str) -> Vec<String> {
    split(source).into_iter().map(|chunk| chunk.code).collect()
}

#[test]
fn atx_headings_of_every_level_start_flat_sections() {
    let source = "# One\ntext one\n\n## Two\ntext two\n\n###### Six\ntext six\n\n### Three\n";
    assert_eq!(ranges(source), [(1, 2), (4, 5), (7, 8), (10, 10)]);
    // A section runs to the next heading of any level: the subsection is not
    // part of its parent.
    assert_eq!(texts(source)[0], "# One\ntext one");
}

#[test]
fn only_real_atx_headings_split() {
    // Seven hashes, no space after the hashes, four spaces of indent, a tab
    // indent and a hash inside a paragraph are text, not headings.
    let text = "#######\n#hashtag\n    # indented code\n\t# tab\nword # word\n";
    assert_eq!(ranges(text), [(1, 5)]);
    // Up to three spaces of indent, closing hashes and an empty heading are
    // headings.
    let headings = "   # indented\nbody\n## closed ##\nbody\n#\nbody\n";
    assert_eq!(ranges(headings), [(1, 2), (3, 4), (5, 6)]);
}

#[test]
fn setext_headings_start_at_their_first_paragraph_line() {
    let source = "intro line\n\nTitle\n=====\nbody\n\nA two line\nsubtitle\n--------\nbody two\n";
    // Preamble, then two sections; a multi-line paragraph is one heading.
    assert_eq!(ranges(source), [(1, 1), (3, 5), (7, 10)]);
    assert_eq!(texts(source)[2], "A two line\nsubtitle\n--------\nbody two");
}

#[test]
fn a_dash_line_that_is_not_under_a_paragraph_is_not_a_heading() {
    // After a blank line it is a thematic break; after a list item or a quote
    // it belongs to them; after an ATX heading it is a break.
    let source = "text\n\n---\nmore\n\n- item\n---\n\n> quote\n---\n\n# H\n---\nafter\n";
    assert_eq!(ranges(source), [(1, 10), (12, 14)]);
}

#[test]
fn headings_inside_fenced_code_are_ignored() {
    let source =
        "# Real\n```sh\n# comment, not a heading\n```\n~~~\n## also not\n~~~\n\n## Next\ntext\n";
    assert_eq!(ranges(source), [(1, 7), (9, 10)]);
    // A longer closing fence closes; a shorter one, or a different character,
    // does not; a fence closed by a four-space-indented line stays open.
    let nested = "# A\n````\n```\n# inside\n~~~~\n    ````\n````\n## B\n";
    assert_eq!(ranges(nested), [(1, 7), (8, 8)]);
    // A backtick fence cannot have a backtick in its info string, so this is
    // inline code, and the heading after it counts.
    let inline = "# A\n```not a fence```\n# B\n";
    assert_eq!(ranges(inline), [(1, 2), (3, 3)]);
}

#[test]
fn an_unclosed_fence_runs_to_the_end_of_the_file() {
    let source = "# A\ntext\n```\n# swallowed\n\nstill code\n";
    assert_eq!(ranges(source), [(1, 6)]);
}

#[test]
fn front_matter_belongs_to_the_first_section() {
    let source = "---\ntitle: x\n# not a heading\n---\n\n# First\nbody\n\n## Second\nbody\n";
    assert_eq!(ranges(source), [(1, 7), (9, 10)]);
    assert!(texts(source)[0].starts_with("---\ntitle: x\n# not a heading\n---\n\n# First\n"));
}

#[test]
fn front_matter_belongs_to_the_preamble_when_text_precedes_the_first_heading() {
    let source = "---\nname: x\n---\n\nWelcome text.\n\n# First\nbody\n";
    assert_eq!(ranges(source), [(1, 5), (7, 8)]);
    assert_eq!(texts(source)[0], "---\nname: x\n---\n\nWelcome text.");
}

#[test]
fn front_matter_alone_or_without_headings_is_one_unit() {
    assert_eq!(ranges("---\nname: x\n---\n"), [(1, 3)]);
    assert_eq!(ranges("---\nname: x\n---\n\nprose\n"), [(1, 5)]);
}

#[test]
fn a_first_dash_line_without_a_closing_one_is_not_front_matter() {
    let source = "---\n# Heading\nbody\n";
    assert_eq!(ranges(source), [(1, 1), (2, 3)]);
    // The dashes are a preamble unit; a closing line later makes it a block.
    assert_eq!(texts(source)[0], "---");
}

#[test]
fn text_before_the_first_heading_is_a_unit_and_blank_edges_are_dropped() {
    let source = "\n\nintro\nmore intro\n\n\n# H\nbody\n\n\n";
    assert_eq!(ranges(source), [(3, 4), (7, 8)]);
    assert_eq!(texts(source), ["intro\nmore intro", "# H\nbody"]);
}

#[test]
fn a_file_without_headings_is_one_unit_and_an_empty_file_has_none() {
    assert_eq!(ranges("just\n\nprose\n"), [(1, 3)]);
    assert!(split("").is_empty());
    assert!(split("\n  \n\t\n").is_empty());
}

#[test]
fn a_heading_without_a_body_is_its_own_unit() {
    assert_eq!(
        texts("# Title\n## Sub\ntext\n"),
        ["# Title", "## Sub\ntext"]
    );
}

#[test]
fn chunk_text_is_exact_with_one_based_inclusive_lines() {
    let source = "# Café ☕\n\n  indented  \n\n## 日本語\nnaïve   text  \n";
    let chunks = split(source);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, 3, "# Café ☕\n\n  indented  "),
            (5, 6, "## 日本語\nnaïve   text  ")
        ]
    );
    for chunk in &chunks {
        assert_eq!(chunk.relative_file_path, "doc.md");
        assert_eq!(chunk.language, "markdown");
        assert_eq!(chunk.content_hash, hash(&chunk.code));
    }
}

#[test]
fn crlf_line_endings_stay_inside_a_chunk_and_never_end_it() {
    let source = "# A\r\nline\r\n\r\n## B\r\nbody";
    let chunks = split(source);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
            .collect::<Vec<_>>(),
        [(1, 2, "# A\r\nline"), (4, 5, "## B\r\nbody")]
    );
    // The same document in LF splits at the same lines.
    assert_eq!(ranges(&source.replace("\r\n", "\n")), ranges(source));
}

/// `count` paragraphs of about `size` bytes each, separated by blank lines.
fn paragraphs(count: usize, size: usize) -> String {
    (0..count)
        .map(|n| format!("p{n} {}", "word ".repeat(size / 5)))
        .map(|paragraph| paragraph.trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn an_oversized_section_is_packed_by_paragraph_up_to_the_target() {
    let body = paragraphs(20, 1000);
    let source = format!("# Big\n\n{body}\n\n# Small\ntext\n");
    let chunks = split(&source);
    assert!(chunks.len() > 3, "{} chunks", chunks.len());
    let big: Vec<_> = chunks
        .iter()
        .filter(|chunk| chunk.start_line < chunks.last().unwrap().start_line)
        .collect();
    for chunk in &big {
        assert!(chunk.code.len() <= TARGET_BYTES, "{}", chunk.code.len());
    }
    // The heading opens the first chunk; every later chunk starts at a
    // paragraph; together they hold every paragraph exactly once.
    assert!(big[0].code.starts_with("# Big\n\np0 "));
    for chunk in &big[1..] {
        assert!(chunk.code.starts_with('p'), "{}", &chunk.code[..20]);
    }
    for n in 0..20 {
        let marker = format!("\n\np{n} ");
        let at_edge = format!("p{n} ");
        let found: usize = big
            .iter()
            .map(|chunk| {
                chunk.code.matches(&marker).count() + usize::from(chunk.code.starts_with(&at_edge))
            })
            .sum();
        assert_eq!(found, 1, "paragraph {n}");
    }
    // Packing is greedy: a chunk could not have taken the next paragraph.
    for pair in big.windows(2) {
        let next_paragraph = pair[1].code.split("\n\n").next().unwrap().len();
        assert!(pair[0].code.len() + 2 + next_paragraph > TARGET_BYTES);
    }
    assert_eq!(chunks.last().unwrap().code, "# Small\ntext");
}

#[test]
fn a_fenced_block_is_never_split_even_when_oversized_or_blank_inside() {
    let fence_body = "x\n\n".repeat(3000);
    let source = format!(
        "# Code\n\n{}\n\n```text\n{fence_body}```\n\n{}\n",
        paragraphs(4, 2000),
        paragraphs(4, 2000)
    );
    let chunks = split(&source);
    let fenced: Vec<_> = chunks
        .iter()
        .filter(|chunk| chunk.code.contains("```text"))
        .collect();
    assert_eq!(fenced.len(), 1);
    assert!(
        fenced[0]
            .code
            .contains(&format!("```text\n{fence_body}```"))
    );
    assert!(fenced[0].code.len() > TARGET_BYTES);
    // No other chunk holds a piece of it.
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| chunk.code.contains("x\n\nx"))
            .count(),
        1
    );
    // The small fence of a normal section stays with its neighbours.
    let packed = format!("# A\n\n{}\n\n```\ncode\n\nmore\n```\n", paragraphs(5, 1500));
    for chunk in split(&packed) {
        assert_eq!(
            chunk.code.matches("```").count() % 2,
            0,
            "fence split: {}",
            chunk.code
        );
    }
}

#[test]
fn an_oversized_paragraph_splits_at_lines_but_a_long_line_stays_whole() {
    let list = (0..400)
        .map(|n| format!("- item {n} {}", "w".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!("# List\n{list}\n");
    let chunks = split(&source);
    assert!(chunks.len() > 2);
    for chunk in &chunks {
        assert!(chunk.code.len() <= TARGET_BYTES);
    }
    // The cuts fall between lines: joined again, the chunks are the source.
    let rejoined = chunks
        .iter()
        .map(|chunk| chunk.code.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(rejoined, source.trim_end());

    // A single line longer than the target stays whole, on its own.
    let long_line = "y".repeat(TARGET_BYTES * 2);
    let source = format!("# Long\n{long_line}\nshort\n");
    assert_eq!(texts(&source), ["# Long", long_line.as_str(), "short"]);
}

#[test]
fn chunking_a_real_document_is_deterministic() {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../README.md")).unwrap();
    let first = split(&source);
    let second = split(&source);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    assert!(first.len() > 5);
}

// -- golden over real files ----------------------------------------------------

/// Real rule documents copied from the frozen tree `bfaff44` of the Track A
/// spike (LF blobs), one with front matter and one list-heavy.
const FIXTURES: [(&str, &str); 5] = [
    (
        "AGENTS.md.fixture",
        include_str!("golden/markdown/AGENTS.md.fixture"),
    ),
    (
        "CLAUDE.md.fixture",
        include_str!("golden/markdown/CLAUDE.md.fixture"),
    ),
    (
        "SKILL.md.fixture",
        include_str!("golden/markdown/SKILL.md.fixture"),
    ),
    (
        "documentation.instructions.md.fixture",
        include_str!("golden/markdown/documentation.instructions.md.fixture"),
    ),
    (
        "app_AGENTS.md.fixture",
        include_str!("golden/markdown/app_AGENTS.md.fixture"),
    ),
];

/// The gold sections of the instructions set (line range and SHA-256 of the
/// section text) that fall in the fixtures.
const GOLD_SECTIONS: &str = include_str!("golden/markdown_gold_sections.json");

/// Gold sections that fit the target (one chunk each) and that do not.
const EXACT_GOLD_SECTIONS: usize = 21;
const PACKED_GOLD_SECTIONS: usize = 0;

/// The chunks of the fixtures as `(fixture, start_line, end_line, hash)`.
const FIXTURE_CHUNKS: &str = include_str!("golden/markdown_fixture_chunks.json");

/// A checkout with `core.autocrlf` turns the fixtures' LF into CRLF; the gold
/// hashes are over LF text.
fn fixture(source: &str) -> String {
    source.replace("\r\n", "\n")
}

fn fixture_chunks() -> Vec<(String, Chunk)> {
    FIXTURES
        .iter()
        .flat_map(|(name, source)| {
            chunks_for_path("doc.md", &fixture(source))
                .unwrap()
                .into_iter()
                .map(move |chunk| ((*name).to_owned(), chunk))
        })
        .collect()
}

#[test]
fn every_gold_section_of_the_instructions_set_is_a_chunk_or_tiles_into_chunks() {
    #[derive(serde::Deserialize)]
    struct Gold {
        fixture: String,
        start_line: u32,
        end_line: u32,
        sha256: String,
    }
    let gold: Vec<Gold> = serde_json::from_str(GOLD_SECTIONS).unwrap();
    assert_eq!(gold.len(), 21);
    let chunks = fixture_chunks();
    let (mut exact, mut packed) = (0, 0);
    for section in &gold {
        let source = fixture(
            FIXTURES
                .iter()
                .find(|(name, _)| *name == section.fixture)
                .unwrap()
                .1,
        );
        let text = source
            .split('\n')
            .skip(section.start_line as usize - 1)
            .take((section.end_line - section.start_line + 1) as usize)
            .collect::<Vec<_>>()
            .join("\n");
        // The gold hash is the hash of the section's text.
        assert_eq!(hash(&text), section.sha256, "{}", section.fixture);
        let own: Vec<_> = chunks
            .iter()
            .filter(|(name, chunk)| {
                *name == section.fixture
                    && chunk.start_line >= section.start_line
                    && chunk.end_line <= section.end_line
            })
            .map(|(_, chunk)| chunk)
            .collect();
        let label = format!(
            "{} {}-{}",
            section.fixture, section.start_line, section.end_line
        );
        if text.len() <= TARGET_BYTES {
            let [only] = own.as_slice() else {
                panic!("{label}: {own:#?}");
            };
            assert_eq!(
                (only.start_line, only.end_line),
                (section.start_line, section.end_line),
                "{label}"
            );
            assert_eq!(only.content_hash, section.sha256, "{label}");
            exact += 1;
        } else {
            // An oversized section is packed: its chunks start at the heading
            // and end at the last line, and each is within the target.
            assert!(own.len() > 1, "{label}");
            assert_eq!(own[0].start_line, section.start_line, "{label}");
            assert_eq!(own.last().unwrap().end_line, section.end_line, "{label}");
            assert!(own.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES));
            packed += 1;
        }
    }
    assert_eq!((exact, packed), (EXACT_GOLD_SECTIONS, PACKED_GOLD_SECTIONS));
}

#[test]
fn the_fixture_chunks_match_the_record() {
    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Recorded(String, u32, u32, String);
    let recorded: Vec<Recorded> = serde_json::from_str(FIXTURE_CHUNKS).unwrap();
    let today: Vec<_> = fixture_chunks()
        .into_iter()
        .map(|(name, chunk)| Recorded(name, chunk.start_line, chunk.end_line, chunk.content_hash))
        .collect();
    assert_eq!(today, recorded);
}

// -- scan ------------------------------------------------------------------------

#[test]
fn the_scan_indexes_md_files_in_any_case_and_applies_ignore_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join(".gitignore"), "build/\n").unwrap();
    std::fs::write(temp.path().join(".lciignore"), "private/**\n").unwrap();
    for (name, source) in [
        ("README.md", "# Readme\ntext\n"),
        ("docs/guide.md", "# Guide\ntext\n"),
        (".hidden/notes.md", "# Hidden\ntext\n"),
        ("build/out.md", "# Built\ntext\n"),
        ("private/secret.md", "# Secret\ntext\n"),
        ("UPPER.MD", "# Upper\ntext\n"),
        ("long.markdown", "# Long\ntext\n"),
        ("page.mdx", "# Mdx\ntext\n"),
        ("notes.md.bak", "# Backup\ntext\n"),
        ("lib.rs", "fn rust_item() {}\n"),
    ] {
        let path = temp.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let chunks = scan(temp.path()).unwrap();
    let languages: HashMap<_, _> = chunks
        .iter()
        .map(|chunk| (chunk.relative_file_path.as_str(), chunk.language.as_str()))
        .collect();
    assert_eq!(
        languages,
        HashMap::from([
            ("README.md", "markdown"),
            ("docs/guide.md", "markdown"),
            (".hidden/notes.md", "markdown"),
            ("UPPER.MD", "markdown"),
            ("lib.rs", "rust"),
        ])
    );
}

#[test]
fn a_markdown_file_is_reparsed_only_when_its_content_or_adapter_version_changes() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.md"), "# A\ntext\n\n## B\ntext\n").unwrap();
    std::fs::write(temp.path().join("lib.rs"), "fn f() {}\n").unwrap();

    let first = scan_incremental(temp.path(), &HashMap::new()).unwrap();
    assert_eq!((first.parsed_files, first.unchanged_files), (2, 0));
    assert_eq!(first.files["a.md"].language, "markdown");
    assert_eq!(first.files["a.md"].adapter_version, "markdown-chunks-v1");
    assert_eq!(first.files["a.md"].chunks.len(), 2);

    let repeat = scan_incremental(temp.path(), &first.files).unwrap();
    assert_eq!((repeat.parsed_files, repeat.unchanged_files), (0, 2));
    assert_eq!(repeat.fingerprint, first.fingerprint);

    // Only the Markdown cache entry is invalidated by its own version.
    let mut cached = first.files.clone();
    cached.get_mut("a.md").unwrap().adapter_version = "markdown-chunks-v0".into();
    let bumped = scan_incremental(temp.path(), &cached).unwrap();
    assert_eq!((bumped.parsed_files, bumped.unchanged_files), (1, 1));

    // A code adapter's version does not touch the Markdown entry.
    let mut cached = first.files.clone();
    cached.get_mut("lib.rs").unwrap().adapter_version = "rust-chunks-v0".into();
    let bumped = scan_incremental(temp.path(), &cached).unwrap();
    assert_eq!((bumped.parsed_files, bumped.unchanged_files), (1, 1));

    // A content change reparses it.
    std::fs::write(temp.path().join("a.md"), "# A\nchanged\n").unwrap();
    let changed = scan_incremental(temp.path(), &first.files).unwrap();
    assert_eq!((changed.parsed_files, changed.unchanged_files), (1, 1));
    assert_ne!(changed.fingerprint, first.fingerprint);
}

#[test]
fn the_extraction_hook_gives_markdown_chunks_and_no_declarations() {
    let source = "# A\ntext\n\n## B\ntext\n";
    let extraction = extract_for_path("docs/a.md", source).unwrap();
    assert!(extraction.declarations.is_none());
    assert_eq!(
        serde_json::to_string(&extraction.chunks).unwrap(),
        serde_json::to_string(&chunks_for_path("docs/a.md", source).unwrap()).unwrap()
    );
}
