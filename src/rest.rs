use bitcoin::util::hash::{Sha256dHash,HexError};
use bitcoin::network::serialize::{serialize,deserialize};
use bitcoin::{Script,network,BitcoinHash};
use config::Config;
use elements::{Block,TxIn,TxOut,OutPoint,Transaction};
use elements::confidential::{Value,Asset};
use errors;
use hex::{self, FromHexError};
use hyper::{Body, Response, Server, Method, Request, StatusCode};
use hyper::service::service_fn_ok;
use hyper::rt::{self, Future};
use lru_cache::LruCache;
use query::Query;
use serde_json;
use serde::Serialize;
use std::collections::HashMap;
use std::error::Error;
use std::num::ParseIntError;
use std::thread;
use std::sync::{Arc,Mutex};
use url::form_urlencoded;
use daemon::Network;
use util::{HeaderEntry,script_to_address};

#[derive(Serialize, Deserialize)]
struct BlockValue {
    id: String,
    height: u32,
    timestamp: u32,
    tx_count: u32,
    size: u32,
    weight: u32,
    confirmations: Option<u32>,
    previousblockhash: String,
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
            confirmations: None,
            previousblockhash: block.header.prev_blockhash.be_hex_string(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BlockAndTxsValue {
    block_summary: BlockValue,
    txs: Vec<TransactionValue>,

}

impl From<Block> for BlockAndTxsValue {
    fn from(block: Block) -> Self {
        let txs = block.txdata.iter().map(|el| TransactionValue::from(el.clone())).collect();
        let block_value = BlockValue::from(block);

        BlockAndTxsValue {
            block_summary: block_value,
            txs: txs,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TransactionValue {
    txid: Sha256dHash,
    vin: Vec<TxInValue>,
    vout: Vec<TxOutValue>,
    confirmations: Option<u32>,
    hex: Option<String>,
    block_hash: Option<String>,
    size: u32,
    weight: u32,
}

impl From<Transaction> for TransactionValue {
    fn from(tx: Transaction) -> Self {
        let vin = tx.input.iter().map(|el| TxInValue::from(el.clone())).collect();
        let vout = tx.output.iter().map(|el| TxOutValue::from(el.clone())).collect();
        let bytes = serialize(&tx).unwrap();

        TransactionValue {
            txid: tx.txid(),
            vin,
            vout,
            confirmations: None,
            hex: None,
            block_hash: None,
            size: bytes.len() as u32,
            weight: tx.get_weight() as u32,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxInValue {
    outpoint: OutPoint,
    prevout: Option<TxOutValue>,
    scriptsig_hex: Script,
    scriptsig_asm: String,
    is_coinbase: bool,
}

impl From<TxIn> for TxInValue {
    fn from(txin: TxIn) -> Self {
        let is_coinbase = txin.is_coinbase();
        let script = txin.script_sig;
        let script_asm = format!("{:?}",script);

        TxInValue {
            outpoint: txin.previous_output,
            prevout: None, // added later
            scriptsig_asm: (&script_asm[7..script_asm.len()-1]).to_string(),
            scriptsig_hex: script,
            is_coinbase,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct TxOutValue {
    scriptpubkey_hex: Script,
    scriptpubkey_asm: String,
    scriptpubkey_address: Option<String>,
    asset: Option<String>,
    assetcommitment: Option<String>,
    value: Option<u64>,
    scriptpubkey_type: String,
}

impl From<TxOut> for TxOutValue {
    fn from(txout: TxOut) -> Self {
        let asset = match txout.asset {
            Asset::Explicit(value) => Some(value.be_hex_string()),
            _ => None
        };
        let assetcommitment = match txout.asset {
            Asset::Confidential(..) => Some(hex::encode(serialize(&txout.asset).unwrap())),
            _ => None
        };
        let value = match txout.value {
            Value::Explicit(value) => Some(value),
            _ => None,
        };
        let is_fee = txout.is_fee();
        let script = txout.script_pubkey;
        let script_asm = format!("{:?}",script);

        // TODO should the following something to put inside rust-elements lib?
        let script_type = if is_fee {
            "fee"
        } else if script.is_op_return() {
            "op_return"
        } else if script.is_p2pk() {
            "p2pk"
        } else if script.is_p2pkh() {
            "p2pkh"
        } else if script.is_p2sh() {
            "p2sh"
        } else if script.is_v0_p2wpkh() {
            "v0_p2wpkh"
        } else if script.is_v0_p2wsh() {
            "v0_p2wsh"
        } else if script.is_provably_unspendable() {
            "provably_unspendable"
        } else {
            "unknown"
        };

        TxOutValue {
            scriptpubkey_hex: script,
            scriptpubkey_asm: (&script_asm[7..script_asm.len()-1]).to_string(),
            scriptpubkey_address: None, // added later
            asset,
            assetcommitment,
            value,
            scriptpubkey_type: script_type.to_string(),
        }
    }
}

// @XXX we should ideally set the scriptpubkey_address inside TxOutValue::from(), but it
// cannot easily access the Network, so for now, we attach it later with mutation instead

fn attach_tx_data(tx: &mut TransactionValue, network: &Network, query: &Arc<Query>) {
    for mut vin in tx.vin.iter_mut() {
        if vin.is_coinbase { continue; }

        let prevtx = get_tx(&query, &vin.outpoint.txid).unwrap();
        let mut prevout = prevtx.vout[vin.outpoint.vout as usize].clone();
        prevout.scriptpubkey_address = script_to_address(&prevout.scriptpubkey_hex, &network);
        vin.prevout = Some(prevout);
    }

    for mut vout in tx.vout.iter_mut() {
        vout.scriptpubkey_address = script_to_address(&vout.scriptpubkey_hex, &network);
    }
}

fn attach_txs_data(txs: &mut Vec<TransactionValue>, network: &Network, query: &Arc<Query>) {
    for mut tx in txs.iter_mut() {
        attach_tx_data(&mut tx, &network, &query);
    }
}

pub fn run_server(config: &Config, query: Arc<Query>) {
    let addr = ([127, 0, 0, 1], 3000).into();  // TODO take from config
    info!("REST server running on {}", addr);

    let cache = Arc::new(Mutex::new(LruCache::new(100)));

    let network = config.network_type;

    let new_service = move || {

        let query = query.clone();
        let cache = cache.clone();

        service_fn_ok(move |req: Request<Body>| {
            match handle_request(req,&query,&cache,&network) {
                Ok(response) => response,
                Err(e) => {
                    warn!("{:?}",e);
                    bad_request()
                },
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

fn handle_request(req: Request<Body>, query: &Arc<Query>, cache: &Arc<Mutex<LruCache<Sha256dHash, Block>>>, network: &Network) -> Result<Response<Body>, StringError> {
    // TODO it looks hyper does not have routing and query parsing :(
    let uri = req.uri();
    let path: Vec<&str> = uri.path().split('/').skip(1).collect();
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
                    let height = height.parse::<usize>()?;
                    let headers = query.get_headers(&[height]);
                    match headers.get(0) {
                        None => Ok(http_message(StatusCode::NOT_FOUND, format!("can't find header at height {}", height))),
                        Some(val) => blocks(&query, &val, limit, &cache)
                    }
                },
                None => last_blocks(&query, limit, &cache),
            }
        },
        (&Method::GET, Some(&"block-height"), Some(height), None) => {
            let height = height.parse::<usize>()?;
            let vec = query.get_headers(&[height]);
            match vec.get(0) {
                None => return Err(StringError(format!("can't find header at height {}", height))),
                Some(val) => {
                    let block = query.get_block_with_cache(val.hash(), &cache)?;
                    json_response(BlockValue::from(block))
                }
            }
        },
        (&Method::GET, Some(&"block-height"), Some(height), Some(&"with-txs")) => {
            let height = height.parse::<usize>()?;
            let vec = query.get_headers(&[height]);
            match vec.get(0) {
                None => return Err(StringError(format!("can't find header at height {}", height))),
                Some(val) => {
                    let block = query.get_block_with_cache(val.hash(), &cache)?;
                    let header_entry : HeaderEntry = query.get_best_header()?;
                    let confirmations = header_entry.height() as u32 - block.header.height + 1;
                    let mut value = BlockAndTxsValue::from(block);
                    value.block_summary.confirmations = Some(confirmations);
                    for tx_value in value.txs.iter_mut() {
                        tx_value.confirmations = value.block_summary.confirmations;
                        tx_value.block_hash = Some(value.block_summary.id.clone());
                    }
                    attach_txs_data(&mut value.txs, &network, &query);
                    json_response(value)
                }
            }
        },
        (&Method::GET, Some(&"block"), Some(hash), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let block = query.get_block_with_cache(&hash, &cache)?;
            json_response(BlockValue::from(block))
        },
        (&Method::GET, Some(&"block"), Some(hash), Some(&"with-txs")) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let block = query.get_block_with_cache(&hash, &cache)?;
            let block_hash = block.header.bitcoin_hash().be_hex_string();
            let header_entry : HeaderEntry = query.get_best_header()?;
            let confirmations = header_entry.height() as u32 - block.header.height + 1;
            let mut value = BlockAndTxsValue::from(block);
            value.block_summary.confirmations = Some(confirmations);
            for tx_value in value.txs.iter_mut() {
                tx_value.confirmations = Some(confirmations);
                tx_value.block_hash = Some(block_hash.clone());
            }
            attach_txs_data(&mut value.txs, &network, &query);
            json_response(value)
        },
        (&Method::GET, Some(&"tx"), Some(hash), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let mut value = get_tx(&query, &hash)?;
            attach_tx_data(&mut value, &network, &query);
            json_response(value)
        },
        _ => {
            Err(StringError(format!("endpoint does not exist {:?}",uri.path())))
        }
    }
}

fn http_message(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(message))
        .unwrap()
}

fn bad_request() -> Response<Body> {
    // TODO should handle hyper unwrap but it's Error type is private, not sure
    http_message(StatusCode::BAD_REQUEST, "400 Bad Request".to_string())
}

fn json_response<T: Serialize>(value : T) -> Result<Response<Body>,StringError> {
    let value = serde_json::to_string(&value)?;
    Ok(Response::builder()
        .header("Content-type","application/json")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(value)).unwrap())
}

fn get_tx(query: &Arc<Query>, hash: &Sha256dHash) -> Result<TransactionValue, StringError> {
    let tx_value = query.get_transaction(&hash,true)?;
    let tx_hex = tx_value
        .get("hex").ok_or(StringError("hex not in tx json".to_string()))?
        .as_str().ok_or(StringError("hex not a string".to_string()))?;

    let confirmations = match tx_value.get("confirmations") {
        Some(confs) => Some(confs.as_u64().ok_or(StringError("confirmations not a u64".to_string()))? as u32),
        None => None
    };

    let blockhash = match tx_value.get("blockhash") {
        Some(hash) => Some(hash.as_str().ok_or(StringError("blockhash not a string".to_string()))?.to_string()),
        None => None
    };

    let tx : Transaction = deserialize(&hex::decode(tx_hex)? )?;

    let mut value = TransactionValue::from(tx);
    value.confirmations = confirmations;
    value.hex = Some(tx_hex.to_string());
    value.block_hash = blockhash;

    Ok(value)
}

fn last_blocks(query: &Arc<Query>, limit: u32, block_cache : &Mutex<LruCache<Sha256dHash,Block>>) -> Result<Response<Body>,StringError> {
    let header_entry : HeaderEntry = query.get_best_header()?;
    blocks(query,&header_entry,limit, block_cache)
}

fn blocks(query: &Arc<Query>, header_entry: &HeaderEntry, limit: u32, block_cache : &Mutex<LruCache<Sha256dHash,Block>>)
    -> Result<Response<Body>,StringError> {
    let mut values = Vec::new();
    let mut current_hash = header_entry.hash().clone();
    for _ in 0..limit {
        let block : Block = query.get_block_with_cache(&current_hash, block_cache)?;
        current_hash = block.header.prev_blockhash.clone();
        match block.header.height {
            0 => break,
            _ => values.push(BlockValue::from(block)),
        }
    }
    json_response(values)
}

#[derive(Debug)]
struct StringError(String);

// TODO boilerplate conversion, use macros or other better error handling
impl From<ParseIntError> for StringError {
    fn from(e: ParseIntError) -> Self {
        StringError(e.description().to_string())
    }
}
impl From<HexError> for StringError {
    fn from(e: HexError) -> Self {
        StringError(e.description().to_string())
    }
}
impl From<FromHexError> for StringError {
    fn from(e: FromHexError) -> Self {
        StringError(e.description().to_string())
    }
}
impl From<errors::Error> for StringError {
    fn from(e: errors::Error) -> Self {
        StringError(e.description().to_string())
    }
}
impl From<serde_json::Error> for StringError {
    fn from(e: serde_json::Error) -> Self {
        StringError(e.description().to_string())
    }
}
impl From<network::serialize::Error> for StringError {
    fn from(e: network::serialize::Error) -> Self {
        StringError(e.description().to_string())
    }
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


