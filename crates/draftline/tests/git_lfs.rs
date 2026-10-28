//! Git LFS through the `git` executable: where the repository configures the
//! LFS filter and git-lfs is installed, saves store LFS pointers, checkouts
//! write the real content, and pushes upload it. These tests need git-lfs;
//! without it they skip, unless `DRAFTLINE_REQUIRE_GIT_LFS` is set (as in CI).

use std::fs;
use std::path::Path;
use std::process::Command;

use draftline::{ContentPolicy, SwitchPolicy, VariationId, Workspace};

const POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git_lfs_installed() -> bool {
    let installed = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !installed {
        assert!(
            std::env::var_os("DRAFTLINE_REQUIRE_GIT_LFS").is_none(),
            "DRAFTLINE_REQUIRE_GIT_LFS is set but git-lfs is not installed"
        );
        eprintln!("skipping: git-lfs is not installed");
    }
    installed
}

fn write(root: &Path, relative: &str, content: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A screenshot-like binary, distinct per `seed`.
fn image(seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend((0..4096u32).map(|i| (i as u8).wrapping_mul(seed) ^ seed));
    bytes
}

fn policy() -> ContentPolicy {
    ContentPolicy::new()
        .include_extensions(["md", "png"])
        .unwrap()
}

/// A developer's repository that keeps `*.png` in Git LFS, with git-lfs set
/// up for it, and a Draftline folder `cutready/` in it.
fn lfs_repo() -> Option<(tempfile::TempDir, git2::Repository, Workspace)> {
    if !git_lfs_installed() {
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Dev"]);
    git(root, &["config", "user.email", "dev@example.test"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["lfs", "install", "--local", "--force"]);
    write(
        root,
        ".gitattributes",
        b"*.png filter=lfs diff=lfs merge=lfs -text\n",
    );
    write(root, "src/main.rs", b"fn main() {}\n");
    write(root, "cutready/notes.md", b"# notes\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "Initial app"]);
    let repo = git2::Repository::open(root).unwrap();
    let workspace = Workspace::init_with_policy(root.join("cutready"), policy()).unwrap();
    Some((temp, repo, workspace))
}

fn committed(repo: &git2::Repository, spec: &str) -> Vec<u8> {
    repo.revparse_single(spec)
        .unwrap()
        .peel_to_blob()
        .unwrap()
        .content()
        .to_vec()
}

fn assert_file(root: &Path, relative: &str, seed: u8) {
    let actual = read(root, relative);
    let expected = image(seed);
    assert!(
        actual == expected,
        "{relative}: {} bytes (pointer: {}), expected image({seed}) of {} bytes; first differing byte at {:?}",
        actual.len(),
        actual.starts_with(POINTER_PREFIX),
        expected.len(),
        actual.iter().zip(&expected).position(|(a, b)| a != b)
    );
}

fn read(root: &Path, relative: &str) -> Vec<u8> {
    fs::read(root.join(relative)).unwrap()
}

/// What Git itself sees as changed: the developer's view of the checkout.
fn git_status(root: &Path) -> String {
    git(root, &["status", "--porcelain"])
}

#[test]
fn a_save_stores_new_lfs_files_as_pointers_and_keeps_the_real_file() {
    let Some((temp, repo, workspace)) = lfs_repo() else {
        return;
    };
    let root = temp.path();
    write(root, "cutready/shots/intro.png", &image(1));

    workspace.save_version("Add intro shot").unwrap();

    let stored = committed(&repo, "HEAD:cutready/shots/intro.png");
    assert!(stored.starts_with(POINTER_PREFIX), "{stored:?}");
    assert_file(root, "cutready/shots/intro.png", 1);
    assert!(workspace.changed_files().unwrap().is_empty());
    assert_eq!(git_status(root), "");
    // Git LFS agrees it is a pointer it holds the content for.
    let listed = git(root, &["lfs", "ls-files", "--name-only"]);
    assert_eq!(listed.trim(), "cutready/shots/intro.png");
    git(root, &["lfs", "fsck"]);

    // Editing it is a change; saving again stores a new pointer.
    write(root, "cutready/shots/intro.png", &image(2));
    let changed = workspace.changed_files().unwrap();
    assert_eq!(changed.len(), 1);
    workspace.save_version("Retake intro").unwrap();
    let retaken = committed(&repo, "HEAD:cutready/shots/intro.png");
    assert!(retaken.starts_with(POINTER_PREFIX));
    assert_ne!(retaken, stored);
    assert_eq!(git_status(root), "");
}

#[test]
fn restore_switch_and_discard_write_real_lfs_content() {
    let Some((temp, repo, workspace)) = lfs_repo() else {
        return;
    };
    let root = temp.path();
    write(root, "cutready/intro.png", &image(1));
    let one = workspace.save_version("One").unwrap();
    write(root, "cutready/intro.png", &image(2));
    workspace.save_version("Two").unwrap();

    workspace
        .restore_version_as_new_save(one.id(), "Back to one")
        .unwrap();
    assert_file(root, "cutready/intro.png", 1);
    assert!(workspace.changed_files().unwrap().is_empty());
    assert_eq!(git_status(root), "");

    let alt = workspace.create_variation("cutready/alt").unwrap();
    workspace
        .switch_variation(alt.id(), SwitchPolicy::AbortIfDirty)
        .unwrap();
    write(root, "cutready/intro.png", &image(3));
    workspace.save_version("Alt take").unwrap();
    workspace
        .switch_variation(&VariationId::from("main"), SwitchPolicy::AbortIfDirty)
        .unwrap();
    assert_file(root, "cutready/intro.png", 1);
    assert_eq!(git_status(root), "");
    workspace
        .switch_variation(alt.id(), SwitchPolicy::AbortIfDirty)
        .unwrap();
    assert_file(root, "cutready/intro.png", 3);
    assert_eq!(git_status(root), "");

    write(root, "cutready/intro.png", &image(4));
    workspace.discard_changes().unwrap();
    assert_file(root, "cutready/intro.png", 3);
    assert!(workspace.changed_files().unwrap().is_empty());
    assert_eq!(git_status(root), "");
    assert!(committed(&repo, "HEAD:cutready/intro.png").starts_with(POINTER_PREFIX));
}

#[test]
fn shelving_keeps_lfs_content() {
    let Some((temp, _repo, workspace)) = lfs_repo() else {
        return;
    };
    let root = temp.path();
    write(root, "cutready/intro.png", &image(1));
    workspace.save_version("One").unwrap();
    write(root, "cutready/intro.png", &image(2));

    let shelf = workspace.shelve_changes("wip").unwrap();
    assert_file(root, "cutready/intro.png", 1);
    assert_eq!(git_status(root), "");

    workspace.apply_shelf(&shelf.id).unwrap();
    assert_file(root, "cutready/intro.png", 2);
}

#[test]
fn publishing_uploads_lfs_content_before_the_branch() {
    let Some((temp, _repo, workspace)) = lfs_repo() else {
        return;
    };
    let root = temp.path();
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "-q", "--bare"]);
    let url = remote.path().to_string_lossy().replace('\\', "/");
    workspace.add_remote("origin", &url).unwrap();
    write(root, "cutready/intro.png", &image(1));
    workspace.save_version("One").unwrap();

    workspace.publish_changes("origin").unwrap();

    // A fresh clone with git-lfs gets the real file, not a pointer.
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", "-q", &url, "copy", "--branch", "main"],
    );
    assert_eq!(
        read(&clone.path().join("copy"), "cutready/intro.png"),
        image(1)
    );
}

#[test]
fn a_pointer_whose_content_is_missing_is_never_pushed() {
    let Some((temp, _repo, workspace)) = lfs_repo() else {
        return;
    };
    let root = temp.path();
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "-q", "--bare"]);
    let url = remote.path().to_string_lossy().replace('\\', "/");
    workspace.add_remote("origin", &url).unwrap();
    write(root, "cutready/intro.png", &image(1));
    workspace.save_version("One").unwrap();
    // As if the local LFS cache was cleared, or the commit came from a
    // clone that never downloaded it.
    fs::remove_dir_all(root.join(".git").join("lfs")).unwrap();

    let error = workspace.publish_changes("origin").unwrap_err();

    assert!(
        matches!(error, draftline::DraftlineError::GitFilterFailed(_)),
        "{error:?}"
    );
    let remote_repo = git2::Repository::open_bare(remote.path()).unwrap();
    assert!(remote_repo.find_reference("refs/heads/main").is_err());
}

#[test]
fn a_failed_upload_refuses_the_push() {
    let Some((temp, _repo, workspace)) = lfs_repo() else {
        return;
    };
    let root = temp.path();
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "-q", "--bare"]);
    let url = remote.path().to_string_lossy().replace('\\', "/");
    workspace.add_remote("origin", &url).unwrap();
    // An LFS endpoint nothing listens on.
    git(root, &["config", "lfs.url", "http://127.0.0.1:9/lfs"]);
    write(root, "cutready/intro.png", &image(1));
    workspace.save_version("One").unwrap();

    let error = workspace.publish_changes("origin").unwrap_err();

    assert!(
        matches!(error, draftline::DraftlineError::GitFilterFailed(_)),
        "{error:?}"
    );
    let remote_repo = git2::Repository::open_bare(remote.path()).unwrap();
    assert!(remote_repo.find_reference("refs/heads/main").is_err());
}
