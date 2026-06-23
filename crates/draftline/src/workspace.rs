use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{
    build::CheckoutBuilder, BranchType, Commit, DiffFormat, DiffOptions, ObjectType, Oid,
    Repository, Signature, Status, StatusOptions, Tree,
};

use crate::recovery::RecoveryOperation;
use crate::{
    path::normalize_workspace_relative, ContentPolicy, Contributor, DraftlineError, PublishResult,
    RecoveryState, RemoteEndpoint, RemoteVersionSummary, Result, SyncState, SyncStatus,
};

/// A folder-backed content workspace.
pub struct Workspace {
    root: PathBuf,
    repo: Repository,
    content_policy: ContentPolicy,
}

/// A named version of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    id: VersionId,
    pub label: String,
    pub author: Contributor,
    pub saved_by: Contributor,
    pub time_seconds: i64,
}

impl Version {
    pub fn id(&self) -> &VersionId {
        &self.id
    }
}

/// Identifier for a version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionId(String);

impl VersionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VersionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<Oid> for VersionId {
    fn from(value: Oid) -> Self {
        Self(value.to_string())
    }
}

/// An alternate direction for workspace content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variation {
    id: VariationId,
    pub name: String,
    pub is_current: bool,
}

impl Variation {
    pub fn id(&self) -> &VariationId {
        &self.id
    }
}

/// Identifier for a variation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VariationId(String);

impl VariationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VariationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for VariationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A changed file in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub is_binary: bool,
    pub is_large: bool,
}

/// High-level kind of file change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Conflicted,
    TypeChanged,
}

/// A content-workflow view of workspace changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    pub files: Vec<ChangedFile>,
    pub diff: Option<String>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Policy for switching variations when unsaved work exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchPolicy {
    AbortIfDirty,
    SaveFirst { label: String },
    Shelve { name: String },
    Discard,
}

/// Dry-run report for a risky operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub operation: String,
    pub will_write_files: bool,
    pub dirty_files: Vec<ChangedFile>,
    pub untracked_assets: Vec<PathBuf>,
    pub unresolved_conflicts: Vec<PathBuf>,
    pub large_files: Vec<PathBuf>,
    pub binary_files: Vec<PathBuf>,
    pub variation_divergence: Option<String>,
    pub can_proceed: bool,
}

/// Read-only view of a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionPreview {
    pub id: VersionId,
    pub files: Vec<PreviewFile>,
}

/// File content from a read-only version preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewFile {
    pub path: PathBuf,
    pub content: Option<String>,
    pub is_binary: bool,
}

