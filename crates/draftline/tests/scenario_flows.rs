use std::fs;
use std::path::Path;

use draftline::{
    ContentPolicy, DiagnosticCode, DraftlineError, RemoteOptions, RestoreVersionTarget,
    SupportRefScope, SwitchPolicy, SyncState, VariationId, VariationMetadata, Workspace,
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

fn configure_identity(root: &Path, name: &str) {
    let repo = git2::Repository::open(root).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", name).unwrap();
    config
        .set_str("user.email", "scenario@example.test")
        .unwrap();
}

fn init_bare_remote(root: &Path) {
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    git2::Repository::init_opts(root, &options).unwrap();
}

#[test]
fn scenario_variation_restore_and_support_ref_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Author");

    write_file(workspace.root(), "story.md", "first draft");
    let base = workspace.save_version("Initial draft").unwrap();
    write_file(workspace.root(), "story.md", "second draft");
    workspace.save_version("Second draft").unwrap();
    let original_variation_name = workspace.current_variation().unwrap();

    let alternate = workspace
        .create_variation_from_with_metadata(
            base.id(),
            "alternate",
            VariationMetadata::new()
                .with_label("Alternate direction")
                .with_slug("alternate-direction"),
        )
        .unwrap();
    assert_eq!(alternate.display_label(), "Alternate direction");

    write_file(workspace.root(), "scratch.md", "unfinished");
    let blocked = workspace.switch_variation(alternate.id(), SwitchPolicy::AbortIfDirty);
    assert!(matches!(
        blocked,
        Err(DraftlineError::PreflightFailed(report)) if report.operation == "switch_variation"
    ));

    let switched = workspace
        .switch_variation(
            alternate.id(),
            SwitchPolicy::SaveFirst {
                label: "Save before switch".to_string(),
            },
        )
        .unwrap();
    assert_eq!(switched.name, "alternate");
    assert_eq!(read_file(workspace.root(), "story.md"), "first draft");

    write_file(workspace.root(), "story.md", "alternate edit");
    workspace.save_version("Alternate edit").unwrap();
    let restored = workspace
        .restore_version_as_new_save(base.id(), "Restore initial as save")
        .unwrap();
    assert_eq!(restored.label, "Restore initial as save");
    assert_eq!(workspace.current_variation().unwrap(), "alternate");
    assert_eq!(read_file(workspace.root(), "story.md"), "first draft");

    let original_variation = VariationId::from(original_variation_name.clone());
    workspace.delete_variation(&original_variation).unwrap();
    let support_refs = workspace.list_support_refs(SupportRefScope::Local).unwrap();
    assert_eq!(support_refs.len(), 1);
    assert_eq!(
        support_refs[0].source_variation.as_deref(),
        Some(original_variation_name.as_str())
    );

    let restored_name = format!("restored-{original_variation_name}");
    let restore = workspace
        .preflight_restore_support_ref(&support_refs[0].id, &restored_name)
        .unwrap();
    assert!(restore.can_restore);
    let restored_variation = workspace.restore_support_ref(restore.token).unwrap();
    assert_eq!(restored_variation.name, restored_name);
}

#[test]
fn targeted_restore_creates_save_on_existing_variation_without_wrong_branch_write() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Targeted Restore");

    write_file(workspace.root(), "story.md", "base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "story.md", "main edit");
    workspace.save_version("Main edit").unwrap();
    let original_variation = workspace.current_variation().unwrap();

    let alternate = workspace.create_variation("alternate").unwrap();
    let restored = workspace
        .restore_version_as_new_save_to_variation(
            base.id(),
            "Restore base onto alternate",
            RestoreVersionTarget::Existing {
                variation: alternate.id().clone(),
            },
        )
        .unwrap();

    assert_eq!(restored.version.label, "Restore base onto alternate");
    assert_eq!(restored.target_variation.name, "alternate");
    assert_eq!(workspace.current_variation().unwrap(), "alternate");
    assert_eq!(read_file(workspace.root(), "story.md"), "base");

    workspace
        .switch_variation(
            &VariationId::from(original_variation.clone()),
            SwitchPolicy::AbortIfDirty,
        )
        .unwrap();
    assert_eq!(workspace.current_variation().unwrap(), original_variation);
    assert_eq!(read_file(workspace.root(), "story.md"), "main edit");
}

