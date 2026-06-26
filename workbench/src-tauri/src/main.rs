use draftline::tauri_contract as contract;
use draftline::tauri_contract::{
    ApplyIncomingCommandResult, FetchRemoteResult, ListSupportRefsRequest,
    MergeIncomingCommandResult, MergeIncomingRequest, PublishCurrentVariationRequest,
    PublishCurrentVariationResult, RemoteRequest, SelectedDiscardRequest, SelectedDiscardResult,
    SelectedSaveRequest, SelectedSaveResult, SelectedShelveRequest, SelectedShelveResult,
    TauriCommandResult, WorkspaceDiagnostics, WorkspaceRequest,
};
use draftline::{
    ApplyIncomingReport, MergeIncomingReport, SupportRef, VariationSummary, WorkspaceVerification,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

#[tauri::command]
fn inspect_workspace(request: WorkspaceRequest) -> TauriCommandResult<WorkspaceDiagnostics> {
    contract::into_tauri_result(contract::inspect_workspace(request))
}

#[tauri::command]
fn verify_workspace(request: WorkspaceRequest) -> TauriCommandResult<WorkspaceVerification> {
    contract::into_tauri_result(contract::verify_workspace(request))
}

#[tauri::command]
fn list_variations(request: WorkspaceRequest) -> TauriCommandResult<Vec<VariationSummary>> {
    contract::into_tauri_result(contract::list_variations(request))
}

#[tauri::command]
fn list_support_refs(request: ListSupportRefsRequest) -> TauriCommandResult<Vec<SupportRef>> {
    contract::into_tauri_result(contract::list_support_refs(request))
}

#[tauri::command]
fn selected_save(request: SelectedSaveRequest) -> TauriCommandResult<SelectedSaveResult> {
    contract::into_tauri_result(contract::selected_save(request))
}

#[tauri::command]
fn selected_shelve(request: SelectedShelveRequest) -> TauriCommandResult<SelectedShelveResult> {
    contract::into_tauri_result(contract::selected_shelve(request))
}

#[tauri::command]
fn selected_discard(request: SelectedDiscardRequest) -> TauriCommandResult<SelectedDiscardResult> {
    contract::into_tauri_result(contract::selected_discard(request))
}

#[tauri::command]
fn publish_current_variation(
    request: PublishCurrentVariationRequest,
) -> TauriCommandResult<PublishCurrentVariationResult> {
    contract::into_tauri_result(contract::publish_current_variation(request))
}

#[tauri::command]
fn fetch_remote(request: RemoteRequest) -> TauriCommandResult<FetchRemoteResult> {
    contract::into_tauri_result(contract::fetch_remote(request))
}

#[tauri::command]
fn preflight_apply_incoming(request: RemoteRequest) -> TauriCommandResult<ApplyIncomingReport> {
    contract::into_tauri_result(contract::preflight_apply_incoming(request))
}

#[tauri::command]
fn apply_incoming(request: RemoteRequest) -> TauriCommandResult<ApplyIncomingCommandResult> {
    contract::into_tauri_result(contract::apply_incoming(request))
}

#[tauri::command]
fn preflight_merge_incoming(request: RemoteRequest) -> TauriCommandResult<MergeIncomingReport> {
    contract::into_tauri_result(contract::preflight_merge_incoming(request))
}

#[tauri::command]
fn merge_incoming(
    request: MergeIncomingRequest,
) -> TauriCommandResult<MergeIncomingCommandResult> {
    contract::into_tauri_result(contract::merge_incoming(request))
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
        .invoke_handler(tauri::generate_handler![
            inspect_workspace,
            verify_workspace,
            list_variations,
            list_support_refs,
            selected_save,
            selected_shelve,
            selected_discard,
            publish_current_variation,
            fetch_remote,
            preflight_apply_incoming,
            apply_incoming,
            preflight_merge_incoming,
            merge_incoming
        ])
        .run(tauri::generate_context!())
        .expect("error while running Draftline Workbench");
}