impl Workspace {
    /// Initializes a new workspace or opens the existing workspace at `path`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_policy(path, ContentPolicy::default())
    }

    /// Initializes a workspace with an explicit content policy.
    pub fn init_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;

        let repo = match Repository::open(path.as_ref()) {
            Ok(repo) => repo,
            Err(_) => Repository::init(path.as_ref())?,
        };

        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());

        Ok(Self {
            root,
            repo,
            content_policy,
        }
        .initialize())
    }

    /// Opens an existing workspace.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_policy(path, ContentPolicy::default())
    }

    /// Opens an existing workspace with an explicit content policy.
    pub fn open_with_policy(path: impl AsRef<Path>, content_policy: ContentPolicy) -> Result<Self> {
        let repo = Repository::discover(path.as_ref())?;
        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());

        Ok(Self {
            root,
            repo,
            content_policy,
        }
        .initialize())
    }

    /// Clones a shared workspace from a remote endpoint.
    pub fn clone_workspace(remote_url: impl AsRef<str>, path: impl AsRef<Path>) -> Result<Self> {
        Self::clone_workspace_with_policy(remote_url, path, ContentPolicy::default())
    }

    /// Clones a shared workspace from a remote endpoint with an explicit content policy.
    pub fn clone_workspace_with_policy(
        remote_url: impl AsRef<str>,
        path: impl AsRef<Path>,
        content_policy: ContentPolicy,
    ) -> Result<Self> {
        let repo = Repository::clone(remote_url.as_ref(), path.as_ref())?;
        let root = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());

        Ok(Self {
            root,
            repo,
            content_policy,
        }
        .initialize())
    }

    /// Returns the root content folder for this workspace.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the current content policy.
    pub fn content_policy(&self) -> &ContentPolicy {
        &self.content_policy
    }

    /// Returns incomplete recovery state, if a prior operation was interrupted.
    pub fn recovery_state(&self) -> Result<Option<RecoveryState>> {
        let path = self.ledger_path();
        if !path.exists() {
            return Ok(None);
        }

        let text = fs::read_to_string(path)?;
        let state: RecoveryState = serde_json::from_str(&text)?;

        if state.completed {
            Ok(None)
        } else {
            Ok(Some(state))
        }
    }

    /// Acknowledges an incomplete recovery record and allows normal operations again.
    pub fn acknowledge_recovery(&self) -> Result<()> {
        if self.ledger_path().exists() {
            fs::remove_file(self.ledger_path())?;
        }
        Ok(())
    }

    /// Resolves a workspace-relative path safely.
    pub fn resolve_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self.root.join(normalize_workspace_relative(path)?))
    }

    /// Saves the current workspace content as a named version.
    pub fn save_version(&self, label: impl AsRef<str>) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        self.save_version_unchecked(label)
    }

    fn save_version_unchecked(&self, label: impl AsRef<str>) -> Result<Version> {
        let mut index = self.repo.index()?;

        for changed in self.changed_files_unchecked()? {
            match changed.kind {
                ChangeKind::Deleted => index.remove_path(&changed.path)?,
                _ => index.add_path(&changed.path)?,
            }
        }

        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let signature = self.workspace_signature()?;
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| self.repo.find_commit(oid).ok());

        let oid = match parent.as_ref() {
            Some(parent) => self.repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                label.as_ref(),
                &tree,
                &[parent],
            )?,
            None => self.repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                label.as_ref(),
                &tree,
                &[],
            )?,
        };

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    /// Lists versions reachable from the current variation, newest first.
    pub fn versions(&self) -> Result<Vec<Version>> {
        self.ensure_no_pending_recovery()?;
        let mut walk = self.repo.revwalk()?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }

        walk.map(|oid| {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            Ok(version_from_commit(&commit))
        })
        .collect()
    }

    /// Returns true when the workspace has unsaved content changes.
    pub fn is_dirty(&self) -> Result<bool> {
        Ok(!self.changed_files()?.is_empty())
    }

    /// Lists changed content files in the workspace.
    pub fn changed_files(&self) -> Result<Vec<ChangedFile>> {
        self.ensure_no_pending_recovery()?;
        self.changed_files_unchecked()
    }

    fn changed_files_unchecked(&self) -> Result<Vec<ChangedFile>> {
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true);

        let statuses = self.repo.statuses(Some(&mut options))?;
        let mut changed = Vec::new();

        for entry in statuses.iter() {
            let Some(path) = entry.path() else {
                continue;
            };
            if !self.content_policy.tracks(path)? {
                continue;
            }

            let relative = PathBuf::from(path);
            let full_path = self.root.join(&relative);
            changed.push(ChangedFile {
                path: relative,
                kind: status_to_change_kind(entry.status()),
                is_binary: file_is_binary(&full_path)?,
                is_large: file_is_large(
                    &full_path,
                    self.content_policy.large_file_threshold_bytes(),
                )?,
            });
        }

        changed.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(changed)
    }

    /// Returns content changes and an optional textual diff of unsaved workspace changes.
    pub fn changes(&self) -> Result<ChangeSet> {
        self.ensure_no_pending_recovery()?;
        self.changes_unchecked()
    }

    fn changes_unchecked(&self) -> Result<ChangeSet> {
        Ok(ChangeSet {
            files: self.changed_files_unchecked()?,
            diff: Some(self.diff_unsaved_text()?),
        })
    }

    /// Preflights switching to another variation without mutating workspace files.
    pub fn preflight_switch_variation(&self, variation: &VariationId) -> Result<PreflightReport> {
        self.ensure_no_pending_recovery()?;
        self.preflight_switch_variation_unchecked(variation)
    }

    fn preflight_switch_variation_unchecked(
        &self,
        variation: &VariationId,
    ) -> Result<PreflightReport> {
        let change_set = self.changes_unchecked()?;
        Ok(preflight_report(
            "switch_variation",
            true,
            change_set.files,
            Some(format!("current -> {}", variation.as_str())),
        ))
    }

    /// Creates a new variation from the current version.
    pub fn create_variation(&self, name: impl AsRef<str>) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(name.as_ref())?;
        let head = self.repo.head()?.peel_to_commit()?;
        self.repo.branch(&name, &head, false)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
        ))
    }

    /// Creates a variation from an older version without switching to it.
    pub fn create_variation_from(
        &self,
        version: &VersionId,
        name: impl AsRef<str>,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let name = validate_variation_name(name.as_ref())?;
        let commit = self.find_version_commit(version)?;
        self.repo.branch(&name, &commit, false)?;

        Ok(variation_from_name(
            name,
            self.current_variation().ok().as_ref(),
        ))
    }

    /// Lists local variations.
    pub fn variations(&self) -> Result<Vec<Variation>> {
        self.ensure_no_pending_recovery()?;
        let current = self.current_variation().ok();
        let mut paths = Vec::new();

        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()? else {
                continue;
            };

            paths.push(variation_from_name(name.to_string(), current.as_ref()));
        }

        paths.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(paths)
    }

    /// Switches to a variation with an explicit safety policy.
    pub fn switch_variation(
        &self,
        variation: &VariationId,
        policy: SwitchPolicy,
    ) -> Result<Variation> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path())?;
        let mut report = self.preflight_switch_variation_unchecked(variation)?;

        match &policy {
            SwitchPolicy::AbortIfDirty if !report.can_proceed => {
                return Err(DraftlineError::PreflightFailed(Box::new(report)));
            }
            SwitchPolicy::SaveFirst { label } if !report.can_proceed => {
                self.save_version_unchecked(label)?;
            }
            SwitchPolicy::Shelve { name } if !report.can_proceed => {
                self.shelve_changes_unchecked(name)?;
            }
            SwitchPolicy::Discard => {
                return Err(DraftlineError::UnsupportedSwitchPolicy(
                    "discard requires an explicit overwrite API and is not implemented",
                ));
            }
            _ => {}
        }

        report = self.preflight_switch_variation_unchecked(variation)?;
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::SwitchVariation,
            original_variation: self.current_variation().ok(),
            target: Some(variation.as_str().to_string()),
            completed: false,
        })?;

        let branch = self
            .repo
            .find_branch(variation.as_str(), BranchType::Local)?;
        let reference = branch.into_reference();
        let target = reference.peel(ObjectType::Commit)?;

        self.repo.checkout_tree(&target, None)?;
        self.repo
            .set_head(&format!("refs/heads/{}", variation.as_str()))?;

        let result = variation_from_name(variation.as_str().to_string(), Some(&variation.0));
        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::SwitchVariation,
            original_variation: None,
            target: Some(variation.as_str().to_string()),
            completed: true,
        })?;

        Ok(result)
    }

    /// Deletes an alternate variation.
    pub fn delete_variation(&self, variation: &VariationId) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        if self.current_variation().ok().as_deref() == Some(variation.as_str()) {
            return Err(DraftlineError::CannotDeleteCurrentVariation(
                variation.as_str().to_string(),
            ));
        }

        self.repo
            .find_branch(variation.as_str(), BranchType::Local)?
            .delete()?;
        Ok(())
    }

    /// Creates a new version from an earlier version without switching variations.
    pub fn restore_version_as_new_save(
        &self,
        version: &VersionId,
        label: impl AsRef<str>,
    ) -> Result<Version> {
        self.ensure_no_pending_recovery()?;
        let _lock = OperationLock::acquire(&self.lock_path())?;
        let report = preflight_report(
            "restore_version_as_new_save",
            true,
            self.changed_files_unchecked()?,
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: self.current_variation().ok(),
            target: Some(version.as_str().to_string()),
            completed: false,
        })?;

        let commit = self.find_version_commit(version)?;
        let tree = commit.tree()?;
        let signature = self.workspace_signature()?;
        let parent = self.repo.head()?.peel_to_commit()?;
        let oid = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            label.as_ref(),
            &tree,
            &[&parent],
        )?;

        self.repo
            .checkout_tree(tree.as_object(), Some(CheckoutBuilder::new().force()))?;

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::RestoreVersionAsNewSave,
            original_variation: None,
            target: Some(version.as_str().to_string()),
            completed: true,
        })?;

        Ok(version_from_commit(&self.repo.find_commit(oid)?))
    }

    /// Reads a version without mutating the live workspace.
    pub fn preview_version(&self, version: &VersionId) -> Result<VersionPreview> {
        self.ensure_no_pending_recovery()?;
        let commit = self.find_version_commit(version)?;
        let tree = commit.tree()?;
        let mut files = Vec::new();
        collect_preview_files(&self.repo, &tree, Path::new(""), &mut files)?;

        Ok(VersionPreview {
            id: version.clone(),
            files,
        })
    }

    /// Returns the current variation name when the workspace is on a normal variation.
    pub fn current_variation(&self) -> Result<String> {
        self.ensure_no_pending_recovery()?;
        self.current_variation_unchecked()
    }

    fn current_variation_unchecked(&self) -> Result<String> {
        let head = self.repo.head()?;
        let Some(name) = head.shorthand() else {
            return Err(DraftlineError::NoCurrentVariation);
        };

        Ok(name.to_string())
    }

    /// Adds or updates a remote endpoint for sharing/backing up this workspace.
    pub fn add_remote(
        &self,
        name: impl AsRef<str>,
        url: impl AsRef<str>,
    ) -> Result<RemoteEndpoint> {
        self.ensure_no_pending_recovery()?;
        let name = name.as_ref().trim();
        let url = url.as_ref().trim();

        match self.repo.find_remote(name) {
            Ok(_) => self.repo.remote_set_url(name, url)?,
            Err(_) => {
                self.repo.remote(name, url)?;
            }
        }

        Ok(RemoteEndpoint {
            name: name.to_string(),
            url: url.to_string(),
        })
    }

    /// Lists configured remote endpoints.
    pub fn remotes(&self) -> Result<Vec<RemoteEndpoint>> {
        self.ensure_no_pending_recovery()?;
        let names = self.repo.remotes()?;
        let mut remotes = Vec::new();

        for name in names.iter().flatten() {
            let remote = self.repo.find_remote(name)?;
            remotes.push(RemoteEndpoint {
                name: name.to_string(),
                url: remote.url().unwrap_or_default().to_string(),
            });
        }

        remotes.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(remotes)
    }

    /// Fetches remote version metadata without changing local content.
    pub fn fetch_remote(&self, remote: impl AsRef<str>) -> Result<()> {
        self.ensure_no_pending_recovery()?;
        self.fetch_remote_unchecked(remote)
    }

    fn fetch_remote_unchecked(&self, remote: impl AsRef<str>) -> Result<()> {
        let variation = self.current_variation_unchecked()?;
        let mut remote = self.repo.find_remote(remote.as_ref())?;
        if let Err(error) = remote.fetch(&[variation.as_str()], None, None) {
            if error.code() != git2::ErrorCode::NotFound {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Returns collaboration status for the current variation.
    pub fn sync_status(&self, remote: impl AsRef<str>) -> Result<SyncStatus> {
        self.ensure_no_pending_recovery()?;
        let remote = remote.as_ref().to_string();
        let variation = self.current_variation_unchecked()?;
        let local = self.repo.head()?.peel_to_commit()?.id();
        let remote_ref = format!("refs/remotes/{remote}/{variation}");

        let Ok(remote_oid) = self.repo.refname_to_id(&remote_ref) else {
            return Ok(SyncStatus {
                remote,
                variation,
                ahead: 0,
                behind: 0,
                state: SyncState::NoRemoteVersion,
                incoming: Vec::new(),
            });
        };

        let (ahead, behind) = self.repo.graph_ahead_behind(local, remote_oid)?;
        let state = match (ahead, behind) {
            (0, 0) => SyncState::UpToDate,
            (_, 0) => SyncState::LocalAhead,
            (0, _) => SyncState::IncomingAvailable,
            _ => SyncState::NeedsMerge,
        };

        Ok(SyncStatus {
            remote,
            variation,
            ahead,
            behind,
            state,
            incoming: self.incoming_versions(local, remote_oid)?,
        })
    }

    /// Publishes local versions for the current variation when doing so will not overwrite remote work.
    pub fn publish_changes(&self, remote: impl AsRef<str>) -> Result<PublishResult> {
        self.ensure_no_pending_recovery()?;
        let report = preflight_report(
            "publish_changes",
            false,
            self.changed_files_unchecked()?,
            None,
        );
        if !report.can_proceed {
            return Err(DraftlineError::PreflightFailed(Box::new(report)));
        }

        let remote_name = remote.as_ref().to_string();
        self.fetch_remote_unchecked(&remote_name)?;
        let status = self.sync_status(&remote_name)?;
        if matches!(
            status.state,
            SyncState::IncomingAvailable | SyncState::NeedsMerge
        ) {
            return Err(DraftlineError::SyncNeedsMerge(Box::new(status)));
        }

        let variation = self.current_variation_unchecked()?;
        let mut remote = self.repo.find_remote(&remote_name)?;
        let refspec = format!("refs/heads/{variation}:refs/heads/{variation}");
        remote.push(&[refspec.as_str()], None)?;

        Ok(PublishResult {
            remote: remote_name,
            variation,
            published_versions: status.ahead,
        })
    }

    fn diff_unsaved_text(&self) -> Result<String> {
        let head_tree = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_tree().ok());

        let mut options = DiffOptions::new();
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))?;

        let mut text = String::new();
        diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
            if let Ok(content) = std::str::from_utf8(line.content()) {
                text.push_str(content);
            }
            true
        })?;

        Ok(text)
    }

    fn find_version_commit(&self, version: &VersionId) -> Result<git2::Commit<'_>> {
        let oid = Oid::from_str(version.as_str())
            .map_err(|_| DraftlineError::VersionNotFound(version.to_string()))?;
        self.repo
            .find_commit(oid)
            .map_err(|_| DraftlineError::VersionNotFound(version.to_string()))
    }

    fn draftline_dir(&self) -> PathBuf {
        self.repo.path().join("draftline")
    }

    fn ledger_path(&self) -> PathBuf {
        self.draftline_dir().join("recovery.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.draftline_dir().join("operation.lock")
    }

    fn write_recovery_state(&self, state: &RecoveryState) -> Result<()> {
        fs::create_dir_all(self.draftline_dir())?;
        fs::write(self.ledger_path(), serde_json::to_vec_pretty(state)?)?;
        Ok(())
    }

    fn initialize(self) -> Self {
        let _ = fs::create_dir_all(self.draftline_dir());
        self
    }

    fn ensure_no_pending_recovery(&self) -> Result<()> {
        if let Some(state) = self.recovery_state()? {
            return Err(DraftlineError::RecoveryRequired(Box::new(state)));
        }

        Ok(())
    }

    fn shelve_changes_unchecked(&self, name: &str) -> Result<()> {
        let safe_name = validate_variation_name(name)?;
        let operation_id = new_operation_id();
        self.write_recovery_state(&RecoveryState {
            operation_id: operation_id.clone(),
            operation: RecoveryOperation::ShelveChanges,
            original_variation: self.current_variation_unchecked().ok(),
            target: Some(safe_name.clone()),
            completed: false,
        })?;

        let changed_files = self.changed_files_unchecked()?;
        let untracked_content: Vec<PathBuf> = changed_files
            .iter()
            .filter(|changed| changed.kind == ChangeKind::Added)
            .map(|changed| changed.path.clone())
            .collect();

        let mut index = self.repo.index()?;
        for changed in changed_files {
            match changed.kind {
                ChangeKind::Deleted => index.remove_path(&changed.path)?,
                _ => index.add_path(&changed.path)?,
            }
        }
        index.write()?;

        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let signature = self.workspace_signature()?;
        let parent = self.repo.head()?.peel_to_commit()?;
        let oid = self.repo.commit(
            None,
            &signature,
            &signature,
            &format!("Shelved changes: {safe_name}"),
            &tree,
            &[&parent],
        )?;
        self.repo.reference(
            &format!("refs/draftline/shelves/{safe_name}"),
            oid,
            false,
            "shelve changes",
        )?;

        self.repo
            .checkout_head(Some(CheckoutBuilder::new().force()))?;

        for path in untracked_content {
            let full_path = self.root.join(path);
            if full_path.is_file() {
                fs::remove_file(full_path)?;
            }
        }

        self.write_recovery_state(&RecoveryState {
            operation_id,
            operation: RecoveryOperation::ShelveChanges,
            original_variation: None,
            target: Some(safe_name),
            completed: true,
        })?;

        Ok(())
    }

    fn incoming_versions(&self, local: Oid, remote: Oid) -> Result<Vec<RemoteVersionSummary>> {
        let mut walk = self.repo.revwalk()?;
        walk.push(remote)?;
        walk.hide(local)?;

        let mut versions = Vec::new();
        for oid in walk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            versions.push(remote_summary_from_commit(&commit));
        }

        Ok(versions)
    }

    fn workspace_signature(&self) -> Result<Signature<'_>> {
        match self.repo.signature() {
            Ok(signature) => Ok(signature),
            Err(_) => Ok(Signature::now("Draftline", "draftline@example.invalid")?),
        }
    }
}

