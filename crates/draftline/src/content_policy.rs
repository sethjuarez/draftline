use std::path::{Path, PathBuf};

use crate::{path::normalize_workspace_relative, DraftlineError, Result};

/// Defines which workspace files are user content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentPolicy {
    includes: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    include_extensions: Vec<String>,
    large_file_threshold_bytes: u64,
}

impl Default for ContentPolicy {
    fn default() -> Self {
        Self {
            includes: Vec::new(),
            excludes: vec![PathBuf::from(".draftline")],
            include_extensions: Vec::new(),
            large_file_threshold_bytes: 10 * 1024 * 1024,
        }
    }
}

impl ContentPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tracks content at one workspace-relative path.
    pub fn include(mut self, path: impl AsRef<Path>) -> Result<Self> {
        self.includes.push(normalize_policy_path(path)?);
        Ok(self)
    }

    /// Tracks content under multiple workspace-relative paths.
    pub fn include_paths<I, P>(mut self, paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        for path in paths {
            self = self.include(path)?;
        }
        Ok(self)
    }

    /// Ignores content at one workspace-relative path.
    pub fn exclude(mut self, path: impl AsRef<Path>) -> Result<Self> {
        self.excludes.push(normalize_policy_path(path)?);
        Ok(self)
    }

    /// Ignores content under multiple workspace-relative paths.
    pub fn exclude_paths<I, P>(mut self, paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        for path in paths {
            self = self.exclude(path)?;
        }
        Ok(self)
    }

    /// Tracks files with one extension, anywhere in the workspace.
    pub fn include_extension(mut self, extension: impl AsRef<str>) -> Result<Self> {
        self.include_extensions
            .push(normalize_extension(extension.as_ref())?);
        Ok(self)
    }

    /// Tracks files with any of the provided extensions, anywhere in the workspace.
    pub fn include_extensions<I, S>(mut self, extensions: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for extension in extensions {
            self = self.include_extension(extension)?;
        }
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

        let included = (self.includes.is_empty() && self.include_extensions.is_empty())
            || self
                .includes
                .iter()
                .any(|include| path == *include || path.starts_with(include))
            || path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| {
                    self.include_extensions
                        .iter()
                        .any(|included| included == &extension.to_ascii_lowercase())
                })
                .unwrap_or(false);
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

fn normalize_extension(extension: &str) -> Result<String> {
    let normalized = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.contains('/')
        || normalized.contains('\\')
        || normalized.contains("..")
        || normalized.chars().any(char::is_control)
    {
        return Err(DraftlineError::InvalidContentPolicyExtension(
            extension.to_string(),
        ));
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

    #[test]
    fn include_extensions_track_matching_files_anywhere() {
        let policy = ContentPolicy::new()
            .include_extension(".sk")
            .unwrap()
            .include_extension("MD")
            .unwrap();

        assert!(policy.tracks("idea.sk").unwrap());
        assert!(policy.tracks("notes/brief.md").unwrap());
        assert!(!policy.tracks("ui-state/panel.json").unwrap());
    }

    #[test]
    fn include_paths_and_extensions_can_be_added_in_batches() {
        let policy = ContentPolicy::new()
            .include_paths(["visuals", "screenshots"])
            .unwrap()
            .include_extensions(["sk", "sb", "md"])
            .unwrap();

        assert!(policy.tracks("visuals/storyboard.json").unwrap());
        assert!(policy.tracks("screenshots/scene.png").unwrap());
        assert!(policy.tracks("notes/script.SK").unwrap());
        assert!(policy.tracks("boards/shot.sb").unwrap());
        assert!(!policy.tracks("app/ui-state.json").unwrap());
    }

    #[test]
    fn exclude_paths_can_be_added_in_batches() {
        let policy = ContentPolicy::new()
            .include_extensions(["md", "sk"])
            .unwrap()
            .exclude_paths(["private", "scratch"])
            .unwrap();

        assert!(policy.tracks("public/notes.md").unwrap());
        assert!(!policy.tracks("private/notes.md").unwrap());
        assert!(!policy.tracks("scratch/idea.sk").unwrap());
    }

    #[test]
    fn excludes_override_extensions() {
        let policy = ContentPolicy::new()
            .include_extension("md")
            .unwrap()
            .exclude("private")
            .unwrap();

        assert!(!policy.tracks("private/notes.md").unwrap());
    }
}
