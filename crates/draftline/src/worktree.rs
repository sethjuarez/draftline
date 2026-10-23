use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Prefix on the Git worktree names Draftline creates; other tools' worktrees
/// (for example the Copilot app's) are never listed or removed.
pub const OWNED_WORKTREE_PREFIX: &str = "draftline-";

/// Where a new owned worktree's branch starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStart {
    /// The base branch as last fetched (`origin/main`), else the local base.
    LatestBase,
    /// The current version, so work in progress carries over.
    CurrentVersion,
}

/// A separate checkout Draftline created for one piece of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedWorktree {
    /// Git's worktree name; pass it to `remove_owned_worktree`.
    pub name: String,
    /// The checkout's root folder.
    pub path: PathBuf,
    /// The project folder inside the checkout.
    pub project_path: PathBuf,
    /// The checked-out branch, if any.
    pub variation: Option<String>,
}
