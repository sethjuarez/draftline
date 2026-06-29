//! Dependency-free command adapter for Tauri Workbench hosts.
//!
//! The functions in this module intentionally do not depend on Tauri. A host can
//! expose them behind `#[tauri::command]` wrappers while preserving Draftline's
//! existing preflight, recovery, and verification semantics.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    merge::MergeConflict, AdoptionPreflightReport, ApplyIncomingReport, ApplyIncomingResult,
    ChangeSet, ContentPolicy, ContentPolicyAudit, ContributorProfile, CurrentFileDiff,
    CurrentFilePreview, DirtySummary, DraftlineError, HistoryEntry, MergeConflictResolution,
    MergeIncomingReport, MergeIncomingResult, MergeIncomingToken, MergeResolutionChoice,
    OperationLockInspection, PreflightReport, PreviewFile, PublishPreflight, PublishResult,
    RecoveryRepairResult, RecoveryState, RemoteCredential, RemoteCredentialRequest, RemoteEndpoint,
    RemoteOptions, RemoteVariation, RemoteVariationDiagnostics, RestoreVersionTarget, Result,
    Shelf, ShelfApplyReport, SupportRef, SupportRefScope, SwitchPolicy, SyncState, SyncStatus,
    Variation, VariationId, VariationMetadata, VariationRenamePreflight, VariationRenameToken,
    VariationSummary, Version, VersionDiff, VersionId, VersionPreview, Workspace,
    WorkspaceDiagnostic, WorkspaceGraph, WorkspaceGraphAgentSummary, WorkspaceGraphCommonAncestor,
    WorkspaceGraphCompareSummary, WorkspaceGraphNodeDetail, WorkspaceGraphOptions,
    WorkspaceGraphOverviewOptions, WorkspaceGraphPath, WorkspaceGraphRefs,
    WorkspaceGraphSearchResult, WorkspaceGraphSummary, WorkspaceId, WorkspaceInspection,
    WorkspaceSummary, WorkspaceVerification,
};

type CredentialProvider<'callbacks> =
    dyn FnMut(RemoteCredentialRequest<'_>) -> Result<RemoteCredential> + Send + 'callbacks;
type EventSink<'callbacks> = dyn FnMut(DraftlineEvent) + Send + 'callbacks;

/// Reusable host-side configuration for Draftline command adapters.
///
/// Tauri hosts can construct one context with their content policy, attribution,
/// credential callback, and event sink, then route all command wrappers through
/// the `_with_context` functions instead of forking per-command behavior.
pub struct DraftlineCommandContext<'callbacks> {
    content_policy: ContentPolicy,
    contributor_profile: Option<ContributorProfile>,
    credential_provider: Option<Box<CredentialProvider<'callbacks>>>,
    event_sink: Option<Box<EventSink<'callbacks>>>,
    next_event_sequence: u64,
}

impl fmt::Debug for DraftlineCommandContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DraftlineCommandContext")
            .field("content_policy", &self.content_policy)
            .field("contributor_profile", &self.contributor_profile)
            .field(
                "has_credential_provider",
                &self.credential_provider.is_some(),
            )
            .field("has_event_sink", &self.event_sink.is_some())
            .field("next_event_sequence", &self.next_event_sequence)
            .finish()
    }
}

impl<'callbacks> Default for DraftlineCommandContext<'callbacks> {
    fn default() -> Self {
        Self {
            content_policy: ContentPolicy::default(),
            contributor_profile: None,
            credential_provider: None,
            event_sink: None,
            next_event_sequence: 1,
        }
    }
}

impl<'callbacks> DraftlineCommandContext<'callbacks> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_content_policy(mut self, content_policy: ContentPolicy) -> Self {
        self.content_policy = content_policy;
        self
    }

    pub fn with_contributor_profile(mut self, contributor_profile: ContributorProfile) -> Self {
        self.contributor_profile = Some(contributor_profile);
        self
    }

    pub fn with_credentials(
        mut self,
        provider: impl FnMut(RemoteCredentialRequest<'_>) -> Result<RemoteCredential>
            + Send
            + 'callbacks,
    ) -> Self {
        self.credential_provider = Some(Box::new(provider));
        self
    }

    pub fn with_event_sink(mut self, sink: impl FnMut(DraftlineEvent) + Send + 'callbacks) -> Self {
        self.event_sink = Some(Box::new(sink));
        self
    }

    fn open_workspace(&self, path: impl AsRef<std::path::Path>) -> Result<Workspace> {
        Workspace::open_with_policy(path, self.content_policy.clone())
    }

    fn remote_options(&mut self) -> RemoteOptions<'_> {
        if let Some(provider) = self.credential_provider.as_mut() {
            RemoteOptions::new().with_credentials(provider)
        } else {
            RemoteOptions::new()
        }
    }

    fn emit(
        &mut self,
        workspace: &Workspace,
        kind: DraftlineEventKind,
        sync_status: Option<SyncStatus>,
    ) {
        let Some(sink) = self.event_sink.as_mut() else {
            return;
        };

        let event =
            DraftlineEvent::from_workspace(workspace, kind, self.next_event_sequence, sync_status);
        self.next_event_sequence += 1;
        sink(event);
    }
}

/// Stable event kinds emitted by Draftline command adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DraftlineEventKind {
    WorkspaceChanged,
    DirtyChanged,
    HistoryChanged,
    SyncChanged,
    RecoveryRequired,
    OperationLockChanged,
    PolicyChanged,
}

/// Redaction-safe event payload for host invalidation and frontend refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftlineEvent {
    pub workspace_id: WorkspaceId,
    pub kind: DraftlineEventKind,
    pub sequence: u64,
    pub changed_paths: Vec<PathBuf>,
    pub active_variation: Option<VariationId>,
    pub dirty: Option<DirtySummary>,
    pub sync: Option<SyncStatus>,
    pub recovery: Option<RecoveryState>,
    pub diagnostics: Vec<WorkspaceDiagnostic>,
}

impl DraftlineEvent {
    fn from_workspace(
        workspace: &Workspace,
        kind: DraftlineEventKind,
        sequence: u64,
        sync: Option<SyncStatus>,
    ) -> Self {
        let inspection = workspace.inspect().ok();
        let dirty = inspection
            .as_ref()
            .map(|inspection| inspection.dirty.clone());
        let changed_paths = dirty
            .as_ref()
            .map(|dirty| dirty.files.iter().map(|file| file.path.clone()).collect())
            .unwrap_or_default();

        Self {
            workspace_id: WorkspaceId {
                root: workspace.root().to_path_buf(),
            },
            kind,
            sequence,
            changed_paths,
            active_variation: inspection
                .as_ref()
                .and_then(|inspection| inspection.current_variation.clone()),
            dirty,
            sync,
            recovery: inspection
                .as_ref()
                .and_then(|inspection| inspection.recovery.clone()),
            diagnostics: inspection
                .map(|inspection| inspection.diagnostics)
                .unwrap_or_default(),
        }
    }
}

/// Common request shape for read-only workspace diagnostics commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    pub workspace_path: PathBuf,
}

/// Request for a graph-ready full-history workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphRequest {
    pub workspace_path: PathBuf,
    #[serde(default)]
    pub options: WorkspaceGraphOptions,
}

/// Request for a compressed graph-ready workspace overview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphOverviewRequest {
    pub workspace_path: PathBuf,
    #[serde(default)]
    pub options: WorkspaceGraphOverviewOptions,
}

/// Request for graph nodes around a saved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphAroundVersionRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
    #[serde(default = "default_graph_radius")]
    pub radius: usize,
    #[serde(default)]
    pub options: WorkspaceGraphOptions,
}

