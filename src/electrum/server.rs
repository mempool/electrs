use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::TryInto;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(feature = "electrum-discovery")]
use std::net::IpAddr;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use bitcoin::hashes::sha256d::Hash as Sha256dHash;
use error_chain::ChainedError;
use hex;
use proxy_protocol::{version1, version2, ProxyHeader};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[cfg(not(feature = "liquid"))]
use bitcoin::consensus::encode::serialize;
#[cfg(feature = "liquid")]
use elements::encode::serialize;

use crate::chain::Txid;
use crate::config::{Config, VERSION_STRING};
use crate::electrum::{get_electrum_height, ProtocolVersion};
use crate::errors::*;
use crate::metrics::{Gauge, HistogramOpts, HistogramVec, MetricOpts, Metrics};
use crate::new_index::{Query, Utxo};
use crate::util::electrum_merkle::{get_header_merkle_proof, get_id_from_pos, get_tx_merkle_proof};
use crate::util::{
    create_socket, full_hash, spawn_thread, BlockId, BoolThen, Channel, FullHash, HeaderEntry,
    SyncChannel,
};

const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 4);
const MAX_HEADERS: usize = 2016;

#[cfg(feature = "electrum-discovery")]
use crate::electrum::{DiscoveryManager, ServerFeatures};

// TODO: Sha256dHash should be a generic hash-container (since script hash is single SHA256)
fn hash_from_value(val: Option<&Value>) -> Result<Sha256dHash> {
    let script_hash = val.chain_err(|| "missing hash")?;
    let script_hash = script_hash.as_str().chain_err(|| "non-string hash")?;
    let script_hash = script_hash.parse().chain_err(|| "non-hex hash")?;
    Ok(script_hash)
}

fn usize_from_value(val: Option<&Value>, name: &str) -> Result<usize> {
    let val = val.chain_err(|| format!("missing {}", name))?;
    let val = val.as_u64().chain_err(|| format!("non-integer {}", name))?;
    Ok(val as usize)
}

fn usize_from_value_or(val: Option<&Value>, name: &str, default: usize) -> Result<usize> {
    if val.is_none() {
        return Ok(default);
    }
    usize_from_value(val, name)
}

fn bool_from_value(val: Option<&Value>, name: &str) -> Result<bool> {
    let val = val.chain_err(|| format!("missing {}", name))?;
    let val = val.as_bool().chain_err(|| format!("not a bool {}", name))?;
    Ok(val)
}

fn bool_from_value_or(val: Option<&Value>, name: &str, default: bool) -> Result<bool> {
    if val.is_none() {
        return Ok(default);
    }
    bool_from_value(val, name)
}

// TODO: implement caching and delta updates
fn get_status_hash(txs: Vec<(Txid, Option<BlockId>)>, query: &Query) -> Option<FullHash> {
    if txs.is_empty() {
        None
    } else {
        let mut hasher = Sha256::new();
        for (txid, blockid) in txs {
            let is_mempool = blockid.is_none();
            let has_unconfirmed_parents = is_mempool
                .and_then(|| Some(query.has_unconfirmed_parents(&txid)))
                .unwrap_or(false);
            let height = get_electrum_height(blockid, has_unconfirmed_parents);
            let part = format!("{}:{}:", txid, height);
            hasher.update(part.as_bytes());
        }
        Some(
            hasher.finalize()[..]
                .try_into()
                .expect("SHA256 size is 32 bytes"),
        )
    }
}

struct Connection {
    proxy_proto_addr: Cell<Option<Option<SocketAddr>>>,
    electrum_proxy_depth: usize,
    // Chain info related
    query: Arc<Query>,
    last_header_entry: Option<HeaderEntry>,
    status_hashes: HashMap<Sha256dHash, Value>, // ScriptHash -> StatusHash
    stats: Arc<Stats>,
    txs_limit: usize,
    // Stream related
    stream: ConnectionStream,
    _arc_stream: Arc<ConnectionStream>, // Needs to be kept alive until drop
    reader: RefCell<Option<BufReader<ConnectionStream>>>,
    // Channel related
    message_chan: SyncChannel<Message>,
    shutdown_replies: crossbeam_channel::Receiver<()>, // For reply select branch
    shutdown_send: crossbeam_channel::Sender<()>,      // For Drop. Kills properly-die thread
    // Discovery related
    #[cfg(feature = "electrum-discovery")]
    discovery: Option<Arc<DiscoveryManager>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.shutdown_send.send(());
    }
}

impl Connection {
    pub fn new(
        query: Arc<Query>,
        stream: ConnectionStream,
        stats: Arc<Stats>,
        (txs_limit, electrum_proxy_depth): (usize, usize),
        shutdown: SyncChannel<()>,
        #[cfg(feature = "electrum-discovery")] discovery: Option<Arc<DiscoveryManager>>,
    ) -> Connection {
        // Channels
        let (reply_killer, shutdown_replies) = crossbeam_channel::unbounded();
        let shutdown_send = shutdown.sender();

        // Using this Arc to prevent any thread leaks from keeping the stream alive
        let _arc_stream = Arc::new(stream.try_clone().expect("failed to clone TcpStream"));
        let maybe_stream = Arc::downgrade(&_arc_stream);

        spawn_thread("properly-die", move || {
            let _ = shutdown.receiver().map(|c| c.recv());
            let _ = maybe_stream.upgrade().map(|s| s.shutdown(Shutdown::Both));
            let _ = reply_killer.send(());
        });

        let ret = Connection {
            proxy_proto_addr: Cell::new(None),
            electrum_proxy_depth,
            query,
            last_header_entry: None, // disable header subscription for now
            status_hashes: HashMap::new(),
            reader: RefCell::new(Some(BufReader::new(
                stream.try_clone().expect("failed to clone TcpStream"),
            ))),
            stream,
            _arc_stream,
            message_chan: SyncChannel::new(10),
            stats,
            txs_limit,
            shutdown_replies,
            shutdown_send,
            #[cfg(feature = "electrum-discovery")]
            discovery,
        };
        // Wait for first request to find
        ret.get_source_addr();
        ret
    }

