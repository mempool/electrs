use std::collections::HashMap;
use std::convert::TryInto;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::IpAddr;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bitcoin::hashes::sha256d::Hash as Sha256dHash;
use error_chain::ChainedError;
use hex;
use ppp::PartialResult;
use serde_json::{from_str, Value};
use sha2::{Digest, Sha256};

#[cfg(not(feature = "liquid"))]
use bitcoin::consensus::encode::serialize;
#[cfg(feature = "liquid")]
use elements::encode::serialize;

use crate::chain::Txid;
use crate::config::{Config, VERSION_STRING};
use crate::electrum::{get_electrum_height, ProtocolVersion, ServerFeatures};
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

/// How long a connection waits for a PROXY-protocol (HAProxy) header to arrive
/// before giving up and treating the peer as a direct client. Proxies send the
/// header immediately after the TCP handshake, so this only bounds misbehaving
/// or non-proxied clients (and caps any delay to shutdown).
const PROXY_HEADER_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(feature = "electrum-discovery")]
use crate::electrum::DiscoveryManager;

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

/// Extracts the source socket address from a parsed PROXY protocol v1 header.
fn proxy_v1_source(addresses: &ppp::v1::Addresses) -> Option<SocketAddr> {
    match addresses {
        ppp::v1::Addresses::Tcp4(ip) => Some(SocketAddr::new(
            IpAddr::V4(ip.source_address),
            ip.source_port,
        )),
        ppp::v1::Addresses::Tcp6(ip) => Some(SocketAddr::new(
            IpAddr::V6(ip.source_address),
            ip.source_port,
        )),
        ppp::v1::Addresses::Unknown => None,
    }
}

/// Extracts the source socket address from a parsed PROXY protocol v2 header.
fn proxy_v2_source(addresses: &ppp::v2::Addresses) -> Option<SocketAddr> {
    match addresses {
        ppp::v2::Addresses::IPv4(ip) => Some(SocketAddr::new(
            IpAddr::V4(ip.source_address),
            ip.source_port,
        )),
        ppp::v2::Addresses::IPv6(ip) => Some(SocketAddr::new(
            IpAddr::V6(ip.source_address),
            ip.source_port,
        )),
        ppp::v2::Addresses::Unspecified | ppp::v2::Addresses::Unix(_) => None,
    }
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

#[repr(i16)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonRpcV2Error {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InternalError = -32603,
}
impl JsonRpcV2Error {
    #[inline]
    fn into_i16(self) -> i16 {
        self as i16
    }
}

struct Connection {
    query: Arc<Query>,
    last_header_entry: Option<HeaderEntry>,
    status_hashes: HashMap<Sha256dHash, Value>, // ScriptHash -> StatusHash
    stream: ConnectionStream,
    chan: SyncChannel<Message>,
    stats: Arc<Stats>,
    txs_limit: usize,
    max_line_size: usize,
    max_subscriptions: usize,
    idle_timeout: u64,
    last_request_at: Instant,
    die_please: Option<Receiver<()>>,
    server_features: Arc<ServerFeatures>,
    haproxy_depth: usize,
    proxy_client: Option<SocketAddr>,
    client_string: String,
    connections_per_client: usize,
    client_counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
    registered_ip: Option<IpAddr>,
    #[cfg(feature = "electrum-discovery")]
    discovery: Option<Arc<DiscoveryManager>>,
}

