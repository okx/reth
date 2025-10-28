use clap::Args;
use reth_chainspec::{ChainKind, NamedChain};
use std::path::Path;
use url::Url;

/// Syncs ERA1 encoded blocks from a local or remote source.
#[derive(Clone, Debug, Default, Args)]
pub struct ApolloArgs {
    /// Enable Apollo config loading.
    #[arg(
        id = "apollo.enable",
        long = "apollo.enable",
        value_name = "APOLLO_ENABLE",
        default_value_t = false
    )]
    pub enabled: bool,

    /// Describes where to get the Apollo config from.
    #[clap(flatten)]
    pub source: ApolloSourceArgs,
}

/// Arguments for the block history import based on ERA1 encoded files.
#[derive(Clone, Debug, Default, Args)]
#[group(required = false, multiple = false)]
pub struct ApolloSourceArgs {
    /// The path to a directory for import.
    ///
    /// The Apollo config is read from the local directory.
    #[arg(long = "apollo.path", value_name = "APOLLO_PATH", verbatim_doc_comment)]
    pub path: Option<Box<Path>>,

    /// The URL to a remote host where the Apollo config is hosted.
    ///
    /// The Apollo config is read from the remote host using HTTP GET requests.
    #[arg(long = "apollo.url", value_name = "APOLLO_URL", verbatim_doc_comment)]
    pub url: Option<Url>,
}

/// The `ExtractApolloHost` trait allows to derive a default URL host for Apollo config.
pub trait DefaultApolloHost {
    /// Converts `self` into [`Url`] index page of the Apollo host.
    ///
    /// Returns `Err` if the conversion is not possible.
    fn default_apollo_host(&self) -> Option<Url>;
}

impl DefaultApolloHost for ChainKind {
    fn default_apollo_host(&self) -> Option<Url> {
        Some(match self {
            Self::Named(NamedChain::Mainnet) => {
                Url::parse("https://era.ithaca.xyz/era1/index.html").expect("URL should be valid")
            }
            Self::Named(NamedChain::Sepolia) => {
                Url::parse("https://era.ithaca.xyz/sepolia-era1/index.html")
                    .expect("URL should be valid")
            }
            _ => return None,
        })
    }
}