#[test]
fn targeted_restore_current_variation_matches_regular_restore_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Targeted Restore Current");

    write_file(workspace.root(), "story.md", "base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "story.md", "main edit");
    workspace.save_version("Main edit").unwrap();
    let original_variation = workspace.current_variation().unwrap();

    let restored = workspace
        .restore_version_as_new_save_to_variation(
            base.id(),
            "Restore base onto current",
            RestoreVersionTarget::Current,
        )
        .unwrap();

    assert_eq!(restored.version.label, "Restore base onto current");
    assert_eq!(restored.target_variation.name, original_variation);
    assert_eq!(workspace.current_variation().unwrap(), original_variation);
    assert_eq!(read_file(workspace.root(), "story.md"), "base");
}

#[test]
fn targeted_restore_creates_new_variation_and_preserves_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Targeted Restore New");

    write_file(workspace.root(), "story.md", "base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "story.md", "main edit");
    workspace.save_version("Main edit").unwrap();

    let restored = workspace
        .restore_version_as_new_save_to_variation(
            base.id(),
            "Restore base onto new variation",
            RestoreVersionTarget::New {
                name: "snapshot-preview".to_string(),
                metadata: VariationMetadata::new()
                    .with_label("Snapshot Preview")
                    .with_slug("snapshot-preview"),
            },
        )
        .unwrap();

    assert_eq!(workspace.current_variation().unwrap(), "snapshot-preview");
    assert_eq!(read_file(workspace.root(), "story.md"), "base");
    assert_eq!(
        restored.target_variation.display_label(),
        "Snapshot Preview"
    );
    assert_eq!(
        workspace
            .variations()
            .unwrap()
            .into_iter()
            .find(|variation| variation.name == "snapshot-preview")
            .unwrap()
            .metadata
            .slug
            .as_deref(),
        Some("snapshot-preview")
    );
}

#[test]
fn targeted_restore_fails_before_writing_for_target_errors() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Targeted Restore Errors");

    write_file(workspace.root(), "story.md", "base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "story.md", "main edit");
    workspace.save_version("Main edit").unwrap();
    let original_variation = workspace.current_variation().unwrap();

    let missing = workspace.restore_version_as_new_save_to_variation(
        base.id(),
        "Missing target",
        RestoreVersionTarget::Existing {
            variation: VariationId::from("missing-target"),
        },
    );
    assert!(matches!(
        missing,
        Err(DraftlineError::VariationNotFound(name)) if name == "missing-target"
    ));
    assert_eq!(workspace.current_variation().unwrap(), original_variation);
    assert_eq!(read_file(workspace.root(), "story.md"), "main edit");

    let collision = workspace.restore_version_as_new_save_to_variation(
        base.id(),
        "Collision target",
        RestoreVersionTarget::New {
            name: original_variation.clone(),
            metadata: VariationMetadata::default(),
        },
    );
    assert!(matches!(
        collision,
        Err(DraftlineError::VariationAlreadyExists(name)) if name == original_variation
    ));
    assert_eq!(workspace.current_variation().unwrap(), original_variation);
    assert_eq!(read_file(workspace.root(), "story.md"), "main edit");
}

#[test]
fn targeted_restore_fails_without_activating_target_when_restore_cannot_proceed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Targeted Restore Failure");

    write_file(workspace.root(), "story.md", "base");
    let base = workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "story.md", "main edit");
    workspace.save_version("Main edit").unwrap();
    let original_variation = workspace.current_variation().unwrap();
    let alternate = workspace.create_variation("alternate").unwrap();

    write_file(workspace.root(), "scratch.md", "unsaved");
    let blocked = workspace.restore_version_as_new_save_to_variation(
        base.id(),
        "Blocked restore",
        RestoreVersionTarget::Existing {
            variation: alternate.id().clone(),
        },
    );
    assert!(matches!(
        blocked,
        Err(DraftlineError::PreflightFailed(report))
            if report.operation == "restore_version_as_new_save_to_variation"
    ));
    assert_eq!(workspace.current_variation().unwrap(), original_variation);
    assert_eq!(read_file(workspace.root(), "story.md"), "main edit");
    assert_eq!(read_file(workspace.root(), "scratch.md"), "unsaved");
}

