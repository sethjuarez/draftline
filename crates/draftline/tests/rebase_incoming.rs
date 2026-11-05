//! `rebase_onto_incoming`: bringing teammates' versions in without a merge
//! version by replaying this checkout's unshared versions on top of them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use draftline::{DraftlineError, RemoteOptions, SyncState, Workspace};

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// File text with line endings as committed (checkouts may add `\r`).
fn read_file(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative))
        .unwrap()
        .replace("\r\n", "\n")
}

fn configure_identity(root: &Path, name: &str) {
    let repo = git2::Repository::open(root).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", name).unwrap();
    config
        .set_str(
            "user.email",
            &format!("{}@example.test", name.to_lowercase()),
        )
        .unwrap();
}

struct Team {
    _remote: tempfile::TempDir,
    _author_dir: tempfile::TempDir,
    _teammate_dir: tempfile::TempDir,
    remote_path: PathBuf,
    author: Workspace,
    teammate: Workspace,
}

/// An author and a teammate sharing a published `shared.md` through a bare
/// `origin`.
fn team() -> Team {
    let remote = tempfile::tempdir().unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    git2::Repository::init_opts(remote.path(), &options).unwrap();

    let author_dir = tempfile::tempdir().unwrap();
    let author = Workspace::init(author_dir.path()).unwrap();
    configure_identity(author.root(), "Author");
    write_file(author.root(), "shared.md", "one\ntwo\nthree\nfour\nfive\n");
    author.save_version("Base").unwrap();
    author
        .add_remote("origin", remote.path().to_string_lossy())
        .unwrap();
    author.publish_changes("origin").unwrap();

    let teammate_dir = tempfile::tempdir().unwrap();
    let teammate =
        Workspace::clone_workspace(remote.path().to_string_lossy(), teammate_dir.path()).unwrap();
    configure_identity(teammate.root(), "Teammate");
    Team {
        remote_path: remote.path().to_path_buf(),
        _remote: remote,
        _author_dir: author_dir,
        _teammate_dir: teammate_dir,
        author,
        teammate,
    }
}

fn teammate_publishes(team: &Team, relative: &str, content: &str, label: &str) {
    write_file(team.teammate.root(), relative, content);
    team.teammate.save_version(label).unwrap();
    team.teammate.publish_changes("origin").unwrap();
}

fn rebase(workspace: &Workspace) -> draftline::Result<draftline::RebaseOutcome> {
    workspace.rebase_onto_incoming("origin", &mut RemoteOptions::new())
}

fn repo(workspace: &Workspace) -> git2::Repository {
    git2::Repository::open(workspace.root()).unwrap()
}

/// Everything a refused rebase must leave alone: local refs, HEAD, the index
/// and every file in the working tree (the fetch may update remote refs).
fn snapshot(workspace: &Workspace) -> BTreeMap<String, Vec<u8>> {
    let repo = repo(workspace);
    let mut state = BTreeMap::new();
    for reference in repo.references().unwrap() {
        let reference = reference.unwrap();
        let name = reference.name().unwrap().to_string();
        if !name.starts_with("refs/remotes/") {
            let target = reference.target().map(|oid| oid.to_string());
            state.insert(name, target.unwrap_or_default().into_bytes());
        }
    }
    let head = fs::read(workspace.root().join(".git/HEAD")).unwrap();
    state.insert("HEAD".into(), head);
    let index = fs::read(workspace.root().join(".git/index")).unwrap();
    state.insert("index".into(), index);
    let draftline_dir = workspace.root().join(".git/draftline");
    if draftline_dir.exists() {
        for entry in fs::read_dir(draftline_dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                let name = format!("draftline/{}", entry.file_name().to_string_lossy());
                state.insert(name, fs::read(entry.path()).unwrap());
            }
        }
    }
    collect_files(workspace.root(), workspace.root(), &mut state);
    state
}

fn collect_files(root: &Path, dir: &Path, state: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            collect_files(root, &path, state);
        } else {
            let relative = path.strip_prefix(root).unwrap().to_string_lossy();
            state.insert(format!("file:{relative}"), fs::read(&path).unwrap());
        }
    }
}

