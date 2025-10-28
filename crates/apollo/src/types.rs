/// Apollo-specific configuration that complements reth's config
#[derive(Debug, Clone)]
pub struct ApolloConfig {
    /// Apollo meta server URLs
    pub meta_server: Vec<String>,
    /// App ID in Apollo
    pub app_id: String,
    /// Cluster name (default: "default")
    pub cluster_name: String,
    /// Namespace (default: "application")
    pub namespaces: Option<Vec<String>>,
    /// Optional authentication token
    pub secret: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApolloError {
    #[error("Failed to initialize Apollo client: {0}")]
    ClientInit(String),
    #[error("Failed to stop Apollo client: {0}")]
    ClientStop(String),
    #[error("Invalid namespace: {0}")]
    InvalidNamespace(String),
    #[error("Failed to fetch config: {0}")]
    FetchConfig(String),
    #[error("Config parsing error: {0}")]
    ParseError(String),
}
