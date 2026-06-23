use std::path::{Path, PathBuf};

use crate::{path::normalize_workspace_relative, DraftlineError, Result};

/// Defines which workspace files are user content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentPolicy {
    includes: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    large_file_threshold_bytes: u64,
}

impl Default for ContentPolicy {
    fn default() -> Self {
        Self {
            includes: Vec::new(),
            excludes: vec![PathBuf::from(".draftline")],
            large_file_threshold_bytes: 10 * 1024 * 1024,
        }
    }
}

impl ContentPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn include(mut self, path: impl AsRef<Path>) -> Result<Self> {
        self.includes.push(normalize_policy_path(path)?);
        Ok(self)
    }

    pub fn exclude(mut self, path: impl AsRef<Path>) -> Result<Self> {
        self.excludes.push(normalize_policy_path(path)?);
        Ok(self)
    }

    pub fn with_large_file_threshold(mut self, bytes: u64) -> Self {
        self.large_file_threshold_bytes = bytes;
        self
    }

    pub fn large_file_threshold_bytes(&self) -> u64 {
        self.large_file_threshold_bytes
    }

    pub fn tracks(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = normalize_workspace_relative(path)?;

        let included = self.includes.is_empty()
            || self
                .includes
                .iter()
                .any(|include| path == *include || path.starts_with(include));
        let excluded = self
            .excludes
            .iter()
            .any(|exclude| path == *exclude || path.starts_with(exclude));

        Ok(included && !excluded)
    }
}

fn normalize_policy_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let normalized = normalize_workspace_relative(path)?;
    if normalized.as_os_str().is_empty() {
        return Err(DraftlineError::InvalidContentPolicyPath(normalized));
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_draftline_state_by_default() {
        let policy = ContentPolicy::default();

        assert!(!policy.tracks(".draftline/ui-state.json").unwrap());
        assert!(policy.tracks("posts/hello.md").unwrap());
    }

    #[test]
    fn include_roots_limit_content() {
        let policy = ContentPolicy::new().include("content").unwrap();

        assert!(policy.tracks("content/post.md").unwrap());
        assert!(!policy.tracks("scratch/post.md").unwrap());
    }
}
