//! The R arm of `sim-structured`: the same simulated agent, with a ripgrep
//! process instead of `live_grep::scan` behind every search. It answers one
//! question: does the agent's behaviour depend on the in-process matcher? The
//! result is a context column in `compare`, not part of any verdict.
//!
//! One call is `rg <ARGS> <needle> .` run in the tree root. Whole-word
//! matching (`-w`) and fixed strings (`-F`; the needles are identifiers and
//! query words, never patterns) differ from the live scan, which also finds a
//! needle inside a longer identifier, and `rg` reads hidden files the scan
//! does not; those differences are the point of measuring it apart.

use anyhow::{Context, Result, bail};
use lci_maps::live_grep::LiveMatch;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The arguments before the needle. `--no-config` keeps a user's ripgreprc out
/// of the measurement, `--sort path` makes the output order (and `rg` single
/// threaded) deterministic, and the size cap is the scan's `max_file_bytes`.
pub const ARGS_BEFORE_NEEDLE: &[&str] = &[
    "--no-config",
    "-n",
    "--no-heading",
    "--color",
    "never",
    "-w",
    "-F",
    "--hidden",
    "-g",
    "!.git",
    "--sort",
    "path",
    "--max-filesize",
    "1M",
    "--",
];

/// The executable to run. Freeze a copy of it for a measurement and record
/// its hash (`sim-structured run` does).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgCommand {
    exe: PathBuf,
}

/// What one `rg` call printed, parsed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RgOutput {
    /// One per printed line (`rg -n` prints a line once however often the
    /// needle occurs in it), column 0, `exact` true. 0-based lines.
    pub matches: Vec<LiveMatch>,
    pub stdout_bytes: u64,
    pub stdout_lines: u64,
    /// Output lines that are not `path:line:text` (a binary-file notice, say).
    pub ignored_lines: u64,
}

impl RgCommand {
    pub fn new(exe: PathBuf) -> Self {
        Self { exe }
    }

    /// The arguments of one call, needle last but one, the search path last.
    pub fn argv(needle: &str) -> Vec<String> {
        let mut args: Vec<String> = ARGS_BEFORE_NEEDLE.iter().map(|a| (*a).to_owned()).collect();
        args.push(needle.to_owned());
        args.push(".".to_owned());
        args
    }

    /// Runs `rg` for `needle` in `root`. Exit 1 is "no match"; any other
    /// non-zero status, or a process that cannot be started, is an error.
    pub fn search(&self, root: &Path, needle: &str) -> Result<RgOutput> {
        let output = Command::new(&self.exe)
            .args(Self::argv(needle))
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .with_context(|| "run rg (is --rg an executable?)")?;
        interpret(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            output.stdout.len() as u64,
        )
    }
}

/// The result of a finished `rg` process.
fn interpret(code: Option<i32>, stdout: &str, stderr: &str, stdout_bytes: u64) -> Result<RgOutput> {
    match code {
        Some(0) => Ok(parse_stdout(stdout, stdout_bytes)),
        Some(1) => Ok(RgOutput {
            stdout_bytes,
            ..RgOutput::default()
        }),
        other => {
            let status = other.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}"));
            let detail: String = stderr.trim().chars().take(300).collect();
            bail!("rg ended with {status}: {detail}")
        }
    }
}

/// `path:line:text` lines to matches. Paths lose a leading `./` and have `\`
/// turned into `/` (Windows `rg` prints `.\src\a.rs`); a trailing `\r` of a
/// CRLF file is dropped with the line end.
pub fn parse_stdout(stdout: &str, stdout_bytes: u64) -> RgOutput {
    let mut out = RgOutput {
        stdout_bytes,
        ..RgOutput::default()
    };
    for line in stdout.lines() {
        out.stdout_lines += 1;
        match parse_line(line) {
            Some((path, number)) => out.matches.push(LiveMatch {
                path,
                line: number - 1,
                col: 0,
                exact: true,
                blob: String::new(),
            }),
            None => out.ignored_lines += 1,
        }
    }
    out
}