fn validate_variation_name(name: &str) -> Result<String> {
    let trimmed = name.trim();

    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.contains("..")
        || trimmed.contains('\\')
        || trimmed.chars().any(|character| {
            character.is_control() || matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[')
        })
    {
        return Err(DraftlineError::InvalidVariationName(name.to_string()));
    }

    Ok(trimmed.to_string())
}

fn variation_from_name(name: String, current: Option<&String>) -> Variation {
    Variation {
        id: VariationId::from(name.clone()),
        is_current: current.map(|current| current == &name).unwrap_or(false),
        name,
    }
}

fn status_to_change_kind(status: Status) -> ChangeKind {
    if status.is_conflicted() {
        ChangeKind::Conflicted
    } else if status.is_wt_new() || status.is_index_new() {
        ChangeKind::Added
    } else if status.is_wt_deleted() || status.is_index_deleted() {
        ChangeKind::Deleted
    } else if status.is_wt_renamed() || status.is_index_renamed() {
        ChangeKind::Renamed
    } else if status.is_wt_typechange() || status.is_index_typechange() {
        ChangeKind::TypeChanged
    } else {
        ChangeKind::Modified
    }
}

fn preflight_report(
    operation: impl Into<String>,
    will_write_files: bool,
    dirty_files: Vec<ChangedFile>,
    variation_divergence: Option<String>,
) -> PreflightReport {
    let untracked_assets = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Added)
        .map(|file| file.path.clone())
        .collect();
    let unresolved_conflicts = dirty_files
        .iter()
        .filter(|file| file.kind == ChangeKind::Conflicted)
        .map(|file| file.path.clone())
        .collect();
    let large_files = dirty_files
        .iter()
        .filter(|file| file.is_large)
        .map(|file| file.path.clone())
        .collect();
    let binary_files = dirty_files
        .iter()
        .filter(|file| file.is_binary)
        .map(|file| file.path.clone())
        .collect();
    let can_proceed = dirty_files.is_empty();

    PreflightReport {
        operation: operation.into(),
        will_write_files,
        dirty_files,
        untracked_assets,
        unresolved_conflicts,
        large_files,
        binary_files,
        variation_divergence,
        can_proceed,
    }
}