impl Connection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        query: Arc<Query>,
        stream: ConnectionStream,
        stats: Arc<Stats>,
        txs_limit: usize,
        max_line_size: usize,
        max_subscriptions: usize,
        idle_timeout: u64,
        die_please: Receiver<()>,
        server_features: Arc<ServerFeatures>,
        haproxy_depth: usize,
        connections_per_client: usize,
        client_counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
        #[cfg(feature = "electrum-discovery")] discovery: Option<Arc<DiscoveryManager>>,
    ) -> Connection {
        Connection {
            query,
            last_header_entry: None, // disable header subscription for now
            status_hashes: HashMap::new(),
            stream,
            chan: SyncChannel::new(10),
            stats,
            txs_limit,
            max_line_size,
            max_subscriptions,
            idle_timeout,
            last_request_at: Instant::now(),
            die_please: Some(die_please),
            server_features,
            haproxy_depth,
            proxy_client: None,
            client_string: String::new(),
            connections_per_client,
            client_counts,
            registered_ip: None,
            #[cfg(feature = "electrum-discovery")]
            discovery,
        }
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

    fn server_features(&self) -> Result<Value> {
        Ok(json!(self.server_features.as_ref()))
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
            .first()
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
        let height = usize_from_value(params.first(), "height")?;
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
        let start_height = usize_from_value(params.first(), "start_height")?;
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
        let conf_target = usize_from_value(params.first(), "blocks_count")?;
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
        let script_hash = hash_from_value(params.first()).chain_err(|| "bad script_hash")?;

        // Enforce per-client subscription limit (don't count re-subscriptions to the same hash)
        if !self.status_hashes.contains_key(&script_hash)
            && self.status_hashes.len() >= self.max_subscriptions
        {
            bail!(
                "subscription limit reached ({} max per client)",
                self.max_subscriptions
            );
        }

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

    fn blockchain_scripthash_unsubscribe(&mut self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.first()).chain_err(|| "bad script_hash")?;

        let removed = self.status_hashes.remove(&script_hash).is_some();
        if removed {
            self.stats.subscriptions.dec();
        }
        Ok(Value::Bool(removed))
    }

    #[cfg(not(feature = "liquid"))]
    fn blockchain_scripthash_get_balance(&self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.first()).chain_err(|| "bad script_hash")?;
        let (chain_stats, mempool_stats) = self.query.stats(&script_hash[..]);

        Ok(json!({
            "confirmed": chain_stats.funded_txo_sum - chain_stats.spent_txo_sum,
            "unconfirmed": mempool_stats.funded_txo_sum as i64 - mempool_stats.spent_txo_sum as i64,
        }))
    }

    fn blockchain_scripthash_get_history(&self, params: &[Value]) -> Result<Value> {
        let script_hash = hash_from_value(params.first()).chain_err(|| "bad script_hash")?;
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
        let script_hash = hash_from_value(params.first()).chain_err(|| "bad script_hash")?;
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
        let tx = params.first().chain_err(|| "missing tx")?;
        let tx = tx.as_str().chain_err(|| "non-string tx")?.to_string();
        let txid = self.query.broadcast_raw(&tx)?;
        if let Err(e) = self.chan.sender().try_send(Message::PeriodicUpdate) {
            warn!("failed to issue PeriodicUpdate after broadcast: {}", e);
        }
        Ok(json!(txid))
    }

    fn blockchain_transaction_get(&self, params: &[Value]) -> Result<Value> {
        let tx_hash = Txid::from(hash_from_value(params.first()).chain_err(|| "bad tx_hash")?);
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
        let txid = Txid::from(hash_from_value(params.first()).chain_err(|| "bad tx_hash")?);
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
        let height = usize_from_value(params.first(), "height")?;
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
        trace!("[{}] rpc {} {:?}", self.client_string(), method, params);
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
            "blockchain.scripthash.unsubscribe" => self.blockchain_scripthash_unsubscribe(params),
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
            "server.features" => self.server_features(),

            #[cfg(feature = "electrum-discovery")]
            "server.add_peer" => self.server_add_peer(params),

            &_ => {
                warn!(
                    "[{}] rpc unknown method #{} {} {:?}",
                    self.client_string(),
                    id,
                    method,
                    params
                );
                return Ok(json_rpc_error(
                    format!("Method {method} not found"),
                    Some(id),
                    JsonRpcV2Error::MethodNotFound,
                ));
            }
        };
        timer.observe_duration();
        // TODO: return application errors should be sent to the client
        Ok(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => {
                warn!(
                    "[{}] rpc #{} {} {:?} failed: {}",
                    self.client_string(),
                    id,
                    method,
                    params,
                    e.display_chain()
                );
                json_rpc_error(e, Some(id), JsonRpcV2Error::InternalError)
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

    fn close_idle_connection(&mut self, idle_for: Duration) {
        info!(
            "[{}] closing idle connection after {} seconds without requests (timeout: {} seconds)",
            self.client_string(),
            idle_for.as_secs(),
            self.idle_timeout,
        );
        self.chan.close();
    }

    /// A human-readable identifier for the connected client, preferring the
    /// HAProxy-reported address (when present) over the direct peer address.
    fn client_string(&self) -> &str {
        &self.client_string
    }

    fn populate_client_string(&mut self) {
        if self.client_string.is_empty() {
            let mut s = match self.proxy_client {
                Some(addr) => format!("{addr}"),
                None => self.stream.addr_string(),
            };
            if s.is_empty() {
                s.push_str("(unnamed)");
            }
            self.client_string = s;
        }
    }

    /// Registers this connection against its client key (the HAProxy-reported IP
    /// when available, otherwise the direct peer IP) and enforces the
    /// `electrum-connections-per-client` limit. Returns an error if the limit has
    /// already been reached, in which case the connection must be closed.
    fn register_client(&mut self) -> Result<()> {
        if self.connections_per_client == 0 {
            // Per-client limit disabled.
            return Ok(());
        }
        let key = match self
            .proxy_client
            .map(|addr| addr.ip())
            .or_else(|| self.stream.direct_ip())
        {
            Some(key) => key,
            // No usable client key (e.g. a unix socket with no PROXY header).
            None => return Ok(()),
        };

        let mut counts = self.client_counts.lock().unwrap();
        let count = counts.entry(key).or_insert(0);
        if *count >= self.connections_per_client {
            bail!(
                "too many connections from client {} ({} max per client)",
                key,
                self.connections_per_client
            );
        }
        *count += 1;
        self.registered_ip = Some(key);
        Ok(())
    }

    /// Releases this connection's slot in the per-client connection counter.
    fn unregister_client(&mut self) {
        if let Some(key) = self.registered_ip.take() {
            let mut counts = self.client_counts.lock().unwrap();
            if let Some(count) = counts.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(&key);
                }
            }
        }
    }

    fn handle_replies(&mut self, shutdown: crossbeam_channel::Receiver<()>) -> Result<()> {
        let idle_timeout = Duration::from_secs(self.idle_timeout);
        loop {
            let elapsed = self.last_request_at.elapsed();
            if elapsed > idle_timeout {
                self.close_idle_connection(elapsed);
                return Ok(());
            }
            let remaining = idle_timeout.saturating_sub(elapsed);
            let idle_deadline = crossbeam_channel::after(remaining);

            crossbeam_channel::select! {
                recv(self.chan.receiver()) -> msg => {
                    let msg = msg.chain_err(|| "channel closed")?;
                    trace!("[{}] RPC {:?}", self.client_string(), msg);
                    match msg {
                        Message::Request(line) => {
                            self.last_request_at = Instant::now();
                            let result = self.handle_line(&line);
                            self.send_values(&[result])?
                        }
                        Message::PeriodicUpdate => {
                            let values = self
                                .update_subscriptions()
                                .chain_err(|| "failed to update subscriptions")?;
                            self.send_values(&values)?
                        }
                        Message::Done => {
                            self.chan.close();
                            return Ok(());
                        }
                    }
                }
                recv(shutdown) -> _ => {
                    self.chan.close();
                    return Ok(());
                }
                recv(idle_deadline) -> _ => {
                    let idle_for = self.last_request_at.elapsed();
                    self.close_idle_connection(idle_for);
                    return Ok(());
                }
            }
        }
    }

    #[inline]
    fn handle_line(&mut self, line: &String) -> Value {
        if let Ok(json_value) = from_str(line) {
            match json_value {
                Value::Array(mut arr) => {
                    for cmd in &mut arr {
                        // Replace each cmd with its response in-memory.
                        *cmd = self.handle_value(cmd);
                    }
                    Value::Array(arr)
                }
                cmd => self.handle_value(&cmd),
            }
        } else {
            // serde_json was unable to parse
            json_rpc_error(
                format!("Invalid JSON: {line}"),
                None,
                JsonRpcV2Error::ParseError,
            )
        }
    }

    #[inline]
    fn handle_value(&mut self, value: &Value) -> Value {
        match (
            value.get("method"),
            value.get("params").unwrap_or(&json!([])),
            value.get("id"),
        ) {
            (Some(Value::String(method)), Value::Array(params), Some(id)) => self
                .handle_command(method, params, id)
                .unwrap_or_else(|err| {
                    json_rpc_error(
                        format!("{method} RPC error: {err}"),
                        Some(id),
                        JsonRpcV2Error::InternalError,
                    )
                }),
            (_, _, Some(id)) => json_rpc_error(value, Some(id), JsonRpcV2Error::InvalidRequest),
            _ => json_rpc_error(value, None, JsonRpcV2Error::InvalidRequest),
        }
    }

    fn handle_requests(
        stream: ConnectionStream,
        tx: crossbeam_channel::Sender<Message>,
        max_line_size: usize,
    ) -> Result<()> {
        // Any PROXY-protocol header was already consumed by `resolve_proxy`
        // before this thread started; the stream replays any leftover bytes
        // (the start of the first request) ahead of the live socket.
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = Vec::<u8>::new();
            // Read up to max_line_size + 1 bytes to detect oversized lines
            let mut limited = (&mut reader).take((max_line_size as u64).saturating_add(1));
            limited
                .read_until(b'\n', &mut line)
                .chain_err(|| "failed to read a request")?;
            if line.is_empty() {
                tx.send(Message::Done).chain_err(|| "channel closed")?;
                return Ok(());
            } else if line.len() > max_line_size {
                let _ = tx.send(Message::Done);
                bail!(
                    "request line too large ({} bytes, max is {})",
                    line.len(),
                    max_line_size
                )
            } else {
                if line.starts_with(&[22, 3, 1]) {
                    // (very) naive SSL handshake detection
                    let _ = tx.send(Message::Done);
                    bail!("invalid request - maybe SSL-encrypted data?: {:?}", line)
                }
                match String::from_utf8(line) {
                    Ok(req) => tx
                        .send(Message::Request(req))
                        .chain_err(|| "channel closed")?,
                    Err(err) => {
                        let _ = tx.send(Message::Done);
                        bail!("invalid UTF8: {}", err)
                    }
                }
            }
        }
    }

    pub fn run(mut self) {
        self.stats.clients.inc();
        let tx = self.chan.sender();

        // Resolve any HAProxy header before doing anything else so the client
        // address is known up-front (for logging and the per-client limit).
        // Unlike the best-effort probe on the shared acceptor loop, this runs on
        // the connection's own thread and blocks (bounded by
        // PROXY_HEADER_TIMEOUT) until the header arrives, so a header split
        // across TCP segments is not missed and mis-attributed to the peer IP.
        self.proxy_client = self
            .stream
            .resolve_proxy_blocking(self.haproxy_depth, PROXY_HEADER_TIMEOUT);
        if let Err(e) = self.register_client() {
            self.populate_client_string();
            info!("[{}] {}", self.client_string(), e);
            self.stats.clients.dec();
            return;
        }
        // Populate the client string for logging purposes.
        self.populate_client_string();
        info!("[{}] connection established", self.client_string());

        let die_please = self.die_please.take().unwrap();
        let (reply_killer, reply_receiver) = crossbeam_channel::unbounded();

        // We create a clone of the stream and put it in an Arc
        // This will drop at the end of the function.
        let arc_stream = Arc::new(self.stream.try_clone().expect("failed to clone stream"));
        // We don't want to keep the stream alive until SIGINT
        // It should drop (close) no matter what.
        let maybe_stream = Arc::downgrade(&arc_stream);
        spawn_thread("properly-die", move || {
            let _ = die_please.recv();
            let _ = maybe_stream.upgrade().map(|s| s.shutdown(Shutdown::Both));
            let _ = reply_killer.send(());
        });

        let max_line_size = self.max_line_size;
        // self.stream contains the leftovers from the PROXY read.
        // Since clones don't copy the leftovers we need to send the one with leftovers
        // to the reader thread and keep the clone for sending replies.
        let mut stream = self.stream.try_clone().expect("failed to clone stream");
        std::mem::swap(&mut self.stream, &mut stream);
        let child = spawn_thread("reader", move || {
            Connection::handle_requests(stream, tx, max_line_size)
        });
        if let Err(e) = self.handle_replies(reply_receiver) {
            error!(
                "[{}] connection handling failed: {}",
                self.client_string(),
                e.display_chain().to_string()
            );
        }
        self.stats.clients.dec();
        self.stats
            .subscriptions
            .sub(self.status_hashes.len() as i64);
        self.unregister_client();

        debug!("[{}] shutting down connection", self.client_string());
        // Drop the Arc so that the stream properly closes.
        drop(arc_stream);
        let _ = self.stream.shutdown(Shutdown::Both);
        info!("[{}] connection closed", self.client_string());
        if let Err(err) = child.join().expect("receiver panicked") {
            error!("[{}] receiver failed: {}", self.client_string(), err);
        }
    }
}

