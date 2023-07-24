extern crate electrs;

use bincode::Options;
use electrs::{
    config::Config,
    new_index::{Store, TxHistoryKey},
};
use std::cmp::Reverse;

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
    let mut total_entries = 0;
    let mut iter_index = 1;
    // Pre-allocate 40 bytes x capacity
    let mut popular_scripts = Vec::with_capacity(16384);

    while iter.valid() {
        let key = iter.key().unwrap();

        if !key.starts_with(b"H") {
            break;
        }

        if iter_index % 1_000_000 == 0 {
            eprintln!("Processing row #{}...", iter_index);
        }

        let entry: TxHistoryKey = bincode::options()
            .with_big_endian()
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .deserialize(key)
            .expect("failed to deserialize TxHistoryKey");

        if curr_scripthash != entry.hash {
            // We have rolled on to a new scripthash
            // If the last scripthash was popular
            // Collect for sorting
            if total_entries >= 4000 {
                popular_scripts.push(Reverse(ScriptHashFrequency::new(
                    curr_scripthash,
                    total_entries,
                )));
            }

            // After collecting, reset values for next scripthash
            curr_scripthash = entry.hash;
            total_entries = 0;
        }

        total_entries += 1;
        iter_index += 1;

        iter.next();
    }

    eprintln!("Starting sort on {} entries...", popular_scripts.len());
    popular_scripts.sort();
    eprintln!("Finished sort...");

    for Reverse(ScriptHashFrequency { script, count }) in popular_scripts {
        println!("scripthash,{},{}", hex::encode(script), count);
    }
}

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

impl PartialEq for ScriptHashFrequency {
    fn eq(&self, other: &Self) -> bool {
        self.count == other.count
    }
}
impl Eq for ScriptHashFrequency {}
