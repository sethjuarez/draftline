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
        let paths = diff
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>();
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

    assert!(
        error.is_err(),
        "must not silently widen scope to the whole repo"
    );
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

    assert_eq!(
        head_paths_changed(&repo),
        vec![PathBuf::from("cutready/intro.sk")]
    );
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

    assert_eq!(
        read(temp.path(), "cutready/intro.sk"),
        "scene one, revised\n"
    );
    assert_eq!(read(temp.path(), "cutready/outro.sk"), "the end\n");
    assert_eq!(
        read(temp.path(), "README.md"),
        "# dev edit\n",
        "dev commit not reverted"
    );
    assert_eq!(read(temp.path(), "src/lib.rs"), "pub fn staged() {}\n");
    assert_eq!(
        read(temp.path(), "src/main.rs"),
        "fn main() { uncommitted(); }\n"
    );
    assert_eq!(
        status(&repo, "cutready/intro.sk"),
        git2::Status::WT_MODIFIED,
        "not staged"
    );
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
    assert_eq!(
        head_paths_changed(&repo),
        vec![PathBuf::from("cutready/intro.sk")]
    );
    let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
    let main_rs = head_tree.get_path(Path::new("src/main.rs")).unwrap();
    let blob = repo.find_blob(main_rs.id()).unwrap();
    assert_eq!(
        blob.content(),
        b"fn main() { committed(); }\n",
        "dev commit kept"
    );
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
fn with_remote(
    repo: &git2::Repository,
) -> (tempfile::TempDir, tempfile::TempDir, git2::Repository) {
    let bare = tempfile::tempdir().unwrap();
    let mut bare_options = git2::RepositoryInitOptions::new();
    bare_options.bare(true).initial_head("main");
    git2::Repository::init_opts(bare.path(), &bare_options).unwrap();
    let mut origin = repo
        .remote("origin", bare.path().to_str().unwrap())
        .unwrap();
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

    write_file(
        other_dir.path(),
        "src/main.rs",
        "fn main() { teammate(); }\n",
    );
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
    write_file(
        temp.path(),
        ".gitattributes",
        "*.sk filter=lfs diff=lfs merge=lfs -text\n",
    );
    commit_all(&repo, "Track sketches with LFS");
    let workspace =
        Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");

    let error = workspace.save_version("Revise intro").unwrap_err();

    assert_eq!(
        hazard_kinds(error),
        vec![(
            PathBuf::from("cutready/intro.sk"),
            FileHazardKind::GitFilter
        )]
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
    write_file(
        temp.path(),
        ".gitattributes",
        "cutready/lfs/** filter=lfs -text\n",
    );
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
        vec![(
            PathBuf::from("cutready/lfs/intro.sk"),
            FileHazardKind::GitFilter
        )]
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
        kinds
            .iter()
            .any(|(_, kind)| *kind == FileHazardKind::Ignored),
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

fn labels(versions: &[draftline::Version]) -> Vec<String> {
    versions
        .iter()
        .map(|version| version.label.clone())
        .collect()
}

fn scoped(temp: &tempfile::TempDir) -> Workspace {
    Workspace::init_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap()
}

#[test]
fn scoped_versions_skip_developer_commits_outside_the_project() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);

    write_file(temp.path(), "src/main.rs", "fn main() { run() }\n");
    commit_all(&repo, "Dev: wire up run");
    write_file(temp.path(), "cutready/intro.sk", "scene one, by dev\n");
    write_file(temp.path(), "src/lib.rs", "pub fn run() {}\n");
    commit_all(&repo, "Dev: tweak intro and code");
    write_file(temp.path(), "cutready/intro.sk", "scene one, by BU\n");
    workspace.save_version("BU revise").unwrap();

    assert_eq!(
        labels(&workspace.versions().unwrap()),
        ["BU revise", "Dev: tweak intro and code", "Initial app"]
    );
    let dev = &workspace.versions().unwrap()[1];
    assert_eq!(dev.author.name, "Dev");
}

#[test]
fn scoped_history_rewrites_parents_across_hidden_commits_and_pins_head() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    let initial = repo.head().unwrap().target().unwrap();

    write_file(temp.path(), "src/main.rs", "fn main() { a() }\n");
    commit_all(&repo, "Dev: a");
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    workspace.save_version("BU revise").unwrap();
    let save = repo.head().unwrap().target().unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { b() }\n");
    let dev_tip = commit_all(&repo, "Dev: b");

    assert_eq!(
        labels(&workspace.versions().unwrap()),
        ["BU revise", "Initial app"]
    );

    let history = workspace.history().unwrap();
    let ids = history
        .iter()
        .map(|entry| entry.version.id().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [dev_tip.to_string(), save.to_string(), initial.to_string()]
    );
    assert!(
        history[0].is_head,
        "HEAD stays visible even when it is code-only"
    );
    let parents = |index: usize| {
        history[index]
            .parent_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(parents(0), [save.to_string()]);
    assert_eq!(parents(1), [initial.to_string()], "skips the hidden Dev: a");
    assert!(parents(2).is_empty());

    let graph = workspace
        .workspace_graph(draftline::WorkspaceGraphOptions::default())
        .unwrap();
    assert_eq!(graph.nodes.len(), 3);
    let save_node = graph
        .nodes
        .iter()
        .find(|node| node.version.id().to_string() == save.to_string())
        .unwrap();
    assert_eq!(
        save_node
            .parent_version_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [initial.to_string()]
    );
}

