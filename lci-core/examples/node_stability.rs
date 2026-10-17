//! Replays a repository's first-parent history and counts, for each
//! consecutive commit pair, how many chunks whose text did not change keep
//! their Chunk NodeId and how many keep today's fusion key
//! `(path, start_line, content_hash)`. The evidence for slice S1
//! (`docs/spike/multimap/s1-stability.md`). Reads git objects only; touches
//! no index, data directory or working tree.
//!
//! ```text
//! cargo run -p lci-core --example node_stability -- [--repo DIR] [--rev REV] [--commits N] [--explain]
//! ```
//!
//! A snapshot is every blob of the commit's tree with a registered source
//! extension that the commit's own root-level `.lciignore` does not exclude
//! and that is not a known non-source format, chunked as the scanner chunks
//! it. Not replayed: `.lciignore` files below the root (the scanner honours
//! them) and the walker's `.gitignore` handling (committed files are taken
//! as tracked).
//!
//! "Kept" means the older commit's id is present in the newer commit's
//! snapshot (set membership), for both kinds of key.
//!
//! Membership cannot see an id that survives but names another chunk (a
//! duplicate inserted earlier in a file shifts the ordinals of the later
//! copies). So the tool also pairs chunks by position: `git diff` of the two
//! blobs maps each older chunk's lines to the newer file, and the newer chunk
//! with the same text on the mapped lines is its successor. An id "points at
//! the wrong chunk" when it resolves to a newer chunk other than the
//! successor. Only files with a repeated `content_hash` in either version
//! are diffed: elsewhere each text has one copy and an id cannot name
//! another.
//!
//! The tool prints how many commits it actually replayed: on a shallow
//! clone the history can end before `--commits` is reached.

use anyhow::{Context, Result, bail, ensure};
use ignore::gitignore::GitignoreBuilder;
use lci_core::{
    chunk::{self, Chunk},
    language,
    node::{NodeId, chunk_ids, legacy_fusion_key},
};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, ChildStdout, Command, Stdio},
};

struct Args {
    repo: PathBuf,
    rev: String,
    commits: usize,
    /// Print every id that points at another chunk.
    explain: bool,
}

fn args() -> Result<Args> {
    let mut parsed = Args {
        repo: PathBuf::from("."),
        rev: "HEAD".into(),
        commits: 200,
        explain: false,
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        if flag == "--explain" {
            parsed.explain = true;
            continue;
        }
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--repo" => parsed.repo = value.into(),
            "--rev" => parsed.rev = value,
            "--commits" => parsed.commits = value.parse()?,
            _ => bail!("unknown flag {flag}"),
        }
    }
    Ok(parsed)
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// One long-running `git cat-file --batch`.
struct Blobs {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Blobs {
    fn open(repo: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        Ok(Self {
            input: child.stdin.take().context("stdin")?,
            output: BufReader::new(child.stdout.take().context("stdout")?),
        })
    }

