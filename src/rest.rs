use bitcoin::util::hash::{Sha256dHash,HexError};
use bitcoin::network::serialize::serialize;
use bitcoin::{Script,network,BitcoinHash};
use config::Config;
use elements::{TxIn,TxOut,Transaction,Proof};
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
use std::num::ParseIntError;
use std::str::FromStr;
use std::thread;
use std::sync::Arc;
use daemon::Network;
use util::{FullHash, BlockHeaderMeta, TransactionStatus, PegOutRequest, script_to_address, get_script_asm};
use index::compute_script_hash;

const TX_LIMIT: usize = 25;
const BLOCK_LIMIT: usize = 10;

const TTL_LONG: u32 = 157784630; // ttl for static resources (5 years)
const TTL_SHORT: u32 = 10; // ttl for volatie resources
const CONF_FINAL: usize = 10; // reorgs deeper than this are considered unlikely

#[derive(Serialize, Deserialize)]
struct BlockValue {
    id: String,
    height: u32,
    version: u32,
    timestamp: u32,
    tx_count: u32,
    size: u32,
    weight: u32,
    merkle_root: String,
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
            version: header.version,
            timestamp: header.time,
            tx_count: blockhm.meta.tx_count,
            size: blockhm.meta.size,
            weight: blockhm.meta.weight,
            merkle_root: header.merkle_root.be_hex_string(),
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
          TransactionStatus { confirmed: true, block_height: Some(height as usize), block_hash: Some(blockhash) }
        } else {
          TransactionStatus::unconfirmed()
        });
        value
    }
}


#[derive(Serialize, Deserialize, Clone)]
struct TxInValue {
    txid: Sha256dHash,
    vout: u32,
    prevout: Option<TxOutValue>,
    scriptsig: Script,
    scriptsig_asm: String,
    is_coinbase: bool,
    is_pegin: bool,
    sequence: u32,
    issuance: Option<IssuanceValue>,
}

