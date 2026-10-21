//! `cargo xtask docs check`: every relative Markdown link and anchor in the
//! tracked `*.md` files must resolve to a tracked path, and the anchor to a
//! heading slug in GitHub's style (`docs/development/documentation.md`, rule 9).
//!
//! Documents are parsed with `pulldown-cmark`, so inline, reference (used or
//! only defined) and image links, angle-bracket targets, nested brackets, and
//! ATX and setext headings follow CommonMark. Links inside code are not links.
//! External targets (`scheme:` such as http, https or mailto, and `//host`)
//! are skipped. Targets resolve against the `git ls-files` paths with `/`
//! separators and case-sensitive matching, never the host filesystem, so the
//! result is the same on every OS. Vendored `.agents/skills/rust-skills/**` is
//! not scanned. Frozen records (the pre-registrations and results under
//! `docs/spike/multimap/v2`) are scanned and reported but never fail the
//! check, because they are never edited.
//!
//! The same check also rejects merge-conflict markers in any scanned Markdown
//! file (a line starting `<<<<<<< ` or `>>>>>>> `, or exactly `=======`) and
//! guards the decision log, `docs/decisions.md`, which is merged with
//! `merge=union`: every table row's first cell must be a decision id
//! (`dNNN`) and no id may repeat. Ids need not be in order, because rows land
//! out of order by design.

use crate::acceptance::workspace_root;
use crate::lint::tracked_files;
use anyhow::{Context, Result, bail};
use pulldown_cmark::{BrokenLink, Event, LinkType, Options, Parser, Tag, TagEnd};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Vendored third-party skill material: not ours to rewrite.
const VENDORED_PREFIX: &str = ".agents/skills/rust-skills/";
/// The decision log, an append-only table merged with `merge=union`.
const DECISIONS_LOG: &str = "docs/decisions.md";
/// Where frozen records live.
const FROZEN_DIR: &str = "docs/spike/multimap/v2/";

/// Frozen records are never edited, so their broken links cannot be fixed.
fn is_frozen(rel: &str) -> bool {
    rel.starts_with(FROZEN_DIR)
        && rel.rsplit('/').next().is_some_and(|name| {
            name.ends_with(".md") && (name.contains("-prereg") || name.ends_with("-results.md"))
        })
}

/// GitHub's heading anchor for the plain text of a heading: lower-cased,
/// everything but letters, digits, spaces, `-` and `_` dropped, spaces to `-`.
pub fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The `id` and `name` attributes of the `<a ...>` tags in a piece of HTML.
fn html_anchors(html: &str, out: &mut HashSet<String>) {
    let lowered = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lowered[from..].find("<a").map(|at| at + from) {
        from = at + 2;
        if !lowered[from..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let end = lowered[from..]
            .find('>')
            .map_or(html.len(), |end| end + from);
        let tag = &html[from..end];
        let mut rest = tag;
        loop {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
            let name_len = rest
                .find(|c: char| c.is_ascii_whitespace() || matches!(c, '=' | '/'))
                .unwrap_or(rest.len());
            if name_len == 0 {
                break;
            }
            let name = rest[..name_len].to_ascii_lowercase();
            rest = rest[name_len..].trim_start();
            let Some(after_eq) = rest.strip_prefix('=') else {
                continue;
            };
            let after_eq = after_eq.trim_start();
            let (value, tail) = match after_eq.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let body = &after_eq[1..];
                    let close = body.find(quote).unwrap_or(body.len());
                    (&body[..close], body.get(close + 1..).unwrap_or(""))
                }
                _ => {
                    let close = after_eq
                        .find(|c: char| c.is_ascii_whitespace())
                        .unwrap_or(after_eq.len());
                    (&after_eq[..close], &after_eq[close..])
                }
            };
            if (name == "id" || name == "name") && !value.is_empty() {
                out.insert(value.to_owned());
            }
            rest = tail;
        }
        from = end;
    }
}

/// What one Markdown document contains that the check cares about.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// `(line, target)` of every link, image and reference definition, as the
    /// parser decoded it (escapes and entities resolved), in line order.
    pub links: Vec<(usize, String)>,
    /// `(line, label)` of every `[text][label]` or `[text][]` whose label has
    /// no definition. A bare `[text]` without a definition is plain text.
    pub undefined: Vec<(usize, String)>,
    /// Every anchor the document defines: heading slugs (a repeated heading
    /// gets `-1`, `-2`, ...) and `<a id>` or `<a name>` attributes.
    pub anchors: HashSet<String>,
}

