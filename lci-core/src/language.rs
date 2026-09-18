use std::path::Path;
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    Rust,
    EcmaScript,
    Python,
    CSharp,
    Go,
    Java,
    C,
    Cpp,
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
            "cs" => "*.cs",
            "go" => "*.go",
            "java" => "*.java",
            "c" => "*.c",
            "cpp" => "*.cpp",
            "hpp" => "*.hpp",
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
            Syntax::CSharp => matches!(
                kind,
                "compilation_unit"
                    | "namespace_declaration"
                    | "file_scoped_namespace_declaration"
                    | "declaration_list"
                    | "class_body"
                    | "enum_body"
                    | "block"
            ),
            // Go has no nested class/impl body: every function, method, type,
            // const, and var declaration lives directly at file scope, so
            // `source_file` is the only container -- matching Rust's
            // minimalism rather than Python's `block`-is-a-container choice,
            // since Go function bodies are idiomatically small and rely on
            // the size-threshold path (MAX_DECLARATION_BYTES) for the rare
            // oversized one instead of always chunking statement-by-statement.
            Syntax::Go => matches!(kind, "source_file"),
            // Java mirrors C#'s class-based container shape: the file, each
            // type body, and a declaration's own block are all containers,
            // so ordinary members chunk individually and only an oversized
            // method body ever needs to descend into per-statement groups.
            Syntax::Java => matches!(
                kind,
                "program"
                    | "class_body"
                    | "interface_body"
                    | "enum_body"
                    | "enum_body_declarations"
                    | "annotation_type_body"
                    | "block"
                    | "constructor_body"
            ),
            // C has no nested container below file scope either: struct/
            // union/enum bodies hold only data fields (never further
            // declarations or code), so decomposing them per-field would add
            // noise without the payoff class/impl decomposition gives OOP
            // languages. Mirrors Go's minimalism exactly, including relying
            // on the size-threshold path for the rare oversized function.
            Syntax::C => matches!(kind, "translation_unit"),
            // Unlike C, C++ classes/structs/unions and namespaces DO nest
            // further declarations (methods, fields, nested types), so this
            // mirrors C#/Java's OOP container shape instead of C's flat one:
            // the file, a namespace's body, a class/struct/union's body, and
            // a function's own block are all containers.
            Syntax::Cpp => matches!(
                kind,
                "translation_unit"
                    | "declaration_list"
                    | "field_declaration_list"
                    | "compound_statement"
            ),
        }
    }

    pub(crate) fn is_boundary(&self, kind: &str) -> bool {
        match self.syntax {
            Syntax::EcmaScript => matches!(
                kind,
                "export_statement"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
            ),
            Syntax::CSharp => matches!(
                kind,
                "class_declaration"
                    | "struct_declaration"
                    | "interface_declaration"
                    | "record_declaration"
                    | "enum_declaration"
                    | "delegate_declaration"
                    | "namespace_declaration"
                    | "file_scoped_namespace_declaration"
            ),
            Syntax::Java => matches!(
                kind,
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
            ),
            Syntax::Cpp => matches!(
                kind,
                "namespace_definition" | "class_specifier" | "struct_specifier" | "union_specifier"
            ),
            Syntax::Rust | Syntax::Python | Syntax::Go | Syntax::C => false,
        }
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
            Syntax::CSharp => matches!(
                kind,
                "namespace_declaration"
                    | "file_scoped_namespace_declaration"
                    | "class_declaration"
                    | "struct_declaration"
                    | "interface_declaration"
                    | "record_declaration"
                    | "enum_declaration"
                    | "delegate_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
                    | "destructor_declaration"
                    | "operator_declaration"
                    | "property_declaration"
                    | "indexer_declaration"
                    | "event_field_declaration"
                    | "event_declaration"
                    | "field_declaration"
                    | "base_field_declaration"
                    | "local_function_statement"
            ),
            Syntax::Go => matches!(
                kind,
                "function_declaration"
                    | "method_declaration"
                    | "type_declaration"
                    | "const_declaration"
                    | "var_declaration"
            ),
            Syntax::Java => matches!(
                kind,
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
                    | "compact_constructor_declaration"
                    | "field_declaration"
                    | "constant_declaration"
                    | "static_initializer"
            ),
            Syntax::C => matches!(
                kind,
                "function_definition"
                    | "declaration"
                    | "type_definition"
                    | "struct_specifier"
                    | "union_specifier"
                    | "enum_specifier"
                    | "preproc_def"
                    | "preproc_function_def"
            ),
            Syntax::Cpp => matches!(
                kind,
                "namespace_definition"
                    | "class_specifier"
                    | "struct_specifier"
                    | "union_specifier"
                    | "function_definition"
                    | "declaration"
                    | "type_definition"
                    | "enum_specifier"
                    | "template_declaration"
                    | "preproc_def"
                    | "preproc_function_def"
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
            Syntax::CSharp => matches!(kind, "comment" | "attribute_list"),
            Syntax::Go => matches!(kind, "comment"),
            // Java's annotations (`@Override`) parse as a child of the
            // declaration they modify (inside its `modifiers` node), not as
            // a preceding sibling, so only leading comments need attaching.
            Syntax::Java => matches!(kind, "line_comment" | "block_comment"),
            // C's attributes (`[[nodiscard]]`, `__attribute__((...))`) are
            // likewise children of the declaration they modify, not
            // preceding siblings -- only comments need attaching.
            Syntax::C => matches!(kind, "comment"),
            Syntax::Cpp => matches!(kind, "comment"),
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
            // C has no container below file scope (see is_container), so
            // this never actually triggers -- a leading comment on an
            // oversized function still attaches correctly via the
            // unconditional `range.0 = child_start` rewrite in `ranges()`,
            // the same mechanism Go relies on.
            Syntax::Rust | Syntax::Go | Syntax::C => false,
            Syntax::CSharp | Syntax::Java => {
                self.is_declaration(parent) && self.is_container(child)
            }
            // `template<typename T> class Foo { ... }` / `template<typename
            // T> T add(...)`: the template parameter list is a sibling
            // inside `template_declaration`, not part of the wrapped
            // class/function itself, so it needs the same decorator-style
            // attachment Python uses for `decorated_definition` -- without
            // it, "template<typename T>" would split into its own orphan
            // chunk instead of staying with the declaration it introduces.
            Syntax::Cpp => {
                (parent == "template_declaration" && self.is_declaration(child))
                    || (self.is_declaration(parent) && self.is_container(child))
            }
        }
    }
}

