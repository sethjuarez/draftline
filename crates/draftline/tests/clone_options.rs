use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use draftline::tauri_contract::{
    clone_repository, clone_repository_with_context, clone_workspace_with_options_with_context,
    CloneProgressEvent, CloneRepositoryRequest, CloneWorkspaceWithOptionsRequest,
    DraftlineCommandContext,
};
use draftline::{
    CloneOptions, CloneProgress, CloneStage, ContentPolicy, DraftlineError, ProjectSettingsUpdate,
    RemoteCredential, RemoteOptions, Workspace,
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

/// A code repository whose `main` has only code, and whose
/// `cutready/review` branch adds a CutReady folder and its marker.
fn source_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = git2::Repository::init_opts(temp.path(), &options).unwrap();
    let files: Vec<(String, String)> = (0..40)
        .map(|index| (format!("src/file{index}.rs"), format!("// {index}\n")))
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect();
    commit_files(&repo, "main", &files, "Code");
    commit_files(
        &repo,
        "cutready/review",
        &[
            (".draftline", "{\"format\":1,\"folder\":\"cutready\"}\n"),
            ("cutready/intro.sk", "scene one\n"),
        ],
        "Add CutReady",
    );
    temp
}

fn url(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

fn file_url(path: &Path) -> String {
    let path = path.to_str().unwrap().replace('\\', "/");
    let slash = if path.starts_with('/') { "" } else { "/" };
    format!("file://{slash}{path}")
}

#[test]
fn a_clone_reports_progress_through_each_stage() {
    let source = source_repo();
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("clone");
    let reports = Arc::new(Mutex::new(Vec::<CloneProgress>::new()));
    let seen = Arc::clone(&reports);
    let mut options =
        RemoteOptions::new().with_progress(move |progress| seen.lock().unwrap().push(*progress));

    // A plain local path is copied, not transferred, so only a URL reports
    // download progress.
    let workspace = Workspace::clone_workspace_with(
        file_url(source.path()),
        &path,
        ContentPolicy::default(),
        &CloneOptions::new(),
        &mut options,
    )
    .unwrap();

    assert!(workspace.root().join("src/file0.rs").exists());
    let reports = reports.lock().unwrap();
    assert!(
        reports
            .iter()
            .any(|report| report.stage == CloneStage::Receiving),
        "{reports:?}"
    );
    for stage in [
        CloneStage::Receiving,
        CloneStage::Resolving,
        CloneStage::CheckingOut,
    ] {
        if let Some(report) = reports.iter().rev().find(|report| report.stage == stage) {
            assert_eq!(
                report.completed, report.total,
                "{stage:?} finishes: {reports:?}"
            );
        }
    }
    let last = reports.last().unwrap();
    assert_eq!(last.stage, CloneStage::CheckingOut, "{reports:?}");
    assert_eq!(last.completed, last.total, "the last report finishes");
    assert!(last.total >= 40);
    assert!(
        reports.len() < 40,
        "reports are throttled, not one per file: {}",
        reports.len()
    );
}

#[test]
fn a_clone_checks_out_the_branch_asked_for_and_opens_its_folder() {
    let source = source_repo();
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("clone");

    let workspace = Workspace::clone_workspace_with(
        url(source.path()),
        &path,
        ContentPolicy::default(),
        &CloneOptions::new().with_branch("cutready/review"),
        &mut RemoteOptions::new(),
    )
    .unwrap();

    assert_eq!(workspace.scope(), Path::new("cutready"));
    assert!(workspace.root().join("cutready/intro.sk").exists());
    let repo = git2::Repository::open(&path).unwrap();
    assert_eq!(
        repo.head().unwrap().shorthand().ok(),
        Some("cutready/review")
    );
    // A full clone: the default branch's history is there too.
    assert!(repo.find_reference("refs/remotes/origin/main").is_ok());
}

#[test]
fn a_clone_refuses_a_branch_it_cannot_check_out() {
    let source = source_repo();
    let target = tempfile::tempdir().unwrap();

    let path = target.path().join("missing");
    let error = Workspace::clone_workspace_with(
        url(source.path()),
        &path,
        ContentPolicy::default(),
        &CloneOptions::new().with_branch("cutready/nope"),
        &mut RemoteOptions::new(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(&error, DraftlineError::VariationNotFound(branch) if branch == "cutready/nope"),
        "{error:?}"
    );
    assert!(
        !path.exists() || std::fs::read_dir(&path).unwrap().next().is_none(),
        "a refused clone leaves nothing behind"
    );
    let path = target.path().join("invalid");
    let error = Workspace::clone_workspace_with(
        url(source.path()),
        &path,
        ContentPolicy::default(),
        &CloneOptions::new().with_branch("bad..name"),
        &mut RemoteOptions::new(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(&error, DraftlineError::InvalidVariationName(_)),
        "{error:?}"
    );
    assert!(!path.exists(), "nothing is created for an invalid name");

    // A remote that isn't there is not reported as a missing branch.
    let error = Workspace::clone_workspace_with(
        url(&target.path().join("no-such-remote")),
        target.path().join("elsewhere"),
        ContentPolicy::default(),
        &CloneOptions::new().with_branch("main"),
        &mut RemoteOptions::new(),
    )
    .err()
    .unwrap();
    assert!(
        !matches!(&error, DraftlineError::VariationNotFound(_)),
        "{error:?}"
    );
}

#[test]
fn contract_clones_a_branch_and_reports_progress_with_its_id() {
    let source = source_repo();
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("clone");
    let events = Arc::new(Mutex::new(Vec::<CloneProgressEvent>::new()));
    let seen = Arc::clone(&events);
    let mut context = DraftlineCommandContext::new()
        .with_clone_progress_sink(move |event| seen.lock().unwrap().push(event));

    let request: CloneWorkspaceWithOptionsRequest = serde_json::from_value(serde_json::json!({
        "remote_url": url(source.path()),
        "workspace_path": path,
        "branch": "cutready/review",
        "progress_id": "clone-1",
    }))
    .unwrap();
    clone_workspace_with_options_with_context(&mut context, request).unwrap();

    assert!(path.join("cutready/intro.sk").exists());
    let events = events.lock().unwrap();
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| {
        event.progress_id.as_deref() == Some("clone-1") && event.workspace_path == path
    }));
    let json = serde_json::to_value(events.last().unwrap()).unwrap();
    assert_eq!(json["stage"], "checking_out");
    assert_eq!(json["progress_id"], "clone-1");
    assert!(json["total"].as_u64().unwrap() > 0);

    // Branch and progress id are optional.
    let request: CloneWorkspaceWithOptionsRequest = serde_json::from_value(serde_json::json!({
        "remote_url": url(source.path()),
        "workspace_path": target.path().join("default"),
    }))
    .unwrap();
    assert_eq!(request.branch, None);
    let error =
        draftline::tauri_contract::into_tauri_result(clone_workspace_with_options_with_context(
            &mut DraftlineCommandContext::new(),
            CloneWorkspaceWithOptionsRequest {
                branch: Some("cutready/nope".to_string()),
                workspace_path: target.path().join("missing"),
                ..request
            },
        ))
        .unwrap_err();
    assert_eq!(error.code, "variation_not_found");
}
/// `source_repo`, with a second version of `src/file0.rs` on `main`, served
/// so that `git` may filter what it sends.
fn history_repo() -> (tempfile::TempDir, git2::Oid) {
    let source = source_repo();
    let repo = git2::Repository::open(source.path()).unwrap();
    let old = repo.revparse_single("main:src/file0.rs").unwrap().id();
    commit_files(
        &repo,
        "main",
        &[("src/file0.rs", "// 0, again\n")],
        "Change",
    );
    repo.config()
        .unwrap()
        .set_bool("uploadpack.allowFilter", true)
        .unwrap();
    (source, old)
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn a_blobless_clone_downloads_only_what_it_checks_out() {
    if !git_available() {
        eprintln!("skipped: no git");
        return;
    }
    let (source, old) = history_repo();
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("clone");
    let reports = Arc::new(Mutex::new(Vec::<CloneProgress>::new()));
    let seen = Arc::clone(&reports);
    let mut options =
        RemoteOptions::new().with_progress(move |progress| seen.lock().unwrap().push(*progress));

    let workspace = Workspace::clone_workspace_with(
        file_url(source.path()),
        &path,
        ContentPolicy::default(),
        &CloneOptions::new().with_blobless(true),
        &mut options,
    )
    .unwrap();

    assert!(workspace.is_partial_clone());
    assert_eq!(
        std::fs::read_to_string(workspace.root().join("src/file0.rs"))
            .unwrap()
            .trim_end(),
        "// 0, again"
    );
    let repo = git2::Repository::open(&path).unwrap();
    assert!(
        repo.find_blob(old).is_err(),
        "older contents stay on the remote"
    );
    assert_eq!(
        workspace.history().unwrap().len(),
        2,
        "but all of history is here"
    );
    let reports = reports.lock().unwrap();
    assert!(
        reports
            .iter()
            .any(|report| report.stage == CloneStage::Receiving),
        "{reports:?}"
    );
    let last = reports.last().unwrap();
    assert_eq!(last.stage, CloneStage::CheckingOut, "{reports:?}");
    assert_eq!(last.completed, last.total);

    // A branch, through git as well.
    let branch = Workspace::clone_workspace_with(
        file_url(source.path()),
        target.path().join("branch"),
        ContentPolicy::default(),
        &CloneOptions::new()
            .with_branch("cutready/review")
            .with_blobless(true),
        &mut RemoteOptions::new(),
    )
    .unwrap();
    assert!(branch.is_partial_clone());
    assert_eq!(branch.scope(), Path::new("cutready"));

    // A full clone is not partial.
    let full = Workspace::clone_workspace_with(
        file_url(source.path()),
        target.path().join("full"),
        ContentPolicy::default(),
        &CloneOptions::new(),
        &mut RemoteOptions::new(),
    )
    .unwrap();
    assert!(!full.is_partial_clone());
}

#[test]
fn a_blobless_clone_refuses_a_missing_branch_and_leaves_nothing() {
    if !git_available() {
        eprintln!("skipped: no git");
        return;
    }
    let (source, _) = history_repo();
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("missing");
    let error = Workspace::clone_workspace_with(
        file_url(source.path()),
        &path,
        ContentPolicy::default(),
        &CloneOptions::new()
            .with_branch("cutready/nope")
            .with_blobless(true),
        &mut RemoteOptions::new(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(&error, DraftlineError::VariationNotFound(branch) if branch == "cutready/nope"),
        "{error:?}"
    );
    assert!(!path.exists() || std::fs::read_dir(&path).unwrap().next().is_none());

    let error = Workspace::clone_workspace_with(
        file_url(&target.path().join("no-such-remote")),
        target.path().join("elsewhere"),
        ContentPolicy::default(),
        &CloneOptions::new().with_blobless(true),
        &mut RemoteOptions::new(),
    )
    .err()
    .unwrap();
    assert!(matches!(&error, DraftlineError::Git(_)), "{error:?}");
}

#[test]
fn a_blobless_clone_of_a_local_remote_needs_no_credentials() {
    if !git_available() {
        eprintln!("skipped: no git");
        return;
    }
    let (source, _) = history_repo();
    let target = tempfile::tempdir().unwrap();
    // A local remote never needs a login, so the host isn't asked for one;
    // an SSH remote or credentials git can't use fall back to libgit2.
    let asked = std::cell::Cell::new(false);
    let mut options = RemoteOptions::new().with_credentials(|_| {
        asked.set(true);
        Ok(draftline::RemoteCredential::SshAgent {
            username: "git".to_string(),
        })
    });
    let workspace = Workspace::clone_workspace_with(
        file_url(source.path()),
        target.path().join("clone"),
        ContentPolicy::default(),
        &CloneOptions::new().with_blobless(true),
        &mut options,
    )
    .unwrap();
    drop(options);
    assert!(!asked.get());
    assert!(workspace.is_partial_clone());
}

#[test]
fn contract_asks_for_a_blobless_clone() {
    if !git_available() {
        eprintln!("skipped: no git");
        return;
    }
    let (source, _) = history_repo();
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("clone");
    let request: CloneWorkspaceWithOptionsRequest = serde_json::from_value(serde_json::json!({
        "remote_url": file_url(source.path()),
        "workspace_path": path,
        "blobless": true,
    }))
    .unwrap();
    clone_workspace_with_options_with_context(&mut DraftlineCommandContext::new(), request)
        .unwrap();
    // history_repo's second commit on main also carries the marker.
    assert!(Workspace::open(path.join("cutready"))
        .unwrap()
        .is_partial_clone());
    let request: CloneWorkspaceWithOptionsRequest = serde_json::from_value(serde_json::json!({
        "remote_url": file_url(source.path()),
        "workspace_path": path,
    }))
    .unwrap();
    assert!(!request.blobless, "blobless is opt-in");
}

fn clone_plain(remote: &Path, dest: &Path) -> draftline::Result<draftline::ClonedRepository> {
    Workspace::clone_repository(
        url(remote),
        dest,
        &CloneOptions::new(),
        &mut RemoteOptions::new(),
    )
}

#[test]
fn a_plain_clone_of_an_unmarked_repository_leaves_room_for_a_project_folder() {
    let source = source_repo();
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("clone");

    let cloned = clone_plain(source.path(), &dest).unwrap();

    assert!(cloned.inspection.is_repository);
    assert_eq!(cloned.inspection.project, None);
    assert_eq!(cloned.project_path, None);
    assert_eq!(cloned.inspection.current_branch.as_deref(), Some("main"));
    // Nothing claimed: no marker, no local record, no state folder.
    assert!(!dest.join(".draftline").exists());
    assert!(!dest.join(".git").join("draftline").exists());
    let repo = git2::Repository::open(&dest).unwrap();
    assert!(repo.statuses(None).unwrap().is_empty());

    let workspace = Workspace::init_project_in_repository(
        &dest,
        "Q3 Demo",
        ProjectSettingsUpdate::default(),
        ContentPolicy::default(),
    )
    .unwrap();
    assert_eq!(workspace.scope(), Path::new("q3-demo"));
}

#[test]
fn a_plain_clone_of_a_marked_branch_reports_and_creates_its_project_folder() {
    let source = source_repo();
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("clone");

    let cloned = Workspace::clone_repository(
        url(source.path()),
        &dest,
        &CloneOptions::new().with_branch("cutready/review"),
        &mut RemoteOptions::new(),
    )
    .unwrap();

    let project = cloned.inspection.project.unwrap();
    assert_eq!(project.folder, Path::new("cutready"));
    assert!(project.in_marker);
    let path = cloned.project_path.unwrap();
    assert!(path.ends_with("cutready") && path.is_dir());
    assert!(!dest.join(".git").join("draftline").exists());
    let workspace = Workspace::open(&path).unwrap();
    assert_eq!(workspace.scope(), Path::new("cutready"));
}

#[test]
fn a_plain_clone_leaves_its_destination_as_it_was_when_it_fails() {
    let source = source_repo();
    let temp = tempfile::tempdir().unwrap();

    // A folder with something in it is refused untouched.
    let full = temp.path().join("full");
    std::fs::create_dir(&full).unwrap();
    std::fs::write(full.join("keep.txt"), "mine").unwrap();
    let error = clone_plain(source.path(), &full).unwrap_err();
    assert!(
        matches!(error, DraftlineError::FolderNotEmpty(_)),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(full.join("keep.txt")).unwrap(),
        "mine"
    );

    // A missing remote leaves no folder behind, or an empty one emptied.
    let missing = temp.path().join("nowhere");
    let dest = temp.path().join("new");
    assert!(clone_plain(&missing, &dest).is_err());
    assert!(!dest.exists());
    let empty = temp.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(clone_plain(&missing, &empty).is_err());
    assert_eq!(empty.read_dir().unwrap().count(), 0);

    // An unreadable marker is refused, and the chosen folder stays, empty.
    let repo = git2::Repository::open(source.path()).unwrap();
    commit_files(
        &repo,
        "main",
        &[(".draftline", "not a marker")],
        "Bad marker",
    );
    let error = clone_plain(source.path(), &empty).unwrap_err();
    assert!(
        matches!(error, DraftlineError::InvalidFolderMarker(_)),
        "{error:?}"
    );
    assert!(empty.is_dir());
    assert_eq!(empty.read_dir().unwrap().count(), 0);
}

#[test]
fn contract_clones_a_repository_without_claiming_it() {
    let source = source_repo();
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("clone");
    let events = Arc::new(Mutex::new(Vec::<CloneProgressEvent>::new()));
    let sink = Arc::clone(&events);
    let mut context = DraftlineCommandContext::new().with_clone_progress_sink(move |event| {
        sink.lock().unwrap().push(event);
    });

    let cloned = clone_repository_with_context(
        &mut context,
        CloneRepositoryRequest {
            remote_url: url(source.path()),
            workspace_path: dest.clone(),
            branch: None,
            progress_id: Some("plain".to_string()),
            blobless: false,
        },
    )
    .unwrap();

    let json = serde_json::to_value(&cloned).unwrap();
    assert_eq!(json["is_repository"], true);
    assert_eq!(json["project"], serde_json::Value::Null);
    assert_eq!(json["project_path"], serde_json::Value::Null);
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .all(|event| event.progress_id.as_deref() == Some("plain")));
    assert!(events
        .iter()
        .any(|event| event.progress.stage == CloneStage::CheckingOut));

    let error = clone_repository(CloneRepositoryRequest {
        remote_url: url(source.path()),
        workspace_path: dest,
        branch: None,
        progress_id: None,
        blobless: false,
    })
    .unwrap_err();
    assert_eq!(
        draftline::tauri_contract::TauriCommandError::from(error).code,
        "folder_not_empty"
    );
}

#[test]
fn a_rejected_login_stops_asking_instead_of_looping() {
    let server = httpmock::MockServer::start();
    server.mock(|_, then| {
        then.status(401)
            .header("WWW-Authenticate", "Basic realm=\"git\"");
    });
    let temp = tempfile::tempdir().unwrap();
    let asked = std::cell::Cell::new(0);
    let mut options = RemoteOptions::new().with_credentials(|_| {
        asked.set(asked.get() + 1);
        Ok(RemoteCredential::UsernamePassword {
            username: "x-access-token".to_string(),
            password: "wrong".to_string(),
        })
    });

    let result = Workspace::clone_repository(
        server.url("/o/r.git"),
        temp.path().join("clone"),
        &CloneOptions::new(),
        &mut options,
    );

    assert!(result.is_err());
    assert!(
        (1..=5).contains(&asked.get()),
        "asked {} times",
        asked.get()
    );
    assert!(!temp.path().join("clone").exists());
}

#[test]
fn a_plain_clone_never_fills_a_link_or_file_and_leaves_no_staging_behind() {
    let source = source_repo();
    let temp = tempfile::tempdir().unwrap();
    let names = |path: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };

    let file = temp.path().join("file");
    std::fs::write(&file, "").unwrap();
    let error = clone_plain(source.path(), &file).unwrap_err();
    assert!(
        matches!(error, DraftlineError::FolderNotEmpty(_)),
        "{error:?}"
    );

    let target = temp.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = temp.path().join("link");
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&target, &link);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &link);
    if linked.is_ok() {
        let error = clone_plain(source.path(), &link).unwrap_err();
        assert!(
            matches!(error, DraftlineError::FolderNotEmpty(_)),
            "{error:?}"
        );
        assert_eq!(target.read_dir().unwrap().count(), 0);
    }

    let before = names(temp.path());
    assert!(clone_plain(&temp.path().join("nowhere"), &temp.path().join("dest")).is_err());
    assert_eq!(names(temp.path()), before);
    let deep = temp.path().join("a").join("b").join("clone");
    assert!(clone_plain(&temp.path().join("nowhere"), &deep).is_err());
    assert_eq!(names(temp.path()), before);

    clone_plain(source.path(), &deep).unwrap();
    assert_eq!(names(&temp.path().join("a").join("b")), ["clone"]);
}
