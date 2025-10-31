use serde_json::Value as JsonValue;

/// Trait for converting from JsonValue to concrete types\
pub trait FromConfigValue: Sized {
    fn from_config_value(value: &ConfigValue) -> Option<Self>;
}

impl FromConfigValue for u64 {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_u64()
    }
}

impl FromConfigValue for u32 {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_u32()
    }
}

impl FromConfigValue for i64 {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_i64()
    }
}

impl FromConfigValue for i32 {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_i32()
    }
}

impl FromConfigValue for f64 {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_f64()
    }
}

impl FromConfigValue for bool {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_bool()
    }
}

impl FromConfigValue for String {
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        value.as_string().map(|s| s.to_string())
    }
}

impl<T> FromConfigValue for Vec<T>
where
    T: FromConfigValue,
{
    fn from_config_value(value: &ConfigValue) -> Option<Self> {
        match value {
            ConfigValue::Array(values) => values.iter().map(|v| T::from_config_value(v)).collect(),
            _ => None,
        }
    }
}

/// Strongly-typed config value - deserialized once, read many times
#[derive(Debug, Clone)]
pub enum ConfigValue {
    /// 64-bit unsigned integer
    U64(u64),
    /// 32-bit unsigned integer
    U32(u32),
    /// 64-bit signed integer
    I64(i64),
    /// 32-bit signed integer
    I32(i32),
    /// Boolean
    Bool(bool),
    /// String
    String(String),
    /// 64-bit floating point number
    F64(f64),
    /// Array of config values
    Array(Vec<ConfigValue>),
}

impl ConfigValue {
    /// Parse from JsonValue once during cache update
    pub fn from_json(value: &JsonValue) -> Option<Self> {
        if let Some(v) = value.as_u64() {
            Some(ConfigValue::U64(v))
        } else if let Some(v) = value.as_i64() {
            Some(ConfigValue::I64(v))
        } else if let Some(v) = value.as_bool() {
            Some(ConfigValue::Bool(v))
        } else if let Some(v) = value.as_f64() {
            Some(ConfigValue::F64(v))
        } else if let Some(v) = value.as_str() {
            Some(ConfigValue::String(v.to_string()))
        } else {
            None
        }
    }

    /// Convert to 64-bit unsigned integer
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            ConfigValue::U64(v) => Some(*v),
            ConfigValue::U32(v) => Some(*v as u64),
            _ => None,
        }
    }

    /// Convert to 32-bit unsigned integer
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            ConfigValue::U32(v) => Some(*v),
            ConfigValue::U64(v) => (*v).try_into().ok(),
            _ => None,
        }
    }

    /// Convert to 64-bit signed integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ConfigValue::I64(v) => Some(*v),
            ConfigValue::I32(v) => (*v).try_into().ok(),
            _ => None,
        }
    }

    /// Convert to 32-bit signed integer
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            ConfigValue::I32(v) => Some(*v),
            ConfigValue::I64(v) => (*v).try_into().ok(),
            _ => None,
        }
    }

    /// Convert to 64-bit floating point number
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ConfigValue::F64(v) => Some(*v),
            _ => None,
        }
    }

    /// Convert to boolean
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ConfigValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// Convert to string
    pub fn as_string(&self) -> Option<&str> {
        match self {
            ConfigValue::String(v) => Some(v),
            _ => None,
        }
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

/// Apollo error enum
#[derive(Debug, thiserror::Error)]
pub enum ApolloError {
    /// Failed to initialize Apollo client
    #[error("Failed to initialize Apollo client: {0}")]
    ClientInit(String),
    /// Invalid namespace
    #[error("Invalid namespace: {0}")]
    InvalidNamespace(String),
}
