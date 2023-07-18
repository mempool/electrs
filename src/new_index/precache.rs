use crate::chain::address::Address;
use crate::errors::*;
use crate::new_index::ChainQuery;
use crate::util::{full_hash, FullHash};

use rayon::prelude::*;
use sha2::{Digest, Sha256};

use hex;
use std::convert::TryInto;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::str::FromStr;

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
            let cols: Vec<&str> = line.split(',').collect();
            to_scripthash(cols[0], cols[1])
        })
        .collect()
}

fn to_scripthash(script_type: &str, script_str: &str) -> Result<FullHash> {
    match script_type {
        "address" => address_to_scripthash(script_str),
        "scripthash" => Ok(full_hash(
            &hex::decode(script_str).chain_err(|| "invalid hex")?,
        )),
        "scriptpubkey" => Ok(compute_script_hash(
            &hex::decode(script_str).chain_err(|| "invalid hex")?,
        )),
        _ => bail!("Invalid script type".to_string()),
    }
}

fn address_to_scripthash(addr: &str) -> Result<FullHash> {
    let addr = Address::from_str(addr).chain_err(|| "invalid address")?;
    Ok(compute_script_hash(addr.script_pubkey().as_bytes()))
}

pub fn compute_script_hash(data: &[u8]) -> FullHash {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()[..]
        .try_into()
        .expect("SHA256 size is 32 bytes")
}
