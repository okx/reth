use alloy_primitives::{BlockHash, TxHash};
use alloy_rlp::{decode_exact, encode, Encodable};
use eyre::Report;
use once_cell::sync::OnceCell;
use std::{fmt::Debug, path::PathBuf};

use crate::{
    internal_transaction_inspector::InternalTransaction,
    structs::{BlockTable, OKDBTables, TxTable},
};
use reth_db::{
    create_db,
    mdbx::{Database, DatabaseArguments, Transaction, WriteFlags, RW},
    table::Table,
    DatabaseEnv,
};

static OKDB: OnceCell<DatabaseEnv> = OnceCell::new();

/*
    Default directories:

    Linux: $XDG_DATA_HOME/reth/ or $HOME/.local/share/reth/
    Windows: {FOLDERID_RoamingAppData}/reth/
    MacOS: $HOME/Library/Application Support/reth/

    https://reth.rs/run/config.html
*/
pub fn initialize(db_path: PathBuf) -> Result<(), Report> {
    // New directory (../reth/okdb/)

    let db_create_result = create_db(db_path.join("okdb"), DatabaseArguments::default());
    if let Err(e) = db_create_result {
        return Err(e.wrap_err(format!(
            "ok okdb creation failed at path {}",
            db_path.join("okdb").display()
        )));
    }

    let mut db = db_create_result.unwrap();

    let tables_create_result = db.create_and_track_tables_for::<OKDBTables>();
    if let Err(err) = tables_create_result {
        return Err(Into::<Report>::into(err).wrap_err("ok okdb tables creation failed"));
    }

    let db_set_result = OKDB.set(db);
    if db_set_result.is_err() {
        return Err(Report::msg("ok okdb was initialized more than once"));
    }

    Ok(())
}

pub fn write_single<T: Table, P: Encodable + Debug>(key: Vec<u8>, value: P) -> Result<(), Report> {
    let txn_begin_result = OKDB.get().unwrap().begin_rw_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("ok write single txn begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("ok write single txn open db failed"));
    }

    let table = txn_opendb_result.unwrap();
    let encoded_bytes = encode(&value);

    let txn_put_result = txn.put(table.dbi(), &key, encoded_bytes, WriteFlags::default());
    if let Err(err) = txn_put_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "ok write single txn put failed with key {:#?} and value {:#?}",
            &key, &value
        )));
    }

    let txn_commit_result = txn.commit();
    if let Err(err) = txn_commit_result {
        return Err(Into::<Report>::into(err).wrap_err("ok write single txn commit failed"));
    }

    Ok(())
}

pub fn read_single<T: Table>(key: Vec<u8>) -> Result<Vec<u8>, Report> {
    let txn_begin_result = OKDB.get().unwrap().begin_ro_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("ok read single txn begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("ok read single txn open db failed"));
    }

    let table = txn_opendb_result.unwrap();

    let txn_get_result = txn.get(table.dbi(), &key);
    if let Err(err) = txn_get_result {
        return Err(Into::<Report>::into(err)
            .wrap_err(format!("ok read single txn get failed with key {:#?}", &key)));
    }

    Ok(txn_get_result.unwrap().unwrap_or_default())
}

pub fn delete_single<T: Table>(key: Vec<u8>) -> Result<(), Report> {
    let txn_begin_result = OKDB.get().unwrap().begin_rw_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("ok delete single txn begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("ok delete single txn open db failed"));
    }

    let table = txn_opendb_result.unwrap();

    let txn_delete_result = txn.del(table.dbi(), &key, None);
    if let Err(err) = txn_delete_result {
        return Err(Into::<Report>::into(err)
            .wrap_err(format!("ok delete single txn put failed with key {:#?}", &key)));
    }

    let txn_commit_result = txn.commit();
    if let Err(err) = txn_commit_result {
        return Err(Into::<Report>::into(err).wrap_err("ok delete single txn commit failed"));
    }

    Ok(())
}

pub fn rw_batch_start<T: Table>() -> Result<(Transaction<RW>, Database), Report> {
    let txn_begin_result = OKDB.get().unwrap().begin_rw_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("ok rw batch start begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("ok rw batch start open db failed"));
    }

    Ok((txn, txn_opendb_result.unwrap()))
}

pub fn rw_batch_write<T: Table>(
    txn: &Transaction<RW>,
    table: &Database,
    key: Vec<u8>,
    value: Vec<u8>,
) -> Result<(), Report> {
    let txn_put_result = txn.put(table.dbi(), &key, &value, WriteFlags::default());
    if let Err(err) = txn_put_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "ok rw batch write failed with key {:#?} and value {:#?}",
            &key, &value
        )));
    }

    Ok(())
}

pub fn rw_batch_delete<T: Table>(
    txn: &Transaction<RW>,
    table: &Database,
    key: Vec<u8>,
) -> Result<(), Report> {
    let txn_del_result = txn.del(table.dbi(), &key, None);
    if let Err(err) = txn_del_result {
        return Err(Into::<Report>::into(err)
            .wrap_err(format!("ok rw batch delete failed with key {:#?}", &key)));
    }

    Ok(())
}

pub fn rw_batch_end<T: Table>(txn: Transaction<RW>) -> Result<(), Report> {
    let txn_commit_result = txn.commit();
    if let Err(err) = txn_commit_result {
        return Err(Into::<Report>::into(err).wrap_err("ok rw batch end commit failed"));
    }

    Ok(())
}

pub fn read_table_tx(tx_hash: TxHash) -> Result<Vec<InternalTransaction>, Report> {
    let read_result = read_single::<TxTable>(tx_hash.to_vec());
    if let Err(err) = read_result {
        return Err(err.wrap_err(format!("ok tx table read failed with tx_hash {:#?}", &tx_hash)));
    }

    let encoded_result = read_result.unwrap();
    if encoded_result.is_empty() {
        return Ok(Vec::<InternalTransaction>::default());
    }

    let decode_result = decode_exact(&encoded_result);
    if let Err(err) = decode_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "ok tx table decode failed with encoded result {:#?}",
            &encoded_result
        )));
    }

    Ok(decode_result.unwrap())
}

pub fn read_table_block(block_hash: BlockHash) -> Result<Vec<TxHash>, Report> {
    let read_result = read_single::<BlockTable>(block_hash.to_vec());
    if let Err(err) = read_result {
        return Err(
            err.wrap_err(format!("ok block table read failed with block_hash {:#?}", &block_hash))
        );
    }

    let encoded_result = read_result.unwrap();
    if encoded_result.is_empty() {
        return Ok(Vec::<TxHash>::default());
    }

    let decode_result = decode_exact(&encoded_result);
    if let Err(err) = decode_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "ok block table decode failed with encoded result {:#?}",
            &encoded_result
        )));
    }

    Ok(decode_result.unwrap())
}