#[inline]
fn json_rpc_error(
    input: impl core::fmt::Display,
    id: Option<&Value>,
    code: JsonRpcV2Error,
) -> Value {
    let mut ret = json!({
        "error": {
            "code": code.into_i16(),
            "message": format!("{input}")
        },
        "jsonrpc": "2.0"
    });
    if let (Some(id), Some(obj)) = (id, ret.as_object_mut()) {
        obj.insert(String::from("id"), id.clone());
    }
    ret
}

fn get_history(
    query: &Query,
    scripthash: &[u8],
    txs_limit: usize,
) -> Result<Vec<(Txid, Option<BlockId>)>> {
    // to avoid silently trunacting history entries, ask for one extra more than the limit and fail if it exists
    let history_txids = query.history_txids(scripthash, txs_limit + 1);
    ensure!(
        history_txids.len() <= txs_limit,
        ErrorKind::TooManyTxs(txs_limit)
    );
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
    Request(String),
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

        let server_features = {
            use crate::chain::genesis_hash;
            let hosts = config.electrum_public_hosts.clone().unwrap_or_default();
            Arc::new(ServerFeatures {
                hosts,
                server_version: VERSION_STRING.clone(),
                genesis_hash: genesis_hash(config.network_type),
                protocol_min: PROTOCOL_VERSION,
                protocol_max: PROTOCOL_VERSION,
                hash_function: "sha256".into(),
                pruning: None,
            })
        };

        // Discovery is enabled when electrum-public-hosts is set
        #[cfg(feature = "electrum-discovery")]
        let discovery = config.electrum_public_hosts.as_ref().map(|_hosts| {
            let discovery = Arc::new(DiscoveryManager::new(
                config.network_type,
                server_features.as_ref().clone(),
                PROTOCOL_VERSION,
                config.electrum_announce,
                config.tor_proxy,
            ));
            DiscoveryManager::spawn_jobs_thread(Arc::clone(&discovery));
            discovery
        });

        let txs_limit = config.electrum_txs_limit;
        let max_line_size = config.electrum_max_line_size;
        let max_subscriptions = config.electrum_max_subscriptions;
        let max_clients = config.electrum_max_clients;
        let idle_timeout = config.electrum_idle_timeout;
        let haproxy_depth = config.electrum_haproxy_depth;
        let connections_per_client = config.electrum_connections_per_client;

        RPC {
            notification: notification.sender(),
            server: Some(spawn_thread("rpc", move || {
                let senders =
                    Arc::new(Mutex::new(Vec::<crossbeam_channel::Sender<Message>>::new()));
                // Tracks the number of live connections per client (keyed by the
                // HAProxy-reported address when available, otherwise the peer IP).
                let client_counts: Arc<Mutex<HashMap<IpAddr, usize>>> =
                    Arc::new(Mutex::new(HashMap::new()));

                let acceptor_shutdown = Channel::unbounded();
                let acceptor_shutdown_sender = acceptor_shutdown.sender();
                let acceptor = RPC::start_acceptor(config, acceptor_shutdown);
                RPC::start_notifier(
                    notification,
                    senders.clone(),
                    acceptor.sender(),
                    acceptor_shutdown_sender,
                );

                let mut threads: HashMap<thread::ThreadId, (thread::JoinHandle<()>, Sender<()>)> =
                    HashMap::new();
                let (garbage_sender, garbage_receiver) = crossbeam_channel::unbounded();

                while let Some(mut stream) = acceptor.receiver().recv().unwrap() {
                    // Clean up finished threads before checking connection limit
                    while let Ok(id) = garbage_receiver.try_recv() {
                        if let Some((thread, killer)) = threads.remove(&id) {
                            let _ = killer.send(());
                            if let Err(error) = thread.join() {
                                error!("failed to join {:?}: {:?}", id, error);
                            }
                        }
                    }

                    // Enforce maximum connection limit
                    if threads.len() >= max_clients {
                        warn!(
                            "[{}] rejecting connection: max clients reached ({}/{})",
                            stream.addr_string(),
                            threads.len(),
                            max_clients
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }

                    // Does not block. Caches the proxied IP.
                    // Explicitly sets the stream to non-blocking mode
                    // so that this thread does not block.
                    // NGINX should send the proxy header
                    // immediately after the TCP handshake.
                    // But even if it doesn't we will just return None,
                    // and the connection will show up as something generic
                    // when using unix sockets to forward. Like (unknown)
                    stream.resolve_proxy(haproxy_depth);

                    // explicitly scope the shadowed variables for the new thread
                    let addr = stream.addr_string();
                    let query = Arc::clone(&query);
                    let senders = Arc::clone(&senders);
                    let stats = Arc::clone(&stats);
                    let client_counts = Arc::clone(&client_counts);
                    let garbage_sender = garbage_sender.clone();

                    // Kill the peers properly
                    let (killer, peace_receiver) = std::sync::mpsc::channel();
                    let killer_clone = killer.clone();

                    #[cfg(feature = "electrum-discovery")]
                    let discovery = discovery.clone();
                    let server_features = Arc::clone(&server_features);

                    let spawned = spawn_thread("peer", move || {
                        let addr = stream.addr_string();
                        info!("[{addr}] client connected");
                        let conn = Connection::new(
                            query,
                            stream,
                            stats,
                            txs_limit,
                            max_line_size,
                            max_subscriptions,
                            idle_timeout,
                            peace_receiver,
                            server_features,
                            haproxy_depth,
                            connections_per_client,
                            client_counts,
                            #[cfg(feature = "electrum-discovery")]
                            discovery,
                        );
                        senders.lock().unwrap().push(conn.chan.sender());
                        conn.run();
                        info!("[{addr}] client disconnected");
                        let _ = killer_clone.send(());
                        let _ = garbage_sender.send(std::thread::current().id());
                    });

                    trace!("[{}] spawned {:?}", addr, spawned.thread().id());
                    threads.insert(spawned.thread().id(), (spawned, killer));
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
            Self::Tcp(c) => c
                .accept()
                .map(|(l, r)| ConnectionStream::new(ConnectionStreamInner::Tcp(l, r))),
            Self::Unix(c, p) => c
                .accept()
                .map(|(l, r)| ConnectionStream::new(ConnectionStreamInner::Unix(l, r, p))),
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

enum ConnectionStreamInner {
    Tcp(TcpStream, std::net::SocketAddr),
    Unix(UnixStream, std::os::unix::net::SocketAddr, &'static Path),
}

impl ConnectionStreamInner {
    fn addr_string(&self, proxy: Option<SocketAddr>) -> String {
        match (self, proxy) {
            (ConnectionStreamInner::Tcp(_, a), None) => format!("{a}"),
            (ConnectionStreamInner::Tcp(_, a), Some(p)) => format!("{p} (via {a})"),
            (ConnectionStreamInner::Unix(_, a, _), None) => format!("{a:?}"),
            (ConnectionStreamInner::Unix(_, a, _), Some(p)) => format!("{p} (via {a:?})"),
        }
    }

    /// The direct peer IP address, if this is a TCP connection. Unix-socket
    /// connections have no IP and return `None`.
    fn direct_ip(&self) -> Option<IpAddr> {
        match self {
            ConnectionStreamInner::Tcp(_, a) => Some(a.ip()),
            ConnectionStreamInner::Unix(..) => None,
        }
    }

    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(match self {
            ConnectionStreamInner::Tcp(s, a) => ConnectionStreamInner::Tcp(s.try_clone()?, *a),
            ConnectionStreamInner::Unix(s, a, p) => {
                ConnectionStreamInner::Unix(s.try_clone()?, a.clone(), p)
            }
        })
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.shutdown(how),
            ConnectionStreamInner::Unix(s, _, _) => s.shutdown(how),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.set_nonblocking(nonblocking),
            ConnectionStreamInner::Unix(s, _, _) => s.set_nonblocking(nonblocking),
        }
    }

    fn read_timeout(&self) -> std::io::Result<Option<Duration>> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.read_timeout(),
            ConnectionStreamInner::Unix(s, _, _) => s.read_timeout(),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.set_read_timeout(timeout),
            ConnectionStreamInner::Unix(s, _, _) => s.set_read_timeout(timeout),
        }
    }

    /// Returns `Some(true)` if the stream is non-blocking, `Some(false)` if it is
    /// blocking, and `None` if the platform does not support checking the mode,
    /// or if an error occurred while checking.
    fn get_nonblocking(&self) -> Option<bool> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd as _;
            let fd = match self {
                ConnectionStreamInner::Tcp(s, _) => s.as_raw_fd(),
                ConnectionStreamInner::Unix(s, _, _) => s.as_raw_fd(),
            };
            // Safety: fcntl is safe to call with F_GETFL,
            // and we check the return value for errors.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return None;
            }
            Some(flags & libc::O_NONBLOCK != 0)
        }
        #[cfg(not(unix))]
        {
            // non-unix platforms not supported.
            None
        }
    }
}

impl Write for ConnectionStreamInner {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.write(buf),
            ConnectionStreamInner::Unix(s, _, _) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.flush(),
            ConnectionStreamInner::Unix(s, _, _) => s.flush(),
        }
    }
}

