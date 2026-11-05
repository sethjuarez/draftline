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
/// Git refreshes a private copy of the index, which replaces the real one
/// (under Git's own `index.lock`) only if nothing changed it meanwhile, so a
/// refresh stopped at `timeout` never leaves or removes a lock in
/// `git_dir`. Best effort: a busy index or a stopped filter just means a
/// slower status later.
pub(crate) fn refresh_index(workdir: &Path, git_dir: &Path, timeout: Option<Duration>) {
    let index = git_dir.join("index");
    let Ok(before) = std::fs::read(&index) else {
        return;
    };
    let scratch = git_dir.join("draftline").join(format!(
        "refresh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    if let Err(error) = refresh_copy(workdir, &index, &scratch, &before, timeout) {
        tracing::debug!(%error, "index refresh did not finish");
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

fn refresh_copy(
    workdir: &Path,
    index: &Path,
    scratch: &Path,
    before: &[u8],
    timeout: Option<Duration>,
) -> Result<()> {
    std::fs::create_dir_all(scratch)?;
    let copy = scratch.join("index");
    std::fs::write(&copy, before)?;
    let mut command = git(workdir);
    command
        .env("GIT_INDEX_FILE", &copy)
        .args(["update-index", "-q", "--refresh"]);
    let action = "refreshing the index through Git filters";
    let stopped = Stopped::default();
    let finished = Running::spawn(command, Vec::new())?.finish(
        action,
        Deadline::filter(timeout, action, &stopped),
        drain,
    );
    if stopped.get() {
        return finished;
    }
    // `-q` still writes what it could refresh when it reports a failure.
    let refreshed = std::fs::read(&copy)?;
    if refreshed != before {
        install_index(index, before, &refreshed)?;
    }
    finished
}

/// Replaces `index` with `refreshed` as Git would, through `index.lock`,
/// unless another process holds the lock or `index` no longer reads `before`.
fn install_index(index: &Path, before: &[u8], refreshed: &[u8]) -> Result<()> {
    let lock = index.with_file_name("index.lock");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    else {
        return Ok(());
    };
    let written = (|| -> Result<bool> {
        if std::fs::read(index)? != before {
            return Ok(false);
        }
        file.write_all(refreshed)?;
        file.sync_all()?;
        Ok(true)
    })();
    drop(file);
    match written {
        Ok(true) => std::fs::rename(&lock, index).map_err(|error| {
            let _ = std::fs::remove_file(&lock);
            error.into()
        }),
        Ok(false) => {
            let _ = std::fs::remove_file(&lock);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&lock);
            Err(error)
        }
    }
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

/// What [`clone_blobless`] clones.
pub(crate) struct BloblessClone<'a> {
    pub url: &'a str,
    pub path: &'a Path,
    pub branch: Option<&'a str>,
    /// A username and password (or token) for an HTTPS remote, handed to
    /// `git` through a credential helper that lives only in its environment.
    pub login: Option<(String, String)>,
    /// How long `git` may go without reporting anything before it is stopped.
    pub idle_timeout: Option<Duration>,
}

/// How a `git clone` failed.
pub(crate) enum CloneFailure {
    /// The remote has no such branch.
    BranchNotFound,
    /// `git` went quiet for longer than the idle timeout and was stopped.
    TimedOut,
    Failed(String),
}

/// One progress line from `git clone --progress`: its stage, `completed` of
/// `total`, and the bytes received so far when the line says.
pub(crate) type CloneProgressLine = (crate::CloneStage, u64, u64, Option<u64>);

/// Clones `clone.url` without file contents (`--filter=blob:none`): every
/// commit and tree, but only the blobs the checkout needs; `git` downloads
/// the rest when something reads them. Never shallow. Progress lines go to
/// `progress` on the calling thread. A missing target folder is created
/// first, so it's ours; a failed clone removes it. Like `git` and libgit2, a
/// failed clone into a folder that was empty empties it again; it touches
/// nothing in a folder it couldn't prove empty.
pub(crate) fn clone_blobless(
    clone: &BloblessClone<'_>,
    mut progress: impl FnMut(CloneProgressLine),
) -> std::result::Result<(), CloneFailure> {
    let target = CloneTarget::claim(clone.path);
    let mut command = git(Path::new("."));
    // Untranslated output, so progress and errors can be read. No prompts:
    // an empty GIT_ASKPASS skips every askpass program.
    command
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "")
        .env_remove("SSH_ASKPASS");
    only_credentials(
        &mut command,
        clone
            .login
            .as_ref()
            .map(|(username, password)| (username.as_str(), password.as_str())),
    );
    command.args(["clone", "--progress", "--filter=blob:none", "--no-local"]);
    if let Some(branch) = clone.branch {
        command.args(["--branch", branch]);
    }
    command.arg("--").arg(clone.url).arg(clone.path);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CloneFailure::Failed(format!("starting git: {error}")))?;
    let stderr = child.stderr.take().expect("piped stderr");
    let (sender, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while let Ok(1) = reader.read(&mut byte) {
            if byte[0] == b'\r' || byte[0] == b'\n' {
                if !line.is_empty() {
                    let text = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if sender.send(text).is_err() {
                        return;
                    }
                }
            } else {
                line.push(byte[0]);
            }
        }
        if !line.is_empty() {
            let _ = sender.send(String::from_utf8_lossy(&line).into_owned());
        }
    });
    let mut messages: Vec<String> = Vec::new();
    let mut heard = Instant::now();
    let outcome = loop {
        match lines.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                heard = Instant::now();
                match parse_clone_progress(&line) {
                    Some(line) => progress(line),
                    None => {
                        if !line.starts_with("remote:") {
                            messages.push(line);
                        }
                    }
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // stderr closed, but git (or a filter it runs) may still be
                // running, so keep the idle timeout.
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(error) => {
                        kill_tree(&mut child);
                        break Err(CloneFailure::Failed(error.to_string()));
                    }
                }
            }
        }
        if clone
            .idle_timeout
            .is_some_and(|after| heard.elapsed() >= after)
        {
            kill_tree(&mut child);
            break Err(CloneFailure::TimedOut);
        }
    };
    let result = match outcome {
        Ok(status) if status.success() => return Ok(()),
        Ok(status) => {
            let text = messages.join("\n");
            if text.contains("Remote branch") && text.contains("not found") {
                Err(CloneFailure::BranchNotFound)
            } else if text.trim().is_empty() {
                Err(CloneFailure::Failed(format!("git clone failed: {status}")))
            } else {
                Err(CloneFailure::Failed(text))
            }
        }
        Err(failure) => Err(failure),
    };
    target.clean_up(clone.path);
    result
}

