//! The pattern table of `grep.structural`: for each language, the syntactic
//! roles in which a name is looked for, as ast-grep patterns.
//!
//! `NAME` in a template stands for the bare last segment of the seed's name;
//! `$X` and `$$$X` are ast-grep metavariables (one node, and a run of nodes).
//! A header template has one metavariable, `$T` or `$C`, that holds the other
//! party of the relation (the trait or the type, the subclass), which the
//! map reads back to place the edge; a header that has none (the C++ base
//! clause) is read from the class around the match. A call template reads no
//! metavariable.
//!
//! A template is one of two forms. A plain one is the pattern's source as it
//! stands. A contextual one (`within`) names a larger piece of code and the
//! kind of the node in it that is the pattern: a call alone, such as
//! `$R.Draw($A)`, parses in Go as a type conversion when it stands at the top
//! of a file, but as the call it is inside a function body; a C++ base clause
//! cannot stand without its class.
//!
//! The table is data: a template that does not compile for its grammar is
//! dropped when the set is built and reported by [`Compiled::dropped`]; a
//! test pins that no entry of the table is dropped.

use super::lang::Lang;
use ast_grep_core::{Pattern, PatternError};

/// The syntactic role a pattern asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// An `impl`, `extends` or `implements` header that names the seed.
    Header,
    /// A call of the seed's name.
    Call,
}

/// One entry of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    pub text: &'static str,
    /// For a contextual template, the kind of the node of `text` that is the
    /// pattern.
    pub selector: Option<&'static str>,
}

/// A template that is the pattern's source.
const fn plain(text: &'static str) -> Template {
    Template {
        text,
        selector: None,
    }
}

/// A template whose pattern is the `selector` node of the code `text`.
const fn within(text: &'static str, selector: &'static str) -> Template {
    Template {
        text,
        selector: Some(selector),
    }
}

/// The templates of `role` for a language identifier
/// (`LanguageAdapter::identifier()`); none for a language with no entry.
pub fn patterns(language: &str, role: Role) -> &'static [Template] {
    use Role::{Call, Header};
    const RUST_HEADER: &[Template] = &[
        plain("impl $T for NAME { $$$ }"),
        plain("impl NAME for $T { $$$ }"),
        plain("impl<$$$G> $T for NAME { $$$ }"),
        plain("impl<$$$G> NAME for $T { $$$ }"),
    ];
    const RUST_CALL: &[Template] = &[
        plain("NAME($$$A)"),
        plain("$R.NAME($$$A)"),
        plain("$P::NAME($$$A)"),
    ];
    const TS_HEADER: &[Template] = &[
        plain("class $C extends NAME { $$$ }"),
        plain("class $C implements NAME { $$$ }"),
    ];
    const JS_HEADER: &[Template] = &[plain("class $C extends NAME { $$$ }")];
    const ECMA_CALL: &[Template] = &[plain("NAME($$$A)"), plain("$R.NAME($$$A)")];
    const PYTHON_HEADER: &[Template] = &[
        plain("class $C(NAME): $$$"),
        plain("class $C($$$B, NAME): $$$"),
    ];
    const GO_CALL: &[Template] = &[
        within("func _() { NAME($$$A) }", "call_expression"),
        within("func _() { $R.NAME($$$A) }", "call_expression"),
    ];
    const JAVA_HEADER: &[Template] = &[
        plain("class $C implements NAME { $$$ }"),
        plain("class $C extends NAME { $$$ }"),
    ];
    const CSHARP_HEADER: &[Template] = &[plain("class $C : NAME { $$$ }")];
    const MEMBER_CALL: &[Template] = &[plain("$R.NAME($$$A)"), plain("NAME($$$A)")];
    // At the top of a file `NAME(..)` parses as a function declaration.
    const C_CALL: &[Template] = &[within("void _() { NAME($$$A); }", "call_expression")];
    const CPP_HEADER: &[Template] = &[
        within("class Z : public NAME {};", "base_class_clause"),
        within("struct Z : NAME {};", "base_class_clause"),
    ];
    match (language, role) {
        ("rust", Header) => RUST_HEADER,
        ("rust", Call) => RUST_CALL,
        ("typescript" | "tsx", Header) => TS_HEADER,
        ("javascript" | "jsx", Header) => JS_HEADER,
        ("typescript" | "tsx" | "javascript" | "jsx", Call) => ECMA_CALL,
        ("python", Header) => PYTHON_HEADER,
        ("python", Call) => ECMA_CALL,
        // Interfaces are structural: no header names one.
        ("go", Header) => &[],
        ("go", Call) => GO_CALL,
        ("java", Header) => JAVA_HEADER,
        ("csharp", Header) => CSHARP_HEADER,
        ("java" | "csharp", Call) => MEMBER_CALL,
        ("c", Call) | ("cpp", Call) => C_CALL,
        ("cpp", Header) => CPP_HEADER,
        _ => &[],
    }
}

/// Every language identifier that has at least one entry.
pub const LANGUAGES: &[&str] = &[
    "rust",
    "typescript",
    "tsx",
    "javascript",
    "jsx",
    "python",
    "go",
    "java",
    "csharp",
    "c",
    "cpp",
];

/// A template's source with `name` in place of `NAME`.
pub fn instantiate(template: &str, name: &str) -> String {
    template.replace("NAME", name)
}

/// One template, parsed for a grammar with `name` in it.
pub fn compile(lang: &Lang, template: &Template, name: &str) -> Result<Pattern, PatternError> {
    let text = instantiate(template.text, name);
    match template.selector {
        Some(selector) => Pattern::contextual(&text, selector, *lang),
        None => Pattern::try_new(&text, *lang),
    }
}

/// The patterns of one language for one name.
pub struct Compiled {
    pub header: Vec<Pattern>,
    pub call: Vec<Pattern>,
    /// Templates (instantiated) that did not compile, with the reason.
    pub dropped: Vec<String>,
}

impl Compiled {
    /// Compiles every template of `lang` for `name`; a template that fails is
    /// listed in `dropped` and costs nothing else.
    pub fn build(lang: &Lang, name: &str) -> Self {
        let mut compiled = Self {
            header: Vec::new(),
            call: Vec::new(),
            dropped: Vec::new(),
        };
        for role in [Role::Header, Role::Call] {
            for template in patterns(lang.identifier(), role) {
                let text = instantiate(template.text, name);
                match compile(lang, template, name) {
                    Ok(pattern) if !pattern.has_error() => match role {
                        Role::Header => compiled.header.push(pattern),
                        Role::Call => compiled.call.push(pattern),
                    },
                    Ok(_) => compiled.dropped.push(format!(
                        "{}: `{text}` parses with an error",
                        lang.identifier()
                    )),
                    Err(error) => compiled
                        .dropped
                        .push(format!("{}: `{text}`: {error}", lang.identifier())),
                }
            }
        }
        compiled
    }
}
