extern crate electrs;

use std::{convert::TryInto, thread::ThreadId, time::Instant};

use electrs::{config::Config, new_index::db::open_raw_db};
use lazy_static::lazy_static;

/*
// How to run:
export ELECTRS_DATA=/path/to/electrs
cargo run \
  -q --release --bin popular-scripts -- \
  --db-dir $ELECTRS_DATA/db \
  > ./contrib/popular-scripts.txt
*/

type DB = rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>;
lazy_static! {
    static ref HISTORY_DB: DB = {
        let config = Config::from_args();
        open_raw_db(&config.db_path.join("newindex").join("history"))
    };
}

// Dev note:
// Only use println for file output (lines for output)
// Use eprintln to print to stderr for dev notifications
fn main() {
    let high_usage_threshold = std::env::var("HIGH_USAGE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(4000);
    let thread_count = std::env::var("JOB_THREAD_COUNT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4);
    eprintln!(
        "Seaching for scripts with history rows of {} or more...",
        high_usage_threshold
    );

    let thread_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .expect("Built threadpool");

    let (sender, receiver) = crossbeam_channel::unbounded::<[u8; 32]>();

    let increment = 256 / thread_count;
    let bytes: Vec<u8> = (0u8..=255u8)
        .filter(|n| *n % increment as u8 == 0)
        .collect();

    let now = Instant::now();
    for i in 0..bytes.len() {
        let sender = sender.clone();
        let first_byte = bytes[i];
        let second_byte = bytes.get(i + 1).copied();

        thread_pool.spawn(move || {
            let id = std::thread::current().id();
            run_iterator(
                id,
                &HISTORY_DB,
                high_usage_threshold,
                first_byte,
                second_byte,
                sender,
                now,
            );
            eprintln!("{id:?} Finished its job!");
        })
    }
    // If we don't drop this sender
    // the receiver will hang forever
    drop(sender);

    while let Ok(script) = receiver.recv() {
        println!("{}", hex::encode(script));
    }
    eprintln!("Finished!!!!");
}

fn run_iterator(
    thread_id: ThreadId,
    db: &DB,
    high_usage_threshold: u32,
    first_byte: u8,
    next_byte: Option<u8>,
    sender: crossbeam_channel::Sender<[u8; 32]>,
    now: Instant,
) {
    let mut iter = db.raw_iterator();
    eprintln!(
        "Thread ({thread_id:?}) Seeking DB to beginning of tx histories for b'H' + {}",
        hex::encode([first_byte])
    );
    // H = 72
    let mut compare_vec: Vec<u8> = vec![72, first_byte];
    iter.seek(&compare_vec); // Seek to beginning of our section

    // Insert the byte of the next section for comparing
    // This will tell us when to stop with a closure
    type Checker<'a> = Box<dyn Fn(&[u8]) -> bool + 'a>;
    let is_finished: Checker<'_> = if let Some(next) = next_byte {
        // Modify the vec to what we're looking for next
        // to indicate we left our section
        compare_vec[1] = next;
        Box::new(|key: &[u8]| -> bool { key.starts_with(&compare_vec) })
    } else {
        // Modify the vec to only have H so we know when we left H
        compare_vec.remove(1);
        Box::new(|key: &[u8]| -> bool { !key.starts_with(&compare_vec) })
    };

    eprintln!("Thread ({thread_id:?}) Seeking done");

    let mut curr_scripthash = [0u8; 32];
    let mut total_entries: usize = 0;
    let mut iter_index: usize = 1;

    while iter.valid() {
        let key = iter.key().unwrap();

        if is_finished(key) {
            // We have left the txhistory section,
            // but we need to check the final scripthash
            send_if_popular(
                high_usage_threshold,
                total_entries,
                curr_scripthash,
                &sender,
            );
            break;
        }

        if iter_index % 10_000_000 == 0 {
            let duration = now.elapsed().as_secs();
            eprintln!(
                "Thread ({thread_id:?}) Processing row #{iter_index}... {duration} seconds elapsed"
            );
        }

        // We know that the TxHistory key is 1 byte "H" followed by
        // 32 byte scripthash
        let entry_hash: [u8; 32] = key[1..33].try_into().unwrap();

        if curr_scripthash != entry_hash {
            // We have rolled on to a new scripthash
            // If the last scripthash was popular
            // Collect for sorting
            send_if_popular(
                high_usage_threshold,
                total_entries,
                curr_scripthash,
                &sender,
            );

            // After collecting, reset values for next scripthash
            curr_scripthash = entry_hash;
            total_entries = 0;
        }

        total_entries += 1;
        iter_index += 1;

        iter.next();
    }
}

#[inline]
fn send_if_popular(
    high_usage_threshold: u32,
    total_entries: usize,
    curr_scripthash: [u8; 32],
    sender: &crossbeam_channel::Sender<[u8; 32]>,
) {
    if total_entries >= high_usage_threshold as usize {
        sender.send(curr_scripthash).unwrap();
    }
}
