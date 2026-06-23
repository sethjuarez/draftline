/// A configured place where a workspace can be shared or backed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    pub name: String,
    pub url: String,
}
