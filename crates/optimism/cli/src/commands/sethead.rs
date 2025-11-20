//! `sethead` command - resets the canonical chain head to a specific block number

use crate::chainspec::OpChainSpecParser;
use alloy_primitives::{BlockNumber, Sealable};
use clap::Parser;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_db::tables;
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW},
    transaction::DbTxMut,
};
use reth_provider::{
    providers::ProviderNodeTypes, BlockNumReader, DatabaseProviderFactory, HeaderProvider,
    ProviderFactory, StaticFileProviderFactory, StageCheckpointWriter,
};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_static_file_types::StaticFileSegment;
use std::sync::Arc;
use tracing::{info, warn};

/// `reth sethead` command
#[derive(Debug, Parser)]
pub struct SetHeadCommand<C: ChainSpecParser = OpChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// The target block number to set as the new chain head
    #[arg(value_name = "BLOCK_NUMBER")]
    pub block_number: BlockNumber,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

impl<C: ChainSpecParser<ChainSpec: EthChainSpec + EthereumHardforks>> SetHeadCommand<C> {
    /// Execute the `sethead` command
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec>>(self) -> eyre::Result<()> {
        info!(
            target: "reth::cli",
            block_number = self.block_number,
            "Preparing to reset chain head"
        );

        // Initialize environment with read-write access
        let Environment { provider_factory, .. } = self.env.init::<N>(AccessRights::RW)?;

        // Execute the sethead operation
        self.execute_sethead(provider_factory)?;

        info!(
            target: "reth::cli",
            block_number = self.block_number,
            "Chain head successfully reset"
        );

        Ok(())
    }

    /// Executes the sethead operation on the database
    fn execute_sethead<N: ProviderNodeTypes>(
        &self,
        provider_factory: ProviderFactory<N>,
    ) -> eyre::Result<()>
    where
        ProviderFactory<N>: DatabaseProviderFactory,
    {
        let target_block = self.block_number;

        // First, verify the target block exists (checking both DB and static files)
        let provider_ro = provider_factory.provider()?;
        let target_header = provider_ro
            .header_by_number(target_block)?
            .ok_or_else(|| eyre::eyre!("Block {target_block} not found in canonical chain"))?;

        let target_hash = target_header.hash_slow();
        
        info!(
            target: "reth::cli",
            block_number = target_block,
            block_hash = ?target_hash,
            "Found target block"
        );

        drop(provider_ro);

        // Get a read-write transaction
        let provider = provider_factory.provider_rw()?;
        let tx = provider.tx_ref();

        // Get the current chain head (from provider which checks both DB and static files)
        let provider_ro_temp = provider_factory.provider()?;
        let current_head = provider_ro_temp.last_block_number()?;
        drop(provider_ro_temp);

        if target_block >= current_head {
            warn!(
                target: "reth::cli",
                target_block,
                current_head,
                "Target block is already at or beyond current head. No action needed."
            );
            return Ok(())
        }

        info!(
            target: "reth::cli",
            current_head,
            target_block,
            blocks_to_remove = current_head - target_block,
            "Will remove blocks from {} to {}",
            target_block + 1,
            current_head
        );

        // Confirm action if not forced
        if !self.force {
            println!(
                "⚠️  WARNING: This will reset the chain head from block {} to block {}",
                current_head, target_block
            );
            println!("   {} blocks will be removed from the canonical chain", current_head - target_block);
            println!("\nAre you sure you want to continue? (y/N): ");

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;

            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Operation cancelled");
                return Ok(())
            }
        }

        // Note: We intentionally do NOT delete static files, as they contain archived data
        // that may no longer exist in the database. The node determines the chain head
        // from the CanonicalHeaders table, not from static files.
        //
        // If you need to also remove static files, you should manually delete them after
        // running this command, being aware that you may need to re-sync that data.

        // Delete all canonical headers beyond the target block from the database
        let mut cursor = tx.cursor_write::<tables::CanonicalHeaders>()?;
        
        // Position cursor at target_block + 1
        if cursor.seek(target_block + 1)?.is_some() {
            let mut deleted_count = 0;

            loop {
                cursor.delete_current()?;
                deleted_count += 1;
                
                // Try to move to the next entry
                if cursor.next()?.is_none() {
                    break;
                }
            }

            info!(
                target: "reth::cli",
                deleted_count,
                "Deleted canonical headers from database"
            );
        }

        drop(cursor);

        // Update stage checkpoints to reflect the new chain head
        // This is crucial because the node determines the best block number from StageId::Finish
        let checkpoint = StageCheckpoint::new(target_block);
        
        info!(
            target: "reth::cli",
            target_block,
            "Updating stage checkpoints to target block"
        );
        
        // Update all relevant stage checkpoints
        provider.save_stage_checkpoint(StageId::Headers, checkpoint)?;
        provider.save_stage_checkpoint(StageId::Bodies, checkpoint)?;
        provider.save_stage_checkpoint(StageId::SenderRecovery, checkpoint)?;
        provider.save_stage_checkpoint(StageId::Execution, checkpoint)?;
        provider.save_stage_checkpoint(StageId::MerkleExecute, checkpoint)?;
        provider.save_stage_checkpoint(StageId::AccountHashing, checkpoint)?;
        provider.save_stage_checkpoint(StageId::StorageHashing, checkpoint)?;
        provider.save_stage_checkpoint(StageId::MerkleUnwind, checkpoint)?;
        provider.save_stage_checkpoint(StageId::TransactionLookup, checkpoint)?;
        provider.save_stage_checkpoint(StageId::IndexAccountHistory, checkpoint)?;
        provider.save_stage_checkpoint(StageId::IndexStorageHistory, checkpoint)?;
        provider.save_stage_checkpoint(StageId::Finish, checkpoint)?; // Most important!
        
        info!(
            target: "reth::cli",
            "Stage checkpoints updated successfully"
        );

        // Commit the transaction
        provider.commit()?;

        Ok(())
    }
}

impl<C: ChainSpecParser> SetHeadCommand<C> {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> &Arc<C::ChainSpec> {
        &self.env.chain
    }
}

