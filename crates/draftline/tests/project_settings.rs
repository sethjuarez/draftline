//! Project settings recorded in `.draftline`: id, base branch and commit template.

use std::fs;
use std::path::{Path, PathBuf};

use draftline::tauri_contract::{
    into_tauri_result, project_settings, update_project_settings_with_context,
    DraftlineCommandContext, UpdateProjectSettingsRequest, WorkspaceRequest,
};
use draftline::{
    ContentPolicy, Contributor, ContributorProfile, DraftlineError, ProjectSettingsUpdate,
    SwitchPolicy, VariationId, Workspace,
};

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn configure(repo: &git2::Repository) {
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Dev").unwrap();
    config.set_str("user.email", "dev@example.test").unwrap();
}

fn init(root: &Path) -> Workspace {
    let workspace = Workspace::init(root).unwrap();
    configure(&git2::Repository::open(root).unwrap());
    workspace
}

fn head(root: &Path) -> git2::Commit<'static> {
    let repo = Box::leak(Box::new(git2::Repository::discover(root).unwrap()));
    repo.head().unwrap().peel_to_commit().unwrap()
}

fn head_message(root: &Path) -> String {
    head(root).message().unwrap().to_string()
}

fn head_paths_changed(root: &Path) -> Vec<PathBuf> {
    let repo = Box::leak(Box::new(git2::Repository::discover(root).unwrap()));
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let parent_tree = head.parent(0).ok().map(|parent| parent.tree().unwrap());
    repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&head.tree().unwrap()), None)
        .unwrap()
        .deltas()
        .map(|delta| delta.new_file().path().unwrap().to_path_buf())
        .collect()
}

fn template(template: &str) -> ProjectSettingsUpdate {
    ProjectSettingsUpdate {
        base_branch: Some("main".to_string()),
        commit_template: Some(template.to_string()),
    }
}

#[test]
fn a_new_folder_has_no_settings_until_they_are_set() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());

    let settings = workspace.project_settings().unwrap();
    assert_eq!(settings.folder, PathBuf::new());
    assert_eq!(settings.id, None);
    assert_eq!(settings.base_branch, None);
    assert_eq!(settings.commit_template, None);
    // Existing markers keep their exact bytes.
    assert_eq!(
        fs::read_to_string(temp.path().join(".draftline")).unwrap(),
        "{\n  \"format\": 1,\n  \"folder\": \".\"\n}\n"
    );
}

#[test]
fn updating_settings_assigns_an_id_and_saves_only_the_marker() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());
    write_file(temp.path(), "a.md", "one");
    workspace.save_version("First").unwrap();
    write_file(temp.path(), "a.md", "unsaved");

    let settings = workspace
        .update_project_settings(template("docs(demo): {subject}"))
        .unwrap();

    let id = settings.id.clone().unwrap();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4");
    assert_eq!(settings.base_branch.as_deref(), Some("main"));
    assert_eq!(
        settings.commit_template.as_deref(),
        Some("docs(demo): {subject}")
    );
    assert_eq!(
        head_paths_changed(temp.path()),
        vec![PathBuf::from(".draftline")]
    );
    assert_eq!(
        head_message(temp.path()),
        "docs(demo): Update project settings"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("a.md")).unwrap(),
        "unsaved"
    );

    // The same settings again change nothing; the id stays.
    let tip = head(temp.path()).id();
    let again = workspace
        .update_project_settings(template("docs(demo): {subject}"))
        .unwrap();
    assert_eq!(again.id.as_deref(), Some(id.as_str()));
    assert_eq!(head(temp.path()).id(), tip);

    // Reopening reads the committed settings.
    let reopened = Workspace::open(temp.path()).unwrap();
    assert_eq!(reopened.project_settings().unwrap(), again);

    // Clearing keeps the id.
    let cleared = workspace
        .update_project_settings(ProjectSettingsUpdate::default())
        .unwrap();
    assert_eq!(cleared.id.as_deref(), Some(id.as_str()));
    assert_eq!(cleared.commit_template, None);
    assert_eq!(head_message(temp.path()), "Update project settings");
}

