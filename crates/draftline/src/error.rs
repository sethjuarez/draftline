use std::path::PathBuf;

use crate::{workspace::VariationCreatePreflight, PreflightReport, RecoveryState, SyncStatus};

/// Result type used by Draftline APIs.
pub type Result<T> = std::result::Result<T, DraftlineError>;

/// Errors returned by Draftline.
#[derive(Debug, thiserror::Error)]
pub enum DraftlineError {
    #[error("git operation failed: {0}")]
    Git(git2::Error),

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

    #[error("another folder in this checkout has an incomplete Draftline operation: {0}")]
    RecoveryInOtherFolder(PathBuf),

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

    /// Another Git process holds one of the repository's lock files (its
    /// `index.lock` or a ref's `.lock`), as `git` does while it stages,
    /// commits, or checks out. Retry once it finishes; if no Git process is
    /// running, the lock file was left by a crash.
    #[error("Git is busy in this repository (a .lock file exists); try again when it finishes")]
    GitIndexLocked,

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

    #[error("invalid contributor identity: {0}")]
    InvalidContributorIdentity(String),

    #[error("remote operation timed out{suffix} while running {operation}", suffix = timeout_suffix(*timeout_ms))]
    RemoteOperationTimedOut { operation: String, timeout_ms: u64 },

    #[error("invalid merge resolution: {0}")]
    InvalidMergeResolution(String),

    #[error("cannot tell which branch to send to; pass a base branch")]
    SendBaseUnknown,

    #[error("the remote has no branch `{0}` to send to; publish it first or pass a base branch")]
    SendBaseNotOnRemote(String),

    /// Draftline manages a different folder in this repository.
    #[error(
        "Draftline manages {} in this repository; open that folder instead",
        workspace_folder(.0)
    )]
    FolderIsElsewhere(std::path::PathBuf),

    /// The incoming version keeps Draftline's folder somewhere else: move the
    /// folder there, then merge.
    #[error(
        "the incoming version keeps Draftline's folder at {}; move the folder there, then merge",
        workspace_folder(.0)
    )]
    IncomingFolderElsewhere(std::path::PathBuf),

    /// The folder Draftline manages isn't here: it was renamed or deleted
    /// outside Draftline, or the current branch doesn't have it.
    #[error(
        "Draftline manages {}, which is missing here; switch back to a branch that has it, or fix `.draftline`",
        workspace_folder(.0)
    )]
    FolderMissing(std::path::PathBuf),

    #[error("unreadable Draftline folder record: {0}")]
    InvalidFolderMarker(String),

    /// `.draftline` uses a newer format; update Draftline to open this folder.
    #[error("this folder needs a newer Draftline: {0}")]
    NewerFolderMarker(String),

    /// The folder can't move there; the text says why.
    #[error("cannot move the Draftline folder: {0}")]
    FolderMoveRefused(String),

    /// The fix isn't one [`crate::Workspace::probe`] offers for the folder now.
    #[error("that fix isn't available for this folder: {0}")]
    FolderFixUnavailable(String),

    /// The remote renamed its default branch from `from` to `to`, and this
    /// branch is the old name: publishing would bring the old name back.
    #[error(
        "the remote renamed `{from}` to `{to}`; rename this branch to `{to}` or start a variation for these saves"
    )]
    BaseRenamed { from: String, to: String },

    #[error("Send branch `{0}` is outside this project's variation namespace")]
    SendOutsideNamespace(String),

    #[error("branch `{0}` is outside this project's variations")]
    VariationOutsideNamespace(String),

    #[error("pull request provider failed: {0}")]
    PullRequest(String),

    #[error("merge needs a developer: files outside the project conflict: {0:?}")]
    MergeNeedsDeveloper(Vec<std::path::PathBuf>),

    #[error("worktree `{0}` was not created by Draftline")]
    NotOwnedWorktree(String),

    #[error("worktree `{0}` is the one this workspace is open in")]
    OwnedWorktreeInUse(String),

    #[error("worktree has unsaved or untracked files: {0:?}")]
    OwnedWorktreeHasChanges(Vec<std::path::PathBuf>),
}

impl From<git2::Error> for DraftlineError {
    /// libgit2 reports a lock file held by Git (index or ref) as `Locked`;
    /// every operation surfaces it as the same retryable error.
    fn from(error: git2::Error) -> Self {
        if error.code() == git2::ErrorCode::Locked {
            DraftlineError::GitIndexLocked
        } else {
            DraftlineError::Git(error)
        }
    }
}

fn workspace_folder(project: &std::path::Path) -> String {
    if project.as_os_str().is_empty() {
        "the repository root".to_string()
    } else {
        format!("`{}`", project.display())
    }
}

fn timeout_suffix(timeout_ms: u64) -> String {
    if timeout_ms == 0 {
        String::new()
    } else {
        format!(" after {timeout_ms}ms")
    }
}
