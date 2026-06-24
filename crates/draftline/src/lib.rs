//! Draftline provides Git-native versioning for creative content workflows.
//!
//! The public API uses content-workflow terms such as [`Workspace`],
//! [`Version`], and [`Variation`] while keeping Git as an implementation
//! detail for most callers.

pub mod content_policy;
pub mod error;
pub mod merge;
pub mod path;
pub mod recovery;
pub mod remote;
pub mod workspace;

pub use content_policy::ContentPolicy;
pub use error::{DraftlineError, Result};
pub use recovery::{RecoveryOperation, RecoveryState};
pub use remote::{
    Contributor, PublishResult, RemoteCredential, RemoteCredentialRequest, RemoteEndpoint,
    RemoteOptions, RemoteVersionSummary, SyncState, SyncStatus,
};
pub use workspace::{
    ChangeKind, ChangeSet, ChangedFile, PreflightReport, PreviewFile, SwitchPolicy, Variation,
    VariationId, VariationMetadata, Version, VersionId, VersionPreview, Workspace,
};
