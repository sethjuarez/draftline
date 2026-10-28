//! Walks every crash point of an operation: runs it once in a child process
//! with `DRAFTLINE_FAULT_TRACE` to learn the points it reaches, then once per
//! point with `DRAFTLINE_FAULT` set so the child aborts there, and checks that
//! the workspace recovers without losing content. Needs the
//! `fault-injection` feature.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use draftline::{SwitchPolicy, VariationId, Workspace};

const REPO: &str = "DRAFTLINE_TEST_FAULT_REPO";
const OPERATION: &str = "DRAFTLINE_TEST_FAULT_OPERATION";

type Setup = fn() -> (tempfile::TempDir, PathBuf);

/// [`workspace`], by path.
fn standalone() -> (tempfile::TempDir, PathBuf) {
    let (temp, workspace) = workspace();
    let root = workspace.root().to_path_buf();
    (temp, root)
}

/// A workspace with `notes.md` saved as `first`, then `main`, on `main`,
/// and as `other` on the variation `other`, checked out on `main`, with the
/// remote `origin` holding `first`.
fn workspace() -> (tempfile::TempDir, Workspace) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    let remote = temp.path().join("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    let workspace = Workspace::init(&root).unwrap();
    workspace
        .add_remote("origin", remote.to_str().unwrap())
        .unwrap();
    fs::write(root.join("notes.md"), "first\n").unwrap();
    workspace.save_version("first").unwrap();
    workspace.publish_changes("origin").unwrap();
    fs::write(root.join("notes.md"), "main\n").unwrap();
    workspace.save_version("main").unwrap();
    let other = workspace.create_variation("other").unwrap();
    workspace
        .switch_variation(other.id(), SwitchPolicy::AbortIfDirty)
        .unwrap();
    fs::write(root.join("notes.md"), "other\n").unwrap();
    workspace.save_version("other").unwrap();
    workspace
        .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
        .unwrap();
    (temp, workspace)
}

fn read(root: &Path) -> String {
    fs::read_to_string(root.join("notes.md"))
        .unwrap()
        .replace("\r\n", "\n")
}

/// The operation under test, run in the child.
fn run(operation: &str, root: &Path) {
    let workspace = Workspace::open(root).unwrap();
    match operation {
        "switch" => {
            workspace
                .switch_variation(&VariationId::from("other"), SwitchPolicy::AbortIfDirty)
                .unwrap();
        }
        "save" => {
            fs::write(root.join("notes.md"), "edited\n").unwrap();
            workspace.save_version("edited").unwrap();
        }
        "restore" => {
            let first = workspace.versions().unwrap()[1].clone();
            workspace
                .restore_version_as_new_save(first.id(), "restored")
                .unwrap();
        }
        "discard" => {
            fs::write(root.join("notes.md"), "edited\n").unwrap();
            workspace.discard_changes().unwrap();
        }
        "publish" => {
            workspace.publish_changes("origin").unwrap();
        }
        other => panic!("unknown operation {other}"),
    }
}

#[test]
#[ignore = "run by the parent tests in a child process"]
fn child() {
    let root = std::env::var_os(REPO).unwrap();
    let operation = std::env::var(OPERATION).unwrap();
    run(&operation, Path::new(&root));
}

/// Runs [`child`] on `root`, with `env` set.
fn spawn(operation: &str, root: &Path, env: &[(&str, &str)]) -> std::process::ExitStatus {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "child",
            "--ignored",
            "--test-threads=1",
            "--quiet",
        ])
        .env(REPO, root)
        .env(OPERATION, operation)
        .env_remove("DRAFTLINE_FAULT")
        .env_remove("DRAFTLINE_FAULT_TRACE");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap().status
}

/// The crash points `operation` reaches, as `DRAFTLINE_FAULT` values.
fn trace(operation: &str, setup: Setup) -> Vec<String> {
    let (temp, root) = setup();
    let trace = temp.path().join("fault-trace");
    let status = spawn(
        operation,
        &root,
        &[("DRAFTLINE_FAULT_TRACE", trace.to_str().unwrap())],
    );
    assert!(status.success(), "{operation} failed without a fault");
    let points = fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(!points.is_empty(), "{operation} reached no crash points");
    points
}

/// Crashes `operation` at each point it reaches, repairs, and hands the
/// recovered workspace to `check`.
fn walk(operation: &str, setup: Setup, check: impl Fn(&str, &Path, &Workspace)) {
    for point in trace(operation, setup) {
        let (_temp, root) = setup();
        let status = spawn(operation, &root, &[("DRAFTLINE_FAULT", &point)]);
        assert!(!status.success(), "{point}: the child did not crash");

        let workspace = Workspace::open(&root).unwrap();
        // As a host does on startup: the dead child's lock is stale.
        workspace.clear_stale_lock().unwrap();
        if let Some(state) = workspace.recovery_state().unwrap() {
            if !state.completed {
                // Finish the operation where Draftline can; undo it otherwise.
                let repaired = workspace.repair_recovery(&state.operation_id).unwrap();
                if !repaired.completed {
                    let rolled_back = workspace.rollback_recovery(&state.operation_id).unwrap();
                    assert!(rolled_back.completed, "{point}: recovery did not complete");
                }
            }
            workspace.acknowledge_recovery().unwrap();
        }
        check(&point, &root, &workspace);
    }
}

