//! Bridge between SALT (megaETH state tree) and reth.
//!
//! This crate provides the integration layer for using SALT as an alternative
//! state commitment scheme in place of the Merkle Patricia Trie (MPT).
//!
//! # Modules
//!
//! - [`account`]: Account encoding/decoding between reth and SALT formats
//! - [`convert`]: `BundleState` to SALT state updates conversion
//! - [`state_root`]: SALT-based state root computation compatible with reth's pipeline
//! - [`mdbx_store`]: MDBX-backed persistent storage for SALT
//! - [`flat_store`]: Flat-file persistent storage for SALT (Bitcask pattern)

pub mod account;
pub mod async_rocks_store;
pub mod convert;
pub mod flat_store;
pub mod mdbx_store;
pub mod rocks_store;
pub mod seidb_adapter;
pub mod state_root;