#[test]
fn scoped_history_follows_the_side_that_brought_project_changes_through_a_merge() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    let initial = repo.head().unwrap().peel_to_commit().unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();

    write_file(temp.path(), "src/main.rs", "fn main() { code() }\n");
    let code = repo.find_commit(commit_all(&repo, "Dev: code")).unwrap();

    let mut feature_tree = repo.treebuilder(Some(&initial.tree().unwrap())).unwrap();
    let mut project = repo
        .treebuilder(Some(
            &repo
                .find_tree(
                    initial
                        .tree()
                        .unwrap()
                        .get_path(Path::new("cutready"))
                        .unwrap()
                        .id(),
                )
                .unwrap(),
        ))
        .unwrap();
    let blob = repo.blob(b"scene one, on a branch\n").unwrap();
    project.insert("intro.sk", blob, 0o100644).unwrap();
    feature_tree
        .insert("cutready", project.write().unwrap(), 0o040000)
        .unwrap();
    let feature_tree = repo.find_tree(feature_tree.write().unwrap()).unwrap();
    let feature = repo
        .find_commit(
            repo.commit(
                None,
                &signature,
                &signature,
                "Branch: intro",
                &feature_tree,
                &[&initial],
            )
            .unwrap(),
        )
        .unwrap();

    let mut merged = repo.merge_commits(&code, &feature, None).unwrap();
    assert!(!merged.has_conflicts());
    let merged_tree = repo
        .find_tree(merged.write_tree_to(&repo).unwrap())
        .unwrap();
    let merge = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Merge branch",
            &merged_tree,
            &[&code, &feature],
        )
        .unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    assert_eq!(
        labels(&workspace.versions().unwrap()),
        ["Branch: intro", "Initial app"],
        "the merge only carries the branch's project change"
    );

    let history = workspace.history().unwrap();
    let head = &history[0];
    assert_eq!(head.version.id().to_string(), merge.to_string());
    assert_eq!(
        head.parent_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [initial.id().to_string(), feature.id().to_string()],
        "the code-only first parent collapses to the last project version"
    );
}

#[test]
fn unscoped_versions_still_list_every_commit() {
    let (temp, repo) = code_repo();
    let workspace = Workspace::init_with_policy(temp.path(), cutready_policy()).unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { a() }\n");
    commit_all(&repo, "Dev: a");

    assert_eq!(
        labels(&workspace.versions().unwrap()),
        ["Dev: a", "Initial app"]
    );
    assert_eq!(workspace.history().unwrap().len(), 2);
}

#[test]
fn variation_namespace_hides_developer_branches() {
    let (temp, repo) = code_repo();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/login", &head, false).unwrap();
    repo.branch("cutready/pitch", &head, false).unwrap();
    let (_bare, _other_dir, _other) = with_remote(&repo);
    repo.find_remote("origin")
        .unwrap()
        .push(
            &[
                "refs/heads/feature/login:refs/heads/feature/login",
                "refs/heads/cutready/pitch:refs/heads/cutready/pitch",
            ],
            None,
        )
        .unwrap();

    assert!(scoped(&temp).with_variation_namespace("cutready").is_err());
    assert!(scoped(&temp).with_variation_namespace("").is_err());
    assert!(scoped(&temp)
        .with_variation_namespace("cutready//")
        .is_err());
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    assert_eq!(workspace.variation_namespace(), Some("cutready/"));

    let mut names = workspace
        .variations()
        .unwrap()
        .into_iter()
        .map(|variation| variation.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        ["cutready/pitch", "main"],
        "current branch always listed"
    );

    workspace.fetch_all_variations("origin").unwrap();
    let mut remote = workspace
        .remote_variations("origin")
        .unwrap()
        .into_iter()
        .map(|variation| variation.name)
        .collect::<Vec<_>>();
    remote.sort();
    assert_eq!(remote, ["cutready/pitch", "main"]);

    let graph = workspace
        .workspace_graph(draftline::WorkspaceGraphOptions::default())
        .unwrap();
    let tips = graph
        .nodes
        .iter()
        .flat_map(|node| {
            node.variation_tips
                .iter()
                .map(|tip| tip.as_str().to_string())
        })
        .collect::<Vec<_>>();
    assert!(!tips.iter().any(|tip| tip == "feature/login"), "{tips:?}");
    assert!(tips.iter().any(|tip| tip == "cutready/pitch"), "{tips:?}");

    let unfiltered = scoped(&temp);
    assert_eq!(unfiltered.variations().unwrap().len(), 3);
}

