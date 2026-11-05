use std::path::{Path, PathBuf};

use draftline::tauri_contract::{inspect_repository, InspectRepositoryRequest};
use draftline::{
    ContentPolicy, InspectRepositoryOptions, ProjectSettingsUpdate, RemoteOptions,
    RepositoryLocationKind, Workspace,
};

fn commit_files(repo: &git2::Repository, branch: &str, files: &[(&str, &str)], message: &str) {
    let root = repo.workdir().unwrap();
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let mut index = repo.index().unwrap();
    for (path, _) in files {
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Dev", "dev@example.test").unwrap();
    let parent = repo
        .find_reference(&format!("refs/heads/{branch}"))
        .ok()
        .and_then(|reference| reference.peel_to_commit().ok())
        .or_else(|| repo.head().ok().and_then(|head| head.peel_to_commit().ok()));
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )
    .unwrap();
}

fn code_repo() -> (tempfile::TempDir, git2::Repository) {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(temp.path(), &options).unwrap();
    commit_files(&repo, "main", &[("src/main.rs", "fn main() {}\n")], "Code");
    (temp, repo)
}

fn file_url(path: &Path) -> String {
    let path = path.to_str().unwrap().replace('\\', "/");
    let slash = if path.starts_with('/') { "" } else { "/" };
    format!("file://{slash}{path}")
}

fn inspect(location: &str, target: Option<&str>) -> draftline::RepositoryInspection {
    let mut options = InspectRepositoryOptions::new();
    if let Some(target) = target {
        options = options.with_target_folder(target);
    }
    Workspace::inspect_repository(location, &options, &mut RemoteOptions::new()).unwrap()
}

fn blocker(location: &str, target: &str) -> Option<String> {
    inspect(location, Some(target))
        .target_folder
        .unwrap()
        .blocker
}

#[test]
fn a_plain_folder_is_not_a_repository() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("notes.txt"), "hi").unwrap();
    let inspection = inspect(temp.path().to_str().unwrap(), Some("."));
    assert_eq!(inspection.location_kind, RepositoryLocationKind::Local);
    assert!(!inspection.is_repository);
    assert_eq!(inspection.root, None);
    assert_eq!(inspection.project, None);
    assert_eq!(inspection.lfs.used, None);
    let target = inspection.target_folder.unwrap();
    assert!(target.exists && target.is_directory && !target.empty);
    assert_eq!(target.blocker.as_deref(), Some("folder_not_empty"));

    let missing = temp.path().join("nowhere");
    let inspection = inspect(missing.to_str().unwrap(), None);
    assert!(!inspection.is_repository);
}

#[test]
fn a_code_repository_reports_branches_remotes_and_a_free_target() {
    let (temp, repo) = code_repo();
    commit_files(&repo, "feature", &[("src/lib.rs", "")], "Feature");
    repo.remote(
        "origin",
        "https://someone:secret@github.com/contoso/app.git",
    )
    .unwrap();
    let head = repo.head().unwrap().target().unwrap();
    repo.reference("refs/remotes/origin/trunk", head, false, "test")
        .unwrap();
    repo.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
        false,
        "test",
    )
    .unwrap();

    // Any folder in the work tree finds the repository.
    let inside = temp.path().join("src");
    let inspection = inspect(inside.to_str().unwrap(), Some("docs/demo"));
    assert!(inspection.is_repository);
    let root = inspection.root.clone().unwrap();
    assert!(same_dir(&root, temp.path()), "{root:?}");
    assert_eq!(inspection.project, None);
    assert_eq!(inspection.current_branch.as_deref(), Some("main"));
    assert_eq!(inspection.default_branch.as_deref(), Some("trunk"));
    assert_eq!(inspection.branches, vec!["feature", "main"]);
    assert_eq!(inspection.remotes.len(), 1);
    assert_eq!(inspection.remotes[0].name, "origin");
    assert_eq!(
        inspection.remotes[0].url,
        "https://github.com/contoso/app.git"
    );
    assert_eq!(inspection.lfs.used, Some(false));
    let target = inspection.target_folder.unwrap();
    assert_eq!(target.folder, Some(PathBuf::from("docs").join("demo")));
    assert!(!target.exists);
    assert_eq!(target.blocker, None);

    let location = temp.path().to_str().unwrap();
    assert_eq!(
        blocker(location, "src").as_deref(),
        Some("folder_not_empty")
    );
    assert_eq!(
        blocker(location, ".git/demo").as_deref(),
        Some("path_escapes_workspace")
    );
    let outside = tempfile::tempdir().unwrap();
    assert_eq!(
        blocker(location, outside.path().to_str().unwrap()).as_deref(),
        Some("path_escapes_workspace")
    );
}

