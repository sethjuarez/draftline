use std::fs;
use std::path::{Path, PathBuf};

use draftline::tauri_contract::{
    adopt_remote_variation, adopt_workspace, apply_history_cleanup, apply_incoming, apply_shelf,
    audit_content_policy, clone_workspace, create_variation_from_version,
    create_variation_from_version_guarded, diff_version_to_workspace, diff_versions,
    diff_workspace_file, fetch_remote, get_changes, get_full_history, get_history,
    get_history_compaction_candidates, get_workspace_graph, get_workspace_graph_agent_summary,
    get_workspace_graph_around_version, get_workspace_graph_common_ancestor,
    get_workspace_graph_compare_summary, get_workspace_graph_for_variation,
    get_workspace_graph_neighborhood, get_workspace_graph_node, get_workspace_graph_overview,
    get_workspace_graph_path, get_workspace_graph_refs, get_workspace_graph_summary,
    inspect_workspace, into_tauri_result, list_remote_variations, list_remotes, list_shelves,
    list_support_refs, list_variations, merge_conflict_view_model, merge_incoming,
    merge_incoming_with_resolutions, merge_incoming_with_resolutions_with_context, open_workspace,
    preflight_apply_incoming, preflight_apply_shelf, preflight_create_variation_from_version,
    preflight_history_cleanup_remote_impact, preflight_merge_incoming,
    preflight_publish_history_cleanup, preflight_rename_variation, preflight_switch_variation,
    preflight_undo_history_cleanup, preview_history_cleanup, preview_shelf, preview_version,
    preview_version_file, preview_workspace_file, publish_current_variation,
    publish_history_cleanup, rename_variation, resolve_rewritten_version,
    restore_version_as_new_save, restore_version_as_new_save_to_variation, save,
    search_workspace_graph, selected_discard, selected_save, selected_save_with_context,
    selected_shelve, switch_variation, undo_history_cleanup, verify_workspace,
    whole_file_use_content_resolutions, ApplyHistoryCleanupRequest, CloneWorkspaceRequest,
    ConflictContentSource, CreateVariationFromVersionRequest, CurrentFileRequest,
    DiffVersionsRequest, DraftlineCommandContext, DraftlineEventKind,
    GuardedCreateVariationFromVersionRequest, HistoryCleanupRemoteImpactRequest,
    HistoryCompactionCandidatesCommandRequest, ListSupportRefsRequest, MergeIncomingRequest,
    MergeIncomingWithResolutionsRequest, PreflightCreateVariationFromVersionRequest,
    PreviewHistoryCleanupRequest, PreviewVersionFileRequest, PublishCurrentVariationRequest,
    PublishHistoryCleanupPreflightRequest, PublishHistoryCleanupRequest, RemoteRequest,
    RemoteVariationRequest, RenameVariationRequest, ResolveRewrittenVersionRequest,
    RestoreVersionRequest, SaveRequest, SelectedDiscardRequest, SelectedSaveRequest,
    SelectedShelveRequest, ShelfRequest, SwitchVariationRequest, TargetedRestoreVersionRequest,
    UndoHistoryCleanupPreflightRequest, UndoHistoryCleanupRequest, VersionRequest,
    WorkspaceGraphAroundVersionRequest, WorkspaceGraphNeighborhoodRequest,
    WorkspaceGraphOverviewRequest, WorkspaceGraphPairRequest, WorkspaceGraphRequest,
    WorkspaceGraphSearchRequest, WorkspaceGraphVariationRequest, WorkspaceRequest,
};
use draftline::{
    CleanupBase, CleanupMode, CleanupSafety, CommitRange, ContentPolicy, Contributor,
    ContributorProfile, HistoryCleanupRequest, HistoryCompactionCandidatesRequest,
    MergeConflictResolution, MergeResolutionChoice, MilestoneSpec, OperationLockState,
    RemoteRewritePolicy, RestoreVersionTarget, RewriteConfirmation, StaleVersionDisposition,
    SupportRefScope, SwitchPolicy, SyncState, VariationId, Workspace,
    WorkspaceGraphOverviewOptions,
};
use serde_json::Value;

fn write_file(root: &Path, relative: &str, content: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read_file(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).unwrap()
}

fn configure_identity(root: &Path) {
    let repo = git2::Repository::open(root).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Dogfood Bot").unwrap();
    config
        .set_str("user.email", "workbench@example.com")
        .unwrap();
}

fn init_bare_remote(root: &Path) {
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    git2::Repository::init_opts(root, &options).unwrap();
}

fn assert_object_keys(value: &Value, path: &[&str], expected: &[&str]) {
    let mut current = value;
    for segment in path {
        current = current
            .get(*segment)
            .unwrap_or_else(|| panic!("missing JSON path segment {segment} in {path:?}"));
    }

    let object = current
        .as_object()
        .unwrap_or_else(|| panic!("expected JSON object at path {path:?}"));
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected, "unexpected JSON keys at path {path:?}");
}

