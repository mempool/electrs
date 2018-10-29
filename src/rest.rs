use bitcoin::util::hash::{Sha256dHash,HexError};
use bitcoin::network::serialize::serialize;
use bitcoin::{Script,network,BitcoinHash};
use config::Config;
use elements::{TxIn,TxOut,OutPoint,Transaction,Proof};
use elements::confidential::{Value,Asset};
use utils::address::Address;
use errors;
use hex::{self, FromHexError};
use hyper::{Body, Response, Server, Method, Request, StatusCode};
use hyper::service::service_fn_ok;
use hyper::rt::{self, Future};
use query::{Query, TxnHeight, FundingOutput, SpendingInput};
use serde_json;
use serde::Serialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::num::ParseIntError;
use std::str::FromStr;
use std::thread;
use std::sync::Arc;
use daemon::Network;
use util::{FullHash, BlockHeaderMeta, TransactionStatus, PegOutRequest, script_to_address, get_script_asm};
use index::compute_script_hash;

const TX_LIMIT: usize = 50;
const BLOCK_LIMIT: usize = 10;

#[derive(Serialize, Deserialize)]
struct BlockValue {
    id: String,
    height: u32,
    timestamp: u32,
    tx_count: u32,
    size: u32,
    weight: u32,
    previousblockhash: Option<String>,
    proof: Option<BlockProofValue>
}

