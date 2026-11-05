use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use draftline::tauri_contract::{
    clone_workspace_with_options_with_context, CloneProgressEvent,
    CloneWorkspaceWithOptionsRequest, DraftlineCommandContext,
};
use draftline::{
    CloneOptions, CloneProgress, CloneStage, ContentPolicy, DraftlineError, RemoteOptions,
    Workspace,
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
