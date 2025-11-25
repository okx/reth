use alloy_primitives::{BlockHash, TxHash};
use alloy_rlp::{decode_exact, encode, Encodable};
use eyre::Report;
use once_cell::sync::OnceCell;
use std::{fmt::Debug, path::PathBuf};

use crate::{
    internal_transaction_inspector::{InternalTransaction, TxHashWithInternalTransaction},
    structs::{BlockTable, CacheTable, DBTables, TxTable},
};
use reth_db::{
    create_db,
    mdbx::{Database, DatabaseArguments, Transaction, WriteFlags, RW},
    table::Table,
    DatabaseEnv,
};

static XLAYERDB: OnceCell<DatabaseEnv> = OnceCell::new();

pub fn initialize(db_path: PathBuf) -> Result<(), Report> {
    let db_create_result = create_db(db_path.join("xlayerdb"), DatabaseArguments::default());
    if let Err(e) = db_create_result {
        return Err(e.wrap_err(format!(
            "xlayerdb creation failed at path {}",
            db_path.join("xlayerdb").display()
        )));
    }

    let mut db = db_create_result.unwrap();

    let tables_create_result = db.create_and_track_tables_for::<DBTables>();
    if let Err(err) = tables_create_result {
        return Err(Into::<Report>::into(err).wrap_err("xlayerdb tables creation failed"));
    }

    let db_set_result = XLAYERDB.set(db);
    if db_set_result.is_err() {
        return Err(Report::msg("xlayerdb was initialized more than once"));
    }

    Ok(())
}

pub fn write_single<T: Table, Key: Encodable + Debug, Value: Encodable + Debug>(
    key: Key,
    value: Value,
) -> Result<(), Report> {
    let txn_begin_result = XLAYERDB.get().unwrap().begin_rw_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("write single txn begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("write single txn open db failed"));
    }

    let table = txn_opendb_result.unwrap();
    let key_encoded_bytes = encode(&key);
    let value_encoded_bytes = encode(&value);

    let txn_put_result =
        txn.put(table.dbi(), &key_encoded_bytes, value_encoded_bytes, WriteFlags::default());
    if let Err(err) = txn_put_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "write single txn put failed with key {:#?} and value {:#?}",
            &key, &value
        )));
    }

    let txn_commit_result = txn.commit();
    if let Err(err) = txn_commit_result {
        return Err(Into::<Report>::into(err).wrap_err("write single txn commit failed"));
    }

    Ok(())
}

pub fn read_single<T: Table, Key: Encodable + Debug>(key: Key) -> Result<Vec<u8>, Report> {
    let txn_begin_result = XLAYERDB.get().unwrap().begin_ro_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("read single txn begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("read single txn open db failed"));
    }

    let table = txn_opendb_result.unwrap();
    let key_encoded_bytes = encode(&key);

    let txn_get_result = txn.get(table.dbi(), &key_encoded_bytes);
    if let Err(err) = txn_get_result {
        return Err(Into::<Report>::into(err)
            .wrap_err(format!("read single txn get failed with key {:#?}", &key)));
    }

    Ok(txn_get_result.unwrap().unwrap_or_default())
}

pub fn delete_single<T: Table, Key: Encodable + Debug>(key: Key) -> Result<(), Report> {
    let txn_begin_result = XLAYERDB.get().unwrap().begin_rw_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("delete single txn begin failed"));
    }

    let txn = txn_begin_result.unwrap();

    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("delete single txn open db failed"));
    }

    let table = txn_opendb_result.unwrap();
    let key_encoded_bytes = encode(&key);

    let txn_delete_result = txn.del(table.dbi(), &key_encoded_bytes, None);
    if let Err(err) = txn_delete_result {
        return Err(Into::<Report>::into(err)
            .wrap_err(format!("delete single txn put failed with key {:#?}", &key)));
    }

    let txn_commit_result = txn.commit();
    if let Err(err) = txn_commit_result {
        return Err(Into::<Report>::into(err).wrap_err("delete single txn commit failed"));
    }

    Ok(())
}

pub fn rw_batch_start() -> Result<Transaction<RW>, Report> {
    let txn_begin_result = XLAYERDB.get().unwrap().begin_rw_txn();
    if let Err(err) = txn_begin_result {
        return Err(Into::<Report>::into(err).wrap_err("rw batch start begin failed"));
    }

    Ok(txn_begin_result.unwrap())
}

