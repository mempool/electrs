use crate::errors::*;
use crate::new_index::ChainQuery;
use crate::util::{full_hash, FullHash};

use rayon::prelude::*;

use hex;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::sync::{atomic::AtomicUsize, Arc};
use std::time::Instant;

pub fn precache(chain: Arc<ChainQuery>, scripthashes: Vec<FullHash>, threads: usize) {
    let total = scripthashes.len();
    info!(
        "Pre-caching stats and utxo set on {} threads for {} scripthashes",
        threads, total
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("precache-{}", i))
        .build()
        .unwrap();
    let now = Instant::now();
    let counter = AtomicUsize::new(0);
    std::thread::spawn(move || {
        pool.install(|| {
            scripthashes
                .par_iter()
                .for_each(|scripthash| {
                    // First, cache
                    chain.stats(&scripthash[..], crate::new_index::db::DBFlush::Disable);
                    let _ = chain.utxo(&scripthash[..], usize::MAX, crate::new_index::db::DBFlush::Disable);

                    // Then, increment the counter
                    let pre_increment = counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    let post_increment_counter = pre_increment + 1;

                    // Then, log
                    if post_increment_counter % 500 == 0 {
                        let now_millis = now.elapsed().as_millis();
                        info!("{post_increment_counter}/{total} Processed in {now_millis} ms running pre-cache for scripthash");
                    }

                    // Every 10k counts, flush the DB to disk
                    if post_increment_counter % 10000 == 0 {
                        info!("Flushing cache_db... {post_increment_counter}");
                        chain.store().cache_db().flush();
                        info!("Done Flushing cache_db!!! {post_increment_counter}");
                    }
                })
        });
        // After everything is done, flush the cache
        chain.store().cache_db().flush();
    });
}

pub fn scripthashes_from_file(path: String) -> Result<Vec<FullHash>> {
    let reader =
        io::BufReader::new(File::open(path).chain_err(|| "cannot open precache scripthash file")?);
    reader
        .lines()
        .map(|line| {
            let line = line.chain_err(|| "cannot read scripthash line")?;
            Ok(full_hash(&hex::decode(line).chain_err(|| "invalid hex")?))
        })
        .collect()
}