/// Request for graph nodes by DAG-hop distance around a saved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphNeighborhoodRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
    #[serde(default = "default_graph_radius")]
    pub radius: usize,
    #[serde(default)]
    pub options: WorkspaceGraphOptions,
}

/// Request for searching graph nodes and refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphSearchRequest {
    pub workspace_path: PathBuf,
    pub query: String,
    #[serde(default)]
    pub options: WorkspaceGraphOptions,
}

/// Request for graph path and compare helpers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphPairRequest {
    pub workspace_path: PathBuf,
    pub from_version_id: String,
    pub to_version_id: String,
    #[serde(default)]
    pub options: WorkspaceGraphOptions,
}

/// Request for a focused variation graph lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGraphVariationRequest {
    pub workspace_path: PathBuf,
    pub variation_id: String,
    #[serde(default)]
    pub options: WorkspaceGraphOptions,
}

fn default_graph_radius() -> usize {
    25
}

/// Request for cloning a workspace from a remote endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneWorkspaceRequest {
    pub remote_url: String,
    pub workspace_path: PathBuf,
}

/// Result returned after opening, cloning, or adopting a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOpenResult {
    pub diagnostics: WorkspaceDiagnostics,
}

/// Result returned after adopting an existing Git repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptWorkspaceResult {
    pub preflight: AdoptionPreflightReport,
    pub diagnostics: WorkspaceDiagnostics,
}

/// One workspace snapshot suitable for a Tauri diagnostics panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDiagnostics {
    pub summary: WorkspaceSummary,
    pub inspection: WorkspaceInspection,
    pub verification: WorkspaceVerification,
    pub operation_lock: OperationLockInspection,
}

/// Best-effort postcondition state collected after a mutating command succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPostconditions {
    pub remaining_changes: Option<ChangeSet>,
    pub verification: Option<WorkspaceVerification>,
    pub errors: Vec<TauriCommandError>,
}

/// Request for listing support refs in a local or remote-tracking scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSupportRefsRequest {
    pub workspace_path: PathBuf,
    pub scope: SupportRefScope,
}

/// Request for renaming a local variation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameVariationRequest {
    pub workspace_path: PathBuf,
    pub source_variation_id: String,
    pub target_variation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<VariationRenameToken>,
}

/// Request for safely switching to an existing local variation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchVariationRequest {
    pub workspace_path: PathBuf,
    pub variation_id: String,
}

/// Request for commands that target one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
}

/// Request for reading diff/preview content for one current workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentFileRequest {
    pub workspace_path: PathBuf,
    pub path: PathBuf,
}

/// Request for reading one file from a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewVersionFileRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
    pub path: PathBuf,
}

/// Request for comparing two saved versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffVersionsRequest {
    pub workspace_path: PathBuf,
    pub from_version_id: String,
    pub to_version_id: String,
}

/// Request for restoring one saved version as a new save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreVersionRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
    pub label: String,
}

/// Request for restoring one saved version as a new save on a target variation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetedRestoreVersionRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
    pub label: String,
    pub target: RestoreVersionTarget,
}

/// Request for creating a new variation from an existing saved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateVariationFromVersionRequest {
    pub workspace_path: PathBuf,
    pub version_id: String,
    pub name: String,
    #[serde(default)]
    pub metadata: VariationMetadata,
}

/// Restore result with postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreVersionResult {
    pub version: Version,
    pub postconditions: CommandPostconditions,
}

/// Targeted restore result with the activated target variation and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetedRestoreVersionCommandResult {
    pub version: Version,
    pub target_variation: Variation,
    pub postconditions: CommandPostconditions,
}

/// Variation creation result with postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVariationFromVersionResult {
    pub variation: Variation,
    pub postconditions: CommandPostconditions,
}

/// Request for saving all tracked workspace changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveRequest {
    pub workspace_path: PathBuf,
    pub label: String,
}

/// Save result with postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveResult {
    pub version: Version,
    pub postconditions: CommandPostconditions,
}

/// Request for one shelf by ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfRequest {
    pub workspace_path: PathBuf,
    pub shelf_id: String,
}

/// Shelf apply result with preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyShelfCommandResult {
    pub preflight: ShelfApplyReport,
    pub shelf: Shelf,
    pub postconditions: CommandPostconditions,
}

/// Shelf delete result with postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteShelfResult {
    pub postconditions: CommandPostconditions,
}

/// Request for recovery repair/rollback commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRequest {
    pub workspace_path: PathBuf,
    pub operation_id: String,
}

/// Request for selected-file save operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedSaveRequest {
    pub workspace_path: PathBuf,
    pub paths: Vec<PathBuf>,
    pub label: String,
}

/// Selected-file save result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedSaveResult {
    pub preflight: PreflightReport,
    pub version: Version,
    pub postconditions: CommandPostconditions,
}

/// Request for selected-file shelf operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedShelveRequest {
    pub workspace_path: PathBuf,
    pub paths: Vec<PathBuf>,
    pub name: String,
}

/// Selected-file shelf result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedShelveResult {
    pub preflight: PreflightReport,
    pub shelf: Shelf,
    pub postconditions: CommandPostconditions,
}

/// Request for selected-file discard operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedDiscardRequest {
    pub workspace_path: PathBuf,
    pub paths: Vec<PathBuf>,
}

/// Selected-file discard result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedDiscardResult {
    pub preflight: PreflightReport,
    pub discarded: ChangeSet,
    pub postconditions: CommandPostconditions,
}

/// Request for publishing the current variation to a configured remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishCurrentVariationRequest {
    pub workspace_path: PathBuf,
    pub remote: String,
}

/// Publish result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishCurrentVariationResult {
    pub preflight: PublishPreflight,
    pub publish: PublishResult,
    pub postconditions: CommandPostconditions,
}

/// Request for remote collaboration diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRequest {
    pub workspace_path: PathBuf,
    pub remote: String,
}

/// Request for a remote-tracking variation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVariationRequest {
    pub workspace_path: PathBuf,
    pub remote: String,
    pub variation_id: String,
}

/// Remote-variation adoption result with postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptRemoteVariationResult {
    pub variation: Variation,
    pub postconditions: CommandPostconditions,
}

/// Variation rename result with preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameVariationResult {
    pub preflight: VariationRenamePreflight,
    pub variation: Variation,
    pub postconditions: CommandPostconditions,
}

/// Safe variation switch result with preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchVariationResult {
    pub preflight: PreflightReport,
    pub variation: Variation,
    pub postconditions: CommandPostconditions,
}

/// Fetch result with current sync status after remote refs refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRemoteResult {
    pub sync_status: SyncStatus,
    pub postconditions: CommandPostconditions,
}

/// Fast-forward apply result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyIncomingCommandResult {
    pub preflight: ApplyIncomingReport,
    pub apply: ApplyIncomingResult,
    pub postconditions: CommandPostconditions,
}

/// Request for writing a clean incoming merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeIncomingRequest {
    pub workspace_path: PathBuf,
    pub remote: String,
    pub label: String,
}

/// Request for writing an incoming merge after explicit conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeIncomingWithResolutionsRequest {
    pub workspace_path: PathBuf,
    pub remote: String,
    pub label: String,
    pub token: MergeIncomingToken,
    pub resolutions: Vec<MergeConflictResolution>,
}

/// Merge result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeIncomingCommandResult {
    pub preflight: MergeIncomingReport,
    pub merge: MergeIncomingResult,
    pub postconditions: CommandPostconditions,
}

