use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{
    build::{CheckoutBuilder, RepoBuilder},
    BranchType, Commit, DiffFormat, DiffOptions, Direction, FetchPrune, ObjectType, Oid,
    Repository, RepositoryInitOptions, Signature, Status, StatusOptions, Tree,
};
use serde::{Deserialize, Serialize};

use crate::merge::{MergeConflict, MergeInput, ResolverRegistry};
use crate::recovery::RecoveryOperation;
use crate::remote::{ensure_supported_remote_url, PushRefExpectation};
use crate::{
    path::normalize_workspace_relative, ContentPolicy, Contributor, ContributorProfile,
    DraftlineError, PublishPreflight, PublishResult, PublishToken, RecoveryState, RemoteEndpoint,
    RemoteOptions, RemoteVersionSummary, Result, SyncState, SyncStatus,
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

/// Current live workspace content for one tracked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentFilePreview {
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

/// A graph-ready saved version node.
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
    pub is_head: bool,
    pub is_current: bool,
    pub is_tip: bool,
    pub is_merge: bool,
    pub is_branch_point: bool,
    pub child_ids: Vec<WorkspaceGraphNodeId>,
    pub child_count: usize,
    pub kind: WorkspaceGraphNodeKind,
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

/// Preflight for replacing shared remote history with the current local variation tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteHistoryReplacePreflight {
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub replacement_oid: String,
    pub support_refs: Vec<SupportRef>,
    pub token: Option<RemoteHistoryReplaceToken>,
    pub can_replace: bool,
}

/// Token tying shared history replacement to a preflighted remote and local state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteHistoryReplaceToken {
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub replacement_oid: String,
    pub support_ref_token: SupportRefPublishToken,
    pub confirmed_rewrite: bool,
}

impl RemoteHistoryReplaceToken {
    /// Records explicit host/user confirmation for replacing shared history.
    pub fn confirm_shared_history_rewrite(mut self) -> Self {
        self.confirmed_rewrite = true;
        self
    }
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

/// Preflight for squashing recent versions after archiving the current tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SquashVersionsPreflight {
    pub variation: VariationId,
    pub count: usize,
    pub head_oid: String,
    pub squash_parent_oid: String,
    pub support_ref: String,
    pub dirty_files: Vec<ChangedFile>,
    pub token: Option<SquashVersionsToken>,
    pub can_squash: bool,
}

/// Token tying local history squash to a preflighted branch tip and parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SquashVersionsToken {
    pub operation_id: String,
    pub variation: VariationId,
    pub count: usize,
    pub head_oid: String,
    pub squash_parent_oid: String,
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

/// Opaque identifier for a durable history cleanup preview plan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CleanupPlanId(String);

impl CleanupPlanId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_string(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !is_safe_operation_id(&value) {
            return Err(DraftlineError::InvalidHistoryCleanup(format!(
                "unsafe cleanup plan id `{value}`"
            )));
        }
        Ok(Self(value))
    }
}

impl std::fmt::Display for CleanupPlanId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Opaque ref name returned by cleanup APIs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RefName(String);

impl RefName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RefName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RefName {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Display for RefName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Request for previewing a timeline cleanup operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCleanupRequest {
    pub target_variation: Option<VariationId>,
    pub base: CleanupBase,
    pub mode: CleanupMode,
    pub safety: CleanupSafety,
    pub remote_policy: RemoteRewritePolicy,
}

/// Base commit selection for a cleanup rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CleanupBase {
    Auto,
    Version { version: VersionId },
}

/// Timeline cleanup mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CleanupMode {
    CompactMilestones {
        milestones: Vec<MilestoneSpec>,
        preserve_named_branches: bool,
        preserve_merge_boundaries: bool,
    },
}

/// User-facing milestone that replaces a noisy commit range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneSpec {
    pub title: String,
    pub description: Option<String>,
    pub include_range: CommitRange,
}

/// Inclusive commit range, ordered from oldest to newest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRange {
    pub start: VersionId,
    pub end: VersionId,
}

/// Request for finding viable compaction partners for one selected version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCompactionCandidatesRequest {
    pub target_variation: Option<VariationId>,
    pub selected_version: VersionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub preserve_named_branches: bool,
    pub preserve_merge_boundaries: bool,
}

/// Candidate compaction partners for one selected version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCompactionCandidates {
    pub target_variation: VariationId,
    pub selected_version: VersionId,
    pub target_head: VersionId,
    pub candidates: Vec<HistoryCompactionCandidate>,
}

/// One possible second endpoint for a compaction selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCompactionCandidate {
    pub version: Version,
    pub include_range: CommitRange,
    pub selected_role: CompactionSelectionRole,
    pub can_compact: bool,
    pub requires_descendant_replay: bool,
    pub selected_commit_count: usize,
    pub descendant_rewrite_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_impact: Option<HistoryCleanupRemoteImpact>,
    pub blockers: Vec<CleanupWarning>,
    pub warnings: Vec<CleanupWarning>,
}

/// Whether the originally selected node becomes the older or newer endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionSelectionRole {
    RangeStart,
    RangeEnd,
}

/// Safety settings for cleanup operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSafety {
    pub create_backup_ref: bool,
    pub backup_ref_name: Option<RefName>,
    pub require_clean_worktree: bool,
}

impl CleanupSafety {
    pub fn default_user_facing() -> Self {
        Self::default()
    }
}

impl Default for CleanupSafety {
    fn default() -> Self {
        Self {
            create_backup_ref: true,
            backup_ref_name: None,
            require_clean_worktree: true,
        }
    }
}

/// Remote rewrite behavior requested for cleanup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RemoteRewritePolicy {
    #[default]
    LocalOnly,
    PushWithLease {
        remote: String,
        branch: String,
    },
}

/// Confirmation marker required before applying a prepared rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteConfirmation {
    UserConfirmed,
}

/// Read-only preview of a durable cleanup plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCleanupPreview {
    pub plan_id: CleanupPlanId,
    pub target_variation: VariationId,
    pub old_head: VersionId,
    pub new_head: VersionId,
    pub preview_ref: RefName,
    pub planned_backup_ref: Option<RefName>,
    pub selected_commit_count: usize,
    pub descendant_rewrite_count: usize,
    pub affected_refs: Vec<CleanupAffectedRef>,
    pub planned_ref_updates: Vec<RefUpdate>,
    pub operations: Vec<CleanupOperation>,
    pub graph_diff: CleanupGraphDiff,
    pub commit_map: Vec<CommitRewriteMap>,
    pub snapshot_map: Vec<SnapshotRewriteMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_impact: Option<HistoryCleanupRemoteImpact>,
    pub warnings: Vec<CleanupWarning>,
}

/// Origin-aware safety classification for a cleanup range or preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCleanupRemoteImpact {
    pub remote: Option<String>,
    pub variation: VariationId,
    pub tracking_ref: Option<RefName>,
    pub upstream_head: Option<VersionId>,
    pub local_head: VersionId,
    pub replacement_head: Option<VersionId>,
    pub selected: CleanupPublicationSummary,
    pub descendants: CleanupPublicationSummary,
    pub publish_status: CleanupPublishStatus,
    pub warnings: Vec<CleanupWarning>,
}

/// Published/private counts for one cleanup commit set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPublicationSummary {
    pub published_count: usize,
    pub private_count: usize,
    pub published_versions: Vec<VersionId>,
    pub private_versions: Vec<VersionId>,
}

/// Host-facing publish guidance for a cleanup preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPublishStatus {
    NoUpstream,
    LocalOnly,
    NormalPublish,
    SharedHistoryRewriteRequired,
    RemoteHasIncoming,
}

/// One planned cleanup operation shown to product UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupOperation {
    pub title: String,
    pub description: Option<String>,
    pub old_versions: Vec<VersionId>,
    pub new_version: VersionId,
}

/// Summary of old versus new graph shape for a cleanup preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupGraphDiff {
    pub old_head: VersionId,
    pub new_head: VersionId,
    pub old_commit_count: usize,
    pub new_commit_count: usize,
    pub squashed_commit_count: usize,
}

/// Mapping from an old commit/version to its cleanup disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRewriteMap {
    pub old: VersionId,
    pub new: Option<VersionId>,
    pub disposition: RewriteDisposition,
}

/// Mapping from an old snapshot to its cleanup disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRewriteMap {
    pub old: VersionId,
    pub new: Option<VersionId>,
    pub disposition: RewriteDisposition,
}

/// Final applied commit map entry.
pub type CommitMapEntry = CommitRewriteMap;

/// Final applied snapshot map entry.
pub type SnapshotMapEntry = SnapshotRewriteMap;

/// Cleanup disposition for an old commit or snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RewriteDisposition {
    Preserved { new_id: VersionId },
    SquashedInto { new_id: VersionId },
    DroppedAsNoise,
    OrphanedButBackedUp { backup_ref: RefName },
    ConflictRequiresUserChoice,
}

/// Machine-readable cleanup warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupWarning {
    pub code: CleanupWarningCode,
    pub message: String,
    pub related_versions: Vec<VersionId>,
    pub safe_next_actions: Vec<SafeNextAction>,
}

/// A ref that cleanup will move, preserve by backup, or require user action for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupAffectedRef {
    pub name: RefName,
    pub old: Option<VersionId>,
    pub new: Option<VersionId>,
    pub impact: CleanupRefImpact,
}

/// Machine-readable cleanup ref impact for host UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupRefImpact {
    TargetVariationMoved,
    DescendantVariationRewritten,
    PointsInsideCompactedRange,
    RemoteTrackingMayNeedPublish,
}

/// Structured diagnostics for cleanup requests that cannot be made safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCleanupBlockReport {
    pub operation: String,
    pub diagnostics: Vec<CleanupWarning>,
    pub can_proceed: bool,
}

/// Stable cleanup warning code for host UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupWarningCode {
    LocalOnlyRewrite,
    RemoteRewriteRequiresSeparatePublish,
    MergeBoundaryRequiresUserChoice,
    NamedBranchWouldBeAffected,
    DirtyWorktreeBlocked,
    PreviewPlanExpired,
    TargetRefChangedSincePreview,
    CandidateRefChangedSincePreview,
    BackupRefAlreadyExists,
    RangeEndNotAncestorOfTargetHead,
    MergeBoundaryWouldBeRewritten,
    NamedBranchInsideCompactedRange,
}

/// Result of applying a prepared timeline cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineCleanupResult {
    pub plan_id: CleanupPlanId,
    pub old_head: VersionId,
    pub new_head: VersionId,
    pub backup_refs: Vec<RefName>,
    pub ref_updates: Vec<RefUpdate>,
    pub commit_map: Vec<CommitMapEntry>,
    pub snapshot_map: Vec<SnapshotMapEntry>,
    pub warnings: Vec<CleanupWarning>,
}

/// Preflight for explicitly publishing an applied cleanup rewrite to a shared remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HistoryCleanupPublishPreflight {
    pub plan_id: CleanupPlanId,
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub replacement_oid: String,
    pub remote_impact: HistoryCleanupRemoteImpact,
    pub support_refs: Vec<SupportRef>,
    pub token: Option<HistoryCleanupPublishToken>,
    pub can_publish: bool,
}

/// Token tying cleanup publish to a preflighted local cleanup ledger and remote state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HistoryCleanupPublishToken {
    pub plan_id: CleanupPlanId,
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub replacement_oid: String,
    pub support_ref_token: SupportRefPublishToken,
}

/// Result of publishing a cleanup rewrite to a shared remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HistoryCleanupPublishResult {
    pub plan_id: CleanupPlanId,
    pub remote: String,
    pub variation: VariationId,
    pub expected_remote_oid: String,
    pub replacement_oid: String,
    pub support_refs: Vec<SupportRef>,
    pub ref_updates: Vec<RefUpdate>,
}

/// One ref movement performed by cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefUpdate {
    pub name: RefName,
    pub old: Option<VersionId>,
    pub new: Option<VersionId>,
}

/// Request for resolving an old version after cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleVersionResolutionRequest {
    pub version: VersionId,
}

/// Resolution for an old version after cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleVersionResolution {
    pub requested: VersionId,
    pub disposition: StaleVersionDisposition,
}

/// Machine-readable stale version resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StaleVersionDisposition {
    Live { version: VersionId },
    SquashedInto { version: VersionId },
    BackedUp { backup_ref: RefName },
    DroppedAsNoise,
    Unknown,
}

/// Preflight for undoing an applied cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCleanupUndoPreflight {
    pub plan_id: CleanupPlanId,
    pub target_variation: VariationId,
    pub backup_ref: RefName,
    pub expected_current_head: VersionId,
    pub restore_head: VersionId,
    pub ref_updates: Vec<RefUpdate>,
    pub token: HistoryCleanupUndoToken,
    pub can_undo: bool,
}

/// Token tying cleanup undo to a preflighted target state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCleanupUndoToken {
    pub plan_id: CleanupPlanId,
    pub target_variation: VariationId,
    pub backup_ref: RefName,
    pub expected_current_head: VersionId,
    pub restore_head: VersionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HistoryCleanupStoredPlan {
    request: HistoryCleanupRequest,
    preview: HistoryCleanupPreview,
}

struct PlannedCompactCleanup {
    new_head_oid: Oid,
    operations: Vec<CleanupOperation>,
    commit_map: Vec<CommitRewriteMap>,
    snapshot_map: Vec<SnapshotRewriteMap>,
    warnings: Vec<CleanupWarning>,
    affected_refs: Vec<CleanupAffectedRef>,
    planned_ref_updates: Vec<RefUpdate>,
    old_commit_count: usize,
    new_commit_count: usize,
    selected_commit_count: usize,
    descendant_rewrite_count: usize,
}