    fn blockchain_headers_subscribe(&mut self) -> Result<Value> {
        let entry = self.query.chain().best_header();
        let hex_header = hex::encode(serialize(entry.header()));
        let result = json!({"hex": hex_header, "height": entry.height()});
        self.last_header_entry = Some(entry);
        Ok(result)
    }

    fn server_version(&self) -> Result<Value> {
        Ok(json!([VERSION_STRING.as_str(), PROTOCOL_VERSION]))
    }

    fn server_banner(&self) -> Result<Value> {
        Ok(json!(self.query.config().electrum_banner.clone()))
    }

    #[cfg(feature = "electrum-discovery")]
    fn server_features(&self) -> Result<Value> {
        let discovery = self
            .discovery
            .as_ref()
            .chain_err(|| "discovery is disabled")?;
        Ok(json!(discovery.our_features()))
    }

    fn server_donation_address(&self) -> Result<Value> {
        Ok(Value::Null)
    }

    fn server_peers_subscribe(&self) -> Result<Value> {
        #[cfg(feature = "electrum-discovery")]
        let servers = self
            .discovery
            .as_ref()
            .map_or_else(|| json!([]), |d| json!(d.get_servers()));

        #[cfg(not(feature = "electrum-discovery"))]
        let servers = json!([]);

        Ok(servers)
    }

    #[cfg(feature = "electrum-discovery")]
    fn server_add_peer(&self, params: &[Value]) -> Result<Value> {
        let ip = self
            .stream
            .ip()
            .ok_or(Error::from("Can't add peer with Unix sockets enabled"))?;
        let discovery = self
            .discovery
            .as_ref()
            .chain_err(|| "discovery is disabled")?;

        let features = params
            .get(0)
            .chain_err(|| "missing features param")?
            .clone();
        let features = serde_json::from_value(features).chain_err(|| "invalid features")?;

        discovery.add_server_request(ip, features)?;
        Ok(json!(true))
    }

    fn mempool_get_fee_histogram(&self) -> Result<Value> {
        Ok(json!(&self.query.mempool().backlog_stats().fee_histogram))
    }

    fn blockchain_block_header(&self, params: &[Value]) -> Result<Value> {
        let height = usize_from_value(params.get(0), "height")?;
        let cp_height = usize_from_value_or(params.get(1), "cp_height", 0)?;

        let raw_header_hex: String = self
            .query
            .chain()
            .header_by_height(height)
            .map(|entry| hex::encode(serialize(entry.header())))
            .chain_err(|| "missing header")?;

        if cp_height == 0 {
            return Ok(json!(raw_header_hex));
        }
        let (branch, root) = get_header_merkle_proof(self.query.chain(), height, cp_height)?;

        Ok(json!({
            "header": raw_header_hex,
            "root": root,
            "branch": branch
        }))
    }

    fn blockchain_block_headers(&self, params: &[Value]) -> Result<Value> {
        let start_height = usize_from_value(params.get(0), "start_height")?;
        let count = MAX_HEADERS.min(usize_from_value(params.get(1), "count")?);
        let cp_height = usize_from_value_or(params.get(2), "cp_height", 0)?;
        let heights: Vec<usize> = (start_height..(start_height + count)).collect();
        let headers: Vec<String> = heights
            .into_iter()
            .filter_map(|height| {
                self.query
                    .chain()
                    .header_by_height(height)
                    .map(|entry| hex::encode(serialize(entry.header())))
            })
            .collect();

        if count == 0 || cp_height == 0 {
            return Ok(json!({
                "count": headers.len(),
                "hex": headers.join(""),
                "max": MAX_HEADERS,
            }));
        }

        let (branch, root) =
            get_header_merkle_proof(self.query.chain(), start_height + (count - 1), cp_height)?;

        Ok(json!({
            "count": headers.len(),
            "hex": headers.join(""),
            "max": MAX_HEADERS,
            "root": root,
            "branch" : branch,
        }))
    }

    fn blockchain_estimatefee(&self, params: &[Value]) -> Result<Value> {
        let conf_target = usize_from_value(params.get(0), "blocks_count")?;
        let fee_rate = self
            .query
            .estimate_fee(conf_target as u16)
            .chain_err(|| format!("cannot estimate fee for {} blocks", conf_target))?;
        // convert from sat/b to BTC/kB, as expected by Electrum clients
        Ok(json!(fee_rate / 100_000f64))
    }

    fn blockchain_relayfee(&self) -> Result<Value> {
        let relayfee = self.query.get_relayfee()?;
        // convert from sat/b to BTC/kB, as expected by Electrum clients
        Ok(json!(relayfee / 100_000f64))
    }

    fn blockchain_scripthash_subscribe(&mut self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.get(0)).chain_err(|| "bad script_hash")?;

        let history_txids = get_history(&self.query, &script_hash[..], self.txs_limit)?;
        let status_hash = get_status_hash(history_txids, &self.query)
            .map_or(Value::Null, |h| json!(hex::encode(full_hash(&h[..]))));

