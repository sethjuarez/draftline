use draftline::tauri_contract as contract;
use draftline::tauri_contract::{
    AdoptRemoteVariationResult, AdoptWorkspaceResult, ApplyIncomingCommandResult,
    ApplyShelfCommandResult, CloneWorkspaceRequest, CommandPostconditions,
    CreateVariationFromVersionRequest, CreateVariationFromVersionResult, CurrentFileRequest,
    DeleteShelfResult, DiffVersionsRequest, FetchRemoteResult, ListSupportRefsRequest,
    MergeIncomingCommandResult, MergeIncomingRequest, MergeIncomingWithResolutionsRequest,
    PreviewVersionFileRequest, PublishCurrentVariationRequest, PublishCurrentVariationResult,
    RecoveryRequest, RemoteRequest, RemoteVariationRequest, RestoreVersionRequest,
    RestoreVersionResult, SaveRequest, SaveResult, SelectedDiscardRequest, SelectedDiscardResult,
    SelectedSaveRequest, SelectedSaveResult, SelectedShelveRequest, SelectedShelveResult,
    ShelfRequest, TargetedRestoreVersionCommandResult, TargetedRestoreVersionRequest,
    TauriCommandError, TauriCommandResult, VersionRequest, WorkspaceDiagnostics,
    WorkspaceGraphAroundVersionRequest, WorkspaceGraphNeighborhoodRequest,
    WorkspaceGraphOverviewRequest, WorkspaceGraphPairRequest, WorkspaceGraphRequest,
    WorkspaceGraphSearchRequest, WorkspaceGraphVariationRequest, WorkspaceOpenResult,
    WorkspaceRequest,
};
use draftline::{
    ApplyIncomingReport, ChangeSet, ContentPolicyAudit, CurrentFileDiff, CurrentFilePreview,
    HistoryEntry, MergeIncomingReport, PreviewFile, RecoveryRepairResult, RemoteEndpoint,
    RemoteVariation, RemoteVariationDiagnostics, Shelf, ShelfApplyReport, SupportRef,
    VariationSummary, VersionDiff, VersionPreview, WorkspaceGraph, WorkspaceGraphAgentSummary,
    WorkspaceGraphCommonAncestor, WorkspaceGraphCompareSummary, WorkspaceGraphNodeDetail,
    WorkspaceGraphPath, WorkspaceGraphRefs, WorkspaceGraphSearchResult, WorkspaceGraphSummary,
    WorkspaceVerification,
};
use std::sync::{Mutex, MutexGuard};
use tauri::{Emitter, Manager};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

type DraftlineContextState = Mutex<contract::DraftlineCommandContext<'static>>;

fn lock_context(
    state: &DraftlineContextState,
) -> TauriCommandResult<MutexGuard<'_, contract::DraftlineCommandContext<'static>>> {
    state.lock().map_err(|_| TauriCommandError {
        code: "command_context_locked".to_string(),
        message: "Draftline command context lock is poisoned".to_string(),
        details: None,
    })
}

#[tauri_plugin_auditaur::auditaur_command]
fn inspect_workspace(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<WorkspaceDiagnostics> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::inspect_workspace_with_context(&context, request))
}

#[tauri::command]
fn open_workspace(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<WorkspaceOpenResult> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::open_workspace_with_context(&context, request))
}

#[tauri::command]
fn clone_workspace(
    state: tauri::State<'_, DraftlineContextState>,
    request: CloneWorkspaceRequest,
) -> TauriCommandResult<WorkspaceOpenResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::clone_workspace_with_context(&mut context, request))
}

#[tauri::command]
fn adopt_workspace(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<AdoptWorkspaceResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::adopt_workspace_with_context(&mut context, request))
}

#[tauri::command]
fn verify_workspace(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<WorkspaceVerification> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::verify_workspace_with_context(&context, request))
}

#[tauri::command]
fn list_variations(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<Vec<VariationSummary>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::list_variations_with_context(&context, request))
}

