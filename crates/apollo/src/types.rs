use serde_json::Value as JsonValue;

/// Trait for converting from JsonValue to concrete types
pub trait FromJsonValue: Sized {
    /// Convert from JsonValue to concrete type
    fn from_json_value(value: &JsonValue) -> Option<Self>;
}

impl FromJsonValue for u64 {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_u64()
    }
}

impl FromJsonValue for i64 {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_i64()
    }
}

impl FromJsonValue for i32 {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_i64().and_then(|n| n.try_into().ok())
    }
}

impl FromJsonValue for u32 {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_u64().and_then(|n| n.try_into().ok())
    }
}

impl FromJsonValue for f64 {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_f64()
    }
}

impl FromJsonValue for bool {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_bool()
    }
}

impl FromJsonValue for String {
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_str().map(|s| s.to_string())
    }
}

impl<T> FromJsonValue for Vec<T>
where
    T: FromJsonValue,
{
    fn from_json_value(value: &JsonValue) -> Option<Self> {
        value.as_array()?.iter().map(|v| T::from_json_value(v)).collect()
    }
}

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
