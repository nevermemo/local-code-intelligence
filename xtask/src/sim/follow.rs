//! Following identifiers: which names in the text the agent just read are
//! worth a grep, and in which order the frontier hands them out.
//!
//! Names come from tree-sitter identifier leaves (any leaf whose node kind
//! ends in `identifier`) inside the lines that were read, through the
//! language adapters of `lci-core`; a file without an adapter falls back to
//! plain identifier-shaped words.

use clap::ValueEnum;
use lci_core::language::{self, LanguageAdapter};
use lci_core::lexical::query_terms;
use lci_maps::live_grep::GrepOptions;
use std::collections::VecDeque;
use std::path::Path;

/// How the frontier orders candidate names.
///
/// Both policies sort the names found in one read by rarity (fewest matching
/// files first, then first-seen order). They differ in where a new batch goes:
/// `Greedy` in front of everything pending (depth-first: the rarest name of
/// the newest read is next), `Bfs` behind it (first come, first followed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Policy {
    Greedy,
    Bfs,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Policy::Greedy => "greedy",
            Policy::Bfs => "bfs",
        }
    }
}

/// A name the agent may grep next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    /// Files the name occurs in.
    pub rarity: usize,
    /// Reads between the query and the text this name came from.
    pub depth: u32,
}

#[derive(Debug)]
pub struct Frontier {
    policy: Policy,
    queue: VecDeque<Candidate>,
}

impl Frontier {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            queue: VecDeque::new(),
        }
    }

    /// Adds one read's candidates, already sorted rarest first.
    pub fn push_batch(&mut self, batch: Vec<Candidate>) {
        match self.policy {
            Policy::Greedy => {
                for candidate in batch.into_iter().rev() {
                    self.queue.push_front(candidate);
                }
            }
            Policy::Bfs => self.queue.extend(batch),
        }
    }

    pub fn pop(&mut self) -> Option<Candidate> {
        self.queue.pop_front()
    }
}

/// Whether a name found in code is worth a grep: long enough, not a name that
/// matches everywhere (`GrepOptions::skip_names`) and not a stop word.
pub fn usable_identifier(name: &str, options: &GrepOptions) -> bool {
    let lower = name.to_lowercase();
    name.len() >= options.min_name_len
        && !options.skip_names.contains(&lower)
        && query_terms(name).contains(&lower)
}

/// Identifier names in the 1-based inclusive line range, in source order
/// (repeats kept).
pub fn identifiers_in(path: &str, source: &str, start: u32, end: u32) -> Vec<String> {
    language::for_path(Path::new(path))
        .and_then(|adapter| tree_identifiers(adapter, source, start, end))
        .unwrap_or_else(|| word_identifiers(source, start, end))
}

fn tree_identifiers(
    adapter: &LanguageAdapter,
    source: &str,
    start: u32,
    end: u32,
) -> Option<Vec<String>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&adapter.grammar()).ok()?;
    let tree = parser.parse(source, None)?;
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let first = node.start_position().row as u32 + 1;
        let last = node.end_position().row as u32 + 1;
        if last >= start && first <= end {
            if node.child_count() == 0 {
                if node.kind().ends_with("identifier")
                    && first >= start
                    && first <= end
                    && let Ok(text) = node.utf8_text(source.as_bytes())
                {
                    out.push(text.to_owned());
                }
            } else if cursor.goto_first_child() {
                continue;
            }
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(out);
            }
        }
    }
}

fn word_identifiers(source: &str, start: u32, end: u32) -> Vec<String> {
    let mut out = Vec::new();
    for (index, line) in source.split('\n').enumerate() {
        let number = index as u32 + 1;
        if number < start {
            continue;
        }
        if number > end {
            break;
        }
        out.extend(
            line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .filter(|w| {
                    w.chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic() || c == '_')
                })
                .map(str::to_owned),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, rarity: usize) -> Candidate {
        Candidate {
            name: name.into(),
            rarity,
            depth: 0,
        }
    }

    #[test]
    fn greedy_puts_a_new_batch_in_front_and_bfs_behind() {
        for (policy, expected) in [
            (Policy::Greedy, ["c", "d", "a", "b"]),
            (Policy::Bfs, ["a", "b", "c", "d"]),
        ] {
            let mut frontier = Frontier::new(policy);
            frontier.push_batch(vec![cand("a", 1), cand("b", 2)]);
            frontier.push_batch(vec![cand("c", 1), cand("d", 3)]);
            let order: Vec<String> = std::iter::from_fn(|| frontier.pop())
                .map(|c| c.name)
                .collect();
            assert_eq!(order, expected, "{policy:?}");
        }
    }

    #[test]
    fn tree_sitter_finds_identifier_leaves_inside_the_window_only() {
        let source = "fn first_fn() {}\nfn second_fn() { inner_call(); }\nfn third_fn() {}\n";
        let names = identifiers_in("x.rs", source, 2, 2);
        assert_eq!(names, ["second_fn", "inner_call"]);
        let all = identifiers_in("x.rs", source, 1, 3);
        assert_eq!(all, ["first_fn", "second_fn", "inner_call", "third_fn"]);
    }

    #[test]
    fn unknown_extensions_fall_back_to_words() {
        let names = identifiers_in("notes.txt", "alpha beta_2 9lives\n_x y", 1, 2);
        assert_eq!(names, ["alpha", "beta_2", "_x", "y"]);
    }

    #[test]
    fn short_common_and_stop_names_are_not_followed() {
        let options = GrepOptions::default();
        assert!(usable_identifier("alpha_branch", &options));
        assert!(!usable_identifier("abc", &options));
        assert!(!usable_identifier("config", &options));
        assert!(!usable_identifier("with", &options));
    }
}
