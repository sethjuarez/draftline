use std::fs;
use std::path::{Path, PathBuf};

use draftline::tauri_contract::{
    apply_incoming, fetch_remote, inspect_workspace, into_tauri_result, list_support_refs,
    list_variations, merge_incoming, preflight_apply_incoming, preflight_merge_incoming,
    publish_current_variation, selected_discard, selected_save, selected_shelve, verify_workspace,
    ListSupportRefsRequest, MergeIncomingRequest, PublishCurrentVariationRequest, RemoteRequest,
    SelectedDiscardRequest, SelectedSaveRequest, SelectedShelveRequest, WorkspaceRequest,
};
use draftline::{SupportRefScope, SyncState, Workspace};
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
fn tauri_contract_smokes_publish_current_variation() {
    let remote_dir = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(remote_dir.path()).unwrap();

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
    git2::Repository::init_bare(remote_dir.path()).unwrap();

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
