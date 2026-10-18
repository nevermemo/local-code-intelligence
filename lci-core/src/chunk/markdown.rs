//! The Markdown extractor: a hand-written heading splitter, because prose has
//! no grammar (see [`crate::language::Extractor::Prose`]).
//!
//! A file is cut into sections. A section is a heading line plus its own body
//! up to the next heading of any level ("flat" sections, not subtrees). The
//! rules reproduce the splitter of the Track A premise spike, whose sections
//! matched every gold section of the instructions set on line range and
//! SHA-256:
//!
//! - Headings are ATX (`# Title`, up to three spaces of indent, up to six
//!   `#`) and setext (a paragraph underlined by `===` or `---`; the section
//!   starts at the first line of the paragraph). Nothing inside a fenced code
//!   block or the front matter is a heading.
//! - Fenced code blocks (```` ``` ```` or `~~~`, closed by a fence of the same
//!   character at least as long; an unclosed fence runs to the end of the
//!   file) are atomic: never split, and never searched for headings.
//! - YAML front matter (a first line `---` up to the next `---` line) is not a
//!   unit of its own. It belongs to the first unit: the preamble when there is
//!   text before the first heading, otherwise the first section.
//! - Text before the first heading (after any front matter) is a preamble
//!   unit. A file without a heading is one unit.
//! - A unit runs from its first line to its last non-blank line. The chunk is
//!   that exact source text: no normalization, original line endings inside.
//!
//! A unit larger than [`TARGET_BYTES`] is packed by paragraph: blank lines
//! separate blocks, a fenced block or the front matter is one atomic block,
//! consecutive blocks are joined greedily while they fit, and a block that is
//! too large on its own is split at line boundaries unless it is atomic (an
//! oversized fence stays whole, like a large indivisible syntax leaf in code).

use super::{Chunk, TARGET_BYTES, hash};

/// One line of the source: its byte range without the line terminator
/// (`\n` or `\r\n`).
struct Line<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn split_lines(source: &str) -> Vec<Line<'_>> {
    let mut offset = 0;
    source
        .split_inclusive('\n')
        .map(|raw| {
            let start = offset;
            offset += raw.len();
            let text = raw.strip_suffix('\n').unwrap_or(raw);
            let text = text.strip_suffix('\r').unwrap_or(text);
            Line {
                start,
                end: start + text.len(),
                text,
            }
        })
        .collect()
}

fn is_blank(line: &Line<'_>) -> bool {
    line.text.trim().is_empty()
}

/// What the line scan found, with zero-based line indices.
#[derive(Debug, Default, PartialEq, Eq)]
struct Layout {
    /// Number of lines of the front matter (0 when there is none).
    front_matter: usize,
    /// First line of each section, in order.
    headings: Vec<usize>,
    /// Inclusive line ranges that are never split: the front matter and the
    /// fenced code blocks, in order.
    atomic: Vec<(usize, usize)>,
}

/// Lines of a leading `---` ... `---` block, or 0. No closing line means no
/// front matter.
fn front_matter_lines(lines: &[Line<'_>]) -> usize {
    if lines.first().is_none_or(|line| line.text.trim() != "---") {
        return 0;
    }
    lines[1..]
        .iter()
        .position(|line| line.text.trim() == "---")
        .map_or(0, |closing| closing + 2)
}

/// The line without up to three leading spaces, or `None` when it has more
/// (four spaces or a tab make an indented code line, not markup).
fn without_markup_indent(text: &str) -> Option<&str> {
    let rest = text.trim_start_matches(' ');
    (text.len() - rest.len() <= 3).then_some(rest)
}

fn is_atx_heading(text: &str) -> bool {
    let Some(rest) = without_markup_indent(text) else {
        return false;
    };
    let hashes = rest.bytes().take_while(|&byte| byte == b'#').count();
    (1..=6).contains(&hashes)
        && rest[hashes..]
            .chars()
            .next()
            .is_none_or(|next| next == ' ' || next == '\t')
}

fn is_setext_underline(text: &str) -> bool {
    let Some(rest) = without_markup_indent(text) else {
        return false;
    };
    let rest = rest.trim_end_matches([' ', '\t']);
    !rest.is_empty() && (rest.bytes().all(|b| b == b'=') || rest.bytes().all(|b| b == b'-'))
}

/// The fence character and run length of an opening fence line.
fn fence_open(text: &str) -> Option<(u8, usize)> {
    let rest = without_markup_indent(text)?;
    let fence = *rest
        .as_bytes()
        .first()
        .filter(|&&b| b == b'`' || b == b'~')?;
    let run = rest.bytes().take_while(|&b| b == fence).count();
    // A backtick fence cannot have a backtick in its info string.
    (run >= 3 && !(fence == b'`' && rest[run..].contains('`'))).then_some((fence, run))
}

fn closes_fence(text: &str, fence: u8, run: usize) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed.len() >= run
        && trimmed.bytes().all(|b| b == fence)
        && !text.starts_with("    ")
}