/// UI-friendly conflict model grouped by file and semantic field path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflictViewModel {
    pub files: Vec<MergeFileConflictGroup>,
    pub token: Option<MergeIncomingToken>,
    pub can_merge_cleanly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeFileConflictGroup {
    pub path: PathBuf,
    pub label: String,
    pub whole_file_conflicts: Vec<MergeConflictItem>,
    pub field_conflicts: Vec<MergeFieldConflictGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeFieldConflictGroup {
    pub field_path: String,
    pub label: String,
    pub conflicts: Vec<MergeConflictItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflictItem {
    pub path: PathBuf,
    pub field_path: Option<String>,
    pub label: String,
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub resolution: crate::merge::ResolutionKind,
}

/// Source content to snapshot into safest whole-file `use_content` resolutions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictContentSource {
    Ours,
    Theirs,
    Base,
}

/// Error shape that Tauri commands can serialize directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TauriCommandError {
    pub code: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

impl From<DraftlineError> for TauriCommandError {
    fn from(error: DraftlineError) -> Self {
        Self {
            code: draftline_error_code(&error).to_string(),
            message: error.to_string(),
            details: draftline_error_details(&error),
        }
    }
}

/// Result alias for Tauri command wrappers that need a serializable error.
pub type TauriCommandResult<T> = std::result::Result<T, TauriCommandError>;

/// Opens an existing Draftline workspace through the host-configured context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn open_workspace(request: WorkspaceRequest) -> Result<WorkspaceOpenResult> {
    open_workspace_with_context(&DraftlineCommandContext::new(), request)
}

/// Opens an existing Draftline workspace through the host-configured context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn open_workspace_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<WorkspaceOpenResult> {
    Ok(WorkspaceOpenResult {
        diagnostics: inspect_workspace_with_context(context, request)?,
    })
}

/// Clones a Draftline workspace using backend-only credentials from the context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn clone_workspace(request: CloneWorkspaceRequest) -> Result<WorkspaceOpenResult> {
    clone_workspace_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Clones a Draftline workspace using backend-only credentials from the context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn clone_workspace_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: CloneWorkspaceRequest,
) -> Result<WorkspaceOpenResult> {
    let content_policy = context.content_policy.clone();
    let workspace = {
        let mut options = context.remote_options();
        Workspace::clone_workspace_with_policy_and_options(
            request.remote_url,
            &request.workspace_path,
            content_policy,
            &mut options,
        )?
    };
    context.emit(&workspace, DraftlineEventKind::WorkspaceChanged, None);
    Ok(WorkspaceOpenResult {
        diagnostics: workspace_diagnostics(&workspace)?,
    })
}

/// Adopts an existing Git repository as a Draftline workspace.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn adopt_workspace(request: WorkspaceRequest) -> Result<AdoptWorkspaceResult> {
    adopt_workspace_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Adopts an existing Git repository using the host-configured content policy.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn adopt_workspace_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<AdoptWorkspaceResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let preflight = workspace.preflight_adopt_workspace(context.content_policy.clone())?;
    Ok(AdoptWorkspaceResult {
        preflight,
        diagnostics: workspace_diagnostics(&workspace)?,
    })
}

/// Returns a full diagnostics payload for the workspace dashboard.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn inspect_workspace(request: WorkspaceRequest) -> Result<WorkspaceDiagnostics> {
    inspect_workspace_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns a full diagnostics payload using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn inspect_workspace_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<WorkspaceDiagnostics> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    workspace_diagnostics(&workspace)
}

/// Returns the workspace verification payload used by smoke postconditions.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn verify_workspace(request: WorkspaceRequest) -> Result<WorkspaceVerification> {
    verify_workspace_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns the workspace verification payload using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn verify_workspace_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<WorkspaceVerification> {
    context
        .open_workspace(&request.workspace_path)?
        .verify_workspace()
}

/// Returns per-variation summaries for a variation switcher.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_variations(request: WorkspaceRequest) -> Result<Vec<VariationSummary>> {
    list_variations_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns per-variation summaries using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_variations_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<Vec<VariationSummary>> {
    context
        .open_workspace(&request.workspace_path)?
        .variation_summaries()
}

/// Preflights a local variation rename without mutating refs.
#[tracing::instrument(
    err,
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        source_variation_id = %request.source_variation_id,
        target_variation_id = %request.target_variation_id
    )
)]
pub fn preflight_rename_variation(
    request: RenameVariationRequest,
) -> Result<VariationRenamePreflight> {
    preflight_rename_variation_with_context(&DraftlineCommandContext::new(), request)
}

/// Preflights a local variation rename using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        source_variation_id = %request.source_variation_id,
        target_variation_id = %request.target_variation_id
    )
)]
pub fn preflight_rename_variation_with_context(
    context: &DraftlineCommandContext<'_>,
    request: RenameVariationRequest,
) -> Result<VariationRenamePreflight> {
    context
        .open_workspace(&request.workspace_path)?
        .preflight_rename_variation(
            &VariationId::from(request.source_variation_id),
            &VariationId::from(request.target_variation_id),
        )
}

/// Renames a local variation through the tokenized guarded path.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        source_variation_id = %request.source_variation_id,
        target_variation_id = %request.target_variation_id
    )
)]
pub fn rename_variation(request: RenameVariationRequest) -> Result<RenameVariationResult> {
    rename_variation_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Renames a local variation using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        source_variation_id = %request.source_variation_id,
        target_variation_id = %request.target_variation_id
    )
)]
pub fn rename_variation_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RenameVariationRequest,
) -> Result<RenameVariationResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let source = VariationId::from(request.source_variation_id);
    let target = VariationId::from(request.target_variation_id);
    let preflight = workspace.preflight_rename_variation(&source, &target)?;
    let token = request.token.unwrap_or_else(|| preflight.token.clone());
    if token.source_variation != source
        || token.target_variation != target
        || token.expected_oid != preflight.expected_oid
    {
        return Err(DraftlineError::LocalStateChanged {
            expected: format!(
                "{}@{}",
                preflight.source_variation.as_str(),
                preflight.expected_oid
            ),
            actual: format!("{}@{}", token.source_variation.as_str(), token.expected_oid),
        });
    }
    let variation = workspace.rename_variation_with_token(token)?;
    context.emit(&workspace, DraftlineEventKind::WorkspaceChanged, None);
    Ok(RenameVariationResult {
        preflight,
        variation,
        postconditions: collect_postconditions(&workspace, false),
    })
}

/// Preflights switching to an existing local variation without mutating workspace files.
#[tracing::instrument(
    err,
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        variation_id = %request.variation_id
    )
)]
pub fn preflight_switch_variation(request: SwitchVariationRequest) -> Result<PreflightReport> {
    preflight_switch_variation_with_context(&DraftlineCommandContext::new(), request)
}

/// Preflights switching to an existing local variation using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        variation_id = %request.variation_id
    )
)]
pub fn preflight_switch_variation_with_context(
    context: &DraftlineCommandContext<'_>,
    request: SwitchVariationRequest,
) -> Result<PreflightReport> {
    context
        .open_workspace(&request.workspace_path)?
        .preflight_switch_variation(&VariationId::from(request.variation_id))
}