#[test]
fn scoped_graph_ancestor_and_counts_skip_hidden_developer_commits() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let initial = repo.head().unwrap().target().unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { fork() }\n");
    let fork = repo
        .find_commit(commit_all(&repo, "Dev: fork point"))
        .unwrap();
    repo.branch("cutready/pitch", &fork, false).unwrap();

    write_file(temp.path(), "cutready/intro.sk", "main take\n");
    let left = workspace.save_version("Main take").unwrap();
    workspace
        .switch_variation(
            &draftline::VariationId::from("cutready/pitch"),
            draftline::SwitchPolicy::AbortIfDirty,
        )
        .unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch take\n");
    let right = workspace.save_version("Pitch take").unwrap();

    let ancestor = workspace
        .workspace_graph_common_ancestor(left.id(), right.id())
        .unwrap()
        .common_ancestor
        .unwrap();
    assert_eq!(ancestor.to_string(), initial.to_string());
    let path = workspace
        .workspace_graph_path(
            left.id(),
            right.id(),
            draftline::WorkspaceGraphOptions::default(),
        )
        .unwrap();
    assert!(path.found);
    assert_eq!(path.version_ids.len(), 3);

    let counts = workspace
        .variation_summaries()
        .unwrap()
        .into_iter()
        .map(|summary| (summary.variation.name, summary.reachable_version_count))
        .collect::<Vec<_>>();
    assert_eq!(
        counts,
        [("cutready/pitch".to_string(), 2), ("main".to_string(), 2)]
    );
}

#[derive(Default)]
struct FakeHost {
    open: Vec<(draftline::PullRequestRequest, draftline::PullRequest)>,
    created: usize,
    fail_next_create: bool,
}

impl draftline::PullRequestProvider for FakeHost {
    fn find_open(
        &mut self,
        request: &draftline::PullRequestRequest,
    ) -> draftline::Result<Option<draftline::PullRequest>> {
        Ok(self
            .open
            .iter()
            .find(|(open, _)| open.head == request.head && open.base == request.base)
            .map(|(_, pull_request)| pull_request.clone()))
    }

    fn create(
        &mut self,
        request: &draftline::PullRequestRequest,
    ) -> draftline::Result<draftline::PullRequest> {
        if std::mem::take(&mut self.fail_next_create) {
            return Err(DraftlineError::PullRequest("host offline".into()));
        }
        self.created += 1;
        let pull_request = draftline::PullRequest {
            number: self.created as u64,
            url: format!("https://example.test/pull/{}", self.created),
        };
        self.open.push((request.clone(), pull_request.clone()));
        Ok(pull_request)
    }
}

fn send_request(title: &str) -> draftline::SendRequest {
    draftline::SendRequest {
        title: title.into(),
        ..Default::default()
    }
}

fn remote_branch(bare: &Path, name: &str) -> Option<git2::Oid> {
    git2::Repository::open_bare(bare)
        .unwrap()
        .refname_to_id(&format!("refs/heads/{name}"))
        .ok()
}

#[test]
fn send_from_the_default_branch_moves_work_to_a_new_branch_and_opens_a_pull_request() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    repo.find_remote("origin")
        .unwrap()
        .fetch(&["main"], None, None)
        .unwrap();
    let base = repo.head().unwrap().target().unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { local() }\n");
    write_file(temp.path(), "cutready/intro.sk", "scene one, for review\n");
    let saved = workspace.save_version("Revise intro").unwrap();
    let mut host = FakeHost::default();

    let result = workspace
        .send(
            "origin",
            &send_request("Revise intro for Q3!"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();

    assert_eq!(result.variation, "cutready/revise-intro-for-q3");
    assert_eq!(result.base, "main");
    assert!(result.created_variation && result.created_pull_request);
    assert_eq!(result.published_versions, 1);
    assert_eq!(host.open[0].0.head, "cutready/revise-intro-for-q3");
    assert_eq!(host.open[0].0.title, "Revise intro for Q3!");
    assert_eq!(
        repo.head().unwrap().shorthand(),
        Some("cutready/revise-intro-for-q3")
    );
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap(),
        base,
        "local main is handed back to its upstream"
    );
    assert_eq!(
        remote_branch(bare.path(), "cutready/revise-intro-for-q3"),
        Some(saved.id().as_str().parse().unwrap())
    );
    assert_eq!(remote_branch(bare.path(), "main"), Some(base));
    let branch = repo
        .find_branch("cutready/revise-intro-for-q3", git2::BranchType::Local)
        .unwrap();
    assert_eq!(
        branch.upstream().unwrap().name().unwrap(),
        Some("origin/cutready/revise-intro-for-q3")
    );
    assert!(status(&repo, "src/main.rs").contains(git2::Status::WT_MODIFIED));

    write_file(
        temp.path(),
        "cutready/intro.sk",
        "scene one, after review\n",
    );
    let second = workspace.save_version("Address feedback").unwrap();
    let again = workspace
        .send(
            "origin",
            &send_request("ignored"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert!(!again.created_variation && !again.created_pull_request);
    assert_eq!(again.pull_request.number, 1);
    assert_eq!(host.created, 1);
    assert_eq!(
        remote_branch(bare.path(), "cutready/revise-intro-for-q3"),
        Some(second.id().as_str().parse().unwrap())
    );
}

#[test]
fn send_retries_safely_after_the_host_fails_and_avoids_taken_branch_names() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, other) = with_remote(&repo);
    let head = other.head().unwrap().peel_to_commit().unwrap();
    other.branch("cutready/pitch", &head, false).unwrap();
    other
        .find_remote("origin")
        .unwrap()
        .push(
            &["refs/heads/cutready/pitch:refs/heads/cutready/pitch"],
            None,
        )
        .unwrap();
    assert!(
        repo.find_reference("refs/remotes/origin/cutready/pitch")
            .is_err(),
        "the name is taken only on the live remote"
    );
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    workspace.save_version("Pitch").unwrap();
    let mut host = FakeHost {
        fail_next_create: true,
        ..Default::default()
    };

    let error = workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap_err();
    assert!(matches!(error, DraftlineError::PullRequest(_)), "{error:?}");
    assert!(remote_branch(bare.path(), "cutready/pitch-2").is_some());

    let retry = workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(retry.variation, "cutready/pitch-2");
    assert!(!retry.created_variation && retry.created_pull_request);
    assert_eq!(retry.published_versions, 0);
}

