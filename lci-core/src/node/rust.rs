//! Rust declarations for [`SymbolKey`]s: a minimal, declaration-only
//! extractor for slice S1 (no references, no edges). The path-free part of a
//! key is extracted per file; the module path is joined from the file path
//! (lesson L2: per-file data never holds a path).

use super::{NodeId, SymbolKey, SymbolKind};
use crate::language;
use anyhow::{Context, Result};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

/// One declaration of a file, without the file's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Inline `mod name { .. }` blocks around the declaration, outermost
    /// first.
    pub inline_modules: Vec<String>,
    /// Enclosing impl headers and traits, outermost first.
    pub container: Vec<String>,
    pub name: String,
    pub kind: SymbolKind,
    /// `0` unless an earlier declaration of the file has the same
    /// modules, container, name and kind (for example two `#[cfg]` variants).
    pub disambiguator: u16,
    /// One-based inclusive lines: attributes of the key, never part of it.
    pub start_line: u32,
    pub end_line: u32,
}

impl Declaration {
    /// The key of this declaration in the file at `path` (Rust only).
    pub fn key(&self, path: &str) -> SymbolKey {
        let mut module_path = rust_module_path(path);
        module_path.extend(self.inline_modules.iter().cloned());
        SymbolKey {
            language: "rust".into(),
            module_path,
            container: self.container.clone(),
            name: self.name.clone(),
            kind: self.kind,
            disambiguator: self.disambiguator,
        }
    }

    pub fn node_id(&self, path: &str) -> NodeId {
        NodeId::Symbol(self.key(path))
    }
}

/// The module path of a Rust file, from its repository-relative path: the
/// components after the last `src` directory with the extension dropped,
/// without a final `mod` (a `mod.rs` is its directory's module) and without
/// a `lib` directly under `src` (the library crate root); prefixed with the
/// crate's directory (the components before `src`, joined by `/`) when the
/// crate is not at the repository root, so two crates' `chunk` modules
/// differ. A binary crate root `src/main.rs` keeps `main`, so it does not
/// share the library root's (empty) path. A file outside any `src` directory
/// keeps all its components.
///
/// `src/app/search.rs` -> `app::search`; `lci-core/src/chunk.rs` ->
/// `lci-core::chunk`; `src/lib.rs` -> (empty); `src/main.rs` -> `main`;
/// `tests/cases/mcp.rs` -> `tests::cases::mcp`.
pub fn rust_module_path(path: &str) -> Vec<String> {
    let mut components: Vec<&str> = path.split('/').collect();
    if let Some(last) = components.last_mut() {
        *last = last.strip_suffix(".rs").unwrap_or(last);
    }
    let mut module_path = Vec::new();
    let modules = match components.iter().rposition(|c| *c == "src") {
        Some(src) => {
            if src > 0 {
                module_path.push(components[..src].join("/"));
            }
            &components[src + 1..]
        }
        None => &components[..],
    };
    module_path.extend(modules.iter().map(|c| (*c).to_owned()));
    let crate_root_lib = modules == ["lib"];
    if (crate_root_lib || modules.last() == Some(&"mod")) && !modules.is_empty() {
        module_path.pop();
    }
    module_path
}

/// The tokens of `node` that end before `end`, as written, skipping
/// comments.
fn tokens<'s>(node: Node<'_>, end: usize, source: &'s str, out: &mut Vec<&'s str>) {
    if node.start_byte() >= end || node.kind().contains("comment") {
        return;
    }
    if node.child_count() == 0 {
        out.push(&source[node.byte_range()]);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        tokens(child, end, source, out);
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// An impl header from its tokens, so that neither whitespace nor comments
/// change it: one space between tokens except before `,` `;` `:` `::` `>`
/// `)` `]` `?`, after `<` `::` `(` `[` `&` `*` `!` `'` and `#`, and between a
/// word and the `<` or `(` that follows it (`Into<u8>`, `Fn(u8)`). The result
/// always holds a space after `impl`, which the canonical string relies on.
fn impl_header(tokens: &[&str]) -> String {
    let mut header = String::new();
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            let previous = tokens[index - 1];
            let glued = matches!(*token, "," | ";" | ":" | "::" | ">" | ")" | "]" | "?")
                || matches!(
                    previous,
                    "<" | "::" | "(" | "[" | "&" | "*" | "!" | "'" | "#"
                )
                || (matches!(*token, "<" | "(") && previous.ends_with(is_word));
            if !glued {
                header.push(' ');
            }
        }
        header.push_str(token);
    }
    if !header.contains(' ') {
        header = header.replacen("impl", "impl ", 1);
    }
    header
}

