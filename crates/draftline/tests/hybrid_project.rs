//! A Draftline project living as a folder inside a developer's code repository.

use std::fs;
use std::path::{Path, PathBuf};

use draftline::{ContentPolicy, DraftlineError, FileHazardKind, RemoteOptions, Workspace};

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn commit_all(repo: &git2::Repository, message: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    let parents = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .into_iter()
        .collect::<Vec<_>>();
    let parents = parents.iter().collect::<Vec<_>>();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )
    .unwrap()
}

/// A code repository with application code and an existing project folder.
fn code_repo() -> (tempfile::TempDir, git2::Repository) {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(temp.path(), &options).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Dev").unwrap();
        config.set_str("user.email", "dev@example.test").unwrap();
    }
    write_file(temp.path(), "README.md", "# app\n");
    write_file(temp.path(), "src/main.rs", "fn main() {}\n");
    write_file(temp.path(), "cutready/intro.sk", "scene one\n");
    commit_all(&repo, "Initial app");
    (temp, repo)
}

fn stage(repo: &git2::Repository, relative: &str) {
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    index.add_path(Path::new(relative)).unwrap();
    index.write().unwrap();
}

fn status(repo: &git2::Repository, relative: &str) -> git2::Status {
    repo.status_file(Path::new(relative)).unwrap()
}

/// File text with line endings normalized, since checkouts honor `core.autocrlf`.
fn read(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative))
        .unwrap()
        .replace("\r\n", "\n")
}

fn head_paths_changed(repo: &git2::Repository) -> Vec<PathBuf> {
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let parent = head.parent(0).unwrap();
    repo.diff_tree_to_tree(
        Some(&parent.tree().unwrap()),
        Some(&head.tree().unwrap()),
        None,
    )
    .unwrap()
    .deltas()
    .map(|delta| delta.new_file().path().unwrap().to_path_buf())
    .collect()
}

fn hazard_kinds(error: DraftlineError) -> Vec<(PathBuf, FileHazardKind)> {
    match error {
        DraftlineError::PreflightFailed(report) => report
            .file_hazards
            .into_iter()
            .map(|hazard| (hazard.path, hazard.kind))
            .collect(),
        other => panic!("expected a preflight failure, got {other:?}"),
    }
}

fn cutready_policy() -> ContentPolicy {
    ContentPolicy::new()
        .include_extensions(["sk", "md"])
        .unwrap()
        .include(".cutready/projects.json")
        .unwrap()
}

#[test]
fn init_in_project_folder_joins_enclosing_repo_without_nesting() {
    let (temp, _repo) = code_repo();
    let project = temp.path().join("cutready");

    let workspace = Workspace::init_with_policy(&project, cutready_policy()).unwrap();

    assert!(!project.join(".git").exists(), "must not nest a repository");
    assert_eq!(workspace.scope(), Path::new("cutready"));
    assert_eq!(
        fs::canonicalize(workspace.root()).unwrap(),
        fs::canonicalize(temp.path()).unwrap()
    );
    assert_eq!(
        fs::canonicalize(workspace.project_root()).unwrap(),
        fs::canonicalize(&project).unwrap()
    );
}

#[test]
fn init_creates_new_project_folder_inside_repo_without_nesting() {
    let (temp, _repo) = code_repo();
    let project = temp.path().join("docs").join("pitch");

    let workspace = Workspace::init_with_policy(&project, cutready_policy()).unwrap();

    assert!(project.is_dir());
    assert!(!project.join(".git").exists());
    assert_eq!(workspace.scope(), PathBuf::from("docs").join("pitch"));
}

#[test]
fn changes_outside_project_folder_are_invisible() {
    let (temp, _repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();

    write_file(temp.path(), "README.md", "# app edited by a developer\n");
    write_file(temp.path(), "notes.md", "dev notes\n");
    write_file(temp.path(), "src/lib.rs", "pub fn x() {}\n");
    assert!(workspace.changed_files().unwrap().is_empty());
    assert!(!workspace.is_dirty().unwrap());

    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    write_file(temp.path(), "cutready/.cutready/projects.json", "{}\n");
    write_file(temp.path(), "cutready/scratch.tmp", "runtime\n");
    let changed = workspace
        .changed_files()
        .unwrap()
        .into_iter()
        .map(|file| file.path)
        .collect::<Vec<_>>();
    assert_eq!(
        changed,
        vec![
            PathBuf::from("cutready/.cutready/projects.json"),
            PathBuf::from("cutready/intro.sk"),
        ]
    );
}

#[test]
fn save_commits_only_project_content() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();

    write_file(temp.path(), "README.md", "# app edited by a developer\n");
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    workspace.save_version("Revise intro").unwrap();

    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let parent = head.parent(0).unwrap();
    let diff = repo
        .diff_tree_to_tree(
            Some(&parent.tree().unwrap()),
            Some(&head.tree().unwrap()),
            None,
        )
        .unwrap();
    let paths = diff
        .deltas()
        .map(|delta| delta.new_file().path().unwrap().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec![PathBuf::from("cutready/intro.sk")]);
    assert_eq!(
        fs::read_to_string(temp.path().join("README.md")).unwrap(),
        "# app edited by a developer\n",
        "developer's uncommitted edit must survive"
    );
    let status = repo.status_file(Path::new("README.md")).unwrap();
    assert!(status.contains(git2::Status::WT_MODIFIED));
}

