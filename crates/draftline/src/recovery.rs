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
    SquashVersions,
    ApplyShelf,
    DeleteRemoteVariation,
    ExpireSupportRefs,
}