        if self
            .status_hashes
            .insert(script_hash, status_hash.clone())
            .is_none()
        {
            self.stats.subscriptions.inc();
        }
        Ok(status_hash)
    }

    #[cfg(not(feature = "liquid"))]
    fn blockchain_scripthash_get_balance(&self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.get(0)).chain_err(|| "bad script_hash")?;
        let (chain_stats, mempool_stats) = self.query.stats(&script_hash[..]);

        Ok(json!({
            "confirmed": chain_stats.funded_txo_sum - chain_stats.spent_txo_sum,
            "unconfirmed": mempool_stats.funded_txo_sum as i64 - mempool_stats.spent_txo_sum as i64,
        }))
    }

    fn blockchain_scripthash_get_history(&self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.get(0)).chain_err(|| "bad script_hash")?;
        let history_txids = get_history(&self.query, &script_hash[..], self.txs_limit)?;

        Ok(json!(history_txids
            .into_iter()
            .map(|(txid, blockid)| {
                let is_mempool = blockid.is_none();
                let fee = is_mempool.and_then(|| self.query.get_mempool_tx_fee(&txid));
                let has_unconfirmed_parents = is_mempool
                    .and_then(|| Some(self.query.has_unconfirmed_parents(&txid)))
                    .unwrap_or(false);
                let height = get_electrum_height(blockid, has_unconfirmed_parents);
                GetHistoryResult { txid, height, fee }
            })
            .collect::<Vec<_>>()))
    }

    fn blockchain_scripthash_listunspent(&self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.get(0)).chain_err(|| "bad script_hash")?;
        let utxos = self.query.utxo(&script_hash[..])?;

        let to_json = |utxo: Utxo| {
            let json = json!({
                "height": utxo.confirmed.map_or(0, |b| b.height),
                "tx_pos": utxo.vout,
                "tx_hash": utxo.txid,
                "value": utxo.value,
            });

            #[cfg(feature = "liquid")]
            let json = {
                let mut json = json;
                json["asset"] = json!(utxo.asset);
                json["nonce"] = json!(utxo.nonce);
                json
            };

            json
        };

        Ok(json!(Value::Array(
            utxos.into_iter().map(to_json).collect()
        )))
    }

    fn blockchain_transaction_broadcast(&self, params: &[Value]) -> Result<Value> {
        let tx = params.get(0).chain_err(|| "missing tx")?;
        let tx = tx.as_str().chain_err(|| "non-string tx")?.to_string();
        let txid = self.query.broadcast_raw(&tx)?;
        if let Err(e) = self.message_chan.sender().try_send(Message::PeriodicUpdate) {
            warn!(
                "[{}] failed to issue PeriodicUpdate after broadcast: {}",
                self.get_source_addr_str(),
                e
            );
        }
        Ok(json!(txid))
    }

    fn blockchain_transaction_get(&self, params: &[Value]) -> Result<Value> {
        let tx_hash = Txid::from(hash_from_value(params.get(0)).chain_err(|| "bad tx_hash")?);
        let verbose = match params.get(1) {
            Some(value) => value.as_bool().chain_err(|| "non-bool verbose value")?,
            None => false,
        };

        // FIXME: implement verbose support
        if verbose {
            bail!("verbose transactions are currently unsupported");
        }

        let tx = self
            .query
            .lookup_raw_txn(&tx_hash)
            .chain_err(|| "missing transaction")?;
        Ok(json!(hex::encode(tx)))
    }

    fn blockchain_transaction_get_merkle(&self, params: &[Value]) -> Result<Value> {
        let txid = Txid::from(hash_from_value(params.get(0)).chain_err(|| "bad tx_hash")?);
        let height = usize_from_value(params.get(1), "height")?;
        let blockid = self
            .query
            .chain()
            .tx_confirming_block(&txid)
            .ok_or("tx not found or is unconfirmed")?;
        if blockid.height != height {
            bail!("invalid confirmation height provided");
        }
        let (merkle, pos) = get_tx_merkle_proof(self.query.chain(), &txid, &blockid.hash)
            .chain_err(|| "cannot create merkle proof")?;
        Ok(json!({
                "block_height": blockid.height,
                "merkle": merkle,
                "pos": pos}))
    }

    fn blockchain_transaction_id_from_pos(&self, params: &[Value]) -> Result<Value> {
        let height = usize_from_value(params.get(0), "height")?;
        let tx_pos = usize_from_value(params.get(1), "tx_pos")?;
        let want_merkle = bool_from_value_or(params.get(2), "merkle", false)?;

        let (txid, merkle) = get_id_from_pos(self.query.chain(), height, tx_pos, want_merkle)?;

        if !want_merkle {
            return Ok(json!(txid));
        }

        Ok(json!({
            "tx_hash": txid,
            "merkle" : merkle}))
    }

    fn handle_command(&mut self, method: &str, params: &[Value], id: &Value) -> Result<Value> {
        let timer = self
            .stats
            .latency
            .with_label_values(&[method])
            .start_timer();
        let result = match method {
            "blockchain.block.header" => self.blockchain_block_header(params),
            "blockchain.block.headers" => self.blockchain_block_headers(params),
            "blockchain.estimatefee" => self.blockchain_estimatefee(params),
            "blockchain.headers.subscribe" => self.blockchain_headers_subscribe(),
            "blockchain.relayfee" => self.blockchain_relayfee(),
            #[cfg(not(feature = "liquid"))]
            "blockchain.scripthash.get_balance" => self.blockchain_scripthash_get_balance(params),
            "blockchain.scripthash.get_history" => self.blockchain_scripthash_get_history(params),
            "blockchain.scripthash.listunspent" => self.blockchain_scripthash_listunspent(params),
            "blockchain.scripthash.subscribe" => self.blockchain_scripthash_subscribe(params),
            "blockchain.transaction.broadcast" => self.blockchain_transaction_broadcast(params),
            "blockchain.transaction.get" => self.blockchain_transaction_get(params),
            "blockchain.transaction.get_merkle" => self.blockchain_transaction_get_merkle(params),
            "blockchain.transaction.id_from_pos" => self.blockchain_transaction_id_from_pos(params),
            "mempool.get_fee_histogram" => self.mempool_get_fee_histogram(),
            "server.banner" => self.server_banner(),
            "server.donation_address" => self.server_donation_address(),
            "server.peers.subscribe" => self.server_peers_subscribe(),
            "server.ping" => Ok(Value::Null),
            "server.version" => self.server_version(),

            #[cfg(feature = "electrum-discovery")]
            "server.features" => self.server_features(),
            #[cfg(feature = "electrum-discovery")]
            "server.add_peer" => self.server_add_peer(params),

            &_ => bail!("unknown method {} {:?}", method, params),
        };
        timer.observe_duration();
        // TODO: return application errors should be sent to the client
        Ok(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => {
                warn!(
                    "[{}] rpc #{} {} {:?} failed: {}",
                    self.get_source_addr_str(),
                    id,
                    method,
                    params,
                    e.display_chain()
                );
                json!({"jsonrpc": "2.0", "id": id, "error": format!("{}", e)})
            }
        })
    }

    fn update_subscriptions(&mut self) -> Result<Vec<Value>> {
        let timer = self
            .stats
            .latency
            .with_label_values(&["periodic_update"])
            .start_timer();
        let mut result = vec![];
        if let Some(ref mut last_entry) = self.last_header_entry {
            let entry = self.query.chain().best_header();
            if *last_entry != entry {
                *last_entry = entry;
                let hex_header = hex::encode(serialize(last_entry.header()));
                let header = json!({"hex": hex_header, "height": last_entry.height()});
                result.push(json!({
                    "jsonrpc": "2.0",
                    "method": "blockchain.headers.subscribe",
                    "params": [header]}));
            }
        }
        for (script_hash, status_hash) in self.status_hashes.iter_mut() {
            let history_txids = get_history(&self.query, &script_hash[..], self.txs_limit)?;
            let new_status_hash = get_status_hash(history_txids, &self.query)
                .map_or(Value::Null, |h| json!(hex::encode(full_hash(&h[..]))));
            if new_status_hash == *status_hash {
                continue;
            }
            result.push(json!({
                "jsonrpc": "2.0",
                "method": "blockchain.scripthash.subscribe",
                "params": [script_hash, new_status_hash]}));
            *status_hash = new_status_hash;
        }
        timer.observe_duration();
        Ok(result)
    }

    fn send_values(&mut self, values: &[Value]) -> Result<()> {
        for value in values {
            let line = value.to_string() + "\n";
            self.stream
                .write_all(line.as_bytes())
                .chain_err(|| format!("failed to send {}", value))?;
        }
        Ok(())
    }

    fn get_source_addr_str(&self) -> String {
        self.get_source_addr()
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.stream.addr_string())
    }

    /// This will only check the PROXY protocol once
    /// and store the result in the first Option.
    /// Some(None) means "we checked, but there was no address"
    /// The inner option is returned as a Copy.
    fn get_source_addr(&self) -> Option<SocketAddr> {
        // Option<SocketAddr> is Copy
        if let Some(v) = self.proxy_proto_addr.get() {
            v
        } else {
            let v = self
                .reader
                .borrow_mut()
                .as_mut()
                .and_then(|r| r.fill_buf().ok())
                .and_then(|mut available| {
                    parse_proxy_headers(&mut available, self.electrum_proxy_depth).0
                })
                .map(|addr| {
                    trace!("RPC Received PROXY Protocol address: {}", addr);
                    addr
                });
            self.proxy_proto_addr.set(Some(v));
            v
        }
    }

    fn handle_replies(&mut self) -> Result<()> {
        let empty_params = json!([]);
        let addr_str = self.get_source_addr_str();
        loop {
            crossbeam_channel::select! {
                recv(self.message_chan.receiver().chain_err(|| format!("[{addr_str}] channel closed"))?) -> msg => {
                    let msg = msg.chain_err(|| format!("[{addr_str}] channel closed"))?;
                    trace!("RPC [{addr_str}] {:?}", msg);
                    match msg {
                        Message::Request(cmd) => {
                            let reply = match (
                                cmd.get("method"),
                                cmd.get("params").unwrap_or(&empty_params),
                                cmd.get("id"),
                            ) {
                                (Some(Value::String(method)), Value::Array(params), Some(id)) => {
                                    self.handle_command(method, params, id)?
                                }
                                _ => bail!("[{addr_str}] invalid command: {}", cmd),
                            };
                            self.send_values(&[reply])?
                        }
                        Message::PeriodicUpdate => {
                            let values = self
                                .update_subscriptions()
                                .chain_err(|| format!("[{addr_str}] failed to update subscriptions"))?;
                            self.send_values(&values)?
                        }
                        Message::Done => {
                            self.message_chan.close();
                            return Ok(());
                        }
                    }
                }
                recv(self.shutdown_replies) -> _ => {
                    self.message_chan.close();
                    return Ok(());
                }
            }
        }
    }

    fn handle_requests(
        mut reader: BufReader<ConnectionStream>,
        tx: &crossbeam_channel::Sender<Message>,
        addr_str: &str,
    ) -> Result<()> {
        loop {
            let mut recv_data = Vec::<u8>::new();
            match read_until(&mut reader, b'\n', &mut recv_data) {
                Ok(bytes) => trace!("[{addr_str}] Read {bytes} bytes from connection"),
                Err(e) => bail!("[{addr_str}] Failed to read: {}", e),
            }
            if recv_data.is_empty() {
                return Ok(());
            } else {
                if recv_data.starts_with(&[22, 3, 1]) {
                    // (very) naive SSL handshake detection
                    bail!(
                        "[{addr_str}] invalid request - maybe SSL-encrypted data?: {:?}",
                        recv_data
                    )
                }
                match serde_json::from_slice(&recv_data) {
                    Ok(req) => tx
                        .send(Message::Request(req))
                        .chain_err(|| format!("[{}] channel closed", addr_str))?,
                    Err(err) => {
                        let _ = tx.send(Message::Done);
                        bail!("[{}] invalid UTF8: {}", addr_str, err)
                    }
                }
            }
        }
    }

    pub fn run(mut self) {
        self.stats.clients.inc();
        let reader = self.reader.take().unwrap();
        let tx = self.message_chan.sender();

        let rpc_addr = self.get_source_addr();
        let addr_str = self.get_source_addr_str();
        let shutdown_send = self.shutdown_send.clone();
        let child = spawn_thread("reader", move || {
            let addr_str = rpc_addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| reader.get_ref().addr_string());
            let result =
                std::panic::catch_unwind(|| Connection::handle_requests(reader, &tx, &addr_str))
                    .unwrap_or_else(|e| {
                        Err(format!(
                            "[{}] RPC Panic in request handler: {}",
                            addr_str,
                            parse_panic_error(&e)
                        )
                        .into())
                    });
            // This shuts down the other graceful shutdown thread,
            // which also shuts down the handle_replies loop
            // regardless of panic, Err, or Ok
            let _ = shutdown_send.send(());
            result
        });
        if let Err(e) = self.handle_replies() {
            error!(
                "[{}] connection handling failed: {}",
                addr_str,
                e.display_chain().to_string()
            );
        }
        self.stats.clients.dec();
        self.stats
            .subscriptions
            .sub(self.status_hashes.len() as i64);

        debug!("[{}] shutting down connection", addr_str);
        let _ = self.stream.shutdown(Shutdown::Both);
        self.message_chan.close();
        if let Err(err) = child.join().expect("receiver can't panic") {
            error!("[{}] receiver failed: {}", addr_str, err);
        }
    }
}

