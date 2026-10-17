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

/// A data format that shares a registered source extension and is therefore
/// not source code in the language the extension names.
///
/// This is never used to choose a language: the extension still does that.
/// It only lets a scan leave out a file whose content cannot be that
/// language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonSource {
    /// A Qt Linguist translation catalog (XML) saved with the `.ts`
    /// extension, which TypeScript also uses.
    QtLinguist,
}

impl NonSource {
    /// Stable name for logs and reports.
    pub const fn identifier(self) -> &'static str {
        match self {
            Self::QtLinguist => "qt-linguist",
        }
    }
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

    /// Reports whether the content of a file with this adapter's extension is
    /// a known non-source data format, so the file must not be parsed or
    /// indexed as this language. Only extensions with a known clash are
    /// checked; every other adapter returns `None` without reading `content`.
    ///
    /// A false positive would drop real source from the index, so each check
    /// requires something the language cannot start with. For `ts` that is a
    /// Qt Linguist XML start in UTF-8 or byte-order-marked UTF-16.
    pub fn non_source(&self, content: &[u8]) -> Option<NonSource> {
        match self.extension {
            "ts" => qt_linguist(content),
            _ => None,
        }
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
            "h" => "*.h",
            "cpp" => "*.cpp",
            "hpp" => "*.hpp",
            "cc" => "*.cc",
            "cxx" => "*.cxx",
            "hh" => "*.hh",
            "hxx" => "*.hxx",
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

/// Code units of a UTF-16 file decoded when looking for a Qt Linguist
/// catalog. An XML declaration or a `<TS` start tag with its first attribute
/// is far shorter, so the rest of the file is never decoded; a UTF-16 catalog
/// whose leading comments run past this limit is not recognized.
const UTF16_PREFIX_UNITS: usize = 512;

/// Detects a Qt Linguist catalog from the start of a `.ts` file, in UTF-8
/// (with or without a byte-order mark) or in UTF-16 with a byte-order mark
/// (little or big endian; a UTF-16 file without a mark is not guessed).
///
/// After the mark and whitespace, leading XML comments and a DOCTYPE are
/// skipped before checking for an XML declaration (`<?xml`, which no
/// TypeScript statement or expression can start with) or a `<TS` root element
/// that carries an attribute (`<TS version="2.1" ...>`). Both encodings share
/// that one rule ([`is_catalog_start`]): a UTF-16 file is decoded to
/// ASCII-compatible bytes first, and only as far as the rule needs, never the
/// whole file.
fn qt_linguist(content: &[u8]) -> Option<NonSource> {
    let catalog = if let Some(units) = content.strip_prefix(b"\xFF\xFE") {
        is_catalog_start(&utf16_ascii_prefix(units, u16::from_le_bytes))
    } else if let Some(units) = content.strip_prefix(b"\xFE\xFF") {
        is_catalog_start(&utf16_ascii_prefix(units, u16::from_be_bytes))
    } else {
        is_catalog_start(content.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(content))
    };
    catalog.then_some(NonSource::QtLinguist)
}

/// The leading text of UTF-16 `units` (two bytes each, byte order given by
/// `decode`) as ASCII-compatible bytes: whitespace is skipped without limit,
/// then at most [`UTF16_PREFIX_UNITS`] units are kept. A unit outside ASCII
/// becomes `0x80`, which neither the whitespace nor the name checks accept; a
/// trailing odd byte is ignored.
fn utf16_ascii_prefix(units: &[u8], decode: fn([u8; 2]) -> u16) -> Vec<u8> {
    units
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| decode(pair))
        .skip_while(|&unit| u8::try_from(unit).is_ok_and(|byte| byte.is_ascii_whitespace()))
        .take(UTF16_PREFIX_UNITS)
        .map(|unit| u8::try_from(unit).ok().filter(u8::is_ascii).unwrap_or(0x80))
        .collect()
}

