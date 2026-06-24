use std::path::PathBuf;

use crate::{PreflightReport, RecoveryState, SyncStatus};

/// Result type used by Draftline APIs.
pub type Result<T> = std::result::Result<T, DraftlineError>;

/// Errors returned by Draftline.
#[derive(Debug, thiserror::Error)]
pub enum DraftlineError {
    #[error("git operation failed: {0}")]
    Git(#[from] git2::Error),

    #[error("I/O operation failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("recovery metadata operation failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("path escapes the workspace: {0}")]
    PathEscapesWorkspace(PathBuf),

    #[error("absolute paths are not accepted for workspace-relative operations: {0}")]
    AbsolutePath(PathBuf),

    #[error(
        "workspace has unsaved changes; choose an explicit policy before switching variations"
    )]
    WorkspaceDirty,

    #[error("operation preflight failed")]
    PreflightFailed(Box<PreflightReport>),

    #[error("workspace has an incomplete Draftline operation")]
    RecoveryRequired(Box<RecoveryState>),

    #[error("invalid variation name: {0}")]
    InvalidVariationName(String),

    #[error("cannot delete the current variation: {0}")]
    CannotDeleteCurrentVariation(String),

    #[error("version was not found: {0}")]
    VersionNotFound(String),

    #[error("workspace has no current variation")]
    NoCurrentVariation,

    #[error("workspace is locked by another Draftline operation")]
    WorkspaceLocked,

    #[error("unsupported switch policy: {0}")]
    UnsupportedSwitchPolicy(&'static str),

    #[error("invalid content policy path: {0}")]
    InvalidContentPolicyPath(PathBuf),

    #[error("invalid content policy extension: {0}")]
    InvalidContentPolicyExtension(String),

    #[error("remote has incoming changes that need an explicit merge plan")]
    SyncNeedsMerge(Box<SyncStatus>),
}