#[test]
fn tauri_contract_keeps_frontend_json_shape_stable() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Base");
    workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "post.md", b"# Edited");

    let request = WorkspaceRequest {
        workspace_path: temp.path().to_path_buf(),
    };
    let diagnostics = serde_json::to_value(inspect_workspace(request.clone()).unwrap()).unwrap();
    assert_object_keys(
        &diagnostics,
        &[],
        &["inspection", "operation_lock", "summary", "verification"],
    );
    assert_object_keys(
        &diagnostics,
        &["summary"],
        &[
            "active_variation",
            "dirty_files",
            "is_dirty",
            "recovery",
            "state_may_be_inconsistent",
            "variations",
            "versions",
        ],
    );
    assert_object_keys(
        &diagnostics,
        &["inspection"],
        &[
            "current_variation",
            "diagnostics",
            "dirty",
            "operation_lock",
            "recovery",
            "remotes",
            "safe_next_actions",
            "sharing_mode",
            "support_refs",
            "workspace_id",
        ],
    );
    assert_object_keys(
        &diagnostics["summary"]["dirty_files"][0],
        &[],
        &["is_binary", "is_large", "kind", "path"],
    );
    assert_eq!(
        diagnostics["summary"]["dirty_files"][0]["path"].as_str(),
        Some("post.md")
    );
    assert_eq!(
        diagnostics["inspection"]["dirty"]["is_dirty"].as_bool(),
        Some(true)
    );

    let saved = serde_json::to_value(
        selected_save(SelectedSaveRequest {
            workspace_path: temp.path().to_path_buf(),
            paths: vec![PathBuf::from("post.md")],
            label: "Edited save".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_object_keys(&saved, &[], &["postconditions", "preflight", "version"]);
    assert_object_keys(
        &saved,
        &["preflight"],
        &[
            "binary_files",
            "can_proceed",
            "dirty_files",
            "file_hazards",
            "large_files",
            "operation",
            "unresolved_conflicts",
            "untracked_assets",
            "variation_divergence",
            "will_write_files",
        ],
    );
    assert_object_keys(
        &saved,
        &["postconditions"],
        &["errors", "remaining_changes", "verification"],
    );

    let error = serde_json::to_value(
        into_tauri_result(selected_save(SelectedSaveRequest {
            workspace_path: temp.path().to_path_buf(),
            paths: vec![PathBuf::from("post.md")],
            label: "No changed files".to_string(),
        }))
        .unwrap_err(),
    )
    .unwrap();
    assert_object_keys(&error, &[], &["code", "details", "message"]);
    assert_eq!(error["code"].as_str(), Some("preflight_failed"));
    assert_object_keys(
        &error,
        &["details"],
        &[
            "binary_files",
            "can_proceed",
            "dirty_files",
            "file_hazards",
            "large_files",
            "operation",
            "unresolved_conflicts",
            "untracked_assets",
            "variation_divergence",
            "will_write_files",
        ],
    );
    assert_eq!(error["details"]["operation"].as_str(), Some("save_files"));
}

#[test]
fn tauri_contract_renders_workspace_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Hello");
    workspace.save_version("Initial draft").unwrap();

    let request = WorkspaceRequest {
        workspace_path: temp.path().to_path_buf(),
    };
    let diagnostics = inspect_workspace(request.clone()).unwrap();
    let diagnostics_json = serde_json::to_value(&diagnostics).unwrap();
    assert_eq!(
        diagnostics_json["summary"]["active_variation"]["name"].as_str(),
        Some(diagnostics.summary.active_variation.name.as_str())
    );
    let active_variation = diagnostics.summary.active_variation.name.clone();
    assert_eq!(diagnostics.summary.versions.len(), 1);
    assert!(diagnostics.verification.recovery_clear);
    assert!(diagnostics.verification.operation_lock_clear);
    assert_eq!(
        diagnostics.inspection.current_variation.unwrap().as_str(),
        active_variation
    );

    let variations = list_variations(request.clone()).unwrap();
    assert_eq!(variations.len(), 1);
    assert_eq!(variations[0].variation.name, active_variation);

    let verification = verify_workspace(request).unwrap();
    assert!(verification.current_variation_present);

    let archived = workspace.create_variation("archive-me").unwrap();
    workspace.delete_variation(archived.id()).unwrap();
    let support_refs = list_support_refs(ListSupportRefsRequest {
        workspace_path: temp.path().to_path_buf(),
        scope: SupportRefScope::Local,
    })
    .unwrap();
    assert_eq!(support_refs.len(), 1);
}

#[test]
fn tauri_contract_renames_variation_with_preflight_shape() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Hello");
    workspace.save_version("Initial draft").unwrap();
    let source = workspace.current_variation().unwrap();
    let target = format!("{source}-renamed");
    let request = RenameVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        source_variation_id: source.clone(),
        target_variation_id: target.clone(),
        token: None,
    };

    let preflight = preflight_rename_variation(request.clone()).unwrap();
    assert_eq!(preflight.source_variation.as_str(), source);
    assert_eq!(preflight.target_variation.as_str(), target);
    assert!(preflight.can_rename);

    let renamed = rename_variation(RenameVariationRequest {
        token: Some(preflight.token.clone()),
        ..request
    })
    .unwrap();
    assert_eq!(renamed.preflight.source_variation.as_str(), source);
    assert_eq!(renamed.variation.name, target);
    assert!(renamed.variation.is_current);
    assert!(
        renamed
            .postconditions
            .verification
            .unwrap()
            .current_variation_present
    );
}

#[test]
fn tauri_contract_rejects_stale_rename_token() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Hello");
    workspace.save_version("Initial draft").unwrap();
    let source = workspace.current_variation().unwrap();
    let request = RenameVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        source_variation_id: source,
        target_variation_id: "renamed".to_string(),
        token: None,
    };
    let preflight = preflight_rename_variation(request.clone()).unwrap();

    write_file(workspace.root(), "post.md", b"# Hello\nupdated");
    workspace.save_version("Updated draft").unwrap();

    let error = rename_variation(RenameVariationRequest {
        token: Some(preflight.token),
        ..request
    })
    .unwrap_err();
    assert!(matches!(
        error,
        draftline::DraftlineError::LocalStateChanged { .. }
    ));
}

#[test]
fn tauri_contract_switches_variation_without_creating_history() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "post.md", b"main");
    workspace.save_version("Main").unwrap();
    let original = workspace.current_variation().unwrap();
    workspace
        .create_variation_from(base.id(), "alternate")
        .unwrap();
    let history_len = workspace.full_history().unwrap().len();
    let request = SwitchVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        variation_id: "alternate".to_string(),
    };

    let preflight = preflight_switch_variation(request.clone()).unwrap();
    assert!(preflight.can_proceed);
    assert_eq!(preflight.operation, "switch_variation");

    let switched = switch_variation(request).unwrap();
    assert_eq!(switched.variation.name, "alternate");
    assert!(switched.variation.is_current);
    assert_eq!(read_file(workspace.root(), "post.md"), "base");
    assert_eq!(workspace.full_history().unwrap().len(), history_len);

    write_file(workspace.root(), "scratch.md", b"dirty");
    let dirty_request = SwitchVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        variation_id: original,
    };
    let dirty_preflight = preflight_switch_variation(dirty_request.clone()).unwrap();
    assert!(!dirty_preflight.can_proceed);
    let error = switch_variation(dirty_request).unwrap_err();
    assert!(matches!(
        error,
        draftline::DraftlineError::PreflightFailed(report) if report.operation == "switch_variation"
    ));
    assert_eq!(workspace.full_history().unwrap().len(), history_len);
}