/// What was at a clone's target before the clone started.
enum CloneTarget {
    /// Missing, so created here: nobody else's.
    Created,
    Empty,
    /// Not empty, not a folder, or unreadable: never cleaned up.
    Other,
}

impl CloneTarget {
    /// Creates `path` when it's missing (atomically, so a folder something
    /// else creates meanwhile is never taken for ours).
    fn claim(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::create_dir(path).is_ok() {
            return Self::Created;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => match std::fs::read_dir(path) {
                Ok(mut entries) => match entries.next() {
                    None => Self::Empty,
                    Some(_) => Self::Other,
                },
                Err(_) => Self::Other,
            },
            _ => Self::Other,
        }
    }

    /// Removes what a failed clone left in `path`.
    fn clean_up(&self, path: &Path) {
        match self {
            Self::Created => {
                let _ = std::fs::remove_dir_all(path);
            }
            Self::Empty => remove_clone_leftovers(path),
            Self::Other => {}
        }
    }
}

/// Turns off the configured credential helpers for `command`, so it neither
/// reads nor stores a login the host didn't hand over. With a `login`, adds
/// one helper that answers `get` with it, from variables only this `git`
/// sees, and ignores `store` and `erase`.
fn only_credentials(command: &mut Command, login: Option<(&str, &str)>) {
    command
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "");
    let Some((username, password)) = login else {
        command.env("GIT_CONFIG_COUNT", "1");
        return;
    };
    command
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env(
            "GIT_CONFIG_VALUE_1",
            "!f() { if test \"$1\" = get; then printf 'username=%s\\npassword=%s\\n' \"$DRAFTLINE_GIT_USERNAME\" \"$DRAFTLINE_GIT_PASSWORD\"; fi; }; f",
        )
        .env("DRAFTLINE_GIT_USERNAME", username)
        .env("DRAFTLINE_GIT_PASSWORD", password);
}

/// Empties a folder a failed clone wrote into.
fn remove_clone_leftovers(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let target = entry.path();
        let _ = if target.is_dir() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
    }
}

/// Reads a `git clone --progress` line such as
/// `Receiving objects:  45% (450/1000), 1.20 MiB | 2.00 MiB/s`.
fn parse_clone_progress(line: &str) -> Option<CloneProgressLine> {
    use crate::CloneStage;
    let (stage, rest) = [
        ("Receiving objects:", CloneStage::Receiving),
        ("Resolving deltas:", CloneStage::Resolving),
        ("Updating files:", CloneStage::CheckingOut),
    ]
    .into_iter()
    .find_map(|(prefix, stage)| line.strip_prefix(prefix).map(|rest| (stage, rest)))?;
    let open = rest.find('(')?;
    let close = open + rest[open..].find(')')?;
    let (completed, total) = rest[open + 1..close].split_once('/')?;
    let completed = completed.trim().parse().ok()?;
    let total = total.trim().parse().ok()?;
    let bytes = rest[close + 1..]
        .trim_start_matches(',')
        .split('|')
        .next()
        .and_then(parse_size);
    Some((stage, completed, total, bytes))
}

