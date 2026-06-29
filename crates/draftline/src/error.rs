use std::path::PathBuf;

use crate::{workspace::VariationCreatePreflight, PreflightReport, RecoveryState, SyncStatus};

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

    #[error("variation creation preflight failed")]
    VariationCreatePreflightFailed(Box<VariationCreatePreflight>),

    #[error("workspace has an incomplete Draftline operation")]
    RecoveryRequired(Box<RecoveryState>),

    #[error("recovery operation was not found: {0}")]
    RecoveryNotFound(String),

    #[error("invalid variation name: {0}")]
    InvalidVariationName(String),

    #[error("invalid graph options: {0}")]
    InvalidGraphOptions(String),

    #[error("variation already exists: {0}")]
    VariationAlreadyExists(String),

    #[error("variation was not found: {0}")]
    VariationNotFound(String),

    #[error("cannot delete the current variation: {0}")]
    CannotDeleteCurrentVariation(String),

    #[error("version was not found: {0}")]
    VersionNotFound(String),

    #[error("version is not reachable from a local variation: {0}")]
    VersionNotLocallyReachable(String),

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

    #[error("path is outside tracked content policy: {0}")]
    PathOutsideContentPolicy(PathBuf),

    #[error("remote has incoming changes that need an explicit merge plan")]
    SyncNeedsMerge(Box<SyncStatus>),

    #[error(
        "remote state changed for {remote}/{variation}; expected {expected:?}, actual {actual:?}"
    )]
    RemoteRace {
        remote: String,
        variation: String,
        expected: Option<String>,
        actual: Option<String>,
    },

    #[error("local publish state changed; expected {expected}, actual {actual}")]
    LocalStateChanged { expected: String, actual: String },

    #[error(
        "remote URL uses {scheme}, but Draftline/libgit2 was built without {required_feature} transport support"
    )]
    UnsupportedRemoteTransport {
        scheme: String,
        required_feature: &'static str,
    },

    #[error("operation requires explicit confirmation: {0}")]
    ConsentRequired(String),

    #[error("squash requires at least 2 versions, got {0}")]
    InvalidSquashCount(usize),

    #[error("not enough versions to squash: need {needed}, available {available}")]
    NotEnoughVersionsToSquash { needed: usize, available: usize },

    #[error("invalid contributor identity: {0}")]
    InvalidContributorIdentity(String),

    #[error("invalid merge resolution: {0}")]
    InvalidMergeResolution(String),

    #[error("invalid history cleanup request: {0}")]
    InvalidHistoryCleanup(String),
}
