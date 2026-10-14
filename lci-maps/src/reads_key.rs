//! The `reads_key` translation kind of slice M1.2 of the multimap spike v2
//! (pre-registration in `docs/spike/multimap/v2/m1.2-prereg.md`): a config key
//! (a TOML path) joined to the declarations that read or set it, with two
//! witnesses per edge, and the key anchors of the "+ translation" arms.
//!
//! The key table is built from the example TOML and the `serde` structs of
//! the tree by lines (no parser). Witness W1 is the SCIP closure of the field
//! symbol, W2 is `rg -w` of the field name, rule L reads TOML text inside Rust
//! string literals. Nothing here reads a gold.

use crate::impact::{Chunk, Decl, ImpactIndex, SourceCache, code_names, rg_hits};
use crate::scip_graph::SourceText;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    process::Command,
};

/// The file that holds the `serde` config structs.
pub const CONFIG_RS: &str = "lci-core/src/config.rs";
/// The example TOML of the key table.
pub const EXAMPLE_TOML: &str = "config.example.toml";

/// One key of the example TOML and the field it deserialises into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    pub section: Option<String>,
    pub key: String,
    /// 1-based line of the assignment in the example TOML.
    pub toml_line: u32,
    /// The SCIP symbol of the field, when it was found.
    pub field_symbol: Option<String>,
    /// The struct that owns the field (`Config`, `ChannelsConfig`, ...).
    pub owner: Option<String>,
    /// A `serde(rename)` or `alias` was involved.
    pub renamed: bool,
}

impl KeyEntry {
    /// `section.key`, or `key` for a top-level key.
    pub fn id(&self) -> String {
        match &self.section {
            Some(s) => format!("{s}.{}", self.key),
            None => self.key.clone(),
        }
    }
}

/// The role of an occurrence, by the text around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Write,
    Init,
    TomlText,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Init => "init",
            Role::TomlText => "toml-text",
        }
    }
}

/// The class of an edge by its witnesses; the order is the strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Confirmed,
    Scip,
    Text,
    TextOnlyL,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Confirmed => "confirmed",
            Class::Scip => "scip",
            Class::Text => "text",
            Class::TextOnlyL => "text-only-L",
        }
    }
}

/// `reads_key(key -> declaration)`.
#[derive(Debug, Clone)]
pub struct Edge {
    pub key: String,
    pub decl: Decl,
    pub w1: bool,
    pub w2: bool,
    pub l: bool,
    pub roles: BTreeSet<Role>,
    pub in_def_file: bool,
    pub outside_scip: bool,
}

impl Edge {
    pub fn class(&self) -> Class {
        match (self.w1, self.w2, self.l) {
            (true, true, _) => Class::Confirmed,
            (true, false, _) => Class::Scip,
            (false, true, _) => Class::Text,
            _ => Class::TextOnlyL,
        }
    }
}

// ---------------------------------------------------------------- key table

/// Keys of the example TOML: (section, key, 1-based line). Only uncommented
/// `key = value` lines count; the section is the nearest `[header]` above.
pub fn parse_example_toml(text: &str) -> Vec<(Option<String>, String, u32)> {
    let mut out = Vec::new();
    let mut section: Option<String> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(h) = header_of(line) {
            section = Some(h);
            continue;
        }
        if let Some(key) = assignment_key(line) {
            out.push((section.clone(), key, i as u32 + 1));
        }
    }
    out
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `[name]` or `[[name]]`; the content must read as a (dotted) name.
fn header_of(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?;
    let inner = inner.strip_prefix('[').unwrap_or(inner);
    let end = inner.find(']')?;
    let name = inner[..end].trim();
    (!name.is_empty() && name.chars().all(|c| is_key_char(c) || c == '.')).then(|| name.to_string())
}

/// The key of a `key = value` line (not `==`).
fn assignment_key(line: &str) -> Option<String> {
    let end = line.find(|c: char| !is_key_char(c))?;
    if end == 0 {
        return None;
    }
    let rest = line[end..].trim_start();
    (rest.starts_with('=') && !rest.starts_with("==")).then(|| line[..end].to_string())
}

/// A field of a struct, read by lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDecl {
    pub name: String,
    pub ty: String,
    /// `rename = ".."` of a `serde` attribute above the field.
    pub rename: Option<String>,
    pub alias: bool,
}

/// The fields of `struct name { .. }` in a source file's lines. The block ends
/// at the first line that starts with `}`.
pub fn struct_fields(lines: &[String], name: &str) -> Vec<FieldDecl> {
    let head = |l: &str| {
        let t = l.trim();
        let t = t.strip_prefix("pub ").unwrap_or(t);
        let t = t.strip_prefix("pub(crate) ").unwrap_or(t);
        t.strip_prefix("struct ")
            .map(|r| r.split(|c: char| !(c.is_alphanumeric() || c == '_')).next() == Some(name))
            .unwrap_or(false)
            && t.trim_end().ends_with('{')
    };
    let Some(start) = lines.iter().position(|l| head(l)) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    let mut rename: Option<String> = None;
    let mut alias = false;
    for l in &lines[start + 1..] {
        if l.starts_with('}') {
            break;
        }
        let t = l.trim();
        if t.starts_with("#[serde(") {
            if let Some(v) = quoted_after(t, "rename") {
                rename = Some(v);
            }
            if t.contains("alias") {
                alias = true;
            }
            continue;
        }
        if t.starts_with("///") || t.starts_with("//") || t.starts_with("#[") || t.is_empty() {
            continue;
        }
        let t = t.strip_prefix("pub ").unwrap_or(t);
        let t = t.strip_prefix("pub(crate) ").unwrap_or(t);
        if let Some((n, rest)) = t.split_once(':') {
            let n = n.trim();
            if !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let ty = rest.trim().trim_end_matches(',').trim();
                let ty = ty.split('<').next().unwrap_or(ty);
                let ty = ty.rsplit("::").next().unwrap_or(ty).trim();
                fields.push(FieldDecl {
                    name: n.to_string(),
                    ty: ty.to_string(),
                    rename: rename.take(),
                    alias,
                });
                alias = false;
                continue;
            }
        }
        rename = None;
        alias = false;
    }
    fields
}

