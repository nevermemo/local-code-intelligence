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

#[cfg(test)]
mod tests {
    use super::normalize_path_separators;

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