pub fn rw_batch_open_db<T: Table>(txn: &Transaction<RW>) -> Result<Database, Report> {
    let txn_opendb_result = txn.open_db(Some(T::NAME));
    if let Err(err) = txn_opendb_result {
        return Err(Into::<Report>::into(err).wrap_err("rw batch open db failed"));
    }

    Ok(txn_opendb_result.unwrap())
}

pub fn rw_batch_write<Key: Encodable + Debug, Value: Encodable + Debug>(
    txn: &Transaction<RW>,
    table: &Database,
    key: Key,
    value: Value,
) -> Result<(), Report> {
    let key_encoded_bytes = encode(&key);
    let value_encoded_bytes = encode(&value);

    let txn_put_result =
        txn.put(table.dbi(), &key_encoded_bytes, value_encoded_bytes, WriteFlags::default());
    if let Err(err) = txn_put_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "rw batch write failed with key {:#?} and value {:#?}",
            &key, &value
        )));
    }

    Ok(())
}

pub fn rw_batch_delete<Key: Encodable + Debug>(
    txn: &Transaction<RW>,
    table: &Database,
    key: Key,
) -> Result<(), Report> {
    let key_encoded_bytes = encode(&key);

    let txn_del_result = txn.del(table.dbi(), &key_encoded_bytes, None);
    if let Err(err) = txn_del_result {
        return Err(Into::<Report>::into(err)
            .wrap_err(format!("rw batch delete failed with key {:#?}", &key)));
    }

    Ok(())
}

pub fn rw_batch_end(txn: Transaction<RW>) -> Result<(), Report> {
    let txn_commit_result = txn.commit();
    if let Err(err) = txn_commit_result {
        return Err(Into::<Report>::into(err).wrap_err("rw batch end commit failed"));
    }

    Ok(())
}

pub fn read_table_tx(tx_hash: TxHash) -> Result<Vec<InternalTransaction>, Report> {
    let read_result = read_single::<TxTable, TxHash>(tx_hash);
    if let Err(err) = read_result {
        return Err(err.wrap_err(format!("tx table read failed with tx_hash {:#?}", &tx_hash)));
    }

    let encoded_result = read_result.unwrap();
    if encoded_result.is_empty() {
        return Ok(Vec::<InternalTransaction>::default());
    }

    let decode_result = decode_exact(&encoded_result);
    if let Err(err) = decode_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "tx table decode failed with encoded result {:#?}",
            &encoded_result
        )));
    }

    Ok(decode_result.unwrap())
}

pub fn read_table_block(block_hash: BlockHash) -> Result<Vec<TxHash>, Report> {
    let read_result = read_single::<BlockTable, BlockHash>(block_hash);
    if let Err(err) = read_result {
        return Err(
            err.wrap_err(format!("block table read failed with block_hash {:#?}", &block_hash))
        );
    }

    let encoded_result = read_result.unwrap();
    if encoded_result.is_empty() {
        return Ok(Vec::<TxHash>::default());
    }

    let decode_result = decode_exact(&encoded_result);
    if let Err(err) = decode_result {
        return Err(Into::<Report>::into(err).wrap_err(format!(
            "block table decode failed with encoded result {:#?}",
            &encoded_result
        )));
    }

    Ok(decode_result.unwrap())
}

pub fn read_table_cache(
    block_hash: BlockHash,
) -> Result<Vec<TxHashWithInternalTransaction>, Report> {
    let read_result = read_single::<CacheTable, BlockHash>(block_hash);
    if let Err(e) = read_result {
        return Err(
            e.wrap_err(format!("cache table read failed with block_hash {:#?}", &block_hash))
        );
    }

    let encoded_result = read_result.unwrap();
    if encoded_result.is_empty() {
        return Ok(Vec::<TxHashWithInternalTransaction>::default());
    }

    let decode_result = decode_exact(&encoded_result);
    if let Err(e) = decode_result {
        return Err(Into::<Report>::into(e).wrap_err(format!(
            "cache table decode failed with encoded result {:#?}",
            &encoded_result
        )));
    }

    Ok(decode_result.unwrap())
}
