//! Replays a repository's first-parent history and counts, for each
//! consecutive commit pair, how many chunks whose text did not change keep
//! their Chunk NodeId and how many keep today's fusion key
//! `(path, start_line, content_hash)`. The evidence for slice S1
//! (`docs/spike/multimap/s1-stability.md`). Reads git objects only; touches
//! no index, data directory or working tree.
//!
//! ```text
//! cargo run -p lci-core --example node_stability -- [--repo DIR] [--rev REV] [--commits N]
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
}

fn args() -> Result<Args> {
    let mut parsed = Args {
        repo: PathBuf::from("."),
        rev: "HEAD".into(),
        commits: 200,
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
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

/// A commit's source chunks, in each file's source order.
struct Snapshot {
    chunks: Vec<Chunk>,
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
    Ok(Snapshot { chunks })
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
    let mut blobs = Blobs::open(&args.repo)?;
    let mut cache = ChunkCache::new();
    let mut total = Counts::default();
    let mut pairs_with_gap = 0;
    let mut worst: Vec<(String, usize, usize, usize)> = Vec::new();
    let mut previous = snapshot(&args.repo, commits[0], &mut blobs, &mut cache)?;
    let mut max_twins = twins(&previous.chunks);
    for window in commits.windows(2) {
        let next = snapshot(&args.repo, window[1], &mut blobs, &mut cache)?;
        let old_ids = chunk_ids(&previous.chunks);
        let new_ids: HashSet<NodeId> = chunk_ids(&next.chunks).into_iter().collect();
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
        "rev {} ({} commits, {pairs} pairs, first-parent)",
        args.rev,
        commits.len()
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
