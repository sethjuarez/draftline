use std::fs;
use std::path::Path;

use draftline::tauri_contract::{
    into_tauri_result, merge_conflict_view_model, merge_incoming_with_resolutions, merge_variation,
    merge_variation_with_context, merge_variation_with_resolutions, preflight_merge_variation,
    DraftlineCommandContext, DraftlineEventKind, MergeIncomingWithResolutionsRequest,
    MergeVariationRequest, MergeVariationWithResolutionsRequest, PreflightMergeVariationRequest,
};
use draftline::{
    Contributor, ContributorProfile, DraftlineError, MergeConflictResolution,
    MergeResolutionChoice, SwitchPolicy, SyncState, VariationId, Workspace,
};

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read_file(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).unwrap()
}

fn init(root: &Path) -> Workspace {
    let workspace = Workspace::init(root).unwrap();
    let repo = git2::Repository::open(root).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Merge Bot").unwrap();
    config.set_str("user.email", "merge@example.com").unwrap();
    workspace
}

fn switch(workspace: &Workspace, name: &str) {
    workspace
        .switch_variation(&VariationId::from(name), SwitchPolicy::AbortIfDirty)
        .unwrap();
}

fn head_parents(root: &Path) -> Vec<git2::Oid> {
    let repo = git2::Repository::open(root).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    head.parent_ids().collect()
}

fn branch_tip(root: &Path, name: &str) -> git2::Oid {
    let repo = git2::Repository::open(root).unwrap();
    let tip = repo
        .find_branch(name, git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    tip
}

/// `main` and `review` both move past a shared base; `review` edits `review_file`.
fn diverged(root: &Path, main_edit: (&str, &str), review_edit: (&str, &str)) -> Workspace {
    let workspace = init(root);
    write_file(root, "a.md", "a base");
    write_file(root, "b.md", "b base");
    workspace.save_version("Base").unwrap();
    workspace.create_variation("review").unwrap();
    switch(&workspace, "review");
    write_file(root, review_edit.0, review_edit.1);
    workspace.save_version("Review edit").unwrap();
    switch(&workspace, "main");
    write_file(root, main_edit.0, main_edit.1);
    workspace.save_version("Main edit").unwrap();
    workspace
}

fn preflight_request(root: &Path, source: &str) -> PreflightMergeVariationRequest {
    PreflightMergeVariationRequest {
        workspace_path: root.to_path_buf(),
        source_variation_id: source.to_string(),
    }
}

#[test]
fn merge_variation_writes_a_named_two_parent_version() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = diverged(temp.path(), ("a.md", "a main"), ("b.md", "b review"));
    let main_tip = branch_tip(temp.path(), "main");
    let review_tip = branch_tip(temp.path(), "review");

    let preflight = workspace
        .preflight_merge_variation(&VariationId::from("review"))
        .unwrap();
    assert_eq!(preflight.sync_status.state, SyncState::NeedsMerge);
    assert_eq!(preflight.sync_status.variation, "review");
    assert_eq!(preflight.sync_status.remote, "");
    assert_eq!(
        (preflight.sync_status.ahead, preflight.sync_status.behind),
        (1, 1)
    );
    assert_eq!(preflight.sync_status.incoming.len(), 1);
    assert!(preflight.can_merge_cleanly);
    let token = preflight.token.unwrap();
    assert!(token.local);
    assert_eq!(token.variation, "main");
    assert_eq!(token.source.as_deref(), Some("review"));

    let merged = workspace
        .merge_variation(token, "Merge review\n\nTake the review edits")
        .unwrap();

    assert_eq!(merged.version.label, "Merge review");
    assert_eq!(
        merged.version.comment.as_deref(),
        Some("Take the review edits")
    );
    assert_eq!(head_parents(temp.path()), vec![main_tip, review_tip]);
    assert_eq!(workspace.current_variation().unwrap(), "main");
    assert_eq!(read_file(temp.path(), "a.md"), "a main");
    assert_eq!(read_file(temp.path(), "b.md"), "b review");
    assert_eq!(branch_tip(temp.path(), "review"), review_tip);
    assert!(workspace.changed_files().unwrap().is_empty());
}