#[test]
fn disjoint_versions_come_in_without_a_merge_or_touching_local_files() {
    let team = team();
    write_file(team.author.root(), "author.md", "mine\n");
    let local = team
        .author
        .save_version("Author notes\n\nWhy I wrote them")
        .unwrap();
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate notes");

    let author_file = team.author.root().join("author.md");
    let before_mtime = fs::metadata(&author_file).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let preflight = {
        team.author.fetch_remote("origin").unwrap();
        team.author.preflight_rebase_incoming("origin").unwrap()
    };
    assert!(preflight.can_rebase);
    assert_eq!(preflight.local_versions.len(), 1);
    assert_eq!(preflight.local_versions[0].label, "Author notes");

    let outcome = rebase(&team.author).unwrap();

    assert!(outcome.changed_workspace);
    assert_eq!(outcome.rewritten.len(), 1);
    assert_eq!(&outcome.rewritten[0].from, local.id());
    assert_eq!(outcome.incoming.len(), 1);
    assert_eq!(outcome.incoming[0].label, "Teammate notes");
    assert_eq!(outcome.incoming[0].author.name, "Teammate");
    assert_eq!(outcome.sync_status.ahead, 1);
    assert_eq!(outcome.sync_status.behind, 0);
    assert_eq!(outcome.sync_status.state, SyncState::LocalAhead);
    assert_eq!(read_file(team.author.root(), "teammate.md"), "theirs\n");
    assert_eq!(
        fs::metadata(&author_file).unwrap().modified().unwrap(),
        before_mtime,
        "a file only the local version touched must not be rewritten"
    );

    let versions = team.author.versions().unwrap();
    assert_eq!(versions[0].id(), &outcome.rewritten[0].to);
    assert_eq!(versions[0].label, "Author notes");
    assert_eq!(versions[0].comment.as_deref(), Some("Why I wrote them"));
    assert_eq!(versions[0].author.name, "Author");
    assert_eq!(versions[0].saved_by.name, "Author");
    assert_eq!(versions[1].label, "Teammate notes");
    let author_repo = repo(&team.author);
    let head = author_repo
        .find_commit(outcome.head.to_string().parse().unwrap())
        .unwrap();
    assert_eq!(head.parent_count(), 1, "no merge version");
    assert!(team.author.changes().unwrap().is_empty());

    team.author.publish_changes("origin").unwrap();
    assert_eq!(
        team.author.sync_status("origin").unwrap().state,
        SyncState::UpToDate
    );
}

#[test]
fn overlapping_edits_that_combine_cleanly_are_replayed() {
    let team = team();
    let base = "---\ntitle: Base\n---\nbody\n";
    write_file(team.author.root(), "scene.md", base);
    team.author.save_version("Scene").unwrap();
    team.author.publish_changes("origin").unwrap();
    team.teammate.fetch_remote("origin").unwrap();
    team.teammate
        .apply_incoming("origin", &mut RemoteOptions::new())
        .unwrap();

    write_file(
        team.author.root(),
        "scene.md",
        "---\ntitle: Author title\n---\nbody\n",
    );
    team.author.save_version("Author retitles").unwrap();
    teammate_publishes(
        &team,
        "scene.md",
        "---\ntitle: Base\n---\nteammate body\n",
        "Teammate rewrites body",
    );

    let outcome = rebase(&team.author).unwrap();

    assert_eq!(outcome.rewritten.len(), 1);
    assert_eq!(
        read_file(team.author.root(), "scene.md"),
        "---\ntitle: Author title\n---\nteammate body\n"
    );
    assert!(team.author.changes().unwrap().is_empty());
}