/// Switches to an existing local variation without saving, shelving, or discarding dirty work.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        variation_id = %request.variation_id
    )
)]
pub fn switch_variation(request: SwitchVariationRequest) -> Result<SwitchVariationResult> {
    switch_variation_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Switches to an existing local variation using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        variation_id = %request.variation_id
    )
)]
pub fn switch_variation_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: SwitchVariationRequest,
) -> Result<SwitchVariationResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let variation_id = VariationId::from(request.variation_id);
    let preflight = workspace.preflight_switch_variation(&variation_id)?;
    let variation = workspace.switch_variation(&variation_id, SwitchPolicy::AbortIfDirty)?;
    context.emit(&workspace, DraftlineEventKind::WorkspaceChanged, None);
    Ok(SwitchVariationResult {
        preflight,
        variation,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Returns hidden recovery support refs for admin/recovery views.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), scope = ?request.scope)
)]
pub fn list_support_refs(request: ListSupportRefsRequest) -> Result<Vec<SupportRef>> {
    list_support_refs_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns support refs using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), scope = ?request.scope)
)]
pub fn list_support_refs_with_context(
    context: &DraftlineCommandContext<'_>,
    request: ListSupportRefsRequest,
) -> Result<Vec<SupportRef>> {
    context
        .open_workspace(&request.workspace_path)?
        .list_support_refs(request.scope)
}

/// Returns current content changes.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_changes(request: WorkspaceRequest) -> Result<ChangeSet> {
    get_changes_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns current content changes using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_changes_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<ChangeSet> {
    context.open_workspace(&request.workspace_path)?.changes()
}

/// Returns current-variation history.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_history(request: WorkspaceRequest) -> Result<Vec<HistoryEntry>> {
    get_history_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns current-variation history using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_history_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<Vec<HistoryEntry>> {
    context.open_workspace(&request.workspace_path)?.history()
}

/// Returns full cross-variation history.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_full_history(request: WorkspaceRequest) -> Result<Vec<HistoryEntry>> {
    get_full_history_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns full cross-variation history using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_full_history_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<Vec<HistoryEntry>> {
    context
        .open_workspace(&request.workspace_path)?
        .full_history()
}

/// Returns a graph-ready full-history snapshot over Draftline variations.
#[tracing::instrument(
    err,
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        include_remotes = request.options.include_remotes,
        include_support_refs = request.options.include_support_refs
    )
)]
pub fn get_workspace_graph(request: WorkspaceGraphRequest) -> Result<WorkspaceGraph> {
    get_workspace_graph_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns a graph-ready full-history snapshot using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        include_remotes = request.options.include_remotes,
        include_support_refs = request.options.include_support_refs
    )
)]
pub fn get_workspace_graph_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphRequest,
) -> Result<WorkspaceGraph> {
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph(request.options)
}

/// Returns graph refs/tips without walking the full node DAG.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_refs(request: WorkspaceGraphRequest) -> Result<WorkspaceGraphRefs> {
    get_workspace_graph_refs_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns graph refs/tips using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_refs_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphRequest,
) -> Result<WorkspaceGraphRefs> {
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_refs(request.options)
}

/// Returns graph counts and health signals for rendering strategy.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_summary(
    request: WorkspaceGraphRequest,
) -> Result<WorkspaceGraphSummary> {
    get_workspace_graph_summary_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns graph counts and health signals using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_summary_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphRequest,
) -> Result<WorkspaceGraphSummary> {
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_summary(request.options)
}

/// Returns a compressed graph overview for large-repo visualization.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_overview(
    request: WorkspaceGraphOverviewRequest,
) -> Result<WorkspaceGraph> {
    get_workspace_graph_overview_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns a compressed graph overview using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_overview_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphOverviewRequest,
) -> Result<WorkspaceGraph> {
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_overview(request.options)
}

/// Returns a focused graph neighborhood around a version.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_around_version(
    request: WorkspaceGraphAroundVersionRequest,
) -> Result<WorkspaceGraph> {
    get_workspace_graph_around_version_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns a focused graph neighborhood around a version using a configured context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_around_version_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphAroundVersionRequest,
) -> Result<WorkspaceGraph> {
    let version = parse_version_id(request.version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_around_version(&version, request.radius, request.options)
}

/// Returns one variation's focused graph lane.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_for_variation(
    request: WorkspaceGraphVariationRequest,
) -> Result<WorkspaceGraph> {
    get_workspace_graph_for_variation_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns one variation's focused graph lane using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_for_variation_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphVariationRequest,
) -> Result<WorkspaceGraph> {
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_for_variation(&VariationId::from(request.variation_id), request.options)
}

/// Returns an agent-oriented graph summary with safe follow-up command hints.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_agent_summary(
    request: WorkspaceGraphRequest,
) -> Result<WorkspaceGraphAgentSummary> {
    get_workspace_graph_agent_summary_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns an agent-oriented graph summary using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_agent_summary_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphRequest,
) -> Result<WorkspaceGraphAgentSummary> {
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_agent_summary(request.options)
}

/// Returns a DAG-hop graph neighborhood around a version.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_neighborhood(
    request: WorkspaceGraphNeighborhoodRequest,
) -> Result<WorkspaceGraph> {
    get_workspace_graph_neighborhood_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns a DAG-hop graph neighborhood using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_neighborhood_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphNeighborhoodRequest,
) -> Result<WorkspaceGraph> {
    let version = parse_version_id(request.version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_neighborhood(&version, request.radius, request.options)
}

/// Searches graph nodes and refs.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn search_workspace_graph(
    request: WorkspaceGraphSearchRequest,
) -> Result<WorkspaceGraphSearchResult> {
    search_workspace_graph_with_context(&DraftlineCommandContext::new(), request)
}

/// Searches graph nodes and refs using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn search_workspace_graph_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphSearchRequest,
) -> Result<WorkspaceGraphSearchResult> {
    context
        .open_workspace(&request.workspace_path)?
        .search_workspace_graph(request.query, request.options)
}

/// Returns a graph path between two versions.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_path(request: WorkspaceGraphPairRequest) -> Result<WorkspaceGraphPath> {
    get_workspace_graph_path_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns a graph path between two versions using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_path_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphPairRequest,
) -> Result<WorkspaceGraphPath> {
    let from = parse_version_id(request.from_version_id)?;
    let to = parse_version_id(request.to_version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_path(&from, &to, request.options)
}

/// Returns the common ancestor of two versions.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_common_ancestor(
    request: WorkspaceGraphPairRequest,
) -> Result<WorkspaceGraphCommonAncestor> {
    get_workspace_graph_common_ancestor_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns the common ancestor of two versions using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_common_ancestor_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphPairRequest,
) -> Result<WorkspaceGraphCommonAncestor> {
    let from = parse_version_id(request.from_version_id)?;
    let to = parse_version_id(request.to_version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_common_ancestor(&from, &to)
}

/// Returns one graph node with refs and lightweight detail metadata.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_node(request: VersionRequest) -> Result<WorkspaceGraphNodeDetail> {
    get_workspace_graph_node_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns one graph node using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_node_with_context(
    context: &DraftlineCommandContext<'_>,
    request: VersionRequest,
) -> Result<WorkspaceGraphNodeDetail> {
    let version = parse_version_id(request.version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_node(
            &version,
            WorkspaceGraphOptions {
                include_remotes: true,
                include_support_refs: true,
                ..WorkspaceGraphOptions::default()
            },
        )
}

/// Returns compare metadata for two graph versions.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_compare_summary(
    request: WorkspaceGraphPairRequest,
) -> Result<WorkspaceGraphCompareSummary> {
    get_workspace_graph_compare_summary_with_context(&DraftlineCommandContext::new(), request)
}

/// Returns compare metadata using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn get_workspace_graph_compare_summary_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceGraphPairRequest,
) -> Result<WorkspaceGraphCompareSummary> {
    let from = parse_version_id(request.from_version_id)?;
    let to = parse_version_id(request.to_version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .workspace_graph_compare_summary(&from, &to)
}

/// Diffs two saved versions.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn diff_versions(request: DiffVersionsRequest) -> Result<VersionDiff> {
    diff_versions_with_context(&DraftlineCommandContext::new(), request)
}

