//! The `git` executable, for what libgit2 cannot do: run filter drivers such
//! as Git LFS. Draftline only calls it for files whose driver the repository
//! has configured; everything else stays in libgit2.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;

use git2::Oid;

use crate::{DraftlineError, Result};

/// Whether a `git` executable runs here (checked once per process).
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        git(&std::env::temp_dir())
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// A `git` command in `workdir` that ignores any repository the host
/// process's environment points at and never prompts on a terminal.
fn git(workdir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(workdir).env("GIT_TERMINAL_PROMPT", "0");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn failed(action: &str, output: String) -> DraftlineError {
    DraftlineError::GitFilterFailed {
        action: action.to_string(),
        output,
    }
}

/// Git's spelling of a repository-relative path: `/`-separated.
fn git_path(path: &Path) -> Result<String> {
    let text = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if text.contains(['\n', '\r']) {
        return Err(failed(
            "naming a file for Git",
            format!("{} has a line break in its name", path.display()),
        ));
    }
    Ok(text)
}

/// A running `git` whose stdin is fed and stderr drained on other threads,
/// so neither pipe can fill up and stall it.
struct Running {
    child: Child,
    feeder: JoinHandle<std::io::Result<()>>,
    errors: JoinHandle<String>,
}

impl Running {
    fn spawn(mut command: Command, input: Vec<u8>) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| failed("starting git", error.to_string()))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let feeder = std::thread::spawn(move || stdin.write_all(&input));
        let stderr: ChildStderr = child.stderr.take().expect("piped stderr");
        let errors = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            text
        });
        Ok(Self {
            child,
            feeder,
            errors,
        })
    }

    fn stdout(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(self.child.stdout.take().expect("piped stdout"))
    }

    /// Waits for `git`, turning a failure (or `read`'s) into its stderr text.
    fn finish<T>(mut self, what: &str, read: Result<T>) -> Result<T> {
        let status = self.child.wait();
        let _ = self.feeder.join();
        let errors = self.errors.join().unwrap_or_default();
        let fail = |detail: String| {
            let errors = errors.trim();
            failed(
                what,
                if errors.is_empty() {
                    detail
                } else {
                    errors.to_string()
                },
            )
        };
        match (status, read) {
            (Ok(status), Ok(value)) if status.success() => Ok(value),
            (Ok(status), Ok(_)) => Err(fail(status.to_string())),
            (Ok(status), Err(error @ DraftlineError::GitFilterFailed { .. }))
                if status.success() =>
            {
                Err(error)
            }
            (Ok(_), Err(error)) => Err(fail(error.to_string())),
            (Err(error), _) => Err(fail(error.to_string())),
        }
    }
}

/// Runs each file in `paths` through its clean filter, as `git add` would,
/// returning the blob id to commit for it. With `write`, the blobs (and any
/// LFS content) are stored; without, they are only hashed.
pub(crate) fn clean(workdir: &Path, paths: &[PathBuf], write: bool) -> Result<Vec<Oid>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend(git_path(path)?.into_bytes());
        input.push(b'\n');
    }
    let mut command = git(workdir);
    command.args(["hash-object", "--stdin-paths"]);
    if write {
        command.arg("-w");
    }
    let mut running = Running::spawn(command, input)?;
    let mut stdout = running.stdout();
    let read = (|| {
        let mut ids = Vec::with_capacity(paths.len());
        let mut line = String::new();
        while stdout.read_line(&mut line)? > 0 {
            ids.push(Oid::from_str(line.trim())?);
            line.clear();
        }
        if ids.len() != paths.len() {
            return Err(failed(
                "running a Git filter to save files",
                format!("git hashed {} of {} files", ids.len(), paths.len()),
            ));
        }
        Ok(ids)
    })();
    running.finish("running a Git filter to save files", read)
}

/// Runs each `(blob, path)` through `path`'s smudge filter, as a checkout
/// would, handing `write` the bytes for the working file.
pub(crate) fn smudge(
    workdir: &Path,
    entries: &[(Oid, PathBuf)],
    mut write: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    // One process per file: `cat-file --batch --filters` headers give the
    // unfiltered size, so its output can't be split reliably.
    for (id, path) in entries {
        let mut command = git(workdir);
        command
            .args(["cat-file", "--filters"])
            .arg(format!("--path={}", git_path(path)?))
            .arg(id.to_string());
        let mut running = Running::spawn(command, Vec::new())?;
        let mut stdout = running.stdout();
        let mut content = Vec::new();
        let read = stdout
            .read_to_end(&mut content)
            .map(|_| ())
            .map_err(DraftlineError::from);
        drop(stdout);
        running.finish("running a Git filter to write files", read)?;
        write(path, &content)?;
    }
    Ok(())
}

/// Lets Git refresh the index's file timestamps through its filters, so a
/// filtered file whose content matches its entry stops looking changed.
/// Best effort: a busy index just means a slower status later.
pub(crate) fn refresh_index(workdir: &Path) {
    let _ = git(workdir)
        .args(["update-index", "-q", "--refresh"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Uploads the Git LFS content `commit` needs that `remote` lacks, as Git's
/// pre-push hook would.
pub(crate) fn lfs_push(workdir: &Path, remote: &str, commit: Oid) -> Result<()> {
    if remote.starts_with('-') {
        return Err(failed(
            &format!("uploading Git LFS content to `{remote}`"),
            "a remote name starting with `-` cannot be passed to git lfs".to_string(),
        ));
    }
    let mut command = git(workdir);
    command.args(["lfs", "push", remote, &commit.to_string()]);
    let mut running = Running::spawn(command, Vec::new())?;
    let read = std::io::copy(&mut running.stdout(), &mut std::io::sink())
        .map(|_| ())
        .map_err(DraftlineError::from);
    running.finish(&format!("uploading Git LFS content to `{remote}`"), read)
}