#[test]
fn reopening_project_folder_keeps_scope() {
    let (temp, _repo) = code_repo();
    let project = temp.path().join("cutready");
    Workspace::init_with_policy(&project, cutready_policy()).unwrap();

    let workspace = Workspace::open_with_policy(&project, cutready_policy()).unwrap();

    assert_eq!(workspace.scope(), Path::new("cutready"));
}

#[test]
fn standalone_project_is_unscoped() {
    let temp = tempfile::tempdir().unwrap();

    let workspace = Workspace::init_with_policy(temp.path(), cutready_policy()).unwrap();

    assert!(temp.path().join(".git").exists());
    assert_eq!(workspace.scope(), Path::new(""));
    write_file(temp.path(), "intro.sk", "scene\n");
    assert_eq!(workspace.changed_files().unwrap().len(), 1);
}

#[test]
fn project_folder_in_linked_worktree_scopes_to_worktree() {
    let (temp, repo) = code_repo();
    let worktrees = tempfile::tempdir().unwrap();
    let worktree_path = worktrees.path().join("session");
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let branch = repo.branch("dev-session", &head, false).unwrap();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(branch.get()));
    repo.worktree("session", &worktree_path, Some(&options))
        .unwrap();

    let workspace =
        Workspace::init_with_policy(worktree_path.join("cutready"), cutready_policy()).unwrap();

    assert_eq!(workspace.scope(), Path::new("cutready"));
    assert_eq!(
        fs::canonicalize(workspace.root()).unwrap(),
        fs::canonicalize(&worktree_path).unwrap()
    );
    assert_eq!(workspace.current_variation().unwrap(), "dev-session");

    write_file(&worktree_path, "cutready/intro.sk", "worktree edit\n");
    workspace.save_version("Worktree save").unwrap();
    let main_intro = fs::read_to_string(temp.path().join("cutready/intro.sk")).unwrap();
    assert_eq!(main_intro, "scene one\n", "main checkout untouched");
    let session_tip = repo
        .find_branch("dev-session", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(session_tip.summary(), Some("Worktree save"));
}

#[test]
fn scope_with_glob_characters_is_literal_and_excludes_siblings() {
    let (temp, repo) = code_repo();
    write_file(temp.path(), "proj[1]/intro.sk", "one\n");
    write_file(temp.path(), "proj1/intro.sk", "sibling\n");
    write_file(temp.path(), "proj[1]-old/intro.sk", "sibling\n");
    commit_all(&repo, "Projects");
    for dir in ["proj[1]", "proj1", "proj[1]-old"] {
        write_file(temp.path(), &format!("{dir}/intro.sk"), "edited\n");
    }
    let workspace =
        Workspace::init_with_policy(temp.path().join("proj[1]"), cutready_policy()).unwrap();

    let changed = workspace
        .changed_files()
        .unwrap()
        .into_iter()
        .map(|file| file.path)
        .collect::<Vec<_>>();
    assert_eq!(changed, vec![PathBuf::from("proj[1]/intro.sk")]);

    let diff = workspace
        .diff_workspace_file("proj[1]/intro.sk")
        .unwrap()
        .unwrap();
    assert!(diff.file.is_some());
    assert!(diff.patch.unwrap().contains("proj[1]/intro.sk"));
}