/// Parses a document.
pub fn scan(text: &str) -> Scan {
    let mut line_starts = vec![0];
    line_starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    let line_of = |offset: usize| line_starts.partition_point(|start| *start <= offset);

    let mut found = Scan::default();
    let mut undefined = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS;
        let callback = |link: BrokenLink<'_>| {
            if matches!(link.link_type, LinkType::Reference | LinkType::Collapsed) {
                undefined.push((link.span.start, link.reference.to_string()));
            }
            None
        };
        let mut parser =
            Parser::new_with_broken_link_callback(text, options, Some(callback)).into_offset_iter();
        let mut heading: Option<String> = None;
        for (event, range) in &mut parser {
            match event {
                Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
                Event::End(TagEnd::Heading(_)) => headings.extend(heading.take()),
                Event::Text(text) | Event::Code(text) => {
                    if let Some(heading) = heading.as_mut() {
                        heading.push_str(&text);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(heading) = heading.as_mut() {
                        heading.push(' ');
                    }
                }
                Event::Html(html) | Event::InlineHtml(html) => {
                    html_anchors(&html, &mut found.anchors);
                }
                Event::Start(Tag::Link {
                    link_type: LinkType::Inline,
                    dest_url,
                    ..
                })
                | Event::Start(Tag::Image {
                    link_type: LinkType::Inline,
                    dest_url,
                    ..
                }) => found
                    .links
                    .push((line_of(range.start), dest_url.to_string())),
                _ => {}
            }
        }
        for (_, definition) in parser.reference_definitions().iter() {
            found
                .links
                .push((line_of(definition.span.start), definition.dest.to_string()));
        }
    }
    found.links.sort_by_key(|(line, _)| *line);
    found.undefined = undefined
        .into_iter()
        .map(|(offset, label)| (line_of(offset), label))
        .collect();

    let mut seen: HashMap<String, usize> = HashMap::new();
    for heading in headings {
        let base = slug(&heading);
        match seen.get_mut(&base) {
            Some(count) => {
                found.anchors.insert(format!("{base}-{count}"));
                *count += 1;
            }
            None => {
                found.anchors.insert(base.clone());
                seen.insert(base, 1);
            }
        }
    }
    found
}

/// `scheme:` with at least two scheme characters (so a drive letter is not
/// one), or a protocol-relative `//host`.
fn is_external(target: &str) -> bool {
    if target.starts_with("//") {
        return true;
    }
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

/// Splits a link target into its decoded path and fragment. The fragment
/// starts at the first `#` (an escaped one is `%23` and stays in the path);
/// a query string is dropped from the path.
fn split_target(target: &str) -> (String, String) {
    let (path, fragment) = target.split_once('#').unwrap_or((target, ""));
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    (unquote(path), unquote(fragment))
}

/// The tracked paths, as sets for exact, case-sensitive lookups.
#[derive(Debug, Default)]
struct Tracked {
    files: HashSet<String>,
    dirs: HashSet<String>,
    /// Lower-cased path to the path as tracked, to name a case mismatch.
    folded: HashMap<String, String>,
}

impl Tracked {
    fn new(paths: &[String]) -> Self {
        let mut tracked = Self::default();
        tracked.dirs.insert(String::new());
        for path in paths {
            tracked.files.insert(path.clone());
            tracked.folded.insert(path.to_lowercase(), path.clone());
            for (at, _) in path.match_indices('/') {
                let dir = &path[..at];
                tracked.dirs.insert(dir.to_owned());
                tracked.folded.insert(dir.to_lowercase(), dir.to_owned());
            }
        }
        tracked
    }
}

/// The repository-relative path a link names, with `.` and `..` resolved
/// lexically; `None` when it climbs out of the repository. `md_rel` is the
/// linking file; a leading `/` is the repository root.
fn resolve(md_rel: &str, path: &str) -> Option<String> {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<&str> = md_rel.split('/').collect();
        dir.pop();
        dir
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    Some(parts.join("/"))
}

/// Checks links against the tracked paths and caches each target's anchors.
struct Checker<'a> {
    root: &'a Path,
    tracked: Tracked,
    anchors: HashMap<String, HashSet<String>>,
}

