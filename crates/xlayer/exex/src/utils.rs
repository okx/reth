use alloy_consensus::transaction::TxHashRef;
use alloy_evm::block::BlockExecutor;
use eyre::Result;
use futures::TryStreamExt;
use reth_ethereum::{
    exex::{ExExContext, ExExEvent, ExExNotification},
    node::api::FullNodeComponents,
};
use reth_revm::primitives::alloy_primitives::TxHash;
use reth_tracing::tracing::info;
use xlayer_db::{
    structs::{BlockTable, CacheTable, TxTable},
    utils::{
        read_table_cache, rw_batch_delete, rw_batch_end, rw_batch_open_db, rw_batch_start,
        rw_batch_write,
    },
};

pub async fn post_exec_exex<Node: FullNodeComponents>(mut ctx: ExExContext<Node>) -> Result<()> {
    while let Some(notif) = ctx.notifications.try_next().await? {
        match &notif {
            ExExNotification::ChainCommitted { new } => {
                info!(target: "reth::cli", "xlayer exex ChainCommitted new range {:#?}", new.range());

                let txn = rw_batch_start()?;

                let txtable_handle = rw_batch_open_db::<TxTable>(&txn)?;
                let blocktable_handle = rw_batch_open_db::<BlockTable>(&txn)?;
                let cachetable_handle = rw_batch_open_db::<CacheTable>(&txn)?;

                for block in new.blocks_iter() {
                    let tx_with_internal_transactions = read_table_cache(block.hash())?;
                    let mut tx_hashes = Vec::<TxHash>::default();

                    for data in tx_with_internal_transactions.into_iter() {
                        tx_hashes.push(data.tx_hash);

                        rw_batch_write(
                            &txn,
                            &txtable_handle,
                            data.tx_hash,
                            data.internal_transactions,
                        )?;
                    }

                    if !tx_hashes.is_empty() {
                        rw_batch_write(&txn, &blocktable_handle, block.hash(), tx_hashes)?;
                    }
                    rw_batch_delete(&txn, &cachetable_handle, block.hash())?;
                }
                rw_batch_end(txn)?;

                // Tell Reth “I’m done up to this height” (unblocks pruning & WAL growth):
                ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReorged { old, new } => {
                info!(target: "reth::cli", "xlayer exex ChainReorged old range {:#?} new range {:#?}", old.range(), new.range());

                let txn = rw_batch_start()?;

                let txtable_handle = rw_batch_open_db::<TxTable>(&txn)?;
                let blocktable_handle = rw_batch_open_db::<BlockTable>(&txn)?;

                for block in old.blocks_iter() {
                    for tx in block.transactions_recovered() {
                        rw_batch_delete(&txn, &txtable_handle, tx.tx_hash())?;
                    }

                    rw_batch_delete(&txn, &blocktable_handle, block.hash())?;
                }

                rw_batch_end(txn)?;

                let txn = rw_batch_start()?;

                let txtable_handle = rw_batch_open_db::<TxTable>(&txn)?;
                let blocktable_handle = rw_batch_open_db::<BlockTable>(&txn)?;
                let cachetable_handle = rw_batch_open_db::<CacheTable>(&txn)?;

                for block in new.blocks_iter() {
                    let tx_with_internal_transactions = read_table_cache(block.hash())?;
                    let mut tx_hashes = Vec::<TxHash>::default();

                    for data in tx_with_internal_transactions.into_iter() {
                        tx_hashes.push(data.tx_hash);

                        rw_batch_write(
                            &txn,
                            &txtable_handle,
                            data.tx_hash,
                            data.internal_transactions,
                        )?;
                    }

                    if !tx_hashes.is_empty() {
                        rw_batch_write(&txn, &blocktable_handle, block.hash(), tx_hashes)?;
                    }
                    rw_batch_delete(&txn, &cachetable_handle, block.hash())?;
                }
                rw_batch_end(txn)?;

                ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReverted { old } => {
                info!(target: "reth::cli", "xlayer exex ChainReverted old range {:#?}", old.range());

                let txn = rw_batch_start()?;

                let txtable_handle = rw_batch_open_db::<TxTable>(&txn)?;
                let blocktable_handle = rw_batch_open_db::<BlockTable>(&txn)?;

                for block in old.blocks_iter() {
                    for tx in block.transactions_recovered() {
                        rw_batch_delete(&txn, &txtable_handle, tx.tx_hash())?;
                    }

                    rw_batch_delete(&txn, &blocktable_handle, block.hash())?;
                }

                rw_batch_end(txn)?;
            }
        }
    }
    Ok(())
}