#[tauri_plugin_auditaur::auditaur_command]
fn list_support_refs(
    state: tauri::State<'_, DraftlineContextState>,
    request: ListSupportRefsRequest,
) -> TauriCommandResult<Vec<SupportRef>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::list_support_refs_with_context(&context, request))
}

#[tauri::command]
fn list_remotes(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<Vec<RemoteEndpoint>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::list_remotes_with_context(&context, request))
}

#[tauri::command]
fn list_remote_variations(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteRequest,
) -> TauriCommandResult<Vec<RemoteVariation>> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::list_remote_variations_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn remote_variation_diagnostics(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteRequest,
) -> TauriCommandResult<RemoteVariationDiagnostics> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::remote_variation_diagnostics_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn adopt_remote_variation(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteVariationRequest,
) -> TauriCommandResult<AdoptRemoteVariationResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::adopt_remote_variation_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn get_changes(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<ChangeSet> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_changes_with_context(&context, request))
}

#[tauri::command]
fn get_history(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<Vec<HistoryEntry>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_history_with_context(&context, request))
}

#[tauri::command]
fn get_full_history(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<Vec<HistoryEntry>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_full_history_with_context(&context, request))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphRequest,
) -> TauriCommandResult<WorkspaceGraph> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_with_context(&context, request))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_refs(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphRequest,
) -> TauriCommandResult<WorkspaceGraphRefs> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_refs_with_context(&context, request))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_summary(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphRequest,
) -> TauriCommandResult<WorkspaceGraphSummary> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_summary_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_overview(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphOverviewRequest,
) -> TauriCommandResult<WorkspaceGraph> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_overview_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_around_version(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphAroundVersionRequest,
) -> TauriCommandResult<WorkspaceGraph> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_around_version_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_for_variation(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphVariationRequest,
) -> TauriCommandResult<WorkspaceGraph> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_for_variation_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_agent_summary(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphRequest,
) -> TauriCommandResult<WorkspaceGraphAgentSummary> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_agent_summary_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_neighborhood(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphNeighborhoodRequest,
) -> TauriCommandResult<WorkspaceGraph> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_neighborhood_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn search_workspace_graph(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphSearchRequest,
) -> TauriCommandResult<WorkspaceGraphSearchResult> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::search_workspace_graph_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_path(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphPairRequest,
) -> TauriCommandResult<WorkspaceGraphPath> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_path_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_common_ancestor(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphPairRequest,
) -> TauriCommandResult<WorkspaceGraphCommonAncestor> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_common_ancestor_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_node(
    state: tauri::State<'_, DraftlineContextState>,
    request: VersionRequest,
) -> TauriCommandResult<WorkspaceGraphNodeDetail> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_node_with_context(
        &context, request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn get_workspace_graph_compare_summary(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceGraphPairRequest,
) -> TauriCommandResult<WorkspaceGraphCompareSummary> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::get_workspace_graph_compare_summary_with_context(
        &context, request,
    ))
}

#[tauri::command]
fn diff_versions(
    state: tauri::State<'_, DraftlineContextState>,
    request: DiffVersionsRequest,
) -> TauriCommandResult<VersionDiff> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::diff_versions_with_context(&context, request))
}

#[tauri::command]
fn diff_version_to_workspace(
    state: tauri::State<'_, DraftlineContextState>,
    request: VersionRequest,
) -> TauriCommandResult<VersionDiff> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::diff_version_to_workspace_with_context(
        &context, request,
    ))
}

#[tauri::command]
fn preview_version(
    state: tauri::State<'_, DraftlineContextState>,
    request: VersionRequest,
) -> TauriCommandResult<VersionPreview> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preview_version_with_context(&context, request))
}

#[tauri::command]
fn preview_version_file(
    state: tauri::State<'_, DraftlineContextState>,
    request: PreviewVersionFileRequest,
) -> TauriCommandResult<Option<PreviewFile>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preview_version_file_with_context(&context, request))
}

#[tauri::command]
fn diff_workspace_file(
    state: tauri::State<'_, DraftlineContextState>,
    request: CurrentFileRequest,
) -> TauriCommandResult<Option<CurrentFileDiff>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::diff_workspace_file_with_context(&context, request))
}

