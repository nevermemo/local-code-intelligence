/// Normalize path separators according to the host operating system.
///
/// On Unix, backslashes are ordinary filename characters. On Windows, they
/// are path separators and are normalized to `/`.
pub fn normalize_path_separators(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

/// Whether the final component of `path` has the file extension `extension`
/// (given without the dot), compared as ASCII case-insensitive text.
///
/// File extensions are not case-sensitive in LCI (N94): Windows file systems
/// do not tell `lib.rs` from `Lib.RS`, so a `.RS` file is a Rust file. This
/// is the one helper for a file-name extension test outside the language
/// registry, whose lookup (`language::for_extension`) folds the same way. A
/// name with nothing before its only dot (`.rs`) has no extension.
pub fn has_extension_ignore_case(path: impl AsRef<std::path::Path>, extension: &str) -> bool {
    path.as_ref()
        .extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
    use super::{has_extension_ignore_case, normalize_path_separators};

    #[test]
    fn an_extension_matches_in_any_ascii_case_and_only_as_the_last_one() {
        for path in ["lib.rs", "Lib.RS", "src/Lib.Rs", "a.b/c.rS"] {
            assert!(has_extension_ignore_case(path, "rs"), "{path}");
        }
        for path in ["lib.rst", "lib.rs.bak", "rs", ".rs", "lib.r", "a.rs/b"] {
            assert!(!has_extension_ignore_case(path, "rs"), "{path}");
        }
        assert!(has_extension_ignore_case(
            std::path::Path::new("A.MD"),
            "md"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_unix_backslashes() {
        assert_eq!(normalize_path_separators(r"a\b/c.rs"), r"a\b/c.rs");
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_both_windows_path_separators() {
        assert_eq!(normalize_path_separators(r"a\b/c.rs"), "a/b/c.rs");
    }
}
