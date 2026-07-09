use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::{DraftlineError, Result};

/// A configured place where a workspace can be shared or backed up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEndpoint {
    pub name: String,
    pub url: String,
}

/// Collaboration status between the current variation and a remote endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    pub remote: String,
    pub variation: String,
    pub ahead: usize,
    pub behind: usize,
    pub state: SyncState,
    pub incoming: Vec<RemoteVersionSummary>,
}

/// High-level sync state for product workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncState {
    UpToDate,
    LocalAhead,
    IncomingAvailable,
    NeedsMerge,
    NoRemoteVersion,
}

/// Summary of a version available from a remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVersionSummary {
    pub id: String,
    pub label: String,
    pub author: Contributor,
    pub time_seconds: i64,
}

/// Result of publishing local versions to a remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishResult {
    pub remote: String,
    pub variation: String,
    pub published_versions: usize,
}

/// Read-only publish preflight with the expected remote state captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishPreflight {
    pub remote: String,
    pub variation: String,
    pub expected_remote_oid: Option<String>,
    pub local_oid: String,
    pub sync_status: SyncStatus,
    pub token: PublishToken,
    pub can_publish: bool,
}

/// Opaque publish execution token tying publish to a preflighted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishToken {
    pub remote: String,
    pub variation: String,
    pub expected_remote_oid: Option<String>,
    pub local_oid: String,
}

/// Attribution metadata from version history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    pub name: String,
    pub email: Option<String>,
}

/// Credential material returned by a host application for a remote operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCredential {
    /// Ask libgit2 to use its default credential behavior.
    Default,
    /// Authenticate with a username and password or token.
    ///
    /// GitHub HTTPS token flows can use username `x-access-token` and the token
    /// as the password.
    UsernamePassword { username: String, password: String },
    /// Authenticate with an SSH key loaded by the local agent.
    SshAgent { username: String },
    /// Authenticate with an explicit SSH private key.
    SshKey {
        username: String,
        public_key: Option<PathBuf>,
        private_key: PathBuf,
        passphrase: Option<String>,
    },
}

/// Information supplied to a remote credential callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCredentialRequest<'a> {
    pub url: &'a str,
    pub username_from_url: Option<&'a str>,
    pub allows_default: bool,
    pub allows_username_password: bool,
    pub allows_ssh_key: bool,
}

/// Options for remote operations such as clone, fetch, and publish.
pub struct RemoteOptions<'callbacks> {
    credentials: Option<Box<RemoteCredentialCallback<'callbacks>>>,
    timeout: Option<Duration>,
    timed_out: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushRefExpectation {
    pub dst_refname: String,
    pub expected_old_oid: Option<String>,
    pub expected_new_oid: Option<String>,
}

type RemoteCredentialCallback<'callbacks> =
    dyn FnMut(RemoteCredentialRequest<'_>) -> Result<RemoteCredential> + 'callbacks;

static LIBGIT2_TIMEOUT_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
struct RemoteTimeoutDeadline {
    operation: &'static str,
    timeout: Duration,
    started_at: Instant,
}

pub(crate) struct Libgit2TimeoutGuard {
    _lock: MutexGuard<'static, ()>,
    previous_connect_timeout_ms: i32,
    previous_server_timeout_ms: i32,
}

impl Drop for Libgit2TimeoutGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = git2::opts::set_server_connect_timeout_in_milliseconds(
                self.previous_connect_timeout_ms,
            );
            let _ = git2::opts::set_server_timeout_in_milliseconds(self.previous_server_timeout_ms);
        }
    }
}

impl RemoteTimeoutDeadline {
    fn new(operation: &'static str, timeout: Duration) -> Self {
        Self {
            operation,
            timeout,
            started_at: Instant::now(),
        }
    }

    fn is_expired(self) -> bool {
        self.started_at.elapsed() >= self.timeout
    }