fn get_history(
    query: &Query,
    scripthash: &[u8],
    txs_limit: usize,
) -> Result<Vec<(Txid, Option<BlockId>)>> {
    // to avoid silently trunacting history entries, ask for one extra more than the limit and fail if it exists
    let history_txids = query.history_txids(scripthash, txs_limit + 1);
    ensure!(history_txids.len() <= txs_limit, ErrorKind::TooPopular);
    Ok(history_txids)
}

#[derive(Serialize, Debug)]
struct GetHistoryResult {
    #[serde(rename = "tx_hash")]
    txid: Txid,
    height: isize,
    #[serde(skip_serializing_if = "Option::is_none")]
    fee: Option<u64>,
}

#[derive(Debug)]
pub enum Message {
    Request(Value),
    PeriodicUpdate,
    Done,
}

pub enum Notification {
    Periodic,
    Exit,
}

pub struct RPC {
    notification: Sender<Notification>,
    server: Option<thread::JoinHandle<()>>, // so we can join the server while dropping this ojbect
}

struct Stats {
    latency: HistogramVec,
    clients: Gauge,
    subscriptions: Gauge,
}

impl RPC {
    fn start_notifier(
        notification: Channel<Notification>,
        senders: Arc<Mutex<Vec<crossbeam_channel::Sender<Message>>>>,
        acceptor: Sender<Option<ConnectionStream>>,
        acceptor_shutdown: Sender<()>,
    ) {
        spawn_thread("notification", move || {
            for msg in notification.receiver().iter() {
                let mut senders = senders.lock().unwrap();
                match msg {
                    Notification::Periodic => {
                        for sender in senders.split_off(0) {
                            if let Err(crossbeam_channel::TrySendError::Disconnected(_)) =
                                sender.try_send(Message::PeriodicUpdate)
                            {
                                continue;
                            }
                            senders.push(sender);
                        }
                    }
                    Notification::Exit => {
                        acceptor_shutdown.send(()).unwrap(); // Stop the acceptor itself
                        acceptor.send(None).unwrap(); // mark acceptor as done
                        break;
                    }
                }
            }
        });
    }