#[test]
fn the_commit_template_applies_to_saves_restores_and_merges() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());
    write_file(temp.path(), "a.md", "one");
    let first = workspace.save_version("First").unwrap();
    workspace
        .update_project_settings(template("docs(demo): {subject}"))
        .unwrap();

    write_file(temp.path(), "a.md", "two");
    let saved = workspace
        .save_version("Second draft\n\nWith a comment")
        .unwrap();
    assert_eq!(
        head_message(temp.path()),
        "docs(demo): Second draft\n\nWith a comment"
    );
    assert_eq!(saved.label, "docs(demo): Second draft");
    assert_eq!(saved.comment.as_deref(), Some("With a comment"));

    // A name that already fits the template isn't prefixed twice.
    write_file(temp.path(), "a.md", "three");
    workspace.save_version("docs(demo): Third").unwrap();
    assert_eq!(head_message(temp.path()), "docs(demo): Third");

    workspace
        .restore_version_as_new_save(first.id(), "Back to one")
        .unwrap();
    assert_eq!(head_message(temp.path()), "docs(demo): Back to one");

    workspace.create_variation("review").unwrap();
    workspace
        .switch_variation(&VariationId::from("review"), SwitchPolicy::AbortIfDirty)
        .unwrap();
    write_file(temp.path(), "b.md", "review");
    workspace.save_version("Review edit").unwrap();
    workspace
        .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
        .unwrap();
    write_file(temp.path(), "a.md", "main");
    workspace.save_version("Main edit").unwrap();
    let preflight = workspace
        .preflight_merge_variation(&VariationId::from("review"))
        .unwrap();
    workspace
        .merge_variation(preflight.token.unwrap(), "Bring in review")
        .unwrap();
    assert_eq!(head_message(temp.path()), "docs(demo): Bring in review");
    assert_eq!(head(temp.path()).parent_count(), 2);
}

#[test]
fn invalid_settings_are_refused_without_a_save() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());
    write_file(temp.path(), "a.md", "one");
    workspace.save_version("First").unwrap();
    let tip = head(temp.path()).id();
    let marker = fs::read(temp.path().join(".draftline")).unwrap();

    for update in [
        template("docs: no subject"),
        template("{subject} and {subject}"),
        template("docs:\n{subject}"),
        ProjectSettingsUpdate {
            base_branch: Some("bad..branch".to_string()),
            commit_template: None,
        },
    ] {
        let error = workspace.update_project_settings(update).unwrap_err();
        assert!(
            matches!(error, DraftlineError::InvalidProjectSettings(_)),
            "{error:?}"
        );
    }
    assert_eq!(head(temp.path()).id(), tip);
    assert_eq!(fs::read(temp.path().join(".draftline")).unwrap(), marker);
}

#[test]
fn markers_from_older_releases_and_unknown_fields_are_read() {
    let temp = tempfile::tempdir().unwrap();
    init(temp.path());
    write_file(
        temp.path(),
        ".draftline",
        "{\"format\":1,\"folder\":\".\",\"id\":\"abc\",\"commit_template\":\"chore: {subject}\",\"future\":true}",
    );

    let settings = Workspace::open(temp.path())
        .unwrap()
        .project_settings()
        .unwrap();
    assert_eq!(settings.id.as_deref(), Some("abc"));
    assert_eq!(
        settings.commit_template.as_deref(),
        Some("chore: {subject}")
    );
}

#[test]
fn an_update_matching_a_differently_written_marker_leaves_nothing_unsaved() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = init(temp.path());
    write_file(
        temp.path(),
        ".draftline",
        "{\"format\":1,\"folder\":\".\",\"id\":\"abc\",\"base_branch\":\"main\"}",
    );
    write_file(temp.path(), "a.md", "one");
    workspace.save_version("First").unwrap();

    let settings = workspace
        .update_project_settings(ProjectSettingsUpdate {
            base_branch: Some("main".to_string()),
            commit_template: None,
        })
        .unwrap();

    assert_eq!(settings.id.as_deref(), Some("abc"));
    assert!(workspace.changed_files().unwrap().is_empty());
}

fn code_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(temp.path(), &options).unwrap();
    configure(&repo);
    write_file(temp.path(), "README.md", "# app\n");
    write_file(temp.path(), "src/main.rs", "fn main() {}\n");
    write_file(temp.path(), "cutready/intro.sk", "scene one\n");
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Initial app",
        &tree,
        &[],
    )
    .unwrap();
    temp
}

fn cutready_policy() -> ContentPolicy {
    ContentPolicy::new()
        .include_extensions(["sk", "md"])
        .unwrap()
}

