//! Cross-platform path joining for multi-segment relative paths and embedded
//! `/` manifest keys.

use std::path::{Path, PathBuf};

/// Joins a sequence of single-component path segments onto `base` using the
/// host platform's path separator.
pub(crate) fn join_all<'a>(
    base: impl AsRef<Path>,
    parts: impl IntoIterator<Item = &'a str>,
) -> PathBuf {
    parts
        .into_iter()
        .fold(base.as_ref().to_path_buf(), |path, part| path.join(part))
}

/// Joins a relative path string that may use `/` or `\` separators (such as an
/// embedded template key or an `nsh.json` path) onto `base` component by
/// component, or returns `relative` directly when it is already absolute.
pub(crate) fn join_relative(base: impl AsRef<Path>, relative: &str) -> PathBuf {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    relative
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .fold(base.as_ref().to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_segments_and_normalizes_relative_separators() {
        let root = std::env::temp_dir().join("nsh-paths-test");
        let expected = root.join("entry").join("libs").join("arm64-v8a");
        assert_eq!(join_all(&root, ["entry", "libs", "arm64-v8a"]), expected);
        assert_eq!(join_relative(&root, "entry/libs/arm64-v8a"), expected);
        assert_eq!(join_relative(&root, r"entry\libs\arm64-v8a"), expected);
        assert_eq!(join_relative(&root, "./entry//libs/arm64-v8a/"), expected);
        assert_eq!(
            join_relative(&root, "../sibling/icon.png"),
            root.join("..").join("sibling").join("icon.png")
        );
        let abs = std::env::temp_dir().join("absolute-icon.png");
        assert_eq!(join_relative(&root, &abs.to_string_lossy()), abs);
    }
}