/// Diffs two saved versions using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn diff_versions_with_context(
    context: &DraftlineCommandContext<'_>,
    request: DiffVersionsRequest,
) -> Result<VersionDiff> {
    let from = parse_version_id(request.from_version_id)?;
    let to = parse_version_id(request.to_version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .diff_versions(&from, &to)
}

/// Diffs a saved version against the live workspace.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn diff_version_to_workspace(request: VersionRequest) -> Result<VersionDiff> {
    diff_version_to_workspace_with_context(&DraftlineCommandContext::new(), request)
}

/// Diffs a saved version against the live workspace using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn diff_version_to_workspace_with_context(
    context: &DraftlineCommandContext<'_>,
    request: VersionRequest,
) -> Result<VersionDiff> {
    let version = parse_version_id(request.version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .diff_version_to_workspace(&version)
}

/// Previews a saved version without mutating the workspace.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn preview_version(request: VersionRequest) -> Result<VersionPreview> {
    preview_version_with_context(&DraftlineCommandContext::new(), request)
}

/// Previews a saved version using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn preview_version_with_context(
    context: &DraftlineCommandContext<'_>,
    request: VersionRequest,
) -> Result<VersionPreview> {
    let version = parse_version_id(request.version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .preview_version(&version)
}

/// Previews one file from a saved version without mutating the workspace.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), path = %request.path.display())
)]
pub fn preview_version_file(request: PreviewVersionFileRequest) -> Result<Option<PreviewFile>> {
    preview_version_file_with_context(&DraftlineCommandContext::new(), request)
}

/// Previews one file from a saved version using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), path = %request.path.display())
)]
pub fn preview_version_file_with_context(
    context: &DraftlineCommandContext<'_>,
    request: PreviewVersionFileRequest,
) -> Result<Option<PreviewFile>> {
    let version = parse_version_id(request.version_id)?;
    context
        .open_workspace(&request.workspace_path)?
        .preview_version_file(&version, request.path)
}

/// Diffs one current workspace file against the current variation head.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), path = %request.path.display())
)]
pub fn diff_workspace_file(request: CurrentFileRequest) -> Result<Option<CurrentFileDiff>> {
    diff_workspace_file_with_context(&DraftlineCommandContext::new(), request)
}

/// Diffs one current workspace file using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), path = %request.path.display())
)]
pub fn diff_workspace_file_with_context(
    context: &DraftlineCommandContext<'_>,
    request: CurrentFileRequest,
) -> Result<Option<CurrentFileDiff>> {
    context
        .open_workspace(&request.workspace_path)?
        .diff_workspace_file(request.path)
}

/// Previews one current workspace file without mutating state.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), path = %request.path.display())
)]
pub fn preview_workspace_file(request: CurrentFileRequest) -> Result<Option<CurrentFilePreview>> {
    preview_workspace_file_with_context(&DraftlineCommandContext::new(), request)
}

/// Previews one current workspace file using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), path = %request.path.display())
)]
pub fn preview_workspace_file_with_context(
    context: &DraftlineCommandContext<'_>,
    request: CurrentFileRequest,
) -> Result<Option<CurrentFilePreview>> {
    context
        .open_workspace(&request.workspace_path)?
        .preview_workspace_file(request.path)
}

/// Restores a saved version as a new save.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn restore_version_as_new_save(request: RestoreVersionRequest) -> Result<RestoreVersionResult> {
    restore_version_as_new_save_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Restores a saved version as a new save using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn restore_version_as_new_save_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RestoreVersionRequest,
) -> Result<RestoreVersionResult> {
    let version_id = parse_version_id(request.version_id)?;
    let workspace = context.open_workspace(&request.workspace_path)?;
    let version = workspace.restore_version_as_new_save(&version_id, request.label)?;
    context.emit(&workspace, DraftlineEventKind::WorkspaceChanged, None);
    Ok(RestoreVersionResult {
        version,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Restores a saved version as a new save on a target variation.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn restore_version_as_new_save_to_variation(
    request: TargetedRestoreVersionRequest,
) -> Result<TargetedRestoreVersionCommandResult> {
    restore_version_as_new_save_to_variation_with_context(
        &mut DraftlineCommandContext::new(),
        request,
    )
}

/// Restores a saved version as a new save on a target variation using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn restore_version_as_new_save_to_variation_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: TargetedRestoreVersionRequest,
) -> Result<TargetedRestoreVersionCommandResult> {
    let version_id = parse_version_id(request.version_id)?;
    let workspace = context.open_workspace(&request.workspace_path)?;
    let result = workspace.restore_version_as_new_save_to_variation(
        &version_id,
        request.label,
        request.target,
    )?;
    context.emit(&workspace, DraftlineEventKind::HistoryChanged, None);
    Ok(TargetedRestoreVersionCommandResult {
        version: result.version,
        target_variation: result.target_variation,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Creates a new local variation from a saved version without switching to it.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        version_id = %request.version_id,
        variation = %request.name
    )
)]
pub fn create_variation_from_version(
    request: CreateVariationFromVersionRequest,
) -> Result<CreateVariationFromVersionResult> {
    create_variation_from_version_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Creates a new local variation from a saved version using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        version_id = %request.version_id,
        variation = %request.name
    )
)]
pub fn create_variation_from_version_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: CreateVariationFromVersionRequest,
) -> Result<CreateVariationFromVersionResult> {
    let version = parse_version_id(request.version_id)?;
    let workspace = context.open_workspace(&request.workspace_path)?;
    let variation =
        workspace.create_variation_from_with_metadata(&version, request.name, request.metadata)?;
    context.emit(&workspace, DraftlineEventKind::HistoryChanged, None);
    Ok(CreateVariationFromVersionResult {
        variation,
        postconditions: collect_postconditions(&workspace, false),
    })
}

/// Saves all tracked workspace changes as a new version.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn save(request: SaveRequest) -> Result<SaveResult> {
    save_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Saves all tracked workspace changes using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn save_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: SaveRequest,
) -> Result<SaveResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let version = if let Some(profile) = context.contributor_profile.as_ref() {
        workspace.save_version_with_profile(request.label, profile)?
    } else {
        workspace.save_version(request.label)?
    };
    context.emit(&workspace, DraftlineEventKind::HistoryChanged, None);
    Ok(SaveResult {
        version,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Lists local shelves.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_shelves(request: WorkspaceRequest) -> Result<Vec<Shelf>> {
    list_shelves_with_context(&DraftlineCommandContext::new(), request)
}

/// Lists local shelves using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_shelves_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<Vec<Shelf>> {
    context
        .open_workspace(&request.workspace_path)?
        .list_shelves()
}

/// Previews a shelf without mutating the workspace.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn preview_shelf(request: ShelfRequest) -> Result<VersionPreview> {
    preview_shelf_with_context(&DraftlineCommandContext::new(), request)
}

/// Previews a shelf using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn preview_shelf_with_context(
    context: &DraftlineCommandContext<'_>,
    request: ShelfRequest,
) -> Result<VersionPreview> {
    context
        .open_workspace(&request.workspace_path)?
        .preview_shelf(request.shelf_id)
}

/// Preflights applying a shelf without mutating the workspace.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn preflight_apply_shelf(request: ShelfRequest) -> Result<ShelfApplyReport> {
    preflight_apply_shelf_with_context(&DraftlineCommandContext::new(), request)
}