impl From<BlockHeaderMeta> for BlockValue {
    fn from(blockhm: BlockHeaderMeta) -> Self {
        let header = blockhm.header_entry.header();
        BlockValue {
            id: header.bitcoin_hash().be_hex_string(),
            height: blockhm.header_entry.height() as u32,
            proof: Some(BlockProofValue::from(header.proof.clone())),
            timestamp: header.time,
            tx_count: blockhm.meta.tx_count,
            size: blockhm.meta.size,
            weight: blockhm.meta.weight,
            previousblockhash: if &header.prev_blockhash != &Sha256dHash::default() { Some(header.prev_blockhash.be_hex_string()) }
                               else { None },
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BlockProofValue {
    challenge: Script,
    challenge_asm: String,
    solution: Script,
    solution_asm: String,
}
impl From<Proof> for BlockProofValue {
    fn from(proof: Proof) -> Self {
        BlockProofValue {
            challenge_asm: get_script_asm(&proof.challenge),
            challenge: proof.challenge,
            solution_asm: get_script_asm(&proof.solution),
            solution: proof.solution,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TransactionValue {
    txid: Sha256dHash,
    version: u32,
    locktime: u32,
    vin: Vec<TxInValue>,
    vout: Vec<TxOutValue>,
    size: u32,
    weight: u32,
    fee: u64,
    status: Option<TransactionStatus>,
}

impl From<Transaction> for TransactionValue {
    fn from(tx: Transaction) -> Self {
        let vin = tx.input.iter().map(|el| TxInValue::from(el.clone())).collect();
        let vout: Vec<TxOutValue> = tx.output.iter().map(|el| TxOutValue::from(el.clone())).collect();
        let bytes = serialize(&tx).unwrap();
        let fee = vout.iter().find(|vout| vout.scriptpubkey_type == "fee")
                             .map_or(0, |vout| vout.value.unwrap());

        TransactionValue {
            txid: tx.txid(),
            version: tx.version,
            locktime: tx.lock_time,
            vin,
            vout,
            size: bytes.len() as u32,
            weight: tx.get_weight() as u32,
            fee,
            status: None,
        }
    }
}

impl From<TxnHeight> for TransactionValue {
    fn from(t: TxnHeight) -> Self {
        let TxnHeight { txn, height, blockhash } = t;
        let mut value = TransactionValue::from(txn);
        value.status = Some(if height != 0 {
          TransactionStatus { confirmed: true, block_height: Some(height), block_hash: Some(blockhash) }
        } else {
          TransactionStatus::unconfirmed()
        });
        value
    }
}


#[derive(Serialize, Deserialize, Clone)]
struct TxInValue {
    outpoint: OutPoint,
    prevout: Option<TxOutValue>,
    scriptsig_hex: Script,
    scriptsig_asm: String,
    is_coinbase: bool,
    sequence: u32,
}

impl From<TxIn> for TxInValue {
    fn from(txin: TxIn) -> Self {
        let is_coinbase = txin.is_coinbase();
        let script = txin.script_sig;

        TxInValue {
            outpoint: txin.previous_output,
            prevout: None, // added later
            scriptsig_asm: get_script_asm(&script),
            scriptsig_hex: script,
            is_coinbase,
            sequence: txin.sequence,
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
    pegout: Option<PegOutRequest>,
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
        let script_asm = get_script_asm(&script);

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
            scriptpubkey_asm: script_asm,
            scriptpubkey_address: None, // added later
            asset,
            assetcommitment,
            value,
            scriptpubkey_type: script_type.to_string(),
            pegout: None, // added later
        }
    }
}

#[derive(Serialize)]
struct UtxoValue {
    txid: Sha256dHash,
    vout: u32,
    value: u64,
    status: TransactionStatus,
}
impl From<FundingOutput> for UtxoValue {
    fn from(out: FundingOutput) -> Self {
        let FundingOutput { txn, txn_id, output_index, value, .. } = out;
        let TxnHeight { height, blockhash, .. } = txn.unwrap(); // we should never get a FundingOutput without a txn here

        UtxoValue {
            txid: txn_id,
            vout: output_index as u32,
            value: value,
            status: if height != 0 {
              TransactionStatus { confirmed: true, block_height: Some(height), block_hash: Some(blockhash) }
            } else {
              TransactionStatus::unconfirmed()
            }
        }
    }
}

#[derive(Serialize)]
struct SpendingValue {
    spent: bool,
    txid: Option<Sha256dHash>,
    vin: Option<u32>,
    status: Option<TransactionStatus>,
}
impl From<SpendingInput> for SpendingValue {
    fn from(out: SpendingInput) -> Self {
        let SpendingInput { txn, txn_id, input_index, .. } = out;
        let TxnHeight { height, blockhash, .. } = txn.unwrap(); // we should never get a SpendingInput without a txn here

        SpendingValue {
            spent: true,
            txid: Some(txn_id),
            vin: Some(input_index as u32),
            status: Some(if height != 0 {
              TransactionStatus { confirmed: true, block_height: Some(height), block_hash: Some(blockhash) }
            } else {
              TransactionStatus::unconfirmed()
            })
        }
    }
}
impl Default for SpendingValue {
    fn default() -> Self {
        SpendingValue {
            spent: false,
            txid: None,
            vin: None,
            status: None,
        }
    }
}

fn attach_tx_data(tx: TransactionValue, config: &Config, query: &Arc<Query>) -> TransactionValue {
    let mut txs = vec![tx];
    attach_txs_data(&mut txs, config, query);
    txs.remove(0)
}

fn attach_txs_data(txs: &mut Vec<TransactionValue>, config: &Config, query: &Arc<Query>) {
    // a map of prev txids/vouts to lookup, with a reference to the "next in" that spends them
    let mut lookups: BTreeMap<Sha256dHash, Vec<(u32, &mut TxInValue)>> = BTreeMap::new();
    // using BTreeMap ensures the txid keys are in order. querying the db with keys in order leverage memory
    // locality from empirical test up to 2 or 3 times faster

    for mut tx in txs.iter_mut() {
        // collect lookups
        for mut vin in tx.vin.iter_mut() {
            if !vin.is_coinbase && !vin.outpoint.is_pegin {
                lookups.entry(vin.outpoint.txid).or_insert(vec![]).push((vin.outpoint.vout, vin));
            }
        }
        // attach encoded address and pegout info (should ideally happen in TxOutValue::from(),
        // but it cannot easily access the network)
        for mut vout in tx.vout.iter_mut() {
            vout.scriptpubkey_address = script_to_address(&vout.scriptpubkey_hex, &config.network_type);
            vout.pegout = PegOutRequest::parse(&vout.scriptpubkey_hex, &config.parent_network, &config.parent_genesis_hash);
        }
    }

    // fetch prevtxs and attach prevouts to nextins
    for (prev_txid, prev_vouts) in lookups {
        let prevtx = query.tx_get(&prev_txid).unwrap();
        for (prev_out_idx, ref mut nextin) in prev_vouts {
            let mut prevout = TxOutValue::from(prevtx.output[prev_out_idx as usize].clone());
            prevout.scriptpubkey_address = script_to_address(&prevout.scriptpubkey_hex, &config.network_type);
            nextin.prevout = Some(prevout);
        }
    }

}


pub fn run_server(config: &Config, query: Arc<Query>) {
    let addr = ([127, 0, 0, 1], 3000).into();  // TODO take from config
    info!("REST server running on {}", addr);
    let config = Arc::new(config.clone());

    let new_service = move || {

        let query = query.clone();
        let config = config.clone();

        service_fn_ok(move |req: Request<Body>| {
            match handle_request(req,&query,&config) {
                Ok(response) => response,
                Err(e) => {
                    warn!("{:?}",e);
                    http_message(StatusCode::BAD_REQUEST, e.0)
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

fn handle_request(req: Request<Body>, query: &Arc<Query>, config: &Config) -> Result<Response<Body>, StringError> {
    // TODO it looks hyper does not have routing and query parsing :(
    let uri = req.uri();
    let path: Vec<&str> = uri.path().split('/').skip(1).collect();
    info!("path {:?}", path);
    match (req.method(), path.get(0), path.get(1), path.get(2), path.get(3)) {
        (&Method::GET, Some(&"tip"), Some(&"hash"), None, None) =>
            Ok(http_message(StatusCode::OK, query.get_best_header_hash().be_hex_string())),

        (&Method::GET, Some(&"tip"), Some(&"height"), None, None) =>
            Ok(http_message(StatusCode::OK, query.get_best_height().to_string())),

        (&Method::GET, Some(&"blocks"), start_height, None, None) => {
            let start_height = start_height.and_then(|height| height.parse::<usize>().ok());
            blocks(&query, start_height)
        },
        (&Method::GET, Some(&"block-height"), Some(height), None, None) => {
            let height = height.parse::<usize>()?;
            match query.get_headers(&[height]).get(0) {
                None => Ok(http_message(StatusCode::NOT_FOUND, format!("can't find header at height {}", height))),
                Some(val) => Ok(http_message(StatusCode::OK, val.hash().be_hex_string()))
            }
        },
        (&Method::GET, Some(&"block"), Some(hash), None, None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let blockhm = query.get_block_header_with_meta(&hash)?;
            let block_value = BlockValue::from(blockhm);
            json_response(block_value)
        },
        (&Method::GET, Some(&"block"), Some(hash), Some(&"status"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let status = query.get_block_status(&hash);
            json_response(status)
        },
        (&Method::GET, Some(&"block"), Some(hash), Some(&"txs"), start_index) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let block = query.get_block(&hash)?;

            // @TODO optimization: skip deserializing transactions outside of range
            let start_index = start_index
                .map_or(0u32, |el| el.parse().unwrap_or(0))
                .max(0u32) as usize;

            if start_index >= block.txdata.len() {
                return Ok(http_message(StatusCode::NOT_FOUND, "start index out of range".to_string()));
            } else if start_index % TX_LIMIT != 0 {
                return Ok(http_message(StatusCode::BAD_REQUEST, format!("start index must be a multipication of {}", TX_LIMIT)));
            }

            let mut txs = block.txdata.iter().skip(start_index).take(TX_LIMIT).map(|tx| TransactionValue::from(tx.clone())).collect();
            attach_txs_data(&mut txs, config, query);
            json_response(txs)
        },
        (&Method::GET, Some(&"address"), Some(address), None, None) => {
            let script_hash = address_to_scripthash(address, &config.network_type)?;
            let status = query.status(&script_hash[..])?;
            // @TODO create new AddressStatsValue struct?
            json_response(json!({ "address": address, "tx_count": status.history().len(), }))
        },
        (&Method::GET, Some(&"address"), Some(address), Some(&"txs"), start_index) => {
            let start_index = start_index
                .map_or(0u32, |el| el.parse().unwrap_or(0))
                .max(0u32) as usize;

            let script_hash = address_to_scripthash(address, &config.network_type)?;
            let status = query.status(&script_hash[..])?;
            let txs = status.history_txs();

            if txs.len() == 0 {
                return json_response(json!([]));
            } else if start_index >= txs.len() {
                return Ok(http_message(StatusCode::NOT_FOUND, "start index out of range".to_string()));
            } else if start_index % TX_LIMIT != 0 {
                return Ok(http_message(StatusCode::BAD_REQUEST, format!("start index must be a multipication of {}", TX_LIMIT)));
            }

            let mut txs = txs.iter().skip(start_index).take(TX_LIMIT).map(|t| TransactionValue::from((*t).clone())).collect();
            attach_txs_data(&mut txs, config, query);

            json_response(txs)
        },
        (&Method::GET, Some(&"address"), Some(address), Some(&"utxo"), None) => {
            let script_hash = address_to_scripthash(address, &config.network_type)?;
            let status = query.status(&script_hash[..])?;
            let utxos: Vec<UtxoValue> = status.unspent().into_iter().map(|o| UtxoValue::from(o.clone())).collect();
            // @XXX no paging, but query.status() is limited to 30 funding txs
            json_response(utxos)
        },
        (&Method::GET, Some(&"tx"), Some(hash), None, None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let transaction = query.tx_get(&hash).ok_or(StringError("cannot find tx".to_string()))?;
            let mut value = TransactionValue::from(transaction);
            let value = attach_tx_data(value, config, query);
            json_response(value)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"hex"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let rawtx = query.tx_get_raw(&hash).ok_or(StringError("cannot find tx".to_string()))?;
            Ok(http_message(StatusCode::OK, hex::encode(rawtx)))
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"status"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let status = query.get_tx_status(&hash)?;
            json_response(status)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"outspend"), Some(index)) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let outpoint = (hash, index.parse::<usize>()?);
            let spend = query.find_spending_by_outpoint(outpoint)?
                .map_or_else(|| SpendingValue::default(), |spend| SpendingValue::from(spend));
            json_response(spend)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"outspends"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let tx = query.tx_get(&hash).ok_or(StringError("cannot find tx".to_string()))?;
            let spends: Vec<SpendingValue> = query.find_spending_for_funding_tx(tx)?
                .into_iter()
                .map(|spend| spend.map_or_else(|| SpendingValue::default(), |spend| SpendingValue::from(spend)))
                .collect();
            json_response(spends)
        },
        _ => {
            Err(StringError(format!("endpoint does not exist {:?}", uri.path())))
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

fn json_response<T: Serialize>(value : T) -> Result<Response<Body>,StringError> {
    let value = serde_json::to_string(&value)?;
    Ok(Response::builder()
        .header("Content-type","application/json")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(value)).unwrap())
}

fn blocks(query: &Arc<Query>, start_height: Option<usize>)
    -> Result<Response<Body>,StringError> {

    let mut values = Vec::new();
    let mut current_hash = match start_height {
        Some(height) => query.get_headers(&[height]).get(0).ok_or(StringError("cannot find block".to_string()))?.hash().clone(),
        None => query.get_best_header()?.hash().clone(),
    };

    let zero = [0u8;32];
    for _ in 0..BLOCK_LIMIT {
        let blockhm = query.get_block_header_with_meta(&current_hash)?;
        current_hash = blockhm.header_entry.header().prev_blockhash.clone();
        let mut value = BlockValue::from(blockhm);
        value.proof = None;
        values.push(value);

        if &current_hash[..] == &zero[..] {
            break;
        }
    }
    json_response(values)
}

fn address_to_scripthash(addr: &str, network: &Network) -> Result<FullHash, StringError> {
    let addr = Address::from_str(addr)?;
    let addr_network = addr.network;
    if addr_network != *network && !(addr_network == Network::Testnet && *network == Network::LiquidRegtest) {
        return Err(StringError("Address on invalid network".to_string()))
    }
    Ok(compute_script_hash(&addr.script_pubkey().into_bytes()))
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