/// A list item marker (`-`, `*`, `+`, `1.`, `1)`) followed by whitespace or
/// the end of the line.
fn is_list_item(text: &str) -> bool {
    let rest = text.trim_start();
    let after_marker = match rest.bytes().next() {
        Some(b'-' | b'*' | b'+') => &rest[1..],
        Some(b'0'..=b'9') => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            match rest.as_bytes().get(digits) {
                Some(b'.' | b')') => &rest[digits + 1..],
                _ => return false,
            }
        }
        _ => return false,
    };
    after_marker.chars().next().is_none_or(char::is_whitespace)
}

/// What the previous non-fence line was, to tell a setext underline from a
/// thematic break or list text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Previous {
    Blank,
    Paragraph,
    /// A list item, quote, table, HTML line or indented code: an underline
    /// after it is not a setext heading.
    Other,
    Heading,
    Fence,
}

fn scan(lines: &[Line<'_>]) -> Layout {
    let front_matter = front_matter_lines(lines);
    let mut layout = Layout {
        front_matter,
        ..Layout::default()
    };
    if front_matter > 0 {
        layout.atomic.push((0, front_matter - 1));
    }
    let mut open_fence: Option<(u8, usize, usize)> = None;
    let mut previous = Previous::Blank;
    let mut paragraph_start = 0;
    for (index, line) in lines.iter().enumerate().skip(front_matter) {
        let text = line.text;
        if let Some((fence, run, first)) = open_fence {
            if closes_fence(text, fence, run) {
                layout.atomic.push((first, index));
                open_fence = None;
            }
            previous = Previous::Fence;
            continue;
        }
        if let Some((fence, run)) = fence_open(text) {
            open_fence = Some((fence, run, index));
            previous = Previous::Fence;
            continue;
        }
        if is_blank(line) {
            previous = Previous::Blank;
            continue;
        }
        if is_atx_heading(text) {
            layout.headings.push(index);
            previous = Previous::Heading;
            continue;
        }
        if previous == Previous::Paragraph && is_setext_underline(text) {
            layout.headings.push(paragraph_start);
            previous = Previous::Heading;
            continue;
        }
        if is_list_item(text) || text.trim_start().starts_with(['>', '|', '<']) {
            previous = Previous::Other;
        } else if text.starts_with("    ") || text.starts_with('\t') {
            previous = if previous == Previous::Paragraph {
                Previous::Paragraph
            } else {
                Previous::Other
            };
        } else if matches!(previous, Previous::Paragraph | Previous::Other) {
            // Continuation of the paragraph, or a lazy continuation of a
            // list item or quote.
        } else {
            previous = Previous::Paragraph;
            paragraph_start = index;
        }
    }
    // An unclosed fence runs to the end of the file.
    if let Some((_, _, first)) = open_fence {
        layout.atomic.push((first, lines.len() - 1));
    }
    layout
}

/// Last non-blank line of `start..=end`, or `start` when all are blank.
fn trim_end(lines: &[Line<'_>], start: usize, mut end: usize) -> usize {
    while end > start && is_blank(&lines[end]) {
        end -= 1;
    }
    end
}

/// First non-blank line at or after `from`.
fn first_content(lines: &[Line<'_>], from: usize) -> Option<usize> {
    (from..lines.len()).find(|&index| !is_blank(&lines[index]))
}

/// The units as inclusive zero-based line ranges, never blank-only.
fn units(lines: &[Line<'_>], layout: &Layout) -> Vec<(usize, usize)> {
    let count = lines.len();
    let Some(&first) = layout.headings.first() else {
        return match first_content(lines, 0) {
            Some(start) => vec![(start, trim_end(lines, start, count - 1))],
            None => Vec::new(),
        };
    };
    let mut units = Vec::with_capacity(layout.headings.len() + 1);
    let preamble = first_content(lines, layout.front_matter).filter(|&line| line < first);
    if let Some(text) = preamble {
        // The front matter, if any, is part of the preamble; otherwise the
        // unit starts at the first non-blank line.
        let start = if layout.front_matter > 0 { 0 } else { text };
        units.push((start, trim_end(lines, start, first - 1)));
    }
    for (position, &heading) in layout.headings.iter().enumerate() {
        let next = layout.headings.get(position + 1).copied().unwrap_or(count);
        let start = if position == 0 && layout.front_matter > 0 && preamble.is_none() {
            0
        } else {
            heading
        };
        units.push((start, trim_end(lines, heading, next - 1)));
    }
    units
}

/// A run of non-blank lines, or one atomic range.
#[derive(Clone, Copy)]
struct Block {
    first: usize,
    last: usize,
    atomic: bool,
}

/// The blocks of the inclusive line range, in order. A fence interrupts a
/// paragraph: the lines before it form their own block.
fn blocks(lines: &[Line<'_>], layout: &Layout, first: usize, last: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Option<usize> = None;
    let mut index = first;
    // The atomic ranges are sorted and disjoint: a cursor finds the one that
    // holds `index`, if any.
    let mut cursor = layout.atomic.partition_point(|&(_, end)| end < first);
    while index <= last {
        while layout
            .atomic
            .get(cursor)
            .is_some_and(|&(_, end)| end < index)
        {
            cursor += 1;
        }
        let atomic = layout
            .atomic
            .get(cursor)
            .filter(|&&(start, _)| start <= index);
        if let Some(&(_, end)) = atomic {
            if let Some(start) = paragraph.take() {
                blocks.push(Block {
                    first: start,
                    last: index - 1,
                    atomic: false,
                });
            }
            let end = end.min(last);
            blocks.push(Block {
                first: index,
                last: end,
                atomic: true,
            });
            index = end + 1;
            continue;
        }
        if is_blank(&lines[index]) {
            if let Some(start) = paragraph.take() {
                blocks.push(Block {
                    first: start,
                    last: index - 1,
                    atomic: false,
                });
            }
        } else if paragraph.is_none() {
            paragraph = Some(index);
        }
        index += 1;
    }
    if let Some(start) = paragraph {
        blocks.push(Block {
            first: start,
            last,
            atomic: false,
        });
    }
    blocks
}

fn span_bytes(lines: &[Line<'_>], first: usize, last: usize) -> usize {
    lines[last].end - lines[first].start
}

/// Splits an oversized non-atomic block at line boundaries into pieces of at
/// most [`TARGET_BYTES`]; a single longer line stays whole.
fn split_block(lines: &[Line<'_>], block: Block, out: &mut Vec<Block>) {
    if block.atomic || span_bytes(lines, block.first, block.last) <= TARGET_BYTES {
        out.push(block);
        return;
    }
    let mut first = block.first;
    for index in block.first + 1..=block.last {
        if span_bytes(lines, first, index) > TARGET_BYTES {
            out.push(Block {
                first,
                last: index - 1,
                atomic: false,
            });
            first = index;
        }
    }
    out.push(Block {
        first,
        last: block.last,
        atomic: false,
    });
}

/// The chunk line ranges of one unit: the unit itself when it fits, else its
/// blocks packed greedily up to [`TARGET_BYTES`].
fn pack(lines: &[Line<'_>], layout: &Layout, (first, last): (usize, usize)) -> Vec<(usize, usize)> {
    if span_bytes(lines, first, last) <= TARGET_BYTES {
        return vec![(first, last)];
    }
    let mut pieces = Vec::new();
    for block in blocks(lines, layout, first, last) {
        split_block(lines, block, &mut pieces);
    }
    let mut packed: Vec<(usize, usize)> = Vec::new();
    for piece in pieces {
        match packed.last_mut() {
            Some((start, end)) if span_bytes(lines, *start, piece.last) <= TARGET_BYTES => {
                *end = piece.last;
            }
            _ => packed.push((piece.first, piece.last)),
        }
    }
    packed
}

/// The chunks of a Markdown file.
pub(super) fn chunks(path: &str, source: &str, language: &str) -> Vec<Chunk> {
    let lines = split_lines(source);
    let layout = scan(&lines);
    let mut chunks = Vec::new();
    for unit in units(&lines, &layout) {
        for (first, last) in pack(&lines, &layout, unit) {
            let code = &source[lines[first].start..lines[last].end];
            chunks.push(Chunk {
                relative_file_path: path.into(),
                language: language.into(),
                start_line: first as u32 + 1,
                end_line: last as u32 + 1,
                code: code.into(),
                content_hash: hash(code),
            });
        }
    }
    #[cfg(test)]
    super::markdown_tests::assert_lines_covered(source, &chunks);
    chunks
}