#[test]
fn an_unborn_repository_reports_its_branch() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("trunk");
    git2::Repository::init_opts(temp.path(), &options).unwrap();
    let inspection = inspect(temp.path().to_str().unwrap(), None);
    assert!(inspection.is_repository);
    assert_eq!(inspection.current_branch.as_deref(), Some("trunk"));
    assert!(inspection.branches.is_empty());
    assert_eq!(inspection.lfs.used, Some(false));
}

#[test]
fn lfs_use_is_read_from_committed_and_root_attributes() {
    let (temp, repo) = code_repo();
    commit_files(
        &repo,
        "main",
        &[(
            "media/.gitattributes",
            "*.mp4 filter=lfs diff=lfs merge=lfs -text\n",
        )],
        "Track video",
    );
    assert_eq!(
        inspect(temp.path().to_str().unwrap(), None).lfs.used,
        Some(true)
    );

    let (temp, _repo) = code_repo();
    std::fs::write(
        temp.path().join(".gitattributes"),
        "# *.png filter=lfs\n*.txt text\n",
    )
    .unwrap();
    assert_eq!(
        inspect(temp.path().to_str().unwrap(), None).lfs.used,
        Some(false)
    );
    std::fs::write(
        temp.path().join(".gitattributes"),
        "*.png filter=lfs -text\n",
    )
    .unwrap();
    assert_eq!(
        inspect(temp.path().to_str().unwrap(), None).lfs.used,
        Some(true)
    );
}

#[test]
fn a_repository_with_a_project_reports_its_marker_and_other_folders_are_elsewhere() {
    let (temp, _repo) = code_repo();
    let workspace = Workspace::init_project_in_repository(
        temp.path().join("docs"),
        "Launch Demo",
        ProjectSettingsUpdate {
            base_branch: Some("main".to_string()),
            commit_template: Some("docs(demo): {subject}".to_string()),
        },
        ContentPolicy::default(),
    )
    .unwrap();
    let id = workspace.project_settings().unwrap().id;
    assert!(id.is_some());

    let location = temp.path().to_str().unwrap();
    let inspection = inspect(location, None);
    let project = inspection.project.unwrap();
    assert_eq!(project.folder, PathBuf::from("docs/launch-demo"));
    assert_eq!(project.id, id);
    assert_eq!(project.base_branch.as_deref(), Some("main"));
    assert_eq!(
        project.commit_template.as_deref(),
        Some("docs(demo): {subject}")
    );
    assert!(project.in_marker);

    assert_eq!(
        blocker(location, "docs/other").as_deref(),
        Some("folder_is_elsewhere")
    );
    assert_eq!(blocker(location, "docs/launch-demo"), None);
}