#[test]
fn send_refuses_unsaved_project_changes_before_creating_a_branch() {
    let (temp, repo) = code_repo();
    let (_bare, _other_dir, _other) = with_remote(&repo);
    let workspace = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "unsaved\n");

    let error = workspace
        .send(
            "origin",
            &send_request("Draft"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();

    assert!(
        matches!(error, DraftlineError::PreflightFailed(_)),
        "{error:?}"
    );
    assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
    assert!(repo.find_branch("draft", git2::BranchType::Local).is_err());
}

#[test]
fn send_needs_a_known_base_branch() {
    let (temp, repo) = code_repo();
    let bare = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(bare.path()).unwrap();
    repo.remote("origin", bare.path().to_str().unwrap())
        .unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("trunk", &head, false).unwrap();
    repo.set_head("refs/heads/trunk").unwrap();
    repo.find_branch("main", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();

    let error = scoped(&temp)
        .send(
            "origin",
            &send_request("Draft"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();

    assert!(
        matches!(error, DraftlineError::SendBaseUnknown),
        "{error:?}"
    );
}

#[test]
fn send_rejects_developer_branches_outside_the_variation_namespace() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/login", &head, false).unwrap();
    repo.set_head("refs/heads/feature/login").unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();

    let error = workspace
        .send(
            "origin",
            &send_request("Login"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();

    assert!(
        matches!(error, DraftlineError::SendOutsideNamespace(ref name) if name == "feature/login"),
        "{error:?}"
    );
    assert!(remote_branch(bare.path(), "feature/login").is_none());

    repo.set_head("refs/heads/main").unwrap();
    let error = workspace
        .send(
            "origin",
            &draftline::SendRequest {
                title: "Login".into(),
                branch: Some("feature/login-2".into()),
                ..Default::default()
            },
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();
    assert!(
        matches!(error, DraftlineError::SendOutsideNamespace(_)),
        "{error:?}"
    );
    assert!(repo
        .find_branch("feature/login-2", git2::BranchType::Local)
        .is_err());
}

#[test]
fn send_hands_a_stale_local_base_back_to_the_fresh_upstream() {
    let (temp, repo) = code_repo();
    let (bare, other_dir, other) = with_remote(&repo);
    write_file(other_dir.path(), "README.md", "# teammate\n");
    commit_all(&other, "Teammate");
    push_main(&other);
    let fresh = remote_branch(bare.path(), "main").unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    workspace.save_version("Pitch").unwrap();

    let result = workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap();

    assert_eq!(result.variation, "cutready/pitch");
    assert_eq!(result.published_versions, 1);
    assert_eq!(repo.refname_to_id("refs/heads/main").unwrap(), fresh);

    // A send interrupted after switching branches is repaired by the next one.
    write_file(temp.path(), "cutready/intro.sk", "pitch v2\n");
    workspace.save_version("Pitch v2").unwrap();
    let carried = repo.head().unwrap().peel_to_commit().unwrap();
    repo.reference(
        "refs/heads/main",
        carried.id(),
        true,
        "simulate interruption",
    )
    .unwrap();
    workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(repo.refname_to_id("refs/heads/main").unwrap(), fresh);
}

fn tree_text(repo: &git2::Repository, rev: &str, relative: &str) -> Option<String> {
    let tree = repo.revparse_single(rev).unwrap().peel_to_tree().unwrap();
    let entry = tree.get_path(Path::new(relative)).ok()?;
    let blob = repo.find_blob(entry.id()).unwrap();
    Some(String::from_utf8_lossy(blob.content()).into_owned())
}

#[test]
fn incoming_merge_keeps_teammate_code_and_the_developer_index() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp);

    write_file(other_dir.path(), "src/lib.rs", "pub fn shared() {}\n");
    write_file(other_dir.path(), "cutready/outro.sk", "teammate outro\n");
    commit_all(&other, "Teammate code and scene");
    push_main(&other);

    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    workspace.save_version("My intro").unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { staged(); }\n");
    stage(&repo, "src/main.rs");

    workspace.fetch_remote("origin").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    assert!(report.developer_conflicts.is_empty());
    assert!(report.can_merge_cleanly, "{report:?}");
    let result = workspace
        .merge_incoming(report.token.unwrap(), "Merge", &mut RemoteOptions::new())
        .unwrap();

    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.id().to_string(), result.version.id().as_str());
    assert_eq!(head.parent_count(), 2);
    assert_eq!(
        tree_text(&repo, "HEAD", "src/lib.rs").as_deref(),
        Some("pub fn shared() {}\n")
    );
    assert_eq!(
        tree_text(&repo, "HEAD", "cutready/outro.sk").as_deref(),
        Some("teammate outro\n")
    );
    assert_eq!(
        tree_text(&repo, "HEAD", "cutready/intro.sk").as_deref(),
        Some("my intro\n")
    );
    assert_eq!(
        tree_text(&repo, "HEAD", "src/main.rs").as_deref(),
        Some("fn main() {}\n")
    );
    assert_eq!(read(temp.path(), "src/lib.rs"), "pub fn shared() {}\n");
    assert_eq!(read(temp.path(), "cutready/outro.sk"), "teammate outro\n");
    assert!(status(&repo, "src/main.rs").contains(git2::Status::INDEX_MODIFIED));
    assert_eq!(
        read(temp.path(), "src/main.rs"),
        "fn main() { staged(); }\n"
    );
}

#[test]
fn incoming_merge_leaves_code_conflicts_to_a_developer() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp);

    write_file(other_dir.path(), "src/main.rs", "fn main() { theirs(); }\n");
    commit_all(&other, "Teammate code");
    push_main(&other);

    write_file(temp.path(), "src/main.rs", "fn main() { mine(); }\n");
    commit_all(&repo, "My code");
    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    workspace.save_version("My intro").unwrap();
    let before = repo.head().unwrap().target().unwrap();

    workspace.fetch_remote("origin").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    assert_eq!(
        report.developer_conflicts,
        vec![PathBuf::from("src/main.rs")]
    );
    assert!(report.token.is_none());
    assert!(!report.can_merge_cleanly);
    assert_eq!(repo.head().unwrap().target().unwrap(), before);
}

#[test]
fn bringing_in_the_latest_base_merges_everything_into_the_variation() {
    let (temp, repo) = code_repo();
    let (bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    workspace.save_version("Pitch").unwrap();
    workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap();
    let main_before = repo.refname_to_id("refs/heads/main").unwrap();

    write_file(other_dir.path(), "src/lib.rs", "pub fn shared() {}\n");
    write_file(other_dir.path(), "cutready/outro.sk", "outro\n");
    commit_all(&other, "Teammate on main");
    push_main(&other);

    workspace.fetch_remote("origin").unwrap();
    let status = workspace.base_status("origin").unwrap();
    assert_eq!(status.variation, "main");
    assert_eq!((status.ahead, status.behind), (1, 1));

    let report = workspace.preflight_merge_base("origin").unwrap();
    let token = report.token.unwrap();
    assert_eq!(token.source.as_deref(), Some("main"));
    workspace
        .merge_incoming(token, "Bring in main", &mut RemoteOptions::new())
        .unwrap();

    assert_eq!(workspace.current_variation().unwrap(), "cutready/pitch");
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.parent_count(), 2);
    assert_eq!(
        head.parent_id(1).unwrap(),
        remote_branch(bare.path(), "main").unwrap()
    );
    assert_eq!(read(temp.path(), "src/lib.rs"), "pub fn shared() {}\n");
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "pitch\n");
    assert_eq!(read(temp.path(), "cutready/outro.sk"), "outro\n");
    assert_eq!(repo.refname_to_id("refs/heads/main").unwrap(), main_before);
    assert_eq!(workspace.base_status("origin").unwrap().behind, 0);
}