#[test]
fn merge_variation_does_not_fast_forward() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());
    write_file(temp.path(), "a.md", "base");
    workspace.save_version("Base").unwrap();
    workspace.create_variation("review").unwrap();
    switch(&workspace, "review");
    write_file(temp.path(), "a.md", "review");
    workspace.save_version("Review edit").unwrap();
    switch(&workspace, "main");
    let main_tip = branch_tip(temp.path(), "main");
    let review_tip = branch_tip(temp.path(), "review");

    let preflight = workspace
        .preflight_merge_variation(&VariationId::from("review"))
        .unwrap();
    assert_eq!(preflight.sync_status.state, SyncState::IncomingAvailable);
    let merged = workspace
        .merge_variation(preflight.token.unwrap(), "Bring in review")
        .unwrap();

    assert_eq!(merged.version.label, "Bring in review");
    assert_eq!(head_parents(temp.path()), vec![main_tip, review_tip]);
    assert_eq!(read_file(temp.path(), "a.md"), "review");
}

#[test]
fn merge_variation_contract_resolves_conflicts_with_attribution_and_events() {
    let temp = tempfile::tempdir().unwrap();
    diverged(temp.path(), ("a.md", "a main"), ("a.md", "a review"));

    let preflight = preflight_merge_variation(preflight_request(temp.path(), "review")).unwrap();
    assert!(!preflight.can_merge_cleanly);
    assert_eq!(preflight.conflicts.len(), 1);
    let view = merge_conflict_view_model(&preflight);
    assert_eq!(view.files.len(), 1);
    let token = preflight.token.clone().unwrap();

    let error = into_tauri_result(merge_variation(MergeVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        source_variation_id: "review".to_string(),
        label: "Unresolved".to_string(),
    }))
    .unwrap_err();
    assert_eq!(error.code, "preflight_failed");
    assert_eq!(read_file(temp.path(), "a.md"), "a main");

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
    let mut events = Vec::new();
    let merged = {
        let mut context = DraftlineCommandContext::new()
            .with_contributor_profile(profile)
            .with_event_sink(|event| events.push(event));
        draftline::tauri_contract::merge_variation_with_resolutions_with_context(
            &mut context,
            MergeVariationWithResolutionsRequest {
                workspace_path: temp.path().to_path_buf(),
                label: "Resolved\n\nKept both".to_string(),
                token,
                resolutions: vec![MergeConflictResolution::new(
                    preflight.conflicts[0].path.clone(),
                    MergeResolutionChoice::UseContent {
                        content: "a resolved".to_string(),
                    },
                )],
            },
        )
        .unwrap()
    };

    assert_eq!(merged.merge.version.label, "Resolved");
    assert_eq!(merged.merge.version.author.name, "Profile Author");
    assert_eq!(merged.merge.version.saved_by.name, "Profile Service");
    assert_eq!(read_file(temp.path(), "a.md"), "a resolved");
    assert_eq!(head_parents(temp.path()).len(), 2);
    assert!(merged.postconditions.errors.is_empty());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, DraftlineEventKind::HistoryChanged);
}

#[test]
fn merge_variation_contract_emits_history_changed() {
    let temp = tempfile::tempdir().unwrap();
    diverged(temp.path(), ("a.md", "a main"), ("b.md", "b review"));
    let mut events = Vec::new();
    {
        let mut context = DraftlineCommandContext::new().with_event_sink(|e| events.push(e));
        merge_variation_with_context(
            &mut context,
            MergeVariationRequest {
                workspace_path: temp.path().to_path_buf(),
                source_variation_id: "review".to_string(),
                label: "Merge review".to_string(),
            },
        )
        .unwrap();
    }
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, DraftlineEventKind::HistoryChanged);
    assert_eq!(read_file(temp.path(), "b.md"), "b review");
}