    fn start_acceptor(
        config: Arc<Config>,
        shutdown_channel: Channel<()>,
    ) -> Channel<Option<ConnectionStream>> {
        let chan = Channel::unbounded();
        let acceptor = chan.sender();
        spawn_thread("acceptor", move || {
            let addr = config.electrum_rpc_addr;
            let listener = if let Some(path) = config.rpc_socket_file.as_ref() {
                // We can leak this Path because we know that this function is only
                // called once on startup.
                let path: &'static Path = Box::leak(path.clone().into_boxed_path());

                ConnectionListener::new_unix(path)
            } else {
                ConnectionListener::new_tcp(&addr)
            };
            listener.run(acceptor, shutdown_channel);
        });
        chan
    }

    pub fn start(config: Arc<Config>, query: Arc<Query>, metrics: &Metrics) -> RPC {
        let stats = Arc::new(Stats {
            latency: metrics.histogram_vec(
                HistogramOpts::new("electrum_rpc", "Electrum RPC latency (seconds)"),
                &["method"],
            ),
            clients: metrics.gauge(MetricOpts::new("electrum_clients", "# of Electrum clients")),
            subscriptions: metrics.gauge(MetricOpts::new(
                "electrum_subscriptions",
                "# of Electrum subscriptions",
            )),
        });
        stats.clients.set(0);
        stats.subscriptions.set(0);

        let notification = Channel::unbounded();

        // Discovery is enabled when electrum-public-hosts is set
        #[cfg(feature = "electrum-discovery")]
        let discovery = config.electrum_public_hosts.clone().map(|hosts| {
            use crate::chain::genesis_hash;
            let features = ServerFeatures {
                hosts,
                server_version: VERSION_STRING.clone(),
                genesis_hash: genesis_hash(config.network_type),
                protocol_min: PROTOCOL_VERSION,
                protocol_max: PROTOCOL_VERSION,
                hash_function: "sha256".into(),
                pruning: None,
            };
            let discovery = Arc::new(DiscoveryManager::new(
                config.network_type,
                features,
                PROTOCOL_VERSION,
                config.electrum_announce,
                config.tor_proxy,
            ));
            DiscoveryManager::spawn_jobs_thread(Arc::clone(&discovery));
            discovery
        });

        let txs_limit = config.electrum_txs_limit;
        let electrum_proxy_depth = config.electrum_proxy_depth;

        RPC {
            notification: notification.sender(),
            server: Some(spawn_thread("rpc", move || {
                let senders =
                    Arc::new(Mutex::new(Vec::<crossbeam_channel::Sender<Message>>::new()));

                let acceptor_shutdown = Channel::unbounded();
                let acceptor_shutdown_sender = acceptor_shutdown.sender();
                let acceptor = RPC::start_acceptor(config, acceptor_shutdown);
                RPC::start_notifier(
                    notification,
                    senders.clone(),
                    acceptor.sender(),
                    acceptor_shutdown_sender,
                );

                let mut threads = HashMap::new();
                let (garbage_sender, garbage_receiver) = crossbeam_channel::unbounded();

                while let Some(stream) = acceptor.receiver().recv().unwrap() {
                    // explicitely scope the shadowed variables for the new thread
                    let query = Arc::clone(&query);
                    let senders = Arc::clone(&senders);
                    let stats = Arc::clone(&stats);
                    let garbage_sender = garbage_sender.clone();

                    // Kill the peers properly
                    let shutdown_channel = SyncChannel::new(1);
                    let shutdown_sender = shutdown_channel.sender();

                    #[cfg(feature = "electrum-discovery")]
                    let discovery = discovery.clone();

                    let spawned = spawn_thread("peer", move || {
                        let shutdown_sender = shutdown_channel.sender();
                        info!("connected peer. waiting for first request...");
                        let conn = Connection::new(
                            query,
                            stream,
                            stats,
                            (txs_limit, electrum_proxy_depth),
                            shutdown_channel,
                            #[cfg(feature = "electrum-discovery")]
                            discovery,
                        );
                        let addr = conn.get_source_addr_str();
                        info!("[{}] connected peer", addr);
                        senders.lock().unwrap().push(conn.message_chan.sender());
                        conn.run();
                        info!("[{}] disconnected peer", addr);
                        let _ = shutdown_sender.send(());
                        let _ = garbage_sender.send(std::thread::current().id());
                    });

                    trace!("spawned {:?}", spawned.thread().id());
                    threads.insert(spawned.thread().id(), (spawned, shutdown_sender));
                    while let Ok(id) = garbage_receiver.try_recv() {
                        if let Some((thread, killer)) = threads.remove(&id) {
                            trace!("joining {:?}", id);
                            let _ = killer.send(());
                            if let Err(error) = thread.join() {
                                error!("failed to join {:?}: {:?}", id, error);
                            }
                        }
                    }
                }
                // Drop these
                drop(acceptor);
                drop(garbage_receiver);

                trace!("closing {} RPC connections", senders.lock().unwrap().len());
                for sender in senders.lock().unwrap().iter() {
                    let _ = sender.try_send(Message::Done);
                }

                for (id, (thread, killer)) in threads {
                    trace!("joining {:?}", id);
                    let _ = killer.send(());
                    if let Err(error) = thread.join() {
                        error!("failed to join {:?}: {:?}", id, error);
                    }
                }

                trace!("RPC connections are closed");
            })),
        }
    }

