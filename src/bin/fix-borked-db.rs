#![allow(unused)]
extern crate electrs;

use electrs::{
    config::Config,
    new_index::{Store, TxHistoryKey},
    util::bincode_util,
};
use std::{cmp::Reverse, collections::BTreeSet};

/*
// How to run:
export ELECTRS_DATA=/path/to/electrs
cargo run \
  -q --release --bin fix-borked-db -- \
  --db-dir $ELECTRS_DATA/db
*/

fn main() {
    let config = Config::from_args();

    let mut db_opts = rocksdb::Options::default();
    db_opts.create_if_missing(true);
    db_opts.set_max_open_files(100_000);
    db_opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);
    db_opts.set_compression_type(rocksdb::DBCompressionType::None);
    db_opts.set_target_file_size_base(1_073_741_824);
    db_opts.set_write_buffer_size(256 << 20);
    db_opts.set_disable_auto_compactions(true);
    db_opts.set_compaction_readahead_size(1 << 20);
    db_opts.increase_parallelism(2);

    let db = rocksdb::DB::open(&db_opts, config.db_path.join("newindex").join("history"))
        .expect("failed to open RocksDB");

    let mut iter = db.raw_iterator();
    iter.seek(b"H");

    let mut index: usize = 1;

    while iter.valid() {
        let key = iter.key().unwrap();

        if index % 10_000_000 == 0 {
            println!("Parsing row #{}", index);
        }
        if !key.starts_with(b"H") {
            break;
        }

        // Fixint will be big endian u32
        // height has not gone up to 4th byte (yet)
        // Won't reach until height 16777216
        // This will tell us which are NOT varint
        if key[33] == 0 {
            let entry: TxHistoryKey = bincode_util::deserialize_big_retry(key)
                .0
                .expect("failed to deserialize TxHistoryKey");
            let re_serialized = bincode_util::serialize_big(&entry).expect("failed to serialize");
            db.put(re_serialized, []);
            db.delete(key);
        }

        iter.next();
        index += 1;
    }
}