#[test]
fn version_diffs_exclude_files_outside_project_folder() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    let base = workspace.versions().unwrap()[0].id().clone();

    write_file(temp.path(), "README.md", "# dev commit\n");
    write_file(temp.path(), "cutready/intro.sk", "scene two\n");
    commit_all(&repo, "Dev touches both");
    let tip = workspace.versions().unwrap()[0].id().clone();
    write_file(temp.path(), "src/main.rs", "fn main() { dev(); }\n");

    for diff in [
        workspace.diff_versions(&base, &tip).unwrap(),
        workspace.diff_version_to_workspace(&base).unwrap(),
    ] {
        let paths = diff.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths, vec![PathBuf::from("cutready/intro.sk")]);
        let patch = diff.patch.unwrap();
        assert!(!patch.contains("README.md") && !patch.contains("src/main.rs"));
    }
}

#[cfg(unix)]
#[test]
fn project_symlinked_outside_repo_is_rejected() {
    let (temp, _repo) = code_repo();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), temp.path().join("linked")).unwrap();

    let error = Workspace::open_with_policy(temp.path().join("linked"), cutready_policy());

    assert!(error.is_err(), "must not silently widen scope to the whole repo");
}

#[test]
fn save_leaves_developer_staged_changes_staged_and_uncommitted() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    write_file(temp.path(), "src/lib.rs", "pub fn staged() {}\n");
    stage(&repo, "src/lib.rs");
    write_file(temp.path(), "README.md", "# staged readme\n");
    stage(&repo, "README.md");

    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    workspace.save_version("Revise intro").unwrap();

    assert_eq!(head_paths_changed(&repo), vec![PathBuf::from("cutready/intro.sk")]);
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
    assert_eq!(status(&repo, "README.md"), git2::Status::INDEX_MODIFIED);
    assert_eq!(status(&repo, "cutready/intro.sk"), git2::Status::CURRENT);
}

#[test]
fn shelve_and_apply_leave_code_alone() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    write_file(temp.path(), "README.md", "# dev edit\n");
    write_file(temp.path(), "src/lib.rs", "pub fn staged() {}\n");
    stage(&repo, "src/lib.rs");
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    write_file(temp.path(), "cutready/outro.sk", "the end\n");

    workspace.shelve_changes("wip").unwrap();

    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");
    assert!(!temp.path().join("cutready/outro.sk").exists());
    assert_eq!(read(temp.path(), "README.md"), "# dev edit\n");
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);

    // The developer commits code after the shelf was made.
    commit_all(&repo, "Dev work");
    write_file(temp.path(), "src/main.rs", "fn main() { uncommitted(); }\n");

    workspace.apply_shelf("wip").unwrap();

    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one, revised\n");
    assert_eq!(read(temp.path(), "cutready/outro.sk"), "the end\n");
    assert_eq!(read(temp.path(), "README.md"), "# dev edit\n", "dev commit not reverted");
    assert_eq!(read(temp.path(), "src/lib.rs"), "pub fn staged() {}\n");
    assert_eq!(read(temp.path(), "src/main.rs"), "fn main() { uncommitted(); }\n");
    assert_eq!(status(&repo, "cutready/intro.sk"), git2::Status::WT_MODIFIED, "not staged");
    assert_eq!(status(&repo, "README.md"), git2::Status::CURRENT);
}

#[test]
fn restore_rolls_back_only_project_content() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    let original = workspace.versions().unwrap()[0].id().clone();
    write_file(temp.path(), "src/main.rs", "fn main() { committed(); }\n");
    commit_all(&repo, "Dev commit");
    write_file(temp.path(), "cutready/intro.sk", "scene two\n");
    workspace.save_version("Scene two").unwrap();
    write_file(temp.path(), "README.md", "# uncommitted\n");
    write_file(temp.path(), "src/lib.rs", "pub fn staged() {}\n");
    stage(&repo, "src/lib.rs");

    workspace
        .restore_version_as_new_save(&original, "Back to scene one")
        .unwrap();

    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");
    assert_eq!(head_paths_changed(&repo), vec![PathBuf::from("cutready/intro.sk")]);
    let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
    let main_rs = head_tree.get_path(Path::new("src/main.rs")).unwrap();
    let blob = repo.find_blob(main_rs.id()).unwrap();
    assert_eq!(blob.content(), b"fn main() { committed(); }\n", "dev commit kept");
    assert_eq!(read(temp.path(), "README.md"), "# uncommitted\n");
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
    assert_eq!(status(&repo, "cutready/intro.sk"), git2::Status::CURRENT);
}

