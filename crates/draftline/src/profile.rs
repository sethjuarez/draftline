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

/// The user's Git identity (`user.name` / `user.email`) from global, XDG and
/// system config, for hosts that attribute work before a workspace is open.
///
/// Returns `Ok(None)` when no non-empty `user.name` is configured, and an
/// error when the config cannot be read. Use
/// [`crate::Workspace::git_identity`] once a workspace is open so repository
/// config is honoured too.
pub fn default_git_identity() -> Result<Option<Contributor>> {
    identity_from_config(&git2::Config::open_default()?)
}

pub(crate) fn identity_from_config(config: &git2::Config) -> Result<Option<Contributor>> {
    let value = |key: &str| -> Result<Option<String>> {
        match config.get_string(key) {
            Ok(value) => Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty())),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    };
    let Some(name) = value("user.name")? else {
        return Ok(None);
    };
    Ok(Some(Contributor {
        name,
        email: value("user.email")?,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(lines: &str) -> Option<Contributor> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, lines).unwrap();
        identity_from_config(&git2::Config::open(&path).unwrap()).unwrap()
    }

    #[test]
    fn identity_reads_trimmed_name_and_email() {
        assert_eq!(
            identity("[user]\n\tname = \" Avery \"\n\temail = avery@example.test\n"),
            Some(Contributor {
                name: "Avery".to_string(),
                email: Some("avery@example.test".to_string()),
            })
        );
    }

    #[test]
    fn identity_needs_a_name_but_not_an_email() {
        assert_eq!(identity("[user]\n\temail = a@example.test\n"), None);
        assert_eq!(identity("[user]\n\tname = \"  \"\n"), None);
        assert_eq!(
            identity("[user]\n\tname = Avery\n\temail = \n"),
            Some(Contributor {
                name: "Avery".to_string(),
                email: None,
            })
        );
    }
}
