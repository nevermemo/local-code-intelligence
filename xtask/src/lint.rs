//! `cargo xtask lint-machine-paths`: fails when a tracked text file names a
//! machine-specific location. The repository is public, so committed text
//! names a value by its role and resolver instead (see
//! `docs/development/machine-values.md`, "Lint").
//!
//! The built-in patterns are generic: a drive-letter user profile path, a
//! macOS or Linux user directory, an `AppData` path component and a client's
//! session scratch folder under a temp directory. Owner-specific tokens (a
//! user name, a local folder name) never live in this file; they go one per
//! line in the gitignored `.local/lint-machine-paths.txt`, which is read when
//! it exists.
//!
//! The lint scans its own source and tests, so neither spells a pattern out:
//! the matchers are code and the sample lines are assembled at run time.

use crate::acceptance::workspace_root;
use anyhow::{Context, Result, bail};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

/// Extra literal patterns, one per line, relative to the repository root.
/// `.local/` is gitignored, so the tokens stay on the machine.
const EXTRA_PATTERNS_FILE: &str = ".local/lint-machine-paths.txt";
/// Vendored third-party skill material: not ours to rewrite.
const EXEMPT_PREFIX: &str = ".agents/skills/rust-skills/";
/// git's own binary heuristic: a NUL byte among the first 8000 bytes.
const BINARY_PROBE: usize = 8000;

const USERS: &str = "users";
const APP_DATA: &str = "appdata";
const TEMP: &str = "temp";
const CLAUDE: &str = "claude";
const MAC_ROOT: &str = "/Users/";
const LINUX_ROOT: &str = "/home/";

const WINDOWS_PROFILE: &str = "drive-letter user profile path";
const MAC_PROFILE: &str = "macOS user directory";
const LINUX_HOME: &str = "Linux home directory";
const APP_DATA_FOLDER: &str = "AppData path component";
const CLIENT_TEMP: &str = "client session folder under a temp directory";

/// Literal patterns from the local file, lower-cased for a case-insensitive
/// match. The text of a pattern is never printed, only its line number.
#[derive(Debug, Default)]
pub struct Extra {
    patterns: Vec<(usize, String)>,
}

impl Extra {
    /// One literal per line; blank lines and lines starting with `#` are
    /// skipped, surrounding whitespace is trimmed.
    pub fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let line = line.trim();
                (!line.is_empty() && !line.starts_with('#'))
                    .then(|| (index + 1, line.to_lowercase()))
            })
            .collect();
        Self { patterns }
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }
}

