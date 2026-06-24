use std::path::PathBuf;

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

    pub(crate) fn push_options(&mut self) -> git2::PushOptions<'_> {
        let mut options = git2::PushOptions::new();
        options.remote_callbacks(self.remote_callbacks());
        options
    }

    pub(crate) fn clone_fetch_options(&mut self) -> git2::FetchOptions<'_> {
        self.fetch_options()
    }

    pub(crate) fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    fn remote_callbacks(&mut self) -> git2::RemoteCallbacks<'_> {
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

    #[test]
    fn converts_username_password_credentials() {
        let credential = credential_to_git(RemoteCredential::UsernamePassword {
            username: "x-access-token".to_string(),
            password: "token".to_string(),
        })
        .unwrap();

        assert!(credential.has_username());
    }
}