#[test]
fn a_conflict_refuses_and_leaves_the_repository_byte_identical() {
    let team = team();
    write_file(
        team.author.root(),
        "shared.md",
        "one\nauthor\nthree\nfour\nfive\n",
    );
    team.author.save_version("Author line two").unwrap();
    write_file(team.author.root(), "author.md", "second\n");
    team.author.save_version("Author second").unwrap();
    teammate_publishes(
        &team,
        "shared.md",
        "one\nteammate\nthree\nfour\nfive\n",
        "Teammate line two",
    );
    team.author.fetch_remote("origin").unwrap();
    let preflight = team.author.preflight_rebase_incoming("origin").unwrap();
    assert!(!preflight.can_rebase);
    assert_eq!(preflight.conflicts, [PathBuf::from("shared.md")]);
    let before = snapshot(&team.author);

    let error = rebase(&team.author).unwrap_err();

    assert!(
        matches!(&error, DraftlineError::IncomingConflicts(paths) if paths == &[PathBuf::from("shared.md")]),
        "{error:?}"
    );
    assert_eq!(snapshot(&team.author), before);
    assert!(team.author.recovery_state().unwrap().is_none());
    assert!(!team.author.root().join(".git/rebase-merge").exists());
    assert!(!team.author.root().join(".git/rebase-apply").exists());
    // The merge flow is still there to fall back to.
    assert!(
        !team
            .author
            .preflight_merge_incoming("origin")
            .unwrap()
            .can_merge_cleanly
    );
}

#[test]
fn unsaved_changes_are_refused() {
    let team = team();
    write_file(team.author.root(), "author.md", "saved\n");
    team.author.save_version("Author").unwrap();
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");
    write_file(team.author.root(), "draft.md", "unsaved\n");
    let before = snapshot(&team.author);

    let error = rebase(&team.author).unwrap_err();

    assert!(
        matches!(&error, DraftlineError::PreflightFailed(report) if report.operation == "rebase_onto_incoming"),
        "{error:?}"
    );
    assert_eq!(snapshot(&team.author), before);
}

#[test]
fn versions_already_on_another_remote_branch_are_never_rewritten() {
    let team = team();
    write_file(team.author.root(), "author.md", "mine\n");
    let local = team.author.save_version("Author").unwrap();
    repo(&team.author)
        .find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/backup"], None)
        .unwrap();
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");
    let before = snapshot(&team.author);

    let error = rebase(&team.author).unwrap_err();

    assert!(
        matches!(&error, DraftlineError::VersionsAlreadyShared(ids) if ids == &[local.id().clone()]),
        "{error:?}"
    );
    assert_eq!(snapshot(&team.author), before);
}

#[test]
fn several_local_versions_keep_their_order_labels_and_authors() {
    let team = team();
    let mut originals = Vec::new();
    for (name, label) in [("a.md", "First"), ("b.md", "Second"), ("c.md", "Third")] {
        write_file(team.author.root(), name, label);
        originals.push(team.author.save_version(label).unwrap());
    }
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");
    teammate_publishes(&team, "teammate2.md", "more\n", "Teammate again");

    let outcome = rebase(&team.author).unwrap();

    let from = outcome
        .rewritten
        .iter()
        .map(|step| step.from.clone())
        .collect::<Vec<_>>();
    let expected = originals
        .iter()
        .map(|version| version.id().clone())
        .collect::<Vec<_>>();
    assert_eq!(from, expected);
    assert_eq!(outcome.incoming.len(), 2);
    let labels = team
        .author
        .versions()
        .unwrap()
        .into_iter()
        .map(|version| version.label)
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        [
            "Third",
            "Second",
            "First",
            "Teammate again",
            "Teammate",
            "Base"
        ]
    );
    let versions = team.author.versions().unwrap();
    for (version, step) in versions.iter().take(3).zip(outcome.rewritten.iter().rev()) {
        assert_eq!(version.id(), &step.to);
        assert_eq!(version.author.name, "Author");
    }
    assert_eq!(outcome.sync_status.ahead, 3);
    for (name, label) in [("a.md", "First"), ("b.md", "Second"), ("c.md", "Third")] {
        assert_eq!(read_file(team.author.root(), name), label);
    }
}