#[test]
fn bringing_in_the_base_when_only_behind_still_records_a_merge() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    repo.branch(
        "cutready/pitch",
        &repo.head().unwrap().peel_to_commit().unwrap(),
        false,
    )
    .unwrap();
    repo.set_head("refs/heads/cutready/pitch").unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();

    write_file(other_dir.path(), "src/lib.rs", "pub fn shared() {}\n");
    commit_all(&other, "Teammate on main");
    push_main(&other);
    workspace.fetch_remote("origin").unwrap();

    let report = workspace.preflight_merge_base("origin").unwrap();
    assert_eq!(report.sync_status.behind, 1);
    workspace
        .merge_incoming(
            report.token.unwrap(),
            "Bring in main",
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(
        repo.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_count(),
        2
    );
    assert_eq!(read(temp.path(), "src/lib.rs"), "pub fn shared() {}\n");
}

#[test]
fn incoming_merge_keeps_executable_and_symlink_modes() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp);

    write_file(other_dir.path(), "cutready/run.sk", "teammate script\n");
    let mut index = other.index().unwrap();
    index.read(true).unwrap();
    index.add_path(Path::new("cutready/run.sk")).unwrap();
    let mut entry = index.get_path(Path::new("cutready/run.sk"), 0).unwrap();
    entry.mode = 0o100755;
    index.add(&entry).unwrap();
    let link = other.blob(b"intro.sk").unwrap();
    entry.id = link;
    entry.mode = 0o120000;
    entry.file_size = 8;
    entry.path = b"cutready/link.sk".to_vec();
    index.add(&entry).unwrap();
    let tree = other.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    let parent = other.head().unwrap().peel_to_commit().unwrap();
    other
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Modes",
            &tree,
            &[&parent],
        )
        .unwrap();
    push_main(&other);

    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    workspace.save_version("My intro").unwrap();
    workspace.fetch_remote("origin").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    workspace
        .merge_incoming(report.token.unwrap(), "Merge", &mut RemoteOptions::new())
        .unwrap();

    let head = repo.head().unwrap().peel_to_tree().unwrap();
    let mode = |path: &str| head.get_path(Path::new(path)).unwrap().filemode();
    assert_eq!(mode("cutready/run.sk"), 0o100755);
    assert_eq!(mode("cutready/link.sk"), 0o120000);
    assert_eq!(mode("cutready/intro.sk"), 0o100644);
}

