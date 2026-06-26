use serde::{Deserialize, Serialize};

use crate::{Contributor, Result};

/// Host-supplied attribution for a Draftline operation.
///
/// Apps can provide product user/service identity without asking users to edit
/// Git configuration. Draftline still writes normal Git author/committer
/// metadata, but the host owns where the profile comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributorProfile {
    pub author: Contributor,
    pub saved_by: Contributor,
    pub service: Option<Contributor>,
    pub device_id: Option<String>,
}

impl ContributorProfile {
    pub fn new(author: Contributor, saved_by: Contributor) -> Self {
        Self {
            author,
            saved_by,
            service: None,
            device_id: None,
        }
    }

    pub fn with_service(mut self, service: Contributor) -> Self {
        self.service = Some(service);
        self
    }

    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }
}

/// Operation kind passed to host attribution providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttributionOperation {
    SaveVersion,
    SaveFiles,
    ShelveChanges,
    ApplyIncoming,
    MergeIncoming,
    RestoreVersion,
    SupportRef,
}

/// Host callback for operation attribution.
pub trait ContributorProvider: Send {
    fn contributor_profile(
        &mut self,
        operation: AttributionOperation,
    ) -> Result<ContributorProfile>;
}