#[test]
fn tauri_contract_smokes_selected_file_operations() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "one.md", b"one");
    write_file(workspace.root(), "two.md", b"two");
    workspace.save_version("Base").unwrap();

    write_file(workspace.root(), "one.md", b"one saved");
    write_file(workspace.root(), "two.md", b"two still dirty");
    let save = selected_save(SelectedSaveRequest {
        workspace_path: temp.path().to_path_buf(),
        paths: vec![PathBuf::from("one.md")],
        label: "Save one".to_string(),
    })
    .unwrap();
    let save_json = serde_json::to_value(&save).unwrap();
    assert_eq!(save_json["version"]["label"], "Save one");
    assert_eq!(save.preflight.dirty_files.len(), 1);
    assert_eq!(save.version.label, "Save one");
    let remaining = save.postconditions.remaining_changes.as_ref().unwrap();
    assert_eq!(remaining.files.len(), 1);
    assert_eq!(remaining.files[0].path, PathBuf::from("two.md"));
    assert!(save.postconditions.verification.unwrap().recovery_clear);
    assert!(save.postconditions.errors.is_empty());

    let shelf = selected_shelve(SelectedShelveRequest {
        workspace_path: temp.path().to_path_buf(),
        paths: vec![PathBuf::from("two.md")],
        name: "two-aside".to_string(),
    })
    .unwrap();
    assert_eq!(shelf.preflight.dirty_files.len(), 1);
    assert_eq!(shelf.shelf.id, "two-aside");
    assert!(shelf
        .postconditions
        .remaining_changes
        .as_ref()
        .unwrap()
        .files
        .is_empty());
    assert!(shelf.postconditions.verification.unwrap().recovery_clear);
    assert!(shelf.postconditions.errors.is_empty());

    write_file(workspace.root(), "one.md", b"one discarded");
    write_file(workspace.root(), "two.md", b"two remains dirty");
    let discard = selected_discard(SelectedDiscardRequest {
        workspace_path: temp.path().to_path_buf(),
        paths: vec![PathBuf::from("one.md")],
    })
    .unwrap();
    assert_eq!(discard.preflight.dirty_files.len(), 1);
    assert_eq!(discard.discarded.files[0].path, PathBuf::from("one.md"));
    let remaining = discard.postconditions.remaining_changes.as_ref().unwrap();
    assert_eq!(remaining.files.len(), 1);
    assert_eq!(remaining.files[0].path, PathBuf::from("two.md"));
    assert!(discard.postconditions.verification.unwrap().recovery_clear);
    assert!(discard.postconditions.errors.is_empty());
}