#[test]
fn preflighting_the_base_from_the_base_itself_is_an_ordinary_merge() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp);

    write_file(other_dir.path(), "src/lib.rs", "pub fn shared() {}\n");
    commit_all(&other, "Teammate on main");
    push_main(&other);
    workspace.fetch_remote("origin").unwrap();

    let behind = workspace.preflight_merge_base("origin").unwrap();
    assert_eq!(behind.sync_status.behind, 1);
    assert!(behind.token.is_none(), "fast-forward with apply_incoming");

    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    workspace.save_version("My intro").unwrap();
    let diverged = workspace.preflight_merge_base("origin").unwrap();
    assert_eq!(diverged.token.unwrap().source, None);
}

#[test]
fn tauri_merge_reports_code_conflicts_as_needing_a_developer() {
    use draftline::tauri_contract::{
        into_tauri_result, merge_incoming_with_context, DraftlineCommandContext,
        MergeIncomingRequest,
    };
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp);

    write_file(other_dir.path(), "src/main.rs", "fn main() { theirs(); }\n");
    commit_all(&other, "Teammate code");
    push_main(&other);
    write_file(temp.path(), "src/main.rs", "fn main() { mine(); }\n");
    commit_all(&repo, "My code");
    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    workspace.save_version("My intro").unwrap();

    let mut context = DraftlineCommandContext::new().with_content_policy(cutready_policy());
    let error = into_tauri_result(merge_incoming_with_context(
        &mut context,
        MergeIncomingRequest {
            workspace_path: temp.path().join("cutready"),
            remote: "origin".to_string(),
            label: "Merge".to_string(),
            from_base: false,
        },
    ))
    .unwrap_err();
    assert_eq!(error.code, "merge_needs_developer");
}

#[test]
fn repairing_a_finished_incoming_merge_keeps_newer_project_edits() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp);
    write_file(other_dir.path(), "cutready/outro.sk", "teammate outro\n");
    commit_all(&other, "Teammate scene");
    push_main(&other);
    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    workspace.save_version("My intro").unwrap();
    workspace.fetch_remote("origin").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    let merged = workspace
        .merge_incoming(report.token.unwrap(), "Merge", &mut RemoteOptions::new())
        .unwrap();

    // Crash after the ref moved but before the ledger was marked complete.
    let state = draftline::RecoveryState {
        operation_id: "interrupted".to_string(),
        operation: draftline::RecoveryOperation::MergeIncoming,
        original_variation: Some("main".to_string()),
        target: Some(merged.version.id().as_str().to_string()),
        completed: false,
    };
    fs::write(
        repo.path().join("draftline").join("recovery.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    write_file(temp.path(), "cutready/intro.sk", "newer edit\n");

    workspace.repair_recovery("interrupted").unwrap();
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "newer edit\n");
    assert!(workspace
        .recovery_state()
        .unwrap()
        .is_none_or(|state| state.completed));
}

