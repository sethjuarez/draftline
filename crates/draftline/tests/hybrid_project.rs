//! A Draftline project living as a folder inside a developer's code repository.

use std::fs;
use std::path::{Path, PathBuf};

use draftline::{
    ContentPolicy, DiagnosticCode, DiagnosticSeverity, DraftlineError, FileHazardKind, FolderFix,
    FolderProbe, FolderState, OutgoingFileKind, RemoteOptions, Workspace,
};

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
    // The first save also commits the folder marker.
    assert_eq!(
        paths,
        vec![
            PathBuf::from(".draftline"),
            PathBuf::from("cutready/intro.sk")
        ]
    );
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
    assert_eq!(session_tip.summary().unwrap(), Some("Worktree save"));
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
        vec![
            PathBuf::from(".draftline"),
            PathBuf::from("cutready/intro.sk")
        ]
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

/// Commits `pointer` for `relative` while the file on disk holds `real`, the way
/// Git LFS leaves a checkout: the index stat matches, so status is clean.
fn commit_smudged(
    repo: &git2::Repository,
    root: &Path,
    relative: &str,
    real: &str,
    pointer: &str,
    message: &str,
) -> git2::Oid {
    write_file(root, relative, real);
    // Back-date the file so the index entry isn't "racily clean": Git (and
    // libgit2) would rehash it and see that it differs from the pointer.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(root.join(relative))
        .unwrap()
        .set_modified(past)
        .unwrap();
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    index.add_path(Path::new(relative)).unwrap();
    let mut entry = index.get_path(Path::new(relative), 0).unwrap();
    entry.id = repo.blob(pointer.as_bytes()).unwrap();
    index.add(&entry).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &[&parent],
    )
    .unwrap()
}

fn lfs_project() -> (tempfile::TempDir, git2::Repository) {
    let (temp, repo) = code_repo();
    write_file(
        temp.path(),
        ".gitattributes",
        "*.sk filter=lfs diff=lfs merge=lfs -text\n",
    );
    commit_all(&repo, "Track sketches with LFS");
    commit_smudged(
        &repo,
        temp.path(),
        "cutready/intro.sk",
        "scene one\n",
        "lfs pointer one\n",
        "Intro in LFS",
    );
    assert!(status(&repo, "cutready/intro.sk").is_empty());
    (temp, repo)
}

fn is_filter_refusal(error: DraftlineError) -> bool {
    hazard_kinds(error).iter().any(|(path, kind)| {
        path == Path::new("cutready/intro.sk") && *kind == FileHazardKind::GitFilter
    })
}

#[test]
fn restore_and_switch_never_write_lfs_pointers_over_real_files() {
    let (temp, repo) = lfs_project();
    let workspace = scoped(&temp);
    let one = workspace.versions().unwrap()[0].id().clone();
    let old = workspace.create_variation("cutready/old").unwrap();
    commit_smudged(
        &repo,
        temp.path(),
        "cutready/intro.sk",
        "scene two\n",
        "lfs pointer two\n",
        "Dev: intro two",
    );
    let head = repo.head().unwrap().target();

    let error = workspace
        .restore_version_as_new_save(&one, "Back to one")
        .unwrap_err();
    assert!(is_filter_refusal(error));
    let error = workspace
        .switch_variation(old.id(), draftline::SwitchPolicy::AbortIfDirty)
        .unwrap_err();
    assert!(is_filter_refusal(error));

    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene two\n");
    assert_eq!(repo.head().unwrap().target(), head);
    assert_eq!(repo.head().unwrap().shorthand().ok(), Some("main"));
    workspace
        .preflight_discard_changes()
        .expect("no pending recovery");
}

#[test]
fn switch_refuses_a_branch_whose_own_rules_put_a_file_in_lfs() {
    let (temp, repo) = code_repo();
    write_file(temp.path(), "cutready/intro.sk", "scene one\n");
    commit_all(&repo, "Intro");
    let workspace = scoped(&temp);
    let lfs = workspace.create_variation("cutready/lfs").unwrap();

    repo.set_head("refs/heads/cutready/lfs").unwrap();
    write_file(
        temp.path(),
        ".gitattributes",
        "*.sk filter=lfs diff=lfs merge=lfs -text\n",
    );
    commit_all(&repo, "Track sketches with LFS");
    commit_smudged(
        &repo,
        temp.path(),
        "cutready/intro.sk",
        "scene two\n",
        "lfs pointer two\n",
        "Intro in LFS",
    );
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    assert!(!temp.path().join(".gitattributes").exists());
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");

    let error = workspace
        .switch_variation(lfs.id(), draftline::SwitchPolicy::AbortIfDirty)
        .unwrap_err();

    assert!(is_filter_refusal(error));
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");
    assert_eq!(repo.head().unwrap().shorthand().ok(), Some("main"));
    workspace
        .preflight_discard_changes()
        .expect("no pending recovery");
}

const LFS_POINTER: &str = "version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";

#[test]
fn a_clone_without_lfs_content_is_reported_not_read_as_content() {
    let (temp, repo) = code_repo();
    write_file(
        temp.path(),
        ".gitattributes",
        "*.sk filter=lfs diff=lfs merge=lfs -text\n",
    );
    write_file(temp.path(), "cutready/intro.sk", LFS_POINTER);
    write_file(temp.path(), "cutready/notes.md", "plain notes\n");
    commit_all(&repo, "Track sketches with LFS (cloned without git-lfs)");
    let workspace = scoped(&temp);

    let files = workspace.filtered_files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, PathBuf::from("cutready/intro.sk"));
    assert_eq!(files[0].driver, "lfs");
    assert!(files[0].content_missing);
    let warning = workspace
        .inspect()
        .unwrap()
        .diagnostics
        .into_iter()
        .find(|diagnostic| diagnostic.code == DiagnosticCode::LfsContentMissing)
        .expect("an LFS warning");
    assert_eq!(warning.severity, DiagnosticSeverity::Warning);

    // A host can probe where it is about to write before it writes there.
    let probed = workspace
        .filtered_files_among(["cutready/next.sk", "cutready/notes.md"])
        .unwrap();
    assert_eq!(probed.len(), 1);
    assert_eq!(probed[0].path, PathBuf::from("cutready/next.sk"));
    assert!(!probed[0].content_missing);

    repo.config()
        .unwrap()
        .set_str("filter.lfs.process", "git-lfs filter-process")
        .unwrap();
    assert!(workspace.filtered_files().unwrap()[0].driver_configured);
}

#[test]
fn command_contract_lists_and_probes_filtered_files() {
    use draftline::tauri_contract::{
        filtered_files_with_context, DraftlineCommandContext, FilteredFilesRequest,
    };
    let (temp, repo) = code_repo();
    write_file(temp.path(), ".gitattributes", "*.sk filter=lfs -text\n");
    write_file(temp.path(), "cutready/intro.sk", LFS_POINTER);
    commit_all(&repo, "Track sketches with LFS");
    scoped(&temp);
    let mut context = DraftlineCommandContext::new().with_content_policy(cutready_policy());
    let folder = temp.path().join("cutready");

    let listed = filtered_files_with_context(
        &mut context,
        FilteredFilesRequest {
            workspace_path: folder.clone(),
            paths: None,
        },
    )
    .unwrap();
    let json = serde_json::to_value(&listed).unwrap();
    assert_eq!(json[0]["path"], "cutready/intro.sk");
    assert_eq!(json[0]["driver"], "lfs");
    assert_eq!(json[0]["content_missing"], true);

    let probed = filtered_files_with_context(
        &mut context,
        FilteredFilesRequest {
            workspace_path: folder,
            paths: Some(vec!["cutready/notes.md".into()]),
        },
    )
    .unwrap();
    assert!(probed.is_empty());
}
#[test]
fn smudged_lfs_files_are_not_reported_missing() {
    let (temp, _repo) = lfs_project();
    let workspace = scoped(&temp);

    let files = workspace.filtered_files().unwrap();
    assert_eq!(files.len(), 1);
    assert!(!files[0].content_missing);
    assert!(workspace
        .inspect()
        .unwrap()
        .diagnostics
        .iter()
        .all(|diagnostic| diagnostic.code != DiagnosticCode::LfsContentMissing));
}

#[test]
fn a_developer_lfs_change_outside_the_folder_is_named_as_theirs() {
    let (temp, repo) = code_repo();
    write_file(
        temp.path(),
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );
    commit_all(&repo, "Track binaries with LFS");
    commit_smudged(
        &repo,
        temp.path(),
        "assets/logo.bin",
        "logo one\n",
        "lfs pointer one\n",
        "Dev: logo",
    );
    let workspace = scoped(&temp);
    let old = workspace.create_variation("cutready/old").unwrap();
    commit_smudged(
        &repo,
        temp.path(),
        "assets/logo.bin",
        "logo two\n",
        "lfs pointer two\n",
        "Dev: new logo",
    );

    let error = workspace
        .switch_variation(old.id(), draftline::SwitchPolicy::AbortIfDirty)
        .unwrap_err();

    assert_eq!(
        hazard_kinds(error),
        vec![(
            PathBuf::from("assets/logo.bin"),
            FileHazardKind::GitFilterOutsideProject
        )]
    );
    assert_eq!(read(temp.path(), "assets/logo.bin"), "logo two\n");
    assert_eq!(repo.head().unwrap().shorthand().ok(), Some("main"));
}

fn blob_text(repo: &git2::Repository, spec: &str) -> Vec<u8> {
    repo.revparse_single(spec)
        .unwrap()
        .peel_to_blob()
        .unwrap()
        .content()
        .to_vec()
}

#[test]
fn line_endings_round_trip_like_git_with_autocrlf() {
    let (temp, repo) = code_repo();
    repo.config()
        .unwrap()
        .set_bool("core.autocrlf", true)
        .unwrap();
    write_file(temp.path(), "cutready/notes.md", "one\r\ntwo\r\n");
    commit_all(&repo, "Dev: notes");
    let first = repo.head().unwrap().target().unwrap();
    assert_eq!(blob_text(&repo, "HEAD:cutready/notes.md"), b"one\ntwo\n");
    let workspace = scoped(&temp);
    assert!(workspace.changed_files().unwrap().is_empty());

    write_file(temp.path(), "cutready/notes.md", "one\r\ntwo edited\r\n");
    workspace.save_version("Edit notes").unwrap();
    assert_eq!(
        blob_text(&repo, "HEAD:cutready/notes.md"),
        b"one\ntwo edited\n"
    );
    assert!(status(&repo, "cutready/notes.md").is_empty());

    let first = draftline::VersionId::from_canonical_string(first.to_string()).unwrap();
    workspace
        .restore_version_as_new_save(&first, "Back")
        .unwrap();
    assert_eq!(
        fs::read(temp.path().join("cutready/notes.md")).unwrap(),
        b"one\r\ntwo\r\n"
    );
    assert!(status(&repo, "cutready/notes.md").is_empty());
    assert!(workspace.changed_files().unwrap().is_empty());
}

fn head_id(repo: &git2::Repository) -> git2::Oid {
    repo.head().unwrap().peel_to_commit().unwrap().id()
}

fn is_git_index_locked(error: &DraftlineError) -> bool {
    matches!(error, DraftlineError::GitIndexLocked)
}

/// A business-user edit plus a developer's staged code, ready to save.
fn busy_project() -> (tempfile::TempDir, git2::Repository, Workspace) {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    write_file(temp.path(), "src/lib.rs", "pub fn staged() {}\n");
    stage(&repo, "src/lib.rs");
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    (temp, repo, workspace)
}

#[test]
fn save_waits_while_git_holds_the_index() {
    let (temp, repo, workspace) = busy_project();
    let before = head_id(&repo);
    let lock = repo.path().join("index.lock");
    fs::write(&lock, "").unwrap();

    let error = workspace.save_version("Revise intro").unwrap_err();

    assert!(is_git_index_locked(&error), "{error:?}");
    assert_eq!(head_id(&repo), before, "no commit while git is busy");
    assert!(workspace.recovery_state().unwrap().is_none());
    fs::remove_file(&lock).unwrap();
    assert_eq!(
        status(&repo, "cutready/intro.sk"),
        git2::Status::WT_MODIFIED
    );
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);

    workspace.save_version("Revise intro").unwrap();

    assert_eq!(status(&repo, "cutready/intro.sk"), git2::Status::CURRENT);
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
    assert_eq!(
        read(temp.path(), "cutready/intro.sk"),
        "scene one, revised\n"
    );
}

#[test]
fn a_save_that_cannot_move_the_branch_leaves_the_index_as_it_was() {
    let (temp, repo, workspace) = busy_project();
    let before = head_id(&repo);
    let ref_lock = repo.path().join("refs").join("heads").join("main.lock");
    fs::create_dir_all(ref_lock.parent().unwrap()).unwrap();
    fs::write(&ref_lock, "").unwrap();

    workspace.save_version("Revise intro").unwrap_err();

    assert_eq!(head_id(&repo), before);
    assert_eq!(
        status(&repo, "cutready/intro.sk"),
        git2::Status::WT_MODIFIED,
        "index rolled back, so a developer commit cannot revert the edit"
    );
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
    fs::remove_file(&ref_lock).unwrap();

    workspace.save_version("Revise intro").unwrap();
    assert_eq!(status(&repo, "cutready/intro.sk"), git2::Status::CURRENT);
    assert_eq!(
        read(temp.path(), "cutready/intro.sk"),
        "scene one, revised\n"
    );
}

#[test]
fn a_failed_save_keeps_a_developer_merge_conflict() {
    let (temp, repo, workspace) = busy_project();
    let mut index = repo.index().unwrap();
    let entry = |stage: u16, text: &str| git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: repo.blob(text.as_bytes()).unwrap(),
        flags: stage << 12,
        flags_extended: 0,
        path: b"cutready/intro.sk".to_vec(),
    };
    index.remove_path(Path::new("cutready/intro.sk")).unwrap();
    for (stage, text) in [(1, "base\n"), (2, "ours\n"), (3, "theirs\n")] {
        index.add(&entry(stage, text)).unwrap();
    }
    index.write().unwrap();
    let ref_lock = repo.path().join("refs").join("heads").join("main.lock");
    fs::write(&ref_lock, "").unwrap();

    workspace.save_version("Resolve intro").unwrap_err();

    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    let conflict = index
        .conflicts()
        .unwrap()
        .map(Result::unwrap)
        .next()
        .expect("conflict kept");
    let text = |entry: Option<git2::IndexEntry>| blob_text(&repo, &entry.unwrap().id.to_string());
    assert_eq!(text(conflict.ancestor), b"base\n");
    assert_eq!(text(conflict.our), b"ours\n");
    assert_eq!(text(conflict.their), b"theirs\n");
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
    fs::remove_file(&ref_lock).unwrap();
    let _ = temp;
}