/// The path and the 1-based line number of one output line: the first `:`
/// followed by digits and another `:` ends the path.
fn parse_line(line: &str) -> Option<(String, u32)> {
    let mut from = 0;
    while let Some(offset) = line[from..].find(':') {
        let colon = from + offset;
        let rest = &line[colon + 1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && rest.as_bytes().get(digits) == Some(&b':') {
            let number: u32 = rest[..digits].parse().ok()?;
            let path = line[..colon].replace('\\', "/");
            let path = path.strip_prefix("./").unwrap_or(&path);
            return (number >= 1 && !path.is_empty()).then(|| (path.to_owned(), number));
        }
        from = colon + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_become_zero_based_matches_with_slash_paths() {
        let out =
            "./src/a.rs:12:    fn needle() {}\n.\\src\\deep\\b.rs:3:needle needle\nc.rs:1:x\n";
        let parsed = parse_stdout(out, out.len() as u64);
        let seen: Vec<(&str, u32, u32, bool)> = parsed
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line, m.col, m.exact))
            .collect();
        assert_eq!(
            seen,
            [
                ("src/a.rs", 11, 0, true),
                ("src/deep/b.rs", 2, 0, true),
                ("c.rs", 0, 0, true)
            ]
        );
        assert_eq!(parsed.stdout_lines, 3);
        assert_eq!(parsed.stdout_bytes, out.len() as u64);
        assert_eq!(parsed.ignored_lines, 0);
    }

    #[test]
    fn crlf_output_and_colons_in_the_text_are_tolerated() {
        let out = "./a.toml:4:name = \"x:9:y\"\r\n./b.toml:7:key: 1:2: end\r\n";
        let parsed = parse_stdout(out, 0);
        assert_eq!(parsed.matches.len(), 2);
        assert_eq!(
            (parsed.matches[0].path.as_str(), parsed.matches[0].line),
            ("a.toml", 3)
        );
        assert_eq!(
            (parsed.matches[1].path.as_str(), parsed.matches[1].line),
            ("b.toml", 6)
        );
        assert_eq!(parsed.stdout_lines, 2);
    }

    #[test]
    fn lines_that_are_not_matches_are_counted_and_dropped() {
        let out = "./bin.dat: binary file matches (found \"\\0\" byte around offset 4)\n./a.rs:2:ok\n:3:no path\n./b.rs:0:line zero\n\n";
        let parsed = parse_stdout(out, 0);
        assert_eq!(parsed.matches.len(), 1);
        assert_eq!(parsed.matches[0].path, "a.rs");
        assert_eq!(parsed.stdout_lines, 5);
        assert_eq!(parsed.ignored_lines, 4);
        assert_eq!(parse_stdout("", 0), RgOutput::default());
    }

    #[test]
    fn exit_codes_follow_the_rg_convention() {
        let ok = interpret(Some(0), "./a.rs:1:x\n", "", 11).unwrap();
        assert_eq!(ok.matches.len(), 1);
        let none = interpret(Some(1), "", "", 0).unwrap();
        assert!(none.matches.is_empty());
        let error = interpret(Some(2), "./a.rs:1:x\n", "rg: bad thing\n", 11)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("status 2") && error.contains("bad thing"),
            "{error}"
        );
        let killed = interpret(None, "", "", 0).unwrap_err().to_string();
        assert!(killed.contains("a signal"), "{killed}");
    }

    #[test]
    fn the_argument_list_is_the_registered_one() {
        let argv = RgCommand::argv("needle");
        assert_eq!(argv.last().map(String::as_str), Some("."));
        assert_eq!(argv[argv.len() - 2], "needle");
        assert_eq!(argv[argv.len() - 3], "--");
        for must in ["--no-config", "-w", "--hidden", "--sort"] {
            assert!(argv.iter().any(|a| a == must), "{must}");
        }
    }

    #[test]
    fn a_missing_executable_is_an_error() {
        let command = RgCommand::new(PathBuf::from("no-such-rg-executable-xtask"));
        assert!(command.search(Path::new("."), "needle").is_err());
    }
}