#[test]
fn a_remote_reports_its_default_branch_and_branches() {
    let (source, repo) = code_repo();
    commit_files(&repo, "cutready/review", &[("notes.md", "hi\n")], "Notes");
    let url = file_url(source.path());
    let destination = tempfile::tempdir().unwrap();
    let target = destination.path().join("clone");

    let inspection = inspect(&url, Some(target.to_str().unwrap()));
    assert_eq!(inspection.location_kind, RepositoryLocationKind::Remote);
    assert!(inspection.is_repository);
    assert_eq!(inspection.root, None);
    assert_eq!(inspection.project, None);
    assert_eq!(inspection.current_branch, None);
    assert_eq!(inspection.default_branch.as_deref(), Some("main"));
    assert_eq!(inspection.branches, vec!["cutready/review", "main"]);
    assert_eq!(inspection.lfs.used, None);
    let target = inspection.target_folder.unwrap();
    assert!(!target.exists);
    assert_eq!(target.folder, None);
    assert_eq!(target.blocker, None);
}

#[test]
fn an_unreachable_remote_fails_and_the_contract_hides_its_login() {
    let missing = tempfile::tempdir().unwrap().path().join("gone");
    let error = Workspace::inspect_repository(
        &file_url(&missing),
        &InspectRepositoryOptions::new(),
        &mut RemoteOptions::new(),
    )
    .unwrap_err();
    assert!(!error.to_string().is_empty());

    let request: InspectRepositoryRequest = serde_json::from_value(serde_json::json!({
        "location": "https://someone:secret@example.invalid/contoso/app.git",
        "timeout_ms": 5000,
    }))
    .unwrap();
    assert!(!format!("{request:?}").contains("secret"));
}

#[test]
fn the_contract_flattens_the_inspection() {
    let (temp, _repo) = code_repo();
    let result = inspect_repository(InspectRepositoryRequest {
        location: temp.path().to_str().unwrap().to_string(),
        target_folder: Some(PathBuf::from("demo")),
        timeout_ms: None,
        #[cfg(feature = "github")]
        github_token: None,
    })
    .unwrap();
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["location_kind"], "local");
    assert_eq!(value["is_repository"], true);
    assert_eq!(value["current_branch"], "main");
    assert_eq!(value["target_folder"]["blocker"], serde_json::Value::Null);
    assert_eq!(value["lfs"]["used"], false);
    assert!(value["lfs"]["available"].is_boolean());
}

#[test]
fn inspecting_writes_nothing() {
    let (temp, repo) = code_repo();
    let snapshot = |path: &Path| {
        let mut entries: Vec<_> = walk(path).collect();
        entries.sort();
        entries
    };
    let before = snapshot(temp.path());
    inspect(temp.path().to_str().unwrap(), Some("docs/demo"));
    assert_eq!(snapshot(temp.path()), before);
    assert!(!repo.path().join("draftline").exists());
}

fn walk(path: &Path) -> Box<dyn Iterator<Item = PathBuf>> {
    Box::new(std::fs::read_dir(path).unwrap().flat_map(|entry| {
        let path = entry.unwrap().path();
        let children = if path.is_dir() {
            walk(&path)
        } else {
            Box::new(std::iter::empty())
        };
        std::iter::once(path).chain(children)
    }))
}

#[test]
fn drive_paths_stay_local_and_scp_logins_are_hidden() {
    for location in [
        r"Q:\nowhere\draftline",
        "Q:/nowhere/draftline",
        "Q://nowhere/draftline",
    ] {
        let inspection = inspect(location, None);
        assert_eq!(
            inspection.location_kind,
            RepositoryLocationKind::Local,
            "{location}"
        );
        assert!(!inspection.is_repository);
    }

    let (temp, repo) = code_repo();
    repo.remote("origin", "git@github.com:contoso/app.git")
        .unwrap();
    repo.remote("mirror", "ghp_secret@ghe.example.com:contoso/app.git")
        .unwrap();
    let inspection = inspect(temp.path().to_str().unwrap(), None);
    let urls: Vec<_> = inspection
        .remotes
        .iter()
        .map(|remote| remote.url.as_str())
        .collect();
    assert_eq!(
        urls,
        [
            "ghe.example.com:contoso/app.git",
            "git@github.com:contoso/app.git"
        ]
    );
}

fn same_dir(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left).unwrap() == std::fs::canonicalize(right).unwrap()
}
