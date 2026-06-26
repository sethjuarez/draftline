//! Dependency-free command adapter for Tauri Workbench hosts.
//!
//! The functions in this module intentionally do not depend on Tauri. A host can
//! expose them behind `#[tauri::command]` wrappers while preserving Draftline's
//! existing preflight, recovery, and verification semantics.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ApplyIncomingReport, ApplyIncomingResult, ChangeSet, DraftlineError, MergeIncomingReport,
    MergeIncomingResult, OperationLockInspection, PreflightReport, PublishPreflight, PublishResult,
    RemoteOptions, Result, Shelf, SupportRef, SupportRefScope, SyncState, SyncStatus,
    VariationSummary, Version, Workspace, WorkspaceInspection, WorkspaceSummary,
    WorkspaceVerification,
};

/// Common request shape for read-only workspace diagnostics commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    pub workspace_path: PathBuf,
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

/// Merge result with the preflight and postcondition state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeIncomingCommandResult {
    pub preflight: MergeIncomingReport,
    pub merge: MergeIncomingResult,
    pub postconditions: CommandPostconditions,
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

/// Returns a full diagnostics payload for the workspace dashboard.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn inspect_workspace(request: WorkspaceRequest) -> Result<WorkspaceDiagnostics> {
    let workspace = Workspace::open(&request.workspace_path)?;
    Ok(WorkspaceDiagnostics {
        summary: workspace.workspace_summary()?,
        inspection: workspace.inspect()?,
        verification: workspace.verify_workspace()?,
        operation_lock: workspace.inspect_operation_lock()?,
    })
}

/// Returns the workspace verification payload used by smoke postconditions.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn verify_workspace(request: WorkspaceRequest) -> Result<WorkspaceVerification> {
    Workspace::open(&request.workspace_path)?.verify_workspace()
}

/// Returns per-variation summaries for a variation switcher.
#[tracing::instrument(err, skip_all, fields(workspace_path = %request.workspace_path.display()))]
pub fn list_variations(request: WorkspaceRequest) -> Result<Vec<VariationSummary>> {
    Workspace::open(&request.workspace_path)?.variation_summaries()
}

/// Returns hidden recovery support refs for admin/recovery views.
#[tracing::instrument(
    err,
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), scope = ?request.scope)
)]
pub fn list_support_refs(request: ListSupportRefsRequest) -> Result<Vec<SupportRef>> {
    Workspace::open(&request.workspace_path)?.list_support_refs(request.scope)
}

/// Saves selected files after preflighting the exact selection.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), selected_count = request.paths.len())
)]
pub fn selected_save(request: SelectedSaveRequest) -> Result<SelectedSaveResult> {
    let workspace = Workspace::open(&request.workspace_path)?;
    let preflight = workspace.preflight_save_files(request.paths.clone())?;
    if !preflight.can_proceed {
        return Err(DraftlineError::PreflightFailed(Box::new(preflight)));
    }

    let version = workspace.save_files(request.paths, request.label)?;
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
    let workspace = Workspace::open(&request.workspace_path)?;
    let preflight = workspace.preflight_shelve_files(&request.name, request.paths.clone())?;
    if !preflight.can_proceed {
        return Err(DraftlineError::PreflightFailed(Box::new(preflight)));
    }

    let shelf = workspace.shelve_files(request.name, request.paths)?;
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
    let workspace = Workspace::open(&request.workspace_path)?;
    let preflight = workspace.preflight_discard_files(request.paths.clone())?;
    if !preflight.can_proceed {
        return Err(DraftlineError::PreflightFailed(Box::new(preflight)));
    }

    let discarded = workspace.discard_files(request.paths)?;
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
    let workspace = Workspace::open(&request.workspace_path)?;
    let preflight = workspace.preflight_publish(&request.remote)?;
    if !preflight.can_publish {
        return Err(DraftlineError::SyncNeedsMerge(Box::new(
            preflight.sync_status.clone(),
        )));
    }

    let publish = workspace.publish(preflight.token.clone())?;
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
    let workspace = Workspace::open(&request.workspace_path)?;
    workspace.fetch_remote(&request.remote)?;
    Ok(FetchRemoteResult {
        sync_status: workspace.sync_status(&request.remote)?,
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
    let workspace = Workspace::open(&request.workspace_path)?;
    workspace.fetch_remote(&request.remote)?;
    workspace.preflight_apply_incoming(&request.remote)
}

/// Applies incoming fast-forward work after preflighting the current remote state.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn apply_incoming(request: RemoteRequest) -> Result<ApplyIncomingCommandResult> {
    let workspace = Workspace::open(&request.workspace_path)?;
    workspace.fetch_remote(&request.remote)?;
    let preflight = workspace.preflight_apply_incoming(&request.remote)?;
    let apply = workspace.apply_incoming(&request.remote, &mut RemoteOptions::new())?;
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
    let workspace = Workspace::open(&request.workspace_path)?;
    workspace.fetch_remote(&request.remote)?;
    workspace.preflight_merge_incoming(&request.remote)
}

/// Writes a clean incoming merge through Draftline's tokenized merge path.
#[tracing::instrument(
    err(level = tracing::Level::WARN),
    skip_all,
    fields(workspace_path = %request.workspace_path.display(), remote = %request.remote)
)]
pub fn merge_incoming(request: MergeIncomingRequest) -> Result<MergeIncomingCommandResult> {
    let workspace = Workspace::open(&request.workspace_path)?;
    workspace.fetch_remote(&request.remote)?;
    let preflight = workspace.preflight_merge_incoming(&request.remote)?;
    let Some(token) = preflight.token.clone() else {
        return Err(merge_preflight_error(preflight));
    };
    let merge = workspace.merge_incoming(token, request.label, &mut RemoteOptions::new())?;
    Ok(MergeIncomingCommandResult {
        preflight,
        merge,
        postconditions: collect_postconditions(&workspace, true),
    })
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
        DraftlineError::CannotDeleteCurrentVariation(_) => "cannot_delete_current_variation",
        DraftlineError::VersionNotFound(_) => "version_not_found",
        DraftlineError::NoCurrentVariation => "no_current_variation",
        DraftlineError::WorkspaceLocked => "workspace_locked",
        DraftlineError::UnsupportedSwitchPolicy(_) => "unsupported_switch_policy",
        DraftlineError::InvalidContentPolicyPath(_) => "invalid_content_policy_path",
        DraftlineError::InvalidContentPolicyExtension(_) => "invalid_content_policy_extension",
        DraftlineError::PathOutsideContentPolicy(_) => "path_outside_content_policy",
        DraftlineError::SyncNeedsMerge(_) => "sync_needs_merge",
        DraftlineError::RemoteRace { .. } => "remote_race",
        DraftlineError::LocalStateChanged { .. } => "local_state_changed",
        DraftlineError::ConsentRequired(_) => "consent_required",
        DraftlineError::InvalidSquashCount(_) => "invalid_squash_count",
        DraftlineError::NotEnoughVersionsToSquash { .. } => "not_enough_versions_to_squash",
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
