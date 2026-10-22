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

/// `path` without a Windows verbatim prefix: `\\?\C:\a` becomes `C:\a` and
/// `\\?\UNC\server\share\a` becomes `\\server\share\a`. Any other path
/// (including every path off Windows, and a verbatim form with no plain
/// equivalent such as `\\?\Volume{..}`) comes back unchanged.
///
/// `dunce::canonicalize` strips the prefix only while the plain path fits in
/// `MAX_PATH`, so a path of 260 characters or more keeps it while a shorter
/// workspace root does not (N48). The two then differ in their first
/// component and `Path::starts_with`/`strip_prefix` call them unrelated.
pub fn without_verbatim(path: &std::path::Path) -> std::borrow::Cow<'_, std::path::Path> {
    use std::borrow::Cow;
    #[cfg(windows)]
    if let Some(text) = path.to_str() {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return Cow::Owned(std::path::PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return Cow::Owned(std::path::PathBuf::from(rest));
        }
    }
    Cow::Borrowed(path)
}

/// `path` relative to `root`, or `None` when `path` is not under `root`.
/// Like `Path::strip_prefix`, except that a verbatim spelling of either side
/// (see [`without_verbatim`]) still matches the plain spelling of the other.
pub fn strip_root(path: &std::path::Path, root: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Some(relative.to_path_buf());
    }
    let (path, root) = (without_verbatim(path), without_verbatim(root));
    path.strip_prefix(root.as_ref())
        .ok()
        .map(std::path::Path::to_path_buf)
}

/// Whether `path` is `root` or under it, with the verbatim-prefix tolerance of
/// [`strip_root`].
pub fn is_within(path: &std::path::Path, root: &std::path::Path) -> bool {
    strip_root(path, root).is_some()
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

    #[cfg(windows)]
    #[test]
    fn a_verbatim_spelling_matches_the_plain_one_in_either_direction() {
        use super::{is_within, strip_root, without_verbatim};
        use std::path::{Path, PathBuf};
        let root = Path::new(r"C:\ws");
        let verbatim = Path::new(r"\\?\C:\ws\src\a.rs");
        // The premise: std treats the two spellings as unrelated.
        assert!(verbatim.strip_prefix(root).is_err());
        assert_eq!(strip_root(verbatim, root), Some(PathBuf::from(r"src\a.rs")));
        assert_eq!(
            strip_root(Path::new(r"C:\ws\src\a.rs"), Path::new(r"\\?\C:\ws")),
            Some(PathBuf::from(r"src\a.rs"))
        );
        assert!(is_within(verbatim, root));
        assert!(is_within(root, Path::new(r"\\?\C:\ws")));
        assert!(!is_within(Path::new(r"\\?\C:\other\a.rs"), root));
        assert!(!is_within(Path::new(r"\\?\C:\wsx\a.rs"), root));
        assert!(!is_within(Path::new(r"\\?\D:\ws\a.rs"), root));
        assert_eq!(
            without_verbatim(Path::new(r"\\?\UNC\srv\share\a")).as_ref(),
            Path::new(r"\\srv\share\a")
        );
        // No plain equivalent: left alone.
        let volume = Path::new(r"\\?\Volume{1234}\a");
        assert_eq!(without_verbatim(volume).as_ref(), volume);
    }

    #[cfg(unix)]
    #[test]
    fn off_windows_the_helpers_are_plain_prefix_tests() {
        use super::{is_within, strip_root};
        use std::path::{Path, PathBuf};
        assert_eq!(
            strip_root(Path::new("/w/src/a.rs"), Path::new("/w")),
            Some(PathBuf::from("src/a.rs"))
        );
        assert!(!is_within(Path::new("/wx/a.rs"), Path::new("/w")));
    }
}