fn file_is_large(path: &Path, threshold: u64) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > threshold),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn file_is_binary(path: &Path) -> Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    Ok(bytes.contains(&0) || std::str::from_utf8(&bytes).is_err())
}

fn collect_preview_files(
    repo: &Repository,
    tree: &Tree<'_>,
    prefix: &Path,
    files: &mut Vec<PreviewFile>,
) -> Result<()> {
    for entry in tree.iter() {
        let Some(name) = entry.name() else {
            continue;
        };
        let path = prefix.join(name);

        match entry.kind() {
            Some(ObjectType::Blob) => {
                let blob = repo.find_blob(entry.id())?;
                let content = std::str::from_utf8(blob.content())
                    .ok()
                    .map(ToString::to_string);
                files.push(PreviewFile {
                    path,
                    is_binary: content.is_none(),
                    content,
                });
            }
            Some(ObjectType::Tree) => {
                let child = repo.find_tree(entry.id())?;
                collect_preview_files(repo, &child, &path, files)?;
            }
            _ => {}
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(())
}

fn contributor_from_signature(signature: &git2::Signature<'_>) -> Contributor {
    Contributor {
        name: signature.name().unwrap_or("Unknown").to_string(),
        email: signature.email().map(ToString::to_string),
    }
}

fn version_from_commit(commit: &Commit<'_>) -> Version {
    Version {
        id: VersionId::from(commit.id()),
        label: commit.summary().unwrap_or_default().to_string(),
        author: contributor_from_signature(&commit.author()),
        saved_by: contributor_from_signature(&commit.committer()),
        time_seconds: commit.time().seconds(),
    }
}

fn remote_summary_from_commit(commit: &Commit<'_>) -> RemoteVersionSummary {
    RemoteVersionSummary {
        id: commit.id().to_string(),
        label: commit.summary().unwrap_or_default().to_string(),
        author: contributor_from_signature(&commit.author()),
        time_seconds: commit.time().seconds(),
    }
}

fn new_operation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("op-{nanos}")
}

struct OperationLock {
    path: PathBuf,
}

impl OperationLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    DraftlineError::WorkspaceLocked
                } else {
                    DraftlineError::Io(error)
                }
            })?;

        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(root: &Path, relative: &str, content: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn configure_identity(workspace: &Workspace, name: &str, email: &str) {
        let mut config = workspace.repo.config().unwrap();
        config.set_str("user.name", name).unwrap();
        config.set_str("user.email", email).unwrap();
    }

    #[test]
    fn saves_and_lists_versions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");
        configure_identity(&workspace, "Seth", "seth@example.com");
        let saved = workspace.save_version("Homepage draft").unwrap();

        let versions = workspace.versions().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id(), saved.id());
        assert_eq!(versions[0].label, "Homepage draft");
        assert_eq!(versions[0].author.name, "Seth");
    }

    #[test]
    fn content_policy_excludes_runtime_state_from_changes_and_versions() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/post.md", b"# Hello");
        write_file(workspace.root(), "ui-state/panel.json", br#"{"open":true}"#);
        let version = workspace.save_version("Content only").unwrap();

        let preview = workspace.preview_version(version.id()).unwrap();
        assert_eq!(preview.files.len(), 1);
        assert_eq!(
            preview.files[0].path,
            PathBuf::from("content").join("post.md")
        );
    }

    #[test]
    fn detects_unsaved_changes_as_changeset() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");

        let changes = workspace.changes().unwrap();
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, PathBuf::from("post.md"));
        assert_eq!(changes.files[0].kind, ChangeKind::Added);
        assert!(!changes.files[0].is_binary);
    }

    #[test]
    fn preflight_reports_binary_and_large_files() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().with_large_file_threshold(3);
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "asset.bin", &[0, 1, 2, 3]);
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "asset.bin", &[0, 1, 2, 3, 4]);

        let report = workspace
            .preflight_switch_variation(variation.id())
            .unwrap();
        assert_eq!(report.binary_files, vec![PathBuf::from("asset.bin")]);
        assert_eq!(report.large_files, vec![PathBuf::from("asset.bin")]);
    }

    #[test]
    fn refuses_to_switch_variations_with_unsaved_changes_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");
        workspace.save_version("First draft").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "post.md", b"# Unsaved");

        let err = workspace
            .switch_variation(variation.id(), SwitchPolicy::AbortIfDirty)
            .unwrap_err();
        assert!(matches!(err, DraftlineError::PreflightFailed(_)));
    }

    #[test]
    fn save_first_policy_preserves_work_before_switching_variations() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"# Hello");
        workspace.save_version("First draft").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "post.md", b"# Save me");

        workspace
            .switch_variation(
                variation.id(),
                SwitchPolicy::SaveFirst {
                    label: "Saved before switch".to_string(),
                },
            )
            .unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "alternate");
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn recovery_state_blocks_normal_operations_until_acknowledged() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        workspace
            .write_recovery_state(&RecoveryState {
                operation_id: "interrupted".to_string(),
                operation: RecoveryOperation::SwitchVariation,
                original_variation: Some("master".to_string()),
                target: Some("alternate".to_string()),
                completed: false,
            })
            .unwrap();

        let err = workspace.changes().unwrap_err();
        assert!(matches!(err, DraftlineError::RecoveryRequired(_)));

        workspace.acknowledge_recovery().unwrap();
        assert!(workspace.changes().is_ok());
    }

    #[test]
    fn shelve_policy_holds_dirty_content_without_replaying_it() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ContentPolicy::new().include("content").unwrap();
        let workspace = Workspace::init_with_policy(temp.path(), policy).unwrap();

        write_file(workspace.root(), "content/post.md", b"base");
        workspace.save_version("Base").unwrap();
        let variation = workspace.create_variation("alternate").unwrap();
        write_file(workspace.root(), "content/post.md", b"dirty");
        write_file(workspace.root(), "ui-state/panel.json", b"keep me");

        workspace
            .switch_variation(
                variation.id(),
                SwitchPolicy::Shelve {
                    name: "before-alternate".to_string(),
                },
            )
            .unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "alternate");
        assert_eq!(
            fs::read_to_string(workspace.root().join("content").join("post.md")).unwrap(),
            "base"
        );
        assert_eq!(
            fs::read_to_string(workspace.root().join("ui-state").join("panel.json")).unwrap(),
            "keep me"
        );
        assert!(workspace
            .repo
            .find_reference("refs/draftline/shelves/before-alternate")
            .is_ok());
    }

    #[test]
    fn previews_version_without_changing_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let preview = workspace.preview_version(first.id()).unwrap();

        assert_eq!(preview.files[0].content.as_deref(), Some("first"));
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn restores_version_as_new_save_without_switching_variations() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let restored = workspace
            .restore_version_as_new_save(first.id(), "Restore first")
            .unwrap();

        assert_eq!(workspace.current_variation().unwrap(), "master");
        assert_eq!(restored.label, "Restore first");
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "first"
        );
        assert!(workspace.recovery_state().unwrap().is_none());
    }

    #[test]
    fn creates_variation_from_version_without_switching() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        write_file(workspace.root(), "post.md", b"first");
        let first = workspace.save_version("First").unwrap();
        write_file(workspace.root(), "post.md", b"second");
        workspace.save_version("Second").unwrap();

        let variation = workspace
            .create_variation_from(first.id(), "recover-first")
            .unwrap();

        assert_eq!(variation.name, "recover-first");
        assert!(!variation.is_current);
        assert_eq!(
            fs::read_to_string(workspace.root().join("post.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn adds_and_lists_remote_endpoints() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(temp.path()).unwrap();

        workspace
            .add_remote("backup", "https://example.invalid/content.git")
            .unwrap();

        let remotes = workspace.remotes().unwrap();
        assert_eq!(
            remotes,
            vec![RemoteEndpoint {
                name: "backup".to_string(),
                url: "https://example.invalid/content.git".to_string(),
            }]
        );
    }

    #[test]
    fn publishes_and_reports_up_to_date_with_local_bare_remote() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();
        let local = tempfile::tempdir().unwrap();
        let workspace = Workspace::init(local.path()).unwrap();
        configure_identity(&workspace, "Seth", "seth@example.com");
        workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();

        write_file(workspace.root(), "post.md", b"hello");
        workspace.save_version("Initial version").unwrap();

        let published = workspace.publish_changes("origin").unwrap();
        assert_eq!(published.published_versions, 0);

        workspace.fetch_remote("origin").unwrap();
        let status = workspace.sync_status("origin").unwrap();
        assert_eq!(status.state, SyncState::UpToDate);
    }

    #[test]
    fn fetch_reports_incoming_versions_and_who_changed_them() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        first_workspace.fetch_remote("origin").unwrap();
        let status = first_workspace.sync_status("origin").unwrap();

        assert_eq!(status.state, SyncState::IncomingAvailable);
        assert_eq!(status.behind, 1);
        assert_eq!(status.incoming[0].label, "Two");
        assert_eq!(status.incoming[0].author.name, "Maria");
    }

    #[test]
    fn publish_refuses_when_remote_has_incoming_changes() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        write_file(first_workspace.root(), "post.md", b"local two");
        first_workspace.save_version("Local two").unwrap();
        first_workspace.fetch_remote("origin").unwrap();

        let err = first_workspace.publish_changes("origin").unwrap_err();
        assert!(matches!(err, DraftlineError::SyncNeedsMerge(_)));
    }

    #[test]
    fn publish_refreshes_remote_before_deciding_safety() {
        let remote = tempfile::tempdir().unwrap();
        Repository::init_bare(remote.path()).unwrap();

        let first = tempfile::tempdir().unwrap();
        let first_workspace = Workspace::init(first.path()).unwrap();
        configure_identity(&first_workspace, "Seth", "seth@example.com");
        first_workspace
            .add_remote("origin", remote.path().to_str().unwrap())
            .unwrap();
        write_file(first_workspace.root(), "post.md", b"one");
        first_workspace.save_version("One").unwrap();
        first_workspace.publish_changes("origin").unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_workspace =
            Workspace::clone_workspace(remote.path().to_str().unwrap(), second.path()).unwrap();
        configure_identity(&second_workspace, "Maria", "maria@example.com");
        write_file(second_workspace.root(), "post.md", b"two");
        second_workspace.save_version("Two").unwrap();
        second_workspace.publish_changes("origin").unwrap();

        write_file(first_workspace.root(), "post.md", b"local two");
        first_workspace.save_version("Local two").unwrap();

        let err = first_workspace.publish_changes("origin").unwrap_err();
        assert!(matches!(err, DraftlineError::SyncNeedsMerge(_)));
    }
}