    fn mark_if_expired(self, timed_out: &AtomicBool) -> bool {
        if self.is_expired() {
            timed_out.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn check(self, timed_out: &AtomicBool) -> std::result::Result<(), git2::Error> {
        if self.mark_if_expired(timed_out) {
            Err(git2::Error::new(
                git2::ErrorCode::Timeout,
                git2::ErrorClass::Net,
                format!(
                    "remote operation timed out after {}ms while running {}",
                    duration_millis_u64(self.timeout),
                    self.operation
                ),
            ))
        } else {
            Ok(())
        }
    }
}

impl Default for RemoteOptions<'_> {
    fn default() -> Self {
        Self {
            credentials: None,
            timeout: None,
            timed_out: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<'callbacks> RemoteOptions<'callbacks> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supplies credentials to clone, fetch, and publish operations.
    ///
    /// ```no_run
    /// use draftline::{RemoteCredential, RemoteOptions};
    ///
    /// let token = std::env::var("GITHUB_TOKEN").unwrap();
    /// let mut options = RemoteOptions::new().with_credentials(move |request| {
    ///     if request.allows_username_password {
    ///         Ok(RemoteCredential::UsernamePassword {
    ///             username: "x-access-token".to_string(),
    ///             password: token.clone(),
    ///         })
    ///     } else {
    ///         Ok(RemoteCredential::Default)
    ///     }
    /// });
    /// # let _ = &mut options;
    /// # Ok::<(), draftline::DraftlineError>(())
    /// ```
    pub fn with_credentials(
        mut self,
        callback: impl FnMut(RemoteCredentialRequest<'_>) -> Result<RemoteCredential> + 'callbacks,
    ) -> Self {
        self.credentials = Some(Box::new(callback));
        self
    }

    /// Bounds libgit2 network operations with a native socket timeout.
    ///
    /// The timeout applies to connection setup, socket reads/writes, and Draftline's
    /// progress/credential callbacks. Hosts should still keep credential callbacks
    /// fast because Rust cannot preempt a synchronous foreign callback once entered.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub(crate) fn fetch_options(&mut self, operation: &'static str) -> git2::FetchOptions<'_> {
        let mut options = git2::FetchOptions::new();
        options.remote_callbacks(self.remote_callbacks(operation));
        options
    }

    pub(crate) fn push_options_with_expectations(
        &mut self,
        operation: &'static str,
        expectations: Vec<PushRefExpectation>,
    ) -> git2::PushOptions<'_> {
        let mut callbacks = self.remote_callbacks(operation);
        callbacks.push_negotiation(move |updates| {
            if updates.len() != expectations.len() {
                return Err(git2::Error::from_str(
                    "push negotiated unexpected ref updates",
                ));
            }

            for expectation in &expectations {
                let Some(update) = updates
                    .iter()
                    .find(|update| update.dst_refname() == Some(expectation.dst_refname.as_str()))
                else {
                    return Err(git2::Error::from_str(&format!(
                        "push did not negotiate expected ref {}",
                        expectation.dst_refname
                    )));
                };

                if oid_to_option(update.src()) != expectation.expected_old_oid {
                    return Err(git2::Error::from_str(&format!(
                        "remote ref {} did not match expected old oid",
                        expectation.dst_refname
                    )));
                }

                if oid_to_option(update.dst()) != expectation.expected_new_oid {
                    return Err(git2::Error::from_str(&format!(
                        "remote ref {} did not match expected new oid",
                        expectation.dst_refname
                    )));
                }
            }
            Ok(())
        });

        let mut options = git2::PushOptions::new();
        options.remote_callbacks(callbacks);
        options
    }

    pub(crate) fn clone_fetch_options(&mut self) -> git2::FetchOptions<'_> {
        self.fetch_options("clone_workspace")
    }

    pub(crate) fn has_network_callbacks(&self) -> bool {
        self.credentials.is_some() || self.timeout.is_some()
    }

    pub(crate) fn server_timeout_guard(&self) -> Result<Option<Libgit2TimeoutGuard>> {
        let Some(timeout) = self.timeout else {
            return Ok(None);
        };
        let timeout_ms = duration_millis_i32(timeout);
        let lock = LIBGIT2_TIMEOUT_MUTEX.lock().map_err(|_| {
            DraftlineError::Git(git2::Error::from_str(
                "libgit2 timeout guard mutex was poisoned",
            ))
        })?;
        let previous_connect_timeout_ms =
            unsafe { git2::opts::get_server_connect_timeout_in_milliseconds()? };
        let previous_server_timeout_ms =
            unsafe { git2::opts::get_server_timeout_in_milliseconds()? };
        unsafe {
            git2::opts::set_server_connect_timeout_in_milliseconds(timeout_ms)?;
            git2::opts::set_server_timeout_in_milliseconds(timeout_ms)?;
        }

        Ok(Some(Libgit2TimeoutGuard {
            _lock: lock,
            previous_connect_timeout_ms,
            previous_server_timeout_ms,
        }))
    }

    pub(crate) fn map_git_result<T>(
        &self,
        operation: &'static str,
        result: std::result::Result<T, git2::Error>,
    ) -> Result<T> {
        result.map_err(|error| {
            if self.timed_out.load(Ordering::SeqCst) || git_error_is_timeout(&error) {
                self.timeout_error(operation)
            } else {
                error.into()
            }
        })
    }

    pub(crate) fn timeout_error(&self, operation: &'static str) -> DraftlineError {
        DraftlineError::RemoteOperationTimedOut {
            operation: operation.to_string(),
            timeout_ms: self.timeout.map(duration_millis_u64).unwrap_or_default(),
        }
    }

    pub(crate) fn remote_callbacks(
        &mut self,
        operation: &'static str,
    ) -> git2::RemoteCallbacks<'_> {
        let mut callbacks = git2::RemoteCallbacks::new();
        self.timed_out.store(false, Ordering::SeqCst);
        let deadline = self
            .timeout
            .map(|timeout| RemoteTimeoutDeadline::new(operation, timeout));

        if let Some(deadline) = deadline {
            let timed_out = Arc::clone(&self.timed_out);
            callbacks.transfer_progress(move |_| !deadline.mark_if_expired(&timed_out));

            let timed_out = Arc::clone(&self.timed_out);
            callbacks.sideband_progress(move |_| !deadline.mark_if_expired(&timed_out));

            let timed_out = Arc::clone(&self.timed_out);
            callbacks.update_tips(move |_, _, _| !deadline.mark_if_expired(&timed_out));
        }

        if let Some(credentials) = self.credentials.as_mut() {
            let timed_out = Arc::clone(&self.timed_out);
            callbacks.credentials(move |url, username_from_url, allowed| {
                if let Some(deadline) = deadline {
                    deadline.check(&timed_out)?;
                }
                let request = RemoteCredentialRequest {
                    url,
                    username_from_url,
                    allows_default: allowed.contains(git2::CredentialType::DEFAULT),
                    allows_username_password: allowed
                        .contains(git2::CredentialType::USER_PASS_PLAINTEXT),
                    allows_ssh_key: allowed.contains(git2::CredentialType::SSH_KEY),
                };

                credentials(request)
                    .and_then(credential_to_git)
                    .map_err(|error| git2::Error::from_str(&error.to_string()))
            });
        }

        callbacks
    }
}

