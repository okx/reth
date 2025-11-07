use crate::args::ApolloArgs;
use clap::Args;

/// X Layer Apollo configuration arguments
#[derive(Clone, Debug, Default, Args)]
pub struct XLayerArgs {
    /// Enable Apollo
    #[command(flatten)]
    pub apollo: ApolloArgs,
}