#[test]
fn saves_log_the_saver_in_the_reflog_like_git_commit() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    let bea = draftline::Contributor {
        name: "Bea".to_string(),
        email: Some("bea@example.test".to_string()),
    };
    let profile = draftline::ContributorProfile::new(bea.clone(), bea);

    workspace
        .save_version_with_profile("Revise intro", &profile)
        .unwrap();

    for name in ["HEAD", "refs/heads/main"] {
        let reflog = repo.reflog(name).unwrap();
        let entry = reflog.get(0).unwrap();
        assert_eq!(
            entry.message().unwrap(),
            Some("commit: Revise intro"),
            "{name}"
        );
        assert_eq!(entry.committer().name().ok(), Some("Bea"), "{name}");
        assert_eq!(entry.id_new(), head_id(&repo), "{name}");
    }
}

#[test]
fn operations_that_write_the_index_refuse_while_git_holds_it() {
    let (temp, repo, workspace) = busy_project();
    let lock = repo.path().join("index.lock");
    fs::write(&lock, "").unwrap();

    let error = workspace.shelve_changes("wip").unwrap_err();
    assert!(is_git_index_locked(&error), "shelve: {error:?}");
    assert!(workspace.list_shelves().unwrap().is_empty());
    let error = workspace.discard_changes().unwrap_err();
    assert!(is_git_index_locked(&error), "discard: {error:?}");
    assert_eq!(
        read(temp.path(), "cutready/intro.sk"),
        "scene one, revised\n"
    );
    assert!(workspace.recovery_state().unwrap().is_none());

    fs::remove_file(&lock).unwrap();
    workspace.save_version("Revise intro").unwrap();
    let original = workspace.versions().unwrap()[1].id().clone();
    let before = head_id(&repo);
    fs::write(&lock, "").unwrap();

    let error = workspace
        .restore_version_as_new_save(&original, "Back")
        .unwrap_err();
    assert!(is_git_index_locked(&error), "restore: {error:?}");
    assert_eq!(head_id(&repo), before);
    assert_eq!(
        read(temp.path(), "cutready/intro.sk"),
        "scene one, revised\n"
    );
    assert!(workspace.recovery_state().unwrap().is_none());
    fs::remove_file(&lock).unwrap();
    assert_eq!(status(&repo, "src/lib.rs"), git2::Status::INDEX_NEW);
}

#[test]
fn discard_and_shelve_never_write_lfs_pointers_over_real_files() {
    let (temp, _repo) = lfs_project();
    let workspace = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "scene edited\n");

    let report = workspace.preflight_discard_changes().unwrap();
    assert!(!report.can_proceed);
    assert!(is_filter_refusal(DraftlineError::PreflightFailed(
        Box::new(report)
    )));
    assert!(is_filter_refusal(workspace.discard_changes().unwrap_err()));
    assert!(is_filter_refusal(
        workspace.discard_file("cutready/intro.sk").unwrap_err()
    ));
    assert!(is_filter_refusal(
        workspace.shelve_changes("aside").unwrap_err()
    ));
    fs::remove_file(temp.path().join("cutready/intro.sk")).unwrap();
    assert!(is_filter_refusal(
        workspace.shelve_changes("aside").unwrap_err()
    ));
    fs::write(temp.path().join("cutready/intro.sk"), "scene edited\n").unwrap();

    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene edited\n");
    workspace
        .preflight_discard_changes()
        .expect("no pending recovery");
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
        vec![
            PathBuf::from(".draftline"),
            PathBuf::from("cutready/intro.sk")
        ]
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
fn scoped_history_lists_teammates_commits_and_flags_project_changes() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    let initial = repo.head().unwrap().target().unwrap();

    write_file(temp.path(), "src/main.rs", "fn main() { a() }\n");
    let dev_a = commit_all(&repo, "Dev: a");
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
        [
            dev_tip.to_string(),
            save.to_string(),
            dev_a.to_string(),
            initial.to_string()
        ]
    );
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.touches_folder)
            .collect::<Vec<_>>(),
        [false, true, false, true]
    );
    assert!(history[0].is_head);
    let parents = |index: usize| {
        history[index]
            .parent_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(parents(0), [save.to_string()]);
    assert_eq!(parents(1), [dev_a.to_string()]);
    assert!(parents(3).is_empty());

    let graph = workspace
        .workspace_graph(draftline::WorkspaceGraphOptions::default())
        .unwrap();
    assert_eq!(graph.nodes.len(), 4);
    let save_node = graph
        .nodes
        .iter()
        .find(|node| node.version.id().to_string() == save.to_string())
        .unwrap();
    assert!(save_node.touches_folder);
    assert_eq!(
        save_node
            .parent_version_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [dev_a.to_string()]
    );
}

#[test]
fn a_merge_that_only_brings_one_sides_project_changes_does_not_touch_the_project() {
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
        [code.id().to_string(), feature.id().to_string()]
    );
    assert!(!head.touches_folder);
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
fn command_context_applies_the_variation_namespace_on_every_open() {
    use draftline::tauri_contract::{
        list_variations_with_context, DraftlineCommandContext, WorkspaceRequest,
    };
    let (temp, repo) = code_repo();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/login", &head, false).unwrap();
    repo.branch("cutready/pitch", &head, false).unwrap();
    scoped(&temp);

    assert!(DraftlineCommandContext::new()
        .with_variation_namespace("cutready")
        .is_err());
    let names = |context: &DraftlineCommandContext<'_>| {
        let mut names = list_variations_with_context(
            context,
            WorkspaceRequest {
                workspace_path: temp.path().join("cutready"),
            },
        )
        .unwrap()
        .into_iter()
        .map(|summary| summary.variation.name)
        .collect::<Vec<_>>();
        names.sort();
        names
    };
    let context = DraftlineCommandContext::new()
        .with_content_policy(cutready_policy())
        .with_variation_namespace("cutready/")
        .unwrap();
    assert_eq!(names(&context), ["cutready/pitch", "main"]);
    let plain = DraftlineCommandContext::new().with_content_policy(cutready_policy());
    assert_eq!(names(&plain).len(), 3);
}

