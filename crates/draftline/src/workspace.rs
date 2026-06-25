use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{
    build::{CheckoutBuilder, RepoBuilder},
    BranchType, Commit, DiffFormat, DiffOptions, ObjectType, Oid, Repository, Signature, Status,
    StatusOptions, Tree,
};
use serde::{Deserialize, Serialize};

use crate::recovery::RecoveryOperation;
use crate::{
    path::normalize_workspace_relative, ContentPolicy, Contributor, DraftlineError,
    PublishPreflight, PublishResult, PublishToken, RecoveryState, RemoteEndpoint, RemoteOptions,
    RemoteVersionSummary, Result, SyncState, SyncStatus,
};

/// A folder-backed content workspace.
pub struct Workspace {
    root: PathBuf,
    repo: Repository,
    content_policy: ContentPolicy,
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

/// Stable diagnostic codes for workspace inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticCode {
    RecoveryRequired,
    WorkspaceLocked,
    DirtyWorkspace,
    LocalOnlyWorkspace,
    SharedCapableWorkspace,
    NoCurrentVariation,
    WorkspaceReadFailed,
    PolicyTrackedFileIgnored,
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

/// Result returned by recovery repair and rollback entry points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryRepairResult {
    pub operation_id: String,
    pub operation: RecoveryOperation,
    pub completed: bool,
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

/// A variation discovered on a remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteVariation {
    pub id: VariationId,
    pub name: String,
    pub remote: String,
    pub head_version: Option<Version>,
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
    pub can_merge_cleanly: bool,
    pub changed_workspace: bool,
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

impl Workspace {
    /// Initializes a new workspace or opens the existing workspace at `path`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_policy(path, ContentPolicy::default())
    }

    /// Initializes a workspace with an explicit content policy.
    pub fn init_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;