fn name_of(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .map(|name| source[name.byte_range()].to_owned())
}

struct Walk<'s> {
    source: &'s str,
    modules: Vec<String>,
    container: Vec<String>,
    out: Vec<Declaration>,
}

impl Walk<'_> {
    fn push(&mut self, node: Node<'_>, name: String, kind: SymbolKind) {
        self.out.push(Declaration {
            inline_modules: self.modules.clone(),
            container: self.container.clone(),
            name,
            kind,
            disambiguator: 0,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
        });
    }

    fn children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children {
            self.item(child);
        }
    }

    /// Records `node` if it is a declaration and walks into module, impl
    /// and trait bodies. Function bodies are not walked: items nested in a
    /// function are not addressable from outside it.
    fn item(&mut self, node: Node<'_>) {
        let in_impl_or_trait = !self.container.is_empty();
        let simple = match node.kind() {
            "function_item" | "function_signature_item" => Some(if in_impl_or_trait {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }),
            "struct_item" => Some(SymbolKind::Struct),
            "enum_item" => Some(SymbolKind::Enum),
            "union_item" => Some(SymbolKind::Union),
            "const_item" => Some(SymbolKind::Const),
            "static_item" => Some(SymbolKind::Static),
            "type_item" | "associated_type" => Some(SymbolKind::TypeAlias),
            "macro_definition" => Some(SymbolKind::Macro),
            _ => None,
        };
        if let Some(kind) = simple {
            if let Some(name) = name_of(node, self.source) {
                self.push(node, name, kind);
            }
            return;
        }
        match node.kind() {
            "mod_item" => {
                let Some(name) = name_of(node, self.source) else {
                    return;
                };
                self.push(node, name.clone(), SymbolKind::Module);
                if let Some(body) = node.child_by_field_name("body") {
                    self.modules.push(name);
                    self.children(body);
                    self.modules.pop();
                }
            }
            "trait_item" => {
                let Some(name) = name_of(node, self.source) else {
                    return;
                };
                self.push(node, name.clone(), SymbolKind::Trait);
                if let Some(body) = node.child_by_field_name("body") {
                    self.container.push(format!("trait {name}"));
                    self.children(body);
                    self.container.pop();
                }
            }
            "impl_item" => {
                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                // The header up to the body, from its tokens: `impl<T> Map
                // for Lexical<T>`, `unsafe impl Send for X`, with `where`
                // clauses (they tell two blanket impls apart).
                let mut words = Vec::new();
                tokens(node, body.start_byte(), self.source, &mut words);
                let header = impl_header(&words);
                self.container.push(header);
                self.children(body);
                self.container.pop();
            }
            _ => {}
        }
    }
}

/// The declarations of a Rust source file, in source order, with
/// disambiguators assigned in that order.
pub fn rust_declarations(source: &str) -> Result<Vec<Declaration>> {
    let adapter = language::for_extension("rs").expect("Rust adapter must be registered");
    let mut parser = Parser::new();
    parser.set_language(&adapter.grammar())?;
    let tree = parser
        .parse(source, None)
        .context("Tree-sitter parse cancelled")?;
    let mut walk = Walk {
        source,
        modules: Vec::new(),
        container: Vec::new(),
        out: Vec::new(),
    };
    walk.children(tree.root_node());
    let mut seen = HashMap::<(Vec<String>, Vec<String>, String, &str), u16>::new();
    for declaration in &mut walk.out {
        let count = seen
            .entry((
                declaration.inline_modules.clone(),
                declaration.container.clone(),
                declaration.name.clone(),
                declaration.kind.as_str(),
            ))
            .or_insert(0);
        declaration.disambiguator = *count;
        *count = count.saturating_add(1);
    }
    Ok(walk.out)
}