/// Preflights applying a shelf using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn preflight_apply_shelf_with_context(
    context: &DraftlineCommandContext<'_>,
    request: ShelfRequest,
) -> Result<ShelfApplyReport> {
    context
        .open_workspace(&request.workspace_path)?
        .preflight_apply_shelf(request.shelf_id)
}

/// Applies a shelf as workspace content and preserves the shelf.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn apply_shelf(request: ShelfRequest) -> Result<ApplyShelfCommandResult> {
    apply_shelf_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Applies a shelf using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn apply_shelf_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: ShelfRequest,
) -> Result<ApplyShelfCommandResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let preflight = workspace.preflight_apply_shelf(&request.shelf_id)?;
    let shelf = workspace.apply_shelf(request.shelf_id)?;
    context.emit(&workspace, DraftlineEventKind::DirtyChanged, None);
    Ok(ApplyShelfCommandResult {
        preflight,
        shelf,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Deletes a shelf.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn delete_shelf(request: ShelfRequest) -> Result<DeleteShelfResult> {
    delete_shelf_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Deletes a shelf using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn delete_shelf_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: ShelfRequest,
) -> Result<DeleteShelfResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    workspace.delete_shelf(request.shelf_id)?;
    context.emit(&workspace, DraftlineEventKind::WorkspaceChanged, None);
    Ok(DeleteShelfResult {
        postconditions: collect_postconditions(&workspace, false),
    })
}

/// Audits the configured content policy against Git metadata.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn audit_content_policy(request: WorkspaceRequest) -> Result<ContentPolicyAudit> {
    audit_content_policy_with_context(&DraftlineCommandContext::new(), request)
}

/// Audits the configured content policy using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn audit_content_policy_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<ContentPolicyAudit> {
    context
        .open_workspace(&request.workspace_path)?
        .audit_content_policy()
}

/// Clears a stale operation lock when Draftline marks it clearable.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn clear_stale_lock(request: WorkspaceRequest) -> Result<CommandPostconditions> {
    clear_stale_lock_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Clears a stale operation lock using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn clear_stale_lock_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<CommandPostconditions> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    workspace.clear_stale_lock()?;
    context.emit(&workspace, DraftlineEventKind::OperationLockChanged, None);
    Ok(collect_postconditions(&workspace, false))
}

/// Repairs an interrupted operation.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn repair_recovery(request: RecoveryRequest) -> Result<RecoveryRepairResult> {
    repair_recovery_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Repairs an interrupted operation using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn repair_recovery_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RecoveryRequest,
) -> Result<RecoveryRepairResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let result = {
        let mut options = context.remote_options();
        workspace.repair_recovery_with_options(request.operation_id, &mut options)?
    };
    context.emit(&workspace, DraftlineEventKind::RecoveryRequired, None);
    Ok(result)
}

/// Rolls back an interrupted operation when possible.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn rollback_recovery(request: RecoveryRequest) -> Result<RecoveryRepairResult> {
    rollback_recovery_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Rolls back an interrupted operation using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display())
)]
pub fn rollback_recovery_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RecoveryRequest,
) -> Result<RecoveryRepairResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let result = workspace.rollback_recovery(request.operation_id)?;
    context.emit(&workspace, DraftlineEventKind::RecoveryRequired, None);
    Ok(result)
}

/// Saves selected files after preflighting the exact selection.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_save(request: SelectedSaveRequest) -> Result<SelectedSaveResult> {
    selected_save_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Saves selected files using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_save_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: SelectedSaveRequest,
) -> Result<SelectedSaveResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let preflight = workspace.preflight_save_files(request.paths.clone())?;
    if !preflight.can_proceed {
        return Err(DraftlineError::PreflightFailed(Box::new(preflight)));
    }

    let version = if let Some(profile) = context.contributor_profile.as_ref() {
        workspace.save_files_with_profile(request.paths, request.label, profile)?
    } else {
        workspace.save_files(request.paths, request.label)?
    };
    context.emit(&workspace, DraftlineEventKind::HistoryChanged, None);
    Ok(SelectedSaveResult {
        preflight,
        version,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Shelves selected files after preflighting the exact selection.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_shelve(request: SelectedShelveRequest) -> Result<SelectedShelveResult> {
    selected_shelve_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Shelves selected files using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_shelve_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: SelectedShelveRequest,
) -> Result<SelectedShelveResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let preflight = workspace.preflight_shelve_files(&request.name, request.paths.clone())?;
    if !preflight.can_proceed {
        return Err(DraftlineError::PreflightFailed(Box::new(preflight)));
    }

    let shelf = workspace.shelve_files(request.name, request.paths)?;
    context.emit(&workspace, DraftlineEventKind::DirtyChanged, None);
    Ok(SelectedShelveResult {
        preflight,
        shelf,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Discards selected files after preflighting the exact selection.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_discard(request: SelectedDiscardRequest) -> Result<SelectedDiscardResult> {
    selected_discard_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Discards selected files using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_discard_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: SelectedDiscardRequest,
) -> Result<SelectedDiscardResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let preflight = workspace.preflight_discard_files(request.paths.clone())?;
    if !preflight.can_proceed {
        return Err(DraftlineError::PreflightFailed(Box::new(preflight)));
    }

    let discarded = workspace.discard_files(request.paths)?;
    context.emit(&workspace, DraftlineEventKind::DirtyChanged, None);
    Ok(SelectedDiscardResult {
        preflight,
        discarded,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Publishes the current variation through the tokenized publish path.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn publish_current_variation(
    request: PublishCurrentVariationRequest,
) -> Result<PublishCurrentVariationResult> {
    publish_current_variation_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Publishes using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn publish_current_variation_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: PublishCurrentVariationRequest,
) -> Result<PublishCurrentVariationResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let (preflight, publish) = {
        let mut options = context.remote_options();
        let preflight = workspace.preflight_publish_with_options(&request.remote, &mut options)?;
        if !preflight.can_publish {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(
                preflight.sync_status.clone(),
            )));
        }

        let publish = workspace.publish_with_options(preflight.token.clone(), &mut options)?;
        (preflight, publish)
    };
    context.emit(
        &workspace,
        DraftlineEventKind::SyncChanged,
        Some(preflight.sync_status.clone()),
    );
    Ok(PublishCurrentVariationResult {
        preflight,
        publish,
        postconditions: collect_postconditions(&workspace, false),
    })
}

/// Fetches remote refs and returns the resulting collaboration status.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn fetch_remote(request: RemoteRequest) -> Result<FetchRemoteResult> {
    fetch_remote_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Fetches remote refs using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn fetch_remote_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteRequest,
) -> Result<FetchRemoteResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    {
        let mut options = context.remote_options();
        workspace.fetch_remote_with_options(&request.remote, &mut options)?;
    }
    let sync_status = workspace.sync_status(&request.remote)?;
    context.emit(
        &workspace,
        DraftlineEventKind::SyncChanged,
        Some(sync_status.clone()),
    );
    Ok(FetchRemoteResult {
        sync_status,
        postconditions: collect_postconditions(&workspace, false),
    })
}

/// Lists configured remotes for the workspace.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_remotes(request: WorkspaceRequest) -> Result<Vec<RemoteEndpoint>> {
    list_remotes_with_context(&DraftlineCommandContext::new(), request)
}

/// Lists configured remotes using a configured host context.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_remotes_with_context(
    context: &DraftlineCommandContext<'_>,
    request: WorkspaceRequest,
) -> Result<Vec<RemoteEndpoint>> {
    context.open_workspace(&request.workspace_path)?.remotes()
}