#[test]
fn command_contract_saves_on_a_developer_branch_and_starts_a_variation_there() {
    use draftline::tauri_contract::{
        save_with_context, start_variation_here_with_context, DraftlineCommandContext, SaveRequest,
        StartVariationHereRequest,
    };
    let (temp, repo) = code_repo();
    scoped(&temp);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/x", &head, false).unwrap();
    repo.set_head("refs/heads/feature/x").unwrap();
    let folder = temp.path().join("cutready");
    std::fs::write(folder.join("intro.sk"), "pitch").unwrap();
    let mut context = DraftlineCommandContext::new()
        .with_content_policy(cutready_policy())
        .with_variation_namespace("cutready/")
        .unwrap();
    let save = |context: &mut DraftlineCommandContext| {
        save_with_context(
            context,
            SaveRequest {
                workspace_path: folder.clone(),
                label: "Pitch".into(),
            },
        )
        .unwrap();
    };

    save(&mut context);
    assert_ne!(
        repo.refname_to_id("refs/heads/feature/x").unwrap(),
        head.id()
    );

    let started = start_variation_here_with_context(
        &mut context,
        StartVariationHereRequest {
            workspace_path: folder.clone(),
            name: "cutready/rescue".into(),
            metadata: Default::default(),
        },
    )
    .unwrap();
    assert_eq!(started.variation.name, "cutready/rescue");
    std::fs::write(folder.join("intro.sk"), "pitch v2").unwrap();
    save(&mut context);
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
fn scoped_graph_ancestor_and_counts_include_teammates_commits() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
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
    assert_eq!(ancestor.to_string(), fork.id().to_string());
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
        [("cutready/pitch".to_string(), 3), ("main".to_string(), 3)]
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
        repo.head().unwrap().shorthand().ok(),
        Some("cutready/revise-intro-for-q3")
    );
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap(),
        saved.id().as_str().parse().unwrap(),
        "Draftline never moves the base; it keeps the save until pushed"
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
    let approve = |workspace: &Workspace| draftline::SendRequest {
        expected: Some(
            workspace
                .preflight_send("origin", &send_request("Pitch"), &mut RemoteOptions::new())
                .unwrap()
                .token,
        ),
        ..send_request("Pitch")
    };
    let approved = approve(&workspace);

    let error = workspace
        .send("origin", &approved, &mut host, &mut RemoteOptions::new())
        .unwrap_err();
    assert!(matches!(error, DraftlineError::PullRequest(_)), "{error:?}");
    assert!(remote_branch(bare.path(), "cutready/pitch-2").is_some());

    // The token approved one attempt; the retry needs a fresh preflight.
    let error = workspace
        .send("origin", &approved, &mut host, &mut RemoteOptions::new())
        .unwrap_err();
    assert!(
        matches!(error, DraftlineError::LocalStateChanged { .. }),
        "{error:?}"
    );
    let retry = workspace
        .send(
            "origin",
            &approve(&workspace),
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
    assert_eq!(repo.head().unwrap().shorthand().ok(), Some("main"));
    assert!(repo.find_branch("draft", git2::BranchType::Local).is_err());

    // On its own branch too: the edit would silently stay behind.
    workspace.save_version("Intro").unwrap();
    let mut host = FakeHost::default();
    workspace
        .send(
            "origin",
            &send_request("Draft"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();
    write_file(temp.path(), "cutready/intro.sk", "unsaved again\n");
    let error = workspace
        .send(
            "origin",
            &send_request("Draft"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap_err();
    assert!(
        matches!(error, DraftlineError::PreflightFailed(_)),
        "{error:?}"
    );
}

#[test]
fn send_needs_a_known_base_branch() {
    let (temp, repo) = code_repo();
    let bare = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(bare.path()).unwrap();
    repo.remote("origin", bare.path().to_str().unwrap())
        .unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("release-train", &head, false).unwrap();
    repo.set_head("refs/heads/release-train").unwrap();
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
fn workspace_reports_the_enclosing_repository_git_identity() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    assert_eq!(
        workspace.git_identity().unwrap(),
        Some(draftline::Contributor {
            name: "Dev".to_string(),
            email: Some("dev@example.test".to_string()),
        })
    );

    repo.config().unwrap().set_str("user.name", " ").unwrap();
    assert_eq!(
        workspace.git_identity().unwrap(),
        None,
        "a blank name is no identity"
    );
}

#[test]
fn send_refuses_a_base_the_remote_lacks_before_changing_anything() {
    let (temp, repo) = code_repo();
    let bare = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(bare.path()).unwrap();
    repo.remote("origin", bare.path().to_str().unwrap())
        .unwrap();
    write_file(temp.path(), "cutready/intro.sk", "scene one, for review\n");
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    workspace.save_version("Revise intro").unwrap();
    let mut host = FakeHost::default();

    // An empty remote advertises no refs; git2 0.20 aborted listing them.
    let error = workspace
        .send(
            "origin",
            &send_request("Revise intro"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap_err();

    assert!(
        matches!(&error, DraftlineError::SendBaseNotOnRemote(base) if base == "main"),
        "{error:?}"
    );
    assert!(host.open.is_empty());
    assert_eq!(repo.head().unwrap().shorthand().ok(), Some("main"));
    assert_eq!(repo.branches(None).unwrap().count(), 1, "no branch created");
    assert!(git2::Repository::open_bare(bare.path())
        .unwrap()
        .references()
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn send_targets_a_default_branch_the_remote_has() {
    let (temp, repo) = code_repo();
    let bare = tempfile::tempdir().unwrap();
    // No advertised default yet: HEAD names a branch that does not exist.
    git2::Repository::init_bare(bare.path())
        .unwrap()
        .set_head("refs/heads/unborn")
        .unwrap();
    repo.remote("origin", bare.path().to_str().unwrap())
        .unwrap()
        .push(&["refs/heads/main:refs/heads/master"], None)
        .unwrap();
    write_file(temp.path(), "cutready/intro.sk", "scene one, for review\n");
    let workspace = scoped(&temp);
    workspace.save_version("Revise intro").unwrap();
    let send = |workspace: &draftline::Workspace| {
        workspace
            .send(
                "origin",
                &send_request("Revise intro"),
                &mut FakeHost::default(),
                &mut RemoteOptions::new(),
            )
            .unwrap()
            .base
    };

    assert_eq!(
        send(&workspace),
        "master",
        "local main is not on the remote"
    );

    repo.find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/develop"], None)
        .unwrap();
    repo.config()
        .unwrap()
        .set_str("init.defaultBranch", "develop")
        .unwrap();
    assert_eq!(send(&workspace), "develop");

    git2::Repository::open_bare(bare.path())
        .unwrap()
        .set_head("refs/heads/master")
        .unwrap();
    assert_eq!(
        send(&workspace),
        "master",
        "the remote's advertised default wins over local guesses"
    );
}

#[test]
fn namespaced_workspace_leaves_developer_branches_alone() {
    use draftline::{RestoreVersionTarget, SwitchPolicy, VariationId};
    let outside = |result: draftline::Result<_>| match result {
        Err(DraftlineError::VariationOutsideNamespace(_)) => {}
        Err(other) => panic!("expected VariationOutsideNamespace, got {other:?}"),
        Ok(_) => panic!("expected VariationOutsideNamespace, got Ok"),
    };
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/login", &head, false).unwrap();
    repo.find_remote("origin")
        .unwrap()
        .push(&["refs/heads/feature/login:refs/heads/feature/login"], None)
        .unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let dev = VariationId::from("feature/login");
    let version = workspace.versions().unwrap()[0].id().clone();

    outside(workspace.create_variation("feature/new").map(|_| ()));
    outside(
        workspace
            .create_variation_from(&version, "feature/new")
            .map(|_| ()),
    );
    outside(
        workspace
            .switch_variation(&dev, SwitchPolicy::AbortIfDirty)
            .map(|_| ()),
    );
    outside(workspace.preflight_delete_variation(&dev).map(|_| ()));
    outside(workspace.delete_variation(&dev));
    outside(
        workspace
            .preflight_rename_variation(&dev, &VariationId::from("cutready/login"))
            .map(|_| ()),
    );
    outside(
        workspace
            .rename_variation(
                &VariationId::from("main"),
                &VariationId::from("cutready/main"),
            )
            .map(|_| ()),
    );
    outside(workspace.adopt_remote_variation("origin", &dev).map(|_| ()));
    outside(
        workspace
            .preflight_delete_remote_variation("origin", &dev)
            .map(|_| ()),
    );
    outside(
        workspace
            .restore_version_as_new_save_to_variation(
                &version,
                "Back",
                RestoreVersionTarget::Existing {
                    variation: dev.clone(),
                },
            )
            .map(|_| ()),
    );
    outside(
        workspace
            .restore_version_as_new_save_to_variation(
                &version,
                "Back",
                RestoreVersionTarget::New {
                    name: "feature/back".into(),
                    metadata: Default::default(),
                },
            )
            .map(|_| ()),
    );
    // The base branch is shared with developers; hybrid sharing goes through Send.
    outside(workspace.preflight_publish("origin").map(|_| ()));
    outside(workspace.publish_changes("origin").map(|_| ()));

    assert!(repo
        .find_branch("feature/login", git2::BranchType::Local)
        .is_ok());
    assert!(repo
        .find_branch("feature/new", git2::BranchType::Local)
        .is_err());
    assert!(remote_branch(bare.path(), "feature/login").is_some());
    assert_eq!(
        repo.head().unwrap().shorthand().ok(),
        Some("main"),
        "nothing switched"
    );

    let pitch = workspace.create_variation("cutready/pitch").unwrap();
    workspace
        .switch_variation(pitch.id(), SwitchPolicy::AbortIfDirty)
        .unwrap();
    workspace
        .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
        .unwrap();
    workspace
        .rename_variation(pitch.id(), &VariationId::from("cutready/demo"))
        .unwrap();
    workspace
        .delete_variation(&VariationId::from("cutready/demo"))
        .unwrap();

    let unfiltered = scoped(&temp);
    unfiltered
        .switch_variation(&dev, SwitchPolicy::AbortIfDirty)
        .unwrap();
}

#[test]
fn namespaced_archive_restore_stays_inside_the_namespace() {
    let (temp, _repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let old = workspace.create_variation("cutready/old").unwrap();
    workspace.delete_variation(old.id()).unwrap();
    let archive = workspace
        .list_support_refs(draftline::SupportRefScope::Local)
        .unwrap()
        .into_iter()
        .next()
        .expect("deleting a variation archives it");

    let error = workspace
        .preflight_restore_support_ref(&archive.id, "feature/old")
        .unwrap_err();
    assert!(
        matches!(error, DraftlineError::VariationOutsideNamespace(_)),
        "{error:?}"
    );
    let mut token = workspace
        .preflight_restore_support_ref(&archive.id, "cutready/old")
        .unwrap()
        .token;
    token.target_variation = draftline::VariationId::from("feature/old");
    let error = workspace.restore_support_ref(token).unwrap_err();
    assert!(
        matches!(error, DraftlineError::VariationOutsideNamespace(_)),
        "{error:?}"
    );
}

#[test]
fn send_from_a_developer_branch_moves_work_to_a_namespaced_branch() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/login", &head, false).unwrap();
    repo.set_head("refs/heads/feature/login").unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "login pitch\n");
    workspace.save_version("Login pitch").unwrap();
    let saved = repo.refname_to_id("refs/heads/feature/login").unwrap();

    let preflight = workspace
        .preflight_send("origin", &send_request("Login"), &mut RemoteOptions::new())
        .unwrap();
    assert!(preflight.creates_variation);
    assert_eq!(preflight.variation, "cutready/login");
    let mut host = FakeHost::default();
    let result = workspace
        .send(
            "origin",
            &send_request("Login"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();

    assert!(result.created_variation);
    assert_eq!(result.variation, "cutready/login");
    assert_eq!(workspace.current_variation().unwrap(), "cutready/login");
    assert_eq!(remote_branch(bare.path(), "cutready/login"), Some(saved));
    assert!(
        remote_branch(bare.path(), "feature/login").is_none(),
        "Draftline only pushes branches it owns"
    );
    assert_eq!(
        repo.refname_to_id("refs/heads/feature/login").unwrap(),
        saved
    );
    assert_eq!(host.open[0].0.head, "cutready/login");

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
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
    let tauri = draftline::tauri_contract::TauriCommandError::from(error);
    assert_eq!(tauri.details.unwrap()["branch"], "feature/login-2");
    assert!(repo
        .find_branch("feature/login-2", git2::BranchType::Local)
        .is_err());
}

#[test]
fn saves_and_updates_work_on_a_developer_branch_like_a_teammates() {
    use draftline::{DiagnosticSeverity, SafeNextAction};
    let (temp, repo) = code_repo();
    let (_bare, _other_dir, _other) = with_remote(&repo);
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    // A developer runs `git switch -c feature/x` in the shared checkout.
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/x", &head, false).unwrap();
    repo.set_head("refs/heads/feature/x").unwrap();
    let inspection = workspace.inspect().unwrap();
    assert!(
        !inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Blocking),
        "{inspection:?}"
    );
    assert_eq!(
        inspection.safe_next_actions,
        vec![SafeNextAction::NormalWork]
    );

    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");
    let saved = workspace.save_version("Pitch").unwrap();
    assert_eq!(workspace.current_variation().unwrap(), "feature/x");
    assert_eq!(
        repo.refname_to_id("refs/heads/feature/x")
            .unwrap()
            .to_string(),
        saved.id().as_str()
    );
    workspace.preflight_apply_incoming("origin").unwrap();
    workspace.preflight_merge_base("origin").unwrap();

    // Starting a variation here is still a way to keep work apart.
    let error = workspace.start_variation_here("feature/y").unwrap_err();
    assert!(
        matches!(error, DraftlineError::VariationOutsideNamespace(_)),
        "{error:?}"
    );
    workspace.start_variation_here("cutready/rescue").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "scene one, again\n");
    workspace.save_version("Again").unwrap();
    assert_eq!(
        repo.refname_to_id("refs/heads/feature/x")
            .unwrap()
            .to_string(),
        saved.id().as_str(),
        "the developer's branch stays where the last save there left it"
    );
}

#[test]
fn a_detached_checkout_refuses_saves_until_a_variation_starts_here() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap().id();
    repo.set_head_detached(head).unwrap();
    write_file(temp.path(), "cutready/intro.sk", "scene one, revised\n");

    let error = workspace.save_version("Pitch").unwrap_err();
    assert!(
        matches!(error, DraftlineError::NoCurrentVariation),
        "{error:?}"
    );
    assert!(workspace
        .inspect()
        .unwrap()
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == draftline::DiagnosticCode::NoCurrentVariation));
    workspace.start_variation_here("cutready/rescue").unwrap();
    workspace.save_version("Pitch").unwrap();
    assert_eq!(workspace.current_variation().unwrap(), "cutready/rescue");
}

#[test]
fn starting_a_variation_on_an_unborn_developer_branch_keeps_it_unborn() {
    let temp = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(temp.path()).unwrap();
    repo.set_head("refs/heads/feature/x").unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();

    workspace.start_variation_here("cutready/first").unwrap();

    assert_eq!(
        repo.find_reference("HEAD")
            .unwrap()
            .symbolic_target()
            .unwrap(),
        Some("refs/heads/cutready/first")
    );
    assert!(repo.find_reference("refs/heads/cutready/first").is_err());
    assert!(repo.find_reference("refs/heads/feature/x").is_err());
}

#[test]
fn starting_a_variation_refuses_an_existing_name_and_leaves_head_alone() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("cutready/taken", &head, false).unwrap();

    assert!(matches!(
        workspace.start_variation_here("cutready/taken"),
        Err(DraftlineError::VariationAlreadyExists(_))
    ));
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
}

#[test]
fn send_leaves_a_stale_local_base_where_it_is() {
    let (temp, repo) = code_repo();
    let (bare, other_dir, other) = with_remote(&repo);
    write_file(other_dir.path(), "README.md", "# teammate\n");
    commit_all(&other, "Teammate");
    push_main(&other);
    let fresh = remote_branch(bare.path(), "main").unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    let saved = workspace.save_version("Pitch").unwrap();

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
    // Draftline never moves or rewinds the base, even one behind upstream.
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap().to_string(),
        saved.id().as_str()
    );
    assert_ne!(repo.refname_to_id("refs/heads/main").unwrap(), fresh);
}
#[test]
fn a_folder_move_and_older_saves_are_not_developer_commits() {
    let (temp, repo) = code_repo();
    let (_bare, _other_dir, _other) = with_remote(&repo);
    let mut workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    workspace.save_version("Pitch").unwrap();
    workspace.move_folder("demos/pitch").unwrap();
    write_file(temp.path(), "demos/pitch/intro.sk", "pitch, moved\n");
    workspace.save_version("Moved pitch").unwrap();

    let preflight = workspace
        .preflight_send("origin", &send_request("Pitch"), &mut RemoteOptions::new())
        .unwrap();
    assert!(preflight.developer_commits.is_empty(), "{preflight:?}");
}
#[test]
fn send_carries_a_teammates_unpushed_base_commits_and_leaves_their_base_alone() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    let upstream = remote_branch(bare.path(), "main").unwrap();
    write_file(temp.path(), "src/secret.rs", "pub fn unreleased() {}\n");
    let developer = commit_all(&repo, "Unreleased developer work");
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    workspace.save_version("Pitch").unwrap();
    let local_main = repo.refname_to_id("refs/heads/main").unwrap();

    let preflight = workspace
        .preflight_send("origin", &send_request("Pitch"), &mut RemoteOptions::new())
        .unwrap();
    assert!(preflight.can_proceed, "{preflight:?}");
    assert!(preflight.creates_variation);
    assert_eq!(preflight.outgoing_versions, 2);
    assert_eq!(preflight.developer_commits.len(), 1);
    assert_eq!(preflight.developer_commits[0].id, developer.to_string());
    assert_eq!(
        preflight.developer_commits[0].outside_paths,
        vec![PathBuf::from("src/secret.rs")]
    );

    let mut host = FakeHost::default();
    let result = workspace
        .send(
            "origin",
            &draftline::SendRequest {
                expected: Some(preflight.token),
                ..send_request("Pitch")
            },
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(result.variation, "cutready/pitch");
    assert_eq!(
        remote_branch(bare.path(), "cutready/pitch"),
        Some(local_main)
    );
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap(),
        local_main,
        "the teammate's unpushed main is theirs to push"
    );
    assert_eq!(remote_branch(bare.path(), "main"), Some(upstream));
    assert_eq!(host.open.len(), 1);
}

#[test]
fn send_carries_a_variation_started_on_a_teammates_branch() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/x", &head, false).unwrap();
    repo.set_head("refs/heads/feature/x").unwrap();
    write_file(temp.path(), "src/feature.rs", "pub fn x() {}\n");
    let feature = commit_all(&repo, "Feature x");
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    workspace.start_variation_here("cutready/x").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "x pitch\n");
    workspace.save_version("X pitch").unwrap();

    let preflight = workspace
        .preflight_send(
            "origin",
            &send_request("X pitch"),
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert!(preflight.can_proceed);
    assert_eq!(preflight.developer_commits.len(), 1);
    workspace
        .send(
            "origin",
            &send_request("X pitch"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap();
    let sent = remote_branch(bare.path(), "cutready/x").unwrap();
    assert!(repo.graph_descendant_of(sent, feature).unwrap());
    assert_eq!(repo.refname_to_id("refs/heads/feature/x").unwrap(), feature);
}

#[test]
fn send_acts_only_on_the_branch_base_and_commit_the_host_approved() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    workspace.start_variation_here("cutready/pitch").unwrap();
    write_file(temp.path(), "cutready/intro.sk", "pitch\n");
    workspace.save_version("Pitch").unwrap();
    let approved = workspace
        .preflight_send("origin", &send_request("Pitch"), &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(approved.token.branch, "cutready/pitch");
    assert_eq!(approved.token.variation, "cutready/pitch");
    assert_eq!(approved.token.base, "main");
    let approved_request = draftline::SendRequest {
        expected: Some(approved.token.clone()),
        ..send_request("Pitch")
    };

    // Same commit, different branch: not what the host approved.
    workspace.start_variation_here("cutready/other").unwrap();
    let error = workspace
        .send(
            "origin",
            &approved_request,
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();
    assert!(
        matches!(error, DraftlineError::LocalStateChanged { .. }),
        "{error:?}"
    );
    assert_eq!(remote_branch(bare.path(), "cutready/other"), None);

    // The default branch renamed between preflight and Send.
    workspace
        .switch_variation(
            &draftline::VariationId::from("cutready/pitch"),
            draftline::SwitchPolicy::AbortIfDirty,
        )
        .unwrap();
    let remote = git2::Repository::open_bare(bare.path()).unwrap();
    let tip = remote.refname_to_id("refs/heads/main").unwrap();
    remote
        .reference("refs/heads/trunk", tip, false, "rename")
        .unwrap();
    remote.set_head("refs/heads/trunk").unwrap();
    remote
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    let error = workspace
        .send(
            "origin",
            &approved_request,
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();
    assert!(
        matches!(error, DraftlineError::LocalStateChanged { .. }),
        "{error:?}"
    );
    assert_eq!(remote_branch(bare.path(), "cutready/pitch"), None);

    // A fresh approval goes through.
    let approved = workspace
        .preflight_send("origin", &send_request("Pitch"), &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(approved.token.base, "trunk");
    let mut host = FakeHost::default();
    workspace
        .send(
            "origin",
            &draftline::SendRequest {
                expected: Some(approved.token),
                ..send_request("Pitch")
            },
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(host.open[0].0.base, "trunk");
    assert!(remote_branch(bare.path(), "cutready/pitch").is_some());
}

#[test]
fn send_and_publish_preflights_report_large_files_and_lfs_pointers_going_out() {
    let (temp, repo) = code_repo();
    let (_bare, _other_dir, _other) = with_remote(&repo);
    write_file(
        temp.path(),
        "cutready/.gitattributes",
        "hero.md filter=lfs -text\n",
    );
    write_file(temp.path(), "cutready/hero.md", LFS_POINTER);
    commit_all(&repo, "Dev: hero image in LFS");
    let workspace = Workspace::init_with_policy(
        temp.path().join("cutready"),
        cutready_policy().with_large_file_threshold(64),
    )
    .unwrap()
    .with_variation_namespace("cutready/")
    .unwrap();
    write_file(temp.path(), "cutready/big.md", &"x".repeat(100));
    workspace.save_version("Big").unwrap();
    let kinds = |files: &[draftline::OutgoingFile]| {
        let mut kinds = files
            .iter()
            .map(|file| (file.path.clone(), file.kind))
            .collect::<Vec<_>>();
        kinds.sort_by(|a, b| a.0.cmp(&b.0));
        kinds
    };
    let expected = vec![
        (PathBuf::from("cutready/big.md"), OutgoingFileKind::Large),
        (
            PathBuf::from("cutready/hero.md"),
            OutgoingFileKind::LfsPointer,
        ),
    ];

    let preflight = workspace
        .preflight_send("origin", &send_request("Big"), &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(kinds(&preflight.outgoing_files), expected);
    assert!(!preflight.outgoing_files_truncated);
    assert!(
        preflight.can_proceed,
        "reported, not refused: {preflight:?}"
    );

    // A stale tracking ref the remote no longer advertises hides nothing.
    let head = repo.head().unwrap().target().unwrap();
    repo.reference("refs/remotes/origin/gone", head, true, "stale")
        .unwrap();
    let approved = workspace
        .preflight_send("origin", &send_request("Big"), &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(kinds(&approved.outgoing_files), expected);
    assert_eq!(approved.token.head, Some(head.to_string()));

    // Send refuses a head the host never checked.
    write_file(temp.path(), "cutready/later.md", "later\n");
    workspace.save_version("Later").unwrap();
    let error = workspace
        .send(
            "origin",
            &draftline::SendRequest {
                expected: Some(approved.token.clone()),
                ..send_request("Big")
            },
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap_err();
    assert!(
        matches!(error, draftline::DraftlineError::LocalStateChanged { .. }),
        "{error:?}"
    );

    workspace.start_variation_here("cutready/big").unwrap();
    let preflight = workspace.preflight_publish("origin").unwrap();
    assert_eq!(kinds(&preflight.outgoing_files), expected);
    assert!(preflight.can_publish);
    assert_eq!(
        serde_json::to_value(&preflight.outgoing_files[0]).unwrap()["kind"]
            .as_str()
            .map(|kind| ["large", "lfs_pointer"].contains(&kind)),
        Some(true)
    );
}

#[test]
fn standalone_send_never_flags_developer_commits() {
    let (temp, repo) = code_repo();
    let (_bare, _other_dir, _other) = with_remote(&repo);
    write_file(temp.path(), "src/main.rs", "fn main() { changed() }\n");
    commit_all(&repo, "Whole-repo work");
    let workspace = Workspace::open(temp.path()).unwrap();
    let preflight = workspace
        .preflight_send("origin", &send_request("All"), &mut RemoteOptions::new())
        .unwrap();
    assert!(preflight.developer_commits.is_empty(), "{preflight:?}");
    assert!(preflight.can_proceed, "{preflight:?}");
}

fn tree_text(repo: &git2::Repository, rev: &str, relative: &str) -> Option<String> {
    let tree = repo.revparse_single(rev).unwrap().peel_to_tree().unwrap();
    let entry = tree.get_path(Path::new(relative)).ok()?;
    let blob = repo.find_blob(entry.id()).unwrap();
    Some(String::from_utf8_lossy(blob.content()).into_owned())
}

#[test]
fn incoming_edits_from_before_a_move_land_in_the_moved_folder() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let mut workspace = scoped(&temp);
    write_file(temp.path(), "cutready/outro.sk", "my outro\n");
    workspace.save_version("My outro").unwrap();
    workspace.move_folder("demos/pitch").unwrap();

    write_file(other_dir.path(), "cutready/intro.sk", "teammate intro\n");
    write_file(other_dir.path(), "src/lib.rs", "pub fn shared() {}\n");
    commit_all(&other, "Teammate intro");
    push_main(&other);

    workspace.fetch_remote("origin").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    assert!(report.can_merge_cleanly, "{report:?}");
    workspace
        .merge_incoming(report.token.unwrap(), "Merge", &mut RemoteOptions::new())
        .unwrap();

    assert_eq!(
        read(temp.path(), "demos/pitch/intro.sk"),
        "teammate intro\n"
    );
    assert_eq!(read(temp.path(), "demos/pitch/outro.sk"), "my outro\n");
    assert_eq!(read(temp.path(), "src/lib.rs"), "pub fn shared() {}\n");
    assert!(!temp.path().join("cutready").exists());
    assert_eq!(tree_text(&repo, "HEAD", "cutready/intro.sk"), None);
}

#[test]
fn incoming_developer_files_where_the_folder_moved_stop_the_merge() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let mut workspace = scoped(&temp);
    workspace.move_folder("demos/pitch").unwrap();

    // A developer, still seeing the folder at cutready, puts code where it now is.
    write_file(other_dir.path(), "demos/pitch/server.rs", "fn serve() {}\n");
    commit_all(&other, "Pitch server");
    push_main(&other);

    workspace.fetch_remote("origin").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    assert_eq!(
        report.developer_conflicts,
        vec![PathBuf::from("demos/pitch/server.rs")]
    );
    assert!(report.token.is_none() && !report.can_merge_cleanly);
    let head = repo.head().unwrap().target().unwrap();
    assert!(!temp.path().join("demos/pitch/server.rs").exists());
    assert_eq!(repo.head().unwrap().target().unwrap(), head);
}

#[test]
fn an_incoming_move_elsewhere_is_followed_before_merging() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let mut theirs =
        Workspace::init_with_policy(other_dir.path().join("cutready"), cutready_policy()).unwrap();
    theirs.move_folder("demos/pitch").unwrap();
    write_file(other_dir.path(), "demos/pitch/intro.sk", "teammate intro\n");
    theirs.save_version("Teammate intro").unwrap();
    push_main(&other);

    let mut workspace = scoped(&temp);
    write_file(temp.path(), "cutready/outro.sk", "my outro\n");
    workspace.save_version("My outro").unwrap();
    workspace.fetch_remote("origin").unwrap();
    let err = workspace.preflight_merge_incoming("origin").unwrap_err();
    assert!(
        matches!(&err, DraftlineError::IncomingFolderElsewhere(folder) if folder == Path::new("demos/pitch")),
        "{err:?}"
    );

    workspace.move_folder("demos/pitch").unwrap();
    let report = workspace.preflight_merge_incoming("origin").unwrap();
    assert!(report.can_merge_cleanly, "{report:?}");
    workspace
        .merge_incoming(report.token.unwrap(), "Merge", &mut RemoteOptions::new())
        .unwrap();
    assert_eq!(
        read(temp.path(), "demos/pitch/intro.sk"),
        "teammate intro\n"
    );
    assert_eq!(read(temp.path(), "demos/pitch/outro.sk"), "my outro\n");
    assert_eq!(tree_text(&repo, "HEAD", "cutready/outro.sk"), None);
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
fn a_renamed_default_branch_is_followed_and_saves_on_the_old_one_can_move() {
    let (temp, repo) = code_repo();
    let (bare, _other_dir, _other) = with_remote(&repo);
    repo.find_remote("origin")
        .unwrap()
        .fetch(&["main"], None, None)
        .unwrap();
    // As a clone leaves it: origin/HEAD names the default branch.
    repo.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        true,
        "clone",
    )
    .unwrap();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    workspace.fetch_remote("origin").unwrap();
    assert_eq!(workspace.base_status("origin").unwrap().variation, "main");
    write_file(temp.path(), "cutready/intro.sk", "scene one, unsent\n");
    let saved = workspace.save_version("Unsent intro").unwrap();

    // GitHub renames main to trunk.
    let remote = git2::Repository::open_bare(bare.path()).unwrap();
    let tip = remote.refname_to_id("refs/heads/main").unwrap();
    remote
        .reference("refs/heads/trunk", tip, false, "rename")
        .unwrap();
    remote.set_head("refs/heads/trunk").unwrap();
    remote
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();

    workspace.fetch_remote("origin").unwrap();
    let status = workspace.base_status("origin").unwrap();
    assert_eq!(status.variation, "trunk");
    assert_eq!(status.base_renamed_from.as_deref(), Some("main"));
    assert_eq!(
        repo.find_reference("refs/remotes/origin/HEAD")
            .unwrap()
            .symbolic_target()
            .ok()
            .flatten(),
        Some("refs/remotes/origin/main"),
        "Draftline never rewrites the remote-tracking HEAD"
    );

    // The old base is now just a branch: saves still land on it, and Send
    // moves them to a variation aimed at the new base.
    write_file(temp.path(), "cutready/intro.sk", "scene one, later\n");
    workspace.save_version("Later").unwrap();
    let mut host = FakeHost::default();
    let result = workspace
        .send(
            "origin",
            &send_request("Intro"),
            &mut host,
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(result.base, "trunk");
    assert_eq!(host.open[0].0.base, "trunk");
    let sent = remote_branch(bare.path(), "cutready/intro").unwrap();
    assert!(repo
        .graph_descendant_of(sent, saved.id().as_str().parse().unwrap())
        .unwrap());

    // A cache recorded for another URL is ignored.
    let url = repo
        .find_remote("origin")
        .unwrap()
        .url()
        .unwrap()
        .to_string();
    repo.remote_set_url("origin", "https://example.invalid/other.git")
        .unwrap();
    assert_eq!(
        workspace.base_status("origin").unwrap().base_renamed_from,
        None
    );
    repo.remote_set_url("origin", &url).unwrap();

    // Once the old local branch is gone, the rename stops being reported.
    repo.find_branch("main", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();
    assert_eq!(
        workspace.base_status("origin").unwrap().base_renamed_from,
        None
    );
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

    // The merge carries developer code, but only from the base: Send passes.
    let sent = workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_eq!(
        remote_branch(bare.path(), "cutready/pitch"),
        Some(head.id())
    );
    assert!(!sent.created_variation);
}

#[test]
fn send_lists_a_merge_that_resolves_code_outside_the_project() {
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
    let sent = remote_branch(bare.path(), "cutready/pitch").unwrap();
    write_file(other_dir.path(), "src/lib.rs", "pub fn shared() {}\n");
    commit_all(&other, "Teammate on main");
    push_main(&other);
    workspace.fetch_remote("origin").unwrap();

    // A developer merges main by hand and edits code in the merge itself.
    let ours = repo.head().unwrap().peel_to_commit().unwrap();
    let theirs = repo
        .find_commit(repo.refname_to_id("refs/remotes/origin/main").unwrap())
        .unwrap();
    let mut index = repo.merge_commits(&ours, &theirs, None).unwrap();
    let blob = repo.blob(b"pub fn shared() { tweaked() }\n").unwrap();
    let mut entry = index.get_path(Path::new("src/lib.rs"), 0).unwrap();
    entry.id = blob;
    entry.file_size = 0;
    index.add(&entry).unwrap();
    let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    let merge = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Hand merge",
            &tree,
            &[&ours, &theirs],
        )
        .unwrap();

    let preflight = workspace
        .preflight_send("origin", &send_request("Pitch"), &mut RemoteOptions::new())
        .unwrap();
    let commits = &preflight.developer_commits;
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].id, merge.to_string());
    assert_eq!(commits[0].outside_paths, vec![PathBuf::from("src/lib.rs")]);
    assert!(preflight.can_proceed);
    workspace
        .send(
            "origin",
            &send_request("Pitch"),
            &mut FakeHost::default(),
            &mut RemoteOptions::new(),
        )
        .unwrap();
    assert_ne!(remote_branch(bare.path(), "cutready/pitch"), Some(sent));
    assert_eq!(remote_branch(bare.path(), "cutready/pitch"), Some(merge));
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
    assert_eq!(error.details.unwrap()["paths"][0], "src/main.rs");
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
fn a_clone_opens_the_folder_its_marker_names() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    let (bare, _other_dir, _other) = with_remote(&repo);

    let target = tempfile::tempdir().unwrap();
    let clone = Workspace::clone_workspace_with_policy(
        bare.path().to_str().unwrap(),
        target.path().join("copy"),
        cutready_policy(),
    )
    .unwrap();
    assert_eq!(clone.scope(), Path::new("cutready"));
    assert_eq!(read(target.path(), "copy/cutready/intro.sk"), "scene one\n");
}

#[test]
fn owned_worktrees_open_where_their_commit_keeps_the_folder() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    let (_bare, other_dir, _other) = with_remote(&repo);
    let worktrees = tempfile::tempdir().unwrap();
    let before = pitch
        .create_owned_worktree(
            worktrees.path(),
            "Before",
            "origin",
            draftline::WorktreeStart::CurrentVersion,
        )
        .unwrap();
    // A teammate moves the folder and shares it.
    let mut theirs =
        Workspace::open_with_policy(other_dir.path().join("cutready"), cutready_policy()).unwrap();
    theirs.move_folder(other_dir.path().join("demos")).unwrap();
    theirs.publish_changes("origin").unwrap();
    pitch.fetch_remote("origin").unwrap();

    let after = pitch
        .create_owned_worktree(
            worktrees.path(),
            "After",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .unwrap();
    assert_eq!(after.project_path, after.path.join("demos"));
    assert_eq!(read(&after.path, "demos/intro.sk"), "scene one\n");
    let mut listed = pitch.owned_worktrees().unwrap();
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    let paths: Vec<_> = listed
        .iter()
        .map(|owned| owned.project_path.clone())
        .collect();
    assert_eq!(
        paths,
        [after.path.join("demos"), before.path.join("cutready")]
    );
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

    assert_eq!(repo.head().unwrap().shorthand().ok(), Some("main"));
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
    // A finished recovery-tracked operation leaves a completed record behind.
    write_file(&owned.path, "cutready/intro.sk", "scrapped\n");
    copy.discard_changes().unwrap();
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

    std::fs::remove_file(owned.path.join("notes.txt")).unwrap();
    workspace.remove_owned_worktree(&owned.name, false).unwrap();
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
fn removing_an_owned_worktree_waits_for_draftline_running_or_unfinished_there() {
    let (temp, _repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let worktrees = tempfile::tempdir().unwrap();
    let owned = workspace
        .create_owned_worktree(
            worktrees.path(),
            "Busy",
            "origin",
            draftline::WorktreeStart::LatestBase,
        )
        .unwrap();
    let draftline = temp
        .path()
        .join(".git/worktrees")
        .join(&owned.name)
        .join("draftline");
    std::fs::create_dir_all(&draftline).unwrap();

    std::fs::write(draftline.join("operation.lock"), "{}").unwrap();
    assert!(matches!(
        workspace.remove_owned_worktree(&owned.name, true),
        Err(DraftlineError::WorkspaceLocked)
    ));
    std::fs::remove_file(draftline.join("operation.lock")).unwrap();

    std::fs::write(draftline.join("recovery.json"), "{}").unwrap();
    assert!(matches!(
        workspace.remove_owned_worktree(&owned.name, false),
        Err(DraftlineError::OwnedWorktreeHasChanges(_))
    ));
    assert!(owned.path.exists());
    workspace.remove_owned_worktree(&owned.name, true).unwrap();
    assert!(!owned.path.exists());
}

#[test]
fn publishing_and_owned_worktrees_wait_for_another_draftline_operation() {
    let (temp, _repo) = code_repo();
    let workspace = scoped(&temp).with_variation_namespace("cutready/").unwrap();
    let lock = temp.path().join(".git/draftline/operation.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "{}").unwrap();

    assert!(matches!(
        workspace.publish_changes("origin"),
        Err(DraftlineError::WorkspaceLocked)
    ));
    let worktrees = tempfile::tempdir().unwrap();
    assert!(matches!(
        workspace.create_owned_worktree(
            worktrees.path(),
            "Blocked",
            "origin",
            draftline::WorktreeStart::LatestBase,
        ),
        Err(DraftlineError::WorkspaceLocked)
    ));
    assert!(workspace.owned_worktrees().unwrap().is_empty());
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

/// Fabricates what Draftline 0.5 left behind after a history cleanup it
/// applied locally but never published: `main` points at a squashed
/// replacement with the same files, and `pending.json` remembers the original.
fn legacy_cleanup(temp: &tempfile::TempDir, repo: &git2::Repository) -> (git2::Oid, git2::Oid) {
    let base = repo.head().unwrap().peel_to_commit().unwrap();
    write_file(temp.path(), "cutready/intro.sk", "draft one\n");
    commit_all(repo, "Draft one");
    write_file(temp.path(), "cutready/intro.sk", "draft two\n");
    let original = commit_all(repo, "Draft two");
    let tree = repo.find_commit(original).unwrap().tree().unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    let replacement = repo
        .commit(None, &signature, &signature, "Cleaned up", &tree, &[&base])
        .unwrap();
    repo.reference("refs/heads/main", replacement, true, "0.5 cleanup")
        .unwrap();
    repo.reference(
        "refs/draftline/backups/history-cleanup/cleanup-1/main",
        original,
        true,
        "0.5 backup",
    )
    .unwrap();
    let pending = serde_json::json!([{
        "plan_id": "cleanup-1",
        "target_variation": "main",
        "expected_local_head": original.to_string(),
        "replacement_head": replacement.to_string(),
        "backup_refs": ["refs/draftline/backups/history-cleanup/cleanup-1/main"],
        "ref_updates": [],
        "publish_status": "not_published",
    }]);
    let path = repo.path().join("draftline/history-cleanup/pending.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&pending).unwrap()).unwrap();
    (original, replacement)
}

#[test]
fn dismissing_an_unpublished_0_5_cleanup_moves_nothing_and_keeps_its_backup() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    let (original, replacement) = legacy_cleanup(&temp, &repo);

    let summary = workspace.workspace_summary().unwrap();
    let [cleanup] = summary.legacy_history_cleanups.as_slice() else {
        panic!("{:?}", summary.legacy_history_cleanups);
    };
    assert_eq!(cleanup.plan_id, "cleanup-1");
    assert_eq!(cleanup.replacement_head, replacement.to_string());

    workspace
        .dismiss_legacy_history_cleanup("cleanup-1")
        .unwrap();
    assert_eq!(
        repo.find_reference("refs/heads/main").unwrap().target(),
        Some(replacement),
        "dismissing never moves a branch"
    );
    assert!(!workspace.workspace_summary().unwrap().is_dirty);
    assert!(workspace.legacy_history_cleanups().unwrap().is_empty());
    assert!(!repo
        .path()
        .join("draftline/history-cleanup/pending.json")
        .exists());
    assert!(matches!(
        workspace.dismiss_legacy_history_cleanup("cleanup-1"),
        Err(DraftlineError::RecoveryNotFound(_))
    ));

    // The original history is still a support ref the host can bring back.
    let backups = workspace
        .list_support_refs(draftline::SupportRefScope::Local)
        .unwrap();
    let [backup] = backups.as_slice() else {
        panic!("{backups:?}");
    };
    let restored = workspace
        .restore_support_ref_as_variation(&backup.id, "before-cleanup")
        .unwrap();
    assert_eq!(
        repo.find_reference("refs/heads/before-cleanup")
            .unwrap()
            .target(),
        Some(original)
    );
    assert_eq!(restored.id().as_str(), "before-cleanup");
    assert_eq!(
        repo.find_reference("refs/heads/main").unwrap().target(),
        Some(replacement)
    );
}

#[test]
fn unreadable_0_5_cleanup_records_are_skipped() {
    let (temp, repo) = code_repo();
    let workspace = scoped(&temp);
    legacy_cleanup(&temp, &repo);
    let path = repo.path().join("draftline/history-cleanup/pending.json");
    let mut records: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    records.push(serde_json::json!({ "plan_id": 7 }));
    fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();

    let cleanups = workspace.legacy_history_cleanups().unwrap();
    let [cleanup] = cleanups.as_slice() else {
        panic!("{cleanups:?}");
    };
    assert_eq!(cleanup.plan_id, "cleanup-1");

    fs::write(&path, b"{ not json").unwrap();
    assert!(workspace.legacy_history_cleanups().unwrap().is_empty());
    assert!(workspace
        .workspace_summary()
        .unwrap()
        .legacy_history_cleanups
        .is_empty());
}

#[test]
fn a_switch_interrupted_onto_a_branch_naming_another_folder_can_finish() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();
    drop(pitch);

    // A branch where the folder moved to deck.
    repo.branch(
        "deck-home",
        &repo.head().unwrap().peel_to_commit().unwrap(),
        false,
    )
    .unwrap();
    repo.set_head("refs/heads/deck-home").unwrap();
    write_file(temp.path(), "deck/intro.sk", "intro\n");
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    fs::write(
        temp.path().join(".draftline"),
        r#"{"format":1,"folder":"deck"}"#,
    )
    .unwrap();
    commit_all(&repo, "Move to deck");
    let target = repo.head().unwrap().peel_to_tree().unwrap();

    // Draftline's switch from main crashed after the checkout, before HEAD moved.
    repo.set_head("refs/heads/main").unwrap();
    let from = repo.head().unwrap().peel_to_tree().unwrap().id();
    let mut index = repo.index().unwrap();
    index.read_tree(&target).unwrap();
    index.write().unwrap();
    let ledger = serde_json::json!({
        "operation_id": "interrupted",
        "operation": "SwitchVariation",
        "original_variation": "main",
        "target": "deck-home",
        "completed": false,
        "project": "cutready",
        "checkout": { "from_tree": from.to_string(), "to_tree": target.id().to_string() },
    });
    fs::write(
        repo.path().join("draftline").join("recovery.json"),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();

    // cutready is gone from disk; deck, which the new marker names, finishes it.
    let deck = Workspace::open_with_policy(temp.path().join("deck"), cutready_policy()).unwrap();
    deck.repair_recovery("interrupted").unwrap();
    assert_eq!(
        repo.head().unwrap().shorthand(),
        Ok("deck-home"),
        "the switch finished"
    );
}

#[test]
fn only_the_project_that_was_interrupted_can_finish_its_recovery() {
    let (temp, repo) = code_repo();
    let (_bare, other_dir, other) = with_remote(&repo);
    let pitch = scoped(&temp);
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

    // Once Draftline manages another folder, it can't write until pitch
    // finishes the recovery.
    let deck = temp.path().join("deck");
    fs::create_dir_all(&deck).unwrap();
    let marker = temp.path().join(".draftline");
    fs::write(&marker, r#"{"format":1,"folder":"deck"}"#).unwrap();
    let moved = Workspace::open_with_policy(&deck, cutready_policy()).unwrap();
    write_file(temp.path(), "deck/slide.sk", "slide\n");
    let error = moved.save_version("Slide").unwrap_err();
    assert!(
        matches!(&error, DraftlineError::RecoveryInOtherFolder(project) if project == Path::new("cutready")),
        "{error:?}"
    );
    assert_eq!(
        pitch.inspect().unwrap().safe_next_actions,
        vec![draftline::SafeNextAction::RepairRecovery]
    );

    // Pitch can still open while the marker names deck, so neither folder is
    // stuck, but it may only drop the record: repairing or rolling back would
    // write to a folder Draftline no longer manages.
    let probe = Workspace::probe(temp.path().join("cutready")).unwrap();
    assert_eq!(probe.suggestions[0].fix, draftline::FolderFix::Open);
    let reopened =
        Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    for error in [
        reopened.repair_recovery("interrupted").unwrap_err(),
        reopened.rollback_recovery("interrupted").unwrap_err(),
    ] {
        assert!(
            matches!(&error, DraftlineError::FolderIsElsewhere(folder) if folder == Path::new("deck")),
            "{error:?}"
        );
    }
    reopened.acknowledge_recovery().unwrap();
    moved.save_version("Slide").unwrap();
    assert!(matches!(
        pitch.save_version("Pitch").unwrap_err(),
        DraftlineError::FolderIsElsewhere(_)
    ));
}

fn refused(result: draftline::Result<Workspace>) -> DraftlineError {
    match result {
        Ok(workspace) => panic!("expected a refusal, opened {:?}", workspace.scope()),
        Err(error) => error,
    }
}

fn claimed_elsewhere(error: DraftlineError) -> PathBuf {
    match error {
        DraftlineError::FolderIsElsewhere(project) => project,
        other => panic!("expected FolderIsElsewhere, got {other:?}"),
    }
}

#[test]
fn a_checkout_holds_one_workspace() {
    let (temp, _repo) = code_repo();
    let pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();

    // Another folder, the repository root, and a nested folder are all refused.
    let deck = temp.path().join("deck").join("slides");
    let error = refused(Workspace::init_with_policy(&deck, cutready_policy()));
    assert_eq!(claimed_elsewhere(error), PathBuf::from("cutready"));
    assert!(
        !temp.path().join("deck").exists(),
        "a refused init leaves no folder behind"
    );
    for path in [
        temp.path().to_path_buf(),
        temp.path().join("cutready").join("scenes"),
    ] {
        fs::create_dir_all(&path).unwrap();
        let error = refused(Workspace::open_with_policy(&path, cutready_policy()));
        assert_eq!(claimed_elsewhere(error), PathBuf::from("cutready"));
    }

    // The claimed folder opens normally, however it is reached.
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    Workspace::init_with_policy(temp.path().join("cutready").join("."), cutready_policy()).unwrap();
}

#[test]
fn a_refused_init_keeps_folders_that_already_existed() {
    let (temp, _repo) = code_repo();
    scoped(&temp);
    write_file(temp.path(), "deck/notes.txt", "keep\n");
    let error = refused(Workspace::init_with_policy(
        temp.path().join("deck").join("new"),
        cutready_policy(),
    ));
    claimed_elsewhere(error);
    assert!(!temp.path().join("deck").join("new").exists());
    assert_eq!(read(temp.path(), "deck/notes.txt"), "keep\n");
}

#[test]
fn a_root_workspace_refuses_project_folders() {
    let (temp, _repo) = code_repo();
    Workspace::open_with_policy(temp.path(), cutready_policy()).unwrap();
    let error = refused(Workspace::init_with_policy(
        temp.path().join("cutready"),
        cutready_policy(),
    ));
    assert!(error.to_string().contains("the repository root"), "{error}");
    assert_eq!(claimed_elsewhere(error), PathBuf::new());
}

#[test]
fn each_linked_worktree_holds_its_own_workspace() {
    let (temp, repo) = code_repo();
    scoped(&temp);
    let other = tempfile::tempdir().unwrap();
    let linked = other.path().join("linked");
    repo.worktree("linked", &linked, None).unwrap();

    let deck = Workspace::init_with_policy(linked.join("deck"), cutready_policy()).unwrap();
    assert_eq!(deck.scope(), Path::new("deck"));
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    let error = refused(Workspace::open_with_policy(
        linked.join("cutready"),
        cutready_policy(),
    ));
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));
}

#[test]
fn a_handle_on_a_folder_draftline_no_longer_manages_cant_keep_writing() {
    let (temp, repo) = code_repo();
    let stale = scoped(&temp);
    fs::create_dir_all(temp.path().join("deck")).unwrap();
    fs::write(
        temp.path().join(".draftline"),
        "{ \"format\": 1, \"folder\": \"deck\" }\n",
    )
    .unwrap();

    write_file(temp.path(), "cutready/intro.sk", "late edit\n");
    let error = stale.save_version("Late").unwrap_err();
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));
    let lock = repo.path().join("draftline").join("operation.lock");
    assert!(!lock.exists(), "the refused save released the lock");
    let error = stale.create_variation("cutready/late").unwrap_err();
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));
    let error = stale.fetch_remote("origin").unwrap_err();
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));
    let error = stale.clear_stale_lock().unwrap_err();
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));
    assert!(repo
        .find_branch("cutready/late", git2::BranchType::Local)
        .is_err());
    Workspace::open_with_policy(temp.path().join("deck"), cutready_policy()).unwrap();
}

#[test]
fn a_missing_folder_is_reported_until_it_comes_back() {
    let (temp, _repo) = code_repo();
    let pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();

    // Someone renames the claimed folder outside Draftline. The handle that
    // was open refuses to save rather than record every file as deleted.
    fs::rename(temp.path().join("cutready"), temp.path().join("pitch")).unwrap();
    let error = pitch.save_version("Gone").unwrap_err();
    assert!(
        matches!(error, DraftlineError::FolderMissing(_)),
        "{error:?}"
    );
    drop(pitch);
    let renamed = temp.path().join("pitch");
    let error = refused(Workspace::open_with_policy(&renamed, cutready_policy()));
    assert!(
        matches!(&error, DraftlineError::FolderMissing(project) if project == Path::new("cutready")),
        "{error:?}"
    );
    assert!(error.to_string().contains("missing"), "{error}");
    let tauri = draftline::tauri_contract::TauriCommandError::from(error);
    assert_eq!(tauri.code, "folder_missing");
    assert_eq!(tauri.details.unwrap()["folder"], "cutready");

    // A refused init cleans up the folders it created.
    let error = refused(Workspace::init_with_policy(
        temp.path().join("deck"),
        cutready_policy(),
    ));
    assert!(
        matches!(error, DraftlineError::FolderMissing(_)),
        "{error:?}"
    );
    assert!(!temp.path().join("deck").exists());

    fs::rename(&renamed, temp.path().join("cutready")).unwrap();
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
}

#[test]
fn a_branch_without_the_folder_reports_it_missing_but_can_switch_back() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    let base = draftline::VariationId::from(pitch.current_variation().unwrap());
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();

    // A developer branch that drops the project folder is checked out.
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/old", &head, false).unwrap();
    repo.set_head("refs/heads/feature/old").unwrap();
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_dir(Path::new("cutready"), 0).unwrap();
    index.write().unwrap();
    commit_all(&repo, "Dev: drop the deck");

    // Opening another folder points at the missing one, not a live claim.
    fs::create_dir_all(temp.path().join("deck")).unwrap();
    let error = refused(Workspace::open_with_policy(
        temp.path().join("deck"),
        cutready_policy(),
    ));
    assert!(
        matches!(&error, DraftlineError::FolderMissing(project) if project == Path::new("cutready")),
        "{error:?}"
    );

    // The open handle can still switch back to where the folder lives.
    pitch
        .switch_variation(&base, draftline::SwitchPolicy::AbortIfDirty)
        .unwrap();
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "intro\n");
}

