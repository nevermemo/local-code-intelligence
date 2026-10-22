//! LCI's own grammars as an ast-grep [`Language`].
//!
//! ast-grep-core is generic over its language. This one is a thin handle on a
//! [`LanguageAdapter`] of `lci-core`, so a pattern is parsed by the same
//! tree-sitter grammar crates the chunker links (one `tree-sitter` in the
//! lock). `ast-grep-language` is not used: it carries grammar crates of its
//! own.
//!
//! # The expando character
//!
//! A pattern writes a metavariable as `$NAME`, but `$` does not start an
//! identifier in most grammars, so the pattern would not parse. ast-grep
//! rewrites the sigil to a character the grammar accepts inside identifiers
//! (the "expando") before it parses the pattern, and reads metavariables back
//! with that character. The value is `µ` (U+00B5, an identifier character in
//! every grammar of the table), as `ast-grep-language` uses for these
//! languages. The searched source is never rewritten, so a `$` in a JS
//! identifier (`$el`) stays what it is.

use ast_grep_core::{
    Language, Pattern, PatternError,
    matcher::PatternBuilder,
    tree_sitter::{LanguageExt, StrDoc, TSLanguage},
};
use lci_core::language::{self, ADAPTERS, LanguageAdapter};
use std::{borrow::Cow, path::Path};

/// The identifier character a metavariable's `$` becomes in a pattern.
const EXPANDO: char = 'µ';

/// A grammar of `lci-core`'s language table, for ast-grep.
#[derive(Clone, Copy)]
pub struct Lang(&'static LanguageAdapter);

impl Lang {
    /// The language of a file by its extension; `None` for an extension with
    /// no adapter and for prose (Markdown has no grammar).
    pub fn for_path(path: &Path) -> Option<Self> {
        language::for_path(path)
            .filter(|adapter| !adapter.is_prose())
            .map(Self)
    }

    /// The first code adapter registered under `identifier` (`cpp` has several
    /// extensions, one grammar).
    pub fn for_identifier(identifier: &str) -> Option<Self> {
        ADAPTERS
            .iter()
            .find(|adapter| adapter.identifier() == identifier && !adapter.is_prose())
            .map(Self)
    }

    /// `LanguageAdapter::identifier()`: the key of the pattern table.
    pub fn identifier(&self) -> &'static str {
        self.0.identifier()
    }
}

/// `pre_process_pattern` of ast-grep's expando languages: a `$` that starts a
/// metavariable (before an uppercase letter or `_`, or a bare `$$$`) becomes
/// the expando; every other `$` stays.
fn expand_dollars(query: &str) -> Cow<'_, str> {
    if !query.contains('$') {
        return Cow::Borrowed(query);
    }
    let mut out = String::with_capacity(query.len());
    let mut dollars = 0_usize;
    let flush = |out: &mut String, dollars: usize, replace: bool| {
        let sigil = if replace { EXPANDO } else { '$' };
        out.extend(std::iter::repeat_n(sigil, dollars));
    };
    for c in query.chars() {
        if c == '$' {
            dollars += 1;
            continue;
        }
        let replace = matches!(c, 'A'..='Z' | '_') || dollars == 3;
        flush(&mut out, dollars, replace);
        dollars = 0;
        out.push(c);
    }
    flush(&mut out, dollars, dollars == 3);
    Cow::Owned(out)
}

impl Language for Lang {
    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        expand_dollars(query)
    }

    fn expando_char(&self) -> char {
        EXPANDO
    }

    fn from_path<P: AsRef<Path>>(path: P) -> Option<Self> {
        Self::for_path(path.as_ref())
    }

    fn kind_to_id(&self, kind: &str) -> u16 {
        self.get_ts_language().id_for_node_kind(kind, true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.get_ts_language()
            .field_id_for_name(field)
            .map(|id| id.get())
    }

    fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
        builder.build(|source| StrDoc::try_new(source, *self))
    }
}

impl LanguageExt for Lang {
    fn get_ts_language(&self) -> TSLanguage {
        self.0.grammar()
    }
}