/// Reads the local patterns file under `root`; a missing file means none.
pub fn load_extra(root: &Path) -> Result<Extra> {
    let path = root.join(EXTRA_PATTERNS_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Extra::parse(&String::from_utf8_lossy(&bytes))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Extra::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn is_separator(byte: u8) -> bool {
    byte == b'\\' || byte == b'/'
}

/// The position after a run of one or more path separators starting at `at`.
/// A run may be longer than one because a path escaped inside a string
/// literal doubles its backslashes.
fn after_separators(bytes: &[u8], at: usize) -> Option<usize> {
    let run = bytes
        .get(at..)?
        .iter()
        .take_while(|byte| is_separator(**byte))
        .count();
    (run > 0).then_some(at + run)
}

/// The position after `word` when it appears at `at`, ignoring ASCII case.
fn word_at(bytes: &[u8], at: usize, word: &str) -> Option<usize> {
    let end = at.checked_add(word.len())?;
    bytes
        .get(at..end)?
        .eq_ignore_ascii_case(word.as_bytes())
        .then_some(end)
}

/// The previous byte does not continue a longer word.
fn at_word_start(bytes: &[u8], at: usize) -> bool {
    at == 0 || !bytes[at - 1].is_ascii_alphanumeric()
}

/// A name starts with a letter or digit. Placeholders such as `<name>`,
/// `%USERNAME%`, `$USER` and `{user}` therefore do not count as one.
fn starts_a_name(text: &str, at: usize) -> bool {
    text.get(at..)
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_alphanumeric)
}

/// A drive letter, a colon, the users folder and a name, with either kind of
/// separator, in any case.
fn windows_profile(line: &str) -> bool {
    let bytes = line.as_bytes();
    (0..bytes.len().saturating_sub(1)).any(|at| {
        bytes[at].is_ascii_alphabetic()
            && bytes[at + 1] == b':'
            && at_word_start(bytes, at)
            && after_separators(bytes, at + 2)
                .and_then(|next| word_at(bytes, next, USERS))
                .and_then(|next| after_separators(bytes, next))
                .is_some_and(|next| starts_a_name(line, next))
    })
}

/// `root` (case-sensitive, ends with a slash), a name and another slash.
fn posix_user_dir(line: &str, root: &str) -> bool {
    line.match_indices(root).any(|(at, _)| {
        let rest = &line[at + root.len()..];
        let name = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .map(char::len_utf8)
            .sum::<usize>();
        starts_a_name(rest, 0) && rest[name..].starts_with('/')
    })
}

/// A path component named like the per-user application-data folder, followed
/// by a separator. `%LOCALAPPDATA%` and `$env:LOCALAPPDATA` are variables, not
/// this.
fn app_data_folder(line: &str) -> bool {
    let bytes = line.as_bytes();
    (0..bytes.len()).any(|at| {
        at_word_start(bytes, at)
            && word_at(bytes, at, APP_DATA)
                .is_some_and(|end| bytes.get(end).is_some_and(|byte| is_separator(*byte)))
    })
}

/// A temp-directory path component followed by the client's session folder.
fn client_temp(line: &str) -> bool {
    let bytes = line.as_bytes();
    (0..bytes.len()).any(|at| {
        at_word_start(bytes, at)
            && word_at(bytes, at, TEMP)
                .and_then(|end| after_separators(bytes, end))
                .and_then(|next| word_at(bytes, next, CLAUDE))
                .is_some()
    })
}

/// The name of every pattern the line matches, in a fixed order.
fn matched_patterns(line: &str, extra: &Extra) -> Vec<String> {
    let mut found = Vec::new();
    if windows_profile(line) {
        found.push(WINDOWS_PROFILE.to_owned());
    }
    if posix_user_dir(line, MAC_ROOT) {
        found.push(MAC_PROFILE.to_owned());
    }
    if posix_user_dir(line, LINUX_ROOT) {
        found.push(LINUX_HOME.to_owned());
    }
    if app_data_folder(line) {
        found.push(APP_DATA_FOLDER.to_owned());
    }
    if client_temp(line) {
        found.push(CLIENT_TEMP.to_owned());
    }
    if !extra.patterns.is_empty() {
        let lowered = line.to_lowercase();
        for (number, pattern) in &extra.patterns {
            if lowered.contains(pattern.as_str()) {
                found.push(format!(
                    "local pattern (line {number} of {EXTRA_PATTERNS_FILE})"
                ));
            }
        }
    }
    found
}

/// One line of a file that matched at least one pattern.
#[derive(Debug, PartialEq, Eq)]
pub struct Hit {
    pub line: usize,
    pub patterns: Vec<String>,
}

/// Scans text; one [`Hit`] per matching line (numbered from 1).
pub fn scan_text(text: &str, extra: &Extra) -> Vec<Hit> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let patterns = matched_patterns(line, extra);
            (!patterns.is_empty()).then_some(Hit {
                line: index + 1,
                patterns,
            })
        })
        .collect()
}

/// Scans file contents; `None` for a binary file (a NUL byte among the first
/// bytes). Text that is not valid UTF-8 is scanned lossily.
pub fn scan_bytes(bytes: &[u8], extra: &Extra) -> Option<Vec<Hit>> {
    if bytes.iter().take(BINARY_PROBE).any(|byte| *byte == 0) {
        return None;
    }
    Some(scan_text(&String::from_utf8_lossy(bytes), extra))
}