#[tauri::command]
fn preview_workspace_file(
    state: tauri::State<'_, DraftlineContextState>,
    request: CurrentFileRequest,
) -> TauriCommandResult<Option<CurrentFilePreview>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preview_workspace_file_with_context(
        &context, request,
    ))
}

#[tauri::command]
fn restore_version_as_new_save(
    state: tauri::State<'_, DraftlineContextState>,
    request: RestoreVersionRequest,
) -> TauriCommandResult<RestoreVersionResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::restore_version_as_new_save_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn restore_version_as_new_save_to_variation(
    state: tauri::State<'_, DraftlineContextState>,
    request: TargetedRestoreVersionRequest,
) -> TauriCommandResult<TargetedRestoreVersionCommandResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::restore_version_as_new_save_to_variation_with_context(
        &mut context,
        request,
    ))
}

#[tauri_plugin_auditaur::auditaur_command]
fn create_variation_from_version(
    state: tauri::State<'_, DraftlineContextState>,
    request: CreateVariationFromVersionRequest,
) -> TauriCommandResult<CreateVariationFromVersionResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::create_variation_from_version_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn save(
    state: tauri::State<'_, DraftlineContextState>,
    request: SaveRequest,
) -> TauriCommandResult<SaveResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::save_with_context(&mut context, request))
}

#[tauri::command]
fn list_shelves(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<Vec<Shelf>> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::list_shelves_with_context(&context, request))
}

#[tauri::command]
fn preview_shelf(
    state: tauri::State<'_, DraftlineContextState>,
    request: ShelfRequest,
) -> TauriCommandResult<VersionPreview> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preview_shelf_with_context(&context, request))
}

#[tauri::command]
fn preflight_apply_shelf(
    state: tauri::State<'_, DraftlineContextState>,
    request: ShelfRequest,
) -> TauriCommandResult<ShelfApplyReport> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preflight_apply_shelf_with_context(&context, request))
}

#[tauri::command]
fn apply_shelf(
    state: tauri::State<'_, DraftlineContextState>,
    request: ShelfRequest,
) -> TauriCommandResult<ApplyShelfCommandResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::apply_shelf_with_context(&mut context, request))
}

#[tauri::command]
fn delete_shelf(
    state: tauri::State<'_, DraftlineContextState>,
    request: ShelfRequest,
) -> TauriCommandResult<DeleteShelfResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::delete_shelf_with_context(&mut context, request))
}

#[tauri::command]
fn audit_content_policy(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<ContentPolicyAudit> {
    let context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::audit_content_policy_with_context(&context, request))
}

#[tauri::command]
fn clear_stale_lock(
    state: tauri::State<'_, DraftlineContextState>,
    request: WorkspaceRequest,
) -> TauriCommandResult<CommandPostconditions> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::clear_stale_lock_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn repair_recovery(
    state: tauri::State<'_, DraftlineContextState>,
    request: RecoveryRequest,
) -> TauriCommandResult<RecoveryRepairResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::repair_recovery_with_context(&mut context, request))
}

#[tauri::command]
fn rollback_recovery(
    state: tauri::State<'_, DraftlineContextState>,
    request: RecoveryRequest,
) -> TauriCommandResult<RecoveryRepairResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::rollback_recovery_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn selected_save(
    state: tauri::State<'_, DraftlineContextState>,
    request: SelectedSaveRequest,
) -> TauriCommandResult<SelectedSaveResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::selected_save_with_context(&mut context, request))
}

#[tauri::command]
fn selected_shelve(
    state: tauri::State<'_, DraftlineContextState>,
    request: SelectedShelveRequest,
) -> TauriCommandResult<SelectedShelveResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::selected_shelve_with_context(&mut context, request))
}

#[tauri::command]
fn selected_discard(
    state: tauri::State<'_, DraftlineContextState>,
    request: SelectedDiscardRequest,
) -> TauriCommandResult<SelectedDiscardResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::selected_discard_with_context(&mut context, request))
}