#[test]
fn scenario_shelf_apply_preview_and_delete_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Shelf");

    write_file(workspace.root(), "notes.md", "base");
    workspace.save_version("Base").unwrap();
    write_file(workspace.root(), "notes.md", "shelved work");

    let shelf = workspace.shelve_files("aside", ["notes.md"]).unwrap();
    assert_eq!(shelf.id, "aside");
    assert!(workspace.changes().unwrap().files.is_empty());
    assert_eq!(workspace.list_shelves().unwrap().len(), 1);

    let preview = workspace.preview_shelf("aside").unwrap();
    let notes_preview = preview
        .files
        .iter()
        .find(|file| file.path == Path::new("notes.md"))
        .unwrap();
    assert_eq!(notes_preview.content.as_deref(), Some("shelved work"));
    let preflight = workspace.preflight_apply_shelf("aside").unwrap();
    assert!(preflight.can_proceed);

    workspace.apply_shelf("aside").unwrap();
    assert_eq!(read_file(workspace.root(), "notes.md"), "shelved work");
    assert_eq!(workspace.list_shelves().unwrap().len(), 1);

    workspace.delete_shelf("aside").unwrap();
    assert!(workspace.list_shelves().unwrap().is_empty());
}

#[test]
fn scenario_collaboration_fast_forward_and_clean_merge() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root(), "Scenario Author");
    write_file(author.root(), "shared.md", "base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    author.publish_changes("origin").unwrap();

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
            .unwrap();
    configure_identity(teammate.root(), "Scenario Teammate");

    write_file(teammate.root(), "shared.md", "base\nteammate fast-forward");
    teammate.save_version("Teammate update").unwrap();
    teammate.publish_changes("origin").unwrap();

    author.fetch_remote("origin").unwrap();
    let incoming = author.preflight_apply_incoming("origin").unwrap();
    assert_eq!(incoming.sync_status.state, SyncState::IncomingAvailable);
    assert!(incoming.can_proceed);
    let applied = author
        .apply_incoming("origin", &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(applied.applied_count, 1);
    assert!(read_file(author.root(), "shared.md").contains("teammate fast-forward"));

    write_file(author.root(), "author.md", "local branch of work");
    author.save_version("Author local update").unwrap();
    write_file(teammate.root(), "teammate.md", "remote branch of work");
    teammate.save_version("Teammate divergent update").unwrap();
    teammate.publish_changes("origin").unwrap();

    author.fetch_remote("origin").unwrap();
    let status = author.sync_status("origin").unwrap();
    assert_eq!(status.state, SyncState::NeedsMerge);
    let merge = author.preflight_merge_incoming("origin").unwrap();
    assert!(merge.can_merge_cleanly);
    let token = merge.token.unwrap();
    let merged = author
        .merge_incoming(token, "Merge teammate work", &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(merged.version.label, "Merge teammate work");
    assert!(merged
        .merged_files
        .iter()
        .any(|path| path == Path::new("teammate.md")));
    assert_eq!(
        read_file(author.root(), "author.md"),
        "local branch of work"
    );
    assert!(read_file(author.root(), "shared.md").contains("teammate fast-forward"));
    assert_eq!(
        author.sync_status("origin").unwrap().state,
        SyncState::LocalAhead
    );
}

#[test]
fn scenario_collaboration_conflict_preflight_reports_without_mutating() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root(), "Scenario Author");
    write_file(author.root(), "shared.md", "base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    author.publish_changes("origin").unwrap();

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
            .unwrap();
    configure_identity(teammate.root(), "Scenario Teammate");

    write_file(author.root(), "shared.md", "ours");
    author.save_version("Author conflicting update").unwrap();
    write_file(teammate.root(), "shared.md", "theirs");
    teammate
        .save_version("Teammate conflicting update")
        .unwrap();
    teammate.publish_changes("origin").unwrap();

    author.fetch_remote("origin").unwrap();
    let preflight = author.preflight_merge_incoming("origin").unwrap();

    assert_eq!(preflight.sync_status.state, SyncState::NeedsMerge);
    assert!(!preflight.can_merge_cleanly);
    assert!(preflight.token.is_some());
    assert!(preflight.dirty_files.is_empty());
    assert!(preflight.file_hazards.is_empty());
    assert_eq!(preflight.conflicts.len(), 1);
    assert_eq!(preflight.conflicts[0].path, Path::new("shared.md"));
    assert_eq!(preflight.conflicts[0].ours.as_deref(), Some("ours"));
    assert_eq!(preflight.conflicts[0].theirs.as_deref(), Some("theirs"));
    assert!(!preflight.changed_workspace);
    assert_eq!(read_file(author.root(), "shared.md"), "ours");
}

#[test]
fn scenario_remote_support_refs_roundtrip_restore_and_local_expire() {
    let remote_dir = tempfile::tempdir().unwrap();
    init_bare_remote(remote_dir.path());

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root(), "Scenario Author");
    write_file(author.root(), "story.md", "base");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote_dir.path().to_string_lossy())
        .unwrap();
    author.publish_changes("origin").unwrap();

    let archived = author.create_variation("archived-direction").unwrap();
    author.delete_variation(archived.id()).unwrap();
    let local_support_refs = author.list_support_refs(SupportRefScope::Local).unwrap();
    assert_eq!(local_support_refs.len(), 1);
    assert_eq!(
        local_support_refs[0].source_variation.as_deref(),
        Some("archived-direction")
    );

    let publish = author.preflight_publish_support_refs("origin").unwrap();
    assert!(publish.can_publish);
    assert_eq!(publish.support_refs.len(), 1);
    author.publish_support_refs(publish.token).unwrap();
    assert!(
        !author
            .preflight_publish_support_refs("origin")
            .unwrap()
            .can_publish
    );

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
            .unwrap();
    configure_identity(teammate.root(), "Scenario Teammate");
    teammate.fetch_support_refs("origin").unwrap();
    let remote_support_refs = teammate
        .list_support_refs(SupportRefScope::RemoteTracking)
        .unwrap();
    assert_eq!(remote_support_refs.len(), 1);
    assert_eq!(
        remote_support_refs[0].source_variation.as_deref(),
        Some("archived-direction")
    );

    let restore = teammate
        .preflight_restore_support_ref(&remote_support_refs[0].id, "restored-direction")
        .unwrap();
    assert!(restore.can_restore);
    let restored = teammate.restore_support_ref(restore.token).unwrap();
    assert_eq!(restored.name, "restored-direction");

    let expire = author
        .preflight_expire_support_refs([local_support_refs[0].id.clone()])
        .unwrap();
    assert!(expire.can_expire);
    author.expire_support_refs(expire.token).unwrap();
    assert!(author
        .list_support_refs(SupportRefScope::Local)
        .unwrap()
        .is_empty());
}