/// The shared catalog rule over ASCII-compatible text (see [`qt_linguist`]).
///
/// A bare `<TS>` or `<TS extends Base>(...) => ...` is not enough: both are
/// valid TypeScript (a type assertion and a generic arrow function), so the
/// element must be followed by `name=`.
fn is_catalog_start(text: &[u8]) -> bool {
    let mut text = text.trim_ascii_start();
    loop {
        if let Some(after_comment) = text.strip_prefix(b"<!--") {
            let Some(comment_end) = after_comment.windows(3).position(|window| window == b"-->")
            else {
                return false;
            };
            text = &after_comment[comment_end + 3..];
        } else if text.starts_with(b"<!DOCTYPE") {
            let Some(after_doctype) = skip_doctype(text) else {
                return false;
            };
            text = after_doctype;
        } else {
            break;
        }
        text = text.trim_ascii_start();
    }
    if text.starts_with(b"<?xml") {
        return true;
    }
    let Some(after_tag) = text.strip_prefix(b"<TS") else {
        return false;
    };
    let attributes = after_tag.trim_ascii_start();
    if attributes.len() == after_tag.len() {
        return false;
    }
    let name_len = attributes
        .iter()
        .take_while(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
        })
        .count();
    name_len > 0 && attributes[name_len..].trim_ascii_start().first() == Some(&b'=')
}

fn skip_doctype(text: &[u8]) -> Option<&[u8]> {
    let after_keyword = text.strip_prefix(b"<!DOCTYPE")?;
    if !after_keyword.first().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }

    let mut quote = None;
    let mut subset_depth = 0_usize;
    for (index, &byte) in after_keyword.iter().enumerate() {
        if let Some(quote_byte) = quote {
            if byte == quote_byte {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'[' => subset_depth += 1,
            b']' => subset_depth = subset_depth.saturating_sub(1),
            b'>' if subset_depth == 0 => return Some(&after_keyword[index + 1..]),
            _ => {}
        }
    }
    None
}