#[test]
fn tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "post.md", b"# Second");
    let second = workspace.save_version("Second").unwrap();

    let workspace_request = WorkspaceRequest {
        workspace_path: temp.path().to_path_buf(),
    };
    let changes = get_changes(workspace_request.clone()).unwrap();
    assert!(changes.files.is_empty());
    assert_eq!(get_history(workspace_request.clone()).unwrap().len(), 2);
    assert_eq!(
        get_full_history(workspace_request.clone()).unwrap().len(),
        2
    );
    let graph_request = WorkspaceGraphRequest {
        workspace_path: temp.path().to_path_buf(),
        options: Default::default(),
    };
    assert_eq!(
        get_workspace_graph(graph_request.clone())
            .unwrap()
            .nodes
            .len(),
        2
    );
    assert_eq!(
        get_workspace_graph_refs(graph_request.clone())
            .unwrap()
            .refs
            .len(),
        1
    );
    assert_eq!(
        get_workspace_graph_summary(graph_request.clone())
            .unwrap()
            .total_nodes,
        2
    );
    assert!(get_workspace_graph_agent_summary(graph_request.clone())
        .unwrap()
        .suggested_next_commands
        .contains(&"get_workspace_graph_for_variation".to_string()));
    assert_eq!(
        get_workspace_graph_overview(WorkspaceGraphOverviewRequest {
            workspace_path: temp.path().to_path_buf(),
            options: WorkspaceGraphOverviewOptions::default(),
        })
        .unwrap()
        .nodes
        .len(),
        2
    );
    assert_eq!(
        get_workspace_graph_around_version(WorkspaceGraphAroundVersionRequest {
            workspace_path: temp.path().to_path_buf(),
            version_id: second.id().as_str().to_string(),
            radius: 0,
            options: Default::default(),
        })
        .unwrap()
        .nodes
        .len(),
        1
    );
    assert_eq!(
        get_workspace_graph_for_variation(WorkspaceGraphVariationRequest {
            workspace_path: temp.path().to_path_buf(),
            variation_id: "main".to_string(),
            options: Default::default(),
        })
        .unwrap()
        .nodes
        .len(),
        2
    );
    assert_eq!(
        get_workspace_graph_neighborhood(WorkspaceGraphNeighborhoodRequest {
            workspace_path: temp.path().to_path_buf(),
            version_id: base.id().as_str().to_string(),
            radius: 1,
            options: Default::default(),
        })
        .unwrap()
        .nodes
        .len(),
        2
    );
    assert_eq!(
        search_workspace_graph(WorkspaceGraphSearchRequest {
            workspace_path: temp.path().to_path_buf(),
            query: "Second".to_string(),
            options: Default::default(),
        })
        .unwrap()
        .total_matches,
        1
    );
    assert!(
        get_workspace_graph_path(WorkspaceGraphPairRequest {
            workspace_path: temp.path().to_path_buf(),
            from_version_id: second.id().as_str().to_string(),
            to_version_id: base.id().as_str().to_string(),
            options: Default::default(),
        })
        .unwrap()
        .found
    );
    assert_eq!(
        get_workspace_graph_common_ancestor(WorkspaceGraphPairRequest {
            workspace_path: temp.path().to_path_buf(),
            from_version_id: second.id().as_str().to_string(),
            to_version_id: base.id().as_str().to_string(),
            options: Default::default(),
        })
        .unwrap()
        .common_ancestor,
        Some(base.id().clone())
    );
    assert_eq!(
        get_workspace_graph_node(VersionRequest {
            workspace_path: temp.path().to_path_buf(),
            version_id: second.id().as_str().to_string(),
        })
        .unwrap()
        .changed_file_count,
        Some(1)
    );
    assert_eq!(
        get_workspace_graph_compare_summary(WorkspaceGraphPairRequest {
            workspace_path: temp.path().to_path_buf(),
            from_version_id: base.id().as_str().to_string(),
            to_version_id: second.id().as_str().to_string(),
            options: Default::default(),
        })
        .unwrap()
        .changed_file_count,
        1
    );
    assert_eq!(
        create_variation_from_version(CreateVariationFromVersionRequest {
            workspace_path: temp.path().to_path_buf(),
            version_id: base.id().as_str().to_string(),
            name: "from-base".to_string(),
            metadata: Default::default(),
        })
        .unwrap()
        .variation
        .id()
        .clone(),
        VariationId::from("from-base")
    );

    let diff = diff_versions(DiffVersionsRequest {
        workspace_path: temp.path().to_path_buf(),
        from_version_id: base.id().as_str().to_string(),
        to_version_id: second.id().as_str().to_string(),
    })
    .unwrap();
    assert_eq!(diff.files[0].path, PathBuf::from("post.md"));

    write_file(workspace.root(), "post.md", b"# Workspace");
    let workspace_diff = diff_version_to_workspace(VersionRequest {
        workspace_path: temp.path().to_path_buf(),
        version_id: second.id().as_str().to_string(),
    })
    .unwrap();
    assert_eq!(workspace_diff.to_version, None);
    fs::write(workspace.root().join("post.md"), b"# Second").unwrap();

    let preview = preview_version(VersionRequest {
        workspace_path: temp.path().to_path_buf(),
        version_id: second.id().as_str().to_string(),
    })
    .unwrap();
    assert_eq!(preview.files.len(), 1);
    let preview_file = preview_version_file(PreviewVersionFileRequest {
        workspace_path: temp.path().to_path_buf(),
        version_id: second.id().as_str().to_string(),
        path: PathBuf::from("post.md"),
    })
    .unwrap()
    .unwrap();
    assert_eq!(preview_file.content.as_deref(), Some("# Second"));

    let restored = restore_version_as_new_save(RestoreVersionRequest {
        workspace_path: temp.path().to_path_buf(),
        version_id: base.id().as_str().to_string(),
        label: "Restore base".to_string(),
    })
    .unwrap();
    assert_eq!(restored.version.label, "Restore base");
    assert!(restored.postconditions.errors.is_empty());

    write_file(workspace.root(), "shelf.md", b"temporary");
    workspace.shelve_changes("temporary-shelf").unwrap();
    let shelves = list_shelves(workspace_request.clone()).unwrap();
    assert_eq!(shelves[0].id, "temporary-shelf");
    let shelf_request = ShelfRequest {
        workspace_path: temp.path().to_path_buf(),
        shelf_id: "temporary-shelf".to_string(),
    };
    let shelf_preview = preview_shelf(shelf_request.clone()).unwrap();
    assert!(shelf_preview
        .files
        .iter()
        .any(|file| file.path == Path::new("shelf.md")));
    let shelf_preflight = preflight_apply_shelf(shelf_request.clone()).unwrap();
    assert!(shelf_preflight.can_proceed);
    let applied = apply_shelf(shelf_request.clone()).unwrap();
    assert_eq!(applied.shelf.id, "temporary-shelf");
    assert!(applied.postconditions.errors.is_empty());
    workspace.discard_changes().unwrap();

    let deleted = draftline::tauri_contract::delete_shelf(shelf_request).unwrap();
    assert!(deleted.postconditions.errors.is_empty());
    assert!(audit_content_policy(workspace_request)
        .unwrap()
        .historical_out_of_policy_paths
        .is_empty());
}

#[test]
fn tauri_contract_restores_version_to_target_variation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "post.md", b"# Current");
    workspace.save_version("Current").unwrap();

    let restored = restore_version_as_new_save_to_variation(TargetedRestoreVersionRequest {
        workspace_path: temp.path().to_path_buf(),
        version_id: base.id().as_str().to_string(),
        label: "Restore to preview branch".to_string(),
        target: RestoreVersionTarget::New {
            name: "preview-branch".to_string(),
            metadata: Default::default(),
        },
    })
    .unwrap();

    assert_eq!(restored.version.label, "Restore to preview branch");
    assert_eq!(restored.target_variation.name, "preview-branch");
    assert!(restored.postconditions.errors.is_empty());
    assert_eq!(
        Workspace::open(temp.path())
            .unwrap()
            .current_variation()
            .unwrap(),
        "preview-branch"
    );

    let json = serde_json::to_value(restored).unwrap();
    assert_object_keys(
        &json,
        &[],
        &["postconditions", "target_variation", "version"],
    );
}

#[test]
fn tauri_contract_smokes_publish_current_variation() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Publish me");
    workspace.save_version("Publishable draft").unwrap();
    workspace
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();

    let published = publish_current_variation(PublishCurrentVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();
    let published_json = serde_json::to_value(&published).unwrap();
    assert_eq!(published_json["publish"]["remote"], "origin");

    assert_eq!(published.publish.remote, "origin");
    let published_variation = published.publish.variation.clone();
    assert_eq!(published.publish.published_versions, 1);
    assert!(published.preflight.can_publish);
    assert!(
        published
            .postconditions
            .verification
            .as_ref()
            .unwrap()
            .recovery_clear
    );
    assert!(published.postconditions.errors.is_empty());

    let remote = git2::Repository::open_bare(remote_dir.path()).unwrap();
    let remote_oid = remote
        .refname_to_id(&format!("refs/heads/{published_variation}"))
        .unwrap();
    let local_oid = git2::Repository::open(temp.path())
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(remote_oid, local_oid);
}