/// Fetches and lists visible remote variations.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn list_remote_variations(request: RemoteRequest) -> Result<Vec<RemoteVariation>> {
    list_remote_variations_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Fetches and lists visible remote variations using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn list_remote_variations_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteRequest,
) -> Result<Vec<RemoteVariation>> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    {
        let mut options = context.remote_options();
        workspace.fetch_all_variations_with_options(&request.remote, &mut options)?;
    }
    workspace.remote_variations(&request.remote)
}

/// Fetches and compares local and remote-tracking variations.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn remote_variation_diagnostics(request: RemoteRequest) -> Result<RemoteVariationDiagnostics> {
    remote_variation_diagnostics_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Fetches and compares variations using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn remote_variation_diagnostics_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteRequest,
) -> Result<RemoteVariationDiagnostics> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    {
        let mut options = context.remote_options();
        workspace.fetch_all_variations_with_options(&request.remote, &mut options)?;
    }
    workspace.remote_variation_diagnostics(&request.remote)
}

/// Adopts a fetched remote-tracking variation as a local Draftline variation.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        remote = %request.remote,
        variation_id = %request.variation_id
    )
)]
pub fn adopt_remote_variation(
    request: RemoteVariationRequest,
) -> Result<AdoptRemoteVariationResult> {
    adopt_remote_variation_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Adopts a fetched remote-tracking variation using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(
        workspace_path = %request.workspace_path.display(),
        remote = %request.remote,
        variation_id = %request.variation_id
    )
)]
pub fn adopt_remote_variation_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteVariationRequest,
) -> Result<AdoptRemoteVariationResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    {
        let mut options = context.remote_options();
        workspace.fetch_all_variations_with_options(&request.remote, &mut options)?;
    }
    let variation = workspace
        .adopt_remote_variation(&request.remote, &VariationId::from(request.variation_id))?;
    context.emit(&workspace, DraftlineEventKind::WorkspaceChanged, None);
    Ok(AdoptRemoteVariationResult {
        variation,
        postconditions: collect_postconditions(&workspace, false),
    })
}

/// Preflights a fast-forward incoming apply without mutating workspace state.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn preflight_apply_incoming(request: RemoteRequest) -> Result<ApplyIncomingReport> {
    preflight_apply_incoming_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Preflights incoming apply using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn preflight_apply_incoming_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteRequest,
) -> Result<ApplyIncomingReport> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let mut options = context.remote_options();
    workspace.fetch_remote_with_options(&request.remote, &mut options)?;
    workspace.preflight_apply_incoming(&request.remote)
}

