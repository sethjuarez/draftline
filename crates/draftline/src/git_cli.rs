//! The `git` executable, for what libgit2 cannot do: run filter drivers such
//! as Git LFS. Draftline only calls it for files whose driver the repository
//! has configured; everything else stays in libgit2.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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
pub(crate) fn git_path(path: &Path) -> Result<String> {
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
    errors: Receiver<String>,
}

/// How long a `git` (and whatever holds its output pipes) may run, and the
/// error for one that runs longer.
struct Deadline {
    after: Duration,
    error: Box<dyn FnOnce(Duration) -> DraftlineError>,
}

/// Whether [`Running::finish`] stopped `git` at its deadline.
type Stopped = std::rc::Rc<std::cell::Cell<bool>>;

impl Deadline {
    /// A filter that runs longer than `after` fails as [`failed`], `action`,
    /// and sets `stopped`.
    fn filter(after: Option<Duration>, action: &str, stopped: &Stopped) -> Option<Self> {
        let action = action.to_string();
        let stopped = stopped.clone();
        Some(Self {
            after: after?,
            error: Box::new(move |after| {
                stopped.set(true);
                failed(
                    &action,
                    format!(
                        "git did not finish within {} ms and was stopped",
                        after.as_millis()
                    ),
                )
            }),
        })
    }
}

impl Running {
    fn spawn(mut command: Command, input: Vec<u8>) -> Result<Self> {
        // Its own process group, so a timeout stops the filters it starts too.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| failed("starting git", error.to_string()))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // Its result shows up as git's own failure, so nothing joins it.
        std::thread::spawn(move || stdin.write_all(&input));
        let stderr: ChildStderr = child.stderr.take().expect("piped stderr");
        let (sender, errors) = mpsc::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            let _ = sender.send(text);
        });
        Ok(Self { child, errors })
    }

    /// Hands `git`'s stdout to `read` and waits for both, turning a failure
    /// (or `read`'s) into its stderr text. Past `deadline` (which also covers
    /// a process `git` started that keeps its pipes open after it exits),
    /// `git` and every process it started are killed, and the threads still
    /// reading its pipes are left to end on their own.
    fn finish<T: Send + 'static>(
        mut self,
        what: &str,
        deadline: Option<Deadline>,
        read: impl FnOnce(BufReader<ChildStdout>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let stdout = BufReader::new(self.child.stdout.take().expect("piped stdout"));
        let (sender, reader) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read(stdout));
        });
        let lost = || failed(what, "reading git's output failed".to_string());
        let (status, read, errors) = match deadline {
            None => {
                let status = self.child.wait();
                let read = reader.recv().unwrap_or_else(|_| Err(lost()));
                (status, read, self.errors.recv().unwrap_or_default())
            }
            Some(deadline) => {
                let ends = Instant::now() + deadline.after;
                let left = || ends.saturating_duration_since(Instant::now());
                let finished = wait_until(&mut self.child, deadline.after).and_then(|status| {
                    let read = match reader.recv_timeout(left()) {
                        Ok(read) => read,
                        Err(mpsc::RecvTimeoutError::Timeout) => return None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => Err(lost()),
                    };
                    let errors = match self.errors.recv_timeout(left()) {
                        Ok(errors) => errors,
                        Err(mpsc::RecvTimeoutError::Timeout) => return None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => String::new(),
                    };
                    Some((status, read, errors))
                });
                match finished {
                    Some(finished) => finished,
                    None => {
                        kill_tree(&mut self.child);
                        return Err((deadline.error)(deadline.after));
                    }
                }
            }
        };
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

/// `child`'s exit status, or `None` once `after` passes first.
fn wait_until(
    child: &mut Child,
    after: Duration,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(Ok(status)),
            Ok(None) if started.elapsed() >= after => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Some(Err(error)),
        }
    }
}

/// Kills `child` and the processes it started (a filter such as git-lfs).
fn kill_tree(child: &mut Child) {
    let id = child.id().to_string();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &id])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{id}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs each file in `paths` through its clean filter, as `git add` would,
/// returning the blob id to commit for it. With `write`, the blobs (and any
/// LFS content) are stored; without, they are only hashed.
pub(crate) fn clean(
    workdir: &Path,
    paths: &[PathBuf],
    write: bool,
    timeout: Option<Duration>,
) -> Result<Vec<Oid>> {
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
    let action = "running a Git filter to save files";
    let expected = paths.len();
    Running::spawn(command, input)?.finish(
        action,
        Deadline::filter(timeout, action, &Stopped::default()),
        move |mut stdout| {
            let mut ids = Vec::with_capacity(expected);
            let mut line = String::new();
            while stdout.read_line(&mut line)? > 0 {
                ids.push(Oid::from_str(line.trim())?);
                line.clear();
            }
            if ids.len() != expected {
                return Err(failed(
                    action,
                    format!("git hashed {} of {expected} files", ids.len()),
                ));
            }
            Ok(ids)
        },
    )
}

