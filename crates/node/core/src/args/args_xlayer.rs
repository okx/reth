use crate::args::ApolloArgs;
use clap::Args;

/// X Layer Apollo configuration arguments
#[derive(Clone, Debug, Default, Args)]
#[group(id = "xlayer_apollo_args")]
pub struct XLayerArgs {
    /// Enable Apollo
    #[command(flatten)]
    pub apollo: ApolloArgs,
}