#[test]
fn a_record_naming_a_folder_outside_the_checkout_is_invalid() {
    let (temp, repo) = code_repo();
    scoped(&temp);
    let marker = temp.path().join(".draftline");
    let claim = repo.path().join("draftline").join("workspace.json");
    let mut folders = vec!["../elsewhere", "/abs", "cutready/", "./cutready", ""];
    if cfg!(windows) {
        // A backslash is a separator only on Windows; records always use `/`.
        folders.push("cutready\\\\scenes");
    }
    for folder in folders {
        fs::write(&marker, format!(r#"{{"format":1,"folder":"{folder}"}}"#)).unwrap();
        let error = refused(Workspace::open_with_policy(
            temp.path().join("cutready"),
            cutready_policy(),
        ));
        assert!(
            matches!(error, DraftlineError::InvalidFolderMarker(_)),
            "marker {folder}: {error:?}"
        );
        let tauri = draftline::tauri_contract::TauriCommandError::from(error);
        assert_eq!(tauri.code, "invalid_folder_marker");
    }
    fs::remove_file(&marker).unwrap();

    // The checkout's own record is held to the same rules.
    for folder in ["../elsewhere", "/abs"] {
        fs::write(&claim, format!(r#"{{"project":"{folder}"}}"#)).unwrap();
        let error = refused(Workspace::open_with_policy(
            temp.path().join("cutready"),
            cutready_policy(),
        ));
        assert!(
            matches!(error, DraftlineError::InvalidFolderMarker(_)),
            "claim {folder}: {error:?}"
        );
    }
    fs::remove_file(&claim).unwrap();
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
}

#[test]
fn an_unreadable_record_is_reported_and_deleting_it_resets_it() {
    let (temp, repo) = code_repo();
    scoped(&temp);
    let marker = temp.path().join(".draftline");
    for contents in ["not json", r#"{"format":2,"folder":"cutready"}"#] {
        fs::write(&marker, contents).unwrap();
        let error = refused(Workspace::open_with_policy(
            temp.path().join("cutready"),
            cutready_policy(),
        ));
        assert!(
            matches!(error, DraftlineError::InvalidFolderMarker(_)),
            "{contents}: {error:?}"
        );
    }
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    let error = refused(Workspace::open_with_policy(
        temp.path().join("cutready"),
        cutready_policy(),
    ));
    assert!(error.to_string().contains("is a folder"), "{error}");
    fs::remove_dir(&marker).unwrap();

    let claim = repo.path().join("draftline").join("workspace.json");
    fs::write(&claim, "not json").unwrap();
    let error = refused(Workspace::open_with_policy(
        temp.path().join("cutready"),
        cutready_policy(),
    ));
    assert!(
        matches!(error, DraftlineError::InvalidFolderMarker(_)),
        "{error:?}"
    );
    fs::remove_file(&claim).unwrap();
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
}

#[test]
fn opening_writes_the_folder_marker_and_the_next_save_commits_it() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    let marker = fs::read_to_string(temp.path().join(".draftline")).unwrap();
    assert_eq!(
        marker,
        "{\n  \"format\": 1,\n  \"folder\": \"cutready\"\n}\n"
    );
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_name(".draftline").is_some());

    // A fresh clone has no local record; the committed marker decides.
    fs::remove_file(repo.path().join("draftline").join("workspace.json")).unwrap();
    drop(pitch);
    fs::create_dir_all(temp.path().join("deck")).unwrap();
    let error = refused(Workspace::open_with_policy(
        temp.path().join("deck"),
        cutready_policy(),
    ));
    assert_eq!(claimed_elsewhere(error), PathBuf::from("cutready"));
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    assert!(repo
        .path()
        .join("draftline")
        .join("workspace.json")
        .exists());
}

#[test]
fn a_root_folder_marker_names_the_whole_repository_and_survives_restore() {
    let (temp, repo) = code_repo();
    let root = Workspace::open_with_policy(temp.path(), cutready_policy()).unwrap();
    let marker = fs::read_to_string(temp.path().join(".draftline")).unwrap();
    assert!(marker.contains("\"folder\": \".\""), "{marker}");
    write_file(temp.path(), ".gitignore", "*.tmp\n");
    commit_all(&repo, "Ignore");
    write_file(temp.path(), "notes.md", "draft\n");
    root.save_version("Draft").unwrap();
    let first = root.versions().unwrap()[0].id().clone();
    write_file(temp.path(), "notes.md", "notes\n");
    root.save_version("Notes").unwrap();
    write_file(temp.path(), ".gitignore", "*.tmp\n*.log\n");
    commit_all(&repo, "Ignore logs");
    let head = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(head.get_name(".draftline").is_some());

    // A restore never rewrites the marker, even to an older copy.
    fs::write(
        temp.path().join(".draftline"),
        "{ \"format\": 1, \"folder\": \".\" }\n",
    )
    .unwrap();
    root.save_version("Reformat").unwrap();
    let marker = head_marker(&repo);
    root.restore_version_as_new_save(&first, "Back").unwrap();
    assert_eq!(head_marker(&repo), marker);
    assert_eq!(read(temp.path(), "notes.md"), "draft\n");
    assert_eq!(
        read(temp.path(), ".gitignore"),
        "*.tmp\n*.log\n",
        "repository files are not the folder's to restore"
    );
    let head = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(head.get_name(".draftline").is_some());
    assert!(temp.path().join(".draftline").is_file());
}

#[test]
fn restoring_a_version_never_brings_back_a_marker_head_lacks() {
    let (temp, repo) = code_repo();
    let root = Workspace::open_with_policy(temp.path(), cutready_policy()).unwrap();
    write_file(temp.path(), "notes.md", "draft\n");
    root.save_version("Draft").unwrap();
    let draft = root.versions().unwrap()[0].id().clone();

    // A developer removes the marker from the branch.
    fs::remove_file(temp.path().join(".draftline")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new(".draftline")).unwrap();
    index.write().unwrap();
    commit_all(&repo, "Dev: drop the marker");
    write_file(temp.path(), "notes.md", "later\n");
    root.save_version("Later").unwrap();
    assert!(repo
        .head()
        .unwrap()
        .peel_to_tree()
        .unwrap()
        .get_name(".draftline")
        .is_none());

    root.restore_version_as_new_save(&draft, "Back").unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_name(".draftline").is_none());
    assert_eq!(read(temp.path(), "notes.md"), "draft\n");
}

#[test]
fn a_committed_marker_corrects_a_stale_local_record() {
    let (temp, repo) = code_repo();
    scoped(&temp);
    let claim = repo.path().join("draftline").join("workspace.json");
    fs::write(&claim, r#"{"project":"deck"}"#).unwrap();
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    let record = fs::read_to_string(&claim).unwrap();
    assert!(record.contains("\"cutready\""), "{record}");
}

#[test]
fn switching_to_a_branch_whose_marker_names_another_folder_follows_it() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    let base = draftline::VariationId::from(pitch.current_variation().unwrap());
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();

    // A teammate's branch where Draftline manages `deck`.
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/deck", &head, false).unwrap();
    repo.set_head("refs/heads/feature/deck").unwrap();
    write_file(temp.path(), "deck/slide.sk", "slide\n");
    fs::write(
        temp.path().join(".draftline"),
        "{ \"format\": 1, \"folder\": \"deck\" }\n",
    )
    .unwrap();
    commit_all(&repo, "Dev: manage deck");
    repo.set_head(&format!("refs/heads/{}", base.as_str()))
        .unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let deck = draftline::VariationId::from("feature/deck".to_string());
    pitch
        .switch_variation(&deck, draftline::SwitchPolicy::AbortIfDirty)
        .unwrap();
    let error = pitch.save_version("Stale").unwrap_err();
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));

    // The host reopens the folder the branch names, and can switch back.
    drop(pitch);
    let deck_ws = Workspace::open_with_policy(temp.path().join("deck"), cutready_policy()).unwrap();
    deck_ws
        .switch_variation(&base, draftline::SwitchPolicy::AbortIfDirty)
        .unwrap();
    drop(deck_ws);
    Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
}

