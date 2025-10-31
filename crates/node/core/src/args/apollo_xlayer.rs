use clap::Args;

/// X Layer Apollo configuration arguments
#[derive(Clone, Debug, Default, Args)]
pub struct ApolloArgs {
    /// Enable Apollo
    #[arg(long = "apollo.enable", default_value_t = false)]
    pub enabled: bool,

    /// Configure Apollo app ID.
    #[arg(long = "apollo.app-id", default_value = "")]
    pub apollo_app_id: String,

    /// Configure Apollo IP.
    #[arg(long = "apollo.ip", default_value = "")]
    pub apollo_ip: String,

    /// Configure Apollo cluster.
    #[arg(long = "apollo.cluster", default_value = "")]
    pub apollo_cluster: String,

    /// Configure Apollo namespace.
    #[arg(long = "apollo.namespace", default_value = "")]
    pub apollo_namespace: String,
}