    fn read(&mut self, object: &str) -> Result<Vec<u8>> {
        writeln!(self.input, "{object}")?;
        self.input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .with_context(|| format!("bad cat-file header {header:?}"))?
            .parse()?;
        let mut bytes = vec![0; size + 1];
        self.output.read_exact(&mut bytes)?;
        bytes.pop();
        Ok(bytes)
    }
}

/// A commit's source chunks, in each file's source order, and the blob of
/// each source file.
struct Snapshot {
    chunks: Vec<Chunk>,
    blobs: HashMap<String, String>,
}

/// Chunks of a blob, without the path (re-attached per snapshot), cached
/// by `(blob, adapter)`.
type ChunkCache = HashMap<(String, &'static str), Vec<Chunk>>;

fn snapshot(
    repo: &Path,
    commit: &str,
    blobs: &mut Blobs,
    cache: &mut ChunkCache,
) -> Result<Snapshot> {
    let tree = git(repo, &["ls-tree", "-r", "-z", commit])?;
    let mut entries = Vec::new();
    let mut ignore = None;
    for entry in tree.split('\0').filter(|e| !e.is_empty()) {
        let (meta, path) = entry.split_once('\t').context("ls-tree entry")?;
        let fields: Vec<&str> = meta.split(' ').collect();
        if fields[1] != "blob" || fields[0] == "120000" {
            continue;
        }
        if path == ".lciignore" {
            ignore = Some(fields[2].to_owned());
        }
        entries.push((path.to_owned(), fields[2].to_owned()));
    }
    let mut builder = GitignoreBuilder::new(repo);
    if let Some(blob) = ignore {
        for line in String::from_utf8_lossy(&blobs.read(&blob)?).lines() {
            builder.add_line(None, line)?;
        }
    }
    let ignore = builder.build()?;
    let mut chunks = Vec::new();
    let mut sources = HashMap::new();
    for (path, blob) in entries {
        let Some(adapter) = language::for_path(Path::new(&path)) else {
            continue;
        };
        if ignore
            .matched_path_or_any_parents(Path::new(&path), false)
            .is_ignore()
        {
            continue;
        }
        sources.insert(path.clone(), blob.clone());
        let key = (blob, adapter.identifier());
        if !cache.contains_key(&key) {
            let bytes = blobs.read(&key.0)?;
            let parsed = if adapter.non_source(&bytes).is_some() {
                Vec::new()
            } else {
                let (source, _) = chunk::decode_source(bytes);
                chunk::chunks_for_path(&path, &source)?
            };
            cache.insert(key.clone(), parsed);
        }
        chunks.extend(cache[&key].iter().map(|chunk| Chunk {
            relative_file_path: path.clone(),
            ..chunk.clone()
        }));
    }
    Ok(Snapshot {
        chunks,
        blobs: sources,
    })
}

#[derive(Default)]
struct Counts {
    /// Chunks of the older commit whose file still holds the same text.
    unchanged: usize,
    node_kept: usize,
    legacy_kept: usize,
    /// The same, restricted to files where some chunk changed its text or
    /// its lines in the pair.
    in_edited: usize,
    node_kept_in_edited: usize,
    legacy_kept_in_edited: usize,
    /// Unchanged chunks with an ordinal above 0 (a duplicate of an earlier
    /// chunk of the same file), where an id can survive while naming
    /// another copy.
    duplicates: usize,
}

/// Each file's chunks (hash and lines) in source order: equal lists mean
/// the commit left the file's chunks where and as they were.
fn file_text(chunks: &[Chunk]) -> HashMap<&str, Vec<(&str, u32, u32)>> {
    let mut files = HashMap::<&str, Vec<_>>::new();
    for c in chunks {
        files.entry(&c.relative_file_path).or_default().push((
            c.content_hash.as_str(),
            c.start_line,
            c.end_line,
        ));
    }
    files
}

/// Changed line ranges of `git diff -U0 old new`: `(old_start, old_count,
/// new_count)` per hunk, in order. A count of 0 is a pure insertion after
/// (or deletion before) the given line. Git's default alignment is pinned
/// (Myers, indent heuristic), so a user's diff settings cannot change the
/// pairing.
fn hunks(repo: &Path, old: &str, new: &str) -> Result<Vec<(u32, u32, u32)>> {
    let diff = git(
        repo,
        &[
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--diff-algorithm=myers",
            "--indent-heuristic",
            old,
            new,
        ],
    )?;
    let range = |text: &str| -> Result<(u32, u32)> {
        let (start, count) = text.split_once(',').unwrap_or((text, "1"));
        Ok((start.parse()?, count.parse()?))
    };
    let mut out = Vec::new();
    for line in diff.lines().filter(|l| l.starts_with("@@ ")) {
        let mut fields = line.split_whitespace().skip(1);
        let old = fields.next().context("hunk")?.trim_start_matches('-');
        let new = fields.next().context("hunk")?.trim_start_matches('+');
        let ((old_start, old_count), (_, new_count)) = (range(old)?, range(new)?);
        out.push((old_start, old_count, new_count));
    }
    Ok(out)
}

/// Where an older line range sits in the newer file, if the diff left every
/// line of it (and nothing between them) in place.
fn map_lines(hunks: &[(u32, u32, u32)], start: u32, end: u32) -> Option<(u32, u32)> {
    let map = |line: u32| -> Option<i64> {
        let mut offset = 0i64;
        for &(old_start, old_count, new_count) in hunks {
            let before = if old_count == 0 {
                old_start < line
            } else if line >= old_start && line < old_start + old_count {
                return None;
            } else {
                old_start + old_count <= line
            };
            if before {
                offset += i64::from(new_count) - i64::from(old_count);
            }
        }
        Some(i64::from(line) + offset)
    };
    let (new_start, new_end) = (map(start)?, map(end)?);
    (new_end - new_start == i64::from(end - start))
        .then(|| Some((u32::try_from(new_start).ok()?, u32::try_from(new_end).ok()?)))
        .flatten()
}

/// What an older chunk's id resolves to in the newer snapshot, against the
/// chunk that really followed it there.
#[derive(Default)]
struct Pointing {
    /// Unchanged chunks of the diffed files (those with a repeated text).
    checked: usize,
    /// Checked chunks with no newer chunk of the same text on the mapped
    /// lines (moved, or the lines around them were rewritten).
    no_successor: usize,
    node_right: usize,
    node_wrong: usize,
    node_lost: usize,
    legacy_right: usize,
    legacy_wrong: usize,
    legacy_lost: usize,
}

impl Pointing {
    fn add(&mut self, other: &Pointing) {
        self.checked += other.checked;
        self.no_successor += other.no_successor;
        self.node_right += other.node_right;
        self.node_wrong += other.node_wrong;
        self.node_lost += other.node_lost;
        self.legacy_right += other.legacy_right;
        self.legacy_wrong += other.legacy_wrong;
        self.legacy_lost += other.legacy_lost;
    }
}

/// Pairs the chunks of one file by position (see the module comment) and
/// counts where each kind of id points. `old` and `new` are indices into
/// the two snapshots' chunk lists, for this file only.
fn pointing(
    repo: &Path,
    (old_blob, new_blob): (&str, &str),
    (previous, next): (&Snapshot, &Snapshot),
    (old_ids, new_ids): (&[NodeId], &[NodeId]),
    old: &[usize],
    new: &[usize],
    explain: bool,
) -> Result<Pointing> {
    let hunks = hunks(repo, old_blob, new_blob)?;
    let by_place: HashMap<(&str, u32, u32), usize> = new
        .iter()
        .map(|&j| {
            let c = &next.chunks[j];
            ((c.content_hash.as_str(), c.start_line, c.end_line), j)
        })
        .collect();
    let by_id: HashMap<&NodeId, usize> = new.iter().map(|&j| (&new_ids[j], j)).collect();
    let by_legacy: HashMap<(u32, &str), usize> = new
        .iter()
        .map(|&j| {
            let c = &next.chunks[j];
            ((c.start_line, c.content_hash.as_str()), j)
        })
        .collect();
    let texts: HashSet<&str> = new
        .iter()
        .map(|&j| next.chunks[j].content_hash.as_str())
        .collect();
    let mut counts = Pointing::default();
    for &i in old {
        let chunk = &previous.chunks[i];
        if !texts.contains(chunk.content_hash.as_str()) {
            continue;
        }
        counts.checked += 1;
        let successor = map_lines(&hunks, chunk.start_line, chunk.end_line)
            .and_then(|(start, end)| by_place.get(&(chunk.content_hash.as_str(), start, end)));
        let Some(&successor) = successor else {
            counts.no_successor += 1;
            continue;
        };
        match by_id.get(&old_ids[i]) {
            None => counts.node_lost += 1,
            Some(&j) if j == successor => counts.node_right += 1,
            Some(&j) => {
                counts.node_wrong += 1;
                if explain {
                    let (s, w) = (&next.chunks[successor], &next.chunks[j]);
                    println!(
                        "wrong NodeId: {} lines {}-{} ({:?}) -> successor {}-{}, id names {}-{}",
                        chunk.relative_file_path,
                        chunk.start_line,
                        chunk.end_line,
                        chunk.code.chars().take(60).collect::<String>(),
                        s.start_line,
                        s.end_line,
                        w.start_line,
                        w.end_line
                    );
                }
            }
        }
        match by_legacy.get(&(chunk.start_line, chunk.content_hash.as_str())) {
            None => counts.legacy_lost += 1,
            Some(&j) if j == successor => counts.legacy_right += 1,
            Some(_) => counts.legacy_wrong += 1,
        }
    }
    Ok(counts)
}

/// Chunk indices of a snapshot per file.
fn by_file(chunks: &[Chunk]) -> HashMap<&str, Vec<usize>> {
    let mut files = HashMap::<&str, Vec<usize>>::new();
    for (index, chunk) in chunks.iter().enumerate() {
        files
            .entry(chunk.relative_file_path.as_str())
            .or_default()
            .push(index);
    }
    files
}

/// Whether two chunks of a file share a text.
fn has_repeat(chunks: &[Chunk], indices: &[usize]) -> bool {
    let mut seen = HashSet::new();
    !indices
        .iter()
        .all(|&i| seen.insert(chunks[i].content_hash.as_str()))
}

/// Fusion keys of a snapshot shared by two or more NodeIds (decision N-5).
fn twins(chunks: &[Chunk]) -> usize {
    let ids = chunk_ids(chunks);
    let mut by_key = HashMap::<_, HashSet<&NodeId>>::new();
    for (chunk, id) in chunks.iter().zip(&ids) {
        by_key
            .entry(legacy_fusion_key(chunk))
            .or_default()
            .insert(id);
    }
    by_key.values().filter(|ids| ids.len() > 1).count()
}

fn main() -> Result<()> {
    let args = args()?;
    let list = git(
        &args.repo,
        &[
            "rev-list",
            "--first-parent",
            &format!("--max-count={}", args.commits + 1),
            &args.rev,
        ],
    )?;
    let mut commits: Vec<&str> = list.lines().collect();
    commits.reverse();
    ensure!(commits.len() >= 2, "need at least two commits");
    let shallow = git(&args.repo, &["rev-parse", "--is-shallow-repository"])?;
    let shallow = shallow.trim() == "true";
    if commits.len() - 1 < args.commits {
        eprintln!(
            "warning: {} pairs requested, history has {} (shallow clone: {})",
            args.commits,
            commits.len() - 1,
            if shallow { "yes" } else { "no" }
        );
    }
    let mut blobs = Blobs::open(&args.repo)?;
    let mut cache = ChunkCache::new();
    let mut total = Counts::default();
    let mut pairs_with_gap = 0;
    let mut pointed = Pointing::default();
    let mut pairs_with_wrong = Vec::new();
    let mut worst: Vec<(String, usize, usize, usize)> = Vec::new();
    let mut previous = snapshot(&args.repo, commits[0], &mut blobs, &mut cache)?;
    let mut max_twins = twins(&previous.chunks);
    for window in commits.windows(2) {
        let next = snapshot(&args.repo, window[1], &mut blobs, &mut cache)?;
        let old_ids = chunk_ids(&previous.chunks);
        let next_ids = chunk_ids(&next.chunks);
        let new_ids: HashSet<&NodeId> = next_ids.iter().collect();
        let new_keys: HashSet<_> = next.chunks.iter().map(legacy_fusion_key).collect();
        let new_texts: HashSet<(&str, &str)> = next
            .chunks
            .iter()
            .map(|c| (c.relative_file_path.as_str(), c.content_hash.as_str()))
            .collect();
        let (old_files, new_files) = (file_text(&previous.chunks), file_text(&next.chunks));
        let mut pair = Counts::default();
        for (chunk, id) in previous.chunks.iter().zip(&old_ids) {
            let path = chunk.relative_file_path.as_str();
            if !new_texts.contains(&(path, chunk.content_hash.as_str())) {
                continue;
            }
            let edited = old_files.get(path) != new_files.get(path);
            let node = new_ids.contains(id);
            let legacy = new_keys.contains(&legacy_fusion_key(chunk));
            pair.unchanged += 1;
            pair.duplicates +=
                usize::from(matches!(id, NodeId::Chunk { ordinal, .. } if *ordinal > 0));
            pair.node_kept += usize::from(node);
            pair.legacy_kept += usize::from(legacy);
            if edited {
                pair.in_edited += 1;
                pair.node_kept_in_edited += usize::from(node);
                pair.legacy_kept_in_edited += usize::from(legacy);
            }
        }
        let (old_by_file, new_by_file) = (by_file(&previous.chunks), by_file(&next.chunks));
        let mut pair_pointed = Pointing::default();
        for (path, old) in &old_by_file {
            let Some(new) = new_by_file.get(path) else {
                continue;
            };
            if old_files.get(path) == new_files.get(path)
                || !(has_repeat(&previous.chunks, old) || has_repeat(&next.chunks, new))
            {
                continue;
            }
            pair_pointed.add(&pointing(
                &args.repo,
                (&previous.blobs[*path], &next.blobs[*path]),
                (&previous, &next),
                (&old_ids, &next_ids),
                old,
                new,
                args.explain,
            )?);
        }
        if pair_pointed.node_wrong + pair_pointed.legacy_wrong > 0 {
            pairs_with_wrong.push(format!(
                "`{}` {}/{}",
                &window[1][..10],
                pair_pointed.node_wrong,
                pair_pointed.legacy_wrong
            ));
        }
        pointed.add(&pair_pointed);
        if pair.node_kept != pair.legacy_kept {
            pairs_with_gap += 1;
        }
        worst.push((
            window[1][..10].to_owned(),
            pair.in_edited,
            pair.node_kept_in_edited,
            pair.legacy_kept_in_edited,
        ));
        max_twins = max_twins.max(twins(&next.chunks));
        total.unchanged += pair.unchanged;
        total.duplicates += pair.duplicates;
        total.node_kept += pair.node_kept;
        total.legacy_kept += pair.legacy_kept;
        total.in_edited += pair.in_edited;
        total.node_kept_in_edited += pair.node_kept_in_edited;
        total.legacy_kept_in_edited += pair.legacy_kept_in_edited;
        previous = next;
    }
    let percent = |part: usize, whole: usize| 100.0 * part as f64 / whole.max(1) as f64;
    let pairs = commits.len() - 1;
    println!(
        "rev {}: replayed {} first-parent commits ({pairs} pairs) of {} pairs requested; oldest {}; shallow clone: {}",
        args.rev,
        commits.len(),
        args.commits,
        &commits[0][..10],
        if shallow { "yes" } else { "no" }
    );
    println!("last snapshot: {} chunks", previous.chunks.len());
    println!();
    println!("| Chunks whose text did not change | Count | NodeId kept | Legacy key kept |");
    println!("| --- | ---: | ---: | ---: |");
    println!(
        "| all | {} | {} ({:.2}%) | {} ({:.2}%) |",
        total.unchanged,
        total.node_kept,
        percent(total.node_kept, total.unchanged),
        total.legacy_kept,
        percent(total.legacy_kept, total.unchanged)
    );
    println!(
        "| in files whose chunks changed or moved | {} | {} ({:.2}%) | {} ({:.2}%) |",
        total.in_edited,
        total.node_kept_in_edited,
        percent(total.node_kept_in_edited, total.in_edited),
        total.legacy_kept_in_edited,
        percent(total.legacy_kept_in_edited, total.in_edited)
    );
    println!();
    println!(
        "unchanged chunks with ordinal > 0 (duplicates): {}",
        total.duplicates
    );
    println!("pairs where the two counts differ: {pairs_with_gap} of {pairs}");
    println!(
        "most fusion keys shared by two or more NodeIds in one snapshot (decision N-5): {max_twins}"
    );
    println!();
    println!(
        "positional pairing (files with a repeated text, diffed): {} unchanged chunks checked, {} without a successor on the mapped lines",
        pointed.checked, pointed.no_successor
    );
    println!("| Key | Points at the successor | Points at another chunk | Lost |");
    println!("| --- | ---: | ---: | ---: |");
    println!(
        "| NodeId | {} | {} | {} |",
        pointed.node_right, pointed.node_wrong, pointed.node_lost
    );
    println!(
        "| Legacy key | {} | {} | {} |",
        pointed.legacy_right, pointed.legacy_wrong, pointed.legacy_lost
    );
    println!(
        "pairs with an id pointing at another chunk (NodeId/legacy): {}",
        if pairs_with_wrong.is_empty() {
            "none".to_owned()
        } else {
            pairs_with_wrong.join(", ")
        }
    );
    worst.sort_by_key(|(_, edited, node, legacy)| {
        std::cmp::Reverse((*node as i64 - *legacy as i64, *edited))
    });
    println!();
    println!("| Commit | Unchanged chunks in edited files | NodeId kept | Legacy key kept |");
    println!("| --- | ---: | ---: | ---: |");
    for (commit, edited, node, legacy) in worst.iter().take(10) {
        println!("| `{commit}` | {edited} | {node} | {legacy} |");
    }
    Ok(())
}