#[tauri::command]
fn publish_current_variation(
    state: tauri::State<'_, DraftlineContextState>,
    request: PublishCurrentVariationRequest,
) -> TauriCommandResult<PublishCurrentVariationResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::publish_current_variation_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn fetch_remote(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteRequest,
) -> TauriCommandResult<FetchRemoteResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::fetch_remote_with_context(&mut context, request))
}

#[tauri::command]
fn preflight_apply_incoming(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteRequest,
) -> TauriCommandResult<ApplyIncomingReport> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preflight_apply_incoming_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn apply_incoming(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteRequest,
) -> TauriCommandResult<ApplyIncomingCommandResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::apply_incoming_with_context(&mut context, request))
}

#[tauri::command]
fn preflight_merge_incoming(
    state: tauri::State<'_, DraftlineContextState>,
    request: RemoteRequest,
) -> TauriCommandResult<MergeIncomingReport> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::preflight_merge_incoming_with_context(
        &mut context,
        request,
    ))
}

#[tauri::command]
fn merge_incoming(
    state: tauri::State<'_, DraftlineContextState>,
    request: MergeIncomingRequest,
) -> TauriCommandResult<MergeIncomingCommandResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::merge_incoming_with_context(&mut context, request))
}

#[tauri::command]
fn merge_incoming_with_resolutions(
    state: tauri::State<'_, DraftlineContextState>,
    request: MergeIncomingWithResolutionsRequest,
) -> TauriCommandResult<MergeIncomingCommandResult> {
    let mut context = lock_context(state.inner())?;
    contract::into_tauri_result(contract::merge_incoming_with_resolutions_with_context(
        &mut context,
        request,
    ))
}

fn main() {
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
        )
        .with(tauri_plugin_auditaur::tracing_layer())
        .try_init();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_auditaur::Builder::new()
                .service_name("draftline-workbench")
                .session_name("workbench-dev")
                .build(),
        )
        .setup(|app| {
            let app_handle = app.handle().clone();
            app.manage(Mutex::new(
                contract::DraftlineCommandContext::new().with_event_sink(move |event| {
                    if let Err(error) = app_handle.emit("draftline://workspace_event", event) {
                        tracing::warn!(?error, "failed to emit Draftline workspace event");
                    }
                }),
            ));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_workspace,
            clone_workspace,
            adopt_workspace,
            inspect_workspace,
            verify_workspace,
            list_variations,
            list_support_refs,
            list_remotes,
            list_remote_variations,
            remote_variation_diagnostics,
            adopt_remote_variation,
            get_changes,
            get_history,
            get_full_history,
            get_workspace_graph,
            get_workspace_graph_refs,
            get_workspace_graph_summary,
            get_workspace_graph_overview,
            get_workspace_graph_around_version,
            get_workspace_graph_for_variation,
            get_workspace_graph_agent_summary,
            get_workspace_graph_neighborhood,
            search_workspace_graph,
            get_workspace_graph_path,
            get_workspace_graph_common_ancestor,
            get_workspace_graph_node,
            get_workspace_graph_compare_summary,
            diff_versions,
            diff_version_to_workspace,
            diff_workspace_file,
            preview_version,
            preview_version_file,
            preview_workspace_file,
            restore_version_as_new_save,
            restore_version_as_new_save_to_variation,
            create_variation_from_version,
            save,
            list_shelves,
            preview_shelf,
            preflight_apply_shelf,
            apply_shelf,
            delete_shelf,
            audit_content_policy,
            clear_stale_lock,
            repair_recovery,
            rollback_recovery,
            selected_save,
            selected_shelve,
            selected_discard,
            publish_current_variation,
            fetch_remote,
            preflight_apply_incoming,
            apply_incoming,
            preflight_merge_incoming,
            merge_incoming,
            merge_incoming_with_resolutions
        ])
        .run(tauri::generate_context!())
        .expect("error while running Draftline Workbench");
}