#[test]
fn scenario_purge_api_is_explicitly_planning_only() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(temp.path()).unwrap();
    configure_identity(workspace.root(), "Scenario Author");
    write_file(workspace.root(), "secret.md", "secret");
    workspace.save_version("Secret").unwrap();

    let preflight = workspace.preflight_purge_content("secret.md").unwrap();

    assert_eq!(preflight.selector, "secret.md");
    assert!(preflight
        .affected_refs
        .iter()
        .any(|reference| reference == "refs/heads/main"));
    assert!(preflight
        .distributed_warning
        .contains("cannot guarantee deletion from existing clones"));
    let verification = workspace.verify_purge(preflight.token).unwrap();
    assert_eq!(verification.selector, "secret.md");
    assert!(verification.checked_refs > 0);
    assert!(!verification.verified_absent);
    assert!(verification
        .limitations
        .iter()
        .any(|limitation| limitation.contains("cannot inspect existing clones")));
}

#[test]
fn scenario_content_policy_api_surfaces_ignored_tracked_content() {
    let temp = tempfile::tempdir().unwrap();
    let policy = ContentPolicy::new().include("content").unwrap();
    let workspace = Workspace::init_with_policy(temp.path(), policy.clone()).unwrap();
    configure_identity(workspace.root(), "Scenario Author");
    write_file(workspace.root(), ".gitignore", "content/hidden.md\n");
    write_file(
        workspace.root(),
        "content/hidden.md",
        "tracked by policy but ignored",
    );

    let diagnostics = workspace.policy_git_diagnostics().unwrap();
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::PolicyTrackedFileIgnored));

    let audit = workspace.audit_content_policy().unwrap();
    assert_eq!(audit.current_diagnostics, diagnostics);
    assert!(audit.historical_out_of_policy_paths.is_empty());

    let adoption = workspace.preflight_adopt_workspace(policy).unwrap();
    assert!(adoption
        .candidate_policy_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::PolicyTrackedFileIgnored));
    assert!(adoption.can_adopt);
}