    pub fn notify(&self) {
        self.notification.send(Notification::Periodic).unwrap();
    }
}

impl Drop for RPC {
    fn drop(&mut self) {
        trace!("stop accepting new RPCs");
        self.notification.send(Notification::Exit).unwrap();
        if let Some(handle) = self.server.take() {
            handle.join().unwrap();
        }
        trace!("RPC server is stopped");
        crate::util::with_spawned_threads(|threads| {
            trace!("Threads after dropping RPC: {:?}", threads);
        });
    }
}

enum ConnectionListener {
    Tcp(TcpListener),
    Unix(UnixListener, &'static Path),
}

impl ConnectionListener {
    fn new_tcp(addr: &SocketAddr) -> Self {
        let socket = create_socket(addr);
        socket.listen(511).expect("setting backlog failed");
        socket
            .set_nonblocking(false)
            .expect("cannot set nonblocking to false");
        info!("Electrum RPC server running on {}", addr);
        Self::Tcp(TcpListener::from(socket))
    }

    /// This takes a static reference to a Path in order to
    /// make shallow clones of UnixStreams much cheaper.
    /// Since this type will only usually be instanciated 1 time
    /// it should be acceptable to just leak the PathBuf.
    /// Do not leak values if you call this an unknown number of
    /// times throughout the program.
    fn new_unix(path: &'static Path) -> Self {
        if let Ok(meta) = fs::metadata(path) {
            // Cleanup socket file left by previous execution
            if meta.file_type().is_socket() {
                fs::remove_file(path).ok();
            }
        }

        let socket = std::os::unix::net::UnixListener::bind(path)
            .expect("cannnot bind to unix socket for RPC");
        socket
            .set_nonblocking(false)
            .expect("cannot set nonblocking to false");
        info!(
            "Electrum RPC server running on unix socket {}",
            path.display()
        );
        Self::Unix(socket, path)
    }

    fn run(&self, acceptor: Sender<Option<ConnectionStream>>, shutdown_channel: Channel<()>) {
        let shutdown_bool = Arc::new(AtomicBool::new(false));

        {
            let shutdown_bool = Arc::clone(&shutdown_bool);
            crate::util::spawn_thread(
                "shutdown-acceptor",
                self.create_shutdown_job(shutdown_channel, shutdown_bool),
            );
        }

        loop {
            let stream = self.accept().expect("accept failed");

            if shutdown_bool.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }

            stream
                .set_nonblocking(false)
                .expect("failed to set connection as blocking");
            acceptor.send(Some(stream)).expect("send failed");
        }
    }

    fn accept(&self) -> std::result::Result<ConnectionStream, std::io::Error> {
        match self {
            Self::Tcp(c) => c.accept().map(|(l, r)| ConnectionStream::Tcp(l, r)),
            Self::Unix(c, p) => c.accept().map(|(l, r)| ConnectionStream::Unix(l, r, p)),
        }
    }

    fn create_shutdown_job(
        &self,
        shutdown_channel: Channel<()>,
        shutdown_bool: Arc<AtomicBool>,
    ) -> Box<dyn FnOnce() + Send + 'static> {
        match self {
            ConnectionListener::Tcp(c) => {
                let local_addr = c.local_addr().unwrap();
                Box::new(move || {
                    // Block until shutdown is sent.
                    let _ = shutdown_channel.receiver().recv();
                    // Store the bool so after the next accept it will break the loop
                    shutdown_bool.store(true, std::sync::atomic::Ordering::Release);
                    // Connect to the socket to cause it to unblock
                    let _ = TcpStream::connect(local_addr);
                })
            }
            ConnectionListener::Unix(_, p) => {
                let path = *p;
                Box::new(move || {
                    // Block until shutdown is sent.
                    let _ = shutdown_channel.receiver().recv();
                    // Store the bool so after the next accept it will break the loop
                    shutdown_bool.store(true, std::sync::atomic::Ordering::Release);
                    // Connect to the socket to cause it to unblock
                    let _ = UnixStream::connect(path);
                })
            }
        }
    }
}

