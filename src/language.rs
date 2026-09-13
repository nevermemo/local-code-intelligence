use std::path::Path;
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    Rust,
    EcmaScript,
    Python,
}

#[derive(Clone, Copy)]
pub struct LanguageAdapter {
    extension: &'static str,
    identifier: &'static str,
    cache_version: &'static str,
    grammar: fn() -> Language,
    syntax: Syntax,
}

impl LanguageAdapter {
    pub const fn identifier(&self) -> &'static str {
        self.identifier
    }

    pub const fn cache_version(&self) -> &'static str {
        self.cache_version
    }

    pub fn grammar(&self) -> Language {
        (self.grammar)()
    }

    pub fn glob(&self) -> &'static str {
        match self.extension {
            "rs" => "*.rs",
            "ts" => "*.ts",
            "tsx" => "*.tsx",
            "js" => "*.js",
            "jsx" => "*.jsx",
            "py" => "*.py",
            _ => unreachable!(),
        }
    }

    pub(crate) fn is_container(&self, kind: &str) -> bool {
        match self.syntax {
            Syntax::Rust => matches!(
                kind,
                "source_file" | "declaration_list" | "impl_item" | "mod_item" | "trait_item"
            ),
            Syntax::EcmaScript => matches!(
                kind,
                "program" | "class_body" | "interface_body" | "object_type" | "statement_block"
            ),
            Syntax::Python => matches!(kind, "module" | "block"),
        }
    }

    pub(crate) fn is_boundary(&self, kind: &str) -> bool {
        self.syntax == Syntax::EcmaScript
            && matches!(
                kind,
                "export_statement"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
            )
    }

    pub(crate) fn is_declaration(&self, kind: &str) -> bool {
        match self.syntax {
            Syntax::Rust => matches!(
                kind,
                "function_item" | "struct_item" | "enum_item" | "macro_definition"
            ),
            Syntax::EcmaScript => matches!(
                kind,
                "function_declaration"
                    | "generator_function_declaration"
                    | "function_signature"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "method_definition"
                    | "method_signature"
                    | "abstract_method_signature"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "ambient_declaration"
                    | "lexical_declaration"
                    | "variable_declaration"
                    | "variable_declarator"
                    | "arrow_function"
                    | "export_statement"
            ),
            Syntax::Python => matches!(
                kind,
                "function_definition" | "class_definition" | "decorated_definition"
            ),
        }
    }

    pub(crate) fn is_prefix(&self, kind: &str) -> bool {
        match self.syntax {
            Syntax::Rust => matches!(
                kind,
                "line_comment" | "block_comment" | "attribute_item" | "inner_attribute_item"
            ),
            Syntax::EcmaScript => matches!(kind, "comment" | "decorator"),
            Syntax::Python => matches!(kind, "comment" | "decorator"),
        }
    }

    pub(crate) fn attaches_header_to_child(&self, parent: &str, child: &str) -> bool {
        match self.syntax {
            Syntax::EcmaScript => {
                matches!(
                    parent,
                    "export_statement"
                        | "lexical_declaration"
                        | "variable_declaration"
                        | "variable_declarator"
                        | "arrow_function"
                ) || (self.is_declaration(parent) && self.is_container(child))
            }
            Syntax::Python => {
                (parent == "decorated_definition" && self.is_declaration(child))
                    || (self.is_declaration(parent) && self.is_container(child))
            }
            Syntax::Rust => false,
        }
    }
}

fn rust() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn javascript() -> Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn typescript() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn tsx() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

fn python() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

pub const ADAPTERS: &[LanguageAdapter] = &[
    LanguageAdapter {
        extension: "rs",
        identifier: "rust",
        cache_version: "rust-chunks-v1",
        grammar: rust,
        syntax: Syntax::Rust,
    },
    LanguageAdapter {
        extension: "ts",
        identifier: "typescript",
        cache_version: "typescript-chunks-v1",
        grammar: typescript,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "tsx",
        identifier: "tsx",
        cache_version: "tsx-chunks-v1",
        grammar: tsx,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "js",
        identifier: "javascript",
        cache_version: "javascript-chunks-v1",
        grammar: javascript,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "jsx",
        identifier: "jsx",
        cache_version: "jsx-chunks-v1",
        grammar: javascript,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "py",
        identifier: "python",
        cache_version: "python-chunks-v1",
        grammar: python,
        syntax: Syntax::Python,
    },
];

pub fn for_extension(extension: &str) -> Option<&'static LanguageAdapter> {
    ADAPTERS
        .iter()
        .find(|adapter| adapter.extension == extension)
}

pub fn for_path(path: &Path) -> Option<&'static LanguageAdapter> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(for_extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_requested_lowercase_extensions() {
        for (extension, identifier, glob) in [
            ("rs", "rust", "*.rs"),
            ("ts", "typescript", "*.ts"),
            ("tsx", "tsx", "*.tsx"),
            ("js", "javascript", "*.js"),
            ("jsx", "jsx", "*.jsx"),
            ("py", "python", "*.py"),
        ] {
            let adapter = for_extension(extension).unwrap();
            assert_eq!(adapter.identifier(), identifier);
            assert_eq!(adapter.glob(), glob);
            assert!(!adapter.cache_version().is_empty());
        }
        for extension in ["RS", "TS", "mts", "cts", "mjs", "cjs", "PY"] {
            assert!(for_extension(extension).is_none(), "{extension}");
        }
    }
}
