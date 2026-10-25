use serde::{Deserialize, Serialize};

/// Incomplete or recently completed Draftline operation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryState {
    pub operation_id: String,
    pub operation: RecoveryOperation,
    pub original_variation: Option<String>,
    pub target: Option<String>,
    pub completed: bool,
}

/// Kind of operation recorded in the recovery ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryOperation {
    SwitchVariation,
    RestoreVersionAsNewSave,
    ShelveChanges,
    ApplyIncoming,
    MergeIncoming,
    DiscardChanges,
    DiscardFile,
    DeleteVariation,
    ApplyShelf,
    DeleteRemoteVariation,
    ExpireSupportRefs,
    RenameVariation,
    /// Moving the folder Draftline manages to another path.
    MoveFolder,
    /// Recorded by older versions for features that no longer exist. Still
    /// read so those workspaces open; repair reports them unavailable.
    MilestoneMerge,
    SquashVersions,
    HistoryCleanup,
}
