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
  -q --release --bin popular-scripts -- \
  --db-dir $ELECTRS_DATA/db \
  > ./contrib/popular-scripts.txt
*/

// Dev note:
// Only use println for file output (lines for output)
// Use eprintln to print to stderr for dev notifications
fn main() {
    let config = Config::from_args();
    let store = Store::open(&config.db_path.join("newindex"), &config);

    let mut iter = store.history_db().raw_iterator();
    eprintln!("Seeking DB to beginning of tx histories");
    iter.seek(b"H");
    eprintln!("Seeking done");

    let mut curr_scripthash = [0u8; 32];
    let mut total_entries: usize = 0;
    let mut iter_index: usize = 1;
    // Pre-allocate 40 bytes x capacity
    let mut popular_scripts = BTreeSet::new();

    while iter.valid() {
        let key = iter.key().unwrap();

        if !key.starts_with(b"H") {
            // We have left the txhistory section,
            // but we need to check the final scripthash
            push_if_popular(total_entries, curr_scripthash, &mut popular_scripts);
            break;
        }

        if iter_index % 1_000_000 == 0 {
            eprintln!("Processing row #{}...", iter_index);
        }

        let entry: TxHistoryKey = bincode_util::deserialize_big_retry(key)
            .0
            .expect("failed to deserialize TxHistoryKey");

        if curr_scripthash != entry.hash {
            // We have rolled on to a new scripthash
            // If the last scripthash was popular
            // Collect for sorting
            push_if_popular(total_entries, curr_scripthash, &mut popular_scripts);

            // After collecting, reset values for next scripthash
            curr_scripthash = entry.hash;
            total_entries = 0;
        }

        total_entries += 1;
        iter_index += 1;

        iter.next();
    }

    for Reverse(ScriptHashFrequency { script, count }) in popular_scripts {
        println!("scripthash,{},{}", hex::encode(script), count);
    }
}

#[inline]
fn push_if_popular(
    total_entries: usize,
    curr_scripthash: [u8; 32],
    popular_scripts: &mut BTreeSet<Reverse<ScriptHashFrequency>>,
) {
    if total_entries >= 4000 {
        popular_scripts.insert(Reverse(ScriptHashFrequency::new(
            curr_scripthash,
            total_entries,
        )));
    }
}

#[derive(PartialEq, Eq)]
struct ScriptHashFrequency {
    script: [u8; 32],
    count: usize,
}

impl ScriptHashFrequency {
    fn new(script: [u8; 32], count: usize) -> Self {
        Self { script, count }
    }
}

impl PartialOrd for ScriptHashFrequency {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.count.partial_cmp(&other.count)
    }
}

impl Ord for ScriptHashFrequency {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.count.cmp(&other.count)
    }
}