#[test]
fn hybrid_settings_leave_code_alone_and_follow_a_folder_move() {
    let temp = code_repo();
    let project = temp.path().join("cutready");
    let mut workspace = Workspace::init_with_policy(&project, cutready_policy()).unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { todo!() }\n");

    let settings = workspace
        .update_project_settings(template("docs(demo): {subject}"))
        .unwrap();
    assert_eq!(settings.folder, PathBuf::from("cutready"));
    assert_eq!(
        head_paths_changed(temp.path()),
        vec![PathBuf::from(".draftline")]
    );
    let marker = fs::read_to_string(temp.path().join(".draftline")).unwrap();
    assert!(marker.contains("\"folder\": \"cutready\""), "{marker}");

    write_file(&project, "intro.sk", "scene two\n");
    workspace.save_version("Scene two").unwrap();
    assert_eq!(head_message(temp.path()), "docs(demo): Scene two");
    assert_eq!(
        head_paths_changed(temp.path()),
        vec![PathBuf::from("cutready/intro.sk")]
    );

    let id = settings.id.unwrap();
    fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    workspace.move_folder("demos/pitch").unwrap();
    assert!(head_message(temp.path()).starts_with("docs(demo): Move Draftline folder"));
    let moved = Workspace::open(temp.path().join("demos/pitch"))
        .unwrap()
        .project_settings()
        .unwrap();
    assert_eq!(moved.folder, PathBuf::from("demos").join("pitch"));
    assert_eq!(moved.id.as_deref(), Some(id.as_str()));
    assert_eq!(
        moved.commit_template.as_deref(),
        Some("docs(demo): {subject}")
    );
    assert_eq!(moved.base_branch.as_deref(), Some("main"));
}

#[test]
fn a_folder_the_marker_does_not_name_cannot_read_or_change_settings() {
    let temp = code_repo();
    let project = temp.path().join("cutready");
    let workspace = Workspace::init_with_policy(&project, cutready_policy()).unwrap();
    workspace
        .update_project_settings(template("docs: {subject}"))
        .unwrap();
    let mut marker = fs::read_to_string(temp.path().join(".draftline")).unwrap();
    marker = marker.replace("\"cutready\"", "\"other\"");
    fs::write(temp.path().join(".draftline"), marker).unwrap();

    assert!(matches!(
        workspace.project_settings().unwrap_err(),
        DraftlineError::FolderIsElsewhere(_)
    ));
    let error = workspace
        .update_project_settings(template("docs: {subject}"))
        .unwrap_err();
    assert!(
        matches!(
            error,
            DraftlineError::FolderIsElsewhere(_) | DraftlineError::FolderMissing(_)
        ),
        "{error:?}"
    );
}

#[test]
fn contract_reads_and_updates_settings_with_attribution() {
    let temp = tempfile::tempdir().unwrap();
    init(temp.path());
    let profile = ContributorProfile::new(
        Contributor {
            name: "Host User".to_string(),
            email: Some("host@example.test".to_string()),
        },
        Contributor {
            name: "Host Service".to_string(),
            email: Some("service@example.test".to_string()),
        },
    );
    let mut context = DraftlineCommandContext::new().with_contributor_profile(profile);

    let settings = update_project_settings_with_context(
        &mut context,
        UpdateProjectSettingsRequest {
            workspace_path: temp.path().to_path_buf(),
            base_branch: Some("main".to_string()),
            commit_template: Some("docs: {subject}".to_string()),
        },
    )
    .unwrap();
    assert!(settings.id.is_some());
    assert_eq!(head(temp.path()).author().name(), Ok("Host User"));

    let read = project_settings(WorkspaceRequest {
        workspace_path: temp.path().to_path_buf(),
    })
    .unwrap();
    assert_eq!(read, settings);
    let json = serde_json::to_value(&read).unwrap();
    assert_eq!(json["commit_template"], "docs: {subject}");

    let error = into_tauri_result(update_project_settings_with_context(
        &mut DraftlineCommandContext::new(),
        UpdateProjectSettingsRequest {
            workspace_path: temp.path().to_path_buf(),
            base_branch: None,
            commit_template: Some("no placeholder".to_string()),
        },
    ))
    .unwrap_err();
    assert_eq!(error.code, "invalid_project_settings");
}
