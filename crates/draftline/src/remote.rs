use std::path::PathBuf;

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::Result;

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
#[derive(Default)]
pub struct RemoteOptions<'callbacks> {
    credentials: Option<Box<RemoteCredentialCallback<'callbacks>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushRefExpectation {
    pub dst_refname: String,
    pub expected_old_oid: Option<String>,
    pub expected_new_oid: Option<String>,
}

type RemoteCredentialCallback<'callbacks> =
    dyn FnMut(RemoteCredentialRequest<'_>) -> Result<RemoteCredential> + 'callbacks;

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

    pub(crate) fn fetch_options(&mut self) -> git2::FetchOptions<'_> {
        let mut options = git2::FetchOptions::new();
        options.remote_callbacks(self.remote_callbacks());
        options
    }

    pub(crate) fn push_options_with_expectations(
        &mut self,
        expectations: Vec<PushRefExpectation>,
    ) -> git2::PushOptions<'_> {
        let mut callbacks = self.remote_callbacks();
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
        self.fetch_options()
    }

    pub(crate) fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    pub(crate) fn remote_callbacks(&mut self) -> git2::RemoteCallbacks<'_> {
        let mut callbacks = git2::RemoteCallbacks::new();

        if let Some(credentials) = self.credentials.as_mut() {
            callbacks.credentials(move |url, username_from_url, allowed| {
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
    use git2::{Repository, Signature};

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
        let mut options = remote_options.push_options_with_expectations(vec![PushRefExpectation {
            dst_refname: "refs/heads/master".to_string(),
            expected_old_oid: None,
            expected_new_oid: Some(second_oid.to_string()),
        }]);

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