impl Checker<'_> {
    /// Why a link does not resolve, or `None` when it does.
    fn broken_reason(&mut self, md_rel: &str, target: &str) -> Result<Option<String>> {
        let (path, anchor) = split_target(target);
        let linked = if path.is_empty() {
            md_rel.to_owned()
        } else {
            match resolve(md_rel, &path) {
                Some(linked) => linked,
                None => return Ok(Some("outside the repository".to_owned())),
            }
        };
        let is_file = self.tracked.files.contains(&linked);
        if !is_file && !self.tracked.dirs.contains(&linked) {
            return Ok(Some(
                match self.tracked.folded.get(&linked.to_lowercase()) {
                    Some(real) => format!("not a tracked path; differs only in case from {real}"),
                    None => "not a tracked path".to_owned(),
                },
            ));
        }
        if anchor.is_empty() || !is_file || !linked.ends_with(".md") {
            return Ok(None);
        }
        if !self.anchors.contains_key(&linked) {
            let bytes = std::fs::read(self.root.join(&linked))
                .with_context(|| format!("read {linked} (a link target)"))?;
            self.anchors.insert(
                linked.clone(),
                scan(&String::from_utf8_lossy(&bytes)).anchors,
            );
        }
        Ok((!self.anchors[&linked].contains(&anchor)).then(|| "anchor not found".to_owned()))
    }
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

/// One merge-conflict marker or decision-log fault.
#[derive(Debug, PartialEq, Eq)]
pub struct LogIssue {
    pub path: String,
    pub line: usize,
    pub reason: String,
    pub frozen: bool,
}

/// What a scan of the tracked Markdown files found.
#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    pub links: usize,
    pub broken: Vec<Broken>,
    /// Conflict markers in any scanned file and decision-log faults.
    pub issues: Vec<LogIssue>,
}

impl Report {
    pub fn failures(&self) -> usize {
        self.broken.iter().filter(|b| !b.frozen).count()
            + self.issues.iter().filter(|i| !i.frozen).count()
    }
}

/// `(line, reason)` of every line that is a merge-conflict marker: it starts
/// with `<<<<<<< ` or `>>>>>>> `, or is exactly `=======` in a file that also
/// has one of the other two (alone, `=======` is a Setext heading underline).
pub fn conflict_markers(text: &str) -> Vec<(usize, String)> {
    let found: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter_map(|(at, line)| {
            let marker = if line.starts_with("<<<<<<< ") {
                "<<<<<<<"
            } else if line.starts_with(">>>>>>> ") {
                ">>>>>>>"
            } else if line == "=======" {
                "======="
            } else {
                return None;
            };
            Some((at + 1, marker))
        })
        .collect();
    let bracketed = found.iter().any(|&(_, marker)| marker != "=======");
    found
        .into_iter()
        .filter(|&(_, marker)| bracketed || marker != "=======")
        .map(|(line, marker)| (line, format!("merge-conflict marker `{marker}`")))
        .collect()
}

/// `(line, reason)` of every decision-log fault: a table row whose first cell
/// is not `dNNN`, or an id that already appeared on an earlier row. The header
/// row (`id`) and the separator row are not decisions.
pub fn decision_log_faults(text: &str) -> Vec<(usize, String)> {
    let mut faults = Vec::new();
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    for (at, line) in text.lines().enumerate() {
        let Some(row) = line.strip_prefix('|') else {
            continue;
        };
        let cell = row.split('|').next().unwrap_or("").trim();
        if cell == "id" || (!cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':'))) {
            continue;
        }
        let is_id = cell
            .strip_prefix('d')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !is_id {
            faults.push((
                at + 1,
                format!("row's first cell `{cell}` is not a decision id (dNNN)"),
            ));
        } else if let Some(first) = first_seen.get(cell) {
            faults.push((
                at + 1,
                format!("duplicate decision id {cell} (first on line {first})"),
            ));
        } else {
            first_seen.insert(cell, at + 1);
        }
    }
    faults
}