fn head_marker(repo: &git2::Repository) -> git2::Oid {
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let id = tree.get_name(".draftline").unwrap().id();
    id
}

#[test]
fn a_branch_without_the_marker_keeps_the_same_folder() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    let base = draftline::VariationId::from(pitch.current_variation().unwrap());
    let before = repo.head().unwrap().peel_to_commit().unwrap();
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();

    // A developer branch from before the marker was committed.
    repo.branch("feature/old", &before, false).unwrap();
    let old = draftline::VariationId::from("feature/old".to_string());
    pitch
        .switch_variation(&old, draftline::SwitchPolicy::AbortIfDirty)
        .unwrap();
    assert!(!temp.path().join(".draftline").exists());
    fs::create_dir_all(temp.path().join("deck")).unwrap();
    let error = refused(Workspace::open_with_policy(
        temp.path().join("deck"),
        cutready_policy(),
    ));
    assert_eq!(claimed_elsewhere(error), PathBuf::from("cutready"));

    // Reopening writes the marker again; switching back still works.
    drop(pitch);
    let pitch =
        Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    assert!(temp.path().join(".draftline").is_file());
    pitch
        .switch_variation(&base, draftline::SwitchPolicy::AbortIfDirty)
        .unwrap();
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "intro\n");
}

#[test]
fn an_interrupted_claim_never_leaves_a_partial_one() {
    let (temp, repo) = code_repo();
    let dir = repo.path().join("draftline");
    fs::create_dir_all(&dir).unwrap();
    // A crash after writing the temporary claim, before publishing it.
    fs::write(dir.join("workspace.json.1.0.tmp"), "").unwrap();
    fs::create_dir_all(temp.path().join("cutready")).unwrap();
    let workspace =
        Workspace::open_with_policy(temp.path().join("cutready"), cutready_policy()).unwrap();
    assert_eq!(workspace.scope(), Path::new("cutready"));
    let claim = fs::read_to_string(dir.join("workspace.json")).unwrap();
    assert!(claim.contains("\"cutready\""), "{claim}");
    let temps = fs::read_dir(&dir)
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            let name = name.to_string_lossy();
            name.ends_with(".tmp") && name != "workspace.json.1.0.tmp"
        })
        .count();
    assert_eq!(
        temps, 0,
        "publishing the claim cleans up its temporary file"
    );
}