#[test]
fn unscoped_restore_refuses_to_overwrite_local_non_content_edits() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init_with_policy(temp.path(), cutready_policy()).unwrap();
    let repo = git2::Repository::open(temp.path()).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Dev").unwrap();
        config.set_str("user.email", "dev@example.test").unwrap();
    }
    write_file(temp.path(), "intro.sk", "one\n");
    write_file(temp.path(), "build.txt", "v1\n");
    commit_all(&repo, "v1");
    let v1 = workspace.versions().unwrap()[0].id().clone();
    write_file(temp.path(), "build.txt", "v2\n");
    commit_all(&repo, "v2");
    write_file(temp.path(), "build.txt", "local\n");

    let error = workspace
        .restore_version_as_new_save(&v1, "Restore")
        .unwrap_err();

    assert_eq!(
        hazard_kinds(error),
        vec![(PathBuf::from("build.txt"), FileHazardKind::LocalChanges)]
    );
    assert_eq!(read(temp.path(), "build.txt"), "local\n");
}

/// A bare `origin` with `main` pushed from `repo`, plus a second clone.
fn with_remote(repo: &git2::Repository) -> (tempfile::TempDir, tempfile::TempDir, git2::Repository) {
    let bare = tempfile::tempdir().unwrap();
    let mut bare_options = git2::RepositoryInitOptions::new();
    bare_options.bare(true).initial_head("main");
    git2::Repository::init_opts(bare.path(), &bare_options).unwrap();
    let mut origin = repo.remote("origin", bare.path().to_str().unwrap()).unwrap();
    origin
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let other = git2::Repository::clone(bare.path().to_str().unwrap(), other_dir.path()).unwrap();
    {
        let mut config = other.config().unwrap();
        config.set_str("user.name", "Other dev").unwrap();
        config.set_str("user.email", "other@example.test").unwrap();
    }
    (bare, other_dir, other)
}

fn push_main(repo: &git2::Repository) {
    repo.find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap();
}

#[test]
fn apply_incoming_keeps_unrelated_local_code_edits_and_refuses_overlaps() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();

    write_file(other_dir.path(), "README.md", "# from teammate\n");
    write_file(other_dir.path(), "cutready/intro.sk", "teammate scene\n");
    commit_all(&other, "Teammate");
    push_main(&other);
    write_file(temp.path(), "src/main.rs", "fn main() { local(); }\n");
    write_file(temp.path(), "src/lib.rs", "pub fn staged() {}\n");
    stage(&repo, "src/lib.rs");

    let result = workspace
        .apply_incoming("origin", &mut RemoteOptions::new())
        .unwrap();

    assert_eq!(result.applied_count, 1);
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "teammate scene\n");
    assert_eq!(read(temp.path(), "README.md"), "# from teammate\n");
    assert_eq!(read(temp.path(), "src/main.rs"), "fn main() { local(); }\n");
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
    assert_eq!(status(&repo, "README.md"), git2::Status::CURRENT);

    write_file(other_dir.path(), "src/main.rs", "fn main() { teammate(); }\n");
    commit_all(&other, "Teammate code");
    push_main(&other);
    let before = repo.head().unwrap().target().unwrap();

    let error = workspace
        .apply_incoming("origin", &mut RemoteOptions::new())
        .unwrap_err();

    assert_eq!(
        hazard_kinds(error),
        vec![(PathBuf::from("src/main.rs"), FileHazardKind::LocalChanges)]
    );
    assert_eq!(repo.head().unwrap().target().unwrap(), before);
    assert_eq!(read(temp.path(), "src/main.rs"), "fn main() { local(); }\n");
}

#[test]
fn save_refuses_content_that_needs_a_git_filter() {
    let (temp, repo) = code_repo();
    write_file(temp.path(), ".gitattributes", "*.sk filter=lfs diff=lfs merge=lfs -text\n");
    commit_all(&repo, "Track sketches with LFS");
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");

    let error = workspace.save_version("Revise intro").unwrap_err();

    assert_eq!(
        hazard_kinds(error),
        vec![(PathBuf::from("cutready/intro.sk"), FileHazardKind::GitFilter)]
    );
}


#[test]
fn save_of_a_rename_out_of_the_project_leaves_the_target_to_the_developer() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    fs::create_dir_all(temp.path().join("notes")).unwrap();
    fs::rename(
        temp.path().join("cutready/intro.sk"),
        temp.path().join("notes/intro.sk"),
    )
    .unwrap();
    {
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        index.remove_path(Path::new("cutready/intro.sk")).unwrap();
        index.add_path(Path::new("notes/intro.sk")).unwrap();
        index.write().unwrap();
    }

    workspace.save_version("Move intro").unwrap();

    assert_eq!(
        head_paths_changed(&repo),
        vec![PathBuf::from("cutready/intro.sk")]
    );
    assert!(status(&repo, "notes/intro.sk").contains(git2::Status::INDEX_NEW));
}

