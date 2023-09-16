use crate::errors::*;
use crate::new_index::ChainQuery;
use crate::util::{full_hash, FullHash};

use rayon::prelude::*;

use hex;
use std::fs::File;
use std::io;
use std::io::prelude::*;

pub fn precache(chain: &ChainQuery, scripthashes: Vec<FullHash>) {
    let total = scripthashes.len();
    info!("Pre-caching stats and utxo set for {} scripthashes", total);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(16)
        .thread_name(|i| format!("precache-{}", i))
        .build()
        .unwrap();
    pool.install(|| {
        scripthashes
            .par_iter()
            .enumerate()
            .for_each(|(i, scripthash)| {
                if i % 5 == 0 {
                    info!("running pre-cache for scripthash {}/{}", i + 1, total);
                }
                chain.stats(&scripthash[..]);
                //chain.utxo(&scripthash[..]);
            })
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