#[test]
fn merge_variation_refuses_unsaved_changes() {
    let temp = tempfile::tempdir().unwrap();
    diverged(temp.path(), ("a.md", "a main"), ("b.md", "b review"));
    write_file(temp.path(), "a.md", "unsaved");

    let preflight = preflight_merge_variation(preflight_request(temp.path(), "review")).unwrap();
    assert!(preflight.token.is_none());
    assert_eq!(preflight.dirty_files.len(), 1);

    let error = into_tauri_result(merge_variation(MergeVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        source_variation_id: "review".to_string(),
        label: "Merge".to_string(),
    }))
    .unwrap_err();
    assert_eq!(error.code, "preflight_failed");
    assert_eq!(read_file(temp.path(), "a.md"), "unsaved");
    assert_eq!(read_file(temp.path(), "b.md"), "b base");
}

#[test]
fn merge_variation_reports_nothing_to_merge_and_missing_sources() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());
    write_file(temp.path(), "a.md", "base");
    workspace.save_version("Base").unwrap();
    workspace.create_variation("review").unwrap();
    write_file(temp.path(), "a.md", "main moved on");
    workspace.save_version("Main edit").unwrap();

    let preflight = preflight_merge_variation(preflight_request(temp.path(), "review")).unwrap();
    assert_eq!(preflight.sync_status.state, SyncState::LocalAhead);
    assert!(preflight.token.is_none());

    let error = into_tauri_result(merge_variation(MergeVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        source_variation_id: "review".to_string(),
        label: "Merge".to_string(),
    }))
    .unwrap_err();
    assert_eq!(error.code, "nothing_to_merge");
    assert_eq!(error.details.unwrap()["branch"], "review");

    let error = into_tauri_result(merge_variation(MergeVariationRequest {
        workspace_path: temp.path().to_path_buf(),
        source_variation_id: "missing".to_string(),
        label: "Merge".to_string(),
    }))
    .unwrap_err();
    assert_eq!(error.code, "variation_not_found");
}

#[test]
fn merge_tokens_are_not_interchangeable() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = diverged(temp.path(), ("a.md", "a main"), ("b.md", "b review"));
    let mut token = workspace
        .preflight_merge_variation(&VariationId::from("review"))
        .unwrap()
        .token
        .unwrap();

    let error = into_tauri_result(merge_incoming_with_resolutions(
        MergeIncomingWithResolutionsRequest {
            workspace_path: temp.path().to_path_buf(),
            remote: "origin".to_string(),
            label: "Wrong door".to_string(),
            token: token.clone(),
            resolutions: Vec::new(),
        },
    ))
    .unwrap_err();
    assert_eq!(error.code, "invalid_merge_token");

    token.local = false;
    let error = into_tauri_result(merge_variation_with_resolutions(
        MergeVariationWithResolutionsRequest {
            workspace_path: temp.path().to_path_buf(),
            label: "Wrong door".to_string(),
            token: token.clone(),
            resolutions: Vec::new(),
        },
    ))
    .unwrap_err();
    assert_eq!(error.code, "invalid_merge_token");
    assert!(matches!(
        workspace.merge_variation(token, "Wrong door"),
        Err(DraftlineError::InvalidMergeToken(_))
    ));
    assert_eq!(head_parents(temp.path()).len(), 1);
}

#[test]
fn merge_variation_rejects_a_token_after_the_source_moves() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = diverged(temp.path(), ("a.md", "a main"), ("b.md", "b review"));
    let token = workspace
        .preflight_merge_variation(&VariationId::from("review"))
        .unwrap()
        .token
        .unwrap();

    switch(&workspace, "review");
    write_file(temp.path(), "b.md", "b review again");
    workspace.save_version("Review second edit").unwrap();
    switch(&workspace, "main");

    let error = into_tauri_result(merge_variation_with_resolutions(
        MergeVariationWithResolutionsRequest {
            workspace_path: temp.path().to_path_buf(),
            label: "Stale".to_string(),
            token: token.clone(),
            resolutions: Vec::new(),
        },
    ))
    .unwrap_err();
    assert_eq!(error.code, "local_state_changed");
    assert!(matches!(
        workspace.merge_variation(token, "Stale"),
        Err(DraftlineError::LocalStateChanged { .. })
    ));
    assert_eq!(head_parents(temp.path()).len(), 1);
    assert_eq!(read_file(temp.path(), "b.md"), "b base");
}
