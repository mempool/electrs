extern crate electrs;

use electrs::{
    config::Config,
    new_index::{Store, TxHistoryKey},
    util::bincode_util,
};

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
    let high_usage_threshold = std::env::var("HIGH_USAGE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(4000);
    eprintln!(
        "Seaching for scripts with history rows of {} or more...",
        high_usage_threshold
    );

    let mut iter = store.history_db().raw_iterator();
    eprintln!("Seeking DB to beginning of tx histories");
    iter.seek(b"H");
    eprintln!("Seeking done");

    let mut curr_scripthash = [0u8; 32];
    let mut total_entries: usize = 0;
    let mut iter_index: usize = 1;
    // Pre-allocate 32 bytes x capacity
    let mut popular_scripts = Vec::with_capacity(16384);

    while iter.valid() {
        let key = iter.key().unwrap();

        if !key.starts_with(b"H") {
            // We have left the txhistory section,
            // but we need to check the final scripthash
            push_if_popular(
                high_usage_threshold,
                total_entries,
                curr_scripthash,
                &mut popular_scripts,
            );
            break;
        }

        if iter_index % 10_000_000 == 0 {
            eprintln!("Processing row #{}...", iter_index);
        }

        let entry: TxHistoryKey = bincode_util::deserialize_big_retry(key)
            .0
            .expect("failed to deserialize TxHistoryKey");

        if curr_scripthash != entry.hash {
            // We have rolled on to a new scripthash
            // If the last scripthash was popular
            // Collect for sorting
            push_if_popular(
                high_usage_threshold,
                total_entries,
                curr_scripthash,
                &mut popular_scripts,
            );

            // After collecting, reset values for next scripthash
            curr_scripthash = entry.hash;
            total_entries = 0;
        }

        total_entries += 1;
        iter_index += 1;

        iter.next();
    }

    for script in popular_scripts {
        println!("{}", hex::encode(script));
    }
}

#[inline]
fn push_if_popular(
    high_usage_threshold: u32,
    total_entries: usize,
    curr_scripthash: [u8; 32],
    popular_scripts: &mut Vec<[u8; 32]>,
) {
    if total_entries >= high_usage_threshold as usize {
        popular_scripts.push(curr_scripthash);
    }
}