enum ConnectionStream {
    Tcp(TcpStream, std::net::SocketAddr),
    Unix(UnixStream, std::os::unix::net::SocketAddr, &'static Path),
}

impl ConnectionStream {
    fn addr_string(&self) -> String {
        match self {
            ConnectionStream::Tcp(_, a) => format!("{a}"),
            ConnectionStream::Unix(_, _, _) => "(Unix socket)".to_string(),
        }
    }

    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(match self {
            ConnectionStream::Tcp(s, a) => ConnectionStream::Tcp(s.try_clone()?, *a),
            ConnectionStream::Unix(s, a, p) => ConnectionStream::Unix(s.try_clone()?, a.clone(), p),
        })
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        match self {
            ConnectionStream::Tcp(s, _) => s.shutdown(how),
            ConnectionStream::Unix(s, _, _) => s.shutdown(how),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            ConnectionStream::Tcp(s, _) => s.set_nonblocking(nonblocking),
            ConnectionStream::Unix(s, _, _) => s.set_nonblocking(nonblocking),
        }
    }

    #[cfg(feature = "electrum-discovery")]
    fn ip(&self) -> Option<IpAddr> {
        match self {
            ConnectionStream::Tcp(_, a) => Some(a.ip()),
            ConnectionStream::Unix(_, _, _) => None,
        }
    }
}

impl Write for ConnectionStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ConnectionStream::Tcp(s, _) => s.write(buf),
            ConnectionStream::Unix(s, _, _) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ConnectionStream::Tcp(s, _) => s.flush(),
            ConnectionStream::Unix(s, _, _) => s.flush(),
        }
    }
}

impl Read for ConnectionStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ConnectionStream::Tcp(s, _) => s.read(buf),
            ConnectionStream::Unix(s, _, _) => s.read(buf),
        }
    }
}

/// This is a slightly modified version of read_until from standard library BufRead trait.
/// After every read we check if there's a PROXY protocol header at the beginning of the read.
fn read_until(r: &mut impl BufRead, delim: u8, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut read = 0;
    let mut carry_over_arr = [0_u8; 256];
    let mut carrying_over = 0;
    loop {
        let (done, used) = {
            let mut available = match r.fill_buf() {
                Ok(n) => n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };

            // If carry over, try to parse PROXY headers.
            let (carry_skipped_count, exit_error) = if carrying_over > 0 {
                process_carry_over(&mut carrying_over, &mut carry_over_arr, &mut available)
            } else {
                (0, false)
            };
            // Rare edge case. If we carry over a proxy parse and it still errors
            // It is most likely an unknown format
            if exit_error {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }

            // Try parsing PROXY headers after every read.
            let (_, skipped_count, exit_error) = parse_proxy_headers(&mut available, 0);
            let skipped_count = carry_skipped_count + skipped_count;

            match (memchr::memchr(delim, available), exit_error) {
                (Some(i), false) => {
                    buf.extend_from_slice(&available[..=i]);
                    (true, i + 1 + skipped_count)
                }
                (None, _) | (_, true) => {
                    // Added: carry over
                    insert_carry_over(&mut carrying_over, &mut carry_over_arr, available);

                    if !exit_error {
                        buf.extend_from_slice(available);
                    }

                    (false, available.len() + skipped_count)
                }
            }
        };
        r.consume(used);
        read += used;
        if done || used == 0 {
            return Ok(read);
        }
    }
}

fn proxy_header_to_source_socket_addr(p_header: ProxyHeader) -> Option<SocketAddr> {
    match p_header {
        ProxyHeader::Version1 {
            addresses: version1::ProxyAddresses::Ipv4 { source, .. },
        } => Some(SocketAddr::V4(source)),
        ProxyHeader::Version1 {
            addresses: version1::ProxyAddresses::Ipv6 { source, .. },
        } => Some(SocketAddr::V6(source)),
        ProxyHeader::Version2 {
            addresses: version2::ProxyAddresses::Ipv4 { source, .. },
            ..
        } => Some(SocketAddr::V4(source)),
        ProxyHeader::Version2 {
            addresses: version2::ProxyAddresses::Ipv6 { source, .. },
            ..
        } => Some(SocketAddr::V6(source)),
        _ => None,
    }
}

fn parse_proxy_headers(
    buf: &mut &[u8],
    electrum_proxy_depth: usize,
) -> (Option<SocketAddr>, usize, bool) {
    trace!("Starting parse PROXY headers: {:?}", buf.get(..12));
    let mut addr = None;
    let mut current_header_index = 0;
    let before_len = buf.len();
    // Save the original state of the buf
    let mut original_buf = *buf;
    let mut error_exit = false;
    // The last header is the outer-most proxy
    // Warning do not early return. ONLY break the loop.
    loop {
        let p_header = match proxy_protocol::parse(buf) {
            Ok(h) => h,
            // NotProxyHeader definitely does not move the buf pointer.
            Err(proxy_protocol::ParseError::NotProxyHeader) => break,
            // This can move the buf cursor forward
            // and will most likely end in an error higher in the call stack
            // This means "PROXY protocol was used, but it was in an unknown format"
            // (Maybe someday if nginx/etc. uses a new version of the protocol
            // and we don't update the dependency to a version that handles the new
            // version, it might break.)
            // OR it could mean a PROXY header fell on the BufReader buffer boundary.
            Err(_) => {
                // This is the buf state before this error returned.
                // This match arm will modify buf past the version bytes
                // and stop in a state pointing to the middle of a broken header.
                // We return the buf state to the beginning of the previous version bytes.
                *buf = original_buf;
                error_exit = true;
                break;
            }
        };
        // After each successful header parse, save the new buf state.
        original_buf = *buf;
        trace!("Parsed PROXY protocol header: {:?}", p_header);
        // Increment from 0 to 1 before the first check
        current_header_index += 1;
        // 0 should always continue
        // 1 should only get the 1st header's IP address etc.
        if current_header_index != electrum_proxy_depth {
            continue;
        }
        // The address is only attempted to be
        // parsed when the 1 based index is equal
        addr = proxy_header_to_source_socket_addr(p_header);
    }
    (addr, before_len - buf.len(), error_exit)
}