/// `1.20 MiB` and the like, as bytes.
fn parse_size(text: &str) -> Option<u64> {
    let (number, unit) = text.trim().split_once(' ')?;
    let number: f64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "bytes" | "byte" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * scale) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_reaches_git_only_through_its_own_helper() {
        if !available() {
            return;
        }
        let mut command = git(Path::new("."));
        only_credentials(&mut command, Some(("x-access-token", "s3cret value")));
        command.args(["credential", "fill"]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=example.test\n\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{text}");
        assert!(text.contains("username=x-access-token"), "{text}");
        assert!(text.contains("password=s3cret value"), "{text}");
    }

    /// Runs `git credential <action>` with a sentinel helper in the global
    /// config; returns whether it succeeded and what the sentinel saw.
    fn credential_with_sentinel(
        action: &str,
        reset: bool,
        login: Option<(&str, &str)>,
    ) -> (bool, String) {
        let temp = tempfile::tempdir().unwrap();
        let seen = temp.path().join("seen.txt");
        let global = temp.path().join("gitconfig");
        let seen_path = seen.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &global,
            format!(
                "[credential]\n\thelper = \"!f() {{ echo \\\"$1\\\" >> '{seen_path}'; }}; f\"\n"
            ),
        )
        .unwrap();
        let mut command = git(temp.path());
        command
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ASKPASS", "")
            .env_remove("SSH_ASKPASS");
        if reset {
            only_credentials(&mut command, login);
        }
        command.args(["credential", action]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=example.test\nusername=u\npassword=p\n\n")
            .unwrap();
        let ok = child.wait().unwrap().success();
        (ok, std::fs::read_to_string(&seen).unwrap_or_default())
    }

    #[test]
    fn configured_credential_helpers_never_see_a_clone() {
        if !available() {
            return;
        }
        // Without the reset, the sentinel is called.
        let (_, seen) = credential_with_sentinel("approve", false, None);
        assert_eq!(seen.trim(), "store");
        for action in ["fill", "approve", "reject"] {
            let (_, seen) = credential_with_sentinel(action, true, None);
            assert_eq!(seen, "", "{action} reached the configured helper");
            let (ok, seen) = credential_with_sentinel(action, true, Some(("u", "p")));
            assert!(ok, "{action} with a login failed");
            assert_eq!(
                seen, "",
                "{action} with a login reached the configured helper"
            );
        }
    }

    #[test]
    fn clone_progress_lines_are_read() {
        use crate::CloneStage;
        assert_eq!(
            parse_clone_progress("Receiving objects:  45% (450/1000), 1.50 MiB | 2.00 MiB/s"),
            Some((CloneStage::Receiving, 450, 1000, Some(1_572_864)))
        );
        assert_eq!(
            parse_clone_progress("Resolving deltas: 100% (7/7), done."),
            Some((CloneStage::Resolving, 7, 7, None))
        );
        assert_eq!(
            parse_clone_progress("Updating files:  50% (20/40)"),
            Some((CloneStage::CheckingOut, 20, 40, None))
        );
        assert_eq!(
            parse_clone_progress("remote: Counting objects: 3, done."),
            None
        );
        assert_eq!(parse_clone_progress("Cloning into 'x'..."), None);
    }

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

    /// An index refresh whose filter hangs stops at the timeout.
    #[test]
    fn a_hung_refresh_is_stopped_at_the_timeout() {
        if !available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let run = |args: &[&str]| assert!(git(root).args(args).status().unwrap().success());
        run(&["init", "-q"]);
        std::fs::write(root.join("a.bin"), "content").unwrap();
        run(&["add", "a.bin"]);
        run(&["config", "filter.slow.clean", "sleep 60; cat"]);
        std::fs::write(root.join(".gitattributes"), "*.bin filter=slow\n").unwrap();
        // A changed stat makes the refresh run the clean filter.
        std::fs::write(root.join("a.bin"), "changed").unwrap();

        let started = Instant::now();
        refresh_index(root, &root.join(".git"), Some(Duration::from_millis(500)));

        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        // The stopped refresh leaves the index unlocked for the next writer.
        assert!(!root.join(".git/index.lock").exists());
    }

    #[test]
    fn a_refresh_replaces_the_index_only_when_its_lock_is_free() {
        if !available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git_dir = root.join(".git");
        let run = |args: &[&str]| assert!(git(root).args(args).status().unwrap().success());
        run(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), "content").unwrap();
        run(&["add", "a.txt"]);
        let stale = std::fs::read(git_dir.join("index")).unwrap();
        // Same content, new stat: only a refresh updates the entry.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(root.join("a.txt"), "content").unwrap();

        std::fs::write(git_dir.join("index.lock"), "").unwrap();
        refresh_index(root, &git_dir, None);
        assert_eq!(std::fs::read(git_dir.join("index")).unwrap(), stale);
        assert!(git_dir.join("index.lock").exists());

        std::fs::remove_file(git_dir.join("index.lock")).unwrap();
        refresh_index(root, &git_dir, None);
        assert_ne!(std::fs::read(git_dir.join("index")).unwrap(), stale);
        assert!(!git_dir.join("index.lock").exists());
        assert!(!git_dir.join("draftline").read_dir().unwrap().any(|_| true));
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