#[test]
fn a_checkout_from_an_older_draftline_is_claimed_on_first_open() {
    let (temp, repo) = code_repo();
    scoped(&temp);
    fs::remove_file(repo.path().join("draftline").join("workspace.json")).unwrap();
    fs::remove_file(temp.path().join(".draftline")).unwrap();
    let deck = temp.path().join("deck");
    fs::create_dir_all(&deck).unwrap();
    Workspace::open_with_policy(&deck, cutready_policy()).unwrap();
    let error = refused(Workspace::open_with_policy(
        temp.path().join("cutready"),
        cutready_policy(),
    ));
    assert_eq!(claimed_elsewhere(error), PathBuf::from("deck"));
}

#[test]
fn concurrent_opens_of_different_folders_let_exactly_one_claim_the_checkout() {
    for _ in 0..20 {
        let (temp, _repo) = code_repo();
        let folders = ["alpha", "beta", "gamma", "delta"];
        for folder in folders {
            fs::create_dir_all(temp.path().join(folder)).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(folders.len()));
        let handles: Vec<_> = folders
            .iter()
            .map(|folder| {
                let path = temp.path().join(folder);
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Workspace::open_with_policy(&path, cutready_policy())
                        .map(|workspace| workspace.scope().to_path_buf())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        for result in &results {
            if let Err(error) = result {
                assert!(
                    matches!(error, DraftlineError::FolderIsElsewhere(p) if p == winners[0]),
                    "{error:?}"
                );
            }
        }
    }
}

fn fixes(probe: &FolderProbe) -> Vec<FolderFix> {
    probe.suggestions.iter().map(|s| s.fix.clone()).collect()
}

fn tauri_code(error: DraftlineError) -> String {
    draftline::tauri_contract::TauriCommandError::from(error).code
}

#[test]
fn probing_reports_each_folder_state_with_fixes() {
    let outside = tempfile::tempdir().unwrap();
    let probe = Workspace::probe(outside.path().join("deck")).unwrap();
    assert_eq!(probe.state, FolderState::NotARepository);
    assert_eq!(fixes(&probe), [FolderFix::ManageHere]);

    let (temp, _repo) = code_repo();
    let cutready = temp.path().join("cutready");
    let probe = Workspace::probe(&cutready).unwrap();
    assert_eq!(probe.state, FolderState::Unmanaged);
    assert_eq!(probe.repository.as_deref(), Some(temp.path()));
    assert_eq!(fixes(&probe), [FolderFix::ManageHere]);
    assert!(
        !temp.path().join(".draftline").exists(),
        "probing changes nothing"
    );

    scoped(&temp);
    let probe = Workspace::probe(&cutready).unwrap();
    assert_eq!(probe.state, FolderState::Ready);
    assert_eq!(probe.folder.as_deref(), Some(Path::new("cutready")));
    assert_eq!(fixes(&probe), [FolderFix::Open]);

    let probe = Workspace::probe(temp.path().join("deck")).unwrap();
    assert_eq!(probe.state, FolderState::ManagedElsewhere);
    assert_eq!(
        fixes(&probe),
        [
            FolderFix::OpenManagedFolder {
                folder: "cutready".into()
            },
            FolderFix::MoveFolderHere {
                from: "cutready".into()
            },
        ]
    );
    // The root can't take the folder while the repository holds code.
    let probe = Workspace::probe(temp.path()).unwrap();
    assert_eq!(
        fixes(&probe),
        [FolderFix::OpenManagedFolder {
            folder: "cutready".into()
        }]
    );

    let marker = temp.path().join(".draftline");
    fs::write(&marker, "not json").unwrap();
    let probe = Workspace::probe(&cutready).unwrap();
    assert_eq!(probe.state, FolderState::InvalidMarker);
    assert_eq!(fixes(&probe), [FolderFix::ManageHere]);
    // A folder named .draftline isn't overwritten; someone has to move it.
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    let probe = Workspace::probe(&cutready).unwrap();
    assert_eq!(probe.state, FolderState::InvalidMarker);
    assert!(fixes(&probe).is_empty());
    assert!(
        Workspace::apply_folder_fix(&cutready, cutready_policy(), &FolderFix::ManageHere).is_err()
    );
    fs::remove_dir(&marker).unwrap();
    fs::write(&marker, r#"{"format":2,"folder":"cutready"}"#).unwrap();
    let probe = Workspace::probe(&cutready).unwrap();
    assert_eq!(probe.state, FolderState::NewerFormat);
    assert_eq!(fixes(&probe), [FolderFix::UpgradeDraftline]);

    // Suggestions serialize flat, the way hosts pass them back.
    let json = serde_json::to_value(&probe.suggestions[0]).unwrap();
    assert_eq!(json["action"], "upgrade_draftline");
    assert!(json["label"].is_string());
}

#[test]
fn an_invalid_marker_is_fixed_by_managing_the_folder_again() {
    let (temp, _repo) = code_repo();
    scoped(&temp);
    fs::write(temp.path().join(".draftline"), "not json").unwrap();
    let cutready = temp.path().join("cutready");
    let workspace =
        Workspace::apply_folder_fix(&cutready, cutready_policy(), &FolderFix::ManageHere).unwrap();
    assert_eq!(workspace.scope(), Path::new("cutready"));
    assert_eq!(
        Workspace::probe(&cutready).unwrap().state,
        FolderState::Ready
    );
}

#[test]
fn managing_a_path_that_is_not_a_plain_folder_changes_nothing() {
    let (temp, _repo) = code_repo();
    scoped(&temp);
    let marker = temp.path().join(".draftline");
    fs::write(&marker, "not json").unwrap();
    write_file(temp.path(), "notes.txt", "notes\n");
    let notes = temp.path().join("notes.txt");
    assert_eq!(
        Workspace::probe(&notes).unwrap().state,
        FolderState::InvalidMarker
    );
    assert!(
        Workspace::apply_folder_fix(&notes, cutready_policy(), &FolderFix::ManageHere).is_err()
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "not json");
    assert_eq!(read(temp.path(), "notes.txt"), "notes\n");

    // Nor in a repository nobody manages yet.
    let (fresh, _repo) = code_repo();
    write_file(fresh.path(), "notes.txt", "notes\n");
    let notes = fresh.path().join("notes.txt");
    assert!(
        Workspace::apply_folder_fix(&notes, cutready_policy(), &FolderFix::ManageHere).is_err()
    );
    assert!(Workspace::open_with_policy(&notes, cutready_policy()).is_err());
    assert!(!fresh.path().join(".draftline").exists());
    assert_eq!(
        Workspace::probe(fresh.path().join("cutready"))
            .unwrap()
            .state,
        FolderState::Unmanaged
    );
}

#[cfg(windows)]
#[test]
fn neither_init_nor_a_move_goes_through_a_junction() {
    let (temp, repo) = code_repo();
    let outside = tempfile::tempdir().unwrap();
    let link = temp.path().join("link");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    assert!(Workspace::init_with_policy(link.join("deck"), cutready_policy()).is_err());
    assert!(!outside.path().join("deck").exists());
    assert!(!temp.path().join(".draftline").exists());

    let mut pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    let before = repo.head().unwrap().target().unwrap();
    for to in ["link", "link/deck"] {
        let error = pitch.move_folder(to).unwrap_err();
        assert!(
            error.to_string().contains("through a link"),
            "{to}: {error}"
        );
    }
    assert_eq!(repo.head().unwrap().target().unwrap(), before);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn managing_a_folder_waits_for_an_unfinished_operation() {
    let (temp, repo) = code_repo();
    fs::create_dir_all(repo.path().join("draftline")).unwrap();
    let ledger = serde_json::json!({
        "operation_id": "interrupted",
        "operation": "SwitchVariation",
        "original_variation": "main",
        "target": "main",
        "completed": false,
        "project": "deck",
    });
    fs::write(
        repo.path().join("draftline").join("recovery.json"),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();
    let error = refused(Workspace::apply_folder_fix(
        temp.path().join("cutready"),
        cutready_policy(),
        &FolderFix::ManageHere,
    ));
    assert!(
        matches!(error, DraftlineError::RecoveryInOtherFolder(_)),
        "{error:?}"
    );
    assert!(!temp.path().join(".draftline").exists());
    assert!(!repo
        .path()
        .join("draftline")
        .join("workspace.json")
        .exists());
}

#[test]
fn a_fix_the_probe_does_not_offer_is_refused() {
    let (temp, _repo) = code_repo();
    scoped(&temp);
    let error = refused(Workspace::apply_folder_fix(
        temp.path().join("cutready"),
        cutready_policy(),
        &FolderFix::SwitchToBranch {
            branch: "main".into(),
        },
    ));
    assert!(
        matches!(error, DraftlineError::FolderFixUnavailable(_)),
        "{error:?}"
    );
    assert_eq!(tauri_code(error), "folder_fix_unavailable");
}

#[test]
fn a_deleted_folder_can_be_restored_from_its_last_save() {
    let (temp, _repo) = code_repo();
    let pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();
    drop(pitch);
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();

    let deck = temp.path().join("deck");
    let probe = Workspace::probe(&deck).unwrap();
    assert_eq!(probe.state, FolderState::FolderMissing);
    assert_eq!(probe.folder.as_deref(), Some(Path::new("cutready")));
    assert_eq!(
        fixes(&probe)[0],
        FolderFix::RestoreFolder {
            folder: "cutready".into()
        }
    );
    let restored = Workspace::apply_folder_fix(
        &deck,
        cutready_policy(),
        &FolderFix::RestoreFolder {
            folder: "cutready".into(),
        },
    )
    .unwrap();
    assert_eq!(restored.scope(), Path::new("cutready"));
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "intro\n");
    assert!(!deck.exists());
}

#[test]
fn a_branch_without_the_folder_offers_branches_that_have_it_or_a_new_folder() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();
    drop(pitch);

    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/old", &head, false).unwrap();
    repo.set_head("refs/heads/feature/old").unwrap();
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_dir(Path::new("cutready"), 0).unwrap();
    index.write().unwrap();
    commit_all(&repo, "Dev: drop the deck");

    let deck = temp.path().join("deck");
    let probe = Workspace::probe(&deck).unwrap();
    assert_eq!(probe.state, FolderState::FolderMissing);
    assert_eq!(
        fixes(&probe),
        [
            FolderFix::SwitchToBranch {
                branch: "main".into()
            },
            FolderFix::ManageHere,
        ]
    );

    let back = Workspace::apply_folder_fix(
        &deck,
        cutready_policy(),
        &FolderFix::SwitchToBranch {
            branch: "main".into(),
        },
    )
    .unwrap();
    assert_eq!(back.current_variation().unwrap(), "main");
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "intro\n");
    drop(back);

    // On the branch without it, the business can start over in `deck`.
    repo.set_head("refs/heads/feature/old").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let managed =
        Workspace::apply_folder_fix(&deck, cutready_policy(), &FolderFix::ManageHere).unwrap();
    assert_eq!(managed.scope(), Path::new("deck"));
    let marker = fs::read_to_string(temp.path().join(".draftline")).unwrap();
    assert!(marker.contains("\"deck\""), "{marker}");
}

#[test]
fn moving_the_folder_is_one_save_that_rewrites_the_marker() {
    let (temp, repo) = code_repo();
    let mut pitch = scoped(&temp);
    let stale = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();
    write_file(temp.path(), "cutready/cache/thumb.bin", "cache\n");
    git2::Repository::init(temp.path().join("cutready/vendor")).unwrap();
    write_file(temp.path(), "cutready/vendor/lib.txt", "lib\n");

    let moved = pitch.move_folder("demos/pitch").unwrap();
    assert_eq!(moved.from, Path::new("cutready"));
    assert_eq!(moved.to, Path::new("demos/pitch"));
    assert!(moved.left_behind.is_empty());
    assert_eq!(pitch.scope(), Path::new("demos/pitch"));
    assert_eq!(
        moved.version.label,
        "Move Draftline folder from cutready to demos/pitch"
    );

    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_path(Path::new("cutready")).is_err());
    assert!(tree.get_path(Path::new("demos/pitch/intro.sk")).is_ok());
    assert!(tree.get_path(Path::new("src/main.rs")).is_ok());
    let marker = repo.find_blob(head_marker(&repo)).unwrap();
    assert!(String::from_utf8_lossy(marker.content()).contains("\"demos/pitch\""));
    assert!(!temp.path().join("cutready").exists());
    assert_eq!(read(temp.path(), "demos/pitch/cache/thumb.bin"), "cache\n");
    assert_eq!(read(temp.path(), "demos/pitch/vendor/lib.txt"), "lib\n");
    assert!(temp.path().join("demos/pitch/vendor/.git").is_dir());

    // Other handles on the old folder stop; the moved one keeps working.
    let error = stale.save_version("Stale").unwrap_err();
    assert_eq!(claimed_elsewhere(error), PathBuf::from("demos/pitch"));
    write_file(temp.path(), "demos/pitch/intro.sk", "moved\n");
    pitch.save_version("After").unwrap();
    let probe = Workspace::probe(temp.path().join("cutready")).unwrap();
    assert_eq!(probe.state, FolderState::ManagedElsewhere);
}

#[test]
fn a_folder_removed_with_git_rm_can_be_restored() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    drop(pitch);
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_dir(Path::new("cutready"), 0).unwrap();
    index.write().unwrap();

    let fix = FolderFix::RestoreFolder {
        folder: "cutready".into(),
    };
    assert!(fixes(&Workspace::probe(temp.path().join("cutready")).unwrap()).contains(&fix));
    Workspace::apply_folder_fix(temp.path().join("cutready"), cutready_policy(), &fix).unwrap();
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "scene one\n");
    assert_eq!(status(&repo, "cutready/intro.sk"), git2::Status::CURRENT);
}