fn parse_panic_error(e: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = e.downcast_ref::<&str>() {
        s
    } else if let Some(s) = e.downcast_ref::<String>() {
        s
    } else {
        "Unknown panic"
    }
}

/// The goal of this function is to take the carried over bytes from the last loop
/// and connect them with the first bytes of the next read, then check if it's a header.
/// This is to prevent headers from straddling the BufReader's buffer end.
/// A simple static array should be quick and easy.
fn process_carry_over(
    carrying_over: &mut usize,
    carry_over: &mut [u8],
    available: &mut &[u8],
) -> (usize, bool) {
    // Step 0: Copy as much from available into carry_over to try and parse
    // How much space do we have left in the array?
    let empty_space = carry_over.len() - *carrying_over;
    // How many bytes should we copy over?
    let copy_bytes = available.len().min(empty_space);
    // Copy over the bytes to join with the carried over bytes
    carry_over[*carrying_over..*carrying_over + copy_bytes]
        .copy_from_slice(&available[..copy_bytes]);

    // Step 1: Figure out if it was a proxy header or not.
    #[allow(clippy::redundant_slicing)]
    let mut cursor = &carry_over[..];
    let (_, skipped_count, exit_error) = parse_proxy_headers(&mut cursor, 0);

    // Step 2: Figure out how much we need to skip available forward
    let skip_count = skipped_count.saturating_sub(*carrying_over);

    // Step 3: Move the available cursor
    *available = &available[skip_count..];
    // Step 4: Reset carrying over (writing 0s to the array is unnecessary)
    *carrying_over = 0;

    // Return the skip count so we can call consume later
    (skip_count, exit_error)
}

/// Insert the carry over
fn insert_carry_over(carrying_over: &mut usize, carry_over_arr: &mut [u8], available: &[u8]) {
    *carrying_over = available.len().min(carry_over_arr.len());
    carry_over_arr[..*carrying_over].copy_from_slice(&available[..*carrying_over]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_until() {
        // len 48
        let v1 = "PROXY TCP6 ab:ce:ef:01:23:45:67:89 ::1 0 65535\r\n"
            .as_bytes()
            .to_vec();
        // len 31
        let v2 =
            hex::decode("0d0a0d0a000d0a515549540a2111000f7f000001c0a80001ffff0101450000").unwrap();
        let simple_json = "{}\n".as_bytes().to_vec();
        // len 26
        let larger_json = r#"{"id":2,"name":"electrs"}\n"#.as_bytes().to_vec();

        let vectors: Vec<(Vec<u8>, usize, &[u8], std::result::Result<usize, String>)> = vec![
            // Simple JSON with LF
            (simple_json.clone(), 3, &simple_json, Ok(3)),
            // Simple JSON with LF + PROXY v1
            (
                [v1.clone(), simple_json.clone()].concat(),
                51,
                &simple_json,
                Ok(51),
            ),
            // Simple JSON with LF + PROXY v2
            (
                [v2.clone(), simple_json.clone()].concat(),
                34,
                &simple_json,
                Ok(34),
            ),
            // Simple JSON with LF + two layers of proxy
            (
                [v1.clone(), v2.clone(), simple_json.clone()].concat(),
                82,
                &simple_json,
                Ok(82),
            ),
            // Simple JSON that goes over the buffer boundary
            (
                larger_json.clone(),
                2, // capacity
                &larger_json,
                Ok(27),
            ),
            // Simple JSON with LF + two layers of proxy
            (
                [v1.clone(), v2.clone(), simple_json.clone()].concat(),
                45, // 3 bytes before v1 header ends
                &simple_json,
                Ok(82),
            ),
            // Simple JSON with LF + two layers of proxy
            (
                [v1.clone(), v2.clone(), simple_json.clone()].concat(),
                46, // 2 bytes before v1 header ends
                &simple_json,
                Ok(82),
            ),
            // Capacity exactly on the last byte of v1 == parser error (library)
            // TODO: make PR for upstream
            // (
            //     [v1.clone(), v2.clone(), simple_json.clone()].concat(),
            //     47, // 1 bytes before v1 header ends (just befor \n)
            //     &simple_json,
            //     Ok(82),
            // ),
            // TODO: When the BufReader boundary hits in a v2 PROXY header it crashes
            // (
            //     [v1.clone(), v2.clone(), simple_json.clone()].concat(),
            //     49, // 1 byte into v2 header
            //     &simple_json,
            //     Ok(82),
            // ),
            // TODO: make PR for upstream to bail when TLV is cut short
            // (
            //     [v1.clone(), v2.clone(), simple_json.clone()].concat(),
            //     77, // 2 bytes short of v2 ending
            //     &simple_json,
            //     Ok(82),
            // ),
            (
                [v1.clone(), v2.clone(), larger_json.clone()].concat(),
                80, // 1 after v2 ends
                &larger_json,
                Ok(106),
            ),
        ];

        for (input, capacity, bytes, count) in vectors {
            let mut buf = BufReader::with_capacity(capacity, input.as_slice());
            let mut v = vec![];
            let result_count = read_until(&mut buf, b'\n', &mut v).map_err(|e| format!("{e}"));
            assert_eq!(count, result_count);
            assert_eq!(bytes, &v);
        }
    }
}
