/// A configured place where a workspace can be shared or backed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    pub name: String,
    pub url: String,
}

/// Collaboration status between the current variation and a remote endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    pub remote: String,
    pub variation: String,
    pub ahead: usize,
    pub behind: usize,
    pub state: SyncState,
    pub incoming: Vec<RemoteVersionSummary>,
}

/// High-level sync state for product workflows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    UpToDate,
    LocalAhead,
    IncomingAvailable,
    NeedsMerge,
    NoRemoteVersion,
}

/// Summary of a version available from a remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVersionSummary {
    pub id: String,
    pub label: String,
    pub author: Contributor,
    pub time_seconds: i64,
}

/// Result of publishing local versions to a remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub remote: String,
    pub variation: String,
    pub published_versions: usize,
}

/// Attribution metadata from version history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    pub name: String,
    pub email: Option<String>,
}