/// Runs `paths` from the index file `index` (not the repository's, which
/// another Git process may change meanwhile) through their smudge filters in
/// one `git checkout-index`, so a long-running filter such as
/// `git-lfs filter-process` serves every file. Git writes into `scratch`,
/// never the working tree; `write` gets each file Git produced. A file Git
/// couldn't produce is skipped, and the failure is returned after the rest
/// are written. Past `timeout`, nothing is written, since a stopped filter
/// may have left a file half-written.
pub(crate) fn smudge(
    workdir: &Path,
    index: &Path,
    scratch: &Path,
    paths: &[PathBuf],
    timeout: Option<Duration>,
    mut write: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(git_path(path)?.as_bytes());
        input.push(0);
    }
    std::fs::create_dir_all(scratch)?;
    let prefix = format!("{}/", scratch.to_string_lossy().replace('\\', "/"));
    let mut command = git(workdir);
    command
        .env("GIT_INDEX_FILE", index)
        .args(["checkout-index", "-f", "-z"])
        .arg(format!("--prefix={prefix}"))
        .arg("--stdin");
    let action = "running a Git filter to write files";
    let stopped = Stopped::default();
    let finished = Running::spawn(command, input)?.finish(
        action,
        Deadline::filter(timeout, action, &stopped),
        drain,
    );
    if stopped.get() {
        return finished;
    }
    for path in paths {
        match std::fs::read(scratch.join(path)) {
            Ok(content) => write(path, &content)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    finished
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

fn drain(mut stdout: BufReader<ChildStdout>) -> Result<()> {
    std::io::copy(&mut stdout, &mut std::io::sink())?;
    Ok(())
}

/// Uploads the Git LFS content `commit` needs that `remote` lacks, as Git's
/// pre-push hook would. Past `timeout`, the upload is stopped and fails as
/// [`DraftlineError::RemoteOperationTimedOut`].
pub(crate) fn lfs_push(
    workdir: &Path,
    remote: &str,
    commit: Oid,
    timeout: Option<Duration>,
) -> Result<()> {
    if remote.starts_with('-') {
        return Err(failed(
            &format!("uploading Git LFS content to `{remote}`"),
            "a remote name starting with `-` cannot be passed to git lfs".to_string(),
        ));
    }
    let mut command = git(workdir);
    command.args(["lfs", "push", remote, &commit.to_string()]);
    let deadline = timeout.map(|after| Deadline {
        after,
        error: Box::new(|after| DraftlineError::RemoteOperationTimedOut {
            operation: "upload_lfs_content".to_string(),
            timeout_ms: u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
        }),
    });
    Running::spawn(command, Vec::new())?.finish(
        &format!("uploading Git LFS content to `{remote}`"),
        deadline,
        drain,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filter that hangs (here, a shell that starts a `sleep`) is killed,
    /// with what it started, once the timeout passes.
    #[test]
    fn a_hung_filter_is_stopped_at_the_timeout() {
        if !available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let run = |args: &[&str]| assert!(git(root).args(args).status().unwrap().success());
        run(&["init", "-q"]);
        run(&["config", "filter.slow.clean", "sleep 60; cat"]);
        std::fs::write(root.join(".gitattributes"), "*.bin filter=slow\n").unwrap();
        std::fs::write(root.join("a.bin"), "content").unwrap();

        let started = Instant::now();
        let error = clean(
            root,
            &[PathBuf::from("a.bin")],
            false,
            Some(Duration::from_millis(500)),
        )
        .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        match error {
            DraftlineError::GitFilterFailed { action, output } => {
                assert_eq!(action, "running a Git filter to save files");
                assert!(output.contains("500 ms"), "{output}");
            }
            error => panic!("{error:?}"),
        }
    }

    /// A smudge stopped at its timeout writes nothing, since the filter may
    /// have left a file half-written.
    #[test]
    fn a_smudge_stopped_at_the_timeout_writes_nothing() {
        if !available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let run = |args: &[&str]| assert!(git(root).args(args).status().unwrap().success());
        run(&["init", "-q"]);
        std::fs::write(root.join("a.bin"), "content").unwrap();
        run(&["add", "a.bin"]);
        run(&["config", "filter.slow.smudge", "printf partial; sleep 60"]);
        std::fs::write(root.join(".gitattributes"), "*.bin filter=slow\n").unwrap();

        let mut written = Vec::new();
        let error = smudge(
            root,
            &root.join(".git").join("index"),
            &root.join("scratch"),
            &[PathBuf::from("a.bin")],
            Some(Duration::from_millis(500)),
            |path, _| {
                written.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap_err();

        assert!(written.is_empty(), "{written:?}");
        assert!(
            matches!(&error, DraftlineError::GitFilterFailed { output, .. } if output.contains("500 ms")),
            "{error:?}"
        );
    }
}