#[test]
fn restoring_refuses_once_the_folder_comes_back_in_part() {
    let (temp, _repo) = code_repo();
    write_file(temp.path(), "cutready/outro.sk", "scene two\n");
    let pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    drop(pitch);
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    let fix = FolderFix::RestoreFolder {
        folder: "cutready".into(),
    };
    assert!(fixes(&Workspace::probe(temp.path().join("cutready")).unwrap()).contains(&fix));

    // Someone puts one file back, edited, before the fix runs: the state
    // changed, so the fix is refused and the host re-probes. (If it races
    // past that check, the safe checkout under the lock restores the rest.)
    write_file(temp.path(), "cutready/intro.sk", "edited meanwhile\n");
    let error = Workspace::apply_folder_fix(temp.path().join("cutready"), cutready_policy(), &fix)
        .err()
        .unwrap();
    assert!(
        matches!(error, DraftlineError::FolderFixUnavailable(_)),
        "{error:?}"
    );
    assert_eq!(read(temp.path(), "cutready/intro.sk"), "edited meanwhile\n");
}

#[test]
fn a_namespaced_probe_offers_only_branches_the_host_may_switch_to() {
    let (temp, repo) = code_repo();
    let pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    drop(pitch);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/x", &head, false).unwrap();
    repo.branch("cutready/pitch", &head, false).unwrap();
    repo.branch("feature/old", &head, false).unwrap();
    repo.set_head("refs/heads/feature/old").unwrap();
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_dir(Path::new("cutready"), 0).unwrap();
    index.write().unwrap();
    commit_all(&repo, "Dev: drop the deck");

    let deck = temp.path().join("cutready");
    let branches = |probe: &draftline::FolderProbe| -> Vec<String> {
        fixes(probe)
            .into_iter()
            .filter_map(|fix| match fix {
                FolderFix::SwitchToBranch { branch } => Some(branch),
                _ => None,
            })
            .collect()
    };
    let mut open = branches(&Workspace::probe(&deck).unwrap());
    open.sort();
    assert_eq!(open, ["cutready/pitch", "feature/x", "main"]);
    let mut namespaced = branches(&Workspace::probe_with_namespace(&deck, "cutready/").unwrap());
    namespaced.sort();
    assert_eq!(namespaced, ["cutready/pitch", "main"]);

    let refused = Workspace::apply_folder_fix_with_namespace(
        &deck,
        cutready_policy(),
        "cutready/",
        &FolderFix::SwitchToBranch {
            branch: "feature/x".into(),
        },
    )
    .err()
    .unwrap();
    assert!(
        matches!(refused, DraftlineError::FolderFixUnavailable(_)),
        "{refused:?}"
    );
    let back = Workspace::apply_folder_fix_with_namespace(
        &deck,
        cutready_policy(),
        "cutready/",
        &FolderFix::SwitchToBranch {
            branch: "cutready/pitch".into(),
        },
    )
    .unwrap();
    assert_eq!(back.current_variation().unwrap(), "cutready/pitch");
    assert_eq!(back.variation_namespace(), Some("cutready/"));
}