fn duration_millis_i32(duration: Duration) -> i32 {
    duration.as_millis().clamp(1, i32::MAX as u128) as i32
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn git_error_is_timeout(error: &git2::Error) -> bool {
    error.code() == git2::ErrorCode::Timeout
        || error.message().to_ascii_lowercase().contains("timed out")
        || error.message().to_ascii_lowercase().contains("timeout")
}

pub(crate) fn ensure_supported_remote_url(url: &str) -> Result<()> {
    let version = git2::Version::get();
    ensure_supported_remote_url_with_features(url, version.https(), version.ssh())
}

fn ensure_supported_remote_url_with_features(url: &str, https: bool, ssh: bool) -> Result<()> {
    let unsupported = if url.starts_with("https://") && !https {
        Some(("https", "https"))
    } else if (url.starts_with("ssh://") || is_scp_like_ssh_url(url)) && !ssh {
        Some(("ssh", "ssh"))
    } else {
        None
    };

    if let Some((scheme, required_feature)) = unsupported {
        return Err(DraftlineError::UnsupportedRemoteTransport {
            scheme: scheme.to_string(),
            required_feature,
        });
    }

    Ok(())
}

fn is_scp_like_ssh_url(url: &str) -> bool {
    let Some(at) = url.find('@') else {
        return false;
    };
    let Some(colon) = url[at + 1..].find(':').map(|offset| at + 1 + offset) else {
        return false;
    };
    !url[..colon].contains('/')
}

fn oid_to_option(oid: Oid) -> Option<String> {
    if oid.is_zero() {
        None
    } else {
        Some(oid.to_string())
    }
}

fn credential_to_git(credential: RemoteCredential) -> Result<git2::Cred> {
    match credential {
        RemoteCredential::Default => Ok(git2::Cred::default()?),
        RemoteCredential::UsernamePassword { username, password } => {
            Ok(git2::Cred::userpass_plaintext(&username, &password)?)
        }
        RemoteCredential::SshAgent { username } => Ok(git2::Cred::ssh_key_from_agent(&username)?),
        RemoteCredential::SshKey {
            username,
            public_key,
            private_key,
            passphrase,
        } => Ok(git2::Cred::ssh_key(
            &username,
            public_key.as_deref(),
            &private_key,
            passphrase.as_deref(),
        )?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature, Version};

    fn commit_file(repo: &Repository, path: &str, content: &[u8], message: &str) -> Oid {
        let workdir = repo.workdir().unwrap();
        let full_path = workdir.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, content).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let signature = Signature::now("Draftline", "draftline@example.invalid").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap()
    }

    #[test]
    fn converts_username_password_credentials() {
        let credential = credential_to_git(RemoteCredential::UsernamePassword {
            username: "x-access-token".to_string(),
            password: "token".to_string(),
        })
        .unwrap();

        assert!(credential.has_username());
    }

    #[test]
    fn libgit2_build_supports_remote_transports() {
        let version = Version::get();

        assert!(
            version.https(),
            "libgit2 must be built with HTTPS/TLS support for remote fetches"
        );
        assert!(
            version.ssh(),
            "libgit2 must be built with SSH support for remote credentials"
        );
    }

    #[test]
    fn reports_missing_https_transport_without_exposing_remote_url() {
        let error = ensure_supported_remote_url_with_features(
            "https://token@example.test/owner/repo.git",
            false,
            true,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "remote URL uses https, but Draftline/libgit2 was built without https transport support"
        );
    }

    #[test]
    fn reports_missing_ssh_transport_for_scp_like_urls() {
        let error = ensure_supported_remote_url_with_features(
            "git@example.test:owner/repo.git",
            true,
            false,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "remote URL uses ssh, but Draftline/libgit2 was built without ssh transport support"
        );
    }

    #[test]
    fn timeout_deadline_returns_git_timeout_error() {
        let timed_out = AtomicBool::new(false);
        let deadline = RemoteTimeoutDeadline::new("fetch_remote", Duration::ZERO);

        let error = deadline.check(&timed_out).unwrap_err();

        assert_eq!(error.code(), git2::ErrorCode::Timeout);
        assert!(timed_out.load(Ordering::SeqCst));
        assert!(error.message().contains("fetch_remote"));
    }

    #[test]
    fn remote_options_maps_git_timeout_to_draftline_timeout() {
        let options = RemoteOptions::new().with_timeout(Duration::from_millis(25));
        let error = options
            .map_git_result::<()>(
                "fetch_remote",
                Err(git2::Error::new(
                    git2::ErrorCode::Timeout,
                    git2::ErrorClass::Net,
                    "socket timed out",
                )),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            DraftlineError::RemoteOperationTimedOut {
                ref operation,
                timeout_ms: 25
            } if operation == "fetch_remote"
        ));
    }

    #[test]
    fn push_expectations_reject_create_only_when_remote_ref_exists() {
        let remote_dir = tempfile::tempdir().unwrap();
        Repository::init_bare(remote_dir.path()).unwrap();

        let first_dir = tempfile::tempdir().unwrap();
        let first_repo = Repository::init(first_dir.path()).unwrap();
        let first_oid = commit_file(&first_repo, "post.md", b"one", "one");
        let mut first_remote = first_repo
            .remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        first_remote
            .push(&["refs/heads/master:refs/heads/master"], None)
            .unwrap();

        let second_dir = tempfile::tempdir().unwrap();
        let second_repo = Repository::init(second_dir.path()).unwrap();
        let second_oid = commit_file(&second_repo, "post.md", b"two", "two");
        let mut second_remote = second_repo
            .remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        let mut remote_options = RemoteOptions::new();
        let mut options = remote_options.push_options_with_expectations(
            "push_expectations_reject_create_only_when_remote_ref_exists",
            vec![PushRefExpectation {
                dst_refname: "refs/heads/master".to_string(),
                expected_old_oid: None,
                expected_new_oid: Some(second_oid.to_string()),
            }],
        );

        let error = second_remote
            .push(&["refs/heads/master:refs/heads/master"], Some(&mut options))
            .unwrap_err();

        assert!(error.message().contains("did not match expected old oid"));
        let remote_repo = Repository::open_bare(remote_dir.path()).unwrap();
        assert_eq!(
            remote_repo
                .refname_to_id("refs/heads/master")
                .unwrap()
                .to_string(),
            first_oid.to_string()
        );
    }
}