#[test]
fn the_operation_lock_is_respected() {
    let team = team();
    write_file(team.author.root(), "author.md", "mine\n");
    team.author.save_version("Author").unwrap();
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");
    let lock = team.author.root().join(".git/draftline/operation.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "{}").unwrap();
    let before = snapshot(&team.author);

    let error = rebase(&team.author).unwrap_err();

    assert!(
        matches!(error, DraftlineError::WorkspaceLocked),
        "{error:?}"
    );
    assert_eq!(snapshot(&team.author), before);
}

#[test]
fn nothing_local_fast_forwards_and_nothing_incoming_is_a_no_op() {
    let team = team();
    let unchanged = rebase(&team.author).unwrap();
    assert!(!unchanged.changed_workspace);
    assert_eq!(unchanged.head, unchanged.previous_head);

    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");
    let outcome = rebase(&team.author).unwrap();
    assert!(outcome.changed_workspace);
    assert!(outcome.rewritten.is_empty());
    assert_eq!(outcome.incoming.len(), 1);
    assert_eq!(
        outcome.sync_status.state,
        SyncState::UpToDate,
        "{:?}",
        outcome.sync_status
    );
    assert_eq!(read_file(team.author.root(), "teammate.md"), "theirs\n");

    write_file(team.author.root(), "author.md", "mine\n");
    team.author.save_version("Author").unwrap();
    let ahead_only = rebase(&team.author).unwrap();
    assert!(!ahead_only.changed_workspace);
    assert_eq!(ahead_only.sync_status.ahead, 1);
}

#[test]
fn a_local_version_the_teammate_already_made_is_dropped() {
    let team = team();
    write_file(team.author.root(), "same.md", "same\n");
    let duplicate = team.author.save_version("Same edit").unwrap();
    write_file(team.author.root(), "author.md", "mine\n");
    team.author.save_version("Author").unwrap();
    teammate_publishes(&team, "same.md", "same\n", "Same edit, by teammate");

    let outcome = rebase(&team.author).unwrap();

    assert_eq!(outcome.dropped, [duplicate.id().clone()]);
    assert_eq!(outcome.rewritten.len(), 1);
    assert_eq!(team.author.versions().unwrap()[0].label, "Author");
    assert_eq!(outcome.sync_status.ahead, 1);
}

#[test]
fn a_local_merge_version_is_left_to_the_merge_flow() {
    let team = team();
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");
    write_file(team.author.root(), "author.md", "mine\n");
    team.author.save_version("Author").unwrap();
    team.author.fetch_remote("origin").unwrap();
    let token = team
        .author
        .preflight_merge_incoming("origin")
        .unwrap()
        .token
        .unwrap();
    team.author
        .merge_incoming(token, "Merge", &mut RemoteOptions::new())
        .unwrap();
    teammate_publishes(&team, "later.md", "later\n", "Teammate later");
    let before = snapshot(&team.author);

    let error = rebase(&team.author).unwrap_err();

    assert!(
        matches!(&error, DraftlineError::RebaseUnsupported(reason) if reason.contains("merge")),
        "{error:?}"
    );
    assert_eq!(snapshot(&team.author), before);
    // Sanity: the remote is reachable for the fallback.
    assert!(team.remote_path.exists());
}

#[test]
fn an_originally_empty_local_commit_is_kept_not_dropped() {
    let team = team();
    write_file(team.author.root(), "author.md", "mine\n");
    team.author.save_version("Author").unwrap();
    let repo = repo(&team.author);
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let signature = repo.signature().unwrap();
    let empty = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Trigger CI",
            &head.tree().unwrap(),
            &[&head],
        )
        .unwrap();
    teammate_publishes(&team, "teammate.md", "theirs\n", "Teammate");

    let outcome = rebase(&team.author).unwrap();

    assert!(outcome.dropped.is_empty());
    assert_eq!(outcome.rewritten.len(), 2);
    assert_eq!(outcome.rewritten[1].from.to_string(), empty.to_string());
    let new_head = repo
        .find_commit(git2::Oid::from_str(&outcome.head.to_string()).unwrap())
        .unwrap();
    assert_eq!(new_head.message().unwrap(), "Trigger CI");
    assert_eq!(new_head.tree_id(), new_head.parent(0).unwrap().tree_id());
}