#[test]
fn switching_to_a_branch_with_the_folder_refuses_untracked_files_in_the_way() {
    let (temp, repo) = code_repo();
    write_file(temp.path(), "src/extra.rs", "pub fn extra() {}\n");
    commit_all(&repo, "Dev: extra");
    let pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    drop(pitch);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature/old", &head, false).unwrap();
    repo.set_head("refs/heads/feature/old").unwrap();
    fs::remove_dir_all(temp.path().join("cutready")).unwrap();
    fs::remove_file(temp.path().join("src/extra.rs")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_dir(Path::new("cutready"), 0).unwrap();
    index.remove_path(Path::new("src/extra.rs")).unwrap();
    index.write().unwrap();
    commit_all(&repo, "Dev: drop the deck");
    write_file(temp.path(), "src/extra.rs", "untracked work\n");

    let error = Workspace::apply_folder_fix(
        temp.path().join("cutready"),
        cutready_policy(),
        &FolderFix::SwitchToBranch {
            branch: "main".into(),
        },
    )
    .err()
    .unwrap();
    assert!(
        matches!(error, DraftlineError::PreflightFailed(_)),
        "{error:?}"
    );
    assert_eq!(read(temp.path(), "src/extra.rs"), "untracked work\n");
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature/old");
}

#[test]
fn a_move_refuses_staged_files_the_folder_skips() {
    let (temp, repo) = code_repo();
    let mut pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    // The cutready policy skips cache/, but Git has it staged.
    write_file(temp.path(), "cutready/cache/thumb.bin", "cache\n");
    let mut index = repo.index().unwrap();
    index
        .add_path(Path::new("cutready/cache/thumb.bin"))
        .unwrap();
    index.write().unwrap();

    let probe = Workspace::probe(temp.path().join("demos")).unwrap();
    assert!(!fixes(&probe).contains(&FolderFix::MoveFolderHere {
        from: "cutready".into()
    }));
    let error = pitch.move_folder(temp.path().join("demos")).err().unwrap();
    assert!(
        matches!(error, DraftlineError::FolderMoveRefused(_)),
        "{error:?}"
    );
    assert_eq!(read(temp.path(), "cutready/cache/thumb.bin"), "cache\n");
    assert_eq!(
        status(&repo, "cutready/cache/thumb.bin"),
        git2::Status::INDEX_NEW
    );
}

#[test]
fn a_folder_can_move_into_its_own_subfolder() {
    let (temp, repo) = code_repo();
    let mut pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    pitch
        .move_folder(temp.path().join("cutready").join("pitch"))
        .unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_path(Path::new("cutready/pitch/intro.sk")).is_ok());
    assert!(tree.get_path(Path::new("cutready/intro.sk")).is_err());
    assert_eq!(read(temp.path(), "cutready/pitch/intro.sk"), "scene one\n");
}

#[test]
fn the_root_folder_moves_into_a_subfolder_and_back() {
    let (temp, repo) = code_repo();
    write_file(temp.path(), ".gitignore", "*.tmp\n");
    write_file(temp.path(), ".github/CODEOWNERS", "* @team\n");
    commit_all(&repo, "Repository files");
    let mut root = Workspace::open_with_policy(temp.path(), cutready_policy()).unwrap();
    root.save_version("Marker").unwrap();
    write_file(temp.path(), "scratch.tmp", "mine\n");

    let moved = root.move_folder("business").unwrap();
    assert_eq!(moved.from, PathBuf::new());
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let names: Vec<_> = tree.iter().map(|e| e.name().unwrap().to_string()).collect();
    assert_eq!(
        names,
        [".draftline", ".github", ".gitignore", "business"],
        "repository files stay at the root"
    );
    assert_eq!(read(temp.path(), "business/src/main.rs"), "fn main() {}\n");
    assert_eq!(read(temp.path(), ".gitignore"), "*.tmp\n");
    assert!(!temp.path().join("business/.gitignore").exists());
    assert_eq!(
        read(temp.path(), "scratch.tmp"),
        "mine\n",
        "root leftovers stay"
    );
    // A version from before the move reads as the folder's content alone.
    let before = root.versions().unwrap().into_iter().nth(1).unwrap();
    let diff = root.diff_version_to_workspace(before.id()).unwrap();
    assert!(diff.files.is_empty(), "{:?}", diff.files);

    // Back to the root: allowed, because the repository holds only its own files.
    root.move_folder("").unwrap();
    assert_eq!(root.scope(), Path::new(""));
    assert_eq!(read(temp.path(), "src/main.rs"), "fn main() {}\n");
    assert_eq!(read(temp.path(), ".gitignore"), "*.tmp\n");
    assert!(!temp.path().join("business").exists());
}

#[test]
fn history_and_restore_follow_the_folder_through_a_move() {
    let (temp, repo) = code_repo();
    let mut pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    let intro = pitch.save_version("Intro").unwrap();
    pitch.move_folder("demos/pitch").unwrap();
    write_file(temp.path(), "demos/pitch/intro.sk", "after\n");
    let after = pitch.save_version("After").unwrap();
    write_file(temp.path(), "src/lib.rs", "pub fn x() {}\n");
    commit_all(&repo, "Developer change");

    let labels: Vec<_> = pitch
        .versions()
        .unwrap()
        .into_iter()
        .map(|v| v.label)
        .collect();
    assert_eq!(
        labels[..3],
        [
            "After",
            "Move Draftline folder from cutready to demos/pitch",
            "Intro"
        ],
        "{labels:?}"
    );
    let history = pitch.history().unwrap();
    let touches = |label: &str| {
        history
            .iter()
            .find(|entry| entry.version.label == label)
            .unwrap()
            .touches_folder
    };
    assert!(touches("Intro") && touches("After"));
    assert!(!touches("Developer change"));

    // A version from before the move reads as the folder's own.
    let diff = pitch.diff_versions(intro.id(), after.id()).unwrap();
    let paths: Vec<_> = diff.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(paths, [PathBuf::from("demos/pitch/intro.sk")]);
    let preview = pitch.preview_version(intro.id()).unwrap();
    assert!(preview
        .files
        .iter()
        .all(|file| !file.path.starts_with("cutready")));
    let file = pitch
        .preview_version_file(intro.id(), "demos/pitch/intro.sk")
        .unwrap()
        .unwrap();
    assert_eq!(file.content.as_deref(), Some("intro\n"));
    let to_workspace = pitch.diff_version_to_workspace(intro.id()).unwrap();
    assert_eq!(to_workspace.files.len(), 1);

    pitch
        .restore_version_as_new_save(intro.id(), "Back")
        .unwrap();
    assert_eq!(read(temp.path(), "demos/pitch/intro.sk"), "intro\n");
    assert!(!temp.path().join("cutready").exists());
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_path(Path::new("cutready")).is_err());
    assert!(tree.get_path(Path::new("src/lib.rs")).is_ok());
    let marker = repo.find_blob(head_marker(&repo)).unwrap();
    assert!(String::from_utf8_lossy(marker.content()).contains("\"demos/pitch\""));
}

#[test]
fn a_move_before_the_first_save_keeps_older_versions() {
    let (temp, repo) = code_repo();
    let initial = repo.head().unwrap().peel_to_commit().unwrap().id();
    let initial = draftline::VersionId::from(initial);
    let mut pitch = scoped(&temp);
    pitch.move_folder("demos/pitch").unwrap();

    let labels: Vec<_> = pitch
        .versions()
        .unwrap()
        .into_iter()
        .map(|v| v.label)
        .collect();
    assert_eq!(
        labels,
        [
            "Move Draftline folder from cutready to demos/pitch",
            "Initial app"
        ]
    );
    let file = pitch
        .preview_version_file(&initial, "demos/pitch/intro.sk")
        .unwrap()
        .unwrap();
    assert_eq!(file.content.as_deref(), Some("scene one\n"));
    write_file(temp.path(), "demos/pitch/intro.sk", "changed\n");
    pitch.save_version("Changed").unwrap();
    pitch.restore_version_as_new_save(&initial, "Back").unwrap();
    assert_eq!(read(temp.path(), "demos/pitch/intro.sk"), "scene one\n");
}

#[test]
fn versions_from_before_the_folder_existed_read_as_empty_after_a_move() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(temp.path(), &options).unwrap();
    write_file(temp.path(), "src/main.rs", "fn main() {}\n");
    let before = draftline::VersionId::from(commit_all(&repo, "Code only"));
    let mut pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    pitch.save_version("Intro").unwrap();
    pitch.move_folder("demos/pitch").unwrap();

    assert!(pitch.preview_version(&before).unwrap().files.is_empty());
    let diff = pitch.diff_version_to_workspace(&before).unwrap();
    let paths: Vec<_> = diff.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(paths, [PathBuf::from("demos/pitch/intro.sk")]);
}

#[test]
fn a_root_folder_reads_versions_from_before_its_marker_after_moving() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(temp.path(), &options).unwrap();
    write_file(temp.path(), "notes.md", "first\n");
    let before = draftline::VersionId::from(commit_all(&repo, "First"));
    let mut root = Workspace::open_with_policy(temp.path(), cutready_policy()).unwrap();
    write_file(temp.path(), "notes.md", "second\n");
    root.save_version("Second").unwrap();
    root.move_folder("business").unwrap();

    let file = root
        .preview_version_file(&before, "business/notes.md")
        .unwrap()
        .unwrap();
    assert_eq!(file.content.as_deref(), Some("first\n"));
}

#[test]
fn restoring_onto_a_variation_from_before_a_move_is_refused() {
    let (temp, repo) = code_repo();
    let mut pitch = scoped(&temp);
    write_file(temp.path(), "cutready/intro.sk", "intro\n");
    let intro = pitch.save_version("Intro").unwrap();
    let old = pitch.create_variation("cutready/old").unwrap();
    pitch.move_folder("demos/pitch").unwrap();
    let tip = repo
        .find_reference("refs/heads/cutready/old")
        .unwrap()
        .target();

    let err = pitch
        .restore_version_as_new_save_to_variation(
            intro.id(),
            "Back",
            draftline::RestoreVersionTarget::Existing {
                variation: old.id().clone(),
            },
        )
        .unwrap_err();
    assert!(
        matches!(&err, DraftlineError::FolderIsElsewhere(folder) if folder == Path::new("cutready")),
        "{err:?}"
    );
    assert_eq!(
        repo.find_reference("refs/heads/cutready/old")
            .unwrap()
            .target(),
        tip
    );
}

#[test]
fn a_root_folder_keeps_its_history_after_moving_into_a_subfolder() {
    let (temp, repo) = code_repo();
    let mut root = Workspace::open_with_policy(temp.path(), cutready_policy()).unwrap();
    write_file(temp.path(), "notes.md", "first\n");
    let first = root.save_version("First").unwrap();
    root.move_folder("business").unwrap();
    write_file(temp.path(), "business/notes.md", "second\n");
    root.save_version("Second").unwrap();

    let labels: Vec<_> = root
        .versions()
        .unwrap()
        .into_iter()
        .map(|v| v.label)
        .collect();
    assert!(labels.contains(&"First".to_string()), "{labels:?}");

    root.restore_version_as_new_save(first.id(), "Back")
        .unwrap();
    assert_eq!(read(temp.path(), "business/notes.md"), "first\n");
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let names: Vec<_> = tree.iter().map(|e| e.name().unwrap().to_string()).collect();
    assert_eq!(names, [".draftline", "business"]);
}

#[test]
fn moves_that_would_mix_with_other_files_are_refused() {
    let (temp, repo) = code_repo();
    let mut pitch = scoped(&temp);
    pitch.save_version("Marker").unwrap();
    let before = repo.head().unwrap().target().unwrap();
    write_file(temp.path(), "notes/todo.txt", "todo\n");
    for (to, reason) in [
        ("", "holds other files"),
        ("src", "already has saved files"),
        ("notes", "isn't empty"),
        ("README.md", "already has saved files"),
        (".git/x", "reserved"),
        ("cutready", "already manages"),
    ] {
        let error = pitch.move_folder(to).unwrap_err();
        assert!(error.to_string().contains(reason), "{to}: {error}");
        assert_eq!(tauri_code(error), "folder_move_refused", "{to}");
    }

    write_file(temp.path(), "cutready/intro.sk", "unsaved\n");
    let probe = Workspace::probe(temp.path().join("deck")).unwrap();
    assert_eq!(
        fixes(&probe),
        [FolderFix::OpenManagedFolder {
            folder: "cutready".into()
        }],
        "a move unsaved edits would block isn't offered"
    );
    let error = pitch.move_folder("deck").unwrap_err();
    assert!(
        matches!(error, DraftlineError::PreflightFailed(_)),
        "{error:?}"
    );
    assert_eq!(repo.head().unwrap().target().unwrap(), before);
    assert!(!temp.path().join("deck").exists());
    assert_eq!(pitch.scope(), Path::new("cutready"));
}

#[test]
fn a_folder_can_be_moved_where_the_host_opened_instead() {
    let (temp, repo) = code_repo();
    scoped(&temp).save_version("Marker").unwrap();
    let deck = temp.path().join("deck");
    let workspace = Workspace::apply_folder_fix(
        &deck,
        cutready_policy(),
        &FolderFix::MoveFolderHere {
            from: "cutready".into(),
        },
    )
    .unwrap();
    assert_eq!(workspace.scope(), Path::new("deck"));
    assert_eq!(read(temp.path(), "deck/intro.sk"), "scene one\n");
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_path(Path::new("deck/intro.sk")).is_ok());
}
