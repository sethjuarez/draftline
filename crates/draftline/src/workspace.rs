use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{
    build::{CheckoutBuilder, RepoBuilder},
    BranchType, Commit, DiffFormat, DiffOptions, Direction, FetchPrune, ObjectType, Oid,
    Repository, RepositoryInitOptions, Signature, Status, StatusOptions, Tree,
};
use serde::{Deserialize, Serialize, Serializer};

use crate::git_cli;
use crate::merge::{MergeConflict, MergeInput, ResolverRegistry};
use crate::pull_request::{
    DeveloperCommit, PullRequestProvider, PullRequestRequest, SendPreflight, SendRequest,
    SendResult, SendToken,
};
use crate::recovery::RecoveryOperation;
use crate::remote::{ensure_supported_remote_url, PushRefExpectation};
use crate::worktree::{OwnedWorktree, WorktreeStart, OWNED_WORKTREE_PREFIX};
use crate::{
    path::normalize_workspace_relative, ContentPolicy, Contributor, ContributorProfile,
    DraftlineError, OutgoingFile, OutgoingFileKind, PublishPreflight, PublishResult, PublishToken,
    RecoveryState, RemoteEndpoint, RemoteOptions, RemoteVersionSummary, Result, SyncState,
    SyncStatus,
};

/// A folder-backed content workspace.
pub struct Workspace {
    root: PathBuf,
    repo: Repository,
    content_policy: ContentPolicy,
    resolver_registry: ResolverRegistry,
    variation_namespace: Option<String>,
}

/// A named version of the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    id: VersionId,
    pub label: String,
    pub author: Contributor,
    pub saved_by: Contributor,
    pub time_seconds: i64,
}

impl Version {
    pub fn id(&self) -> &VersionId {
        &self.id
    }
}

/// Identifier for a version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionId(String);

impl VersionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parses a version identifier from a canonical 40-character hex SHA string.
    /// Returns an error if the string is not a valid full-length (40-character) hex OID.
    ///
    /// Abbreviated OIDs are intentionally rejected.  The method is named
    /// `from_canonical_string` because it accepts only the unambiguous, fully
    /// spelled-out form that round-trips safely across process boundaries and
    /// storage layers.
    ///
    /// ```no_run
    /// use draftline::VersionId;
    ///
    /// let id = VersionId::from_canonical_string("a1b2c3d4e5f6...").unwrap();
    /// ```
    pub fn from_canonical_string(s: impl AsRef<str>) -> crate::Result<Self> {
        let s = s.as_ref();
        // Require exactly 40 lowercase hex characters — the full SHA-1 OID.
        // git2::Oid::from_str accepts abbreviated prefixes, so we enforce the
        // length constraint here before delegating format validation.
        if s.len() != 40 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(crate::DraftlineError::VersionNotFound(s.to_string()));
        }
        Oid::from_str(s).map_err(|_| crate::DraftlineError::VersionNotFound(s.to_string()))?;
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for VersionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<Oid> for VersionId {
    fn from(value: Oid) -> Self {
        Self(value.to_string())
    }
}

/// An alternate direction for workspace content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variation {
    id: VariationId,
    pub name: String,
    pub metadata: VariationMetadata,
    pub is_current: bool,
}

impl Variation {
    pub fn id(&self) -> &VariationId {
        &self.id
    }

    pub fn display_label(&self) -> &str {
        self.metadata.label.as_deref().unwrap_or(&self.name)
    }
}

/// Host-provided display metadata for a variation.
///
/// Draftline persists this metadata alongside the variation but does not use it
/// to name Git refs or enforce product-specific uniqueness rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariationMetadata {
    /// Optional user-facing name. When omitted, [`Variation::display_label`]
    /// falls back to the variation's stored name.
    pub label: Option<String>,
    /// Optional host-owned slug for URLs, routing, or app integration.
    ///
    /// This is stored as display metadata only; it is not derived from the
    /// variation name and does not affect the underlying Git branch name.
    pub slug: Option<String>,
}

impl VariationMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = normalize_optional_metadata(label.into());
        self
    }

    pub fn with_slug(mut self, slug: impl Into<String>) -> Self {
        self.slug = normalize_optional_metadata(slug.into());
        self
    }
}

/// Identifier for a variation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VariationId(String);

impl VariationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VariationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for VariationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VariationId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// A changed file in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    #[serde(serialize_with = "serialize_workspace_path")]
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub is_binary: bool,
    pub is_large: bool,
}

/// High-level kind of file change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Conflicted,
    TypeChanged,
}

/// A content-workflow view of workspace changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub files: Vec<ChangedFile>,
    pub diff: Option<String>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Policy for switching variations when unsaved work exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchPolicy {
    AbortIfDirty,
    SaveFirst { label: String },
    Shelve { name: String },
    Discard,
}

/// Dry-run report for a risky operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    pub operation: String,
    pub will_write_files: bool,
    pub dirty_files: Vec<ChangedFile>,
    pub file_hazards: Vec<FileHazard>,
    pub untracked_assets: Vec<PathBuf>,
    pub unresolved_conflicts: Vec<PathBuf>,
    pub large_files: Vec<PathBuf>,
    pub binary_files: Vec<PathBuf>,
    pub variation_divergence: Option<String>,
    pub can_proceed: bool,
}

/// A local file hazard that would make writing a target tree unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileHazard {
    #[serde(serialize_with = "serialize_workspace_path")]
    pub path: PathBuf,
    pub kind: FileHazardKind,
}

/// Why a local path is hazardous for a target-tree write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileHazardKind {
    Ignored,
    Untracked,
    PolicyExcluded,
    /// Needs a Git filter driver (for example Git LFS) that Draftline cannot run.
    GitFilter,
    /// A developer's file outside the folder needs a Git filter
    /// driver; someone with the driver must make this update with Git.
    GitFilterOutsideProject,
    /// Has uncommitted local edits that the operation would overwrite.
    LocalChanges,
}

/// A file in the folder that Git stores through a filter driver such as Git LFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FilteredFile {
    #[serde(serialize_with = "serialize_workspace_path")]
    pub path: PathBuf,
    /// The `filter` attribute's driver, for example `lfs`.
    pub driver: String,
    /// Git config defines the driver (`filter.<driver>.*`), so plain Git can
    /// run it here.
    pub driver_configured: bool,
    /// Draftline runs the driver here (it is configured and the `git`
    /// executable is available), so saves, checkouts, and pushes handle this
    /// file; otherwise they refuse with a `GitFilter` hazard.
    #[serde(default)]
    pub driver_runs: bool,
    /// The file on disk is a Git LFS pointer: its content was never downloaded.
    pub content_missing: bool,
}

/// Read-only view of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPreview {
    pub id: VersionId,
    pub files: Vec<PreviewFile>,
}

/// File content from a read-only version preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewFile {
    #[serde(serialize_with = "serialize_workspace_path")]
    pub path: PathBuf,
    pub content: Option<String>,
    pub is_binary: bool,
}

/// Current live workspace content for one tracked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentFilePreview {
    #[serde(serialize_with = "serialize_workspace_path")]
    pub path: PathBuf,
    pub content: Option<String>,
    pub is_binary: bool,
}

/// Comprehensive UI snapshot returned by [`Workspace::workspace_summary`].
///
/// Collects all state a host UI needs to render the workspace panel — active
/// variation, version history, pending changes, and any interrupted-operation
/// context — in a single, allocation-bounded call.
///
/// When [`recovery`](WorkspaceSummary::recovery) is `Some`, the workspace may
/// be mid-operation.  Check
/// [`state_may_be_inconsistent`](WorkspaceSummary::state_may_be_inconsistent)
/// before trusting the `versions` / `dirty_files` snapshot; render a recovery
/// prompt instead of a normal history view in that case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    /// The variation that is currently checked out.
    pub active_variation: Variation,
    /// All local variations, sorted by name.
    pub variations: Vec<Variation>,
    /// Versions reachable from the current variation, newest first.
    pub versions: Vec<Version>,
    /// Files with unsaved changes in the workspace.
    pub dirty_files: Vec<ChangedFile>,
    /// `true` when `dirty_files` is non-empty.
    pub is_dirty: bool,
    /// Incomplete operation state if a prior Draftline operation was interrupted.
    pub recovery: Option<crate::RecoveryState>,
    /// `true` when a pending recovery means `versions` and `dirty_files` may
    /// describe two different Git states simultaneously and should not be
    /// rendered as a coherent history view.
    pub state_may_be_inconsistent: bool,
    /// History cleanups that Draftline 0.5 applied locally but never
    /// published. Dismiss each with [`Workspace::dismiss_legacy_history_cleanup`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_history_cleanups: Vec<LegacyHistoryCleanup>,
}

/// A history cleanup (squash) that Draftline 0.5 applied to a local branch
/// and recorded as pending publication. Current Draftline never moves a
/// branch back, so the record is only reported and dismissed; the original
/// history stays in the backup refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LegacyHistoryCleanup {
    pub plan_id: String,
    pub variation: String,
    /// The branch tip before the cleanup rewrote it.
    pub original_head: String,
    /// The rewritten tip the cleanup left on the branch.
    pub replacement_head: String,
    /// Backup refs holding the original history. They stay listed as support
    /// refs, so [`Workspace::restore_support_ref_as_variation`] can bring it
    /// back as a variation.
    pub backup_refs: Vec<String>,
}

#[derive(Deserialize)]
struct LegacyPendingCleanupRecord {
    plan_id: String,
    target_variation: String,
    expected_local_head: String,
    replacement_head: String,
    #[serde(default)]
    backup_refs: Vec<String>,
}

/// Stable workspace identity returned by [`Workspace::inspect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceId {
    pub root: PathBuf,
}

/// Whether the workspace currently has a configured sharing destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SharingMode {
    LocalOnly,
    SharedCapable,
}

/// Machine-readable next action a host or agent can safely offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SafeNextAction {
    NormalWork,
    SaveFirst,
    DiscardChanges,
    RepairRecovery,
    ConfigureRemote,
    /// Another folder in this checkout must finish its interrupted operation.
    FinishInOtherFolder,
}

/// Stable diagnostic codes for workspace inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticCode {
    RecoveryRequired,
    RecoveryInOtherFolder,
    WorkspaceLocked,
    DirtyWorkspace,
    LocalOnlyWorkspace,
    SharedCapableWorkspace,
    NoCurrentVariation,
    WorkspaceReadFailed,
    PolicyTrackedFileIgnored,
    /// The folder's files that Git stores with LFS hold pointer text instead of
    /// their content, because the clone never downloaded it.
    LfsContentMissing,
}

/// Severity for a workspace diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Blocking,
}

/// Human and machine readable workspace inspection diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceDiagnostic {
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// Dirty-state summary returned by [`Workspace::inspect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirtySummary {
    pub is_dirty: bool,
    pub files: Vec<ChangedFile>,
}

/// Operation-lock state visible to inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OperationLockState {
    Unlocked,
    Locked,
}

/// Summary of the current operation lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OperationLockSummary {
    pub state: OperationLockState,
}

/// Metadata written into the operation lock while a risky mutation is active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OperationLockMetadata {
    pub operation_id: String,
    pub operation: String,
    pub process_id: u32,
    pub owner: Option<String>,
    pub created_at_seconds: u64,
    /// Machine that took the lock; absent in locks from older versions.
    #[serde(default)]
    pub host: Option<String>,
}

/// Read-only report for the current operation lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OperationLockInspection {
    pub state: OperationLockState,
    pub metadata: Option<OperationLockMetadata>,
    pub is_stale: bool,
    pub can_clear: bool,
    pub diagnostics: Vec<WorkspaceDiagnostic>,
}

/// The checkout's own record of the folder Draftline manages, relative to the
/// repository root (`""` for the root itself). [`FOLDER_MARKER`] wins when the
/// working tree has one; this covers branches without it.
#[derive(Serialize, Deserialize)]
struct WorkspaceClaim {
    project: String,
}

/// The file at the repository root that names the folder Draftline manages.
/// It is committed with saves, so every clone and branch agrees.
const FOLDER_MARKER: &str = ".draftline";

/// Files that describe the repository rather than a folder in it. They stay
/// at the root when a root folder moves into a subfolder.
const REPOSITORY_FILES: [&str; 5] = [
    FOLDER_MARKER,
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".github",
];

/// Whether `path`'s first component is one of [`REPOSITORY_FILES`].
fn is_repository_file(path: &[u8]) -> bool {
    let first = path.split(|byte| *byte == b'/').next().unwrap_or_default();
    REPOSITORY_FILES.iter().any(|name| name.as_bytes() == first)
}

/// [`is_repository_file`] for a repository-relative path.
fn is_repository_path(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|first| is_repository_file(first.as_os_str().as_encoded_bytes()))
}

/// The trailer on a folder move's commit naming where the folder was.
const MOVED_FROM_TRAILER: &str = "Draftline-Moved-From";

/// The contents of [`FOLDER_MARKER`]: `{ "format": 1, "folder": "cutready" }`,
/// with `"."` for the whole repository.
#[derive(Serialize, Deserialize)]
struct FolderMarker {
    format: u32,
    folder: String,
}

/// Who manages the repository, as the marker or local record says.
#[derive(PartialEq)]
enum FolderOwner {
    Nobody,
    Folder(String),
    Invalid(String),
    Newer(String),
}

/// Where a folder stands before the host opens it; see [`Workspace::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FolderState {
    /// The path isn't inside a Git repository.
    NotARepository,
    /// No folder in the repository is managed yet.
    Unmanaged,
    /// Draftline manages this folder; open it.
    Ready,
    /// Draftline manages another folder in this repository.
    ManagedElsewhere,
    /// The folder Draftline manages isn't on disk.
    FolderMissing,
    /// `.draftline`, or the checkout's local record of it, can't be read.
    InvalidMarker,
    /// `.draftline` was written by a newer Draftline.
    NewerFormat,
}

/// A fix the host can offer for a probed folder. Pass it back unchanged to
/// [`Workspace::apply_folder_fix`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FolderFix {
    /// Open the requested folder.
    Open,
    /// Make the requested folder the one Draftline manages, creating it (and
    /// a repository) when needed.
    ManageHere,
    /// Open the folder Draftline already manages.
    OpenManagedFolder { folder: PathBuf },
    /// Manage `folder` instead: a commit renamed the managed folder there.
    ManageRenamedFolder { folder: PathBuf },
    /// Move the managed folder to the requested path, as a save.
    MoveFolderHere { from: PathBuf },
    /// Put the managed folder back from the current version.
    RestoreFolder { folder: PathBuf },
    /// Switch the checkout to a local branch that has the managed folder.
    SwitchToBranch { branch: String },
    /// Only a newer Draftline can read this repository.
    UpgradeDraftline,
}

/// One fix with a plain-language label for a button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSuggestion {
    #[serde(flatten)]
    pub fix: FolderFix,
    pub label: String,
}

/// What [`Workspace::probe`] found. Folders are relative to `repository`,
/// empty for the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FolderProbe {
    pub state: FolderState,
    pub requested: PathBuf,
    pub repository: Option<PathBuf>,
    /// The folder Draftline manages, when the repository names one.
    pub folder: Option<PathBuf>,
    /// Why the folder isn't ready, in plain language.
    pub detail: Option<String>,
    /// Fixes in recommended order.
    pub suggestions: Vec<FolderSuggestion>,
}

/// Result of [`Workspace::move_folder`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FolderMove {
    pub from: PathBuf,
    pub to: PathBuf,
    /// The save that records the move.
    pub version: Version,
    /// Untracked or ignored files that stayed put: the new folder already had
    /// a file at that path, or the old folder was the repository root.
    pub left_behind: Vec<PathBuf>,
}

/// A remote's default branch as it last advertised it, and the one it
/// replaced. Plain fetches never refresh `refs/remotes/<remote>/HEAD`, and
/// Draftline doesn't write it, so a renamed default lives here instead.
#[derive(Clone, Serialize, Deserialize)]
struct RemoteHead {
    /// The remote's URL when recorded; a changed URL voids the entry.
    url: String,
    head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<String>,
}

/// The recovery file on disk: the state plus the folder that wrote it.
/// Records from older versions have no folder.
#[derive(Serialize, Deserialize)]
struct RecoveryLedger {
    #[serde(flatten)]
    state: RecoveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    /// Trees an interrupted checkout was moving between, so recovery works
    /// from what was actually written even if branches move afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<RecordedCheckout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedCheckout {
    from_tree: String,
    to_tree: String,
}

/// Result returned by recovery repair and rollback entry points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryRepairResult {
    pub operation_id: String,
    pub operation: RecoveryOperation,
    pub completed: bool,
    /// True when repair or rollback changed files or refs beyond the recovery ledger.
    pub changed_workspace: bool,
    pub safe_next_actions: Vec<SafeNextAction>,
}

/// Summary of hidden Draftline support refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefSummary {
    pub local_count: usize,
    pub remote_count: usize,
}

/// Scope for listing Draftline support refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SupportRefScope {
    Local,
    RemoteTracking,
}

/// Kind of hidden Draftline recovery support ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SupportRefKind {
    DeletedVariation,
    Rewrite,
    HistoryCleanupBackup,
}

/// Hidden support ref that can be used for recovery/admin workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRef {
    pub id: String,
    pub ref_name: String,
    pub kind: SupportRefKind,
    pub source_variation: Option<String>,
    pub target_oid: String,
    pub scope: SupportRefScope,
}

/// One support ref captured for create-only publishing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefPublishItem {
    pub ref_name: String,
    pub target_oid: String,
}

/// Read-only plan for publishing hidden support refs to a shared remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefPublishPreflight {
    pub remote: String,
    pub support_refs: Vec<SupportRef>,
    pub token: SupportRefPublishToken,
    pub can_publish: bool,
}

/// Opaque token tying support-ref publication to a preflighted local state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefPublishToken {
    pub remote: String,
    pub refs: Vec<SupportRefPublishItem>,
}

/// Read-only plan for restoring a hidden support ref as a visible variation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefRestorePreflight {
    pub support_ref: SupportRef,
    pub target_variation: VariationId,
    pub token: SupportRefRestoreToken,
    pub can_restore: bool,
}

/// Token tying support-ref restoration to a preflighted support ref target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefRestoreToken {
    pub support_ref_id: String,
    pub target_oid: String,
    pub target_variation: VariationId,
}

/// Structured, read-only safety snapshot for hosts and agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceInspection {
    pub workspace_id: WorkspaceId,
    pub sharing_mode: SharingMode,
    pub current_variation: Option<VariationId>,
    pub remotes: Vec<RemoteEndpoint>,
    pub dirty: DirtySummary,
    pub recovery: Option<crate::RecoveryState>,
    pub operation_lock: OperationLockSummary,
    pub support_refs: SupportRefSummary,
    pub diagnostics: Vec<WorkspaceDiagnostic>,
    pub safe_next_actions: Vec<SafeNextAction>,
}

/// Feature flags for the workflows supported by this Draftline crate version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceCapabilities {
    pub inspect: bool,
    pub workspace_summary: bool,
    pub save_version: bool,
    pub switch_variation: bool,
    pub publish_changes: bool,
    pub apply_incoming: bool,
    /// Stale lock inspection and guarded lock clearing are available.
    ///
    /// Operation-specific repair and rollback still return skeleton recovery
    /// reports and do not mutate workspace state.
    pub stale_lock_repair: bool,
    /// File-writing operations perform target-tree collision checks where wired.
    ///
    /// Current coverage includes switch, restore, apply incoming, and shelf
    /// apply. This does not imply every future checkout-like operation is wired.
    pub target_tree_collision_preflight: bool,
    pub support_ref_sync: bool,
    /// A standalone CLI/tool facade is available when this flag is `true`.
    ///
    /// Rust JSON helpers exist separately as `inspect_json`,
    /// `capabilities_json`, and `verify_workspace`; they are not a standalone
    /// facade.
    pub agent_cli_facade: bool,
}

/// Retry guidance for agent/tool callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RetryClass {
    Retryable,
    RetryAfterRepair,
    RetryAfterUserChoice,
    NotRetryable,
}

/// Stable explanation for a diagnostic/error code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ErrorExplanation {
    pub code: DiagnosticCode,
    pub message: String,
    pub safe_next_actions: Vec<SafeNextAction>,
    pub retry: RetryClass,
}

/// Verification summary for workspace postconditions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceVerification {
    pub recovery_clear: bool,
    pub operation_lock_clear: bool,
    pub current_variation_present: bool,
    pub diagnostics: Vec<WorkspaceDiagnostic>,
}

/// Content-policy audit report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContentPolicyAudit {
    pub current_diagnostics: Vec<WorkspaceDiagnostic>,
    pub historical_out_of_policy_paths: Vec<PathBuf>,
}

/// Read-only setup report for adopting an existing Git repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdoptionPreflightReport {
    pub inspection: WorkspaceInspection,
    pub candidate_policy_diagnostics: Vec<WorkspaceDiagnostic>,
    pub blockers: Vec<WorkspaceDiagnostic>,
    pub warnings: Vec<WorkspaceDiagnostic>,
    pub safe_next_actions: Vec<SafeNextAction>,
    pub can_adopt: bool,
}

/// Identifier for one node in a workspace graph snapshot.
///
/// This is intentionally opaque to callers. Use [`WorkspaceGraphNode::version`]
/// when an action needs the underlying saved version ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceGraphNodeId(String);

impl WorkspaceGraphNodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkspaceGraphNodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<Oid> for WorkspaceGraphNodeId {
    fn from(value: Oid) -> Self {
        Self(format!("node-{value}"))
    }
}

/// Read-only action a host may offer for a graph node or ref.
///
/// Mutation actions are hints only. Hosts must still use the corresponding
/// Draftline preflight/execute operation instead of moving refs or the working
/// directory directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceGraphAction {
    Preview,
    CompareToCurrent,
    RestoreAsNewSave,
    CreateVariationFromHere,
    SwitchToVariation,
    AdoptRemoteVariation,
    RestoreSupportRefAsVariation,
}

/// UI-safe details for enabling or explaining a graph action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphActionHint {
    pub action: WorkspaceGraphAction,
    pub enabled: bool,
    pub command: String,
    pub disabled_reason: Option<String>,
    pub destructive: bool,
    pub switches_workspace: bool,
    pub creates_version: bool,
}

/// Why a graph node is present in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceGraphNodeKind {
    Normal,
    RemoteOnly,
    SupportRefOnly,
}

/// Stable graph layout hints for host renderers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphLayoutHint {
    pub lane: usize,
    pub row: usize,
    pub group: Option<String>,
    pub display_label: String,
}

/// Boundary metadata for graph slices that omit related nodes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphBoundary {
    pub missing_parent_ids: Vec<WorkspaceGraphNodeId>,
    pub missing_child_ids: Vec<WorkspaceGraphNodeId>,
    pub hidden_parent_count: usize,
    pub hidden_child_count: usize,
}

/// A graph-ready commit node. In a folder, `touches_folder` is false
/// for teammates' commits that don't change the folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphNode {
    pub id: WorkspaceGraphNodeId,
    pub version: Version,
    /// Parent graph node IDs for DAG rendering.
    ///
    /// When pagination or focused graph slices are active, a parent ID can point
    /// to a node outside the current `nodes` response. Merge pages, expand the
    /// slice, or fetch around that parent before assuming it is missing.
    pub parent_ids: Vec<WorkspaceGraphNodeId>,
    pub parent_version_ids: Vec<VersionId>,
    pub variation_tips: Vec<VariationId>,
    /// Whether this commit changes the folder; see
    /// [`HistoryEntry::touches_folder`].
    #[serde(default = "touches_folder_by_default")]
    pub touches_folder: bool,
    pub is_head: bool,
    pub is_current: bool,
    pub is_tip: bool,
    pub is_merge: bool,
    pub is_branch_point: bool,
    pub child_ids: Vec<WorkspaceGraphNodeId>,
    pub child_count: usize,
    pub kind: WorkspaceGraphNodeKind,
    pub reachable_from_local_variation: bool,
    pub reachable_from_remote_variation: bool,
    pub reachable_from_support_ref: bool,
    pub topo_index: usize,
    pub layout: WorkspaceGraphLayoutHint,
    pub boundary: WorkspaceGraphBoundary,
    pub available_actions: Vec<WorkspaceGraphAction>,
    pub action_hints: Vec<WorkspaceGraphActionHint>,
}

/// Scope of a ref rendered on top of the workspace graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceGraphRefScope {
    Local,
    RemoteTracking,
}

/// Kind of ref rendered on top of the workspace graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceGraphRefKind {
    LocalVariation,
    RemoteVariation,
    SupportRef,
}

/// Ref/tip metadata for a workspace graph snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphRef {
    pub id: String,
    pub name: String,
    pub display_label: String,
    pub kind: WorkspaceGraphRefKind,
    pub scope: WorkspaceGraphRefScope,
    pub target: WorkspaceGraphNodeId,
    pub target_version: VersionId,
    pub remote: Option<String>,
    pub variation: Option<VariationId>,
    pub metadata: Option<VariationMetadata>,
    pub support_ref_kind: Option<SupportRefKind>,
    pub group: Option<String>,
    pub is_current: bool,
    pub is_user_facing: bool,
    pub available_actions: Vec<WorkspaceGraphAction>,
    pub action_hints: Vec<WorkspaceGraphActionHint>,
}

/// Options for constructing a graph-ready workspace history snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphOptions {
    #[serde(default)]
    pub include_remotes: bool,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub include_support_refs: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<usize>,
}

/// Options for constructing a compressed graph overview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphOverviewOptions {
    #[serde(flatten)]
    pub graph: WorkspaceGraphOptions,
    /// Maximum nodes to keep in the overview after preserving graph landmarks.
    #[serde(default = "default_workspace_graph_overview_max_nodes")]
    pub max_nodes: usize,
    /// Number of recent topological nodes to preserve before landmark pruning.
    #[serde(default = "default_workspace_graph_overview_recent_nodes")]
    pub recent_nodes: usize,
}

impl Default for WorkspaceGraphOverviewOptions {
    fn default() -> Self {
        Self {
            graph: WorkspaceGraphOptions::default(),
            max_nodes: default_workspace_graph_overview_max_nodes(),
            recent_nodes: default_workspace_graph_overview_recent_nodes(),
        }
    }
}

/// Ref-only graph surface for cheap tip overlays and navigation labels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphRefs {
    pub workspace_id: WorkspaceId,
    pub current_variation: Option<VariationId>,
    pub current_version: Option<VersionId>,
    pub dirty: DirtySummary,
    pub recovery: Option<crate::RecoveryState>,
    pub state_may_be_inconsistent: bool,
    pub refs: Vec<WorkspaceGraphRef>,
    pub graph_fingerprint: String,
}

/// Counts and health signals for choosing graph rendering strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphSummary {
    pub workspace_id: WorkspaceId,
    pub current_variation: Option<VariationId>,
    pub current_version: Option<VersionId>,
    pub dirty: DirtySummary,
    pub recovery: Option<crate::RecoveryState>,
    pub state_may_be_inconsistent: bool,
    pub total_nodes: usize,
    pub normal_nodes: usize,
    pub remote_only_nodes: usize,
    pub support_ref_only_nodes: usize,
    pub merge_nodes: usize,
    pub branch_points: usize,
    pub local_ref_count: usize,
    pub remote_ref_count: usize,
    pub support_ref_count: usize,
    pub graph_fingerprint: String,
}

/// Agent-oriented graph summary for deciding safe navigation and next commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphAgentSummary {
    pub summary: WorkspaceGraphSummary,
    pub suggested_next_commands: Vec<String>,
    pub warnings: Vec<String>,
    pub current_ref: Option<WorkspaceGraphRef>,
    pub nearby_refs: Vec<WorkspaceGraphRef>,
}

/// Result for graph search queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphSearchResult {
    pub graph: WorkspaceGraph,
    pub matched_refs: Vec<WorkspaceGraphRef>,
    pub query: String,
    pub matched_node_count: usize,
    pub total_matches: usize,
}

/// Path between two graph nodes for compare and ancestry explanations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphPath {
    pub from_version: VersionId,
    pub to_version: VersionId,
    pub node_ids: Vec<WorkspaceGraphNodeId>,
    pub version_ids: Vec<VersionId>,
    pub common_ancestor: Option<VersionId>,
    pub found: bool,
}

/// Common ancestor result for graph compare/merge UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphCommonAncestor {
    pub left_version: VersionId,
    pub right_version: VersionId,
    pub common_ancestor: Option<VersionId>,
}

/// Lightweight node details for inspectors and hover cards.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphNodeDetail {
    pub node: WorkspaceGraphNode,
    pub refs: Vec<WorkspaceGraphRef>,
    pub changed_file_count: Option<usize>,
    pub changed_files: Vec<ChangedFile>,
}

/// Compare summary without requiring consumers to infer safe graph defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraphCompareSummary {
    pub from_version: VersionId,
    pub to_version: VersionId,
    pub changed_file_count: usize,
    pub files: Vec<ChangedFile>,
    pub action_hints: Vec<WorkspaceGraphActionHint>,
    pub common_ancestor: Option<VersionId>,
}

/// Full-history DAG view over existing Draftline variation semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceGraph {
    pub workspace_id: WorkspaceId,
    pub current_variation: Option<VariationId>,
    pub current_version: Option<VersionId>,
    pub dirty: DirtySummary,
    pub recovery: Option<crate::RecoveryState>,
    pub state_may_be_inconsistent: bool,
    pub nodes: Vec<WorkspaceGraphNode>,
    pub refs: Vec<WorkspaceGraphRef>,
    /// Page-scoped identity for this graph response.
    ///
    /// With pagination, different pages from the same unchanged workspace have
    /// different snapshot IDs because the hash includes the returned nodes.
    pub snapshot_id: String,
    /// True when the response intentionally omitted graph nodes.
    ///
    /// For paginated graphs, use `next_cursor` to fetch more. For focused or
    /// overview graphs, re-query with a wider focus/overview if needed.
    pub was_pruned: bool,
    pub has_more: bool,
    pub next_cursor: Option<usize>,
}

/// A version annotated with variation-tip context for timeline/graph rendering.
///
/// Host UIs can render a simple history list or a branch graph by iterating
/// [`HistoryEntry`] values returned from [`Workspace::history`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The version at this position in the history walk.
    pub version: Version,
    /// Identifiers of any variations whose tip commit is this exact version.
    ///
    /// A non-empty slice means this version is the current tip of one or more
    /// variations and can be used as a branch-point indicator in a graph UI.
    pub variation_tips: Vec<VariationId>,
    /// `true` when this version is the current `HEAD` of the active variation.
    pub is_head: bool,
    /// Identifiers of the parent version(s) of this version.
    ///
    /// Most versions have exactly one parent.  The initial version has no
    /// parents.  Merge commits have multiple parents, but Draftline
    /// discourages merge commits in favour of sequential saves.
    pub parent_ids: Vec<VersionId>,
    /// `true` when this commit changes the folder. In a folder
    /// inside a larger repository, teammates' commits that only touch other
    /// files are listed too, with `false`; standalone workspaces are always
    /// `true`.
    #[serde(default = "touches_folder_by_default")]
    pub touches_folder: bool,
}

fn touches_folder_by_default() -> bool {
    true
}

/// Per-variation snapshot with head version and total version count.
///
/// Returned by [`Workspace::variation_summaries`].  Provides the information
/// a host UI needs to render a variation picker without switching variations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariationSummary {
    /// The variation this summary describes.
    pub variation: Variation,
    /// The tip (newest) version of this variation, or `None` for an empty workspace.
    pub head_version: Option<Version>,
    /// Number of commits reachable from this variation's tip, including all
    /// shared ancestor history.  This is **not** the number of commits
    /// exclusive to this variation — shared ancestry is counted for every
    /// variation that can reach those commits.  Use this for a total-depth
    /// indicator; do not label it "commits on this branch."
    pub reachable_version_count: usize,
}

/// Target variation for restoring a saved version as a new save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum RestoreVersionTarget {
    /// Restore onto the currently active variation.
    Current,
    /// Restore onto an existing local variation and activate it after the save is ready.
    Existing { variation: VariationId },
    /// Create a new local variation, restore onto it, and activate it after the save is ready.
    New {
        name: String,
        #[serde(default)]
        metadata: VariationMetadata,
    },
}

/// Result of restoring a version onto a selected target variation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TargetedRestoreVersionResult {
    pub version: Version,
    pub target_variation: Variation,
}

/// A variation discovered on a remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteVariation {
    pub id: VariationId,
    pub name: String,
    pub remote: String,
    pub head_version: Option<Version>,
}

/// Diagnostic comparison between local variations and fetched remote variations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteVariationDiagnostics {
    pub remote: String,
    pub shared_variations: Vec<VariationId>,
    pub local_only_variations: Vec<VariationId>,
    pub remote_only_variations: Vec<VariationId>,
}

/// Remote-aware preflight for creating a new local variation from a saved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VariationCreatePreflight {
    pub from_version: VersionId,
    pub variation: VariationId,
    pub remote: Option<String>,
    pub can_create: bool,
    pub local_collision: bool,
    pub remote_collision: bool,
    pub remote_only_collision: bool,
    pub existing_remote_head: Option<Version>,
    pub suggested_alternative: Option<String>,
    pub token: Option<VariationCreateToken>,
}

/// Token tying variation creation to a preflighted local version and remote-tracking state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VariationCreateToken {
    pub operation_id: String,
    pub from_version: VersionId,
    pub variation: VariationId,
    pub remote: Option<String>,
    pub expected_source_oid: String,
    pub expected_remote_oid: Option<String>,
}

/// Preflight report for applying incoming changes from a remote.
///
/// Returned by [`Workspace::preflight_apply_incoming`].  Lets host UIs show
/// a "safe to apply" indicator before committing to the apply operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyIncomingReport {
    /// Sync status at the time of the preflight check.
    pub sync_status: SyncStatus,
    /// Files with unsaved changes that would block the apply.
    pub dirty_files: Vec<ChangedFile>,
    /// Local files that the incoming target tree would overwrite unsafely.
    pub file_hazards: Vec<FileHazard>,
    /// `true` when the apply can be done as a fast-forward (no three-way merge needed).
    pub is_fast_forward: bool,
    /// `true` when it is safe to call [`Workspace::apply_incoming`] immediately.
    pub can_proceed: bool,
}

/// Result of a successful [`Workspace::apply_incoming`] call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyIncomingResult {
    /// Number of versions fast-forwarded into the local variation.
    pub applied_count: usize,
}

/// A local shelf of work intentionally put aside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Shelf {
    pub id: String,
    pub version: Version,
}

/// Preflight report for applying a shelf into the current workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ShelfApplyReport {
    pub shelf: Shelf,
    pub dirty_files: Vec<ChangedFile>,
    pub file_hazards: Vec<FileHazard>,
    pub can_proceed: bool,
}

/// Read-only merge-incoming preflight report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MergeIncomingReport {
    pub sync_status: SyncStatus,
    pub dirty_files: Vec<ChangedFile>,
    pub file_hazards: Vec<FileHazard>,
    pub conflicts: Vec<MergeConflict>,
    /// Conflicts outside the content policy (for example code); a developer
    /// has to merge these, so no token is issued.
    #[serde(default)]
    pub developer_conflicts: Vec<PathBuf>,
    pub token: Option<MergeIncomingToken>,
    pub can_merge_cleanly: bool,
    pub changed_workspace: bool,
}

/// Opaque token tying merge execution to a preflighted local/remote/base state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MergeIncomingToken {
    pub remote: String,
    pub variation: String,
    pub local_oid: String,
    pub remote_oid: String,
    pub merge_base_oid: String,
    /// Branch merged in when it is not the variation's own upstream (the base
    /// branch, from [`Workspace::preflight_merge_base`]).
    #[serde(default)]
    pub source: Option<String>,
}

/// Explicit user choice for resolving one merge conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum MergeResolutionChoice {
    UseOurs,
    UseTheirs,
    UseBase,
    Delete,
    UseContent { content: String },
}

/// User-provided resolution for a conflict returned by [`MergeIncomingReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MergeConflictResolution {
    pub path: PathBuf,
    pub field_path: Option<String>,
    pub choice: MergeResolutionChoice,
}

impl MergeConflictResolution {
    /// Creates an explicit resolution for a whole-file merge conflict.
    pub fn new(path: impl Into<PathBuf>, choice: MergeResolutionChoice) -> Self {
        Self {
            path: path.into(),
            field_path: None,
            choice,
        }
    }

    /// Creates an explicit resolution for a semantic conflict at a field path.
    pub fn with_field_path(
        path: impl Into<PathBuf>,
        field_path: impl Into<String>,
        choice: MergeResolutionChoice,
    ) -> Self {
        Self {
            path: path.into(),
            field_path: Some(field_path.into()),
            choice,
        }
    }
}

/// Result of writing a clean incoming merge as a new version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MergeIncomingResult {
    pub version: Version,
    pub merged_files: Vec<PathBuf>,
}

struct IncomingMergeInput<'repo> {
    local_commit: Commit<'repo>,
    remote_commit: Commit<'repo>,
    base_commit: Commit<'repo>,
}

struct CleanMergePlan {
    files: Vec<MergeFileChange>,
    conflicts: Vec<MergeConflict>,
}

struct SendTarget {
    base: String,
    advertised: HashSet<String>,
    /// Commits the remote advertised just now: what a push won't upload.
    advertised_oids: Vec<git2::Oid>,
    current: String,
}

/// The branch and commit Send checked, which publishing must still find.
struct SendGuard<'a> {
    head: git2::Oid,
    variation: &'a str,
}

struct MergeFileChange {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

/// Preflight for removing a visible variation from a shared remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteVariationDeletePreflight {
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub support_ref: String,
    pub token: RemoteVariationDeleteToken,
    pub can_delete: bool,
}

/// Token tying remote variation deletion to a preflighted remote state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteVariationDeleteToken {
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub support_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteVariationDeleteRecoveryMetadata {
    remote: String,
    variation: String,
    expected_remote_oid: String,
    support_ref: String,
}

/// Preflight for deleting a local variation after archiving its tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VariationDeletePreflight {
    pub variation: VariationId,
    pub target_oid: String,
    pub support_ref: String,
    pub token: VariationDeleteToken,
    pub can_delete: bool,
}

/// Token tying local variation deletion to a preflighted branch tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VariationDeleteToken {
    pub operation_id: String,
    pub variation: VariationId,
    pub expected_oid: String,
    pub support_ref: String,
}

/// Preflight for renaming a visible local variation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VariationRenamePreflight {
    pub source_variation: VariationId,
    pub target_variation: VariationId,
    pub expected_oid: String,
    pub support_ref: String,
    pub token: VariationRenameToken,
    pub can_rename: bool,
}

/// Token tying a local variation rename to a preflighted branch tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VariationRenameToken {
    pub operation_id: String,
    pub source_variation: VariationId,
    pub target_variation: VariationId,
    pub expected_oid: String,
    pub support_ref: String,
}

/// Preflight for expiring support refs as retention cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefExpirationPreflight {
    pub support_refs: Vec<SupportRef>,
    pub token: SupportRefExpirationToken,
    pub can_expire: bool,
}

/// Token tying support-ref expiration to selected support refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SupportRefExpirationToken {
    pub ids: Vec<String>,
}

/// Preflight for destructive purge/redaction planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PurgePreflight {
    pub selector: String,
    pub affected_refs: Vec<String>,
    pub distributed_warning: String,
    pub token: PurgeToken,
}

/// Token identifying a purge plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PurgeToken {
    pub selector: String,
    pub affected_refs: Vec<String>,
}

/// Verification summary for a purge plan or execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PurgeVerification {
    pub selector: String,
    pub checked_refs: usize,
    pub verified_absent: bool,
    pub limitations: Vec<String>,
}

/// Diff between two versions or between a version and the current workspace.
///
/// Returned by [`Workspace::diff_versions`] and
/// [`Workspace::diff_version_to_workspace`].  When `to_version` is `None`
/// the diff is against the live workspace files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDiff {
    /// Base version for this diff.
    pub from_version: Option<VersionId>,
    /// Target version, or `None` when diffing against live workspace files.
    pub to_version: Option<VersionId>,
    /// Files that changed between the two points, sorted by path.
    pub files: Vec<ChangedFile>,
    /// Unified diff patch text, or `None` when there are no text changes.
    pub patch: Option<String>,
}

/// Diff and live preview content for one tracked workspace file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentFileDiff {
    pub path: PathBuf,
    pub file: Option<ChangedFile>,
    pub patch: Option<String>,
    pub preview: Option<CurrentFilePreview>,
}

impl Workspace {
    /// Initializes a new workspace or opens the existing workspace at `path`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_policy(path, ContentPolicy::default())
    }

    /// Initializes a workspace with an explicit content policy.
    ///
    /// When `path` is inside an existing repository (for example a project
    /// folder in a code repository, or a linked worktree), Draftline joins that
    /// repository and scopes itself to the folder instead of creating a nested
    /// repository.
    pub fn init_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        let path = absolute_path(path.as_ref())?;
        let path = path.as_path();
        ensure_plain_or_missing(path)?;
        let anchor = path
            .ancestors()
            .find(|ancestor| ancestor.exists())
            .unwrap_or(path);
        let mut created = Vec::new();
        let result = match Repository::discover(anchor) {
            // Inside a repository, create the folder only within it, never
            // through a file, symlink, or junction.
            Ok(repo) => match repo.workdir().map(Path::to_path_buf) {
                Some(workdir) => folder_in(&workdir, path)
                    .and_then(|folder| create_folder_tracking(&workdir, &folder, &mut created))
                    .and_then(|_| Self::from_repository(repo, path, content_policy)),
                None => create_missing_dirs(path)
                    .map_err(DraftlineError::from)
                    .and_then(|dirs| {
                        created = dirs;
                        Self::from_repository(repo, path, content_policy)
                    }),
            },
            Err(_) => create_missing_dirs(path)
                .map_err(DraftlineError::from)
                .and_then(|dirs| {
                    created = dirs;
                    let initial_head = default_initial_variation_name();
                    let mut options = RepositoryInitOptions::new();
                    options.initial_head(&initial_head);
                    Ok(Repository::init_opts(path, &options)?)
                })
                .and_then(|repo| Self::from_repository(repo, path, content_policy)),
        };
        if result.is_err() {
            // Leave no trace of a refused folder; `remove_dir` only removes empty ones.
            for dir in created.iter().rev() {
                if fs::remove_dir(dir).is_err() {
                    break;
                }
            }
        }
        result
    }

    fn from_repository(
        repo: Repository,
        project_path: &Path,
        content_policy: ContentPolicy,
    ) -> Result<Self> {
        ensure_plain_or_missing(project_path)?;
        let workspace = Self::unclaimed(repo, project_path, content_policy)?;
        workspace.claim_checkout(true)?;
        Ok(workspace)
    }

    fn unclaimed(
        repo: Repository,
        project_path: &Path,
        content_policy: ContentPolicy,
    ) -> Result<Self> {
        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project_path.to_path_buf());
        let scope = project_scope(&root, project_path)?;
        let content_policy = content_policy.scoped_to(scope)?;

        Ok(Self {
            root,
            repo,
            content_policy,
            resolver_registry: ResolverRegistry::with_default_resolvers(),
            variation_namespace: None,
        }
        .initialize())
    }

    /// Opens an existing workspace.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_policy(path, ContentPolicy::default())
    }

    /// Opens an existing workspace with an explicit content policy.
    ///
    /// Opening a folder below the repository root scopes the workspace to that
    /// folder; see [`Workspace::init_with_policy`].
    pub fn open_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        let path = absolute_path(path.as_ref())?;
        if !path.exists() {
            if let Some(error) = Self::missing_folder_error(&path)? {
                return Err(error);
            }
        }
        let anchor = path
            .ancestors()
            .find(|ancestor| ancestor.exists())
            .unwrap_or(&path);
        let repo = Repository::discover(anchor)?;
        Self::from_repository(repo, &path, content_policy)
    }

    /// Why a folder that isn't on disk can't be opened, from what
    /// [`Workspace::probe`] reports, so the host can offer its fixes. `None`
    /// when it can still open, for example to finish an interrupted move.
    fn missing_folder_error(path: &Path) -> Result<Option<DraftlineError>> {
        let probe = Self::probe_in(path, None)?;
        let requested = || {
            probe
                .repository
                .as_deref()
                .and_then(|root| folder_in(root, &probe.requested).ok())
                .unwrap_or_else(|| probe.requested.clone())
        };
        Ok(match probe.state {
            FolderState::ManagedElsewhere => {
                probe.folder.clone().map(DraftlineError::FolderIsElsewhere)
            }
            FolderState::FolderMissing => Some(DraftlineError::FolderMissing(
                probe.folder.clone().unwrap_or_else(requested),
            )),
            FolderState::Unmanaged => Some(DraftlineError::FolderMissing(requested())),
            FolderState::InvalidMarker => Some(DraftlineError::InvalidFolderMarker(
                probe.detail.clone().unwrap_or_default(),
            )),
            FolderState::NewerFormat => Some(DraftlineError::NewerFolderMarker(
                probe.detail.clone().unwrap_or_default(),
            )),
            _ => None,
        })
    }

    /// Reports what opening `path` would find, without changing anything,
    /// with fixes the host can offer. Call it before opening a folder and pass
    /// the chosen fix to [`Workspace::apply_folder_fix`].
    pub fn probe(path: impl AsRef<Path>) -> Result<FolderProbe> {
        Self::probe_in(path.as_ref(), None)
    }

    /// [`Workspace::probe`] for a host that keeps its variations in
    /// `namespace` (see [`Workspace::with_variation_namespace`]): only the
    /// base branch and branches inside it are offered to switch to.
    pub fn probe_with_namespace(
        path: impl AsRef<Path>,
        namespace: impl Into<String>,
    ) -> Result<FolderProbe> {
        Self::probe_in(path.as_ref(), Some(namespace.into()))
    }

    fn probe_in(path: &Path, namespace: Option<String>) -> Result<FolderProbe> {
        let requested = absolute_path(path)?;
        match Self::located_in(&requested, ContentPolicy::default(), namespace)? {
            Some(located) => located.probe_located(requested),
            None => Ok(FolderProbe {
                state: FolderState::NotARepository,
                requested,
                repository: None,
                folder: None,
                detail: Some("This folder isn't inside a Git repository.".to_string()),
                suggestions: vec![suggest(FolderFix::ManageHere, "Start managing this folder")],
            }),
        }
    }

    /// Applies a fix [`Workspace::probe`] offers for `path` now and opens the
    /// result. A fix the probe doesn't offer is refused with
    /// [`DraftlineError::FolderFixUnavailable`].
    pub fn apply_folder_fix(
        path: impl AsRef<Path>,
        content_policy: ContentPolicy,
        fix: &FolderFix,
    ) -> Result<Self> {
        Self::apply_folder_fix_in(path.as_ref(), content_policy, None, fix)
    }

    /// [`Workspace::apply_folder_fix`] for a fix
    /// [`Workspace::probe_with_namespace`] offered. The result has the
    /// namespace set.
    pub fn apply_folder_fix_with_namespace(
        path: impl AsRef<Path>,
        content_policy: ContentPolicy,
        namespace: impl Into<String>,
        fix: &FolderFix,
    ) -> Result<Self> {
        Self::apply_folder_fix_in(path.as_ref(), content_policy, Some(namespace.into()), fix)
    }

    fn apply_folder_fix_in(
        path: &Path,
        content_policy: ContentPolicy,
        namespace: Option<String>,
        fix: &FolderFix,
    ) -> Result<Self> {
        let workspace = Self::apply_offered_fix(path, content_policy, namespace.clone(), fix)?;
        match namespace {
            Some(namespace) => workspace.with_variation_namespace(namespace),
            None => Ok(workspace),
        }
    }

    fn apply_offered_fix(
        path: &Path,
        content_policy: ContentPolicy,
        namespace: Option<String>,
        fix: &FolderFix,
    ) -> Result<Self> {
        let probe = Self::probe_in(path, namespace.clone())?;
        if !probe.suggestions.iter().any(|offered| &offered.fix == fix) {
            return Err(DraftlineError::FolderFixUnavailable(format!(
                "{} is not offered for {}",
                serde_json::to_string(fix)?,
                probe.requested.display()
            )));
        }
        let requested = probe.requested.clone();
        let root = probe.repository.clone().unwrap_or_default();
        match fix {
            FolderFix::Open => Self::open_with_policy(&requested, content_policy),
            FolderFix::ManageHere => {
                if let Some(located) = Self::located(&requested, content_policy.clone())? {
                    // Never claim a checkout while an operation is unfinished.
                    located.ensure_no_pending_recovery_in_checkout()?;
                    if probe.state != FolderState::Unmanaged {
                        // Decide and publish under the lock, so two folders
                        // can't both take over in a race.
                        let _lock = OperationLock::acquire(&located.lock_path(), "manage_here")?;
                        let now = located.probe_located(requested.clone())?;
                        if !now
                            .suggestions
                            .iter()
                            .any(|s| s.fix == FolderFix::ManageHere)
                        {
                            return Err(DraftlineError::FolderFixUnavailable(format!(
                                "managing {} is no longer offered",
                                requested.display()
                            )));
                        }
                        located.ensure_no_pending_recovery_in_checkout()?;
                        // The folder must be one Draftline can manage before
                        // anything is written.
                        create_folder_within(
                            &located.root,
                            &folder_in(&located.root, &requested)?,
                        )?;
                        // Marker first: if the claim then fails, opening
                        // repairs it from the marker.
                        located.write_marker()?;
                        located.write_claim()?;
                    }
                }
                Self::init_with_policy(&requested, content_policy)
            }
            FolderFix::OpenManagedFolder { folder } => {
                Self::open_with_policy(root.join(folder), content_policy)
            }
            FolderFix::ManageRenamedFolder { folder } => Self::apply_offered_fix(
                &root.join(folder),
                content_policy,
                namespace,
                &FolderFix::ManageHere,
            ),
            FolderFix::MoveFolderHere { from } => {
                let mut workspace = Self::open_with_policy(root.join(from), content_policy)?;
                workspace.move_folder(&requested)?;
                Ok(workspace)
            }
            FolderFix::RestoreFolder { folder } => {
                let managed =
                    Self::located_in(&root.join(folder), content_policy.clone(), namespace)?
                        .ok_or_else(|| DraftlineError::FolderMissing(folder.clone()))?;
                managed.restore_missing_folder()?;
                Self::open_with_policy(root.join(folder), content_policy)
            }
            FolderFix::SwitchToBranch { branch } => {
                let folder = probe.folder.clone().unwrap_or_default();
                let managed =
                    Self::located_in(&root.join(&folder), content_policy.clone(), namespace)?
                        .ok_or_else(|| DraftlineError::FolderMissing(folder.clone()))?;
                managed.switch_to_folder_branch(branch)?;
                Self::open_with_policy(root.join(folder), content_policy)
            }
            FolderFix::UpgradeDraftline => Err(DraftlineError::NewerFolderMarker(
                probe.detail.unwrap_or_default(),
            )),
        }
    }

    /// Moves the folder Draftline manages to `to` (absolute, or relative to the
    /// repository root) as one save on the current variation, which also
    /// rewrites `.draftline`. The folder may move from the root into a
    /// subfolder, between subfolders, and to the root only when the
    /// repository holds nothing else. Untracked and ignored files follow when
    /// they can; see [`FolderMove::left_behind`]. Other handles on the old
    /// folder are refused from then on.
    pub fn move_folder(&mut self, to: impl AsRef<Path>) -> Result<FolderMove> {
        let to = to.as_ref();
        let to = if to.is_absolute() {
            folder_in(&self.root, to)?
        } else {
            normalize_workspace_relative(to)?
        };
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("move_folder")?;
        let branch = self.ensure_on_branch()?;
        let from = self.scope().to_path_buf();
        let head_commit = self.repo.head()?.peel_to_commit()?;
        let head_tree = head_commit.tree()?;
        if let Some(reason) = self.move_refusal(&head_tree, &from, &to)? {
            return Err(DraftlineError::FolderMoveRefused(reason));
        }
        let moved = self
            .repo
            .find_tree(self.moved_tree(&head_tree, &from, &to)?)?;
        let report = preflight_report(
            "move_folder",
            true,
            self.changed_files_unchecked()?,
            self.target_tree_hazards(&moved)?,
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }
        // Changes the policy skips still matter to Git's index.
        if let Some(path) = self.move_blockers(&from)?.into_iter().next() {
            return Err(DraftlineError::FolderMoveRefused(format!(
                "{} has changes Git hasn't committed; commit or discard them first",
                folder_key(&path)
            )));
        }

        let (author, committer) = self.rewrite_signatures(None)?;
        let operation_id = new_operation_id();
        let target = format!(
            "{} -> {}",
            marker_folder(&folder_key(&from)),
            marker_folder(&folder_key(&to))
        );
        self.write_recovery_state_for_checkout(
            &RecoveryState {
                operation_id: operation_id.clone(),
                operation: RecoveryOperation::MoveFolder,
                original_variation: Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(&branch)
                        .to_string(),
                ),
                target: Some(target.clone()),
                completed: false,
            },
            &head_tree,
            &moved,
        )?;
        self.checkout_or_keep_recovery(&moved)?;
        test_hooks::fire("after_folder_move_checkout", self.repo.path());
        // The trailer names where the folder was, so commits from before the
        // marker existed still read at the right place after this move.
        let message = format!(
            "Move Draftline folder from {} to {}\n\n{MOVED_FROM_TRAILER}: {}\n",
            folder_label(&from),
            folder_label(&to),
            marker_folder(&folder_key(&from))
        );
        let oid = self
            .repo
            .commit(None, &author, &committer, &message, &moved, &[&head_commit])?;
        let commit = self.repo.find_commit(oid)?;
        self.advance_head(&branch, &commit, Some(&head_commit), &committer)?;
        test_hooks::fire("after_folder_move_commit", self.repo.path());

        let (left_behind, _) = self.move_leftovers(&from, &to);
        self.content_policy = self.content_policy.clone().scoped_to(&to)?;
        self.write_claim()?;
        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::MoveFolder,
            original_variation: None,
            target: Some(target),
            completed: true,
        })?;

        Ok(FolderMove {
            from,
            to,
            version: version_from_commit(&commit),
            left_behind,
        })
    }

    /// A handle on `path` (which need not exist) in the repository holding it,
    /// without claiming or creating anything. `None` outside a repository with
    /// a working tree.
    fn located(path: &Path, content_policy: ContentPolicy) -> Result<Option<Self>> {
        Self::located_in(path, content_policy, None)
    }

    fn located_in(
        path: &Path,
        content_policy: ContentPolicy,
        namespace: Option<String>,
    ) -> Result<Option<Self>> {
        if let Some(namespace) = &namespace {
            validate_variation_namespace(namespace)?;
        }
        let Some(existing) = path.ancestors().find(|ancestor| ancestor.exists()) else {
            return Ok(None);
        };
        let Ok(repo) = Repository::discover(existing) else {
            return Ok(None);
        };
        let Some(root) = repo.workdir().map(Path::to_path_buf) else {
            return Ok(None);
        };
        let content_policy = content_policy.scoped_to(folder_in(&root, path)?)?;
        Ok(Some(Self {
            root,
            repo,
            content_policy,
            resolver_registry: ResolverRegistry::with_default_resolvers(),
            variation_namespace: namespace,
        }))
    }

    fn probe_located(&self, requested: PathBuf) -> Result<FolderProbe> {
        let mine = self.project_key();
        let mut probe = FolderProbe {
            state: FolderState::Ready,
            requested,
            repository: Some(self.root.clone()),
            folder: None,
            detail: None,
            suggestions: Vec::new(),
        };
        if self.unfinished_operation_touches(&mine) {
            probe.folder = Some(PathBuf::from(&mine));
            probe.detail =
                Some("An operation was interrupted; open to finish or acknowledge it.".to_string());
            probe.suggestions = vec![suggest(FolderFix::Open, "Open and finish it")];
            return Ok(probe);
        }
        let owner = match self.marker_owner()? {
            FolderOwner::Nobody => self.claim_owner()?,
            owner => owner,
        };
        let owner = match owner {
            FolderOwner::Nobody => {
                probe.state = FolderState::Unmanaged;
                probe.suggestions =
                    vec![suggest(FolderFix::ManageHere, "Start managing this folder")];
                return Ok(probe);
            }
            FolderOwner::Newer(reason) => {
                probe.state = FolderState::NewerFormat;
                probe.detail = Some(reason);
                probe.suggestions = vec![suggest(FolderFix::UpgradeDraftline, "Update Draftline")];
                return Ok(probe);
            }
            FolderOwner::Invalid(reason) => {
                probe.state = FolderState::InvalidMarker;
                probe.detail = Some(reason);
                // A folder named .draftline can't be replaced with the marker
                // without deleting what's in it; someone has to move it.
                if !self.marker_path().is_dir() {
                    probe.suggestions =
                        vec![suggest(FolderFix::ManageHere, "Manage this folder instead")];
                }
                return Ok(probe);
            }
            FolderOwner::Folder(owner) => owner,
        };

        let folder = PathBuf::from(&owner);
        let label = folder_label(&folder);
        probe.folder = Some(folder.clone());
        let head = self.head_tree_or_empty()?;
        if !self.claimed_folder_missing(&folder)? {
            if owner == mine {
                probe.suggestions = vec![suggest(FolderFix::Open, "Open")];
            } else {
                probe.state = FolderState::ManagedElsewhere;
                probe.detail = Some(format!("Draftline manages {label} in this repository."));
                probe.suggestions.push(suggest(
                    FolderFix::OpenManagedFolder {
                        folder: folder.clone(),
                    },
                    &format!("Open {label}"),
                ));
                if self.move_refusal(&head, &folder, self.scope())?.is_none()
                    && self.move_blockers(&folder)?.is_empty()
                {
                    probe.suggestions.push(suggest(
                        FolderFix::MoveFolderHere { from: folder },
                        &format!("Move {label} here"),
                    ));
                }
            }
            return Ok(probe);
        }

        probe.state = FolderState::FolderMissing;
        probe.detail = Some(format!("Draftline manages {label}, which isn't on disk."));
        let tracked = tracks_folder(&head, &folder);
        if tracked {
            probe.suggestions.push(suggest(
                FolderFix::RestoreFolder {
                    folder: folder.clone(),
                },
                &format!("Restore {label}"),
            ));
        } else {
            // A switch recreates only files that differ, so it can't bring
            // back a folder the checked-out commit still tracks.
            for branch in self.branches_with_folder(&folder, &owner)? {
                let label = format!("Switch to {branch}, which has {label}");
                probe
                    .suggestions
                    .push(suggest(FolderFix::SwitchToBranch { branch }, &label));
            }
        }
        if owner == mine && !tracked {
            probe
                .suggestions
                .push(suggest(FolderFix::ManageHere, "Create this folder"));
        } else if owner != mine && self.can_take_over_from(&head, &folder)? {
            probe
                .suggestions
                .push(suggest(FolderFix::ManageHere, "Manage this folder instead"));
        }
        // A commit renamed the folder: following it comes first, since
        // creating the old one would start empty.
        if !tracked {
            match self.renamed_folder(&folder)? {
                Some(renamed) if renamed == self.scope() => {
                    if let Some(at) = probe
                        .suggestions
                        .iter()
                        .position(|s| s.fix == FolderFix::ManageHere)
                    {
                        let mut first = probe.suggestions.remove(at);
                        first.label = format!("Manage this folder ({label} was renamed here)");
                        probe.suggestions.insert(0, first);
                    }
                }
                Some(renamed) => {
                    let offered = Self::located_in(
                        &self.root.join(&renamed),
                        ContentPolicy::default(),
                        self.variation_namespace.clone(),
                    )?
                    .map(|there| there.probe_located(self.root.join(&renamed)))
                    .transpose()?
                    .is_some_and(|there| {
                        there
                            .suggestions
                            .iter()
                            .any(|s| s.fix == FolderFix::ManageHere)
                    });
                    if offered {
                        let text = format!(
                            "Manage {} instead ({label} was renamed there)",
                            folder_label(&renamed)
                        );
                        probe.suggestions.insert(
                            0,
                            suggest(FolderFix::ManageRenamedFolder { folder: renamed }, &text),
                        );
                    }
                }
                None => {}
            }
        }
        Ok(probe)
    }

    /// Where a commit on the current branch renamed `folder`, when at least
    /// half of its files went to one folder that is still checked out here.
    fn renamed_folder(&self, folder: &Path) -> Result<Option<PathBuf>> {
        const MAX_COMMITS: usize = 500;
        if folder.as_os_str().is_empty() {
            return Ok(None);
        }
        let Ok(mut commit) = self.repo.head().and_then(|head| head.peel_to_commit()) else {
            return Ok(None);
        };
        let head_tree = commit.tree()?;
        for _ in 0..MAX_COMMITS {
            let Ok(parent) = commit.parent(0) else {
                return Ok(None);
            };
            let before = parent.tree()?;
            if !tracks_folder(&before, folder) {
                commit = parent;
                continue;
            }
            let after = commit.tree()?;
            let mut diff = self
                .repo
                .diff_tree_to_tree(Some(&before), Some(&after), None)?;
            diff.find_similar(Some(git2::DiffFindOptions::new().renames(true)))?;
            let mut moved_from = 0usize;
            let mut targets: BTreeMap<PathBuf, usize> = BTreeMap::new();
            for delta in diff.deltas() {
                let (Some(old), Some(new)) = (delta.old_file().path(), delta.new_file().path())
                else {
                    continue;
                };
                let Ok(rest) = old.strip_prefix(folder) else {
                    continue;
                };
                moved_from += 1;
                if delta.status() != git2::Delta::Renamed {
                    continue;
                }
                let new_parts: Vec<_> = new.components().collect();
                let rest_len = rest.components().count();
                if new_parts.len() <= rest_len || !new.ends_with(rest) {
                    continue;
                }
                let target: PathBuf = new_parts[..new_parts.len() - rest_len].iter().collect();
                *targets.entry(target).or_default() += 1;
            }
            return Ok(targets
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .filter(|(_, count)| *count * 2 >= moved_from)
                .map(|(target, _)| target)
                .filter(|target| target != folder)
                .filter(|target| tracks_folder(&head_tree, target))
                .filter(|target| self.root.join(target).is_dir()));
        }
        Ok(None)
    }

    /// Files under `folder` a move would lose or split from Git's index:
    /// staged or edited tracked files, conflicts, and new files the folder
    /// keeps. Untracked files it leaves out follow the move as leftovers.
    fn move_blockers(&self, folder: &Path) -> Result<Vec<PathBuf>> {
        let policy = self.content_policy.clone().scoped_to(folder)?;
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        limit_status_to_scope(&mut options, folder);
        let mut blockers = Vec::new();
        let statuses = self.repo.statuses(Some(&mut options))?;
        let filter_clean = self.filter_clean_in_index(&statuses)?;
        for entry in statuses.iter() {
            let status = entry.status();
            let Ok(path) = entry.path() else {
                continue;
            };
            if status == Status::CURRENT
                || (status == Status::WT_NEW && !policy.tracks(path)?)
                || filter_clean.contains(Path::new(path))
            {
                continue;
            }
            blockers.push(PathBuf::from(path));
        }
        Ok(blockers)
    }

    /// Managing this folder in place of a missing `old` one keeps the one
    /// rule moves follow: the root only when the repository holds nothing else.
    fn can_take_over_from(&self, head: &Tree<'_>, old: &Path) -> Result<bool> {
        if !self.scope().as_os_str().is_empty() {
            return Ok(true);
        }
        Ok(tracked_paths_under(head, Path::new(""))?
            .iter()
            .all(|path| path.starts_with(old) || path == Path::new(FOLDER_MARKER)))
    }

    /// Up to five local branches, newest first, whose tip has `folder` and
    /// doesn't name another one.
    fn branches_with_folder(&self, folder: &Path, key: &str) -> Result<Vec<String>> {
        let current = self.current_variation_unchecked().ok();
        let mut found = Vec::new();
        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()?.map(str::to_string) else {
                continue;
            };
            if current.as_deref() == Some(name.as_str())
                || self.ensure_variation_switchable(&name).is_err()
            {
                continue;
            }
            let commit = branch.get().peel_to_commit()?;
            let tree = commit.tree()?;
            let has_folder = tree
                .get_path(folder)
                .is_ok_and(|entry| entry.kind() == Some(ObjectType::Tree));
            if has_folder && self.tree_marker_allows(&tree, key) {
                found.push((commit.time().seconds(), name));
            }
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(found.into_iter().take(5).map(|(_, name)| name).collect())
    }

    fn tree_marker_allows(&self, tree: &Tree<'_>, key: &str) -> bool {
        let Some(entry) = tree.get_name(FOLDER_MARKER) else {
            return true;
        };
        self.repo
            .find_blob(entry.id())
            .ok()
            .and_then(|blob| serde_json::from_slice::<FolderMarker>(blob.content()).ok())
            .is_some_and(|marker| marker.format == 1 && marker.folder == marker_folder(key))
    }

    /// Under the lock, refuses a folder fix once another folder took over
    /// since the probe: the marker, or the claim when there is no marker,
    /// must still name this folder.
    fn ensure_still_managed(&self, doing: &str) -> Result<()> {
        let owner = match self.marker_owner()? {
            FolderOwner::Nobody => self.claim_owner()?,
            owner => owner,
        };
        if owner == FolderOwner::Folder(self.project_key()) {
            return Ok(());
        }
        Err(DraftlineError::FolderFixUnavailable(format!(
            "{doing} {} is no longer offered: Draftline manages another folder now",
            folder_label(self.scope())
        )))
    }

    /// Puts this folder's files back from HEAD, leaving everything else alone.
    fn restore_missing_folder(&self) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        let _lock = OperationLock::acquire(&self.lock_path(), "restore_folder")?;
        self.while_git_busy(|| self.ensure_git_index_unlocked())?;
        self.ensure_no_pending_recovery_in_checkout()?;
        self.ensure_still_managed("restoring")?;
        // No early return if the folder reappeared: it may be back only in
        // part, and a safe checkout only recreates what is still missing.
        let head = self.repo.head()?.peel_to_tree()?;
        let paths = if tracks_folder(&head, self.scope()) {
            tracked_paths_under(&head, self.scope())?
        } else {
            Vec::new()
        };
        if paths.is_empty() {
            return Err(DraftlineError::FolderFixUnavailable(format!(
                "the checked-out commit has no {} to restore",
                folder_label(self.scope())
            )));
        }
        // From HEAD's tree, so files whose deletion is staged come back too.
        let mut checkout = CheckoutBuilder::new();
        checkout
            .safe()
            .recreate_missing(true)
            .overwrite_ignored(false)
            .disable_pathspec_match(true);
        for path in &paths {
            checkout.path(path);
        }
        let refused = RefCell::default();
        self.checkout_refusing_filters(checkout, &refused, &head, |checkout| {
            self.repo.checkout_tree(head.as_object(), Some(checkout))
        })
    }

    /// Switches the checkout to `branch` for a folder that can't open yet,
    /// recorded like any switch so an interruption is recoverable.
    fn switch_to_folder_branch(&self, branch: &str) -> Result<()> {
        let branch = validate_variation_name(branch)?;
        fs::create_dir_all(self.draftline_dir())?;
        let _lock = OperationLock::acquire(&self.lock_path(), "switch_to_folder_branch")?;
        self.while_git_busy(|| self.ensure_git_index_unlocked())?;
        self.ensure_no_pending_recovery_in_checkout()?;
        self.ensure_still_managed("switching")?;
        // Decide again under the lock: the folder may be back, or the branch
        // may no longer have it.
        let key = self.project_key();
        if !self.claimed_folder_missing(self.scope())?
            || tracks_folder(&self.head_tree_or_empty()?, self.scope())
            || !self
                .branches_with_folder(self.scope(), &key)?
                .contains(&branch)
        {
            return Err(DraftlineError::FolderFixUnavailable(format!(
                "switching to {branch} is no longer offered"
            )));
        }
        let target = self.branch_tree(&branch)?;
        let file_hazards = self.target_tree_hazards(&target)?;
        if !file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "switch_variation",
                true,
                Vec::new(),
                file_hazards,
                Some(format!("current -> {branch}")),
            ))));
        }
        let operation_id = new_operation_id();
        self.write_recovery_state_for_checkout(
            &RecoveryState {
                operation_id: operation_id.clone(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: self.current_variation_unchecked().ok(),
                target: Some(branch.clone()),
                completed: false,
            },
            &self.head_tree_or_empty()?,
            &target,
        )?;
        self.checkout_or_keep_recovery(&target)?;
        self.point_head_at(&branch)?;
        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::SwitchVariation,
            original_variation: None,
            target: Some(branch),
            completed: true,
        })
    }

    /// Why the folder can't move from `from` to `to`, if it can't.
    fn move_refusal(&self, head: &Tree<'_>, from: &Path, to: &Path) -> Result<Option<String>> {
        let to_label = folder_label(to);
        if from == to {
            return Ok(Some(format!("Draftline already manages {to_label}")));
        }
        if let Some(first) = to.components().next() {
            let first = first.as_os_str();
            if first == ".git" || is_repository_path(to) {
                return Ok(Some(format!("{to_label} is reserved")));
            }
        }
        // Files already saved under `to` would mix with the folder's own,
        // except the folder itself when it moves up into `to`. The root's
        // repository files stay put unless the folder brings its own.
        let into_own_subfolder = to.starts_with(from);
        let to_root = to.as_os_str().is_empty();
        let brings = |path: &Path| {
            path.components()
                .next()
                .is_some_and(|first| head.get_path(&from.join(first)).is_ok())
        };
        let blocking = tracked_paths_under(head, to)?.into_iter().find(|path| {
            path != Path::new(FOLDER_MARKER)
                && (into_own_subfolder || !path.starts_with(from))
                && !(to_root && is_repository_path(path) && !brings(path))
        });
        if let Some(path) = blocking {
            return Ok(Some(if to.as_os_str().is_empty() {
                format!(
                    "the repository holds other files, such as {}",
                    folder_key(&path)
                )
            } else {
                format!("{to_label} already has saved files")
            }));
        }
        if to.as_os_str().is_empty() {
            return Ok(None);
        }
        let mut walked = self.root.clone();
        for component in to.components() {
            walked.push(component);
            if fs::symlink_metadata(&walked).is_ok_and(|meta| meta.file_type().is_symlink()) {
                return Ok(Some(format!("{to_label} is reached through a link")));
            }
        }
        let dir = self.root.join(to);
        if dir.is_file() {
            return Ok(Some(format!("{to_label} is a file")));
        }
        if dir.is_dir() {
            let own = from
                .strip_prefix(to)
                .ok()
                .and_then(|inner| inner.components().next())
                .map(|first| first.as_os_str().to_os_string());
            let occupied = fs::read_dir(&dir)?
                .flatten()
                .any(|entry| Some(entry.file_name()) != own);
            if occupied {
                return Ok(Some(format!("{to_label} isn't empty")));
            }
        }
        Ok(None)
    }

    /// `head` with the folder's files moved from `from` to `to` and
    /// [`FOLDER_MARKER`] naming `to`. From the root, [`REPOSITORY_FILES`] stay.
    fn moved_tree(&self, head: &Tree<'_>, from: &Path, to: &Path) -> Result<Oid> {
        let mut source = git2::Index::new()?;
        source.read_tree(head)?;
        let from = folder_key(from);
        let to = folder_key(to);
        let mut moved = git2::Index::new()?;
        for mut entry in source.iter() {
            if entry.path == FOLDER_MARKER.as_bytes() {
                continue;
            }
            let stays = from.is_empty() && is_repository_file(&entry.path);
            if let Some(rest) = strip_folder(&entry.path, from.as_bytes()).filter(|_| !stays) {
                let mut path = to.as_bytes().to_vec();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(rest);
                entry.path = path;
            }
            moved.add(&entry)?;
        }
        let tree = self.repo.find_tree(moved.write_tree_to(&self.repo)?)?;
        let marker = self.repo.blob(&marker_bytes(&to)?)?;
        let mut builder = self.repo.treebuilder(Some(&tree))?;
        builder.insert(FOLDER_MARKER, marker, 0o100644)?;
        Ok(builder.write()?)
    }

    /// Moves untracked and ignored files the checkout left in `from` into
    /// `to`, returning those that stayed and whether any moved. Nothing
    /// follows from the root: its untracked files are anyone's. Files Git's
    /// index tracks are never moved.
    fn move_leftovers(&self, from: &Path, to: &Path) -> (Vec<PathBuf>, bool) {
        if from.as_os_str().is_empty() {
            return (Vec::new(), false);
        }
        // Read fresh: another process may have committed since this opened.
        let index = self
            .repo
            .index()
            .and_then(|mut index| index.read(false).map(|()| index))
            .ok();
        let tracked = |relative: &Path| {
            index.as_ref().is_none_or(|index| {
                let key = folder_key(&from.join(relative));
                index.get_path(Path::new(&key), 0).is_some()
                    || index.find_prefix(format!("{key}/")).is_ok()
            })
        };
        let source = self.root.join(from);
        let destination = self.root.join(to);
        let (mut files, mut dirs) = (Vec::new(), Vec::new());
        collect_leftovers(&source, &destination, &mut files, &mut dirs);
        let mut left_behind = Vec::new();
        let mut moved_any = false;
        for file in files {
            let Ok(relative) = file.strip_prefix(&source) else {
                continue;
            };
            if tracked(relative) {
                continue;
            }
            let target = destination.join(relative);
            let moved = target.symlink_metadata().is_err()
                && target
                    .parent()
                    .is_none_or(|parent| fs::create_dir_all(parent).is_ok())
                && fs::rename(&file, &target).is_ok();
            if moved {
                moved_any = true;
            } else {
                left_behind.push(from.join(relative));
            }
        }
        for dir in dirs.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
        let _ = fs::remove_dir(&source);
        (left_behind, moved_any)
    }

    /// Points the local record at the folder the marker on disk names.
    fn sync_claim_to_marker(&self) -> Result<()> {
        if let FolderOwner::Folder(folder) = self.marker_owner()? {
            self.write_claim_for(&folder)?;
        }
        Ok(())
    }

    /// Clones a shared workspace from a remote endpoint.
    pub fn clone_workspace(remote_url: impl AsRef<str>, path: impl AsRef<Path>) -> Result<Self> {
        Self::clone_workspace_with_policy(remote_url, path, ContentPolicy::default())
    }

    /// Clones a shared workspace from a remote endpoint with an explicit content policy.
    pub fn clone_workspace_with_policy(
        remote_url: impl AsRef<str>,
        path: impl AsRef<Path>,
        content_policy: ContentPolicy,
    ) -> Result<Self> {
        let mut options = RemoteOptions::new();
        Self::clone_workspace_with_policy_and_options(
            remote_url,
            path,
            content_policy,
            &mut options,
        )
    }

    /// Clones a shared workspace from a remote endpoint with explicit remote options.
    pub fn clone_workspace_with_options(
        remote_url: impl AsRef<str>,
        path: impl AsRef<Path>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<Self> {
        Self::clone_workspace_with_policy_and_options(
            remote_url,
            path,
            ContentPolicy::default(),
            options,
        )
    }

    /// Clones a shared workspace with explicit content policy and remote options.
    pub fn clone_workspace_with_policy_and_options(
        remote_url: impl AsRef<str>,
        path: impl AsRef<Path>,
        content_policy: ContentPolicy,
        options: &mut RemoteOptions<'_>,
    ) -> Result<Self> {
        ensure_supported_remote_url(remote_url.as_ref())?;
        let mut builder = RepoBuilder::new();
        let _timeout_guard = options.server_timeout_guard()?;
        if options.has_network_callbacks() {
            let fetch_options = options.clone_fetch_options();
            builder.fetch_options(fetch_options);
        }
        let repo = builder.clone(remote_url.as_ref(), path.as_ref())?;
        // Open the folder the repository's marker names, not its root.
        // The folder may not exist on the cloned branch yet.
        let folder = head_marker_folder(&repo).unwrap_or_default();
        let project = create_folder_within(path.as_ref(), &folder)?;
        Self::from_repository(repo, &project, content_policy)
    }

    /// Adds a semantic merge resolver ahead of Draftline's built-in resolvers.
    ///
    /// The first matching resolver wins, so resolvers added with this method
    /// take precedence over the built-in Markdown and plain-text behavior.
    /// Each subsequent call also places the new resolver ahead of resolvers
    /// added by earlier calls.
    ///
    /// ```no_run
    /// use std::path::Path;
    ///
    /// use draftline::{
    ///     MergeInput, MergeOutcome, SemanticMergeResolver, Workspace,
    /// };
    ///
    /// struct SketchResolver;
    ///
    /// impl SemanticMergeResolver for SketchResolver {
    ///     fn matches(&self, path: &Path) -> bool {
    ///         matches!(
    ///             path.extension().and_then(|extension| extension.to_str()),
    ///             Some("sk" | "sb")
    ///         )
    ///     }
    ///
    ///     fn merge(&self, input: MergeInput<'_>) -> MergeOutcome {
    ///         // Perform the host application's content-aware three-way merge.
    ///         MergeOutcome::clean(input.ours)
    ///     }
    /// }
    ///
    /// let workspace = Workspace::open("my-content")?
    ///     .with_merge_resolver(SketchResolver);
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn with_merge_resolver<R>(mut self, resolver: R) -> Self
    where
        R: crate::merge::SemanticMergeResolver + 'static,
    {
        self.resolver_registry =
            std::mem::take(&mut self.resolver_registry).register_first(resolver);
        self
    }

    /// Replaces the complete semantic merge resolver registry.
    ///
    /// Use [`ResolverRegistry::with_default_resolvers`] together with
    /// [`ResolverRegistry::register_first`] to retain Draftline's built-ins
    /// while giving custom resolvers precedence. A registry created with
    /// [`ResolverRegistry::new`] still falls back to plain-text merging when no
    /// registered resolver matches.
    pub fn with_resolver_registry(mut self, resolver_registry: ResolverRegistry) -> Self {
        self.resolver_registry = resolver_registry;
        self
    }

    /// Returns the repository working-tree root.
    ///
    /// Paths accepted and returned by Draftline are relative to this root. For a
    /// folder inside a larger repository, see [`Workspace::scope`] and
    /// [`Workspace::project_root`].
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the repository-relative folder, or an empty path when
    /// the project is the repository root.
    pub fn scope(&self) -> &Path {
        self.content_policy.scope()
    }

    /// The Git identity (`user.name` / `user.email`) this repository resolves,
    /// honouring repository, global, XDG and system config. Returns `Ok(None)`
    /// when no non-empty `user.name` is configured, and an error when the
    /// config cannot be read.
    pub fn git_identity(&self) -> Result<Option<Contributor>> {
        crate::profile::identity_from_config(&self.repo.config()?)
    }

    /// Returns the absolute folder.
    pub fn project_root(&self) -> PathBuf {
        self.root.join(self.scope())
    }

    /// Lists only branches under `namespace` (e.g. `"cutready/"`) as
    /// variations, plus the default branch and whatever branch is currently
    /// checked out. Use this
    /// inside a shared code repository so developers' branches stay out of the
    /// business user's view; without it every local branch is a variation.
    pub fn with_variation_namespace(mut self, namespace: impl Into<String>) -> Result<Self> {
        let namespace = namespace.into();
        validate_variation_namespace(&namespace)?;
        self.variation_namespace = Some(namespace);
        Ok(self)
    }

    /// The branch namespace set by [`Workspace::with_variation_namespace`].
    pub fn variation_namespace(&self) -> Option<&str> {
        self.variation_namespace.as_deref()
    }

    /// With a namespace set, only branches inside it may be created, renamed,
    /// deleted, adopted, or restored onto, so developers' branches stay theirs.
    fn ensure_variation_in_namespace(&self, name: &str) -> Result<()> {
        match self.variation_namespace.as_deref() {
            Some(namespace) if !name.starts_with(namespace) => {
                Err(DraftlineError::VariationOutsideNamespace(name.to_string()))
            }
            _ => Ok(()),
        }
    }

    /// Switching may also reach the base branch, which is always listed.
    fn ensure_variation_switchable(&self, name: &str) -> Result<()> {
        if self.variation_namespace.is_some() && self.default_branch().as_deref() != Some(name) {
            self.ensure_variation_in_namespace(name)?;
        }
        Ok(())
    }

    /// Saves and updates land on whatever branch is checked out, like a
    /// teammate's would, but never on a detached `HEAD`, where the next switch
    /// would strand them. Shelve and discard stay available. Returns the
    /// branch's full ref name, which the write then pins.
    fn ensure_on_branch(&self) -> Result<String> {
        self.repo
            .find_reference("HEAD")?
            .symbolic_target()
            .ok()
            .flatten()
            .filter(|target| target.starts_with("refs/heads/"))
            .map(str::to_string)
            .ok_or(DraftlineError::NoCurrentVariation)
    }

    /// Returns the current content policy.
    pub fn content_policy(&self) -> &ContentPolicy {
        &self.content_policy
    }

    /// Returns a structured, read-only safety snapshot for hosts and agents.
    ///
    /// Inspection is intentionally diagnostic: it does not require the workspace
    /// to be free of pending recovery, and it reports lock/recovery state instead
    /// of attempting to repair it. In this slice, `RepairRecovery` is advisory;
    /// operation-specific repair and stale-lock clearing are exposed by later
    /// recovery APIs.
    pub fn inspect(&self) -> Result<WorkspaceInspection> {
        let mut diagnostics = Vec::new();
        let recovery = match self.recovery_state() {
            Ok(recovery) => recovery,
            Err(error) => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::RecoveryRequired,
                    DiagnosticSeverity::Blocking,
                    format!("workspace recovery state could not be read: {error}"),
                ));
                None
            }
        };

        let current_variation = match self.current_variation_unchecked() {
            Ok(variation) => Some(VariationId::from(variation)),
            Err(DraftlineError::NoCurrentVariation) => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::NoCurrentVariation,
                    DiagnosticSeverity::Blocking,
                    "workspace is not on a normal Draftline variation",
                ));
                None
            }
            Err(error) => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::WorkspaceReadFailed,
                    DiagnosticSeverity::Warning,
                    format!("could not read current variation: {error}"),
                ));
                None
            }
        };

        let dirty_files = match self.changed_files_unchecked() {
            Ok(files) => files,
            Err(error) => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::WorkspaceReadFailed,
                    DiagnosticSeverity::Warning,
                    format!("could not read dirty files: {error}"),
                ));
                Vec::new()
            }
        };
        let dirty = DirtySummary {
            is_dirty: !dirty_files.is_empty(),
            files: dirty_files,
        };

        let remotes = match self.remotes_unchecked() {
            Ok(remotes) => remotes,
            Err(error) => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::WorkspaceReadFailed,
                    DiagnosticSeverity::Warning,
                    format!("could not read remotes: {error}"),
                ));
                Vec::new()
            }
        };
        let sharing_mode = if remotes.is_empty() {
            diagnostics.push(workspace_diagnostic(
                DiagnosticCode::LocalOnlyWorkspace,
                DiagnosticSeverity::Info,
                "workspace has no configured remote",
            ));
            SharingMode::LocalOnly
        } else {
            diagnostics.push(workspace_diagnostic(
                DiagnosticCode::SharedCapableWorkspace,
                DiagnosticSeverity::Info,
                "workspace has at least one configured remote",
            ));
            SharingMode::SharedCapable
        };

        if dirty.is_dirty {
            diagnostics.push(workspace_diagnostic(
                DiagnosticCode::DirtyWorkspace,
                DiagnosticSeverity::Warning,
                "workspace has unsaved tracked content changes",
            ));
        }

        match self.missing_lfs_content() {
            Ok(missing) if !missing.is_empty() => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::LfsContentMissing,
                    DiagnosticSeverity::Warning,
                    format!(
                        "{} project file(s), such as `{}`, are Git LFS pointers; a developer must install Git LFS and run `git lfs pull`",
                        missing.len(),
                        missing[0].display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) => diagnostics.push(workspace_diagnostic(
                DiagnosticCode::WorkspaceReadFailed,
                DiagnosticSeverity::Warning,
                format!("could not check for Git LFS pointers: {error}"),
            )),
        }

        if recovery.is_some() {
            diagnostics.push(match self.recovery_owner_elsewhere() {
                Some(project) => workspace_diagnostic(
                    DiagnosticCode::RecoveryInOtherFolder,
                    DiagnosticSeverity::Blocking,
                    format!(
                        "project {} has an incomplete Draftline operation",
                        project.display()
                    ),
                ),
                None => workspace_diagnostic(
                    DiagnosticCode::RecoveryRequired,
                    DiagnosticSeverity::Blocking,
                    "workspace has an incomplete Draftline operation",
                ),
            });
        }

        match self.policy_git_diagnostics_unchecked() {
            Ok(policy_diagnostics) => diagnostics.extend(policy_diagnostics),
            Err(error) => diagnostics.push(workspace_diagnostic(
                DiagnosticCode::WorkspaceReadFailed,
                DiagnosticSeverity::Warning,
                format!("could not read content-policy Git diagnostics: {error}"),
            )),
        }

        let operation_lock = OperationLockSummary {
            state: if self.lock_path().exists() {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::WorkspaceLocked,
                    DiagnosticSeverity::Blocking,
                    "workspace has an operation lock",
                ));
                OperationLockState::Locked
            } else {
                OperationLockState::Unlocked
            },
        };

        let safe_next_actions =
            safe_next_actions_for_inspection(&recovery, &operation_lock, &dirty, &diagnostics);

        Ok(WorkspaceInspection {
            workspace_id: WorkspaceId {
                root: self.root.clone(),
            },
            sharing_mode,
            current_variation,
            remotes,
            dirty,
            recovery,
            operation_lock,
            support_refs: SupportRefSummary {
                local_count: self
                    .list_local_support_refs()
                    .map(|support_refs| support_refs.len())
                    .unwrap_or_default(),
                remote_count: 0,
            },
            diagnostics,
            safe_next_actions,
        })
    }

    /// Returns [`Workspace::inspect`] as JSON for agents/tools.
    pub fn inspect_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.inspect()?)?)
    }

    /// Reports feature availability for this Draftline crate version.
    pub fn capabilities() -> WorkspaceCapabilities {
        WorkspaceCapabilities {
            inspect: true,
            workspace_summary: true,
            save_version: true,
            switch_variation: true,
            publish_changes: true,
            apply_incoming: true,
            stale_lock_repair: true,
            target_tree_collision_preflight: true,
            support_ref_sync: true,
            agent_cli_facade: true,
        }
    }

    /// Returns [`Workspace::capabilities`] as JSON for agents/tools.
    pub fn capabilities_json() -> Result<String> {
        Ok(serde_json::to_string(&Self::capabilities())?)
    }

    /// Verifies key workspace postconditions for agents/tools.
    pub fn verify_workspace(&self) -> Result<WorkspaceVerification> {
        let inspection = self.inspect()?;
        let recovery_clear = inspection.recovery.is_none()
            && !inspection
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == DiagnosticCode::RecoveryRequired);
        Ok(WorkspaceVerification {
            recovery_clear,
            operation_lock_clear: inspection.operation_lock.state == OperationLockState::Unlocked,
            current_variation_present: inspection.current_variation.is_some(),
            diagnostics: inspection.diagnostics,
        })
    }

    /// Explains a stable diagnostic code for agents/tools.
    pub fn explain_error(code: DiagnosticCode) -> ErrorExplanation {
        match code {
            DiagnosticCode::RecoveryRequired => ErrorExplanation {
                code,
                message: "workspace has an incomplete Draftline operation".to_string(),
                safe_next_actions: vec![SafeNextAction::RepairRecovery],
                retry: RetryClass::RetryAfterRepair,
            },
            DiagnosticCode::RecoveryInOtherFolder => ErrorExplanation {
                code,
                message: "another folder in this checkout has an incomplete Draftline operation"
                    .to_string(),
                safe_next_actions: vec![SafeNextAction::FinishInOtherFolder],
                retry: RetryClass::RetryAfterRepair,
            },
            DiagnosticCode::WorkspaceLocked => ErrorExplanation {
                code,
                message: "workspace is locked by a Draftline operation".to_string(),
                safe_next_actions: vec![SafeNextAction::RepairRecovery],
                retry: RetryClass::RetryAfterRepair,
            },
            DiagnosticCode::DirtyWorkspace => ErrorExplanation {
                code,
                message: "workspace has unsaved tracked content changes".to_string(),
                safe_next_actions: vec![SafeNextAction::SaveFirst, SafeNextAction::DiscardChanges],
                retry: RetryClass::RetryAfterUserChoice,
            },
            DiagnosticCode::LocalOnlyWorkspace => ErrorExplanation {
                code,
                message: "workspace has no configured remote".to_string(),
                safe_next_actions: vec![SafeNextAction::ConfigureRemote],
                retry: RetryClass::RetryAfterUserChoice,
            },
            DiagnosticCode::SharedCapableWorkspace => ErrorExplanation {
                code,
                message: "workspace has a configured remote".to_string(),
                safe_next_actions: vec![SafeNextAction::NormalWork],
                retry: RetryClass::Retryable,
            },
            DiagnosticCode::NoCurrentVariation => ErrorExplanation {
                code,
                message: "workspace is not on a normal Draftline variation".to_string(),
                safe_next_actions: vec![SafeNextAction::RepairRecovery],
                retry: RetryClass::RetryAfterRepair,
            },
            DiagnosticCode::WorkspaceReadFailed => ErrorExplanation {
                code,
                message: "workspace state could not be read completely".to_string(),
                safe_next_actions: Vec::new(),
                retry: RetryClass::NotRetryable,
            },
            DiagnosticCode::PolicyTrackedFileIgnored => ErrorExplanation {
                code,
                message: "Git ignore rules hide content tracked by policy".to_string(),
                safe_next_actions: Vec::new(),
                retry: RetryClass::RetryAfterUserChoice,
            },
            DiagnosticCode::LfsContentMissing => ErrorExplanation {
                code,
                message:
                    "some project files are Git LFS pointers; their content was never downloaded"
                        .to_string(),
                safe_next_actions: Vec::new(),
                retry: RetryClass::RetryAfterUserChoice,
            },
        }
    }

    /// Reports current Git metadata behavior that can hide policy-tracked content.
    pub fn policy_git_diagnostics(&self) -> Result<Vec<WorkspaceDiagnostic>> {
        self.policy_git_diagnostics_unchecked()
    }

    /// Audits the current content policy against Git metadata and known history.
    pub fn audit_content_policy(&self) -> Result<ContentPolicyAudit> {
        Ok(ContentPolicyAudit {
            current_diagnostics: self.policy_git_diagnostics_unchecked()?,
            historical_out_of_policy_paths: Vec::new(),
        })
    }

    /// Produces a read-only setup report for adopting an existing Git repository.
    pub fn preflight_adopt_workspace(
        &self,
        policy: ContentPolicy,
    ) -> Result<AdoptionPreflightReport> {
        let inspection = self.inspect()?;
        let policy = policy.scoped_to(self.scope())?;
        let candidate_policy_diagnostics = self.policy_git_diagnostics_for_policy(&policy)?;
        let blockers: Vec<WorkspaceDiagnostic> = inspection
            .diagnostics
            .iter()
            .chain(candidate_policy_diagnostics.iter())
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Blocking)
            .cloned()
            .collect();
        let warnings = inspection
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning)
            .cloned()
            .collect();
        let safe_next_actions = inspection.safe_next_actions.clone();
        let can_adopt = blockers.is_empty();

        Ok(AdoptionPreflightReport {
            inspection,
            candidate_policy_diagnostics,
            blockers,
            warnings,
            safe_next_actions,
            can_adopt,
        })
    }

    /// Generates generic rules of engagement for coding agents in this workspace.
    pub fn generate_agent_instructions(&self) -> Result<String> {
        Ok(
            "Draftline-managed repository rules:\n\
             - Reading Git history/status, editing worktree files, and running tests is safe.\n\
             - Do not rewrite, delete, rename, or force-update Draftline visible refs directly.\n\
             - Do not remove refs/draftline/ support refs except through purge or retention flows.\n\
             - Do not clear operation locks manually; use Draftline recovery tooling.\n\
             - Fetch before reasoning about shared remote state.\n\
             - Use Draftline primitives for variations, shelves, incoming merges, support-ref sync, and recovery.\n"
                .to_string(),
        )
    }

    /// Returns incomplete recovery state, if a prior operation was interrupted.
    pub fn recovery_state(&self) -> Result<Option<RecoveryState>> {
        let path = self.ledger_path();
        if !path.exists() {
            return Ok(None);
        }

        let text = fs::read_to_string(path)?;
        let state = serde_json::from_str::<RecoveryLedger>(&text)?.state;

        if state.completed {
            Ok(None)
        } else {
            Ok(Some(state))
        }
    }

    /// Inspects the operation lock without mutating it.
    pub fn inspect_operation_lock(&self) -> Result<OperationLockInspection> {
        inspect_operation_lock_path(&self.lock_path())
    }

    /// Clears an abandoned operation lock only when lock metadata marks it stale.
    pub fn clear_stale_lock(&self) -> Result<()> {
        self.claim_checkout(false)?;
        let inspection = self.inspect_operation_lock()?;
        if inspection.state == OperationLockState::Unlocked {
            return Ok(());
        }

        if !inspection.can_clear {
            return Err(DraftlineError::WorkspaceLocked);
        }

        fs::remove_file(self.lock_path())?;
        Ok(())
    }

    /// Repairs an interrupted operation when the recovery ledger has enough
    /// state to finish the operation safely.
    pub fn repair_recovery(&self, operation_id: impl AsRef<str>) -> Result<RecoveryRepairResult> {
        let mut options = RemoteOptions::new();
        self.repair_recovery_with_options(operation_id, &mut options)
    }

    /// Repairs an interrupted operation using host-provided remote options.
    pub fn repair_recovery_with_options(
        &self,
        operation_id: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<RecoveryRepairResult> {
        self.clear_stale_lock()?;
        let _lock = self.acquire_for_index("repair_recovery")?;
        let state = self.pending_recovery(operation_id.as_ref())?;
        self.ensure_recovery_may_write()?;

        match state.operation {
            RecoveryOperation::SwitchVariation => {
                let Some(target) = state.target.as_deref() else {
                    return Ok(self.recovery_unavailable(state));
                };
                self.ensure_variation_switchable(target)?;
                let changed_workspace =
                    self.finish_switch_at(state.original_variation.as_deref(), target)?;
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::ApplyIncoming | RecoveryOperation::MergeIncoming => {
                let (Some(variation), Some(target)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                let oid = Oid::from_str(target)
                    .map_err(|_| DraftlineError::VersionNotFound(target.to_string()))?;
                let commit = self.repo.find_commit(oid)?;
                let branch_ref = format!("refs/heads/{}", validate_variation_name(variation)?);
                let current = self.repo.refname_to_id(&branch_ref).ok();
                if current == Some(oid) {
                    // The ref only moves after a finished checkout, so files are
                    // already in place; anything different now is a newer edit.
                    return self.complete_recovery(state, false);
                } else if current.is_some_and(|current| {
                    self.repo.graph_descendant_of(oid, current).unwrap_or(false)
                }) {
                    // Interrupted mid-checkout: safe checkout treats files already at
                    // either side as clean, so rerunning it is idempotent and still
                    // refuses to clobber local edits.
                    self.checkout_tree_safely(&commit.tree()?)?;
                    self.repo
                        .reference(&branch_ref, oid, true, "repair incoming")?;
                } else {
                    // Someone else moved the branch since; don't guess.
                    return Ok(self.recovery_unavailable(state));
                }
                self.repo.set_head(&branch_ref)?;
                self.complete_recovery(state, true)
            }
            RecoveryOperation::DiscardChanges => {
                let changed_files = self.changed_files_unchecked()?;
                let changed_workspace = !changed_files.is_empty();
                if changed_workspace {
                    self.discard_changed_files(&changed_files)?;
                }
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::DiscardFile => {
                let Some(target) = state.target.as_deref() else {
                    return Ok(self.recovery_unavailable(state));
                };
                let target = PathBuf::from(target);
                let changed_file = self
                    .changed_files_unchecked()?
                    .into_iter()
                    .find(|changed| changed.path == target);
                let changed_workspace = changed_file.is_some();
                if let Some(changed_file) = changed_file {
                    self.discard_changed_files(std::slice::from_ref(&changed_file))?;
                }
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::DeleteVariation => {
                let (Some(variation), Some(_)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                self.ensure_variation_in_namespace(variation)?;
                self.repair_delete_variation_recovery(&state)?;
                self.complete_recovery(state, true)
            }
            RecoveryOperation::RenameVariation => {
                match self.repair_rename_variation_recovery(&state)? {
                    Some(changed_workspace) => self.complete_recovery(state, changed_workspace),
                    None => Ok(self.recovery_unavailable(state)),
                }
            }
            RecoveryOperation::ApplyShelf => {
                let Some(target) = state.target.as_deref() else {
                    return Ok(self.recovery_unavailable(state));
                };
                let (_id, commit) = self.find_shelf_commit(target)?;
                self.apply_shelf_paths(&commit)?;
                self.complete_recovery(state, true)
            }
            RecoveryOperation::ExpireSupportRefs => {
                if let Some(target) = state.target.as_deref() {
                    for id in target.split(',').filter(|id| !id.is_empty()) {
                        if let Ok(mut reference) = self.repo.find_reference(id) {
                            reference.delete()?;
                        }
                    }
                }
                self.complete_recovery(state, true)
            }
            RecoveryOperation::ShelveChanges => {
                let shelf_exists = state
                    .target
                    .as_deref()
                    .is_some_and(|target| self.find_shelf_commit(target).is_ok());
                if shelf_exists && self.changed_files_unchecked()?.is_empty() {
                    self.complete_recovery(state, false)
                } else {
                    Ok(self.recovery_unavailable(state))
                }
            }
            RecoveryOperation::DeleteRemoteVariation => {
                self.repair_delete_remote_variation_recovery(state, options)
            }
            RecoveryOperation::MoveFolder => self.settle_folder_move(state),
            RecoveryOperation::RestoreVersionAsNewSave
            | RecoveryOperation::MilestoneMerge
            | RecoveryOperation::SquashVersions
            | RecoveryOperation::HistoryCleanup => Ok(self.recovery_unavailable(state)),
        }
    }

    /// Rolls back an interrupted operation when the recovery ledger captured the
    /// prior state needed for a safe rollback.
    pub fn rollback_recovery(&self, operation_id: impl AsRef<str>) -> Result<RecoveryRepairResult> {
        self.clear_stale_lock()?;
        let _lock = self.acquire_for_index("rollback_recovery")?;
        let state = self.pending_recovery(operation_id.as_ref())?;
        self.ensure_recovery_may_write()?;

        match state.operation {
            RecoveryOperation::SwitchVariation => {
                // Going back to where the user was, like staying on the current branch.
                let (Some(original), Some(target)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                let changed_workspace = self.finish_switch_at(Some(target), original)?;
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::DeleteVariation => {
                let (Some(variation), Some(target)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                self.ensure_variation_in_namespace(variation)?;
                let oid = Oid::from_str(target)
                    .map_err(|_| DraftlineError::VersionNotFound(target.to_string()))?;
                let branch_ref = format!("refs/heads/{}", validate_variation_name(variation)?);
                self.repo
                    .reference(&branch_ref, oid, true, "rollback recovery")?;
                if self.current_variation_unchecked().ok().as_deref() == Some(variation) {
                    let commit = self.repo.find_commit(oid)?;
                    self.checkout_scope_forced(&commit.tree()?)?;
                    self.repo.set_head(&branch_ref)?;
                }
                let archive_ref = archive_ref("deleted-variations", variation, &state.operation_id);
                if let Ok(mut reference) = self.repo.find_reference(&archive_ref) {
                    reference.delete()?;
                }
                self.complete_recovery(state, true)
            }
            RecoveryOperation::RenameVariation => {
                match self.rollback_rename_variation_recovery(&state)? {
                    Some(changed_workspace) => self.complete_recovery(state, changed_workspace),
                    None => Ok(self.recovery_unavailable(state)),
                }
            }
            RecoveryOperation::ApplyShelf => {
                let changed_files = self.changed_files_unchecked()?;
                let changed_workspace = !changed_files.is_empty();
                if changed_workspace {
                    self.discard_changed_files(&changed_files)?;
                }
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::DiscardChanges | RecoveryOperation::DiscardFile => {
                // A discard only removes edits; stopping keeps whatever is on disk.
                self.complete_recovery(state, false)
            }
            RecoveryOperation::ShelveChanges => {
                let changed_workspace = self.rollback_shelve_recovery(&state)?;
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::RestoreVersionAsNewSave => {
                match self.rollback_restore_recovery(&state)? {
                    Some(changed_workspace) => self.complete_recovery(state, changed_workspace),
                    None => Ok(self.recovery_unavailable(state)),
                }
            }
            RecoveryOperation::MoveFolder => self.settle_folder_move(state),
            RecoveryOperation::ApplyIncoming
            | RecoveryOperation::DeleteRemoteVariation
            | RecoveryOperation::ExpireSupportRefs
            | RecoveryOperation::MergeIncoming
            | RecoveryOperation::MilestoneMerge
            | RecoveryOperation::SquashVersions
            | RecoveryOperation::HistoryCleanup => Ok(self.recovery_unavailable(state)),
        }
    }

    /// Settles an interrupted folder move on whatever its variation records:
    /// the moved folder once the save landed, the original one before. Repair
    /// and rollback are the same, since nothing else is safe to guess.
    fn settle_folder_move(&self, state: RecoveryState) -> Result<RecoveryRepairResult> {
        let Some(branch) = state.original_variation.clone() else {
            return Ok(self.recovery_unavailable(state));
        };
        let changed_workspace = self.finish_switch_at(None, &branch)?;
        self.sync_claim_to_marker()?;
        let mut moved_leftovers = false;
        if let Some((from, to)) = state.target.as_deref().and_then(|t| t.split_once(" -> ")) {
            // Settled on the moved side: untracked files follow, as in a move
            // that finished. Otherwise they never left.
            if let FolderOwner::Folder(settled) = self.marker_owner()? {
                if marker_folder(&settled) == to && from != to {
                    let key = |side: &str| PathBuf::from(if side == "." { "" } else { side });
                    moved_leftovers = self.move_leftovers(&key(from), &key(to)).1;
                }
            }
            // The checkout leaves the abandoned side's folder behind when
            // empty; its parents may predate the move, so they stay.
            for side in [from, to] {
                if side != "." {
                    let _ = fs::remove_dir(self.root.join(side));
                }
            }
        }
        self.complete_recovery(state, changed_workspace || moved_leftovers)
    }

    /// Undoes an interrupted shelve. The shelf is deleted only while every
    /// path it captured is still unchanged on disk; once the discard step has
    /// touched a file, the shelf is the only copy of the edit and is kept.
    fn rollback_shelve_recovery(&self, state: &RecoveryState) -> Result<bool> {
        let Some(name) = state.target.as_deref() else {
            return Ok(false);
        };
        let Ok((id, shelf)) = self.find_shelf_commit(name) else {
            return Ok(false);
        };
        let parent_tree = match shelf.parent(0) {
            Ok(parent) => Some(parent.tree()?),
            Err(_) => None,
        };
        let shelf_tree = shelf.tree()?;
        let captured = self.paths_changed_between(parent_tree.as_ref(), &shelf_tree)?;
        if self.workdir_matches(&shelf_tree, &captured)?.len() == captured.len() {
            self.repo
                .find_reference(&format!("refs/draftline/shelves/{id}"))?
                .delete()?;
        }
        Ok(false)
    }

    /// Undoes an interrupted restore: files still at the restored version go
    /// back to the variation the user was on; files edited since the failure
    /// are left alone. A targeted restore also points HEAD back at that
    /// variation. `None` when the restore save already landed on the current
    /// variation, which only a new restore undoes.
    fn rollback_restore_recovery(&self, state: &RecoveryState) -> Result<Option<bool>> {
        let Some(target) = state.target.as_deref() else {
            return Ok(None);
        };
        // Targeted restores record "VERSION -> VARIATION".
        let (version, variation) = match target.split_once(" -> ") {
            Some((version, variation)) => (version, Some(variation)),
            None => (target, None),
        };
        let Ok(version) = VersionId::from_canonical_string(version) else {
            return Ok(None);
        };
        let original = match (variation, state.original_variation.as_deref()) {
            (Some(_), Some(original)) => Some(validate_variation_name(original)?),
            (Some(_), None) => return Ok(None),
            (None, _) => None,
        };
        let home = match &original {
            Some(name) => self.branch_tree(name)?,
            None => self.repo.head()?.peel_to_tree()?,
        };
        let (before, restored) = match self.recorded_checkout() {
            Some((before, restored)) => (Some(before), restored),
            None => {
                // Older records: the restore built on its target's tip,
                // whether or not its save landed.
                let source = self.relocated_tree(&self.find_version_commit(&version)?)?;
                let base = match variation.map(|name| self.branch_tree(name)) {
                    Some(Ok(tree)) => tree,
                    _ => self.repo.head()?.peel_to_tree()?,
                };
                let restored = self.compose_scoped_tree(&base, &source)?;
                (None, self.repo.find_tree(restored)?)
            }
        };
        // A targeted restore can legitimately match its original variation,
        // and still has HEAD to point back.
        if original.is_none() && restored.id() == home.id() {
            return Ok(None);
        }
        Ok(Some(match original {
            Some(original) => self.settle_onto_branch(&restored, before.as_ref(), &original)?,
            None => self.settle_from(&restored, before.as_ref(), &home)?,
        }))
    }

    /// Settles an interrupted switch onto the `to` variation's current tip,
    /// then points HEAD at it. Files and index entries may sit at either
    /// recorded tree; older records fall back to `from`'s tip (a variation,
    /// or HEAD).
    fn finish_switch_at(&self, from: Option<&str>, to: &str) -> Result<bool> {
        let to = validate_variation_name(to)?;
        match self.recorded_checkout() {
            Some((before, after)) => self.settle_onto_branch(&after, Some(&before), &to),
            None => {
                let from_tree = match from {
                    Some(name) => self.branch_tree(&validate_variation_name(name)?)?,
                    None => self.repo.head()?.peel_to_tree()?,
                };
                self.settle_onto_branch(&from_tree, None, &to)
            }
        }
    }

    /// Settles an interrupted checkout onto `branch`'s tip and points
    /// HEAD at it, following the branch if another Git process moves it
    /// meanwhile, so HEAD never ends at a tip the files don't match.
    fn settle_onto_branch(
        &self,
        written: &Tree<'_>,
        before: Option<&Tree<'_>>,
        branch: &str,
    ) -> Result<bool> {
        let mut tip = self.branch_tree(branch)?;
        let mut changed = self.settle_from(written, before, &tip)?;
        changed |= self.point_head_at(branch)?;
        for _ in 0..5 {
            let now = self.branch_tree(branch)?;
            if now.id() == tip.id() {
                return Ok(changed);
            }
            changed |= self.settle_paths(&tip, &now, &BTreeSet::new(), &BTreeSet::new())?;
            tip = now;
        }
        Err(DraftlineError::LocalStateChanged {
            expected: tip.id().to_string(),
            actual: self.branch_tree(branch)?.id().to_string(),
        })
    }

    /// Settles an interrupted checkout from `before` to `written` onto `to`:
    /// paths at `written` first, then paths the checkout never reached.
    fn settle_from(
        &self,
        written: &Tree<'_>,
        before: Option<&Tree<'_>>,
        to: &Tree<'_>,
    ) -> Result<bool> {
        // Half-written files go the way of `written`; read before anything moves.
        // `written` → `to` covers an earlier recovery that died following a
        // branch that moved.
        let mut torn = match before {
            Some(before) if before.id() != written.id() => self.torn_writes(before, written)?,
            _ => BTreeSet::new(),
        };
        if written.id() != to.id() {
            torn.extend(self.torn_writes(written, to)?);
        }
        let before = before.filter(|before| ![written.id(), to.id()].contains(&before.id()));
        // Git writes the index last, so an entry already at `written` proves
        // the checkout wrote that file: one back at `before` is the
        // developer's edit. Read before anything moves the index. A path the
        // developer also staged back to `before` looks untouched and is
        // finished; its content stays in `before`'s commit.
        let mut proven = BTreeSet::new();
        if let Some(before) = before {
            self.while_git_busy(|| self.ensure_git_index_unlocked())?;
            let mut index = self.repo.index()?;
            index.read(true)?;
            for path in self.paths_changed_between(Some(before), written)? {
                let staged = index.get_path(&path, 0).map(|entry| (entry.id, entry.mode));
                let in_written = written
                    .get_path(&path)
                    .ok()
                    .map(|entry| (entry.id(), entry.filemode() as u32));
                if staged == in_written {
                    proven.insert(path);
                }
            }
        }
        let mut changed = false;
        if written.id() != to.id() {
            changed |= self.settle_paths(written, to, &BTreeSet::new(), &torn)?;
        } else {
            changed |= self.write_torn(to, &torn)?;
        }
        if let Some(before) = before {
            changed |= self.settle_paths(before, to, &proven, &BTreeSet::new())?;
        }
        Ok(changed)
    }

    /// Paths an interrupted checkout from `before` toward `written` left
    /// half-written: missing, or holding only the start of `written`'s
    /// content. Rewriting them loses nothing, since both are in `written`.
    /// Git writes the index last, so a path already staged at `written` was
    /// finished and anything since is the developer's.
    fn torn_writes(&self, before: &Tree<'_>, written: &Tree<'_>) -> Result<BTreeSet<PathBuf>> {
        let paths = self.paths_changed_between(Some(before), written)?;
        self.while_git_busy(|| self.ensure_git_index_unlocked())?;
        let mut index = self.repo.index()?;
        index.read(true)?;
        let settled: BTreeSet<PathBuf> = self
            .workdir_matches(written, &paths)?
            .into_iter()
            .chain(self.workdir_matches(before, &paths)?)
            .collect();
        let mut torn = BTreeSet::new();
        for path in paths.into_iter().filter(|path| !settled.contains(path)) {
            let Ok(entry) = written.get_path(&path) else {
                continue;
            };
            let staged = index
                .get_path(&path, 0)
                .map(|staged| (staged.id, staged.mode));
            if staged == Some((entry.id(), entry.filemode() as u32)) {
                continue;
            }
            let on_disk = match fs::symlink_metadata(self.root.join(&path)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    torn.insert(path);
                    continue;
                }
                Ok(metadata) if metadata.is_file() => fs::read(self.root.join(&path))?,
                _ => continue,
            };
            let Ok(blob) = self.repo.find_blob(entry.id()) else {
                continue;
            };
            if is_start_of(&on_disk, blob.content()) {
                torn.insert(path);
            }
        }
        Ok(torn)
    }

    /// Writes `paths` from `tree` over whatever is on disk.
    fn write_torn(&self, tree: &Tree<'_>, paths: &BTreeSet<PathBuf>) -> Result<bool> {
        if paths.is_empty() {
            return Ok(false);
        }
        let (present, gone): (Vec<&PathBuf>, Vec<&PathBuf>) =
            paths.iter().partition(|path| tree.get_path(path).is_ok());
        for path in gone {
            remove_workspace_path_if_exists(&self.root.join(path))?;
        }
        if !present.is_empty() {
            let mut checkout = CheckoutBuilder::new();
            checkout
                .force()
                .update_index(false)
                .disable_pathspec_match(true);
            for path in &present {
                checkout.path(path.to_string_lossy().replace('\\', "/"));
            }
            let refused = RefCell::default();
            self.checkout_refusing_filters(checkout, &refused, tree, |checkout| {
                self.repo.checkout_tree(tree.as_object(), Some(checkout))
            })?;
        }
        Ok(true)
    }

    /// HEAD's tree, or the empty tree while HEAD is unborn (a fresh clone).
    fn head_tree_or_empty(&self) -> Result<Tree<'_>> {
        match self.repo.head().and_then(|head| head.peel_to_tree()) {
            Ok(tree) => Ok(tree),
            Err(_) => Ok(self.repo.find_tree(self.empty_tree_id()?)?),
        }
    }

    fn empty_tree_id(&self) -> Result<Oid> {
        Ok(self.repo.treebuilder(None)?.write()?)
    }

    fn branch_tree(&self, variation: &str) -> Result<Tree<'_>> {
        Ok(self
            .repo
            .find_branch(variation, BranchType::Local)?
            .into_reference()
            .peel_to_tree()?)
    }

    /// Points HEAD at `variation`; `false` when it already is.
    fn point_head_at(&self, variation: &str) -> Result<bool> {
        if self.current_variation_unchecked().ok().as_deref() == Some(variation) {
            return Ok(false);
        }
        let reference = format!("refs/heads/{variation}");
        self.while_git_busy(|| Ok(self.repo.set_head(&reference)?))?;
        Ok(true)
    }

    /// Moves an interrupted checkout from `from` the rest of the way to `to`,
    /// in or out of scope: a working file still at `from` ends at `to`, and
    /// an index entry still at `from` moves to `to`. Files edited since stay
    /// as they are (showing as unsaved changes) and entries a developer
    /// staged differently are left alone, so rerunning it is safe.
    ///
    /// Working files at `kept` are the developer's and stay as they are;
    /// those at `torn` count as still at `from`. Like
    /// Git, this writes files first and the index last. Returns whether any
    /// file was rewritten.
    fn settle_paths(
        &self,
        from: &Tree<'_>,
        to: &Tree<'_>,
        kept: &BTreeSet<PathBuf>,
        torn: &BTreeSet<PathBuf>,
    ) -> Result<bool> {
        let paths = self.paths_changed_between(Some(from), to)?;
        // Blob and mode, so a staged mode-only change counts as the developer's.
        let entry_in = |tree: &Tree<'_>, path: &Path| {
            tree.get_path(path)
                .ok()
                .map(|entry| (entry.id(), entry.filemode() as u32))
        };
        let staged = |index: &git2::Index, path: &Path| {
            index.get_path(path, 0).map(|entry| (entry.id, entry.mode))
        };
        // Wait out Git first so nothing is written while it holds the index.
        self.while_git_busy(|| self.ensure_git_index_unlocked())?;
        test_hooks::fire("before_settle_files", self.repo.path());
        // Read as late as possible so an edit made meanwhile is kept; what
        // remains is the checkout's own window, as with `git switch`.
        let at_from: Vec<PathBuf> = self
            .workdir_matches(from, &paths)?
            .into_iter()
            .chain(paths.iter().filter(|path| torn.contains(*path)).cloned())
            .filter(|path| !kept.contains(path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let (in_to, gone): (Vec<_>, Vec<_>) = at_from
            .iter()
            .cloned()
            .partition(|path| to.get_path(path).is_ok());
        for path in &gone {
            remove_workspace_path_if_exists(&self.root.join(path))?;
        }
        if !in_to.is_empty() {
            // The index is written below, so the checkout never takes Git's lock.
            let mut checkout = CheckoutBuilder::new();
            checkout
                .force()
                .update_index(false)
                .disable_pathspec_match(true);
            for path in &in_to {
                checkout.path(path.to_string_lossy().replace('\\', "/"));
            }
            let refused = RefCell::default();
            self.checkout_refusing_filters(checkout, &refused, to, |checkout| {
                self.repo.checkout_tree(to.as_object(), Some(checkout))
            })?;
        }
        self.while_git_busy(|| {
            let mut index = self.repo.index()?;
            index.read(true)?;
            let index_paths: Vec<PathBuf> = paths
                .iter()
                .filter(|path| staged(&index, path) == entry_in(from, path))
                .cloned()
                .collect();
            drop(index);
            if !index_paths.is_empty() {
                self.write_index_from_tree(to, &index_paths, &[])?;
            }
            Ok(())
        })?;
        Ok(!at_from.is_empty())
    }

    /// Paths whose blobs differ between two trees.
    fn paths_changed_between(
        &self,
        old: Option<&Tree<'_>>,
        new: &Tree<'_>,
    ) -> Result<Vec<PathBuf>> {
        let diff = self.repo.diff_tree_to_tree(old, Some(new), None)?;
        let mut paths = BTreeSet::new();
        for delta in diff.deltas() {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    paths.insert(path.to_path_buf());
                }
            }
        }
        Ok(paths.into_iter().collect())
    }

    /// The subset of `paths` whose working files (after Git filters) are
    /// exactly as in `tree`, including paths absent from both.
    fn workdir_matches(&self, tree: &Tree<'_>, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut options = git2::DiffOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .disable_pathspec_match(true);
        for path in paths {
            options.pathspec(path.to_string_lossy().replace('\\', "/"));
        }
        // A scratch index of `tree` has no stat cache, so every file is hashed
        // through Git's filters instead of trusting the real index's timestamps.
        let mut expected = git2::Index::new()?;
        expected.read_tree(tree)?;
        let diff = self
            .repo
            .diff_index_to_workdir(Some(&expected), Some(&mut options))?;
        let mut differing = BTreeSet::new();
        for delta in diff.deltas() {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    differing.insert(path.to_path_buf());
                }
            }
        }
        // libgit2 hashes filtered files raw; Git's clean filter decides.
        let candidates = differing
            .iter()
            .filter_map(|path| {
                let entry = tree.get_path(path).ok()?;
                (entry.kind() == Some(ObjectType::Blob)).then(|| (path.clone(), entry.id()))
            })
            .collect::<Vec<_>>();
        for path in self.filter_clean(&candidates)? {
            differing.remove(&path);
        }
        Ok(paths
            .iter()
            .filter(|path| !differing.contains(*path))
            .cloned()
            .collect())
    }

    /// Those of `candidates` (path, stored blob) whose working file Git's
    /// clean filter turns into exactly that blob, for drivers Draftline runs.
    /// libgit2 hashes such files raw, so it reports them as changed. Any
    /// failure just leaves them reported as changed.
    fn filter_clean(&self, candidates: &[(PathBuf, Oid)]) -> Result<Vec<PathBuf>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let runner = FilterRunner::of(&self.repo)?;
        let mut filtered = Vec::new();
        for (path, id) in candidates {
            if self.filter_use(&runner, None, path)? == FilterUse::Runs
                && fs::symlink_metadata(self.root.join(path)).is_ok_and(|meta| meta.is_file())
            {
                filtered.push((path.clone(), *id));
            }
        }
        if filtered.is_empty() {
            return Ok(Vec::new());
        }
        let paths = filtered
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let Ok(ids) = git_cli::clean(&self.root, &paths, false) else {
            return Ok(Vec::new());
        };
        Ok(filtered
            .into_iter()
            .zip(ids)
            .filter(|((_, expected), id)| expected == id)
            .map(|((path, _), _)| path)
            .collect())
    }

    /// Acknowledges an incomplete recovery record and allows normal operations again.
    pub fn acknowledge_recovery(&self) -> Result<()> {
        self.clear_stale_lock()?;
        let _lock = self.acquire_operation_lock("acknowledge_recovery")?;
        self.ensure_recovery_is_ours()?;
        self.clear_recovery_records()
    }

    fn legacy_pending_cleanup_path(&self) -> PathBuf {
        self.draftline_dir()
            .join("history-cleanup")
            .join("pending.json")
    }

    /// Reads 0.5's pending history cleanups, skipping any it can't parse:
    /// they are informational, so a damaged file never blocks the folder.
    fn read_legacy_pending_cleanups(&self) -> Result<Vec<LegacyPendingCleanupRecord>> {
        let bytes = match fs::read(self.legacy_pending_cleanup_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let Ok(raw) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
            tracing::warn!("skipping unreadable 0.5 history cleanup records");
            return Ok(Vec::new());
        };
        Ok(raw
            .into_iter()
            .filter_map(|value| {
                serde_json::from_value(value)
                    .inspect_err(|error| {
                        tracing::warn!(%error, "skipping unreadable 0.5 history cleanup record")
                    })
                    .ok()
            })
            .collect())
    }

    /// Lists history cleanups that Draftline 0.5 applied locally but never
    /// published. Current Draftline ignores them otherwise: the branch keeps
    /// its rewritten history and the next publish merges the remote in.
    pub fn legacy_history_cleanups(&self) -> Result<Vec<LegacyHistoryCleanup>> {
        Ok(self
            .read_legacy_pending_cleanups()?
            .into_iter()
            .map(|record| LegacyHistoryCleanup {
                plan_id: record.plan_id,
                variation: record.target_variation,
                original_head: record.expected_local_head,
                replacement_head: record.replacement_head,
                backup_refs: record.backup_refs,
            })
            .collect())
    }

    /// Dismisses a history cleanup left pending by Draftline 0.5. Only the
    /// record is dropped: no branch moves and no file changes. The original
    /// history stays in the cleanup's backup refs, which remain listed as
    /// support refs.
    pub fn dismiss_legacy_history_cleanup(&self, plan_id: impl AsRef<str>) -> Result<()> {
        let plan_id = plan_id.as_ref();
        let _lock = self.acquire_operation_lock("dismiss_legacy_history_cleanup")?;
        self.ensure_no_pending_recovery()?;
        if !self
            .read_legacy_pending_cleanups()?
            .iter()
            .any(|record| record.plan_id == plan_id)
        {
            return Err(DraftlineError::RecoveryNotFound(plan_id.to_string()));
        }
        let path = self.legacy_pending_cleanup_path();
        let raw: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(&path)?)?;
        let remaining = raw
            .into_iter()
            .filter(|value| value.get("plan_id").and_then(|id| id.as_str()) != Some(plan_id))
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            fs::remove_file(&path)?;
        } else {
            let temp = unique_claim_temp(&path);
            fs::write(&temp, serde_json::to_vec_pretty(&remaining)?)?;
            replace_file(&temp, &path)?;
        }
        Ok(())
    }

    /// Deletes recovery records; the caller holds the operation lock.
    fn clear_recovery_records(&self) -> Result<()> {
        if self.draftline_dir().exists() {
            for entry in fs::read_dir(self.draftline_dir())? {
                let entry = entry?;
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name.starts_with("recovery-delete-remote-") && file_name.ends_with(".json")
                {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        if self.ledger_path().exists() {
            fs::remove_file(self.ledger_path())?;
        }
        Ok(())
    }

    /// Resolves a workspace-relative path safely.
    pub fn resolve_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self.root.join(normalize_workspace_relative(path)?))
    }

    /// Saves the current workspace content as a named version.
    pub fn save_version(&self, label: impl AsRef<str>) -> Result<Version> {
        let _lock = self.acquire_for_index("save_version")?;
        self.ensure_no_pending_recovery()?;
        self.save_version_unchecked(label, None)
    }

    /// Saves the current workspace content using host-supplied attribution.
    pub fn save_version_with_profile(
        &self,
        label: impl AsRef<str>,
        profile: &ContributorProfile,
    ) -> Result<Version> {
        let _lock = self.acquire_for_index("save_version")?;
        self.ensure_no_pending_recovery()?;
        self.save_version_unchecked(label, Some(profile))
    }

    fn save_version_unchecked(
        &self,
        label: impl AsRef<str>,
        profile: Option<&ContributorProfile>,
    ) -> Result<Version> {
        let branch = self.ensure_on_branch()?;
        test_hooks::fire("after_branch_pinned", self.repo.path());
        self.while_head_moves_forward(|retrying| {
            let changed_files = self.changed_files_unchecked()?;
            if retrying && changed_files.is_empty() {
                return Ok(None);
            }
            self.commit_content_changes(
                &branch,
                &changed_files,
                "save_version",
                label.as_ref(),
                profile,
            )
            .map(Some)
        })
    }

    /// Preflights saving only selected workspace-relative tracked content files.
    pub fn preflight_save_files<I, P>(&self, paths: I) -> Result<PreflightReport>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
        self.ensure_on_branch()?;
        let changed_files = self.selected_changed_files(paths)?;
        Ok(selected_files_preflight_report(
            "save_files",
            false,
            changed_files,
        ))
    }

    /// Saves only selected workspace-relative tracked content files as a named version.
    pub fn save_files<I, P>(&self, paths: I, label: impl AsRef<str>) -> Result<Version>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.save_files_with_optional_profile(paths, label, None)
    }

    /// Saves only selected tracked content files using host-supplied attribution.
    pub fn save_files_with_profile<I, P>(
        &self,
        paths: I,
        label: impl AsRef<str>,
        profile: &ContributorProfile,
    ) -> Result<Version>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.save_files_with_optional_profile(paths, label, Some(profile))
    }

    fn save_files_with_optional_profile<I, P>(
        &self,
        paths: I,
        label: impl AsRef<str>,
        profile: Option<&ContributorProfile>,
    ) -> Result<Version>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let _lock = self.acquire_for_index("save_files")?;
        self.ensure_no_pending_recovery()?;
        let branch = self.ensure_on_branch()?;
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        self.while_head_moves_forward(|retrying| {
            let changed_files = self.selected_changed_files(&paths)?;
            if retrying && changed_files.is_empty() {
                return Ok(None);
            }
            if changed_files.is_empty() {
                return Err(DraftlineError::PreflightFailed(Box::new(
                    selected_files_preflight_report("save_files", false, changed_files),
                )));
            }
            self.commit_content_changes(
                &branch,
                &changed_files,
                "save_files",
                label.as_ref(),
                profile,
            )
            .map(Some)
        })
    }

    /// Reruns a save whose branch a developer committed onto meanwhile, as
    /// pressing Save again would. A switched, reset, or rewound branch still
    /// refuses, since the save would then land somewhere the user didn't pick.
    /// A rerun with nothing left to save (`Ok(None)`, say because the
    /// developer's commit took the same edits) keeps the original refusal.
    fn while_head_moves_forward<T>(
        &self,
        mut save: impl FnMut(bool) -> Result<Option<T>>,
    ) -> Result<T> {
        let deadline = std::time::Instant::now() + GIT_BUSY_WAIT;
        let mut refusal = None;
        loop {
            let before = self.head_position();
            match save(refusal.is_some()) {
                Ok(Some(saved)) => return Ok(saved),
                Ok(None) => {
                    return Err(refusal.expect("only a rerun reports nothing left to save"));
                }
                Err(error @ DraftlineError::LocalStateChanged { .. })
                    if std::time::Instant::now() < deadline && self.head_moved_forward(&before) =>
                {
                    refusal.get_or_insert(error);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn head_position(&self) -> (Option<String>, Option<Oid>) {
        let name = self
            .repo
            .find_reference("HEAD")
            .ok()
            .and_then(|head| head.symbolic_target().ok().flatten().map(str::to_string));
        let tip = self.repo.head().ok().and_then(|head| head.target());
        (name, tip)
    }

    fn head_moved_forward(&self, before: &(Option<String>, Option<Oid>)) -> bool {
        let now = self.head_position();
        if now.0 != before.0 {
            return false;
        }
        match (before.1, now.1) {
            (Some(old), Some(new)) => {
                new != old && self.repo.graph_descendant_of(new, old).unwrap_or(false)
            }
            (None, Some(_)) => true,
            _ => false,
        }
    }

    /// Lists versions reachable from the current variation, newest first.
    ///
    /// In a folder inside a larger repository, only commits that change
    /// the folder are versions (like `git log -- <folder>`); developers' other
    /// commits are skipped.
    pub fn versions(&self) -> Result<Vec<Version>> {
        self.ensure_no_pending_recovery()?;
        self.versions_unchecked()
    }

    /// Returns true when the workspace has unsaved content changes.
    pub fn is_dirty(&self) -> Result<bool> {
        Ok(!self.changed_files()?.is_empty())
    }

    /// Lists changed content files in the workspace.
    pub fn changed_files(&self) -> Result<Vec<ChangedFile>> {
        self.ensure_no_pending_recovery()?;
        self.changed_files_unchecked()
    }

    fn changed_files_unchecked(&self) -> Result<Vec<ChangedFile>> {
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true);
        limit_status_to_scope(&mut options, self.scope());

        let statuses = self.repo.statuses(Some(&mut options))?;
        let filter_clean = self.filter_clean_in_index(&statuses)?;
        let mut changed = Vec::new();

        for entry in statuses.iter() {
            let Ok(path) = entry.path() else {
                continue;
            };
            if !self.content_policy.tracks(path)? || filter_clean.contains(Path::new(path)) {
                continue;
            }

            let relative = PathBuf::from(path);
            let full_path = self.root.join(&relative);
            changed.push(ChangedFile {
                path: relative,
                kind: status_to_change_kind(entry.status()),
                is_binary: file_is_binary(&full_path)?,
                is_large: file_is_large(
                    &full_path,
                    self.content_policy.large_file_threshold_bytes(),
                )?,
            });
        }

        changed.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(changed)
    }

    /// Paths `statuses` reports as edited only in the working tree that are
    /// really unchanged: filtered files whose clean output is their index
    /// entry (see [`Self::filter_clean`]).
    fn filter_clean_in_index(&self, statuses: &git2::Statuses<'_>) -> Result<HashSet<PathBuf>> {
        let edited = statuses
            .iter()
            .filter(|entry| entry.status() == Status::WT_MODIFIED)
            .filter_map(|entry| entry.path().ok().map(PathBuf::from))
            .collect::<Vec<_>>();
        if edited.is_empty() {
            return Ok(HashSet::new());
        }
        let index = self.repo.index()?;
        let candidates = edited
            .into_iter()
            .filter_map(|path| {
                let id = index.get_path(&path, 0)?.id;
                Some((path, id))
            })
            .collect::<Vec<_>>();
        Ok(self.filter_clean(&candidates)?.into_iter().collect())
    }

    fn policy_git_diagnostics_unchecked(&self) -> Result<Vec<WorkspaceDiagnostic>> {
        self.policy_git_diagnostics_for_policy(&self.content_policy)
    }

    fn policy_git_diagnostics_for_policy(
        &self,
        policy: &ContentPolicy,
    ) -> Result<Vec<WorkspaceDiagnostic>> {
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .include_ignored(true)
            .recurse_untracked_dirs(true)
            .recurse_ignored_dirs(true);
        limit_status_to_scope(&mut options, policy.scope());

        let statuses = self.repo.statuses(Some(&mut options))?;
        let mut ignored_paths = Vec::new();

        for entry in statuses.iter() {
            if !entry.status().contains(Status::IGNORED) {
                continue;
            }

            let Ok(path) = entry.path() else {
                continue;
            };

            if policy.tracks(path)? {
                ignored_paths.push(path.to_string());
            }
        }

        ignored_paths.sort();
        ignored_paths.dedup();

        Ok(ignored_paths
            .into_iter()
            .map(|path| {
                workspace_diagnostic(
                    DiagnosticCode::PolicyTrackedFileIgnored,
                    DiagnosticSeverity::Warning,
                    format!("content policy tracks `{path}`, but Git ignore rules hide it"),
                )
            })
            .collect())
    }

    fn target_tree_hazards(&self, target_tree: &Tree<'_>) -> Result<Vec<FileHazard>> {
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .include_ignored(true)
            .recurse_untracked_dirs(true)
            .recurse_ignored_dirs(true);

        // Only paths the target actually changes can be overwritten, so scan
        // just those instead of walking the whole (possibly huge) repository.
        let head_tree = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_tree().ok());
        let diff = self
            .repo
            .diff_tree_to_tree(head_tree.as_ref(), Some(target_tree), None)?;
        let mut target_paths = BTreeSet::new();
        let mut written = Vec::new();
        for delta in diff.deltas() {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    target_paths.insert(path.to_string_lossy().replace('\\', "/"));
                }
            }
            if let Some(path) = delta
                .new_file()
                .path()
                .filter(|_| delta.new_file().exists())
            {
                written.push(path.to_path_buf());
            }
        }
        if target_paths.is_empty() {
            return Ok(Vec::new());
        }
        options.disable_pathspec_match(true);
        for path in &target_paths {
            options.pathspec(path);
        }

        let statuses = self.repo.statuses(Some(&mut options))?;
        let local_change = Status::WT_MODIFIED
            | Status::WT_DELETED
            | Status::WT_TYPECHANGE
            | Status::WT_RENAMED
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_TYPECHANGE
            | Status::INDEX_RENAMED;
        let mut hazards = Vec::new();

        for entry in statuses.iter() {
            let Ok(path) = entry.path() else {
                continue;
            };
            // Draftline owns its marker; checkout handles an unsaved one.
            if path == FOLDER_MARKER {
                continue;
            }
            let path_ref = Path::new(path);
            let status = entry.status();

            // Uncommitted edits outside the content policy (developer code) are
            // never overwritten: flag them when the target changes that path.
            if status.intersects(local_change) && !status.is_wt_new() && !status.is_index_new() {
                if self.content_policy.tracks(path)? {
                    continue;
                }
                let head_entry = match head_tree.as_ref() {
                    Some(tree) => optional_tree_entry(tree, path_ref)?,
                    None => None,
                };
                if head_entry != optional_tree_entry(target_tree, path_ref)? {
                    hazards.push(FileHazard {
                        path: PathBuf::from(path),
                        kind: FileHazardKind::LocalChanges,
                    });
                }
                continue;
            }

            if !tree_contains_path(target_tree, path_ref)? {
                continue;
            }

            let kind = if status.contains(Status::IGNORED) {
                Some(FileHazardKind::Ignored)
            } else if status.is_wt_new() || status.is_index_new() {
                if self.content_policy.tracks(path)? {
                    None
                } else {
                    Some(FileHazardKind::PolicyExcluded)
                }
            } else {
                None
            };

            if let Some(kind) = kind {
                hazards.push(FileHazard {
                    path: PathBuf::from(path),
                    kind,
                });
            }
        }

        // Safe checkout also refuses file/folder obstructions the status scan
        // cannot see, so flag those up front instead of failing mid-operation.
        for path in self.obstructed_paths(head_tree.as_ref(), target_tree, &target_paths)? {
            if hazards.iter().any(|hazard| hazard.path == path)
                || path == Path::new(FOLDER_MARKER)
                || self.content_policy.tracks(&path)?
            {
                continue;
            }
            let kind = if self.repo.is_path_ignored(&path)? {
                FileHazardKind::Ignored
            } else {
                FileHazardKind::PolicyExcluded
            };
            hazards.push(FileHazard { path, kind });
        }
        hazards.extend(self.filtered_hazards_toward(&written, Some(target_tree))?);

        hazards.sort_by(|left, right| left.path.cmp(&right.path));
        hazards.dedup_by(|left, right| left.path == right.path);
        Ok(hazards)
    }

    /// Untracked or ignored paths that block the target's files or folders: a
    /// file where the target needs a folder, or files inside a folder the
    /// target turns into a file.
    fn obstructed_paths(
        &self,
        head_tree: Option<&Tree<'_>>,
        target_tree: &Tree<'_>,
        target_paths: &BTreeSet<String>,
    ) -> Result<Vec<PathBuf>> {
        let Some(workdir) = self.repo.workdir() else {
            return Ok(Vec::new());
        };
        let in_head = |path: &Path| -> Result<bool> {
            match head_tree {
                Some(tree) => tree_contains_path(tree, path),
                None => Ok(false),
            }
        };
        let mut obstructed = BTreeSet::new();
        for path in target_paths {
            let path = Path::new(path);
            if !tree_contains_path(target_tree, path)? {
                continue;
            }
            for ancestor in path.ancestors().skip(1) {
                if ancestor.as_os_str().is_empty() {
                    break;
                }
                let on_disk = workdir.join(ancestor);
                if fs::symlink_metadata(&on_disk).is_ok_and(|meta| !meta.is_dir())
                    && !in_head(ancestor)?
                {
                    obstructed.insert(ancestor.to_path_buf());
                }
            }
            let on_disk = workdir.join(path);
            if fs::symlink_metadata(&on_disk).is_ok_and(|meta| meta.is_dir()) {
                let mut pending = vec![on_disk];
                while let Some(dir) = pending.pop() {
                    for entry in fs::read_dir(&dir)? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            pending.push(entry.path());
                            continue;
                        }
                        let relative = entry
                            .path()
                            .strip_prefix(workdir)
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|_| entry.path());
                        if !in_head(&relative)? {
                            obstructed.insert(relative);
                        }
                    }
                }
            }
        }
        Ok(obstructed
            .into_iter()
            .map(|path| PathBuf::from(path.to_string_lossy().replace('\\', "/")))
            .collect())
    }

    /// Returns content changes and an optional textual diff of unsaved workspace changes.
    pub fn changes(&self) -> Result<ChangeSet> {
        self.ensure_no_pending_recovery()?;
        self.changes_unchecked()
    }

    fn changes_unchecked(&self) -> Result<ChangeSet> {
        Ok(ChangeSet {
            files: self.changed_files_unchecked()?,
            diff: Some(self.diff_unsaved_text()?),
        })
    }

    /// Preflights discarding all unsaved tracked content changes without mutating files.
    pub fn preflight_discard_changes(&self) -> Result<PreflightReport> {
        self.ensure_no_pending_recovery()?;
        self.discard_preflight("discard_changes", self.changed_files_unchecked()?)
    }

    /// Discards all unsaved changes tracked by the active content policy.
    ///
    /// Files excluded by [`ContentPolicy`] are preserved. Added tracked files are
    /// removed; modified, deleted, renamed, type-changed, and conflicted tracked
    /// files are restored from the current `HEAD`.
    pub fn discard_changes(&self) -> Result<ChangeSet> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("discard_changes")?;
        let changed_files = self.changed_files_unchecked()?;
        if changed_files.is_empty() {
            return Ok(ChangeSet {
                files: Vec::new(),
                diff: None,
            });
        }

        self.ensure_discard_unfiltered(&changed_files, "discard_changes")?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::DiscardChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: None,
            completed: false,
        })?;

        self.discard_changed_files(&changed_files)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::DiscardChanges,
            original_variation: None,
            target: None,
            completed: true,
        })?;

        Ok(ChangeSet {
            files: changed_files,
            diff: None,
        })
    }

    /// Preflights discarding one workspace-relative tracked content file.
    pub fn preflight_discard_file(&self, path: impl AsRef<Path>) -> Result<PreflightReport> {
        self.ensure_no_pending_recovery()?;
        let path = self.normalize_tracked_content_path(path)?;
        let changed_files = self
            .changed_files_unchecked()?
            .into_iter()
            .filter(|changed| changed.path == path)
            .collect();

        self.discard_preflight("discard_file", changed_files)
    }

    /// Discards one changed tracked content file by workspace-relative path.
    ///
    /// The path is normalized and must stay inside the workspace and inside the
    /// active content policy. Excluded runtime files are rejected instead of
    /// being reset.
    pub fn discard_file(&self, path: impl AsRef<Path>) -> Result<Option<ChangedFile>> {
        self.ensure_no_pending_recovery()?;
        let path = self.normalize_tracked_content_path(path)?;
        let _lock = self.acquire_for_index("discard_file")?;
        let changed_file = self
            .changed_files_unchecked()?
            .into_iter()
            .find(|changed| changed.path == path);

        let Some(changed_file) = changed_file else {
            return Ok(None);
        };
        self.ensure_discard_unfiltered(std::slice::from_ref(&changed_file), "discard_file")?;

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::DiscardFile,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(path.to_string_lossy().into_owned()),
            completed: false,
        })?;

        self.discard_changed_files(std::slice::from_ref(&changed_file))?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::DiscardFile,
            original_variation: None,
            target: Some(path.to_string_lossy().into_owned()),
            completed: true,
        })?;

        Ok(Some(changed_file))
    }

    /// Preflights discarding selected workspace-relative tracked content files.
    pub fn preflight_discard_files<I, P>(&self, paths: I) -> Result<PreflightReport>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
        self.discard_preflight("discard_files", self.selected_changed_files(paths)?)
    }

    /// Discards selected changed tracked content files.
    pub fn discard_files<I, P>(&self, paths: I) -> Result<ChangeSet>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
        let changed_files = self.selected_changed_files(paths)?;
        if changed_files.is_empty() {
            return Ok(ChangeSet {
                files: Vec::new(),
                diff: None,
            });
        }

        let _lock = self.acquire_for_index("discard_files")?;
        self.ensure_discard_unfiltered(&changed_files, "discard_files")?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::DiscardChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(
                changed_files
                    .iter()
                    .map(|changed| changed.path.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            completed: false,
        })?;

        self.discard_changed_files(&changed_files)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::DiscardChanges,
            original_variation: None,
            target: None,
            completed: true,
        })?;

        Ok(ChangeSet {
            files: changed_files,
            diff: None,
        })
    }

    /// Preflights switching to another variation without mutating workspace files.
    pub fn preflight_switch_variation(&self, variation: &VariationId) -> Result<PreflightReport> {
        self.ensure_no_pending_recovery()?;
        self.preflight_switch_variation_unchecked(variation)
    }

    fn preflight_switch_variation_unchecked(
        &self,
        variation: &VariationId,
    ) -> Result<PreflightReport> {
        self.ensure_variation_switchable(variation.as_str())?;
        let change_set = self.changes_unchecked()?;
        let target_commit = self
            .repo
            .find_branch(variation.as_str(), BranchType::Local)?
            .get()
            .peel_to_commit()?;
        let target_tree = target_commit.tree()?;
        let file_hazards = self.target_tree_hazards(&target_tree)?;
        Ok(preflight_report(
            "switch_variation",
            true,
            change_set.files,
            file_hazards,
            Some(format!("current -> {}", variation.as_str())),
        ))
    }

    /// Starts a variation at the current commit and moves this checkout onto
    /// it without touching any file or the index, like `git switch -c`.
    /// Unsaved edits come along. This is the way out when a developer
    /// switched the shared checkout to their own branch: their branch keeps
    /// its commits, and later saves land on the new variation instead.
    pub fn start_variation_here(&self, name: impl AsRef<str>) -> Result<Variation> {
        self.start_variation_here_with_metadata(name, VariationMetadata::default())
    }

    /// [`Self::start_variation_here`] with display metadata.
    pub fn start_variation_here_with_metadata(
        &self,
        name: impl AsRef<str>,
        metadata: VariationMetadata,
    ) -> Result<Variation> {
        let _lock = self.acquire_operation_lock("start_variation_here")?;
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(name.as_ref())?;
        self.ensure_variation_in_namespace(&name)?;
        let branch_ref = format!("refs/heads/{name}");
        if self.repo.find_reference(&branch_ref).is_ok() {
            return Err(DraftlineError::VariationAlreadyExists(name));
        }
        self.write_variation_metadata(&name, &metadata)?;
        if let Err(error) = self.branch_here_and_switch(&name, None) {
            self.clear_unowned_variation_metadata(&name);
            return Err(error);
        }
        Ok(variation_from_name(name.clone(), Some(&name), metadata))
    }

    /// Undoes metadata written for a branch this operation failed to create,
    /// unless someone else created that branch meanwhile.
    fn clear_unowned_variation_metadata(&self, name: &str) {
        if self
            .repo
            .find_reference(&format!("refs/heads/{name}"))
            .is_err()
        {
            let _ = self.clear_variation_metadata(name);
        }
    }

    /// Creates `name` at HEAD's commit and points HEAD at it, like
    /// `git switch -c`, without touching files. HEAD, the old branch, and the
    /// new one stay locked while HEAD is rechecked, so a developer's commit or
    /// switch meanwhile refuses and nothing is left behind. With `expected`,
    /// also refuses unless HEAD is still at that commit.
    fn branch_here_and_switch(&self, name: &str, expected: Option<Oid>) -> Result<()> {
        let branch_ref = format!("refs/heads/{name}");
        let read_head = || -> Result<(Option<String>, Option<Oid>)> {
            let head = self.repo.find_reference("HEAD")?;
            let symbolic = head.symbolic_target().ok().flatten().map(str::to_string);
            let commit = head.resolve().ok().and_then(|head| head.target());
            Ok((symbolic, commit))
        };
        let (previous, commit) = read_head()?;
        if expected.is_some() && commit != expected {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{expected:?}"),
                actual: format!("{commit:?}"),
            });
        }
        test_hooks::fire("before_branch_switch", self.repo.path());
        let mut committing = false;
        let message = format!("draftline: start variation {name} here");
        let recheck = || -> Result<()> {
            let now = read_head()?;
            if now != (previous.clone(), commit) {
                return Err(DraftlineError::LocalStateChanged {
                    expected: format!("{previous:?}@{commit:?}"),
                    actual: format!("{:?}@{:?}", now.0, now.1),
                });
            }
            Ok(())
        };
        let lock_head = |transaction: &mut git2::Transaction<'_>| -> Result<()> {
            transaction.lock_ref("HEAD")?;
            if let Some(previous) = previous.as_deref() {
                transaction.lock_ref(previous)?;
            }
            transaction.lock_ref(&branch_ref)?;
            Ok(())
        };
        let switched = (|| -> Result<()> {
            // libgit2 applies a transaction's refs in no set order, so create
            // the branch first and move HEAD after: a crash in between leaves
            // a spare branch, never HEAD on a branch that doesn't exist.
            let mut transaction = self.repo.transaction()?;
            lock_head(&mut transaction)?;
            recheck()?;
            if self.repo.find_reference(&branch_ref).is_ok() {
                return Err(DraftlineError::VariationAlreadyExists(name.to_string()));
            }
            if let Some(commit) = commit {
                transaction.set_target(&branch_ref, commit, None, &message)?;
                committing = true;
                transaction.commit()?;
                test_hooks::fire("between_branch_and_switch", self.repo.path());
                transaction = self.repo.transaction()?;
                lock_head(&mut transaction)?;
                recheck()?;
                if self.repo.refname_to_id(&branch_ref).ok() != Some(commit) {
                    return Err(DraftlineError::LocalStateChanged {
                        expected: format!("{branch_ref}@{commit}"),
                        actual: format!("{branch_ref} moved"),
                    });
                }
            }
            transaction.set_symbolic_target("HEAD", &branch_ref, None, &message)?;
            committing = true;
            transaction.commit()?;
            Ok(())
        })();
        let Err(error) = switched else {
            return Ok(());
        };
        if !committing {
            // Refused before writing anything; the refs aren't ours to touch.
            return Err(error);
        }
        // libgit2 applies a transaction's refs one by one, so a failed
        // commit may have applied some. Finish or undo what landed.
        let head_on_new = self
            .repo
            .find_reference("HEAD")
            .ok()
            .and_then(|head| head.symbolic_target().ok().flatten().map(str::to_string))
            .as_deref()
            == Some(branch_ref.as_str());
        let new_tip = self.repo.refname_to_id(&branch_ref).ok();
        if head_on_new && new_tip == commit {
            return Ok(());
        }
        if head_on_new {
            if let Some(previous) = previous.as_deref() {
                let _ = self.repo.reference_symbolic(
                    "HEAD",
                    previous,
                    true,
                    "draftline: undo start variation",
                );
            }
        } else if new_tip.is_some() && new_tip == commit {
            if let Ok(mut reference) = self.repo.find_reference(&branch_ref) {
                let _ = reference.delete();
            }
        }
        Err(error)
    }

    /// Creates a new variation from the current version. The workspace stays
    /// on the current variation; call [`Workspace::switch_variation`] to move.
    pub fn create_variation(&self, name: impl AsRef<str>) -> Result<Variation> {
        self.create_variation_with_metadata(name, VariationMetadata::default())
    }

    /// Creates a new variation with display metadata from the current version.
    pub fn create_variation_with_metadata(
        &self,
        name: impl AsRef<str>,
        metadata: VariationMetadata,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(name.as_ref())?;
        self.ensure_variation_in_namespace(&name)?;
        let head = self.repo.head()?.peel_to_commit()?;
        self.repo.branch(&name, &head, false)?;
        self.write_variation_metadata(&name, &metadata)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
            metadata,
        ))
    }

    /// Creates a variation from an older version without switching to it.
    pub fn create_variation_from(
        &self,
        version: &VersionId,
        name: impl AsRef<str>,
    ) -> Result<Variation> {
        self.create_variation_from_with_metadata(version, name, VariationMetadata::default())
    }

    /// Creates a variation with display metadata from an older version without switching to it.
    pub fn create_variation_from_with_metadata(
        &self,
        version: &VersionId,
        name: impl AsRef<str>,
        metadata: VariationMetadata,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(name.as_ref())?;
        self.ensure_variation_in_namespace(&name)?;
        let commit = self.find_version_commit(version)?;
        if !self.version_is_reachable_from_local_variation(commit.id())? {
            return Err(DraftlineError::VersionNotLocallyReachable(
                version.to_string(),
            ));
        }
        self.repo.branch(&name, &commit, false)?;
        self.write_variation_metadata(&name, &metadata)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
            metadata,
        ))
    }

    /// Preflights creating a local variation from a saved version after refreshing a remote namespace.
    pub fn preflight_create_variation_from_version(
        &self,
        version: &VersionId,
        name: impl AsRef<str>,
        remote: Option<&str>,
    ) -> Result<VariationCreatePreflight> {
        let mut options = RemoteOptions::new();
        self.preflight_create_variation_from_version_with_options(
            version,
            name,
            remote,
            &mut options,
        )
    }

    /// Preflights creating a local variation using explicit remote options.
    pub fn preflight_create_variation_from_version_with_options(
        &self,
        version: &VersionId,
        name: impl AsRef<str>,
        remote: Option<&str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<VariationCreatePreflight> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(name.as_ref())?;
        self.ensure_variation_in_namespace(&name)?;
        let commit = self.find_version_commit(version)?;
        if !self.version_is_reachable_from_local_variation(commit.id())? {
            return Err(DraftlineError::VersionNotLocallyReachable(
                version.to_string(),
            ));
        }

        let remote = remote.map(|remote| remote.to_string());
        if let Some(remote) = remote.as_deref() {
            self.fetch_all_variations_with_options(remote, options)?;
        }

        self.create_variation_preflight_from_fetched_state(
            version.clone(),
            name,
            remote,
            commit.id().to_string(),
        )
    }

    /// Creates a preflighted variation, refusing if local or remote state changed.
    pub fn create_variation_from_version_with_token(
        &self,
        token: VariationCreateToken,
        metadata: VariationMetadata,
        options: &mut RemoteOptions<'_>,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("create_variation_from_version")?;
        if !is_safe_operation_id(&token.operation_id) {
            return Err(DraftlineError::LocalStateChanged {
                expected: "create variation operation id issued by preflight".to_string(),
                actual: token.operation_id,
            });
        }

        let name = validate_variation_name(token.variation.as_str())?;
        self.ensure_variation_in_namespace(&name)?;
        let commit = self.find_version_commit(&token.from_version)?;
        if commit.id().to_string() != token.expected_source_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: token.expected_source_oid,
                actual: commit.id().to_string(),
            });
        }
        if !self.version_is_reachable_from_local_variation(commit.id())? {
            return Err(DraftlineError::VersionNotLocallyReachable(
                token.from_version.to_string(),
            ));
        }
        if self.repo.find_branch(&name, BranchType::Local).is_ok() {
            return Err(DraftlineError::VariationAlreadyExists(name));
        }

        if let Some(remote) = token.remote.as_deref() {
            self.fetch_all_variations_with_options(remote, options)?;
            let actual_remote_oid = remote_tracking_oid(&self.repo, remote, &name);
            if actual_remote_oid != token.expected_remote_oid {
                return Err(DraftlineError::RemoteRace {
                    remote: remote.to_string(),
                    variation: name,
                    expected: token.expected_remote_oid,
                    actual: actual_remote_oid,
                });
            }
        }

        self.repo.branch(&name, &commit, false)?;
        self.write_variation_metadata(&name, &metadata)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
            metadata,
        ))
    }

    /// Lists local variations.
    pub fn variations(&self) -> Result<Vec<Variation>> {
        self.ensure_no_pending_recovery()?;
        self.variations_unchecked()
    }

    /// Lists visible variations discovered on a remote-tracking namespace.
    pub fn remote_variations(&self, remote: impl AsRef<str>) -> Result<Vec<RemoteVariation>> {
        self.ensure_no_pending_recovery()?;
        self.remote_variations_unchecked(remote)
    }

    /// Fetches all visible remote variations and prunes deleted remote-tracking refs.
    pub fn fetch_all_variations(&self, remote: impl AsRef<str>) -> Result<()> {
        let mut options = RemoteOptions::new();
        self.fetch_all_variations_with_options(remote, &mut options)
    }

    /// Fetches all visible remote variations with explicit remote options.
    pub fn fetch_all_variations_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let remote_name = remote.as_ref();
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let refspecs = [
            format!("+refs/heads/*:refs/remotes/{remote_name}/*"),
            format!(
                "+refs/draftline/deleted-variations/*:refs/remotes/{remote_name}/draftline/deleted-variations/*"
            ),
            format!(
                "+refs/draftline/rewrites/squash/*:refs/remotes/{remote_name}/draftline/rewrites/squash/*"
            ),
        ];
        let refspec_refs = refspecs.iter().map(String::as_str).collect::<Vec<_>>();
        let _timeout_guard = options.server_timeout_guard()?;
        let fetch_result = if options.has_network_callbacks() {
            let mut fetch_options = options.fetch_options("fetch_all_variations");
            fetch_options.prune(FetchPrune::On);
            remote.fetch(&refspec_refs, Some(&mut fetch_options), None)
        } else {
            let mut fetch_options = git2::FetchOptions::new();
            fetch_options.prune(FetchPrune::On);
            remote.fetch(&refspec_refs, Some(&mut fetch_options), None)
        };
        options.map_git_result("fetch_all_variations", fetch_result)?;
        self.record_remote_head(&remote)?;
        Ok(())
    }

    /// Compares local variations with the fetched remote-tracking namespace.
    pub fn remote_variation_diagnostics(
        &self,
        remote: impl AsRef<str>,
    ) -> Result<RemoteVariationDiagnostics> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        self.repo.find_remote(&remote)?;
        let local = self
            .variations()?
            .into_iter()
            .map(|variation| variation.id)
            .collect::<BTreeSet<_>>();
        let remote_variations = self
            .remote_variations(&remote)?
            .into_iter()
            .map(|variation| variation.id)
            .collect::<BTreeSet<_>>();

        let shared_variations = local
            .intersection(&remote_variations)
            .cloned()
            .collect::<Vec<_>>();
        let local_only_variations = local
            .difference(&remote_variations)
            .cloned()
            .collect::<Vec<_>>();
        let remote_only_variations = remote_variations
            .difference(&local)
            .cloned()
            .collect::<Vec<_>>();

        Ok(RemoteVariationDiagnostics {
            remote,
            shared_variations,
            local_only_variations,
            remote_only_variations,
        })
    }

    fn create_variation_preflight_from_fetched_state(
        &self,
        from_version: VersionId,
        name: String,
        remote: Option<String>,
        source_oid: String,
    ) -> Result<VariationCreatePreflight> {
        let local_collision = self.repo.find_branch(&name, BranchType::Local).is_ok();
        let existing_remote_oid = remote
            .as_deref()
            .and_then(|remote| remote_tracking_oid(&self.repo, remote, &name));
        let remote_collision = existing_remote_oid.is_some();
        let existing_remote_head = match (remote.as_deref(), existing_remote_oid.as_deref()) {
            (Some(remote), Some(_)) => self.remote_variation_head_version(remote, &name)?,
            _ => None,
        };
        let can_create = !local_collision && !remote_collision;
        let suggested_alternative = if can_create {
            None
        } else {
            self.suggest_variation_alternative(&name, remote.as_deref())?
        };
        let token = can_create.then(|| VariationCreateToken {
            operation_id: new_operation_id(),
            from_version: from_version.clone(),
            variation: VariationId::from(name.clone()),
            remote: remote.clone(),
            expected_source_oid: source_oid,
            expected_remote_oid: existing_remote_oid,
        });

        Ok(VariationCreatePreflight {
            from_version,
            variation: VariationId::from(name),
            remote,
            can_create,
            local_collision,
            remote_collision,
            remote_only_collision: remote_collision && !local_collision,
            existing_remote_head,
            suggested_alternative,
            token,
        })
    }

    fn remote_variation_head_version(
        &self,
        remote: &str,
        variation: &str,
    ) -> Result<Option<Version>> {
        let remote_ref = format!("refs/remotes/{remote}/{variation}");
        match self.repo.find_reference(&remote_ref) {
            Ok(reference) => {
                let commit = reference.peel_to_commit()?;
                Ok(Some(version_from_commit(&commit)))
            }
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn suggest_variation_alternative(
        &self,
        base_name: &str,
        remote: Option<&str>,
    ) -> Result<Option<String>> {
        for suffix in 2..=100 {
            let candidate = validate_variation_name(&format!("{base_name}-{suffix}"))?;
            if self.repo.find_branch(&candidate, BranchType::Local).is_ok() {
                continue;
            }
            if remote
                .and_then(|remote| remote_tracking_oid(&self.repo, remote, &candidate))
                .is_some()
            {
                continue;
            }
            return Ok(Some(candidate));
        }

        Ok(None)
    }

    /// Creates a local variation from a remote-tracking variation.
    pub fn adopt_remote_variation(
        &self,
        remote: impl AsRef<str>,
        variation: &VariationId,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(variation.as_str())?;
        self.ensure_variation_in_namespace(&name)?;
        let remote_ref = format!("refs/remotes/{}/{}", remote.as_ref(), name);
        let commit = self.repo.find_reference(&remote_ref)?.peel_to_commit()?;
        self.repo.branch(&name, &commit, false)?;
        let metadata = self.read_variation_metadata(&name)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
            metadata,
        ))
    }

    /// Shelves all current tracked content changes without switching variations.
    pub fn shelve_changes(&self, name: impl AsRef<str>) -> Result<Shelf> {
        self.shelve_changes_with_optional_profile(name, None)
    }

    /// Shelves all current tracked content changes using host-supplied attribution.
    pub fn shelve_changes_with_profile(
        &self,
        name: impl AsRef<str>,
        profile: &ContributorProfile,
    ) -> Result<Shelf> {
        self.shelve_changes_with_optional_profile(name, Some(profile))
    }

    fn shelve_changes_with_optional_profile(
        &self,
        name: impl AsRef<str>,
        profile: Option<&ContributorProfile>,
    ) -> Result<Shelf> {
        self.ensure_no_pending_recovery()?;
        let safe_name = validate_variation_name(name.as_ref())?;
        let _lock = self.acquire_for_index("shelve_changes")?;
        self.shelve_changes_unchecked(&safe_name, profile)?;
        self.shelf_by_id(&safe_name)
    }

    /// Preflights shelving only selected workspace-relative tracked content files.
    pub fn preflight_shelve_files<I, P>(
        &self,
        name: impl AsRef<str>,
        paths: I,
    ) -> Result<PreflightReport>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
        let safe_name = validate_variation_name(name.as_ref())?;
        let changed_files = self.selected_changed_files(paths)?;
        Ok(selected_files_preflight_report(
            format!("shelve_files:{safe_name}"),
            true,
            changed_files,
        ))
    }

    /// Shelves only selected tracked content changes without switching variations.
    pub fn shelve_files<I, P>(&self, name: impl AsRef<str>, paths: I) -> Result<Shelf>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.shelve_files_with_optional_profile(name, paths, None)
    }

    /// Shelves only selected tracked content changes using host-supplied attribution.
    pub fn shelve_files_with_profile<I, P>(
        &self,
        name: impl AsRef<str>,
        paths: I,
        profile: &ContributorProfile,
    ) -> Result<Shelf>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.shelve_files_with_optional_profile(name, paths, Some(profile))
    }

    fn shelve_files_with_optional_profile<I, P>(
        &self,
        name: impl AsRef<str>,
        paths: I,
        profile: Option<&ContributorProfile>,
    ) -> Result<Shelf>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
        let safe_name = validate_variation_name(name.as_ref())?;
        let changed_files = self.selected_changed_files(paths)?;
        if changed_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(
                selected_files_preflight_report(
                    format!("shelve_files:{safe_name}"),
                    true,
                    changed_files,
                ),
            )));
        }

        let _lock = self.acquire_for_index("shelve_files")?;
        let (author, committer) = self.rewrite_signatures(profile)?;
        let parent = self.repo.head()?.peel_to_commit()?;
        let (tree_id, _) =
            self.content_changes_tree(Some(&parent), &changed_files, "shelve_files")?;
        self.ensure_discard_unfiltered(&changed_files, "shelve_files")?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ShelveChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(safe_name.clone()),
            completed: false,
        })?;

        let tree = self.repo.find_tree(tree_id)?;
        let oid = self.repo.commit(
            None,
            &author,
            &committer,
            &format!("Shelved changes: {safe_name}"),
            &tree,
            &[&parent],
        )?;
        self.repo.reference(
            &format!("refs/draftline/shelves/{safe_name}"),
            oid,
            false,
            "shelve selected changes",
        )?;

        self.discard_changed_files(&changed_files)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ShelveChanges,
            original_variation: None,
            target: Some(safe_name.clone()),
            completed: true,
        })?;

        self.shelf_by_id(&safe_name)
    }

    /// Lists local shelves.
    pub fn list_shelves(&self) -> Result<Vec<Shelf>> {
        self.ensure_no_pending_recovery()?;
        let mut shelves = Vec::new();
        let references = self.repo.references_glob("refs/draftline/shelves/*")?;

        for reference in references {
            let reference = reference?;
            let Ok(name) = reference.name() else {
                continue;
            };
            let Some(id) = name.strip_prefix("refs/draftline/shelves/") else {
                continue;
            };
            let Some(oid) = reference.target() else {
                continue;
            };
            let commit = self.repo.find_commit(oid)?;
            shelves.push(Shelf {
                id: id.to_string(),
                version: version_from_commit(&commit),
            });
        }

        shelves.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(shelves)
    }

    /// Previews a shelf without mutating the workspace.
    pub fn preview_shelf(&self, id: impl AsRef<str>) -> Result<VersionPreview> {
        self.ensure_no_pending_recovery()?;
        let (_id, commit) = self.find_shelf_commit(id.as_ref())?;
        let tree = commit.tree()?;
        let mut files = Vec::new();
        collect_preview_files(
            &self.repo,
            &tree,
            Path::new(""),
            &mut files,
            &self.content_policy,
        )?;

        Ok(VersionPreview {
            id: VersionId::from(commit.id()),
            files,
        })
    }

    /// Preflights applying a shelf into the current workspace.
    pub fn preflight_apply_shelf(&self, id: impl AsRef<str>) -> Result<ShelfApplyReport> {
        self.ensure_no_pending_recovery()?;
        let (safe_name, commit) = self.find_shelf_commit(id.as_ref())?;
        let (tree_id, _paths) = self.shelf_target_tree(&commit)?;
        let tree = self.repo.find_tree(tree_id)?;
        let dirty_files = self.changed_files_unchecked()?;
        let file_hazards = self.target_tree_hazards(&tree)?;
        let can_proceed = dirty_files.is_empty() && file_hazards.is_empty();

        Ok(ShelfApplyReport {
            shelf: Shelf {
                id: safe_name,
                version: version_from_commit(&commit),
            },
            dirty_files,
            file_hazards,
            can_proceed,
        })
    }

    /// Applies a shelf as working-tree content and preserves the shelf.
    pub fn apply_shelf(&self, id: impl AsRef<str>) -> Result<Shelf> {
        self.ensure_no_pending_recovery()?;
        let safe_name = validate_variation_name(id.as_ref())?;
        let _lock = self.acquire_for_index("apply_shelf")?;
        let report = self.preflight_apply_shelf(&safe_name)?;
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "apply_shelf",
                true,
                report.dirty_files,
                report.file_hazards,
                None,
            ))));
        }

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ApplyShelf,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(safe_name.clone()),
            completed: false,
        })?;

        let (_id, commit) = self.find_shelf_commit(&safe_name)?;
        self.clear_recovery_if_refused(self.apply_shelf_paths(&commit))?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ApplyShelf,
            original_variation: None,
            target: Some(safe_name.clone()),
            completed: true,
        })?;

        self.shelf_by_id(&safe_name)
    }

    /// Deletes a local shelf ref.
    pub fn delete_shelf(&self, id: impl AsRef<str>) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let safe_name = validate_variation_name(id.as_ref())?;
        let mut reference = self
            .repo
            .find_reference(&format!("refs/draftline/shelves/{safe_name}"))?;
        reference.delete()?;
        Ok(())
    }

    /// Lists hidden Draftline archive support refs.
    pub fn list_support_refs(&self, scope: SupportRefScope) -> Result<Vec<SupportRef>> {
        self.ensure_no_pending_recovery()?;
        match scope {
            SupportRefScope::Local => self.list_local_support_refs(),
            SupportRefScope::RemoteTracking => self.list_remote_tracking_support_refs(),
        }
    }

    /// Fetches hidden support refs into a remote-tracking support namespace.
    pub fn fetch_support_refs(&self, remote: impl AsRef<str>) -> Result<()> {
        let mut options = RemoteOptions::new();
        self.fetch_support_refs_with_options(remote, &mut options)
    }

    /// Fetches hidden support refs with explicit remote options.
    pub fn fetch_support_refs_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref();
        let mut git_remote = self.repo.find_remote(remote)?;
        let refspecs = [
            format!(
                "+refs/draftline/deleted-variations/*:refs/remotes/{remote}/draftline/deleted-variations/*"
            ),
            format!(
                "+refs/draftline/rewrites/squash/*:refs/remotes/{remote}/draftline/rewrites/squash/*"
            ),
            format!(
                "+refs/draftline/backups/history-cleanup/*:refs/remotes/{remote}/draftline/backups/history-cleanup/*"
            ),
        ];
        let refspec_refs = refspecs.iter().map(String::as_str).collect::<Vec<_>>();
        let _timeout_guard = options.server_timeout_guard()?;
        let fetch_result = if options.has_network_callbacks() {
            let mut fetch_options = options.fetch_options("fetch_support_refs");
            git_remote.fetch(&refspec_refs, Some(&mut fetch_options), None)
        } else {
            git_remote.fetch(&refspec_refs, None, None)
        };
        options.map_git_result("fetch_support_refs", fetch_result)?;
        Ok(())
    }

    /// Plans create-only publication of all local support refs to a shared remote.
    pub fn preflight_publish_support_refs(
        &self,
        remote: impl AsRef<str>,
    ) -> Result<SupportRefPublishPreflight> {
        let mut options = RemoteOptions::new();
        self.preflight_publish_support_refs_with_options(remote, &mut options)
    }

    /// Plans create-only support-ref publication with explicit remote options.
    pub fn preflight_publish_support_refs_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<SupportRefPublishPreflight> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        self.repo.find_remote(&remote)?;
        self.fetch_support_refs_with_options(&remote, options)?;
        let local_support_refs = self.list_local_support_refs()?;
        let remote_support_refs = self.list_remote_tracking_support_refs()?;
        let mut remote_by_local_ref = HashMap::new();
        for remote_ref in remote_support_refs {
            if let Some(local_ref_name) =
                local_support_ref_from_remote_tracking(&remote, &remote_ref.ref_name)
            {
                remote_by_local_ref.insert(local_ref_name, remote_ref.target_oid);
            }
        }

        let mut support_refs = Vec::new();
        for support_ref in local_support_refs {
            match remote_by_local_ref.get(&support_ref.ref_name) {
                Some(remote_oid) if remote_oid == &support_ref.target_oid => continue,
                Some(remote_oid) => {
                    return Err(DraftlineError::RemoteRace {
                        remote,
                        variation: support_ref.ref_name,
                        expected: Some(support_ref.target_oid),
                        actual: Some(remote_oid.clone()),
                    });
                }
                None => support_refs.push(support_ref),
            }
        }

        let token = SupportRefPublishToken {
            remote: remote.clone(),
            refs: support_refs
                .iter()
                .map(|support_ref| SupportRefPublishItem {
                    ref_name: support_ref.ref_name.clone(),
                    target_oid: support_ref.target_oid.clone(),
                })
                .collect(),
        };
        let can_publish = !support_refs.is_empty();

        Ok(SupportRefPublishPreflight {
            remote,
            support_refs,
            token,
            can_publish,
        })
    }

    /// Publishes preflighted support refs using create-only remote updates.
    pub fn publish_support_refs(&self, token: SupportRefPublishToken) -> Result<()> {
        let mut options = RemoteOptions::new();
        self.publish_support_refs_with_options(token, &mut options)
    }

    /// Publishes preflighted support refs with explicit remote options.
    pub fn publish_support_refs_with_options(
        &self,
        token: SupportRefPublishToken,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        self.repo.find_remote(&token.remote)?;
        self.fetch_support_refs_with_options(&token.remote, options)?;
        for item in token.refs {
            if !is_restorable_support_ref(&item.ref_name) {
                return Err(DraftlineError::VersionNotFound(item.ref_name));
            }
            let reference = self.repo.find_reference(&item.ref_name)?;
            let actual_oid = reference
                .target()
                .ok_or_else(|| DraftlineError::VersionNotFound(item.ref_name.clone()))?
                .to_string();
            if actual_oid != item.target_oid {
                return Err(DraftlineError::LocalStateChanged {
                    expected: format!("{}@{}", item.ref_name, item.target_oid),
                    actual: format!("{}@{actual_oid}", item.ref_name),
                });
            }

            let remote_support_ref =
                remote_tracking_support_ref_from_local(&token.remote, &item.ref_name);
            match self.repo.refname_to_id(&remote_support_ref) {
                Ok(oid) if oid.to_string() == item.target_oid => continue,
                Ok(oid) => {
                    return Err(DraftlineError::RemoteRace {
                        remote: token.remote.clone(),
                        variation: item.ref_name,
                        expected: Some(item.target_oid),
                        actual: Some(oid.to_string()),
                    });
                }
                Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                Err(error) => return Err(error.into()),
            }

            self.push_refspec(
                &token.remote,
                &format!("{}:{}", item.ref_name, item.ref_name),
                vec![PushRefExpectation {
                    dst_refname: item.ref_name.clone(),
                    expected_old_oid: None,
                    expected_new_oid: Some(item.target_oid),
                }],
                options,
            )?;
        }
        Ok(())
    }

    /// Plans restoration of an archive support ref as a new visible variation.
    pub fn preflight_restore_support_ref(
        &self,
        id: impl AsRef<str>,
        name: impl AsRef<str>,
    ) -> Result<SupportRefRestorePreflight> {
        self.ensure_no_pending_recovery()?;
        let id = id.as_ref();
        let name = validate_variation_name(name.as_ref())?;
        self.ensure_variation_in_namespace(&name)?;
        if self.repo.find_branch(&name, BranchType::Local).is_ok() {
            return Err(DraftlineError::InvalidVariationName(name));
        }

        let support_ref = self
            .list_local_support_refs()?
            .into_iter()
            .chain(self.list_remote_tracking_support_refs()?)
            .find(|support_ref| support_ref.id == id || support_ref.ref_name == id)
            .ok_or_else(|| DraftlineError::VersionNotFound(id.to_string()))?;

        if !is_restorable_support_ref(&support_ref.ref_name)
            && !is_remote_tracking_restorable_support_ref(&support_ref.ref_name)
        {
            return Err(DraftlineError::VersionNotFound(id.to_string()));
        }

        let reference = self.repo.find_reference(&support_ref.ref_name)?;
        let actual_oid = reference
            .target()
            .ok_or_else(|| DraftlineError::VersionNotFound(support_ref.ref_name.clone()))?
            .to_string();
        if actual_oid != support_ref.target_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", support_ref.ref_name, support_ref.target_oid),
                actual: format!("{}@{actual_oid}", support_ref.ref_name),
            });
        }

        let target_variation = VariationId::from(name);
        let token = SupportRefRestoreToken {
            support_ref_id: support_ref.id.clone(),
            target_oid: support_ref.target_oid.clone(),
            target_variation: target_variation.clone(),
        };

        Ok(SupportRefRestorePreflight {
            support_ref,
            target_variation: target_variation.clone(),
            token,
            can_restore: true,
        })
    }

    /// Restores a preflighted archive support ref as a new visible variation.
    pub fn restore_support_ref(&self, token: SupportRefRestoreToken) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(token.target_variation.as_str())?;
        self.ensure_variation_in_namespace(&name)?;
        let support_ref = self
            .list_local_support_refs()?
            .into_iter()
            .chain(self.list_remote_tracking_support_refs()?)
            .find(|support_ref| support_ref.id == token.support_ref_id)
            .ok_or_else(|| DraftlineError::VersionNotFound(token.support_ref_id.clone()))?;

        let reference = self.repo.find_reference(&support_ref.ref_name)?;
        let actual_oid = reference
            .target()
            .ok_or_else(|| DraftlineError::VersionNotFound(support_ref.ref_name.clone()))?
            .to_string();
        if actual_oid != token.target_oid || actual_oid != support_ref.target_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", support_ref.ref_name, token.target_oid),
                actual: format!("{}@{actual_oid}", support_ref.ref_name),
            });
        }

        let commit = reference.peel_to_commit()?;
        self.repo.branch(&name, &commit, false)?;
        let metadata = self.read_variation_metadata(&name)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
            metadata,
        ))
    }

    /// Restores an archive support ref as a new visible variation.
    pub fn restore_support_ref_as_variation(
        &self,
        id: impl AsRef<str>,
        name: impl AsRef<str>,
    ) -> Result<Variation> {
        let preflight = self.preflight_restore_support_ref(id, name)?;
        self.restore_support_ref(preflight.token)
    }

    /// Preflights deleting a visible variation from a shared remote.
    pub fn preflight_delete_remote_variation(
        &self,
        remote: impl AsRef<str>,
        variation: &VariationId,
    ) -> Result<RemoteVariationDeletePreflight> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        let variation_name = validate_variation_name(variation.as_str())?;
        self.ensure_variation_in_namespace(&variation_name)?;
        let mut options = RemoteOptions::new();
        self.fetch_remote_variation_ref(&remote, &variation_name, &mut options)?;
        let Some(expected_remote_oid) = remote_tracking_oid(&self.repo, &remote, &variation_name)
        else {
            return Err(DraftlineError::VersionNotFound(variation_name));
        };
        let operation_id = new_operation_id();
        let support_ref = archive_ref("deleted-variations", &variation_name, &operation_id);
        let token = RemoteVariationDeleteToken {
            remote: remote.clone(),
            variation: VariationId::from(variation_name.clone()),
            expected_remote_oid: expected_remote_oid.clone(),
            support_ref: support_ref.clone(),
        };

        Ok(RemoteVariationDeletePreflight {
            remote,
            variation: VariationId::from(variation_name),
            expected_remote_oid,
            support_ref,
            token,
            can_delete: true,
        })
    }

    /// Deletes a visible remote variation only after publishing a support ref.
    pub fn delete_remote_variation(&self, token: RemoteVariationDeleteToken) -> Result<()> {
        let mut options = RemoteOptions::new();
        self.delete_remote_variation_with_options(token, &mut options)
    }

    /// Deletes a visible remote variation with explicit remote options.
    pub fn delete_remote_variation_with_options(
        &self,
        token: RemoteVariationDeleteToken,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_operation_lock("delete_remote_variation")?;
        let variation_name = validate_variation_name(token.variation.as_str())?;
        self.ensure_variation_in_namespace(&variation_name)?;
        self.fetch_remote_variation_ref(&token.remote, &variation_name, options)?;
        let actual_remote_oid = remote_tracking_oid(&self.repo, &token.remote, &variation_name);
        if actual_remote_oid.as_deref() != Some(token.expected_remote_oid.as_str()) {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: variation_name,
                expected: Some(token.expected_remote_oid),
                actual: actual_remote_oid,
            });
        }

        self.fetch_support_refs_with_options(&token.remote, options)?;
        let remote_support_ref =
            remote_tracking_support_ref_from_local(&token.remote, &token.support_ref);
        let remote_support_ref_already_published =
            match self.repo.refname_to_id(&remote_support_ref) {
                Ok(oid) if oid.to_string() == token.expected_remote_oid => true,
                Ok(oid) => {
                    return Err(DraftlineError::RemoteRace {
                        remote: token.remote,
                        variation: token.support_ref,
                        expected: Some(token.expected_remote_oid),
                        actual: Some(oid.to_string()),
                    });
                }
                Err(error) if error.code() == git2::ErrorCode::NotFound => false,
                Err(error) => return Err(error.into()),
            };
        let operation_id = new_operation_id();
        self.write_remote_delete_recovery_metadata(
            &operation_id,
            &RemoteVariationDeleteRecoveryMetadata {
                remote: token.remote.clone(),
                variation: variation_name.clone(),
                expected_remote_oid: token.expected_remote_oid.clone(),
                support_ref: token.support_ref.clone(),
            },
        )?;
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::DeleteRemoteVariation,
            original_variation: Some(variation_name.clone()),
            target: Some(token.expected_remote_oid.clone()),
            completed: false,
        })?;

        let target_oid = Oid::from_str(&token.expected_remote_oid)
            .map_err(|_| DraftlineError::VersionNotFound(token.expected_remote_oid.clone()))?;
        match self.repo.refname_to_id(&token.support_ref) {
            Ok(oid) if oid == target_oid => {}
            Ok(oid) => {
                return Err(DraftlineError::LocalStateChanged {
                    expected: format!("{}@{}", token.support_ref, target_oid),
                    actual: format!("{}@{oid}", token.support_ref),
                });
            }
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                self.repo.reference(
                    &token.support_ref,
                    target_oid,
                    false,
                    "archive remote variation before delete",
                )?;
            }
            Err(error) => return Err(error.into()),
        }
        if !remote_support_ref_already_published {
            self.push_refspec(
                &token.remote,
                &format!("{}:{}", token.support_ref, token.support_ref),
                vec![PushRefExpectation {
                    dst_refname: token.support_ref.clone(),
                    expected_old_oid: None,
                    expected_new_oid: Some(token.expected_remote_oid.clone()),
                }],
                options,
            )?;
        }
        self.push_refspec(
            &token.remote,
            &format!(":refs/heads/{variation_name}"),
            vec![PushRefExpectation {
                dst_refname: format!("refs/heads/{variation_name}"),
                expected_old_oid: Some(token.expected_remote_oid.clone()),
                expected_new_oid: None,
            }],
            options,
        )?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::DeleteRemoteVariation,
            original_variation: None,
            target: Some(token.expected_remote_oid),
            completed: true,
        })?;

        Ok(())
    }

    /// Preflights expiring selected local support refs as retention cleanup.
    pub fn preflight_expire_support_refs<I, S>(
        &self,
        ids: I,
    ) -> Result<SupportRefExpirationPreflight>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ensure_no_pending_recovery()?;
        let ids: Vec<String> = ids.into_iter().map(Into::into).collect();
        let local_refs = self.list_local_support_refs()?;
        let mut support_refs = Vec::new();

        for id in &ids {
            if !is_restorable_support_ref(id) {
                return Err(DraftlineError::VersionNotFound(id.clone()));
            }
            let Some(support_ref) = local_refs.iter().find(|support_ref| &support_ref.id == id)
            else {
                return Err(DraftlineError::VersionNotFound(id.clone()));
            };
            support_refs.push(support_ref.clone());
        }

        Ok(SupportRefExpirationPreflight {
            support_refs,
            token: SupportRefExpirationToken { ids },
            can_expire: true,
        })
    }

    /// Expires selected local support refs as retention cleanup, not purge.
    pub fn expire_support_refs(&self, token: SupportRefExpirationToken) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_operation_lock("expire_support_refs")?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ExpireSupportRefs,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(token.ids.join(",")),
            completed: false,
        })?;

        for id in &token.ids {
            if !is_restorable_support_ref(id) {
                return Err(DraftlineError::VersionNotFound(id.clone()));
            }
            let mut reference = self.repo.find_reference(id)?;
            reference.delete()?;
        }

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ExpireSupportRefs,
            original_variation: None,
            target: Some(token.ids.join(",")),
            completed: true,
        })?;

        Ok(())
    }

    /// Plans a destructive purge/redaction workflow without rewriting history.
    pub fn preflight_purge_content(&self, selector: impl AsRef<str>) -> Result<PurgePreflight> {
        self.ensure_no_pending_recovery()?;
        let selector = selector.as_ref().to_string();
        let affected_refs = self.purge_ref_candidates()?;
        let token = PurgeToken {
            selector: selector.clone(),
            affected_refs: affected_refs.clone(),
        };

        Ok(PurgePreflight {
            selector,
            affected_refs,
            distributed_warning:
                "Git cannot guarantee deletion from existing clones, forks, backups, caches, or offline devices."
                    .to_string(),
            token,
        })
    }

    /// Verifies a purge plan's candidate refs; no destructive purge is performed here.
    pub fn verify_purge(&self, token: PurgeToken) -> Result<PurgeVerification> {
        Ok(PurgeVerification {
            selector: token.selector,
            checked_refs: token.affected_refs.len(),
            verified_absent: false,
            limitations: vec![
                "Verification is local to this repository and cannot inspect existing clones."
                    .to_string(),
            ],
        })
    }

    /// Returns display metadata for a local variation.
    pub fn variation_metadata(&self, variation: &VariationId) -> Result<VariationMetadata> {
        self.ensure_no_pending_recovery()?;
        self.repo
            .find_branch(variation.as_str(), BranchType::Local)?;
        self.read_variation_metadata(variation.as_str())
    }

    /// Adds, updates, or clears display metadata for a local variation.
    pub fn set_variation_metadata(
        &self,
        variation: &VariationId,
        metadata: VariationMetadata,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        self.repo
            .find_branch(variation.as_str(), BranchType::Local)?;
        self.write_variation_metadata(variation.as_str(), &metadata)?;

        Ok(variation_from_name(
            variation.as_str().to_string(),
            self.current_variation().ok().as_ref(),
            metadata,
        ))
    }

    /// Switches to a variation with an explicit safety policy.
    pub fn switch_variation(
        &self,
        variation: &VariationId,
        policy: SwitchPolicy,
    ) -> Result<Variation> {
        self.switch_variation_with_optional_profile(variation, policy, None)
    }

    /// Switches to a variation, attributing any `SaveFirst` or `Shelve` commit to a host profile.
    pub fn switch_variation_with_profile(
        &self,
        variation: &VariationId,
        policy: SwitchPolicy,
        profile: &ContributorProfile,
    ) -> Result<Variation> {
        self.switch_variation_with_optional_profile(variation, policy, Some(profile))
    }

    fn switch_variation_with_optional_profile(
        &self,
        variation: &VariationId,
        policy: SwitchPolicy,
        profile: Option<&ContributorProfile>,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("switch_variation")?;
        let mut report = self.preflight_switch_variation_unchecked(variation)?;

        if !report.file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        match &policy {
            SwitchPolicy::AbortIfDirty if !report.can_proceed => {
                return Err(DraftlineError::PreflightFailed(Box::new(report)));
            }
            SwitchPolicy::SaveFirst { label } if !report.can_proceed => {
                self.save_version_unchecked(label, profile)?;
            }
            SwitchPolicy::Shelve { name } if !report.can_proceed => {
                self.shelve_changes_unchecked(name, profile)?;
            }
            SwitchPolicy::Discard => {
                return Err(DraftlineError::UnsupportedSwitchPolicy(
                    "discard requires an explicit overwrite API and is not implemented",
                ));
            }
            _ => {}
        }

        report = self.preflight_switch_variation_unchecked(variation)?;
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let target = self.branch_tree(variation.as_str())?;
        let operation_id = new_operation_id();
        self.write_recovery_state_for_checkout(
            &RecoveryState {
                operation_id: operation_id.clone(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: self.current_variation().ok(),
                target: Some(variation.as_str().to_string()),
                completed: false,
            },
            &self.head_tree_or_empty()?,
            &target,
        )?;

        self.checkout_or_keep_recovery(&target)?;
        self.point_head_at(variation.as_str())?;

        let metadata = self.read_variation_metadata(variation.as_str())?;
        let result =
            variation_from_name(variation.as_str().to_string(), Some(&variation.0), metadata);
        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::SwitchVariation,
            original_variation: None,
            target: Some(variation.as_str().to_string()),
            completed: true,
        })?;

        Ok(result)
    }

    /// Plans deletion of an alternate variation, including the archive ref.
    pub fn preflight_delete_variation(
        &self,
        variation: &VariationId,
    ) -> Result<VariationDeletePreflight> {
        self.ensure_no_pending_recovery()?;
        self.ensure_variation_in_namespace(variation.as_str())?;
        if self.current_variation().ok().as_deref() == Some(variation.as_str()) {
            return Err(DraftlineError::CannotDeleteCurrentVariation(
                variation.as_str().to_string(),
            ));
        }

        let operation_id = new_operation_id();
        let branch = self
            .repo
            .find_branch(variation.as_str(), BranchType::Local)?;
        let target_oid = branch.get().peel_to_commit()?.id();
        let support_ref = archive_ref("deleted-variations", variation.as_str(), &operation_id);
        let target_oid = target_oid.to_string();
        let token = VariationDeleteToken {
            operation_id,
            variation: variation.clone(),
            expected_oid: target_oid.clone(),
            support_ref: support_ref.clone(),
        };

        Ok(VariationDeletePreflight {
            variation: variation.clone(),
            target_oid,
            support_ref,
            token,
            can_delete: true,
        })
    }

    /// Deletes a preflighted alternate variation.
    pub fn delete_variation_with_token(&self, token: VariationDeleteToken) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_operation_lock("delete_variation")?;
        self.ensure_variation_in_namespace(token.variation.as_str())?;
        if self.current_variation().ok().as_deref() == Some(token.variation.as_str()) {
            return Err(DraftlineError::CannotDeleteCurrentVariation(
                token.variation.as_str().to_string(),
            ));
        }

        let mut branch = self
            .repo
            .find_branch(token.variation.as_str(), BranchType::Local)?;
        let target_oid = branch.get().peel_to_commit()?.id();
        if target_oid.to_string() != token.expected_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", token.variation.as_str(), token.expected_oid),
                actual: format!("{}@{target_oid}", token.variation.as_str()),
            });
        }
        self.write_recovery_state(&RecoveryState {
            operation_id: token.operation_id.clone(),
            operation: RecoveryOperation::DeleteVariation,
            original_variation: Some(token.variation.as_str().to_string()),
            target: Some(token.expected_oid.clone()),
            completed: false,
        })?;

        self.repo.reference(
            &token.support_ref,
            target_oid,
            false,
            "archive deleted variation",
        )?;
        branch.delete()?;

        self.write_recovery_state(&RecoveryState {
            operation_id: token.operation_id,
            operation: RecoveryOperation::DeleteVariation,
            original_variation: None,
            target: Some(token.expected_oid),
            completed: true,
        })?;
        Ok(())
    }

    /// Deletes an alternate variation.
    pub fn delete_variation(&self, variation: &VariationId) -> Result<()> {
        let preflight = self.preflight_delete_variation(variation)?;
        self.delete_variation_with_token(preflight.token)
    }

    /// Plans renaming a visible local variation, including the archive ref used for recovery.
    pub fn preflight_rename_variation(
        &self,
        source: &VariationId,
        target: &VariationId,
    ) -> Result<VariationRenamePreflight> {
        self.ensure_no_pending_recovery()?;
        let source_name = validate_variation_name(source.as_str())?;
        let target_name = validate_variation_name(target.as_str())?;
        self.ensure_variation_in_namespace(&source_name)?;
        self.ensure_variation_in_namespace(&target_name)?;
        if source_name == target_name {
            return Err(DraftlineError::VariationAlreadyExists(target_name));
        }

        let branch = self.find_local_variation_branch(&source_name)?;
        if self
            .repo
            .find_branch(&target_name, BranchType::Local)
            .is_ok()
        {
            return Err(DraftlineError::VariationAlreadyExists(target_name));
        }

        let operation_id = new_operation_id();
        let expected_oid = branch.get().peel_to_commit()?.id().to_string();
        let support_ref = archive_ref("deleted-variations", &source_name, &operation_id);
        let token = VariationRenameToken {
            operation_id,
            source_variation: VariationId::from(source_name.clone()),
            target_variation: VariationId::from(target_name.clone()),
            expected_oid: expected_oid.clone(),
            support_ref: support_ref.clone(),
        };

        Ok(VariationRenamePreflight {
            source_variation: VariationId::from(source_name),
            target_variation: VariationId::from(target_name),
            expected_oid,
            support_ref,
            token,
            can_rename: true,
        })
    }

    /// Renames a preflighted visible local variation while preserving its tip in a support ref.
    pub fn rename_variation_with_token(&self, token: VariationRenameToken) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_operation_lock("rename_variation")?;
        let source_name = validate_variation_name(token.source_variation.as_str())?;
        let target_name = validate_variation_name(token.target_variation.as_str())?;
        self.ensure_variation_in_namespace(&source_name)?;
        self.ensure_variation_in_namespace(&target_name)?;
        if source_name == target_name {
            return Err(DraftlineError::VariationAlreadyExists(target_name));
        }
        if !is_safe_operation_id(&token.operation_id) {
            return Err(DraftlineError::LocalStateChanged {
                expected: "rename operation id issued by preflight".to_string(),
                actual: token.operation_id,
            });
        }
        let expected_support_ref =
            archive_ref("deleted-variations", &source_name, &token.operation_id);
        if token.support_ref != expected_support_ref {
            return Err(DraftlineError::LocalStateChanged {
                expected: expected_support_ref,
                actual: token.support_ref,
            });
        }
        if self
            .repo
            .find_branch(&target_name, BranchType::Local)
            .is_ok()
        {
            return Err(DraftlineError::VariationAlreadyExists(target_name));
        }

        let mut branch = self.find_local_variation_branch(&source_name)?;
        let target_oid = branch.get().peel_to_commit()?.id();
        if target_oid.to_string() != token.expected_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", source_name, token.expected_oid),
                actual: format!("{}@{target_oid}", source_name),
            });
        }

        self.write_recovery_state(&RecoveryState {
            operation_id: token.operation_id.clone(),
            operation: RecoveryOperation::RenameVariation,
            original_variation: Some(source_name.clone()),
            target: Some(target_name.clone()),
            completed: false,
        })?;

        self.ensure_archive_ref(&token.support_ref, target_oid, "archive renamed variation")?;
        let source_was_current = self.head_symbolic_variation().as_deref() == Some(&source_name);
        let metadata = self.read_variation_metadata(&source_name)?;
        branch.rename(&target_name, false)?;
        self.write_variation_metadata(&target_name, &metadata)?;
        self.clear_variation_metadata(&source_name)?;
        if source_was_current {
            self.repo.set_head(&format!("refs/heads/{target_name}"))?;
        }

        self.write_recovery_state(&RecoveryState {
            operation_id: token.operation_id,
            operation: RecoveryOperation::RenameVariation,
            original_variation: None,
            target: Some(target_name.clone()),
            completed: true,
        })?;

        let current = self.current_variation_unchecked().ok();
        Ok(variation_from_name(target_name, current.as_ref(), metadata))
    }

    /// Renames a visible local variation.
    pub fn rename_variation(
        &self,
        source: &VariationId,
        target: &VariationId,
    ) -> Result<Variation> {
        let preflight = self.preflight_rename_variation(source, target)?;
        self.rename_variation_with_token(preflight.token)
    }

    /// Creates a new version from an earlier version without switching variations.
    pub fn restore_version_as_new_save(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
    ) -> Result<Version> {
        self.restore_version_as_new_save_with_optional_profile(version, label, None)
    }

    /// Creates a new version from an earlier version using host-supplied attribution.
    pub fn restore_version_as_new_save_with_profile(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
        profile: &ContributorProfile,
    ) -> Result<Version> {
        self.restore_version_as_new_save_with_optional_profile(version, label, Some(profile))
    }

    fn restore_version_as_new_save_with_optional_profile(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
        profile: Option<&ContributorProfile>,
    ) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("restore_version_as_new_save")?;
        let branch = self.ensure_on_branch()?;
        let head_commit = self.repo.head()?.peel_to_commit()?;
        let restored_tree = {
            let source = self.relocated_tree(&self.find_version_commit(version)?)?;
            let tree_id = self.compose_scoped_tree(&head_commit.tree()?, &source)?;
            self.repo.find_tree(tree_id)?
        };
        let report = preflight_report(
            "restore_version_as_new_save",
            true,
            self.changed_files_unchecked()?,
            self.target_tree_hazards(&restored_tree)?,
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let (author, committer) = self.rewrite_signatures(profile)?;
        let operation_id = new_operation_id();
        self.write_recovery_state_for_checkout(
            &RecoveryState {
                operation_id: operation_id.clone(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: self.current_variation().ok(),
                target: Some(version.as_str().to_string()),
                completed: false,
            },
            &head_commit.tree()?,
            &restored_tree,
        )?;

        self.checkout_or_keep_recovery(&restored_tree)?;
        let oid = self.repo.commit(
            None,
            &author,
            &committer,
            label.as_ref(),
            &restored_tree,
            &[&head_commit],
        )?;
        self.advance_head(
            &branch,
            &self.repo.find_commit(oid)?,
            Some(&head_commit),
            &committer,
        )?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: None,
            target: Some(version.as_str().to_string()),
            completed: true,
        })?;

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    /// Restores a saved version as a new save on a selected target variation.
    ///
    /// The target is resolved and preflighted before Draftline writes the restored
    /// save. Non-current targets are only activated after the target commit has
    /// been created, avoiding accidental restores onto the previously active
    /// variation when target creation or resolution fails.
    pub fn restore_version_as_new_save_to_variation(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
        target: RestoreVersionTarget,
    ) -> Result<TargetedRestoreVersionResult> {
        self.restore_version_as_new_save_to_variation_with_optional_profile(
            version, label, target, None,
        )
    }

    /// Restores a saved version as a new save on a selected target variation using
    /// host-supplied attribution.
    pub fn restore_version_as_new_save_to_variation_with_profile(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
        target: RestoreVersionTarget,
        profile: &ContributorProfile,
    ) -> Result<TargetedRestoreVersionResult> {
        self.restore_version_as_new_save_to_variation_with_optional_profile(
            version,
            label,
            target,
            Some(profile),
        )
    }

    fn restore_version_as_new_save_to_variation_with_optional_profile(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
        target: RestoreVersionTarget,
        profile: Option<&ContributorProfile>,
    ) -> Result<TargetedRestoreVersionResult> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("restore_version_as_new_save_to_variation")?;

        let source_commit = self.find_version_commit(version)?;
        let source_tree = self.relocated_tree(&source_commit)?;
        let current_variation = self.current_variation_unchecked()?;
        let (target_name, target_metadata, parent_oid) =
            self.resolve_restore_version_target(target, &current_variation)?;
        let parent = self.repo.find_commit(parent_oid)?;
        // A target from before a move keeps the folder elsewhere; restoring
        // into it would leave the folder in two places.
        let target_folder = FolderChanges::new(&self.repo, self.scope()).folder_at(&parent)?;
        if target_folder != self.scope() {
            return Err(DraftlineError::FolderIsElsewhere(target_folder));
        }
        let restored_tree = {
            let tree_id = self.compose_scoped_tree(&parent.tree()?, &source_tree)?;
            self.repo.find_tree(tree_id)?
        };

        let report = preflight_report(
            "restore_version_as_new_save_to_variation",
            true,
            self.changed_files_unchecked()?,
            self.target_tree_hazards(&restored_tree)?,
            Some(format!("{current_variation} -> {target_name}")),
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let (author, committer) = self.rewrite_signatures(profile)?;
        let operation_id = new_operation_id();
        self.write_recovery_state_for_checkout(
            &RecoveryState {
                operation_id: operation_id.clone(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: Some(current_variation),
                target: Some(format!("{} -> {}", version.as_str(), target_name)),
                completed: false,
            },
            &self.repo.head()?.peel_to_tree()?,
            &restored_tree,
        )?;

        // Move files before any ref so HEAD is still the checkout baseline.
        self.checkout_or_keep_recovery(&restored_tree)?;
        let target_ref = format!("refs/heads/{target_name}");
        // The ref moves only if it is still at `parent`, so a retry after
        // Git's lock clears can't land twice or over a developer's commit.
        let oid = self.while_git_busy(|| {
            Ok(self.repo.commit(
                Some(&target_ref),
                &author,
                &committer,
                label.as_ref(),
                &restored_tree,
                &[&parent],
            )?)
        })?;
        self.write_variation_metadata(&target_name, &target_metadata)?;
        self.point_head_at(&target_name)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: None,
            target: Some(format!("{} -> {}", version.as_str(), target_name)),
            completed: true,
        })?;

        Ok(TargetedRestoreVersionResult {
            version: version_from_commit(&self.repo.find_commit(oid)?),
            target_variation: variation_from_name(
                target_name.clone(),
                Some(&target_name),
                target_metadata,
            ),
        })
    }

    /// Reads a version without mutating the live workspace.
    pub fn preview_version(&self, version: &VersionId) -> Result<VersionPreview> {
        self.ensure_no_pending_recovery()?;
        let commit = self.find_version_commit(version)?;
        let tree = self.relocated_tree(&commit)?;
        let mut files = Vec::new();
        collect_preview_files(
            &self.repo,
            &tree,
            Path::new(""),
            &mut files,
            &self.content_policy,
        )?;

        Ok(VersionPreview {
            id: version.clone(),
            files,
        })
    }

    /// Reads one file from a version without mutating the live workspace.
    pub fn preview_version_file(
        &self,
        version: &VersionId,
        path: impl AsRef<Path>,
    ) -> Result<Option<PreviewFile>> {
        self.ensure_no_pending_recovery()?;
        let path = normalize_workspace_relative(path)?;
        if !self.content_policy.tracks(&path)? {
            return Ok(None);
        }

        let commit = self.find_version_commit(version)?;
        let tree = self.relocated_tree(&commit)?;
        let entry = match tree.get_path(&path) {
            Ok(entry) => entry,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        if entry.kind() != Some(ObjectType::Blob) {
            return Ok(None);
        }

        let blob = self.repo.find_blob(entry.id())?;
        let content = std::str::from_utf8(blob.content())
            .ok()
            .map(ToString::to_string);

        Ok(Some(PreviewFile {
            path,
            is_binary: content.is_none(),
            content,
        }))
    }

    /// Reads one tracked file from the current workspace without mutating state.
    pub fn preview_workspace_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Option<CurrentFilePreview>> {
        self.ensure_no_pending_recovery()?;
        let path = normalize_workspace_relative(path)?;
        if !self.content_policy.tracks(&path)? {
            return Ok(None);
        }

        let full_path = self.root.join(&path);
        if !full_path.exists() {
            return Ok(None);
        }

        let bytes = fs::read(full_path)?;
        let content = std::str::from_utf8(&bytes).ok().map(ToString::to_string);
        Ok(Some(CurrentFilePreview {
            path,
            is_binary: content.is_none(),
            content,
        }))
    }

    /// Returns a comprehensive UI snapshot of this workspace.
    ///
    /// Unlike individual accessor methods, `workspace_summary` succeeds even
    /// when a prior operation left an incomplete recovery record — the
    /// `recovery` field of the returned struct will carry the state, so the
    /// host UI can surface a recovery prompt without needing a separate call.
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// let summary = workspace.workspace_summary()?;
    /// println!("on variation: {}", summary.active_variation.name);
    /// println!("versions: {}", summary.versions.len());
    /// println!("dirty: {}", summary.is_dirty);
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn workspace_summary(&self) -> Result<WorkspaceSummary> {
        let recovery = self.recovery_state()?;
        let variations = self.variations_unchecked().unwrap_or_default();
        let active_variation = variations
            .iter()
            .find(|v| v.is_current)
            .cloned()
            .or_else(|| {
                self.current_variation_unchecked()
                    .ok()
                    .map(|name| Variation {
                        id: VariationId::from(name.clone()),
                        name,
                        metadata: VariationMetadata::default(),
                        is_current: true,
                    })
            })
            .ok_or(DraftlineError::NoCurrentVariation)?;
        let versions = self.versions_unchecked().unwrap_or_default();
        let dirty_files = self.changed_files_unchecked().unwrap_or_default();
        let is_dirty = !dirty_files.is_empty();
        let state_may_be_inconsistent = recovery.is_some();
        let legacy_history_cleanups = self.legacy_history_cleanups().unwrap_or_default();

        Ok(WorkspaceSummary {
            active_variation,
            variations,
            versions,
            dirty_files,
            is_dirty,
            recovery,
            state_may_be_inconsistent,
            legacy_history_cleanups,
        })
    }

    /// Returns the version history for the current variation with variation-tip annotations.
    ///
    /// Like `git log`, this lists every commit on the branch, including
    /// teammates' commits that don't change the folder; check
    /// [`HistoryEntry::touches_folder`]. [`Workspace::versions`] lists only
    /// the commits that change the folder.
    ///
    /// Each entry carries the version metadata plus the list of variation IDs
    /// whose tip commit is exactly that version — useful for rendering a branch
    /// graph or timeline where multiple variations share common ancestors.
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// for entry in workspace.history()? {
    ///     println!("{} {:?}", entry.version.label, entry.variation_tips);
    /// }
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn history(&self) -> Result<Vec<HistoryEntry>> {
        self.ensure_no_pending_recovery()?;
        let tips = self.variation_tips_map()?;
        let head_oid = self.repo.head().ok().and_then(|h| h.target());

        let mut walk = self.repo.revwalk()?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }

        let mut project = FolderChanges::new(&self.repo, self.scope());
        let mut entries = Vec::new();
        for oid in walk {
            let commit = self.repo.find_commit(oid?)?;
            let touches = project.touches(&commit)?;
            entries.push(history_entry_from_commit(
                &commit, touches, &tips, head_oid,
            )?);
        }
        Ok(entries)
    }

    /// Returns the combined version history across **all** local variations.
    ///
    /// Unlike [`Workspace::history`], which only walks the current variation,
    /// this method pushes all variation tips into the revwalk so every commit
    /// reachable from any variation appears exactly once, ordered
    /// topologically (children before parents) then by time.
    ///
    /// Each [`HistoryEntry`] carries:
    /// - `variation_tips` — which variation(s) point at this exact version
    /// - `parent_ids` — the parent version IDs for graph-edge rendering
    /// - `is_head` — whether this is the current `HEAD` commit
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// for entry in workspace.full_history()? {
    ///     let tips: Vec<&str> = entry.variation_tips.iter().map(|id| id.as_str()).collect();
    ///     println!("{} [{}]", entry.version.label, tips.join(", "));
    /// }
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn full_history(&self) -> Result<Vec<HistoryEntry>> {
        self.ensure_no_pending_recovery()?;
        let tips = self.variation_tips_map()?;
        let head_oid = self.repo.head().ok().and_then(|h| h.target());

        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;

        for (_, tip) in self.listed_variation_tip_oids()? {
            walk.push(tip)?;
        }

        let mut project = FolderChanges::new(&self.repo, self.scope());
        let mut entries = Vec::new();
        for oid in walk {
            let commit = self.repo.find_commit(oid?)?;
            let touches = project.touches(&commit)?;
            entries.push(history_entry_from_commit(
                &commit, touches, &tips, head_oid,
            )?);
        }
        Ok(entries)
    }

    /// Returns a graph-ready full-history snapshot over Draftline variations.
    ///
    /// The graph is read-only. It exposes the same version and variation
    /// semantics as [`Workspace::full_history`] but includes explicit nodes,
    /// refs, current markers, dirty state, and optional remote/support-ref
    /// visibility so consuming apps do not need to infer graph structure from
    /// Git ref names.
    ///
    /// When a recovery record is present, the graph is diagnostic: callers
    /// should render recovery UI and avoid offering mutation actions until the
    /// recovery is repaired, rolled back, or acknowledged.
    pub fn workspace_graph(&self, options: WorkspaceGraphOptions) -> Result<WorkspaceGraph> {
        let recovery = self.recovery_state()?;
        let state_may_be_inconsistent = recovery.is_some();
        let current_variation = self
            .current_variation_unchecked()
            .ok()
            .map(VariationId::from);
        let current_version = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(VersionId::from);
        let dirty_files = if state_may_be_inconsistent {
            self.changed_files_unchecked().unwrap_or_default()
        } else {
            self.changed_files_unchecked()?
        };
        let dirty = DirtySummary {
            is_dirty: !dirty_files.is_empty(),
            files: dirty_files,
        };
        let local_tips = if state_may_be_inconsistent {
            self.listed_variation_tip_oids().unwrap_or_default()
        } else {
            self.listed_variation_tip_oids()?
        };
        let remote_variations = if options.include_remotes && state_may_be_inconsistent {
            self.graph_remote_variations(options.remote.as_deref())
                .unwrap_or_default()
        } else if options.include_remotes {
            self.graph_remote_variations(options.remote.as_deref())?
        } else {
            Vec::new()
        };
        let support_refs = if options.include_support_refs && state_may_be_inconsistent {
            self.list_local_support_refs()
                .unwrap_or_default()
                .into_iter()
                .chain(self.list_remote_tracking_support_refs().unwrap_or_default())
                .collect::<Vec<_>>()
        } else if options.include_support_refs {
            self.list_local_support_refs()?
                .into_iter()
                .chain(self.list_remote_tracking_support_refs()?)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let tips = if state_may_be_inconsistent {
            self.variation_tips_map().unwrap_or_default()
        } else {
            self.variation_tips_map()?
        };
        let mut push_oids = local_tips.iter().map(|(_, oid)| *oid).collect::<Vec<_>>();
        let remote_tip_oids = remote_variations
            .iter()
            .filter_map(|remote_variation| {
                remote_variation
                    .head_version
                    .as_ref()
                    .and_then(|version| Oid::from_str(version.id().as_str()).ok())
            })
            .collect::<BTreeSet<_>>();
        let support_tip_oids = support_refs
            .iter()
            .filter_map(|support_ref| Oid::from_str(&support_ref.target_oid).ok())
            .collect::<BTreeSet<_>>();
        push_oids.extend(remote_tip_oids.iter().copied());
        push_oids.extend(support_tip_oids.iter().copied());

        let local_reachable = if state_may_be_inconsistent {
            self.reachable_oids(local_tips.iter().map(|(_, oid)| *oid))
                .unwrap_or_default()
        } else {
            self.reachable_oids(local_tips.iter().map(|(_, oid)| *oid))?
        };
        let support_reachable = if state_may_be_inconsistent {
            self.reachable_oids(support_tip_oids.iter().copied())
                .unwrap_or_default()
        } else {
            self.reachable_oids(support_tip_oids.iter().copied())?
        };
        let remote_reachable = if state_may_be_inconsistent {
            self.reachable_oids(remote_tip_oids.iter().copied())
                .unwrap_or_default()
        } else {
            self.reachable_oids(remote_tip_oids.iter().copied())?
        };
        let mut nodes = Vec::new();
        let start = options.cursor.unwrap_or_default();
        let limit = options.limit.unwrap_or(usize::MAX);
        let end = start.saturating_add(limit);
        let mut total = 0usize;
        let head_oid = current_version
            .as_ref()
            .and_then(|version| Oid::from_str(version.as_str()).ok());

        let walk_result = (|| -> Result<()> {
            let mut walk = self.repo.revwalk()?;
            walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;
            let mut project = FolderChanges::new(&self.repo, self.scope());
            for oid in push_oids {
                walk.push(oid)?;
            }
            for oid in walk {
                let oid = oid?;
                let commit = self.repo.find_commit(oid)?;
                if total >= start && total < end {
                    let touches = project.touches(&commit)?;
                    nodes.push(workspace_graph_node_from_commit(
                        &commit,
                        history_entry_from_commit(&commit, touches, &tips, head_oid)?,
                        &local_reachable,
                        &remote_reachable,
                        &support_reachable,
                        total,
                    )?);
                }
                total += 1;
            }
            Ok(())
        })();
        if state_may_be_inconsistent {
            if walk_result.is_err() {
                nodes.clear();
                total = 0;
            }
        } else {
            walk_result?;
        }

        let refs = if state_may_be_inconsistent {
            self.build_workspace_graph_refs(&local_tips, &remote_variations, &support_refs)
                .unwrap_or_default()
        } else {
            self.build_workspace_graph_refs(&local_tips, &remote_variations, &support_refs)?
        };
        let snapshot_id = workspace_graph_snapshot_id(
            &current_variation,
            &current_version,
            &dirty,
            &nodes,
            &refs,
            total,
        );
        let was_pruned = start > 0 || end < total;
        let has_more = end < total;
        let next_cursor = has_more.then_some(end);

        let mut graph = WorkspaceGraph {
            workspace_id: WorkspaceId {
                root: self.root.clone(),
            },
            current_variation,
            current_version,
            dirty,
            recovery,
            state_may_be_inconsistent,
            nodes,
            refs,
            snapshot_id,
            was_pruned,
            has_more,
            next_cursor,
        };
        annotate_workspace_graph(&mut graph);
        Ok(graph)
    }

    /// Returns graph refs/tips without walking the full node DAG.
    ///
    /// Use this for large-repo overlays, jump lists, and labels before the host
    /// decides whether it needs a full graph page or focused slice.
    pub fn workspace_graph_refs(
        &self,
        options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraphRefs> {
        if options.limit.is_some() || options.cursor.is_some() {
            return Err(DraftlineError::InvalidGraphOptions(
                "refs-only graph requests do not accept limit or cursor".to_string(),
            ));
        }
        let recovery = self.recovery_state()?;
        let state_may_be_inconsistent = recovery.is_some();
        let current_variation = self
            .current_variation_unchecked()
            .ok()
            .map(VariationId::from);
        let current_version = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(VersionId::from);
        let dirty_files = if state_may_be_inconsistent {
            self.changed_files_unchecked().unwrap_or_default()
        } else {
            self.changed_files_unchecked()?
        };
        let dirty = DirtySummary {
            is_dirty: !dirty_files.is_empty(),
            files: dirty_files,
        };
        let local_tips = if state_may_be_inconsistent {
            self.listed_variation_tip_oids().unwrap_or_default()
        } else {
            self.listed_variation_tip_oids()?
        };
        let remote_variations = if options.include_remotes && state_may_be_inconsistent {
            self.graph_remote_variations(options.remote.as_deref())
                .unwrap_or_default()
        } else if options.include_remotes {
            self.graph_remote_variations(options.remote.as_deref())?
        } else {
            Vec::new()
        };
        let support_refs = if options.include_support_refs && state_may_be_inconsistent {
            self.list_local_support_refs()
                .unwrap_or_default()
                .into_iter()
                .chain(self.list_remote_tracking_support_refs().unwrap_or_default())
                .collect::<Vec<_>>()
        } else if options.include_support_refs {
            self.list_local_support_refs()?
                .into_iter()
                .chain(self.list_remote_tracking_support_refs()?)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let refs = if state_may_be_inconsistent {
            self.build_workspace_graph_refs(&local_tips, &remote_variations, &support_refs)
                .unwrap_or_default()
        } else {
            self.build_workspace_graph_refs(&local_tips, &remote_variations, &support_refs)?
        };
        let graph_fingerprint =
            workspace_graph_fingerprint(&current_variation, &current_version, &dirty, &refs);

        Ok(WorkspaceGraphRefs {
            workspace_id: WorkspaceId {
                root: self.root.clone(),
            },
            current_variation,
            current_version,
            dirty,
            recovery,
            state_may_be_inconsistent,
            refs,
            graph_fingerprint,
        })
    }

    /// Returns bounded-payload counts and health signals for large-repo graph rendering.
    ///
    /// This avoids returning the full node payload, but it still walks the graph
    /// to count and classify nodes.
    pub fn workspace_graph_summary(
        &self,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraphSummary> {
        options.limit = None;
        options.cursor = None;
        let graph = self.workspace_graph(options)?;
        let mut child_counts: HashMap<WorkspaceGraphNodeId, usize> = HashMap::new();
        for node in &graph.nodes {
            for parent in &node.parent_ids {
                *child_counts.entry(parent.clone()).or_default() += 1;
            }
        }

        Ok(workspace_graph_summary_from_graph(&graph, &child_counts))
    }

    /// Returns a compressed overview that preserves tips, merges, branch points,
    /// current node, and recent nodes.
    pub fn workspace_graph_overview(
        &self,
        mut options: WorkspaceGraphOverviewOptions,
    ) -> Result<WorkspaceGraph> {
        options.graph.limit = None;
        options.graph.cursor = None;
        let mut graph = self.workspace_graph(options.graph)?;
        let mut child_counts: HashMap<WorkspaceGraphNodeId, usize> = HashMap::new();
        for node in &graph.nodes {
            for parent in &node.parent_ids {
                *child_counts.entry(parent.clone()).or_default() += 1;
            }
        }
        let ref_targets = graph
            .refs
            .iter()
            .map(|graph_ref| graph_ref.target.clone())
            .collect::<BTreeSet<_>>();
        let mut keep = graph
            .nodes
            .iter()
            .filter(|node| {
                node.topo_index < options.recent_nodes
                    || node.is_head
                    || !node.variation_tips.is_empty()
                    || node.parent_ids.len() > 1
                    || child_counts.get(&node.id).copied().unwrap_or_default() > 1
                    || ref_targets.contains(&node.id)
            })
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        if keep.len() > options.max_nodes {
            let mut kept = graph
                .nodes
                .iter()
                .filter(|node| keep.contains(&node.id))
                .map(|node| node.id.clone())
                .take(options.max_nodes)
                .collect::<BTreeSet<_>>();
            std::mem::swap(&mut keep, &mut kept);
        }
        let original_node_count = graph.nodes.len();
        graph.nodes.retain(|node| keep.contains(&node.id));
        graph
            .refs
            .retain(|graph_ref| keep.contains(&graph_ref.target));
        graph.was_pruned = graph.nodes.len() < original_node_count;
        graph.has_more = false;
        graph.next_cursor = None;
        annotate_workspace_graph(&mut graph);
        graph.snapshot_id = workspace_graph_snapshot_id(
            &graph.current_variation,
            &graph.current_version,
            &graph.dirty,
            &graph.nodes,
            &graph.refs,
            graph.nodes.len(),
        );
        Ok(graph)
    }

    /// Returns a topological-sort window around one version.
    ///
    /// `radius` is measured by topo-index distance in the rendered graph order,
    /// not by DAG hop count, so sibling branch nodes can appear in the slice.
    pub fn workspace_graph_around_version(
        &self,
        version: &VersionId,
        radius: usize,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraph> {
        options.limit = None;
        options.cursor = None;
        let mut graph = self.workspace_graph(options)?;
        let Some(center) = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == version)
            .map(|node| node.topo_index)
        else {
            return Err(DraftlineError::VersionNotFound(version.to_string()));
        };
        let start = center.saturating_sub(radius);
        let end = center.saturating_add(radius);
        let original_node_count = graph.nodes.len();
        graph
            .nodes
            .retain(|node| node.topo_index >= start && node.topo_index <= end);
        let kept = graph
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        graph
            .refs
            .retain(|graph_ref| kept.contains(&graph_ref.target));
        graph.was_pruned =
            graph.nodes.len() < original_node_count || graph_has_missing_parent(&graph);
        graph.has_more = false;
        graph.next_cursor = None;
        annotate_workspace_graph(&mut graph);
        graph.snapshot_id = workspace_graph_snapshot_id(
            &graph.current_variation,
            &graph.current_version,
            &graph.dirty,
            &graph.nodes,
            &graph.refs,
            graph.nodes.len(),
        );
        Ok(graph)
    }

    /// Returns the ancestor lane for a variation or remote/support ref target.
    pub fn workspace_graph_for_variation(
        &self,
        variation: &VariationId,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraph> {
        options.include_remotes = true;
        options.include_support_refs = true;
        options.limit = None;
        options.cursor = None;
        let mut graph = self.workspace_graph(options)?;
        let Some(target) = graph
            .refs
            .iter()
            .find(|graph_ref| graph_ref.variation.as_ref() == Some(variation))
            .map(|graph_ref| graph_ref.target.clone())
        else {
            return Err(DraftlineError::VariationNotFound(variation.to_string()));
        };

        let original_node_count = graph.nodes.len();
        let by_id = graph
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.parent_ids.clone()))
            .collect::<HashMap<_, _>>();
        let mut keep = BTreeSet::new();
        let mut stack = vec![target];
        while let Some(id) = stack.pop() {
            if !keep.insert(id.clone()) {
                continue;
            }
            if let Some(parents) = by_id.get(&id) {
                stack.extend(parents.iter().cloned());
            }
        }
        graph.nodes.retain(|node| keep.contains(&node.id));
        graph
            .refs
            .retain(|graph_ref| keep.contains(&graph_ref.target));
        graph.was_pruned =
            graph.nodes.len() < original_node_count || graph_has_missing_parent(&graph);
        graph.has_more = false;
        graph.next_cursor = None;
        annotate_workspace_graph(&mut graph);
        graph.snapshot_id = workspace_graph_snapshot_id(
            &graph.current_variation,
            &graph.current_version,
            &graph.dirty,
            &graph.nodes,
            &graph.refs,
            graph.nodes.len(),
        );
        Ok(graph)
    }

    /// Returns an agent-oriented graph summary with safe follow-up command hints.
    pub fn workspace_graph_agent_summary(
        &self,
        options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraphAgentSummary> {
        let mut refs_options = options.clone();
        refs_options.limit = None;
        refs_options.cursor = None;
        let refs = self.workspace_graph_refs(refs_options)?;
        let summary = self.workspace_graph_summary(options)?;
        let current_ref = refs
            .refs
            .iter()
            .find(|graph_ref| {
                graph_ref.kind == WorkspaceGraphRefKind::LocalVariation
                    && graph_ref.variation == refs.current_variation
            })
            .cloned();
        let nearby_refs = refs
            .refs
            .iter()
            .filter(|graph_ref| Some(&graph_ref.target_version) == refs.current_version.as_ref())
            .cloned()
            .collect::<Vec<_>>();
        let mut warnings = Vec::new();
        if summary.state_may_be_inconsistent {
            warnings.push(
                "workspace has recovery state; avoid graph mutations until repaired".to_string(),
            );
        }
        if summary.remote_only_nodes > 0 {
            warnings.push(
                "remote-only graph nodes require adoption before local branching".to_string(),
            );
        }
        if summary.support_ref_only_nodes > 0 {
            warnings.push(
                "support-ref-only nodes should be restored through support-ref workflows"
                    .to_string(),
            );
        }
        let mut suggested_next_commands = vec![
            "get_workspace_graph_refs".to_string(),
            "get_workspace_graph_summary".to_string(),
            "get_workspace_graph_around_version".to_string(),
            "get_workspace_graph_neighborhood".to_string(),
            "search_workspace_graph".to_string(),
            "get_workspace_graph_node".to_string(),
            "get_workspace_graph_compare_summary".to_string(),
        ];
        if summary.local_ref_count > 0 {
            suggested_next_commands.push("get_workspace_graph_for_variation".to_string());
        }

        Ok(WorkspaceGraphAgentSummary {
            summary,
            suggested_next_commands,
            warnings,
            current_ref,
            nearby_refs,
        })
    }

    /// Returns a DAG-hop neighborhood around one version.
    pub fn workspace_graph_neighborhood(
        &self,
        version: &VersionId,
        radius: usize,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraph> {
        options.limit = None;
        options.cursor = None;
        let mut graph = self.workspace_graph(options)?;
        let Some(center) = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == version)
            .map(|node| node.id.clone())
        else {
            return Err(DraftlineError::VersionNotFound(version.to_string()));
        };

        let by_id = graph
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.clone()))
            .collect::<HashMap<_, _>>();
        let original_node_count = graph.nodes.len();
        let mut keep = BTreeSet::new();
        let mut queue = VecDeque::from([(center, 0usize)]);
        while let Some((id, distance)) = queue.pop_front() {
            if !keep.insert(id.clone()) || distance >= radius {
                continue;
            }
            let Some(node) = by_id.get(&id) else {
                continue;
            };
            for neighbor in node.parent_ids.iter().chain(node.child_ids.iter()) {
                queue.push_back((neighbor.clone(), distance + 1));
            }
        }
        apply_workspace_graph_node_filter(&mut graph, keep, original_node_count);
        Ok(graph)
    }

    /// Searches graph nodes and refs by label, author, ref name, or version prefix.
    pub fn search_workspace_graph(
        &self,
        query: impl AsRef<str>,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraphSearchResult> {
        let query = query.as_ref().trim().to_string();
        let limit = options.limit.unwrap_or(50);
        options.limit = None;
        options.cursor = None;
        let mut graph = self.workspace_graph(options)?;
        let query_lower = query.to_lowercase();
        let matched_node_ids = graph
            .nodes
            .iter()
            .filter(|node| workspace_graph_node_matches(node, &query_lower))
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let matched_node_count = matched_node_ids.len();
        let all_matched_refs = graph
            .refs
            .iter()
            .filter(|graph_ref| workspace_graph_ref_matches(graph_ref, &query_lower))
            .cloned()
            .collect::<Vec<_>>();
        let total_matches = matched_node_count + all_matched_refs.len();
        let ref_target_ids = all_matched_refs
            .iter()
            .map(|graph_ref| graph_ref.target.clone())
            .collect::<BTreeSet<_>>();
        let mut keep_order = matched_node_ids;
        for node in &graph.nodes {
            if ref_target_ids.contains(&node.id) && !keep_order.contains(&node.id) {
                keep_order.push(node.id.clone());
            }
        }
        let keep = keep_order.into_iter().take(limit).collect::<BTreeSet<_>>();
        let matched_refs = all_matched_refs
            .into_iter()
            .filter(|graph_ref| keep.contains(&graph_ref.target))
            .collect::<Vec<_>>();
        let original_node_count = graph.nodes.len();
        apply_workspace_graph_node_filter(&mut graph, keep, original_node_count);
        Ok(WorkspaceGraphSearchResult {
            graph,
            matched_refs,
            query,
            matched_node_count,
            total_matches,
        })
    }

    /// Returns a graph path between two versions through their common ancestor.
    pub fn workspace_graph_path(
        &self,
        from: &VersionId,
        to: &VersionId,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraphPath> {
        options.limit = None;
        options.cursor = None;
        let graph = self.workspace_graph(options)?;
        let from_id = workspace_graph_node_id_from_version(from)?;
        let to_id = workspace_graph_node_id_from_version(to)?;
        let by_id = graph
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.parent_ids.clone()))
            .collect::<HashMap<_, _>>();
        let version_by_id = graph
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.version.id().clone()))
            .collect::<HashMap<_, _>>();
        if !by_id.contains_key(&from_id) {
            return Err(DraftlineError::VersionNotFound(from.to_string()));
        }
        if !by_id.contains_key(&to_id) {
            return Err(DraftlineError::VersionNotFound(to.to_string()));
        }
        // Use the graph's own (possibly scope-simplified) parents so the
        // ancestor is always a node the caller can see.
        let ancestor = workspace_graph_nearest_common_ancestor(&by_id, &from_id, &to_id)
            .and_then(|id| version_by_id.get(&id).cloned());
        let node_ids = if let Some(ancestor) = ancestor.as_ref() {
            let ancestor_id = workspace_graph_node_id_from_version(ancestor)?;
            let mut left = workspace_graph_path_to_ancestor(&by_id, &from_id, &ancestor_id)
                .unwrap_or_default();
            let mut right =
                workspace_graph_path_to_ancestor(&by_id, &to_id, &ancestor_id).unwrap_or_default();
            right.reverse();
            left.extend(right.into_iter().skip(1));
            left
        } else {
            Vec::new()
        };
        let version_ids = node_ids
            .iter()
            .filter_map(|id| version_by_id.get(id))
            .cloned()
            .collect::<Vec<_>>();
        Ok(WorkspaceGraphPath {
            from_version: from.clone(),
            to_version: to.clone(),
            found: !node_ids.is_empty(),
            node_ids,
            version_ids,
            common_ancestor: ancestor,
        })
    }

    /// Returns the nearest common ancestor of two graph versions.
    pub fn workspace_graph_common_ancestor(
        &self,
        left: &VersionId,
        right: &VersionId,
    ) -> Result<WorkspaceGraphCommonAncestor> {
        let left_oid = Oid::from_str(left.as_str())
            .map_err(|_| DraftlineError::VersionNotFound(left.to_string()))?;
        let right_oid = Oid::from_str(right.as_str())
            .map_err(|_| DraftlineError::VersionNotFound(right.to_string()))?;
        let merge_base = match self.repo.merge_base(left_oid, right_oid) {
            Ok(oid) => Some(oid),
            Err(error) if error.code() == git2::ErrorCode::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let common_ancestor = merge_base.map(VersionId::from);
        Ok(WorkspaceGraphCommonAncestor {
            left_version: left.clone(),
            right_version: right.clone(),
            common_ancestor,
        })
    }

    /// Returns one graph node with refs and a lightweight change summary.
    pub fn workspace_graph_node(
        &self,
        version: &VersionId,
        mut options: WorkspaceGraphOptions,
    ) -> Result<WorkspaceGraphNodeDetail> {
        options.limit = None;
        options.cursor = None;
        options.include_remotes = true;
        options.include_support_refs = true;
        let graph = self.workspace_graph(options)?;
        let Some(node) = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == version)
            .cloned()
        else {
            return Err(DraftlineError::VersionNotFound(version.to_string()));
        };
        let refs = graph
            .refs
            .iter()
            .filter(|graph_ref| graph_ref.target == node.id)
            .cloned()
            .collect::<Vec<_>>();
        // A commit that doesn't touch the project matches some parent's project
        // tree, so it has no project changes even when it is a merge.
        let changed_files = if !node.touches_folder {
            Vec::new()
        } else if let Some(parent) = node.parent_version_ids.first() {
            self.diff_versions(parent, version)?.files
        } else {
            Vec::new()
        };
        Ok(WorkspaceGraphNodeDetail {
            node,
            refs,
            changed_file_count: Some(changed_files.len()),
            changed_files,
        })
    }

    /// Returns compare metadata without requiring a full graph render.
    pub fn workspace_graph_compare_summary(
        &self,
        from: &VersionId,
        to: &VersionId,
    ) -> Result<WorkspaceGraphCompareSummary> {
        let diff = self.diff_versions(from, to)?;
        let common_ancestor = self
            .workspace_graph_common_ancestor(from, to)?
            .common_ancestor;
        Ok(WorkspaceGraphCompareSummary {
            from_version: from.clone(),
            to_version: to.clone(),
            changed_file_count: diff.files.len(),
            files: diff.files,
            action_hints: vec![workspace_graph_action_hint(
                WorkspaceGraphAction::CompareToCurrent,
                true,
                None,
            )],
            common_ancestor,
        })
    }

    /// Returns a per-variation snapshot with head version and total version count.
    ///
    /// This is the primary entry point for a variation picker UI that needs to
    /// show, for every variation, its display name, tip version label, and how
    /// many versions it contains — without switching to each variation.
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// for summary in workspace.variation_summaries()? {
    ///     println!(
    ///         "{}: {} version(s) reachable",
    ///         summary.variation.display_label(),
    ///         summary.reachable_version_count
    ///     );
    /// }
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn variation_summaries(&self) -> Result<Vec<VariationSummary>> {
        self.ensure_no_pending_recovery()?;
        let current = self.current_variation_unchecked().ok();
        let mut summaries = Vec::new();

        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };
            if !self.lists_variation(name, current.as_deref()) {
                continue;
            }
            let metadata = self.read_variation_metadata(name)?;
            let variation = variation_from_name(name.to_string(), current.as_ref(), metadata);

            let (head_version, reachable_version_count) = match branch.get().peel_to_commit() {
                Ok(tip) => {
                    let head_version = Some(version_from_commit(&tip));
                    let mut walk = self.repo.revwalk()?;
                    walk.push(tip.id())?;
                    let mut count = 0;
                    for oid in walk {
                        oid?;
                        count += 1;
                    }
                    (head_version, count)
                }
                Err(_) => (None, 0),
            };

            summaries.push(VariationSummary {
                variation,
                head_version,
                reachable_version_count,
            });
        }

        summaries.sort_by(|a, b| a.variation.name.cmp(&b.variation.name));
        Ok(summaries)
    }

    /// Preflights applying incoming changes from a remote without mutating the workspace.
    ///
    /// Call this before [`Workspace::apply_incoming`] to let the host UI
    /// display a summary of what would happen.  It checks workspace cleanliness
    /// and evaluates the **cached** remote-tracking state; it does **not** fetch
    /// or modify any files or Git refs.
    ///
    /// **Important:** call [`Workspace::fetch_remote`] first to ensure
    /// `sync_status` reflects the current remote state.  Stale remote-tracking
    /// refs will cause an inaccurate `is_fast_forward` / `can_proceed` result.
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// let report = workspace.preflight_apply_incoming("origin")?;
    /// if report.can_proceed {
    ///     println!("{} version(s) incoming", report.sync_status.behind);
    /// }
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn preflight_apply_incoming(&self, remote: impl AsRef<str>) -> Result<ApplyIncomingReport> {
        self.ensure_no_pending_recovery()?;
        self.ensure_on_branch()?;
        let sync_status = self.sync_status(remote)?;
        let dirty_files = self.changed_files_unchecked()?;

        let is_fast_forward = matches!(sync_status.state, SyncState::IncomingAvailable);
        let file_hazards = if is_fast_forward {
            let remote_ref = format!(
                "refs/remotes/{}/{}",
                sync_status.remote, sync_status.variation
            );
            let remote_oid = self.repo.refname_to_id(&remote_ref)?;
            let hazard_commit = self.repo.find_commit(remote_oid)?;
            self.target_tree_hazards(&hazard_commit.tree()?)?
        } else {
            Vec::new()
        };
        let can_proceed = dirty_files.is_empty() && file_hazards.is_empty() && is_fast_forward;

        Ok(ApplyIncomingReport {
            sync_status,
            dirty_files,
            file_hazards,
            is_fast_forward,
            can_proceed,
        })
    }

    /// Preflights a diverged incoming merge without mutating workspace state.
    pub fn preflight_merge_incoming(&self, remote: impl AsRef<str>) -> Result<MergeIncomingReport> {
        self.ensure_no_pending_recovery()?;
        let sync_status = self.sync_status(remote)?;
        self.preflight_merge_from(sync_status, None)
    }

    /// Compares the current variation with the base branch (usually `main`) as
    /// last fetched; `variation` in the result names the base. Use it to show
    /// "behind main by N" without changing anything.
    pub fn base_status(&self, remote: impl AsRef<str>) -> Result<SyncStatus> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref();
        let base = self
            .default_branch_on(remote)
            .ok_or(DraftlineError::SendBaseUnknown)?;
        self.base_status_unchecked(remote, &base)
    }

    /// Preflights bringing the latest base branch into the current variation.
    /// The token works with [`Self::merge_incoming`] and
    /// [`Self::merge_incoming_with_resolutions`]. Developers' changes outside
    /// the project come along as-is; conflicts there need a developer.
    pub fn preflight_merge_base(&self, remote: impl AsRef<str>) -> Result<MergeIncomingReport> {
        let status = self.base_status(remote)?;
        // On the base itself this is an ordinary incoming merge; when only
        // behind, use [`Self::apply_incoming`] to fast-forward instead.
        let source = (status.variation != self.current_variation_unchecked()?)
            .then(|| status.variation.clone());
        self.preflight_merge_from(status, source)
    }

    fn base_status_unchecked(&self, remote: &str, base: &str) -> Result<SyncStatus> {
        let local = self.repo.head()?.peel_to_commit()?.id();
        let Ok(remote_oid) = self
            .repo
            .refname_to_id(&format!("refs/remotes/{remote}/{base}"))
        else {
            return Ok(SyncStatus {
                remote: remote.to_string(),
                variation: base.to_string(),
                ahead: 0,
                behind: 0,
                state: SyncState::NoRemoteVersion,
                incoming: Vec::new(),
                base_renamed_from: self.base_renamed_from(remote, base),
            });
        };
        let (ahead, behind) = self.repo.graph_ahead_behind(local, remote_oid)?;
        let state = match (ahead, behind) {
            (0, 0) => SyncState::UpToDate,
            (_, 0) => SyncState::LocalAhead,
            (0, _) => SyncState::IncomingAvailable,
            _ => SyncState::NeedsMerge,
        };
        Ok(SyncStatus {
            remote: remote.to_string(),
            variation: base.to_string(),
            ahead,
            behind,
            state,
            incoming: self.incoming_versions(local, remote_oid)?,
            base_renamed_from: self.base_renamed_from(remote, base),
        })
    }

    fn mergeable_state(state: &SyncState, from_base: bool) -> bool {
        *state == SyncState::NeedsMerge || (from_base && *state == SyncState::IncomingAvailable)
    }

    fn preflight_merge_from(
        &self,
        sync_status: SyncStatus,
        source: Option<String>,
    ) -> Result<MergeIncomingReport> {
        self.ensure_on_branch()?;
        let dirty_files = self.changed_files_unchecked()?;
        let mut file_hazards = Vec::new();
        let mut conflicts = Vec::new();
        let mut developer_conflicts = Vec::new();
        let mut token = None;

        if Self::mergeable_state(&sync_status.state, source.is_some()) {
            let merge_input = self.merge_input_for_status(&sync_status)?;
            let (trees, dropped) = self.merge_trees_for(&merge_input)?;
            let plan = self.plan_clean_merge(&trees[0], &trees[1], &trees[2])?;
            if !dropped.is_empty() {
                developer_conflicts = dropped;
            } else {
                match self.merged_tree(&trees, &plan.files)? {
                    Ok(tree) => {
                        file_hazards = self.target_tree_hazards(&self.repo.find_tree(tree)?)?
                    }
                    Err(paths) => developer_conflicts = paths,
                }
            }
            conflicts = plan.conflicts;
            if dirty_files.is_empty() && file_hazards.is_empty() && developer_conflicts.is_empty() {
                token = Some(MergeIncomingToken {
                    remote: sync_status.remote.clone(),
                    variation: self.current_variation_unchecked()?,
                    local_oid: merge_input.local_commit.id().to_string(),
                    remote_oid: merge_input.remote_commit.id().to_string(),
                    merge_base_oid: merge_input.base_commit.id().to_string(),
                    source,
                });
            }
        }
        let can_merge_cleanly = token.is_some() && conflicts.is_empty();

        Ok(MergeIncomingReport {
            sync_status,
            dirty_files,
            file_hazards,
            conflicts,
            developer_conflicts,
            token,
            can_merge_cleanly,
            changed_workspace: false,
        })
    }

    /// Writes a clean incoming merge as a new two-parent version.
    pub fn merge_incoming(
        &self,
        token: MergeIncomingToken,
        label: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<MergeIncomingResult> {
        self.merge_incoming_with_optional_profile(token, label, options, None)
    }

    /// Writes a clean incoming merge as a new two-parent version using host-supplied attribution.
    pub fn merge_incoming_with_profile(
        &self,
        token: MergeIncomingToken,
        label: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
        profile: &ContributorProfile,
    ) -> Result<MergeIncomingResult> {
        self.merge_incoming_with_optional_profile(token, label, options, Some(profile))
    }

    fn merge_incoming_with_optional_profile(
        &self,
        token: MergeIncomingToken,
        label: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
        profile: Option<&ContributorProfile>,
    ) -> Result<MergeIncomingResult> {
        self.merge_incoming_core(
            "merge_incoming",
            token,
            label.as_ref(),
            None,
            options,
            profile,
        )
    }

    /// Writes an incoming merge using explicit user-provided conflict resolutions.
    pub fn merge_incoming_with_resolutions(
        &self,
        token: MergeIncomingToken,
        label: impl AsRef<str>,
        resolutions: impl IntoIterator<Item = MergeConflictResolution>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<MergeIncomingResult> {
        self.merge_incoming_with_resolutions_and_optional_profile(
            token,
            label,
            resolutions,
            options,
            None,
        )
    }

    /// Writes an incoming merge with explicit resolutions using host-supplied attribution.
    pub fn merge_incoming_with_resolutions_and_profile(
        &self,
        token: MergeIncomingToken,
        label: impl AsRef<str>,
        resolutions: impl IntoIterator<Item = MergeConflictResolution>,
        options: &mut RemoteOptions<'_>,
        profile: &ContributorProfile,
    ) -> Result<MergeIncomingResult> {
        self.merge_incoming_with_resolutions_and_optional_profile(
            token,
            label,
            resolutions,
            options,
            Some(profile),
        )
    }

    fn merge_incoming_with_resolutions_and_optional_profile(
        &self,
        token: MergeIncomingToken,
        label: impl AsRef<str>,
        resolutions: impl IntoIterator<Item = MergeConflictResolution>,
        options: &mut RemoteOptions<'_>,
        profile: Option<&ContributorProfile>,
    ) -> Result<MergeIncomingResult> {
        self.merge_incoming_core(
            "merge_incoming_with_resolutions",
            token,
            label.as_ref(),
            Some(resolutions.into_iter().collect()),
            options,
            profile,
        )
    }

    fn merge_incoming_core(
        &self,
        operation: &str,
        token: MergeIncomingToken,
        label: &str,
        resolutions: Option<Vec<MergeConflictResolution>>,
        options: &mut RemoteOptions<'_>,
        profile: Option<&ContributorProfile>,
    ) -> Result<MergeIncomingResult> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index(operation)?;
        self.ensure_on_branch()?;
        let dirty_files = self.changed_files_unchecked()?;
        if !dirty_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                operation,
                true,
                dirty_files,
                Vec::new(),
                None,
            ))));
        }

        let variation = self.current_variation_unchecked()?;
        let local_oid = self.repo.head()?.peel_to_commit()?.id().to_string();
        if variation != token.variation || local_oid != token.local_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", token.variation, token.local_oid),
                actual: format!("{variation}@{local_oid}"),
            });
        }

        let from_base = token
            .source
            .as_ref()
            .is_some_and(|source| *source != token.variation);
        let source = token
            .source
            .clone()
            .unwrap_or_else(|| token.variation.clone());
        let status = if from_base {
            self.fetch_remote_variation_ref(&token.remote, &source, options)?;
            self.base_status_unchecked(&token.remote, &source)?
        } else {
            self.fetch_remote_unchecked(&token.remote, options)?;
            self.sync_status(&token.remote)?
        };
        if !Self::mergeable_state(&status.state, from_base) {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }
        let remote_oid = remote_tracking_oid(&self.repo, &token.remote, &source);
        if remote_oid.as_deref() != Some(token.remote_oid.as_str()) {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: source,
                expected: Some(token.remote_oid),
                actual: remote_oid,
            });
        }

        let merge_input = self.merge_input_for_status(&status)?;
        if merge_input.base_commit.id().to_string() != token.merge_base_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: token.merge_base_oid,
                actual: merge_input.base_commit.id().to_string(),
            });
        }

        let (trees, dropped) = self.merge_trees_for(&merge_input)?;
        if !dropped.is_empty() {
            return Err(DraftlineError::MergeNeedsDeveloper(dropped));
        }
        let plan = self.plan_clean_merge(&trees[0], &trees[1], &trees[2])?;
        let files = match resolutions {
            Some(resolutions) => self.resolve_merge_plan(plan, resolutions)?,
            None if !plan.conflicts.is_empty() => {
                return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                    operation,
                    true,
                    Vec::new(),
                    Vec::new(),
                    Some(plan.conflicts[0].label.clone()),
                ))));
            }
            None => plan.files,
        };
        let tree = match self.merged_tree(&trees, &files)? {
            Ok(tree) => self.repo.find_tree(tree)?,
            Err(paths) => return Err(DraftlineError::MergeNeedsDeveloper(paths)),
        };
        let file_hazards = self.target_tree_hazards(&tree)?;
        if !file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                operation,
                true,
                Vec::new(),
                file_hazards,
                None,
            ))));
        }

        self.write_incoming_merge_version(variation, &merge_input, label, &tree, files, profile)
    }

    fn ensure_head_unchanged(&self, variation: &str, oid: git2::Oid) -> Result<()> {
        let now_variation = self.current_variation_unchecked()?;
        let now_oid = self.repo.head()?.peel_to_commit()?.id();
        if now_variation != variation || now_oid != oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{variation}@{oid}"),
                actual: format!("{now_variation}@{now_oid}"),
            });
        }
        Ok(())
    }

    /// Applies incoming changes from a remote using a fast-forward when possible.
    ///
    /// This method is safe to call when [`ApplyIncomingReport::can_proceed`] is
    /// `true`.  It fetches the latest remote state, then fast-forwards the local
    /// variation to match.
    ///
    /// Returns [`DraftlineError::SyncNeedsMerge`] when the histories have
    /// diverged (`NeedsMerge`); in that case the host UI should surface an
    /// explicit conflict-resolution flow rather than calling this method.
    ///
    /// The workspace must be clean (no unsaved changes) before calling.
    ///
    /// ```no_run
    /// use draftline::{RemoteOptions, Workspace};
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// let mut options = RemoteOptions::new();
    /// let result = workspace.apply_incoming("origin", &mut options)?;
    /// println!("{} version(s) applied", result.applied_count);
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn apply_incoming(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<ApplyIncomingResult> {
        self.ensure_no_pending_recovery()?;
        let _lock = self.acquire_for_index("apply_incoming")?;
        self.ensure_on_branch()?;

        let dirty_files = self.changed_files_unchecked()?;
        if !dirty_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "apply_incoming",
                true,
                dirty_files,
                Vec::new(),
                None,
            ))));
        }

        // Pin the branch and commit the guard approved: a developer may switch
        // branches in the shared checkout while the fetch runs.
        let variation = self.current_variation_unchecked()?;
        let local_oid = self.repo.head()?.peel_to_commit()?.id();
        let remote_name = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote_name, options)?;
        self.fetch_support_refs_with_options(&remote_name, options)?;
        test_hooks::fire("after_apply_fetch", self.repo.path());
        self.ensure_head_unchanged(&variation, local_oid)?;
        let status = self.sync_status(&remote_name)?;

        match status.state {
            SyncState::UpToDate | SyncState::LocalAhead | SyncState::NoRemoteVersion => {
                return Ok(ApplyIncomingResult { applied_count: 0 });
            }
            SyncState::NeedsMerge => {
                return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
            }
            SyncState::IncomingAvailable => {}
        }

        let applied_count = status.behind;
        let remote_ref = format!("refs/remotes/{remote_name}/{variation}");
        let remote_oid = self.repo.refname_to_id(&remote_ref)?;
        let hazard_commit = self.repo.find_commit(remote_oid)?;
        let file_hazards = self.target_tree_hazards(&hazard_commit.tree()?)?;
        if !file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "apply_incoming",
                true,
                Vec::new(),
                file_hazards,
                None,
            ))));
        }
        let branch_ref = format!("refs/heads/{variation}");

        let operation_id = new_operation_id();
        let target_oid = remote_oid;
        let checkout_commit = hazard_commit;

        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ApplyIncoming,
            original_variation: Some(variation.clone()),
            target: Some(target_oid.to_string()),
            completed: false,
        })?;

        // Git has no lock against an external `git switch`; recheck right
        // before touching files to keep that window as small as possible.
        self.ensure_head_unchanged(&variation, local_oid)?;
        // Bring files up to the incoming tree like `git merge --ff-only`: local
        // edits are never overwritten. This runs before the ref moves so HEAD
        // is still the checkout baseline.
        self.checkout_or_keep_recovery(&checkout_commit.tree()?)?;
        self.repo
            .reference_matching(&branch_ref, target_oid, true, local_oid, "apply_incoming")?;

        self.repo.set_head(&branch_ref)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ApplyIncoming,
            original_variation: None,
            target: Some(target_oid.to_string()),
            completed: true,
        })?;

        Ok(ApplyIncomingResult { applied_count })
    }

    /// Returns a diff between two specific versions.
    ///
    /// The patch field contains a unified diff suitable for display.  When both
    /// versions are identical the patch is `None` and `files` is empty.
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// let versions = workspace.versions()?;
    /// if versions.len() >= 2 {
    ///     let diff = workspace.diff_versions(versions[1].id(), versions[0].id())?;
    ///     println!("{} file(s) changed", diff.files.len());
    /// }
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn diff_versions(&self, from: &VersionId, to: &VersionId) -> Result<VersionDiff> {
        self.ensure_no_pending_recovery()?;
        let from_commit = self.find_version_commit(from)?;
        let to_commit = self.find_version_commit(to)?;
        let from_tree = self.relocated_tree(&from_commit)?;
        let to_tree = self.relocated_tree(&to_commit)?;

        let mut opts = DiffOptions::new();
        limit_diff_to_scope(&mut opts, self.scope());
        let diff =
            self.repo
                .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut opts))?;

        let files = diff_deltas_to_changed_files(&diff);
        let patch = diff_to_patch_text(&diff)?;

        Ok(VersionDiff {
            from_version: Some(from.clone()),
            to_version: Some(to.clone()),
            files,
            patch: if patch.is_empty() { None } else { Some(patch) },
        })
    }

    /// Returns a diff between a version and the current workspace files.
    ///
    /// This is similar to [`Workspace::changes`] but lets the host UI diff any
    /// historical version against the live workspace, not just `HEAD`.  The
    /// content policy is applied: files excluded by the policy are omitted.
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// let versions = workspace.versions()?;
    /// if let Some(version) = versions.last() {
    ///     let diff = workspace.diff_version_to_workspace(version.id())?;
    ///     println!("{} file(s) differ from version", diff.files.len());
    /// }
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn diff_version_to_workspace(&self, version: &VersionId) -> Result<VersionDiff> {
        self.ensure_no_pending_recovery()?;
        let commit = self.find_version_commit(version)?;
        let tree = self.relocated_tree(&commit)?;

        let mut opts = DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        limit_diff_to_scope(&mut opts, self.scope());
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))?;

        let files =
            diff_deltas_to_changed_files_with_policy(&diff, &self.root, &self.content_policy)?;
        let patch = diff_to_patch_text(&diff)?;

        Ok(VersionDiff {
            from_version: Some(version.clone()),
            to_version: None,
            files,
            patch: if patch.is_empty() { None } else { Some(patch) },
        })
    }

    /// Returns a diff and live preview for one tracked file in the workspace.
    pub fn diff_workspace_file(&self, path: impl AsRef<Path>) -> Result<Option<CurrentFileDiff>> {
        self.ensure_no_pending_recovery()?;
        let path = normalize_workspace_relative(path)?;
        if !self.content_policy.tracks(&path)? {
            return Ok(None);
        }

        let head_tree = self.repo.head()?.peel_to_commit()?.tree()?;
        let mut opts = DiffOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .disable_pathspec_match(true)
            .pathspec(&path);
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))?;
        let files =
            diff_deltas_to_changed_files_with_policy(&diff, &self.root, &self.content_policy)?;
        let file = files.into_iter().find(|file| file.path == path);
        let patch = diff_to_patch_text(&diff)?;
        let preview = self.preview_workspace_file(&path)?;

        Ok(Some(CurrentFileDiff {
            path,
            file,
            patch: if patch.is_empty() { None } else { Some(patch) },
            preview,
        }))
    }

    /// Returns the current variation name when the workspace is on a normal variation.
    pub fn current_variation(&self) -> Result<String> {
        self.ensure_no_pending_recovery()?;
        self.current_variation_unchecked()
    }

    fn current_variation_unchecked(&self) -> Result<String> {
        match self.repo.head() {
            Ok(head) => {
                // Reject detached HEAD — the resolved reference must be a local branch
                // (name starts with "refs/heads/") so callers can safely rewrite it.
                if !head.is_branch() {
                    return Err(DraftlineError::NoCurrentVariation);
                }
                let Ok(name) = head.shorthand() else {
                    return Err(DraftlineError::NoCurrentVariation);
                };
                Ok(name.to_string())
            }
            Err(error) if error.code() == git2::ErrorCode::UnbornBranch => {
                // New repository with no commits — derive the intended initial branch
                // name from the HEAD symbolic reference (e.g. refs/heads/main → "main").
                self.repo
                    .find_reference("HEAD")
                    .ok()
                    .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_string))
                    .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_string))
                    .ok_or(DraftlineError::NoCurrentVariation)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn head_symbolic_variation(&self) -> Option<String> {
        self.repo
            .find_reference("HEAD")
            .ok()
            .and_then(|reference| {
                reference
                    .symbolic_target()
                    .ok()
                    .flatten()
                    .map(str::to_string)
            })
            .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_string))
    }

    fn resolve_restore_version_target(
        &self,
        target: RestoreVersionTarget,
        current_variation: &str,
    ) -> Result<(String, VariationMetadata, Oid)> {
        match target {
            RestoreVersionTarget::Current => {
                self.ensure_on_branch()?;
                let parent_oid = self.repo.head()?.peel_to_commit()?.id();
                let metadata = self.read_variation_metadata(current_variation)?;
                Ok((current_variation.to_string(), metadata, parent_oid))
            }
            RestoreVersionTarget::Existing { variation } => {
                if variation.as_str() != current_variation {
                    self.ensure_variation_in_namespace(variation.as_str())?;
                } else {
                    self.ensure_on_branch()?;
                }
                let branch = self
                    .repo
                    .find_branch(variation.as_str(), BranchType::Local)
                    .map_err(|error| match error.code() {
                        git2::ErrorCode::NotFound => {
                            DraftlineError::VariationNotFound(variation.as_str().to_string())
                        }
                        _ => DraftlineError::from(error),
                    })?;
                let parent_oid = branch.get().peel_to_commit()?.id();
                let metadata = self.read_variation_metadata(variation.as_str())?;
                Ok((variation.as_str().to_string(), metadata, parent_oid))
            }
            RestoreVersionTarget::New { name, metadata } => {
                let name = validate_variation_name(&name)?;
                self.ensure_variation_in_namespace(&name)?;
                match self.repo.find_branch(&name, BranchType::Local) {
                    Ok(_) => Err(DraftlineError::VariationAlreadyExists(name)),
                    Err(error) if error.code() == git2::ErrorCode::NotFound => {
                        let parent_oid = self.repo.head()?.peel_to_commit()?.id();
                        Ok((name, metadata, parent_oid))
                    }
                    Err(error) => Err(error.into()),
                }
            }
        }
    }

    /// Adds or updates a remote endpoint for sharing/backing up this workspace.
    pub fn add_remote(
        &self,
        name: impl AsRef<str>,
        url: impl AsRef<str>,
    ) -> Result<RemoteEndpoint> {
        self.ensure_no_pending_recovery()?;
        let name = name.as_ref().trim();
        let url = url.as_ref().trim();

        match self.repo.find_remote(name) {
            Ok(_) => self.repo.remote_set_url(name, url)?,
            Err(_) => {
                self.repo.remote(name, url)?;
            }
        }

        Ok(RemoteEndpoint {
            name: name.to_string(),
            url: redact_remote_url(url),
        })
    }

    /// Lists configured remote endpoints.
    pub fn remotes(&self) -> Result<Vec<RemoteEndpoint>> {
        self.ensure_no_pending_recovery()?;
        self.remotes_unchecked()
    }

    fn remotes_unchecked(&self) -> Result<Vec<RemoteEndpoint>> {
        let names = self.repo.remotes()?;
        let mut remotes = Vec::new();

        for name in names.iter().filter_map(|name| name.ok().flatten()) {
            let remote = self.repo.find_remote(name)?;
            remotes.push(RemoteEndpoint {
                name: name.to_string(),
                url: redact_remote_url(remote.url().unwrap_or_default()),
            });
        }

        remotes.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(remotes)
    }

    fn pending_recovery(&self, operation_id: &str) -> Result<RecoveryState> {
        self.ensure_recovery_is_ours()?;
        let Some(state) = self.recovery_state()? else {
            return Err(DraftlineError::RecoveryNotFound(operation_id.to_string()));
        };

        if state.operation_id != operation_id {
            return Err(DraftlineError::RecoveryNotFound(operation_id.to_string()));
        }

        Ok(state)
    }

    fn complete_recovery(
        &self,
        state: RecoveryState,
        changed_workspace: bool,
    ) -> Result<RecoveryRepairResult> {
        // A checkout killed after writing a filtered file's pointer leaves
        // nothing for the finishing checkout to update, so smudge it here.
        let changed_workspace = match self.smudge_left_pointers() {
            Ok(smudged) => changed_workspace || smudged,
            Err(error) => {
                tracing::warn!(%error, "could not check for Git filter pointers");
                changed_workspace
            }
        };
        let completed_state = RecoveryState {
            operation_id: state.operation_id.clone(),
            operation: state.operation.clone(),
            original_variation: None,
            target: state.target,
            completed: true,
        };
        self.write_recovery_state(&completed_state)?;
        if matches!(
            &completed_state.operation,
            RecoveryOperation::DeleteRemoteVariation
        ) {
            match fs::remove_file(
                self.remote_delete_recovery_metadata_path(&completed_state.operation_id),
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        Ok(RecoveryRepairResult {
            operation_id: state.operation_id,
            operation: state.operation,
            completed: true,
            changed_workspace,
            safe_next_actions: vec![SafeNextAction::NormalWork],
        })
    }

    fn recovery_unavailable(&self, state: RecoveryState) -> RecoveryRepairResult {
        RecoveryRepairResult {
            operation_id: state.operation_id,
            operation: state.operation,
            completed: false,
            changed_workspace: false,
            safe_next_actions: vec![SafeNextAction::RepairRecovery],
        }
    }

    fn repair_delete_variation_recovery(&self, state: &RecoveryState) -> Result<()> {
        let (Some(variation), Some(target)) =
            (state.original_variation.as_deref(), state.target.as_deref())
        else {
            return Ok(());
        };
        let safe_variation = validate_variation_name(variation)?;
        let oid = Oid::from_str(target)
            .map_err(|_| DraftlineError::VersionNotFound(target.to_string()))?;
        let archive_ref = archive_ref("deleted-variations", &safe_variation, &state.operation_id);
        if self.repo.find_reference(&archive_ref).is_err() {
            self.repo
                .reference(&archive_ref, oid, false, "repair deleted variation archive")?;
        }
        if let Ok(mut branch) = self.repo.find_branch(&safe_variation, BranchType::Local) {
            if branch.get().target() == Some(oid) {
                branch.delete()?;
            }
        }
        Ok(())
    }

    fn repair_rename_variation_recovery(&self, state: &RecoveryState) -> Result<Option<bool>> {
        let Some((source, target)) = self.rename_recovery_names(state)? else {
            return Ok(None);
        };
        let Some(expected_oid) = self.rename_recovery_oid(&source, state)? else {
            return Ok(None);
        };

        let source_oid = self.local_variation_oid(&source)?;
        let target_oid = self.local_variation_oid(&target)?;
        if target_oid == Some(expected_oid) && source_oid.is_none() {
            if self.head_symbolic_variation().as_deref() == Some(&source) {
                self.repo.set_head(&format!("refs/heads/{target}"))?;
            }
            let changed_metadata =
                self.complete_renamed_variation_metadata_if_needed(&source, &target)?;
            return Ok(Some(changed_metadata));
        }

        if source_oid == Some(expected_oid) && target_oid.is_none() {
            self.rename_variation_ref_and_metadata(&source, &target)?;
            if self.head_symbolic_variation().as_deref() == Some(&source) {
                self.repo.set_head(&format!("refs/heads/{target}"))?;
            }
            return Ok(Some(true));
        }

        if source_oid == Some(expected_oid) && target_oid == Some(expected_oid) {
            if self.head_symbolic_variation().as_deref() == Some(&source) {
                self.repo.set_head(&format!("refs/heads/{target}"))?;
            }
            self.complete_renamed_variation_metadata_if_needed(&source, &target)?;
            let mut source_branch = self.find_local_variation_branch(&source)?;
            source_branch.delete()?;
            return Ok(Some(true));
        }

        if source_oid.is_none() && target_oid.is_none() {
            self.restore_variation_ref(&target, expected_oid)?;
            if self.head_symbolic_variation().as_deref() == Some(&source) {
                self.repo.set_head(&format!("refs/heads/{target}"))?;
            }
            return Ok(Some(true));
        }

        Ok(None)
    }

    fn rollback_rename_variation_recovery(&self, state: &RecoveryState) -> Result<Option<bool>> {
        let Some((source, target)) = self.rename_recovery_names(state)? else {
            return Ok(None);
        };
        let Some(expected_oid) = self.rename_recovery_oid(&source, state)? else {
            return Ok(None);
        };

        let source_oid = self.local_variation_oid(&source)?;
        let target_oid = self.local_variation_oid(&target)?;
        if source_oid == Some(expected_oid) && target_oid.is_none() {
            if self.head_symbolic_variation().as_deref() == Some(&target) {
                self.repo.set_head(&format!("refs/heads/{source}"))?;
            }
            return Ok(Some(false));
        }

        if target_oid == Some(expected_oid) && source_oid.is_none() {
            let target_was_current = self.head_symbolic_variation().as_deref() == Some(&target);
            let metadata = self.rollback_renamed_variation_metadata(&source, &target)?;
            self.rename_variation_ref(&target, &source)?;
            self.write_variation_metadata(&source, &metadata)?;
            self.clear_variation_metadata(&target)?;
            if target_was_current {
                self.repo.set_head(&format!("refs/heads/{source}"))?;
            }
            return Ok(Some(true));
        }

        if source_oid == Some(expected_oid) && target_oid == Some(expected_oid) {
            if self.head_symbolic_variation().as_deref() == Some(&target) {
                self.repo.set_head(&format!("refs/heads/{source}"))?;
            }
            let metadata = self.rollback_renamed_variation_metadata(&source, &target)?;
            let mut target_branch = self.find_local_variation_branch(&target)?;
            target_branch.delete()?;
            self.write_variation_metadata(&source, &metadata)?;
            self.clear_variation_metadata(&target)?;
            return Ok(Some(true));
        }

        if source_oid.is_none() && target_oid.is_none() {
            self.restore_variation_ref(&source, expected_oid)?;
            if self.head_symbolic_variation().as_deref() == Some(&target) {
                self.repo.set_head(&format!("refs/heads/{source}"))?;
            }
            return Ok(Some(true));
        }

        Ok(None)
    }

    fn rename_recovery_names(&self, state: &RecoveryState) -> Result<Option<(String, String)>> {
        let (Some(source), Some(target)) =
            (state.original_variation.as_deref(), state.target.as_deref())
        else {
            return Ok(None);
        };
        let (source, target) = (
            validate_variation_name(source)?,
            validate_variation_name(target)?,
        );
        self.ensure_variation_in_namespace(&source)?;
        self.ensure_variation_in_namespace(&target)?;
        Ok(Some((source, target)))
    }

    fn rename_recovery_oid(&self, source: &str, state: &RecoveryState) -> Result<Option<Oid>> {
        let archive_ref = archive_ref("deleted-variations", source, &state.operation_id);
        match self.repo.refname_to_id(&archive_ref) {
            Ok(oid) => Ok(Some(oid)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                if let Some(oid) = self.local_variation_oid(source)? {
                    self.ensure_archive_ref(&archive_ref, oid, "repair renamed variation archive")?;
                    Ok(Some(oid))
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn restore_variation_ref(&self, variation: &str, oid: Oid) -> Result<()> {
        let commit = self.repo.find_commit(oid)?;
        self.repo.branch(variation, &commit, false)?;
        Ok(())
    }

    fn rename_variation_ref(&self, source: &str, target: &str) -> Result<()> {
        let mut branch = self.find_local_variation_branch(source)?;
        branch.rename(target, false)?;
        Ok(())
    }

    fn rename_variation_ref_and_metadata(
        &self,
        source: &str,
        target: &str,
    ) -> Result<VariationMetadata> {
        let metadata = self.read_variation_metadata(source)?;
        self.rename_variation_ref(source, target)?;
        self.write_variation_metadata(target, &metadata)?;
        self.clear_variation_metadata(source)?;
        Ok(metadata)
    }

    fn local_variation_oid(&self, variation: &str) -> Result<Option<Oid>> {
        match self.repo.find_branch(variation, BranchType::Local) {
            Ok(branch) => Ok(Some(branch.get().peel_to_commit()?.id())),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn find_local_variation_branch(&self, variation: &str) -> Result<git2::Branch<'_>> {
        self.repo
            .find_branch(variation, BranchType::Local)
            .map_err(|error| match error.code() {
                git2::ErrorCode::NotFound => {
                    DraftlineError::VariationNotFound(variation.to_string())
                }
                _ => DraftlineError::from(error),
            })
    }

    fn ensure_archive_ref(&self, ref_name: &str, oid: Oid, log: &str) -> Result<()> {
        match self.repo.refname_to_id(ref_name) {
            Ok(existing) if existing == oid => Ok(()),
            Ok(existing) => Err(DraftlineError::LocalStateChanged {
                expected: format!("{ref_name}@{oid}"),
                actual: format!("{ref_name}@{existing}"),
            }),
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                self.repo.reference(ref_name, oid, false, log)?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn repair_delete_remote_variation_recovery(
        &self,
        state: RecoveryState,
        options: &mut RemoteOptions<'_>,
    ) -> Result<RecoveryRepairResult> {
        let Some(metadata) = self.read_remote_delete_recovery_metadata(&state.operation_id)? else {
            return Ok(self.recovery_unavailable(state));
        };
        self.ensure_variation_in_namespace(&metadata.variation)?;
        let expected_remote_oid = Oid::from_str(&metadata.expected_remote_oid)
            .map_err(|_| DraftlineError::VersionNotFound(metadata.expected_remote_oid.clone()))?;
        if state.target.as_deref() != Some(metadata.expected_remote_oid.as_str())
            || state.original_variation.as_deref() != Some(metadata.variation.as_str())
        {
            return Ok(self.recovery_unavailable(state));
        }

        match self.repo.refname_to_id(&metadata.support_ref) {
            Ok(oid) if oid == expected_remote_oid => {}
            Ok(_) => return Ok(self.recovery_unavailable(state)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                self.repo.reference(
                    &metadata.support_ref,
                    expected_remote_oid,
                    false,
                    "repair remote variation delete archive",
                )?;
            }
            Err(error) => return Err(error.into()),
        }

        let advertised_support_oid =
            self.remote_advertised_oid(&metadata.remote, &metadata.support_ref, options)?;
        let advertised_variation_oid = self.remote_advertised_oid(
            &metadata.remote,
            &format!("refs/heads/{}", metadata.variation),
            options,
        )?;

        if advertised_support_oid.as_deref() != Some(metadata.expected_remote_oid.as_str()) {
            return Ok(self.recovery_unavailable(state));
        }

        match advertised_variation_oid.as_deref() {
            None => self.complete_recovery(state, false),
            Some(actual_oid) if actual_oid == metadata.expected_remote_oid => {
                self.push_refspec(
                    &metadata.remote,
                    &format!(":refs/heads/{}", metadata.variation),
                    vec![PushRefExpectation {
                        dst_refname: format!("refs/heads/{}", metadata.variation),
                        expected_old_oid: Some(metadata.expected_remote_oid),
                        expected_new_oid: None,
                    }],
                    options,
                )?;
                self.complete_recovery(state, false)
            }
            Some(_) => Ok(self.recovery_unavailable(state)),
        }
    }

    fn push_current_variation_with_options(
        &self,
        remote_name: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        let old_oid = remote_tracking_oid(&self.repo, remote_name, &variation);
        let new_oid = self
            .repo
            .refname_to_id(&format!("refs/heads/{variation}"))?
            .to_string();
        self.push_current_variation_with_lease(remote_name, old_oid, new_oid, options)
    }

    fn push_current_variation_with_lease(
        &self,
        remote_name: &str,
        expected_old_oid: Option<String>,
        expected_new_oid: String,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        self.push_variation_with_lease(
            remote_name,
            &variation,
            expected_old_oid,
            expected_new_oid,
            options,
        )
    }

    fn push_variation_with_lease(
        &self,
        remote_name: &str,
        variation: &str,
        expected_old_oid: Option<String>,
        expected_new_oid: String,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let refspec = format!("refs/heads/{variation}:refs/heads/{variation}");
        self.upload_filtered_content(remote_name, &format!("refs/heads/{variation}"))?;
        let _timeout_guard = options.server_timeout_guard()?;
        let push_result = {
            let mut push_options = options.push_options_with_expectations(
                "push_current_variation",
                vec![PushRefExpectation {
                    dst_refname: format!("refs/heads/{variation}"),
                    expected_old_oid,
                    expected_new_oid: Some(expected_new_oid),
                }],
            );
            remote.push(&[refspec.as_str()], Some(&mut push_options))
        };
        options.map_git_result("push_current_variation", push_result)?;

        Ok(())
    }

    fn push_refspec(
        &self,
        remote_name: &str,
        refspec: &str,
        expectations: Vec<PushRefExpectation>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let source = refspec.trim_start_matches('+');
        let source = source.split_once(':').map_or(source, |(source, _)| source);
        if !source.is_empty() {
            self.upload_filtered_content(remote_name, source)?;
        }
        let _timeout_guard = options.server_timeout_guard()?;
        let push_result = {
            let mut push_options =
                options.push_options_with_expectations("push_refspec", expectations);
            remote.push(&[refspec], Some(&mut push_options))
        };
        options.map_git_result("push_refspec", push_result)?;

        Ok(())
    }

    /// Before a push of `source`, uploads the Git LFS content its commits
    /// point at that `remote` lacks, as Git's pre-push hook does; otherwise
    /// the remote would get pointers to content no one can download. Runs
    /// only where Draftline runs the LFS filter and this repository has LFS
    /// content; a failed upload refuses the push.
    fn upload_filtered_content(&self, remote: &str, source: &str) -> Result<()> {
        if !FilterRunner::of(&self.repo)?.runs("lfs") {
            return Ok(());
        }
        let commit = self.repo.revparse_single(source)?.peel_to_commit()?.id();
        git_cli::lfs_push(&self.root, remote, commit)
    }

    fn remote_advertised_oid(
        &self,
        remote_name: &str,
        ref_name: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<Option<String>> {
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let _timeout_guard = options.server_timeout_guard()?;
        let oid = if options.has_network_callbacks() {
            let callbacks = options.remote_callbacks("remote_advertised_oid");
            let connection = remote.connect_auth(Direction::Fetch, Some(callbacks), None)?;
            connection
                .list()?
                .iter()
                .find(|head| head.name() == ref_name)
                .map(|head| head.oid().to_string())
        } else {
            remote.connect(Direction::Fetch)?;
            remote
                .list()?
                .iter()
                .find(|head| head.name() == ref_name)
                .map(|head| head.oid().to_string())
        };
        Ok(oid)
    }

    fn fetch_remote_variation_ref(
        &self,
        remote_name: &str,
        variation: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let refspec = format!("+refs/heads/{variation}:refs/remotes/{remote_name}/{variation}");
        let _timeout_guard = options.server_timeout_guard()?;
        let fetch_result = if options.has_network_callbacks() {
            let mut fetch_options = options.fetch_options("fetch_remote_variation_ref");
            remote.fetch(&[refspec.as_str()], Some(&mut fetch_options), None)
        } else {
            remote.fetch(&[refspec.as_str()], None, None)
        };

        if let Err(error) = fetch_result {
            if error.code() != git2::ErrorCode::NotFound {
                return options.map_git_result("fetch_remote_variation_ref", Err(error));
            }
        }

        Ok(())
    }

    fn shelf_by_id(&self, id: &str) -> Result<Shelf> {
        let (id, commit) = self.find_shelf_commit(id)?;
        Ok(Shelf {
            id,
            version: version_from_commit(&commit),
        })
    }

    fn find_shelf_commit(&self, id: &str) -> Result<(String, Commit<'_>)> {
        let safe_name = validate_variation_name(id)?;
        let reference = self
            .repo
            .find_reference(&format!("refs/draftline/shelves/{safe_name}"))?;
        let Some(oid) = reference.target() else {
            return Err(DraftlineError::VersionNotFound(safe_name));
        };
        let commit = self.repo.find_commit(oid)?;
        Ok((safe_name, commit))
    }

    fn list_local_support_refs(&self) -> Result<Vec<SupportRef>> {
        let mut support_refs = Vec::new();
        self.collect_support_refs(
            "refs/draftline/deleted-variations/*/*",
            SupportRefKind::DeletedVariation,
            "refs/draftline/deleted-variations/",
            SupportRefScope::Local,
            &mut support_refs,
        )?;
        self.collect_support_refs(
            "refs/draftline/rewrites/squash/*/*",
            SupportRefKind::Rewrite,
            "refs/draftline/rewrites/squash/",
            SupportRefScope::Local,
            &mut support_refs,
        )?;
        self.collect_support_refs(
            "refs/draftline/backups/history-cleanup/*/*",
            SupportRefKind::HistoryCleanupBackup,
            "refs/draftline/backups/history-cleanup/",
            SupportRefScope::Local,
            &mut support_refs,
        )?;

        support_refs.sort_by(|left, right| left.ref_name.cmp(&right.ref_name));
        Ok(support_refs)
    }

    fn list_remote_tracking_support_refs(&self) -> Result<Vec<SupportRef>> {
        let mut support_refs = Vec::new();
        self.collect_support_refs(
            "refs/remotes/*/draftline/deleted-variations/*/*",
            SupportRefKind::DeletedVariation,
            "draftline/deleted-variations/",
            SupportRefScope::RemoteTracking,
            &mut support_refs,
        )?;
        self.collect_support_refs(
            "refs/remotes/*/draftline/rewrites/squash/*/*",
            SupportRefKind::Rewrite,
            "draftline/rewrites/squash/",
            SupportRefScope::RemoteTracking,
            &mut support_refs,
        )?;
        self.collect_support_refs(
            "refs/remotes/*/draftline/backups/history-cleanup/*/*",
            SupportRefKind::HistoryCleanupBackup,
            "draftline/backups/history-cleanup/",
            SupportRefScope::RemoteTracking,
            &mut support_refs,
        )?;

        support_refs.sort_by(|left, right| left.ref_name.cmp(&right.ref_name));
        Ok(support_refs)
    }

    fn collect_support_refs(
        &self,
        glob: &str,
        kind: SupportRefKind,
        prefix: &str,
        scope: SupportRefScope,
        support_refs: &mut Vec<SupportRef>,
    ) -> Result<()> {
        for reference in self.repo.references_glob(glob)? {
            let reference = reference?;
            let Ok(ref_name) = reference.name() else {
                continue;
            };
            let Some(target_oid) = reference.target() else {
                continue;
            };

            let source_variation = source_variation_from_support_ref(ref_name, prefix);
            support_refs.push(SupportRef {
                id: ref_name.to_string(),
                ref_name: ref_name.to_string(),
                kind: kind.clone(),
                source_variation,
                target_oid: target_oid.to_string(),
                scope: scope.clone(),
            });
        }

        Ok(())
    }

    fn purge_ref_candidates(&self) -> Result<Vec<String>> {
        let mut refs = Vec::new();
        for glob in [
            "refs/heads/*",
            "refs/draftline/deleted-variations/*/*",
            "refs/draftline/rewrites/squash/*/*",
            "refs/draftline/backups/history-cleanup/*/*",
            "refs/tags/*",
            "refs/notes/*",
            "refs/replace/*",
        ] {
            for reference in self.repo.references_glob(glob)? {
                let reference = reference?;
                if reference.kind() != Some(git2::ReferenceType::Direct) {
                    continue;
                }
                if let Ok(name) = reference.name() {
                    refs.push(name.to_string());
                }
            }
        }

        refs.sort();
        refs.dedup();
        Ok(refs)
    }

    /// Fetches remote version metadata without changing local content.
    pub fn fetch_remote(&self, remote: impl AsRef<str>) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let mut options = RemoteOptions::new();
        let remote = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote, &mut options)?;
        self.fetch_base_unchecked(&remote, &mut options)?;
        self.fetch_support_refs_with_options(&remote, &mut options)
    }

    /// Fetches remote version metadata with explicit remote options.
    pub fn fetch_remote_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote, options)?;
        self.fetch_base_unchecked(&remote, options)?;
        self.fetch_support_refs_with_options(&remote, options)
    }

    /// Fetches the base branch too, so [`Self::base_status`] is current.
    fn fetch_base_unchecked(&self, remote: &str, options: &mut RemoteOptions<'_>) -> Result<()> {
        match self.default_branch_on(remote) {
            Some(base) if base != self.current_variation_unchecked()? => {
                self.fetch_remote_variation_ref(remote, &base, options)
            }
            _ => Ok(()),
        }
    }

    fn fetch_remote_unchecked(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        let mut remote = self.repo.find_remote(remote.as_ref())?;
        ensure_remote_transport_supported(&remote)?;
        let _timeout_guard = options.server_timeout_guard()?;
        let fetch_result = if options.has_network_callbacks() {
            let mut fetch_options = options.fetch_options("fetch_remote");
            remote.fetch(&[variation.as_str()], Some(&mut fetch_options), None)
        } else {
            remote.fetch(&[variation.as_str()], None, None)
        };
        if let Err(error) = fetch_result {
            if error.code() != git2::ErrorCode::NotFound {
                return options.map_git_result("fetch_remote", Err(error));
            }
        }
        // Like `git fetch --prune` for this one branch: a tracking ref for a
        // branch the remote no longer has would make it look up to date.
        if let (Some(name), Ok(advertised)) = (remote.name().ok().flatten(), remote.list()) {
            let merge = format!("refs/heads/{variation}");
            if !advertised.iter().any(|head| head.name() == merge) {
                if let Ok(mut tracking) = self
                    .repo
                    .find_reference(&format!("refs/remotes/{name}/{variation}"))
                {
                    tracking.delete()?;
                }
            }
        }
        self.record_remote_head(&remote)
    }

    /// Refuses to publish a branch whose remote counterpart was renamed as
    /// the remote's default: pushing would bring the old name back.
    fn ensure_base_not_renamed(&self, remote: &str) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        if remote_tracking_oid(&self.repo, remote, &variation).is_some() {
            return Ok(());
        }
        match self.default_branch_on(remote) {
            Some(base) if self.base_renamed_from(remote, &base).as_deref() == Some(&variation) => {
                Err(DraftlineError::BaseRenamed {
                    from: variation,
                    to: base,
                })
            }
            _ => Ok(()),
        }
    }

    /// Returns collaboration status for the current variation.
    pub fn sync_status(&self, remote: impl AsRef<str>) -> Result<SyncStatus> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        let variation = self.current_variation_unchecked()?;
        let local = self.repo.head()?.peel_to_commit()?.id();
        let remote_ref = format!("refs/remotes/{remote}/{variation}");

        let Ok(remote_oid) = self.repo.refname_to_id(&remote_ref) else {
            let ahead = self.local_version_count(local)?;
            let base_renamed_from = self
                .default_branch_on(&remote)
                .and_then(|base| self.base_renamed_from(&remote, &base))
                .filter(|old| *old == variation);
            return Ok(SyncStatus {
                remote,
                variation,
                ahead,
                behind: 0,
                state: SyncState::NoRemoteVersion,
                incoming: Vec::new(),
                base_renamed_from,
            });
        };

        let (ahead, behind) = self.repo.graph_ahead_behind(local, remote_oid)?;
        let state = match (ahead, behind) {
            (0, 0) => SyncState::UpToDate,
            (_, 0) => SyncState::LocalAhead,
            (0, _) => SyncState::IncomingAvailable,
            _ => SyncState::NeedsMerge,
        };

        Ok(SyncStatus {
            remote,
            variation,
            ahead,
            behind,
            state,
            incoming: self.incoming_versions(local, remote_oid)?,
            base_renamed_from: None,
        })
    }

    /// Shares saved work the way the Copilot app does: push the current branch
    /// (never forced) and open a pull request, or reuse the one already open.
    ///
    /// On the base branch, or on a branch outside the variation namespace,
    /// Send first moves the work onto a new branch named from the namespace
    /// and title, so it only ever pushes branches it owns. The base itself is
    /// never moved or rewound. Teammates' commits on the branch go along:
    /// [`Self::preflight_send`] lists them so the host can say so. Retrying
    /// after a failure is safe: the push becomes a no-op and the open pull
    /// request is found.
    pub fn send(
        &self,
        remote: impl AsRef<str>,
        request: &SendRequest,
        provider: &mut dyn PullRequestProvider,
        options: &mut RemoteOptions<'_>,
    ) -> Result<SendResult> {
        // Held through the push; the pull request is the host's, outside it.
        let lock = self.acquire_operation_lock("send")?;
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref();
        let target = self.resolve_send_target(remote, request, options)?;
        let SendTarget {
            base,
            advertised,
            current,
            ..
        } = target;
        // Every later step works on this exact commit, so a teammate's commit
        // landing mid-send never goes out unapproved.
        let scanned = self.repo.head().ok().and_then(|head| head.target());
        let creates_variation = self.send_creates_variation(&current, &base);
        // Unsaved edits would silently stay behind; the host saves first.
        let report = preflight_report(
            "send",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }
        let planned = if creates_variation {
            self.new_send_branch_name(request, &advertised)?
        } else {
            current.clone()
        };
        if let Some(expected) = &request.expected {
            let actual = SendToken {
                remote: remote.to_string(),
                base: base.clone(),
                branch: current.clone(),
                variation: planned.clone(),
                head: scanned.map(|oid| oid.to_string()),
            };
            if *expected != actual {
                return Err(DraftlineError::LocalStateChanged {
                    expected: Self::send_token_label(expected),
                    actual: Self::send_token_label(&actual),
                });
            }
        }
        let ensure_unmoved = || -> Result<git2::Oid> {
            let actual = self.repo.head().ok().and_then(|head| head.target());
            match (scanned, actual) {
                (Some(expected), Some(actual)) if expected == actual => Ok(expected),
                _ => Err(DraftlineError::LocalStateChanged {
                    expected: scanned.map(|oid| oid.to_string()).unwrap_or_default(),
                    actual: actual.map(|oid| oid.to_string()).unwrap_or_default(),
                }),
            }
        };

        let mut created_variation = false;
        let variation = if creates_variation {
            let name = planned;
            let labeled = !request.title.trim().is_empty();
            if labeled {
                self.write_variation_metadata(
                    &name,
                    &VariationMetadata::default().with_label(request.title.trim()),
                )?;
            }
            // Creating the branch and moving HEAD together means a crash
            // can't strand a branch that the retry would then avoid.
            if let Err(error) = self.branch_here_and_switch(&name, Some(ensure_unmoved()?)) {
                if labeled {
                    self.clear_unowned_variation_metadata(&name);
                }
                return Err(error);
            }
            created_variation = true;
            name
        } else {
            current
        };

        let base_ahead = match (
            advertised.contains(&variation),
            self.repo
                .refname_to_id(&format!("refs/remotes/{remote}/{base}")),
        ) {
            (false, Ok(base_oid)) => {
                let head = self.repo.head()?.peel_to_commit()?.id();
                Some(self.repo.graph_ahead_behind(head, base_oid)?.0)
            }
            _ => None,
        };
        let published = self.publish_changes_expecting(
            remote,
            options,
            Some(SendGuard {
                head: ensure_unmoved()?,
                variation: &variation,
            }),
        )?;
        let published_versions = base_ahead.unwrap_or(published.published_versions);
        if self
            .repo
            .find_reference(&format!("refs/remotes/{remote}/{variation}"))
            .is_ok()
        {
            self.repo
                .find_branch(&variation, BranchType::Local)?
                .set_upstream(Some(&format!("{remote}/{variation}")))?;
        }
        drop(lock);

        let pull_request_request = PullRequestRequest {
            remote: remote.to_string(),
            head: variation.clone(),
            base: base.clone(),
            title: request.title.clone(),
            body: request.body.clone(),
        };
        let (pull_request, created_pull_request) =
            match provider.find_open(&pull_request_request)? {
                Some(open) => (open, false),
                None => (provider.create(&pull_request_request)?, true),
            };

        Ok(SendResult {
            remote: remote.to_string(),
            variation,
            base,
            created_variation,
            published_versions,
            pull_request,
            created_pull_request,
        })
    }

    /// Checks what [`Self::send`] would do without pushing, creating a branch,
    /// or moving the base. It refreshes the base's remote-tracking ref. Lists
    /// teammates' commits that change files outside the project, which Send
    /// carries too, so the host can tell the user before they confirm.
    pub fn preflight_send(
        &self,
        remote: impl AsRef<str>,
        request: &SendRequest,
        options: &mut RemoteOptions<'_>,
    ) -> Result<SendPreflight> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref();
        let SendTarget {
            base,
            advertised,
            advertised_oids,
            current,
        } = self.resolve_send_target(remote, request, options)?;
        let creates_variation = self.send_creates_variation(&current, &base);
        let variation = if creates_variation {
            self.new_send_branch_name(request, &advertised)?
        } else {
            current.clone()
        };
        let head = self.repo.head().ok().and_then(|head| head.target());
        let developer_commits = self.developer_commits_to_send(remote, &base, head)?;
        let has_unsaved_changes = !self.changed_files_unchecked()?.is_empty();
        let outgoing_versions = match self
            .repo
            .refname_to_id(&format!("refs/remotes/{remote}/{base}"))
        {
            Ok(base_oid) => match head {
                Some(head) => self.repo.graph_ahead_behind(head, base_oid)?.0,
                None => 0,
            },
            Err(_) => 0,
        };
        let (outgoing_files, outgoing_files_truncated) =
            self.outgoing_files(head, &advertised_oids)?;
        let can_proceed = !has_unsaved_changes;
        let token = SendToken {
            remote: remote.to_string(),
            base: base.clone(),
            branch: current,
            variation: variation.clone(),
            head: head.map(|oid| oid.to_string()),
        };
        Ok(SendPreflight {
            remote: remote.to_string(),
            base,
            variation,
            token,
            creates_variation,
            outgoing_versions,
            has_unsaved_changes,
            developer_commits,
            outgoing_files,
            outgoing_files_truncated,
            can_proceed,
        })
    }

    fn send_token_label(token: &SendToken) -> String {
        format!(
            "{}/{} from {} as {}@{}",
            token.remote,
            token.base,
            token.branch,
            token.variation,
            token.head.as_deref().unwrap_or("none")
        )
    }

    /// Resolves Send's base and refreshes the base's and current variation's
    /// remote-tracking refs.
    fn resolve_send_target(
        &self,
        remote: &str,
        request: &SendRequest,
        options: &mut RemoteOptions<'_>,
    ) -> Result<SendTarget> {
        let explicit = match &request.base {
            Some(base) => Some(validate_variation_name(base)?),
            None => None,
        };
        let advertised_heads = self.remote_advertised_heads(remote, options)?;
        let advertised_oids = advertised_heads.values().copied().collect::<Vec<_>>();
        let advertised = advertised_heads.into_keys().collect::<HashSet<_>>();
        let base = match explicit {
            Some(base) => base,
            None => match self.default_branch_on(remote) {
                Some(base) if advertised.contains(&base) => base,
                inferred => self
                    .default_branch_candidates()
                    .into_iter()
                    .find(|name| advertised.contains(name))
                    .or(inferred)
                    .ok_or(DraftlineError::SendBaseUnknown)?,
            },
        };
        if !advertised.contains(&base) {
            return Err(DraftlineError::SendBaseNotOnRemote(base));
        }
        let current = self.current_variation_unchecked()?;
        self.fetch_remote_variation_ref(remote, &base, options)?;
        if current != base && advertised.contains(&current) {
            self.fetch_remote_variation_ref(remote, &current, options)?;
        }
        Ok(SendTarget {
            base,
            advertised,
            advertised_oids,
            current,
        })
    }

    /// Send pushes only branches Draftline owns: from the base, or from a
    /// developer's branch outside the namespace, it starts a new one.
    fn send_creates_variation(&self, current: &str, base: &str) -> bool {
        current == base
            || self
                .variation_namespace
                .as_deref()
                .is_some_and(|namespace| !current.starts_with(namespace))
    }

    /// The branch Send creates when run on the base or a developer's branch.
    fn new_send_branch_name(
        &self,
        request: &SendRequest,
        advertised: &HashSet<String>,
    ) -> Result<String> {
        let taken = |name: &str| {
            self.repo.find_branch(name, BranchType::Local).is_ok() || advertised.contains(name)
        };
        let name = match &request.branch {
            Some(name) => validate_variation_name(name)?,
            None => self.unused_send_branch_name(&request.title, taken)?,
        };
        if let Some(namespace) = &self.variation_namespace {
            if !name.starts_with(namespace.as_str()) {
                return Err(DraftlineError::SendOutsideNamespace(name));
            }
        }
        if taken(&name) {
            return Err(DraftlineError::VariationAlreadyExists(name));
        }
        Ok(name)
    }

    /// Large files and Git LFS pointers in commits reachable from `head`
    /// that the remote doesn't advertise: what a push would upload. Reports a
    /// path once, at its newest outgoing version. `true` when it stopped at
    /// 2000 commits or 100 files, so the list may be incomplete.
    fn outgoing_files(
        &self,
        head: Option<git2::Oid>,
        advertised: &[git2::Oid],
    ) -> Result<(Vec<OutgoingFile>, bool)> {
        const MAX_COMMITS: usize = 2000;
        const MAX_FILES: usize = 100;
        const MAX_POINTER: usize = 1024;
        let Some(head) = head else {
            return Ok((Vec::new(), false));
        };
        let threshold = self.content_policy.large_file_threshold_bytes();
        let odb = self.repo.odb()?;
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;
        walk.push(head)?;
        for oid in advertised {
            if self.repo.find_commit(*oid).is_ok() {
                walk.hide(*oid)?;
            }
        }
        let uploads_lfs = FilterRunner::of(&self.repo)?.runs("lfs");
        let mut seen = BTreeSet::new();
        let mut files = Vec::new();
        for (scanned, oid) in walk.enumerate() {
            if scanned == MAX_COMMITS {
                return Ok((files, true));
            }
            let commit = self.repo.find_commit(oid?)?;
            let parent = commit
                .parents()
                .next()
                .map(|parent| parent.tree())
                .transpose()?;
            let diff = self
                .repo
                .diff_tree_to_tree(parent.as_ref(), Some(&commit.tree()?), None)?;
            for delta in diff.deltas() {
                let new = delta.new_file();
                let (Some(path), true) = (new.path(), new.exists()) else {
                    continue;
                };
                if new.mode() == git2::FileMode::Commit || seen.contains(path) {
                    continue;
                }
                let (size, _) = odb.read_header(new.id())?;
                let size = size as u64;
                let kind = if size <= MAX_POINTER as u64
                    && is_lfs_pointer(self.repo.find_blob(new.id())?.content())
                {
                    OutgoingFileKind::LfsPointer
                } else if size >= threshold {
                    OutgoingFileKind::Large
                } else {
                    continue;
                };
                seen.insert(path.to_path_buf());
                files.push(OutgoingFile {
                    path: path.to_path_buf(),
                    commit: commit.id().to_string(),
                    size,
                    kind,
                    uploaded: kind == OutgoingFileKind::LfsPointer && uploads_lfs,
                });
                if files.len() == MAX_FILES {
                    return Ok((files, true));
                }
            }
        }
        Ok((files, false))
    }

    /// Commits Send would push that change files outside the folder.
    /// Outgoing means reachable from `head` but not from the remote base. Draftline saves only touch the
    /// folder. A merge is listed only for outside paths that match none of its
    /// parents (a hand-made resolution), so Draftline's merges of the base
    /// pass. Anything listed is a developer's work. Standalone projects own
    /// the whole repository.
    fn developer_commits_to_send(
        &self,
        remote: &str,
        base: &str,
        head: Option<git2::Oid>,
    ) -> Result<Vec<DeveloperCommit>> {
        const MAX_PATHS: usize = 20;
        let scope = self.scope();
        if scope.as_os_str().is_empty() {
            return Ok(Vec::new());
        }
        let Some(head) = head else {
            return Ok(Vec::new());
        };
        let mut walk = self.repo.revwalk()?;
        walk.push(head)?;
        // Only the base proves a commit isn't new to the pull request; the
        // variation's own remote branch may already hold published developer
        // commits.
        if let Ok(oid) = self
            .repo
            .refname_to_id(&format!("refs/remotes/{remote}/{base}"))
        {
            walk.hide(oid)?;
        }
        // Inside means inside the folder where the commit or its parent kept
        // it, so Draftline's own saves from before a move and the move itself
        // aren't developer work.
        let outside_changes = |from: Option<&git2::Tree<'_>>,
                               to: &git2::Tree<'_>,
                               folders: &[&Path]|
         -> Result<BTreeSet<PathBuf>> {
            if folders.iter().any(|folder| folder.as_os_str().is_empty()) {
                return Ok(BTreeSet::new());
            }
            let diff = self.repo.diff_tree_to_tree(from, Some(to), None)?;
            Ok(diff
                .deltas()
                .flat_map(|delta| [delta.old_file().path(), delta.new_file().path()])
                .flatten()
                .filter(|path| {
                    *path != Path::new(FOLDER_MARKER)
                        && !folders.iter().any(|folder| path.starts_with(folder))
                })
                .map(Path::to_path_buf)
                .collect())
        };
        let mut folders = FolderChanges::new(&self.repo, scope);
        let mut found = Vec::new();
        for oid in walk {
            let commit = self.repo.find_commit(oid?)?;
            let tree = commit.tree()?;
            let own = folders.folder_at(&commit)?;
            let mut outside: Option<BTreeSet<PathBuf>> = None;
            if commit.parent_count() == 0 {
                outside = Some(outside_changes(None, &tree, &[&own])?);
            }
            for parent in commit.parents() {
                let theirs = folders.folder_at(&parent)?;
                let changed = outside_changes(Some(&parent.tree()?), &tree, &[&own, &theirs])?;
                outside = Some(match outside {
                    None => changed,
                    Some(previous) => previous.intersection(&changed).cloned().collect(),
                });
            }
            let outside = outside.unwrap_or_default();
            if !outside.is_empty() {
                found.push(DeveloperCommit {
                    id: commit.id().to_string(),
                    summary: commit
                        .summary()
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                        .to_string(),
                    outside_paths: outside.into_iter().take(MAX_PATHS).collect(),
                });
            }
        }
        Ok(found)
    }

    /// Creates a separate checkout under `worktrees_dir` on a new branch named
    /// after `title` (inside the variation namespace), like the Copilot app's
    /// per-task worktrees. This checkout, its index, and its branch are left
    /// alone. Fetch first to start from the latest base, then open the
    /// returned `project_path` as usual.
    pub fn create_owned_worktree(
        &self,
        worktrees_dir: impl AsRef<Path>,
        title: impl AsRef<str>,
        remote: impl AsRef<str>,
        start: WorktreeStart,
    ) -> Result<OwnedWorktree> {
        let _lock = self.acquire_operation_lock("create_owned_worktree")?;
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref();
        let start_commit = match start {
            WorktreeStart::CurrentVersion => self.repo.head()?.peel_to_commit()?,
            WorktreeStart::LatestBase => {
                let base = self
                    .default_branch_on(remote)
                    .ok_or(DraftlineError::SendBaseUnknown)?;
                let oid = self
                    .repo
                    .refname_to_id(&format!("refs/remotes/{remote}/{base}"))
                    .or_else(|_| self.repo.refname_to_id(&format!("refs/heads/{base}")))?;
                self.repo.find_commit(oid)?
            }
        };
        let worktrees_dir = worktrees_dir.as_ref();
        let worktree_name =
            |branch: &str| format!("{OWNED_WORKTREE_PREFIX}{}", branch.replace('/', "-"));
        let variation = self.unused_send_branch_name(title.as_ref(), |name| {
            self.repo.find_branch(name, BranchType::Local).is_ok()
                || self
                    .repo
                    .find_reference(&format!("refs/remotes/{remote}/{name}"))
                    .is_ok()
                || self.repo.find_worktree(&worktree_name(name)).is_ok()
                || worktrees_dir.join(worktree_name(name)).exists()
        })?;
        let name = worktree_name(&variation);
        let path = worktrees_dir.join(&name);
        // The folder lives where the starting commit's marker says.
        let folder = tree_marker_folder(&self.repo, &start_commit.tree()?)
            .unwrap_or_else(|| self.scope().to_path_buf());
        fs::create_dir_all(worktrees_dir)?;

        let reference = self
            .repo
            .branch(&variation, &start_commit, false)?
            .into_reference();
        let mut options = git2::WorktreeAddOptions::new();
        options.reference(Some(&reference));
        // The folder may not exist yet where the branch starts.
        let added = self
            .repo
            .worktree(&name, &path, Some(&options))
            .map_err(DraftlineError::from)
            .and_then(|_| create_folder_within(&path, &folder));
        if let Err(error) = added {
            drop(reference);
            self.roll_back_owned_worktree(&name, &path, &variation)?;
            return Err(error);
        }
        // Report the path the way Git recorded it (symlinks and 8.3 names
        // resolved), so it matches what owned_worktrees lists.
        let path = self
            .repo
            .find_worktree(&name)
            .map(|worktree| worktree.path().components().collect::<PathBuf>())
            .unwrap_or(path);
        Ok(OwnedWorktree {
            name,
            project_path: path.join(folder),
            path,
            variation: Some(variation),
        })
    }

    /// Lists the checkouts [`Self::create_owned_worktree`] made in this
    /// repository. Other tools' worktrees are never included.
    pub fn owned_worktrees(&self) -> Result<Vec<OwnedWorktree>> {
        let mut owned = Vec::new();
        for name in self
            .repo
            .worktrees()?
            .iter()
            .filter_map(|name| name.ok().flatten())
        {
            if !name.starts_with(OWNED_WORKTREE_PREFIX) {
                continue;
            }
            let worktree = self.repo.find_worktree(name)?;
            let path = worktree.path().components().collect::<PathBuf>();
            let checkout = Repository::open_from_worktree(&worktree).ok();
            let variation = checkout.as_ref().and_then(|repo| {
                repo.head()
                    .ok()
                    .and_then(|head| head.shorthand().ok().map(str::to_string))
            });
            let folder = checkout
                .as_ref()
                .and_then(head_marker_folder)
                .unwrap_or_else(|| self.scope().to_path_buf());
            owned.push(OwnedWorktree {
                name: name.to_string(),
                project_path: path.join(folder),
                path,
                variation,
            });
        }
        Ok(owned)
    }

    /// Deletes an owned checkout's folder; its branch and versions stay, like
    /// `git worktree remove`. Refuses while it has unsaved or untracked files
    /// unless `discard_changes`, and refuses the checkout this workspace is in.
    pub fn remove_owned_worktree(
        &self,
        name: impl AsRef<str>,
        discard_changes: bool,
    ) -> Result<()> {
        let name = name.as_ref();
        if !name.starts_with(OWNED_WORKTREE_PREFIX) {
            return Err(DraftlineError::NotOwnedWorktree(name.to_string()));
        }
        let _lock = self.acquire_operation_lock("remove_owned_worktree")?;
        self.ensure_no_pending_recovery()?;
        let worktree = self.repo.find_worktree(name)?;
        let path = worktree.path().to_path_buf();
        let in_use = match (fs::canonicalize(&path), fs::canonicalize(&self.root)) {
            (Ok(path), Ok(root)) => path == root,
            _ => false,
        };
        if in_use {
            return Err(DraftlineError::OwnedWorktreeInUse(name.to_string()));
        }
        // Draftline running there, or an operation it must still finish,
        // keeps the checkout unless the caller discards it. Its lock is held
        // through the prune so nothing starts there meanwhile.
        let mut _target_lock = None;
        if let Ok(checkout) = Repository::open_from_worktree(&worktree) {
            let draftline = checkout.path().join("draftline");
            _target_lock = Some(OperationLock::acquire(
                &draftline.join("operation.lock"),
                "remove_owned_worktree",
            )?);
            let ledger = draftline.join("recovery.json");
            // A finished operation leaves its record behind marked completed;
            // only an unfinished (or unreadable) one keeps the checkout.
            let pending = ledger.exists()
                && fs::read(&ledger)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<RecoveryLedger>(&bytes).ok())
                    .is_none_or(|ledger| !ledger.state.completed);
            if !discard_changes && pending {
                return Err(DraftlineError::OwnedWorktreeHasChanges(vec![ledger]));
            }
        }
        if !discard_changes && path.exists() {
            // Damaged metadata means its changes can't be checked; keep it.
            if worktree.validate().is_err() {
                return Err(DraftlineError::OwnedWorktreeHasChanges(vec![path]));
            }
            let repo = Repository::open_from_worktree(&worktree)?;
            let mut options = StatusOptions::new();
            options.include_untracked(true).recurse_untracked_dirs(true);
            let statuses = repo.statuses(Some(&mut options))?;
            if !statuses.is_empty() {
                let changed = statuses
                    .iter()
                    .map(|entry| {
                        PathBuf::from(String::from_utf8_lossy(entry.path_bytes()).as_ref())
                    })
                    .collect();
                return Err(DraftlineError::OwnedWorktreeHasChanges(changed));
            }
        }
        let mut prune = git2::WorktreePruneOptions::new();
        prune.valid(true).working_tree(true);
        worktree.prune(Some(&mut prune))?;
        Ok(())
    }

    /// Undoes a half-made owned worktree: its folder, admin data, then the
    /// new branch. Errors if anything is left so it isn't silently orphaned.
    fn roll_back_owned_worktree(&self, name: &str, path: &Path, branch: &str) -> Result<()> {
        if let Ok(worktree) = self.repo.find_worktree(name) {
            let mut prune = git2::WorktreePruneOptions::new();
            prune.valid(true).working_tree(true).locked(true);
            worktree.prune(Some(&mut prune))?;
        }
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        let admin = self.repo.commondir().join("worktrees").join(name);
        if admin.exists() {
            fs::remove_dir_all(admin)?;
        }
        if let Ok(mut created) = self.repo.find_branch(branch, BranchType::Local) {
            created.delete()?;
        }
        Ok(())
    }

    fn unused_send_branch_name(&self, title: &str, taken: impl Fn(&str) -> bool) -> Result<String> {
        let mut slug = String::new();
        for character in title.trim().to_lowercase().chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character);
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
            if slug.len() >= 48 {
                break;
            }
        }
        let slug = slug.trim_end_matches('-');
        let stem = format!(
            "{}{}",
            self.variation_namespace.as_deref().unwrap_or_default(),
            if slug.is_empty() { "send" } else { slug }
        );
        let name = (1..)
            .map(|n| {
                if n == 1 {
                    stem.clone()
                } else {
                    format!("{stem}-{n}")
                }
            })
            .find(|name| !taken(name))
            .expect("an unused suffix exists");
        validate_variation_name(&name)
    }

    /// Branches the remote advertises right now, with their commits.
    fn remote_advertised_heads(
        &self,
        remote_name: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<HashMap<String, git2::Oid>> {
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let _timeout_guard = options.server_timeout_guard()?;
        let branches = |heads: &[git2::RemoteHead<'_>]| {
            heads
                .iter()
                .filter_map(|head| {
                    head.name()
                        .strip_prefix("refs/heads/")
                        .map(|name| (name.to_string(), head.oid()))
                })
                .collect::<HashMap<_, _>>()
        };
        if options.has_network_callbacks() {
            let callbacks = options.remote_callbacks("remote_advertised_heads");
            let mut connection = remote.connect_auth(Direction::Fetch, Some(callbacks), None)?;
            let advertised = branches(connection.list()?);
            self.record_remote_head(connection.remote())?;
            Ok(advertised)
        } else {
            remote.connect(Direction::Fetch)?;
            let advertised = branches(remote.list()?);
            self.record_remote_head(&remote)?;
            Ok(advertised)
        }
    }

    /// Publishes local versions for the current variation when doing so will not overwrite remote work.
    pub fn publish_changes(&self, remote: impl AsRef<str>) -> Result<PublishResult> {
        let mut options = RemoteOptions::new();
        self.publish_changes_with_options(remote, &mut options)
    }

    /// Preflights publishing and captures the expected remote OID or absence.
    pub fn preflight_publish(&self, remote: impl AsRef<str>) -> Result<PublishPreflight> {
        let mut options = RemoteOptions::new();
        self.preflight_publish_with_options(remote, &mut options)
    }

    /// Preflights publishing with explicit remote options.
    pub fn preflight_publish_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<PublishPreflight> {
        self.ensure_no_pending_recovery()?;
        let report = preflight_report(
            "publish_changes",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        self.ensure_variation_in_namespace(&self.current_variation_unchecked()?)?;
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let remote = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote, options)?;
        self.ensure_base_not_renamed(&remote)?;
        let sync_status = self.sync_status(&remote)?;
        if matches!(
            sync_status.state,
            SyncState::IncomingAvailable | SyncState::NeedsMerge
        ) {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(sync_status)));
        }

        let variation = self.current_variation_unchecked()?;
        let local_oid = self.repo.head()?.peel_to_commit()?.id().to_string();
        let expected_remote_oid = remote_tracking_oid(&self.repo, &remote, &variation);
        let advertised_oids = self
            .remote_advertised_heads(&remote, options)?
            .into_values()
            .collect::<Vec<_>>();
        let (outgoing_files, outgoing_files_truncated) =
            self.outgoing_files(self.repo.head()?.target(), &advertised_oids)?;
        let token = PublishToken {
            remote: remote.clone(),
            variation: variation.clone(),
            expected_remote_oid: expected_remote_oid.clone(),
            local_oid: local_oid.clone(),
        };

        Ok(PublishPreflight {
            remote,
            variation,
            expected_remote_oid,
            local_oid,
            sync_status,
            token,
            outgoing_files,
            outgoing_files_truncated,
            can_publish: true,
        })
    }

    /// Publishes using a token returned by [`Workspace::preflight_publish`].
    pub fn publish(&self, token: PublishToken) -> Result<PublishResult> {
        let mut options = RemoteOptions::new();
        self.publish_with_options(token, &mut options)
    }

    /// Publishes with explicit remote options using a preflight token.
    pub fn publish_with_options(
        &self,
        token: PublishToken,
        options: &mut RemoteOptions<'_>,
    ) -> Result<PublishResult> {
        self.ensure_no_pending_recovery()?;
        let report = preflight_report(
            "publish_changes",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        self.ensure_variation_in_namespace(&self.current_variation_unchecked()?)?;
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let variation = self.current_variation_unchecked()?;
        let local_oid = self.repo.head()?.peel_to_commit()?.id().to_string();
        if variation != token.variation || local_oid != token.local_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", token.variation, token.local_oid),
                actual: format!("{variation}@{local_oid}"),
            });
        }

        self.fetch_remote_unchecked(&token.remote, options)?;
        self.ensure_base_not_renamed(&token.remote)?;
        let actual_remote_oid = remote_tracking_oid(&self.repo, &token.remote, &token.variation);
        if actual_remote_oid != token.expected_remote_oid {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: token.variation,
                expected: token.expected_remote_oid,
                actual: actual_remote_oid,
            });
        }

        let status = self.sync_status(&token.remote)?;
        if matches!(
            status.state,
            SyncState::IncomingAvailable | SyncState::NeedsMerge
        ) {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }

        if status.ahead > 0 {
            self.push_current_variation_with_lease(
                &token.remote,
                token.expected_remote_oid.clone(),
                token.local_oid.clone(),
                options,
            )?;
        }

        Ok(PublishResult {
            remote: token.remote,
            variation,
            published_versions: status.ahead,
        })
    }

    /// Publishes local versions with explicit remote options.
    pub fn publish_changes_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<PublishResult> {
        let _lock = self.acquire_operation_lock("publish_changes")?;
        self.publish_changes_expecting(remote.as_ref(), options, None)
    }

    /// Publishes. Send passes a [`SendGuard`]: the branch and commit it checked
    /// must still be checked out after this fetch, and exactly that commit is
    /// pushed to that branch.
    fn publish_changes_expecting(
        &self,
        remote: &str,
        options: &mut RemoteOptions<'_>,
        send_guard: Option<SendGuard<'_>>,
    ) -> Result<PublishResult> {
        self.ensure_no_pending_recovery()?;
        let report = preflight_report(
            "publish_changes",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        self.ensure_variation_in_namespace(&self.current_variation_unchecked()?)?;
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let remote_name = remote.to_string();
        self.fetch_remote_unchecked(&remote_name, options)?;
        self.ensure_base_not_renamed(&remote_name)?;
        if let Some(guard) = &send_guard {
            let current = self.current_variation_unchecked()?;
            let actual = self
                .repo
                .refname_to_id(&format!("refs/heads/{current}"))
                .ok();
            if current != guard.variation || actual != Some(guard.head) {
                return Err(DraftlineError::LocalStateChanged {
                    expected: format!("{}@{}", guard.variation, guard.head),
                    actual: format!(
                        "{current}@{}",
                        actual.map(|oid| oid.to_string()).unwrap_or_default()
                    ),
                });
            }
        }
        let status = self.sync_status(&remote_name)?;
        if matches!(
            status.state,
            SyncState::IncomingAvailable | SyncState::NeedsMerge
        ) {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }

        let variation = match &send_guard {
            Some(guard) => guard.variation.to_string(),
            None => self.current_variation_unchecked()?,
        };
        if status.ahead > 0 {
            let push_result = match &send_guard {
                None => self.push_current_variation_with_options(&remote_name, options),
                Some(guard) => self.push_variation_with_lease(
                    &remote_name,
                    guard.variation,
                    remote_tracking_oid(&self.repo, &remote_name, guard.variation),
                    guard.head.to_string(),
                    options,
                ),
            };
            if let Err(error) = push_result {
                self.fetch_remote_unchecked(&remote_name, options)?;
                let refreshed = self.sync_status(&remote_name)?;
                if matches!(
                    refreshed.state,
                    SyncState::IncomingAvailable | SyncState::NeedsMerge
                ) {
                    return Err(DraftlineError::SyncNeedsMerge(Box::new(refreshed)));
                }

                return Err(error);
            }
        }

        Ok(PublishResult {
            remote: remote_name,
            variation,
            published_versions: status.ahead,
        })
    }

    fn read_variation_metadata(&self, variation: &str) -> Result<VariationMetadata> {
        let config = self.repo.config()?;

        Ok(VariationMetadata {
            label: read_optional_config(&config, &variation_metadata_key(variation, "label"))?,
            slug: read_optional_config(&config, &variation_metadata_key(variation, "slug"))?,
        })
    }

    fn write_variation_metadata(
        &self,
        variation: &str,
        metadata: &VariationMetadata,
    ) -> Result<()> {
        let mut config = self.repo.config()?;

        write_optional_config(
            &mut config,
            &variation_metadata_key(variation, "label"),
            metadata.label.as_deref(),
        )?;
        write_optional_config(
            &mut config,
            &variation_metadata_key(variation, "slug"),
            metadata.slug.as_deref(),
        )?;

        Ok(())
    }

    fn clear_variation_metadata(&self, variation: &str) -> Result<()> {
        self.write_variation_metadata(variation, &VariationMetadata::default())
    }

    fn complete_renamed_variation_metadata_if_needed(
        &self,
        source: &str,
        target: &str,
    ) -> Result<bool> {
        let source_metadata = self.read_variation_metadata(source)?;
        let target_metadata = self.read_variation_metadata(target)?;
        if source_metadata != VariationMetadata::default()
            && target_metadata == VariationMetadata::default()
        {
            self.write_variation_metadata(target, &source_metadata)?;
            self.clear_variation_metadata(source)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn rollback_renamed_variation_metadata(
        &self,
        source: &str,
        target: &str,
    ) -> Result<VariationMetadata> {
        let source_metadata = self.read_variation_metadata(source)?;
        let target_metadata = self.read_variation_metadata(target)?;
        if target_metadata != VariationMetadata::default() {
            Ok(target_metadata)
        } else {
            Ok(source_metadata)
        }
    }

    fn diff_unsaved_text(&self) -> Result<String> {
        let head_tree = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_tree().ok());

        let mut options = DiffOptions::new();
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))?;

        let mut text = String::new();
        diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
            if let Ok(content) = std::str::from_utf8(line.content()) {
                text.push_str(content);
            }
            true
        })?;

        Ok(text)
    }

    fn find_version_commit(&self, version: &VersionId) -> Result<git2::Commit<'_>> {
        let oid = Oid::from_str(version.as_str())
            .map_err(|_| DraftlineError::VersionNotFound(version.to_string()))?;
        self.repo
            .find_commit(oid)
            .map_err(|_| DraftlineError::VersionNotFound(version.to_string()))
    }

    fn draftline_dir(&self) -> PathBuf {
        self.repo.path().join("draftline")
    }

    fn ledger_path(&self) -> PathBuf {
        self.draftline_dir().join("recovery.json")
    }

    fn remote_delete_recovery_metadata_path(&self, operation_id: &str) -> PathBuf {
        self.draftline_dir()
            .join(format!("recovery-delete-remote-{operation_id}.json"))
    }

    /// Takes the Draftline lock for an operation that writes Git's index, and
    /// refuses up front while another Git process holds that index.
    fn acquire_for_index(&self, operation: &str) -> Result<OperationLock> {
        let lock = self.acquire_operation_lock(operation)?;
        self.while_git_busy(|| self.ensure_git_index_unlocked())?;
        Ok(lock)
    }

    /// Git holds `index.lock` only while it stages, commits, or checks out, so
    /// a step that finds it taken is retried briefly before reporting
    /// [`DraftlineError::GitIndexLocked`]. Every wrapped step is idempotent.
    fn while_git_busy<T>(&self, mut step: impl FnMut() -> Result<T>) -> Result<T> {
        let deadline = std::time::Instant::now() + GIT_BUSY_WAIT;
        loop {
            match step() {
                Err(DraftlineError::GitIndexLocked) if std::time::Instant::now() < deadline => {
                    test_hooks::fire("git_busy_retry", self.repo.path());
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(DraftlineError::GitIndexLocked) => {
                    // A failed write leaves libgit2's shared index ahead of disk.
                    if let Ok(mut index) = self.repo.index() {
                        let _ = index.read(true);
                    }
                    return Err(DraftlineError::GitIndexLocked);
                }
                result => return result,
            }
        }
    }

    fn ensure_git_index_unlocked(&self) -> Result<()> {
        if self.repo.path().join("index.lock").exists() {
            return Err(DraftlineError::GitIndexLocked);
        }
        test_hooks::fire("after_index_precheck", self.repo.path());
        Ok(())
    }

    /// Takes the Draftline lock, then confirms Draftline still manages this
    /// folder, so a handle on a folder that was moved can't keep writing.
    fn acquire_operation_lock(&self, operation: &str) -> Result<OperationLock> {
        let lock = OperationLock::acquire(&self.lock_path(), operation)?;
        self.claim_checkout(false)?;
        Ok(lock)
    }

    fn lock_path(&self) -> PathBuf {
        self.draftline_dir().join("operation.lock")
    }

    fn write_recovery_state(&self, state: &RecoveryState) -> Result<()> {
        self.write_recovery_ledger(state, None)
    }

    /// Records an operation that is about to move the checkout from `from`
    /// to `to`.
    fn write_recovery_state_for_checkout(
        &self,
        state: &RecoveryState,
        from: &Tree<'_>,
        to: &Tree<'_>,
    ) -> Result<()> {
        self.write_recovery_ledger(
            state,
            Some(RecordedCheckout {
                from_tree: from.id().to_string(),
                to_tree: to.id().to_string(),
            }),
        )
    }

    /// The trees recorded for the pending checkout, when the record has them.
    fn recorded_checkout(&self) -> Option<(Tree<'_>, Tree<'_>)> {
        let bytes = fs::read(self.ledger_path()).ok()?;
        let checkout = serde_json::from_slice::<RecoveryLedger>(&bytes)
            .ok()?
            .checkout?;
        let tree = |id: &str| {
            Oid::from_str(id)
                .ok()
                .and_then(|oid| self.repo.find_tree(oid).ok())
        };
        Some((tree(&checkout.from_tree)?, tree(&checkout.to_tree)?))
    }

    fn write_recovery_ledger(
        &self,
        state: &RecoveryState,
        checkout: Option<RecordedCheckout>,
    ) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        let ledger = RecoveryLedger {
            state: state.clone(),
            project: Some(self.project_key()),
            checkout,
        };
        fs::write(self.ledger_path(), serde_json::to_vec_pretty(&ledger)?)?;
        if state.completed && matches!(&state.operation, RecoveryOperation::DeleteRemoteVariation) {
            match fs::remove_file(self.remote_delete_recovery_metadata_path(&state.operation_id)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn write_remote_delete_recovery_metadata(
        &self,
        operation_id: &str,
        metadata: &RemoteVariationDeleteRecoveryMetadata,
    ) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        fs::write(
            self.remote_delete_recovery_metadata_path(operation_id),
            serde_json::to_vec_pretty(metadata)?,
        )?;
        Ok(())
    }

    fn read_remote_delete_recovery_metadata(
        &self,
        operation_id: &str,
    ) -> Result<Option<RemoteVariationDeleteRecoveryMetadata>> {
        let path = self.remote_delete_recovery_metadata_path(operation_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
    }

    fn initialize(self) -> Self {
        let _ = fs::create_dir_all(self.draftline_dir());
        self
    }

    fn claim_path(&self) -> PathBuf {
        self.draftline_dir().join("workspace.json")
    }

    /// Draftline manages one folder per repository: the one [`FOLDER_MARKER`]
    /// names. A branch without the file falls back to the checkout's local
    /// record, and the first folder opened claims a repository with neither.
    /// Opening writes the marker when the working tree lacks it, and the next
    /// save commits it. Any other folder is refused.
    fn claim_checkout(&self, opening: bool) -> Result<()> {
        let mine = self.project_key();
        // Either end of an interrupted move may open, so recovery can finish it.
        if self.unfinished_operation_touches(&mine) {
            return Ok(());
        }
        let owner = match self.read_marker()? {
            Some(folder) => {
                if opening && folder == mine {
                    self.record_local_claim()?;
                }
                folder
            }
            None => {
                let path = self.claim_path();
                let mut published = false;
                if !path.exists() {
                    match self.publish_new_claim(&path) {
                        Ok(()) => published = true,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                let folder = if published {
                    mine.clone()
                } else {
                    folder_key(&claimed_project(&self.read_claim(&path)?, &path)?)
                };
                if opening && folder == mine {
                    self.write_marker()?;
                }
                folder
            }
        };

        let folder = PathBuf::from(&owner);
        let missing = self.claimed_folder_missing(&folder)?;
        if owner == mine {
            // Our own folder vanished while HEAD still tracks it (renamed or
            // deleted outside Draftline): refuse, or a save would record
            // every file as deleted. A branch that lacks the folder is fine,
            // so the host can still switch back.
            if missing && self.head_tree_or_empty()?.get_path(&folder).is_ok() {
                return Err(DraftlineError::FolderMissing(folder));
            }
            Ok(())
        } else if missing {
            Err(DraftlineError::FolderMissing(folder))
        } else {
            Err(DraftlineError::FolderIsElsewhere(folder))
        }
    }

    fn marker_path(&self) -> PathBuf {
        self.root.join(FOLDER_MARKER)
    }

    /// The folder the working tree's [`FOLDER_MARKER`] names, if it has one.
    fn read_marker(&self) -> Result<Option<String>> {
        match self.marker_owner()? {
            FolderOwner::Nobody => Ok(None),
            FolderOwner::Folder(folder) => Ok(Some(folder)),
            FolderOwner::Invalid(reason) => Err(DraftlineError::InvalidFolderMarker(reason)),
            FolderOwner::Newer(reason) => Err(DraftlineError::NewerFolderMarker(reason)),
        }
    }

    fn marker_owner(&self) -> Result<FolderOwner> {
        let path = self.marker_path();
        let invalid =
            |reason: String| FolderOwner::Invalid(format!("{}: {reason}", path.display()));
        if path.is_dir() {
            return Ok(invalid("is a folder, not a file".to_string()));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FolderOwner::Nobody)
            }
            Err(error) => return Err(error.into()),
        };
        // Read the format alone first: a newer one may change everything else.
        let format = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("format").and_then(serde_json::Value::as_u64));
        if let Some(format) = format.filter(|format| *format > 1) {
            return Ok(FolderOwner::Newer(format!(
                "{}: format {format} needs a newer Draftline",
                path.display()
            )));
        }
        let marker: FolderMarker = match serde_json::from_slice(&bytes) {
            Ok(marker) => marker,
            Err(error) => return Ok(invalid(error.to_string())),
        };
        if marker.format != 1 {
            return Ok(invalid(format!("format {} is not valid", marker.format)));
        }
        if marker.folder == "." {
            return Ok(FolderOwner::Folder(String::new()));
        }
        Ok(canonical_folder(&marker.folder)
            .filter(|folder| !folder.as_os_str().is_empty())
            .map(|folder| FolderOwner::Folder(folder_key(&folder)))
            .unwrap_or_else(|| {
                invalid(format!(
                    "folder {:?} is not a folder inside this repository",
                    marker.folder
                ))
            }))
    }

    /// The checkout's local record, read without claiming anything.
    fn claim_owner(&self) -> Result<FolderOwner> {
        let path = self.claim_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FolderOwner::Nobody)
            }
            Err(error) => return Err(error.into()),
        };
        let claim = match serde_json::from_slice::<WorkspaceClaim>(&bytes) {
            Ok(claim) => claim,
            Err(error) => return Ok(FolderOwner::Invalid(format!("{}: {error}", path.display()))),
        };
        Ok(match claimed_project(&claim, &path) {
            Ok(folder) => FolderOwner::Folder(folder_key(&folder)),
            Err(error) => FolderOwner::Invalid(error.to_string()),
        })
    }

    /// Writes [`FOLDER_MARKER`] for this folder; a rename keeps readers from
    /// ever seeing a partial file.
    fn write_marker(&self) -> Result<()> {
        let bytes = marker_bytes(&self.project_key())?;
        let path = self.marker_path();
        // Rewriting the same record would leave Git seeing a change when the
        // checkout converted its line endings.
        let unchanged = fs::read(&path).is_ok_and(|existing| {
            existing
                .trim_ascii_end()
                .split(|byte| *byte == b'\n')
                .map(<[u8]>::trim_ascii_end)
                .eq(bytes
                    .trim_ascii_end()
                    .split(|byte| *byte == b'\n')
                    .map(<[u8]>::trim_ascii_end))
        });
        if unchanged {
            return Ok(());
        }
        let temp = unique_claim_temp(&path);
        fs::write(&temp, bytes)?;
        replace_file(&temp, &path)?;
        Ok(())
    }

    /// Points the local record at this folder once the marker agrees, so
    /// branches without the marker keep the same owner.
    fn record_local_claim(&self) -> Result<()> {
        let path = self.claim_path();
        let current = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WorkspaceClaim>(&bytes).ok());
        if current.is_some_and(|claim| claim.project == self.project_key()) {
            return Ok(());
        }
        self.write_claim()
    }

    /// A marker no commit has yet would block checking out a branch that has
    /// one, as any untracked file would; the branch's own marker wins.
    fn drop_unsaved_marker_for(&self, target: &Tree<'_>) -> Result<()> {
        if target.get_name(FOLDER_MARKER).is_none() || !self.marker_path().is_file() {
            return Ok(());
        }
        let mut index = self.repo.index()?;
        index.read(true)?;
        if index.get_path(Path::new(FOLDER_MARKER), 0).is_some() {
            return Ok(());
        }
        match fs::remove_file(self.marker_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether the claimed folder is absent; other filesystem errors surface
    /// rather than inviting a relocate.
    fn claimed_folder_missing(&self, project: &Path) -> Result<bool> {
        match fs::metadata(self.root.join(project)) {
            Ok(metadata) => Ok(!metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    /// Publishes a complete claim only if none exists, so a crash never leaves
    /// a partial one: the claim is written and synced to a temporary file,
    /// then hard-linked into place, which fails if another opener won. File
    /// systems without hard links fall back to an exclusive create.
    fn publish_new_claim(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write as _;
        fs::create_dir_all(self.draftline_dir())?;
        let bytes = self.claim_bytes().map_err(std::io::Error::other)?;
        let temp = unique_claim_temp(path);
        let written = fs::File::create(&temp).and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()
        });
        let linked = written.and_then(|()| fs::hard_link(&temp, path));
        let _ = fs::remove_file(&temp);
        match linked {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists && !path.exists() => {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                file.write_all(&bytes)?;
                file.sync_all()
            }
            result => result,
        }
    }

    /// Reads the claim, waiting briefly for a racing opener that created the
    /// file but has not written it yet.
    fn read_claim(&self, path: &Path) -> Result<WorkspaceClaim> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let bytes = fs::read(path)?;
            if !bytes.is_empty() || std::time::Instant::now() >= deadline {
                return serde_json::from_slice(&bytes).map_err(|error| {
                    DraftlineError::InvalidFolderMarker(format!(
                        "{}: {error}; delete it to reset it",
                        path.display()
                    ))
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn write_claim(&self) -> Result<()> {
        self.write_claim_for(&self.project_key())
    }

    fn write_claim_for(&self, folder: &str) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        let path = self.claim_path();
        let temp = unique_claim_temp(&path);
        fs::write(
            &temp,
            serde_json::to_vec_pretty(&WorkspaceClaim {
                project: folder.to_string(),
            })?,
        )?;
        replace_file(&temp, &path)?;
        Ok(())
    }

    fn claim_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(&WorkspaceClaim {
            project: self.project_key(),
        })?)
    }

    /// Every operation that reads or writes the checkout passes here, so a
    /// handle whose folder no longer holds the checkout's claim is refused.
    fn ensure_no_pending_recovery(&self) -> Result<()> {
        self.claim_checkout(false)?;
        self.ensure_no_pending_recovery_in_checkout()
    }

    fn ensure_no_pending_recovery_in_checkout(&self) -> Result<()> {
        self.ensure_recovery_is_ours()?;
        if let Some(state) = self.recovery_state()? {
            return Err(DraftlineError::RecoveryRequired(Box::new(state)));
        }

        Ok(())
    }

    /// Folders in one checkout share its branch, so one folder's unfinished
    /// operation blocks the others, but only that folder may finish it.
    fn ensure_recovery_is_ours(&self) -> Result<()> {
        match self.recovery_owner_elsewhere() {
            Some(project) => Err(DraftlineError::RecoveryInOtherFolder(project)),
            None => Ok(()),
        }
    }

    /// The other folder that owns the incomplete recovery record, if any. An
    /// unreadable record is reported by `recovery_state` and stays clearable.
    fn recovery_owner_elsewhere(&self) -> Option<PathBuf> {
        let bytes = fs::read(self.ledger_path()).ok()?;
        let ledger = serde_json::from_slice::<RecoveryLedger>(&bytes).ok()?;
        let project = ledger.project?;
        (!ledger.state.completed
            && project != self.project_key()
            && !self.unfinished_operation_touches(&self.project_key())
            && !self.marked_by_recovery())
        .then(|| PathBuf::from(project))
    }

    /// Whether the checkout's marker names this folder because the interrupted
    /// operation checked out one of its recorded commits, so this folder (the
    /// one now on disk) may finish it.
    fn marked_by_recovery(&self) -> bool {
        let mine = self.project_key();
        self.read_marker().ok().flatten().as_deref() == Some(mine.as_str())
            && self.recorded_markers().contains(&mine)
    }

    /// Whether `folder` recorded an unfinished operation, or is either end of
    /// an unfinished move. That folder may always open to finish it, even when
    /// the interrupted checkout left a marker naming another folder.
    fn unfinished_operation_touches(&self, folder: &str) -> bool {
        let Ok(bytes) = fs::read(self.ledger_path()) else {
            return false;
        };
        let Ok(ledger) = serde_json::from_slice::<RecoveryLedger>(&bytes) else {
            return false;
        };
        let state = ledger.state;
        if state.completed {
            return false;
        }
        ledger.project.as_deref() == Some(folder) || self.pending_move_touches(folder)
    }

    /// Whether an unfinished folder move has `folder` at either end.
    fn pending_move_touches(&self, folder: &str) -> bool {
        let Ok(bytes) = fs::read(self.ledger_path()) else {
            return false;
        };
        let Ok(ledger) = serde_json::from_slice::<RecoveryLedger>(&bytes) else {
            return false;
        };
        let state = ledger.state;
        if state.completed || state.operation != RecoveryOperation::MoveFolder {
            return false;
        }
        let folder = marker_folder(folder);
        state.target.as_deref().is_some_and(|target| {
            target
                .split_once(" -> ")
                .is_some_and(|(from, to)| from == folder || to == folder)
        })
    }

    /// Repair and rollback write files, the index, HEAD or refs, so outside an
    /// interrupted move they run only while this is still the managed folder.
    /// The operation itself may have checked out a tree whose marker names
    /// another folder (for example a switch interrupted before HEAD moved), so
    /// a marker from one of its recorded checkout trees still allows it. After a
    /// developer checks out some other branch naming another folder, the
    /// recording folder can only acknowledge (drop) the record.
    fn ensure_recovery_may_write(&self) -> Result<()> {
        let mine = self.project_key();
        if self.pending_move_touches(&mine) {
            return Ok(());
        }
        let owner = match self.read_marker()? {
            Some(folder) => folder,
            None => {
                // With neither, this folder claims the checkout as the first
                // one opened would; a folder that claimed it first wins.
                let path = self.claim_path();
                match self.publish_new_claim(&path) {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                folder_key(&claimed_project(&self.read_claim(&path)?, &path)?)
            }
        };
        if owner == mine || self.recorded_markers().contains(&owner) {
            Ok(())
        } else {
            Err(DraftlineError::FolderIsElsewhere(PathBuf::from(owner)))
        }
    }

    /// The folders named by the markers of the trees the pending checkout was
    /// moving between. Only these immutable recorded trees count, never
    /// branch tips that may have moved since.
    fn recorded_markers(&self) -> Vec<String> {
        self.recorded_checkout()
            .map(|(from, to)| {
                [from, to]
                    .iter()
                    .filter_map(|tree| tree_marker_folder(&self.repo, tree))
                    .map(|folder| folder_key(&folder))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn project_key(&self) -> String {
        folder_key(self.scope())
    }

    fn normalize_tracked_content_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = normalize_workspace_relative(path)?;
        if path.as_os_str().is_empty() || !self.content_policy.tracks(&path)? {
            return Err(DraftlineError::PathOutsideContentPolicy(path));
        }

        Ok(path)
    }

    fn selected_changed_files<I, P>(&self, paths: I) -> Result<Vec<ChangedFile>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let selected_paths = paths
            .into_iter()
            .map(|path| self.normalize_tracked_content_path(path))
            .collect::<Result<BTreeSet<_>>>()?;
        Ok(self
            .changed_files_unchecked()?
            .into_iter()
            .filter(|changed| selected_paths.contains(&changed.path))
            .collect())
    }

    /// Builds the tree for `changed_files` on top of HEAD in a private
    /// in-memory index so the developer's staged changes are never consumed.
    /// Returns the tree and every path whose real index entry should follow it.
    fn content_changes_tree(
        &self,
        base: Option<&Commit<'_>>,
        changed_files: &[ChangedFile],
        operation: &str,
    ) -> Result<(Oid, Vec<PathBuf>)> {
        // Resolve every path first so filter checks see what will really be added.
        let mut additions = Vec::new();
        let mut removals = Vec::new();
        for changed in changed_files {
            match changed.kind {
                ChangeKind::Renamed => {
                    let target = self
                        .staged_rename_target_for_path(&changed.path)?
                        .ok_or_else(|| DraftlineError::LocalStateChanged {
                            expected: format!(
                                "staged rename target for {}",
                                changed.path.display()
                            ),
                            actual: "missing rename target".to_string(),
                        })?;
                    removals.push(changed.path.clone());
                    // A rename out of the project leaves the target to whoever made it.
                    if self.content_policy.tracks(&target)? {
                        additions.push(target);
                    }
                }
                ChangeKind::Deleted => removals.push(changed.path.clone()),
                _ => additions.push(changed.path.clone()),
            }
        }
        // The marker is committed with every save, so clones and other
        // branches learn which folder Draftline manages.
        if self.marker_path().is_file() {
            additions.push(PathBuf::from(FOLDER_MARKER));
        }
        self.ensure_no_filtered_content(&additions, operation)?;

        // A second handle owns its own index object; it is never written to disk.
        let scratch = Repository::open(&self.root)?;
        let mut index = scratch.index()?;
        index.clear()?;
        // Built from the exact parent the commit will get, so a developer
        // commit landing meanwhile can never be reverted by this tree.
        if let Some(base) = base {
            index.read_tree(&base.tree()?)?;
        }
        for path in &removals {
            remove_index_path(&mut index, path)?;
        }
        let filtered = self.filtered_additions(&additions)?;
        for path in additions.iter().filter(|path| !filtered.contains(*path)) {
            index.add_path(path)?;
        }
        self.add_filtered(&mut index, &filtered)?;
        let affected_paths = removals.into_iter().chain(additions).collect::<Vec<_>>();

        Ok((index.write_tree()?, affected_paths))
    }

    /// The regular files among `additions` whose filter driver Draftline
    /// runs through `git` (the others were refused already).
    fn filtered_additions(&self, additions: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let runner = FilterRunner::of(&self.repo)?;
        let mut filtered = Vec::new();
        for path in additions {
            if self.filter_use(&runner, None, path)? == FilterUse::Runs
                && fs::symlink_metadata(self.root.join(path)).is_ok_and(|meta| meta.is_file())
            {
                filtered.push(path.clone());
            }
        }
        Ok(filtered)
    }

    /// Stages `paths` as `git add` would, running their clean filters (Git
    /// LFS stores the content under `.git/lfs` and returns a pointer).
    fn add_filtered(&self, index: &mut git2::Index, paths: &[PathBuf]) -> Result<()> {
        let ids = git_cli::clean(&self.root, paths, true)?;
        // Like `git add`: the file's executable bit counts only where the
        // repository trusts file modes; elsewhere the staged mode stays.
        let trust_mode = cfg!(unix)
            && self
                .repo
                .config()?
                .get_bool("core.filemode")
                .unwrap_or(true);
        for (path, id) in paths.iter().zip(ids) {
            let executable = if trust_mode {
                is_executable(&self.root.join(path))
            } else {
                index
                    .get_path(path, 0)
                    .is_some_and(|entry| entry.mode == 0o100755)
            };
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: if executable { 0o100755 } else { 0o100644 },
                uid: 0,
                gid: 0,
                file_size: 0,
                id,
                flags: 0,
                flags_extended: 0,
                path: path.to_string_lossy().replace('\\', "/").into_bytes(),
            })?;
        }
        Ok(())
    }

    /// Commits `changed_files` on top of HEAD, then syncs only the saved paths
    /// in the real index so `git status` stays clean for them.
    /// Commits onto `branch`, the branch the save started on; a switch to
    /// another branch at any point refuses with `LocalStateChanged`.
    fn commit_content_changes(
        &self,
        branch: &str,
        changed_files: &[ChangedFile],
        operation: &str,
        label: &str,
        profile: Option<&ContributorProfile>,
    ) -> Result<Version> {
        let now = self.ensure_on_branch()?;
        if now != branch {
            return Err(DraftlineError::LocalStateChanged {
                expected: branch.to_string(),
                actual: now,
            });
        }
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        test_hooks::fire("after_parent_read", self.repo.path());
        let (tree_id, affected_paths) =
            self.content_changes_tree(parent.as_ref(), changed_files, operation)?;
        let tree = self.repo.find_tree(tree_id)?;
        let (author, committer) = self.workspace_signatures(profile)?;
        let parents = parent.iter().collect::<Vec<_>>();
        self.while_git_busy(|| self.ensure_git_index_unlocked())?;
        // Like `git commit`: the index is updated before the branch moves, so a
        // busy index fails the save cleanly instead of leaving a commit the
        // index would silently revert on the developer's next commit.
        let oid = self
            .repo
            .commit(None, &author, &committer, label, &tree, &parents)?;
        let commit = self.repo.find_commit(oid)?;
        let previous = self.while_git_busy(|| self.stage_from(&commit, &affected_paths))?;
        if let Err(error) =
            self.while_git_busy(|| self.advance_head(branch, &commit, parent.as_ref(), &committer))
        {
            self.restore_index_entries(&affected_paths, previous)?;
            return Err(error);
        }
        // Entries for filtered files carry no timestamps, and libgit2 can't
        // rehash such a file to match them; Git can.
        if !self.filtered_additions(&affected_paths)?.is_empty() {
            git_cli::refresh_index(&self.root);
        }
        Ok(version_from_commit(&commit))
    }

    /// Sets the index entries for `paths` to `commit`'s, returning every entry
    /// (all stages) they replace. libgit2 cannot hold Git's index lock across
    /// a read and a write, so the index is reread just before the write.
    fn stage_from(&self, commit: &Commit<'_>, paths: &[PathBuf]) -> Result<Vec<git2::IndexEntry>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut index = self.repo.index()?;
        index.read(true)?;
        let wanted = paths
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/").into_bytes())
            .collect::<HashSet<_>>();
        let previous = index
            .iter()
            .filter(|entry| wanted.contains(&entry.path))
            .collect();
        drop(index);
        self.write_index_from_tree(&commit.tree()?, paths, &[])?;
        Ok(previous)
    }

    /// Puts back the entries [`Self::stage_from`] replaced.
    fn restore_index_entries(
        &self,
        paths: &[PathBuf],
        entries: Vec<git2::IndexEntry>,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.while_git_busy(|| self.write_index_entries(paths, &entries))
    }

    fn write_index_entries(&self, paths: &[PathBuf], entries: &[git2::IndexEntry]) -> Result<()> {
        let mut index = self.repo.index()?;
        index.read(true)?;
        for path in paths {
            if let Err(error) = index.remove_path(path) {
                if error.code() != git2::ErrorCode::NotFound {
                    return Err(error.into());
                }
            }
        }
        for entry in entries {
            index.add(entry)?;
        }
        write_index_or_reload(&mut index)
    }

    /// Moves `branch`, the one checked out when the save began, to `commit`
    /// only if `HEAD` is still on it and it still points at `parent`, holding
    /// the refs' locks as `git commit` does.
    fn advance_head(
        &self,
        branch: &str,
        commit: &Commit<'_>,
        parent: Option<&Commit<'_>>,
        committer: &Signature<'_>,
    ) -> Result<()> {
        let name = branch.to_string();
        let mut transaction = self.repo.transaction()?;
        transaction.lock_ref("HEAD")?;
        transaction.lock_ref(&name)?;
        // A switch to another branch at the same commit after the save began
        // must not land the save there.
        let now = self.repo.find_reference("HEAD")?;
        let now = now.symbolic_target().ok().flatten().unwrap_or("HEAD");
        if now != name {
            return Err(DraftlineError::LocalStateChanged {
                expected: name,
                actual: now.to_string(),
            });
        }
        let current = self.repo.refname_to_id(&name).ok();
        let expected = parent.map(Commit::id);
        if current != expected {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{name} at {expected:?}"),
                actual: format!("{current:?}"),
            });
        }
        let summary = commit.summary().ok().flatten().unwrap_or_default();
        let message = match parent {
            Some(_) => format!("commit: {summary}"),
            None => format!("commit (initial): {summary}"),
        };
        transaction.set_target(&name, commit.id(), Some(committer), &message)?;
        transaction.commit()?;
        Ok(())
    }

    /// Refuses content that needs a Git filter driver (for example Git LFS),
    /// which libgit2 cannot run; committing it raw would corrupt the repository.
    fn ensure_no_filtered_content(&self, paths: &[PathBuf], operation: &str) -> Result<()> {
        let hazards = self.filtered_hazards(paths)?;
        if hazards.is_empty() {
            Ok(())
        } else {
            Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                operation,
                false,
                Vec::new(),
                hazards,
                None,
            ))))
        }
    }

    /// A discard rewrites paths from `HEAD`, so `HEAD`'s rules count too.
    fn discard_filtered_hazards(&self, changed_files: &[ChangedFile]) -> Result<Vec<FileHazard>> {
        let paths = Self::discard_write_paths(changed_files);
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let head = self.repo.head()?.peel_to_tree()?;
        self.filtered_hazards_toward(&paths, Some(&head))
    }

    fn ensure_discard_unfiltered(
        &self,
        changed_files: &[ChangedFile],
        operation: &str,
    ) -> Result<()> {
        let hazards = self.discard_filtered_hazards(changed_files)?;
        if hazards.is_empty() {
            return Ok(());
        }
        Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
            operation,
            false,
            Vec::new(),
            hazards,
            None,
        ))))
    }

    fn filtered_hazards(&self, paths: &[PathBuf]) -> Result<Vec<FileHazard>> {
        self.filtered_hazards_toward(paths, None)
    }

    /// Like [`Self::filtered_hazards`], but a path also counts when `target`'s
    /// own `.gitattributes` would filter it, since a checkout installs those
    /// rules only after it has written the file.
    fn filtered_hazards_toward(
        &self,
        paths: &[PathBuf],
        target: Option<&Tree<'_>>,
    ) -> Result<Vec<FileHazard>> {
        let attributes = target.map(|tree| self.attributes_of(tree)).transpose()?;
        let runner = FilterRunner::of(&self.repo)?;
        let mut hazards = Vec::new();
        for path in paths {
            if self.filter_use(&runner, attributes.as_ref(), path)? == FilterUse::Refused {
                hazards.push(self.filter_hazard(path.clone()));
            }
        }
        Ok(hazards)
    }

    /// A filter refusal, naming whether the file is the project's or a
    /// developer's outside the folder.
    fn filter_hazard(&self, path: PathBuf) -> FileHazard {
        let kind = if path.starts_with(self.scope()) {
            FileHazardKind::GitFilter
        } else {
            FileHazardKind::GitFilterOutsideProject
        };
        FileHazard { path, kind }
    }

    /// A second handle whose index is `tree`, so attribute lookups see the
    /// `.gitattributes` rules `tree` carries rather than the checkout's.
    fn attributes_of(&self, tree: &Tree<'_>) -> Result<Repository> {
        let repo = Repository::open(self.repo.path())?;
        let mut index = git2::Index::new()?;
        index.read_tree(tree)?;
        repo.set_index(&mut index)?;
        Ok(repo)
    }

    /// How `path` is filtered under the current rules and, for a checkout,
    /// `target`'s: not at all, by drivers Draftline runs through `git`, or by
    /// one it can't run (unconfigured, or no `git` here).
    fn filter_use(
        &self,
        runner: &FilterRunner,
        target: Option<&Repository>,
        path: &Path,
    ) -> Result<FilterUse> {
        let drivers = [
            git_filter_driver(&self.repo, path, git2::AttrCheckFlags::FILE_THEN_INDEX)?,
            match target {
                Some(repo) => git_filter_driver(repo, path, git2::AttrCheckFlags::INDEX_ONLY)?,
                None => None,
            },
        ];
        let mut found = FilterUse::None;
        for driver in drivers.into_iter().flatten() {
            if !runner.runs(&driver) {
                return Ok(FilterUse::Refused);
            }
            found = FilterUse::Runs;
        }
        Ok(found)
    }

    /// libgit2 cannot run filter drivers such as Git LFS, so it writes the
    /// stored pointer text in place of the real file. A checkout here aborts
    /// while planning, before any file is written, if it would update a path
    /// whose driver (under the current rules or `target`'s) Draftline can't
    /// run; paths whose driver it can run are rewritten through `git` once
    /// the checkout has written them.
    fn checkout_refusing_filters<'cb>(
        &'cb self,
        mut checkout: CheckoutBuilder<'cb>,
        plan: &'cb RefCell<FilterPlan>,
        target: &Tree<'_>,
        run: impl FnOnce(&mut CheckoutBuilder<'cb>) -> std::result::Result<(), git2::Error>,
    ) -> Result<()> {
        let attributes = self.attributes_of(target)?;
        let runner = FilterRunner::of(&self.repo)?;
        checkout
            .notify_on(git2::CheckoutNotificationType::UPDATED)
            .notify(move |_, path, _, file, _| {
                let Some(path) = path.filter(|_| file.is_some_and(|file| file.exists())) else {
                    return true;
                };
                match self.filter_use(&runner, Some(&attributes), path) {
                    Ok(FilterUse::None) => true,
                    Ok(FilterUse::Runs) => {
                        plan.borrow_mut().smudge.push(path.to_path_buf());
                        true
                    }
                    Ok(FilterUse::Refused) | Err(_) => {
                        plan.borrow_mut().refused.push(path.to_path_buf());
                        false
                    }
                }
            });
        let result = run(&mut checkout);
        let FilterPlan { refused, smudge } = plan.take();
        if refused.is_empty() {
            // A locked index fails only libgit2's last step, the index
            // write, so the files are already written.
            if matches!(&result, Ok(()))
                || matches!(&result, Err(error) if error.code() == git2::ErrorCode::Locked)
            {
                self.smudge_written(target, &smudge);
            }
            return Ok(result?);
        }
        Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
            "checkout",
            true,
            Vec::new(),
            refused
                .into_iter()
                .map(|path| self.filter_hazard(path))
                .collect(),
            None,
        ))))
    }

    /// Rewrites the `paths` a checkout just wrote from `target` through their
    /// smudge filters, then lets Git refresh their index timestamps. Content
    /// Git can't produce (say, LFS objects it can't download offline) stays
    /// as the pointer, as with `GIT_LFS_SKIP_SMUDGE`, and shows up in
    /// `inspect` as missing; the checkout itself still stands.
    fn smudge_written(&self, target: &Tree<'_>, paths: &[PathBuf]) {
        let entries = paths
            .iter()
            .filter_map(|path| {
                let entry = target.get_path(path).ok()?;
                let mode = entry.filemode();
                (mode == i32::from(git2::FileMode::Blob)
                    || mode == i32::from(git2::FileMode::BlobExecutable))
                .then(|| (entry.id(), path.clone()))
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return;
        }
        let pointers = entries
            .iter()
            .map(|(id, path)| (path.as_path(), *id))
            .collect::<HashMap<_, _>>();
        let write = |path: &Path, content: &[u8]| {
            let Some(pointer) = pointers.get(path) else {
                return Ok(());
            };
            let pointer = self.repo.find_blob(*pointer)?;
            self.replace_working_file(path, pointer.content(), content)
        };
        let scratch = self.repo.path().join("draftline").join(format!(
            "smudge-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let result = self.smudge_through_index(&scratch, &entries, write);
        let _ = fs::remove_dir_all(&scratch);
        if let Err(error) = result {
            tracing::warn!(%error, "left Git filter pointers in place of file content");
        }
        // The checkout stamped these entries with the pointer's stat, which
        // Git takes as proof the real file changed. A zero size makes Git
        // compare content instead, so the refresh restamps them.
        if let Err(error) = self.forget_index_stat(&entries) {
            tracing::warn!(%error, "could not mark smudged files for an index refresh");
        }
        git_cli::refresh_index(&self.root);
    }

    /// Smudges `entries` through a private index holding exactly them, so a
    /// concurrent change to the repository's index can't swap in other blobs.
    fn smudge_through_index(
        &self,
        scratch: &Path,
        entries: &[(Oid, PathBuf)],
        write: impl FnMut(&Path, &[u8]) -> Result<()>,
    ) -> Result<()> {
        fs::create_dir_all(scratch)?;
        let index_file = scratch.join("index");
        let mut index = git2::Index::open(&index_file)?;
        for (id, path) in entries {
            let name = git_cli::git_path(path)?.into_bytes();
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: u32::from(git2::FileMode::Blob),
                uid: 0,
                gid: 0,
                file_size: 0,
                id: *id,
                flags: name.len().min(0xfff) as u16,
                flags_extended: 0,
                path: name,
            })?;
        }
        index.write()?;
        let paths = entries
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>();
        git_cli::smudge(
            &self.root,
            &index_file,
            &scratch.join("files"),
            &paths,
            write,
        )
    }

    /// Smudges the folder's filtered files whose disk content is still the
    /// exact blob `HEAD` stores for them (the pointer a checkout wrote before
    /// its smudge ran). Such a file looks unchanged, since cleaning it gives
    /// the same blob, so nothing else would ever replace it. Returns whether
    /// any file changed.
    fn smudge_left_pointers(&self) -> Result<bool> {
        let Ok(head) = self.repo.head().and_then(|head| head.peel_to_tree()) else {
            return Ok(false);
        };
        let runner = FilterRunner::of(&self.repo)?;
        let mut pointers = Vec::new();
        for path in self.tracked_project_paths()? {
            if !matches!(self.filter_use(&runner, None, &path)?, FilterUse::Runs) {
                continue;
            }
            let Ok(entry) = head.get_path(&path) else {
                continue;
            };
            let Ok(blob) = self.repo.find_blob(entry.id()) else {
                continue;
            };
            let full = self.root.join(&path);
            let same_size = fs::symlink_metadata(&full)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == blob.size() as u64);
            if same_size && fs::read(&full).is_ok_and(|disk| disk == blob.content()) {
                pointers.push(path);
            }
        }
        if pointers.is_empty() {
            return Ok(false);
        }
        self.smudge_written(&head, &pointers);
        Ok(pointers.iter().any(|path| {
            !fs::read(self.root.join(path)).is_ok_and(|disk| {
                head.get_path(path)
                    .ok()
                    .and_then(|entry| self.repo.find_blob(entry.id()).ok())
                    .is_some_and(|blob| disk == blob.content())
            })
        }))
    }

    fn forget_index_stat(&self, entries: &[(Oid, PathBuf)]) -> Result<()> {
        let mut index = self.repo.index()?;
        index.read(true)?;
        let mut changed = false;
        for (id, path) in entries {
            let Some(mut entry) = index.get_path(path, 0) else {
                continue;
            };
            if entry.id == *id && entry.file_size != 0 {
                entry.file_size = 0;
                entry.mtime = git2::IndexTime::new(0, 0);
                entry.ctime = git2::IndexTime::new(0, 0);
                index.add(&entry)?;
                changed = true;
            }
        }
        if changed {
            write_index_or_reload(&mut index)?;
        }
        Ok(())
    }

    /// Atomically sets the working file at `path` to `content`, keeping its
    /// permissions, but only while it still holds `expected` (what the
    /// checkout wrote): anything else is someone's edit, and stays.
    fn replace_working_file(&self, path: &Path, expected: &[u8], content: &[u8]) -> Result<()> {
        let full = self.root.join(path);
        let metadata = fs::symlink_metadata(&full)?;
        if !metadata.is_file() || metadata.len() != expected.len() as u64 {
            return Ok(());
        }
        if fs::read(&full)? != expected || expected == content {
            return Ok(());
        }
        let temp = unique_claim_temp(&full);
        let written = fs::write(&temp, content)
            .and_then(|()| fs::set_permissions(&temp, metadata.permissions()));
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        replace_file(&temp, &full)?;
        Ok(())
    }

    /// Whether Draftline runs the Git filter `driver` (for example `lfs`) in
    /// this repository: the driver is configured and the `git` executable is
    /// available. Where it doesn't, files under it are refused.
    pub fn runs_filter(&self, driver: impl AsRef<str>) -> Result<bool> {
        Ok(FilterRunner::of(&self.repo)?.runs(driver.as_ref()))
    }

    /// Tracked files in the folder that Git stores through a filter driver, such as
    /// Git LFS, with whether their content is missing from disk. Untracked files
    /// are not listed; check those with [`Workspace::filtered_files_among`].
    pub fn filtered_files(&self) -> Result<Vec<FilteredFile>> {
        let paths = self.tracked_project_paths()?;
        self.filtered_among(paths)
    }

    /// Tracked files in the folder under a filter whose disk content is a Git LFS
    /// pointer. Cheap enough for `inspect`: only files of at most 1 KiB are read.
    fn missing_lfs_content(&self) -> Result<Vec<PathBuf>> {
        let mut missing = Vec::new();
        for path in self.tracked_project_paths()? {
            if is_lfs_pointer_file(&self.root.join(&path))
                && needs_git_filter(&self.repo, &path, git2::AttrCheckFlags::FILE_THEN_INDEX)?
            {
                missing.push(path);
            }
        }
        Ok(missing)
    }

    /// Stage-0 regular-file index paths in the project that the content
    /// policy tracks.
    fn tracked_project_paths(&self) -> Result<Vec<PathBuf>> {
        let mut index = self.repo.index()?;
        index.read(false)?;
        let scope = self.scope().to_path_buf();
        let mut paths = Vec::new();
        for entry in index.iter() {
            if (entry.flags >> 12) & 0x3 != 0 || entry.mode & 0o170000 != 0o100000 {
                continue;
            }
            let path = PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned());
            if path.starts_with(&scope) && self.content_policy.tracks(&path)? {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// Which of `paths` Git would store through a filter driver. Paths need
    /// not exist or be tracked, so a host can check where it is about to write.
    /// Like every Draftline path, they are relative to the repository, not the
    /// folder (`cutready/shot.png`); returned paths use `/`.
    pub fn filtered_files_among<I, P>(&self, paths: I) -> Result<Vec<FilteredFile>>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| {
                self.normalize_tracked_content_path(path).map(|path| {
                    PathBuf::from(
                        path.components()
                            .map(|part| part.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                    )
                })
            })
            .collect::<Result<BTreeSet<_>>>()?;
        self.filtered_among(paths)
    }

    fn filtered_among(
        &self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Vec<FilteredFile>> {
        let runner = FilterRunner::of(&self.repo)?;
        let mut files = Vec::new();
        for path in paths {
            let Some(driver) =
                git_filter_driver(&self.repo, &path, git2::AttrCheckFlags::FILE_THEN_INDEX)?
            else {
                continue;
            };
            let driver_configured = driver_configured(&runner.config, &driver);
            let driver_runs = runner.runs(&driver);
            let content_missing = is_lfs_pointer_file(&self.root.join(&path));
            files.push(FilteredFile {
                path,
                driver,
                driver_configured,
                driver_runs,
                content_missing,
            });
        }
        Ok(files)
    }

    /// Moves working files from the current HEAD tree to `target` the way
    /// `git checkout` does: files with local edits are never overwritten.
    /// Must run before HEAD or its branch moves, since HEAD is the baseline.
    fn checkout_tree_safely(&self, target: &Tree<'_>) -> Result<()> {
        self.drop_unsaved_marker_for(target)?;
        let baseline = self.repo.head().and_then(|head| head.peel_to_tree()).ok();
        let position = self.head_position();
        let mut checkout = CheckoutBuilder::new();
        checkout.safe().overwrite_ignored(false);
        let refused = RefCell::default();
        let result = self.checkout_refusing_filters(checkout, &refused, target, |checkout| {
            self.repo.checkout_tree(target.as_object(), Some(checkout))
        });
        if !matches!(result, Err(DraftlineError::GitIndexLocked)) {
            return result;
        }
        // libgit2 writes the index last, so every file is already at `target`.
        // Rerunning a safe checkout would see files it just added as untracked
        // and refuse; writing the moved paths' index entries from `target`
        // finishes the job instead. If HEAD (its branch or tip) or any of those files changed while
        // Git held the index, the lock error stands and recovery takes over.
        let paths = self.paths_changed_between(baseline.as_ref(), target)?;
        self.while_git_busy(|| {
            if self.head_position() != position
                || self.workdir_matches(target, &paths)?.len() != paths.len()
            {
                return Err(DraftlineError::GitIndexLocked);
            }
            self.write_index_from_tree(target, &paths, &[])
        })
    }

    /// Sets the index entries for `paths` to `tree`'s (removing those `tree`
    /// lacks) and drops `remove`, in a single index write. Entries carry no
    /// stat data, so Git rehashes those files on the next status.
    fn write_index_from_tree(
        &self,
        tree: &Tree<'_>,
        paths: &[PathBuf],
        remove: &[PathBuf],
    ) -> Result<()> {
        let mut index = self.repo.index()?;
        index.read(true)?;
        for path in paths.iter().chain(remove) {
            if let Err(error) = index.remove_path(path) {
                if error.code() != git2::ErrorCode::NotFound {
                    return Err(error.into());
                }
            }
        }
        for path in paths {
            let entry = match tree.get_path(path) {
                Ok(entry) if entry.kind() != Some(ObjectType::Tree) => entry,
                Ok(_) => continue,
                Err(error) if error.code() == git2::ErrorCode::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let key = path.to_string_lossy().replace('\\', "/").into_bytes();
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: entry.filemode() as u32,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: entry.id(),
                flags: key.len().min(0xfff) as u16,
                flags_extended: 0,
                path: key,
            })?;
        }
        write_index_or_reload(&mut index)
    }

    /// Safe checkout for an operation with a pending recovery record. A refused
    /// checkout (local edits in the way) writes nothing, so the record is
    /// dropped; any other failure may have written some files, so the record
    /// stays for [`Self::repair_recovery`].
    fn checkout_or_keep_recovery(&self, target: &Tree<'_>) -> Result<()> {
        match self.checkout_tree_safely(target) {
            Err(DraftlineError::Git(error)) if error.code() == git2::ErrorCode::Conflict => {
                let _ = self.clear_recovery_records();
                Err(DraftlineError::Git(error))
            }
            result => self.clear_recovery_if_refused(result),
        }
    }

    /// A filter refusal aborts a checkout before it writes anything, so the
    /// pending record has nothing to recover.
    fn clear_recovery_if_refused(&self, result: Result<()>) -> Result<()> {
        if matches!(result, Err(DraftlineError::PreflightFailed(_))) {
            let _ = self.clear_recovery_records();
        }
        result
    }

    /// Forces only the folder to `target`; paths outside it are untouched.
    fn checkout_scope_forced(&self, target: &Tree<'_>) -> Result<()> {
        let mut checkout = CheckoutBuilder::new();
        checkout.force();
        if let Some(pathspec) = scope_pathspec(self.scope()) {
            checkout.disable_pathspec_match(true).path(pathspec);
        }
        let refused = RefCell::default();
        let result = self.checkout_refusing_filters(checkout, &refused, target, |checkout| {
            self.repo.checkout_tree(target.as_object(), Some(checkout))
        });
        if !matches!(result, Err(DraftlineError::GitIndexLocked)) {
            return result;
        }
        // libgit2 writes the index last, so the folder's files are already at
        // `target`. Forcing them again could overwrite an edit made while Git
        // held the index; finishing only the index leaves such an edit visible
        // as a change instead.
        self.while_git_busy(|| {
            let paths = self.scope_index_paths(target)?;
            self.write_index_from_tree(target, &paths, &[])
        })
    }

    /// Every path in the folder that `target` or the index has.
    fn scope_index_paths(&self, target: &Tree<'_>) -> Result<Vec<PathBuf>> {
        let scope = scope_pathspec(self.scope());
        let in_scope = |path: &str| {
            scope.as_deref().is_none_or(|scope| {
                path == scope
                    || path
                        .strip_prefix(scope)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        let mut paths = BTreeSet::new();
        target.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(ObjectType::Tree) {
                let path = format!("{dir}{}", String::from_utf8_lossy(entry.name_bytes()));
                if in_scope(&path) {
                    paths.insert(path);
                }
            }
            git2::TreeWalkResult::Ok
        })?;
        let mut index = self.repo.index()?;
        index.read(true)?;
        for entry in index.iter() {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            if in_scope(&path) {
                paths.insert(path);
            }
        }
        Ok(paths.into_iter().map(PathBuf::from).collect())
    }

    /// `base` with its folder replaced by `source`'s folder.
    /// Unscoped workspaces use `source` as-is.
    /// `commit`'s tree with Draftline's folder where the folder is now, so a
    /// version saved before a move reads, diffs and restores as this folder's.
    fn relocated_tree(&self, commit: &Commit<'_>) -> Result<Tree<'_>> {
        let tree = commit.tree()?;
        let from = FolderChanges::new(&self.repo, self.scope()).folder_at(commit)?;
        if from == self.scope() {
            return Ok(self.repo.find_tree(tree.id())?);
        }
        // The folder's content as it was: a root folder is everything but the
        // repository files.
        let content = if from.as_os_str().is_empty() {
            Some(split_root(&self.repo, &tree)?.0)
        } else {
            optional_tree_entry(&tree, &from)?
        };
        let id = match scope_pathspec(self.scope()) {
            // Now the root: the folder's content is the whole tree, beside
            // today's repository files.
            None => {
                let content = match content {
                    Some(content) => content,
                    None => self.empty_tree_id()?,
                };
                let content = self.repo.find_tree(content)?;
                let head = self.head_tree_or_empty()?;
                let mut update = git2::build::TreeUpdateBuilder::new();
                for entry in head.iter() {
                    if is_repository_file(entry.name_bytes()) {
                        if let Ok(name) = entry.name() {
                            update.upsert(name, entry.id(), entry_file_mode(&entry));
                        }
                    }
                }
                update.create_updated(&self.repo, &content)?
            }
            Some(pathspec) => {
                // The folder leaves its old place; a root folder leaves only
                // the repository files behind.
                let base = if from.as_os_str().is_empty() {
                    split_root(&self.repo, &tree)?.1
                } else {
                    tree_without(&self.repo, &tree, &folder_key(&from))?
                };
                let base = self.repo.find_tree(base)?;
                match content {
                    Some(content) => {
                        let mut update = git2::build::TreeUpdateBuilder::new();
                        update.upsert(pathspec.as_str(), content, git2::FileMode::Tree);
                        update.create_updated(&self.repo, &base)?
                    }
                    None => tree_without(&self.repo, &base, pathspec.as_str())?,
                }
            }
        };
        Ok(self.repo.find_tree(id)?)
    }

    fn compose_scoped_tree(&self, base: &Tree<'_>, source: &Tree<'_>) -> Result<Oid> {
        let Some(pathspec) = scope_pathspec(self.scope()) else {
            // Repository files describe the repository, not a version of the
            // folder, so restoring never changes them.
            let mut update = git2::build::TreeUpdateBuilder::new();
            let mut changed = false;
            for name in REPOSITORY_FILES {
                let wanted = base.get_name(name);
                let have = source.get_name(name);
                if wanted.as_ref().map(|entry| (entry.id(), entry.filemode()))
                    == have.as_ref().map(|entry| (entry.id(), entry.filemode()))
                {
                    continue;
                }
                changed = true;
                match wanted {
                    Some(entry) => update.upsert(name, entry.id(), entry_file_mode(&entry)),
                    None => update.remove(name),
                };
            }
            if !changed {
                return Ok(source.id());
            }
            return Ok(update.create_updated(&self.repo, source)?);
        };
        let source_entry = optional_tree_entry(source, self.scope())?;
        let base_entry = optional_tree_entry(base, self.scope())?;
        let mut update = git2::build::TreeUpdateBuilder::new();
        match (source_entry, base_entry) {
            (Some(entry), _) => {
                update.upsert(pathspec.as_str(), entry, git2::FileMode::Tree);
            }
            (None, Some(_)) => {
                update.remove(pathspec.as_str());
            }
            (None, None) => return Ok(base.id()),
        }
        Ok(update.create_updated(&self.repo, base)?)
    }

    /// HEAD with only the content paths the shelf changed replaced by the
    /// shelf's versions, plus those paths. Everything else stays at HEAD, so
    /// applying a shelf never reverts commits made since it was created.
    fn shelf_target_tree(&self, shelf: &Commit<'_>) -> Result<(Oid, Vec<PathBuf>)> {
        let shelf_tree = shelf.tree()?;
        let parent_tree = match shelf.parent(0) {
            Ok(parent) => Some(parent.tree()?),
            Err(_) => None,
        };
        let head_tree = self.repo.head()?.peel_to_tree()?;
        let diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&shelf_tree), None)?;
        let mut paths = BTreeSet::new();
        for delta in diff.deltas() {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    if self.content_policy.tracks(path)? {
                        paths.insert(path.to_path_buf());
                    }
                }
            }
        }

        let mut update = git2::build::TreeUpdateBuilder::new();
        for path in &paths {
            let key = path.to_string_lossy().replace('\\', "/");
            match shelf_tree.get_path(path) {
                Ok(entry) => {
                    update.upsert(key.as_str(), entry.id(), blob_file_mode(entry.filemode()));
                }
                Err(error) if error.code() == git2::ErrorCode::NotFound => {
                    update.remove(key.as_str());
                }
                Err(error) => return Err(error.into()),
            }
        }
        let tree = update.create_updated(&self.repo, &head_tree)?;
        Ok((tree, paths.into_iter().collect()))
    }

    /// Writes the shelf's content paths into the working tree without staging
    /// them, leaving every other path and the index alone.
    fn apply_shelf_paths(&self, shelf: &Commit<'_>) -> Result<()> {
        let (tree_id, paths) = self.shelf_target_tree(shelf)?;
        if paths.is_empty() {
            return Ok(());
        }
        let tree = self.repo.find_tree(tree_id)?;
        let mut checkout = CheckoutBuilder::new();
        checkout
            .force()
            .update_index(false)
            .disable_pathspec_match(true);
        for path in &paths {
            checkout.path(path.to_string_lossy().replace('\\', "/"));
        }
        let refused = RefCell::default();
        self.checkout_refusing_filters(checkout, &refused, &tree, |checkout| {
            self.repo.checkout_tree(tree.as_object(), Some(checkout))
        })
    }

    fn discard_preflight(
        &self,
        operation: &str,
        changed_files: Vec<ChangedFile>,
    ) -> Result<PreflightReport> {
        let hazards = self.discard_filtered_hazards(&changed_files)?;
        let mut report = discard_preflight_report(operation, changed_files);
        if !hazards.is_empty() {
            report.file_hazards.extend(hazards);
            report.can_proceed = false;
        }
        Ok(report)
    }

    /// Paths a discard would rewrite from `HEAD`.
    fn discard_write_paths(changed_files: &[ChangedFile]) -> Vec<PathBuf> {
        changed_files
            .iter()
            .filter(|changed| changed.kind != ChangeKind::Added)
            .map(|changed| changed.path.clone())
            .collect()
    }

    fn discard_changed_files(&self, changed_files: &[ChangedFile]) -> Result<()> {
        self.ensure_discard_unfiltered(changed_files, "discard")?;
        let mut paths_to_unstage = Vec::new();
        let mut paths_to_reset = Vec::new();
        let mut paths_to_remove_after_checkout = Vec::new();

        for changed in changed_files {
            match changed.kind {
                ChangeKind::Added => paths_to_unstage.push(changed.path.clone()),
                ChangeKind::Renamed => {
                    let target = match self.staged_rename_target_for_path(&changed.path)? {
                        Some(target) if self.content_policy.tracks(&target)? => Some(target),
                        _ => None,
                    };
                    // A target outside the content policy belongs to the developer.
                    if let Some(target) = target {
                        paths_to_reset.push(target.clone());
                        paths_to_remove_after_checkout.push(target);
                    }
                    paths_to_reset.push(changed.path.clone());
                }
                ChangeKind::Modified
                | ChangeKind::Deleted
                | ChangeKind::Conflicted
                | ChangeKind::TypeChanged => paths_to_reset.push(changed.path.clone()),
            }
        }

        // The whole index change is one write made before any file is touched,
        // and the file steps never write the index, so a busy index leaves the
        // workspace exactly as it was.
        let head_tree = self.repo.head()?.peel_to_tree()?;
        if !paths_to_unstage.is_empty() || !paths_to_reset.is_empty() {
            self.while_git_busy(|| {
                self.write_index_from_tree(&head_tree, &paths_to_reset, &paths_to_unstage)
            })?;
        }
        for path in &paths_to_unstage {
            remove_workspace_path_if_exists(&self.root.join(path))?;
        }

        if !paths_to_reset.is_empty() {
            let mut checkout = CheckoutBuilder::new();
            checkout.force().update_index(false);
            for path in &paths_to_reset {
                checkout.path(path);
            }
            let refused = RefCell::default();
            self.checkout_refusing_filters(checkout, &refused, &head_tree, |checkout| {
                self.repo
                    .checkout_tree(head_tree.as_object(), Some(checkout))
            })?;
        }

        for path in paths_to_remove_after_checkout {
            remove_workspace_path_if_exists(&self.root.join(path))?;
        }

        Ok(())
    }

    fn staged_rename_target_for_path(&self, path: &Path) -> Result<Option<PathBuf>> {
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true);

        let statuses = self.repo.statuses(Some(&mut options))?;
        for entry in statuses.iter() {
            if !entry.status().is_index_renamed() || entry.path().ok().map(Path::new) != Some(path)
            {
                continue;
            }

            let Some(target) = entry
                .head_to_index()
                .and_then(|delta| delta.new_file().path().map(PathBuf::from))
            else {
                return Ok(None);
            };

            if target != path {
                return Ok(Some(target));
            }
        }

        Ok(None)
    }

    fn shelve_changes_unchecked(
        &self,
        name: &str,
        profile: Option<&ContributorProfile>,
    ) -> Result<()> {
        let safe_name = validate_variation_name(name)?;
        let (author, committer) = self.rewrite_signatures(profile)?;
        let changed_files = self.changed_files_unchecked()?;
        let parent = self.repo.head()?.peel_to_commit()?;
        let (tree_id, _) =
            self.content_changes_tree(Some(&parent), &changed_files, "shelve_changes")?;
        self.ensure_discard_unfiltered(&changed_files, "shelve_changes")?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ShelveChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(safe_name.clone()),
            completed: false,
        })?;

        let tree = self.repo.find_tree(tree_id)?;
        let oid = self.repo.commit(
            None,
            &author,
            &committer,
            &format!("Shelved changes: {safe_name}"),
            &tree,
            &[&parent],
        )?;
        self.repo.reference(
            &format!("refs/draftline/shelves/{safe_name}"),
            oid,
            false,
            "shelve changes",
        )?;

        self.discard_changed_files(&changed_files)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ShelveChanges,
            original_variation: None,
            target: Some(safe_name),
            completed: true,
        })?;

        Ok(())
    }

    fn incoming_versions(&self, local: Oid, remote: Oid) -> Result<Vec<RemoteVersionSummary>> {
        let mut walk = self.repo.revwalk()?;
        walk.push(remote)?;
        walk.hide(local)?;

        let mut versions = Vec::new();
        for oid in walk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            versions.push(remote_summary_from_commit(&commit));
        }

        Ok(versions)
    }

    fn merge_input_for_status(&self, status: &SyncStatus) -> Result<IncomingMergeInput<'_>> {
        let local_oid = self.repo.head()?.peel_to_commit()?.id();
        let remote_ref = format!("refs/remotes/{}/{}", status.remote, status.variation);
        let remote_oid = self.repo.refname_to_id(&remote_ref)?;
        let base_oid = self.repo.merge_base(local_oid, remote_oid)?;

        Ok(IncomingMergeInput {
            local_commit: self.repo.find_commit(local_oid)?,
            remote_commit: self.repo.find_commit(remote_oid)?,
            base_commit: self.repo.find_commit(base_oid)?,
        })
    }

    /// The merge's base, local and remote trees, with the folder where the
    /// local side keeps it, so a move on one side doesn't drop the other
    /// side's edits. Refuses when the remote moved the folder somewhere else:
    /// the host moves it there too, then merges.
    ///
    /// Relocating replaces whatever the other trees hold where the folder is
    /// now (everything outside it, when it is now the root). Files the
    /// incoming side changed there would vanish from the merge, so they come
    /// back as developer conflicts instead.
    fn merge_trees_for(
        &self,
        input: &IncomingMergeInput<'_>,
    ) -> Result<([Tree<'_>; 3], Vec<PathBuf>)> {
        let mut folders = FolderChanges::new(&self.repo, self.scope());
        let ours = folders.folder_at(&input.local_commit)?;
        let base = folders.folder_at(&input.base_commit)?;
        let theirs = folders.folder_at(&input.remote_commit)?;
        if theirs != ours && theirs != base {
            return Err(DraftlineError::IncomingFolderElsewhere(theirs));
        }
        let mut dropped = Vec::new();
        if theirs != ours && !theirs.as_os_str().is_empty() {
            let remote = input.remote_commit.tree()?;
            let original = input.base_commit.tree()?;
            let blob =
                |tree: &Tree<'_>, path: &Path| tree.get_path(path).ok().map(|entry| entry.id());
            for path in tracked_paths_under(&remote, &ours)? {
                if !path.starts_with(&theirs)
                    && path != Path::new(FOLDER_MARKER)
                    && blob(&remote, &path) != blob(&original, &path)
                {
                    dropped.push(path);
                }
            }
        }
        let local = self.repo.find_tree(input.local_commit.tree_id())?;
        let marker = local.get_name(FOLDER_MARKER).map(|entry| entry.id());
        let fit = |commit: &Commit<'_>, folder: &Path| -> Result<Tree<'_>> {
            if folder == ours {
                return Ok(self.repo.find_tree(commit.tree_id())?);
            }
            let tree = self.relocated_tree(commit)?;
            let id = match marker {
                Some(marker) => {
                    let mut update = git2::build::TreeUpdateBuilder::new();
                    update.upsert(FOLDER_MARKER, marker, git2::FileMode::Blob);
                    update.create_updated(&self.repo, &tree)?
                }
                None => tree_without(&self.repo, &tree, FOLDER_MARKER)?,
            };
            Ok(self.repo.find_tree(id)?)
        };
        Ok((
            [
                fit(&input.base_commit, &base)?,
                local,
                fit(&input.remote_commit, &theirs)?,
            ],
            dropped,
        ))
    }

    fn plan_clean_merge(
        &self,
        base_tree: &Tree<'_>,
        local_tree: &Tree<'_>,
        remote_tree: &Tree<'_>,
    ) -> Result<CleanMergePlan> {
        let mut paths = BTreeSet::new();
        self.collect_tracked_tree_paths(base_tree, Path::new(""), &mut paths)?;
        self.collect_tracked_tree_paths(local_tree, Path::new(""), &mut paths)?;
        self.collect_tracked_tree_paths(remote_tree, Path::new(""), &mut paths)?;

        let mut files = Vec::new();
        let mut conflicts = Vec::new();

        for path in paths {
            let base = self.tree_blob_bytes(base_tree, &path)?;
            let ours = self.tree_blob_bytes(local_tree, &path)?;
            let theirs = self.tree_blob_bytes(remote_tree, &path)?;
            let merged =
                match merge_blob_contents(&self.resolver_registry, &path, base, ours, theirs) {
                    Ok(content) => content,
                    Err(conflict) => {
                        conflicts.push(*conflict);
                        continue;
                    }
                };

            if merged != self.tree_blob_bytes(local_tree, &path)? {
                files.push(MergeFileChange {
                    path,
                    content: merged,
                });
            }
        }

        Ok(CleanMergePlan { files, conflicts })
    }

    fn resolve_merge_plan(
        &self,
        plan: CleanMergePlan,
        resolutions: impl IntoIterator<Item = MergeConflictResolution>,
    ) -> Result<Vec<MergeFileChange>> {
        let mut resolved_files = plan
            .files
            .into_iter()
            .map(|file| (file.path, file.content))
            .collect::<BTreeMap<_, _>>();
        let mut resolutions_by_conflict = BTreeMap::new();

        for resolution in resolutions {
            let key = (resolution.path.clone(), resolution.field_path.clone());
            if resolutions_by_conflict.insert(key, resolution).is_some() {
                return Err(DraftlineError::InvalidMergeResolution(
                    "duplicate resolution for the same conflict".to_string(),
                ));
            }
        }

        for conflict in &plan.conflicts {
            let key = (conflict.path.clone(), conflict.field_path.clone());
            let Some(resolution) = resolutions_by_conflict.remove(&key) else {
                return Err(DraftlineError::InvalidMergeResolution(format!(
                    "missing resolution for `{}`",
                    conflict.path.display()
                )));
            };
            let content = resolved_conflict_content(conflict, &resolution.choice)?;
            if let Some(existing) = resolved_files.insert(conflict.path.clone(), content.clone()) {
                if existing != content {
                    return Err(DraftlineError::InvalidMergeResolution(format!(
                        "conflicting resolutions for `{}`",
                        conflict.path.display()
                    )));
                }
            }
        }

        if let Some(((path, field_path), _)) = resolutions_by_conflict.into_iter().next() {
            let field = field_path
                .map(|field| format!(" field `{field}`"))
                .unwrap_or_default();
            return Err(DraftlineError::InvalidMergeResolution(format!(
                "resolution does not match any conflict: `{}`{field}",
                path.display()
            )));
        }

        Ok(resolved_files
            .into_iter()
            .map(|(path, content)| MergeFileChange { path, content })
            .collect())
    }

    /// The whole-repository merge of `input` with every content path set to
    /// the planned (resolver-aware) result, built in memory so the real index
    /// is never touched. Conflicts outside the content policy are returned
    /// instead: they belong to a developer.
    fn merged_tree(
        &self,
        trees: &[Tree<'_>; 3],
        files: &[MergeFileChange],
    ) -> Result<std::result::Result<Oid, Vec<PathBuf>>> {
        let [base_tree, local_tree, remote_tree] = trees;
        let mut index = self
            .repo
            .merge_trees(base_tree, local_tree, remote_tree, None)?;

        let mut developer_conflicts = BTreeSet::new();
        let mut content_conflicts = BTreeSet::new();
        for conflict in index.conflicts()? {
            let conflict = conflict?;
            for entry in [&conflict.ancestor, &conflict.our, &conflict.their]
                .into_iter()
                .flatten()
            {
                let path = PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned());
                if self.content_policy.tracks(&path)? {
                    content_conflicts.insert(path);
                } else {
                    developer_conflicts.insert(path);
                }
            }
        }
        if !developer_conflicts.is_empty() {
            return Ok(Err(developer_conflicts.into_iter().collect()));
        }
        for path in &content_conflicts {
            index.conflict_remove(path)?;
        }
        let merged = self.repo.find_tree(index.write_tree_to(&self.repo)?)?;

        let mut paths = BTreeSet::new();
        for tree in [base_tree, local_tree, remote_tree, &merged] {
            self.collect_tracked_tree_paths(tree, Path::new(""), &mut paths)?;
        }
        let planned = files
            .iter()
            .map(|file| (file.path.as_path(), file.content.as_deref()))
            .collect::<BTreeMap<_, _>>();
        let mut update = git2::build::TreeUpdateBuilder::new();
        for path in &paths {
            let desired = match planned.get(path.as_path()) {
                Some(Some(content)) => Some(self.repo.blob(content)?),
                Some(None) => None,
                None => local_tree.get_path(path).ok().map(|entry| entry.id()),
            };
            let current = merged
                .get_path(path)
                .ok()
                .map(|entry| (entry.id(), entry.filemode()));
            let Some(oid) = desired else {
                if current.is_some() {
                    update.remove(path);
                }
                continue;
            };
            // Git's own mode merge when it settled one; otherwise keep a side's.
            let mode = current
                .map(|(_, mode)| mode)
                .or_else(|| local_tree.get_path(path).ok().map(|entry| entry.filemode()))
                .or_else(|| {
                    remote_tree
                        .get_path(path)
                        .ok()
                        .map(|entry| entry.filemode())
                })
                .unwrap_or(i32::from(git2::FileMode::Blob));
            let file_mode = [
                git2::FileMode::Blob,
                git2::FileMode::BlobExecutable,
                git2::FileMode::Link,
            ]
            .into_iter()
            .find(|candidate| i32::from(*candidate) == mode);
            match file_mode {
                Some(file_mode) if current != Some((oid, mode)) => {
                    update.upsert(path, oid, file_mode);
                }
                Some(_) => {}
                // A folder, submodule, or other non-file entry: not ours to merge.
                None => {
                    developer_conflicts.insert(path.clone());
                }
            }
        }
        if !developer_conflicts.is_empty() {
            return Ok(Err(developer_conflicts.into_iter().collect()));
        }
        Ok(Ok(update.create_updated(&self.repo, &merged)?))
    }

    #[allow(clippy::too_many_arguments)]
    fn write_incoming_merge_version(
        &self,
        variation: String,
        merge_input: &IncomingMergeInput<'_>,
        label: &str,
        tree: &Tree<'_>,
        files: Vec<MergeFileChange>,
        profile: Option<&ContributorProfile>,
    ) -> Result<MergeIncomingResult> {
        let (author, committer) = self.workspace_signatures(profile)?;
        let oid = self.repo.commit(
            None,
            &author,
            &committer,
            label,
            tree,
            &[&merge_input.local_commit, &merge_input.remote_commit],
        )?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::MergeIncoming,
            original_variation: Some(variation.clone()),
            target: Some(oid.to_string()),
            completed: false,
        })?;

        // Like `git merge`: files move from HEAD to the merged tree without
        // overwriting local edits, before the branch moves.
        self.checkout_or_keep_recovery(tree)?;
        self.repo.reference(
            &format!("refs/heads/{variation}"),
            oid,
            true,
            "merge_incoming",
        )?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::MergeIncoming,
            original_variation: None,
            target: Some(oid.to_string()),
            completed: true,
        })?;

        Ok(MergeIncomingResult {
            version: version_from_commit(&self.repo.find_commit(oid)?),
            merged_files: files.into_iter().map(|file| file.path).collect(),
        })
    }

    fn collect_tracked_tree_paths(
        &self,
        tree: &Tree<'_>,
        prefix: &Path,
        paths: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        for entry in tree.iter() {
            let Ok(name) = entry.name() else {
                continue;
            };
            let path = prefix.join(name);
            match entry.kind() {
                Some(ObjectType::Blob) if self.content_policy.tracks(&path)? => {
                    paths.insert(path);
                }
                Some(ObjectType::Tree) => {
                    let child = self.repo.find_tree(entry.id())?;
                    self.collect_tracked_tree_paths(&child, &path, paths)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn tree_blob_bytes(&self, tree: &Tree<'_>, path: &Path) -> Result<Option<Vec<u8>>> {
        let entry = match tree.get_path(path) {
            Ok(entry) => entry,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        if entry.kind() != Some(ObjectType::Blob) {
            return Ok(None);
        }

        Ok(Some(self.repo.find_blob(entry.id())?.content().to_vec()))
    }

    fn local_version_count(&self, local: Oid) -> Result<usize> {
        let mut walk = self.repo.revwalk()?;
        walk.push(local)?;
        Ok(walk.count())
    }

    fn workspace_signature(&self) -> Result<Signature<'_>> {
        match self.repo.signature() {
            Ok(signature) => Ok(signature),
            Err(_) => Ok(Signature::now("Draftline", "draftline@example.invalid")?),
        }
    }

    fn workspace_signatures(
        &self,
        profile: Option<&ContributorProfile>,
    ) -> Result<(Signature<'static>, Signature<'static>)> {
        if let Some(profile) = profile {
            return Ok((
                signature_from_contributor(&profile.author)?,
                signature_from_contributor(&profile.saved_by)?,
            ));
        }

        let signature = self.workspace_signature()?;
        let contributor = contributor_from_signature(&signature);
        let signature = signature_from_contributor(&contributor)?;
        Ok((signature.clone(), signature))
    }

    /// Like [`Self::workspace_signatures`], but without a profile it returns the exact
    /// Git config signature that these operations used before they became profile-aware.
    fn rewrite_signatures(
        &self,
        profile: Option<&ContributorProfile>,
    ) -> Result<(Signature<'static>, Signature<'static>)> {
        if profile.is_some() {
            return self.workspace_signatures(profile);
        }
        let signature = self.workspace_signature()?.to_owned();
        Ok((signature.clone(), signature))
    }

    fn versions_unchecked(&self) -> Result<Vec<Version>> {
        let mut walk = self.repo.revwalk()?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }
        let mut project = FolderChanges::new(&self.repo, self.scope());
        let mut versions = Vec::new();
        for oid in walk {
            let commit = self.repo.find_commit(oid?)?;
            if project.touches(&commit)? {
                versions.push(version_from_commit(&commit));
            }
        }
        Ok(versions)
    }

    fn lists_variation(&self, name: &str, current: Option<&str>) -> bool {
        match self.variation_namespace.as_deref() {
            Some(namespace) => {
                name.starts_with(namespace)
                    || Some(name) == current
                    || self.default_branch().as_deref() == Some(name)
            }
            None => true,
        }
    }

    /// The branch new work starts from: `origin/HEAD`'s target, else the first
    /// of `init.defaultBranch`, `main`, `master` and `trunk` that exists on
    /// `origin` (as last fetched), else the first that exists locally.
    fn default_branch(&self) -> Option<String> {
        self.default_branch_on("origin")
    }

    fn default_branch_on(&self, remote: &str) -> Option<String> {
        if let Some(head) = self.remote_head(remote) {
            return Some(head.head);
        }
        let prefix = format!("refs/remotes/{remote}/");
        if let Ok(reference) = self.repo.find_reference(&format!("{prefix}HEAD")) {
            if let Some(name) = reference
                .symbolic_target()
                .ok()
                .flatten()
                .and_then(|target| target.strip_prefix(prefix.as_str()))
            {
                return Some(name.to_string());
            }
        }
        let candidates = self.default_branch_candidates();
        candidates
            .iter()
            .find(|name| self.repo.find_reference(&format!("{prefix}{name}")).is_ok())
            .or_else(|| {
                candidates
                    .iter()
                    .find(|name| self.repo.find_branch(name, BranchType::Local).is_ok())
            })
            .cloned()
    }

    fn default_branch_candidates(&self) -> Vec<String> {
        let configured = self
            .repo
            .config()
            .and_then(|config| config.get_string("init.defaultBranch"))
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        let mut names = Vec::new();
        for name in configured
            .into_iter()
            .chain(["main", "master", "trunk"].map(str::to_string))
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Where a remote's advertised default branch is cached: one file per
    /// remote, shared by every worktree of the repository.
    fn remote_head_path(&self, remote: &str) -> PathBuf {
        let key: String = remote.bytes().map(|byte| format!("{byte:02x}")).collect();
        self.repo
            .commondir()
            .join("draftline")
            .join("remotes")
            .join(format!("{key}.json"))
    }

    fn remote_head(&self, remote: &str) -> Option<RemoteHead> {
        let url = self.repo.find_remote(remote).ok()?.url().ok()?.to_string();
        let bytes = fs::read(self.remote_head_path(remote)).ok()?;
        serde_json::from_slice::<RemoteHead>(&bytes)
            .ok()
            .filter(|entry| entry.url == url)
    }

    /// Records the default branch a remote advertised during the fetch that
    /// just ran, keeping the one it replaced so hosts can spot a rename.
    fn record_remote_head(&self, remote: &git2::Remote<'_>) -> Result<()> {
        let (Some(name), Ok(url), Ok(advertised)) = (
            remote.name().ok().flatten(),
            remote.url(),
            remote.default_branch(),
        ) else {
            return Ok(());
        };
        let Some(head) = advertised
            .as_str()
            .ok()
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .map(str::to_string)
        else {
            return Ok(());
        };
        let existing = self.remote_head(name);
        let known = existing
            .as_ref()
            .map(|entry| entry.head.clone())
            .or_else(|| {
                let prefix = format!("refs/remotes/{name}/");
                self.repo
                    .find_reference(&format!("{prefix}HEAD"))
                    .ok()?
                    .symbolic_target()
                    .ok()
                    .flatten()?
                    .strip_prefix(prefix.as_str())
                    .map(str::to_string)
            })
            .or_else(|| self.vanished_default_branch(remote, name, &head));
        let previous = match known {
            Some(old) if old != head => Some(old),
            _ => existing.as_ref().and_then(|entry| entry.previous.clone()),
        }
        .filter(|previous| *previous != head);
        if existing
            .as_ref()
            .is_some_and(|entry| entry.head == head && entry.previous == previous)
        {
            return Ok(());
        }
        let entry = RemoteHead {
            url: url.to_string(),
            head,
            previous,
        };
        let path = self.remote_head_path(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = unique_claim_temp(&path);
        fs::write(&temp, serde_json::to_vec_pretty(&entry)?)?;
        replace_file(&temp, &path)?;
        Ok(())
    }

    /// With no earlier record of the remote's default, the old name is a
    /// local default-named branch that tracks a branch of the same name the
    /// remote no longer advertises.
    fn vanished_default_branch(
        &self,
        remote: &git2::Remote<'_>,
        name: &str,
        head: &str,
    ) -> Option<String> {
        let advertised: BTreeSet<String> = remote
            .list()
            .ok()?
            .iter()
            .map(|entry| entry.name().to_string())
            .collect();
        self.default_branch_candidates().into_iter().find(|old| {
            let merge = format!("refs/heads/{old}");
            old != head
                && !advertised.contains(&merge)
                && self.repo.find_branch(old, BranchType::Local).is_ok()
                && self
                    .repo
                    .branch_upstream_remote(&merge)
                    .ok()
                    .and_then(|buf| buf.as_str().map(str::to_string).ok())
                    .as_deref()
                    == Some(name)
                && self
                    .repo
                    .branch_upstream_merge(&merge)
                    .ok()
                    .and_then(|buf| buf.as_str().map(str::to_string).ok())
                    .as_deref()
                    == Some(merge.as_str())
        })
    }

    /// The base's old name when the remote renamed its default branch and a
    /// local branch still carries the old name, so saves there can be moved.
    fn base_renamed_from(&self, remote: &str, base: &str) -> Option<String> {
        self.remote_head(remote)?
            .previous
            .filter(|previous| previous != base)
            .filter(|previous| self.repo.find_branch(previous, BranchType::Local).is_ok())
    }

    fn listed_variation_tip_oids(&self) -> Result<Vec<(VariationId, Oid)>> {
        let current = self.current_variation_unchecked().ok();
        Ok(self
            .local_variation_tip_oids()?
            .into_iter()
            .filter(|(variation, _)| self.lists_variation(variation.as_str(), current.as_deref()))
            .collect())
    }

    fn variations_unchecked(&self) -> Result<Vec<Variation>> {
        let current = self.current_variation_unchecked().ok();
        let mut paths = Vec::new();
        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };
            if !self.lists_variation(name, current.as_deref()) {
                continue;
            }
            let metadata = self.read_variation_metadata(name)?;
            paths.push(variation_from_name(
                name.to_string(),
                current.as_ref(),
                metadata,
            ));
        }
        paths.sort_by(|l, r| l.name.cmp(&r.name));
        Ok(paths)
    }

    fn local_variation_tip_oids(&self) -> Result<Vec<(VariationId, Oid)>> {
        let mut tips = Vec::new();
        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };
            if let Ok(tip) = branch.get().peel_to_commit() {
                tips.push((VariationId::from(name), tip.id()));
            }
        }
        tips.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(tips)
    }

    fn graph_remote_variations(&self, remote: Option<&str>) -> Result<Vec<RemoteVariation>> {
        if let Some(remote) = remote {
            self.repo.find_remote(remote)?;
            return self.remote_variations_unchecked(remote);
        }

        let mut variations = Vec::new();
        for remote in self.remotes_unchecked()? {
            variations.extend(self.remote_variations_unchecked(remote.name)?);
        }
        Ok(variations)
    }

    fn remote_variations_unchecked(&self, remote: impl AsRef<str>) -> Result<Vec<RemoteVariation>> {
        let remote = remote.as_ref().to_string();
        let prefix = format!("refs/remotes/{remote}/");
        let current = self.current_variation_unchecked().ok();
        let mut variations = Vec::new();

        for reference in self.repo.references_glob(&format!("{prefix}*"))? {
            let reference = reference?;
            if reference.kind() != Some(git2::ReferenceType::Direct) {
                continue;
            }

            let Ok(ref_name) = reference.name() else {
                continue;
            };
            let Some(name) = ref_name.strip_prefix(&prefix) else {
                continue;
            };
            if name == "HEAD"
                || name.ends_with("/HEAD")
                || name == "draftline"
                || name.starts_with("draftline/")
                || !self.lists_variation(name, current.as_deref())
            {
                continue;
            }

            let head_version = reference
                .peel_to_commit()
                .ok()
                .map(|commit| version_from_commit(&commit));
            variations.push(RemoteVariation {
                id: VariationId::from(name),
                name: name.to_string(),
                remote: remote.clone(),
                head_version,
            });
        }

        variations.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(variations)
    }

    fn reachable_oids(&self, tips: impl IntoIterator<Item = Oid>) -> Result<BTreeSet<Oid>> {
        let mut walk = self.repo.revwalk()?;
        for oid in tips {
            walk.push(oid)?;
        }
        walk.map(|oid| oid.map_err(DraftlineError::from)).collect()
    }

    fn version_is_reachable_from_local_variation(&self, oid: Oid) -> Result<bool> {
        Ok(self
            .reachable_oids(
                self.local_variation_tip_oids()?
                    .into_iter()
                    .map(|(_, tip)| tip),
            )?
            .contains(&oid))
    }

    fn build_workspace_graph_refs(
        &self,
        local_tips: &[(VariationId, Oid)],
        remote_variations: &[RemoteVariation],
        support_refs: &[SupportRef],
    ) -> Result<Vec<WorkspaceGraphRef>> {
        let current = self
            .current_variation_unchecked()
            .ok()
            .map(VariationId::from);
        let mut refs = Vec::new();

        for (variation, oid) in local_tips {
            let name = variation.as_str().to_string();
            let metadata = self.read_variation_metadata(&name)?;
            refs.push(WorkspaceGraphRef {
                id: format!("local-variation:{name}"),
                display_label: metadata.label.clone().unwrap_or_else(|| name.clone()),
                name: name.clone(),
                kind: WorkspaceGraphRefKind::LocalVariation,
                scope: WorkspaceGraphRefScope::Local,
                target: WorkspaceGraphNodeId::from(*oid),
                target_version: VersionId::from(*oid),
                remote: None,
                variation: Some(variation.clone()),
                metadata: Some(metadata),
                support_ref_kind: None,
                group: Some(name.clone()),
                is_current: current.as_ref() == Some(variation),
                is_user_facing: true,
                available_actions: if current.as_ref() == Some(variation) {
                    vec![
                        WorkspaceGraphAction::Preview,
                        WorkspaceGraphAction::CompareToCurrent,
                    ]
                } else {
                    vec![
                        WorkspaceGraphAction::Preview,
                        WorkspaceGraphAction::CompareToCurrent,
                        WorkspaceGraphAction::SwitchToVariation,
                    ]
                },
                action_hints: workspace_graph_ref_action_hints(
                    WorkspaceGraphRefKind::LocalVariation,
                    current.as_ref() == Some(variation),
                ),
            });
        }

        for remote_variation in remote_variations {
            let Some(head_version) = remote_variation.head_version.as_ref() else {
                continue;
            };
            let Ok(oid) = Oid::from_str(head_version.id().as_str()) else {
                continue;
            };
            refs.push(WorkspaceGraphRef {
                id: format!(
                    "remote-variation:{}:{}",
                    remote_variation.remote, remote_variation.name
                ),
                name: remote_variation.name.clone(),
                display_label: remote_variation.name.clone(),
                kind: WorkspaceGraphRefKind::RemoteVariation,
                scope: WorkspaceGraphRefScope::RemoteTracking,
                target: WorkspaceGraphNodeId::from(oid),
                target_version: head_version.id().clone(),
                remote: Some(remote_variation.remote.clone()),
                variation: Some(remote_variation.id.clone()),
                metadata: None,
                support_ref_kind: None,
                group: Some(format!(
                    "{}/{}",
                    remote_variation.remote, remote_variation.name
                )),
                is_current: false,
                is_user_facing: true,
                available_actions: vec![
                    WorkspaceGraphAction::Preview,
                    WorkspaceGraphAction::CompareToCurrent,
                    WorkspaceGraphAction::AdoptRemoteVariation,
                ],
                action_hints: workspace_graph_ref_action_hints(
                    WorkspaceGraphRefKind::RemoteVariation,
                    false,
                ),
            });
        }

        for support_ref in support_refs {
            let Ok(oid) = Oid::from_str(&support_ref.target_oid) else {
                continue;
            };
            refs.push(WorkspaceGraphRef {
                id: support_ref.id.clone(),
                name: support_ref.ref_name.clone(),
                display_label: support_ref
                    .source_variation
                    .clone()
                    .unwrap_or_else(|| support_ref.ref_name.clone()),
                kind: WorkspaceGraphRefKind::SupportRef,
                scope: match support_ref.scope {
                    SupportRefScope::Local => WorkspaceGraphRefScope::Local,
                    SupportRefScope::RemoteTracking => WorkspaceGraphRefScope::RemoteTracking,
                },
                target: WorkspaceGraphNodeId::from(oid),
                target_version: VersionId::from(oid),
                remote: remote_from_remote_tracking_ref(&support_ref.ref_name),
                variation: support_ref
                    .source_variation
                    .as_deref()
                    .map(VariationId::from),
                metadata: None,
                support_ref_kind: Some(support_ref.kind.clone()),
                group: support_ref.source_variation.clone(),
                is_current: false,
                is_user_facing: false,
                available_actions: vec![WorkspaceGraphAction::RestoreSupportRefAsVariation],
                action_hints: workspace_graph_ref_action_hints(
                    WorkspaceGraphRefKind::SupportRef,
                    false,
                ),
            });
        }

        refs.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(refs)
    }

    fn variation_tips_map(&self) -> Result<HashMap<Oid, Vec<VariationId>>> {
        let mut map: HashMap<Oid, Vec<VariationId>> = HashMap::new();
        for (variation, tip) in self.listed_variation_tip_oids()? {
            map.entry(tip).or_default().push(variation);
        }
        Ok(map)
    }
}

fn default_initial_variation_name() -> String {
    "main".to_string()
}

pub(crate) fn validate_variation_namespace(namespace: &str) -> Result<()> {
    let probe = format!("{namespace}x");
    if !namespace.ends_with('/') || !git2::Reference::is_valid_name(&format!("refs/heads/{probe}"))
    {
        return Err(DraftlineError::InvalidVariationName(namespace.to_string()));
    }
    validate_variation_name(&probe)?;
    Ok(())
}

fn validate_variation_name(name: &str) -> Result<String> {
    let trimmed = name.trim();

    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.contains("..")
        || trimmed.contains('\\')
        || trimmed == "draftline"
        || trimmed.starts_with("draftline/")
        || trimmed.chars().any(|character| {
            character.is_control() || matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[')
        })
    {
        return Err(DraftlineError::InvalidVariationName(name.to_string()));
    }

    Ok(trimmed.to_string())
}

fn normalize_optional_metadata(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn variation_metadata_key(variation: &str, name: &str) -> String {
    format!("branch.{variation}.draftline-{name}")
}

fn archive_ref(namespace: &str, variation: &str, operation_id: &str) -> String {
    format!("refs/draftline/{namespace}/{variation}/{operation_id}")
}

fn read_optional_config(config: &git2::Config, key: &str) -> Result<Option<String>> {
    match config.get_string(key) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_optional_config(config: &mut git2::Config, key: &str, value: Option<&str>) -> Result<()> {
    match value.and_then(|value| normalize_optional_metadata(value.to_string())) {
        Some(value) => config.set_str(key, &value)?,
        None => {
            if let Err(error) = config.remove(key) {
                if error.code() != git2::ErrorCode::NotFound {
                    return Err(error.into());
                }
            }
        }
    }

    Ok(())
}

fn variation_from_name(
    name: String,
    current: Option<&String>,
    metadata: VariationMetadata,
) -> Variation {
    Variation {
        id: VariationId::from(name.clone()),
        metadata,
        is_current: current.map(|current| current == &name).unwrap_or(false),
        name,
    }
}

fn status_to_change_kind(status: Status) -> ChangeKind {
    if status.is_conflicted() {
        ChangeKind::Conflicted
    } else if status.is_wt_new() || status.is_index_new() {
        ChangeKind::Added
    } else if status.is_wt_deleted() || status.is_index_deleted() {
        ChangeKind::Deleted
    } else if status.is_wt_renamed() || status.is_index_renamed() {
        ChangeKind::Renamed
    } else if status.is_wt_typechange() || status.is_index_typechange() {
        ChangeKind::TypeChanged
    } else {
        ChangeKind::Modified
    }
}

fn selected_files_preflight_report(
    operation: impl Into<String>,
    will_write_files: bool,
    dirty_files: Vec<ChangedFile>,
) -> PreflightReport {
    let untracked_assets = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Added)
        .map(|file| file.path.clone())
        .collect();
    let unresolved_conflicts = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Conflicted)
        .map(|file| file.path.clone())
        .collect();
    let large_files = dirty_files
        .iter()
        .filter(|file| file.is_large)
        .map(|file| file.path.clone())
        .collect();
    let binary_files = dirty_files
        .iter()
        .filter(|file| file.is_binary)
        .map(|file| file.path.clone())
        .collect();
    let can_proceed = !dirty_files.is_empty();

    PreflightReport {
        operation: operation.into(),
        will_write_files,
        dirty_files,
        file_hazards: Vec::new(),
        untracked_assets,
        unresolved_conflicts,
        large_files,
        binary_files,
        variation_divergence: None,
        can_proceed,
    }
}

fn preflight_report(
    operation: impl Into<String>,
    will_write_files: bool,
    dirty_files: Vec<ChangedFile>,
    file_hazards: Vec<FileHazard>,
    variation_divergence: Option<String>,
) -> PreflightReport {
    let untracked_assets = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Added)
        .map(|file| file.path.clone())
        .collect();
    let unresolved_conflicts = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Conflicted)
        .map(|file| file.path.clone())
        .collect();
    let large_files = dirty_files
        .iter()
        .filter(|file| file.is_large)
        .map(|file| file.path.clone())
        .collect();
    let binary_files = dirty_files
        .iter()
        .filter(|file| file.is_binary)
        .map(|file| file.path.clone())
        .collect();
    let can_proceed = dirty_files.is_empty() && file_hazards.is_empty();

    PreflightReport {
        operation: operation.into(),
        will_write_files,
        dirty_files,
        file_hazards,
        untracked_assets,
        unresolved_conflicts,
        large_files,
        binary_files,
        variation_divergence,
        can_proceed,
    }
}

/// Whether Git would run a filter driver (for example Git LFS) for `path`.
fn needs_git_filter(repo: &Repository, path: &Path, flags: git2::AttrCheckFlags) -> Result<bool> {
    Ok(git_filter_driver(repo, path, flags)?.is_some())
}

/// Whether `config` gives `driver` the commands Git runs: a long-running
/// `process`, or both `clean` and `smudge`.
fn driver_configured(config: &git2::Config, driver: &str) -> bool {
    let command = |key: &str| {
        config
            .get_string(&format!("filter.{driver}.{key}"))
            .is_ok_and(|value| !value.trim().is_empty())
    };
    !driver.is_empty() && (command("process") || (command("clean") && command("smudge")))
}

/// Which filter drivers Draftline can run, through the `git` executable:
/// those the repository configures, when `git` is available.
struct FilterRunner {
    config: git2::Config,
    git: bool,
}

impl FilterRunner {
    fn of(repo: &Repository) -> Result<Self> {
        Ok(Self {
            config: repo.config()?.snapshot()?,
            git: git_cli::available(),
        })
    }

    fn runs(&self, driver: &str) -> bool {
        self.git && driver_configured(&self.config, driver)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterUse {
    None,
    Runs,
    Refused,
}

/// What a checkout's planning found: paths to refuse, and paths to rewrite
/// through their filters once it has written them.
#[derive(Default)]
struct FilterPlan {
    refused: Vec<PathBuf>,
    smudge: Vec<PathBuf>,
}

/// The filter driver Git would run for `path`, if any.
fn git_filter_driver(
    repo: &Repository,
    path: &Path,
    flags: git2::AttrCheckFlags,
) -> Result<Option<String>> {
    let filter = repo.get_attr(path, "filter", flags)?;
    Ok(match git2::AttrValue::from_string(filter) {
        git2::AttrValue::True => Some(String::new()),
        git2::AttrValue::String(driver) => Some(driver.to_string()),
        _ => None,
    })
}

/// Whether the file is a Git LFS pointer (spec v1: at most 1024 bytes, a
/// `version` line first, then `oid` and `size` lines), meaning its content
/// was never downloaded.
fn is_lfs_pointer_file(path: &Path) -> bool {
    const MAX: u64 = 1024;
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > MAX {
        return false;
    }
    fs::read(path).is_ok_and(|bytes| is_lfs_pointer(&bytes))
}

fn is_lfs_pointer(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut lines = text.lines();
    let versioned = lines.next().is_some_and(|line| {
        line == "version https://git-lfs.github.com/spec/v1"
            || line == "version https://hawser.github.com/spec/v1"
    });
    let rest: Vec<&str> = lines.collect();
    versioned
        && rest.iter().any(|line| line.starts_with("oid sha256:"))
        && rest.iter().any(|line| line.starts_with("size "))
}

fn discard_preflight_report(
    operation: impl Into<String>,
    dirty_files: Vec<ChangedFile>,
) -> PreflightReport {
    let untracked_assets = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Added)
        .map(|file| file.path.clone())
        .collect();
    let unresolved_conflicts = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Conflicted)
        .map(|file| file.path.clone())
        .collect();
    let large_files = dirty_files
        .iter()
        .filter(|file| file.is_large)
        .map(|file| file.path.clone())
        .collect();
    let binary_files = dirty_files
        .iter()
        .filter(|file| file.is_binary)
        .map(|file| file.path.clone())
        .collect();

    PreflightReport {
        operation: operation.into(),
        will_write_files: true,
        dirty_files,
        untracked_assets,
        unresolved_conflicts,
        large_files,
        binary_files,
        file_hazards: Vec::new(),
        variation_divergence: None,
        can_proceed: true,
    }
}

fn file_is_large(path: &Path, threshold: u64) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > threshold),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn file_is_binary(path: &Path) -> Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    Ok(bytes.contains(&0) || std::str::from_utf8(&bytes).is_err())
}

/// libgit2 shares one in-memory index per repository, so a failed write
/// would leave later reads seeing changes that never reached disk.
fn write_index_or_reload(index: &mut git2::Index) -> Result<()> {
    if let Err(error) = index.write() {
        let _ = index.read(true);
        return Err(error.into());
    }
    Ok(())
}

/// Creates `root/folder` one level at a time, refusing to pass through a
/// file, symlink, or junction, so a checked-out link can't place it outside
/// `root`.
fn create_folder_within(root: &Path, folder: &Path) -> Result<PathBuf> {
    create_folder_tracking(root, folder, &mut Vec::new())
}

/// [`create_folder_within`], recording each folder it creates in `created`.
fn create_folder_tracking(
    root: &Path,
    folder: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<PathBuf> {
    let canonical_root = fs::canonicalize(root)?;
    let mut path = root.to_path_buf();
    for component in folder.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(not_a_plain_folder(&path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
                created.push(path.clone());
                // A parent swapped for a link after it was checked would put
                // the new folder outside the repository. Deleting by path
                // would follow that link too, so leave the empty folders and
                // forget them rather than risk removing someone else's.
                let inside = fs::canonicalize(&path)
                    .map(|resolved| resolved.starts_with(&canonical_root))
                    .unwrap_or(false);
                if !inside {
                    created.clear();
                    return Err(not_a_plain_folder(&path));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

/// Whether `on_disk` is the start of `content` as Git would write it, with
/// or without CRLF line endings: what a write cut short leaves behind.
fn is_start_of(on_disk: &[u8], content: &[u8]) -> bool {
    if content.starts_with(on_disk) {
        return true;
    }
    let mut converted = Vec::with_capacity(content.len() + content.len() / 16);
    let mut previous = 0u8;
    for &byte in content {
        if byte == b'\n' && previous != b'\r' {
            converted.push(b'\r');
        }
        converted.push(byte);
        previous = byte;
    }
    converted.starts_with(on_disk)
}

/// Refuses a path that exists but isn't a real directory (a file, symlink, or junction).
fn ensure_plain_or_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(not_a_plain_folder(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Whether the file at `path` has an executable bit set (never on Windows).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Atomically replaces `path` with `temp`, removing `temp` on failure. On
/// Windows, a scanner or indexer briefly holding `path` makes the rename fail
/// with access denied, so it retries for a moment as Git for Windows does.
fn replace_file(temp: &Path, path: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match fs::rename(temp, path) {
            Ok(()) => return Ok(()),
            Err(error)
                if cfg!(windows)
                    && error.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < 10 =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
            }
            Err(error) => {
                let _ = fs::remove_file(temp);
                return Err(error);
            }
        }
    }
}

fn not_a_plain_folder(path: &Path) -> DraftlineError {
    DraftlineError::InvalidFolderMarker(format!("{} is not a plain folder", path.display()))
}

fn remove_workspace_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }

    Ok(())
}

fn serialize_workspace_path<S>(path: &Path, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let value = path
        .iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    serializer.serialize_str(&value)
}

fn collect_preview_files(
    repo: &Repository,
    tree: &Tree<'_>,
    prefix: &Path,
    files: &mut Vec<PreviewFile>,
    content_policy: &ContentPolicy,
) -> Result<()> {
    for entry in tree.iter() {
        let Ok(name) = entry.name() else {
            continue;
        };
        let path = prefix.join(name);

        match entry.kind() {
            Some(ObjectType::Blob) => {
                if !content_policy.tracks(&path)? {
                    continue;
                }

                let blob = repo.find_blob(entry.id())?;
                let content = std::str::from_utf8(blob.content())
                    .ok()
                    .map(ToString::to_string);
                files.push(PreviewFile {
                    path,
                    is_binary: content.is_none(),
                    content,
                });
            }
            Some(ObjectType::Tree) => {
                let child = repo.find_tree(entry.id())?;
                collect_preview_files(repo, &child, &path, files, content_policy)?;
            }
            _ => {}
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(())
}

fn contributor_from_signature(signature: &git2::Signature<'_>) -> Contributor {
    Contributor {
        name: signature.name().unwrap_or("Unknown").to_string(),
        email: signature.email().ok().map(ToString::to_string),
    }
}

fn signature_from_contributor(contributor: &Contributor) -> Result<Signature<'static>> {
    let name = contributor.name.trim();
    if name.is_empty() {
        return Err(DraftlineError::InvalidContributorIdentity(
            "contributor name is required".to_string(),
        ));
    }

    let email = contributor
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .unwrap_or("draftline@example.invalid");

    Ok(Signature::now(name, email)?)
}

/// Tells which commits change Draftline's folder, like `git log -- <folder>`,
/// following the folder through moves: each commit's own [`FOLDER_MARKER`]
/// says where the folder was. Commits from before the marker existed use the
/// first folder one recorded.
struct FolderChanges<'r> {
    repo: &'r Repository,
    current: PathBuf,
    first: Option<PathBuf>,
    /// Per commit: where the folder was, and its tree there.
    seen: HashMap<Oid, (PathBuf, Option<Oid>)>,
}

impl<'r> FolderChanges<'r> {
    fn new(repo: &'r Repository, current: &Path) -> Self {
        Self {
            repo,
            current: current.to_path_buf(),
            first: None,
            seen: HashMap::new(),
        }
    }

    /// Where the folder was in `commit`.
    fn folder_at(&mut self, commit: &Commit<'_>) -> Result<PathBuf> {
        Ok(self.at(commit)?.0)
    }

    fn at(&mut self, commit: &Commit<'_>) -> Result<(PathBuf, Option<Oid>)> {
        if let Some(cached) = self.seen.get(&commit.id()) {
            return Ok(cached.clone());
        }
        let tree = commit.tree()?;
        let folder = match tree_marker_folder(self.repo, &tree) {
            Some(folder) => folder,
            None => self.first_folder()?,
        };
        let id = if folder.as_os_str().is_empty() {
            Some(tree.id())
        } else {
            optional_tree_entry(&tree, &folder)?
        };
        self.seen.insert(commit.id(), (folder.clone(), id));
        Ok((folder, id))
    }

    /// The folder commits from before the marker existed used: the oldest
    /// marker on HEAD's history, or where the folder was moved from when that
    /// marker came with a move. Else the current folder.
    fn first_folder(&mut self) -> Result<PathBuf> {
        if let Some(first) = &self.first {
            return Ok(first.clone());
        }
        let mut first = self.current.clone();
        let mut walk = self.repo.revwalk()?;
        if walk.push_head().is_ok() {
            walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
            for oid in walk {
                let commit = self.repo.find_commit(oid?)?;
                if let Some(folder) = tree_marker_folder(self.repo, &commit.tree()?) {
                    first = moved_from(&commit).unwrap_or(folder);
                    break;
                }
            }
        }
        self.first = Some(first.clone());
        Ok(first)
    }

    /// A commit touches the folder unless some parent has the same folder in
    /// the same place; a merge that only takes one side's folder does not,
    /// and a move does. Every commit touches a folder that is the root.
    fn touches(&mut self, commit: &Commit<'_>) -> Result<bool> {
        let own = self.at(commit)?;
        if own.0.as_os_str().is_empty() {
            return Ok(true);
        }
        if commit.parent_count() == 0 {
            return Ok(own.1.is_some());
        }
        for parent in commit.parents() {
            if self.at(&parent)? == own {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// `tree` without the entry at `path`, if it has one.
/// A root folder's tree split into its content and the
/// [`REPOSITORY_FILES`] beside it, the marker in neither.
fn split_root(repo: &Repository, tree: &Tree<'_>) -> Result<(Oid, Oid)> {
    let mut content = git2::build::TreeUpdateBuilder::new();
    let mut repository = git2::build::TreeUpdateBuilder::new();
    for entry in tree.iter() {
        let name = String::from_utf8_lossy(entry.name_bytes()).into_owned();
        if is_repository_file(entry.name_bytes()) {
            content.remove(&name);
        }
        if !is_repository_file(entry.name_bytes()) || name == FOLDER_MARKER {
            repository.remove(&name);
        }
    }
    Ok((
        content.create_updated(repo, tree)?,
        repository.create_updated(repo, tree)?,
    ))
}

fn tree_without(repo: &Repository, tree: &Tree<'_>, path: &str) -> Result<Oid> {
    if tree.get_path(Path::new(path)).is_err() {
        return Ok(tree.id());
    }
    let mut update = git2::build::TreeUpdateBuilder::new();
    update.remove(path);
    Ok(update.create_updated(repo, tree)?)
}

/// Where a folder move's commit says the folder was.
fn moved_from(commit: &Commit<'_>) -> Option<PathBuf> {
    let prefix = format!("{MOVED_FROM_TRAILER}: ");
    let value = std::str::from_utf8(commit.message_bytes())
        .ok()?
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(&prefix))?
        .trim();
    if value == "." {
        return Some(PathBuf::new());
    }
    canonical_folder(value).filter(|folder| !folder.as_os_str().is_empty())
}

/// The folder the checked-out commit's [`FOLDER_MARKER`] names, if any.
fn head_marker_folder(repo: &Repository) -> Option<PathBuf> {
    let tree = repo.head().ok()?.peel_to_tree().ok()?;
    tree_marker_folder(repo, &tree)
}

/// The folder `tree`'s own [`FOLDER_MARKER`] names, if it is a valid one.
fn tree_marker_folder(repo: &Repository, tree: &Tree<'_>) -> Option<PathBuf> {
    let blob = repo.find_blob(tree.get_name(FOLDER_MARKER)?.id()).ok()?;
    let marker = serde_json::from_slice::<FolderMarker>(blob.content()).ok()?;
    if marker.format != 1 {
        return None;
    }
    if marker.folder == "." {
        return Some(PathBuf::new());
    }
    canonical_folder(&marker.folder).filter(|folder| !folder.as_os_str().is_empty())
}

fn version_from_commit(commit: &Commit<'_>) -> Version {
    Version {
        id: VersionId::from(commit.id()),
        label: commit
            .summary()
            .ok()
            .flatten()
            .unwrap_or_default()
            .to_string(),
        author: contributor_from_signature(&commit.author()),
        saved_by: contributor_from_signature(&commit.committer()),
        time_seconds: commit.time().seconds(),
    }
}

fn history_entry_from_commit(
    commit: &Commit<'_>,
    touches_folder: bool,
    tips: &HashMap<Oid, Vec<VariationId>>,
    head_oid: Option<Oid>,
) -> Result<HistoryEntry> {
    let oid = commit.id();
    let version = version_from_commit(commit);
    let mut variation_tips = tips.get(&oid).cloned().unwrap_or_default();
    variation_tips.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let is_head = head_oid == Some(oid);
    let parent_ids = commit.parent_ids().map(VersionId::from).collect();
    Ok(HistoryEntry {
        version,
        variation_tips,
        is_head,
        parent_ids,
        touches_folder,
    })
}

/// The common ancestor of `left` and `right` nearest to `right`, walking the
/// graph's parent links breadth-first.
fn workspace_graph_nearest_common_ancestor(
    parents: &HashMap<WorkspaceGraphNodeId, Vec<WorkspaceGraphNodeId>>,
    left: &WorkspaceGraphNodeId,
    right: &WorkspaceGraphNodeId,
) -> Option<WorkspaceGraphNodeId> {
    let ancestors = |start: &WorkspaceGraphNodeId| {
        let mut seen = HashSet::new();
        let mut order = Vec::new();
        let mut queue = VecDeque::from([start.clone()]);
        while let Some(id) = queue.pop_front() {
            if seen.insert(id.clone()) {
                queue.extend(parents.get(&id).into_iter().flatten().cloned());
                order.push(id);
            }
        }
        order
    };
    let left_ancestors = ancestors(left).into_iter().collect::<HashSet<_>>();
    ancestors(right)
        .into_iter()
        .find(|id| left_ancestors.contains(id))
}

fn workspace_graph_node_from_commit(
    commit: &Commit<'_>,
    history_entry: HistoryEntry,
    local_reachable: &BTreeSet<Oid>,
    remote_reachable: &BTreeSet<Oid>,
    support_reachable: &BTreeSet<Oid>,
    topo_index: usize,
) -> Result<WorkspaceGraphNode> {
    let oid = commit.id();
    let reachable_from_local_variation = local_reachable.contains(&oid);
    let reachable_from_remote_variation = remote_reachable.contains(&oid);
    let reachable_from_support_ref = support_reachable.contains(&oid);
    let kind = if reachable_from_local_variation {
        WorkspaceGraphNodeKind::Normal
    } else if reachable_from_support_ref {
        WorkspaceGraphNodeKind::SupportRefOnly
    } else {
        WorkspaceGraphNodeKind::RemoteOnly
    };
    let mut available_actions = vec![
        WorkspaceGraphAction::Preview,
        WorkspaceGraphAction::CompareToCurrent,
    ];
    if matches!(kind, WorkspaceGraphNodeKind::Normal) {
        available_actions.extend([
            WorkspaceGraphAction::RestoreAsNewSave,
            WorkspaceGraphAction::CreateVariationFromHere,
        ]);
    }
    let display_label = history_entry.version.label.clone();
    let is_head = history_entry.is_head;
    let is_merge = history_entry.parent_ids.len() > 1;
    let is_tip = !history_entry.variation_tips.is_empty();
    let parent_ids = history_entry
        .parent_ids
        .iter()
        .filter_map(|parent| Oid::from_str(parent.as_str()).ok())
        .map(WorkspaceGraphNodeId::from)
        .collect();

    Ok(WorkspaceGraphNode {
        id: WorkspaceGraphNodeId::from(oid),
        version: history_entry.version,
        parent_ids,
        parent_version_ids: history_entry.parent_ids,
        variation_tips: history_entry.variation_tips,
        touches_folder: history_entry.touches_folder,
        is_head,
        is_current: is_head,
        is_tip,
        is_merge,
        is_branch_point: false,
        child_ids: Vec::new(),
        child_count: 0,
        kind: kind.clone(),
        reachable_from_local_variation,
        reachable_from_remote_variation,
        reachable_from_support_ref,
        topo_index,
        layout: WorkspaceGraphLayoutHint {
            lane: 0,
            row: topo_index,
            group: None,
            display_label,
        },
        boundary: WorkspaceGraphBoundary::default(),
        available_actions,
        action_hints: workspace_graph_node_action_hints(&kind),
    })
}

fn workspace_graph_snapshot_id(
    current_variation: &Option<VariationId>,
    current_version: &Option<VersionId>,
    dirty: &DirtySummary,
    nodes: &[WorkspaceGraphNode],
    refs: &[WorkspaceGraphRef],
    total_nodes: usize,
) -> String {
    let mut input = String::new();
    input.push_str("current_variation=");
    input.push_str(
        current_variation
            .as_ref()
            .map(VariationId::as_str)
            .unwrap_or("none"),
    );
    input.push_str("|current_version=");
    input.push_str(
        current_version
            .as_ref()
            .map(VersionId::as_str)
            .unwrap_or("none"),
    );
    input.push_str("|dirty=");
    input.push_str(if dirty.is_dirty { "true" } else { "false" });
    for file in &dirty.files {
        input.push_str("|dirty_file=");
        input.push_str(&file.path.to_string_lossy());
        input.push(':');
        input.push_str(change_kind_token(&file.kind));
    }
    input.push_str("|total=");
    input.push_str(&total_nodes.to_string());
    for node in nodes {
        input.push_str("|node=");
        input.push_str(node.id.as_str());
        input.push(':');
        input.push_str(workspace_graph_node_kind_token(&node.kind));
        for parent in &node.parent_ids {
            input.push('<');
            input.push_str(parent.as_str());
        }
    }
    for graph_ref in refs {
        input.push_str("|ref=");
        input.push_str(&graph_ref.id);
        input.push(':');
        input.push_str(workspace_graph_ref_kind_token(&graph_ref.kind));
        input.push(':');
        input.push_str(workspace_graph_ref_scope_token(&graph_ref.scope));
        input.push('@');
        input.push_str(graph_ref.target.as_str());
    }
    format!("graph-{:016x}", stable_fnv1a64(input.as_bytes()))
}

fn workspace_graph_fingerprint(
    current_variation: &Option<VariationId>,
    current_version: &Option<VersionId>,
    dirty: &DirtySummary,
    refs: &[WorkspaceGraphRef],
) -> String {
    let mut input = String::new();
    input.push_str("current_variation=");
    input.push_str(
        current_variation
            .as_ref()
            .map(VariationId::as_str)
            .unwrap_or("none"),
    );
    input.push_str("|current_version=");
    input.push_str(
        current_version
            .as_ref()
            .map(VersionId::as_str)
            .unwrap_or("none"),
    );
    input.push_str("|dirty=");
    input.push_str(if dirty.is_dirty { "true" } else { "false" });
    for graph_ref in refs {
        input.push_str("|ref=");
        input.push_str(&graph_ref.id);
        input.push('@');
        input.push_str(graph_ref.target.as_str());
    }
    format!(
        "graph-fingerprint-{:016x}",
        stable_fnv1a64(input.as_bytes())
    )
}

fn workspace_graph_action_hint(
    action: WorkspaceGraphAction,
    enabled: bool,
    disabled_reason: Option<&str>,
) -> WorkspaceGraphActionHint {
    let switches_workspace = matches!(action, WorkspaceGraphAction::SwitchToVariation);
    let creates_version = matches!(action, WorkspaceGraphAction::RestoreAsNewSave);
    let command = match action {
        WorkspaceGraphAction::Preview => "preview_version",
        WorkspaceGraphAction::CompareToCurrent => "diff_version_to_workspace",
        WorkspaceGraphAction::RestoreAsNewSave => "restore_version_as_new_save",
        WorkspaceGraphAction::CreateVariationFromHere => "create_variation_from_version",
        WorkspaceGraphAction::SwitchToVariation => "switch_variation",
        WorkspaceGraphAction::AdoptRemoteVariation => "adopt_remote_variation",
        WorkspaceGraphAction::RestoreSupportRefAsVariation => "restore_support_ref_as_variation",
    }
    .to_string();
    WorkspaceGraphActionHint {
        action,
        enabled,
        command,
        disabled_reason: disabled_reason.map(ToString::to_string),
        destructive: false,
        switches_workspace,
        creates_version,
    }
}

fn workspace_graph_node_action_hints(
    kind: &WorkspaceGraphNodeKind,
) -> Vec<WorkspaceGraphActionHint> {
    let local = matches!(kind, WorkspaceGraphNodeKind::Normal);
    let disabled_reason = match kind {
        WorkspaceGraphNodeKind::Normal => None,
        WorkspaceGraphNodeKind::RemoteOnly => {
            Some("version is remote-only; adopt the remote variation before local mutations")
        }
        WorkspaceGraphNodeKind::SupportRefOnly => Some(
            "version is only reachable through a support ref; restore the support ref before local mutations",
        ),
    };
    vec![
        workspace_graph_action_hint(WorkspaceGraphAction::Preview, true, None),
        workspace_graph_action_hint(WorkspaceGraphAction::CompareToCurrent, true, None),
        workspace_graph_action_hint(
            WorkspaceGraphAction::RestoreAsNewSave,
            local,
            disabled_reason,
        ),
        workspace_graph_action_hint(
            WorkspaceGraphAction::CreateVariationFromHere,
            local,
            disabled_reason,
        ),
    ]
}

fn workspace_graph_ref_action_hints(
    kind: WorkspaceGraphRefKind,
    is_current: bool,
) -> Vec<WorkspaceGraphActionHint> {
    let switch_reason = is_current.then_some("variation is already current");
    match kind {
        WorkspaceGraphRefKind::LocalVariation => vec![
            workspace_graph_action_hint(WorkspaceGraphAction::Preview, true, None),
            workspace_graph_action_hint(WorkspaceGraphAction::CompareToCurrent, true, None),
            workspace_graph_action_hint(
                WorkspaceGraphAction::SwitchToVariation,
                !is_current,
                switch_reason,
            ),
        ],
        WorkspaceGraphRefKind::RemoteVariation => vec![
            workspace_graph_action_hint(WorkspaceGraphAction::Preview, true, None),
            workspace_graph_action_hint(WorkspaceGraphAction::CompareToCurrent, true, None),
            workspace_graph_action_hint(WorkspaceGraphAction::AdoptRemoteVariation, true, None),
        ],
        WorkspaceGraphRefKind::SupportRef => vec![workspace_graph_action_hint(
            WorkspaceGraphAction::RestoreSupportRefAsVariation,
            true,
            None,
        )],
    }
}

fn annotate_workspace_graph(graph: &mut WorkspaceGraph) {
    let node_ids = graph
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let mut computed_child_ids: HashMap<WorkspaceGraphNodeId, Vec<WorkspaceGraphNodeId>> =
        HashMap::new();
    for node in &graph.nodes {
        for parent in &node.parent_ids {
            computed_child_ids
                .entry(parent.clone())
                .or_default()
                .push(node.id.clone());
        }
    }
    for children in computed_child_ids.values_mut() {
        children.sort();
    }
    let lane_by_ref = graph
        .refs
        .iter()
        .enumerate()
        .filter_map(|(index, graph_ref)| {
            graph_ref
                .variation
                .as_ref()
                .map(|variation| (variation.clone(), index))
        })
        .collect::<HashMap<_, _>>();

    for node in &mut graph.nodes {
        let previous_child_ids = node.child_ids.clone();
        let children = computed_child_ids
            .get(&node.id)
            .cloned()
            .unwrap_or_default();
        let missing_parent_ids = node
            .parent_ids
            .iter()
            .filter(|parent| !node_ids.contains(*parent))
            .cloned()
            .collect::<Vec<_>>();
        let missing_child_ids = previous_child_ids
            .iter()
            .filter(|child| !node_ids.contains(*child))
            .cloned()
            .collect::<Vec<_>>();
        let primary_variation = node.variation_tips.first().cloned();
        let lane = primary_variation
            .as_ref()
            .and_then(|variation| lane_by_ref.get(variation).copied())
            .unwrap_or_default();
        node.child_count = children.len();
        node.child_ids = children;
        node.is_branch_point = node.child_count > 1;
        node.is_tip = !node.variation_tips.is_empty();
        node.is_merge = node.parent_ids.len() > 1;
        node.is_current = graph.current_version.as_ref() == Some(node.version.id());
        node.layout = WorkspaceGraphLayoutHint {
            lane,
            row: node.topo_index,
            group: primary_variation.map(|variation| variation.as_str().to_string()),
            display_label: node.version.label.clone(),
        };
        node.boundary = WorkspaceGraphBoundary {
            hidden_parent_count: missing_parent_ids.len(),
            hidden_child_count: missing_child_ids.len(),
            missing_parent_ids,
            missing_child_ids,
        };
        node.action_hints = workspace_graph_node_action_hints(&node.kind);
    }
}

fn apply_workspace_graph_node_filter(
    graph: &mut WorkspaceGraph,
    keep: BTreeSet<WorkspaceGraphNodeId>,
    original_node_count: usize,
) {
    graph.nodes.retain(|node| keep.contains(&node.id));
    graph
        .refs
        .retain(|graph_ref| keep.contains(&graph_ref.target));
    graph.was_pruned = graph.nodes.len() < original_node_count || graph_has_missing_parent(graph);
    graph.has_more = false;
    graph.next_cursor = None;
    annotate_workspace_graph(graph);
    graph.snapshot_id = workspace_graph_snapshot_id(
        &graph.current_variation,
        &graph.current_version,
        &graph.dirty,
        &graph.nodes,
        &graph.refs,
        graph.nodes.len(),
    );
}

fn workspace_graph_node_matches(node: &WorkspaceGraphNode, query: &str) -> bool {
    query.is_empty()
        || node.version.id().as_str().contains(query)
        || node.version.label.to_lowercase().contains(query)
        || node.version.author.name.to_lowercase().contains(query)
        || node
            .version
            .author
            .email
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains(query)
        || node
            .variation_tips
            .iter()
            .any(|variation| variation.as_str().to_lowercase().contains(query))
}

fn workspace_graph_ref_matches(graph_ref: &WorkspaceGraphRef, query: &str) -> bool {
    query.is_empty()
        || graph_ref.id.to_lowercase().contains(query)
        || graph_ref.name.to_lowercase().contains(query)
        || graph_ref.display_label.to_lowercase().contains(query)
        || graph_ref
            .remote
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains(query)
        || graph_ref
            .variation
            .as_ref()
            .map(VariationId::as_str)
            .unwrap_or_default()
            .to_lowercase()
            .contains(query)
}

fn workspace_graph_node_id_from_version(version: &VersionId) -> Result<WorkspaceGraphNodeId> {
    Oid::from_str(version.as_str())
        .map(WorkspaceGraphNodeId::from)
        .map_err(|_| DraftlineError::VersionNotFound(version.to_string()))
}

fn workspace_graph_path_to_ancestor(
    by_id: &HashMap<WorkspaceGraphNodeId, Vec<WorkspaceGraphNodeId>>,
    start: &WorkspaceGraphNodeId,
    ancestor: &WorkspaceGraphNodeId,
) -> Option<Vec<WorkspaceGraphNodeId>> {
    let mut queue = VecDeque::from([(start.clone(), vec![start.clone()])]);
    let mut seen = BTreeSet::new();
    while let Some((id, path)) = queue.pop_front() {
        if id == *ancestor {
            return Some(path);
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        for parent in by_id.get(&id).into_iter().flatten() {
            let mut next_path = path.clone();
            next_path.push(parent.clone());
            queue.push_back((parent.clone(), next_path));
        }
    }
    None
}

fn workspace_graph_summary_from_graph(
    graph: &WorkspaceGraph,
    child_counts: &HashMap<WorkspaceGraphNodeId, usize>,
) -> WorkspaceGraphSummary {
    let normal_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.kind == WorkspaceGraphNodeKind::Normal)
        .count();
    let remote_only_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.kind == WorkspaceGraphNodeKind::RemoteOnly)
        .count();
    let support_ref_only_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.kind == WorkspaceGraphNodeKind::SupportRefOnly)
        .count();
    let merge_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.parent_ids.len() > 1)
        .count();
    let branch_points = graph
        .nodes
        .iter()
        .filter(|node| child_counts.get(&node.id).copied().unwrap_or_default() > 1)
        .count();
    let local_ref_count = graph
        .refs
        .iter()
        .filter(|graph_ref| graph_ref.kind == WorkspaceGraphRefKind::LocalVariation)
        .count();
    let remote_ref_count = graph
        .refs
        .iter()
        .filter(|graph_ref| graph_ref.kind == WorkspaceGraphRefKind::RemoteVariation)
        .count();
    let support_ref_count = graph
        .refs
        .iter()
        .filter(|graph_ref| graph_ref.kind == WorkspaceGraphRefKind::SupportRef)
        .count();

    WorkspaceGraphSummary {
        workspace_id: graph.workspace_id.clone(),
        current_variation: graph.current_variation.clone(),
        current_version: graph.current_version.clone(),
        dirty: graph.dirty.clone(),
        recovery: graph.recovery.clone(),
        state_may_be_inconsistent: graph.state_may_be_inconsistent,
        total_nodes: graph.nodes.len(),
        normal_nodes,
        remote_only_nodes,
        support_ref_only_nodes,
        merge_nodes,
        branch_points,
        local_ref_count,
        remote_ref_count,
        support_ref_count,
        graph_fingerprint: workspace_graph_fingerprint(
            &graph.current_variation,
            &graph.current_version,
            &graph.dirty,
            &graph.refs,
        ),
    }
}

fn graph_has_missing_parent(graph: &WorkspaceGraph) -> bool {
    let node_ids = graph
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    graph.nodes.iter().any(|node| {
        node.parent_ids
            .iter()
            .any(|parent| !node_ids.contains(parent))
    })
}

fn default_workspace_graph_overview_max_nodes() -> usize {
    200
}

fn default_workspace_graph_overview_recent_nodes() -> usize {
    50
}

fn stable_fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn workspace_graph_node_kind_token(kind: &WorkspaceGraphNodeKind) -> &'static str {
    match kind {
        WorkspaceGraphNodeKind::Normal => "normal",
        WorkspaceGraphNodeKind::RemoteOnly => "remote_only",
        WorkspaceGraphNodeKind::SupportRefOnly => "support_ref_only",
    }
}

fn workspace_graph_ref_kind_token(kind: &WorkspaceGraphRefKind) -> &'static str {
    match kind {
        WorkspaceGraphRefKind::LocalVariation => "local_variation",
        WorkspaceGraphRefKind::RemoteVariation => "remote_variation",
        WorkspaceGraphRefKind::SupportRef => "support_ref",
    }
}

fn workspace_graph_ref_scope_token(scope: &WorkspaceGraphRefScope) -> &'static str {
    match scope {
        WorkspaceGraphRefScope::Local => "local",
        WorkspaceGraphRefScope::RemoteTracking => "remote_tracking",
    }
}

fn change_kind_token(kind: &ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Conflicted => "conflicted",
        ChangeKind::TypeChanged => "type_changed",
    }
}

fn remote_from_remote_tracking_ref(ref_name: &str) -> Option<String> {
    let remainder = ref_name.strip_prefix("refs/remotes/")?;
    let (remote, _) = remainder.split_once('/')?;
    Some(remote.to_string())
}

fn remote_summary_from_commit(commit: &Commit<'_>) -> RemoteVersionSummary {
    RemoteVersionSummary {
        id: commit.id().to_string(),
        label: commit
            .summary()
            .ok()
            .flatten()
            .unwrap_or_default()
            .to_string(),
        author: contributor_from_signature(&commit.author()),
        time_seconds: commit.time().seconds(),
    }
}

fn new_operation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("op-{nanos}")
}

fn is_safe_operation_id(operation_id: &str) -> bool {
    !operation_id.is_empty()
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn workspace_diagnostic(
    code: DiagnosticCode,
    severity: DiagnosticSeverity,
    message: impl Into<String>,
) -> WorkspaceDiagnostic {
    WorkspaceDiagnostic {
        code,
        severity,
        message: message.into(),
    }
}

fn safe_next_actions_for_inspection(
    recovery: &Option<RecoveryState>,
    operation_lock: &OperationLockSummary,
    dirty: &DirtySummary,
    diagnostics: &[WorkspaceDiagnostic],
) -> Vec<SafeNextAction> {
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::RecoveryInOtherFolder)
    {
        return vec![SafeNextAction::FinishInOtherFolder];
    }
    if recovery.is_some()
        || operation_lock.state == OperationLockState::Locked
        || diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Blocking)
    {
        return vec![SafeNextAction::RepairRecovery];
    }

    if dirty.is_dirty {
        return vec![SafeNextAction::SaveFirst, SafeNextAction::DiscardChanges];
    }

    vec![SafeNextAction::NormalWork]
}

const STALE_LOCK_SECONDS: u64 = 30 * 60;

fn inspect_operation_lock_path(path: &Path) -> Result<OperationLockInspection> {
    if !path.exists() {
        return Ok(OperationLockInspection {
            state: OperationLockState::Unlocked,
            metadata: None,
            is_stale: false,
            can_clear: false,
            diagnostics: Vec::new(),
        });
    }

    let mut diagnostics = vec![workspace_diagnostic(
        DiagnosticCode::WorkspaceLocked,
        DiagnosticSeverity::Blocking,
        "workspace has an operation lock",
    )];

    let bytes = fs::read(path)?;
    let metadata = match serde_json::from_slice::<OperationLockMetadata>(&bytes) {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            diagnostics.push(workspace_diagnostic(
                DiagnosticCode::WorkspaceReadFailed,
                DiagnosticSeverity::Warning,
                format!("operation lock metadata is unreadable: {error}"),
            ));
            None
        }
    };

    let is_stale = metadata
        .as_ref()
        .map(operation_lock_metadata_is_stale)
        .unwrap_or(false);

    Ok(OperationLockInspection {
        state: OperationLockState::Locked,
        metadata,
        is_stale,
        can_clear: is_stale,
        diagnostics,
    })
}

/// A lock is stale once its process is gone on this machine, or after
/// `STALE_LOCK_SECONDS` when that can't be known (another machine, or an
/// unsupported platform). A reused process id only delays clearing.
fn operation_lock_metadata_is_stale(metadata: &OperationLockMetadata) -> bool {
    if metadata.process_id == std::process::id() {
        return false;
    }
    if now_seconds().saturating_sub(metadata.created_at_seconds) >= STALE_LOCK_SECONDS {
        return true;
    }
    let same_host = match (&metadata.host, current_host()) {
        (Some(recorded), Some(current)) => recorded.eq_ignore_ascii_case(&current),
        _ => false,
    };
    same_host && process_is_running(metadata.process_id) == Some(false)
}

/// Names the process table a lock's process id belongs to. The OS family
/// keeps Windows and WSL apart, which share a machine name by default.
fn current_host() -> Option<String> {
    let family = if cfg!(windows) { "windows" } else { "unix" };
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .or_else(|| fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .map(|host| format!("{family}:{host}"))
}

/// `Some(false)` only when the process is known to be gone.
#[cfg(windows)]
fn process_is_running(process_id: u32) -> Option<bool> {
    type Handle = *mut std::ffi::c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if handle.is_null() {
            let error = std::io::Error::last_os_error().raw_os_error();
            return Some(error != Some(ERROR_INVALID_PARAMETER));
        }
        let mut exit_code = 0u32;
        let read = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        (read != 0).then_some(exit_code == STILL_ACTIVE)
    }
}

/// `Some(false)` only when the process is known to be gone.
#[cfg(unix)]
fn process_is_running(process_id: u32) -> Option<bool> {
    const ESRCH: i32 = 3;
    extern "C" {
        fn kill(process_id: i32, signal: i32) -> i32;
    }
    let process_id = i32::try_from(process_id).ok().filter(|id| *id > 0)?;
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { kill(process_id, 0) } == 0 {
        return Some(true);
    }
    Some(std::io::Error::last_os_error().raw_os_error() != Some(ESRCH))
}

#[cfg(not(any(windows, unix)))]
fn process_is_running(_process_id: u32) -> Option<bool> {
    None
}

fn new_operation_lock_metadata(operation: impl Into<String>) -> OperationLockMetadata {
    OperationLockMetadata {
        operation_id: new_operation_id(),
        operation: operation.into(),
        process_id: std::process::id(),
        owner: std::env::var("USER")
            .ok()
            .or_else(|| std::env::var("USERNAME").ok()),
        created_at_seconds: now_seconds(),
        host: current_host(),
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn tree_contains_path(tree: &Tree<'_>, path: &Path) -> Result<bool> {
    match tree.get_path(path) {
        Ok(_) => Ok(true),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn ensure_remote_transport_supported(remote: &git2::Remote<'_>) -> Result<()> {
    if let Ok(url) = remote.url() {
        ensure_supported_remote_url(url)?;
    }
    Ok(())
}

fn remote_tracking_oid(repo: &Repository, remote: &str, variation: &str) -> Option<String> {
    let remote_ref = format!("refs/remotes/{remote}/{variation}");
    repo.refname_to_id(&remote_ref)
        .ok()
        .map(|oid| oid.to_string())
}

fn source_variation_from_support_ref(ref_name: &str, prefix: &str) -> Option<String> {
    let remainder = if let Some(remainder) = ref_name.strip_prefix(prefix) {
        remainder
    } else {
        let marker = format!("/{prefix}");
        ref_name.split_once(&marker)?.1
    };
    let (source_variation, _operation_id) = remainder.rsplit_once('/')?;
    Some(source_variation.to_string())
}

fn local_support_ref_from_remote_tracking(remote: &str, ref_name: &str) -> Option<String> {
    let prefix = format!("refs/remotes/{remote}/draftline/");
    let remainder = ref_name.strip_prefix(&prefix)?;
    Some(format!("refs/draftline/{remainder}"))
}

fn remote_tracking_support_ref_from_local(remote: &str, ref_name: &str) -> String {
    ref_name.replacen(
        "refs/draftline/",
        &format!("refs/remotes/{remote}/draftline/"),
        1,
    )
}

fn merge_blob_contents(
    registry: &ResolverRegistry,
    path: &Path,
    base: Option<Vec<u8>>,
    ours: Option<Vec<u8>>,
    theirs: Option<Vec<u8>>,
) -> std::result::Result<Option<Vec<u8>>, Box<MergeConflict>> {
    if ours == theirs {
        return Ok(ours);
    }

    if base == ours {
        return Ok(theirs);
    }

    if base == theirs {
        return Ok(ours);
    }

    let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
        return Err(Box::new(MergeConflict {
            path: path.to_path_buf(),
            field_path: None,
            label: "File was changed and deleted across versions".to_string(),
            base: None,
            ours: None,
            theirs: None,
            resolution: crate::merge::ResolutionKind::Choose,
        }));
    };

    let (Ok(base), Ok(ours), Ok(theirs)) = (
        std::str::from_utf8(&base),
        std::str::from_utf8(&ours),
        std::str::from_utf8(&theirs),
    ) else {
        return Err(Box::new(MergeConflict {
            path: path.to_path_buf(),
            field_path: None,
            label: "Binary content changed in both versions".to_string(),
            base: None,
            ours: None,
            theirs: None,
            resolution: crate::merge::ResolutionKind::Choose,
        }));
    };

    let outcome = registry.merge(MergeInput {
        path,
        base,
        ours,
        theirs,
    });
    if let Some(merged) = outcome.merged {
        Ok(Some(merged.into_bytes()))
    } else {
        Err(Box::new(
            outcome
                .conflicts
                .into_iter()
                .next()
                .unwrap_or_else(|| MergeConflict {
                    path: path.to_path_buf(),
                    field_path: None,
                    label: "Content changed in both versions".to_string(),
                    base: Some(base.to_string()),
                    ours: Some(ours.to_string()),
                    theirs: Some(theirs.to_string()),
                    resolution: crate::merge::ResolutionKind::Edit,
                }),
        ))
    }
}

fn resolved_conflict_content(
    conflict: &MergeConflict,
    choice: &MergeResolutionChoice,
) -> Result<Option<Vec<u8>>> {
    if conflict.field_path.is_some() && !matches!(choice, MergeResolutionChoice::UseContent { .. })
    {
        return Err(DraftlineError::InvalidMergeResolution(format!(
            "semantic conflict `{}` requires full resolved file content",
            conflict.path.display()
        )));
    }

    let selected = match choice {
        MergeResolutionChoice::UseOurs => conflict.ours.as_deref(),
        MergeResolutionChoice::UseTheirs => conflict.theirs.as_deref(),
        MergeResolutionChoice::UseBase => conflict.base.as_deref(),
        MergeResolutionChoice::Delete => return Ok(None),
        MergeResolutionChoice::UseContent { content } => {
            return Ok(Some(content.as_bytes().to_vec()))
        }
    };

    selected
        .map(|content| Some(content.as_bytes().to_vec()))
        .ok_or_else(|| {
            DraftlineError::InvalidMergeResolution(format!(
                "selected content is unavailable for `{}`",
                conflict.path.display()
            ))
        })
}

fn is_restorable_support_ref(ref_name: &str) -> bool {
    ref_name.starts_with("refs/draftline/deleted-variations/")
        || ref_name.starts_with("refs/draftline/rewrites/squash/")
        || ref_name.starts_with("refs/draftline/backups/history-cleanup/")
}

fn is_remote_tracking_restorable_support_ref(ref_name: &str) -> bool {
    ref_name.contains("/draftline/deleted-variations/")
        || ref_name.contains("/draftline/rewrites/squash/")
        || ref_name.contains("/draftline/backups/history-cleanup/")
}

/// Converts a libgit2 `Delta` status to a [`ChangeKind`].
fn git2_delta_to_change_kind(status: git2::Delta) -> ChangeKind {
    match status {
        git2::Delta::Added | git2::Delta::Copied | git2::Delta::Untracked => ChangeKind::Added,
        git2::Delta::Deleted => ChangeKind::Deleted,
        git2::Delta::Renamed => ChangeKind::Renamed,
        git2::Delta::Typechange => ChangeKind::TypeChanged,
        git2::Delta::Conflicted => ChangeKind::Conflicted,
        _ => ChangeKind::Modified,
    }
}

/// Collects [`ChangedFile`] entries from a tree-to-tree diff.
///
/// `is_large` is always `false` because size thresholds are not meaningful
/// for historical object comparisons.
fn diff_deltas_to_changed_files(diff: &git2::Diff<'_>) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(PathBuf::from)
            .unwrap_or_default();

        if path.as_os_str().is_empty() {
            continue;
        }

        let kind = git2_delta_to_change_kind(delta.status());
        let is_binary = delta.flags().contains(git2::DiffFlags::BINARY);

        files.push(ChangedFile {
            path,
            kind,
            is_binary,
            is_large: false,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn remove_index_path(index: &mut git2::Index, path: &Path) -> Result<()> {
    match index.remove_path(path) {
        Err(error) if error.code() != git2::ErrorCode::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn blob_file_mode(mode: i32) -> git2::FileMode {
    match mode {
        0o100755 => git2::FileMode::BlobExecutable,
        0o120000 => git2::FileMode::Link,
        _ => git2::FileMode::Blob,
    }
}

fn entry_file_mode(entry: &git2::TreeEntry<'_>) -> git2::FileMode {
    match entry.filemode() {
        0o040000 => git2::FileMode::Tree,
        0o160000 => git2::FileMode::Commit,
        mode => blob_file_mode(mode),
    }
}

fn optional_tree_entry(tree: &Tree<'_>, path: &Path) -> Result<Option<Oid>> {
    match tree.get_path(path) {
        Ok(entry) => Ok(Some(entry.id())),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Literal (non-glob) pathspec for the folder, or `None` when unscoped.
fn scope_pathspec(scope: &Path) -> Option<String> {
    if scope.as_os_str().is_empty() {
        return None;
    }
    Some(scope.to_string_lossy().replace('\\', "/"))
}

/// Restricts a status scan to the folder so large code repositories
/// are not walked in full.
fn limit_status_to_scope(options: &mut StatusOptions, scope: &Path) {
    if let Some(pathspec) = scope_pathspec(scope) {
        options.disable_pathspec_match(true).pathspec(pathspec);
    }
}

/// Restricts a diff to the folder.
fn limit_diff_to_scope(options: &mut DiffOptions, scope: &Path) {
    if let Some(pathspec) = scope_pathspec(scope) {
        options.disable_pathspec_match(true).pathspec(pathspec);
    }
}

/// Computes the repository-relative folder for `project_path`.
fn project_scope(workdir: &Path, project_path: &Path) -> Result<PathBuf> {
    let workdir = fs::canonicalize(workdir)?;
    let project = fs::canonicalize(project_path)?;
    match project.strip_prefix(&workdir) {
        Ok(relative) => normalize_workspace_relative(relative),
        // e.g. a symlinked folder that resolves outside the repository.
        Err(_) => Err(DraftlineError::PathEscapesWorkspace(project)),
    }
}

/// Collects [`ChangedFile`] entries from a tree-to-workdir diff, filtered by
/// `content_policy`.  `is_large` and `is_binary` are derived from the actual
/// workspace file, matching the behaviour of [`Workspace::changed_files`].
fn diff_deltas_to_changed_files_with_policy(
    diff: &git2::Diff<'_>,
    root: &Path,
    policy: &ContentPolicy,
) -> Result<Vec<ChangedFile>> {
    let mut files = Vec::new();
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(PathBuf::from)
            .unwrap_or_default();

        if path.as_os_str().is_empty() || !policy.tracks(&path)? {
            continue;
        }

        let kind = git2_delta_to_change_kind(delta.status());
        let full_path = root.join(&path);
        let is_binary = file_is_binary(&full_path)?;
        let is_large = file_is_large(&full_path, policy.large_file_threshold_bytes())?;

        files.push(ChangedFile {
            path,
            kind,
            is_binary,
            is_large,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Renders a diff as a unified patch string.
fn diff_to_patch_text(diff: &git2::Diff<'_>) -> Result<String> {
    let mut text = String::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        if let Ok(content) = std::str::from_utf8(line.content()) {
            text.push_str(content);
        }
        true
    })?;
    Ok(text)
}

fn redact_remote_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let path_start = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(url.len());
    let authority = &url[authority_start..path_start];
    let Some(userinfo_end) = authority.rfind('@') else {
        return url.to_string();
    };

    format!(
        "{}://{}{}",
        &url[..scheme_end],
        &authority[userinfo_end + 1..],
        &url[path_start..]
    )
}

/// Creates `path` and any missing parents, returning only the folders this
/// call created, outermost first.
fn create_missing_dirs(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut missing: Vec<&Path> = path
        .ancestors()
        .take_while(|ancestor| !ancestor.as_os_str().is_empty() && !ancestor.is_dir())
        .collect();
    missing.reverse();
    let mut created = Vec::new();
    for dir in missing {
        match fs::create_dir(dir) {
            Ok(()) => created.push(dir.to_path_buf()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(created)
}

/// A claim's folder must be a canonical path inside the checkout, so a
/// tampered claim can't point the host at a folder elsewhere.
fn claimed_project(claim: &WorkspaceClaim, path: &Path) -> Result<PathBuf> {
    canonical_folder(&claim.project).ok_or_else(|| {
        DraftlineError::InvalidFolderMarker(format!(
            "{}: folder {:?} is not a folder inside this checkout; delete it to reset it",
            path.display(),
            claim.project
        ))
    })
}

/// `raw` as a repository-relative folder, only if it is already written the
/// canonical way (`a/b`, no `.`/`..`, no trailing slash; `""` for the root).
fn canonical_folder(raw: &str) -> Option<PathBuf> {
    let folder = normalize_workspace_relative(raw).ok()?;
    (folder_key(&folder) == raw).then_some(folder)
}

/// A repository-relative folder written with `/`, `""` for the root.
fn folder_key(folder: &Path) -> String {
    folder
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A folder key as [`FOLDER_MARKER`] writes it: `"."` for the root.
fn marker_folder(key: &str) -> String {
    if key.is_empty() {
        ".".to_string()
    } else {
        key.to_string()
    }
}

/// A folder for people to read: "the repository root" or `cutready`.
fn folder_label(folder: &Path) -> String {
    if folder.as_os_str().is_empty() {
        "the repository root".to_string()
    } else {
        folder_key(folder)
    }
}

fn marker_bytes(key: &str) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&FolderMarker {
        format: 1,
        folder: marker_folder(key),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// `path` made absolute against the current directory, without resolving it,
/// so a folder that doesn't exist yet still has a place.
fn absolute_path(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

/// The repository-relative folder for `path`, which need not exist: its
/// nearest existing ancestor anchors it inside `root`.
fn folder_in(root: &Path, path: &Path) -> Result<PathBuf> {
    let existing = path
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .ok_or_else(|| DraftlineError::PathEscapesWorkspace(path.to_path_buf()))?;
    let tail = path
        .strip_prefix(existing)
        .map_err(|_| DraftlineError::PathEscapesWorkspace(path.to_path_buf()))?;
    normalize_workspace_relative(project_scope(root, existing)?.join(tail))
}

/// Every tracked path in `tree` at or below `prefix` (all of them for the root).
/// Whether `tree` has `folder` as a directory (a file at that path isn't it).
fn tracks_folder(tree: &Tree<'_>, folder: &Path) -> bool {
    folder.as_os_str().is_empty()
        || tree
            .get_path(folder)
            .is_ok_and(|entry| entry.kind() == Some(ObjectType::Tree))
}

fn tracked_paths_under(tree: &Tree<'_>, prefix: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() != Some(ObjectType::Tree) {
            let path = PathBuf::from(format!("{dir}{}", entry.name().unwrap_or_default()));
            if path.starts_with(prefix) {
                paths.push(path);
            }
        }
        git2::TreeWalkResult::Ok
    })?;
    Ok(paths)
}

/// `path` with the folder `prefix` removed, when it is inside it.
fn strip_folder<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix(b"/")
}

fn suggest(fix: FolderFix, label: &str) -> FolderSuggestion {
    FolderSuggestion {
        fix,
        label: label.to_string(),
    }
}

/// Files and folders under `dir`, without following links or entering `skip`
/// or a nested repository.
fn collect_leftovers(dir: &Path, skip: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip || entry.file_name() == ".git" {
            continue;
        }
        match entry.file_type() {
            // A nested repository moves whole or not at all.
            Ok(kind) if kind.is_dir() && path.join(".git").exists() => files.push(path),
            Ok(kind) if kind.is_dir() => {
                dirs.push(path.clone());
                collect_leftovers(&path, skip, files, dirs);
            }
            Ok(_) => files.push(path),
            Err(_) => {}
        }
    }
}

fn unique_claim_temp(path: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("json.{}.{next}.tmp", std::process::id()))
}

struct OperationLock {
    path: PathBuf,
}

impl OperationLock {
    fn acquire(path: &Path, operation: impl Into<String>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    DraftlineError::WorkspaceLocked
                } else {
                    DraftlineError::Io(error)
                }
            })?;

        let metadata = new_operation_lock_metadata(operation);
        if let Err(error) = serde_json::to_writer_pretty(&mut file, &metadata) {
            let _ = fs::remove_file(path);
            return Err(error.into());
        }

        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Lets unit tests run Git between two steps of an operation, to force
/// interleavings that real timing only hits by chance.
/// How long an operation waits for another Git process to release the index.
const GIT_BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod test_hooks {
    use std::{cell::RefCell, path::Path};

    type Hook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static HOOKS: RefCell<Vec<(&'static str, Hook)>> = RefCell::new(Vec::new());
    }

    /// Runs `hook` once, the next time `point` is reached on this thread.
    pub fn once(point: &'static str, hook: impl FnMut(&Path) + 'static) {
        HOOKS.with(|hooks| hooks.borrow_mut().push((point, Box::new(hook))));
    }

    pub fn fire(point: &str, git_dir: &Path) {
        let hook = HOOKS.with(|hooks| {
            let mut hooks = hooks.borrow_mut();
            let index = hooks.iter().position(|(name, _)| *name == point)?;
            Some(hooks.remove(index).1)
        });
        if let Some(mut hook) = hook {
            hook(git_dir);
        }
    }
}

#[cfg(not(test))]
mod test_hooks {
    #[inline(always)]
    pub fn fire(_point: &str, _git_dir: &std::path::Path) {}
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;
    use crate::merge::{MergeOutcome, SemanticMergeResolver};

    struct CleanSketchResolver {
        calls: Arc<AtomicUsize>,
    }

    impl SemanticMergeResolver for CleanSketchResolver {
        fn matches(&self, path: &Path) -> bool {
            path.extension().and_then(|extension| extension.to_str()) == Some("sk")
        }

        fn merge(&self, _input: MergeInput<'_>) -> MergeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            MergeOutcome::clean("semantic merge")
        }
    }

    struct ConflictingSketchResolver {
        calls: Arc<AtomicUsize>,
    }

    impl SemanticMergeResolver for ConflictingSketchResolver {
        fn matches(&self, path: &Path) -> bool {
            path.extension().and_then(|extension| extension.to_str()) == Some("sk")
        }

        fn merge(&self, input: MergeInput<'_>) -> MergeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            MergeOutcome::conflicted(MergeConflict {
                path: input.path.to_path_buf(),
                field_path: Some("rows".to_string()),
                label: "Sketch rows changed in both versions".to_string(),
                base: Some(input.base.to_string()),
                ours: Some(input.ours.to_string()),
                theirs: Some(input.theirs.to_string()),
                resolution: crate::merge::ResolutionKind::Edit,
            })
        }
    }

    fn write_file(root: &Path, relative: &str, content: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn stage_file(workspace: &Workspace, relative: &str) {
        let mut index = workspace.repo.index().unwrap();
        index.add_path(Path::new(relative)).unwrap();
        index.write().unwrap();
    }

    fn stage_rename(workspace: &Workspace, old: &str, new: &str) {
        fs::rename(workspace.root().join(old), workspace.root().join(new)).unwrap();
        let mut index = workspace.repo.index().unwrap();
        index.remove_path(Path::new(old)).unwrap();
        index.add_path(Path::new(new)).unwrap();
        index.write().unwrap();
    }

    fn configure_identity(workspace: &Workspace, name: &str, email: &str) {
        let mut config = workspace.repo.config().unwrap();
        config.set_str("user.name", name).unwrap();
        config.set_str("user.email", email).unwrap();
    }

    fn init_workspace_with_initial_branch(root: &Path, branch: &str) -> Workspace {
        let mut options = RepositoryInitOptions::new();
        options.initial_head(branch);
        Repository::init_opts(root, &options).unwrap();
        Workspace::open(root).unwrap()
    }

    fn init_bare_remote(root: &Path) -> Repository {
        let initial_head = default_initial_variation_name();
        let mut options = RepositoryInitOptions::new();
        options.bare(true).initial_head(&initial_head);
        Repository::init_opts(root, &options).unwrap()
    }

    fn diverged_sketch_workspace() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        Workspace,
    ) {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let author_dir = tempfile::tempdir().unwrap();
        let author = Workspace::init(author_dir.path()).unwrap();
        configure_identity(&author, "Seth", "seth@example.com");
        author
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(author.root(), "plan.sk", b"base");
        author.save_version("Base").unwrap();
        author.publish_changes("origin").unwrap();

        let peer_dir = tempfile::tempdir().unwrap();
        let peer =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), peer_dir.path()).unwrap();
        configure_identity(&peer, "Maria", "maria@example.com");

        write_file(author.root(), "plan.sk", b"remote");
        author.save_version("Remote").unwrap();
        author.publish_changes("origin").unwrap();

        write_file(peer.root(), "plan.sk", b"local");
        peer.save_version("Local").unwrap();
        peer.fetch_remote("origin").unwrap();

        (remote, author_dir, peer_dir, peer)
    }

    /// Git grabs `index.lock` right after Draftline's precheck passed.
    fn lock_index_after_precheck() {
        test_hooks::once("after_index_precheck", |git_dir| {
            fs::write(git_dir.join("index.lock"), b"").unwrap();
        });
    }

    /// After a late lock is released, the operation has either not happened
    /// or can be rolled back, and nothing outside it changed.
    fn settle_after_late_lock(workspace: &Workspace) {
        fs::remove_file(workspace.repo.path().join("index.lock")).unwrap();
        if let Some(state) = workspace.recovery_state().unwrap() {
            workspace.rollback_recovery(&state.operation_id).unwrap();
        }
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    fn content(workspace: &Workspace, path: &str) -> String {
        fs::read_to_string(workspace.root().join(path)).unwrap()
    }

    fn head_summary(workspace: &Workspace) -> String {
        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        head.summary().unwrap().unwrap().to_string()
    }

    fn assert_clean(workspace: &Workspace) {
        let statuses = workspace.repo.statuses(None).unwrap();
        let dirty = statuses
            .iter()
            .filter(|entry| entry.status() != git2::Status::IGNORED)
            .map(|entry| entry.path().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(dirty.is_empty(), "unexpected changes: {dirty:?}");
    }

    fn two_saves() -> (tempfile::TempDir, Workspace, Version) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "plan.sk", b"v2");
        workspace.save_version("Second").unwrap();
        (temp, workspace, first)
    }

    #[test]
    fn a_late_index_lock_during_a_save_keeps_the_edit_unsaved() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        lock_index_after_precheck();

        let error = workspace.save_version("Third").unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        settle_after_late_lock(&workspace);
        assert_eq!(head_summary(&workspace), "Second");
        assert_eq!(content(&workspace, "plan.sk"), "edit");
        workspace.save_version("Third").unwrap();
        assert_clean(&workspace);
    }

    #[test]
    fn a_late_index_lock_during_a_restore_is_retryable() {
        let (_temp, workspace, first) = two_saves();
        lock_index_after_precheck();

        let error = workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        settle_after_late_lock(&workspace);
        assert_eq!(head_summary(&workspace), "Second");
        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
        workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap();
        assert_eq!(content(&workspace, "plan.sk"), "v1");
        assert_clean(&workspace);
    }

    #[test]
    fn a_late_index_lock_during_a_switch_is_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        workspace.save_version("First").unwrap();
        let alternate = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "plan.sk", b"v2");
        workspace.save_version("Second").unwrap();
        lock_index_after_precheck();

        let error = workspace
            .switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        settle_after_late_lock(&workspace);
        assert_eq!(head_summary(&workspace), "Second");
        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
        workspace
            .switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        assert_eq!(content(&workspace, "plan.sk"), "v1");
        assert_clean(&workspace);
    }

    #[test]
    fn a_late_index_lock_during_a_discard_keeps_the_edit() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        lock_index_after_precheck();

        let error = workspace.discard_changes().unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        settle_after_late_lock(&workspace);
        assert_eq!(content(&workspace, "plan.sk"), "edit");
        workspace.discard_changes().unwrap();
        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_late_index_lock_during_a_shelve_keeps_the_edit() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        lock_index_after_precheck();

        let error = workspace.shelve_changes("later").unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        settle_after_late_lock(&workspace);
        assert_eq!(head_summary(&workspace), "Second");
        assert_eq!(content(&workspace, "plan.sk"), "edit");
        assert!(workspace.list_shelves().unwrap().is_empty());
    }

    #[test]
    fn a_shelve_rollback_keeps_the_shelf_once_an_edit_was_discarded() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        lock_index_after_precheck();
        workspace.shelve_changes("later").unwrap_err();
        // As if the discard step had reverted the file before failing.
        write_file(workspace.root(), "plan.sk", b"v2");

        settle_after_late_lock(&workspace);

        let shelves = workspace.list_shelves().unwrap();
        assert_eq!(shelves.len(), 1);
        workspace.apply_shelf(&shelves[0].id).unwrap();
        assert_eq!(content(&workspace, "plan.sk"), "edit");
    }

    #[test]
    fn a_restore_rollback_keeps_files_edited_after_the_failure() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        write_file(workspace.root(), "notes.md", b"v1");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "plan.sk", b"v2");
        write_file(workspace.root(), "notes.md", b"v2");
        fs::remove_file(workspace.root().join("plan.sk")).unwrap();
        write_file(workspace.root(), "added.md", b"v2");
        workspace.save_version("Second").unwrap();
        lock_index_after_precheck();
        workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap_err();
        assert_eq!(content(&workspace, "notes.md"), "v1");
        write_file(workspace.root(), "notes.md", b"mine");

        settle_after_late_lock(&workspace);

        assert_eq!(head_summary(&workspace), "Second");
        assert_eq!(content(&workspace, "notes.md"), "mine");
        assert!(!workspace.root().join("plan.sk").exists());
        assert_eq!(content(&workspace, "added.md"), "v2");
        let changed = workspace.changed_files().unwrap();
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0].path, Path::new("notes.md"));
    }

    /// Git grabs `index.lock` after the precheck and lets go shortly after.
    fn lock_index_briefly_after_precheck() {
        test_hooks::once("after_index_precheck", |git_dir| {
            let lock = git_dir.join("index.lock");
            fs::write(&lock, b"").unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                fs::remove_file(lock).unwrap();
            });
        });
    }

    #[test]
    fn a_save_waits_out_a_brief_index_lock() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        lock_index_briefly_after_precheck();

        workspace.save_version("Third").unwrap();

        assert_eq!(head_summary(&workspace), "Third");
        assert!(workspace
            .recovery_state()
            .unwrap()
            .is_none_or(|state| state.completed));
        assert_clean(&workspace);
    }

    #[test]
    fn a_restore_waits_out_a_brief_index_lock() {
        let (_temp, workspace, first) = two_saves();
        lock_index_briefly_after_precheck();

        workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap();

        assert_eq!(head_summary(&workspace), "Restore first");
        assert_eq!(content(&workspace, "plan.sk"), "v1");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_waits_out_a_brief_index_lock() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        workspace.save_version("First").unwrap();
        let main = workspace.current_variation().unwrap();
        let alternate = workspace.create_variation("alternate").unwrap();
        workspace
            .switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        fs::remove_file(workspace.root().join("plan.sk")).unwrap();
        write_file(workspace.root(), "other.sk", b"alt");
        workspace.save_version("Alternate").unwrap();
        lock_index_briefly_after_precheck();

        let main_id = VariationId::from(main);
        workspace
            .switch_variation(&main_id, SwitchPolicy::AbortIfDirty)
            .unwrap();

        assert_eq!(content(&workspace, "plan.sk"), "v1");
        assert!(!workspace.root().join("other.sk").exists());
        assert!(workspace
            .recovery_state()
            .unwrap()
            .is_none_or(|state| state.completed));
        assert_clean(&workspace);
    }

    #[test]
    fn a_late_index_lock_during_a_mixed_discard_changes_nothing() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        write_file(workspace.root(), "new.sk", b"new");
        let mut index = workspace.repo.index().unwrap();
        index.add_path(Path::new("new.sk")).unwrap();
        index.write().unwrap();
        lock_index_after_precheck();

        let error = workspace.discard_changes().unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        assert_eq!(content(&workspace, "plan.sk"), "edit");
        assert_eq!(content(&workspace, "new.sk"), "new");
        settle_after_late_lock(&workspace);
        assert_eq!(content(&workspace, "new.sk"), "new");
        let status = workspace.repo.status_file(Path::new("new.sk")).unwrap();
        assert!(status.contains(git2::Status::INDEX_NEW), "{status:?}");
    }

    #[test]
    fn a_switch_never_stages_a_file_edited_while_it_waits_for_git() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        workspace.save_version("First").unwrap();
        let alternate = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "plan.sk", b"v2");
        workspace.save_version("Second").unwrap();
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_index_precheck", |git_dir| {
            fs::write(git_dir.join("index.lock"), b"").unwrap();
        });
        // The developer edits while Draftline waits for Git's lock.
        test_hooks::once("git_busy_retry", move |git_dir| {
            fs::write(root.join("plan.sk"), b"theirs").unwrap();
            fs::remove_file(git_dir.join("index.lock")).unwrap();
        });

        let error = workspace
            .switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        assert_eq!(content(&workspace, "plan.sk"), "theirs");
        assert!(workspace.recovery_state().unwrap().is_some());
        let entry = workspace
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("plan.sk"), 0)
            .unwrap();
        let head = workspace.repo.head().unwrap().peel_to_tree().unwrap();
        assert_eq!(entry.id, head.get_path(Path::new("plan.sk")).unwrap().id());
    }

    /// Git holds the branch's ref lock while it moves the branch.
    fn lock_branch_after_precheck(workspace: &Workspace, release_after_ms: Option<u64>) {
        let branch = workspace.repo.head().unwrap().name().unwrap().to_string();
        test_hooks::once("after_index_precheck", move |git_dir| {
            let lock = git_dir.join(format!("{branch}.lock"));
            fs::write(&lock, b"").unwrap();
            if let Some(ms) = release_after_ms {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    fs::remove_file(lock).unwrap();
                });
            }
        });
    }

    #[test]
    fn a_save_waits_out_a_brief_branch_ref_lock() {
        let (_temp, workspace, _first) = two_saves();
        write_file(workspace.root(), "plan.sk", b"third");
        lock_branch_after_precheck(&workspace, Some(300));

        workspace.save_version("Third").unwrap();

        assert_eq!(head_summary(&workspace), "Third");
        assert_clean(&workspace);
    }

    #[test]
    fn a_lingering_branch_ref_lock_fails_the_save_cleanly() {
        let (_temp, workspace, _first) = two_saves();
        let before = workspace.repo.head().unwrap().target().unwrap();
        write_file(workspace.root(), "plan.sk", b"third");
        lock_branch_after_precheck(&workspace, None);

        let error = workspace.save_version("Third").unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        assert_eq!(workspace.repo.head().unwrap().target().unwrap(), before);
        let status = workspace.repo.status_file(Path::new("plan.sk")).unwrap();
        assert_eq!(status, git2::Status::WT_MODIFIED);
        let branch = workspace.repo.head().unwrap().name().unwrap().to_string();
        fs::remove_file(workspace.repo.path().join(format!("{branch}.lock"))).unwrap();
        workspace.save_version("Third").unwrap();
        assert_eq!(head_summary(&workspace), "Third");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_stops_when_a_developer_commits_while_it_waits_for_git() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        workspace.save_version("First").unwrap();
        let alternate = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "plan.sk", b"v2");
        workspace.save_version("Second").unwrap();
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_index_precheck", |git_dir| {
            fs::write(git_dir.join("index.lock"), b"").unwrap();
        });
        // The developer commits while Draftline waits for Git's lock.
        test_hooks::once("git_busy_retry", move |git_dir| {
            let repo = Repository::open(&root).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            let blob = repo.blob(b"fn main() {}").unwrap();
            let mut builder = repo.treebuilder(Some(&head.tree().unwrap())).unwrap();
            builder.insert("code.rs", blob, 0o100644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Dev", "dev@example.com").unwrap();
            repo.commit(Some("HEAD"), &signature, &signature, "dev", &tree, &[&head])
                .unwrap();
            fs::remove_file(git_dir.join("index.lock")).unwrap();
        });

        let error = workspace
            .switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        assert!(workspace.recovery_state().unwrap().is_some());
        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.message().ok(), Some("dev"));
    }

    /// A developer commit of `code.rs` on the current branch, outside Draftline.
    fn dev_commit(workspace: &Workspace, code: &[u8]) {
        let repo = &workspace.repo;
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let blob = repo.blob(code).unwrap();
        let mut builder = repo.treebuilder(Some(&head.tree().unwrap())).unwrap();
        builder.insert("code.rs", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Dev", "dev@example.com").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "dev", &tree, &[&head])
            .unwrap();
        let mut checkout = CheckoutBuilder::new();
        repo.checkout_head(Some(checkout.force())).unwrap();
    }

    /// `alt` has code.rs "v1"; the current branch has the developer's "v2".
    fn branches_with_different_code() -> (tempfile::TempDir, Workspace, String, Version) {
        let (temp, workspace, first) = two_saves();
        dev_commit(&workspace, b"v1");
        {
            let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
            workspace.repo.branch("alt", &head, false).unwrap();
        }
        dev_commit(&workspace, b"v2");
        let original = workspace.current_variation_unchecked().unwrap();
        (temp, workspace, original, first)
    }

    fn interrupted_switch(workspace: &Workspace, original: &str) {
        workspace
            .write_recovery_state_for_checkout(
                &RecoveryState {
                    operation_id: "interrupted".to_string(),
                    operation: RecoveryOperation::SwitchVariation,
                    original_variation: Some(original.to_string()),
                    target: Some("alt".to_string()),
                    completed: false,
                },
                &workspace.branch_tree(original).unwrap(),
                &workspace.branch_tree("alt").unwrap(),
            )
            .unwrap();
    }

    /// Commits `code.rs` = `text` on `branch` without touching the checkout,
    /// as another worktree or `git branch -f` would.
    fn move_branch(workspace: &Workspace, branch: &str, text: &str) {
        let repo = &workspace.repo;
        let tip = repo
            .find_branch(branch, BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap();
        let blob = repo.blob(text.as_bytes()).unwrap();
        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        builder.insert("code.rs", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Dev", "dev@example.com").unwrap();
        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "dev moves branch",
            &tree,
            &[&tip],
        )
        .unwrap();
    }

    #[test]
    fn a_switch_repair_follows_a_target_that_moved_since() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        move_branch(&workspace, "alt", "v3");

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v3");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_repair_finishes_a_checkout_that_never_started() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        interrupted_switch(&workspace, &original);

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v1");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_repair_keeps_a_file_the_developer_reverted() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        // The checkout finished; the developer then put the old code back.
        std::fs::write(workspace.root().join("code.rs"), "v2").unwrap();

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v2");
    }

    #[test]
    fn a_switch_repair_keeps_a_reverted_file_when_the_target_moved_too() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        std::fs::write(workspace.root().join("code.rs"), "v2").unwrap();
        move_branch(&workspace, "alt", "v3");

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v2");
    }

    #[test]
    fn a_switch_rollback_follows_a_branch_that_moves_while_it_settles() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        let branch = original.clone();
        test_hooks::once("before_settle_files", move |git_dir| {
            let workspace_repo = Repository::open(git_dir).unwrap();
            let tip = workspace_repo
                .find_branch(&branch, BranchType::Local)
                .unwrap()
                .into_reference()
                .peel_to_commit()
                .unwrap();
            let blob = workspace_repo.blob(b"v4").unwrap();
            let mut builder = workspace_repo
                .treebuilder(Some(&tip.tree().unwrap()))
                .unwrap();
            builder.insert("code.rs", blob, 0o100644).unwrap();
            let tree = workspace_repo.find_tree(builder.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Dev", "dev@example.com").unwrap();
            workspace_repo
                .commit(
                    Some(&format!("refs/heads/{branch}")),
                    &signature,
                    &signature,
                    "dev moves branch",
                    &tree,
                    &[&tip],
                )
                .unwrap();
        });

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v4");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_repair_finishes_a_file_the_checkout_left_missing() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        interrupted_switch(&workspace, &original);
        // Killed after removing the old file, before writing the new one.
        fs::remove_file(workspace.root().join("code.rs")).unwrap();

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v1");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_rollback_rewrites_a_half_written_file() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        interrupted_switch(&workspace, &original);
        fs::write(workspace.root().join("code.rs"), "v").unwrap();

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_repair_finishes_a_file_an_earlier_repair_left_missing() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        move_branch(&workspace, "alt", "v3");
        // A repair following alt died after removing the file, before writing it.
        fs::remove_file(workspace.root().join("code.rs")).unwrap();

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v3");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_repair_keeps_a_file_deleted_after_the_checkout_finished() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        let mut checkout = CheckoutBuilder::new();
        workspace
            .repo
            .checkout_tree(alt.as_object(), Some(checkout.force()))
            .unwrap();
        interrupted_switch(&workspace, &original);
        fs::remove_file(workspace.root().join("code.rs")).unwrap();

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert!(!workspace.root().join("code.rs").exists());
    }

    #[test]
    fn a_restore_rollback_puts_back_a_file_the_checkout_left_missing() {
        let (_temp, workspace, first) = two_saves();
        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        let source = workspace
            .relocated_tree(&workspace.find_version_commit(&first.id).unwrap())
            .unwrap();
        let restored = workspace
            .compose_scoped_tree(&head.tree().unwrap(), &source)
            .unwrap();
        workspace
            .write_recovery_state_for_checkout(
                &RecoveryState {
                    operation_id: "interrupted".to_string(),
                    operation: RecoveryOperation::RestoreVersionAsNewSave,
                    original_variation: workspace.current_variation().ok(),
                    target: Some(first.id.as_str().to_string()),
                    completed: false,
                },
                &head.tree().unwrap(),
                &workspace.repo.find_tree(restored).unwrap(),
            )
            .unwrap();
        fs::remove_file(workspace.root().join("plan.sk")).unwrap();

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert!(workspace.changed_files().unwrap().is_empty());
    }

    #[test]
    fn a_switch_rollback_ignores_a_target_that_moved_since() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        move_branch(&workspace, "alt", "v3");

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_rollback_keeps_a_mode_change_the_developer_staged() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        let mut index = workspace.repo.index().unwrap();
        let mut entry = index.get_path(Path::new("code.rs"), 0).unwrap();
        entry.mode = 0o100755;
        index.add(&entry).unwrap();
        index.write().unwrap();

        workspace.rollback_recovery("interrupted").unwrap();

        let mut index = workspace.repo.index().unwrap();
        index.read(true).unwrap();
        let entry = index.get_path(Path::new("code.rs"), 0).unwrap();
        assert_eq!(entry.mode, 0o100755);
        assert_eq!(entry.id, alt.get_path(Path::new("code.rs")).unwrap().id());
    }

    #[test]
    fn a_switch_rollback_keeps_an_edit_made_after_the_index_moved() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        let root = workspace.root().to_path_buf();
        test_hooks::once("before_settle_files", move |_| {
            std::fs::write(root.join("code.rs"), "mine").unwrap();
        });

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "mine");
    }

    #[test]
    fn a_switch_rollback_puts_developer_code_back_when_head_never_moved() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        // The switch wrote every file and the index, then failed to move HEAD.
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        assert_eq!(content(&workspace, "code.rs"), "v1");
        interrupted_switch(&workspace, &original);

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v2");
        assert_clean(&workspace);
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn a_switch_rollback_keeps_what_the_developer_staged_meanwhile() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        let alt = workspace.branch_tree("alt").unwrap();
        workspace.repo.checkout_tree(alt.as_object(), None).unwrap();
        interrupted_switch(&workspace, &original);
        std::fs::write(workspace.root().join("code.rs"), "mine").unwrap();
        let mut index = workspace.repo.index().unwrap();
        index.add_path(Path::new("code.rs")).unwrap();
        index.write().unwrap();
        let staged = index.get_path(Path::new("code.rs"), 0).unwrap().id;

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "mine");
        let mut index = workspace.repo.index().unwrap();
        index.read(true).unwrap();
        assert_eq!(index.get_path(Path::new("code.rs"), 0).unwrap().id, staged);
    }

    #[test]
    fn a_switch_repair_finishes_a_partly_written_checkout() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        // Only code.rs reached the disk; the index and HEAD never moved.
        let alt = workspace.branch_tree("alt").unwrap();
        let mut checkout = CheckoutBuilder::new();
        checkout.force().update_index(false).path("code.rs");
        workspace
            .repo
            .checkout_tree(alt.as_object(), Some(&mut checkout))
            .unwrap();
        interrupted_switch(&workspace, &original);

        workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(content(&workspace, "code.rs"), "v1");
        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_rollback_after_head_moved_returns_to_the_original_branch() {
        let (_temp, workspace, original, _) = branches_with_different_code();
        workspace.repo.set_head("refs/heads/alt").unwrap();
        let mut checkout = CheckoutBuilder::new();
        workspace
            .repo
            .checkout_head(Some(checkout.force()))
            .unwrap();
        interrupted_switch(&workspace, &original);

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_targeted_restore_rollback_puts_the_checkout_back() {
        let (_temp, workspace, original, first) = branches_with_different_code();
        // Restoring "First" onto alt wrote the files and index, then the
        // commit hit Git's lock: alt and HEAD never moved.
        let source = workspace
            .find_version_commit(first.id())
            .unwrap()
            .tree()
            .unwrap();
        let alt = workspace.branch_tree("alt").unwrap();
        let restored = workspace
            .repo
            .find_tree(workspace.compose_scoped_tree(&alt, &source).unwrap())
            .unwrap();
        workspace
            .repo
            .checkout_tree(restored.as_object(), None)
            .unwrap();
        assert_eq!(content(&workspace, "plan.sk"), "v1");
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: Some(original.clone()),
                target: Some(format!("{} -> alt", first.id().as_str())),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert!(rollback.completed);
        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v2");
        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_targeted_restore_rollback_after_its_save_landed_returns_to_the_original_branch() {
        let (_temp, workspace, original, first) = branches_with_different_code();
        workspace
            .restore_version_as_new_save_to_variation(
                first.id(),
                "Restore onto alt",
                RestoreVersionTarget::Existing {
                    variation: VariationId::from("alt".to_string()),
                },
            )
            .unwrap();
        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        // As if the final record write failed after the save landed.
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: Some(original.clone()),
                target: Some(format!("{} -> alt", first.id().as_str())),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert!(rollback.completed);
        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "code.rs"), "v2");
        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_targeted_restore_rollback_returns_home_even_when_the_trees_match() {
        let (_temp, workspace, first) = two_saves();
        let original = workspace.current_variation_unchecked().unwrap();
        let second = workspace.versions().unwrap()[0].id().clone();
        {
            let first_commit = workspace.find_version_commit(first.id()).unwrap();
            workspace.repo.branch("alt", &first_commit, false).unwrap();
        }
        // Restoring the current version onto alt yields the original's tree.
        workspace
            .restore_version_as_new_save_to_variation(
                &second,
                "Restore onto alt",
                RestoreVersionTarget::Existing {
                    variation: VariationId::from("alt".to_string()),
                },
            )
            .unwrap();
        assert_eq!(workspace.current_variation_unchecked().unwrap(), "alt");
        assert_eq!(
            workspace.branch_tree("alt").unwrap().id(),
            workspace.branch_tree(&original).unwrap().id()
        );
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: Some(original.clone()),
                target: Some(format!("{} -> alt", second.as_str())),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert!(rollback.completed);
        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_clean(&workspace);
    }

    #[test]
    fn the_default_branch_can_be_trunk_or_configured() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "trunk");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        workspace.save_version("First").unwrap();
        assert_eq!(workspace.default_branch().as_deref(), Some("trunk"));

        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        workspace.repo.branch("develop", &head, false).unwrap();
        workspace
            .repo
            .config()
            .unwrap()
            .set_str("init.defaultBranch", "develop")
            .unwrap();
        assert_eq!(workspace.default_branch().as_deref(), Some("develop"));

        workspace
            .repo
            .config()
            .unwrap()
            .set_str("init.defaultBranch", "release")
            .unwrap();
        workspace
            .repo
            .reference("refs/remotes/origin/release", head.id(), false, "test")
            .unwrap();
        assert_eq!(
            workspace.default_branch().as_deref(),
            Some("release"),
            "a configured default that exists only on origin still counts"
        );

        workspace
            .repo
            .config()
            .unwrap()
            .set_str("init.defaultBranch", "develop")
            .unwrap();
        workspace
            .repo
            .reference("refs/remotes/origin/main", head.id(), false, "test")
            .unwrap();
        assert_eq!(
            workspace.default_branch().as_deref(),
            Some("main"),
            "a branch origin has beats a local-only configured default"
        );
    }

    #[test]
    fn a_discard_waits_out_a_brief_index_lock() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        lock_index_briefly_after_precheck();

        workspace.discard_changes().unwrap();

        assert_eq!(content(&workspace, "plan.sk"), "v2");
        assert_clean(&workspace);
    }

    #[test]
    fn a_developer_commit_that_lands_mid_save_is_never_reverted() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_parent_read", move |_| {
            let repo = Repository::open(&root).unwrap();
            fs::write(root.join("code.rs"), b"dev").unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("code.rs")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            let signature = repo.signature().unwrap();
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "dev",
                &tree,
                &[&parent],
            )
            .unwrap();
        });

        workspace.save_version("Third").unwrap();

        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary().unwrap(), Some("Third"));
        assert_eq!(head.parent(0).unwrap().summary().unwrap(), Some("dev"));
        let tree = head.tree().unwrap();
        assert!(tree.get_name("code.rs").is_some(), "keeps the dev commit");
        let plan = workspace
            .repo
            .find_blob(tree.get_name("plan.sk").unwrap().id());
        assert_eq!(plan.unwrap().content(), b"edit");
        assert_clean(&workspace);
    }

    #[test]
    fn a_save_refuses_a_switch_to_another_branch_at_the_same_commit() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        workspace.repo.branch("feature/x", &head, false).unwrap();
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_parent_read", move |_| {
            Repository::open(&root)
                .unwrap()
                .set_head("refs/heads/feature/x")
                .unwrap();
        });

        let error = workspace.save_version("Third").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { ref actual, .. } if actual == "refs/heads/feature/x"),
            "{error:?}"
        );
        assert_eq!(
            workspace
                .repo
                .refname_to_id("refs/heads/feature/x")
                .unwrap(),
            head.id()
        );
    }

    #[test]
    fn a_save_refuses_a_switch_to_another_branch_while_it_scans_changes() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        workspace.repo.branch("feature/x", &head, false).unwrap();
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_branch_pinned", move |_| {
            Repository::open(&root)
                .unwrap()
                .set_head("refs/heads/feature/x")
                .unwrap();
        });

        let error = workspace.save_version("Third").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { ref actual, .. } if actual == "refs/heads/feature/x"),
            "{error:?}"
        );
        assert_eq!(
            workspace
                .repo
                .refname_to_id("refs/heads/feature/x")
                .unwrap(),
            head.id()
        );
    }

    #[test]
    fn a_save_refuses_when_a_developer_rewinds_the_branch_mid_save() {
        let (_temp, workspace, first) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        let root = workspace.root().to_path_buf();
        let first = first.id().as_str().to_string();
        test_hooks::once("after_parent_read", move |_| {
            let repo = Repository::open(&root).unwrap();
            let target = Oid::from_str(&first).unwrap();
            repo.head()
                .unwrap()
                .set_target(target, "reset: moving to First")
                .unwrap();
        });

        let error = workspace.save_version("Third").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert_eq!(head_summary(&workspace), "First");
        assert_eq!(content(&workspace, "plan.sk"), "edit");
    }

    #[test]
    fn a_save_refuses_when_a_developer_switches_branch_mid_save() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_parent_read", move |_| {
            let repo = Repository::open(&root).unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            let signature = repo.signature().unwrap();
            let tree = parent.tree().unwrap();
            let oid = repo
                .commit(None, &signature, &signature, "dev", &tree, &[&parent])
                .unwrap();
            repo.branch("feature", &repo.find_commit(oid).unwrap(), false)
                .unwrap();
            repo.set_head("refs/heads/feature").unwrap();
        });

        let error = workspace.save_version("Third").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert_eq!(head_summary(&workspace), "dev");
        assert_eq!(content(&workspace, "plan.sk"), "edit");
    }

    #[test]
    fn a_save_whose_edit_a_developer_already_committed_refuses_instead_of_saving_nothing() {
        let (_temp, workspace, _) = two_saves();
        write_file(workspace.root(), "plan.sk", b"edit");
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_parent_read", move |_| {
            let repo = Repository::open(&root).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("plan.sk")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            let signature = repo.signature().unwrap();
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "dev",
                &tree,
                &[&parent],
            )
            .unwrap();
        });

        let error = workspace.save_version("Third").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert_eq!(head_summary(&workspace), "dev");
        assert_clean(&workspace);
    }

    #[test]
    fn a_switch_stops_when_a_developer_switches_to_an_identical_branch_while_it_waits() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"v1");
        workspace.save_version("First").unwrap();
        let alternate = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "plan.sk", b"v2");
        workspace.save_version("Second").unwrap();
        let root = workspace.root().to_path_buf();
        test_hooks::once("after_index_precheck", |git_dir| {
            fs::write(git_dir.join("index.lock"), b"").unwrap();
        });
        // The developer switches while Draftline waits for Git's lock.
        test_hooks::once("git_busy_retry", move |git_dir| {
            let repo = Repository::open(&root).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("twin", &head, false).unwrap();
            repo.set_head("refs/heads/twin").unwrap();
            fs::remove_file(git_dir.join("index.lock")).unwrap();
        });

        let error = workspace
            .switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();

        assert!(matches!(error, DraftlineError::GitIndexLocked), "{error:?}");
        assert!(workspace.recovery_state().unwrap().is_some());
        let head = workspace.repo.head().unwrap();
        assert_eq!(head.name().ok(), Some("refs/heads/twin"));
    }

    #[test]
    fn a_switch_rollback_keeps_an_edit_made_while_it_waits_for_git() {
        let (_temp, workspace, first) = two_saves();
        let original = workspace.current_variation_unchecked().unwrap();
        let commit = workspace
            .repo
            .find_commit(first.id().as_str().parse().unwrap());
        workspace
            .repo
            .branch("alt", &commit.unwrap(), false)
            .unwrap();
        workspace.repo.set_head("refs/heads/alt").unwrap();
        let mut checkout = CheckoutBuilder::new();
        workspace
            .repo
            .checkout_head(Some(checkout.force()))
            .unwrap();
        assert_eq!(content(&workspace, "plan.sk"), "v1");
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: Some(original.clone()),
                target: Some("alt".to_string()),
                completed: false,
            })
            .unwrap();
        let plan = workspace.root().join("plan.sk");
        test_hooks::once("after_index_precheck", move |git_dir| {
            let lock = git_dir.join("index.lock");
            fs::write(&lock, b"").unwrap();
            let plan = plan.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                fs::write(plan, b"theirs").unwrap();
                fs::remove_file(lock).unwrap();
            });
        });

        workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(workspace.current_variation_unchecked().unwrap(), original);
        assert_eq!(content(&workspace, "plan.sk"), "theirs");
        let status = workspace.repo.status_file(Path::new("plan.sk")).unwrap();
        assert_eq!(status, git2::Status::WT_MODIFIED);
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn a_save_never_reverts_a_commit_that_lands_while_it_is_built() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        let base = workspace.repo.head().unwrap().peel_to_commit().unwrap();

        // A developer commit lands after the save read HEAD.
        write_file(workspace.root(), "code.rs", b"dev");
        stage_file(&workspace, "code.rs");
        let signature = workspace.repo.signature().unwrap();
        let dev_tree = workspace
            .repo
            .find_tree(workspace.repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        workspace
            .repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "dev",
                &dev_tree,
                &[&base],
            )
            .unwrap();

        write_file(workspace.root(), "plan.sk", b"edited");
        let changed = vec![ChangedFile {
            path: PathBuf::from("plan.sk"),
            kind: ChangeKind::Modified,
            is_binary: false,
            is_large: false,
        }];
        let (tree_id, _) = workspace
            .content_changes_tree(Some(&base), &changed, "save")
            .unwrap();
        let tree = workspace.repo.find_tree(tree_id).unwrap();
        assert!(
            tree.get_name("code.rs").is_none(),
            "tree must come from the save's parent"
        );
        let saved = workspace
            .repo
            .find_commit(
                workspace
                    .repo
                    .commit(None, &signature, &signature, "save", &tree, &[&base])
                    .unwrap(),
            )
            .unwrap();

        let branch = workspace.repo.head().unwrap().name().unwrap().to_string();
        let moved = workspace.advance_head(&branch, &saved, Some(&base), &signature);
        assert!(matches!(
            moved,
            Err(DraftlineError::LocalStateChanged { .. })
        ));
        let head = workspace.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary().unwrap(), Some("dev"));
    }

    #[test]
    fn init_defaults_to_main_instead_of_libgit2_master() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        assert_eq!(
            workspace.current_variation().unwrap(),
            default_initial_variation_name()
        );
    }

    #[test]
    fn open_preserves_existing_git_main_branch_as_variation_name() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "main");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"# Main");
        workspace.save_version("Main draft").unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "main");
        let summaries = workspace.variation_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].variation.name, "main");
    }

    #[test]
    fn rename_variation_migrates_current_branch_metadata_and_preserves_archive() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "master");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"# Master");
        let saved = workspace.save_version("Master draft").unwrap();
        workspace
            .set_variation_metadata(
                &VariationId::from("master"),
                VariationMetadata::new()
                    .with_label("Primary")
                    .with_slug("primary"),
            )
            .unwrap();

        let preflight = workspace
            .preflight_rename_variation(&VariationId::from("master"), &VariationId::from("main"))
            .unwrap();
        assert_eq!(preflight.source_variation.as_str(), "master");
        assert_eq!(preflight.target_variation.as_str(), "main");
        assert_eq!(preflight.expected_oid, saved.id().as_str());
        assert!(preflight
            .support_ref
            .starts_with("refs/draftline/deleted-variations/master/"));

        let renamed = workspace
            .rename_variation_with_token(preflight.token.clone())
            .unwrap();
        assert_eq!(renamed.name, "main");
        assert!(renamed.is_current);
        assert_eq!(renamed.metadata.label.as_deref(), Some("Primary"));
        assert_eq!(workspace.current_variation().unwrap(), "main");
        assert!(workspace
            .repo
            .find_branch("master", BranchType::Local)
            .is_err());
        assert_eq!(
            workspace
                .repo
                .refname_to_id("refs/heads/main")
                .unwrap()
                .to_string(),
            saved.id().as_str()
        );
        assert_eq!(
            workspace
                .repo
                .refname_to_id(&preflight.support_ref)
                .unwrap()
                .to_string(),
            saved.id().as_str()
        );
        assert_eq!(
            workspace
                .variation_metadata(&VariationId::from("main"))
                .unwrap()
                .slug
                .as_deref(),
            Some("primary")
        );
        assert_eq!(
            workspace.read_variation_metadata("master").unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn rename_variation_rejects_stale_or_tampered_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "master");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"# Master");
        workspace.save_version("Master draft").unwrap();

        let preflight = workspace
            .preflight_rename_variation(&VariationId::from("master"), &VariationId::from("main"))
            .unwrap();

        write_file(workspace.root(), "post.md", b"# Master\nupdated");
        workspace.save_version("Updated draft").unwrap();
        assert!(matches!(
            workspace.rename_variation_with_token(preflight.token.clone()),
            Err(DraftlineError::LocalStateChanged { .. })
        ));

        let fresh = workspace
            .preflight_rename_variation(&VariationId::from("master"), &VariationId::from("main"))
            .unwrap();
        let mut tampered = fresh.token.clone();
        tampered.support_ref = "refs/heads/not-an-archive".to_string();
        assert!(matches!(
            workspace.rename_variation_with_token(tampered),
            Err(DraftlineError::LocalStateChanged { .. })
        ));
    }

    #[test]
    fn repair_rename_recovery_preserves_metadata_after_ref_move() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "master");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"# Master");
        let saved = workspace.save_version("Master draft").unwrap();
        let metadata = VariationMetadata::new()
            .with_label("Primary")
            .with_slug("primary");
        workspace
            .set_variation_metadata(&VariationId::from("master"), metadata.clone())
            .unwrap();
        let preflight = workspace
            .preflight_rename_variation(&VariationId::from("master"), &VariationId::from("main"))
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: preflight.token.operation_id.clone(),
                operation: RecoveryOperation::RenameVariation,
                original_variation: Some("master".to_string()),
                target: Some("main".to_string()),
                completed: false,
            })
            .unwrap();
        workspace
            .ensure_archive_ref(
                &preflight.support_ref,
                Oid::from_str(saved.id().as_str()).unwrap(),
                "archive renamed variation",
            )
            .unwrap();
        workspace.rename_variation_ref("master", "main").unwrap();
        workspace
            .write_variation_metadata("master", &metadata)
            .unwrap();
        workspace.clear_variation_metadata("main").unwrap();

        let repair = workspace
            .repair_recovery(&preflight.token.operation_id)
            .unwrap();

        assert!(repair.completed);
        assert_eq!(
            workspace
                .variation_metadata(&VariationId::from("main"))
                .unwrap(),
            metadata
        );
        assert_eq!(
            workspace.read_variation_metadata("master").unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn rollback_rename_recovery_preserves_source_metadata_after_ref_move() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "master");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"# Master");
        let saved = workspace.save_version("Master draft").unwrap();
        let metadata = VariationMetadata::new()
            .with_label("Primary")
            .with_slug("primary");
        workspace
            .set_variation_metadata(&VariationId::from("master"), metadata.clone())
            .unwrap();
        let preflight = workspace
            .preflight_rename_variation(&VariationId::from("master"), &VariationId::from("main"))
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: preflight.token.operation_id.clone(),
                operation: RecoveryOperation::RenameVariation,
                original_variation: Some("master".to_string()),
                target: Some("main".to_string()),
                completed: false,
            })
            .unwrap();
        workspace
            .ensure_archive_ref(
                &preflight.support_ref,
                Oid::from_str(saved.id().as_str()).unwrap(),
                "archive renamed variation",
            )
            .unwrap();
        workspace.rename_variation_ref("master", "main").unwrap();
        workspace
            .write_variation_metadata("master", &metadata)
            .unwrap();
        workspace.clear_variation_metadata("main").unwrap();

        let rollback = workspace
            .rollback_recovery(&preflight.token.operation_id)
            .unwrap();

        assert!(rollback.completed);
        assert_eq!(workspace.current_variation().unwrap(), "master");
        assert!(workspace
            .repo
            .find_branch("main", BranchType::Local)
            .is_err());
        assert_eq!(
            workspace
                .variation_metadata(&VariationId::from("master"))
                .unwrap(),
            metadata
        );
        assert_eq!(
            workspace.read_variation_metadata("main").unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn rollback_rename_recovery_restores_metadata_after_target_write() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = init_workspace_with_initial_branch(temp.path(), "master");
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"# Master");
        let saved = workspace.save_version("Master draft").unwrap();
        let metadata = VariationMetadata::new()
            .with_label("Primary")
            .with_slug("primary");
        workspace
            .set_variation_metadata(&VariationId::from("master"), metadata.clone())
            .unwrap();
        let preflight = workspace
            .preflight_rename_variation(&VariationId::from("master"), &VariationId::from("main"))
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: preflight.token.operation_id.clone(),
                operation: RecoveryOperation::RenameVariation,
                original_variation: Some("master".to_string()),
                target: Some("main".to_string()),
                completed: false,
            })
            .unwrap();
        workspace
            .ensure_archive_ref(
                &preflight.support_ref,
                Oid::from_str(saved.id().as_str()).unwrap(),
                "archive renamed variation",
            )
            .unwrap();
        workspace.rename_variation_ref("master", "main").unwrap();
        workspace
            .write_variation_metadata("main", &metadata)
            .unwrap();
        workspace.clear_variation_metadata("master").unwrap();

        let rollback = workspace
            .rollback_recovery(&preflight.token.operation_id)
            .unwrap();

        assert!(rollback.completed);
        assert_eq!(workspace.current_variation().unwrap(), "master");
        assert!(workspace
            .repo
            .find_branch("main", BranchType::Local)
            .is_err());
        assert_eq!(
            workspace
                .variation_metadata(&VariationId::from("master"))
                .unwrap(),
            metadata
        );
        assert_eq!(
            workspace.read_variation_metadata("main").unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn saves_and_lists_versions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");
        configure_identity(&workspace, "Seth", "seth@example.com");
        let saved = workspace.save_version("Homepage draft").unwrap();

        let versions = workspace.versions().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id(), saved.id());
        assert_eq!(versions[0].label, "Homepage draft");
        assert_eq!(versions[0].author.name, "Seth");
    }

    #[test]
    fn save_files_commits_only_selected_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one saved");
        write_file(workspace.root(), "two.md", b"two unsaved");

        let report = workspace.preflight_save_files(["one.md"]).unwrap();
        assert!(report.can_proceed);
        assert_eq!(report.dirty_files.len(), 1);
        assert_eq!(report.dirty_files[0].path, PathBuf::from("one.md"));

        workspace.save_files(["one.md"], "Save one").unwrap();

        assert_eq!(workspace.changed_files().unwrap().len(), 1);
        assert_eq!(
            workspace.changed_files().unwrap()[0].path,
            PathBuf::from("two.md")
        );
        let preview = workspace
            .preview_version(workspace.versions().unwrap()[0].id())
            .unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("one.md"))
                .and_then(|file| file.content.as_deref()),
            Some("one saved")
        );
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("two.md"))
                .and_then(|file| file.content.as_deref()),
            Some("two")
        );
    }

    #[test]
    fn save_files_preserves_unselected_staged_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one saved");
        write_file(workspace.root(), "two.md", b"two staged");
        stage_file(&workspace, "two.md");

        workspace.save_files(["one.md"], "Save one").unwrap();

        let two_status = workspace.repo.status_file(Path::new("two.md")).unwrap();
        assert!(two_status.contains(Status::INDEX_MODIFIED));
        assert!(!two_status.contains(Status::WT_MODIFIED));
        assert_eq!(
            fs::read_to_string(workspace.root().join("two.md")).unwrap(),
            "two staged"
        );
        let preview = workspace
            .preview_version(workspace.versions().unwrap()[0].id())
            .unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("one.md"))
                .and_then(|file| file.content.as_deref()),
            Some("one saved")
        );
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("two.md"))
                .and_then(|file| file.content.as_deref()),
            Some("two")
        );
    }

    #[test]
    fn save_files_preserves_unselected_staged_rename() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "old.md", b"old");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one saved");
        stage_rename(&workspace, "old.md", "renamed.md");

        workspace.save_files(["one.md"], "Save one").unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("renamed.md")).unwrap(),
            "old"
        );
        let changed = workspace.changed_files().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("old.md"));
        assert_eq!(changed[0].kind, ChangeKind::Renamed);
        let preview = workspace
            .preview_version(workspace.versions().unwrap()[0].id())
            .unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("one.md"))
                .and_then(|file| file.content.as_deref()),
            Some("one saved")
        );
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("old.md"))
                .and_then(|file| file.content.as_deref()),
            Some("old")
        );
        assert!(!preview
            .files
            .iter()
            .any(|file| file.path == Path::new("renamed.md")));
    }

    #[test]
    fn save_files_commits_selected_staged_rename() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "old.md", b"old");
        workspace.save_version("Base").unwrap();
        stage_rename(&workspace, "old.md", "renamed.md");

        workspace.save_files(["old.md"], "Save rename").unwrap();

        assert!(workspace.changed_files().unwrap().is_empty());
        let preview = workspace
            .preview_version(workspace.versions().unwrap()[0].id())
            .unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("renamed.md"))
                .and_then(|file| file.content.as_deref()),
            Some("old")
        );
        assert!(!preview
            .files
            .iter()
            .any(|file| file.path == Path::new("old.md")));
    }

    #[test]
    fn content_policy_excludes_runtime_state_from_changes_and_versions() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new()
            .include("content")
            .unwrap()
            .include_extension("draft")
            .unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/post.md", b"# Hello");
        write_file(workspace.root(), "root-note.draft", br#"{"title":"Root"}"#);
        write_file(workspace.root(), "ui-state/panel.json", br#"{"open":true}"#);
        let version = workspace.save_version("Content only").unwrap();

        let preview = workspace.preview_version(version.id()).unwrap();
        assert_eq!(preview.files.len(), 2);
        assert!(preview
            .files
            .iter()
            .any(|file| file.path == PathBuf::from("content").join("post.md")));
        assert!(preview
            .files
            .iter()
            .any(|file| file.path == Path::new("root-note.draft")));
        assert!(preview
            .files
            .iter()
            .all(|file| file.path != PathBuf::from("ui-state").join("panel.json")));
    }

    #[test]
    fn detects_unsaved_changes_as_changeset() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");

        let changes = workspace.changes().unwrap();
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, PathBuf::from("post.md"));
        assert_eq!(changes.files[0].kind, ChangeKind::Added);
        assert!(!changes.files[0].is_binary);
    }

    #[test]
    fn discard_changes_restores_tracked_content_and_preserves_excluded_files() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/modified.md", b"base");
        write_file(workspace.root(), "content/deleted.md", b"delete me");
        workspace.save_version("Base").unwrap();

        write_file(workspace.root(), "content/modified.md", b"dirty");
        fs::remove_file(workspace.root().join("content").join("deleted.md")).unwrap();
        write_file(workspace.root(), "content/added.md", b"new");
        write_file(workspace.root(), "ui-state/panel.json", b"keep me");

        let report = workspace.preflight_discard_changes().unwrap();
        assert_eq!(report.operation, "discard_changes");
        assert!(report.can_proceed);
        assert_eq!(report.dirty_files.len(), 3);

        let discarded = workspace.discard_changes().unwrap();

        assert_eq!(discarded.files.len(), 3);
        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("modified.md")).unwrap(),
            "base"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("deleted.md")).unwrap(),
            "delete me"
        );
        assert!(!workspace.root().join("content").join("added.md").exists());
        assert_eq!(
            fs::read_to_string(workspace.root().join("ui-state").join("panel.json")).unwrap(),
            "keep me"
        );
        assert!(workspace.changed_files().unwrap().is_empty());
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn discard_file_restores_only_the_requested_tracked_file() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/one.md", b"one");
        write_file(workspace.root(), "content/two.md", b"two");
        workspace.save_version("Base").unwrap();

        write_file(workspace.root(), "content/one.md", b"dirty one");
        write_file(workspace.root(), "content/two.md", b"dirty two");

        let report = workspace
            .preflight_discard_file("./content/one.md")
            .unwrap();
        assert_eq!(report.operation, "discard_file");
        assert_eq!(
            report.dirty_files[0].path,
            PathBuf::from("content").join("one.md")
        );

        let discarded = workspace.discard_file("./content/one.md").unwrap().unwrap();

        assert_eq!(discarded.path, PathBuf::from("content").join("one.md"));
        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("one.md")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("two.md")).unwrap(),
            "dirty two"
        );
        assert_eq!(
            workspace.changed_files().unwrap()[0].path,
            PathBuf::from("content").join("two.md")
        );
    }

    #[test]
    fn discard_files_restores_only_selected_tracked_files() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        write_file(workspace.root(), "three.md", b"three");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one dirty");
        write_file(workspace.root(), "two.md", b"two dirty");
        write_file(workspace.root(), "three.md", b"three dirty");

        let report = workspace
            .preflight_discard_files(["one.md", "two.md"])
            .unwrap();
        assert!(report.can_proceed);
        assert_eq!(report.dirty_files.len(), 2);

        let discarded = workspace.discard_files(["one.md", "two.md"]).unwrap();

        assert_eq!(discarded.files.len(), 2);
        assert_eq!(
            fs::read_to_string(workspace.root().join("one.md")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("two.md")).unwrap(),
            "two"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("three.md")).unwrap(),
            "three dirty"
        );
        let changed = workspace.changed_files().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("three.md"));
    }

    #[test]
    fn discard_files_preserves_unselected_staged_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one dirty");
        write_file(workspace.root(), "two.md", b"two staged");
        stage_file(&workspace, "two.md");

        workspace.discard_files(["one.md"]).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("one.md")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("two.md")).unwrap(),
            "two staged"
        );
        let two_status = workspace.repo.status_file(Path::new("two.md")).unwrap();
        assert!(two_status.contains(Status::INDEX_MODIFIED));
        assert!(!two_status.contains(Status::WT_MODIFIED));
    }

    #[test]
    fn discard_file_removes_added_tracked_file() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/base.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "content/added.md", b"new");

        let discarded = workspace.discard_file("content/added.md").unwrap().unwrap();

        assert_eq!(discarded.kind, ChangeKind::Added);
        assert!(!workspace.root().join("content").join("added.md").exists());
        assert!(workspace.changed_files().unwrap().is_empty());
    }

    #[test]
    fn discard_file_restores_staged_rename() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/old.md", b"old");
        workspace.save_version("Base").unwrap();

        fs::rename(
            workspace.root().join("content").join("old.md"),
            workspace.root().join("content").join("new.md"),
        )
        .unwrap();
        let mut index = workspace.repo.index().unwrap();
        index
            .remove_path(Path::new("content").join("old.md").as_path())
            .unwrap();
        index
            .add_path(Path::new("content").join("new.md").as_path())
            .unwrap();
        index.write().unwrap();

        let changed = workspace.changed_files().unwrap();
        assert_eq!(changed[0].kind, ChangeKind::Renamed);

        workspace.discard_file("content/old.md").unwrap().unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("old.md")).unwrap(),
            "old"
        );
        assert!(!workspace.root().join("content").join("new.md").exists());
        assert!(workspace.changed_files().unwrap().is_empty());
    }

    #[test]
    fn discard_file_rejects_paths_outside_workspace_or_content_policy() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        let err = workspace.discard_file("../secret.md").unwrap_err();
        assert!(matches!(err, DraftlineError::PathEscapesWorkspace(_)));

        let err = workspace.discard_file("ui-state/panel.json").unwrap_err();
        assert!(matches!(err, DraftlineError::PathOutsideContentPolicy(_)));
    }

    #[test]
    fn discard_operations_respect_recovery_and_operation_lock() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"dirty");

        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::DiscardChanges,
                original_variation: Some("main".to_string()),
                target: None,
                completed: false,
            })
            .unwrap();

        let err = workspace.discard_changes().unwrap_err();
        assert!(matches!(err, DraftlineError::RecoveryRequired(_)));

        workspace.acknowledge_recovery().unwrap();
        fs::write(workspace.lock_path(), b"locked").unwrap();

        let err = workspace.discard_file("post.md").unwrap_err();
        assert!(matches!(err, DraftlineError::WorkspaceLocked));

        fs::remove_file(workspace.lock_path()).unwrap();
        assert!(workspace.discard_changes().is_ok());
    }

    #[test]
    fn preflight_reports_binary_and_large_files() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().with_large_file_threshold(3);
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "asset.bin", &[0, 1, 2, 3]);
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "asset.bin", &[0, 1, 2, 3, 4]);

        let report = workspace
            .preflight_switch_variation(variation.id())
            .unwrap();
        assert_eq!(report.binary_files, vec![PathBuf::from("asset.bin")]);
        assert_eq!(report.large_files, vec![PathBuf::from("asset.bin")]);
    }

    #[test]
    fn variation_names_reserve_draftline_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();

        for name in ["draftline", "draftline/feature"] {
            let err = workspace.create_variation(name).unwrap_err();
            assert!(matches!(err, DraftlineError::InvalidVariationName(_)));
        }

        workspace.create_variation("draftline-feature").unwrap();
    }

    #[test]
    fn refuses_to_switch_variations_with_unsaved_changes_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");
        workspace.save_version("First draft").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "post.md", b"# Unsaved");

        let err = workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();
        assert!(matches!(err, DraftlineError::PreflightFailed(_)));
    }

    #[test]
    fn save_first_policy_preserves_work_before_switching_variations() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");
        workspace.save_version("First draft").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "post.md", b"# Save me");

        workspace
            .switch_variation(
                variation.id(),
                SwitchPolicy::SaveFirst {
                    label: "Saved before switch".to_string(),
                },
            )
            .unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "alternate");
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn recovery_state_blocks_normal_operations_until_acknowledged() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: Some("main".to_string()),
                target: Some("alternate".to_string()),
                completed: false,
            })
            .unwrap();

        let err = workspace.changes().unwrap_err();
        assert!(matches!(err, DraftlineError::RecoveryRequired(_)));

        workspace.acknowledge_recovery().unwrap();
        assert!(workspace.changes().is_ok());
    }

    #[test]
    fn shelve_policy_holds_dirty_content_without_replaying_it() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "content/post.md", b"dirty");
        write_file(workspace.root(), "ui-state/panel.json", b"keep me");

        workspace
            .switch_variation(
                variation.id(),
                SwitchPolicy::Shelve {
                    name: "before-alternate".to_string(),
                },
            )
            .unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "alternate");
        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("post.md")).unwrap(),
            "base"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("ui-state").join("panel.json")).unwrap(),
            "keep me"
        );
        assert!(workspace
            .repo
            .find_reference("refs/draftline/shelves/before-alternate")
            .is_ok());
    }

    #[test]
    fn previews_version_without_changing_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let preview = workspace.preview_version(first.id()).unwrap();

        assert_eq!(preview.files[0].content.as_deref(), Some("first"));
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn previews_one_version_file_without_reading_whole_tree() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include_extension("md").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "post.md", b"hello");
        write_file(workspace.root(), "ui-state.json", b"ignore me");
        let version = workspace.save_version("Post").unwrap();

        let preview = workspace
            .preview_version_file(version.id(), "post.md")
            .unwrap()
            .unwrap();
        assert_eq!(preview.path, PathBuf::from("post.md"));
        assert_eq!(preview.content.as_deref(), Some("hello"));

        assert!(workspace
            .preview_version_file(version.id(), "ui-state.json")
            .unwrap()
            .is_none());
    }

    #[test]
    fn restores_version_as_new_save_without_switching_variations() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let restored = workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "main");
        assert_eq!(restored.label, "Restore first");
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "first"
        );
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn creates_variation_from_version_without_switching() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let variation = workspace
            .create_variation_from(first.id(), "recover-first")
            .unwrap();

        assert_eq!(variation.name, "recover-first");
        assert!(!variation.is_current);
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn delete_variation_archives_tip_before_removing_branch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"alternate only");
        let alternate_version = workspace.save_version("Alternate only").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        let preflight = workspace
            .preflight_delete_variation(variation.id())
            .unwrap();
        assert!(preflight.can_delete);
        assert_eq!(preflight.variation, *variation.id());
        assert_eq!(preflight.target_oid, alternate_version.id().as_str());
        assert!(preflight
            .support_ref
            .starts_with("refs/draftline/deleted-variations/alternate/"));
        workspace
            .delete_variation_with_token(preflight.token)
            .unwrap();

        assert!(workspace
            .repo
            .find_branch("alternate", BranchType::Local)
            .is_err());
        assert!(workspace
            .repo
            .references()
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|reference| {
                reference
                    .name()
                    .map(|name| name.starts_with("refs/draftline/deleted-variations/alternate/"))
                    .unwrap_or(false)
                    && reference.target() == Some(alternate_version.id().as_str().parse().unwrap())
            }));
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn stores_and_lists_variation_display_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        workspace.save_version("First").unwrap();

        let metadata = VariationMetadata::new()
            .with_label("Launch timeline")
            .with_slug("launch-timeline");
        let variation = workspace
            .create_variation_with_metadata("timeline-launch", metadata.clone())
            .unwrap();

        assert_eq!(variation.metadata, metadata);
        assert_eq!(variation.display_label(), "Launch timeline");
        assert_eq!(
            workspace.variation_metadata(variation.id()).unwrap(),
            metadata
        );

        let listed = workspace
            .variations()
            .unwrap()
            .into_iter()
            .find(|variation| variation.name == "timeline-launch")
            .unwrap();
        assert_eq!(listed.metadata.label.as_deref(), Some("Launch timeline"));
        assert_eq!(listed.metadata.slug.as_deref(), Some("launch-timeline"));
    }

    #[test]
    fn updates_and_clears_variation_display_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        workspace.save_version("First").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();

        let updated = workspace
            .set_variation_metadata(
                variation.id(),
                VariationMetadata::new().with_label("Human label"),
            )
            .unwrap();
        assert_eq!(updated.display_label(), "Human label");

        let cleared = workspace
            .set_variation_metadata(variation.id(), VariationMetadata::default())
            .unwrap();
        assert_eq!(cleared.display_label(), "alternate");
        assert_eq!(
            workspace.variation_metadata(variation.id()).unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn adds_and_lists_remote_endpoints() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        workspace
            .add_remote("backup", "https://example.invalid/content.git")
            .unwrap();

        let remotes = workspace.remotes().unwrap();
        assert_eq!(
            remotes,
            vec![RemoteEndpoint {
                name: "backup".to_string(),
                url: "https://example.invalid/content.git".to_string(),
            }]
        );
    }

    #[test]
    fn remote_endpoint_urls_redact_embedded_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        let added = workspace
            .add_remote(
                "backup",
                "https://x-access-token:secret@example.invalid/content.git",
            )
            .unwrap();
        assert_eq!(added.url, "https://example.invalid/content.git");

        let remotes = workspace.remotes().unwrap();
        assert_eq!(remotes[0].url, "https://example.invalid/content.git");
    }

    #[test]
    fn publishes_and_reports_up_to_date_with_local_bare_remote() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();

        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Initial version").unwrap();

        let published = workspace.publish_changes("origin").unwrap();
        assert_eq!(published.published_versions, 1);

        workspace.fetch_remote("origin").unwrap();
        let status = workspace.sync_status("origin").unwrap();
        assert_eq!(status.state, SyncState::UpToDate);
    }

    #[test]
    fn remote_options_work_for_clone_fetch_and_publish() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();

        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Initial version").unwrap();

        let mut publish_options = RemoteOptions::new();
        workspace
            .publish_changes_with_options("origin", &mut publish_options)
            .unwrap();

        let clone = tempfile::tempdir().unwrap();
        let mut clone_options = RemoteOptions::new();
        let cloned = Workspace::clone_workspace_with_options(
            remote.path().to_str().unwrap(),
            clone.path(),
            &mut clone_options,
        )
        .unwrap();

        let mut fetch_options = RemoteOptions::new();
        cloned
            .fetch_remote_with_options("origin", &mut fetch_options)
            .unwrap();
        assert_eq!(
            cloned.sync_status("origin").unwrap().state,
            SyncState::UpToDate
        );
    }

    #[test]
    fn fetch_reports_incoming_versions_and_who_changed_them() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        first_workspace.fetch_remote("origin").unwrap();
        let status = first_workspace.sync_status("origin").unwrap();

        assert_eq!(status.state, SyncState::IncomingAvailable);
        assert_eq!(status.behind, 1);
        assert_eq!(status.incoming[0].label, "Two");
        assert_eq!(status.incoming[0].author.name, "Maria");
    }

    #[test]
    fn publish_refuses_when_remote_has_incoming_changes() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        write_file(first_workspace.root(), "post.md", b"local two");
        first_workspace.save_version("Local two").unwrap();
        first_workspace.fetch_remote("origin").unwrap();

        let err = first_workspace.publish_changes("origin").unwrap_err();
        assert!(matches!(err, DraftlineError::SyncNeedsMerge(_)));
    }

    #[test]
    fn publish_refreshes_remote_before_deciding_safety() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        write_file(first_workspace.root(), "post.md", b"local two");
        first_workspace.save_version("Local two").unwrap();

        let err = first_workspace.publish_changes("origin").unwrap_err();
        assert!(matches!(err, DraftlineError::SyncNeedsMerge(_)));
    }

    #[test]
    fn preflight_publish_reports_expected_absent_remote_for_first_publish() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Initial version").unwrap();

        let preflight = workspace.preflight_publish("origin").unwrap();

        assert!(preflight.can_publish);
        assert!(preflight.expected_remote_oid.is_none());
        assert_eq!(preflight.sync_status.state, SyncState::NoRemoteVersion);

        let published = workspace.publish(preflight.token).unwrap();
        assert_eq!(published.published_versions, 1);
    }

    #[test]
    fn publish_token_refuses_when_remote_appears_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        let preflight = first_workspace.preflight_publish("origin").unwrap();
        assert!(preflight.expected_remote_oid.is_none());

        let second = tempfile::tempdir().unwrap();
        let second_workspace = Workspace::init(second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        second_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        let err = first_workspace.publish(preflight.token).unwrap_err();

        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
    }

    // ── workspace_summary ────────────────────────────────────────────────────

    #[test]
    fn workspace_summary_returns_active_variation_and_versions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("Draft one").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("Draft two").unwrap();

        let summary = workspace.workspace_summary().unwrap();

        assert_eq!(summary.active_variation.name, "main");
        assert!(summary.active_variation.is_current);
        assert_eq!(summary.versions.len(), 2);
        assert_eq!(summary.versions[0].label, "Draft two");
        assert!(!summary.is_dirty);
        assert!(summary.dirty_files.is_empty());
        assert!(summary.recovery.is_none());
    }

    #[test]
    fn workspace_summary_reports_dirty_files() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"unsaved");

        let summary = workspace.workspace_summary().unwrap();

        assert!(summary.is_dirty);
        assert_eq!(summary.dirty_files.len(), 1);
        assert_eq!(summary.dirty_files[0].path, PathBuf::from("post.md"));
    }

    #[test]
    fn workspace_summary_includes_recovery_state_without_error() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: Some("main".to_string()),
                target: Some("alternate".to_string()),
                completed: false,
            })
            .unwrap();

        // summary must succeed even when recovery is pending
        let summary = workspace.workspace_summary().unwrap();
        assert!(summary.recovery.is_some());
        let state = summary.recovery.unwrap();
        assert_eq!(state.operation_id, "interrupted");
    }

    #[test]
    fn workspace_summary_lists_all_variations() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        workspace.create_variation("alt-a").unwrap();
        workspace.create_variation("alt-b").unwrap();

        let summary = workspace.workspace_summary().unwrap();

        assert_eq!(summary.variations.len(), 3);
        let names: Vec<&str> = summary.variations.iter().map(|v| v.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"alt-a"));
        assert!(names.contains(&"alt-b"));
    }

    #[test]
    fn inspect_reports_local_workspace_ready_for_normal_work() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        let inspection = workspace.inspect().unwrap();

        assert_eq!(inspection.workspace_id.root, workspace.root());
        assert_eq!(inspection.sharing_mode, SharingMode::LocalOnly);
        assert_eq!(
            inspection
                .current_variation
                .as_ref()
                .map(VariationId::as_str),
            Some("main")
        );
        assert!(!inspection.dirty.is_dirty);
        assert!(inspection.remotes.is_empty());
        assert!(inspection.recovery.is_none());
        assert_eq!(
            inspection.operation_lock.state,
            OperationLockState::Unlocked
        );
        assert_eq!(
            inspection.safe_next_actions,
            vec![SafeNextAction::NormalWork]
        );
    }

    #[test]
    fn inspect_reports_dirty_remote_workspace_with_safe_next_actions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        workspace
            .add_remote("origin", "https://example.com/draftline.git")
            .unwrap();
        write_file(workspace.root(), "post.md", b"unsaved");

        let inspection = workspace.inspect().unwrap();

        assert_eq!(inspection.sharing_mode, SharingMode::SharedCapable);
        assert_eq!(inspection.remotes.len(), 1);
        assert!(inspection.dirty.is_dirty);
        assert_eq!(inspection.dirty.files[0].path, PathBuf::from("post.md"));
        assert!(inspection
            .safe_next_actions
            .contains(&SafeNextAction::SaveFirst));
    }

    #[test]
    fn inspect_surfaces_recovery_and_lock_without_blocking() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: Some("main".to_string()),
                target: Some("alternate".to_string()),
                completed: false,
            })
            .unwrap();
        fs::write(workspace.lock_path(), b"locked").unwrap();

        let inspection = workspace.inspect().unwrap();

        assert!(inspection.recovery.is_some());
        assert_eq!(inspection.operation_lock.state, OperationLockState::Locked);
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::RecoveryRequired));
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::WorkspaceLocked));
        assert_eq!(
            inspection.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
    }

    #[test]
    fn capabilities_report_supported_and_future_workflows() {
        let capabilities = Workspace::capabilities();

        assert!(capabilities.inspect);
        assert!(capabilities.workspace_summary);
        assert!(capabilities.save_version);
        assert!(capabilities.switch_variation);
        assert!(capabilities.publish_changes);
        assert!(capabilities.apply_incoming);
        assert!(capabilities.stale_lock_repair);
        assert!(capabilities.target_tree_collision_preflight);
        assert!(capabilities.support_ref_sync);
        assert!(capabilities.agent_cli_facade);
    }

    #[test]
    fn inspect_operation_lock_reports_metadata_for_active_lock() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        let _lock = OperationLock::acquire(&workspace.lock_path(), "scenario_test").unwrap();

        let inspection = workspace.inspect_operation_lock().unwrap();

        assert_eq!(inspection.state, OperationLockState::Locked);
        assert!(!inspection.is_stale);
        assert!(!inspection.can_clear);
        let metadata = inspection.metadata.unwrap();
        assert_eq!(metadata.operation, "scenario_test");
        assert_eq!(metadata.process_id, std::process::id());
        assert!(!metadata.operation_id.is_empty());
    }

    #[test]
    fn clear_stale_lock_removes_only_stale_metadata_lock() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        let stale_metadata = OperationLockMetadata {
            operation_id: "stale-op".to_string(),
            operation: "stale_test".to_string(),
            process_id: u32::MAX,
            owner: None,
            created_at_seconds: 1,
            host: None,
        };
        fs::create_dir_all(workspace.draftline_dir()).unwrap();
        fs::write(
            workspace.lock_path(),
            serde_json::to_vec_pretty(&stale_metadata).unwrap(),
        )
        .unwrap();

        let inspection = workspace.inspect_operation_lock().unwrap();
        assert!(inspection.is_stale);
        assert!(inspection.can_clear);

        workspace.clear_stale_lock().unwrap();
        assert!(!workspace.lock_path().exists());

        let _lock = OperationLock::acquire(&workspace.lock_path(), "active_test").unwrap();
        let err = workspace.clear_stale_lock().unwrap_err();
        assert!(matches!(err, DraftlineError::WorkspaceLocked));
        assert!(workspace.lock_path().exists());
    }

    #[test]
    fn a_lock_from_a_crashed_process_clears_right_away() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
        } else {
            std::process::Command::new("cat")
        }
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
        let mut metadata = new_operation_lock_metadata("crash_test");
        metadata.process_id = child.id();
        fs::create_dir_all(workspace.draftline_dir()).unwrap();
        let write_lock = |metadata: &OperationLockMetadata| {
            fs::write(
                workspace.lock_path(),
                serde_json::to_vec_pretty(metadata).unwrap(),
            )
            .unwrap()
        };
        write_lock(&metadata);

        assert!(!workspace.inspect_operation_lock().unwrap().is_stale);
        assert!(matches!(
            workspace.clear_stale_lock().unwrap_err(),
            DraftlineError::WorkspaceLocked
        ));

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(workspace.inspect_operation_lock().unwrap().is_stale);

        // Another machine's lock can't be judged by process id.
        metadata.host = Some("another-machine".to_string());
        write_lock(&metadata);
        assert!(!workspace.inspect_operation_lock().unwrap().is_stale);

        // Nor can a lock that never recorded its machine.
        metadata.host = None;
        write_lock(&metadata);
        assert!(!workspace.inspect_operation_lock().unwrap().is_stale);

        // Same machine name, other OS (Windows vs WSL): other process ids.
        let host = current_host().unwrap();
        let (_, name) = host.split_once(':').unwrap();
        let other = if cfg!(windows) { "unix" } else { "windows" };
        metadata.host = Some(format!("{other}:{name}"));
        write_lock(&metadata);
        assert!(!workspace.inspect_operation_lock().unwrap().is_stale);

        metadata.host = current_host();
        write_lock(&metadata);
        workspace.clear_stale_lock().unwrap();
        assert!(!workspace.lock_path().exists());
    }

    #[test]
    fn inspect_operation_lock_treats_legacy_lock_as_unknown_not_clearable() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        fs::create_dir_all(workspace.draftline_dir()).unwrap();
        fs::write(workspace.lock_path(), b"locked").unwrap();

        let inspection = workspace.inspect_operation_lock().unwrap();

        assert_eq!(inspection.state, OperationLockState::Locked);
        assert!(inspection.metadata.is_none());
        assert!(!inspection.is_stale);
        assert!(!inspection.can_clear);
    }

    #[test]
    fn repair_recovery_finishes_discard_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"dirty");
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::DiscardChanges,
                original_variation: Some("main".to_string()),
                target: None,
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery("interrupted").unwrap();

        assert_eq!(repair.operation_id, "interrupted");
        assert_eq!(repair.operation, RecoveryOperation::DiscardChanges);
        assert!(repair.completed);
        assert!(repair.changed_workspace);
        assert_eq!(repair.safe_next_actions, vec![SafeNextAction::NormalWork]);
        assert!(workspace.changed_files().unwrap().is_empty());
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn repair_recovery_completes_apply_incoming_fast_forward() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"incoming");
        let incoming = workspace.save_version("Incoming").unwrap();
        let base_oid = Oid::from_str(base.id().as_str()).unwrap();
        workspace
            .repo
            .reference(
                "refs/heads/main",
                base_oid,
                true,
                "simulate interrupted apply",
            )
            .unwrap();
        workspace
            .repo
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::ApplyIncoming,
                original_variation: Some("main".to_string()),
                target: Some(incoming.id().as_str().to_string()),
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery("interrupted").unwrap();

        assert!(repair.completed);
        assert!(repair.changed_workspace);
        assert_eq!(workspace.current_variation().unwrap(), "main");
        assert_eq!(
            workspace.repo.head().unwrap().target().unwrap().to_string(),
            incoming.id().as_str()
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "incoming"
        );
    }

    /// A root folder whose move to `deck` crashes at `point`.
    fn interrupted_move(point: &'static str) -> (tempfile::TempDir, String) {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        test_hooks::once(point, |_| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            workspace.move_folder("deck").map(|_| ())
        }));
        assert!(crashed.is_err());
        let operation = workspace.recovery_state().unwrap().unwrap().operation_id;
        (temp, operation)
    }

    #[test]
    fn a_move_interrupted_before_its_save_goes_back_to_the_old_folder() {
        let (temp, operation) = interrupted_move("after_folder_move_checkout");
        assert!(temp.path().join("deck").join("plan.sk").is_file());
        let probe = Workspace::probe(temp.path().join("deck")).unwrap();
        assert_eq!(probe.state, FolderState::Ready);
        assert_eq!(probe.suggestions[0].fix, FolderFix::Open);

        let workspace = Workspace::open(temp.path().join("deck")).unwrap();
        let repair = workspace.repair_recovery(&operation).unwrap();
        assert!(repair.completed);
        assert!(temp.path().join("plan.sk").is_file());
        assert!(!temp.path().join("deck").exists());
        let root = Workspace::open(temp.path()).unwrap();
        assert!(root.recovery_state().unwrap().is_none());
        assert_eq!(
            Workspace::probe(temp.path()).unwrap().state,
            FolderState::Ready
        );
    }

    #[test]
    fn a_move_interrupted_after_its_save_finishes_in_the_new_folder() {
        let (temp, operation) = interrupted_move("after_folder_move_commit");
        let workspace = Workspace::open(temp.path()).unwrap();
        workspace.rollback_recovery(&operation).unwrap();
        assert!(temp.path().join("deck").join("plan.sk").is_file());
        let probe = Workspace::probe(temp.path()).unwrap();
        assert_eq!(probe.state, FolderState::ManagedElsewhere);
        assert_eq!(probe.folder.as_deref(), Some(Path::new("deck")));
        let deck = Workspace::open(temp.path().join("deck")).unwrap();
        assert!(deck.recovery_state().unwrap().is_none());
    }

    #[test]
    fn a_folder_is_created_inside_its_root_and_never_through_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let made = create_folder_within(temp.path(), Path::new("a/b")).unwrap();
        assert_eq!(made, temp.path().join("a").join("b"));
        assert!(made.is_dir());
        create_folder_within(temp.path(), Path::new("a/b")).unwrap();

        write_file(temp.path(), "file", b"x");
        assert!(matches!(
            create_folder_within(temp.path(), Path::new("file/inner")),
            Err(DraftlineError::InvalidFolderMarker(_))
        ));
        assert!(!temp.path().join("file").join("inner").exists());

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), temp.path().join("link")).unwrap();
            assert!(create_folder_within(temp.path(), Path::new("link/inner")).is_err());
            assert!(!outside.path().join("inner").exists());
        }
    }

    #[test]
    fn folder_fixes_stop_once_another_folder_took_over() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        write_file(temp.path(), "README.md", b"app");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        index.write().unwrap();
        let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "App", &tree, &[])
            .unwrap();
        let workspace = Workspace::init(temp.path().join("deck")).unwrap();
        write_file(workspace.root(), "deck/plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        let branch = workspace.current_variation().unwrap();
        fs::remove_dir_all(temp.path().join("deck")).unwrap();
        // The handle a fix would use, taken before the race.
        let managed =
            Workspace::located_in(&temp.path().join("deck"), ContentPolicy::default(), None)
                .unwrap()
                .unwrap();

        // Meanwhile another folder takes over.
        let marker = fs::read(temp.path().join(FOLDER_MARKER)).unwrap();
        write_file(temp.path(), FOLDER_MARKER, &marker_bytes("other").unwrap());
        for result in [
            managed.restore_missing_folder(),
            managed.switch_to_folder_branch(&branch),
        ] {
            assert!(
                matches!(&result, Err(DraftlineError::FolderFixUnavailable(reason))
                    if reason.contains("another folder")),
                "{:?}",
                result.err()
            );
        }
        assert!(!temp.path().join("deck").exists());

        fs::write(temp.path().join(FOLDER_MARKER), marker).unwrap();
        managed.restore_missing_folder().unwrap();
        assert_eq!(fs::read(temp.path().join("deck/plan.sk")).unwrap(), b"base");
    }

    #[test]
    fn a_recovered_move_brings_ignored_leftovers_along() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        write_file(temp.path(), "README.md", b"app");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        index.write().unwrap();
        let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "App", &tree, &[])
            .unwrap();
        let mut workspace = Workspace::init(temp.path().join("deck")).unwrap();
        write_file(workspace.root(), "deck/plan.sk", b"base");
        write_file(workspace.root(), "deck/.gitignore", b"cache/\n");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "deck/cache/thumb.bin", b"cache");
        test_hooks::once("after_folder_move_commit", |_| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            workspace.move_folder("pitch").map(|_| ())
        }));
        assert!(crashed.is_err());
        let operation = workspace.recovery_state().unwrap().unwrap().operation_id;
        assert!(temp.path().join("deck/cache/thumb.bin").is_file());
        // A developer commits a file into the old folder after the crash.
        write_file(temp.path(), "deck/dev.txt", b"dev");
        let mut index = repo.index().unwrap();
        index.read(false).unwrap();
        index.add_path(Path::new("deck/dev.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "Dev", &tree, &[&head])
            .unwrap();

        let pitch = Workspace::open(temp.path().join("pitch")).unwrap();
        pitch.rollback_recovery(&operation).unwrap();
        assert_eq!(
            fs::read(temp.path().join("pitch/cache/thumb.bin")).unwrap(),
            b"cache"
        );
        assert!(!temp.path().join("deck/cache").exists());
        assert_eq!(fs::read(temp.path().join("deck/dev.txt")).unwrap(), b"dev");
        assert!(!temp.path().join("pitch/dev.txt").exists());
    }

    /// A developer commits on the checked-out branch from another process.
    fn developer_commits_at(point: &'static str) {
        test_hooks::once(point, |git_dir| {
            let repo = Repository::open(git_dir).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Developer",
                &head.tree().unwrap(),
                &[&head],
            )
            .unwrap();
        });
    }

    #[test]
    fn start_variation_here_keeps_metadata_of_a_branch_created_meanwhile() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        let theirs = VariationMetadata::default().with_label("Theirs");
        let expected = theirs.clone();
        let root = workspace.root().to_path_buf();
        test_hooks::once("before_branch_switch", move |_| {
            let other = Workspace::open(&root).unwrap();
            other.create_variation("idea").unwrap();
            other.write_variation_metadata("idea", &theirs).unwrap();
        });

        let error = workspace
            .start_variation_here_with_metadata(
                "idea",
                VariationMetadata::default().with_label("Mine"),
            )
            .unwrap_err();

        assert!(
            matches!(error, DraftlineError::VariationAlreadyExists(_)),
            "{error:?}"
        );
        assert!(workspace
            .repo
            .find_branch("idea", BranchType::Local)
            .is_ok());
        assert_eq!(workspace.read_variation_metadata("idea").unwrap(), expected);
    }

    #[test]
    fn start_variation_here_refuses_and_leaves_nothing_when_head_moves() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        let main = workspace.current_variation().unwrap();
        developer_commits_at("before_branch_switch");

        let error = workspace.start_variation_here("idea").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert_eq!(workspace.current_variation().unwrap(), main);
        assert!(workspace
            .repo
            .find_branch("idea", BranchType::Local)
            .is_err());
        assert_eq!(
            workspace.read_variation_metadata("idea").unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn start_variation_here_removes_its_branch_when_head_moves_before_the_switch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        let main = workspace.current_variation().unwrap();
        developer_commits_at("between_branch_and_switch");

        let error = workspace.start_variation_here("idea").unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert_eq!(workspace.current_variation().unwrap(), main);
        assert!(workspace
            .repo
            .find_branch("idea", BranchType::Local)
            .is_err());
    }

    struct UnreachableHost;

    impl PullRequestProvider for UnreachableHost {
        fn find_open(
            &mut self,
            _: &crate::PullRequestRequest,
        ) -> Result<Option<crate::PullRequest>> {
            panic!("Send must stop before the host")
        }
        fn create(&mut self, _: &crate::PullRequestRequest) -> Result<crate::PullRequest> {
            panic!("Send must stop before the host")
        }
    }

    #[test]
    fn send_leaves_no_branch_behind_when_head_moves_as_it_branches() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "plan.sk", b"base");
        workspace.save_version("Base").unwrap();
        workspace.publish_changes("origin").unwrap();
        write_file(workspace.root(), "plan.sk", b"pitch");
        workspace.save_version("Pitch").unwrap();
        let main = workspace.current_variation().unwrap();
        let request = SendRequest {
            title: "Pitch".to_string(),
            base: Some(main.clone()),
            ..Default::default()
        };
        developer_commits_at("before_branch_switch");

        let error = workspace
            .send(
                "origin",
                &request,
                &mut UnreachableHost,
                &mut RemoteOptions::new(),
            )
            .unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert_eq!(workspace.current_variation().unwrap(), main);
        let branches = workspace
            .repo
            .branches(Some(BranchType::Local))
            .unwrap()
            .map(|branch| branch.unwrap().0.name().unwrap().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(branches, vec![main.clone()]);
        assert_eq!(
            workspace.read_variation_metadata("pitch").unwrap(),
            VariationMetadata::default()
        );
    }

    #[test]
    fn apply_incoming_refuses_when_the_branch_switches_during_the_fetch() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let author_dir = tempfile::tempdir().unwrap();
        let author = Workspace::init(author_dir.path()).unwrap();
        configure_identity(&author, "Seth", "seth@example.com");
        author
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(author.root(), "plan.sk", b"base");
        author.save_version("Base").unwrap();
        author.publish_changes("origin").unwrap();
        let peer_dir = tempfile::tempdir().unwrap();
        let peer =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), peer_dir.path()).unwrap();
        write_file(author.root(), "plan.sk", b"remote");
        author.save_version("Remote").unwrap();
        author.publish_changes("origin").unwrap();

        let main = peer.current_variation().unwrap();
        let head = peer.repo.head().unwrap().peel_to_commit().unwrap();
        peer.repo.branch("feature/x", &head, false).unwrap();
        test_hooks::once("after_apply_fetch", |git_dir| {
            let repo = Repository::open(git_dir).unwrap();
            repo.set_head("refs/heads/feature/x").unwrap();
        });

        let error = peer
            .apply_incoming("origin", &mut RemoteOptions::new())
            .unwrap_err();

        assert!(
            matches!(error, DraftlineError::LocalStateChanged { .. }),
            "{error:?}"
        );
        assert!(peer.recovery_state().unwrap().is_none());
        for branch in [main.as_str(), "feature/x"] {
            assert_eq!(
                peer.repo
                    .refname_to_id(&format!("refs/heads/{branch}"))
                    .unwrap(),
                head.id()
            );
        }
        assert_eq!(fs::read(peer.root().join("plan.sk")).unwrap(), b"base");
    }

    #[test]
    fn repair_recovery_leaves_apply_incoming_alone_when_branch_moved_elsewhere() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"incoming");
        let incoming = workspace.save_version("Incoming").unwrap();
        let base_oid = Oid::from_str(base.id().as_str()).unwrap();
        workspace
            .repo
            .reference("refs/heads/main", base_oid, true, "rewind")
            .unwrap();
        workspace
            .repo
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
        write_file(workspace.root(), "post.md", b"developer");
        let developer = workspace.save_version("Developer commit").unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::ApplyIncoming,
                original_variation: Some("main".to_string()),
                target: Some(incoming.id().as_str().to_string()),
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery("interrupted").unwrap();

        assert!(!repair.completed);
        assert_eq!(
            workspace.repo.head().unwrap().target().unwrap().to_string(),
            developer.id().as_str()
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "developer"
        );
    }

    #[test]
    fn rollback_recovery_restores_deleted_variation() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        let variation = VariationId::from("old-option");
        workspace.create_variation(variation.as_str()).unwrap();
        workspace
            .repo
            .find_branch(variation.as_str(), BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
        let archive_ref = archive_ref("deleted-variations", variation.as_str(), "interrupted");
        workspace
            .repo
            .reference(
                &archive_ref,
                Oid::from_str(base.id().as_str()).unwrap(),
                false,
                "simulate interrupted delete archive",
            )
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::DeleteVariation,
                original_variation: Some(variation.as_str().to_string()),
                target: Some(base.id().as_str().to_string()),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert!(rollback.completed);
        assert!(rollback.changed_workspace);
        assert!(workspace
            .repo
            .find_branch(variation.as_str(), BranchType::Local)
            .is_ok());
        assert!(workspace.repo.find_reference(&archive_ref).is_err());
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn rollback_recovery_reports_unavailable_when_metadata_is_insufficient() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::DeleteVariation,
                original_variation: None,
                target: Some("0000000000000000000000000000000000000000".to_string()),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert_eq!(rollback.operation_id, "interrupted");
        assert_eq!(rollback.operation, RecoveryOperation::DeleteVariation);
        assert!(!rollback.completed);
        assert!(!rollback.changed_workspace);
        assert_eq!(
            rollback.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
    }

    #[test]
    fn rollback_restore_recovery_leaves_later_saves_and_edits_alone() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"restored");
        let advanced = workspace.save_version("Restored").unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: Some("main".to_string()),
                target: Some(base.id().as_str().to_string()),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert!(rollback.completed);
        assert!(!rollback.changed_workspace);
        assert_eq!(
            workspace.repo.head().unwrap().target().unwrap().to_string(),
            advanced.id().as_str()
        );
        assert_eq!(content(&workspace, "post.md"), "restored");
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn rollback_restore_recovery_is_unavailable_once_the_restore_save_landed() {
        let (_temp, workspace, first) = two_saves();
        workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::RestoreVersionAsNewSave,
                original_variation: None,
                target: Some(first.id().as_str().to_string()),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery("interrupted").unwrap();

        assert!(!rollback.completed);
        assert_eq!(head_summary(&workspace), "Restore first");
        assert_eq!(content(&workspace, "plan.sk"), "v1");
    }

    #[test]
    fn policy_git_diagnostics_warn_when_policy_tracked_file_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();
        write_file(workspace.root(), ".gitignore", b"content/hidden.md\n");
        write_file(workspace.root(), "content/hidden.md", b"business content");

        assert!(workspace.changed_files().unwrap().is_empty());

        let diagnostics = workspace.policy_git_diagnostics().unwrap();

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::PolicyTrackedFileIgnored
                && diagnostic.message.contains("content/hidden.md")
        }));
    }

    #[test]
    fn audit_content_policy_includes_current_git_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();
        write_file(workspace.root(), ".gitignore", b"content/hidden.md\n");
        write_file(workspace.root(), "content/hidden.md", b"business content");

        let audit = workspace.audit_content_policy().unwrap();

        assert_eq!(audit.current_diagnostics.len(), 1);
        assert_eq!(
            audit.current_diagnostics[0].code,
            DiagnosticCode::PolicyTrackedFileIgnored
        );
        assert!(audit.historical_out_of_policy_paths.is_empty());
    }

    #[test]
    fn inspect_includes_policy_git_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();
        write_file(workspace.root(), ".gitignore", b"content/hidden.md\n");
        write_file(workspace.root(), "content/hidden.md", b"business content");

        let inspection = workspace.inspect().unwrap();

        assert!(inspection.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::PolicyTrackedFileIgnored
                && diagnostic.severity == DiagnosticSeverity::Warning
        }));
    }

    #[test]
    fn preflight_adopt_workspace_reports_existing_repo_without_mutating() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        workspace
            .add_remote("origin", "https://example.com/draftline.git")
            .unwrap();
        write_file(workspace.root(), "post.md", b"unsaved");

        let report = workspace
            .preflight_adopt_workspace(ContentPolicy::default())
            .unwrap();

        assert_eq!(report.inspection.sharing_mode, SharingMode::SharedCapable);
        assert!(report.inspection.dirty.is_dirty);
        assert!(report.can_adopt);
        assert!(report.blockers.is_empty());
        assert!(report
            .safe_next_actions
            .contains(&SafeNextAction::SaveFirst));
    }

    #[test]
    fn preflight_adopt_workspace_blocks_detached_head() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let version = workspace.save_version("Base").unwrap();
        workspace
            .repo
            .set_head_detached(Oid::from_str(version.id().as_str()).unwrap())
            .unwrap();

        let report = workspace
            .preflight_adopt_workspace(ContentPolicy::default())
            .unwrap();

        assert!(!report.can_adopt);
        assert!(report
            .blockers
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::NoCurrentVariation));
    }

    #[test]
    fn generate_agent_instructions_mentions_draftline_owned_state() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        let instructions = workspace.generate_agent_instructions().unwrap();

        assert!(instructions.contains("Do not rewrite, delete, rename, or force-update"));
        assert!(instructions.contains("refs/draftline/"));
        assert!(instructions.contains("Fetch before reasoning about shared remote state"));
    }

    #[test]
    fn preflight_switch_blocks_ignored_file_that_target_tree_would_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "base.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();

        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "generated.txt", b"from alternate");
        workspace.save_version("Alternate adds generated").unwrap();

        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), ".gitignore", b"generated.txt\n");
        workspace.save_version("Ignore generated").unwrap();
        write_file(workspace.root(), "generated.txt", b"local generated output");

        let report = workspace
            .preflight_switch_variation(variation.id())
            .unwrap();

        assert!(!report.can_proceed);
        assert_eq!(report.file_hazards.len(), 1);
        assert_eq!(report.file_hazards[0].path, PathBuf::from("generated.txt"));
        assert_eq!(report.file_hazards[0].kind, FileHazardKind::Ignored);
    }

    #[test]
    fn switch_refuses_ignored_file_collision_before_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "base.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();

        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "generated.txt", b"from alternate");
        workspace.save_version("Alternate adds generated").unwrap();

        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), ".gitignore", b"generated.txt\n");
        workspace.save_version("Ignore generated").unwrap();
        write_file(workspace.root(), "generated.txt", b"local generated output");

        let err = workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();

        let DraftlineError::PreflightFailed(report) = err else {
            panic!("expected preflight failure");
        };
        assert_eq!(report.file_hazards[0].kind, FileHazardKind::Ignored);
        assert_eq!(
            fs::read_to_string(workspace.root().join("generated.txt")).unwrap(),
            "local generated output"
        );
    }

    #[test]
    fn shelve_lifecycle_lists_previews_applies_and_deletes_shelf() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"shelved");

        let shelf = workspace.shelve_changes("draft-shelf").unwrap();

        assert_eq!(shelf.id, "draft-shelf");
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "base"
        );
        let shelves = workspace.list_shelves().unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].id, "draft-shelf");

        let preview = workspace.preview_shelf("draft-shelf").unwrap();
        let post = preview
            .files
            .iter()
            .find(|file| file.path == Path::new("post.md"))
            .unwrap();
        assert_eq!(post.content.as_deref(), Some("shelved"));

        let report = workspace.preflight_apply_shelf("draft-shelf").unwrap();
        assert!(report.can_proceed);
        workspace.apply_shelf("draft-shelf").unwrap();
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "shelved"
        );

        workspace.delete_shelf("draft-shelf").unwrap();
        assert!(workspace.list_shelves().unwrap().is_empty());
    }

    #[test]
    fn shelve_files_shelves_only_selected_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one shelved");
        write_file(workspace.root(), "two.md", b"two remains");

        let report = workspace
            .preflight_shelve_files("partial-shelf", ["one.md"])
            .unwrap();
        assert!(report.can_proceed);
        assert_eq!(report.dirty_files.len(), 1);

        workspace.shelve_files("partial-shelf", ["one.md"]).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("one.md")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("two.md")).unwrap(),
            "two remains"
        );
        let changed = workspace.changed_files().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("two.md"));
        let preview = workspace.preview_shelf("partial-shelf").unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("one.md"))
                .and_then(|file| file.content.as_deref()),
            Some("one shelved")
        );
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("two.md"))
                .and_then(|file| file.content.as_deref()),
            Some("two")
        );
    }

    #[test]
    fn shelve_files_preserves_unselected_staged_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one shelved");
        write_file(workspace.root(), "two.md", b"two staged");
        stage_file(&workspace, "two.md");

        workspace.shelve_files("partial-shelf", ["one.md"]).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("one.md")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("two.md")).unwrap(),
            "two staged"
        );
        let two_status = workspace.repo.status_file(Path::new("two.md")).unwrap();
        assert!(two_status.contains(Status::INDEX_MODIFIED));
        assert!(!two_status.contains(Status::WT_MODIFIED));
        let preview = workspace.preview_shelf("partial-shelf").unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("one.md"))
                .and_then(|file| file.content.as_deref()),
            Some("one shelved")
        );
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("two.md"))
                .and_then(|file| file.content.as_deref()),
            Some("two")
        );
    }

    #[test]
    fn shelve_files_preserves_unselected_staged_rename() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "old.md", b"old");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one shelved");
        stage_rename(&workspace, "old.md", "renamed.md");

        workspace.shelve_files("partial-shelf", ["one.md"]).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("one.md")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("renamed.md")).unwrap(),
            "old"
        );
        let changed = workspace.changed_files().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("old.md"));
        assert_eq!(changed[0].kind, ChangeKind::Renamed);
        let preview = workspace.preview_shelf("partial-shelf").unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("one.md"))
                .and_then(|file| file.content.as_deref()),
            Some("one shelved")
        );
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("old.md"))
                .and_then(|file| file.content.as_deref()),
            Some("old")
        );
        assert!(!preview
            .files
            .iter()
            .any(|file| file.path == Path::new("renamed.md")));
    }

    #[test]
    fn shelve_files_shelves_selected_staged_rename() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "old.md", b"old");
        workspace.save_version("Base").unwrap();
        stage_rename(&workspace, "old.md", "renamed.md");

        workspace.shelve_files("rename-shelf", ["old.md"]).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.root().join("old.md")).unwrap(),
            "old"
        );
        assert!(!workspace.root().join("renamed.md").exists());
        assert!(workspace.changed_files().unwrap().is_empty());
        let preview = workspace.preview_shelf("rename-shelf").unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .find(|file| file.path == Path::new("renamed.md"))
                .and_then(|file| file.content.as_deref()),
            Some("old")
        );
        assert!(!preview
            .files
            .iter()
            .any(|file| file.path == Path::new("old.md")));
    }

    #[test]
    fn apply_shelf_preserves_shelf_when_workspace_is_dirty() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"shelved");
        workspace.shelve_changes("draft-shelf").unwrap();
        write_file(workspace.root(), "post.md", b"dirty");

        let report = workspace.preflight_apply_shelf("draft-shelf").unwrap();
        assert!(!report.can_proceed);
        assert_eq!(report.dirty_files[0].path, PathBuf::from("post.md"));

        let err = workspace.apply_shelf("draft-shelf").unwrap_err();
        assert!(matches!(err, DraftlineError::PreflightFailed(_)));
        assert_eq!(workspace.list_shelves().unwrap().len(), 1);
    }

    #[test]
    fn support_refs_list_deleted_variation_archives_and_restore_as_variation() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("old-option").unwrap();
        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"old option");
        workspace.save_version("Old option").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        workspace.delete_variation(variation.id()).unwrap();

        let support_refs = workspace.list_support_refs(SupportRefScope::Local).unwrap();

        assert_eq!(support_refs.len(), 1);
        assert_eq!(support_refs[0].kind, SupportRefKind::DeletedVariation);
        assert_eq!(
            support_refs[0].source_variation.as_deref(),
            Some("old-option")
        );

        let restore_preflight = workspace
            .preflight_restore_support_ref(&support_refs[0].id, "restored-option")
            .unwrap();
        assert!(restore_preflight.can_restore);
        assert_eq!(
            restore_preflight.support_ref.ref_name,
            support_refs[0].ref_name
        );

        let restored = workspace
            .restore_support_ref(restore_preflight.token)
            .unwrap();

        assert_eq!(restored.name, "restored-option");
        assert!(workspace
            .variations()
            .unwrap()
            .iter()
            .any(|variation| variation.name == "restored-option"));
    }

    #[test]
    fn remote_variations_can_be_discovered_and_adopted_locally() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        let variation = first_workspace.create_variation("teammate-option").unwrap();
        first_workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"teammate");
        first_workspace.save_version("Teammate").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        second_workspace.fetch_remote("origin").unwrap();

        let remote_variations = second_workspace.remote_variations("origin").unwrap();
        assert!(remote_variations
            .iter()
            .any(|variation| variation.id.as_str() == "teammate-option"));

        let adopted = second_workspace
            .adopt_remote_variation("origin", &VariationId::from("teammate-option"))
            .unwrap();
        assert_eq!(adopted.name, "teammate-option");
        assert!(second_workspace
            .variations()
            .unwrap()
            .iter()
            .any(|variation| variation.name == "teammate-option"));
    }

    #[test]
    fn guarded_variation_creation_rejects_remote_race_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let author_dir = tempfile::tempdir().unwrap();
        let author = Workspace::init(author_dir.path()).unwrap();
        configure_identity(&author, "Seth", "seth@example.com");
        author
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(author.root(), "post.md", b"base");
        let base = author.save_version("Base").unwrap();
        author.publish_changes("origin").unwrap();

        let consumer_dir = tempfile::tempdir().unwrap();
        let consumer =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), consumer_dir.path())
                .unwrap();
        let mut options = RemoteOptions::new();
        let preflight = consumer
            .preflight_create_variation_from_version_with_options(
                base.id(),
                "race-name",
                Some("origin"),
                &mut options,
            )
            .unwrap();
        assert!(preflight.can_create);
        assert_eq!(
            preflight
                .token
                .as_ref()
                .and_then(|token| token.expected_remote_oid.as_deref()),
            None
        );

        let raced = author
            .create_variation_from(base.id(), "race-name")
            .unwrap();
        author
            .switch_variation(raced.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(author.root(), "post.md", b"remote race");
        author.save_version("Remote race").unwrap();
        author.publish_changes("origin").unwrap();

        let mut options = RemoteOptions::new();
        assert!(matches!(
            consumer.create_variation_from_version_with_token(
                preflight.token.unwrap(),
                VariationMetadata::default(),
                &mut options,
            ),
            Err(DraftlineError::RemoteRace {
                remote,
                variation,
                expected: None,
                actual: Some(_),
            }) if remote == "origin" && variation == "race-name"
        ));
        assert!(consumer
            .repo
            .find_branch("race-name", BranchType::Local)
            .is_err());
    }

    #[test]
    fn remote_variations_hide_draftline_namespace_refs() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let version = workspace.save_version("Base").unwrap();
        let target = Oid::from_str(version.id().as_str()).unwrap();

        workspace
            .repo
            .reference("refs/remotes/origin/feature", target, false, "test")
            .unwrap();
        workspace
            .repo
            .reference(
                "refs/remotes/origin/draftline/deleted-variations/feature/op",
                target,
                false,
                "test",
            )
            .unwrap();

        let remote_variations = workspace.remote_variations("origin").unwrap();

        assert_eq!(remote_variations.len(), 1);
        assert_eq!(remote_variations[0].name, "feature");

        workspace
            .repo
            .reference("refs/remotes/upstream/feature", target, false, "test")
            .unwrap();
        workspace
            .repo
            .reference("refs/remotes/upstream/draftline", target, false, "test")
            .unwrap();

        let remote_variations = workspace.remote_variations("upstream").unwrap();

        assert_eq!(remote_variations.len(), 1);
        assert_eq!(remote_variations[0].name, "feature");
    }

    #[test]
    fn remote_variation_diagnostics_reports_local_and_remote_only_variations_after_prune() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let deleted = first_workspace
            .create_variation("deleted-remotely")
            .unwrap();
        first_workspace
            .switch_variation(deleted.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"deleted branch");
        first_workspace.save_version("Deleted branch").unwrap();
        first_workspace.publish_changes("origin").unwrap();
        first_workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        let teammate = second_workspace.create_variation("teammate").unwrap();
        second_workspace
            .switch_variation(teammate.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(second_workspace.root(), "post.md", b"teammate branch");
        second_workspace.save_version("Teammate branch").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        let bare = Repository::open_bare(remote.path()).unwrap();
        bare.find_reference("refs/heads/deleted-remotely")
            .unwrap()
            .delete()
            .unwrap();

        first_workspace.fetch_all_variations("origin").unwrap();
        let diagnostics = first_workspace
            .remote_variation_diagnostics("origin")
            .unwrap();

        assert!(diagnostics
            .shared_variations
            .contains(&VariationId::from("main")));
        assert!(diagnostics
            .local_only_variations
            .contains(&VariationId::from("deleted-remotely")));
        assert!(diagnostics
            .remote_only_variations
            .contains(&VariationId::from("teammate")));
    }

    #[test]
    fn preflight_merge_incoming_reports_needs_merge_without_mutating() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        write_file(first_workspace.root(), "post.md", b"local two");
        first_workspace.save_version("Local two").unwrap();
        first_workspace.fetch_remote("origin").unwrap();

        let report = first_workspace.preflight_merge_incoming("origin").unwrap();

        assert_eq!(report.sync_status.state, SyncState::NeedsMerge);
        assert!(!report.can_merge_cleanly);
        assert!(!report.changed_workspace);
    }

    #[test]
    fn merge_incoming_writes_clean_two_parent_version() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "base.md", b"base");
        first_workspace.save_version("Base").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");

        write_file(first_workspace.root(), "remote.md", b"remote");
        first_workspace.save_version("Remote").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        write_file(second_workspace.root(), "local.md", b"local");
        second_workspace.save_version("Local").unwrap();
        second_workspace.fetch_remote("origin").unwrap();

        let report = second_workspace.preflight_merge_incoming("origin").unwrap();
        assert!(report.can_merge_cleanly);
        assert!(report.conflicts.is_empty());
        let token = report.token.unwrap();
        let mut options = RemoteOptions::new();
        let result = second_workspace
            .merge_incoming(token, "Merge remote work", &mut options)
            .unwrap();

        assert!(result
            .merged_files
            .iter()
            .any(|path| path == Path::new("remote.md")));
        assert_eq!(
            fs::read_to_string(second_workspace.root().join("local.md")).unwrap(),
            "local"
        );
        assert_eq!(
            fs::read_to_string(second_workspace.root().join("remote.md")).unwrap(),
            "remote"
        );
        let commit = second_workspace
            .repo
            .find_commit(Oid::from_str(result.version.id().as_str()).unwrap())
            .unwrap();
        assert_eq!(commit.parent_count(), 2);
        assert!(second_workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn custom_resolver_precedes_plain_text_in_preflight_and_clean_merge() {
        let (_remote, _author_dir, _peer_dir, peer) = diverged_sketch_workspace();
        let calls = Arc::new(AtomicUsize::new(0));
        let peer = peer.with_merge_resolver(CleanSketchResolver {
            calls: Arc::clone(&calls),
        });

        let report = peer.preflight_merge_incoming("origin").unwrap();

        assert!(report.can_merge_cleanly);
        assert!(report.conflicts.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        calls.store(0, Ordering::SeqCst);
        let mut options = RemoteOptions::new();
        peer.merge_incoming(report.token.unwrap(), "Semantic sketch merge", &mut options)
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(peer.root().join("plan.sk")).unwrap(),
            "semantic merge"
        );
    }

    #[test]
    fn replacement_registry_is_used_when_merging_with_resolutions() {
        let (_remote, _author_dir, _peer_dir, peer) = diverged_sketch_workspace();
        let calls = Arc::new(AtomicUsize::new(0));
        let registry =
            ResolverRegistry::with_default_resolvers().register_first(ConflictingSketchResolver {
                calls: Arc::clone(&calls),
            });
        let peer = peer.with_resolver_registry(registry);

        let report = peer.preflight_merge_incoming("origin").unwrap();
        let token = report.token.unwrap();
        let conflict = report.conflicts.first().unwrap();
        assert_eq!(conflict.field_path.as_deref(), Some("rows"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        calls.store(0, Ordering::SeqCst);
        let mut options = RemoteOptions::new();
        peer.merge_incoming_with_resolutions(
            token,
            "Resolved semantic sketch merge",
            [MergeConflictResolution {
                path: conflict.path.clone(),
                field_path: conflict.field_path.clone(),
                choice: MergeResolutionChoice::UseContent {
                    content: "resolved sketch".to_string(),
                },
            }],
            &mut options,
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(peer.root().join("plan.sk")).unwrap(),
            "resolved sketch"
        );
    }

    #[test]
    fn preflight_merge_incoming_reports_semantic_conflicts() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");

        write_file(first_workspace.root(), "post.md", b"remote");
        first_workspace.save_version("Remote").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        write_file(second_workspace.root(), "post.md", b"local");
        second_workspace.save_version("Local").unwrap();
        second_workspace.fetch_remote("origin").unwrap();

        let report = second_workspace.preflight_merge_incoming("origin").unwrap();

        assert!(!report.can_merge_cleanly);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].path, Path::new("post.md"));
        assert!(report.token.is_some());
    }

    #[test]
    fn merge_incoming_with_resolutions_writes_two_parent_version() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");

        write_file(first_workspace.root(), "post.md", b"remote");
        first_workspace.save_version("Remote").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        write_file(second_workspace.root(), "post.md", b"local");
        second_workspace.save_version("Local").unwrap();
        second_workspace.fetch_remote("origin").unwrap();

        let report = second_workspace.preflight_merge_incoming("origin").unwrap();
        let token = report.token.unwrap();
        let conflict = report.conflicts[0].clone();
        let mut options = RemoteOptions::new();
        let result = second_workspace
            .merge_incoming_with_resolutions(
                token,
                "Merge with explicit resolution",
                [MergeConflictResolution {
                    path: conflict.path.clone(),
                    field_path: conflict.field_path.clone(),
                    choice: MergeResolutionChoice::UseContent {
                        content: "resolved".to_string(),
                    },
                }],
                &mut options,
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(second_workspace.root().join("post.md")).unwrap(),
            "resolved"
        );
        assert!(result
            .merged_files
            .iter()
            .any(|path| path == Path::new("post.md")));
        let commit = second_workspace
            .repo
            .find_commit(Oid::from_str(result.version.id().as_str()).unwrap())
            .unwrap();
        assert_eq!(commit.parent_count(), 2);
        assert!(second_workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn merge_incoming_with_resolutions_requires_matching_resolution() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");

        write_file(first_workspace.root(), "post.md", b"remote");
        first_workspace.save_version("Remote").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        write_file(second_workspace.root(), "post.md", b"local");
        second_workspace.save_version("Local").unwrap();
        second_workspace.fetch_remote("origin").unwrap();

        let token = second_workspace
            .preflight_merge_incoming("origin")
            .unwrap()
            .token
            .unwrap();
        let mut options = RemoteOptions::new();
        let error = second_workspace
            .merge_incoming_with_resolutions(
                token,
                "Merge without resolution",
                Vec::<MergeConflictResolution>::new(),
                &mut options,
            )
            .unwrap_err();

        assert!(matches!(error, DraftlineError::InvalidMergeResolution(_)));
        assert_eq!(
            fs::read_to_string(second_workspace.root().join("post.md")).unwrap(),
            "local"
        );
    }

    #[test]
    fn delete_remote_variation_publishes_support_ref_before_deleting_visible_ref() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("old-option").unwrap();
        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"old option");
        workspace.save_version("Old option").unwrap();
        workspace.publish_changes("origin").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        let preflight = workspace
            .preflight_delete_remote_variation("origin", variation.id())
            .unwrap();
        assert!(preflight.can_delete);
        assert!(preflight
            .support_ref
            .starts_with("refs/draftline/deleted-variations/old-option/"));

        workspace.delete_remote_variation(preflight.token).unwrap();

        let bare = Repository::open_bare(remote.path()).unwrap();
        assert!(bare.find_reference("refs/heads/old-option").is_err());
        assert!(bare.find_reference(&preflight.support_ref).is_ok());
    }

    #[test]
    fn delete_remote_variation_retries_after_support_ref_was_already_published() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("old-option").unwrap();
        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"old option");
        workspace.save_version("Old option").unwrap();
        workspace.publish_changes("origin").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        let preflight = workspace
            .preflight_delete_remote_variation("origin", variation.id())
            .unwrap();
        let target_oid = Oid::from_str(&preflight.expected_remote_oid).unwrap();
        workspace
            .repo
            .reference(
                &preflight.support_ref,
                target_oid,
                false,
                "simulate archived remote delete",
            )
            .unwrap();
        let mut options = RemoteOptions::new();
        workspace
            .push_refspec(
                "origin",
                &format!("{}:{}", preflight.support_ref, preflight.support_ref),
                vec![PushRefExpectation {
                    dst_refname: preflight.support_ref.clone(),
                    expected_old_oid: None,
                    expected_new_oid: Some(preflight.expected_remote_oid.clone()),
                }],
                &mut options,
            )
            .unwrap();

        let support_ref = preflight.support_ref.clone();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted-delete".to_string(),
                operation: RecoveryOperation::DeleteRemoteVariation,
                original_variation: Some("old-option".to_string()),
                target: Some(preflight.expected_remote_oid.clone()),
                completed: false,
            })
            .unwrap();
        let err = workspace
            .delete_remote_variation(preflight.token.clone())
            .unwrap_err();
        assert!(matches!(err, DraftlineError::RecoveryRequired(_)));

        workspace.acknowledge_recovery().unwrap();
        workspace.delete_remote_variation(preflight.token).unwrap();

        let bare = Repository::open_bare(remote.path()).unwrap();
        assert!(bare.find_reference("refs/heads/old-option").is_err());
        assert!(bare.find_reference(&support_ref).is_ok());
    }

    #[test]
    fn repair_remote_delete_recovers_after_visible_ref_was_deleted() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("old-option").unwrap();
        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"old option");
        workspace.save_version("Old option").unwrap();
        workspace.publish_changes("origin").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        let preflight = workspace
            .preflight_delete_remote_variation("origin", variation.id())
            .unwrap();
        let target_oid = Oid::from_str(&preflight.expected_remote_oid).unwrap();
        workspace
            .repo
            .reference(
                &preflight.support_ref,
                target_oid,
                false,
                "simulate archived remote delete",
            )
            .unwrap();
        let mut options = RemoteOptions::new();
        workspace
            .push_refspec(
                "origin",
                &format!("{}:{}", preflight.support_ref, preflight.support_ref),
                vec![PushRefExpectation {
                    dst_refname: preflight.support_ref.clone(),
                    expected_old_oid: None,
                    expected_new_oid: Some(preflight.expected_remote_oid.clone()),
                }],
                &mut options,
            )
            .unwrap();
        workspace
            .push_refspec(
                "origin",
                ":refs/heads/old-option",
                vec![PushRefExpectation {
                    dst_refname: "refs/heads/old-option".to_string(),
                    expected_old_oid: Some(preflight.expected_remote_oid.clone()),
                    expected_new_oid: None,
                }],
                &mut options,
            )
            .unwrap();
        workspace
            .repo
            .find_reference(&preflight.support_ref)
            .unwrap()
            .delete()
            .unwrap();

        let operation_id = "interrupted-delete";
        workspace
            .write_remote_delete_recovery_metadata(
                operation_id,
                &RemoteVariationDeleteRecoveryMetadata {
                    remote: "origin".to_string(),
                    variation: "old-option".to_string(),
                    expected_remote_oid: preflight.expected_remote_oid.clone(),
                    support_ref: preflight.support_ref.clone(),
                },
            )
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: operation_id.to_string(),
                operation: RecoveryOperation::DeleteRemoteVariation,
                original_variation: Some("old-option".to_string()),
                target: Some(preflight.expected_remote_oid.clone()),
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery(operation_id).unwrap();

        assert!(repair.completed);
        assert!(!repair.changed_workspace);
        assert!(workspace.recovery_state().unwrap().is_none());
        assert!(workspace
            .repo
            .find_reference(&preflight.support_ref)
            .is_ok());
        assert!(!workspace
            .remote_delete_recovery_metadata_path(operation_id)
            .exists());
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert!(bare.find_reference("refs/heads/old-option").is_err());
        assert!(bare.find_reference(&preflight.support_ref).is_ok());
    }

    #[test]
    fn repair_remote_delete_finishes_when_visible_ref_is_still_present() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("old-option").unwrap();
        workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"old option");
        workspace.save_version("Old option").unwrap();
        workspace.publish_changes("origin").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        let preflight = workspace
            .preflight_delete_remote_variation("origin", variation.id())
            .unwrap();
        let target_oid = Oid::from_str(&preflight.expected_remote_oid).unwrap();
        workspace
            .repo
            .reference(
                &preflight.support_ref,
                target_oid,
                false,
                "simulate archived remote delete",
            )
            .unwrap();
        let mut options = RemoteOptions::new();
        workspace
            .push_refspec(
                "origin",
                &format!("{}:{}", preflight.support_ref, preflight.support_ref),
                vec![PushRefExpectation {
                    dst_refname: preflight.support_ref.clone(),
                    expected_old_oid: None,
                    expected_new_oid: Some(preflight.expected_remote_oid.clone()),
                }],
                &mut options,
            )
            .unwrap();

        let operation_id = "interrupted-delete-before-visible-delete";
        workspace
            .write_remote_delete_recovery_metadata(
                operation_id,
                &RemoteVariationDeleteRecoveryMetadata {
                    remote: "origin".to_string(),
                    variation: "old-option".to_string(),
                    expected_remote_oid: preflight.expected_remote_oid.clone(),
                    support_ref: preflight.support_ref.clone(),
                },
            )
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: operation_id.to_string(),
                operation: RecoveryOperation::DeleteRemoteVariation,
                original_variation: Some("old-option".to_string()),
                target: Some(preflight.expected_remote_oid.clone()),
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery(operation_id).unwrap();

        assert!(repair.completed);
        assert!(workspace.recovery_state().unwrap().is_none());
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert!(bare.find_reference("refs/heads/old-option").is_err());
        assert!(bare.find_reference(&preflight.support_ref).is_ok());
    }

    #[test]
    fn acknowledge_recovery_removes_remote_delete_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        let operation_id = "interrupted-delete";
        workspace
            .write_remote_delete_recovery_metadata(
                operation_id,
                &RemoteVariationDeleteRecoveryMetadata {
                    remote: "origin".to_string(),
                    variation: "old-option".to_string(),
                    expected_remote_oid: "0".repeat(40),
                    support_ref: "refs/draftline/deleted-variations/old-option/op".to_string(),
                },
            )
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: operation_id.to_string(),
                operation: RecoveryOperation::DeleteRemoteVariation,
                original_variation: Some("old-option".to_string()),
                target: Some("0".repeat(40)),
                completed: false,
            })
            .unwrap();

        workspace.acknowledge_recovery().unwrap();

        assert!(!workspace.ledger_path().exists());
        assert!(!workspace
            .remote_delete_recovery_metadata_path(operation_id)
            .exists());
    }

    #[test]
    fn delete_remote_variation_refuses_remote_tip_changed_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        let variation = first_workspace.create_variation("old-option").unwrap();
        first_workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"old option");
        first_workspace.save_version("Old option").unwrap();
        first_workspace.publish_changes("origin").unwrap();
        first_workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        let preflight = first_workspace
            .preflight_delete_remote_variation("origin", variation.id())
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        second_workspace
            .adopt_remote_variation("origin", variation.id())
            .unwrap();
        second_workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(second_workspace.root(), "post.md", b"updated remotely");
        let raced_tip = second_workspace.save_version("Updated remotely").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        let support_ref = preflight.support_ref.clone();
        let err = first_workspace
            .delete_remote_variation(preflight.token)
            .unwrap_err();

        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/old-option")
                .unwrap()
                .to_string(),
            raced_tip.id().as_str()
        );
        assert!(bare.find_reference(&support_ref).is_err());
    }

    #[test]
    fn delete_remote_variation_support_ref_collision_leaves_no_recovery_residue() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        let variation = first_workspace.create_variation("old-option").unwrap();
        first_workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"old option");
        first_workspace.save_version("Old option").unwrap();
        first_workspace.publish_changes("origin").unwrap();
        first_workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        let preflight = first_workspace
            .preflight_delete_remote_variation("origin", variation.id())
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace = Workspace::init(second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        second_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(second_workspace.root(), "post.md", b"different support ref");
        second_workspace
            .save_version("Different support ref")
            .unwrap();
        let different_oid = second_workspace
            .repo
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        second_workspace
            .repo
            .reference(
                &preflight.support_ref,
                Oid::from_str(&different_oid).unwrap(),
                false,
                "colliding archive",
            )
            .unwrap();
        let mut options = RemoteOptions::new();
        second_workspace
            .push_refspec(
                "origin",
                &format!("{}:{}", preflight.support_ref, preflight.support_ref),
                vec![PushRefExpectation {
                    dst_refname: preflight.support_ref.clone(),
                    expected_old_oid: None,
                    expected_new_oid: Some(different_oid),
                }],
                &mut options,
            )
            .unwrap();

        let err = first_workspace
            .delete_remote_variation(preflight.token)
            .unwrap_err();

        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
        assert!(first_workspace.recovery_state().unwrap().is_none());
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert!(bare.find_reference("refs/heads/old-option").is_ok());
    }

    #[test]
    fn support_refs_publish_create_only_and_fetch_remote_tracking_refs() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        first_workspace.publish_changes("origin").unwrap();
        let variation = first_workspace.create_variation("old-option").unwrap();
        first_workspace.delete_variation(variation.id()).unwrap();

        let preflight = first_workspace
            .preflight_publish_support_refs("origin")
            .unwrap();
        assert!(preflight.can_publish);
        assert_eq!(preflight.support_refs.len(), 1);
        first_workspace
            .publish_support_refs(preflight.token.clone())
            .unwrap();
        let repeated_preflight = first_workspace
            .preflight_publish_support_refs("origin")
            .unwrap();
        assert!(!repeated_preflight.can_publish);
        assert!(repeated_preflight.support_refs.is_empty());
        first_workspace
            .publish_support_refs(repeated_preflight.token)
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        second_workspace.fetch_support_refs("origin").unwrap();

        let remote_support_refs = second_workspace
            .list_support_refs(SupportRefScope::RemoteTracking)
            .unwrap();
        assert_eq!(remote_support_refs.len(), 1);
        assert_eq!(
            remote_support_refs[0].scope,
            SupportRefScope::RemoteTracking
        );
        assert_eq!(
            remote_support_refs[0].source_variation.as_deref(),
            Some("old-option")
        );
        assert!(remote_support_refs[0]
            .ref_name
            .starts_with("refs/remotes/origin/draftline/deleted-variations/old-option/"));
        second_workspace.fetch_all_variations("origin").unwrap();
        let remote_support_refs_after_fetch_all = second_workspace
            .list_support_refs(SupportRefScope::RemoteTracking)
            .unwrap();
        assert_eq!(remote_support_refs_after_fetch_all.len(), 1);
        assert!(second_workspace
            .remote_variations("origin")
            .unwrap()
            .iter()
            .all(|variation| !variation.name.starts_with("draftline/")));

        let restore_preflight = second_workspace
            .preflight_restore_support_ref(&remote_support_refs[0].id, "restored-old-option")
            .unwrap();
        assert_eq!(
            restore_preflight.support_ref.scope,
            SupportRefScope::RemoteTracking
        );
        let restored = second_workspace
            .restore_support_ref(restore_preflight.token)
            .unwrap();
        assert_eq!(restored.name, "restored-old-option");
        assert!(second_workspace
            .variations()
            .unwrap()
            .iter()
            .any(|variation| variation.name == "restored-old-option"));
    }

    #[test]
    fn support_ref_publish_preflight_rejects_same_name_different_oid_collision() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        let first_ref_name = "refs/draftline/deleted-variations/old-option/same-operation";
        first_workspace
            .repo
            .reference(
                first_ref_name,
                first_workspace.repo.head().unwrap().target().unwrap(),
                false,
                "first archive",
            )
            .unwrap();
        let first_preflight = first_workspace
            .preflight_publish_support_refs("origin")
            .unwrap();
        first_workspace
            .publish_support_refs(first_preflight.token)
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace = Workspace::init(second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        second_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(second_workspace.root(), "post.md", b"different");
        second_workspace.save_version("Different").unwrap();
        second_workspace
            .repo
            .reference(
                first_ref_name,
                second_workspace.repo.head().unwrap().target().unwrap(),
                false,
                "colliding archive",
            )
            .unwrap();

        let err = second_workspace
            .preflight_publish_support_refs("origin")
            .unwrap_err();

        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
    }

    #[test]
    fn publish_support_refs_token_noops_when_same_ref_appears_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let workspace_dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(workspace_dir.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let ref_name = "refs/draftline/deleted-variations/old-option/same-operation";
        let target_oid = workspace.repo.head().unwrap().target().unwrap().to_string();
        workspace
            .repo
            .reference(
                ref_name,
                Oid::from_str(&target_oid).unwrap(),
                false,
                "archive",
            )
            .unwrap();
        let preflight = workspace.preflight_publish_support_refs("origin").unwrap();
        let mut options = RemoteOptions::new();
        workspace
            .push_refspec(
                "origin",
                &format!("{ref_name}:{ref_name}"),
                vec![PushRefExpectation {
                    dst_refname: ref_name.to_string(),
                    expected_old_oid: None,
                    expected_new_oid: Some(target_oid),
                }],
                &mut options,
            )
            .unwrap();

        workspace.publish_support_refs(preflight.token).unwrap();
    }

    #[test]
    fn publish_support_refs_token_rejects_different_ref_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"base");
        first_workspace.save_version("Base").unwrap();
        let ref_name = "refs/draftline/deleted-variations/old-option/same-operation";
        first_workspace
            .repo
            .reference(
                ref_name,
                first_workspace.repo.head().unwrap().target().unwrap(),
                false,
                "first archive",
            )
            .unwrap();
        let preflight = first_workspace
            .preflight_publish_support_refs("origin")
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace = Workspace::init(second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        second_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(second_workspace.root(), "post.md", b"different");
        second_workspace.save_version("Different").unwrap();
        let different_oid = second_workspace
            .repo
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        second_workspace
            .repo
            .reference(
                ref_name,
                Oid::from_str(&different_oid).unwrap(),
                false,
                "different archive",
            )
            .unwrap();
        let mut options = RemoteOptions::new();
        second_workspace
            .push_refspec(
                "origin",
                &format!("{ref_name}:{ref_name}"),
                vec![PushRefExpectation {
                    dst_refname: ref_name.to_string(),
                    expected_old_oid: None,
                    expected_new_oid: Some(different_oid),
                }],
                &mut options,
            )
            .unwrap();

        let err = first_workspace
            .publish_support_refs(preflight.token)
            .unwrap_err();

        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
    }

    #[test]
    fn expire_support_refs_removes_selected_local_archive_refs() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("old-option").unwrap();
        workspace.delete_variation(variation.id()).unwrap();
        let support_ref = workspace
            .list_support_refs(SupportRefScope::Local)
            .unwrap()
            .pop()
            .unwrap();

        let preflight = workspace
            .preflight_expire_support_refs([support_ref.id.clone()])
            .unwrap();
        assert!(preflight.can_expire);

        workspace.expire_support_refs(preflight.token).unwrap();

        assert!(workspace
            .list_support_refs(SupportRefScope::Local)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn preflight_purge_content_enumerates_refs_and_limits() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "secret.md", b"secret");
        workspace.save_version("Secret").unwrap();

        let preflight = workspace.preflight_purge_content("secret.md").unwrap();

        assert_eq!(preflight.selector, "secret.md");
        assert!(preflight
            .distributed_warning
            .contains("cannot guarantee deletion from existing clones"));
        assert!(preflight
            .affected_refs
            .iter()
            .any(|reference| reference == "refs/heads/main"));
        assert!(
            workspace
                .verify_purge(preflight.token)
                .unwrap()
                .checked_refs
                > 0
        );
    }

    #[test]
    fn agent_json_facade_serializes_inspect_capabilities_and_verify() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        let inspect_json = workspace.inspect_json().unwrap();
        assert!(inspect_json.contains("safe_next_actions"));

        let capabilities_json = Workspace::capabilities_json().unwrap();
        assert!(capabilities_json.contains("inspect"));

        let verification = workspace.verify_workspace().unwrap();
        assert!(verification.recovery_clear);
        assert!(verification.operation_lock_clear);
        assert!(verification.current_variation_present);
    }

    #[test]
    fn agent_json_facade_blocks_normal_work_on_detached_head() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let version = workspace.save_version("Base").unwrap();
        workspace
            .repo
            .set_head_detached(Oid::from_str(version.id().as_str()).unwrap())
            .unwrap();

        let inspection = workspace.inspect().unwrap();

        assert!(inspection
            .diagnostics
            .iter()
            .any(
                |diagnostic| diagnostic.code == DiagnosticCode::NoCurrentVariation
                    && diagnostic.severity == DiagnosticSeverity::Blocking
            ));
        assert_eq!(
            inspection.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
    }

    #[test]
    fn recovery_records_name_the_project_that_wrote_them() {
        let temp = tempfile::tempdir().unwrap();
        Repository::init(temp.path()).unwrap();
        let workspace = Workspace::init(temp.path().join("decks").join("pitch")).unwrap();
        let state = RecoveryState {
            operation_id: "op".to_string(),
            operation: RecoveryOperation::ShelveChanges,
            original_variation: None,
            target: None,
            completed: false,
        };
        workspace.write_recovery_state(&state).unwrap();

        let text = fs::read_to_string(workspace.ledger_path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["project"], "decks/pitch");
        assert_eq!(workspace.recovery_state().unwrap(), Some(state));
        workspace.acknowledge_recovery().unwrap();
        assert!(!workspace.ledger_path().exists());
    }

    #[test]
    fn a_corrupt_recovery_record_can_still_be_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        fs::write(workspace.ledger_path(), b"{not-json").unwrap();
        assert!(workspace.save_version("Blocked").is_err());
        workspace.acknowledge_recovery().unwrap();
        assert!(!workspace.ledger_path().exists());

        // A record cut off mid-character isn't valid UTF-8.
        fs::write(workspace.ledger_path(), b"{\"project\": \"caf\xc3").unwrap();
        workspace.acknowledge_recovery().unwrap();
        assert!(!workspace.ledger_path().exists());
    }

    #[test]
    fn agent_json_facade_reports_corrupt_recovery_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        fs::write(workspace.ledger_path(), b"{not-json").unwrap();

        let inspection = workspace.inspect().unwrap();
        let verification = workspace.verify_workspace().unwrap();

        assert!(inspection
            .diagnostics
            .iter()
            .any(
                |diagnostic| diagnostic.code == DiagnosticCode::RecoveryRequired
                    && diagnostic.severity == DiagnosticSeverity::Blocking
            ));
        assert_eq!(
            inspection.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
        assert!(!verification.recovery_clear);
        assert!(verification
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::RecoveryRequired));
    }

    #[test]
    fn explain_error_maps_stable_code_to_safe_next_action() {
        let explanation = Workspace::explain_error(DiagnosticCode::WorkspaceLocked);

        assert_eq!(explanation.code, DiagnosticCode::WorkspaceLocked);
        assert_eq!(
            explanation.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
        assert_eq!(explanation.retry, RetryClass::RetryAfterRepair);
    }

    // ── history ──────────────────────────────────────────────────────────────

    #[test]
    fn history_marks_variation_tips_at_correct_versions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"first");
        workspace.save_version("First").unwrap();
        // create a variation that diverges at this point
        workspace.create_variation("feature").unwrap();

        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let history = workspace.history().unwrap();
        assert_eq!(history.len(), 2);

        // HEAD (newest) should be marked as the tip of "main"
        let head_entry = &history[0];
        assert!(head_entry.is_head);
        assert!(head_entry
            .variation_tips
            .iter()
            .any(|id| id.as_str() == "main"));

        // older version should show "feature" as a tip
        let older_entry = &history[1];
        assert!(!older_entry.is_head);
        assert!(older_entry
            .variation_tips
            .iter()
            .any(|id| id.as_str() == "feature"));
    }

    #[test]
    fn history_returns_empty_for_brand_new_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        let history = workspace.history().unwrap();
        assert!(history.is_empty());
    }

    // ── diff_versions ─────────────────────────────────────────────────────────

    #[test]
    fn diff_versions_reports_changed_files_and_patch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"hello");
        let v1 = workspace.save_version("v1").unwrap();

        write_file(workspace.root(), "post.md", b"hello world");
        let v2 = workspace.save_version("v2").unwrap();

        let diff = workspace.diff_versions(v1.id(), v2.id()).unwrap();

        assert_eq!(diff.from_version.as_ref(), Some(v1.id()));
        assert_eq!(diff.to_version.as_ref(), Some(v2.id()));
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, PathBuf::from("post.md"));
        assert_eq!(diff.files[0].kind, ChangeKind::Modified);
        assert!(diff.patch.is_some());
    }

    #[test]
    fn diff_versions_empty_when_identical() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"same");
        let v1 = workspace.save_version("v1").unwrap();

        // save again without changes
        let v2 = workspace.save_version("v2").unwrap();

        let diff = workspace.diff_versions(v1.id(), v2.id()).unwrap();

        assert!(diff.files.is_empty());
        assert!(diff.patch.is_none());
    }

    // ── diff_version_to_workspace ─────────────────────────────────────────────

    #[test]
    fn diff_version_to_workspace_detects_uncommitted_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"saved");
        let version = workspace.save_version("Saved").unwrap();

        write_file(workspace.root(), "post.md", b"modified in workspace");

        let diff = workspace.diff_version_to_workspace(version.id()).unwrap();

        assert_eq!(diff.from_version.as_ref(), Some(version.id()));
        assert!(diff.to_version.is_none());
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].kind, ChangeKind::Modified);
        assert!(diff.patch.is_some());
    }

    #[test]
    fn diff_version_to_workspace_applies_content_policy() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include_extension("md").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"saved");
        let version = workspace.save_version("Saved").unwrap();

        // policy-excluded file should not appear in the diff
        write_file(workspace.root(), "state.json", b"{}");
        write_file(workspace.root(), "post.md", b"modified");

        let diff = workspace.diff_version_to_workspace(version.id()).unwrap();

        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, PathBuf::from("post.md"));
    }

    // ── VersionId::from_canonical_string ─────────────────────────────────────

    #[test]
    fn version_id_round_trips_through_canonical_string() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"hello");
        let version = workspace.save_version("Draft").unwrap();

        let parsed = VersionId::from_canonical_string(version.id().as_str()).unwrap();
        assert_eq!(&parsed, version.id());
    }

    #[test]
    fn version_id_from_canonical_string_rejects_invalid_hex() {
        let err = VersionId::from_canonical_string("not-a-sha").unwrap_err();
        assert!(matches!(err, DraftlineError::VersionNotFound(_)));
    }

    #[test]
    fn version_id_from_canonical_string_rejects_abbreviated_prefix() {
        // git2::Oid::from_str accepts short prefixes — from_canonical_string must not.
        let abbreviated = "0123456789abcdef"; // 16 chars, valid hex but not 40
        let err = VersionId::from_canonical_string(abbreviated).unwrap_err();
        assert!(matches!(err, DraftlineError::VersionNotFound(_)));
    }

    #[test]
    fn version_id_from_canonical_string_rejects_uppercase_hex() {
        // Canonical OIDs are lowercase; uppercase should be rejected so IDs
        // always compare equal as strings without case-folding.
        let upper = "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6A1B2";
        let err = VersionId::from_canonical_string(upper).unwrap_err();
        assert!(matches!(err, DraftlineError::VersionNotFound(_)));
    }

    // ── serde round-trips ─────────────────────────────────────────────────────

    #[test]
    fn version_id_serializes_as_plain_string() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"hello");
        let version = workspace.save_version("Draft").unwrap();

        let json = serde_json::to_string(version.id()).unwrap();
        // should be a plain JSON string, not an object
        assert!(json.starts_with('"'));
        let parsed: VersionId = serde_json::from_str(&json).unwrap();
        assert_eq!(&parsed, version.id());
    }

    #[test]
    fn workspace_summary_is_serializable() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Draft").unwrap();

        let summary = workspace.workspace_summary().unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("active_variation"));
        assert!(json.contains("versions"));
    }

    // ── parent_ids in history ─────────────────────────────────────────────────

    #[test]
    fn history_entries_carry_parent_ids() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        let v1 = workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();

        let history = workspace.history().unwrap();
        assert_eq!(history.len(), 2);

        // Newest (v2): should have v1 as its sole parent
        assert_eq!(history[0].version.id(), v2.id());
        assert_eq!(history[0].parent_ids.len(), 1);
        assert_eq!(&history[0].parent_ids[0], v1.id());

        // Initial (v1): no parents
        assert_eq!(history[1].version.id(), v1.id());
        assert!(history[1].parent_ids.is_empty());
    }

    // ── full_history ──────────────────────────────────────────────────────────

    #[test]
    fn full_history_includes_commits_from_all_variations() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();

        // Diverge a variation; switch to it and add an exclusive commit.
        workspace
            .create_variation_from(base.id(), "feature")
            .unwrap();
        workspace
            .switch_variation(&VariationId::from("feature"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"feature work");
        let feature_v = workspace.save_version("Feature commit").unwrap();

        // Switch back and add a commit on main too.
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"main work");
        let main_v = workspace.save_version("Main commit").unwrap();

        let all = workspace.full_history().unwrap();
        let all_ids: Vec<&VersionId> = all.iter().map(|e| e.version.id()).collect();

        assert!(all_ids.contains(&base.id()), "base missing");
        assert!(all_ids.contains(&feature_v.id()), "feature commit missing");
        assert!(all_ids.contains(&main_v.id()), "main commit missing");
    }

    #[test]
    fn full_history_parent_ids_form_valid_dag() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        let v1 = workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();

        let entries = workspace.full_history().unwrap();
        let v2_entry = entries.iter().find(|e| e.version.id() == v2.id()).unwrap();
        let v1_entry = entries.iter().find(|e| e.version.id() == v1.id()).unwrap();

        assert_eq!(v2_entry.parent_ids.len(), 1);
        assert_eq!(&v2_entry.parent_ids[0], v1.id());
        assert!(v1_entry.parent_ids.is_empty());
    }

    #[test]
    fn workspace_graph_reports_local_variation_dag_refs_and_dirty_state() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        workspace
            .create_variation_from(base.id(), "feature")
            .unwrap();
        workspace
            .switch_variation(&VariationId::from("feature"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"feature work");
        let feature = workspace.save_version("Feature").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"main work");
        let main = workspace.save_version("Main").unwrap();
        write_file(workspace.root(), "post.md", b"unsaved main work");

        let graph = workspace
            .workspace_graph(WorkspaceGraphOptions::default())
            .unwrap();
        let node_versions = graph
            .nodes
            .iter()
            .map(|node| node.version.id())
            .collect::<Vec<_>>();

        assert!(node_versions.contains(&base.id()));
        assert!(node_versions.contains(&feature.id()));
        assert!(node_versions.contains(&main.id()));
        assert_eq!(graph.current_variation, Some(VariationId::from("main")));
        assert_eq!(graph.current_version, Some(main.id().clone()));
        assert!(graph.dirty.is_dirty);
        assert!(graph.recovery.is_none());
        assert!(!graph.state_may_be_inconsistent);
        assert_eq!(graph.refs.len(), 2);
        assert!(graph.refs.iter().any(|graph_ref| {
            graph_ref.kind == WorkspaceGraphRefKind::LocalVariation
                && graph_ref.name == "main"
                && graph_ref.target_version == *main.id()
        }));
        assert!(graph.refs.iter().any(|graph_ref| {
            graph_ref.kind == WorkspaceGraphRefKind::LocalVariation
                && graph_ref.name == "feature"
                && graph_ref.target_version == *feature.id()
        }));

        let main_node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == main.id())
            .unwrap();
        let feature_node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == feature.id())
            .unwrap();
        let base_node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == base.id())
            .unwrap();
        assert_eq!(main_node.parent_version_ids, vec![base.id().clone()]);
        assert_eq!(feature_node.parent_version_ids, vec![base.id().clone()]);
        assert!(base_node.parent_version_ids.is_empty());
        assert_eq!(
            main_node.parent_ids,
            vec![WorkspaceGraphNodeId::from(
                Oid::from_str(base.id().as_str()).unwrap()
            )]
        );
        assert!(main_node
            .available_actions
            .contains(&WorkspaceGraphAction::CreateVariationFromHere));
        assert!(main_node
            .available_actions
            .contains(&WorkspaceGraphAction::RestoreAsNewSave));
    }

    #[test]
    fn workspace_graph_helpers_focus_and_summarize_graphs() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        workspace
            .create_variation_from(base.id(), "feature")
            .unwrap();
        workspace
            .switch_variation(&VariationId::from("feature"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"feature work");
        let feature = workspace.save_version("Feature").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"main work");
        let main = workspace.save_version("Main").unwrap();

        let refs = workspace
            .workspace_graph_refs(WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(refs.refs.len(), 2);
        assert!(refs.refs.iter().any(|graph_ref| {
            graph_ref.name == "main" && graph_ref.is_current && graph_ref.is_user_facing
        }));
        assert!(refs.graph_fingerprint.starts_with("graph-fingerprint-"));
        assert!(matches!(
            workspace.workspace_graph_refs(WorkspaceGraphOptions {
                limit: Some(1),
                ..WorkspaceGraphOptions::default()
            }),
            Err(DraftlineError::InvalidGraphOptions(_))
        ));

        let summary = workspace
            .workspace_graph_summary(WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(summary.total_nodes, 3);
        assert_eq!(summary.normal_nodes, 3);
        assert_eq!(summary.local_ref_count, 2);
        assert_eq!(summary.branch_points, 1);
        assert_eq!(summary.merge_nodes, 0);

        let graph = workspace
            .workspace_graph(WorkspaceGraphOptions::default())
            .unwrap();
        let base_node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == base.id())
            .unwrap();
        assert!(base_node.is_branch_point);
        assert_eq!(base_node.child_count, 2);
        assert!(!base_node.action_hints.is_empty());

        let overview = workspace
            .workspace_graph_overview(WorkspaceGraphOverviewOptions::default())
            .unwrap();
        assert_eq!(overview.nodes.len(), 3);
        assert!(!overview.was_pruned);
        assert!(!overview.has_more);
        assert!(overview
            .refs
            .iter()
            .any(|graph_ref| graph_ref.name == "feature"));

        let around_base = workspace
            .workspace_graph_around_version(base.id(), 0, WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(around_base.nodes.len(), 1);
        assert_eq!(around_base.nodes[0].version.id(), base.id());
        assert!(around_base.was_pruned);
        assert!(!around_base.has_more);
        assert!(around_base.nodes[0].child_ids.is_empty());
        assert_eq!(around_base.nodes[0].boundary.hidden_child_count, 2);
        assert_eq!(around_base.nodes[0].boundary.missing_child_ids.len(), 2);
        let around_node_ids = around_base
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        assert!(around_base.nodes[0]
            .boundary
            .missing_child_ids
            .iter()
            .all(|child| !around_node_ids.contains(child)));

        let neighborhood = workspace
            .workspace_graph_neighborhood(base.id(), 1, WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(neighborhood.nodes.len(), 3);

        let search = workspace
            .search_workspace_graph(
                "feature",
                WorkspaceGraphOptions {
                    limit: Some(10),
                    ..WorkspaceGraphOptions::default()
                },
            )
            .unwrap();
        assert!(search
            .graph
            .nodes
            .iter()
            .any(|node| node.version.id() == feature.id()));
        assert!(search
            .matched_refs
            .iter()
            .any(|graph_ref| graph_ref.name == "feature"));
        assert_eq!(search.matched_node_count, 1);
        assert_eq!(search.total_matches, 2);
        assert!(search.matched_refs.iter().all(|graph_ref| search
            .graph
            .nodes
            .iter()
            .any(|node| node.id == graph_ref.target)));

        let feature_lane = workspace
            .workspace_graph_for_variation(
                &VariationId::from("feature"),
                WorkspaceGraphOptions::default(),
            )
            .unwrap();
        assert!(feature_lane
            .nodes
            .iter()
            .any(|node| node.version.id() == feature.id()));
        assert!(feature_lane
            .nodes
            .iter()
            .any(|node| node.version.id() == base.id()));
        assert!(!feature_lane
            .nodes
            .iter()
            .any(|node| node.version.id() == main.id()));
        assert!(feature_lane.was_pruned);
        assert!(!feature_lane.has_more);

        let common = workspace
            .workspace_graph_common_ancestor(main.id(), feature.id())
            .unwrap();
        assert_eq!(common.common_ancestor, Some(base.id().clone()));
        let path = workspace
            .workspace_graph_path(main.id(), feature.id(), WorkspaceGraphOptions::default())
            .unwrap();
        assert!(path.found);
        assert_eq!(path.common_ancestor, Some(base.id().clone()));
        assert_eq!(
            path.version_ids,
            vec![main.id().clone(), base.id().clone(), feature.id().clone()]
        );
        assert!(matches!(
            workspace.workspace_graph_path(
                &VersionId::from_canonical_string("0123456789012345678901234567890123456789")
                    .unwrap(),
                feature.id(),
                WorkspaceGraphOptions::default()
            ),
            Err(DraftlineError::VersionNotFound(_))
        ));
        let detail = workspace
            .workspace_graph_node(feature.id(), WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(detail.node.version.id(), feature.id());
        assert_eq!(detail.changed_file_count, Some(1));
        let compare = workspace
            .workspace_graph_compare_summary(base.id(), feature.id())
            .unwrap();
        assert_eq!(compare.changed_file_count, 1);

        let agent_summary = workspace
            .workspace_graph_agent_summary(WorkspaceGraphOptions {
                limit: Some(1),
                cursor: Some(1),
                ..WorkspaceGraphOptions::default()
            })
            .unwrap();
        assert!(agent_summary.warnings.is_empty());
        assert!(agent_summary
            .suggested_next_commands
            .contains(&"get_workspace_graph_for_variation".to_string()));
        assert_eq!(
            agent_summary
                .current_ref
                .and_then(|graph_ref| graph_ref.variation),
            Some(VariationId::from("main"))
        );
    }

    #[test]
    fn workspace_graph_paginates_with_stable_topo_indices() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        workspace.save_version("v3").unwrap();

        let first_page = workspace
            .workspace_graph(WorkspaceGraphOptions {
                limit: Some(2),
                ..WorkspaceGraphOptions::default()
            })
            .unwrap();
        let second_page = workspace
            .workspace_graph(WorkspaceGraphOptions {
                cursor: first_page.next_cursor,
                limit: Some(2),
                ..WorkspaceGraphOptions::default()
            })
            .unwrap();

        assert_eq!(first_page.nodes.len(), 2);
        assert!(first_page.was_pruned);
        assert!(first_page.has_more);
        assert_eq!(first_page.nodes[0].topo_index, 0);
        assert_eq!(first_page.nodes[1].topo_index, 1);
        assert_eq!(second_page.nodes.len(), 1);
        assert_eq!(second_page.nodes[0].topo_index, 2);
        assert!(second_page.was_pruned);
        assert!(!second_page.has_more);
    }

    #[test]
    fn workspace_graph_can_include_remote_only_variation_nodes() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_ws = Workspace::init(first.path()).unwrap();
        configure_identity(&first_ws, "Seth", "seth@example.com");
        first_ws
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_ws.root(), "post.md", b"base");
        let base = first_ws.save_version("Base").unwrap();
        first_ws.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_ws =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_ws, "Maria", "maria@example.com");
        second_ws
            .create_variation_from(base.id(), "remote-only")
            .unwrap();
        second_ws
            .switch_variation(
                &VariationId::from("remote-only"),
                SwitchPolicy::AbortIfDirty,
            )
            .unwrap();
        write_file(second_ws.root(), "post.md", b"remote only");
        let remote_only = second_ws.save_version("Remote only").unwrap();
        second_ws.publish_changes("origin").unwrap();

        first_ws.fetch_all_variations("origin").unwrap();
        let graph = first_ws
            .workspace_graph(WorkspaceGraphOptions {
                include_remotes: true,
                remote: Some("origin".to_string()),
                ..WorkspaceGraphOptions::default()
            })
            .unwrap();

        let remote_ref = graph
            .refs
            .iter()
            .find(|graph_ref| {
                graph_ref.kind == WorkspaceGraphRefKind::RemoteVariation
                    && graph_ref.name == "remote-only"
            })
            .unwrap();
        assert_eq!(remote_ref.remote.as_deref(), Some("origin"));
        assert_eq!(remote_ref.target_version, *remote_only.id());

        let remote_node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == remote_only.id())
            .unwrap();
        assert_eq!(remote_node.kind, WorkspaceGraphNodeKind::RemoteOnly);
        assert!(!remote_node.reachable_from_local_variation);
        assert!(remote_node.reachable_from_remote_variation);
        assert!(!remote_node.reachable_from_support_ref);
        let remote_detail = first_ws
            .workspace_graph_node(remote_only.id(), WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(remote_detail.node.kind, WorkspaceGraphNodeKind::RemoteOnly);
        assert_eq!(remote_detail.changed_file_count, Some(1));
        assert!(!remote_node
            .available_actions
            .contains(&WorkspaceGraphAction::CreateVariationFromHere));
        assert!(matches!(
            first_ws
            .create_variation_from(remote_only.id(), "should-fail")
                .unwrap_err(),
            DraftlineError::VersionNotLocallyReachable(version) if version == remote_only.id().as_str()
        ));
    }

    #[test]
    fn workspace_graph_can_include_support_ref_only_nodes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        workspace
            .create_variation_from(base.id(), "deleted")
            .unwrap();
        workspace
            .switch_variation(&VariationId::from("deleted"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        write_file(workspace.root(), "post.md", b"deleted tip");
        let deleted_tip = workspace.save_version("Deleted tip").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        workspace
            .delete_variation(&VariationId::from("deleted"))
            .unwrap();

        let default_graph = workspace
            .workspace_graph(WorkspaceGraphOptions::default())
            .unwrap();
        assert!(!default_graph
            .nodes
            .iter()
            .any(|node| node.version.id() == deleted_tip.id()));

        let graph = workspace
            .workspace_graph(WorkspaceGraphOptions {
                include_support_refs: true,
                ..WorkspaceGraphOptions::default()
            })
            .unwrap();
        let support_ref = graph
            .refs
            .iter()
            .find(|graph_ref| {
                graph_ref.kind == WorkspaceGraphRefKind::SupportRef
                    && graph_ref.variation == Some(VariationId::from("deleted"))
            })
            .unwrap();
        assert_eq!(
            support_ref.support_ref_kind,
            Some(SupportRefKind::DeletedVariation)
        );
        assert_eq!(support_ref.target_version, *deleted_tip.id());

        let support_node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == deleted_tip.id())
            .unwrap();
        assert_eq!(support_node.kind, WorkspaceGraphNodeKind::SupportRefOnly);
        assert!(!support_node.reachable_from_local_variation);
        assert!(!support_node.reachable_from_remote_variation);
        assert!(support_node.reachable_from_support_ref);
        let support_detail = workspace
            .workspace_graph_node(deleted_tip.id(), WorkspaceGraphOptions::default())
            .unwrap();
        assert_eq!(
            support_detail.node.kind,
            WorkspaceGraphNodeKind::SupportRefOnly
        );
        assert_eq!(support_detail.changed_file_count, Some(1));
        assert!(!support_node
            .available_actions
            .contains(&WorkspaceGraphAction::CreateVariationFromHere));
        assert!(matches!(
            workspace
                .create_variation_from(deleted_tip.id(), "should-fail")
                .unwrap_err(),
            DraftlineError::VersionNotLocallyReachable(version) if version == deleted_tip.id().as_str()
        ));
    }

    #[test]
    fn workspace_graph_support_refs_take_priority_over_remote_reachability() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"base");
        let base = workspace.save_version("Base").unwrap();
        workspace
            .create_variation_from(base.id(), "published-deleted")
            .unwrap();
        workspace
            .switch_variation(
                &VariationId::from("published-deleted"),
                SwitchPolicy::AbortIfDirty,
            )
            .unwrap();
        write_file(workspace.root(), "post.md", b"published then deleted");
        let published_deleted = workspace.save_version("Published deleted").unwrap();
        workspace.publish_changes("origin").unwrap();
        workspace
            .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        workspace
            .delete_variation(&VariationId::from("published-deleted"))
            .unwrap();
        workspace.fetch_all_variations("origin").unwrap();

        let graph = workspace
            .workspace_graph(WorkspaceGraphOptions {
                include_remotes: true,
                remote: Some("origin".to_string()),
                include_support_refs: true,
                ..WorkspaceGraphOptions::default()
            })
            .unwrap();
        let node = graph
            .nodes
            .iter()
            .find(|node| node.version.id() == published_deleted.id())
            .unwrap();
        assert_eq!(node.kind, WorkspaceGraphNodeKind::SupportRefOnly);
        assert!(!node.reachable_from_local_variation);
        assert!(node.reachable_from_remote_variation);
        assert!(node.reachable_from_support_ref);
        let support_ref = graph
            .refs
            .iter()
            .find(|graph_ref| {
                graph_ref.kind == WorkspaceGraphRefKind::SupportRef
                    && graph_ref.variation == Some(VariationId::from("published-deleted"))
            })
            .unwrap();
        assert!(support_ref
            .available_actions
            .contains(&WorkspaceGraphAction::RestoreSupportRefAsVariation));
    }

    // ── variation_summaries ───────────────────────────────────────────────────

    #[test]
    fn variation_summaries_reports_head_and_count_per_variation() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        let second = workspace.save_version("Second").unwrap();

        // Create a variation that branches off at "Base" (2 commits on main,
        // 1 commit on the new variation).
        workspace
            .create_variation_from(workspace.versions().unwrap().last().unwrap().id(), "side")
            .unwrap();

        let summaries = workspace.variation_summaries().unwrap();
        let main = summaries
            .iter()
            .find(|s| s.variation.name == "main")
            .unwrap();
        let side = summaries
            .iter()
            .find(|s| s.variation.name == "side")
            .unwrap();

        assert_eq!(main.reachable_version_count, 2);
        assert_eq!(
            main.head_version.as_ref().map(|v| v.id()),
            Some(second.id())
        );

        // "side" branches from "Base" — the earliest commit — so 1 version.
        assert_eq!(side.reachable_version_count, 1);
        assert!(side.head_version.is_some());
    }

    #[test]
    fn variation_summaries_is_serializable() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Draft").unwrap();

        let summaries = workspace.variation_summaries().unwrap();
        let json = serde_json::to_string(&summaries).unwrap();
        assert!(json.contains("reachable_version_count"));
    }

    // ── preflight_apply_incoming ──────────────────────────────────────────────

    #[test]
    fn preflight_apply_incoming_reports_fast_forward_available() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_ws = Workspace::init(first.path()).unwrap();
        configure_identity(&first_ws, "Seth", "seth@example.com");
        first_ws
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_ws.root(), "post.md", b"one");
        first_ws.save_version("One").unwrap();
        first_ws.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_ws =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_ws, "Maria", "maria@example.com");
        write_file(second_ws.root(), "post.md", b"two");
        second_ws.save_version("Two").unwrap();
        second_ws.publish_changes("origin").unwrap();

        // first_ws is behind — preflight should see IncomingAvailable
        first_ws.fetch_remote("origin").unwrap();
        let report = first_ws.preflight_apply_incoming("origin").unwrap();

        assert!(report.is_fast_forward);
        assert!(report.can_proceed);
        assert!(report.dirty_files.is_empty());
        assert_eq!(report.sync_status.behind, 1);
    }

    #[test]
    fn preflight_apply_incoming_blocks_when_workspace_dirty() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_ws = Workspace::init(first.path()).unwrap();
        configure_identity(&first_ws, "Seth", "seth@example.com");
        first_ws
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_ws.root(), "post.md", b"one");
        first_ws.save_version("One").unwrap();
        first_ws.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_ws =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_ws, "Maria", "maria@example.com");
        write_file(second_ws.root(), "post.md", b"two");
        second_ws.save_version("Two").unwrap();
        second_ws.publish_changes("origin").unwrap();

        first_ws.fetch_remote("origin").unwrap();
        // dirty workspace should prevent proceed
        write_file(first_ws.root(), "post.md", b"unsaved");
        let report = first_ws.preflight_apply_incoming("origin").unwrap();

        assert!(!report.can_proceed);
        assert!(!report.dirty_files.is_empty());
    }

    // ── apply_incoming ────────────────────────────────────────────────────────

    #[test]
    fn apply_incoming_fast_forwards_local_variation() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let first = tempfile::tempdir().unwrap();
        let first_ws = Workspace::init(first.path()).unwrap();
        configure_identity(&first_ws, "Seth", "seth@example.com");
        first_ws
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_ws.root(), "post.md", b"one");
        first_ws.save_version("One").unwrap();
        first_ws.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_ws =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_ws, "Maria", "maria@example.com");
        write_file(second_ws.root(), "post.md", b"two");
        second_ws.save_version("Two").unwrap();
        second_ws.publish_changes("origin").unwrap();

        let mut options = RemoteOptions::new();
        let result = first_ws.apply_incoming("origin", &mut options).unwrap();

        assert_eq!(result.applied_count, 1);
        // workspace file should reflect the applied version
        let content = std::fs::read_to_string(first_ws.root().join("post.md")).unwrap();
        assert_eq!(content, "two");
        assert!(first_ws.recovery_state().unwrap().is_none());
    }

    #[test]
    fn apply_incoming_returns_zero_when_already_up_to_date() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Hello").unwrap();
        workspace.publish_changes("origin").unwrap();

        // already up to date
        let mut options = RemoteOptions::new();
        let result = workspace.apply_incoming("origin", &mut options).unwrap();
        assert_eq!(result.applied_count, 0);
    }

    // ── contributor profile attribution ─────────────────────────────────────

    fn attribution_profile() -> ContributorProfile {
        ContributorProfile::new(
            Contributor {
                name: "Profile Author".to_string(),
                email: Some("author@profile.invalid".to_string()),
            },
            Contributor {
                name: "Profile Saver".to_string(),
                email: Some("saver@profile.invalid".to_string()),
            },
        )
    }

    fn assert_profile_attribution(version: &Version) {
        assert_eq!(version.author.name, "Profile Author");
        assert_eq!(
            version.author.email.as_deref(),
            Some("author@profile.invalid")
        );
        assert_eq!(version.saved_by.name, "Profile Saver");
        assert_eq!(
            version.saved_by.email.as_deref(),
            Some("saver@profile.invalid")
        );
    }

    fn assert_git_config_attribution(version: &Version) {
        assert_eq!(version.author.name, "Seth");
        assert_eq!(version.author.email.as_deref(), Some("seth@example.com"));
        assert_eq!(version.saved_by.name, "Seth");
        assert_eq!(version.saved_by.email.as_deref(), Some("seth@example.com"));
    }

    fn attribution_workspace() -> (tempfile::TempDir, Workspace) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        (temp, workspace)
    }

    #[test]
    fn no_profile_operations_keep_their_pre_profile_git_config_signatures() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        let mut config = fs::read(workspace.repo.path().join("config")).unwrap();
        config.extend_from_slice(b"[user]\n\tname = Jos\xe9\n\temail = jose@example.com\n");
        fs::write(workspace.repo.path().join("config"), config).unwrap();
        let names = |version: &Version| {
            let commit = workspace
                .repo
                .find_commit(Oid::from_str(version.id().as_str()).unwrap())
                .unwrap();
            let author = commit.author().name_bytes().to_vec();
            let committer = commit.committer().name_bytes().to_vec();
            (author, committer)
        };

        write_file(workspace.root(), "post.md", b"base");
        let saved = workspace.save_version("Base").unwrap();
        // Save has always normalized the config identity through `Contributor`.
        assert_eq!(names(&saved), (b"Unknown".to_vec(), b"Unknown".to_vec()));

        write_file(workspace.root(), "post.md", b"shelved");
        let shelf = workspace.shelve_changes("wip").unwrap();
        // Shelve and restore have always used the raw config signature.
        assert_eq!(
            names(&shelf.version),
            (b"Jos\xe9".to_vec(), b"Jos\xe9".to_vec())
        );
    }

    #[test]
    fn shelve_changes_with_profile_records_profile_attribution() {
        let (_temp, workspace) = attribution_workspace();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"shelved");

        let shelf = workspace
            .shelve_changes_with_profile("profile-shelf", &attribution_profile())
            .unwrap();

        assert_profile_attribution(&shelf.version);
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "base"
        );
    }

    #[test]
    fn shelve_changes_without_profile_keeps_git_config_attribution() {
        let (_temp, workspace) = attribution_workspace();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"shelved");

        let shelf = workspace.shelve_changes("plain-shelf").unwrap();

        assert_git_config_attribution(&shelf.version);
    }

    #[test]
    fn shelve_files_with_profile_records_profile_attribution() {
        let (_temp, workspace) = attribution_workspace();
        write_file(workspace.root(), "one.md", b"one");
        write_file(workspace.root(), "two.md", b"two");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one shelved");
        write_file(workspace.root(), "two.md", b"two remains");

        let shelf = workspace
            .shelve_files_with_profile("profile-shelf", ["one.md"], &attribution_profile())
            .unwrap();

        assert_profile_attribution(&shelf.version);
        assert_eq!(
            fs::read_to_string(workspace.root().join("two.md")).unwrap(),
            "two remains"
        );
    }

    #[test]
    fn shelve_files_without_profile_keeps_git_config_attribution() {
        let (_temp, workspace) = attribution_workspace();
        write_file(workspace.root(), "one.md", b"one");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "one.md", b"one shelved");

        let shelf = workspace.shelve_files("plain-shelf", ["one.md"]).unwrap();

        assert_git_config_attribution(&shelf.version);
    }

    #[test]
    fn switch_variation_with_profile_attributes_shelve_and_save_first() {
        let (_temp, workspace) = attribution_workspace();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        let main = VariationId::from(workspace.current_variation().unwrap());
        workspace.create_variation("alt").unwrap();

        write_file(workspace.root(), "post.md", b"shelve me");
        workspace
            .switch_variation_with_profile(
                &VariationId::from("alt"),
                SwitchPolicy::Shelve {
                    name: "switch-shelf".to_string(),
                },
                &attribution_profile(),
            )
            .unwrap();
        let shelf = workspace
            .list_shelves()
            .unwrap()
            .into_iter()
            .find(|shelf| shelf.id == "switch-shelf")
            .unwrap();
        assert_profile_attribution(&shelf.version);

        write_file(workspace.root(), "post.md", b"save me");
        workspace
            .switch_variation_with_profile(
                &main,
                SwitchPolicy::SaveFirst {
                    label: "Saved before switch".to_string(),
                },
                &attribution_profile(),
            )
            .unwrap();
        let alt_tip = workspace
            .repo
            .find_branch("alt", BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let saved = version_from_commit(&alt_tip);
        assert_eq!(saved.label, "Saved before switch");
        assert_profile_attribution(&saved);
    }

    fn invalid_profile() -> ContributorProfile {
        ContributorProfile::new(
            Contributor {
                name: "   ".to_string(),
                email: None,
            },
            Contributor {
                name: "Profile Saver".to_string(),
                email: None,
            },
        )
    }

    #[test]
    fn shelve_with_invalid_profile_fails_without_pending_recovery() {
        let (_temp, workspace) = attribution_workspace();
        write_file(workspace.root(), "post.md", b"base");
        workspace.save_version("Base").unwrap();
        write_file(workspace.root(), "post.md", b"dirty");

        let err = workspace
            .shelve_files_with_profile("bad-shelf", ["post.md"], &invalid_profile())
            .unwrap_err();
        assert!(matches!(err, DraftlineError::InvalidContributorIdentity(_)));
        let err = workspace
            .shelve_changes_with_profile("bad-shelf", &invalid_profile())
            .unwrap_err();
        assert!(matches!(err, DraftlineError::InvalidContributorIdentity(_)));

        workspace.ensure_no_pending_recovery().unwrap();
        assert!(workspace.list_shelves().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "dirty"
        );
    }

    fn restore_fixture(workspace: &Workspace) -> Version {
        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();
        first
    }

    #[test]
    fn restore_version_as_new_save_with_profile_records_profile_attribution() {
        let (_temp, workspace) = attribution_workspace();
        let first = restore_fixture(&workspace);

        let restored = workspace
            .restore_version_as_new_save_with_profile(
                first.id(),
                "Restore first",
                &attribution_profile(),
            )
            .unwrap();

        assert_eq!(restored.label, "Restore first");
        assert_profile_attribution(&restored);
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "first"
        );
    }

    #[test]
    fn restore_version_as_new_save_without_profile_keeps_git_config_attribution() {
        let (_temp, workspace) = attribution_workspace();
        let first = restore_fixture(&workspace);

        let restored = workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap();

        assert_git_config_attribution(&restored);
    }

    #[test]
    fn restore_with_invalid_profile_fails_without_pending_recovery() {
        let (_temp, workspace) = attribution_workspace();
        let first = restore_fixture(&workspace);
        let head = workspace.repo.head().unwrap().target().unwrap();

        let err = workspace
            .restore_version_as_new_save_with_profile(first.id(), "Bad", &invalid_profile())
            .unwrap_err();
        assert!(matches!(err, DraftlineError::InvalidContributorIdentity(_)));
        let err = workspace
            .restore_version_as_new_save_to_variation_with_profile(
                first.id(),
                "Bad",
                RestoreVersionTarget::Current,
                &invalid_profile(),
            )
            .unwrap_err();
        assert!(matches!(err, DraftlineError::InvalidContributorIdentity(_)));

        workspace.ensure_no_pending_recovery().unwrap();
        assert_eq!(workspace.repo.head().unwrap().target().unwrap(), head);
    }

    #[test]
    fn restore_version_to_variation_with_profile_records_profile_attribution() {
        let (_temp, workspace) = attribution_workspace();
        let first = restore_fixture(&workspace);

        let current = workspace
            .restore_version_as_new_save_to_variation_with_profile(
                first.id(),
                "Restore on current",
                RestoreVersionTarget::Current,
                &attribution_profile(),
            )
            .unwrap();
        assert_profile_attribution(&current.version);

        let created = workspace
            .restore_version_as_new_save_to_variation_with_profile(
                first.id(),
                "Restore on new",
                RestoreVersionTarget::New {
                    name: "restored".to_string(),
                    metadata: VariationMetadata::default(),
                },
                &attribution_profile(),
            )
            .unwrap();
        assert_eq!(created.target_variation.id().as_str(), "restored");
        assert_profile_attribution(&created.version);
    }

    #[test]
    fn restore_version_to_variation_without_profile_keeps_git_config_attribution() {
        let (_temp, workspace) = attribution_workspace();
        let first = restore_fixture(&workspace);

        let restored = workspace
            .restore_version_as_new_save_to_variation(
                first.id(),
                "Restore on current",
                RestoreVersionTarget::Current,
            )
            .unwrap();

        assert_git_config_attribution(&restored.version);
    }
}