#[test]
fn owned_worktree_starts_from_the_latest_base_and_leaves_this_checkout_alone() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(other_dir.path(), "cutready/outro.sk", "outro on main\n");
    commit_all(&other, "Teammate scene");
    push_main(&other);
    workspace.fetch_remote("origin").unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() { staged(); }\n");
    stage(&repo, "src/main.rs");
    let head_before = repo.head().unwrap().target().unwrap();

    let worktrees = tempfile::tempdir().unwrap();
    let owned = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Pitch deck",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .unwrap();
    assert_eq!(owned.variation.as_deref(), Some("cutready/pitch-deck"));
    assert_eq!(owned.name, "draftline-cutready-pitch-deck");
    assert_eq!(owned.project_path, owned.path.join("cutready"));
    assert_eq!(read(&owned.path, "cutready/outro.sk"), "outro on main\n");

    assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
    assert_eq!(repo.head().unwrap().target().unwrap(), head_before);
    assert!(status(&repo, "src/main.rs").contains(git2::Status::INDEX_MODIFIED));

    let copy = Workspace::open_with_policy(&owned.project_path, cutready_policy()).unwrap();
    write_file(&owned.path, "cutready/intro.sk", "in the copy\n");
    copy.save_version("Copy edit").unwrap();
    assert_eq!(copy.current_variation().unwrap(), "cutready/pitch-deck");
    assert_eq!(repo.head().unwrap().target().unwrap(), head_before);
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");

    let again = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Pitch deck",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .unwrap();
    assert_eq!(again.variation.as_deref(), Some("cutready/pitch-deck-2"));
}

#[test]
fn owned_worktree_can_carry_the_current_version_and_lists_only_its_own() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "unsent save\n");
    workspace.save_version("Unsent").unwrap();

    let other_tools = tempfile::tempdir().unwrap();
    let copilot_branch = repo
        .branch(
            "copilot/task",
            &repo.head().unwrap().peel_to_commit().unwrap(),
            false,
        )
        .unwrap()
        .into_reference();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&copilot_branch));
    repo.worktree(
        "copilot-task",
        &other_tools.path().join("task"),
        Some(&options),
    )
    .unwrap();

    let worktrees = tempfile::tempdir().unwrap();
    let owned = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Carry on",
            "origin",
            draftline::WorktreeStart::CurrentVersion,
        )
        .unwrap();
    assert_eq!(read(&owned.path, "cutready/intro.sk"), "unsent save\n");
    assert_eq!(workspace.owned_worktrees().unwrap(), vec![owned.clone()]);
    assert!(matches!(
        workspace.remove_owned_worktree("copilot-task", true),
        Err(DraftlineError::NotOwnedWorktree(_))
    ));
    assert!(other_tools.path().join("task").exists());
}

#[test]
fn removing_an_owned_worktree_keeps_its_branch_and_refuses_unsaved_work() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let worktrees = tempfile::tempdir().unwrap();
    let owned = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Draft",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .unwrap();
    let copy = Workspace::open_with_policy(&owned.project_path, cutready_policy()).unwrap();
    write_file(&owned.path, "cutready/intro.sk", "saved in copy\n");
    copy.save_version("Saved").unwrap();
    assert!(matches!(
        copy.remove_owned_worktree(&owned.name, true),
        Err(DraftlineError::OwnedWorktreeInUse(_))
    ));
    drop(copy);

    write_file(&owned.path, "notes.txt", "scratch\n");
    assert!(matches!(
        workspace.remove_owned_worktree(&owned.name, false),
        Err(DraftlineError::OwnedWorktreeHasChanges(paths)) if paths == [PathBuf::from("notes.txt")]
    ));
    assert!(owned.path.exists());

    workspace.remove_owned_worktree(&owned.name, true).unwrap();
    assert!(!owned.path.exists());
    assert!(workspace.owned_worktrees().unwrap().is_empty());
    let branch = repo
        .find_branch("cutready/draft", git2::BranchType::Local)
        .unwrap();
    assert_eq!(
        tree_text(&repo, "cutready/draft", "cutready/intro.sk").as_deref(),
        Some("saved in copy\n")
    );
    drop(branch);
}

