//! A Draftline project living as a folder inside a developer's code repository.

use std::fs;
use std::path::{Path, PathBuf};

use draftline::{ContentPolicy, Workspace};

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn commit_all(repo: &git2::Repository, message: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
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