#[test]
fn tauri_contract_smokes_collaboration_incoming_and_merge() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root());
    write_file(author.root(), "shared.md", b"base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    publish_current_variation(PublishCurrentVariationRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
            .unwrap();
    configure_identity(teammate.root());

    write_file(teammate.root(), "shared.md", b"base\nteammate fast-forward");
    teammate.save_version("Teammate update").unwrap();
    teammate.publish_changes("origin").unwrap();

    let remote_request = RemoteRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    };
    let fetched = fetch_remote(remote_request.clone()).unwrap();
    assert_eq!(fetched.sync_status.state, SyncState::IncomingAvailable);
    assert!(fetched.postconditions.errors.is_empty());

    let apply_preflight = preflight_apply_incoming(remote_request.clone()).unwrap();
    assert!(apply_preflight.can_proceed);
    assert_eq!(
        apply_preflight.sync_status.state,
        SyncState::IncomingAvailable
    );
    let applied = apply_incoming(remote_request.clone()).unwrap();
    assert_eq!(applied.apply.applied_count, 1);
    assert!(applied.postconditions.errors.is_empty());
    assert!(read_file(author.root(), "shared.md").contains("teammate fast-forward"));

    write_file(author.root(), "author.md", b"local branch of work");
    author.save_version("Author local update").unwrap();
    write_file(teammate.root(), "teammate.md", b"remote branch of work");
    teammate.save_version("Teammate divergent update").unwrap();
    teammate.publish_changes("origin").unwrap();

    let merge_preflight = preflight_merge_incoming(remote_request).unwrap();
    assert_eq!(merge_preflight.sync_status.state, SyncState::NeedsMerge);
    assert!(merge_preflight.can_merge_cleanly);

    write_file(author.root(), "draft.md", b"unsaved blocker");
    let blocked_merge = into_tauri_result(merge_incoming(MergeIncomingRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
        label: "Blocked merge".to_string(),
    }))
    .unwrap_err();
    assert_eq!(blocked_merge.code, "preflight_failed");
    let blocked_json = serde_json::to_value(blocked_merge).unwrap();
    assert_eq!(blocked_json["details"]["operation"], "merge_incoming");
    assert_eq!(
        blocked_json["details"]["dirty_files"][0]["path"].as_str(),
        Some("draft.md")
    );
    fs::remove_file(author.root().join("draft.md")).unwrap();

    let merged = merge_incoming(MergeIncomingRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
        label: "Workbench contract merge".to_string(),
    })
    .unwrap();
    assert_eq!(merged.merge.version.label, "Workbench contract merge");
    assert_eq!(
        read_file(author.root(), "author.md"),
        "local branch of work"
    );
    assert_eq!(
        read_file(author.root(), "teammate.md"),
        "remote branch of work"
    );
    assert!(merged.postconditions.errors.is_empty());
}

#[test]
fn tauri_contract_preflights_remote_aware_variation_creation() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root());
    write_file(author.root(), "post.md", b"base");
    let base = author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    publish_current_variation(PublishCurrentVariationRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();

    let consumer_dir = tempfile::tempdir().unwrap();
    let consumer =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), consumer_dir.path())
            .unwrap();
    configure_identity(consumer.root());

    let remote_only = author
        .create_variation_from(base.id(), "cutready-name")
        .unwrap();
    author
        .switch_variation(remote_only.id(), draftline::SwitchPolicy::AbortIfDirty)
        .unwrap();
    write_file(author.root(), "post.md", b"remote-only");
    author.save_version("Remote only").unwrap();
    publish_current_variation(PublishCurrentVariationRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();

    let collision =
        preflight_create_variation_from_version(PreflightCreateVariationFromVersionRequest {
            workspace_path: consumer_dir.path().to_path_buf(),
            version_id: base.id().to_string(),
            name: "cutready-name".to_string(),
            remote: Some("origin".to_string()),
        })
        .unwrap();
    assert!(!collision.can_create);
    assert!(!collision.local_collision);
    assert!(collision.remote_collision);
    assert!(collision.remote_only_collision);
    assert_eq!(
        collision
            .existing_remote_head
            .as_ref()
            .map(|version| version.label.as_str()),
        Some("Remote only")
    );
    assert_eq!(
        collision.suggested_alternative.as_deref(),
        Some("cutready-name-2")
    );
    assert!(collision.token.is_none());
    assert!(git2::Repository::open(consumer.root())
        .unwrap()
        .find_branch("cutready-name", git2::BranchType::Local)
        .is_err());

    let clean =
        preflight_create_variation_from_version(PreflightCreateVariationFromVersionRequest {
            workspace_path: consumer_dir.path().to_path_buf(),
            version_id: base.id().to_string(),
            name: "cutready-local".to_string(),
            remote: Some("origin".to_string()),
        })
        .unwrap();
    assert!(clean.can_create);
    let created = create_variation_from_version_guarded(GuardedCreateVariationFromVersionRequest {
        workspace_path: consumer_dir.path().to_path_buf(),
        token: clean.token.unwrap(),
        metadata: Default::default(),
    })
    .unwrap();
    assert_eq!(created.variation.name, "cutready-local");
    assert!(created.postconditions.errors.is_empty());
}

