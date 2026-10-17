//! `cargo xtask docs check`: every relative Markdown link and anchor in the
//! tracked `*.md` files must resolve to an existing file, and the anchor to a
//! heading slug in GitHub's style (`docs/development/documentation.md`, rule 9).
//!
//! External links (any `scheme:` target such as http, https or mailto) are
//! skipped, and so are fenced code blocks and inline code spans. Vendored
//! `.agents/skills/rust-skills/**` is not scanned. Frozen records (the
//! pre-registrations and results under `docs/spike/multimap/v2`) are scanned
//! and reported but never fail the check, because they are never edited.

use crate::acceptance::workspace_root;
use crate::lint::tracked_files;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Vendored third-party skill material: not ours to rewrite.
const VENDORED_PREFIX: &str = ".agents/skills/rust-skills/";
/// Where frozen records live.
const FROZEN_DIR: &str = "docs/spike/multimap/v2/";

/// Frozen records are never edited, so their broken links cannot be fixed.
fn is_frozen(rel: &str) -> bool {
    rel.starts_with(FROZEN_DIR)
        && rel.rsplit('/').next().is_some_and(|name| {
            name.ends_with(".md") && (name.contains("-prereg") || name.ends_with("-results.md"))
        })
}

fn is_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

/// GitHub's heading anchor: link syntax reduced to its text, lower-cased,
/// everything but letters, digits, spaces, `-` and `_` dropped, spaces to `-`.
pub fn slug(heading: &str) -> String {
    let mut text = String::new();
    let mut rest = heading;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let linked = after.find(']').and_then(|close| {
            after[close + 1..]
                .starts_with('(')
                .then(|| (close, after[close + 1..].find(')')))
        });
        match linked {
            Some((close, Some(end))) => {
                text.push_str(&rest[..open]);
                text.push_str(&after[..close]);
                rest = &after[close + 1 + end + 1..];
            }
            _ => {
                text.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    text.push_str(rest);
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The text of an ATX heading line (`## Title ##`), or `None`.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    let rest = line.get(hashes..)?;
    if !(1..=6).contains(&hashes) || !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim();
    // A closing run of `#` counts only after a space (so `C#` keeps its `#`).
    let stripped = text.trim_end_matches('#');
    if stripped.len() == text.len() {
        Some(text)
    } else if stripped.is_empty() {
        Some("")
    } else if stripped.ends_with([' ', '\t']) {
        Some(stripped.trim_end())
    } else {
        Some(text)
    }
}

/// Every anchor a document defines. A repeated heading gets `-1`, `-2`, ...
pub fn anchors(text: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut fence = false;
    for line in text.lines() {
        if is_fence(line) {
            fence = !fence;
        } else if !fence && let Some(heading) = heading_text(line) {
            let base = slug(heading);
            match seen.get_mut(&base) {
                Some(count) => {
                    found.insert(format!("{base}-{count}"));
                    *count += 1;
                }
                None => {
                    found.insert(base.clone());
                    seen.insert(base, 1);
                }
            }
        }
    }
    found
}

/// One line with its inline code spans blanked out.
fn blank_code_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                out.push_str(&rest[..open]);
                out.extend(rest[open..open + close + 2].chars().map(|_| ' '));
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// The targets of `[text](target)` and `![alt](target)` on one line, with an
/// optional `"title"` after the target.
fn line_links(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut from = 0;
    while let Some(at) = line[from..].find("](").map(|at| at + from) {
        from = at + 2;
        let Some(open) = line[..at].rfind('[') else {
            continue;
        };
        if line[open + 1..at].contains(']') {
            continue;
        }
        let rest = &line[at + 2..];
        let end = rest
            .find(|c: char| c == ')' || c.is_whitespace())
            .unwrap_or(rest.len());
        if end == 0 {
            continue;
        }
        let tail = &rest[end..];
        let closes = tail.starts_with(')')
            || tail
                .strip_prefix(char::is_whitespace)
                .is_some_and(|titled| {
                    titled
                        .trim_start()
                        .strip_prefix('"')
                        .and_then(|title| title.split_once('"'))
                        .is_some_and(|(_, after)| after.starts_with(')'))
                });
        if closes {
            targets.push(rest[..end].to_owned());
        }
    }
    targets
}

/// Every link target in a document, numbered from line 1, outside fenced
/// blocks and inline code.
pub fn links(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut fence = false;
    for (index, line) in text.lines().enumerate() {
        if is_fence(line) {
            fence = !fence;
        } else if !fence {
            found.extend(
                line_links(&blank_code_spans(line))
                    .into_iter()
                    .map(|target| (index + 1, target)),
            );
        }
    }
    found
}

/// `scheme:` with at least two scheme characters, so a drive letter is not one.
fn has_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once(':') else {
        return false;
    };
    scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Decodes `%XX` escapes; anything malformed is kept as written.
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let decoded = (bytes[at] == b'%')
            .then(|| text.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why a link does not resolve, or `None` when it does. `md_rel` is the
/// linking file relative to `root`; `cache` holds the anchors per target.
fn broken_reason(
    root: &Path,
    md_rel: &str,
    target: &str,
    cache: &mut HashMap<String, HashSet<String>>,
) -> Option<String> {
    if has_scheme(target) {
        return None;
    }
    let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
    let (path, anchor) = (unquote(path), unquote(anchor));
    let linked = if path.is_empty() {
        md_rel.to_owned()
    } else if let Some(from_root) = path.strip_prefix('/') {
        from_root.to_owned()
    } else {
        match md_rel.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{path}"),
            None => path,
        }
    };
    let full = root.join(&linked);
    if !full.exists() {
        return Some("file not found".to_owned());
    }
    if anchor.is_empty() || !linked.ends_with(".md") || !full.is_file() {
        return None;
    }
    if !cache.contains_key(&linked) {
        let text = std::fs::read(&full)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        cache.insert(linked.clone(), anchors(&text));
    }
    (!cache[&linked].contains(&anchor)).then(|| "anchor not found".to_owned())
}

/// One link that does not resolve.
#[derive(Debug, PartialEq, Eq)]
pub struct Broken {
    pub path: String,
    pub line: usize,
    pub target: String,
    pub reason: String,
    pub frozen: bool,
}

/// What a scan of the tracked Markdown files found.
#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    pub links: usize,
    pub broken: Vec<Broken>,
}

impl Report {
    pub fn failures(&self) -> usize {
        self.broken.iter().filter(|b| !b.frozen).count()
    }
}

/// Checks every tracked `*.md` file under `root` (a git working tree).
pub fn check_repo(root: &Path) -> Result<Report> {
    let mut report = Report::default();
    let mut cache = HashMap::new();
    for rel in tracked_files(root)? {
        if !rel.ends_with(".md") || rel.starts_with(VENDORED_PREFIX) {
            continue;
        }
        let path = root.join(&rel);
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {rel}"))?;
        report.files += 1;
        let frozen = is_frozen(&rel);
        for (line, target) in links(&String::from_utf8_lossy(&bytes)) {
            if has_scheme(&target) {
                continue;
            }
            report.links += 1;
            if let Some(reason) = broken_reason(root, &rel, &target, &mut cache) {
                report.broken.push(Broken {
                    path: rel.clone(),
                    line,
                    target,
                    reason,
                    frozen,
                });
            }
        }
    }
    Ok(report)
}

pub fn run() -> Result<()> {
    let report = check_repo(&workspace_root())?;
    for broken in &report.broken {
        let note = if broken.frozen {
            "; frozen record, reported only"
        } else {
            ""
        };
        println!(
            "{}:{}: {} ({}{note})",
            broken.path, broken.line, broken.target, broken.reason
        );
    }
    let failures = report.failures();
    let frozen = report.broken.len() - failures;
    println!(
        "docs check: {} Markdown files, {} relative links, {failures} broken, {frozen} broken in frozen records (reported, never fail)",
        report.files, report.links
    );
    if failures > 0 {
        bail!("docs check: {failures} broken link(s); fix them or the heading they point at");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A git repository under the system temp directory, removed on drop.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "xtask-docs-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn track(&self) {
            for args in [["init", "-q"], ["add", "-A"]] {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&self.0)
                    .args(args)
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .env_remove("GIT_COMMON_DIR")
                    .env_remove("GIT_INDEX_FILE")
                    .output()
                    .expect("git is required for the repository tests");
                assert!(output.status.success());
            }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn slugs_follow_github() {
        for (heading, expected) in [
            ("Plain Title", "plain-title"),
            ("What's new? (v2.0)", "whats-new-v20"),
            ("`code` in a heading", "code-in-a-heading"),
            ("[Linked](other.md) text", "linked-text"),
            ("snake_case and-dash", "snake_case-and-dash"),
            ("Two  spaces", "two--spaces"),
            ("Ünïcode Ärger", "ünïcode-ärger"),
            ("C#", "c"),
        ] {
            assert_eq!(slug(heading), expected, "{heading}");
        }
    }

    #[test]
    fn anchors_dedupe_skip_fences_and_strip_closing_hashes() {
        let text =
            "# Title\n## Same\n## Same\n### Same ###\n```\n# not a heading\n```\n#nospace\n## C#\n";
        let found = anchors(text);
        for expected in ["title", "same", "same-1", "same-2", "c"] {
            assert!(found.contains(expected), "{expected} in {found:?}");
        }
        assert!(!found.contains("not-a-heading"));
        assert!(!found.contains("nospace"));
        assert_eq!(found.len(), 5);
    }

    #[test]
    fn links_are_found_with_line_numbers() {
        let text = "See [a](one.md) and ![img](pic.png \"A title\").\n\
                    ```\n[skipped](fenced.md)\n```\n\
                    Code `[x](inline.md)` is skipped, [b](two.md#sec) is not.\n\
                    [brackets] (not a link) and [open](missing-close.md\n\
                    [out](https://example.com/x) and [mail](mailto:a@b.c)\n";
        assert_eq!(
            links(text),
            [
                (1, "one.md".to_owned()),
                (1, "pic.png".to_owned()),
                (5, "two.md#sec".to_owned()),
                (7, "https://example.com/x".to_owned()),
                (7, "mailto:a@b.c".to_owned()),
            ]
        );
    }

    #[test]
    fn schemes_are_external_but_drive_letters_are_not() {
        assert!(has_scheme("https://example.com"));
        assert!(has_scheme("mailto:a@b.c"));
        assert!(!has_scheme("C:/dir/file.md"));
        assert!(!has_scheme("docs/a.md#x:y"));
        assert!(!has_scheme("#anchor"));
    }

    #[test]
    fn a_repository_check_reports_broken_links_and_anchors() {
        let repo = TempRepo::new();
        repo.write(
            "README.md",
            "# Top\n[ok](docs/a.md#section-one) [self](#top) [dir](docs/)\n\
             [gone](docs/missing.md)\n[bad](docs/a.md#nope)\n\
             [root](/docs/a.md) [enc](docs/a%20b.md) [web](https://example.com/x.md)\n",
        );
        repo.write("docs/a.md", "# A\n## Section One\n[up](../README.md#top)\n");
        repo.write("docs/a b.md", "plain\n");
        repo.write(".agents/skills/rust-skills/x.md", "[v](gone.md)\n");
        repo.write("docs/spike/multimap/v2/m1-prereg.md", "[old](gone.md)\n");
        repo.write("docs/spike/multimap/v2/guide.md", "[new](gone.md)\n");
        repo.track();

        let report = check_repo(&repo.0).unwrap();
        let found: Vec<_> = report
            .broken
            .iter()
            .map(|b| {
                (
                    b.path.as_str(),
                    b.line,
                    b.target.as_str(),
                    b.reason.as_str(),
                    b.frozen,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("README.md", 3, "docs/missing.md", "file not found", false),
                ("README.md", 4, "docs/a.md#nope", "anchor not found", false),
                (
                    "docs/spike/multimap/v2/guide.md",
                    1,
                    "gone.md",
                    "file not found",
                    false
                ),
                (
                    "docs/spike/multimap/v2/m1-prereg.md",
                    1,
                    "gone.md",
                    "file not found",
                    true
                ),
            ]
        );
        assert_eq!(report.failures(), 3);
        // The vendored file is not scanned at all.
        assert_eq!(report.files, 5);
    }

    #[test]
    fn frozen_names_are_prereg_and_results_files_under_the_v2_directory() {
        assert!(is_frozen("docs/spike/multimap/v2/m0.3-prereg.md"));
        assert!(is_frozen("docs/spike/multimap/v2/m0.3-prereg-amendment.md"));
        assert!(is_frozen("docs/spike/multimap/v2/engine-results.md"));
        assert!(!is_frozen("docs/spike/multimap/v2/trend.md"));
        assert!(!is_frozen("docs/spike/multimap/m0-results.md"));
    }
}