fn rust() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn go() -> Language {
    tree_sitter_go::LANGUAGE.into()
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

fn csharp() -> Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

fn java() -> Language {
    tree_sitter_java::LANGUAGE.into()
}

fn c() -> Language {
    tree_sitter_c::LANGUAGE.into()
}

fn cpp() -> Language {
    tree_sitter_cpp::LANGUAGE.into()
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
    LanguageAdapter {
        extension: "cs",
        identifier: "csharp",
        cache_version: "csharp-chunks-v1",
        grammar: csharp,
        syntax: Syntax::CSharp,
    },
    LanguageAdapter {
        extension: "go",
        identifier: "go",
        cache_version: "go-chunks-v1",
        grammar: go,
        syntax: Syntax::Go,
    },
    LanguageAdapter {
        extension: "java",
        identifier: "java",
        cache_version: "java-chunks-v1",
        grammar: java,
        syntax: Syntax::Java,
    },
    LanguageAdapter {
        extension: "c",
        identifier: "c",
        cache_version: "c-chunks-v1",
        grammar: c,
        syntax: Syntax::C,
    },
    LanguageAdapter {
        extension: "cpp",
        identifier: "cpp",
        cache_version: "cpp-chunks-v1",
        grammar: cpp,
        syntax: Syntax::Cpp,
    },
    LanguageAdapter {
        extension: "hpp",
        identifier: "cpp",
        cache_version: "cpp-chunks-v1",
        grammar: cpp,
        syntax: Syntax::Cpp,
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

/// Iterate over the registered language identifiers.
pub fn identifiers() -> impl Iterator<Item = &'static str> {
    ADAPTERS.iter().map(|adapter| adapter.identifier)
}

/// Report whether `identifier` is a registered language identifier.
///
/// Extensions are not accepted as identifiers.
pub fn is_registered_identifier(identifier: &str) -> bool {
    ADAPTERS
        .iter()
        .any(|adapter| adapter.identifier == identifier)
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
            ("cs", "csharp", "*.cs"),
            ("go", "go", "*.go"),
            ("java", "java", "*.java"),
            ("c", "c", "*.c"),
            ("cpp", "cpp", "*.cpp"),
            ("hpp", "cpp", "*.hpp"),
        ] {
            let adapter = for_extension(extension).unwrap();
            assert_eq!(adapter.identifier(), identifier);
            assert_eq!(adapter.glob(), glob);
            assert!(!adapter.cache_version().is_empty());
        }
        for extension in [
            "RS", "TS", "mts", "cts", "mjs", "cjs", "PY", "CS", "GO", "JAVA", "class", "C", "h",
            "CPP", "HPP", "cc", "cxx", "hh", "hxx",
        ] {
            assert!(for_extension(extension).is_none(), "{extension}");
        }
    }
}