impl Read for ConnectionStreamInner {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ConnectionStreamInner::Tcp(s, _) => s.read(buf),
            ConnectionStreamInner::Unix(s, _, _) => s.read(buf),
        }
    }
}

/// A client connection stream that strips any PROXY-protocol (HAProxy) headers
/// found at the very start of the connection before exposing the underlying
/// Electrum request bytes. Bytes read while probing for headers but not part of
/// a header are buffered in `leftover` and replayed on the next `read`.
struct ConnectionStream {
    inner: ConnectionStreamInner,
    /// Bytes read past the PROXY header(s) (or while probing) that belong to the
    /// Electrum request stream and must be returned before reading from `inner`.
    leftover: Vec<u8>,
    /// Whether we already attempted to parse PROXY headers.
    proxy_parsed: bool,
    /// The HAProxy-reported client address resolved at the configured depth.
    proxy_client: Option<SocketAddr>,
}

impl ConnectionStream {
    fn new(inner: ConnectionStreamInner) -> Self {
        ConnectionStream {
            inner,
            leftover: Vec::new(),
            proxy_parsed: false,
            proxy_client: None,
        }
    }

    fn addr_string(&self) -> String {
        let proxy = self.proxy_client.as_ref().copied();
        self.inner.addr_string(proxy)
    }