#[test]
fn removing_an_owned_worktree_with_damaged_metadata_keeps_its_files() {
    let (temp, _repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let worktrees = tempfile::tempdir().unwrap();
    let owned = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Damaged",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .unwrap();
    write_file(&owned.path, "cutready/unsaved.sk", "only copy\n");
    std::fs::remove_file(
        temp.path()
            .join(".git/worktrees")
            .join(&owned.name)
            .join("HEAD"),
    )
    .unwrap();

    let result = workspace.remove_owned_worktree(&owned.name, false);
    assert!(result.is_err(), "{result:?}");

    assert_eq!(read(&owned.path, "cutready/unsaved.sk"), "only copy\n");
}

#[test]
fn a_failed_owned_worktree_leaves_nothing_behind() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    // A base where the project folder's name is taken by a file.
    let blob = repo.blob(b"not a folder\n").unwrap();
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("cutready", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Teammate", "t@example.com").unwrap();
    let commit = repo
        .commit(None, &signature, &signature, "Odd base", &tree, &[])
        .unwrap();
    repo.reference("refs/remotes/origin/main", commit, true, "test")
        .unwrap();

    let worktrees = tempfile::tempdir().unwrap();
    assert!(workspace
        .create_owned_worktree(
            worktrees.path(),
            "Broken",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .is_err());
    assert!(repo
        .find_branch("cutready/broken", git2::BranchType::Local)
        .is_err());
    assert!(repo.worktrees().unwrap().is_empty());
    assert!(!worktrees.path().join("draftline-cutready-broken").exists());
    assert!(!temp
        .path()
        .join(".git/worktrees/draftline-cutready-broken")
        .exists());

    // The same from inside a linked worktree, whose own .git is elsewhere.
    let linked = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Linked",
            "origin",
            draftline::WorktreeStart::CurrentVersion,
        )
        .unwrap();
    let inner = Workspace::open_with_policy(&linked.project_path, cutready_policy())
        .unwrap()
        .with_variation_namespace("cutready/")
        .unwrap();
    assert!(inner
        .create_owned_worktree(
            worktrees.path(),
            "Broken",
            "origin",
            draftline::WorktreeStart::LatestBase
        )
        .is_err());
    assert_eq!(repo.worktrees().unwrap().len(), 1);
    assert!(!temp
        .path()
        .join(".git/worktrees/draftline-cutready-broken")
        .exists());
}

#[test]
fn recovery_records_from_removed_features_still_open_and_can_be_cleared() {
    for operation in ["MilestoneMerge", "SquashVersions", "HistoryCleanup"] {
        for completed in [true, false] {
            let (temp, repo) = code_repo();
            let workspace = scoped(&temp);
            let ledger = repo.path().join("draftline").join("recovery.json");
            fs::create_dir_all(ledger.parent().unwrap()).unwrap();
            fs::write(
                &ledger,
                format!(
                    r#"{{"operation_id":"old","operation":"{operation}","original_variation":"main","target":null,"completed":{completed}}}"#
                ),
            )
            .unwrap();

            let pending = workspace.recovery_state().unwrap();
            assert_eq!(pending.is_some(), !completed);
            write_file(temp.path(), "cutready/intro.sk", "after upgrade\n");
            if !completed {
                assert!(workspace.save_version("Blocked").is_err());
                let repair = workspace.repair_recovery("old").unwrap();
                assert!(!repair.completed && !repair.changed_workspace);
                let rollback = workspace.rollback_recovery("old").unwrap();
                assert!(!rollback.completed && !rollback.changed_workspace);
                workspace.acknowledge_recovery().unwrap();
            }
            workspace.save_version("After upgrade").unwrap();
        }
    }
}

#[test]
fn only_the_project_that_was_interrupted_can_finish_its_recovery() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let pitch = scoped(&temp);
    let deck = Workspace::init_with_policy(temp.path().join("deck"), cutready_policy()).unwrap();
    write_file(other_dir.path(), "cutready/outro.sk", "teammate outro\n");
    commit_all(&other, "Teammate scene");
    push_main(&other);
    write_file(temp.path(), "cutready/intro.sk", "my intro\n");
    pitch.save_version("My intro").unwrap();
    pitch.fetch_remote("origin").unwrap();
    let report = pitch.preflight_merge_incoming("origin").unwrap();
    let merged = pitch
        .merge_incoming(report.token.unwrap(), "Merge", &mut RemoteOptions::new())
        .unwrap();

    // The pitch project crashed after the ref moved.
    let ledger = serde_json::json!({
        "operation_id": "interrupted",
        "operation": "MergeIncoming",
        "original_variation": "main",
        "target": merged.version.id().as_str(),
        "completed": false,
        "project": "cutready",
    });
    fs::write(
        repo.path().join("draftline").join("recovery.json"),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();

    // The deck project shares the branch, so it waits, but it can't finish
    // or discard the pitch project's operation.
    write_file(temp.path(), "deck/slide.sk", "slide\n");
    let in_other = |error: DraftlineError| {
        assert!(
            matches!(&error, DraftlineError::RecoveryInOtherProject(project) if project == Path::new("cutready")),
            "{error:?}"
        );
    };
    in_other(deck.save_version("Slide").unwrap_err());
    in_other(deck.repair_recovery("interrupted").unwrap_err());
    in_other(deck.rollback_recovery("interrupted").unwrap_err());
    in_other(deck.acknowledge_recovery().unwrap_err());
    assert!(deck.recovery_state().unwrap().is_some());
    let inspection = deck.inspect().unwrap();
    assert_eq!(
        inspection.safe_next_actions,
        vec![draftline::SafeNextAction::FinishInOtherProject]
    );
    assert_eq!(
        pitch.inspect().unwrap().safe_next_actions,
        vec![draftline::SafeNextAction::RepairRecovery]
    );

    pitch.repair_recovery("interrupted").unwrap();
    deck.save_version("Slide").unwrap();
}
