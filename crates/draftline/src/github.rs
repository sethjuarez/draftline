//! GitHub pull requests for Send (`github` feature): a [`PullRequestProvider`]
//! over the GitHub REST API, plus a read-only look at what a token may do
//! with a repository.
//!
//! The host owns sign-in and stores the token; Draftline only uses the token
//! it's handed, for the calls it makes, and never keeps it. Calls are
//! blocking (Send calls its provider synchronously), so async hosts run them
//! off their runtime, e.g. with `tokio::task::spawn_blocking`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{DraftlineError, PullRequest, PullRequestProvider, PullRequestRequest, Result};

/// The public GitHub API. For GitHub Enterprise Server, see
/// [`GitHubProvider::for_enterprise_remote_url`].
pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";
const DEFAULT_USER_AGENT: &str = "draftline";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Marks a create that failed because the pull request already exists.
const DUPLICATE_PULL_REQUEST: &str = "\u{0}duplicate\u{0}";

/// A repository on GitHub: `owner/repo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubRepo {
    pub owner: String,
    pub repo: String,
}

impl GitHubRepo {
    pub fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    /// `owner/repo`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn from_full_name(full_name: &str) -> Option<Self> {
        let (owner, repo) = full_name.split_once('/')?;
        (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

/// The GitHub repository behind a remote URL: `https://github.com/o/r(.git)`,
/// `git@github.com:o/r(.git)` or `ssh://git@github.com/o/r(.git)`. `None` for
/// any other host.
pub fn parse_github_remote_url(url: &str) -> Option<GitHubRepo> {
    parse_remote_url_on(url, "github.com")
}

/// The repository behind a remote URL on `host` (`github.com`, or a GitHub
/// Enterprise Server host such as `ghe.example.com`), in the same forms as
/// [`parse_github_remote_url`]. A login in the URL is ignored.
pub fn parse_remote_url_on(url: &str, host: &str) -> Option<GitHubRepo> {
    let url = url.trim();
    let path = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("ssh") {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let address = authority
                .rsplit_once('@')
                .map_or(authority, |(_, address)| address);
            let without_port = address.rsplit_once(':').map_or(address, |(name, _)| name);
            if !address.eq_ignore_ascii_case(host) && !without_port.eq_ignore_ascii_case(host) {
                return None;
            }
            path
        }
        // scp-like: `git@host:owner/repo`.
        None => {
            let (login_host, path) = url.split_once(':')?;
            let (_, address) = login_host.rsplit_once('@')?;
            if !address.eq_ignore_ascii_case(host) {
                return None;
            }
            path
        }
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    GitHubRepo::from_full_name(path)
}

/// The API base for a GitHub Enterprise Server `host`: `https://HOST/api/v3`.
/// `host` is a bare host name (with an optional port). In debug builds only,
/// a loopback host (`127.0.0.1:PORT`, `localhost:PORT`) is reached over plain
/// HTTP, for local fakes in tests and drills.
pub fn enterprise_api_base(host: &str) -> Result<String> {
    let host = host.trim();
    let valid = !host.is_empty()
        && host
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-:".contains(character));
    if !valid {
        return Err(DraftlineError::PullRequest(format!(
            "`{host}` is not a GitHub Enterprise Server host name"
        )));
    }
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let loopback = name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost");
    let scheme = if loopback && cfg!(debug_assertions) {
        "http"
    } else {
        "https"
    };
    Ok(format!("{scheme}://{host}/api/v3"))
}

/// What a token may do with a repository, from [`GitHubProvider::repo_access`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubRepoAccess {
    /// The repository, as GitHub names it now (after any rename or transfer).
    pub repository: GitHubRepo,
    pub default_branch: String,
    /// The token may push branches here (and so open pull requests from them).
    pub can_push: bool,
    pub private: bool,
    /// The repository this one is a fork of.
    pub fork_parent: Option<GitHubRepo>,
}

/// Opens and finds pull requests, and reads repository access, through the
/// GitHub REST API with a host-supplied token.
pub struct GitHubProvider {
    client: reqwest::blocking::Client,
    api_base: String,
    user_agent: String,
    repo: GitHubRepo,
    head_owner: Option<String>,
    token: String,
}

impl std::fmt::Debug for GitHubProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubProvider")
            .field("api_base", &self.api_base)
            .field("repo", &self.repo)
            .field("head_owner", &self.head_owner)
            .finish_non_exhaustive()
    }
}