struct CompactCleanupPlanInput<'a> {
    target_variation: &'a VariationId,
    old_head_oid: Oid,
    base: &'a CleanupBase,
    milestones: &'a [MilestoneSpec],
    preserve_named_branches: bool,
    preserve_merge_boundaries: bool,
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
    pub fn init_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;

        let repo = match Repository::open(path.as_ref()) {
            Ok(repo) => repo,
            Err(_) => {
                let initial_head = default_initial_variation_name();
                let mut options = RepositoryInitOptions::new();
                options.initial_head(&initial_head);
                Repository::init_opts(path.as_ref(), &options)?
            }
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
        ensure_supported_remote_url(remote_url.as_ref())?;
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
        let _lock = OperationLock::acquire(&self.lock_path(), "repair_recovery")?;
        let state = self.pending_recovery(operation_id.as_ref())?;

        match state.operation {
            RecoveryOperation::SwitchVariation => {
                let Some(target) = state.target.as_deref() else {
                    return Ok(self.recovery_unavailable(state));
                };
                let changed_workspace =
                    self.current_variation_unchecked().ok().as_deref() != Some(target);
                self.checkout_variation_unchecked(target)?;
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::ApplyIncoming => {
                let (Some(variation), Some(target)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                let oid = Oid::from_str(target)
                    .map_err(|_| DraftlineError::VersionNotFound(target.to_string()))?;
                let commit = self.repo.find_commit(oid)?;
                let branch_ref = format!("refs/heads/{}", validate_variation_name(variation)?);
                self.repo
                    .reference(&branch_ref, oid, true, "repair apply_incoming")?;
                self.repo.checkout_tree(
                    commit.tree()?.as_object(),
                    Some(CheckoutBuilder::new().force()),
                )?;
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
                if state.original_variation.is_none() || state.target.is_none() {
                    return Ok(self.recovery_unavailable(state));
                }
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
                self.repo.checkout_tree(
                    commit.tree()?.as_object(),
                    Some(CheckoutBuilder::new().force()),
                )?;
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
            RecoveryOperation::SquashVersions | RecoveryOperation::HistoryCleanup => {
                Ok(self.recovery_unavailable(state))
            }
            RecoveryOperation::DeleteRemoteVariation => {
                self.repair_delete_remote_variation_recovery(state, options)
            }
            RecoveryOperation::RestoreVersionAsNewSave | RecoveryOperation::MergeIncoming => {
                Ok(self.recovery_unavailable(state))
            }
        }
    }

    /// Rolls back an interrupted operation when the recovery ledger captured the
    /// prior state needed for a safe rollback.
    pub fn rollback_recovery(&self, operation_id: impl AsRef<str>) -> Result<RecoveryRepairResult> {
        self.clear_stale_lock()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "rollback_recovery")?;
        let state = self.pending_recovery(operation_id.as_ref())?;

        match state.operation {
            RecoveryOperation::SwitchVariation => {
                let Some(original) = state.original_variation.as_deref() else {
                    return Ok(self.recovery_unavailable(state));
                };
                let changed_workspace =
                    self.current_variation_unchecked().ok().as_deref() != Some(original);
                self.checkout_variation_unchecked(original)?;
                self.complete_recovery(state, changed_workspace)
            }
            RecoveryOperation::DeleteVariation => {
                let (Some(variation), Some(target)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                let oid = Oid::from_str(target)
                    .map_err(|_| DraftlineError::VersionNotFound(target.to_string()))?;
                let branch_ref = format!("refs/heads/{}", validate_variation_name(variation)?);
                self.repo
                    .reference(&branch_ref, oid, true, "rollback recovery")?;
                if self.current_variation_unchecked().ok().as_deref() == Some(variation) {
                    let commit = self.repo.find_commit(oid)?;
                    self.repo.checkout_tree(
                        commit.tree()?.as_object(),
                        Some(CheckoutBuilder::new().force()),
                    )?;
                    self.repo.set_head(&branch_ref)?;
                }
                let archive_ref = archive_ref("deleted-variations", variation, &state.operation_id);
                if let Ok(mut reference) = self.repo.find_reference(&archive_ref) {
                    reference.delete()?;
                }
                self.complete_recovery(state, true)
            }
            RecoveryOperation::SquashVersions | RecoveryOperation::HistoryCleanup => {
                let (Some(variation), Some(target)) =
                    (state.original_variation.as_deref(), state.target.as_deref())
                else {
                    return Ok(self.recovery_unavailable(state));
                };
                let oid = Oid::from_str(target)
                    .map_err(|_| DraftlineError::VersionNotFound(target.to_string()))?;
                let branch_ref = format!("refs/heads/{}", validate_variation_name(variation)?);
                self.repo
                    .reference(&branch_ref, oid, true, "rollback recovery")?;
                if self.current_variation_unchecked().ok().as_deref() == Some(variation) {
                    let commit = self.repo.find_commit(oid)?;
                    self.repo.checkout_tree(
                        commit.tree()?.as_object(),
                        Some(CheckoutBuilder::new().force()),
                    )?;
                    self.repo.set_head(&branch_ref)?;
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
            RecoveryOperation::ShelveChanges
            | RecoveryOperation::ApplyIncoming
            | RecoveryOperation::RestoreVersionAsNewSave
            | RecoveryOperation::DiscardChanges
            | RecoveryOperation::DiscardFile
            | RecoveryOperation::DeleteRemoteVariation
            | RecoveryOperation::ExpireSupportRefs
            | RecoveryOperation::MergeIncoming => Ok(self.recovery_unavailable(state)),
        }
    }

    /// Acknowledges an incomplete recovery record and allows normal operations again.
    pub fn acknowledge_recovery(&self) -> Result<()> {
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
        self.ensure_no_pending_recovery()?;
        self.save_version_unchecked(label, None)
    }

    /// Saves the current workspace content using host-supplied attribution.
    pub fn save_version_with_profile(
        &self,
        label: impl AsRef<str>,
        profile: &ContributorProfile,
    ) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        self.save_version_unchecked(label, Some(profile))
    }

    fn save_version_unchecked(
        &self,
        label: impl AsRef<str>,
        profile: Option<&ContributorProfile>,
    ) -> Result<Version> {
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
        let (author, committer) = self.workspace_signatures(profile)?;
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| self.repo.find_commit(oid).ok());

        let oid = match parent.as_ref() {
            Some(parent) => self.repo.commit(
                Some("HEAD"),
                &author,
                &committer,
                label.as_ref(),
                &tree,
                &[parent],
            )?,
            None => self.repo.commit(
                Some("HEAD"),
                &author,
                &committer,
                label.as_ref(),
                &tree,
                &[],
            )?,
        };

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    /// Preflights saving only selected workspace-relative tracked content files.
    pub fn preflight_save_files<I, P>(&self, paths: I) -> Result<PreflightReport>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
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
        self.ensure_no_pending_recovery()?;
        let changed_files = self.selected_changed_files(paths)?;
        if changed_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(
                selected_files_preflight_report("save_files", false, changed_files),
            )));
        }

        let tree_id = self.write_selected_changes_tree(&changed_files)?;
        let tree = self.repo.find_tree(tree_id)?;
        let (author, committer) = self.workspace_signatures(profile)?;
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| self.repo.find_commit(oid).ok());

        let oid = match parent.as_ref() {
            Some(parent) => self.repo.commit(
                Some("HEAD"),
                &author,
                &committer,
                label.as_ref(),
                &tree,
                &[parent],
            )?,
            None => self.repo.commit(
                Some("HEAD"),
                &author,
                &committer,
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

    /// Preflights discarding selected workspace-relative tracked content files.
    pub fn preflight_discard_files<I, P>(&self, paths: I) -> Result<PreflightReport>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.ensure_no_pending_recovery()?;
        Ok(discard_preflight_report(
            "discard_files",
            self.selected_changed_files(paths)?,
        ))
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

        let _lock = OperationLock::acquire(&self.lock_path(), "discard_files")?;
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
        let _lock = OperationLock::acquire(&self.lock_path(), "create_variation_from_version")?;
        if !is_safe_operation_id(&token.operation_id) {
            return Err(DraftlineError::LocalStateChanged {
                expected: "create variation operation id issued by preflight".to_string(),
                actual: token.operation_id,
            });
        }

        let name = validate_variation_name(token.variation.as_str())?;
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
        if options.has_credentials() {
            let mut fetch_options = options.fetch_options();
            fetch_options.prune(FetchPrune::On);
            remote.fetch(&refspec_refs, Some(&mut fetch_options), None)?;
        } else {
            let mut fetch_options = git2::FetchOptions::new();
            fetch_options.prune(FetchPrune::On);
            remote.fetch(&refspec_refs, Some(&mut fetch_options), None)?;
        }
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

        let _lock = OperationLock::acquire(&self.lock_path(), "shelve_files")?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ShelveChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(safe_name.clone()),
            completed: false,
        })?;

        let tree_id = self.write_selected_changes_tree(&changed_files)?;
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
        ];
        let refspec_refs = refspecs.iter().map(String::as_str).collect::<Vec<_>>();
        if options.has_credentials() {
            let mut fetch_options = options.fetch_options();
            git_remote.fetch(&refspec_refs, Some(&mut fetch_options), None)?;
        } else {
            git_remote.fetch(&refspec_refs, None, None)?;
        }
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

    /// Plans replacing shared remote history with the current local variation tip.
    pub fn preflight_replace_remote_history(
        &self,
        remote: impl AsRef<str>,
    ) -> Result<RemoteHistoryReplacePreflight> {
        let mut options = RemoteOptions::new();
        self.preflight_replace_remote_history_with_options(remote, &mut options)
    }

    /// Plans shared history replacement with explicit remote options.
    pub fn preflight_replace_remote_history_with_options(
        &self,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<RemoteHistoryReplacePreflight> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        self.repo.find_remote(&remote)?;
        let variation = self.current_variation_unchecked()?;
        let report = preflight_report(
            "replace_remote_history",
            false,
            self.changed_files_unchecked()?,
            Vec::new(),
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        self.fetch_remote_variation_ref(&remote, &variation, options)?;
        let Some(expected_remote_oid) = remote_tracking_oid(&self.repo, &remote, &variation) else {
            return Err(DraftlineError::VersionNotFound(variation));
        };
        let replacement_oid = self.repo.head()?.peel_to_commit()?.id().to_string();
        let support_ref_preflight =
            self.preflight_publish_support_refs_with_options(&remote, options)?;
        let support_refs = self
            .list_local_support_refs()?
            .into_iter()
            .filter(|support_ref| {
                support_ref.kind == SupportRefKind::Rewrite
                    && support_ref.source_variation.as_deref() == Some(variation.as_str())
                    && support_ref.target_oid == expected_remote_oid
            })
            .collect::<Vec<_>>();
        let can_replace = !support_refs.is_empty();
        let token = can_replace.then(|| RemoteHistoryReplaceToken {
            remote: remote.clone(),
            variation: VariationId::from(variation.clone()),
            expected_remote_oid: expected_remote_oid.clone(),
            replacement_oid: replacement_oid.clone(),
            support_ref_token: support_ref_preflight.token.clone(),
            confirmed_rewrite: false,
        });

        Ok(RemoteHistoryReplacePreflight {
            remote,
            variation: VariationId::from(variation),
            expected_remote_oid,
            replacement_oid,
            support_refs,
            token,
            can_replace,
        })
    }

    /// Replaces shared remote history after publishing recovery support refs.
    pub fn replace_remote_history(&self, token: RemoteHistoryReplaceToken) -> Result<()> {
        let mut options = RemoteOptions::new();
        self.replace_remote_history_with_options(token, &mut options)
    }

    /// Replaces shared remote history with explicit remote options.
    pub fn replace_remote_history_with_options(
        &self,
        token: RemoteHistoryReplaceToken,
        options: &mut RemoteOptions<'_>,
    ) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        if !token.confirmed_rewrite {
            return Err(DraftlineError::ConsentRequired(
                "replace_remote_history".to_string(),
            ));
        }
        let variation = self.current_variation_unchecked()?;
        let local_oid = self.repo.head()?.peel_to_commit()?.id().to_string();
        if variation != token.variation.as_str() || local_oid != token.replacement_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", token.variation.as_str(), token.replacement_oid),
                actual: format!("{variation}@{local_oid}"),
            });
        }

        self.publish_support_refs_with_options(token.support_ref_token, options)?;
        self.fetch_remote_variation_ref(&token.remote, token.variation.as_str(), options)?;
        let actual_remote_oid =
            remote_tracking_oid(&self.repo, &token.remote, token.variation.as_str());
        if actual_remote_oid.as_deref() != Some(token.expected_remote_oid.as_str()) {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: token.variation.as_str().to_string(),
                expected: Some(token.expected_remote_oid),
                actual: actual_remote_oid,
            });
        }

        self.push_refspec(
            &token.remote,
            &format!(
                "+refs/heads/{}:refs/heads/{}",
                token.variation.as_str(),
                token.variation.as_str()
            ),
            vec![PushRefExpectation {
                dst_refname: format!("refs/heads/{}", token.variation.as_str()),
                expected_old_oid: Some(token.expected_remote_oid),
                expected_new_oid: Some(token.replacement_oid),
            }],
            options,
        )
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
                self.save_version_unchecked(label, None)?;
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

    /// Plans deletion of an alternate variation, including the archive ref.
    pub fn preflight_delete_variation(
        &self,
        variation: &VariationId,
    ) -> Result<VariationDeletePreflight> {
        self.ensure_no_pending_recovery()?;
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
        let _lock = OperationLock::acquire(&self.lock_path(), "delete_variation")?;
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
        let _lock = OperationLock::acquire(&self.lock_path(), "rename_variation")?;
        let source_name = validate_variation_name(token.source_variation.as_str())?;
        let target_name = validate_variation_name(token.target_variation.as_str())?;
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
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(
            &self.lock_path(),
            "restore_version_as_new_save_to_variation",
        )?;

        let source_commit = self.find_version_commit(version)?;
        let source_tree = source_commit.tree()?;
        let current_variation = self.current_variation_unchecked()?;
        let (target_name, target_metadata, parent_oid) =
            self.resolve_restore_version_target(target, &current_variation)?;

        let report = preflight_report(
            "restore_version_as_new_save_to_variation",
            true,
            self.changed_files_unchecked()?,
            self.target_tree_hazards(&source_tree)?,
            Some(format!("{current_variation} -> {target_name}")),
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: Some(current_variation),
            target: Some(format!("{} -> {}", version.as_str(), target_name)),
            completed: false,
        })?;

        let parent = self.repo.find_commit(parent_oid)?;
        let signature = self.workspace_signature()?;
        let target_ref = format!("refs/heads/{target_name}");
        let oid = self.repo.commit(
            Some(&target_ref),
            &signature,
            &signature,
            label.as_ref(),
            &source_tree,
            &[&parent],
        )?;
        self.write_variation_metadata(&target_name, &target_metadata)?;

        self.repo.checkout_tree(
            source_tree.as_object(),
            Some(CheckoutBuilder::new().force()),
        )?;
        self.repo.set_head(&target_ref)?;

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

    /// Returns viable second endpoints for compacting history around one version.
    pub fn history_compaction_candidates(
        &self,
        request: HistoryCompactionCandidatesRequest,
    ) -> Result<HistoryCompactionCandidates> {
        self.ensure_no_pending_recovery()?;
        let target_variation = match request.target_variation {
            Some(variation) => VariationId::from(validate_variation_name(variation.as_str())?),
            None => VariationId::from(self.current_variation_unchecked()?),
        };
        let target_branch = self.find_local_variation_branch(target_variation.as_str())?;
        let target_head_oid = target_branch.get().peel_to_commit()?.id();
        let selected_oid = oid_from_version(&request.selected_version)?;
        let chain = self.first_parent_chain_to_root(target_head_oid)?;
        let selected_index = chain
            .iter()
            .position(|oid| *oid == selected_oid)
            .ok_or_else(|| DraftlineError::VersionNotLocallyReachable(selected_oid.to_string()))?;
        let local_tips = self.local_variation_tip_oids()?;
        let mut candidates = Vec::new();
        let mut remote_options = RemoteOptions::new();
        if let Some(remote) = request.remote.as_deref() {
            self.fetch_remote_variation_ref(
                remote,
                target_variation.as_str(),
                &mut remote_options,
            )?;
        }

        for (candidate_index, candidate_oid) in chain.iter().copied().enumerate() {
            if candidate_oid == selected_oid {
                continue;
            }

            let (start_index, end_index, selected_role) = if candidate_index > selected_index {
                (
                    candidate_index,
                    selected_index,
                    CompactionSelectionRole::RangeEnd,
                )
            } else {
                (
                    selected_index,
                    candidate_index,
                    CompactionSelectionRole::RangeStart,
                )
            };
            let start_oid = chain[start_index];
            let end_oid = chain[end_index];
            let range_oids = chain[end_index..=start_index]
                .iter()
                .rev()
                .copied()
                .collect::<Vec<_>>();
            let range_set = range_oids.iter().copied().collect::<BTreeSet<_>>();
            let descendant_rewrite_count = end_index;
            let descendant_set = chain[..end_index].iter().copied().collect::<BTreeSet<_>>();
            let mut blockers = Vec::new();
            let mut warnings = Vec::new();

            if request.preserve_merge_boundaries {
                for oid in &range_oids {
                    let commit = self.repo.find_commit(*oid)?;
                    if commit.parent_count() > 1 {
                        blockers.push(cleanup_warning(
                            CleanupWarningCode::MergeBoundaryRequiresUserChoice,
                            format!("cleanup range crosses merge commit `{oid}`"),
                            vec![VersionId::from(*oid)],
                        ));
                    }
                }
            }
            for oid in chain[..end_index].iter().copied() {
                let commit = self.repo.find_commit(oid)?;
                for parent_index in 1..commit.parent_count() {
                    let parent = commit.parent(parent_index)?;
                    let parent_oid = parent.id();
                    if range_set.contains(&parent_oid) {
                        blockers.push(cleanup_warning(
                            CleanupWarningCode::MergeBoundaryWouldBeRewritten,
                            format!(
                                "descendant merge commit `{oid}` has a secondary parent inside the compacted range"
                            ),
                            vec![VersionId::from(oid), VersionId::from(parent_oid)],
                        ));
                    }
                }
            }

            for (variation, tip) in &local_tips {
                if *variation == target_variation {
                    continue;
                }
                if range_set.contains(tip) {
                    let warning = cleanup_warning(
                        if request.preserve_named_branches {
                            CleanupWarningCode::NamedBranchInsideCompactedRange
                        } else {
                            CleanupWarningCode::NamedBranchWouldBeAffected
                        },
                        format!(
                            "variation `{}` points inside the compacted range",
                            variation.as_str()
                        ),
                        vec![VersionId::from(*tip)],
                    );
                    if request.preserve_named_branches {
                        blockers.push(warning);
                    } else {
                        warnings.push(warning);
                    }
                } else if descendant_set.contains(tip) {
                    warnings.push(cleanup_warning(
                        CleanupWarningCode::NamedBranchWouldBeAffected,
                        format!(
                            "variation `{}` points to a descendant that cleanup would rewrite",
                            variation.as_str()
                        ),
                        vec![VersionId::from(*tip)],
                    ));
                }
            }

            let candidate_commit = self.repo.find_commit(candidate_oid)?;
            let remote_impact = self.cleanup_remote_impact_for_oids(
                request.remote.as_deref(),
                &target_variation,
                target_head_oid,
                None,
                &range_oids,
                &chain[..end_index],
            )?;
            candidates.push(HistoryCompactionCandidate {
                version: version_from_commit(&candidate_commit),
                include_range: CommitRange {
                    start: VersionId::from(start_oid),
                    end: VersionId::from(end_oid),
                },
                selected_role,
                can_compact: blockers.is_empty(),
                requires_descendant_replay: descendant_rewrite_count > 0,
                selected_commit_count: range_oids.len(),
                descendant_rewrite_count,
                remote_impact,
                blockers,
                warnings,
            });
        }

        Ok(HistoryCompactionCandidates {
            target_variation: target_variation.clone(),
            selected_version: request.selected_version,
            target_head: VersionId::from(target_head_oid),
            candidates,
        })
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
            self.local_variation_tip_oids().unwrap_or_default()
        } else {
            self.local_variation_tip_oids()?
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
            for oid in push_oids {
                walk.push(oid)?;
            }
            for oid in walk {
                let oid = oid?;
                if total >= start && total < end {
                    let commit = self.repo.find_commit(oid)?;
                    nodes.push(workspace_graph_node_from_commit(
                        &commit,
                        &tips,
                        head_oid,
                        &local_reachable,
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
            self.local_variation_tip_oids().unwrap_or_default()
        } else {
            self.local_variation_tip_oids()?
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
        let ancestor = self
            .workspace_graph_common_ancestor(from, to)?
            .common_ancestor;
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
        let common_ancestor = match self.repo.merge_base(left_oid, right_oid) {
            Ok(oid) => Some(VersionId::from(oid)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => None,
            Err(error) => return Err(error.into()),
        };
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
        let changed_files = if let Some(parent) = node.parent_version_ids.first() {
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
        let mut file_hazards = Vec::new();
        let mut conflicts = Vec::new();
        let mut token = None;

        if matches!(sync_status.state, SyncState::NeedsMerge) {
            let merge_input = self.merge_input_for_status(&sync_status)?;
            file_hazards = self.target_tree_hazards(&merge_input.remote_commit.tree()?)?;
            let plan = self.plan_clean_merge(
                &merge_input.base_commit.tree()?,
                &merge_input.local_commit.tree()?,
                &merge_input.remote_commit.tree()?,
            )?;
            conflicts = plan.conflicts;
            if dirty_files.is_empty() && file_hazards.is_empty() {
                token = Some(MergeIncomingToken {
                    remote: sync_status.remote.clone(),
                    variation: sync_status.variation.clone(),
                    local_oid: merge_input.local_commit.id().to_string(),
                    remote_oid: merge_input.remote_commit.id().to_string(),
                    merge_base_oid: merge_input.base_commit.id().to_string(),
                });
            }
        }
        let can_merge_cleanly = token.is_some() && conflicts.is_empty();

        Ok(MergeIncomingReport {
            sync_status,
            dirty_files,
            file_hazards,
            conflicts,
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
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "merge_incoming")?;
        let dirty_files = self.changed_files_unchecked()?;
        if !dirty_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "merge_incoming",
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

        self.fetch_remote_unchecked(&token.remote, options)?;
        let status = self.sync_status(&token.remote)?;
        if status.state != SyncState::NeedsMerge {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }
        let remote_oid = remote_tracking_oid(&self.repo, &token.remote, &token.variation);
        if remote_oid.as_deref() != Some(token.remote_oid.as_str()) {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: token.variation,
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

        let remote_tree = merge_input.remote_commit.tree()?;
        let file_hazards = self.target_tree_hazards(&remote_tree)?;
        if !file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "merge_incoming",
                true,
                Vec::new(),
                file_hazards,
                None,
            ))));
        }

        let plan = self.plan_clean_merge(
            &merge_input.base_commit.tree()?,
            &merge_input.local_commit.tree()?,
            &remote_tree,
        )?;
        if !plan.conflicts.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "merge_incoming",
                true,
                Vec::new(),
                Vec::new(),
                Some(plan.conflicts[0].label.clone()),
            ))));
        }

        self.write_incoming_merge_version(
            variation,
            token.remote_oid,
            &merge_input,
            label.as_ref(),
            plan.files,
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
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "merge_incoming_with_resolutions")?;
        let dirty_files = self.changed_files_unchecked()?;
        if !dirty_files.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "merge_incoming_with_resolutions",
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

        self.fetch_remote_unchecked(&token.remote, options)?;
        let status = self.sync_status(&token.remote)?;
        if status.state != SyncState::NeedsMerge {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }
        let remote_oid = remote_tracking_oid(&self.repo, &token.remote, &token.variation);
        if remote_oid.as_deref() != Some(token.remote_oid.as_str()) {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: token.variation,
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

        let remote_tree = merge_input.remote_commit.tree()?;
        let file_hazards = self.target_tree_hazards(&remote_tree)?;
        if !file_hazards.is_empty() {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "merge_incoming_with_resolutions",
                true,
                Vec::new(),
                file_hazards,
                None,
            ))));
        }

        let plan = self.plan_clean_merge(
            &merge_input.base_commit.tree()?,
            &merge_input.local_commit.tree()?,
            &remote_tree,
        )?;
        let files = self.resolve_merge_plan(plan, resolutions)?;

        self.write_incoming_merge_version(
            variation,
            token.remote_oid,
            &merge_input,
            label.as_ref(),
            files,
            profile,
        )
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
    pub fn preflight_squash_versions(&self, count: usize) -> Result<SquashVersionsPreflight> {
        self.ensure_no_pending_recovery()?;

        if count < 2 {
            return Err(DraftlineError::InvalidSquashCount(count));
        }

        let dirty_files = self.changed_files_unchecked()?;
        if !dirty_files.is_empty() {
            let variation = VariationId::from(self.current_variation_unchecked()?);
            return Ok(SquashVersionsPreflight {
                variation,
                count,
                head_oid: String::new(),
                squash_parent_oid: String::new(),
                support_ref: String::new(),
                dirty_files,
                token: None,
                can_squash: false,
            });
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

        let variation = VariationId::from(self.current_variation_unchecked()?);
        let operation_id = new_operation_id();
        let support_ref = archive_ref("rewrites/squash", variation.as_str(), &operation_id);
        let token = SquashVersionsToken {
            operation_id,
            variation: variation.clone(),
            count,
            head_oid: head_commit.id().to_string(),
            squash_parent_oid: squash_parent.id().to_string(),
            support_ref: support_ref.clone(),
        };

        Ok(SquashVersionsPreflight {
            variation,
            count,
            head_oid: head_commit.id().to_string(),
            squash_parent_oid: squash_parent.id().to_string(),
            support_ref,
            dirty_files: Vec::new(),
            token: Some(token),
            can_squash: true,
        })
    }

    /// Squashes the last `count` preflighted versions into a single new version.
    pub fn squash_versions_with_token(
        &self,
        token: SquashVersionsToken,
        label: impl AsRef<str>,
    ) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "squash_versions")?;
        let variation = self.current_variation_unchecked()?;
        if variation != token.variation.as_str() {
            return Err(DraftlineError::LocalStateChanged {
                expected: token.variation.as_str().to_string(),
                actual: variation,
            });
        }

        let head_commit = self.repo.head()?.peel_to_commit()?;
        if head_commit.id().to_string() != token.head_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", token.variation.as_str(), token.head_oid),
                actual: format!("{}@{}", token.variation.as_str(), head_commit.id()),
            });
        }
        let squash_parent_oid = Oid::from_str(&token.squash_parent_oid)?;
        let squash_parent = self.repo.find_commit(squash_parent_oid)?;
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
        let branch_ref = format!("refs/heads/{variation}");
        self.write_recovery_state(&RecoveryState {
            operation_id: token.operation_id.clone(),
            operation: RecoveryOperation::SquashVersions,
            original_variation: Some(variation.clone()),
            target: Some(token.head_oid.clone()),
            completed: false,
        })?;
        self.repo.reference(
            &token.support_ref,
            head_commit.id(),
            false,
            "archive pre-squash tip",
        )?;
        self.repo
            .reference(&branch_ref, oid, true, "squash_versions")?;
        self.write_recovery_state(&RecoveryState {
            operation_id: token.operation_id,
            operation: RecoveryOperation::SquashVersions,
            original_variation: None,
            target: Some(oid.to_string()),
            completed: true,
        })?;

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    pub fn squash_versions(&self, count: usize, label: impl AsRef<str>) -> Result<Version> {
        let preflight = self.preflight_squash_versions(count)?;
        let Some(token) = preflight.token else {
            return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                "squash_versions",
                false,
                preflight.dirty_files,
                Vec::new(),
                None,
            ))));
        };
        self.squash_versions_with_token(token, label)
    }

    /// Prepares a durable local timeline cleanup plan without moving visible refs.
    pub fn preview_history_cleanup(
        &self,
        request: HistoryCleanupRequest,
    ) -> Result<HistoryCleanupPreview> {
        self.ensure_no_pending_recovery()?;
        let target_variation = match request.target_variation.clone() {
            Some(variation) => VariationId::from(validate_variation_name(variation.as_str())?),
            None => VariationId::from(self.current_variation_unchecked()?),
        };
        let target_name = target_variation.as_str();
        let target_branch = self.find_local_variation_branch(target_name)?;
        let old_head_oid = target_branch.get().peel_to_commit()?.id();

        if request.safety.require_clean_worktree {
            let dirty_files = self.changed_files_unchecked()?;
            if !dirty_files.is_empty() {
                return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                    "history_cleanup",
                    false,
                    dirty_files,
                    Vec::new(),
                    None,
                ))));
            }
        }

        let plan_id = CleanupPlanId::from_string(new_operation_id())?;
        let planned_backup_ref = if request.safety.create_backup_ref {
            Some(request.safety.backup_ref_name.clone().unwrap_or_else(|| {
                RefName::from(archive_ref(
                    "backups/history-cleanup",
                    target_name,
                    plan_id.as_str(),
                ))
            }))
        } else {
            None
        };

        let (milestones, preserve_named_branches, preserve_merge_boundaries) = match &request.mode {
            CleanupMode::CompactMilestones {
                milestones,
                preserve_named_branches,
                preserve_merge_boundaries,
            } => (
                milestones.as_slice(),
                *preserve_named_branches,
                *preserve_merge_boundaries,
            ),
        };

        let mut warnings = cleanup_remote_warnings(&request.remote_policy);
        let planned = self.plan_compact_milestones(CompactCleanupPlanInput {
            target_variation: &target_variation,
            old_head_oid,
            base: &request.base,
            milestones,
            preserve_named_branches,
            preserve_merge_boundaries,
        })?;
        let remote_impact_remote =
            if let RemoteRewritePolicy::PushWithLease { remote, branch } = &request.remote_policy {
                let mut options = RemoteOptions::new();
                self.fetch_remote_variation_ref(remote, branch, &mut options)?;
                Some(remote.as_str())
            } else {
                None
            };
        warnings.extend(planned.warnings);

        let preview_ref = RefName::from(format!(
            "refs/draftline/previews/history-cleanup/{target_name}/{}",
            plan_id.as_str()
        ));
        self.repo.reference(
            preview_ref.as_str(),
            planned.new_head_oid,
            false,
            "prepare history cleanup preview",
        )?;

        let preview = HistoryCleanupPreview {
            plan_id,
            target_variation: target_variation.clone(),
            old_head: VersionId::from(old_head_oid),
            new_head: VersionId::from(planned.new_head_oid),
            preview_ref,
            planned_backup_ref,
            selected_commit_count: planned.selected_commit_count,
            descendant_rewrite_count: planned.descendant_rewrite_count,
            affected_refs: planned.affected_refs,
            planned_ref_updates: planned.planned_ref_updates,
            operations: planned.operations,
            graph_diff: CleanupGraphDiff {
                old_head: VersionId::from(old_head_oid),
                new_head: VersionId::from(planned.new_head_oid),
                old_commit_count: planned.old_commit_count,
                new_commit_count: planned.new_commit_count,
                squashed_commit_count: planned
                    .old_commit_count
                    .saturating_sub(planned.new_commit_count),
            },
            commit_map: planned.commit_map.clone(),
            snapshot_map: planned.snapshot_map.clone(),
            remote_impact: self.cleanup_remote_impact_for_oids(
                remote_impact_remote,
                &target_variation,
                old_head_oid,
                Some(planned.new_head_oid),
                &planned
                    .commit_map
                    .iter()
                    .filter_map(|entry| {
                        matches!(entry.disposition, RewriteDisposition::SquashedInto { .. })
                            .then(|| oid_from_version(&entry.old).ok())
                            .flatten()
                    })
                    .collect::<Vec<_>>(),
                &planned
                    .commit_map
                    .iter()
                    .filter_map(|entry| {
                        matches!(entry.disposition, RewriteDisposition::Preserved { .. })
                            .then(|| oid_from_version(&entry.old).ok())
                            .flatten()
                    })
                    .collect::<Vec<_>>(),
            )?,
            warnings,
        };

        self.write_history_cleanup_plan(&HistoryCleanupStoredPlan {
            request,
            preview: preview.clone(),
        })?;

        Ok(preview)
    }

    /// Applies a durable cleanup preview after confirming the target state is unchanged.
    pub fn apply_history_cleanup(
        &self,
        plan_id: CleanupPlanId,
        _confirmation: RewriteConfirmation,
    ) -> Result<TimelineCleanupResult> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "history_cleanup")?;
        let stored = self.read_history_cleanup_plan(&plan_id)?;
        if stored.request.safety.require_clean_worktree {
            let dirty_files = self.changed_files_unchecked()?;
            if !dirty_files.is_empty() {
                return Err(DraftlineError::PreflightFailed(Box::new(preflight_report(
                    "history_cleanup",
                    false,
                    dirty_files,
                    Vec::new(),
                    None,
                ))));
            }
        }

        let variation = stored.preview.target_variation.as_str();
        let branch_ref = format!("refs/heads/{variation}");
        let old_head_oid = oid_from_version(&stored.preview.old_head)?;
        let new_head_oid = oid_from_version(&stored.preview.new_head)?;
        for update in &stored.preview.planned_ref_updates {
            let Some(expected_old) = update.old.as_ref() else {
                continue;
            };
            let expected_old_oid = oid_from_version(expected_old)?;
            let actual = self.repo.refname_to_id(update.name.as_str())?;
            if actual != expected_old_oid {
                return Err(DraftlineError::LocalStateChanged {
                    expected: format!("{}@{expected_old_oid}", update.name.as_str()),
                    actual: format!("{}@{actual}", update.name.as_str()),
                });
            }
        }

        let actual_preview = self
            .repo
            .refname_to_id(stored.preview.preview_ref.as_str())?;
        if actual_preview != new_head_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{new_head_oid}", stored.preview.preview_ref.as_str()),
                actual: format!("{}@{actual_preview}", stored.preview.preview_ref.as_str()),
            });
        }

        let mut backup_refs = Vec::new();
        if let Some(backup_ref) = stored.preview.planned_backup_ref.as_ref() {
            self.ensure_archive_ref(
                backup_ref.as_str(),
                old_head_oid,
                "backup before history cleanup",
            )?;
            backup_refs.push(backup_ref.clone());
        }

        self.write_recovery_state(&RecoveryState {
            operation_id: plan_id.as_str().to_string(),
            operation: RecoveryOperation::HistoryCleanup,
            original_variation: Some(variation.to_string()),
            target: Some(stored.preview.old_head.to_string()),
            completed: false,
        })?;

        for update in &stored.preview.planned_ref_updates {
            let Some(new) = update.new.as_ref() else {
                continue;
            };
            self.repo.reference(
                update.name.as_str(),
                oid_from_version(new)?,
                true,
                "history_cleanup",
            )?;
        }
        if self.head_symbolic_variation().as_deref() == Some(variation) {
            self.repo.set_head(&branch_ref)?;
            self.repo
                .checkout_head(Some(CheckoutBuilder::new().force()))?;
        }

        let result = TimelineCleanupResult {
            plan_id: plan_id.clone(),
            old_head: stored.preview.old_head.clone(),
            new_head: stored.preview.new_head.clone(),
            backup_refs,
            ref_updates: stored.preview.planned_ref_updates.clone(),
            commit_map: stored.preview.commit_map.clone(),
            snapshot_map: stored.preview.snapshot_map.clone(),
            warnings: stored.preview.warnings.clone(),
        };
        self.write_history_cleanup_ledger(&result)?;
        self.write_recovery_state(&RecoveryState {
            operation_id: plan_id.as_str().to_string(),
            operation: RecoveryOperation::HistoryCleanup,
            original_variation: None,
            target: Some(stored.preview.new_head.to_string()),
            completed: true,
        })?;
        Ok(result)
    }

    /// Reports origin-aware publish impact for a prepared cleanup plan.
    pub fn preflight_history_cleanup_remote_impact(
        &self,
        plan_id: CleanupPlanId,
        remote: impl AsRef<str>,
    ) -> Result<HistoryCleanupRemoteImpact> {
        self.ensure_no_pending_recovery()?;
        let stored = self.read_history_cleanup_plan(&plan_id)?;
        let remote = remote.as_ref().to_string();
        let mut options = RemoteOptions::new();
        self.fetch_remote_variation_ref(
            &remote,
            stored.preview.target_variation.as_str(),
            &mut options,
        )?;
        self.cleanup_remote_impact_from_preview(&stored.preview, Some(remote.as_str()))
    }

    /// Preflights explicitly publishing an applied cleanup rewrite to a shared remote.
    pub fn preflight_publish_history_cleanup(
        &self,
        plan_id: CleanupPlanId,
        remote: impl AsRef<str>,
    ) -> Result<HistoryCleanupPublishPreflight> {
        let mut options = RemoteOptions::new();
        self.preflight_publish_history_cleanup_with_options(plan_id, remote, &mut options)
    }

    /// Preflights cleanup publish with explicit remote options.
    pub fn preflight_publish_history_cleanup_with_options(
        &self,
        plan_id: CleanupPlanId,
        remote: impl AsRef<str>,
        options: &mut RemoteOptions<'_>,
    ) -> Result<HistoryCleanupPublishPreflight> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        self.repo.find_remote(&remote)?;
        let stored = self.read_history_cleanup_plan(&plan_id)?;
        let ledger = self.read_history_cleanup_ledger(&plan_id)?;
        let variation = stored.preview.target_variation.clone();
        let branch_ref = format!("refs/heads/{}", variation.as_str());
        let actual_local_oid = self.repo.refname_to_id(&branch_ref)?.to_string();
        if actual_local_oid != ledger.new_head.as_str() {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{branch_ref}@{}", ledger.new_head),
                actual: format!("{branch_ref}@{actual_local_oid}"),
            });
        }

        self.fetch_remote_variation_ref(&remote, variation.as_str(), options)?;
        let expected_remote_oid = remote_tracking_oid(&self.repo, &remote, variation.as_str())
            .ok_or_else(|| DraftlineError::VersionNotFound(variation.as_str().to_string()))?;
        let remote_impact =
            self.cleanup_remote_impact_from_preview(&stored.preview, Some(&remote))?;
        if remote_impact.publish_status == CleanupPublishStatus::RemoteHasIncoming {
            return Err(DraftlineError::RemoteRace {
                remote,
                variation: variation.as_str().to_string(),
                expected: Some(ledger.old_head.to_string()),
                actual: Some(expected_remote_oid),
            });
        }

        let support_ref = archive_ref("rewrites/squash", variation.as_str(), plan_id.as_str());
        self.ensure_archive_ref(
            &support_ref,
            Oid::from_str(&expected_remote_oid)?,
            "backup before publishing history cleanup rewrite",
        )?;
        let support_ref_preflight =
            self.preflight_publish_support_refs_with_options(&remote, options)?;
        let support_refs = self
            .list_local_support_refs()?
            .into_iter()
            .filter(|support_ref| {
                support_ref.kind == SupportRefKind::Rewrite
                    && support_ref.source_variation.as_deref() == Some(variation.as_str())
                    && support_ref.target_oid == expected_remote_oid
            })
            .collect::<Vec<_>>();
        let can_publish = !support_refs.is_empty()
            && remote_impact.publish_status == CleanupPublishStatus::SharedHistoryRewriteRequired;
        let token = can_publish.then(|| HistoryCleanupPublishToken {
            plan_id: plan_id.clone(),
            remote: remote.clone(),
            variation: variation.clone(),
            expected_remote_oid: expected_remote_oid.clone(),
            replacement_oid: ledger.new_head.to_string(),
            support_ref_token: support_ref_preflight.token,
        });

        Ok(HistoryCleanupPublishPreflight {
            plan_id,
            remote,
            variation,
            expected_remote_oid,
            replacement_oid: ledger.new_head.to_string(),
            remote_impact,
            support_refs,
            token,
            can_publish,
        })
    }

    /// Publishes an applied cleanup rewrite after explicit shared-history confirmation.
    pub fn publish_history_cleanup(
        &self,
        token: HistoryCleanupPublishToken,
        confirmation: RewriteConfirmation,
    ) -> Result<HistoryCleanupPublishResult> {
        let mut options = RemoteOptions::new();
        self.publish_history_cleanup_with_options(token, confirmation, &mut options)
    }

    /// Publishes an applied cleanup rewrite with explicit remote options.
    pub fn publish_history_cleanup_with_options(
        &self,
        token: HistoryCleanupPublishToken,
        _confirmation: RewriteConfirmation,
        options: &mut RemoteOptions<'_>,
    ) -> Result<HistoryCleanupPublishResult> {
        self.ensure_no_pending_recovery()?;
        let ledger = self.read_history_cleanup_ledger(&token.plan_id)?;
        let branch_ref = format!("refs/heads/{}", token.variation.as_str());
        let local_oid = self.repo.refname_to_id(&branch_ref)?.to_string();
        if local_oid != token.replacement_oid || ledger.new_head.as_str() != token.replacement_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{branch_ref}@{}", token.replacement_oid),
                actual: format!("{branch_ref}@{local_oid}"),
            });
        }

        self.publish_support_refs_with_options(token.support_ref_token.clone(), options)?;
        self.fetch_remote_variation_ref(&token.remote, token.variation.as_str(), options)?;
        let actual_remote_oid =
            remote_tracking_oid(&self.repo, &token.remote, token.variation.as_str());
        if actual_remote_oid.as_deref() != Some(token.expected_remote_oid.as_str()) {
            return Err(DraftlineError::RemoteRace {
                remote: token.remote,
                variation: token.variation.as_str().to_string(),
                expected: Some(token.expected_remote_oid),
                actual: actual_remote_oid,
            });
        }

        self.push_refspec(
            &token.remote,
            &format!(
                "+refs/heads/{}:refs/heads/{}",
                token.variation.as_str(),
                token.variation.as_str()
            ),
            vec![PushRefExpectation {
                dst_refname: format!("refs/heads/{}", token.variation.as_str()),
                expected_old_oid: Some(token.expected_remote_oid.clone()),
                expected_new_oid: Some(token.replacement_oid.clone()),
            }],
            options,
        )?;

        let support_refs = token
            .support_ref_token
            .refs
            .into_iter()
            .map(|item| SupportRef {
                id: item.ref_name.clone(),
                ref_name: item.ref_name,
                kind: SupportRefKind::Rewrite,
                source_variation: Some(token.variation.as_str().to_string()),
                target_oid: item.target_oid,
                scope: SupportRefScope::Local,
            })
            .collect::<Vec<_>>();

        Ok(HistoryCleanupPublishResult {
            plan_id: token.plan_id,
            remote: token.remote,
            variation: token.variation,
            expected_remote_oid: token.expected_remote_oid.clone(),
            replacement_oid: token.replacement_oid.clone(),
            support_refs,
            ref_updates: vec![RefUpdate {
                name: RefName::from(branch_ref),
                old: Some(VersionId::from_canonical_string(token.expected_remote_oid)?),
                new: Some(VersionId::from_canonical_string(token.replacement_oid)?),
            }],
        })
    }

    /// Resolves an old version ID through applied cleanup ledgers and support refs.
    pub fn resolve_rewritten_version(
        &self,
        request: StaleVersionResolutionRequest,
    ) -> Result<StaleVersionResolution> {
        let requested_oid = oid_from_version(&request.version)?;
        if self.version_is_reachable_from_local_variation(requested_oid)? {
            return Ok(StaleVersionResolution {
                requested: request.version.clone(),
                disposition: StaleVersionDisposition::Live {
                    version: request.version,
                },
            });
        }

        for ledger in self.read_history_cleanup_ledgers()? {
            if let Some(entry) = ledger
                .commit_map
                .iter()
                .find(|entry| entry.old == request.version)
            {
                let disposition = match &entry.disposition {
                    RewriteDisposition::Preserved { new_id } => StaleVersionDisposition::Live {
                        version: new_id.clone(),
                    },
                    RewriteDisposition::SquashedInto { new_id } => {
                        StaleVersionDisposition::SquashedInto {
                            version: new_id.clone(),
                        }
                    }
                    RewriteDisposition::DroppedAsNoise => StaleVersionDisposition::DroppedAsNoise,
                    RewriteDisposition::OrphanedButBackedUp { backup_ref } => {
                        StaleVersionDisposition::BackedUp {
                            backup_ref: backup_ref.clone(),
                        }
                    }
                    RewriteDisposition::ConflictRequiresUserChoice => {
                        StaleVersionDisposition::Unknown
                    }
                };
                return Ok(StaleVersionResolution {
                    requested: request.version,
                    disposition,
                });
            }
        }

        if let Some(support_ref) = self
            .list_local_support_refs()?
            .into_iter()
            .chain(self.list_remote_tracking_support_refs()?)
            .find(|support_ref| support_ref.target_oid == requested_oid.to_string())
        {
            return Ok(StaleVersionResolution {
                requested: request.version,
                disposition: StaleVersionDisposition::BackedUp {
                    backup_ref: RefName::from(support_ref.ref_name),
                },
            });
        }

        Ok(StaleVersionResolution {
            requested: request.version,
            disposition: StaleVersionDisposition::Unknown,
        })
    }

    /// Plans undoing an applied cleanup by restoring its generated backup ref.
    pub fn preflight_undo_history_cleanup(
        &self,
        plan_id: CleanupPlanId,
    ) -> Result<HistoryCleanupUndoPreflight> {
        self.ensure_no_pending_recovery()?;
        let stored = self.read_history_cleanup_plan(&plan_id)?;
        let ledger = self.read_history_cleanup_ledger(&plan_id)?;
        let backup_ref = ledger.backup_refs.first().cloned().ok_or_else(|| {
            DraftlineError::InvalidHistoryCleanup(format!(
                "cleanup plan `{plan_id}` did not create a backup ref"
            ))
        })?;
        let restore_oid = self.repo.refname_to_id(backup_ref.as_str())?;
        let restore_head = VersionId::from(restore_oid);
        if restore_head != ledger.old_head {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{}", backup_ref.as_str(), ledger.old_head),
                actual: format!("{}@{restore_head}", backup_ref.as_str()),
            });
        }
        let branch_ref = format!("refs/heads/{}", stored.preview.target_variation.as_str());
        let current_oid = self.repo.refname_to_id(&branch_ref)?;
        let expected_current_head = VersionId::from(current_oid);
        if expected_current_head != ledger.new_head {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{branch_ref}@{}", ledger.new_head),
                actual: format!("{branch_ref}@{expected_current_head}"),
            });
        }
        let token = HistoryCleanupUndoToken {
            plan_id: plan_id.clone(),
            target_variation: stored.preview.target_variation.clone(),
            backup_ref: backup_ref.clone(),
            expected_current_head: expected_current_head.clone(),
            restore_head: restore_head.clone(),
        };
        Ok(HistoryCleanupUndoPreflight {
            plan_id,
            target_variation: stored.preview.target_variation,
            backup_ref,
            expected_current_head,
            restore_head,
            ref_updates: ledger.ref_updates.clone(),
            token,
            can_undo: true,
        })
    }

    /// Restores a cleanup backup ref after a successful undo preflight.
    pub fn undo_history_cleanup(
        &self,
        token: HistoryCleanupUndoToken,
    ) -> Result<TimelineCleanupResult> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path(), "undo_history_cleanup")?;
        let ledger = self.read_history_cleanup_ledger(&token.plan_id)?;
        let branch_ref = format!("refs/heads/{}", token.target_variation.as_str());
        let current_oid = oid_from_version(&token.expected_current_head)?;
        for update in &ledger.ref_updates {
            let Some(expected_new) = update.new.as_ref() else {
                continue;
            };
            let expected_new_oid = oid_from_version(expected_new)?;
            let actual = self.repo.refname_to_id(update.name.as_str())?;
            if actual != expected_new_oid {
                return Err(DraftlineError::LocalStateChanged {
                    expected: format!("{}@{expected_new_oid}", update.name.as_str()),
                    actual: format!("{}@{actual}", update.name.as_str()),
                });
            }
        }
        let restore_oid = self.repo.refname_to_id(token.backup_ref.as_str())?;
        let expected_restore_oid = oid_from_version(&token.restore_head)?;
        if restore_oid != expected_restore_oid {
            return Err(DraftlineError::LocalStateChanged {
                expected: format!("{}@{expected_restore_oid}", token.backup_ref.as_str()),
                actual: format!("{}@{restore_oid}", token.backup_ref.as_str()),
            });
        }

        let undo_operation_id = new_operation_id();
        let undo_backup_ref = RefName::from(archive_ref(
            "backups/history-cleanup-undo",
            token.target_variation.as_str(),
            &undo_operation_id,
        ));
        self.ensure_archive_ref(
            undo_backup_ref.as_str(),
            current_oid,
            "backup before undoing history cleanup",
        )?;
        self.write_recovery_state(&RecoveryState {
            operation_id: undo_operation_id.clone(),
            operation: RecoveryOperation::HistoryCleanup,
            original_variation: Some(token.target_variation.to_string()),
            target: Some(token.expected_current_head.to_string()),
            completed: false,
        })?;
        for update in &ledger.ref_updates {
            let Some(old) = update.old.as_ref() else {
                continue;
            };
            self.repo.reference(
                update.name.as_str(),
                oid_from_version(old)?,
                true,
                "undo_history_cleanup",
            )?;
        }
        if self.head_symbolic_variation().as_deref() == Some(token.target_variation.as_str()) {
            self.repo.set_head(&branch_ref)?;
            self.repo
                .checkout_head(Some(CheckoutBuilder::new().force()))?;
        }
        let ref_updates = ledger
            .ref_updates
            .iter()
            .map(|update| RefUpdate {
                name: update.name.clone(),
                old: update.new.clone(),
                new: update.old.clone(),
            })
            .collect();

        let result = TimelineCleanupResult {
            plan_id: token.plan_id,
            old_head: token.expected_current_head.clone(),
            new_head: token.restore_head.clone(),
            backup_refs: vec![token.backup_ref, undo_backup_ref],
            ref_updates,
            commit_map: ledger.commit_map,
            snapshot_map: ledger.snapshot_map,
            warnings: ledger.warnings,
        };
        self.write_recovery_state(&RecoveryState {
            operation_id: undo_operation_id,
            operation: RecoveryOperation::HistoryCleanup,
            original_variation: None,
            target: Some(result.new_head.to_string()),
            completed: true,
        })?;
        Ok(result)
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
                let Some(name) = head.shorthand() else {
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
                    .and_then(|r| r.symbolic_target().map(str::to_string))
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
            .and_then(|reference| reference.symbolic_target().map(str::to_string))
            .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_string))
    }

    fn resolve_restore_version_target(
        &self,
        target: RestoreVersionTarget,
        current_variation: &str,
    ) -> Result<(String, VariationMetadata, Oid)> {
        match target {
            RestoreVersionTarget::Current => {
                let parent_oid = self.repo.head()?.peel_to_commit()?.id();
                let metadata = self.read_variation_metadata(current_variation)?;
                Ok((current_variation.to_string(), metadata, parent_oid))
            }
            RestoreVersionTarget::Existing { variation } => {
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

        for name in names.iter().flatten() {
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

    fn checkout_variation_unchecked(&self, variation: &str) -> Result<()> {
        let safe_variation = validate_variation_name(variation)?;
        let branch = self.repo.find_branch(&safe_variation, BranchType::Local)?;
        let reference = branch.into_reference();
        let target = reference.peel(ObjectType::Commit)?;
        self.repo
            .checkout_tree(&target, Some(CheckoutBuilder::new().force()))?;
        self.repo
            .set_head(&format!("refs/heads/{safe_variation}"))?;
        Ok(())
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
        Ok(Some((
            validate_variation_name(source)?,
            validate_variation_name(target)?,
        )))
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
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let refspec = format!("refs/heads/{variation}:refs/heads/{variation}");
        let mut push_options = options.push_options_with_expectations(vec![PushRefExpectation {
            dst_refname: format!("refs/heads/{variation}"),
            expected_old_oid,
            expected_new_oid: Some(expected_new_oid),
        }]);
        remote.push(&[refspec.as_str()], Some(&mut push_options))?;

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
        let mut push_options = options.push_options_with_expectations(expectations);
        remote.push(&[refspec], Some(&mut push_options))?;

        Ok(())
    }

    fn remote_advertised_oid(
        &self,
        remote_name: &str,
        ref_name: &str,
        options: &mut RemoteOptions<'_>,
    ) -> Result<Option<String>> {
        let mut remote = self.repo.find_remote(remote_name)?;
        ensure_remote_transport_supported(&remote)?;
        let oid = if options.has_credentials() {
            let callbacks = options.remote_callbacks();
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
        ensure_remote_transport_supported(&remote)?;
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
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let remote = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote, options)?;
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
        if status.ahead > 0 {
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

    fn plan_compact_milestones(
        &self,
        input: CompactCleanupPlanInput<'_>,
    ) -> Result<PlannedCompactCleanup> {
        let CompactCleanupPlanInput {
            target_variation,
            old_head_oid,
            base,
            milestones,
            preserve_named_branches,
            preserve_merge_boundaries,
        } = input;

        if milestones.is_empty() {
            return Err(DraftlineError::InvalidHistoryCleanup(
                "compact cleanup requires at least one milestone".to_string(),
            ));
        }

        let first_start_oid = oid_from_version(&milestones[0].include_range.start)?;
        let first_start = self.repo.find_commit(first_start_oid)?;
        let base_parent_oid = match base {
            CleanupBase::Auto => first_start.parent(0).ok().map(|parent| parent.id()),
            CleanupBase::Version { version } => Some(oid_from_version(version)?),
        };
        if let (CleanupBase::Version { version }, Some(parent)) =
            (base, first_start.parent(0).ok().map(|parent| parent.id()))
        {
            let requested = oid_from_version(version)?;
            if requested != parent {
                return Err(DraftlineError::InvalidHistoryCleanup(format!(
                    "explicit cleanup base `{requested}` is not the first milestone parent `{parent}`"
                )));
            }
        }

        let last_end = oid_from_version(&milestones[milestones.len() - 1].include_range.end)?;
        let descendant_old_oids = self.first_parent_descendants_after(last_end, old_head_oid)?;

        let mut all_old_oids = Vec::new();
        let mut seen_old_oids = BTreeSet::new();
        let mut operations = Vec::new();
        let mut commit_map = Vec::new();
        let mut oid_rewrites = HashMap::new();
        let mut previous_new_oid = base_parent_oid;
        let mut previous_old_end = base_parent_oid;
        let signature = self.workspace_signature()?;

        for milestone in milestones {
            let title = milestone.title.trim();
            if title.is_empty() {
                return Err(DraftlineError::InvalidHistoryCleanup(
                    "milestone title is required".to_string(),
                ));
            }
            let old_oids = self.first_parent_range(&milestone.include_range)?;
            if let Some(expected_parent) = previous_old_end {
                let start_commit = self.repo.find_commit(old_oids[0])?;
                let actual_parent = start_commit.parent(0).ok().map(|parent| parent.id());
                if actual_parent != Some(expected_parent) {
                    return Err(DraftlineError::InvalidHistoryCleanup(format!(
                        "milestone `{title}` is not contiguous with the previous cleanup range"
                    )));
                }
            }
            for oid in &old_oids {
                if !seen_old_oids.insert(*oid) {
                    return Err(DraftlineError::InvalidHistoryCleanup(format!(
                        "cleanup range includes version `{oid}` more than once"
                    )));
                }
                let commit = self.repo.find_commit(*oid)?;
                if preserve_merge_boundaries && commit.parent_count() > 1 {
                    return Err(DraftlineError::InvalidHistoryCleanup(format!(
                        "cleanup milestone `{title}` crosses merge commit `{oid}`"
                    )));
                }
            }

            let end_oid = *old_oids.last().ok_or_else(|| {
                DraftlineError::InvalidHistoryCleanup("milestone range is empty".to_string())
            })?;
            let end_commit = self.repo.find_commit(end_oid)?;
            let tree = end_commit.tree()?;
            let parent_commit = previous_new_oid
                .map(|oid| self.repo.find_commit(oid))
                .transpose()?;
            let parents = parent_commit.iter().collect::<Vec<_>>();
            let message = cleanup_commit_message(title, milestone.description.as_deref());
            let new_oid = self.repo.commit(
                None,
                &signature,
                &signature,
                &message,
                &tree,
                parents.as_slice(),
            )?;
            let new_version = VersionId::from(new_oid);
            let old_versions = old_oids
                .iter()
                .copied()
                .map(VersionId::from)
                .collect::<Vec<_>>();
            operations.push(CleanupOperation {
                title: title.to_string(),
                description: milestone
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(ToString::to_string),
                old_versions: old_versions.clone(),
                new_version: new_version.clone(),
            });
            for old_oid in &old_oids {
                commit_map.push(CommitRewriteMap {
                    old: VersionId::from(*old_oid),
                    new: Some(new_version.clone()),
                    disposition: RewriteDisposition::SquashedInto {
                        new_id: new_version.clone(),
                    },
                });
                oid_rewrites.insert(*old_oid, new_oid);
            }
            all_old_oids.extend(old_oids);
            previous_new_oid = Some(new_oid);
            previous_old_end = Some(end_oid);
        }

        let selected_commit_count = all_old_oids.len();
        let selected_oid_set = all_old_oids.iter().copied().collect::<BTreeSet<_>>();
        let descendant_oid_set = descendant_old_oids.iter().copied().collect::<BTreeSet<_>>();

        for old_oid in &descendant_old_oids {
            let old_commit = self.repo.find_commit(*old_oid)?;
            let first_parent = old_commit.parent(0).map_err(|_| {
                DraftlineError::InvalidHistoryCleanup(format!(
                    "descendant `{old_oid}` has no first parent to replay"
                ))
            })?;
            let first_parent_oid =
                oid_rewrites
                    .get(&first_parent.id())
                    .copied()
                    .ok_or_else(|| {
                        DraftlineError::InvalidHistoryCleanup(format!(
                    "descendant `{old_oid}` is not contiguous with the rewritten cleanup range"
                ))
                    })?;
            let mut parent_oids = vec![first_parent_oid];
            for index in 1..old_commit.parent_count() {
                let parent = old_commit.parent(index)?;
                let parent_oid = parent.id();
                if selected_oid_set.contains(&parent_oid) {
                    return Err(cleanup_blocked(
                        CleanupWarningCode::MergeBoundaryWouldBeRewritten,
                        format!(
                            "merge commit `{old_oid}` has a secondary parent inside the compacted range"
                        ),
                        vec![VersionId::from(*old_oid), VersionId::from(parent_oid)],
                    ));
                }
                parent_oids.push(oid_rewrites.get(&parent_oid).copied().unwrap_or(parent_oid));
            }

            let parent_commits = parent_oids
                .iter()
                .map(|oid| self.repo.find_commit(*oid).map_err(DraftlineError::from))
                .collect::<Result<Vec<_>>>()?;
            let parents = parent_commits.iter().collect::<Vec<_>>();
            let tree = old_commit.tree()?;
            let author = old_commit.author();
            let committer = old_commit.committer();
            let message = old_commit.message().unwrap_or_default();
            let new_oid = self.repo.commit(
                None,
                &author,
                &committer,
                message,
                &tree,
                parents.as_slice(),
            )?;
            let new_version = VersionId::from(new_oid);
            commit_map.push(CommitRewriteMap {
                old: VersionId::from(*old_oid),
                new: Some(new_version.clone()),
                disposition: RewriteDisposition::Preserved {
                    new_id: new_version,
                },
            });
            oid_rewrites.insert(*old_oid, new_oid);
            previous_new_oid = Some(new_oid);
        }

        let new_head_oid = previous_new_oid.ok_or_else(|| {
            DraftlineError::InvalidHistoryCleanup("cleanup did not produce a new head".to_string())
        })?;
        let old_tree = self.repo.find_commit(old_head_oid)?.tree_id();
        let new_tree = self.repo.find_commit(new_head_oid)?.tree_id();
        if old_tree != new_tree {
            return Err(DraftlineError::InvalidHistoryCleanup(
                "compact cleanup must preserve final workspace file content".to_string(),
            ));
        }

        let mut warnings = Vec::new();
        let mut affected_refs = Vec::new();
        let mut planned_ref_updates = Vec::new();
        let target_ref_update = RefUpdate {
            name: RefName::from(format!("refs/heads/{}", target_variation.as_str())),
            old: Some(VersionId::from(old_head_oid)),
            new: Some(VersionId::from(new_head_oid)),
        };
        affected_refs.push(CleanupAffectedRef {
            name: RefName::from(format!("refs/heads/{}", target_variation.as_str())),
            old: Some(VersionId::from(old_head_oid)),
            new: Some(VersionId::from(new_head_oid)),
            impact: CleanupRefImpact::TargetVariationMoved,
        });

        for (variation, tip) in self.local_variation_tip_oids()? {
            if variation == *target_variation {
                continue;
            }
            let ref_name = RefName::from(format!("refs/heads/{}", variation.as_str()));
            if selected_oid_set.contains(&tip) {
                let warning = cleanup_warning(
                    if preserve_named_branches {
                        CleanupWarningCode::NamedBranchInsideCompactedRange
                    } else {
                        CleanupWarningCode::NamedBranchWouldBeAffected
                    },
                    format!(
                        "variation `{}` points inside the compacted range",
                        variation.as_str()
                    ),
                    vec![VersionId::from(tip)],
                );
                if preserve_named_branches {
                    return Err(DraftlineError::HistoryCleanupBlocked(Box::new(
                        HistoryCleanupBlockReport {
                            operation: "history_cleanup".to_string(),
                            diagnostics: vec![warning],
                            can_proceed: false,
                        },
                    )));
                }
                warnings.push(warning);
                affected_refs.push(CleanupAffectedRef {
                    name: ref_name,
                    old: Some(VersionId::from(tip)),
                    new: None,
                    impact: CleanupRefImpact::PointsInsideCompactedRange,
                });
            } else if descendant_oid_set.contains(&tip) {
                let new_tip = oid_rewrites.get(&tip).copied().ok_or_else(|| {
                    DraftlineError::InvalidHistoryCleanup(format!(
                        "rewritten descendant `{tip}` is missing from cleanup map"
                    ))
                })?;
                planned_ref_updates.push(RefUpdate {
                    name: ref_name.clone(),
                    old: Some(VersionId::from(tip)),
                    new: Some(VersionId::from(new_tip)),
                });
                affected_refs.push(CleanupAffectedRef {
                    name: ref_name,
                    old: Some(VersionId::from(tip)),
                    new: Some(VersionId::from(new_tip)),
                    impact: CleanupRefImpact::DescendantVariationRewritten,
                });
            }
        }
        planned_ref_updates.push(target_ref_update);

        let snapshot_map = commit_map
            .iter()
            .map(|entry| SnapshotRewriteMap {
                old: entry.old.clone(),
                new: entry.new.clone(),
                disposition: entry.disposition.clone(),
            })
            .collect();

        Ok(PlannedCompactCleanup {
            new_head_oid,
            operations,
            commit_map,
            snapshot_map,
            warnings,
            affected_refs,
            planned_ref_updates,
            old_commit_count: selected_commit_count + descendant_old_oids.len(),
            new_commit_count: milestones.len() + descendant_old_oids.len(),
            selected_commit_count,
            descendant_rewrite_count: descendant_old_oids.len(),
        })
    }

    fn first_parent_descendants_after(&self, ancestor_oid: Oid, head_oid: Oid) -> Result<Vec<Oid>> {
        let range = CommitRange {
            start: VersionId::from(ancestor_oid),
            end: VersionId::from(head_oid),
        };
        let mut oids = match self.first_parent_range(&range) {
            Ok(oids) => oids,
            Err(DraftlineError::InvalidHistoryCleanup(_)) => {
                return Err(DraftlineError::HistoryCleanupBlocked(Box::new(
                    HistoryCleanupBlockReport {
                        operation: "history_cleanup".to_string(),
                        diagnostics: vec![cleanup_warning(
                            CleanupWarningCode::RangeEndNotAncestorOfTargetHead,
                            format!(
                                "cleanup range end `{ancestor_oid}` is not a first-parent ancestor of target head `{head_oid}`"
                            ),
                            vec![VersionId::from(ancestor_oid), VersionId::from(head_oid)],
                        )],
                        can_proceed: false,
                    },
                )));
            }
            Err(error) => return Err(error),
        };
        if !oids.is_empty() {
            oids.remove(0);
        }
        Ok(oids)
    }

    fn first_parent_chain_to_root(&self, head_oid: Oid) -> Result<Vec<Oid>> {
        let mut commit = self.repo.find_commit(head_oid)?;
        let mut oids = Vec::new();

        loop {
            oids.push(commit.id());
            let Ok(parent) = commit.parent(0) else {
                break;
            };
            commit = parent;
        }

        Ok(oids)
    }

    fn first_parent_range(&self, range: &CommitRange) -> Result<Vec<Oid>> {
        let start_oid = oid_from_version(&range.start)?;
        let mut commit = self.repo.find_commit(oid_from_version(&range.end)?)?;
        let mut oids = Vec::new();

        loop {
            let oid = commit.id();
            oids.push(oid);
            if oid == start_oid {
                break;
            }
            commit = commit.parent(0).map_err(|_| {
                DraftlineError::InvalidHistoryCleanup(format!(
                    "range end `{}` is not descended from start `{}`",
                    range.end, range.start
                ))
            })?;
        }

        oids.reverse();
        Ok(oids)
    }

    fn cleanup_remote_impact_from_preview(
        &self,
        preview: &HistoryCleanupPreview,
        remote: Option<&str>,
    ) -> Result<HistoryCleanupRemoteImpact> {
        let selected = preview
            .commit_map
            .iter()
            .filter_map(|entry| {
                matches!(entry.disposition, RewriteDisposition::SquashedInto { .. })
                    .then(|| oid_from_version(&entry.old).ok())
                    .flatten()
            })
            .collect::<Vec<_>>();
        let descendants = preview
            .commit_map
            .iter()
            .filter_map(|entry| {
                matches!(entry.disposition, RewriteDisposition::Preserved { .. })
                    .then(|| oid_from_version(&entry.old).ok())
                    .flatten()
            })
            .collect::<Vec<_>>();
        self.cleanup_remote_impact_for_oids(
            remote,
            &preview.target_variation,
            oid_from_version(&preview.old_head)?,
            Some(oid_from_version(&preview.new_head)?),
            &selected,
            &descendants,
        )
        .and_then(|impact| {
            impact.ok_or_else(|| {
                DraftlineError::InvalidHistoryCleanup(
                    "cleanup remote impact requires a remote".to_string(),
                )
            })
        })
    }

    fn cleanup_remote_impact_for_oids(
        &self,
        remote: Option<&str>,
        variation: &VariationId,
        local_head_oid: Oid,
        replacement_head_oid: Option<Oid>,
        selected_oids: &[Oid],
        descendant_oids: &[Oid],
    ) -> Result<Option<HistoryCleanupRemoteImpact>> {
        let Some(remote) = remote else {
            return Ok(None);
        };
        let tracking_ref = RefName::from(format!("refs/remotes/{remote}/{}", variation.as_str()));
        let upstream_head = remote_tracking_oid(&self.repo, remote, variation.as_str())
            .map(VersionId::from_canonical_string)
            .transpose()?;
        let upstream_oid = upstream_head.as_ref().map(oid_from_version).transpose()?;
        let selected = self.cleanup_publication_summary(upstream_oid, selected_oids)?;
        let descendants = self.cleanup_publication_summary(upstream_oid, descendant_oids)?;
        let publish_status = if let Some(upstream_oid) = upstream_oid {
            if local_head_oid != upstream_oid
                && !self
                    .repo
                    .graph_descendant_of(local_head_oid, upstream_oid)?
            {
                CleanupPublishStatus::RemoteHasIncoming
            } else if selected.published_count > 0 || descendants.published_count > 0 {
                CleanupPublishStatus::SharedHistoryRewriteRequired
            } else {
                CleanupPublishStatus::NormalPublish
            }
        } else {
            CleanupPublishStatus::NoUpstream
        };
        let mut warnings = Vec::new();
        match publish_status {
            CleanupPublishStatus::NoUpstream => warnings.push(cleanup_warning(
                CleanupWarningCode::LocalOnlyRewrite,
                format!(
                    "remote `{remote}` has no upstream variation `{}`",
                    variation.as_str()
                ),
                Vec::new(),
            )),
            CleanupPublishStatus::NormalPublish => {}
            CleanupPublishStatus::SharedHistoryRewriteRequired => warnings.push(cleanup_warning(
                CleanupWarningCode::RemoteRewriteRequiresSeparatePublish,
                "cleanup rewrites commits that are already reachable from the upstream remote"
                    .to_string(),
                selected
                    .published_versions
                    .iter()
                    .chain(descendants.published_versions.iter())
                    .cloned()
                    .collect(),
            )),
            CleanupPublishStatus::RemoteHasIncoming => warnings.push(cleanup_warning(
                CleanupWarningCode::TargetRefChangedSincePreview,
                "upstream contains commits that are not in the local cleanup base".to_string(),
                upstream_head.iter().cloned().collect(),
            )),
            CleanupPublishStatus::LocalOnly => {}
        }

        Ok(Some(HistoryCleanupRemoteImpact {
            remote: Some(remote.to_string()),
            variation: variation.clone(),
            tracking_ref: Some(tracking_ref),
            upstream_head,
            local_head: VersionId::from(local_head_oid),
            replacement_head: replacement_head_oid.map(VersionId::from),
            selected,
            descendants,
            publish_status,
            warnings,
        }))
    }

    fn cleanup_publication_summary(
        &self,
        upstream_oid: Option<Oid>,
        oids: &[Oid],
    ) -> Result<CleanupPublicationSummary> {
        let mut published_versions = Vec::new();
        let mut private_versions = Vec::new();
        for oid in oids {
            let version = VersionId::from(*oid);
            let published = upstream_oid
                .map(|upstream| {
                    if upstream == *oid {
                        Ok(true)
                    } else {
                        self.repo.graph_descendant_of(upstream, *oid)
                    }
                })
                .transpose()?
                .unwrap_or(false);
            if published {
                published_versions.push(version);
            } else {
                private_versions.push(version);
            }
        }
        Ok(CleanupPublicationSummary {
            published_count: published_versions.len(),
            private_count: private_versions.len(),
            published_versions,
            private_versions,
        })
    }

    fn history_cleanup_dir(&self) -> PathBuf {
        self.draftline_dir().join("history-cleanup")
    }

    fn history_cleanup_plans_dir(&self) -> PathBuf {
        self.history_cleanup_dir().join("plans")
    }

    fn history_cleanup_ledgers_dir(&self) -> PathBuf {
        self.history_cleanup_dir().join("ledgers")
    }

    fn history_cleanup_plan_path(&self, plan_id: &CleanupPlanId) -> PathBuf {
        self.history_cleanup_plans_dir()
            .join(format!("{}.json", plan_id.as_str()))
    }

    fn history_cleanup_ledger_path(&self, plan_id: &CleanupPlanId) -> PathBuf {
        self.history_cleanup_ledgers_dir()
            .join(format!("{}.json", plan_id.as_str()))
    }

    fn write_history_cleanup_plan(&self, plan: &HistoryCleanupStoredPlan) -> Result<()> {
        fs::create_dir_all(self.history_cleanup_plans_dir())?;
        fs::write(
            self.history_cleanup_plan_path(&plan.preview.plan_id),
            serde_json::to_vec_pretty(plan)?,
        )?;
        Ok(())
    }

    fn read_history_cleanup_plan(
        &self,
        plan_id: &CleanupPlanId,
    ) -> Result<HistoryCleanupStoredPlan> {
        let path = self.history_cleanup_plan_path(plan_id);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(DraftlineError::InvalidHistoryCleanup(format!(
                    "cleanup plan `{plan_id}` was not found"
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn write_history_cleanup_ledger(&self, result: &TimelineCleanupResult) -> Result<()> {
        fs::create_dir_all(self.history_cleanup_ledgers_dir())?;
        fs::write(
            self.history_cleanup_ledger_path(&result.plan_id),
            serde_json::to_vec_pretty(result)?,
        )?;
        Ok(())
    }

    fn read_history_cleanup_ledger(
        &self,
        plan_id: &CleanupPlanId,
    ) -> Result<TimelineCleanupResult> {
        let path = self.history_cleanup_ledger_path(plan_id);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(DraftlineError::InvalidHistoryCleanup(format!(
                    "cleanup ledger `{plan_id}` was not found"
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn read_history_cleanup_ledgers(&self) -> Result<Vec<TimelineCleanupResult>> {
        let dir = self.history_cleanup_ledgers_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut ledgers = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json")
            {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            ledgers.push(serde_json::from_slice(&bytes)?);
        }
        Ok(ledgers)
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

    fn lock_path(&self) -> PathBuf {
        self.draftline_dir().join("operation.lock")
    }

    fn write_recovery_state(&self, state: &RecoveryState) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        fs::write(self.ledger_path(), serde_json::to_vec_pretty(state)?)?;
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

    fn write_selected_changes_tree(&self, changed_files: &[ChangedFile]) -> Result<Oid> {
        let all_changed_files = self.changed_files_unchecked()?;
        let selected_paths: BTreeSet<_> = changed_files
            .iter()
            .map(|changed| changed.path.as_path())
            .collect();
        let mut rename_targets = HashMap::new();
        let mut paths_to_reset = BTreeSet::new();
        let mut paths_to_restore = BTreeSet::new();
        for changed in &all_changed_files {
            let mut affected_paths = vec![changed.path.clone()];
            if changed.kind == ChangeKind::Renamed {
                if let Some(target) = self.staged_rename_target_for_path(&changed.path)? {
                    affected_paths.push(target.clone());
                    rename_targets.insert(changed.path.clone(), target);
                }
            }
            paths_to_reset.extend(affected_paths.iter().cloned());
            if !selected_paths.contains(changed.path.as_path()) {
                paths_to_restore.extend(affected_paths);
            }
        }

        let original_index_entries = {
            let index = self.repo.index()?;
            paths_to_restore
                .iter()
                .map(|path| (path.clone(), index.get_path(path, 0)))
                .collect::<Vec<_>>()
        };

        if let Ok(head) = self
            .repo
            .head()
            .and_then(|head| head.peel(ObjectType::Commit))
        {
            self.repo
                .reset_default(Some(&head), paths_to_reset.iter().map(PathBuf::as_path))?;
        }

        let mut index = self.repo.index()?;
        if self.repo.head().is_err() {
            for path in &paths_to_reset {
                if let Err(error) = index.remove_path(path) {
                    if error.code() != git2::ErrorCode::NotFound {
                        return Err(error.into());
                    }
                }
            }
        }

        for changed in changed_files {
            match changed.kind {
                ChangeKind::Renamed => {
                    let target = rename_targets.get(&changed.path).ok_or_else(|| {
                        DraftlineError::LocalStateChanged {
                            expected: format!(
                                "staged rename target for {}",
                                changed.path.display()
                            ),
                            actual: "missing rename target".to_string(),
                        }
                    })?;
                    index.add_path(target)?;
                    if let Err(error) = index.remove_path(&changed.path) {
                        if error.code() != git2::ErrorCode::NotFound {
                            return Err(error.into());
                        }
                    }
                }
                ChangeKind::Deleted => {
                    if let Err(error) = index.remove_path(&changed.path) {
                        if error.code() != git2::ErrorCode::NotFound {
                            return Err(error.into());
                        }
                    }
                }
                _ => index.add_path(&changed.path)?,
            }
        }

        index.write()?;
        let selected_tree_id = index.write_tree()?;

        for (path, entry) in original_index_entries {
            if let Some(entry) = entry {
                index.add(&entry)?;
            } else if let Err(error) = index.remove_path(&path) {
                if error.code() != git2::ErrorCode::NotFound {
                    return Err(error.into());
                }
            }
        }
        index.write()?;

        Ok(selected_tree_id)
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

        let registry = ResolverRegistry::with_default_resolvers();
        let mut files = Vec::new();
        let mut conflicts = Vec::new();

        for path in paths {
            let base = self.tree_blob_bytes(base_tree, &path)?;
            let ours = self.tree_blob_bytes(local_tree, &path)?;
            let theirs = self.tree_blob_bytes(remote_tree, &path)?;
            let merged = match merge_blob_contents(&registry, &path, base, ours, theirs) {
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

    fn write_incoming_merge_version(
        &self,
        variation: String,
        remote_oid: String,
        merge_input: &IncomingMergeInput<'_>,
        label: &str,
        files: Vec<MergeFileChange>,
        profile: Option<&ContributorProfile>,
    ) -> Result<MergeIncomingResult> {
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::MergeIncoming,
            original_variation: Some(variation),
            target: Some(remote_oid),
            completed: false,
        })?;

        let mut index = self.repo.index()?;
        for change in &files {
            let full_path = self.root.join(&change.path);
            match &change.content {
                Some(content) => {
                    if let Some(parent) = full_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&full_path, content)?;
                    index.add_path(&change.path)?;
                }
                None => {
                    remove_workspace_path_if_exists(&full_path)?;
                    if let Err(error) = index.remove_path(&change.path) {
                        if error.code() != git2::ErrorCode::NotFound {
                            return Err(error.into());
                        }
                    }
                }
            }
        }
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let (author, committer) = self.workspace_signatures(profile)?;
        let oid = self.repo.commit(
            Some("HEAD"),
            &author,
            &committer,
            label,
            &tree,
            &[&merge_input.local_commit, &merge_input.remote_commit],
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
            let Some(name) = entry.name() else {
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
            if name == "HEAD"
                || name.ends_with("/HEAD")
                || name == "draftline"
                || name.starts_with("draftline/")
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

fn default_initial_variation_name() -> String {
    "main".to_string()
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

fn version_from_commit(commit: &Commit<'_>) -> Version {
    Version {
        id: VersionId::from(commit.id()),
        label: commit.summary().unwrap_or_default().to_string(),
        author: contributor_from_signature(&commit.author()),
        saved_by: contributor_from_signature(&commit.committer()),
        time_seconds: commit.time().seconds(),
    }
}

fn oid_from_version(version: &VersionId) -> Result<Oid> {
    Oid::from_str(version.as_str())
        .map_err(|_| DraftlineError::VersionNotFound(version.to_string()))
}

fn cleanup_commit_message(title: &str, description: Option<&str>) -> String {
    match description
        .map(str::trim)
        .filter(|description| !description.is_empty())
    {
        Some(description) => format!("{title}\n\n{description}"),
        None => title.to_string(),
    }
}

fn cleanup_warning(
    code: CleanupWarningCode,
    message: impl Into<String>,
    related_versions: Vec<VersionId>,
) -> CleanupWarning {
    CleanupWarning {
        code,
        message: message.into(),
        related_versions,
        safe_next_actions: Vec::new(),
    }
}

fn cleanup_blocked(
    code: CleanupWarningCode,
    message: impl Into<String>,
    related_versions: Vec<VersionId>,
) -> DraftlineError {
    DraftlineError::HistoryCleanupBlocked(Box::new(HistoryCleanupBlockReport {
        operation: "history_cleanup".to_string(),
        diagnostics: vec![cleanup_warning(code, message, related_versions)],
        can_proceed: false,
    }))
}

fn cleanup_remote_warnings(policy: &RemoteRewritePolicy) -> Vec<CleanupWarning> {
    match policy {
        RemoteRewritePolicy::LocalOnly => vec![cleanup_warning(
            CleanupWarningCode::LocalOnlyRewrite,
            "history cleanup will update only local Draftline refs",
            Vec::new(),
        )],
        RemoteRewritePolicy::PushWithLease { remote, branch } => vec![cleanup_warning(
            CleanupWarningCode::RemoteRewriteRequiresSeparatePublish,
            format!(
                "apply_history_cleanup is local-first; publish `{branch}` to `{remote}` with preflight_replace_remote_history after apply"
            ),
            Vec::new(),
        )],
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

fn workspace_graph_node_from_commit(
    commit: &Commit<'_>,
    tips: &HashMap<Oid, Vec<VariationId>>,
    head_oid: Option<Oid>,
    local_reachable: &BTreeSet<Oid>,
    support_reachable: &BTreeSet<Oid>,
    topo_index: usize,
) -> Result<WorkspaceGraphNode> {
    let oid = commit.id();
    let history_entry = history_entry_from_commit(commit, tips, head_oid)?;
    let kind = if local_reachable.contains(&oid) {
        WorkspaceGraphNodeKind::Normal
    } else if support_reachable.contains(&oid) {
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
        is_head,
        is_current: is_head,
        is_tip,
        is_merge,
        is_branch_point: false,
        child_ids: Vec::new(),
        child_count: 0,
        kind: kind.clone(),
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

fn ensure_remote_transport_supported(remote: &git2::Remote<'_>) -> Result<()> {
    if let Some(url) = remote.url() {
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
    fn rollback_restore_recovery_reports_unavailable_without_original_oid() {
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

        assert!(!rollback.completed);
        assert!(!rollback.changed_workspace);
        assert_eq!(
            rollback.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
        assert_eq!(
            workspace.repo.head().unwrap().target().unwrap().to_string(),
            advanced.id().as_str()
        );
    }

    #[test]
    fn repair_squash_recovery_reports_unavailable_even_when_archive_exists() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        write_file(workspace.root(), "post.md", b"one");
        workspace.save_version("One").unwrap();
        write_file(workspace.root(), "post.md", b"two");
        let original_tip = workspace.save_version("Two").unwrap();
        let archive_ref = archive_ref("rewrites/squash", "main", "interrupted");
        workspace
            .repo
            .reference(
                &archive_ref,
                Oid::from_str(original_tip.id().as_str()).unwrap(),
                false,
                "simulate interrupted squash archive",
            )
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::SquashVersions,
                original_variation: Some("main".to_string()),
                target: Some(original_tip.id().as_str().to_string()),
                completed: false,
            })
            .unwrap();

        let repair = workspace.repair_recovery("interrupted").unwrap();

        assert!(!repair.completed);
        assert!(!repair.changed_workspace);
        assert_eq!(
            repair.safe_next_actions,
            vec![SafeNextAction::RepairRecovery]
        );
        assert_eq!(
            workspace.repo.head().unwrap().target().unwrap().to_string(),
            original_tip.id().as_str()
        );
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
    fn replace_remote_history_publishes_support_ref_before_force_with_lease() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let original_remote_tip = workspace.save_version("v3").unwrap();
        workspace.publish_changes("origin").unwrap();

        let squashed = workspace.squash_versions(2, "Squashed v2+v3").unwrap();
        let preflight = workspace
            .preflight_replace_remote_history("origin")
            .unwrap();
        assert!(preflight.can_replace);
        assert_eq!(
            preflight.expected_remote_oid,
            original_remote_tip.id().as_str()
        );
        assert_eq!(preflight.replacement_oid, squashed.id().as_str());
        assert_eq!(preflight.support_refs.len(), 1);

        workspace
            .replace_remote_history(preflight.token.unwrap().confirm_shared_history_rewrite())
            .unwrap();

        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            squashed.id().as_str()
        );
        assert!(bare
            .references_glob("refs/draftline/rewrites/squash/main/*")
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|reference| reference.target().map(|oid| oid.to_string())
                == Some(original_remote_tip.id().as_str().to_string())));
    }

    #[test]
    fn replace_remote_history_refuses_remote_race_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        workspace.save_version("v3").unwrap();
        workspace.publish_changes("origin").unwrap();

        workspace.squash_versions(2, "Squashed v2+v3").unwrap();
        let preflight = workspace
            .preflight_replace_remote_history("origin")
            .unwrap();

        let other = tempfile::tempdir().unwrap();
        let other_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), other.path()).unwrap();
        configure_identity(&other_workspace, "Maria", "maria@example.com");
        write_file(other_workspace.root(), "post.md", b"remote race");
        let raced_tip = other_workspace.save_version("Remote race").unwrap();
        other_workspace.publish_changes("origin").unwrap();

        let err = workspace
            .replace_remote_history(preflight.token.unwrap().confirm_shared_history_rewrite())
            .unwrap_err();

        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            raced_tip.id().as_str()
        );
    }

    #[test]
    fn replace_remote_history_requires_explicit_confirmation() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let original_remote_tip = workspace.save_version("v3").unwrap();
        workspace.publish_changes("origin").unwrap();

        workspace.squash_versions(2, "Squashed v2+v3").unwrap();
        let preflight = workspace
            .preflight_replace_remote_history("origin")
            .unwrap();
        assert!(!preflight.token.as_ref().unwrap().confirmed_rewrite);

        let err = workspace
            .replace_remote_history(preflight.token.unwrap())
            .unwrap_err();

        assert!(matches!(err, DraftlineError::ConsentRequired(_)));
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            original_remote_tip.id().as_str()
        );
    }

    #[test]
    fn replace_remote_history_can_retry_after_support_ref_was_already_published() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());

        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        workspace.save_version("v3").unwrap();
        workspace.publish_changes("origin").unwrap();

        let squashed = workspace.squash_versions(2, "Squashed v2+v3").unwrap();
        let first_preflight = workspace
            .preflight_replace_remote_history("origin")
            .unwrap();
        workspace
            .publish_support_refs(first_preflight.token.unwrap().support_ref_token)
            .unwrap();

        let retry_preflight = workspace
            .preflight_replace_remote_history("origin")
            .unwrap();
        assert!(retry_preflight.can_replace);
        assert!(retry_preflight.token.is_some());
        assert_eq!(retry_preflight.support_refs.len(), 1);

        workspace
            .replace_remote_history(
                retry_preflight
                    .token
                    .unwrap()
                    .confirm_shared_history_rewrite(),
            )
            .unwrap();

        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            squashed.id().as_str()
        );
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

    // ── history_cleanup ───────────────────────────────────────────────────────

    fn compact_cleanup_request(start: &Version, end: &Version) -> HistoryCleanupRequest {
        HistoryCleanupRequest {
            target_variation: None,
            base: CleanupBase::Auto,
            mode: CleanupMode::CompactMilestones {
                milestones: vec![MilestoneSpec {
                    title: "Clean milestone".to_string(),
                    description: Some("Compacted noisy saves".to_string()),
                    include_range: CommitRange {
                        start: start.id().clone(),
                        end: end.id().clone(),
                    },
                }],
                preserve_named_branches: true,
                preserve_merge_boundaries: true,
            },
            safety: CleanupSafety::default_user_facing(),
            remote_policy: RemoteRewritePolicy::LocalOnly,
        }
    }

    #[test]
    fn history_cleanup_compacts_milestones_maps_old_versions_and_undoes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("noisy autosave").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("another noisy autosave").unwrap();

        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();
        assert_eq!(preview.target_variation, VariationId::from("main"));
        assert_eq!(preview.old_head, v3.id().clone());
        assert_ne!(preview.new_head, preview.old_head);
        assert_eq!(preview.operations.len(), 1);
        assert_eq!(
            preview.operations[0].old_versions,
            vec![v2.id().clone(), v3.id().clone()]
        );
        assert_eq!(preview.graph_diff.old_commit_count, 2);
        assert_eq!(preview.graph_diff.new_commit_count, 1);
        assert_eq!(preview.graph_diff.squashed_commit_count, 1);
        assert!(preview
            .commit_map
            .iter()
            .all(|entry| matches!(entry.disposition, RewriteDisposition::SquashedInto { .. })));

        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        assert_eq!(result.old_head, v3.id().clone());
        assert_eq!(result.new_head, preview.new_head);
        assert_eq!(
            workspace
                .repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            result.new_head.as_str()
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "v3"
        );
        assert_eq!(result.backup_refs.len(), 1);
        assert_eq!(
            workspace
                .repo
                .refname_to_id(result.backup_refs[0].as_str())
                .unwrap()
                .to_string(),
            v3.id().as_str()
        );

        let resolution = workspace
            .resolve_rewritten_version(StaleVersionResolutionRequest {
                version: v2.id().clone(),
            })
            .unwrap();
        assert!(matches!(
            resolution.disposition,
            StaleVersionDisposition::SquashedInto { ref version } if version == &result.new_head
        ));

        let undo = workspace
            .preflight_undo_history_cleanup(result.plan_id.clone())
            .unwrap();
        assert!(undo.can_undo);
        let undo_result = workspace.undo_history_cleanup(undo.token).unwrap();
        assert_eq!(undo_result.new_head, v3.id().clone());
        assert_eq!(
            workspace
                .repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            v3.id().as_str()
        );
    }

    #[test]
    fn history_cleanup_compacts_middle_range_and_replays_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("noisy autosave").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("another noisy autosave").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let v4 = workspace.save_version("keep v4").unwrap();
        write_file(workspace.root(), "post.md", b"v5");
        let v5 = workspace.save_version("keep v5").unwrap();

        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();

        assert_eq!(preview.old_head, v5.id().clone());
        assert_eq!(preview.selected_commit_count, 2);
        assert_eq!(preview.descendant_rewrite_count, 2);
        assert_eq!(preview.graph_diff.old_commit_count, 4);
        assert_eq!(preview.graph_diff.new_commit_count, 3);
        assert_eq!(preview.graph_diff.squashed_commit_count, 1);
        assert_eq!(preview.planned_ref_updates.len(), 1);
        assert!(preview.commit_map.iter().any(|entry| {
            entry.old == *v4.id()
                && matches!(entry.disposition, RewriteDisposition::Preserved { .. })
        }));

        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        assert_eq!(result.old_head, v5.id().clone());
        assert_eq!(result.new_head, preview.new_head);
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "v5"
        );

        let squashed = workspace
            .resolve_rewritten_version(StaleVersionResolutionRequest {
                version: v2.id().clone(),
            })
            .unwrap();
        assert!(matches!(
            squashed.disposition,
            StaleVersionDisposition::SquashedInto { .. }
        ));
        let preserved = workspace
            .resolve_rewritten_version(StaleVersionResolutionRequest {
                version: v4.id().clone(),
            })
            .unwrap();
        assert!(matches!(
            preserved.disposition,
            StaleVersionDisposition::Live { ref version } if version != v4.id()
        ));
    }

    #[test]
    fn history_compaction_candidates_reports_viable_partner_nodes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let v4 = workspace.save_version("v4").unwrap();

        let candidates = workspace
            .history_compaction_candidates(HistoryCompactionCandidatesRequest {
                target_variation: None,
                selected_version: v2.id().clone(),
                remote: None,
                preserve_named_branches: true,
                preserve_merge_boundaries: true,
            })
            .unwrap();

        assert_eq!(candidates.target_variation, VariationId::from("main"));
        assert_eq!(candidates.target_head, v4.id().clone());
        let v3_candidate = candidates
            .candidates
            .iter()
            .find(|candidate| candidate.version.id() == v3.id())
            .unwrap();
        assert_eq!(v3_candidate.version.id(), v3.id());
        assert!(v3_candidate.can_compact);
        assert_eq!(
            v3_candidate.selected_role,
            CompactionSelectionRole::RangeStart
        );
        assert_eq!(v3_candidate.include_range.start, v2.id().clone());
        assert_eq!(v3_candidate.include_range.end, v3.id().clone());
        assert_eq!(v3_candidate.selected_commit_count, 2);
        assert_eq!(v3_candidate.descendant_rewrite_count, 1);
        assert!(v3_candidate.requires_descendant_replay);
        let candidate_json = serde_json::to_value(v3_candidate).unwrap();
        assert_eq!(candidate_json["version"]["id"], v3.id().as_str());
    }

    #[test]
    fn history_compaction_candidates_supports_selected_range_end() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let v4 = workspace.save_version("v4").unwrap();
        write_file(workspace.root(), "post.md", b"v5");
        let v5 = workspace.save_version("v5").unwrap();

        let candidates = workspace
            .history_compaction_candidates(HistoryCompactionCandidatesRequest {
                target_variation: None,
                selected_version: v4.id().clone(),
                remote: None,
                preserve_named_branches: true,
                preserve_merge_boundaries: true,
            })
            .unwrap();

        let v2_candidate = candidates
            .candidates
            .iter()
            .find(|candidate| candidate.version.id() == v2.id())
            .unwrap();
        assert_eq!(
            v2_candidate.selected_role,
            CompactionSelectionRole::RangeEnd
        );
        assert_eq!(v2_candidate.include_range.start, v2.id().clone());
        assert_eq!(v2_candidate.include_range.end, v4.id().clone());
        assert_eq!(v2_candidate.selected_commit_count, 3);
        assert_eq!(v2_candidate.descendant_rewrite_count, 1);
        assert!(v2_candidate.requires_descendant_replay);
        assert!(v2_candidate.can_compact);
        assert_eq!(candidates.target_head, v5.id().clone());
    }

    #[test]
    fn history_compaction_candidates_reports_remote_impact_for_published_range() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let v4 = workspace.save_version("v4").unwrap();
        workspace.publish_changes("origin").unwrap();

        let candidates = workspace
            .history_compaction_candidates(HistoryCompactionCandidatesRequest {
                target_variation: None,
                selected_version: v2.id().clone(),
                remote: Some("origin".to_string()),
                preserve_named_branches: true,
                preserve_merge_boundaries: true,
            })
            .unwrap();

        let v3_candidate = candidates
            .candidates
            .iter()
            .find(|candidate| candidate.version.id() == v3.id())
            .unwrap();
        let impact = v3_candidate.remote_impact.as_ref().unwrap();
        assert_eq!(
            impact.publish_status,
            CleanupPublishStatus::SharedHistoryRewriteRequired
        );
        assert_eq!(impact.selected.published_count, 2);
        assert_eq!(impact.descendants.published_count, 1);
        assert_eq!(impact.upstream_head, Some(v4.id().clone()));
    }

    #[test]
    fn history_cleanup_publish_replaces_remote_with_support_ref_and_lease() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let original_remote_tip = workspace.save_version("v4").unwrap();
        workspace.publish_changes("origin").unwrap();

        let mut request = compact_cleanup_request(&v2, &v3);
        request.remote_policy = RemoteRewritePolicy::PushWithLease {
            remote: "origin".to_string(),
            branch: "main".to_string(),
        };
        let preview = workspace.preview_history_cleanup(request).unwrap();
        assert_eq!(
            preview.remote_impact.as_ref().unwrap().publish_status,
            CleanupPublishStatus::SharedHistoryRewriteRequired
        );
        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        let preflight = workspace
            .preflight_publish_history_cleanup(result.plan_id.clone(), "origin")
            .unwrap();
        assert!(preflight.can_publish);
        assert_eq!(
            preflight.expected_remote_oid,
            original_remote_tip.id().as_str()
        );
        assert_eq!(preflight.replacement_oid, result.new_head.as_str());
        assert_eq!(preflight.support_refs.len(), 1);

        let publish = workspace
            .publish_history_cleanup(preflight.token.unwrap(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        assert_eq!(publish.replacement_oid, result.new_head.as_str());

        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            result.new_head.as_str()
        );
        assert!(bare
            .references_glob("refs/draftline/rewrites/squash/main/*")
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|reference| reference.target().map(|oid| oid.to_string())
                == Some(original_remote_tip.id().as_str().to_string())));
    }

    #[test]
    fn history_cleanup_publish_support_ref_targets_remote_tip_when_local_was_ahead() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        workspace.publish_changes("origin").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let v4 = workspace.save_version("v4").unwrap();

        let mut request = compact_cleanup_request(&v2, &v3);
        request.remote_policy = RemoteRewritePolicy::PushWithLease {
            remote: "origin".to_string(),
            branch: "main".to_string(),
        };
        let preview = workspace.preview_history_cleanup(request).unwrap();
        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();

        let preflight = workspace
            .preflight_publish_history_cleanup(result.plan_id.clone(), "origin")
            .unwrap();
        assert!(preflight.can_publish);
        assert_eq!(preflight.expected_remote_oid, v2.id().as_str());
        assert_eq!(preflight.replacement_oid, result.new_head.as_str());
        assert_eq!(preflight.support_refs[0].target_oid, v2.id().as_str());

        workspace
            .publish_history_cleanup(preflight.token.unwrap(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            result.new_head.as_str()
        );
        assert_ne!(result.new_head, v4.id().clone());
    }

    #[test]
    fn history_cleanup_publish_refuses_remote_race_after_preflight() {
        let remote = tempfile::tempdir().unwrap();
        init_bare_remote(remote.path());
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        workspace.publish_changes("origin").unwrap();

        let mut request = compact_cleanup_request(&v2, &v3);
        request.remote_policy = RemoteRewritePolicy::PushWithLease {
            remote: "origin".to_string(),
            branch: "main".to_string(),
        };
        let preview = workspace.preview_history_cleanup(request).unwrap();
        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        let preflight = workspace
            .preflight_publish_history_cleanup(result.plan_id.clone(), "origin")
            .unwrap();

        let other = tempfile::tempdir().unwrap();
        let other_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), other.path()).unwrap();
        configure_identity(&other_workspace, "Maria", "maria@example.com");
        write_file(other_workspace.root(), "post.md", b"remote race");
        let raced_tip = other_workspace.save_version("Remote race").unwrap();
        other_workspace.publish_changes("origin").unwrap();

        let err = workspace
            .publish_history_cleanup(preflight.token.unwrap(), RewriteConfirmation::UserConfirmed)
            .unwrap_err();
        assert!(matches!(err, DraftlineError::RemoteRace { .. }));
        let bare = Repository::open_bare(remote.path()).unwrap();
        assert_eq!(
            bare.refname_to_id("refs/heads/main").unwrap().to_string(),
            raced_tip.id().as_str()
        );
    }

    #[test]
    fn history_cleanup_rewrites_descendant_variation_refs_and_undo_restores_them() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        let v4 = workspace.save_version("v4").unwrap();
        let v4_commit = workspace.find_version_commit(v4.id()).unwrap();
        workspace.repo.branch("side", &v4_commit, false).unwrap();
        drop(v4_commit);
        write_file(workspace.root(), "post.md", b"v5");
        let v5 = workspace.save_version("v5").unwrap();

        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();
        assert_eq!(preview.planned_ref_updates.len(), 2);
        assert!(preview.affected_refs.iter().any(|affected| {
            affected.name.as_str() == "refs/heads/side"
                && affected.impact == CleanupRefImpact::DescendantVariationRewritten
        }));
        let side_new = preview
            .planned_ref_updates
            .iter()
            .find(|update| update.name.as_str() == "refs/heads/side")
            .and_then(|update| update.new.clone())
            .unwrap();

        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        assert_eq!(
            workspace.repo.refname_to_id("refs/heads/side").unwrap(),
            oid_from_version(&side_new).unwrap()
        );

        let undo = workspace
            .preflight_undo_history_cleanup(result.plan_id.clone())
            .unwrap();
        assert_eq!(undo.ref_updates.len(), 2);
        let undo_result = workspace.undo_history_cleanup(undo.token).unwrap();
        assert_eq!(undo_result.new_head, v5.id().clone());
        assert_eq!(
            workspace.repo.refname_to_id("refs/heads/side").unwrap(),
            oid_from_version(v4.id()).unwrap()
        );
    }

    #[test]
    fn history_cleanup_blocks_preserved_variation_inside_compacted_range() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        let v2_commit = workspace.find_version_commit(v2.id()).unwrap();
        workspace.repo.branch("side", &v2_commit, false).unwrap();
        drop(v2_commit);
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "post.md", b"v4");
        workspace.save_version("v4").unwrap();

        let err = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap_err();

        assert!(matches!(
            err,
            DraftlineError::HistoryCleanupBlocked(report)
                if report.diagnostics.iter().any(|diagnostic| diagnostic.code == CleanupWarningCode::NamedBranchInsideCompactedRange)
        ));
    }

    #[test]
    fn history_cleanup_rejects_dirty_worktree_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        write_file(workspace.root(), "scratch.md", b"dirty");

        let err = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap_err();

        assert!(
            matches!(err, DraftlineError::PreflightFailed(report) if report.operation == "history_cleanup")
        );
    }

    #[test]
    fn history_cleanup_apply_rejects_changed_target_ref_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();

        write_file(workspace.root(), "post.md", b"v4");
        workspace.save_version("v4").unwrap();
        let err = workspace
            .apply_history_cleanup(preview.plan_id, RewriteConfirmation::UserConfirmed)
            .unwrap_err();

        assert!(matches!(err, DraftlineError::LocalStateChanged { .. }));
    }

    #[test]
    fn history_cleanup_apply_rejects_changed_preview_ref_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        let v1 = workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();

        workspace
            .repo
            .reference(
                preview.preview_ref.as_str(),
                oid_from_version(v1.id()).unwrap(),
                true,
                "tamper cleanup preview",
            )
            .unwrap();
        let err = workspace
            .apply_history_cleanup(preview.plan_id, RewriteConfirmation::UserConfirmed)
            .unwrap_err();

        assert!(matches!(err, DraftlineError::LocalStateChanged { .. }));
    }

    #[test]
    fn history_cleanup_undo_rejects_work_added_after_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();
        let result = workspace
            .apply_history_cleanup(preview.plan_id, RewriteConfirmation::UserConfirmed)
            .unwrap();

        write_file(workspace.root(), "post.md", b"v4");
        workspace.save_version("v4").unwrap();
        let err = workspace
            .preflight_undo_history_cleanup(result.plan_id)
            .unwrap_err();

        assert!(matches!(err, DraftlineError::LocalStateChanged { .. }));
    }

    #[test]
    fn history_cleanup_recovery_rolls_back_interrupted_branch_move() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();
        let branch_ref = "refs/heads/main";
        let new_head = oid_from_version(&preview.new_head).unwrap();
        workspace
            .repo
            .reference(branch_ref, new_head, true, "simulate interrupted cleanup")
            .unwrap();
        workspace.repo.set_head(branch_ref).unwrap();
        let commit = workspace.repo.find_commit(new_head).unwrap();
        workspace
            .repo
            .checkout_tree(
                commit.tree().unwrap().as_object(),
                Some(CheckoutBuilder::new().force()),
            )
            .unwrap();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: preview.plan_id.to_string(),
                operation: RecoveryOperation::HistoryCleanup,
                original_variation: Some("main".to_string()),
                target: Some(v3.id().to_string()),
                completed: false,
            })
            .unwrap();

        let rollback = workspace
            .rollback_recovery(preview.plan_id.as_str())
            .unwrap();

        assert!(rollback.completed);
        assert_eq!(
            workspace
                .repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            v3.id().as_str()
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "v3"
        );
        assert!(workspace.recovery_state().unwrap().is_none());
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
    fn history_cleanup_undo_recovery_rolls_back_interrupted_branch_move() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        write_file(workspace.root(), "post.md", b"v1");
        workspace.save_version("v1").unwrap();
        write_file(workspace.root(), "post.md", b"v2");
        let v2 = workspace.save_version("v2").unwrap();
        write_file(workspace.root(), "post.md", b"v3");
        let v3 = workspace.save_version("v3").unwrap();
        let preview = workspace
            .preview_history_cleanup(compact_cleanup_request(&v2, &v3))
            .unwrap();
        let result = workspace
            .apply_history_cleanup(preview.plan_id.clone(), RewriteConfirmation::UserConfirmed)
            .unwrap();
        let undo = workspace
            .preflight_undo_history_cleanup(result.plan_id.clone())
            .unwrap();
        let branch_ref = "refs/heads/main";
        let restore_head = oid_from_version(&undo.restore_head).unwrap();
        workspace
            .repo
            .reference(
                branch_ref,
                restore_head,
                true,
                "simulate interrupted cleanup undo",
            )
            .unwrap();
        workspace.repo.set_head(branch_ref).unwrap();
        let commit = workspace.repo.find_commit(restore_head).unwrap();
        workspace
            .repo
            .checkout_tree(
                commit.tree().unwrap().as_object(),
                Some(CheckoutBuilder::new().force()),
            )
            .unwrap();
        let undo_operation_id = new_operation_id();
        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: undo_operation_id.clone(),
                operation: RecoveryOperation::HistoryCleanup,
                original_variation: Some("main".to_string()),
                target: Some(result.new_head.to_string()),
                completed: false,
            })
            .unwrap();

        let rollback = workspace.rollback_recovery(undo_operation_id).unwrap();

        assert!(rollback.completed);
        assert_eq!(
            workspace
                .repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            result.new_head.as_str()
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "v3"
        );
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

        let preflight = workspace.preflight_squash_versions(2).unwrap();
        assert!(preflight.can_squash);
        assert_eq!(preflight.variation, VariationId::from("main"));
        assert_eq!(preflight.head_oid, original_tip.id().as_str());
        assert!(preflight
            .support_ref
            .starts_with("refs/draftline/rewrites/squash/main/"));
        workspace
            .squash_versions_with_token(preflight.token.unwrap(), "Squashed v2+v3")
            .unwrap();

        assert!(workspace
            .repo
            .references()
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|reference| {
                reference
                    .name()
                    .map(|name| name.starts_with("refs/draftline/rewrites/squash/main/"))
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