impl From<TxIn> for TxInValue {
    fn from(txin: TxIn) -> Self {
        let is_coinbase = txin.is_coinbase();

        let zero = [0u8;32];
        let issuance = txin.asset_issuance;
        let is_reissuance = issuance.asset_blinding_nonce != zero;

        let issuance_val = if txin.has_issuance() { Some(IssuanceValue {
            is_reissuance: is_reissuance,
            asset_blinding_nonce: if is_reissuance { Some(hex::encode(issuance.asset_blinding_nonce)) } else { None },
            asset_entropy: if issuance.asset_entropy != zero { Some(hex::encode(issuance.asset_entropy)) } else { None },
            assetamount: match issuance.amount {
                Value::Explicit(value) => Some(value),
                _ => None
            },
            assetamountcommitment: match issuance.amount {
                Value::Confidential(..) => Some(hex::encode(serialize(&issuance.amount).unwrap())),
                _ => None
            },
            tokenamount: match issuance.inflation_keys {
                Value::Explicit(value) => Some(value / 100000000), // https://github.com/ElementsProject/rust-elements/issues/7
                _ => None,
            },
            tokenamountcommitment: match issuance.inflation_keys {
                Value::Confidential(..) => Some(hex::encode(serialize(&issuance.inflation_keys).unwrap())),
                _ => None
            },
        }) } else { None };

        let script = txin.script_sig;

        TxInValue {
            txid: txin.previous_output.txid,
            vout: txin.previous_output.vout,
            is_pegin: txin.previous_output.is_pegin,
            prevout: None, // added later
            scriptsig_asm: get_script_asm(&script),
            scriptsig: script,
            is_coinbase,
            sequence: txin.sequence,
            //issuance: if txin.has_issuance() { Some(IssuanceValue::from(txin.asset_issuance)) } else { None },
            issuance: issuance_val
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct IssuanceValue {
    pub is_reissuance: bool,
    pub asset_blinding_nonce: Option<String>,
    pub asset_entropy: Option<String>,
    pub assetamount: Option<u64>,
    pub assetamountcommitment: Option<String>,
    pub tokenamount: Option<u64>,
    pub tokenamountcommitment: Option<String>,
}

/*
// pending https://github.com/ElementsProject/rust-elements/pull/6
impl From<AssetIssuance> for IssuanceValue {
    fn from(issuance: AssetIssuance) -> Self {
        let zero = [0u8;32];
        let is_reissuance = issuance.asset_blinding_nonce != zero;

        IssuanceValue {
            is_reissuance: is_reissuance,
            asset_blinding_nonce: if is_reissuance { Some(hex::encode(issuance.asset_blinding_nonce)) } else { None },
            asset_entropy: hex::encode(issuance.asset_entropy),
            assetamount: match issuance.amount {
                Asset::Explicit(value) => Some(value.be_hex_string()),
                _ => None
            },
            amountcommitment: match issuance.amount {
                Asset::Confidential(..) => Some(hex::encode(serialize(&issuance.amount).unwrap())),
                _ => None
            },
            tokenamount: match issuance.inflation_keys {
                Value::Explicit(value) => Some(value),
                _ => None,
            },
            tokenamountcommitment: match issuance.inflation_keys {
                Asset::Confidential(..) => Some(hex::encode(serialize(&issuance.inflation_keys).unwrap())),
                _ => None
            },
        }
    }
}
*/

#[derive(Serialize, Deserialize, Clone)]
struct TxOutValue {
    scriptpubkey: Script,
    scriptpubkey_asm: String,
    scriptpubkey_address: Option<String>,
    asset: Option<String>,
    assetcommitment: Option<String>,
    value: Option<u64>,
    valuecommitment: Option<String>,
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
        let valuecommitment = match txout.value {
            Value::Confidential(..) => Some(hex::encode(serialize(&txout.value).unwrap())),
            _ => None
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
            scriptpubkey: script,
            scriptpubkey_asm: script_asm,
            scriptpubkey_address: None, // added later
            asset,
            assetcommitment,
            value,
            valuecommitment,
            scriptpubkey_type: script_type.to_string(),
            pegout: None, // added later
        }
    }
}

#[derive(Serialize)]
struct UtxoValue {
    txid: Sha256dHash,
    vout: u32,
    value: Option<u64>,
    asset: Option<String>,
    status: TransactionStatus,
}
impl From<FundingOutput> for UtxoValue {
    fn from(out: FundingOutput) -> Self {
        let FundingOutput { txn, txn_id, output_index, value, asset, .. } = out;
        let TxnHeight { height, blockhash, .. } = txn.unwrap(); // we should never get a FundingOutput without a txn here

        UtxoValue {
            txid: txn_id,
            vout: output_index as u32,
            value: if value != 0 { Some(value) } else { None },
            asset: asset.map(|val| val.be_hex_string()),
            status: if height != 0 {
              TransactionStatus { confirmed: true, block_height: Some(height as usize), block_hash: Some(blockhash) }
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
              TransactionStatus { confirmed: true, block_height: Some(height as usize), block_hash: Some(blockhash) }
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

fn ttl_by_depth(height: Option<usize>, query: &Query) -> u32 {
    height.map_or(TTL_SHORT, |height| if query.get_best_height() - height >= CONF_FINAL { TTL_LONG }
                                      else { TTL_SHORT })
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
            if !vin.is_coinbase && !vin.is_pegin {
                lookups.entry(vin.txid).or_insert(vec![]).push((vin.vout, vin));
            }
        }
        // attach encoded address and pegout info (should ideally happen in TxOutValue::from(),
        // but it cannot easily access the network)
        for mut vout in tx.vout.iter_mut() {
            vout.scriptpubkey_address = script_to_address(&vout.scriptpubkey, &config.network_type);
            vout.pegout = PegOutRequest::parse(&vout.scriptpubkey, &config.parent_network, &config.parent_genesis_hash);
        }
    }

    // fetch prevtxs and attach prevouts to nextins
    for (prev_txid, prev_vouts) in lookups {
        let prevtx = query.tx_get(&prev_txid).unwrap();
        for (prev_out_idx, ref mut nextin) in prev_vouts {
            let mut prevout = TxOutValue::from(prevtx.output[prev_out_idx as usize].clone());
            prevout.scriptpubkey_address = script_to_address(&prevout.scriptpubkey, &config.network_type);
            nextin.prevout = Some(prevout);
        }
    }

}


pub fn run_server(config: &Config, query: Arc<Query>) {
    let addr = &config.http_addr;
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
                    Response::builder()
                        .status(e.0)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(e.1))
                        .unwrap()
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

fn handle_request(req: Request<Body>, query: &Arc<Query>, config: &Config) -> Result<Response<Body>, HttpError> {
    // TODO it looks hyper does not have routing and query parsing :(
    let uri = req.uri();
    let path: Vec<&str> = uri.path().split('/').skip(1).collect();
    info!("path {:?}", path);
    match (req.method(), path.get(0), path.get(1), path.get(2), path.get(3)) {
        (&Method::GET, Some(&"blocks"), Some(&"tip"), Some(&"hash"), None) =>
            http_message(StatusCode::OK, query.get_best_header_hash().be_hex_string(), TTL_SHORT),

        (&Method::GET, Some(&"blocks"), Some(&"tip"), Some(&"height"), None) =>
            http_message(StatusCode::OK, query.get_best_height().to_string(), TTL_SHORT),

        (&Method::GET, Some(&"blocks"), start_height, None, None) => {
            let start_height = start_height.and_then(|height| height.parse::<usize>().ok());
            blocks(&query, start_height)
        },
        (&Method::GET, Some(&"block-height"), Some(height), None, None) => {
            let height = height.parse::<usize>()?;
            let headers = query.get_headers(&[height]);
            let header = headers.get(0).ok_or_else(|| HttpError::not_found("Block not found".to_string()))?;
            let ttl = ttl_by_depth(Some(height), query);
            http_message(StatusCode::OK, header.hash().be_hex_string(), ttl)
        },
        (&Method::GET, Some(&"block"), Some(hash), None, None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let blockhm = query.get_block_header_with_meta(&hash)?;
            let block_value = BlockValue::from(blockhm);
            json_response(block_value, TTL_LONG)
        },
        (&Method::GET, Some(&"block"), Some(hash), Some(&"status"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let status = query.get_block_status(&hash);
            let ttl = ttl_by_depth(status.height, query);
            json_response(status, ttl)
        },
        (&Method::GET, Some(&"block"), Some(hash), Some(&"txids"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let txids = query.get_block_txids(&hash).map_err(|_| HttpError::not_found("Block not found".to_string()))?;
            json_response(txids, TTL_LONG)
        },
        (&Method::GET, Some(&"block"), Some(hash), Some(&"txs"), start_index) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let txids = query.get_block_txids(&hash).map_err(|_| HttpError::not_found("Block not found".to_string()))?;

            let start_index = start_index
                .map_or(0u32, |el| el.parse().unwrap_or(0))
                .max(0u32) as usize;
            if start_index >= txids.len() {
                bail!(HttpError::not_found("start index out of range".to_string()));
            } else if start_index % TX_LIMIT != 0 {
                bail!(HttpError::from(format!("start index must be a multipication of {}", TX_LIMIT)));
            }

            let mut txs = txids.iter().skip(start_index).take(TX_LIMIT)
                .map(|txid| query.tx_get(&txid).map(TransactionValue::from).ok_or("missing tx".to_string()))
                .collect::<Result<Vec<TransactionValue>, _>>()?;
            attach_txs_data(&mut txs, config, query);
            json_response(txs, TTL_LONG)
        },
        (&Method::GET, Some(&"address"), Some(address), None, None) => {
            // @TODO create new AddressStatsValue struct?
            let script_hash = address_to_scripthash(address, &config.network_type)?;
            match query.status(&script_hash[..]) {
                Ok(status) => json_response(json!({
                    "address": address,
                    "tx_count": status.history().len(),
                }), TTL_SHORT),

                // if the address has too many txs, just return the address with no additional info (but no error)
                Err(errors::Error(errors::ErrorKind::Msg(ref msg), _)) if *msg == "Too many txs".to_string() =>
                    json_response(json!({ "address": address }), TTL_SHORT),

                Err(err) => bail!(err)
            }
        },
        (&Method::GET, Some(&"address"), Some(address), Some(&"txs"), start_index) => {
            let start_index = start_index
                .map_or(0u32, |el| el.parse().unwrap_or(0))
                .max(0u32) as usize;

            let script_hash = address_to_scripthash(address, &config.network_type)?;
            let status = query.status(&script_hash[..])?;
            let txs = status.history_txs();

            if txs.len() == 0 {
                return json_response(json!([]), TTL_SHORT);
            } else if start_index >= txs.len() {
                bail!(HttpError::not_found("start index out of range".to_string()));
            } else if start_index % TX_LIMIT != 0 {
                bail!(HttpError::from(format!("start index must be a multipication of {}", TX_LIMIT)));
            }

            let mut txs = txs.iter().skip(start_index).take(TX_LIMIT).map(|t| TransactionValue::from((*t).clone())).collect();
            attach_txs_data(&mut txs, config, query);

            json_response(txs, TTL_SHORT)
        },
        (&Method::GET, Some(&"address"), Some(address), Some(&"utxo"), None) => {
            let script_hash = address_to_scripthash(address, &config.network_type)?;
            let status = query.status(&script_hash[..])?;
            let utxos: Vec<UtxoValue> = status.unspent().into_iter().map(|o| UtxoValue::from(o.clone())).collect();
            // @XXX no paging, but query.status() is limited to 30 funding txs
            json_response(utxos, TTL_SHORT)
        },
        (&Method::GET, Some(&"tx"), Some(hash), None, None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let transaction = query.tx_get(&hash).ok_or(HttpError::not_found("Transaction not found".to_string()))?;
            let status = query.get_tx_status(&hash)?;
            let ttl = ttl_by_depth(status.block_height, query);

            let mut value = TransactionValue::from(transaction);
            value.status = Some(status);
            let value = attach_tx_data(value, config, query);
            json_response(value, ttl)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"hex"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let rawtx = query.tx_get_raw(&hash).ok_or(HttpError::not_found("Transaction not found".to_string()))?;
            let ttl = ttl_by_depth(query.get_tx_status(&hash)?.block_height, query);
            http_message(StatusCode::OK, hex::encode(rawtx), ttl)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"status"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let status = query.get_tx_status(&hash)?;
            let ttl = ttl_by_depth(status.block_height, query);
            json_response(status, ttl)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"merkle-proof"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let status = query.get_tx_status(&hash)?;
            if !status.confirmed { bail!("Transaction is unconfirmed".to_string()) };
            let proof = query.get_merkle_proof(&hash, &status.block_hash.unwrap())?;
            let ttl = ttl_by_depth(status.block_height, query);
            json_response(proof, ttl)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"outspend"), Some(index)) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let outpoint = (hash, index.parse::<usize>()?);
            let spend = query.find_spending_by_outpoint(outpoint)?
                .map_or_else(|| SpendingValue::default(), |spend| SpendingValue::from(spend));
            let ttl = ttl_by_depth(spend.status.as_ref().and_then(|ref status| status.block_height), query);
            json_response(spend, ttl)
        },
        (&Method::GET, Some(&"tx"), Some(hash), Some(&"outspends"), None) => {
            let hash = Sha256dHash::from_hex(hash)?;
            let tx = query.tx_get(&hash).ok_or(HttpError::not_found("Transaction not found".to_string()))?;
            let spends: Vec<SpendingValue> = query.find_spending_for_funding_tx(tx)?
                .into_iter()
                .map(|spend| spend.map_or_else(|| SpendingValue::default(), |spend| SpendingValue::from(spend)))
                .collect();
            // @TODO long ttl if all outputs are either spent long ago or unspendable
            json_response(spends, TTL_SHORT)
        },
        _ => {
            Err(HttpError::not_found(format!("endpoint does not exist {:?}", uri.path())))
        }
    }
}

fn http_message(status: StatusCode, message: String, ttl: u32) -> Result<Response<Body>,HttpError> {
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .header("Cache-Control", format!("public, max-age={:}", ttl))
        .body(Body::from(message))
        .unwrap())
}

fn json_response<T: Serialize>(value : T, ttl: u32) -> Result<Response<Body>,HttpError> {
    let value = serde_json::to_string(&value)?;
    Ok(Response::builder()
        .header("Content-Type","application/json")
        .header("Cache-Control", format!("public, max-age={:}", ttl))
        .body(Body::from(value))
        .unwrap())
}

fn blocks(query: &Arc<Query>, start_height: Option<usize>)
    -> Result<Response<Body>,HttpError> {

    let mut values = Vec::new();
    let mut current_hash = match start_height {
        Some(height) => query.get_headers(&[height]).get(0).ok_or(HttpError::not_found("Block not found".to_string()))?.hash().clone(),
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
    json_response(values, TTL_SHORT)
}

fn address_to_scripthash(addr: &str, network: &Network) -> Result<FullHash, HttpError> {
    let addr = Address::from_str(addr)?;
    let addr_network = addr.network;
    if addr_network != *network && !(addr_network == Network::Testnet && *network == Network::LiquidRegtest) {
        bail!(HttpError::from("Address on invalid network".to_string()))
    }
    Ok(compute_script_hash(&addr.script_pubkey().into_bytes()))
}

#[derive(Debug)]
struct HttpError(StatusCode, String);

impl HttpError {
    fn not_found(msg: String) -> Self {
        HttpError(StatusCode::NOT_FOUND, msg)
    }
    fn generic() -> Self {
        HttpError::from("We encountered an error. Please try again later.".to_string())
    }
}

impl From<String> for HttpError {
    fn from(msg: String) -> Self {
        HttpError(StatusCode::BAD_REQUEST, msg)
    }
}
impl From<ParseIntError> for HttpError {
    fn from(_e: ParseIntError) -> Self {
        //HttpError::from(e.description().to_string())
        HttpError::from("Invalid number".to_string())
    }
}
impl From<HexError> for HttpError {
    fn from(_e: HexError) -> Self {
        //HttpError::from(e.description().to_string())
        HttpError::from("Invalid hex string".to_string())
    }
}
impl From<FromHexError> for HttpError {
    fn from(_e: FromHexError) -> Self {
        //HttpError::from(e.description().to_string())
        HttpError::from("Invalid hex string".to_string())
    }
}
impl From<errors::Error> for HttpError {
    fn from(e: errors::Error) -> Self {
        warn!("errors::Error: {:?}", e);
        match e.description().to_string().as_ref() {
            "getblock RPC error: {\"code\":-5,\"message\":\"Block not found\"}" => HttpError::not_found("Block not found".to_string()),
            "Too many txs" => HttpError(StatusCode::TOO_MANY_REQUESTS, "Sorry! Addresses with a large number of transactions aren\'t currently supported.".to_string()),
            _ => HttpError::generic()
        }
    }
}
impl From<serde_json::Error> for HttpError {
    fn from(_e: serde_json::Error) -> Self {
        //HttpError::from(e.description().to_string())
        HttpError::generic()
    }
}
impl From<network::serialize::Error> for HttpError {
    fn from(_e: network::serialize::Error) -> Self {
        //HttpError::from(e.description().to_string())
        HttpError::generic()
    }
}