pub const ADAPTERS: &[LanguageAdapter] = &[
    LanguageAdapter {
        extension: "rs",
        identifier: "rust",
        cache_version: "rust-chunks-v4",
        grammar: rust,
        syntax: Syntax::Rust,
    },
    LanguageAdapter {
        extension: "ts",
        identifier: "typescript",
        cache_version: "typescript-chunks-v4",
        grammar: typescript,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "tsx",
        identifier: "tsx",
        cache_version: "tsx-chunks-v4",
        grammar: tsx,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "js",
        identifier: "javascript",
        cache_version: "javascript-chunks-v4",
        grammar: javascript,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "jsx",
        identifier: "jsx",
        cache_version: "jsx-chunks-v4",
        grammar: javascript,
        syntax: Syntax::EcmaScript,
    },
    LanguageAdapter {
        extension: "py",
        identifier: "python",
        cache_version: "python-chunks-v4",
        grammar: python,
        syntax: Syntax::Python,
    },
    LanguageAdapter {
        extension: "cs",
        identifier: "csharp",
        cache_version: "csharp-chunks-v4",
        grammar: csharp,
        syntax: Syntax::CSharp,
    },
    LanguageAdapter {
        extension: "go",
        identifier: "go",
        cache_version: "go-chunks-v4",
        grammar: go,
        syntax: Syntax::Go,
    },
    LanguageAdapter {
        extension: "java",
        identifier: "java",
        cache_version: "java-chunks-v4",
        grammar: java,
        syntax: Syntax::Java,
    },
    LanguageAdapter {
        extension: "c",
        identifier: "c",
        cache_version: "c-chunks-v4",
        grammar: c,
        syntax: Syntax::C,
    },
    // `.h` is registered as `c`, a documented decision and not a heuristic:
    // the extension alone cannot tell a C header from a C++ one, and the
    // registry never inspects content to choose a language. `.h` is the C
    // header extension, so it follows `.c`. A C++ project's `.h` headers are
    // therefore chunked with the C grammar (recovered, still searchable) and
    // carry the language `c`; C++ projects that want the C++ grammar name
    // their headers `.hpp`, `.hh`, or `.hxx`. clangd serves both families,
    // so navigation on a `.h` file is unaffected.
    LanguageAdapter {
        extension: "h",
        identifier: "c",
        cache_version: "c-chunks-v4",
        grammar: c,
        syntax: Syntax::C,
    },
    LanguageAdapter {
        extension: "cpp",
        identifier: "cpp",
        cache_version: "cpp-chunks-v4",
        grammar: cpp,
        syntax: Syntax::Cpp,
    },
    LanguageAdapter {
        extension: "hpp",
        identifier: "cpp",
        cache_version: "cpp-chunks-v4",
        grammar: cpp,
        syntax: Syntax::Cpp,
    },
    LanguageAdapter {
        extension: "cc",
        identifier: "cpp",
        cache_version: "cpp-chunks-v4",
        grammar: cpp,
        syntax: Syntax::Cpp,
    },
    LanguageAdapter {
        extension: "cxx",
        identifier: "cpp",
        cache_version: "cpp-chunks-v4",
        grammar: cpp,
        syntax: Syntax::Cpp,
    },
    LanguageAdapter {
        extension: "hh",
        identifier: "cpp",
        cache_version: "cpp-chunks-v4",
        grammar: cpp,
        syntax: Syntax::Cpp,
    },
    LanguageAdapter {
        extension: "hxx",
        identifier: "cpp",
        cache_version: "cpp-chunks-v4",
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

/// Iterate over the registered language identifiers, each once, in the order
/// of their first adapter (`cpp` belongs to two extensions).
pub fn identifiers() -> impl Iterator<Item = &'static str> {
    ADAPTERS
        .iter()
        .enumerate()
        .filter(|(index, adapter)| {
            !ADAPTERS[..*index]
                .iter()
                .any(|earlier| earlier.identifier == adapter.identifier)
        })
        .map(|(_, adapter)| adapter.identifier)
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
        for (extension, identifier, glob, cache_version) in [
            ("rs", "rust", "*.rs", "rust-chunks-v4"),
            ("ts", "typescript", "*.ts", "typescript-chunks-v4"),
            ("tsx", "tsx", "*.tsx", "tsx-chunks-v4"),
            ("js", "javascript", "*.js", "javascript-chunks-v4"),
            ("jsx", "jsx", "*.jsx", "jsx-chunks-v4"),
            ("py", "python", "*.py", "python-chunks-v4"),
            ("cs", "csharp", "*.cs", "csharp-chunks-v4"),
            ("go", "go", "*.go", "go-chunks-v4"),
            ("java", "java", "*.java", "java-chunks-v4"),
            ("c", "c", "*.c", "c-chunks-v4"),
            ("h", "c", "*.h", "c-chunks-v4"),
            ("cpp", "cpp", "*.cpp", "cpp-chunks-v4"),
            ("hpp", "cpp", "*.hpp", "cpp-chunks-v4"),
            ("cc", "cpp", "*.cc", "cpp-chunks-v4"),
            ("cxx", "cpp", "*.cxx", "cpp-chunks-v4"),
            ("hh", "cpp", "*.hh", "cpp-chunks-v4"),
            ("hxx", "cpp", "*.hxx", "cpp-chunks-v4"),
        ] {
            let adapter = for_extension(extension).unwrap();
            assert_eq!(adapter.identifier(), identifier);
            assert_eq!(adapter.glob(), glob);
            assert_eq!(adapter.cache_version(), cache_version);
        }
        for extension in [
            "RS", "TS", "mts", "cts", "mjs", "cjs", "PY", "CS", "GO", "JAVA", "class", "C", "H",
            "CPP", "HPP", "CC", "CXX", "HH", "HXX", "Cc", "Hxx", "c++", "h++", "hp", "ino", "inl",
            "ipp", "tpp", "cu", "m", "mm",
        ] {
            assert!(for_extension(extension).is_none(), "{extension}");
        }
    }

    #[test]
    fn the_c_and_cpp_header_and_source_extensions_map_to_one_family_each() {
        // `.h` is `c` by decision (see the registry comment), never `cpp`, and
        // never sniffed from content; the other header and source extensions
        // are unambiguous C++.
        for (path, identifier) in [
            ("src/a.c", "c"),
            ("include/a.h", "c"),
            ("src/a.cpp", "cpp"),
            ("src/a.cc", "cpp"),
            ("src/a.cxx", "cpp"),
            ("include/a.hpp", "cpp"),
            ("include/a.hh", "cpp"),
            ("include/a.hxx", "cpp"),
            ("include/archive.tar.hh", "cpp"),
        ] {
            let adapter = for_path(Path::new(path)).unwrap();
            assert_eq!(adapter.identifier(), identifier, "{path}");
        }
        // A `.h` file with unmistakable C++ in it is still `c`.
        assert_eq!(for_path(Path::new("widget.h")).unwrap().identifier(), "c");
        for path in ["README", "a.h.bak", "a.hh~", "Makefile.cc.in", ".h"] {
            assert!(for_path(Path::new(path)).is_none(), "{path}");
        }
    }

    fn ts_non_source(content: &[u8]) -> Option<NonSource> {
        for_extension("ts").unwrap().non_source(content)
    }

    #[test]
    fn qt_linguist_catalogs_are_recognized_by_their_xml_start() {
        for content in [
            &b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE TS>\n<TS version=\"2.1\" language=\"de\">\n</TS>\n"[..],
            b"\xEF\xBB\xBF<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<TS version=\"2.1\">",
            b"\n  \r\n\t<?xml version=\"1.0\"?>",
            b"\xEF\xBB\xBF \n<?xml?>",
            b"<!-- hand-edited catalog -->\n<TS version=\"2.1\">",
            b"<!-- before declaration -->\n<?xml version=\"1.0\"?>\n<!-- after declaration -->\n<TS version=\"2.1\">",
            b"<?xml version=\"1.0\"?>\n<!-- after declaration -->\n<TS version=\"2.1\">",
            b"<!DOCTYPE TS>\n<TS version=\"2.1\">",
            b"<!-- catalog -->\n<!DOCTYPE TS>\n<TS version=\"2.1\">",
            b"<TS version=\"2.1\" language=\"de\">\n</TS>",
            b"  <TS\n  version = \"2.1\">",
            b"<TS language=\"de\" version=\"2.1\">",
        ] {
            assert_eq!(
                ts_non_source(content),
                Some(NonSource::QtLinguist),
                "{}",
                String::from_utf8_lossy(content)
            );
        }
        assert_eq!(NonSource::QtLinguist.identifier(), "qt-linguist");
    }

    #[test]
    fn typescript_is_never_mistaken_for_a_qt_linguist_catalog() {
        for content in [
            &b""[..],
            b"   \n",
            b"\xEF\xBB\xBF",
            b"export const value = 1;\n",
            b"// <?xml version=\"1.0\"?>\nexport const value = 1;\n",
            b"// <TS version=\"2.1\">\nexport const value = 1;\n",
            b"/* <?xml */\nexport const value = 1;\n",
            b"\xEF\xBB\xBF// bom then a comment\nexport const value = 1;\n",
            b"const xml = '<?xml version=\"1.0\"?>';\n",
            b"const template = `<!-- not an XML prolog -->`;\nexport const value = 1;\n",
            // A leading `<!--` or `<!DOCTYPE` is skipped, but what follows must
            // still be an XML declaration or a `<TS` root with an attribute.
            b"<!-- an html-style comment line -->\nexport const value = 1;\n",
            b"<!-- unterminated\nexport const value = 1;\n",
            b"<!DOCTYPE html>\nexport const value = 1;\n",
            // Valid TypeScript that starts with `<TS`: a type assertion and
            // generic arrow functions.
            b"<TS>value;\n",
            b"<TS extends Base>(item: TS) => item;\n",
            b"<TS = string>(item: TS) => item;\n",
            b"<TS\n  extends Base>(item: TS) => item;\n",
            b"<TS>\nvalue;\n",
            b"<TSX version=\"1\">",
            b"<T version=\"1\">",
            b"<ts version=\"2.1\">",
            b"<TS",
            b"<TS ",
            b"<TS version",
            b"<?xm",
            b"< ?xml",
        ] {
            assert_eq!(
                ts_non_source(content),
                None,
                "{}",
                String::from_utf8_lossy(content)
            );
        }
    }

    /// `text` as UTF-16, with a byte-order mark when `bom` is set.
    fn utf16(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        for unit in bom
            .then_some(0xFEFF_u16)
            .into_iter()
            .chain(text.encode_utf16())
        {
            bytes.extend(if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            });
        }
        bytes
    }

    #[test]
    fn each_identifier_is_listed_once_in_registration_order() {
        assert_eq!(
            identifiers().collect::<Vec<_>>(),
            [
                "rust",
                "typescript",
                "tsx",
                "javascript",
                "jsx",
                "python",
                "csharp",
                "go",
                "java",
                "c",
                "cpp"
            ]
        );
    }

    #[test]
    fn utf16_qt_linguist_catalogs_with_a_byte_order_mark_are_recognized() {
        let declared = "<?xml version=\"1.0\" encoding=\"utf-16\"?>\n<!DOCTYPE TS>\n<TS version=\"2.1\" language=\"de\">\n<translation>M\u{fc}nzen senden \u{1F4B0}</translation>\n</TS>\n";
        let root_only = "<TS version=\"2.1\" language=\"de\">\n</TS>\n";
        let spaced = " \r\n\t<TS\n  version = \"2.1\">";
        let commented = "<!-- hand-edited catalog -->\n<TS version=\"2.1\">";
        for text in [declared, root_only, spaced, commented, "<?xml?>"] {
            for big_endian in [false, true] {
                let content = utf16(text, big_endian, true);
                assert_eq!(
                    ts_non_source(&content),
                    Some(NonSource::QtLinguist),
                    "big_endian={big_endian}: {text}"
                );
            }
        }
        // Only a bounded prefix is decoded: a large body changes nothing,
        // and neither does whitespace before the root element.
        for big_endian in [false, true] {
            let text = format!("{}<TS version=\"2.1\">", " ".repeat(5000));
            let mut content = utf16(&text, big_endian, true);
            content.extend(std::iter::repeat_n(0xD8, 200_000));
            assert_eq!(ts_non_source(&content), Some(NonSource::QtLinguist));
        }
    }

    #[test]
    fn utf16_typescript_and_look_alikes_are_not_qt_linguist_catalogs() {
        let texts = [
            "export const a = 1;\n",
            "<TS>value;\n",
            "<TS extends Base>(item: TS) => item;\n",
            "// <?xml version=\"1.0\"?>\nexport const a = 1;\n",
            "// <TS version=\"2.1\">\nexport const a = 1;\n",
            "\u{e9}<?xml?>",
            "<?xm",
            "<TS version",
            "",
            "  \n",
        ];
        for text in texts {
            for big_endian in [false, true] {
                let content = utf16(text, big_endian, true);
                assert_eq!(
                    ts_non_source(&content),
                    None,
                    "big_endian={big_endian}: {text:?}"
                );
            }
        }
        // A lone byte-order mark.
        assert_eq!(ts_non_source(b"\xFF\xFE"), None);
        assert_eq!(ts_non_source(b"\xFE\xFF"), None);
        // UTF-16 without a byte-order mark is not guessed, even for a catalog.
        for big_endian in [false, true] {
            let text = "<?xml version=\"1.0\"?>\n<TS version=\"2.1\">";
            let catalog = utf16(text, big_endian, false);
            assert_eq!(ts_non_source(&catalog), None, "big_endian={big_endian}");
        }
        // A UTF-32 little-endian mark starts with the UTF-16 one but is not
        // decoded as UTF-16 text.
        assert_eq!(
            ts_non_source(b"\xFF\xFE\x00\x00<\x00\x00\x00?\x00\x00\x00"),
            None
        );
    }

    #[test]
    fn a_truncated_utf16_stream_is_handled_without_panicking() {
        for big_endian in [false, true] {
            let complete = utf16("<?xml?>", big_endian, true);
            // An odd trailing byte after complete units is ignored.
            let mut odd = complete.clone();
            odd.push(0x3C);
            assert_eq!(ts_non_source(&odd), Some(NonSource::QtLinguist));
            // Every truncation is either a catalog (the `<?xml` units are
            // complete) or not, and none panics.
            let needed = 2 + 2 * "<?xml".len();
            for length in 0..=complete.len() {
                let expected = (length >= needed).then_some(NonSource::QtLinguist);
                assert_eq!(
                    ts_non_source(&complete[..length]),
                    expected,
                    "big_endian={big_endian} length={length}"
                );
            }
            // A cut before the attribute's `=` never matches, odd or even.
            let root = utf16("<TS version=\"2.1\">", big_endian, true);
            let cut = 2 + 2 * "<TS version".len();
            assert_eq!(ts_non_source(&root[..cut]), None);
            assert_eq!(ts_non_source(&root[..=cut]), None);
            assert_eq!(ts_non_source(&root[..cut + 2]), Some(NonSource::QtLinguist));
        }
        assert_eq!(ts_non_source(b"\xFF\xFE<"), None);
        assert_eq!(ts_non_source(b"\xFE\xFF\x00"), None);
    }

    #[test]
    fn only_the_ts_extension_checks_for_a_clash() {
        let utf16_catalog = utf16("<?xml?>", false, true);
        for catalog in [
            &b"<?xml version=\"1.0\"?>\n<TS version=\"2.1\">"[..],
            &utf16_catalog,
        ] {
            for adapter in ADAPTERS {
                let expected = (adapter.glob() == "*.ts").then_some(NonSource::QtLinguist);
                assert_eq!(adapter.non_source(catalog), expected, "{}", adapter.glob());
            }
        }
    }
}