/// Applies incoming fast-forward work after preflighting the current remote state.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn apply_incoming(request: RemoteRequest) -> Result<ApplyIncomingCommandResult> {
    apply_incoming_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Applies incoming fast-forward work using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn apply_incoming_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteRequest,
) -> Result<ApplyIncomingCommandResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let (preflight, apply) = {
        let mut options = context.remote_options();
        workspace.fetch_remote_with_options(&request.remote, &mut options)?;
        let preflight = workspace.preflight_apply_incoming(&request.remote)?;
        let apply = workspace.apply_incoming(&request.remote, &mut options)?;
        (preflight, apply)
    };
    context.emit(&workspace, DraftlineEventKind::HistoryChanged, None);
    Ok(ApplyIncomingCommandResult {
        preflight,
        apply,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Preflights an incoming merge without mutating workspace state.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn preflight_merge_incoming(request: RemoteRequest) -> Result<MergeIncomingReport> {
    preflight_merge_incoming_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Preflights incoming merge using a configured host context.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn preflight_merge_incoming_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: RemoteRequest,
) -> Result<MergeIncomingReport> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let mut options = context.remote_options();
    workspace.fetch_remote_with_options(&request.remote, &mut options)?;
    workspace.preflight_merge_incoming(&request.remote)
}

/// Writes a clean incoming merge through Draftline's tokenized merge path.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn merge_incoming(request: MergeIncomingRequest) -> Result<MergeIncomingCommandResult> {
    merge_incoming_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Writes a clean incoming merge using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn merge_incoming_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: MergeIncomingRequest,
) -> Result<MergeIncomingCommandResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let contributor_profile = context.contributor_profile.clone();
    let (preflight, merge) = {
        let mut options = context.remote_options();
        workspace.fetch_remote_with_options(&request.remote, &mut options)?;
        let preflight = workspace.preflight_merge_incoming(&request.remote)?;
        let Some(token) = preflight.token.clone() else {
            return Err(merge_preflight_error(preflight));
        };
        let merge = match contributor_profile.as_ref() {
            Some(profile) => workspace.merge_incoming_with_profile(
                token,
                request.label,
                &mut options,
                profile,
            )?,
            None => workspace.merge_incoming(token, request.label, &mut options)?,
        };
        (preflight, merge)
    };
    context.emit(&workspace, DraftlineEventKind::HistoryChanged, None);
    Ok(MergeIncomingCommandResult {
        preflight,
        merge,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Writes an incoming merge using explicit user-provided conflict resolutions.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn merge_incoming_with_resolutions(
    request: MergeIncomingWithResolutionsRequest,
) -> Result<MergeIncomingCommandResult> {
    merge_incoming_with_resolutions_with_context(&mut DraftlineCommandContext::new(), request)
}

/// Writes an incoming merge with resolutions using a configured host context.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn merge_incoming_with_resolutions_with_context(
    context: &mut DraftlineCommandContext<'_>,
    request: MergeIncomingWithResolutionsRequest,
) -> Result<MergeIncomingCommandResult> {
    let workspace = context.open_workspace(&request.workspace_path)?;
    let contributor_profile = context.contributor_profile.clone();
    let (preflight, merge) = {
        let mut options = context.remote_options();
        let preflight = workspace.preflight_merge_incoming(&request.remote)?;
        if preflight.token.as_ref() != Some(&request.token) {
            return Err(merge_preflight_error(preflight));
        }
        let merge = match contributor_profile.as_ref() {
            Some(profile) => workspace.merge_incoming_with_resolutions_and_profile(
                request.token,
                request.label,
                request.resolutions,
                &mut options,
                profile,
            )?,
            None => workspace.merge_incoming_with_resolutions(
                request.token,
                request.label,
                request.resolutions,
                &mut options,
            )?,
        };
        (preflight, merge)
    };
    context.emit(
        &workspace,
        DraftlineEventKind::HistoryChanged,
        Some(preflight.sync_status.clone()),
    );
    Ok(MergeIncomingCommandResult {
        preflight,
        merge,
        postconditions: collect_postconditions(&workspace, true),
    })
}

/// Converts a merge preflight report into grouped conflict UI data.
pub fn merge_conflict_view_model(report: &MergeIncomingReport) -> MergeConflictViewModel {
    let mut files = BTreeMap::<PathBuf, MergeFileConflictGroup>::new();

    for conflict in &report.conflicts {
        let entry = files
            .entry(conflict.path.clone())
            .or_insert_with(|| MergeFileConflictGroup {
                path: conflict.path.clone(),
                label: conflict.path.display().to_string(),
                whole_file_conflicts: Vec::new(),
                field_conflicts: Vec::new(),
            });
        let item = merge_conflict_item(conflict);
        if let Some(field_path) = conflict.field_path.as_ref() {
            if let Some(group) = entry
                .field_conflicts
                .iter_mut()
                .find(|group| group.field_path == *field_path)
            {
                group.conflicts.push(item);
            } else {
                entry.field_conflicts.push(MergeFieldConflictGroup {
                    field_path: field_path.clone(),
                    label: conflict.label.clone(),
                    conflicts: vec![item],
                });
            }
        } else {
            entry.whole_file_conflicts.push(item);
        }
    }

    MergeConflictViewModel {
        files: files.into_values().collect(),
        token: report.token.clone(),
        can_merge_cleanly: report.can_merge_cleanly,
    }
}

/// Builds explicit whole-file `use_content` resolutions from preflight conflict snapshots.
///
/// The helper intentionally snapshots the selected side into `UseContent` instead
/// of returning symbolic choices, so stale tokens cannot silently resolve content
/// the user did not review.
pub fn whole_file_use_content_resolutions(
    report: &MergeIncomingReport,
    source: ConflictContentSource,
) -> Vec<MergeConflictResolution> {
    report
        .conflicts
        .iter()
        .filter(|conflict| conflict.field_path.is_none())
        .filter_map(|conflict| {
            conflict_content(conflict, source).map(|content| MergeConflictResolution {
                path: conflict.path.clone(),
                field_path: None,
                choice: MergeResolutionChoice::UseContent { content },
            })
        })
        .collect()
}

fn merge_conflict_item(conflict: &MergeConflict) -> MergeConflictItem {
    MergeConflictItem {
        path: conflict.path.clone(),
        field_path: conflict.field_path.clone(),
        label: conflict.label.clone(),
        base: conflict.base.clone(),
        ours: conflict.ours.clone(),
        theirs: conflict.theirs.clone(),
        resolution: conflict.resolution.clone(),
    }
}

fn conflict_content(conflict: &MergeConflict, source: ConflictContentSource) -> Option<String> {
    match source {
        ConflictContentSource::Ours => conflict.ours.clone(),
        ConflictContentSource::Theirs => conflict.theirs.clone(),
        ConflictContentSource::Base => conflict.base.clone(),
    }
}

fn merge_preflight_error(preflight: MergeIncomingReport) -> DraftlineError {
    if matches!(preflight.sync_status.state, SyncState::NeedsMerge) {
        return DraftlineError::PreflightFailed(Box::new(PreflightReport {
            operation: "merge_incoming".to_string(),
            will_write_files: true,
            dirty_files: preflight.dirty_files,
            file_hazards: preflight.file_hazards,
            untracked_assets: Vec::new(),
            unresolved_conflicts: preflight
                .conflicts
                .into_iter()
                .map(|conflict| conflict.path)
                .collect(),
            large_files: Vec::new(),
            binary_files: Vec::new(),
            variation_divergence: Some("incoming merge is blocked".to_string()),
            can_proceed: false,
        }));
    }

    DraftlineError::SyncNeedsMerge(Box::new(preflight.sync_status))
}

/// Converts a Draftline result into a Tauri-serializable result.
pub fn into_tauri_result<T>(result: Result<T>) -> TauriCommandResult<T> {
    result.map_err(TauriCommandError::from)
}

fn parse_version_id(version_id: String) -> Result<VersionId> {
    VersionId::from_canonical_string(version_id)
}

fn workspace_diagnostics(workspace: &Workspace) -> Result<WorkspaceDiagnostics> {
    Ok(WorkspaceDiagnostics {
        summary: workspace.workspace_summary()?,
        inspection: workspace.inspect()?,
        verification: workspace.verify_workspace()?,
        operation_lock: workspace.inspect_operation_lock()?,
    })
}

fn collect_postconditions(workspace: &Workspace, include_changes: bool) -> CommandPostconditions {
    let mut errors = Vec::new();
    let remaining_changes = if include_changes {
        match workspace.changes() {
            Ok(changes) => Some(changes),
            Err(error) => {
                errors.push(TauriCommandError::from(error));
                None
            }
        }
    } else {
        None
    };
    let verification = match workspace.verify_workspace() {
        Ok(verification) => Some(verification),
        Err(error) => {
            errors.push(TauriCommandError::from(error));
            None
        }
    };

    if !errors.is_empty() {
        tracing::warn!(
            postcondition_error_count = errors.len(),
            "mutating command succeeded but postcondition collection failed"
        );
    }

    CommandPostconditions {
        remaining_changes,
        verification,
        errors,
    }
}

fn draftline_error_code(error: &DraftlineError) -> &'static str {
    match error {
        DraftlineError::Git(_) => "git",
        DraftlineError::Io(_) => "io",
        DraftlineError::Json(_) => "json",
        DraftlineError::PathEscapesWorkspace(_) => "path_escapes_workspace",
        DraftlineError::AbsolutePath(_) => "absolute_path",
        DraftlineError::WorkspaceDirty => "dirty_workspace",
        DraftlineError::PreflightFailed(_) => "preflight_failed",
        DraftlineError::RecoveryRequired(_) => "recovery_required",
        DraftlineError::RecoveryNotFound(_) => "recovery_not_found",
        DraftlineError::InvalidVariationName(_) => "invalid_variation_name",
        DraftlineError::InvalidGraphOptions(_) => "invalid_graph_options",
        DraftlineError::VariationAlreadyExists(_) => "variation_already_exists",
        DraftlineError::VariationNotFound(_) => "variation_not_found",
        DraftlineError::CannotDeleteCurrentVariation(_) => "cannot_delete_current_variation",
        DraftlineError::VersionNotFound(_) => "version_not_found",
        DraftlineError::VersionNotLocallyReachable(_) => "version_not_locally_reachable",
        DraftlineError::NoCurrentVariation => "no_current_variation",
        DraftlineError::WorkspaceLocked => "workspace_locked",
        DraftlineError::UnsupportedSwitchPolicy(_) => "unsupported_switch_policy",
        DraftlineError::InvalidContentPolicyPath(_) => "invalid_content_policy_path",
        DraftlineError::InvalidContentPolicyExtension(_) => "invalid_content_policy_extension",
        DraftlineError::PathOutsideContentPolicy(_) => "path_outside_content_policy",
        DraftlineError::SyncNeedsMerge(_) => "sync_needs_merge",
        DraftlineError::RemoteRace { .. } => "remote_race",
        DraftlineError::LocalStateChanged { .. } => "local_state_changed",
        DraftlineError::UnsupportedRemoteTransport { .. } => "unsupported_remote_transport",
        DraftlineError::ConsentRequired(_) => "consent_required",
        DraftlineError::InvalidSquashCount(_) => "invalid_squash_count",
        DraftlineError::NotEnoughVersionsToSquash { .. } => "not_enough_versions_to_squash",
        DraftlineError::InvalidContributorIdentity(_) => "invalid_contributor_identity",
        DraftlineError::InvalidMergeResolution(_) => "invalid_merge_resolution",
    }
}

fn draftline_error_details(error: &DraftlineError) -> Option<serde_json::Value> {
    match error {
        DraftlineError::PreflightFailed(report) => match serde_json::to_value(report.as_ref()) {
            Ok(value) => Some(value),
            Err(error) => Some(serde_json::json!({
                "serialization_error": error.to_string(),
            })),
        },
        DraftlineError::RecoveryRequired(state) => match serde_json::to_value(state.as_ref()) {
            Ok(value) => Some(value),
            Err(error) => Some(serde_json::json!({
                "serialization_error": error.to_string(),
            })),
        },
        DraftlineError::SyncNeedsMerge(status) => match serde_json::to_value(status.as_ref()) {
            Ok(value) => Some(value),
            Err(error) => Some(serde_json::json!({
                "serialization_error": error.to_string(),
            })),
        },
        _ => None,
    }
}