/// The tracked paths, as git prints them (forward slashes on every platform).
pub(crate) fn tracked_files(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .context("run git ls-files (git must be on PATH)")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// What a scan of the tracked files found.
#[derive(Debug, Default)]
pub struct Report {
    pub scanned: usize,
    pub binary: usize,
    /// `(path, hit)` in `git ls-files` order.
    pub hits: Vec<(String, Hit)>,
}

/// Scans every tracked text file under `root`.
pub fn scan_repo(root: &Path, extra: &Extra) -> Result<Report> {
    let mut report = Report::default();
    for rel in tracked_files(root)? {
        if rel.starts_with(EXEMPT_PREFIX) {
            continue;
        }
        let path = root.join(&rel);
        // Tracked but deleted in the working tree, a submodule, a link to a
        // directory: nothing to read.
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {rel}"))?;
        match scan_bytes(&bytes, extra) {
            None => report.binary += 1,
            Some(hits) => {
                report.scanned += 1;
                report
                    .hits
                    .extend(hits.into_iter().map(|hit| (rel.clone(), hit)));
            }
        }
    }
    Ok(report)
}

pub fn run() -> Result<()> {
    let root = workspace_root();
    let extra = load_extra(&root)?;
    let report = scan_repo(&root, &extra)?;
    for (path, hit) in &report.hits {
        println!("{path}:{}: {}", hit.line, hit.patterns.join(", "));
    }
    if !report.hits.is_empty() {
        bail!(
            "lint-machine-paths: {} line(s) name a machine-specific value; name its role and \
             resolver instead (docs/development/machine-values.md, \"Lint\")",
            report.hits.len()
        );
    }
    println!(
        "lint-machine-paths: ok ({} text files scanned, {} binary skipped, {} local pattern(s) from {EXTRA_PATTERNS_FILE})",
        report.scanned,
        report.binary,
        extra.pattern_count()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A directory under the system temp directory, removed on drop. Its name
    /// avoids the `lci-` prefix that the live-acceptance cleanup deletes.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "xtask-lint-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The lint scans this file, so sample lines are assembled at run time and
    // no literal here forms a pattern.

    const NAME: &str = "someone";

    /// `<letter>:<sep>Users<sep><name>`, the case of the folder as given.
    fn profile(letter: char, sep: &str, folder: &str, name: &str) -> String {
        format!("{letter}:{sep}{folder}{sep}{name}")
    }

    fn mac(name: &str) -> String {
        format!("{MAC_ROOT}{name}/project")
    }

    fn linux(name: &str) -> String {
        format!("{LINUX_ROOT}{name}/project")
    }

    fn app_data(sep: &str) -> String {
        format!("{}{sep}Local", "AppData")
    }

    fn client_temp_path(temp: &str, sep: &str) -> String {
        format!("{temp}{sep}claude{sep}session")
    }

    fn names(line: &str) -> Vec<String> {
        matched_patterns(line, &Extra::default())
    }

    fn is_hit(line: &str) -> bool {
        !names(line).is_empty()
    }

    #[test]
    fn a_drive_letter_user_profile_is_a_hit_in_either_separator_and_any_case() {
        for line in [
            profile('C', "\\", "Users", NAME),
            profile('c', "/", "Users", NAME),
            profile('D', "\\", "users", NAME),
            profile('E', "/", "USERS", NAME),
            // An escaped path inside a string literal doubles the backslash.
            profile('C', "\\\\", "Users", NAME),
            profile('C', "\\\\\\\\", "Users", NAME),
            // Mixed separators.
            format!("C:\\Users/{NAME}"),
        ] {
            assert_eq!(names(&line), [WINDOWS_PROFILE], "{line}");
        }
    }

    #[test]
    fn a_drive_letter_profile_is_found_inside_a_longer_line() {
        let path = profile('C', "\\", "Users", NAME);
        for line in [
            format!("Serving lives in `{path}` outside the repo."),
            format!("file:///{}", profile('C', "/", "Users", NAME)),
            format!("--config={path}\\cfg.toml"),
            format!("{path}\\data"),
            format!("see ({path})"),
        ] {
            assert_eq!(names(&line), [WINDOWS_PROFILE], "{line}");
        }
    }

    #[test]
    fn a_profile_path_without_a_real_name_is_a_placeholder() {
        for name in [
            "<name>",
            "<user>",
            "%USERNAME%",
            "$USER",
            "${USER}",
            "{user}",
            "*",
            "...",
        ] {
            let line = profile('C', "\\", "Users", name);
            assert!(!is_hit(&line), "{line}");
        }
        // The folder name alone, or with nothing after it.
        assert!(!is_hit("C:\\Users"));
        assert!(!is_hit("C:\\Users\\"));
        assert!(!is_hit("C:/Users/"));
    }

    #[test]
    fn a_literal_placeholder_name_is_still_a_name() {
        for name in ["you", "user", "foo", "Public"] {
            let line = profile('C', "\\", "Users", name);
            assert_eq!(names(&line), [WINDOWS_PROFILE], "{line}");
        }
    }

    #[test]
    fn a_drive_letter_must_stand_alone_and_the_folder_must_be_the_users_folder() {
        // "http" is a scheme, not a drive letter.
        assert!(!is_hit(&format!("http://users/{NAME}")));
        assert!(!is_hit(&format!("https://example.com/users/{NAME}")));
        assert!(!is_hit(&format!("http://localhost:3000/users/{NAME}")));
        assert!(!is_hit(&profile('C', "\\", "Windows", NAME)));
        assert!(!is_hit(&profile('C', "\\", "Program Files", NAME)));
        assert!(!is_hit(&profile('C', "\\", "UsersX", NAME)));
        assert!(!is_hit(&format!("C:{NAME}")));
        assert!(!is_hit(&format!("C:Users{NAME}")));
    }

    #[test]
    fn a_macos_user_directory_is_a_hit() {
        for line in [
            mac(NAME),
            format!("cd {} && ls", mac("Some-One_2")),
            // The Windows drive seen from WSL.
            format!("/mnt/c{}", mac(NAME)),
        ] {
            assert_eq!(names(&line), [MAC_PROFILE], "{line}");
        }
    }

    #[test]
    fn a_linux_home_directory_is_a_hit() {
        for line in [
            linux(NAME),
            format!("export X={}", linux("some.one")),
            format!("'{}'", linux("someone-else")),
        ] {
            assert_eq!(names(&line), [LINUX_HOME], "{line}");
        }
    }

    #[test]
    fn posix_user_directories_are_case_sensitive_and_need_a_name_and_a_slash() {
        // Web routes and ordinary words.
        assert!(!is_hit(&format!("/api/users/{NAME}/orders")));
        assert!(!is_hit(&format!("https://example.com/users/{NAME}/")));
        assert!(!is_hit(&format!("{}{NAME}/", "/HOME/")));
        // No name, a placeholder, no trailing slash.
        assert!(!is_hit(MAC_ROOT));
        assert!(!is_hit(LINUX_ROOT));
        assert!(!is_hit("the /home directory"));
        assert!(!is_hit(&format!("{LINUX_ROOT}<name>/project")));
        assert!(!is_hit(&format!("{MAC_ROOT}<name>/project")));
        assert!(!is_hit(&format!("{LINUX_ROOT}$USER/project")));
        assert!(!is_hit(&format!("{LINUX_ROOT}{NAME}")));
        assert!(!is_hit(&format!("{LINUX_ROOT}{NAME} and more")));
        assert!(!is_hit(&format!("{LINUX_ROOT}./project")));
    }

    #[test]
    fn an_app_data_path_component_is_a_hit() {
        for sep in ["\\", "/", "\\\\"] {
            let line = format!("%USERPROFILE%{sep}{}", app_data(sep));
            assert_eq!(names(&line), [APP_DATA_FOLDER], "{line}");
        }
        for line in [
            app_data("\\"),
            app_data("/").to_lowercase(),
            format!("`{}`", app_data("\\")),
            format!("under {}", app_data("/")),
        ] {
            assert_eq!(names(&line), [APP_DATA_FOLDER], "{line}");
        }
    }

    #[test]
    fn variables_and_bare_words_about_app_data_are_not_hits() {
        for line in [
            "%LOCALAPPDATA%\\lci",
            "%LOCALAPPDATA%/lci",
            "%APPDATA%\\lci",
            "$env:LOCALAPPDATA\\lci",
            "$env:LocalAppData/lci",
            "the AppData folder",
            "AppData",
            "AppData is hidden",
        ] {
            assert!(!is_hit(line), "{line}");
        }
    }

    #[test]
    fn a_client_session_folder_under_a_temp_directory_is_a_hit() {
        for line in [
            client_temp_path("Temp", "\\"),
            client_temp_path("Temp", "/"),
            client_temp_path("temp", "/"),
            client_temp_path("TEMP", "\\\\"),
            format!("scratch at {}", client_temp_path("Temp", "/")),
        ] {
            let found = names(&line);
            assert_eq!(found, [CLIENT_TEMP], "{line}");
        }
        // Reached through the profile folder, both patterns apply.
        let line = format!(
            "{}\\{}\\{}",
            profile('C', "\\", "Users", NAME),
            app_data("\\"),
            client_temp_path("Temp", "\\")
        );
        assert_eq!(
            names(&line),
            [WINDOWS_PROFILE, APP_DATA_FOLDER, CLIENT_TEMP]
        );
    }

    #[test]
    fn ordinary_temp_paths_are_not_hits() {
        for line in [
            "%TEMP%\\claude",
            "$env:TEMP\\scratch",
            "the system temp directory",
            "Temp/other",
            "template/claude",
            "claude/temp",
            "<system temp dir>/local-code-intelligence-scratch",
        ] {
            assert!(!is_hit(line), "{line}");
        }
    }

    #[test]
    fn ordinary_lines_are_not_hits() {
        for line in [
            "",
            "fn main() {}",
            "docs/development/roadmap.md",
            "src/lib.rs:12:5",
            "/usr/local/bin/tool",
            "/etc/passwd",
            "../etc/passwd",
            "~/.config/lci/config.local.toml",
            "$HOME/.local/share",
            "C:/projects/app",
            "C:\\Windows\\System32",
            "D:/lci-work",
            "<work_area>/plans",
            "<local data dir>/local-code-intelligence-dev/work",
            "the work area (`cargo xtask env work_area`)",
            "http://127.0.0.1:8768/mcp",
            "https://example.com/docs/index.html",
            "ssh://git@example.com:22/repo.git",
            "127.0.0.1:8767",
            "a:b:c",
        ] {
            assert!(!is_hit(line), "{line}");
        }
    }

    #[test]
    fn several_patterns_on_one_line_are_all_named_once() {
        let line = format!("{} and {}", mac(NAME), linux(NAME));
        assert_eq!(names(&line), [MAC_PROFILE, LINUX_HOME]);
        let line = format!("{} {}", mac(NAME), mac("another"));
        assert_eq!(names(&line), [MAC_PROFILE]);
    }

    #[test]
    fn scan_text_numbers_lines_from_one_and_handles_crlf() {
        let text = format!("clean\r\n{}\r\nclean again\r\n{}\n", linux(NAME), mac(NAME));
        let hits = scan_text(&text, &Extra::default());
        assert_eq!(
            hits,
            [
                Hit {
                    line: 2,
                    patterns: vec![LINUX_HOME.to_owned()]
                },
                Hit {
                    line: 4,
                    patterns: vec![MAC_PROFILE.to_owned()]
                },
            ]
        );
    }

    #[test]
    fn extra_patterns_skip_comments_and_blanks_and_keep_line_numbers() {
        let extra =
            Extra::parse("# a comment\n\n  zq-token-one  \r\n#zq-token-skipped\nZQ-Token-Two\n");
        assert_eq!(extra.pattern_count(), 2);
        assert_eq!(extra.patterns[0], (3, "zq-token-one".to_owned()));
        assert_eq!(extra.patterns[1], (5, "zq-token-two".to_owned()));
    }

    #[test]
    fn extra_patterns_match_case_insensitively_and_never_echo_the_token() {
        let extra = Extra::parse("zq-token-one\nzq-token-two\n");
        assert!(matched_patterns("nothing here", &extra).is_empty());
        let found = matched_patterns("see the ZQ-Token-Two folder", &extra);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("line 2"), "{}", found[0]);
        assert!(found[0].contains(EXTRA_PATTERNS_FILE), "{}", found[0]);
        assert!(
            !found[0].to_lowercase().contains("zq-token"),
            "{}",
            found[0]
        );
        let both = matched_patterns("zq-token-one then zq-token-two", &extra);
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn extra_patterns_combine_with_the_built_in_ones() {
        let extra = Extra::parse("zq-token-one\n");
        let line = format!("{} zq-token-one", linux(NAME));
        let found = matched_patterns(&line, &extra);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], LINUX_HOME);
    }

    #[test]
    fn the_extra_patterns_file_is_read_from_the_local_directory_and_may_be_absent() {
        let tmp = TempDir::new();
        assert_eq!(load_extra(tmp.path()).unwrap().pattern_count(), 0);
        tmp.write(
            EXTRA_PATTERNS_FILE,
            b"# local tokens\nzq-token-one\n\nzq-token-two\n",
        );
        let extra = load_extra(tmp.path()).unwrap();
        assert_eq!(extra.pattern_count(), 2);
        let hits = scan_text("a\nzq-token-two\n", &extra);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
    }

    #[test]
    fn binary_content_is_skipped_and_invalid_utf8_text_is_still_scanned() {
        let line = linux(NAME);
        let mut binary = b"\x00\x01\x02 ".to_vec();
        binary.extend_from_slice(line.as_bytes());
        assert!(scan_bytes(&binary, &Extra::default()).is_none());

        // A NUL beyond the probe window does not make a file binary.
        let mut late = vec![b'a'; BINARY_PROBE];
        late.push(0);
        assert!(scan_bytes(&late, &Extra::default()).is_some());

        let mut latin1 = b"caf\xe9 ".to_vec();
        latin1.extend_from_slice(line.as_bytes());
        let hits = scan_bytes(&latin1, &Extra::default()).expect("text");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].patterns, [LINUX_HOME]);
    }

    #[test]
    fn the_lint_does_not_flag_its_own_source() {
        let source = include_str!("lint.rs");
        assert_eq!(scan_text(source, &Extra::default()), []);
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git is required for the repository tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_repository_scan_reads_tracked_text_files_only() {
        let tmp = TempDir::new();
        let hit = profile('C', "\\", "Users", NAME);
        tmp.write("docs/clean.md", b"nothing to see\n");
        tmp.write(
            "docs/leak.md",
            format!("one\ntwo\npath: {hit}\n").as_bytes(),
        );
        tmp.write("src/main.rs", format!("// {}\n", linux(NAME)).as_bytes());
        // Vendored material is exempt.
        tmp.write(
            ".agents/skills/rust-skills/rule.md",
            format!("{hit}\n").as_bytes(),
        );
        // A binary file is skipped.
        let mut binary = vec![0u8, 1, 2];
        binary.extend_from_slice(hit.as_bytes());
        tmp.write("assets/blob.bin", &binary);
        // Tracked, then deleted from the working tree.
        tmp.write("gone.md", format!("{hit}\n").as_bytes());
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["add", "-A"]);
        std::fs::remove_file(tmp.path().join("gone.md")).unwrap();
        // Present on disk but never added: not scanned.
        tmp.write("untracked.md", format!("{hit}\n").as_bytes());
        // Ignored and local, like `.local/`: not scanned.
        tmp.write(EXTRA_PATTERNS_FILE, b"zq-token-one\n");

        let report = scan_repo(tmp.path(), &Extra::default()).unwrap();
        assert_eq!(report.binary, 1);
        assert_eq!(report.scanned, 3, "clean, leak and main");
        let found: Vec<(&str, usize)> = report
            .hits
            .iter()
            .map(|(path, hit)| (path.as_str(), hit.line))
            .collect();
        assert_eq!(found, [("docs/leak.md", 3), ("src/main.rs", 1)]);
    }

    #[test]
    fn a_repository_scan_applies_the_local_patterns() {
        let tmp = TempDir::new();
        tmp.write("notes.md", b"first\nthe zq-token-one folder\n");
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["add", "-A"]);
        assert!(
            scan_repo(tmp.path(), &Extra::default())
                .unwrap()
                .hits
                .is_empty()
        );
        tmp.write(EXTRA_PATTERNS_FILE, b"zq-token-one\n");
        let extra = load_extra(tmp.path()).unwrap();
        let report = scan_repo(tmp.path(), &extra).unwrap();
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].0, "notes.md");
        assert_eq!(report.hits[0].1.line, 2);
    }
}