#[test]
fn tauri_contract_smokes_merge_incoming_with_resolutions() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root());
    write_file(author.root(), "shared.md", b"base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    author.publish_changes("origin").unwrap();

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
            .unwrap();
    configure_identity(teammate.root());

    write_file(author.root(), "shared.md", b"ours");
    author.save_version("Author local update").unwrap();
    write_file(teammate.root(), "shared.md", b"theirs");
    teammate.save_version("Teammate update").unwrap();
    teammate.publish_changes("origin").unwrap();

    let remote_request = RemoteRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    };
    let preflight = preflight_merge_incoming(remote_request).unwrap();
    assert_eq!(preflight.sync_status.state, SyncState::NeedsMerge);
    assert!(!preflight.can_merge_cleanly);
    assert_eq!(preflight.conflicts.len(), 1);
    assert!(preflight.token.is_some());

    let profile = ContributorProfile::new(
        Contributor {
            name: "Profile Author".to_string(),
            email: Some("author@example.invalid".to_string()),
        },
        Contributor {
            name: "Profile Service".to_string(),
            email: Some("service@example.invalid".to_string()),
        },
    );
    let mut context = DraftlineCommandContext::new().with_contributor_profile(profile);
    let merged = merge_incoming_with_resolutions_with_context(
        &mut context,
        MergeIncomingWithResolutionsRequest {
            workspace_path: author_dir.path().to_path_buf(),
            remote: "origin".to_string(),
            label: "Resolved merge".to_string(),
            token: preflight.token.clone().unwrap(),
            resolutions: vec![MergeConflictResolution::new(
                preflight.conflicts[0].path.clone(),
                MergeResolutionChoice::UseContent {
                    content: "resolved".to_string(),
                },
            )],
        },
    )
    .unwrap();

    assert_eq!(merged.merge.version.label, "Resolved merge");
    assert_eq!(merged.merge.version.author.name, "Profile Author");
    assert_eq!(merged.merge.version.saved_by.name, "Profile Service");
    assert_eq!(read_file(author.root(), "shared.md"), "resolved");
    assert!(merged.postconditions.errors.is_empty());
}

#[test]
fn tauri_contract_rejects_stale_merge_resolution_token() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root());
    write_file(author.root(), "shared.md", b"base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    author.publish_changes("origin").unwrap();

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
            .unwrap();
    configure_identity(teammate.root());

    write_file(author.root(), "shared.md", b"ours");
    author.save_version("Author local update").unwrap();
    write_file(teammate.root(), "shared.md", b"theirs");
    teammate.save_version("Teammate update").unwrap();
    teammate.publish_changes("origin").unwrap();

    let remote_request = RemoteRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    };
    let preflight = preflight_merge_incoming(remote_request).unwrap();
    let stale_token = preflight.token.clone().unwrap();

    write_file(teammate.root(), "shared.md", b"new theirs");
    teammate.save_version("Teammate second update").unwrap();
    teammate.publish_changes("origin").unwrap();

    let error = into_tauri_result(merge_incoming_with_resolutions(
        MergeIncomingWithResolutionsRequest {
            workspace_path: author_dir.path().to_path_buf(),
            remote: "origin".to_string(),
            label: "Stale resolved merge".to_string(),
            token: stale_token,
            resolutions: vec![MergeConflictResolution::new(
                preflight.conflicts[0].path.clone(),
                MergeResolutionChoice::UseTheirs,
            )],
        },
    ))
    .unwrap_err();

    assert_eq!(error.code, "remote_race");
    assert_eq!(read_file(author.root(), "shared.md"), "ours");
}

#[test]
fn tauri_contract_serializes_errors_for_frontend_calls() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Hello");
    workspace.save_version("Initial draft").unwrap();

    let error = into_tauri_result(selected_save(SelectedSaveRequest {
        workspace_path: temp.path().to_path_buf(),
        paths: vec![PathBuf::from("post.md")],
        label: "No changed files".to_string(),
    }))
    .unwrap_err();

    assert_eq!(error.code, "preflight_failed");
    let json = serde_json::to_value(error).unwrap();
    assert_eq!(json["code"], "preflight_failed");
    assert!(json["message"].as_str().unwrap().contains("preflight"));
    assert_eq!(json["details"]["operation"], "save_files");
    assert_eq!(json["details"]["can_proceed"], false);
}

#[test]
fn tauri_contract_smokes_history_cleanup_commands() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"v1");
    workspace.save_version("v1").unwrap();
    write_file(workspace.root(), "post.md", b"v2");
    let v2 = workspace.save_version("noisy v2").unwrap();
    write_file(workspace.root(), "post.md", b"v3");
    let v3 = workspace.save_version("noisy v3").unwrap();

    let cleanup = HistoryCleanupRequest {
        target_variation: None,
        base: CleanupBase::Auto,
        mode: CleanupMode::CompactMilestones {
            milestones: vec![MilestoneSpec {
                title: "Clean milestone".to_string(),
                description: Some("Compacted noisy saves".to_string()),
                include_range: CommitRange {
                    start: v2.id().clone(),
                    end: v3.id().clone(),
                },
            }],
            preserve_named_branches: true,
            preserve_merge_boundaries: true,
        },
        safety: CleanupSafety::default_user_facing(),
        remote_policy: RemoteRewritePolicy::LocalOnly,
    };

    let preview = preview_history_cleanup(PreviewHistoryCleanupRequest {
        workspace_path: temp.path().to_path_buf(),
        cleanup,
    })
    .unwrap();
    let preview_json = serde_json::to_value(&preview).unwrap();
    assert_object_keys(
        &preview_json,
        &[],
        &[
            "affected_refs",
            "commit_map",
            "descendant_rewrite_count",
            "graph_diff",
            "new_head",
            "old_head",
            "operations",
            "plan_id",
            "planned_backup_ref",
            "planned_ref_updates",
            "preview_ref",
            "selected_commit_count",
            "snapshot_map",
            "target_variation",
            "warnings",
        ],
    );
    let candidates = get_history_compaction_candidates(HistoryCompactionCandidatesCommandRequest {
        workspace_path: temp.path().to_path_buf(),
        request: HistoryCompactionCandidatesRequest {
            target_variation: None,
            selected_version: v2.id().clone(),
            remote: None,
            preserve_named_branches: true,
            preserve_merge_boundaries: true,
        },
    })
    .unwrap();
    let candidates_json = serde_json::to_value(&candidates).unwrap();
    assert_object_keys(
        &candidates_json,
        &[],
        &[
            "candidates",
            "selected_version",
            "target_head",
            "target_variation",
        ],
    );

    let result = apply_history_cleanup(ApplyHistoryCleanupRequest {
        workspace_path: temp.path().to_path_buf(),
        plan_id: preview.plan_id.to_string(),
        confirmation: RewriteConfirmation::UserConfirmed,
    })
    .unwrap();
    assert_eq!(result.new_head, preview.new_head);

    let resolution = resolve_rewritten_version(ResolveRewrittenVersionRequest {
        workspace_path: temp.path().to_path_buf(),
        version_id: v2.id().to_string(),
    })
    .unwrap();
    assert!(matches!(
        resolution.disposition,
        StaleVersionDisposition::SquashedInto { .. }
    ));

    let undo = preflight_undo_history_cleanup(UndoHistoryCleanupPreflightRequest {
        workspace_path: temp.path().to_path_buf(),
        plan_id: result.plan_id.to_string(),
    })
    .unwrap();
    assert!(undo.can_undo);
    let undo_result = undo_history_cleanup(UndoHistoryCleanupRequest {
        workspace_path: temp.path().to_path_buf(),
        token: undo.token,
    })
    .unwrap();
    assert_eq!(undo_result.new_head, v3.id().clone());
}