/// Checks every tracked `*.md` file under `root` (a git working tree).
pub fn check_repo(root: &Path) -> Result<Report> {
    let paths = tracked_files(root)?;
    let mut checker = Checker {
        root,
        tracked: Tracked::new(&paths),
        anchors: HashMap::new(),
    };
    let mut report = Report::default();
    for rel in &paths {
        if !rel.ends_with(".md") || rel.starts_with(VENDORED_PREFIX) {
            continue;
        }
        let path = root.join(rel);
        // Tracked but deleted in the working tree: nothing to read.
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {rel}"))?;
        report.files += 1;
        let frozen = is_frozen(rel);
        let text = String::from_utf8_lossy(&bytes);
        let mut faults = conflict_markers(&text);
        if rel == DECISIONS_LOG {
            faults.extend(decision_log_faults(&text));
        }
        report
            .issues
            .extend(faults.into_iter().map(|(line, reason)| LogIssue {
                path: rel.clone(),
                line,
                reason,
                frozen,
            }));
        let found = scan(&text);
        let mut broken = |line: usize, target: String, reason: String| {
            report.broken.push(Broken {
                path: rel.clone(),
                line,
                target,
                reason,
                frozen,
            });
        };
        let mut checked = 0;
        for (line, target) in found.links {
            if is_external(&target) {
                continue;
            }
            checked += 1;
            if let Some(reason) = checker.broken_reason(rel, &target)? {
                broken(line, target, reason);
            }
        }
        for (line, label) in found.undefined {
            checked += 1;
            broken(line, format!("[{label}]"), "undefined reference".to_owned());
        }
        report.links += checked;
    }
    report
        .issues
        .sort_by(|a, b| (a.path.as_str(), a.line).cmp(&(b.path.as_str(), b.line)));
    report.broken.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.target.as_str()).cmp(&(
            b.path.as_str(),
            b.line,
            b.target.as_str(),
        ))
    });
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
    for issue in &report.issues {
        let note = if issue.frozen {
            "; frozen record, reported only"
        } else {
            ""
        };
        println!("{}:{}: {}{note}", issue.path, issue.line, issue.reason);
    }
    let failures = report.failures();
    let frozen = report.broken.len() + report.issues.len() - failures;
    println!(
        "docs check: {} Markdown files, {} relative links, {failures} problems (broken links, conflict markers, decision log), {frozen} in frozen records (reported, never fail)",
        report.files, report.links
    );
    if failures > 0 {
        bail!(
            "docs check: {failures} problem(s); fix the links, remove the conflict markers, or repair the decision log (unique `dNNN` ids)"
        );
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

    fn targets(text: &str) -> Vec<(usize, String)> {
        scan(text).links
    }

    fn sorted(set: &HashSet<String>) -> Vec<&str> {
        let mut all: Vec<&str> = set.iter().map(String::as_str).collect();
        all.sort_unstable();
        all
    }

    #[test]
    fn slugs_follow_github() {
        for (heading, expected) in [
            ("Plain Title", "plain-title"),
            ("What's new? (v2.0)", "whats-new-v20"),
            ("snake_case and-dash", "snake_case-and-dash"),
            ("Two  spaces", "two--spaces"),
            ("Ünïcode Ärger", "ünïcode-ärger"),
            ("C#", "c"),
        ] {
            assert_eq!(slug(heading), expected, "{heading}");
        }
    }

    #[test]
    fn heading_anchors_come_from_the_rendered_text() {
        let text = "# Title\n## `code` and *emphasis*\n## [Linked](other.md) text\n\
                    ## Same\n## Same\n### Same ###\n## C#\n\
                    ```\n# not a heading\n```\n#nospace\n";
        let found = scan(text).anchors;
        assert_eq!(
            sorted(&found),
            [
                "c",
                "code-and-emphasis",
                "linked-text",
                "same",
                "same-1",
                "same-2",
                "title"
            ]
        );
    }

    #[test]
    fn setext_headings_are_anchors() {
        let found = scan("Big title\n=========\n\nSecond `part`\n-------------\n").anchors;
        assert_eq!(sorted(&found), ["big-title", "second-part"]);
    }

    #[test]
    fn html_anchors_by_id_and_name_are_anchors() {
        let text = "<a id=\"by-id\"></a>\n\nText <a href=\"x\" name='by-name'>here</a> and \
                    <A ID=upper>x</A> and <abbr id=\"not-an-anchor\">y</abbr>.\n";
        let found = scan(text).anchors;
        assert_eq!(sorted(&found), ["by-id", "by-name", "upper"]);
    }

    #[test]
    fn inline_links_and_images_are_found_with_line_numbers() {
        let text = "See [a](one.md) and ![img](pic.png \"A title\").\n\
                    ```\n[skipped](fenced.md)\n```\n\
                    Code `[x](inline.md)` is skipped, [b](two.md#sec) is not.\n\
                    [out](https://example.com/x) and [mail](mailto:a@b.c)\n";
        assert_eq!(
            targets(text),
            [
                (1, "one.md".to_owned()),
                (1, "pic.png".to_owned()),
                (5, "two.md#sec".to_owned()),
                (6, "https://example.com/x".to_owned()),
                (6, "mailto:a@b.c".to_owned()),
            ]
        );
    }

    #[test]
    fn reference_definitions_are_links_used_or_not() {
        let text = "Use [full][one], [collapsed][] and [short].\n\n\
                    [one]: docs/one.md\n[collapsed]: docs/two.md#sec \"Title\"\n\
                    [short]: <docs/with space.md>\n[unused]: docs/unused.md\n";
        let mut found: Vec<_> = targets(text).into_iter().map(|(_, t)| t).collect();
        found.sort();
        assert_eq!(
            found,
            [
                "docs/one.md",
                "docs/two.md#sec",
                "docs/unused.md",
                "docs/with space.md"
            ]
        );
        assert_eq!(targets(text).first().map(|(line, _)| *line), Some(3));
    }

    #[test]
    fn undefined_references_are_reported_but_bare_brackets_are_text() {
        let found = scan("A [text][nowhere] and [other][] and [just brackets] and [ ] and [x].\n");
        assert_eq!(
            found.undefined,
            [(1, "nowhere".to_owned()), (1, "other".to_owned())]
        );
        assert!(found.links.is_empty());
    }

    #[test]
    fn angle_bracket_nested_and_parenthesised_targets_follow_commonmark() {
        let text = "[a](<my file.md>) [b [nested] text](two.md) [c](three(1).md) \
                    [d](four.md \"a (title)\")\n";
        assert_eq!(
            targets(text),
            [
                (1, "my file.md".to_owned()),
                (1, "two.md".to_owned()),
                (1, "three(1).md".to_owned()),
                (1, "four.md".to_owned()),
            ]
        );
        // A link whose text spans lines.
        assert_eq!(
            targets("[multi\nline](split.md)\n"),
            [(1, "split.md".to_owned())]
        );
    }

    #[test]
    fn targets_split_into_path_and_fragment_safely() {
        for (target, path, fragment) in [
            ("a.md", "a.md", ""),
            ("a.md#sec", "a.md", "sec"),
            ("#sec", "", "sec"),
            ("a.md?raw=true", "a.md", ""),
            ("a.md?x=1#sec", "a.md", "sec"),
            ("a%20b.md#two%20words", "a b.md", "two words"),
            // An escaped '#' belongs to the file name.
            ("a%23b.md", "a#b.md", ""),
            ("a%23b.md#sec", "a#b.md", "sec"),
            ("a.md#x#y", "a.md", "x#y"),
        ] {
            assert_eq!(
                split_target(target),
                (path.to_owned(), fragment.to_owned()),
                "{target}"
            );
        }
    }

    #[test]
    fn schemes_are_external_but_drive_letters_are_not() {
        assert!(is_external("https://example.com"));
        assert!(is_external("mailto:a@b.c"));
        assert!(is_external("//cdn.example.com/x.js"));
        assert!(!is_external("C:/dir/file.md"));
        assert!(!is_external("docs/a.md#x:y"));
        assert!(!is_external("#anchor"));
    }

    #[test]
    fn paths_resolve_lexically_from_the_linking_file() {
        assert_eq!(
            resolve("README.md", "docs/a.md").as_deref(),
            Some("docs/a.md")
        );
        assert_eq!(
            resolve("docs/a.md", "../README.md").as_deref(),
            Some("README.md")
        );
        assert_eq!(
            resolve("docs/a.md", "./b/../c.md").as_deref(),
            Some("docs/c.md")
        );
        assert_eq!(resolve("docs/a.md", "/x/y.md").as_deref(), Some("x/y.md"));
        assert_eq!(resolve("docs/a.md", "sub/").as_deref(), Some("docs/sub"));
        assert_eq!(resolve("README.md", "../outside.md"), None);
    }

    fn broken_in(repo: &TempRepo) -> Vec<(String, usize, String, String, bool)> {
        check_repo(&repo.0)
            .unwrap()
            .broken
            .into_iter()
            .map(|b| (b.path, b.line, b.target, b.reason, b.frozen))
            .collect()
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
        let tracked_path = "not a tracked path";
        assert_eq!(
            broken_in(&repo),
            [
                (
                    "README.md".to_owned(),
                    3,
                    "docs/missing.md".to_owned(),
                    tracked_path.to_owned(),
                    false
                ),
                (
                    "README.md".to_owned(),
                    4,
                    "docs/a.md#nope".to_owned(),
                    "anchor not found".to_owned(),
                    false
                ),
                (
                    "docs/spike/multimap/v2/guide.md".to_owned(),
                    1,
                    "gone.md".to_owned(),
                    tracked_path.to_owned(),
                    false
                ),
                (
                    "docs/spike/multimap/v2/m1-prereg.md".to_owned(),
                    1,
                    "gone.md".to_owned(),
                    tracked_path.to_owned(),
                    true
                ),
            ]
        );
        assert_eq!(report.failures(), 3);
        // The vendored file is not scanned at all.
        assert_eq!(report.files, 5);
    }

    #[test]
    fn a_repository_check_covers_the_markdown_forms_inline_scanning_missed() {
        let repo = TempRepo::new();
        repo.write(
            "index.md",
            "[ref ok][good] and [ref bad][bad] and [undef][nowhere]\n\n\
             [good]: docs/a.md#setext-title\n[bad]: docs/gone.md\n\n\
             [angle ok](<docs/with space.md>) [angle bad](<docs/no space.md>)\n\
             [nested [ok] text](docs/a.md#html-spot) [nested [inner] text](docs/gone2.md)\n\
             [escaped](docs/a%23b.md) [escaped frag](docs/a.md#setext-title) \
             [query](docs/a.md?plain=1#setext-title) [badfrag](docs/a.md#x#y)\n",
        );
        repo.write(
            "docs/a.md",
            "Setext Title\n============\n\n<a id=\"html-spot\"></a>\n",
        );
        repo.write("docs/a#b.md", "hash in the name\n");
        repo.write("docs/with space.md", "x\n");
        repo.track();

        let found: Vec<(usize, String, String)> = broken_in(&repo)
            .into_iter()
            .map(|(_, line, target, reason, _)| (line, target, reason))
            .collect();
        let tracked_path = "not a tracked path".to_owned();
        assert_eq!(
            found,
            [
                (1, "[nowhere]".to_owned(), "undefined reference".to_owned()),
                (4, "docs/gone.md".to_owned(), tracked_path.clone()),
                (6, "docs/no space.md".to_owned(), tracked_path.clone()),
                (7, "docs/gone2.md".to_owned(), tracked_path),
                (8, "docs/a.md#x#y".to_owned(), "anchor not found".to_owned()),
            ]
        );
    }

    #[test]
    fn a_wrong_case_path_fails_on_every_platform() {
        let repo = TempRepo::new();
        repo.write(
            "README.md",
            "[wrong file](docs/Guide.md) [wrong dir](Docs/guide.md) [right](docs/guide.md)\n\
             [back\\slash](docs\\guide.md)\n",
        );
        repo.write("docs/guide.md", "# Guide\n");
        repo.track();
        let found: Vec<(usize, String, String)> = broken_in(&repo)
            .into_iter()
            .map(|(_, line, target, reason, _)| (line, target, reason))
            .collect();
        assert_eq!(
            found,
            [
                (
                    1,
                    "Docs/guide.md".to_owned(),
                    "not a tracked path; differs only in case from docs/guide.md".to_owned()
                ),
                (
                    1,
                    "docs/Guide.md".to_owned(),
                    "not a tracked path; differs only in case from docs/guide.md".to_owned()
                ),
                (
                    2,
                    "docs\\guide.md".to_owned(),
                    "not a tracked path".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_link_out_of_the_repository_and_an_untracked_target_fail() {
        let repo = TempRepo::new();
        repo.write("README.md", "[up](../elsewhere.md) [new](new.md)\n");
        repo.track();
        repo.write("new.md", "not added to git\n");
        let found: Vec<_> = broken_in(&repo)
            .into_iter()
            .map(|(_, _, target, reason, _)| (target, reason))
            .collect();
        assert_eq!(
            found,
            [
                (
                    "../elsewhere.md".to_owned(),
                    "outside the repository".to_owned()
                ),
                ("new.md".to_owned(), "not a tracked path".to_owned()),
            ]
        );
    }

    #[test]
    fn an_unreadable_anchor_target_is_an_error_not_a_pass() {
        let repo = TempRepo::new();
        repo.write("README.md", "[x](other.md#top)\n");
        repo.write("other.md", "# Top\n");
        repo.track();
        std::fs::remove_file(repo.0.join("other.md")).unwrap();
        let error = check_repo(&repo.0).unwrap_err();
        assert!(format!("{error:#}").contains("other.md"), "{error:#}");
    }

    #[test]
    fn frozen_names_are_prereg_and_results_files_under_the_v2_directory() {
        assert!(is_frozen("docs/spike/multimap/v2/m0.3-prereg.md"));
        assert!(is_frozen("docs/spike/multimap/v2/m0.3-prereg-amendment.md"));
        assert!(is_frozen("docs/spike/multimap/v2/engine-results.md"));
        assert!(!is_frozen("docs/spike/multimap/v2/trend.md"));
        assert!(!is_frozen("docs/spike/multimap/m0-results.md"));
    }

    #[test]
    fn conflict_markers_are_found_with_line_numbers() {
        let text = "ok\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n\
                    <<<<<<<nospace\n======== longer\n  =======\n";
        let found: Vec<usize> = conflict_markers(text)
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(found, [2, 4, 6]);
        assert_eq!(
            conflict_markers("<<<<<<< HEAD\r\na\r\n=======\r\n").len(),
            2
        );
        // A lone `=======` is a Setext heading underline, not a marker.
        assert!(conflict_markers("Title\n=======\ntext\n").is_empty());
        assert!(conflict_markers("no markers\n| d001 | x |\n").is_empty());
    }

    const LOG_HEAD: &str = "Intro\n\n| id | date | kind | area | text | ref |\n| --- | --- | --- | --- | --- | --- |\n";

    #[test]
    fn an_out_of_order_decision_table_is_valid() {
        let text = format!("{LOG_HEAD}| d010 | a |\n| d002 | b |\n| d1000 | c |\n| d003 | d |\n");
        assert!(decision_log_faults(&text).is_empty());
    }

    #[test]
    fn a_duplicate_decision_id_is_reported_on_the_later_row() {
        let text = format!("{LOG_HEAD}| d010 | a |\n| d011 | b |\n| d010 | c |\n");
        assert_eq!(
            decision_log_faults(&text),
            [(7, "duplicate decision id d010 (first on line 5)".to_owned())]
        );
    }

    #[test]
    fn a_row_without_a_decision_id_is_reported() {
        let text =
            format!("{LOG_HEAD}| d010 | a |\n| 11 | b |\n| d | c |\n| dx1 | d |\n|  | e |\n");
        let lines: Vec<usize> = decision_log_faults(&text)
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, [6, 7, 8, 9]);
    }

    #[test]
    fn a_repository_check_fails_on_conflict_markers_and_a_bad_decision_log() {
        let repo = TempRepo::new();
        repo.write(
            "README.md",
            "# Top\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n",
        );
        repo.write(
            DECISIONS_LOG,
            &format!("{LOG_HEAD}| d002 | a |\n| d001 | b |\n| d002 | c |\n"),
        );
        repo.write("docs/other.md", "| not-a-decision | x |\n");
        repo.write(".agents/skills/rust-skills/x.md", "<<<<<<< vendored\n");
        repo.write("docs/spike/multimap/v2/m1-prereg.md", "<<<<<<< frozen\n");
        repo.track();
        let report = check_repo(&repo.0).unwrap();
        let found: Vec<(&str, usize, bool)> = report
            .issues
            .iter()
            .map(|issue| (issue.path.as_str(), issue.line, issue.frozen))
            .collect();
        assert_eq!(
            found,
            [
                ("README.md", 2, false),
                ("README.md", 4, false),
                ("README.md", 6, false),
                ("docs/decisions.md", 7, false),
                ("docs/spike/multimap/v2/m1-prereg.md", 1, true),
            ]
        );
        assert_eq!(report.failures(), 4);
    }

    #[test]
    fn a_clean_out_of_order_log_passes_the_repository_check() {
        let repo = TempRepo::new();
        repo.write(
            DECISIONS_LOG,
            &format!("{LOG_HEAD}| d005 | a |\n| d003 | b |\n| d004 | c |\n"),
        );
        repo.track();
        let report = check_repo(&repo.0).unwrap();
        assert!(report.issues.is_empty());
        assert_eq!(report.failures(), 0);
    }
}