impl GitHubProvider {
    /// A provider for `repo` that authenticates with `token`. Fails with
    /// [`DraftlineError::GitHubUnauthorized`] when the token is empty.
    pub fn new(repo: GitHubRepo, token: impl Into<String>) -> Result<Self> {
        let token = token.into().trim().to_string();
        if token.is_empty() {
            return Err(DraftlineError::GitHubUnauthorized(
                "sign in to GitHub to continue".to_string(),
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| {
                DraftlineError::PullRequest(format!("could not create an HTTP client: {error}"))
            })?;
        Ok(Self {
            client,
            api_base: GITHUB_API_BASE_URL.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            repo,
            head_owner: None,
            token,
        })
    }

    /// A provider for the GitHub repository behind `remote_url`. Fails with
    /// [`DraftlineError::PullRequest`] for a remote that isn't on github.com.
    pub fn for_remote_url(remote_url: &str, token: impl Into<String>) -> Result<Self> {
        let repo = parse_github_remote_url(remote_url).ok_or_else(|| {
            // The URL may carry a login, so it stays out of the message.
            DraftlineError::PullRequest("the remote is not a github.com repository".to_string())
        })?;
        Self::new(repo, token)
    }

    /// A provider for the repository behind `remote_url` on the GitHub
    /// Enterprise Server `host` (see [`enterprise_api_base`]).
    pub fn for_enterprise_remote_url(
        remote_url: &str,
        host: &str,
        token: impl Into<String>,
    ) -> Result<Self> {
        let api_base = enterprise_api_base(host)?;
        let repo = parse_remote_url_on(remote_url, host).ok_or_else(|| {
            DraftlineError::PullRequest(format!("the remote is not a repository on `{host}`"))
        })?;
        Ok(Self::new(repo, token)?.with_api_base(api_base))
    }

    /// Sends requests to another API base. The token goes with every
    /// request, so only pass a base the host trusts (prefer
    /// [`Self::for_enterprise_remote_url`]).
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim().trim_end_matches('/').to_string();
        self
    }

    /// The `User-Agent` GitHub sees; defaults to `draftline`.
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Pull requests come from `owner`'s fork of the repository: Send
    /// pushed the branch there, and the pull request merges it into this
    /// provider's repository (such as [`GitHubRepoAccess::fork_parent`]).
    pub fn with_head_owner(mut self, owner: impl Into<String>) -> Self {
        self.head_owner = Some(owner.into());
        self
    }

    /// The repository pull requests merge into.
    pub fn repo(&self) -> &GitHubRepo {
        &self.repo
    }

    /// `owner:branch` for a pull request's head.
    fn head(&self, branch: &str) -> String {
        let owner = self.head_owner.as_deref().unwrap_or(&self.repo.owner);
        format!("{owner}:{branch}")
    }

    /// What the token may do with the repository: push access, its default
    /// branch and the repository it's a fork of. Read-only.
    pub fn repo_access(&self) -> Result<GitHubRepoAccess> {
        let builder = self.client.get(self.repo_url(""));
        let repo: GitHubRepoBody = self.send_json(Action::ReadRepository, builder)?;
        Ok(GitHubRepoAccess {
            repository: GitHubRepo::from_full_name(&repo.full_name)
                .unwrap_or_else(|| self.repo.clone()),
            default_branch: repo.default_branch,
            can_push: repo.permissions.is_some_and(|permissions| permissions.push),
            private: repo.private,
            fork_parent: repo
                .parent
                .and_then(|parent| GitHubRepo::from_full_name(&parent.full_name)),
        })
    }

    fn repo_url(&self, path: &str) -> String {
        format!(
            "{}/repos/{}/{}{path}",
            self.api_base, self.repo.owner, self.repo.repo
        )
    }

    fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        action: Action,
        builder: reqwest::blocking::RequestBuilder,
    ) -> Result<T> {
        let response = builder
            .bearer_auth(&self.token)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .map_err(|error| {
                DraftlineError::PullRequest(format!("GitHub {} failed: {error}", action.name()))
            })?;
        let status = response.status();
        if status.is_success() {
            return response.json::<T>().map_err(|error| {
                DraftlineError::PullRequest(format!(
                    "GitHub {} returned an unexpected response: {error}",
                    action.name()
                ))
            });
        }
        let headers = response.headers();
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || headers.contains_key(reqwest::header::RETRY_AFTER)
            || headers
                .get("x-ratelimit-remaining")
                .is_some_and(|remaining| remaining == "0");
        let detail = response
            .json::<GitHubErrorBody>()
            .ok()
            .map(GitHubErrorBody::message)
            .filter(|message| !message.trim().is_empty());
        let message = match &detail {
            Some(detail) => format!("GitHub {} failed ({status}): {detail}", action.name()),
            None => format!("GitHub {} failed ({status})", action.name()),
        };
        let said = |words: &[&str]| {
            let lower = detail.as_deref().unwrap_or_default().to_ascii_lowercase();
            words.iter().any(|word| lower.contains(word))
        };
        let rate_limited = rate_limited || said(&["rate limit"]);
        Err(match status.as_u16() {
            _ if rate_limited => DraftlineError::PullRequest(message),
            401 => DraftlineError::GitHubUnauthorized(message),
            403 if said(&["saml", "single sign-on"]) => DraftlineError::GitHubUnauthorized(message),
            // GitHub's own words when the token can't push to the repository.
            403 if action == Action::CreatePullRequest && said(&["must have push access"]) => {
                DraftlineError::GitHubNoPushAccess {
                    repository: self.repo.full_name(),
                    message,
                }
            }
            403 if action != Action::CreatePullRequest => {
                DraftlineError::GitHubUnauthorized(message)
            }
            // GitHub hides private repositories the token can't see.
            404 => DraftlineError::GitHubRepositoryNotFound(self.repo.full_name()),
            422 if action == Action::CreatePullRequest && said(&["already exists"]) => {
                DraftlineError::PullRequest(format!("{DUPLICATE_PULL_REQUEST}{message}"))
            }
            _ => DraftlineError::PullRequest(message),
        })
    }
}