#[test]
fn save_refuses_a_rename_onto_a_filtered_path() {
    let (temp, repo) = code_repo();
    write_file(temp.path(), ".gitattributes", "cutready/lfs/** filter=lfs -text\n");
    commit_all(&repo, "LFS folder");
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    fs::create_dir_all(temp.path().join("cutready/lfs")).unwrap();
    fs::rename(
        temp.path().join("cutready/intro.sk"),
        temp.path().join("cutready/lfs/intro.sk"),
    )
    .unwrap();
    {
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        index.remove_path(Path::new("cutready/intro.sk")).unwrap();
        index.add_path(Path::new("cutready/lfs/intro.sk")).unwrap();
        index.write().unwrap();
    }

    let error = workspace.save_version("Move intro").unwrap_err();

    assert_eq!(
        hazard_kinds(error),
        vec![(PathBuf::from("cutready/lfs/intro.sk"), FileHazardKind::GitFilter)]
    );
}

fn unscoped_repo() -> (tempfile::TempDir, Workspace, git2::Repository) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::init_with_policy(temp.path(), cutready_policy()).unwrap();
    let repo = git2::Repository::open(temp.path()).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Dev").unwrap();
        config.set_str("user.email", "dev@example.test").unwrap();
    }
    (temp, workspace, repo)
}

fn delete_and_commit(repo: &git2::Repository, root: &Path, relative: &str, message: &str) {
    let path = root.join(relative);
    if path.is_dir() {
        fs::remove_dir_all(&path).unwrap();
    } else {
        fs::remove_file(&path).unwrap();
    }
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    index.remove_all([relative], None).unwrap();
    index.write().unwrap();
    commit_all(repo, message);
}

#[test]
fn restore_refuses_when_an_untracked_folder_blocks_a_file() {
    let (temp, workspace, repo) = unscoped_repo();
    write_file(temp.path(), "intro.sk", "one\n");
    write_file(temp.path(), "assets/logo", "logo\n");
    commit_all(&repo, "v1");
    let v1 = workspace.versions().unwrap()[0].id().clone();
    delete_and_commit(&repo, temp.path(), "assets/logo", "drop logo");
    write_file(temp.path(), "assets/logo/notes.txt", "mine\n");

    let error = workspace
        .restore_version_as_new_save(&v1, "Restore")
        .unwrap_err();

    assert!(!hazard_kinds(error).is_empty());
    assert_eq!(read(temp.path(), "assets/logo/notes.txt"), "mine\n");
    assert!(workspace.recovery_state().unwrap().is_none());
}

#[test]
fn restore_refuses_when_an_ignored_file_blocks_a_folder() {
    let (temp, workspace, repo) = unscoped_repo();
    write_file(temp.path(), ".gitignore", "/assets/logo\n");
    write_file(temp.path(), "intro.sk", "one\n");
    commit_all(&repo, "v0");
    write_file(temp.path(), "assets/logo/a.txt", "a\n");
    {
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        index.add_path(Path::new("assets/logo/a.txt")).unwrap();
        index.write().unwrap();
    }
    commit_all(&repo, "v1");
    let v1 = workspace.versions().unwrap()[0].id().clone();
    delete_and_commit(&repo, temp.path(), "assets/logo", "drop logo");
    write_file(temp.path(), "assets/logo", "ignored build output\n");

    let error = workspace
        .restore_version_as_new_save(&v1, "Restore")
        .unwrap_err();

    let kinds = hazard_kinds(error);
    assert!(
        kinds.iter().any(|(_, kind)| *kind == FileHazardKind::Ignored),
        "{kinds:?}"
    );
    assert_eq!(read(temp.path(), "assets/logo"), "ignored build output\n");
    assert!(workspace.recovery_state().unwrap().is_none());
}

#[test]
fn shelving_a_rename_out_of_the_project_keeps_the_developer_target() {
    let (temp, repo) = code_repo();
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    fs::create_dir_all(temp.path().join("notes")).unwrap();
    fs::rename(
        temp.path().join("cutready/intro.sk"),
        temp.path().join("notes/intro.sk"),
    )
    .unwrap();
    {
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        index.remove_path(Path::new("cutready/intro.sk")).unwrap();
        index.add_path(Path::new("notes/intro.sk")).unwrap();
        index.write().unwrap();
    }

    workspace.shelve_changes("park-move").unwrap();

    assert_eq!(read(temp.path(), "notes/intro.sk"), "scene one\n");
    assert!(status(&repo, "notes/intro.sk").contains(git2::Status::INDEX_NEW));
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");
}