        let repo = match Repository::open(path.as_ref()) {
            Ok(repo) => repo,
            Err(_) => Repository::init(path.as_ref())?,
        };

        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());

        Ok(Self {
            root,
            repo,
            content_policy,
        }
        .initialize())
    }

    /// Opens an existing workspace.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_policy(path, ContentPolicy::default())
    }

    /// Opens an existing workspace with an explicit content policy.
    pub fn open_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        let repo = Repository::discover(path.as_ref())?;
        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());

        Ok(Self {
            root,
            repo,
            content_policy,
        }
        .initialize())
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
        let mut builder = RepoBuilder::new();
        if options.has_credentials() {
            let fetch_options = options.clone_fetch_options();
            builder.fetch_options(fetch_options);
        }
        let repo = builder.clone(remote_url.as_ref(), path.as_ref())?;
        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());

        Ok(Self {
            root,
            repo,
            content_policy,
        }
        .initialize())
    }

    /// Returns the root content folder for this workspace.
    pub fn root(&self) -> &Path {
        &self.root
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

        if recovery.is_some() {
            diagnostics.push(workspace_diagnostic(
                DiagnosticCode::RecoveryRequired,
                DiagnosticSeverity::Blocking,
                "workspace has an incomplete Draftline operation",
            ));
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
            support_ref_sync: false,
            agent_cli_facade: false,
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
             - Use Draftline primitives for variation, cleanup, support-ref sync, recovery, and shared history replacement.\n"
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
        let state: RecoveryState = serde_json::from_str(&text)?;

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

    /// Entry point for operation-specific recovery repair.
    ///
    /// This slice exposes the typed repair surface and reports the interrupted
    /// operation. Operation-specific mutation is added by later recovery work.
    pub fn repair_recovery(&self, operation_id: impl AsRef<str>) -> Result<RecoveryRepairResult> {
        self.recovery_skeleton(operation_id.as_ref())
    }

    /// Entry point for operation-specific recovery rollback.
    ///
    /// This slice exposes the typed rollback surface and reports the interrupted
    /// operation. Operation-specific mutation is added by later recovery work.
    pub fn rollback_recovery(&self, operation_id: impl AsRef<str>) -> Result<RecoveryRepairResult> {
        self.recovery_skeleton(operation_id.as_ref())
    }

    /// Acknowledges an incomplete recovery record and allows normal operations again.
    pub fn acknowledge_recovery(&self) -> Result<()> {
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
        self.ensure_no_pending_recovery()?;
        self.save_version_unchecked(label)
    }

    fn save_version_unchecked(&self, label: impl AsRef<str>) -> Result<Version> {
        let mut index = self.repo.index()?;

        for changed in self.changed_files_unchecked()? {
            match changed.kind {
                ChangeKind::Deleted => index.remove_path(&changed.path)?,
                _ => index.add_path(&changed.path)?,
            }
        }

        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let signature = self.workspace_signature()?;
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| self.repo.find_commit(oid).ok());

        let oid = match parent.as_ref() {
            Some(parent) => self.repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                label.as_ref(),
                &tree,
                &[parent],
            )?,
            None => self.repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                label.as_ref(),
                &tree,
                &[],
            )?,
        };

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    /// Lists versions reachable from the current variation, newest first.
    pub fn versions(&self) -> Result<Vec<Version>> {
        self.ensure_no_pending_recovery()?;
        let mut walk = self.repo.revwalk()?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }

        walk.map(|oid| {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            Ok(version_from_commit(&commit))
        })
        .collect()
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

        let statuses = self.repo.statuses(Some(&mut options))?;
        let mut changed = Vec::new();

        for entry in statuses.iter() {
            let Some(path) = entry.path() else {
                continue;
            };
            if !self.content_policy.tracks(path)? {
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

        let statuses = self.repo.statuses(Some(&mut options))?;
        let mut ignored_paths = Vec::new();

        for entry in statuses.iter() {
            if !entry.status().contains(Status::IGNORED) {
                continue;
            }

            let Some(path) = entry.path() else {
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

        let statuses = self.repo.statuses(Some(&mut options))?;
        let mut hazards = Vec::new();

        for entry in statuses.iter() {
            let Some(path) = entry.path() else {
                continue;
            };

            if !tree_contains_path(target_tree, Path::new(path))? {
                continue;
            }

            let status = entry.status();
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

        hazards.sort_by(|left, right| left.path.cmp(&right.path));
        hazards.dedup_by(|left, right| left.path == right.path);
        Ok(hazards)
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
        Ok(discard_preflight_report(
            "discard_changes",
            self.changed_files_unchecked()?,
        ))
    }

    /// Discards all unsaved changes tracked by the active content policy.
    ///
    /// Files excluded by [`ContentPolicy`] are preserved. Added tracked files are
    /// removed; modified, deleted, renamed, type-changed, and conflicted tracked
    /// files are restored from the current `HEAD`.
    pub fn discard_changes(&self) -> Result<ChangeSet> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "discard_changes")?;
        let changed_files = self.changed_files_unchecked()?;
        if changed_files.is_empty() {
            return Ok(ChangeSet {
                files: Vec::new(),
                diff: None,
            });
        }

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

        Ok(discard_preflight_report("discard_file", changed_files))
    }

    /// Discards one changed tracked content file by workspace-relative path.
    ///
    /// The path is normalized and must stay inside the workspace and inside the
    /// active content policy. Excluded runtime files are rejected instead of
    /// being reset.
    pub fn discard_file(&self, path: impl AsRef<Path>) -> Result<Option<ChangedFile>> {
        self.ensure_no_pending_recovery()?;
        let path = self.normalize_tracked_content_path(path)?;
        let _lock = OperationLock::acquire(&self.lock_path(), "discard_file")?;
        let changed_file = self
            .changed_files_unchecked()?
            .into_iter()
            .find(|changed| changed.path == path);

        let Some(changed_file) = changed_file else {
            return Ok(None);
        };

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

    /// Preflights switching to another variation without mutating workspace files.
    pub fn preflight_switch_variation(&self, variation: &VariationId) -> Result<PreflightReport> {
        self.ensure_no_pending_recovery()?;
        self.preflight_switch_variation_unchecked(variation)
    }

    fn preflight_switch_variation_unchecked(
        &self,
        variation: &VariationId,
    ) -> Result<PreflightReport> {
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

    /// Creates a new variation from the current version.
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
        let commit = self.find_version_commit(version)?;
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
        let current = self.current_variation().ok();
        let mut paths = Vec::new();

        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };

            let metadata = self.read_variation_metadata(name)?;
            paths.push(variation_from_name(
                name.to_string(),
                current.as_ref(),
                metadata,
            ));
        }

        paths.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(paths)
    }

    /// Lists visible variations discovered on a remote-tracking namespace.
    pub fn remote_variations(&self, remote: impl AsRef<str>) -> Result<Vec<RemoteVariation>> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        let prefix = format!("refs/remotes/{remote}/");
        let mut variations = Vec::new();

        for reference in self.repo.references_glob(&format!("{prefix}*"))? {
            let reference = reference?;
            if reference.kind() != Some(git2::ReferenceType::Direct) {
                continue;
            }

            let Some(ref_name) = reference.name() else {
                continue;
            };
            let Some(name) = ref_name.strip_prefix(&prefix) else {
                continue;
            };
            if name == "HEAD" || name.ends_with("/HEAD") {
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

    /// Creates a local variation from a remote-tracking variation.
    pub fn adopt_remote_variation(
        &self,
        remote: impl AsRef<str>,
        variation: &VariationId,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(variation.as_str())?;
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
        self.ensure_no_pending_recovery()?;
        let safe_name = validate_variation_name(name.as_ref())?;
        let _lock = OperationLock::acquire(&self.lock_path(), "shelve_changes")?;
        self.shelve_changes_unchecked(&safe_name)?;
        self.shelf_by_id(&safe_name)
    }

    /// Lists local shelves.
    pub fn list_shelves(&self) -> Result<Vec<Shelf>> {
        self.ensure_no_pending_recovery()?;
        let mut shelves = Vec::new();
        let references = self.repo.references_glob("refs/draftline/shelves/*")?;

        for reference in references {
            let reference = reference?;
            let Some(name) = reference.name() else {
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
        let tree = commit.tree()?;
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
        let _lock = OperationLock::acquire(&self.lock_path(), "apply_shelf")?;
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
        let tree = commit.tree()?;
        self.repo
            .checkout_tree(tree.as_object(), Some(CheckoutBuilder::new().force()))?;

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
            SupportRefScope::RemoteTracking => Ok(Vec::new()),
        }
    }

    /// Restores an archive support ref as a new visible variation.
    pub fn restore_support_ref_as_variation(
        &self,
        id: impl AsRef<str>,
        name: impl AsRef<str>,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let id = id.as_ref();
        if !is_restorable_support_ref(id) {
            return Err(DraftlineError::VersionNotFound(id.to_string()));
        }

        let name = validate_variation_name(name.as_ref())?;
        let reference = self.repo.find_reference(id)?;
        let commit = reference.peel_to_commit()?;
        self.repo.branch(&name, &commit, false)?;
        let metadata = self.read_variation_metadata(&name)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
            metadata,
        ))
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
        let _lock = OperationLock::acquire(&self.lock_path(), "delete_remote_variation")?;
        let variation_name = validate_variation_name(token.variation.as_str())?;
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

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::DeleteRemoteVariation,
            original_variation: Some(variation_name.clone()),
            target: Some(token.expected_remote_oid.clone()),
            completed: false,
        })?;

        let target_oid = Oid::from_str(&token.expected_remote_oid)
            .map_err(|_| DraftlineError::VersionNotFound(token.expected_remote_oid.clone()))?;
        self.repo.reference(
            &token.support_ref,
            target_oid,
            false,
            "archive remote variation before delete",
        )?;
        self.push_refspec(
            &token.remote,
            &format!("{}:{}", token.support_ref, token.support_ref),
            options,
        )?;
        self.push_refspec(
            &token.remote,
            &format!(":refs/heads/{variation_name}"),
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
        let _lock = OperationLock::acquire(&self.lock_path(), "expire_support_refs")?;
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
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "switch_variation")?;
        let mut report = self.preflight_switch_variation_unchecked(variation)?;

        if !report.file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        match &policy {
            SwitchPolicy::AbortIfDirty if !report.can_proceed => {
                return Err(DraftlineError::PreflightFailed(Box::new(report)));
            }
            SwitchPolicy::SaveFirst { label } if !report.can_proceed => {
                self.save_version_unchecked(label)?;
            }
            SwitchPolicy::Shelve { name } if !report.can_proceed => {
                self.shelve_changes_unchecked(name)?;
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

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::SwitchVariation,
            original_variation: self.current_variation().ok(),
            target: Some(variation.as_str().to_string()),
            completed: false,
        })?;

        let branch = self
            .repo
            .find_branch(variation.as_str(), BranchType::Local)?;
        let reference = branch.into_reference();
        let target = reference.peel(ObjectType::Commit)?;

        self.repo.checkout_tree(&target, None)?;
        self.repo
            .set_head(&format!("refs/heads/{}", variation.as_str()))?;

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

    /// Deletes an alternate variation.
    pub fn delete_variation(&self, variation: &VariationId) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "delete_variation")?;
        if self.current_variation().ok().as_deref() == Some(variation.as_str()) {
            return Err(DraftlineError::CannotDeleteCurrentVariation(
                variation.as_str().to_string(),
            ));
        }

        let operation_id = new_operation_id();
        let mut branch = self
            .repo
            .find_branch(variation.as_str(), BranchType::Local)?;
        let target_oid = branch.get().peel_to_commit()?.id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::DeleteVariation,
            original_variation: Some(variation.as_str().to_string()),
            target: Some(target_oid.to_string()),
            completed: false,
        })?;

        let archive_ref = archive_ref("deleted-variations", variation.as_str(), &operation_id);
        self.repo
            .reference(&archive_ref, target_oid, false, "archive deleted variation")?;
        branch.delete()?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::DeleteVariation,
            original_variation: None,
            target: Some(target_oid.to_string()),
            completed: true,
        })?;
        Ok(())
    }

    /// Creates a new version from an earlier version without switching variations.
    pub fn restore_version_as_new_save(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
    ) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "restore_version_as_new_save")?;
        let report = preflight_report(
            "restore_version_as_new_save",
            true,
            self.changed_files_unchecked()?,
            {
                let commit = self.find_version_commit(version)?;
                self.target_tree_hazards(&commit.tree()?)?
            },
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: self.current_variation().ok(),
            target: Some(version.as_str().to_string()),
            completed: false,
        })?;

        let commit = self.find_version_commit(version)?;
        let tree = commit.tree()?;
        let signature = self.workspace_signature()?;
        let parent = self.repo.head()?.peel_to_commit()?;
        let oid = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            label.as_ref(),
            &tree,
            &[&parent],
        )?;

        self.repo
            .checkout_tree(tree.as_object(), Some(CheckoutBuilder::new().force()))?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: None,
            target: Some(version.as_str().to_string()),
            completed: true,
        })?;

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    /// Reads a version without mutating the live workspace.
    pub fn preview_version(&self, version: &VersionId) -> Result<VersionPreview> {
        self.ensure_no_pending_recovery()?;
        let commit = self.find_version_commit(version)?;
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
        let tree = commit.tree()?;
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

        Ok(WorkspaceSummary {
            active_variation,
            variations,
            versions,
            dirty_files,
            is_dirty,
            recovery,
            state_may_be_inconsistent,
        })
    }

    /// Returns the version history for the current variation with variation-tip annotations.
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

        walk.map(|oid| {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            history_entry_from_commit(&commit, &tips, head_oid)
        })
        .collect()
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

        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            if let Ok(tip) = branch.get().peel_to_commit() {
                walk.push(tip.id())?;
            }
        }

        walk.map(|oid| {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            history_entry_from_commit(&commit, &tips, head_oid)
        })
        .collect()
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
            let metadata = self.read_variation_metadata(name)?;
            let variation = variation_from_name(name.to_string(), current.as_ref(), metadata);

            let (head_version, reachable_version_count) = match branch.get().peel_to_commit() {
                Ok(tip) => {
                    let head_version = Some(version_from_commit(&tip));
                    let mut walk = self.repo.revwalk()?;
                    walk.push(tip.id())?;
                    let count = walk.count();
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
        let sync_status = self.sync_status(remote)?;
        let dirty_files = self.changed_files_unchecked()?;

        let is_fast_forward = matches!(sync_status.state, SyncState::IncomingAvailable);
        let file_hazards = if is_fast_forward {
            let remote_ref = format!(
                "refs/remotes/{}/{}",
                sync_status.remote, sync_status.variation
            );
            let remote_oid = self.repo.refname_to_id(&remote_ref)?;
            let remote_commit = self.repo.find_commit(remote_oid)?;
            self.target_tree_hazards(&remote_commit.tree()?)?
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
        let dirty_files = self.changed_files_unchecked()?;

        Ok(MergeIncomingReport {
            sync_status,
            dirty_files,
            file_hazards: Vec::new(),
            can_merge_cleanly: false,
            changed_workspace: false,
        })
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
        let _lock = OperationLock::acquire(&self.lock_path(), "apply_incoming")?;

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

        let remote_name = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote_name, options)?;
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
        let variation = self.current_variation_unchecked()?;
        let remote_ref = format!("refs/remotes/{remote_name}/{variation}");
        let remote_oid = self.repo.refname_to_id(&remote_ref)?;
        let remote_commit = self.repo.find_commit(remote_oid)?;
        let file_hazards = self.target_tree_hazards(&remote_commit.tree()?)?;
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

        // Save the original OID so we can roll back if checkout fails.
        let original_oid = self.repo.refname_to_id(&branch_ref).ok();

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ApplyIncoming,
            original_variation: Some(variation.clone()),
            target: Some(remote_oid.to_string()),
            completed: false,
        })?;

        // Fast-forward the local branch ref.
        self.repo
            .reference(&branch_ref, remote_oid, true, "apply_incoming fast-forward")?;

        // Bring the working directory up to the new tree.  Roll back the ref if
        // checkout fails so the repo is not left with a moved branch and stale tree.
        if let Err(checkout_err) = self.repo.checkout_tree(
            remote_commit.tree()?.as_object(),
            Some(CheckoutBuilder::new().force()),
        ) {
            if let Some(orig) = original_oid {
                let _ = self
                    .repo
                    .reference(&branch_ref, orig, true, "apply_incoming rollback");
            }
            let _ = self.acknowledge_recovery();
            return Err(checkout_err.into());
        }

        self.repo.set_head(&branch_ref)?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ApplyIncoming,
            original_variation: None,
            target: Some(remote_oid.to_string()),
            completed: true,
        })?;

        Ok(ApplyIncomingResult { applied_count })
    }

    /// Squashes the last `count` versions into a single new version.
    ///
    /// The new version has the same tree as the current `HEAD` but is parented
    /// directly on the commit that preceded the squashed range, collapsing
    /// `count` intermediate commits into one.
    ///
    /// Requires:
    /// - `count >= 2`
    /// - The workspace must be clean (no unsaved changes).
    /// - The current variation must have at least `count + 1` versions (so
    ///   there is a parent commit outside the squash range to attach to).
    ///
    /// ```no_run
    /// use draftline::Workspace;
    ///
    /// let workspace = Workspace::open("my-content")?;
    /// let squashed = workspace.squash_versions(3, "Squashed three drafts")?;
    /// println!("squashed → {}", squashed.label);
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn squash_versions(&self, count: usize, label: impl AsRef<str>) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "squash_versions")?;

        if count < 2 {
            return Err(DraftlineError::InvalidSquashCount(count));
        }

        let dirty_files = self.changed_files_unchecked()?;
        if !dirty_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "squash_versions",
                false,
                dirty_files,
                Vec::new(),
                None,
            ))));
        }

        let mut walk = self.repo.revwalk()?;
        walk.push_head()?;
        let commit_oids: Vec<Oid> = walk
            .take(count)
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if commit_oids.len() < count {
            return Err(DraftlineError::NotEnoughVersionsToSquash {
                needed: count,
                available: commit_oids.len(),
            });
        }

        let head_commit = self.repo.find_commit(commit_oids[0])?;
        let oldest_commit = self.repo.find_commit(commit_oids[count - 1])?;

        // The squash commit's parent is the commit that precedes the squash range.
        let squash_parent =
            oldest_commit
                .parent(0)
                .map_err(|_| DraftlineError::NotEnoughVersionsToSquash {
                    needed: count + 1,
                    available: count,
                })?;

        let tree = head_commit.tree()?;
        let signature = self.workspace_signature()?;

        // Create the squash commit without touching any ref yet — git2 would
        // reject Some("HEAD") here because squash_parent is not the current tip.
        let oid = self.repo.commit(
            None,
            &signature,
            &signature,
            label.as_ref(),
            &tree,
            &[&squash_parent],
        )?;

        // Force the current branch to point at the new squash commit.
        let variation = self.current_variation_unchecked()?;
        let branch_ref = format!("refs/heads/{variation}");
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::SquashVersions,
            original_variation: Some(variation.clone()),
            target: Some(head_commit.id().to_string()),
            completed: false,
        })?;
        let archive_ref = archive_ref("rewrites/squash", &variation, &operation_id);
        self.repo.reference(
            &archive_ref,
            head_commit.id(),
            false,
            "archive pre-squash tip",
        )?;
        self.repo
            .reference(&branch_ref, oid, true, "squash_versions")?;
        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::SquashVersions,
            original_variation: None,
            target: Some(oid.to_string()),
            completed: true,
        })?;

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
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
        let from_tree = from_commit.tree()?;
        let to_tree = to_commit.tree()?;

        let mut opts = DiffOptions::new();
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
        let tree = commit.tree()?;

        let mut opts = DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
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
                let Some(name) = head.shorthand() else {
                    return Err(DraftlineError::NoCurrentVariation);
                };
                Ok(name.to_string())
            }
            Err(error) if error.code() == git2::ErrorCode::UnbornBranch => {
                // New repository with no commits — derive the intended initial branch
                // name from the HEAD symbolic reference (e.g. refs/heads/master → "master").
                self.repo
                    .find_reference("HEAD")
                    .ok()
                    .and_then(|r| r.symbolic_target().map(str::to_string))
                    .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_string))
                    .ok_or(DraftlineError::NoCurrentVariation)
            }
            Err(error) => Err(error.into()),
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
            url: url.to_string(),
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

        for name in names.iter().flatten() {
            let remote = self.repo.find_remote(name)?;
            remotes.push(RemoteEndpoint {
                name: name.to_string(),
                url: remote.url().unwrap_or_default().to_string(),
            });
        }

        remotes.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(remotes)
    }

    fn recovery_skeleton(&self, operation_id: &str) -> Result<RecoveryRepairResult> {
        let Some(state) = self.recovery_state()? else {
            return Err(DraftlineError::RecoveryNotFound(operation_id.to_string()));
        };

        if state.operation_id != operation_id {
            return Err(DraftlineError::RecoveryNotFound(operation_id.to_string()));
        }

        Ok(RecoveryRepairResult {
            operation_id: state.operation_id,
            operation: state.operation,
            completed: false,
            changed_workspace: false,
            safe_next_actions: vec![SafeNextAction::RepairRecovery],
        })
    }

    fn push_current_variation_with_options(
        &self,
        remote_name: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        let mut remote = self.repo.find_remote(remote_name)?;
        let refspec = format!("refs/heads/{variation}:refs/heads/{variation}");
        if options.has_credentials() {
            let mut push_options = options.push_options();
            remote.push(&[refspec.as_str()], Some(&mut push_options))?;
        } else {
            remote.push(&[refspec.as_str()], None)?;
        }

        Ok(())
    }

    fn push_refspec(
        &self,
        remote_name: &str,
        refspec: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let mut remote = self.repo.find_remote(remote_name)?;
        if options.has_credentials() {
            let mut push_options = options.push_options();
            remote.push(&[refspec], Some(&mut push_options))?;
        } else {
            remote.push(&[refspec], None)?;
        }

        Ok(())
    }

    fn fetch_remote_variation_ref(
        &self,
        remote_name: &str,
        variation: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let mut remote = self.repo.find_remote(remote_name)?;
        let refspec = format!("+refs/heads/{variation}:refs/remotes/{remote_name}/{variation}");
        let fetch_result = if options.has_credentials() {
            let mut fetch_options = options.fetch_options();
            remote.fetch(&[refspec.as_str()], Some(&mut fetch_options), None)
        } else {
            remote.fetch(&[refspec.as_str()], None, None)
        };

        if let Err(error) = fetch_result {
            if error.code() != git2::ErrorCode::NotFound {
                return Err(error.into());
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
            &mut support_refs,
        )?;
        self.collect_support_refs(
            "refs/draftline/rewrites/squash/*/*",
            SupportRefKind::Rewrite,
            "refs/draftline/rewrites/squash/",
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
        support_refs: &mut Vec<SupportRef>,
    ) -> Result<()> {
        for reference in self.repo.references_glob(glob)? {
            let reference = reference?;
            let Some(ref_name) = reference.name() else {
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
                scope: SupportRefScope::Local,
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
            "refs/tags/*",
            "refs/notes/*",
            "refs/replace/*",
        ] {
            for reference in self.repo.references_glob(glob)? {
                let reference = reference?;
                if reference.kind() != Some(git2::ReferenceType::Direct) {
                    continue;
                }
                if let Some(name) = reference.name() {
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
        self.fetch_remote_unchecked(remote, &mut options)
    }

    /// Fetches remote version metadata with explicit remote options.
    pub fn fetch_remote_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        self.fetch_remote_unchecked(remote, options)
    }

    fn fetch_remote_unchecked(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        let mut remote = self.repo.find_remote(remote.as_ref())?;
        let fetch_result = if options.has_credentials() {
            let mut fetch_options = options.fetch_options();
            remote.fetch(&[variation.as_str()], Some(&mut fetch_options), None)
        } else {
            remote.fetch(&[variation.as_str()], None, None)
        };
        if let Err(error) = fetch_result {
            if error.code() != git2::ErrorCode::NotFound {
                return Err(error.into());
            }
        }
        Ok(())
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
            return Ok(SyncStatus {
                remote,
                variation,
                ahead,
                behind: 0,
                state: SyncState::NoRemoteVersion,
                incoming: Vec::new(),
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
        })
    }

    /// Publishes local versions for the current variation when doing so will not overwrite remote work.
    pub fn publish_changes(&self, remote: impl AsRef<str>) -> Result<PublishResult> {
        let mut options = RemoteOptions::new();
        self.publish_changes_with_options(remote, &mut options)
    }

    /// Preflights publishing and captures the expected remote OID or absence.
    pub fn preflight_publish(&self, remote: impl AsRef<str>) -> Result<PublishPreflight> {
        self.ensure_no_pending_recovery()?;
        let report = preflight_report(
            "publish_changes",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let remote = remote.as_ref().to_string();
        let mut options = RemoteOptions::new();
        self.fetch_remote_unchecked(&remote, &mut options)?;
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

        self.push_current_variation_with_options(&token.remote, options)?;

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
        self.ensure_no_pending_recovery()?;
        let report = preflight_report(
            "publish_changes",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let remote_name = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote_name, options)?;
        let status = self.sync_status(&remote_name)?;
        if matches!(
            status.state,
            SyncState::IncomingAvailable | SyncState::NeedsMerge
        ) {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }

        let variation = self.current_variation_unchecked()?;
        let push_result = self.push_current_variation_with_options(&remote_name, options);
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

    fn lock_path(&self) -> PathBuf {
        self.draftline_dir().join("operation.lock")
    }

    fn write_recovery_state(&self, state: &RecoveryState) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        fs::write(self.ledger_path(), serde_json::to_vec_pretty(state)?)?;
        Ok(())
    }

    fn initialize(self) -> Self {
        let _ = fs::create_dir_all(self.draftline_dir());
        self
    }

    fn ensure_no_pending_recovery(&self) -> Result<()> {
        if let Some(state) = self.recovery_state()? {
            return Err(DraftlineError::RecoveryRequired(Box::new(state)));
        }

        Ok(())
    }

    fn normalize_tracked_content_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = normalize_workspace_relative(path)?;
        if path.as_os_str().is_empty() || !self.content_policy.tracks(&path)? {
            return Err(DraftlineError::PathOutsideContentPolicy(path));
        }

        Ok(path)
    }

    fn discard_changed_files(&self, changed_files: &[ChangedFile]) -> Result<()> {
        let mut index = self.repo.index()?;
        let mut checkout = CheckoutBuilder::new();
        checkout.force();
        let mut paths_to_reset = Vec::new();
        let mut paths_to_remove_after_checkout = Vec::new();

        for changed in changed_files {
            match changed.kind {
                ChangeKind::Added => {
                    if let Err(error) = index.remove_path(&changed.path) {
                        if error.code() != git2::ErrorCode::NotFound {
                            return Err(error.into());
                        }
                    }
                    remove_workspace_path_if_exists(&self.root.join(&changed.path))?;
                }
                ChangeKind::Renamed => {
                    if let Some(target) = self.staged_rename_target_for_path(&changed.path)? {
                        checkout.path(&target);
                        paths_to_reset.push(target.clone());
                        paths_to_remove_after_checkout.push(target);
                    }
                    checkout.path(&changed.path);
                    paths_to_reset.push(changed.path.clone());
                }
                ChangeKind::Modified
                | ChangeKind::Deleted
                | ChangeKind::Conflicted
                | ChangeKind::TypeChanged => {
                    checkout.path(&changed.path);
                    paths_to_reset.push(changed.path.clone());
                }
            }
        }

        index.write()?;
        drop(index);

        if !paths_to_reset.is_empty() {
            let head = self.repo.head()?.peel(ObjectType::Commit)?;
            self.repo
                .reset_default(Some(&head), paths_to_reset.iter().map(PathBuf::as_path))?;
            self.repo.checkout_head(Some(&mut checkout))?;
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
            if !entry.status().is_index_renamed() || entry.path().map(Path::new) != Some(path) {
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

    fn shelve_changes_unchecked(&self, name: &str) -> Result<()> {
        let safe_name = validate_variation_name(name)?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ShelveChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(safe_name.clone()),
            completed: false,
        })?;

        let changed_files = self.changed_files_unchecked()?;
        let untracked_content: Vec<PathBuf> = changed_files
            .iter()
            .filter(|changed| changed.kind == ChangeKind::Added)
            .map(|changed| changed.path.clone())
            .collect();

        let mut index = self.repo.index()?;
        for changed in changed_files {
            match changed.kind {
                ChangeKind::Deleted => index.remove_path(&changed.path)?,
                _ => index.add_path(&changed.path)?,
            }
        }
        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let signature = self.workspace_signature()?;
        let parent = self.repo.head()?.peel_to_commit()?;
        let oid = self.repo.commit(
            None,
            &signature,
            &signature,
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

        self.repo
            .checkout_head(Some(CheckoutBuilder::new().force()))?;

        for path in untracked_content {
            let full_path = self.root.join(path);
            if full_path.is_file() {
                fs::remove_file(full_path)?;
            }
        }

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

    fn versions_unchecked(&self) -> Result<Vec<Version>> {
        let mut walk = self.repo.revwalk()?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }
        walk.map(|oid| {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            Ok(version_from_commit(&commit))
        })
        .collect()
    }

    fn variations_unchecked(&self) -> Result<Vec<Variation>> {
        let current = self.current_variation_unchecked().ok();
        let mut paths = Vec::new();
        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };
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

    fn variation_tips_map(&self) -> Result<HashMap<Oid, Vec<VariationId>>> {
        let mut map: HashMap<Oid, Vec<VariationId>> = HashMap::new();
        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };
            if let Ok(tip) = branch.get().peel_to_commit() {
                map.entry(tip.id())
                    .or_default()
                    .push(VariationId::from(name));
            }
        }
        Ok(map)
    }
}

fn validate_variation_name(name: &str) -> Result<String> {
    let trimmed = name.trim();

    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.contains("..")
        || trimmed.contains('\\')
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

fn collect_preview_files(
    repo: &Repository,
    tree: &Tree<'_>,
    prefix: &Path,
    files: &mut Vec<PreviewFile>,
    content_policy: &ContentPolicy,
) -> Result<()> {
    for entry in tree.iter() {
        let Some(name) = entry.name() else {
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
        email: signature.email().map(ToString::to_string),
    }
}

fn version_from_commit(commit: &Commit<'_>) -> Version {
    Version {
        id: VersionId::from(commit.id()),
        label: commit.summary().unwrap_or_default().to_string(),
        author: contributor_from_signature(&commit.author()),
        saved_by: contributor_from_signature(&commit.committer()),
        time_seconds: commit.time().seconds(),
    }
}

fn history_entry_from_commit(
    commit: &Commit<'_>,
    tips: &HashMap<Oid, Vec<VariationId>>,
    head_oid: Option<Oid>,
) -> Result<HistoryEntry> {
    let oid = commit.id();
    let version = version_from_commit(commit);
    let mut variation_tips = tips.get(&oid).cloned().unwrap_or_default();
    variation_tips.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let is_head = head_oid == Some(oid);
    let parent_ids = (0..commit.parent_count())
        .map(|i| commit.parent_id(i).map(VersionId::from))
        .collect::<std::result::Result<Vec<_>, git2::Error>>()?;
    Ok(HistoryEntry {
        version,
        variation_tips,
        is_head,
        parent_ids,
    })
}

fn remote_summary_from_commit(commit: &Commit<'_>) -> RemoteVersionSummary {
    RemoteVersionSummary {
        id: commit.id().to_string(),
        label: commit.summary().unwrap_or_default().to_string(),
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

    let metadata = match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<OperationLockMetadata>(&bytes) {
            Ok(metadata) => Some(metadata),
            Err(error) => {
                diagnostics.push(workspace_diagnostic(
                    DiagnosticCode::WorkspaceReadFailed,
                    DiagnosticSeverity::Warning,
                    format!("operation lock metadata is unreadable: {error}"),
                ));
                None
            }
        },
        Err(error) => return Err(error.into()),
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

fn operation_lock_metadata_is_stale(metadata: &OperationLockMetadata) -> bool {
    metadata.process_id != std::process::id()
        && now_seconds().saturating_sub(metadata.created_at_seconds) >= STALE_LOCK_SECONDS
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

fn remote_tracking_oid(repo: &Repository, remote: &str, variation: &str) -> Option<String> {
    let remote_ref = format!("refs/remotes/{remote}/{variation}");
    repo.refname_to_id(&remote_ref)
        .ok()
        .map(|oid| oid.to_string())
}

fn source_variation_from_support_ref(ref_name: &str, prefix: &str) -> Option<String> {
    let remainder = ref_name.strip_prefix(prefix)?;
    let (source_variation, _operation_id) = remainder.rsplit_once('/')?;
    Some(source_variation.to_string())
}

fn is_restorable_support_ref(ref_name: &str) -> bool {
    ref_name.starts_with("refs/draftline/deleted-variations/")
        || ref_name.starts_with("refs/draftline/rewrites/squash/")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(root: &Path, relative: &str, content: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn configure_identity(workspace: &Workspace, name: &str, email: &str) {
        let mut config = workspace.repo.config().unwrap();
        config.set_str("user.name", name).unwrap();
        config.set_str("user.email", email).unwrap();
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
                original_variation: Some("master".to_string()),
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
                original_variation: Some("master".to_string()),
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

        assert_eq!(workspace.current_variation().unwrap(), "master");
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
            .switch_variation(&VariationId::from("master"), SwitchPolicy::AbortIfDirty)
            .unwrap();

        workspace.delete_variation(variation.id()).unwrap();

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
    fn publishes_and_reports_up_to_date_with_local_bare_remote() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();
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
        Repository::init_bare(remote.path()).unwrap();
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
        Repository::init_bare(remote.path()).unwrap();

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
        Repository::init_bare(remote.path()).unwrap();

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
        Repository::init_bare(remote.path()).unwrap();

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
        Repository::init_bare(remote.path()).unwrap();
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
        Repository::init_bare(remote.path()).unwrap();

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

        assert_eq!(summary.active_variation.name, "master");
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
                original_variation: Some("master".to_string()),
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
        assert!(names.contains(&"master"));
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
            Some("master")
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
                original_variation: Some("master".to_string()),
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
        assert!(!capabilities.support_ref_sync);
        assert!(!capabilities.agent_cli_facade);
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
    fn repair_and_rollback_recovery_skeletons_report_unavailable_actions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::ApplyIncoming,
                original_variation: Some("master".to_string()),
                target: Some("origin/master".to_string()),
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery("interrupted").unwrap();
        assert_eq!(repair.operation_id, "interrupted");
        assert_eq!(repair.operation, RecoveryOperation::ApplyIncoming);
        assert!(!repair.completed);
        assert!(!repair.changed_workspace);
        assert_eq!(
            repair.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );

        let rollback = workspace.rollback_recovery("interrupted").unwrap();
        assert_eq!(rollback.operation_id, "interrupted");
        assert_eq!(rollback.operation, RecoveryOperation::ApplyIncoming);
        assert!(!rollback.completed);
        assert!(!rollback.changed_workspace);
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
            .switch_variation(&VariationId::from("master"), SwitchPolicy::AbortIfDirty)
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
            .switch_variation(&VariationId::from("master"), SwitchPolicy::AbortIfDirty)
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
            .switch_variation(&VariationId::from("master"), SwitchPolicy::AbortIfDirty)
            .unwrap();
        workspace.delete_variation(variation.id()).unwrap();

        let support_refs = workspace.list_support_refs(SupportRefScope::Local).unwrap();

        assert_eq!(support_refs.len(), 1);
        assert_eq!(support_refs[0].kind, SupportRefKind::DeletedVariation);
        assert_eq!(
            support_refs[0].source_variation.as_deref(),
            Some("old-option")
        );

        let restored = workspace
            .restore_support_ref_as_variation(&support_refs[0].id, "restored-option")
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
        Repository::init_bare(remote.path()).unwrap();

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
    fn preflight_merge_incoming_reports_needs_merge_without_mutating() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();

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
    fn delete_remote_variation_publishes_support_ref_before_deleting_visible_ref() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();

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
            .switch_variation(&VariationId::from("master"), SwitchPolicy::AbortIfDirty)
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
            .any(|reference| reference == "refs/heads/master"));
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

        // HEAD (newest) should be marked as the tip of "master"
        let head_entry = &history[0];
        assert!(head_entry.is_head);
        assert!(head_entry
            .variation_tips
            .iter()
            .any(|id| id.as_str() == "master"));

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

        // Switch back and add a commit on master too.
        workspace
            .switch_variation(&VariationId::from("master"), SwitchPolicy::AbortIfDirty)
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

        // Create a variation that branches off at "Base" (2 commits on master,
        // 1 commit on the new variation).
        workspace
            .create_variation_from(workspace.versions().unwrap().last().unwrap().id(), "side")
            .unwrap();

        let summaries = workspace.variation_summaries().unwrap();
        let master = summaries
            .iter()
            .find(|s| s.variation.name == "master")
            .unwrap();
        let side = summaries
            .iter()
            .find(|s| s.variation.name == "side")
            .unwrap();

        assert_eq!(master.reachable_version_count, 2);
        assert_eq!(
            master.head_version.as_ref().map(|v| v.id()),
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
        Repository::init_bare(remote.path()).unwrap();

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
        Repository::init_bare(remote.path()).unwrap();

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
        Repository::init_bare(remote.path()).unwrap();

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
        Repository::init_bare(remote.path()).unwrap();

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

    // ── squash_versions ───────────────────────────────────────────────────────

    #[test]
    fn squash_versions_collapses_commits_and_produces_single_version() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        workspace.save_version("v3").unwrap();

        let squashed = workspace.squash_versions(2, "Squashed v2+v3").unwrap();

        assert_eq!(squashed.label, "Squashed v2+v3");
        // after squash: 2 commits — v1 (base) + squash commit
        let versions = workspace.versions().unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].label, "Squashed v2+v3");
        assert_eq!(versions[1].label, "v1");
        // workspace files must still reflect v3 content
        let content = std::fs::read_to_string(workspace.root().join("post.md")).unwrap();
        assert_eq!(content, "v3");
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn squash_versions_archives_original_tip_before_rewriting_branch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let original_tip = workspace.save_version("v3").unwrap();

        workspace.squash_versions(2, "Squashed v2+v3").unwrap();

        assert!(workspace
            .repo
            .references()
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|reference| {
                reference
                    .name()
                    .map(|name| name.starts_with("refs/draftline/rewrites/squash/master/"))
                    .unwrap_or(false)
                    && reference.target() == Some(original_tip.id().as_str().parse().unwrap())
            }));
    }

    #[test]
    fn squash_versions_rejects_count_less_than_two() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();

        let err = workspace.squash_versions(1, "Single").unwrap_err();
        assert!(matches!(err, DraftlineError::InvalidSquashCount(1)));
    }

    #[test]
    fn squash_versions_rejects_dirty_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        // leave v3 unsaved
        write_file(workspace.root(), "post.md", b"unsaved");

        let err = workspace.squash_versions(2, "Squashed").unwrap_err();
        assert!(matches!(err, DraftlineError::PreflightFailed(_)));
    }

    #[test]
    fn squash_versions_rejects_when_not_enough_commits() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");

        // Only 2 commits, trying to squash 2 requires a parent outside the range
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();

        let err = workspace
            .squash_versions(2, "Squash everything")
            .unwrap_err();
        assert!(matches!(
            err,
            DraftlineError::NotEnoughVersionsToSquash { .. }
        ));
    }
}
