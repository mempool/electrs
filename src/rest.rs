use config::Config;

use std::sync::Arc;

use hyper::{Body, Response, Server, Method};
use hyper::service::service_fn_ok;
use hyper::rt::{self, Future};
use std::thread;
use query::Query;
use hyper::Request;
use serde_json;
use util::HeaderEntry;
use elements::Block;
use bitcoin::network::serialize::serialize;
use bitcoin::BitcoinHash;
use bitcoin::util::hash::Sha256dHash;
use std::collections::HashMap;
use url::form_urlencoded;
use serde::Serialize;
use lru_cache::LruCache;
use std::sync::Mutex;


#[derive(Serialize, Deserialize)]
struct BlockValue {
    id: String,
    height: u32,
    timestamp: u32,
    tx_count: u32,
    size: u32,
    weight: u32,
    //tx_hashes: Option<Vec<Sha256dHash>>,
}

impl From<Block> for BlockValue {
    fn from(block: Block) -> Self {
        let weight : usize = block.txdata.iter().fold(0, |sum, val| sum + val.get_weight());
        let serialized_block = serialize(&block).unwrap();
        BlockValue {
            height: block.header.height,
            timestamp: block.header.time,
            tx_count: block.txdata.len() as u32,
            size: serialized_block.len() as u32,
            weight: weight as u32,
            id: block.header.bitcoin_hash().be_hex_string(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BlockAndTxsHashesValue {
    block_summary: BlockValue,
    txs_hashes: Vec<Sha256dHash>,
}

impl From<Block> for BlockAndTxsHashesValue {
    fn from(block: Block) -> Self {
        let txs_hashes = block.txdata.iter().map(|el| el.txid()).collect();

        BlockAndTxsHashesValue {
            block_summary: BlockValue::from(block),
            txs_hashes: txs_hashes,
        }
    }
}

pub fn run_server(_config: &Config, query: Arc<Query>) {

    let addr = ([127, 0, 0, 1], 3000).into();  // TODO take from config
    info!("REST server running on {}", addr);

    let cache = Arc::new(Mutex::new(LruCache::new(100)));

    let new_service = move || {

        let query = query.clone();
        let block_cache = cache.clone();

        //TODO handle errors using service_fn
        service_fn_ok(move |req: Request<Body>| {

            // TODO it looks hyper does not have routing and query parsing :(
            let uri = req.uri();
            let path: Vec<&str> = uri.path().split('/').filter(|el| !el.is_empty()).collect();
            let query_params = match uri.query() {
                Some(value) => form_urlencoded::parse(&value.as_bytes()).into_owned().collect::<HashMap<String, String>>(),
                None => HashMap::new(),
            };

            info!("path {:?} params {:?}", path, query_params);

            match (req.method(), path.get(0), path.get(1), path.get(2)) {
                (&Method::GET, Some(&"blocks"), None, None) => {
                    let limit = query_params.get("limit")
                        .map_or(10u32,|el| el.parse().unwrap_or(10u32) )
                        .min(30u32);
                    match query_params.get("start_height") {
                        Some(height) => {
                            match height.parse::<usize>() {
                                Ok(par) => {
                                    let vec = query.get_headers(&[par]);
                                    match vec.get(0) {
                                        None => bad_request(),
                                        Some(val) => blocks(&query, &val, limit, &block_cache)
                                    }
                                },
                                Err(_) => {
                                    bad_request()
                                }
                            }
                        },
                        None => last_blocks(&query, limit, &block_cache),
                    }
                },
                (&Method::GET, Some(&"block"), Some(par), None) => {
                    match Sha256dHash::from_hex(par) {
                        Ok(par) => {
                            let block = query.get_block_with_cache(&par, &block_cache).unwrap();
                            json_response(BlockAndTxsHashesValue::from(block))
                        },
                        Err(_) => {
                            warn!("can't find block with hash {:?}", par);
                            bad_request()
                        }
                    }
                },
                (&Method::GET, Some(&"block"), Some(str), Some(&"txs")) => {
                    Response::new(Body::from(format!("block with txs not yet implemented {}", str )))
                },
                (&Method::GET, Some(&"tx"), Some(par), None) => {
                    match Sha256dHash::from_hex(par) {
                        Ok(par) => {
                            match query.get_transaction(&par,true) {
                                Ok(value) => { ;
                                    json_response(value)
                                },
                                Err(_) => {
                                    warn!("can't find tx with hash {:?}", par);
                                    bad_request()
                                }
                            }
                        },
                        Err(_) => bad_request()
                    }

                },
                _ => {
                    bad_request()
                }
            }

        })
    };

    let server = Server::bind(&addr)
        .serve(new_service)
        .map_err(|e| eprintln!("server error: {}", e));


    thread::spawn(move || {
        rt::run(server);
    });
}

fn bad_request() -> Response<Body> {
    Response::builder().status(400).body(Body::from("")).unwrap()
}

fn json_response<T: Serialize>(value : T) -> Response<Body> {
    let value = serde_json::to_string(&value).unwrap();
    Response::builder()
        .header("Content-type","application/json")
        .body(Body::from(value)).unwrap()
}

fn last_blocks(query: &Arc<Query>, limit: u32, block_cache : &Mutex<LruCache<Sha256dHash,Block>>) -> Response<Body> {
    let header_entry : HeaderEntry = query.get_best_header().unwrap();
    blocks(query,&header_entry,limit, block_cache)
}

fn blocks(query: &Arc<Query>, header_entry: &HeaderEntry, limit: u32, block_cache : &Mutex<LruCache<Sha256dHash,Block>>) -> Response<Body> {
    let mut values = Vec::new();
    let mut current_hash = header_entry.hash().clone();
    for _ in 0..limit {
        let block : Block = query.get_block_with_cache(&current_hash, block_cache).unwrap();
        current_hash = block.header.prev_blockhash.clone();
        match block.header.height {
            0 => break,
            _ => values.push(BlockValue::from(block)),
        }
    }
    json_response(values)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_fakestore() {
        let x = "a b c d  as asfas ";
        let y : Vec<&str> = x.split(' ').collect();
        println!("{:?}", y);
        let y : Vec<&str> = x.split(' ').filter(|el| !el.is_empty() ).collect();
        println!("{:?}", y);
    }

    #[test]
    fn test_opts() {
        let val = None.map(|el : u32| Some(1));
        println!("{:?}",val);
        let val = Some("1000").map(|el| el.parse().unwrap_or(10u32) )
            .min(Some(30u32)).unwrap();
        println!("{:?}",val);
        let val = None.map_or(10u32,|el: &str| el.parse().unwrap_or(10u32) )
            .min(30u32);
        println!("{:?}",val);
    }
}
