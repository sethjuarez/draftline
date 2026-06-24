use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Input passed to a semantic merge resolver.
#[derive(Debug, Clone, Copy)]
pub struct MergeInput<'a> {
    pub path: &'a Path,
    pub base: &'a str,
    pub ours: &'a str,
    pub theirs: &'a str,
}

/// Result of a semantic merge attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub merged: Option<String>,
    pub conflicts: Vec<MergeConflict>,
}

impl MergeOutcome {
    pub fn clean(merged: impl Into<String>) -> Self {
        Self {
            merged: Some(merged.into()),
            conflicts: Vec::new(),
        }
    }

    pub fn conflicted(conflict: MergeConflict) -> Self {
        Self {
            merged: None,
            conflicts: vec![conflict],
        }
    }

    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// A human-resolvable content conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflict {
    pub path: PathBuf,
    pub field_path: Option<String>,
    pub label: String,
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub resolution: ResolutionKind,
}

/// Suggested shape of resolution a UI or caller can present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionKind {
    Choose,
    Edit,
    Combine,
    Delete,
}

/// Trait implemented by content-aware merge resolvers.
pub trait SemanticMergeResolver: Send + Sync {
    fn matches(&self, path: &Path) -> bool;

    fn merge(&self, input: MergeInput<'_>) -> MergeOutcome;
}

/// Registry that selects the first matching resolver and falls back to plain text.
#[derive(Default)]
pub struct ResolverRegistry {
    resolvers: Vec<Box<dyn SemanticMergeResolver>>,
}

impl ResolverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_default_resolvers() -> Self {
        Self::new()
            .register(MarkdownResolver)
            .register(PlainTextResolver)
    }

    pub fn register<R>(mut self, resolver: R) -> Self
    where
        R: SemanticMergeResolver + 'static,
    {
        self.resolvers.push(Box::new(resolver));
        self
    }

    pub fn merge(&self, input: MergeInput<'_>) -> MergeOutcome {
        if let Some(resolver) = self
            .resolvers
            .iter()
            .find(|resolver| resolver.matches(input.path))
        {
            return resolver.merge(input);
        }

        PlainTextResolver.merge(input)
    }
}

/// Plain text resolver using safe three-way choices before surfacing a conflict.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainTextResolver;

impl SemanticMergeResolver for PlainTextResolver {
    fn matches(&self, _path: &Path) -> bool {
        true
    }

    fn merge(&self, input: MergeInput<'_>) -> MergeOutcome {
        if input.ours == input.theirs {
            return MergeOutcome::clean(input.ours);
        }

        if input.base == input.ours {
            return MergeOutcome::clean(input.theirs);
        }

        if input.base == input.theirs {
            return MergeOutcome::clean(input.ours);
        }

        MergeOutcome::conflicted(MergeConflict {
            path: input.path.to_path_buf(),
            field_path: None,
            label: "Text content changed in both versions".to_string(),
            base: Some(input.base.to_string()),
            ours: Some(input.ours.to_string()),
            theirs: Some(input.theirs.to_string()),
            resolution: ResolutionKind::Edit,
        })
    }
}

/// Lightweight Markdown/frontmatter proof resolver.
#[derive(Debug, Default, Clone, Copy)]
pub struct MarkdownResolver;

impl SemanticMergeResolver for MarkdownResolver {
    fn matches(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("md" | "markdown")
        )
    }

    fn merge(&self, input: MergeInput<'_>) -> MergeOutcome {
        let base = MarkdownParts::split(input.base);
        let ours = MarkdownParts::split(input.ours);
        let theirs = MarkdownParts::split(input.theirs);

        let frontmatter = PlainTextResolver.merge(MergeInput {
            path: input.path,
            base: base.frontmatter.unwrap_or_default(),
            ours: ours.frontmatter.unwrap_or_default(),
            theirs: theirs.frontmatter.unwrap_or_default(),
        });

        if frontmatter.has_conflicts() {
            return MergeOutcome {
                merged: None,
                conflicts: frontmatter
                    .conflicts
                    .into_iter()
                    .map(|mut conflict| {
                        conflict.field_path = Some("frontmatter".to_string());
                        conflict.label = "Frontmatter changed in both versions".to_string();
                        conflict
                    })
                    .collect(),
            };
        }

        let body = PlainTextResolver.merge(MergeInput {
            path: input.path,
            base: base.body,
            ours: ours.body,
            theirs: theirs.body,
        });

        if body.has_conflicts() {
            return body;
        }

        let merged_frontmatter = frontmatter.merged.unwrap_or_default();
        let merged_body = body.merged.unwrap_or_default();

        if merged_frontmatter.is_empty() {
            MergeOutcome::clean(merged_body)
        } else {
            MergeOutcome::clean(format!("---\n{}---\n{}", merged_frontmatter, merged_body))
        }
    }
}

#[derive(Debug)]
struct MarkdownParts<'a> {
    frontmatter: Option<&'a str>,
    body: &'a str,
}

impl<'a> MarkdownParts<'a> {
    fn split(markdown: &'a str) -> Self {
        let Some(rest) = markdown.strip_prefix("---\n") else {
            return Self {
                frontmatter: None,
                body: markdown,
            };
        };

        let Some(end) = rest.find("\n---\n") else {
            return Self {
                frontmatter: None,
                body: markdown,
            };
        };

        let frontmatter = &rest[..end + 1];
        let body = &rest[end + "\n---\n".len()..];

        Self {
            frontmatter: Some(frontmatter),
            body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_accepts_their_change_when_ours_is_unchanged() {
        let outcome = PlainTextResolver.merge(MergeInput {
            path: Path::new("note.txt"),
            base: "hello",
            ours: "hello",
            theirs: "hello world",
        });

        assert_eq!(outcome.merged.as_deref(), Some("hello world"));
        assert!(!outcome.has_conflicts());
    }

    #[test]
    fn plain_text_reports_real_conflicts() {
        let outcome = PlainTextResolver.merge(MergeInput {
            path: Path::new("note.txt"),
            base: "hello",
            ours: "hello ours",
            theirs: "hello theirs",
        });

        assert!(outcome.has_conflicts());
        assert_eq!(outcome.conflicts[0].resolution, ResolutionKind::Edit);
    }

    #[test]
    fn markdown_marks_frontmatter_conflicts_structurally() {
        let outcome = MarkdownResolver.merge(MergeInput {
            path: Path::new("post.md"),
            base: "---\ntitle: Old\n---\nBody",
            ours: "---\ntitle: Ours\n---\nBody",
            theirs: "---\ntitle: Theirs\n---\nBody",
        });

        assert!(outcome.has_conflicts());
        assert_eq!(
            outcome.conflicts[0].field_path.as_deref(),
            Some("frontmatter")
        );
    }
}