    fn direct_ip(&self) -> Option<IpAddr> {
        self.inner.direct_ip()
    }

    fn try_clone(&self) -> std::io::Result<Self> {
        // The buffered leftover and parse state belong to the reader that
        // performed the PROXY probe; clones (used for shutdown) start clean.
        Ok(ConnectionStream::new(self.inner.try_clone()?))
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        self.inner.shutdown(how)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    #[cfg(feature = "electrum-discovery")]
    fn ip(&self) -> Option<IpAddr> {
        self.inner.direct_ip()
    }

    /// Probes the start of the connection for PROXY-protocol (HAProxy) headers
    /// and resolves the client address at the configured `haproxy_depth`.
    ///
    /// Parsing is non-destructive to the request stream: it reads in
    /// non-blocking mode (restoring the previous mode afterwards), so a client
    /// that opens a socket without sending data is not blocked on. Any bytes
    /// read that turn out not to be a PROXY header are buffered and replayed on
    /// the next `read`. A PROXY header split across TCP segments is only
    /// partially available on the first probe; in that case probing is *not*
    /// marked finished and resumes from the buffered bytes on a later probe.
    /// Returns `None` when HAProxy support is disabled (depth 0), when reading
    /// would block, or when no header is found.
    fn resolve_proxy(&mut self, haproxy_depth: usize) -> Option<SocketAddr> {
        if let Some(client) = self.proxy_client {
            return Some(client);
        }
        if self.proxy_parsed {
            return None;
        }
        // Always probe once (non-blocking) to strip any PROXY header and/or
        // buffer already-available request bytes into `leftover`, even when
        // haproxy_depth == 0 (addresses ignored).

        let nonblocking_status = self.inner.get_nonblocking().unwrap_or(false);
        if self.inner.set_nonblocking(true).is_err() {
            return None;
        }
        let (addrs, complete) = self.read_proxy_headers(None);
        let _ = self.inner.set_nonblocking(nonblocking_status);

        self.finish_proxy_resolve(addrs, complete, haproxy_depth)
    }

    /// Like [`resolve_proxy`](Self::resolve_proxy), but blocks (up to `timeout`)
    /// waiting for the PROXY header instead of giving up the moment a read would
    /// block.
    ///
    /// The best-effort probe on the shared acceptor loop must never block, so a
    /// PROXY header that hasn't fully arrived yet (e.g. split across TCP segments
    /// or delayed after the handshake) is missed there. The later request-stream
    /// reads still strip the header, but discard the parsed address, which would
    /// permanently mis-attribute the connection to the direct peer IP for
    /// logging and per-client limiting. Running this once per connection on its
    /// own thread resolves the address up-front. The wait is bounded so a client
    /// that never sends a header can't stall the thread.
    fn resolve_proxy_blocking(
        &mut self,
        haproxy_depth: usize,
        timeout: Duration,
    ) -> Option<SocketAddr> {
        // With PROXY support disabled there is no address to wait for; fall back
        // to the non-blocking probe so the connection isn't blocked waiting for
        // its first request.
        if haproxy_depth == 0 {
            return self.resolve_proxy(haproxy_depth);
        }
        if let Some(client) = self.proxy_client {
            return Some(client);
        }
        if self.proxy_parsed {
            return None;
        }

        let nonblocking_status = self.inner.get_nonblocking().unwrap_or(false);
        let _ = self.inner.set_nonblocking(false);
        let prev_timeout = self.inner.read_timeout().ok().flatten();

        let (addrs, complete) = self.read_proxy_headers(Some(Instant::now() + timeout));

        // Restore the socket's blocking/timeout state for the request-stream
        // reads that follow (the underlying socket is shared by all clones).
        let _ = self.inner.set_read_timeout(prev_timeout);
        let _ = self.inner.set_nonblocking(nonblocking_status);

        self.finish_proxy_resolve(addrs, complete, haproxy_depth)
    }

    /// Records the parse outcome from [`read_proxy_headers`](Self::read_proxy_headers)
    /// and resolves the client address at the configured `haproxy_depth`.
    fn finish_proxy_resolve(
        &mut self,
        addrs: Option<Vec<SocketAddr>>,
        complete: bool,
        haproxy_depth: usize,
    ) -> Option<SocketAddr> {
        // Only stop probing once parsing has definitively finished. If a PROXY
        // header was only partially available (e.g. split across TCP segments),
        // `complete` is false and the buffered partial header is resumed on the
        // next probe instead of being misread as request data.
        if complete {
            self.proxy_parsed = true;
        }

        if haproxy_depth == 0 {
            return None;
        }

        let addrs = addrs.filter(|addrs| !addrs.is_empty())?;
        self.proxy_client = addrs.get(haproxy_depth - 1).copied();
        self.proxy_client
    }

    /// Reads and parses any stacked PROXY headers available on the socket.
    /// Parsing resumes from any bytes buffered by a previous probe
    /// (`self.leftover`), so a PROXY header split across TCP segments is
    /// completed rather than misread as request data.
    ///
    /// With `deadline` of `None` the caller has put the socket in non-blocking
    /// mode and a `WouldBlock` simply ends the probe. With `deadline` set the
    /// reads block (each bounded by the time remaining until the deadline) so
    /// the header can be resolved up-front on the connection thread.
    ///
    /// Returns the source address reported by each proxy layer (or `None` if no
    /// header was present) together with a `complete` flag. `complete` is `true`
    /// once parsing has definitively finished (a full header chain followed by
    /// non-PROXY data, EOF, a read error, or the size cap). When `complete` is
    /// `false`, the buffered bytes are the partial start of a PROXY header and
    /// must be resumed on the next probe. Any bytes read past the header(s) are
    /// stored in `self.leftover`.
    fn read_proxy_headers(&mut self, deadline: Option<Instant>) -> (Option<Vec<SocketAddr>>, bool) {
        // Upper bound on how much we buffer while looking for PROXY headers, to
        // avoid unbounded memory use from a slow/malicious peer.
        const MAX_PROXY_HEADER_SIZE: usize = 4096;

        enum Step {
            Parsed(usize, Option<SocketAddr>),
            NeedMore,
            Done,
        }

        // Resume from any bytes buffered by a previous probe (a PROXY header
        // split across TCP segments); `leftover` is empty on the first probe.
        let mut buf: Vec<u8> = std::mem::take(&mut self.leftover);
        let mut addrs: Vec<SocketAddr> = Vec::new();
        let mut saw_proxy = false;
        let mut complete = false;
        let mut chunk = [0u8; 256];

        loop {
            // Parse as many complete, stacked PROXY headers as the buffer allows.
            let need_more = loop {
                if buf.is_empty() {
                    break true;
                }
                let step = match ppp::HeaderResult::parse(&buf) {
                    ppp::HeaderResult::V2(Ok(header)) => {
                        Step::Parsed(header.len(), proxy_v2_source(&header.addresses))
                    }
                    ppp::HeaderResult::V1(Ok(header)) => {
                        Step::Parsed(header.header.len(), proxy_v1_source(&header.addresses))
                    }
                    other => {
                        if other.is_incomplete() {
                            Step::NeedMore
                        } else {
                            Step::Done
                        }
                    }
                };
                match step {
                    Step::Parsed(consumed, src) => {
                        saw_proxy = true;
                        if let Some(src) = src {
                            addrs.push(src);
                        }
                        if consumed == 0 || consumed > buf.len() {
                            // Defensive: never spin forever on a degenerate parse.
                            complete = true;
                            break false;
                        }
                        buf.drain(..consumed);
                    }
                    // Incomplete header: more bytes are needed to finish parsing it.
                    Step::NeedMore => break true,
                    // Not (or no longer) a PROXY header: the rest is request data.
                    Step::Done => {
                        complete = true;
                        break false;
                    }
                }
            };

            if !need_more {
                break;
            }
            if buf.len() > MAX_PROXY_HEADER_SIZE {
                // Give up on an over-long header: treat the bytes as request data.
                complete = true;
                break;
            }
            // When resolving in blocking mode, bound the total wait so a slow or
            // stalled peer can't hold the connection thread indefinitely: each
            // read gets the time remaining until the deadline.
            if let Some(deadline) = deadline {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                if remaining.is_zero() {
                    break;
                }
                let _ = self.inner.set_read_timeout(Some(remaining));
            }
            match self.inner.read(&mut chunk) {
                Ok(0) => {
                    // EOF before another complete header.
                    complete = true;
                    break;
                }
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                }
                // No (more) data available right now: a non-blocking probe with
                // nothing buffered, or a blocking read that hit the deadline. If a
                // partial PROXY header is buffered, parsing stays incomplete and
                // resumes on the next probe (`complete` remains false).
                Err(ref e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(_) => {
                    complete = true;
                    break;
                }
            }
        }

        // Whatever wasn't consumed as a PROXY header is either request data (when
        // parsing completed) or the partial start of a PROXY header to resume.
        self.leftover = buf;
        (saw_proxy.then_some(addrs), complete)
    }
}

impl Write for ConnectionStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Read for ConnectionStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Ensure PROXY headers are stripped before exposing any bytes to the caller.
        if !self.proxy_parsed {
            let nonblocking_status = self.inner.get_nonblocking().unwrap_or(false);
            let _ = self.inner.set_nonblocking(false);
            let (_addrs, complete) = self.read_proxy_headers(None);
            let _ = self.inner.set_nonblocking(nonblocking_status);
            if complete {
                self.proxy_parsed = true;
            }
        }

        if !self.leftover.is_empty() {
            let n = self.leftover.len().min(buf.len());
            buf[..n].copy_from_slice(&self.leftover[..n]);
            self.leftover.drain(..n);
            return Ok(n);
        }
        self.inner.read(buf)
    }
}
