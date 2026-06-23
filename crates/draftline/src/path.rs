use std::path::{Component, Path, PathBuf};

use crate::{DraftlineError, Result};

/// Normalizes a user-provided relative path without allowing it to leave the workspace.
pub fn normalize_workspace_relative(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();

    if path.is_absolute() {
        return Err(DraftlineError::AbsolutePath(path.to_path_buf()));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DraftlineError::PathEscapesWorkspace(path.to_path_buf()));
            }
        }
    }

    Ok(normalized)
}

/// Resolves a user-provided relative path against a workspace root.
pub fn resolve_workspace_path(
    root: impl AsRef<Path>,
    relative: impl AsRef<Path>,
) -> Result<PathBuf> {
    let normalized = normalize_workspace_relative(relative)?;
    Ok(root.as_ref().join(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_components() {
        let err = normalize_workspace_relative("../secret.txt").unwrap_err();
        assert!(matches!(err, DraftlineError::PathEscapesWorkspace(_)));
    }

    #[test]
    fn rejects_absolute_paths() {
        let absolute = if cfg!(windows) {
            PathBuf::from(r"C:\secret.txt")
        } else {
            PathBuf::from("/secret.txt")
        };

        let err = normalize_workspace_relative(&absolute).unwrap_err();
        assert!(matches!(err, DraftlineError::AbsolutePath(_)));
    }

    #[test]
    fn keeps_safe_relative_paths() {
        assert_eq!(
            normalize_workspace_relative("./posts/hello.md").unwrap(),
            PathBuf::from("posts").join("hello.md")
        );
    }
}