#[test]
fn tauri_contract_smokes_history_cleanup_remote_publish_commands() {
    let remote = tempfile::tempdir().unwrap();
    init_bare_remote(remote.path());
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    workspace
        .add_remote("origin", remote.path().to_string_lossy())
        .unwrap();
    write_file(workspace.root(), "post.md", b"v1");
    workspace.save_version("v1").unwrap();
    write_file(workspace.root(), "post.md", b"v2");
    let v2 = workspace.save_version("noisy v2").unwrap();
    write_file(workspace.root(), "post.md", b"v3");
    let v3 = workspace.save_version("noisy v3").unwrap();
    workspace.publish_changes("origin").unwrap();

    let cleanup = HistoryCleanupRequest {
        target_variation: None,
        base: CleanupBase::Auto,
        mode: CleanupMode::CompactMilestones {
            milestones: vec![MilestoneSpec {
                title: "Clean milestone".to_string(),
                description: Some("Compacted noisy saves".to_string()),
                include_range: CommitRange {
                    start: v2.id().clone(),
                    end: v3.id().clone(),
                },
            }],
            preserve_named_branches: true,
            preserve_merge_boundaries: true,
        },
        safety: CleanupSafety::default_user_facing(),
        remote_policy: RemoteRewritePolicy::PushWithLease {
            remote: "origin".to_string(),
            branch: "main".to_string(),
        },
    };

    let preview = preview_history_cleanup(PreviewHistoryCleanupRequest {
        workspace_path: temp.path().to_path_buf(),
        cleanup,
    })
    .unwrap();
    let impact = preflight_history_cleanup_remote_impact(HistoryCleanupRemoteImpactRequest {
        workspace_path: temp.path().to_path_buf(),
        plan_id: preview.plan_id.to_string(),
        remote: "origin".to_string(),
    })
    .unwrap();
    assert_object_keys(
        &serde_json::to_value(&impact).unwrap(),
        &[],
        &[
            "descendants",
            "local_head",
            "publish_status",
            "remote",
            "replacement_head",
            "selected",
            "tracking_ref",
            "upstream_head",
            "variation",
            "warnings",
        ],
    );

    let result = apply_history_cleanup(ApplyHistoryCleanupRequest {
        workspace_path: temp.path().to_path_buf(),
        plan_id: preview.plan_id.to_string(),
        confirmation: RewriteConfirmation::UserConfirmed,
    })
    .unwrap();
    let preflight = preflight_publish_history_cleanup(PublishHistoryCleanupPreflightRequest {
        workspace_path: temp.path().to_path_buf(),
        plan_id: result.plan_id.to_string(),
        remote: "origin".to_string(),
    })
    .unwrap();
    assert_object_keys(
        &serde_json::to_value(&preflight).unwrap(),
        &[],
        &[
            "can_publish",
            "expected_remote_oid",
            "plan_id",
            "remote",
            "remote_impact",
            "replacement_oid",
            "support_refs",
            "token",
            "variation",
        ],
    );

    let publish = publish_history_cleanup(PublishHistoryCleanupRequest {
        workspace_path: temp.path().to_path_buf(),
        token: preflight.token.unwrap(),
        confirmation: RewriteConfirmation::UserConfirmed,
    })
    .unwrap();
    assert_object_keys(
        &serde_json::to_value(&publish).unwrap(),
        &[],
        &[
            "expected_remote_oid",
            "plan_id",
            "ref_updates",
            "remote",
            "replacement_oid",
            "support_refs",
            "variation",
        ],
    );
}

#[test]
fn tauri_contract_context_applies_policy_profile_and_events() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "content/post.md", b"# Base");
    write_file(workspace.root(), ".chats/transcript.json", b"{}");
    workspace.save_version("Base").unwrap();

    write_file(workspace.root(), "content/post.md", b"# Edited");
    write_file(
        workspace.root(),
        ".chats/transcript.json",
        b"{\"runtime\":true}",
    );

    let profile = ContributorProfile::new(
        Contributor {
            name: "Product Author".to_string(),
            email: Some("author@example.invalid".to_string()),
        },
        Contributor {
            name: "Draftline Service".to_string(),
            email: Some("service@example.invalid".to_string()),
        },
    );
    let policy = ContentPolicy::new()
        .include("content")
        .unwrap()
        .exclude(".chats")
        .unwrap();
    let mut events = Vec::new();

    let saved = {
        let mut context = DraftlineCommandContext::new()
            .with_content_policy(policy)
            .with_contributor_profile(profile)
            .with_event_sink(|event| events.push(event));

        selected_save_with_context(
            &mut context,
            SelectedSaveRequest {
                workspace_path: temp.path().to_path_buf(),
                paths: vec![PathBuf::from("content/post.md")],
                label: "Profile save".to_string(),
            },
        )
        .unwrap()
    };

    assert_eq!(saved.version.author.name, "Product Author");
    assert_eq!(saved.version.saved_by.name, "Draftline Service");
    assert_eq!(
        saved.postconditions.remaining_changes.unwrap().files.len(),
        0
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, DraftlineEventKind::HistoryChanged);
    assert_eq!(events[0].sequence, 1);
    assert!(events[0].changed_paths.is_empty());
}