#[test]
fn a_switch_recovers_from_a_crash_at_every_point() {
    walk("switch", standalone, |point, root, workspace| {
        let expected = match workspace.current_variation().unwrap().as_str() {
            "main" => "main\n",
            "other" => "other\n",
            variation => panic!("{point}: on {variation}"),
        };
        assert_eq!(read(root), expected, "{point}");
        assert!(
            workspace.changed_files().unwrap().is_empty(),
            "{point}: changes left after recovery"
        );
    });
}

#[test]
fn a_save_keeps_the_edit_through_a_crash_at_every_point() {
    walk("save", standalone, |point, root, workspace| {
        assert_eq!(read(root), "edited\n", "{point}: the edit was lost");
        let saved = workspace.versions().unwrap()[0].label == "edited";
        let pending = !workspace.changed_files().unwrap().is_empty();
        assert!(
            saved != pending,
            "{point}: saved {saved}, pending {pending}"
        );
        if pending {
            workspace
                .save_version("again")
                .unwrap_or_else(|error| panic!("{point}: {error:?}"));
        }
        assert!(workspace.changed_files().unwrap().is_empty(), "{point}");
    });
}

/// The labels of the newest versions, newest first.
fn labels(workspace: &Workspace) -> Vec<String> {
    workspace
        .versions()
        .unwrap()
        .into_iter()
        .map(|version| version.label)
        .collect()
}

#[test]
fn a_restore_recovers_from_a_crash_at_every_point() {
    walk("restore", standalone, |point, root, workspace| {
        let expected = match labels(workspace)[0].as_str() {
            "restored" => "first\n",
            "main" => "main\n",
            label => panic!("{point}: newest version {label}"),
        };
        assert_eq!(read(root), expected, "{point}");
        assert!(workspace.changed_files().unwrap().is_empty(), "{point}");
    });
}

#[test]
fn a_discard_recovers_from_a_crash_at_every_point() {
    walk("discard", standalone, |point, root, workspace| {
        let content = read(root);
        let pending = !workspace.changed_files().unwrap().is_empty();
        match content.as_str() {
            "edited\n" => assert!(pending, "{point}"),
            "main\n" => assert!(!pending, "{point}"),
            content => panic!("{point}: {content:?}"),
        }
        assert_eq!(labels(workspace)[0], "main", "{point}");
    });
}

#[test]
fn a_publish_can_be_retried_after_a_crash_at_every_point() {
    walk("publish", standalone, |point, root, workspace| {
        assert_eq!(read(root), "main\n", "{point}");
        workspace
            .publish_changes("origin")
            .unwrap_or_else(|error| panic!("{point}: {error:?}"));
        let remote = git2::Repository::open(root.parent().unwrap().join("remote.git")).unwrap();
        let published = remote.refname_to_id("refs/heads/main").unwrap();
        let head = workspace.versions().unwrap()[0].id().as_str().to_string();
        assert_eq!(published.to_string(), head, "{point}");
    });
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const IMAGES: u8 = 5;

/// A screenshot-like binary, distinct per `seed`.
fn image(seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend((0..4096u32).map(|i| (i as u8).wrapping_mul(seed) ^ seed));
    bytes
}

/// A developer's repository that keeps `*.png` in Git LFS, with a Draftline
/// folder `cutready/` holding images `1` on `main` and `2` on `other`,
/// checked out on `main`. `None` without git-lfs, unless CI requires it.
fn lfs() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("work");
    fs::create_dir_all(repo.join("cutready")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Dev"]);
    git(&repo, &["config", "user.email", "dev@example.test"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    git(&repo, &["lfs", "install", "--local", "--force"]);
    fs::write(
        repo.join(".gitattributes"),
        "*.png filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    fs::write(repo.join("cutready").join("notes.md"), "main\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "Initial app"]);
    let root = repo.join("cutready");
    let policy = draftline::ContentPolicy::new()
        .include_extensions(["md", "png"])
        .unwrap();
    let workspace = Workspace::init_with_policy(&root, policy).unwrap();
    let images = |seed: u8| {
        for index in 0..IMAGES {
            fs::write(root.join(format!("{index}.png")), image(seed + index)).unwrap();
        }
    };
    images(10);
    workspace.save_version("main").unwrap();
    let other = workspace.create_variation("other").unwrap();
    workspace
        .switch_variation(other.id(), SwitchPolicy::AbortIfDirty)
        .unwrap();
    images(20);
    fs::write(root.join("notes.md"), "other\n").unwrap();
    workspace.save_version("other").unwrap();
    workspace
        .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
        .unwrap();
    (temp, root)
}

fn git_lfs_installed() -> bool {
    let installed = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        installed || std::env::var_os("DRAFTLINE_REQUIRE_GIT_LFS").is_none(),
        "DRAFTLINE_REQUIRE_GIT_LFS is set but git-lfs is not installed"
    );
    installed
}

#[test]
fn a_git_lfs_switch_recovers_real_content_from_a_crash_at_every_point() {
    if !git_lfs_installed() {
        eprintln!("skipping: git-lfs is not installed");
        return;
    }
    walk("switch", lfs, |point, root, workspace| {
        let (notes, seed) = match workspace.current_variation().unwrap().as_str() {
            "main" => ("main\n", 10),
            "other" => ("other\n", 20),
            variation => panic!("{point}: on {variation}"),
        };
        assert_eq!(read(root), notes, "{point}");
        for index in 0..IMAGES {
            let actual = fs::read(root.join(format!("{index}.png"))).unwrap();
            assert!(
                actual == image(seed + index),
                "{point}: {index}.png is not image {} ({} bytes)",
                seed + index,
                actual.len()
            );
        }
        assert!(workspace.changed_files().unwrap().is_empty(), "{point}");
    });
}