impl PullRequestProvider for GitHubProvider {
    fn find_open(&mut self, request: &PullRequestRequest) -> Result<Option<PullRequest>> {
        let head = self.head(&request.head);
        let builder = self.client.get(self.repo_url("/pulls")).query(&[
            ("state", "open"),
            ("head", head.as_str()),
            ("base", request.base.as_str()),
        ]);
        let pulls: Vec<GitHubPullRequest> = self.send_json(Action::FindPullRequest, builder)?;
        Ok(pulls.into_iter().next().map(PullRequest::from))
    }

    fn create(&mut self, request: &PullRequestRequest) -> Result<PullRequest> {
        let builder = self
            .client
            .post(self.repo_url("/pulls"))
            .json(&serde_json::json!({
                "title": request.title,
                "head": self.head(&request.head),
                "base": request.base,
                "body": request.body,
            }));
        match self.send_json::<GitHubPullRequest>(Action::CreatePullRequest, builder) {
            Ok(pull) => Ok(pull.into()),
            // Someone opened it since Send looked: use theirs.
            Err(DraftlineError::PullRequest(message))
                if message.starts_with(DUPLICATE_PULL_REQUEST) =>
            {
                match self.find_open(request)? {
                    Some(pull) => Ok(pull),
                    None => Err(DraftlineError::PullRequest(
                        message[DUPLICATE_PULL_REQUEST.len()..].to_string(),
                    )),
                }
            }
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    FindPullRequest,
    CreatePullRequest,
    ReadRepository,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::FindPullRequest => "pull request lookup",
            Self::CreatePullRequest => "pull request creation",
            Self::ReadRepository => "repository lookup",
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequest {
    number: u64,
    html_url: String,
}

impl From<GitHubPullRequest> for PullRequest {
    fn from(pull: GitHubPullRequest) -> Self {
        PullRequest {
            number: pull.number,
            url: pull.html_url,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRepoBody {
    full_name: String,
    default_branch: String,
    #[serde(default)]
    private: bool,
    permissions: Option<GitHubPermissions>,
    parent: Option<GitHubParent>,
}

#[derive(Debug, Deserialize)]
struct GitHubPermissions {
    #[serde(default)]
    push: bool,
}

#[derive(Debug, Deserialize)]
struct GitHubParent {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GitHubErrorBody {
    message: Option<String>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

impl GitHubErrorBody {
    fn message(self) -> String {
        let mut message = self.message.unwrap_or_default();
        let extra: Vec<&str> = self
            .errors
            .iter()
            .filter_map(|error| error.get("message").and_then(|value| value.as_str()))
            .collect();
        if !extra.is_empty() {
            message = format!("{message} ({})", extra.join("; "));
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remote_url_variants() {
        let expected = Some(GitHubRepo {
            owner: "contoso".to_string(),
            repo: "app".to_string(),
        });
        for url in [
            "https://github.com/contoso/app",
            "https://github.com/contoso/app.git",
            "https://github.com/contoso/app/",
            "git@github.com:contoso/app.git",
            "git@github.com:contoso/app",
            "ssh://git@github.com/contoso/app",
            "ssh://git@github.com/contoso/app.git",
            "  HTTPS://GitHub.com/contoso/app.git  ",
        ] {
            assert_eq!(parse_github_remote_url(url), expected, "{url}");
        }
        for url in [
            "https://gitlab.com/contoso/app.git",
            "https://github.com/contoso",
            "https://github.com/contoso/app/extra",
            "C:/remotes/app.git",
            "",
        ] {
            assert_eq!(parse_github_remote_url(url), None, "{url}");
        }
    }

    #[test]
    fn a_provider_needs_a_token_and_a_github_remote() {
        assert!(matches!(
            GitHubProvider::for_remote_url("https://github.com/o/r", "  "),
            Err(DraftlineError::GitHubUnauthorized(_))
        ));
        assert!(matches!(
            GitHubProvider::for_remote_url("https://dev.azure.com/o/p/_git/r", "token"),
            Err(DraftlineError::PullRequest(_))
        ));
        let provider = GitHubProvider::for_remote_url("git@github.com:o/r.git", "s3cr3t").unwrap();
        assert!(!format!("{provider:?}").contains("s3cr3t"));
    }
}