#[test]
fn tauri_contract_smokes_setup_and_current_file_commands() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root());
    write_file(author.root(), "post.md", b"# Base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    publish_current_variation(PublishCurrentVariationRequest {
        workspace_path: author_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();

    let clone_dir = tempfile::tempdir().unwrap();
    let cloned = clone_workspace(CloneWorkspaceRequest {
        remote_url: remote_dir.path().to_string_lossy().to_string(),
        workspace_path: clone_dir.path().to_path_buf(),
    })
    .unwrap();
    assert!(!cloned.diagnostics.summary.state_may_be_inconsistent);

    let opened = open_workspace(WorkspaceRequest {
        workspace_path: clone_dir.path().to_path_buf(),
    })
    .unwrap();
    assert_eq!(opened.diagnostics.inspection.remotes[0].name, "origin");

    let adopted = adopt_workspace(WorkspaceRequest {
        workspace_path: clone_dir.path().to_path_buf(),
    })
    .unwrap();
    assert!(adopted.preflight.can_adopt);

    configure_identity(clone_dir.path());
    write_file(clone_dir.path(), "post.md", b"# Edited");
    let file_request = CurrentFileRequest {
        workspace_path: clone_dir.path().to_path_buf(),
        path: PathBuf::from("post.md"),
    };
    let diff = diff_workspace_file(file_request.clone()).unwrap().unwrap();
    assert_eq!(diff.path, PathBuf::from("post.md"));
    assert!(diff.patch.unwrap().contains("# Edited"));
    let preview = preview_workspace_file(file_request).unwrap().unwrap();
    assert_eq!(preview.content.as_deref(), Some("# Edited"));

    let saved = save(SaveRequest {
        workspace_path: clone_dir.path().to_path_buf(),
        label: "Current file edit".to_string(),
    })
    .unwrap();
    assert_eq!(saved.version.label, "Current file edit");
}

#[test]
fn tauri_contract_adopt_workspace_returns_blockers_without_mutating() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root());
    write_file(workspace.root(), "post.md", b"# Base");
    workspace.save_version("Base").unwrap();
    let draftline_dir = workspace.root().join(".git").join("draftline");
    fs::create_dir_all(&draftline_dir).unwrap();
    fs::write(draftline_dir.join("operation.lock"), b"{not-json").unwrap();

    let result = adopt_workspace(WorkspaceRequest {
        workspace_path: temp.path().to_path_buf(),
    })
    .unwrap();

    assert!(!result.preflight.can_adopt);
    assert!(!result.preflight.blockers.is_empty());
    assert_eq!(
        result.diagnostics.operation_lock.state,
        OperationLockState::Locked
    );
    assert!(!result.diagnostics.operation_lock.diagnostics.is_empty());
}

#[test]
fn tauri_contract_groups_conflicts_for_host_ui() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let first_dir = tempfile::tempdir().unwrap();
    let first = Workspace::init(first_dir.path()).unwrap();
    configure_identity(first.root());
    write_file(first.root(), "post.md", b"base");
    first.save_version("Base").unwrap();
    first
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    publish_current_variation(PublishCurrentVariationRequest {
        workspace_path: first_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();

    let second_dir = tempfile::tempdir().unwrap();
    let second =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), second_dir.path()).unwrap();
    configure_identity(second.root());

    write_file(first.root(), "post.md", b"ours");
    first.save_version("Ours").unwrap();
    write_file(second.root(), "post.md", b"theirs");
    second.save_version("Theirs").unwrap();
    second.publish_changes("origin").unwrap();

    let report = preflight_merge_incoming(RemoteRequest {
        workspace_path: first_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    })
    .unwrap();
    let model = merge_conflict_view_model(&report);
    assert_eq!(model.files.len(), 1);
    assert_eq!(model.files[0].path, PathBuf::from("post.md"));
    assert_eq!(model.files[0].whole_file_conflicts.len(), 1);

    let resolutions = whole_file_use_content_resolutions(&report, ConflictContentSource::Theirs);
    assert_eq!(resolutions.len(), 1);
    assert!(matches!(
        resolutions[0].choice,
        MergeResolutionChoice::UseContent { .. }
    ));
}

#[test]
fn tauri_contract_smokes_remote_variation_lifecycle_commands() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let first_dir = tempfile::tempdir().unwrap();
    let first = Workspace::init(first_dir.path()).unwrap();
    configure_identity(first.root());
    write_file(first.root(), "post.md", b"main");
    let base = first.save_version("Base").unwrap();
    first
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    first.publish_changes("origin").unwrap();

    let second_dir = tempfile::tempdir().unwrap();
    let second =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), second_dir.path()).unwrap();
    configure_identity(second.root());

    first
        .create_variation_from(base.id(), "teammate-option")
        .unwrap();
    first
        .switch_variation(
            &VariationId::from("teammate-option"),
            SwitchPolicy::AbortIfDirty,
        )
        .unwrap();
    write_file(first.root(), "post.md", b"remote variation");
    first.save_version("Remote variation").unwrap();
    first.publish_changes("origin").unwrap();

    let request = RemoteRequest {
        workspace_path: second_dir.path().to_path_buf(),
        remote: "origin".to_string(),
    };
    assert_eq!(
        list_remotes(WorkspaceRequest {
            workspace_path: second_dir.path().to_path_buf(),
        })
        .unwrap()[0]
            .name,
        "origin"
    );
    let remote_variations = list_remote_variations(request.clone()).unwrap();
    assert!(remote_variations
        .iter()
        .any(|variation| variation.name == "teammate-option"));
    let diagnostics = draftline::tauri_contract::remote_variation_diagnostics(request).unwrap();
    assert!(diagnostics
        .remote_only_variations
        .contains(&draftline::VariationId::from("teammate-option")));

    let adopted = adopt_remote_variation(RemoteVariationRequest {
        workspace_path: second_dir.path().to_path_buf(),
        remote: "origin".to_string(),
        variation_id: "teammate-option".to_string(),
    })
    .unwrap();
    assert_eq!(adopted.variation.name, "teammate-option");
}