/// The string literal after `word =` in an attribute line.
fn quoted_after(text: &str, word: &str) -> Option<String> {
    let i = text.find(&format!("{word} = \""))? + word.len() + 4;
    let end = text[i..].find('"')?;
    Some(text[i..i + end].to_string())
}

fn find_field<'a>(fields: &'a [FieldDecl], toml_name: &str) -> Option<&'a FieldDecl> {
    fields.iter().find(|f| match &f.rename {
        Some(r) => r == toml_name,
        None => f.name == toml_name.replace('-', "_"),
    })
}

fn text_lines(src: &dyn SourceText, path: &str) -> Vec<String> {
    src.read(path)
        .map(|b| {
            String::from_utf8_lossy(&b)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The key table of a tree: every key of the example TOML, with its field.
pub fn build_key_table(ix: &ImpactIndex, src: &dyn SourceText) -> Vec<KeyEntry> {
    let toml = src
        .read(EXAMPLE_TOML)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let rs = text_lines(src, CONFIG_RS);
    let root_fields = struct_fields(&rs, "Config");
    let mut table = Vec::new();
    for (section, key, toml_line) in parse_example_toml(&toml) {
        let mut entry = KeyEntry {
            section: section.clone(),
            key: key.clone(),
            toml_line,
            field_symbol: None,
            owner: None,
            renamed: false,
        };
        let located = match &section {
            None => find_field(&root_fields, &key).map(|f| ("Config".to_string(), f.clone())),
            Some(sec) => find_field(&root_fields, sec).and_then(|sf| {
                let inner = struct_fields(&rs, &sf.ty);
                find_field(&inner, &key).map(|f| {
                    entry.renamed |= sf.rename.is_some() || sf.alias;
                    (sf.ty.clone(), f.clone())
                })
            }),
        };
        if let Some((owner, f)) = located {
            entry.renamed |= f.rename.is_some() || f.alias;
            entry.field_symbol = field_symbol(ix, &owner, &f.name);
            entry.owner = Some(owner);
        }
        table.push(entry);
    }
    table
}

/// The SCIP definition of `Owner::field` in the config file.
fn field_symbol(ix: &ImpactIndex, owner: &str, field: &str) -> Option<String> {
    ix.defs_in_file(CONFIG_RS)
        .iter()
        .find(|&&d| {
            ix.chains[d].as_ref().is_some_and(|c| {
                c.owner.as_deref() == Some(owner)
                    && c.names.last().map(String::as_str) == Some(field)
            })
        })
        .map(|&d| ix.defs[d].symbol.clone())
}

// ------------------------------------------------------------ text helpers

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The role of the `len` bytes at `col` in a line, by the text around them.
pub fn role_in_line(line: &[u8], col: usize, len: usize) -> Role {
    let after = String::from_utf8_lossy(line.get(col + len..).unwrap_or(&[]))
        .trim_start()
        .to_string();
    let before = String::from_utf8_lossy(line.get(..col).unwrap_or(&[]))
        .trim_end()
        .to_string();
    let compound = ["+=", "-=", "*=", "/=", "%=", "|=", "&=", "^="];
    if (after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>"))
        || compound.iter().any(|c| after.starts_with(c))
    {
        return Role::Write;
    }
    if after.starts_with(':')
        && !after.starts_with("::")
        && (before.ends_with('{') || before.ends_with(','))
    {
        return Role::Init;
    }
    Role::Read
}

/// A logical line of a string literal: its characters and where each sits.
#[derive(Debug, Default, Clone)]
pub struct LitLine {
    pub chars: Vec<char>,
    pub pos: Vec<(u32, u32)>,
}

/// Logical lines of every string literal of a file (plain, byte, raw; a real
/// line break, a `\n` escape each start a new logical line; a `\` at the end of
/// a line joins the next).
pub fn literal_lines(text: &[u8]) -> Vec<Vec<LitLine>> {
    let mut starts = vec![0usize];
    for (i, &b) in text.iter().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let pos = |i: usize| -> (u32, u32) {
        let line = match starts.binary_search(&i) {
            Ok(l) => l,
            Err(l) => l - 1,
        };
        (line as u32, (i - starts[line]) as u32)
    };
    let mut out: Vec<Vec<LitLine>> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        let two = text.get(i..i + 2).unwrap_or(&[]);
        if two == b"//" {
            while i < text.len() && text[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if two == b"/*" {
            let mut depth = 1;
            i += 2;
            while i < text.len() && depth > 0 {
                let t = text.get(i..i + 2).unwrap_or(&[]);
                if t == b"/*" {
                    depth += 1;
                    i += 2;
                } else if t == b"*/" {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let prev_ident = i > 0 && is_ident(text[i - 1]);
        if b == b'r' && !prev_ident {
            let hashes = text[i + 1..].iter().take_while(|&&c| c == b'#').count();
            if text.get(i + 1 + hashes) == Some(&b'"') {
                let body = i + 2 + hashes;
                let mut closing = vec![b'"'];
                closing.extend(std::iter::repeat_n(b'#', hashes));
                let end = text[body..]
                    .windows(closing.len())
                    .position(|w| w == closing.as_slice())
                    .map_or(text.len(), |p| body + p);
                let mut lines = vec![LitLine::default()];
                for (j, &c) in text.iter().enumerate().take(end).skip(body) {
                    if c == b'\n' {
                        lines.push(LitLine::default());
                    } else if c != b'\r' {
                        let l = lines.last_mut().expect("line");
                        l.chars
                            .push(if c.is_ascii() { c as char } else { '\u{fffd}' });
                        l.pos.push(pos(j));
                    }
                }
                out.push(lines);
                i = (end + closing.len()).min(text.len());
                continue;
            }
        }
        if b == b'"' {
            let mut lines = vec![LitLine::default()];
            let mut j = i + 1;
            while j < text.len() && text[j] != b'"' {
                let c = text[j];
                if c == b'\\' {
                    let n = text.get(j + 1).copied().unwrap_or(b'"');
                    match n {
                        b'n' => {
                            lines.push(LitLine::default());
                            j += 2;
                        }
                        b'\n' | b'\r' => {
                            j += 1;
                            while j < text.len() && matches!(text[j], b' ' | b'\t' | b'\n' | b'\r')
                            {
                                j += 1;
                            }
                        }
                        b'u' => {
                            while j < text.len() && text[j] != b'}' {
                                j += 1;
                            }
                            j += 1;
                        }
                        b'x' => j += 4,
                        other => {
                            let ch = match other {
                                b't' => '\t',
                                b'r' => continue_skip(&mut j),
                                b'0' => '\0',
                                o => o as char,
                            };
                            if ch != '\u{0}' || other == b'0' {
                                let l = lines.last_mut().expect("line");
                                l.chars.push(ch);
                                l.pos.push(pos(j));
                            }
                            j += 2;
                        }
                    }
                    continue;
                }
                if c == b'\n' {
                    lines.push(LitLine::default());
                } else if c != b'\r' {
                    let l = lines.last_mut().expect("line");
                    l.chars
                        .push(if c.is_ascii() { c as char } else { '\u{fffd}' });
                    l.pos.push(pos(j));
                }
                j += 1;
            }
            out.push(lines);
            i = j + 1;
            continue;
        }
        if b == b'\'' {
            // Character literal or lifetime.
            if text.get(i + 1) == Some(&b'\\') {
                let mut j = i + 2;
                while j < text.len() && text[j] != b'\'' {
                    j += 1;
                }
                i = j + 1;
            } else if text.get(i + 2) == Some(&b'\'') {
                i += 3;
            } else if text.get(i + 1).is_some_and(|c| !c.is_ascii()) {
                // A multi-byte character literal.
                let mut j = i + 1;
                while j < text.len() && text[j] != b'\'' && j < i + 6 {
                    j += 1;
                }
                i = if text.get(j) == Some(&b'\'') {
                    j + 1
                } else {
                    i + 1
                };
            } else {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// `\r` escapes carry no content (a placeholder keeps the match arm total).
fn continue_skip(_j: &mut usize) -> char {
    '\u{0}'
}

/// One TOML-text setter found by rule L: (key index, file line, file col).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextHit {
    pub entry: usize,
    pub line: u32,
    pub col: u32,
}

/// Rule L: a key at the start of a TOML line inside a string literal, in the
/// key's table (the nearest `[header]` of the same literal), or without any
/// header in the literal when the bare key is unique in the table.
pub fn rule_l(table: &[KeyEntry], text: &[u8]) -> Vec<TextHit> {
    let mut bare: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in table.iter().enumerate() {
        if e.field_symbol.is_some() {
            bare.entry(e.key.as_str()).or_default().push(i);
        }
    }
    let mut hits = Vec::new();
    for lit in literal_lines(text) {
        let mut header: Option<String> = None;
        for l in &lit {
            let s: String = l.chars.iter().collect();
            let trimmed = s.trim_start();
            let lead = s.len() - trimmed.len();
            if let Some(h) = header_of(trimmed.trim_end()) {
                header = Some(h);
                continue;
            }
            let Some(key) = assignment_key(trimmed) else {
                continue;
            };
            let Some(cands) = bare.get(key.as_str()) else {
                continue;
            };
            for &ci in cands {
                let e = &table[ci];
                let ok = match (&e.section, &header) {
                    (a, b) if a == b => true,
                    (Some(_), None) => cands.len() == 1,
                    _ => false,
                };
                if ok {
                    let at = l.pos.get(lead).copied().unwrap_or((0, 0));
                    hits.push(TextHit {
                        entry: ci,
                        line: at.0,
                        col: at.1,
                    });
                }
            }
        }
    }
    hits
}

// -------------------------------------------------- units outside the index

/// The innermost `fn` (by brace matching over code bytes) of a file that holds
/// the line; for files the SCIP index does not hold.
pub fn outside_unit(cache: &mut SourceCache<'_>, path: &str, line: usize) -> Option<Decl> {
    let lines: Vec<String> = cache
        .lines(path)?
        .iter()
        .map(|l| {
            String::from_utf8_lossy(l)
                .trim_end_matches('\r')
                .to_string()
        })
        .collect();
    for start in (0..=line.min(lines.len().saturating_sub(1))).rev() {
        let Some(name) = fn_name(&lines[start]) else {
            continue;
        };
        // Brace-match from the fn line over code bytes.
        let (mut depth, mut seen) = (0i32, false);
        let mut end = None;
        'scan: for (l, text) in lines.iter().enumerate().skip(start) {
            for (c, &b) in text.as_bytes().iter().enumerate() {
                if !cache.is_code(path, l, c) {
                    continue;
                }
                if b == b'{' {
                    depth += 1;
                    seen = true;
                } else if b == b'}' {
                    depth -= 1;
                } else if b == b';' && !seen {
                    break 'scan;
                }
                if seen && depth == 0 {
                    end = Some(l);
                    break 'scan;
                }
            }
        }
        if end.is_some_and(|e| e >= line) {
            return Some(Decl {
                path: path.to_string(),
                line: start as u32 + 1,
                name: name.clone(),
                symbol: format!("outside-scip:{path}#{name}"),
            });
        }
    }
    None
}

/// The name of a function declared on a line (`pub async fn name`), if any.
pub fn fn_name(line: &str) -> Option<String> {
    let mut t = line.trim_start();
    for m in [
        "pub(crate) ",
        "pub ",
        "const ",
        "async ",
        "unsafe ",
        "extern \"C\" ",
    ] {
        while let Some(r) = t.strip_prefix(m) {
            t = r;
        }
    }
    let r = t.strip_prefix("fn ")?;
    let name: String = r
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Whether a line (zero-based) lies inside a `#[cfg(test)]` module.
pub fn in_cfg_test_module(cache: &mut SourceCache<'_>, path: &str, line: usize) -> bool {
    let lines: Vec<String> = match cache.lines(path) {
        Some(l) => l
            .iter()
            .map(|b| {
                String::from_utf8_lossy(b)
                    .trim_end_matches('\r')
                    .to_string()
            })
            .collect(),
        None => return false,
    };
    for (i, l) in lines.iter().enumerate().take(line + 1) {
        if !l.trim().starts_with("#[cfg(test)]") {
            continue;
        }
        let Some(m) = (i + 1..lines.len().min(i + 4)).find(|&k| {
            let t = lines[k].trim_start();
            t.starts_with("mod ") || t.starts_with("pub mod ")
        }) else {
            continue;
        };
        let (mut depth, mut seen) = (0i32, false);
        for (k, text) in lines.iter().enumerate().skip(m) {
            for (c, &b) in text.as_bytes().iter().enumerate() {
                if !cache.is_code(path, k, c) {
                    continue;
                }
                if b == b'{' {
                    depth += 1;
                    seen = true;
                } else if b == b'}' {
                    depth -= 1;
                }
            }
            if seen && depth <= 0 {
                if line <= k {
                    return true;
                }
                break;
            }
        }
    }
    false
}

// ------------------------------------------------------------- extraction

/// All `.rs` files of the tree, sorted, `/` separators.
pub fn rs_files(root: &Path) -> Result<Vec<String>, String> {
    let out = Command::new("rg")
        .current_dir(root)
        .args([
            "--files",
            "--no-config",
            "--sort",
            "path",
            "--path-separator",
            "/",
            "-g",
            "*.rs",
            ".",
        ])
        .output()
        .map_err(|e| format!("rg: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim_start_matches("./").to_string())
        .collect())
}

#[derive(Default)]
struct Acc {
    w1: bool,
    w2: bool,
    l: bool,
    roles: BTreeSet<Role>,
    decl: Option<Decl>,
    outside: bool,
}

/// Every `reads_key` edge of the tree, for every mapped key of the table.
pub fn extract(
    ix: &ImpactIndex,
    root: &Path,
    src: &dyn SourceText,
    table: &[KeyEntry],
) -> Result<Vec<Edge>, String> {
    let mut cache = SourceCache::new(src);
    let docs = ix.doc_paths();
    let mut acc: BTreeMap<(String, String, u32, String), Acc> = BTreeMap::new();
    let touch = |acc: &mut BTreeMap<(String, String, u32, String), Acc>,
                 key: &str,
                 decl: Decl,
                 outside: bool,
                 w: (bool, bool, bool),
                 role: Role| {
        let k = (
            key.to_string(),
            decl.path.clone(),
            decl.line,
            decl.symbol.clone(),
        );
        let a = acc.entry(k).or_default();
        a.w1 |= w.0;
        a.w2 |= w.1;
        a.l |= w.2;
        a.roles.insert(role);
        a.outside = outside;
        a.decl = Some(decl);
    };
    let mut rg_cache: HashMap<String, Vec<(String, usize, usize)>> = HashMap::new();

    for e in table {
        let Some(sym) = &e.field_symbol else { continue };
        let id = e.id();
        let owner = e.owner.clone().unwrap_or_default();
        let field_def = ix.def_index(sym).map(|d| ix.defs[d].name_range);
        let def_path = ix.def_index(sym).map(|d| ix.defs[d].path.clone());
        // Width of the field name as written at its definition (a renamed key
        // differs from the key text).
        let field_len = field_def
            .map(|r| r.3.saturating_sub(r.1) as usize)
            .filter(|n| *n > 0)
            .unwrap_or(e.key.len());

        // W1: SCIP occurrences of the field symbol.
        for (path, line, col) in ix.occurrences(sym) {
            let Some(u) = ix.unit_at(&path, line, col) else {
                continue;
            };
            let role = match cache.lines(&path).and_then(|ls| ls.get(line as usize)) {
                Some(l) => role_in_line(l, col as usize, field_len),
                None => Role::Read,
            };
            touch(&mut acc, &id, ix.decl(u), false, (true, false, false), role);
        }

        // W2: `rg -w` of the field name.
        let name = e.key.replace('-', "_");
        if !rg_cache.contains_key(&name) {
            rg_cache.insert(name.clone(), rg_hits(root, &name)?);
        }
        for (path, line, col) in rg_cache[&name].clone() {
            if !cache.is_code(&path, line, col) {
                continue;
            }
            if field_def.is_some_and(|r| r.0 as usize == line && r.1 as usize == col)
                && def_path.as_deref() == Some(path.as_str())
            {
                continue;
            }
            let dotted = col > 0
                && cache
                    .lines(&path)
                    .and_then(|ls| ls.get(line))
                    .is_some_and(|l| l.get(col - 1) == Some(&b'.'));
            if !dotted && !cache.has_word(&path, &owner) {
                continue;
            }
            let (decl, outside) = if docs.contains(path.as_str()) {
                match ix.unit_at(&path, line as u32, col as u32) {
                    Some(u) => (ix.decl(u), false),
                    None => continue,
                }
            } else {
                match outside_unit(&mut cache, &path, line) {
                    Some(d) => (d, true),
                    None => continue,
                }
            };
            let role = match cache.lines(&path).and_then(|ls| ls.get(line)) {
                Some(l) => role_in_line(l, col, name.len()),
                None => Role::Read,
            };
            touch(&mut acc, &id, decl, outside, (false, true, false), role);
        }
    }

    // Rule L over every file, and the outside-SCIP initialiser rule.
    for path in rs_files(root)? {
        let Some(bytes) = src.read(&path) else {
            continue;
        };
        let in_index = docs.contains(path.as_str());
        for h in rule_l(table, &bytes) {
            let e = &table[h.entry];
            let (decl, outside) = if in_index {
                match ix.unit_at(&path, h.line, h.col) {
                    Some(u) => (ix.decl(u), false),
                    None => continue,
                }
            } else {
                match outside_unit(&mut cache, &path, h.line as usize) {
                    Some(d) => (d, true),
                    None => continue,
                }
            };
            touch(
                &mut acc,
                &e.id(),
                decl,
                outside,
                (false, false, true),
                Role::TomlText,
            );
        }
        if in_index {
            continue;
        }
        let lines: Vec<String> = String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect();
        for e in table.iter().filter(|e| e.field_symbol.is_some()) {
            let owner = e.owner.clone().unwrap_or_default();
            let name = e.key.replace('-', "_");
            for (li, text) in lines.iter().enumerate() {
                let Some(col) = word_at_init(text, &name) else {
                    continue;
                };
                if !cache.is_code(&path, li, col) {
                    continue;
                }
                let lo = li.saturating_sub(14);
                let opens = lines[lo..li].iter().any(|p| {
                    p.find(&format!("{owner} {{"))
                        .is_some_and(|k| k == 0 || !is_ident(p.as_bytes()[k - 1]))
                });
                if !opens {
                    continue;
                }
                if let Some(d) = outside_unit(&mut cache, &path, li) {
                    touch(&mut acc, &e.id(), d, true, (false, false, true), Role::Init);
                }
            }
        }
    }

    let mut edges = Vec::new();
    for ((key, path, _, _), a) in acc {
        let Some(decl) = a.decl else { continue };
        let in_def_file = path == CONFIG_RS;
        edges.push(Edge {
            key,
            decl,
            w1: a.w1,
            w2: a.w2,
            l: a.l,
            roles: a.roles,
            in_def_file,
            outside_scip: a.outside,
        });
    }
    Ok(edges)
}

/// Column of `name` written as a struct-literal initialiser (`name:` but not
/// `name::`), whole word.
fn word_at_init(line: &str, name: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut from = 0;
    while let Some(p) = line[from..].find(name) {
        let i = from + p;
        let end = i + name.len();
        let word = (i == 0 || !is_ident(b[i - 1])) && b.get(end).is_none_or(|&c| !is_ident(c));
        if word {
            let rest = line[end..].trim_start();
            if rest.starts_with(':') && !rest.starts_with("::") {
                return Some(i);
            }
        }
        from = i + 1;
    }
    None
}

// ------------------------------------------------------------- key anchors

/// Section names and `[section]` / `section.key` hints written in the code
/// spans of a query, and the key names of spans that `code_names` rejects.
pub fn query_hints(query: &str, table: &[KeyEntry]) -> (BTreeSet<String>, Vec<String>) {
    let sections: BTreeSet<&str> = table.iter().filter_map(|e| e.section.as_deref()).collect();
    let mut hints = BTreeSet::new();
    let mut extra = Vec::new();
    let parts: Vec<&str> = query.split('`').collect();
    for k in 0..parts.len() / 2 {
        let span = parts[2 * k + 1].trim();
        if let Some(h) = header_of(span)
            && sections.contains(h.as_str())
        {
            hints.insert(h);
            continue;
        }
        if sections.contains(span) {
            hints.insert(span.to_string());
            continue;
        }
        if let Some((a, b)) = span.split_once('.')
            && sections.contains(a)
            && b.chars().all(is_key_char)
            && !b.is_empty()
        {
            hints.insert(a.to_string());
            extra.push(b.to_string());
        }
    }
    (hints, extra)
}

/// What a query's names resolve to in the key table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResolution {
    /// Index of the entry.
    Key(usize),
    /// A name matched several entries and no section hint chose one.
    Abstain,
    /// No name is a key.
    None,
}

/// Resolve the names of a query to a table entry (first name in query order
/// that resolves; `ref_count` breaks ties between several resolved names).
pub fn resolve_key(
    query: &str,
    table: &[KeyEntry],
    ref_count: &dyn Fn(&str) -> usize,
) -> KeyResolution {
    let (names, _) = code_names(query);
    let (hints, extra) = query_hints(query, table);
    let mut all: Vec<Vec<String>> = names;
    for x in extra {
        if !all.contains(&vec![x.clone()]) {
            all.push(vec![x]);
        }
    }
    let mut resolved: Vec<usize> = Vec::new();
    let mut abstained = false;
    for n in &all {
        let Some(last) = n.last() else { continue };
        let cands: Vec<usize> = table
            .iter()
            .enumerate()
            .filter(|(_, e)| e.key == *last && e.field_symbol.is_some())
            .map(|(i, _)| i)
            .collect();
        let pick = match cands.len() {
            0 => continue,
            1 => Some(cands[0]),
            _ => {
                let by_name: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let e = &table[i];
                        let first = if n.len() >= 2 {
                            Some(n[n.len() - 2].as_str())
                        } else {
                            None
                        };
                        let sec = e.section.as_deref();
                        sec.is_some_and(|s| hints.contains(s))
                            || first
                                .is_some_and(|f| Some(f) == sec || Some(f) == e.owner.as_deref())
                    })
                    .collect();
                (by_name.len() == 1).then(|| by_name[0])
            }
        };
        match pick {
            Some(i) if !resolved.contains(&i) => resolved.push(i),
            Some(_) => {}
            None => abstained = true,
        }
    }
    resolved.sort_by_key(|&i| {
        (
            std::cmp::Reverse(ref_count(table[i].field_symbol.as_deref().unwrap_or(""))),
            table[i].id(),
        )
    });
    match resolved.first() {
        Some(&i) => KeyResolution::Key(i),
        None if abstained => KeyResolution::Abstain,
        None => KeyResolution::None,
    }
}

/// The table entry whose field is this symbol.
pub fn key_of_symbol<'a>(table: &'a [KeyEntry], symbol: &str) -> Option<&'a KeyEntry> {
    table
        .iter()
        .find(|e| e.field_symbol.as_deref() == Some(symbol))
}

/// How a key-translated anchor was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// The base rule's anchor (a SCIP symbol).
    Base,
    /// A key resolved through the table (N) or the example TOML (C).
    Table,
}

/// A key-translated anchor.
#[derive(Debug, Clone)]
pub struct KeyAnchor {
    pub symbol: String,
    pub via: Via,
    pub chunk_rank: Option<u32>,
}

/// Arm N+T: a name that resolves to a key supplies the field as the anchor;
/// otherwise the base N anchor.
pub fn anchor_n_t(
    ix: &ImpactIndex,
    table: &[KeyEntry],
    query: &str,
) -> (Option<KeyAnchor>, KeyResolution) {
    let rc = |s: &str| ix.def_index(s).map_or(0, |d| ix.ref_count(d));
    let res = resolve_key(query, table, &rc);
    if let KeyResolution::Key(i) = res {
        let sym = table[i].field_symbol.clone().expect("mapped");
        return (
            Some(KeyAnchor {
                symbol: sym,
                via: Via::Table,
                chunk_rank: None,
            }),
            res,
        );
    }
    let (names, _) = code_names(query);
    let base = ix.anchor_n(&names).map(|a| KeyAnchor {
        symbol: ix.defs[a.def].symbol.clone(),
        via: Via::Base,
        chunk_rank: None,
    });
    (base, res)
}

/// Arm C+T: the C rule, plus table candidates from chunks of the example TOML
/// whose span holds an assignment of a key that a name of the query spells.
pub fn anchor_c_t(
    ix: &ImpactIndex,
    table: &[KeyEntry],
    query: &str,
    evidence: &[Chunk],
    k: u32,
) -> Option<KeyAnchor> {
    let (names, _) = code_names(query);
    let (_, extra) = query_hints(query, table);
    let mut keys: Vec<String> = names.iter().filter_map(|n| n.last().cloned()).collect();
    keys.extend(extra);
    let mut chunks: Vec<&Chunk> = evidence
        .iter()
        .filter(|c| c.rank >= 1 && c.rank <= k)
        .collect();
    chunks.sort_by_key(|c| c.rank);
    for chunk in chunks {
        let base = ix.anchor_c(&names, std::slice::from_ref(chunk), u32::MAX);
        let mut best: Option<(std::cmp::Reverse<usize>, u32, String, Via)> = base.map(|a| {
            let d = &ix.defs[a.def];
            (
                std::cmp::Reverse(a.segments),
                d.name_range.0,
                d.symbol.clone(),
                Via::Base,
            )
        });
        if chunk.path == EXAMPLE_TOML {
            for e in table {
                let Some(sym) = &e.field_symbol else { continue };
                if e.toml_line < chunk.start_line
                    || e.toml_line > chunk.end_line
                    || !keys.contains(&e.key)
                {
                    continue;
                }
                let cand = (
                    std::cmp::Reverse(1usize),
                    e.toml_line - 1,
                    sym.clone(),
                    Via::Table,
                );
                if best
                    .as_ref()
                    .is_none_or(|b| (&cand.0, cand.1, &cand.2) < (&b.0, b.1, &b.2))
                {
                    best = Some(cand);
                }
            }
        }
        if let Some((_, _, symbol, via)) = best {
            return Some(KeyAnchor {
                symbol,
                via,
                chunk_rank: Some(chunk.rank),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(section: Option<&str>, key: &str, sym: &str) -> KeyEntry {
        KeyEntry {
            section: section.map(str::to_string),
            key: key.to_string(),
            toml_line: 1,
            field_symbol: Some(sym.to_string()),
            owner: Some("Config".to_string()),
            renamed: false,
        }
    }

    #[test]
    fn example_toml_keys_have_sections_and_skip_comments() {
        let t = "a = 1\n# b = 2\n[x]\nc = \"v\"\n[[y]]\nd=3\nk == 3\n";
        let k = parse_example_toml(t);
        assert_eq!(
            k,
            vec![
                (None, "a".to_string(), 1),
                (Some("x".to_string()), "c".to_string(), 4),
                (Some("y".to_string()), "d".to_string(), 6),
            ]
        );
    }

    #[test]
    fn struct_fields_read_renames_and_types() {
        let src = "pub struct Config {\n    /// doc\n    pub a_b: usize,\n    #[serde(rename = \"zed\")]\n    pub z: Option<String>,\n    pub inner: InnerConfig,\n}\nstruct Other {\n    x: u8,\n}\n";
        let lines: Vec<String> = src.lines().map(str::to_string).collect();
        let f = struct_fields(&lines, "Config");
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].name, "a_b");
        assert_eq!(f[1].rename.as_deref(), Some("zed"));
        assert_eq!(f[1].ty, "Option");
        assert_eq!(f[2].ty, "InnerConfig");
        assert_eq!(struct_fields(&lines, "Other").len(), 1);
        assert!(find_field(&f, "zed").is_some());
        assert!(find_field(&f, "a-b").is_some());
    }

    #[test]
    fn roles_by_text() {
        let l = b"cfg.rerank = 4;";
        assert_eq!(role_in_line(l, 4, 6), Role::Write);
        assert_eq!(role_in_line(b"if cfg.rerank == 4 {", 7, 6), Role::Read);
        assert_eq!(role_in_line(b"Config { rerank: 4 }", 9, 6), Role::Init);
        assert_eq!(role_in_line(b"let v = cfg.rerank;", 12, 6), Role::Read);
        assert_eq!(role_in_line(b"cfg.n += 1;", 4, 1), Role::Write);
        assert_eq!(role_in_line(b"Config::rerank::x", 8, 6), Role::Read);
    }

    #[test]
    fn rule_l_reads_toml_in_plain_raw_and_continued_strings() {
        let table = vec![
            entry(None, "top", "S#top."),
            entry(Some("index"), "wait", "S#wait."),
            entry(Some("index"), "dup", "S#d1."),
            entry(Some("fusion"), "dup", "S#d2."),
        ];
        let src = br##"
fn a() {
    let s = "top = 4\n[index]\nwait = 3\ndup = 1\n";
    let r = r#"
[fusion]
dup = 2
"#;
    let c = "top = 1 \
             # not a key";
    let u = "wait = 9";
    let n = "dup = 7";
    // top = 5 in a comment
}
"##;
        let hits = rule_l(&table, src);
        let got: Vec<(usize, u32)> = hits.iter().map(|h| (h.entry, h.line)).collect();
        // `top` (literal 1), `wait` in [index], `dup` in [index] (entry 2), then
        // `dup` in [fusion] (entry 3), `top` in the continued string, `wait`
        // without a header (unique), and no `dup` without a header (ambiguous).
        assert_eq!(got, vec![(0, 2), (1, 2), (2, 2), (3, 5), (0, 7), (1, 9)]);
    }

    #[test]
    fn rule_l_rejects_a_key_under_another_table() {
        let table = vec![entry(None, "top", "S#top.")];
        let src = b"let s = \"[index]\\ntop = 4\";";
        assert!(rule_l(&table, src).is_empty());
    }

    #[test]
    fn fn_names_and_outside_units() {
        assert_eq!(
            fn_name("    pub async fn go(x: u8) {").as_deref(),
            Some("go")
        );
        assert_eq!(fn_name("let fn_x = 1;"), None);
        struct Mem(String);
        impl SourceText for Mem {
            fn read(&self, _p: &str) -> Option<Vec<u8>> {
                Some(self.0.clone().into_bytes())
            }
        }
        let m = Mem(
            "fn a() {\n    let x = 1;\n}\nfn b() {\n    let s = \"}\";\n    let y = 2;\n}\n".into(),
        );
        let mut cache = SourceCache::new(&m);
        let d = outside_unit(&mut cache, "f.rs", 5).expect("unit");
        assert_eq!((d.name.as_str(), d.line), ("b", 4));
        let d = outside_unit(&mut cache, "f.rs", 1).expect("unit");
        assert_eq!(d.name, "a");
        assert!(outside_unit(&mut cache, "f.rs", 2).is_some());
    }

    #[test]
    fn cfg_test_modules_are_found() {
        struct Mem(String);
        impl SourceText for Mem {
            fn read(&self, _p: &str) -> Option<Vec<u8>> {
                Some(self.0.clone().into_bytes())
            }
        }
        let m =
            Mem("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {\n    }\n}\nfn b() {}\n".into());
        let mut cache = SourceCache::new(&m);
        assert!(!in_cfg_test_module(&mut cache, "f.rs", 0));
        assert!(in_cfg_test_module(&mut cache, "f.rs", 3));
        assert!(!in_cfg_test_module(&mut cache, "f.rs", 6));
    }

    #[test]
    fn resolution_abstains_without_a_section_and_uses_hints() {
        let table = vec![
            entry(Some("a"), "dup", "S#d1."),
            entry(Some("b"), "dup", "S#d2."),
            entry(None, "solo", "S#solo."),
        ];
        let rc = |_: &str| 0;
        assert_eq!(
            resolve_key("which reads `dup`?", &table, &rc),
            KeyResolution::Abstain
        );
        assert_eq!(
            resolve_key("which reads `dup` of the `[b]` table?", &table, &rc),
            KeyResolution::Key(1)
        );
        assert_eq!(
            resolve_key("the `a.dup` setting", &table, &rc),
            KeyResolution::Key(0)
        );
        assert_eq!(
            resolve_key("the `a::dup` setting", &table, &rc),
            KeyResolution::Key(0)
        );
        assert_eq!(
            resolve_key("who reads `solo`", &table, &rc),
            KeyResolution::Key(2)
        );
        assert_eq!(
            resolve_key("who calls `other`", &table, &rc),
            KeyResolution::None
        );
    }

    #[test]
    fn initialiser_words_need_a_single_colon() {
        assert_eq!(word_at_init("    rerank: 4,", "rerank"), Some(4));
        assert_eq!(word_at_init("    rerank::x", "rerank"), None);
        assert_eq!(word_at_init("    xrerank: 4", "rerank"), None);
    }
}

/// Files that are out-of-line test modules: `#[cfg(test)]` followed (within 3
/// lines) by `mod name;` in a file, resolved to `name.rs` or `name/mod.rs`
/// beside it (a file other than `mod.rs`, `lib.rs`, `main.rs` looks in the
/// directory named after its stem).
pub fn test_module_files(src: &dyn SourceText, files: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in files {
        let lines = text_lines(src, f);
        let (dir, stem) = match f.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.trim_end_matches(".rs").to_string()),
            None => (String::new(), f.trim_end_matches(".rs").to_string()),
        };
        let base = if matches!(stem.as_str(), "mod" | "lib" | "main") {
            dir.clone()
        } else if dir.is_empty() {
            stem.clone()
        } else {
            format!("{dir}/{stem}")
        };
        for (i, l) in lines.iter().enumerate() {
            if !l.trim().starts_with("#[cfg(test)]") {
                continue;
            }
            for next in lines.iter().skip(i + 1).take(3) {
                let t = next.trim();
                let t = t.strip_prefix("pub ").unwrap_or(t);
                if let Some(name) = t.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) {
                    let name = name.trim();
                    let join = |n: &str| {
                        if base.is_empty() {
                            n.to_string()
                        } else {
                            format!("{base}/{n}")
                        }
                    };
                    out.insert(join(&format!("{name}.rs")));
                    out.insert(join(&format!("{name}/mod.rs")));
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod test_module_tests {
    use super::*;

    struct Mem(HashMap<String, String>);
    impl SourceText for Mem {
        fn read(&self, p: &str) -> Option<Vec<u8>> {
            self.0.get(p).map(|s| s.clone().into_bytes())
        }
    }

    #[test]
    fn out_of_line_test_modules_resolve_beside_the_declaring_file() {
        let mut m = HashMap::new();
        m.insert(
            "src/evaluate.rs".to_string(),
            "mod a;\n#[cfg(test)]\nmod tests;\n".to_string(),
        );
        m.insert(
            "src/lib.rs".to_string(),
            "#[cfg(test)]\npub mod t;\n".to_string(),
        );
        let files = vec!["src/evaluate.rs".to_string(), "src/lib.rs".to_string()];
        let got = test_module_files(&Mem(m), &files);
        assert!(got.contains("src/evaluate/tests.rs"));
        assert!(got.contains("src/t.rs"));
        assert!(!got.contains("src/evaluate/a.rs"));
    }
}
