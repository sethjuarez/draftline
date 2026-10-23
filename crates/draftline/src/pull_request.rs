//! Send: share local saves by pushing the current branch and opening a pull
//! request. Draftline stays git-only; the host app talks to its code host
//! (GitHub, Azure DevOps, ...) through a [`PullRequestProvider`].

use serde::{Deserialize, Serialize};

use crate::Result;

/// A pull request on the code host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

/// The pull request Send wants: `head` merging into `base` on `remote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRequest {
    pub remote: String,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
}

/// Host-implemented bridge to the code host's pull request API.
pub trait PullRequestProvider {
    /// The open pull request from `request.head` into `request.base`, if any.
    fn find_open(&mut self, request: &PullRequestRequest) -> Result<Option<PullRequest>>;

    /// Opens a new pull request.
    fn create(&mut self, request: &PullRequestRequest) -> Result<PullRequest>;
}

/// Options for [`crate::Workspace::send`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendRequest {
    /// Pull request title; also names the new branch when Send creates one.
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Branch to merge into. Defaults to the remote's default branch.
    #[serde(default)]
    pub base: Option<String>,
    /// Name for the branch Send creates when on the base branch. Defaults to
    /// the variation namespace plus a slug of the title.
    #[serde(default)]
    pub branch: Option<String>,
}

/// What [`crate::Workspace::send`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendResult {
    pub remote: String,
    /// The branch that was pushed.
    pub variation: String,
    pub base: String,
    /// `true` when Send moved the work off the base branch onto a new branch.
    pub created_variation: bool,
    pub published_versions: usize,
    pub pull_request: PullRequest,
    /// `false` when an already-open pull request was reused.
    pub created_pull_request: bool,
}
