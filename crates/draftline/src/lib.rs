//! Draftline provides Git-native versioning for creative content workflows.
//!
//! The public API uses content-workflow terms such as [`Workspace`],
//! [`Version`], and [`Variation`] while keeping Git as an implementation
//! detail for most callers.
//!
//! # Content policy
//!
//! Use [`ContentPolicy`] to define which workspace files are user content.
//!
//! ```no_run
//! use draftline::{ContentPolicy, Workspace};
//!
//! fn main() -> Result<(), draftline::DraftlineError> {
//!     let policy = ContentPolicy::new()
//!         .include_paths(["content", "assets"])?
//!         .include_extensions(["md", "txt"])?
//!         .exclude_paths(["content/private"])?;
//!
//!     let workspace = Workspace::init_with_policy("my-content", policy)?;
//!     Ok(())
//! }
//! ```
//!
//! # Variation metadata
//!
//! Variation names are stable Draftline identifiers. Hosts can attach display
//! metadata such as labels and slugs without changing the underlying name.
//!
//! ```no_run
//! use draftline::{VariationMetadata, Workspace};
//!
//! fn main() -> Result<(), draftline::DraftlineError> {
//!     let workspace = Workspace::init("my-content")?;
//!     let version = workspace.save_version("Initial draft")?;
//!     let variation = workspace.create_variation_from_with_metadata(
//!         version.id(),
//!         "draft-a",
//!         VariationMetadata::new()
//!             .with_label("Draft A")
//!             .with_slug("draft-a"),
//!     )?;
//!
//!     assert_eq!(variation.display_label(), "Draft A");
//!     Ok(())
//! }
//! ```
//!
//! # Remote credentials
//!
//! Remote operations accept credential callbacks so host applications can
//! provide credentials from their own authentication flow.
//!
//! ```no_run
//! use draftline::{RemoteCredential, RemoteOptions, Workspace};
//!
//! fn main() -> Result<(), draftline::DraftlineError> {
//!     let token = std::env::var("GITHUB_TOKEN").unwrap();
//!     let mut options = RemoteOptions::new().with_credentials(move |request| {
//!         if request.allows_username_password {
//!             Ok(RemoteCredential::UsernamePassword {
//!                 username: "x-access-token".to_string(),
//!                 password: token.clone(),
//!             })
//!         } else {
//!             Ok(RemoteCredential::Default)
//!         }
//!     });
//!
//!     let workspace = Workspace::open("my-content")?;
//!     workspace.fetch_remote_with_options("origin", &mut options)?;
//!     Ok(())
//! }
//! ```

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
    Contributor, PublishPreflight, PublishResult, PublishToken, RemoteCredential,
    RemoteCredentialRequest, RemoteEndpoint, RemoteOptions, RemoteVersionSummary, SyncState,
    SyncStatus,
};
pub use workspace::{
    AdoptionPreflightReport, ApplyIncomingReport, ApplyIncomingResult, ChangeKind, ChangeSet,
    ChangedFile, ContentPolicyAudit, DiagnosticCode, DiagnosticSeverity, DirtySummary,
    ErrorExplanation, FileHazard, FileHazardKind, HistoryEntry, MergeIncomingReport,
    MergeIncomingResult, MergeIncomingToken, OperationLockInspection, OperationLockMetadata,
    OperationLockState, OperationLockSummary, PreflightReport, PreviewFile, PurgePreflight,
    PurgeToken, PurgeVerification, RecoveryRepairResult, RemoteVariation,
    RemoteVariationDeletePreflight, RemoteVariationDeleteToken, RetryClass, SafeNextAction,
    SharingMode, Shelf, ShelfApplyReport, SupportRef, SupportRefExpirationPreflight,
    SupportRefExpirationToken, SupportRefKind, SupportRefPublishItem, SupportRefPublishPreflight,
    SupportRefPublishToken, SupportRefScope, SupportRefSummary, SwitchPolicy, Variation,
    VariationId, VariationMetadata, VariationSummary, Version, VersionDiff, VersionId,
    VersionPreview, Workspace, WorkspaceCapabilities, WorkspaceDiagnostic, WorkspaceId,
    WorkspaceInspection, WorkspaceSummary, WorkspaceVerification,
};
