use bitcoin::hashes::sha256d::Hash as Sha256dHash;
#[cfg(not(feature = "liquid"))]
use bitcoin::util::merkleblock::MerkleBlock;
use bitcoin::VarInt;
use itertools::Itertools;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

#[cfg(not(feature = "liquid"))]
use bitcoin::consensus::encode::{deserialize, serialize};
#[cfg(feature = "liquid")]
use elements::{
    encode::{deserialize, serialize},
    AssetId,
};

use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::chain::{
    BlockHash, BlockHeader, Network, OutPoint, Script, Transaction, TxOut, Txid, Value,
};
use crate::config::Config;
use crate::daemon::Daemon;
use crate::errors::*;
use crate::metrics::{Gauge, HistogramOpts, HistogramTimer, HistogramVec, MetricOpts, Metrics};
use crate::util::{
    bincode_util, full_hash, has_prevout, is_spendable, BlockHeaderMeta, BlockId, BlockMeta,
    BlockStatus, Bytes, HeaderEntry, HeaderList, ScriptToAddr,
};

use crate::new_index::db::{DBFlush, DBRow, ReverseScanIterator, ScanIterator, DB};
use crate::new_index::fetch::{start_fetcher, BlockEntry, FetchFrom};

#[cfg(feature = "liquid")]
use crate::elements::{asset, peg};

use super::db::ReverseScanGroupIterator;

const MIN_HISTORY_ITEMS_TO_CACHE: usize = 100;

pub struct Store {
    // TODO: should be column families
    txstore_db: DB,
    history_db: DB,
    cache_db: DB,
    added_blockhashes: RwLock<HashSet<BlockHash>>,
    indexed_blockhashes: RwLock<HashSet<BlockHash>>,
    indexed_headers: RwLock<HeaderList>,
}

impl Store {
    pub fn open(path: &Path, config: &Config) -> Self {
        let txstore_db = DB::open(&path.join("txstore"), config);
        let added_blockhashes = load_blockhashes(&txstore_db, &BlockRow::done_filter());
        debug!("{} blocks were added", added_blockhashes.len());

        let history_db = DB::open(&path.join("history"), config);
        let indexed_blockhashes = load_blockhashes(&history_db, &BlockRow::done_filter());
        debug!("{} blocks were indexed", indexed_blockhashes.len());

        let cache_db = DB::open(&path.join("cache"), config);

        let headers = if let Some(tip_hash) = txstore_db.get(b"t") {
            let tip_hash = deserialize(&tip_hash).expect("invalid chain tip in `t`");
            let headers_map = load_blockheaders(&txstore_db);
            debug!(
                "{} headers were loaded, tip at {:?}",
                headers_map.len(),
                tip_hash
            );
            HeaderList::new(headers_map, tip_hash)
        } else {
            HeaderList::empty()
        };

        Store {
            txstore_db,
            history_db,
            cache_db,
            added_blockhashes: RwLock::new(added_blockhashes),
            indexed_blockhashes: RwLock::new(indexed_blockhashes),
            indexed_headers: RwLock::new(headers),
        }
    }

    pub fn txstore_db(&self) -> &DB {
        &self.txstore_db
    }

    pub fn history_db(&self) -> &DB {
        &self.history_db
    }

    pub fn cache_db(&self) -> &DB {
        &self.cache_db
    }

    pub fn done_initial_sync(&self) -> bool {
        self.txstore_db.get(b"t").is_some()
    }
}

type UtxoMap = HashMap<OutPoint, (BlockId, Value)>;

#[derive(Debug)]
pub struct Utxo {
    pub txid: Txid,
    pub vout: u32,
    pub confirmed: Option<BlockId>,
    pub value: Value,

    #[cfg(feature = "liquid")]
    pub asset: elements::confidential::Asset,
    #[cfg(feature = "liquid")]
    pub nonce: elements::confidential::Nonce,
    #[cfg(feature = "liquid")]
    pub witness: elements::TxOutWitness,
}

impl From<&Utxo> for OutPoint {
    fn from(utxo: &Utxo) -> Self {
        OutPoint {
            txid: utxo.txid,
            vout: utxo.vout,
        }
    }
}

#[derive(Debug)]
pub struct SpendingInput {
    pub txid: Txid,
    pub vin: u32,
    pub confirmed: Option<BlockId>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct ScriptStats {
    pub tx_count: usize,
    pub funded_txo_count: usize,
    pub spent_txo_count: usize,
    #[cfg(not(feature = "liquid"))]
    pub funded_txo_sum: u64,
    #[cfg(not(feature = "liquid"))]
    pub spent_txo_sum: u64,
}

impl ScriptStats {
    #[cfg(feature = "liquid")]
    fn is_sane(&self) -> bool {
        // See below for comments.
        self.spent_txo_count <= self.funded_txo_count
            && self.tx_count <= self.spent_txo_count + self.funded_txo_count
    }
    #[cfg(not(feature = "liquid"))]
    fn is_sane(&self) -> bool {
        // There are less or equal spends to funds
        self.spent_txo_count <= self.funded_txo_count
        // There are less or equal transactions to total spent+funded txo counts
        // (Most spread out txo case = N funds in 1 tx each + M spends in 1 tx each = N + M txes)
        && self.tx_count <= self.spent_txo_count + self.funded_txo_count
        // There are less or equal spent coins to funded coins
        && self.spent_txo_sum <= self.funded_txo_sum
        // If funded and spent txos are equal (0 balance)
        // Then funded and spent coins must be equal (0 balance)
        && (self.funded_txo_count == self.spent_txo_count)
            == (self.funded_txo_sum == self.spent_txo_sum)
    }
}

pub struct Indexer {
    store: Arc<Store>,
    flush: DBFlush,
    from: FetchFrom,
    iconfig: IndexerConfig,
    duration: HistogramVec,
    tip_metric: Gauge,
}

struct IndexerConfig {
    light_mode: bool,
    address_search: bool,
    index_unspendables: bool,
    network: Network,
    #[cfg(feature = "liquid")]
    parent_network: crate::chain::BNetwork,
}

impl From<&Config> for IndexerConfig {
    fn from(config: &Config) -> Self {
        IndexerConfig {
            light_mode: config.light_mode,
            address_search: config.address_search,
            index_unspendables: config.index_unspendables,
            network: config.network_type,
            #[cfg(feature = "liquid")]
            parent_network: config.parent_network,
        }
    }
}

pub struct ChainQuery {
    store: Arc<Store>, // TODO: should be used as read-only
    daemon: Arc<Daemon>,
    light_mode: bool,
    duration: HistogramVec,
    network: Network,
}

// TODO: &[Block] should be an iterator / a queue.
impl Indexer {
    pub fn open(store: Arc<Store>, from: FetchFrom, config: &Config, metrics: &Metrics) -> Self {
        Indexer {
            store,
            flush: DBFlush::Disable,
            from,
            iconfig: IndexerConfig::from(config),
            duration: metrics.histogram_vec(
                HistogramOpts::new("index_duration", "Index update duration (in seconds)"),
                &["step"],
            ),
            tip_metric: metrics.gauge(MetricOpts::new("tip_height", "Current chain tip height")),
        }
    }

    fn start_timer(&self, name: &str) -> HistogramTimer {
        self.duration.with_label_values(&[name]).start_timer()
    }

    fn headers_to_add(&self, new_headers: &[HeaderEntry]) -> Vec<HeaderEntry> {
        let added_blockhashes = self.store.added_blockhashes.read().unwrap();
        new_headers
            .iter()
            .filter(|e| !added_blockhashes.contains(e.hash()))
            .cloned()
            .collect()
    }

    fn headers_to_index(&self, new_headers: &[HeaderEntry]) -> Vec<HeaderEntry> {
        let indexed_blockhashes = self.store.indexed_blockhashes.read().unwrap();
        new_headers
            .iter()
            .filter(|e| !indexed_blockhashes.contains(e.hash()))
            .cloned()
            .collect()
    }

    fn start_auto_compactions(&self, db: &DB) {
        let key = b"F".to_vec();
        if db.get(&key).is_none() {
            db.full_compaction();
            db.put_sync(&key, b"");
            assert!(db.get(&key).is_some());
        }
        db.enable_auto_compaction();
    }

    fn get_new_headers(&self, daemon: &Daemon, tip: &BlockHash) -> Result<Vec<HeaderEntry>> {
        let headers = self.store.indexed_headers.read().unwrap();
        let new_headers = daemon.get_new_headers(&headers, tip)?;
        let result = headers.order(new_headers);

        if let Some(tip) = result.last() {
            info!("{:?} ({} left to index)", tip, result.len());
        };
        Ok(result)
    }

    pub fn update(&mut self, daemon: &Daemon) -> Result<BlockHash> {
        let daemon = daemon.reconnect()?;
        let tip = daemon.getbestblockhash()?;
        let new_headers = self.get_new_headers(&daemon, &tip)?;

        let to_add = self.headers_to_add(&new_headers);
        debug!(
            "adding transactions from {} blocks using {:?}",
            to_add.len(),
            self.from
        );
        start_fetcher(self.from, &daemon, to_add)?.map(|blocks| self.add(&blocks));
        self.start_auto_compactions(&self.store.txstore_db);

        let to_index = self.headers_to_index(&new_headers);
        debug!(
            "indexing history from {} blocks using {:?}",
            to_index.len(),
            self.from
        );
        start_fetcher(self.from, &daemon, to_index)?.map(|blocks| self.index(&blocks));
        self.start_auto_compactions(&self.store.history_db);

        if let DBFlush::Disable = self.flush {
            debug!("flushing to disk");
            self.store.txstore_db.flush();
            self.store.history_db.flush();
            self.flush = DBFlush::Enable;
        }

        // update the synced tip *after* the new data is flushed to disk
        debug!("updating synced tip to {:?}", tip);
        self.store.txstore_db.put_sync(b"t", &serialize(&tip));

        let mut headers = self.store.indexed_headers.write().unwrap();
        headers.apply(new_headers);
        assert_eq!(tip, *headers.tip());

        if let FetchFrom::BlkFiles = self.from {
            self.from = FetchFrom::Bitcoind;
        }

        self.tip_metric.set(headers.len() as i64 - 1);

        Ok(tip)
    }

    fn add(&self, blocks: &[BlockEntry]) {
        debug!("Adding {} blocks to Indexer", blocks.len());
        // TODO: skip orphaned blocks?
        let rows = {
            let _timer = self.start_timer("add_process");
            add_blocks(blocks, &self.iconfig)
        };
        {
            let _timer = self.start_timer("add_write");
            self.store.txstore_db.write(rows, self.flush);
        }

        self.store
            .added_blockhashes
            .write()
            .unwrap()
            .extend(blocks.iter().map(|b| {
                if b.entry.height() % 10_000 == 0 {
                    info!("Tx indexing is up to height={}", b.entry.height());
                }
                b.entry.hash()
            }));
    }

    fn index(&self, blocks: &[BlockEntry]) {
        debug!("Indexing {} blocks with Indexer", blocks.len());
        let previous_txos_map = {
            let _timer = self.start_timer("index_lookup");
            lookup_txos(&self.store.txstore_db, &get_previous_txos(blocks), false)
        };
        let rows = {
            let _timer = self.start_timer("index_process");
            let added_blockhashes = self.store.added_blockhashes.read().unwrap();
            for b in blocks {
                if b.entry.height() % 10_000 == 0 {
                    info!("History indexing is up to height={}", b.entry.height());
                }
                let blockhash = b.entry.hash();
                // TODO: replace by lookup into txstore_db?
                if !added_blockhashes.contains(blockhash) {
                    panic!("cannot index block {} (missing from store)", blockhash);
                }
            }
            index_blocks(blocks, &previous_txos_map, &self.iconfig)
        };
        self.store.history_db.write(rows, self.flush);
    }
}

impl ChainQuery {
    pub fn new(store: Arc<Store>, daemon: Arc<Daemon>, config: &Config, metrics: &Metrics) -> Self {
        ChainQuery {
            store,
            daemon,
            light_mode: config.light_mode,
            network: config.network_type,
            duration: metrics.histogram_vec(
                HistogramOpts::new("query_duration", "Index query duration (in seconds)"),
                &["name"],
            ),
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn start_timer(&self, name: &str) -> HistogramTimer {
        self.duration.with_label_values(&[name]).start_timer()
    }

    pub fn get_block_txids(&self, hash: &BlockHash) -> Option<Vec<Txid>> {
        let _timer = self.start_timer("get_block_txids");

        if self.light_mode {
            // TODO fetch block as binary from REST API instead of as hex
            let mut blockinfo = self.daemon.getblock_raw(hash, 1).ok()?;
            Some(serde_json::from_value(blockinfo["tx"].take()).unwrap())
        } else {
            self.store
                .txstore_db
                .get(&BlockRow::txids_key(full_hash(&hash[..])))
                .map(|val| {
                    bincode_util::deserialize_little(&val).expect("failed to parse block txids")
                })
        }
    }

    pub fn get_block_txs(&self, hash: &BlockHash) -> Option<Vec<Transaction>> {
        let _timer = self.start_timer("get_block_txs");

        let txids: Option<Vec<Txid>> = if self.light_mode {
            // TODO fetch block as binary from REST API instead of as hex
            let mut blockinfo = self.daemon.getblock_raw(hash, 1).ok()?;
            Some(serde_json::from_value(blockinfo["tx"].take()).unwrap())
        } else {
            self.store
                .txstore_db
                .get(&BlockRow::txids_key(full_hash(&hash[..])))
                .map(|val| {
                    bincode_util::deserialize_little(&val).expect("failed to parse block txids")
                })
        };

        txids.and_then(|txid_vec| {
            let mut transactions = Vec::with_capacity(txid_vec.len());

            for txid in txid_vec {
                match self.lookup_txn(&txid, Some(hash)) {
                    Some(transaction) => transactions.push(transaction),
                    None => return None,
                }
            }

            Some(transactions)
        })
    }

    pub fn get_block_meta(&self, hash: &BlockHash) -> Option<BlockMeta> {
        let _timer = self.start_timer("get_block_meta");

        if self.light_mode {
            let blockinfo = self.daemon.getblock_raw(hash, 1).ok()?;
            Some(serde_json::from_value(blockinfo).unwrap())
        } else {
            self.store
                .txstore_db
                .get(&BlockRow::meta_key(full_hash(&hash[..])))
                .map(|val| {
                    bincode_util::deserialize_little(&val).expect("failed to parse BlockMeta")
                })
        }
    }

    pub fn get_block_raw(&self, hash: &BlockHash) -> Option<Vec<u8>> {
        let _timer = self.start_timer("get_block_raw");

        if self.light_mode {
            let blockhex = self.daemon.getblock_raw(hash, 0).ok()?;
            Some(hex::decode(blockhex.as_str().unwrap()).unwrap())
        } else {
            let entry = self.header_by_hash(hash)?;
            let meta = self.get_block_meta(hash)?;
            let txids = self.get_block_txids(hash)?;

            // Reconstruct the raw block using the header and txids,
            // as <raw header><tx count varint><raw txs>
            let mut raw = Vec::with_capacity(meta.size as usize);

            raw.append(&mut serialize(entry.header()));
            raw.append(&mut serialize(&VarInt(txids.len() as u64)));

            for txid in txids {
                // we don't need to provide the blockhash because we know we're not in light mode
                raw.append(&mut self.lookup_raw_txn(&txid, None)?);
            }

            Some(raw)
        }
    }

    pub fn get_block_header(&self, hash: &BlockHash) -> Option<BlockHeader> {
        let _timer = self.start_timer("get_block_header");
        #[allow(clippy::clone_on_copy)]
        Some(self.header_by_hash(hash)?.header().clone())
    }

    pub fn get_mtp(&self, height: usize) -> u32 {
        let _timer = self.start_timer("get_block_mtp");
        self.store.indexed_headers.read().unwrap().get_mtp(height)
    }

    pub fn get_block_with_meta(&self, hash: &BlockHash) -> Option<BlockHeaderMeta> {
        let _timer = self.start_timer("get_block_with_meta");
        let header_entry = self.header_by_hash(hash)?;
        Some(BlockHeaderMeta {
            meta: self.get_block_meta(hash)?,
            mtp: self.get_mtp(header_entry.height()),
            header_entry,
        })
    }

    pub fn history_iter_scan(&self, code: u8, hash: &[u8], start_height: usize) -> ScanIterator {
        self.store.history_db.iter_scan_from(
            &TxHistoryRow::filter(code, hash),
            &TxHistoryRow::prefix_height(code, hash, start_height as u32),
        )
    }
    fn history_iter_scan_reverse(
        &self,
        code: u8,
        hash: &[u8],
        start_height: Option<u32>,
    ) -> ReverseScanIterator {
        self.store.history_db.iter_scan_reverse(
            &TxHistoryRow::filter(code, hash),
            &start_height.map_or(TxHistoryRow::prefix_end(code, hash), |start_height| {
                TxHistoryRow::prefix_height_end(code, hash, start_height)
            }),
        )
    }
    fn history_iter_scan_group_reverse(
        &self,
        code: u8,
        hashes: &[[u8; 32]],
        start_height: Option<u32>,
    ) -> ReverseScanGroupIterator {
        self.store.history_db.iter_scan_group_reverse(
            hashes.iter().map(|hash| {
                let prefix = TxHistoryRow::filter(code, &hash[..]);
                let prefix_max = start_height
                    .map_or(TxHistoryRow::prefix_end(code, &hash[..]), |start_height| {
                        TxHistoryRow::prefix_height_end(code, &hash[..], start_height)
                    });
                (prefix, prefix_max)
            }),
            33,
        )
    }

    fn collate_summaries(
        &self,
        iter: impl Iterator<Item = TxHistoryRow>,
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<TxHistorySummary> {
        // collate utxo funding/spending events by transaction

        let rows = iter
            .map(|row| (row.get_txid(), row.key.txinfo, row.key.tx_position))
            .skip_while(|(txid, _, _)| {
                // skip until we reach the last_seen_txid
                last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid != txid)
            })
            .skip_while(|(txid, _, _)| {
                // skip the last_seen_txid itself
                last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid == txid)
            })
            .filter_map(|(txid, info, tx_position)| {
                self.tx_confirming_block(&txid)
                    .map(|b| (txid, info, b.height, b.time, tx_position))
            });
        let mut map: HashMap<Txid, TxHistorySummary> = HashMap::new();
        for (txid, info, height, time, tx_position) in rows {
            if !map.contains_key(&txid) && map.len() == limit {
                break;
            }
            match info {
                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Funding(info) => {
                    map.entry(txid)
                        .and_modify(|tx| {
                            tx.value = tx.value.saturating_add(info.value.try_into().unwrap_or(0))
                        })
                        .or_insert(TxHistorySummary {
                            txid,
                            value: info.value.try_into().unwrap_or(0),
                            height,
                            time,
                            tx_position,
                        });
                }
                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Spending(info) => {
                    map.entry(txid)
                        .and_modify(|tx| {
                            tx.value = tx.value.saturating_sub(info.value.try_into().unwrap_or(0))
                        })
                        .or_insert(TxHistorySummary {
                            txid,
                            value: 0_i64.saturating_sub(info.value.try_into().unwrap_or(0)),
                            height,
                            time,
                            tx_position,
                        });
                }
                #[cfg(feature = "liquid")]
                TxHistoryInfo::Funding(_info) => {
                    map.entry(txid).or_insert(TxHistorySummary {
                        txid,
                        value: 0,
                        height,
                        time,
                        tx_position,
                    });
                }
                #[cfg(feature = "liquid")]
                TxHistoryInfo::Spending(_info) => {
                    map.entry(txid).or_insert(TxHistorySummary {
                        txid,
                        value: 0,
                        height,
                        time,
                        tx_position,
                    });
                }
                #[cfg(feature = "liquid")]
                _ => {}
            }
        }
        let mut tx_summaries = map.into_values().collect::<Vec<TxHistorySummary>>();
        tx_summaries.sort_by(|a, b| {
            if a.height == b.height {
                if a.tx_position == b.tx_position {
                    a.value.cmp(&b.value)
                } else {
                    b.tx_position.cmp(&a.tx_position)
                }
            } else {
                b.height.cmp(&a.height)
            }
        });
        tx_summaries
    }

    pub fn summary(
        &self,
        scripthash: &[u8],
        last_seen_txid: Option<&Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> Vec<TxHistorySummary> {
        // scripthash lookup
        self._summary(b'H', scripthash, last_seen_txid, start_height, limit)
    }

    fn _summary(
        &self,
        code: u8,
        hash: &[u8],
        last_seen_txid: Option<&Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> Vec<TxHistorySummary> {
        let _timer_scan = self.start_timer("address_summary");
        let rows = self
            .history_iter_scan_reverse(code, hash, start_height)
            .map(TxHistoryRow::from_row);

        self.collate_summaries(rows, last_seen_txid, limit)
    }

    pub fn summary_group(
        &self,
        scripthashes: &[[u8; 32]],
        last_seen_txid: Option<&Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> Vec<TxHistorySummary> {
        // scripthash lookup
        let _timer_scan = self.start_timer("address_group_summary");
        let rows = self
            .history_iter_scan_group_reverse(b'H', scripthashes, start_height)
            .map(TxHistoryRow::from_row);

        self.collate_summaries(rows, last_seen_txid, limit)
    }

    pub fn history<'a>(
        &'a self,
        scripthash: &[u8],
        last_seen_txid: Option<&'a Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> impl rayon::iter::ParallelIterator<Item = Result<(Transaction, BlockId)>> + 'a {
        // scripthash lookup
        self._history(b'H', scripthash, last_seen_txid, start_height, limit)
    }

    pub fn history_txids_iter<'a>(&'a self, scripthash: &[u8]) -> impl Iterator<Item = Txid> + 'a {
        self.history_iter_scan_reverse(b'H', scripthash, None)
            .map(|row| TxHistoryRow::from_row(row).get_txid())
            .unique()
    }

    fn _history<'a>(
        &'a self,
        code: u8,
        hash: &[u8],
        last_seen_txid: Option<&'a Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> impl rayon::iter::ParallelIterator<Item = Result<(Transaction, BlockId)>> + 'a {
        let _timer_scan = self.start_timer("history");

        self.lookup_txns(
            self.history_iter_scan_reverse(code, hash, start_height)
                .map(|row| TxHistoryRow::from_row(row).get_txid())
                // XXX: unique() requires keeping an in-memory list of all txids, can we avoid that?
                .unique()
                // TODO seek directly to last seen tx without reading earlier rows
                .skip_while(move |txid| {
                    // skip until we reach the last_seen_txid
                    last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid != txid)
                })
                .skip(match last_seen_txid {
                    Some(_) => 1, // skip the last_seen_txid itself
                    None => 0,
                })
                .filter_map(move |txid| self.tx_confirming_block(&txid).map(|b| (txid, b))),
            limit,
        )
    }

    pub fn history_txids(&self, scripthash: &[u8], limit: usize) -> Vec<(Txid, BlockId)> {
        // scripthash lookup
        self._history_txids(b'H', scripthash, limit)
    }

    fn _history_txids(&self, code: u8, hash: &[u8], limit: usize) -> Vec<(Txid, BlockId)> {
        let _timer = self.start_timer("history_txids");
        self.history_iter_scan(code, hash, 0)
            .map(|row| TxHistoryRow::from_row(row).get_txid())
            .unique()
            .filter_map(|txid| self.tx_confirming_block(&txid).map(|b| (txid, b)))
            .take(limit)
            .collect()
    }

    pub fn history_group<'a>(
        &'a self,
        scripthashes: &[[u8; 32]],
        last_seen_txid: Option<&'a Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> impl rayon::iter::ParallelIterator<Item = Result<(Transaction, BlockId)>> + 'a {
        // scripthash lookup
        self._history_group(b'H', scripthashes, last_seen_txid, start_height, limit)
    }

    pub fn history_txids_iter_group(
        &self,
        scripthashes: &[[u8; 32]],
        start_height: Option<u32>,
    ) -> impl Iterator<Item = Txid> + '_ {
        self.history_iter_scan_group_reverse(b'H', scripthashes, start_height)
            .map(|row| TxHistoryRow::from_row(row).get_txid())
            .unique()
    }

    fn _history_group<'a>(
        &'a self,
        code: u8,
        hashes: &[[u8; 32]],
        last_seen_txid: Option<&'a Txid>,
        start_height: Option<u32>,
        limit: usize,
    ) -> impl rayon::iter::ParallelIterator<Item = Result<(Transaction, BlockId)>> + 'a {
        debug!("limit {} | last_seen {:?}", limit, last_seen_txid);
        let _timer_scan = self.start_timer("history_group");

        self.lookup_txns(
            self.history_iter_scan_group_reverse(code, hashes, start_height)
                .map(|row| TxHistoryRow::from_row(row).get_txid())
                // XXX: unique() requires keeping an in-memory list of all txids, can we avoid that?
                .unique()
                .skip_while(move |txid| {
                    // we already seeked to the last txid at this height
                    // now skip just past the last_seen_txid itself
                    last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid != txid)
                })
                .skip(match last_seen_txid {
                    Some(_) => 1, // skip the last_seen_txid itself
                    None => 0,
                })
                .filter_map(move |txid| self.tx_confirming_block(&txid).map(|b| (txid, b))),
            limit,
        )
    }

    // TODO: avoid duplication with stats/stats_delta?
    pub fn utxo(&self, scripthash: &[u8], limit: usize, flush: DBFlush) -> Result<Vec<Utxo>> {
        let _timer = self.start_timer("utxo");

        // get the last known utxo set and the blockhash it was updated for.
        // invalidates the cache if the block was orphaned.
        let cache: Option<(UtxoMap, usize)> = self
            .store
            .cache_db
            .get(&UtxoCacheRow::key(scripthash))
            .map(|c| bincode_util::deserialize_little(&c).unwrap())
            .and_then(|(utxos_cache, blockhash)| {
                self.height_by_hash(&blockhash)
                    .map(|height| (utxos_cache, height))
            })
            .map(|(utxos_cache, height)| (from_utxo_cache(utxos_cache, self), height));
        let had_cache = cache.is_some();

        // update utxo set with new transactions since
        let (newutxos, lastblock, processed_items) = cache.map_or_else(
            || self.utxo_delta(scripthash, HashMap::new(), 0, limit),
            |(oldutxos, blockheight)| self.utxo_delta(scripthash, oldutxos, blockheight + 1, limit),
        )?;

        // save updated utxo set to cache
        if let Some(lastblock) = lastblock {
            if had_cache || processed_items > MIN_HISTORY_ITEMS_TO_CACHE {
                self.store.cache_db.write(
                    vec![UtxoCacheRow::new(scripthash, &newutxos, &lastblock).into_row()],
                    flush,
                );
            }
        }

        // format as Utxo objects
        Ok(newutxos
            .into_iter()
            .map(|(outpoint, (blockid, value))| {
                // in elements/liquid chains, we have to lookup the txo in order to get its
                // associated asset. the asset information could be kept in the db history rows
                // alongside the value to avoid this.
                #[cfg(feature = "liquid")]
                let txo = self.lookup_txo(&outpoint).expect("missing utxo");

                Utxo {
                    txid: outpoint.txid,
                    vout: outpoint.vout,
                    value,
                    confirmed: Some(blockid),

                    #[cfg(feature = "liquid")]
                    asset: txo.asset,
                    #[cfg(feature = "liquid")]
                    nonce: txo.nonce,
                    #[cfg(feature = "liquid")]
                    witness: txo.witness,
                }
            })
            .collect())
    }

    fn utxo_delta(
        &self,
        scripthash: &[u8],
        init_utxos: UtxoMap,
        start_height: usize,
        limit: usize,
    ) -> Result<(UtxoMap, Option<BlockHash>, usize)> {
        let _timer = self.start_timer("utxo_delta");
        let history_iter = self
            .history_iter_scan(b'H', scripthash, start_height)
            .map(TxHistoryRow::from_row)
            .filter_map(|history| {
                self.tx_confirming_block(&history.get_txid())
                    // drop history entries that were previously confirmed in a re-orged block and later
                    // confirmed again at a different height
                    .filter(|blockid| blockid.height == history.key.confirmed_height as usize)
                    .map(|b| (history, b))
            });

        let mut utxos = init_utxos;
        let mut processed_items = 0;
        let mut lastblock = None;

        for (history, blockid) in history_iter {
            processed_items += 1;
            lastblock = Some(blockid.hash);

            match history.key.txinfo {
                TxHistoryInfo::Funding(ref info) => {
                    utxos.insert(history.get_funded_outpoint(), (blockid, info.value))
                }
                TxHistoryInfo::Spending(_) => utxos.remove(&history.get_funded_outpoint()),
                #[cfg(feature = "liquid")]
                TxHistoryInfo::Issuing(_)
                | TxHistoryInfo::Burning(_)
                | TxHistoryInfo::Pegin(_)
                | TxHistoryInfo::Pegout(_) => unreachable!(),
            };

            // abort if the utxo set size excedees the limit at any point in time
            if utxos.len() > limit {
                bail!(ErrorKind::TooManyUtxos(limit))
            }
        }

        Ok((utxos, lastblock, processed_items))
    }

    pub fn stats(&self, scripthash: &[u8], flush: DBFlush) -> ScriptStats {
        let _timer = self.start_timer("stats");

        // get the last known stats and the blockhash they are updated for.
        // invalidates the cache if the block was orphaned or if values are out of sync.
        let cache: Option<(ScriptStats, usize)> = self
            .store
            .cache_db
            .get(&StatsCacheRow::key(scripthash))
            .map(|c| bincode_util::deserialize_little::<(ScriptStats, BlockHash)>(&c).unwrap())
            // Check that the values are sane (No negative balances or balances with 0 utxos)
            .filter(|(stats, _)| stats.is_sane())
            .and_then(|(stats, blockhash)| {
                self.height_by_hash(&blockhash)
                    .map(|height| (stats, height))
            });

        // update stats with new transactions since
        let (newstats, lastblock) = cache.map_or_else(
            || self.stats_delta(scripthash, ScriptStats::default(), 0),
            |(oldstats, blockheight)| self.stats_delta(scripthash, oldstats, blockheight + 1),
        );

        // save updated stats to cache
        if let Some(lastblock) = lastblock {
            if newstats.funded_txo_count + newstats.spent_txo_count > MIN_HISTORY_ITEMS_TO_CACHE {
                self.store.cache_db.write(
                    vec![StatsCacheRow::new(scripthash, &newstats, &lastblock).into_row()],
                    flush,
                );
            }
        }

        newstats
    }

    fn stats_delta(
        &self,
        scripthash: &[u8],
        init_stats: ScriptStats,
        start_height: usize,
    ) -> (ScriptStats, Option<BlockHash>) {
        let _timer = self.start_timer("stats_delta"); // TODO: measure also the number of txns processed.
        let history_iter = self
            .history_iter_scan(b'H', scripthash, start_height)
            .map(TxHistoryRow::from_row)
            .filter_map(|history| {
                self.tx_confirming_block(&history.get_txid())
                    // drop history entries that were previously confirmed in a re-orged block and later
                    // confirmed again at a different height
                    .filter(|blockid| blockid.height == history.key.confirmed_height as usize)
                    .map(|blockid| (history, blockid))
            });

        let mut stats = init_stats;
        let mut seen_txids = HashSet::new();
        let mut lastblock = None;

        for (history, blockid) in history_iter {
            if lastblock != Some(blockid.hash) {
                seen_txids.clear();
            }

            if seen_txids.insert(history.get_txid()) {
                stats.tx_count += 1;
            }

            match history.key.txinfo {
                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Funding(ref info) => {
                    stats.funded_txo_count += 1;
                    stats.funded_txo_sum += info.value;
                }

                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Spending(ref info) => {
                    stats.spent_txo_count += 1;
                    stats.spent_txo_sum += info.value;
                }

                #[cfg(feature = "liquid")]
                TxHistoryInfo::Funding(_) => {
                    stats.funded_txo_count += 1;
                }

                #[cfg(feature = "liquid")]
                TxHistoryInfo::Spending(_) => {
                    stats.spent_txo_count += 1;
                }

                #[cfg(feature = "liquid")]
                TxHistoryInfo::Issuing(_)
                | TxHistoryInfo::Burning(_)
                | TxHistoryInfo::Pegin(_)
                | TxHistoryInfo::Pegout(_) => unreachable!(),
            }

            lastblock = Some(blockid.hash);
        }

        (stats, lastblock)
    }

    pub fn address_search(&self, prefix: &str, limit: usize) -> Vec<String> {
        let _timer_scan = self.start_timer("address_search");
        self.store
            .history_db
            .iter_scan(&addr_search_filter(prefix))
            .take(limit)
            .map(|row| std::str::from_utf8(&row.key[1..]).unwrap().to_string())
            .collect()
    }

    fn header_by_hash(&self, hash: &BlockHash) -> Option<HeaderEntry> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_blockhash(hash)
            .cloned()
    }

    // Get the height of a blockhash, only if its part of the best chain
    pub fn height_by_hash(&self, hash: &BlockHash) -> Option<usize> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_blockhash(hash)
            .map(|header| header.height())
    }

    pub fn header_by_height(&self, height: usize) -> Option<HeaderEntry> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_height(height)
            .cloned()
    }

    pub fn hash_by_height(&self, height: usize) -> Option<BlockHash> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_height(height)
            .map(|entry| *entry.hash())
    }

    pub fn blockid_by_height(&self, height: usize) -> Option<BlockId> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_height(height)
            .map(BlockId::from)
    }

    // returns None for orphaned blocks
    pub fn blockid_by_hash(&self, hash: &BlockHash) -> Option<BlockId> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_blockhash(hash)
            .map(BlockId::from)
    }

    pub fn best_height(&self) -> usize {
        self.store.indexed_headers.read().unwrap().len() - 1
    }

    pub fn best_hash(&self) -> BlockHash {
        *self.store.indexed_headers.read().unwrap().tip()
    }

    pub fn best_header(&self) -> HeaderEntry {
        let headers = self.store.indexed_headers.read().unwrap();
        headers
            .header_by_blockhash(headers.tip())
            .expect("missing chain tip")
            .clone()
    }

    // TODO: can we pass txids as a "generic iterable"?
    // TODO: should also use a custom ThreadPoolBuilder?
    pub fn lookup_txns<'a, I>(
        &'a self,
        txids: I,
        take: usize,
    ) -> impl rayon::iter::ParallelIterator<Item = Result<(Transaction, BlockId)>> + 'a
    where
        I: Iterator<Item = (Txid, BlockId)> + Send + rayon::iter::ParallelBridge + 'a,
    {
        txids
            .take(take)
            .par_bridge()
            .map(move |(txid, blockid)| -> Result<_> {
                Ok((
                    self.lookup_txn(&txid, Some(&blockid.hash))
                        .chain_err(|| "missing tx")?,
                    blockid,
                ))
            })
    }

    pub fn lookup_txn(&self, txid: &Txid, blockhash: Option<&BlockHash>) -> Option<Transaction> {
        let _timer = self.start_timer("lookup_txn");
        self.lookup_raw_txn(txid, blockhash).map(|rawtx| {
            let txn: Transaction = deserialize(&rawtx).expect("failed to parse Transaction");
            assert_eq!(*txid, txn.txid());
            txn
        })
    }

    pub fn lookup_raw_txn(&self, txid: &Txid, blockhash: Option<&BlockHash>) -> Option<Bytes> {
        let _timer = self.start_timer("lookup_raw_txn");

        if self.light_mode {
            let queried_blockhash =
                blockhash.map_or_else(|| self.tx_confirming_block(txid).map(|b| b.hash), |_| None);
            let blockhash = blockhash.or(queried_blockhash.as_ref())?;
            // TODO fetch transaction as binary from REST API instead of as hex
            let txhex = self
                .daemon
                .gettransaction_raw(txid, blockhash, false)
                .ok()?;
            Some(hex::decode(txhex.as_str().unwrap()).unwrap())
        } else {
            self.store.txstore_db.get(&TxRow::key(&txid[..]))
        }
    }

    pub fn lookup_txo(&self, outpoint: &OutPoint) -> Option<TxOut> {
        let _timer = self.start_timer("lookup_txo");
        lookup_txo(&self.store.txstore_db, outpoint)
    }

    pub fn lookup_txos(&self, outpoints: &BTreeSet<OutPoint>) -> HashMap<OutPoint, TxOut> {
        let _timer = self.start_timer("lookup_txos");
        lookup_txos(&self.store.txstore_db, outpoints, false)
    }

    pub fn lookup_avail_txos(&self, outpoints: &BTreeSet<OutPoint>) -> HashMap<OutPoint, TxOut> {
        let _timer = self.start_timer("lookup_available_txos");
        lookup_txos(&self.store.txstore_db, outpoints, true)
    }

    pub fn lookup_spend(&self, outpoint: &OutPoint) -> Option<SpendingInput> {
        let _timer = self.start_timer("lookup_spend");
        self.store
            .history_db
            .iter_scan(&TxEdgeRow::filter(outpoint))
            .map(TxEdgeRow::from_row)
            .find_map(|edge| {
                let txid: Txid = deserialize(&edge.key.spending_txid).unwrap();
                self.tx_confirming_block(&txid).map(|b| SpendingInput {
                    txid,
                    vin: edge.key.spending_vin,
                    confirmed: Some(b),
                })
            })
    }
    pub fn tx_confirming_block(&self, txid: &Txid) -> Option<BlockId> {
        let _timer = self.start_timer("tx_confirming_block");
        let headers = self.store.indexed_headers.read().unwrap();
        self.store
            .txstore_db
            .iter_scan(&TxConfRow::filter(&txid[..]))
            .map(TxConfRow::from_row)
            // header_by_blockhash only returns blocks that are part of the best chain,
            // or None for orphaned blocks.
            .filter_map(|conf| {
                headers.header_by_blockhash(&deserialize(&conf.key.blockhash).unwrap())
            })
            .next()
            .map(BlockId::from)
    }

    pub fn get_block_status(&self, hash: &BlockHash) -> BlockStatus {
        // TODO differentiate orphaned and non-existing blocks? telling them apart requires
        // an additional db read.

        let headers = self.store.indexed_headers.read().unwrap();

        // header_by_blockhash only returns blocks that are part of the best chain,
        // or None for orphaned blocks.
        headers
            .header_by_blockhash(hash)
            .map_or_else(BlockStatus::orphaned, |header| {
                BlockStatus::confirmed(
                    header.height(),
                    headers
                        .header_by_height(header.height() + 1)
                        .map(|h| *h.hash()),
                )
            })
    }

    #[cfg(not(feature = "liquid"))]
    pub fn get_merkleblock_proof(&self, txid: &Txid) -> Option<MerkleBlock> {
        let _timer = self.start_timer("get_merkleblock_proof");
        let blockid = self.tx_confirming_block(txid)?;
        let headerentry = self.header_by_hash(&blockid.hash)?;
        let block_txids = self.get_block_txids(&blockid.hash)?;

        Some(MerkleBlock::from_header_txids_with_predicate(
            headerentry.header(),
            &block_txids,
            |t| t == txid,
        ))
    }

    #[cfg(feature = "liquid")]
    pub fn asset_history<'a>(
        &'a self,
        asset_id: &'a AssetId,
        last_seen_txid: Option<&'a Txid>,
        limit: usize,
    ) -> impl rayon::iter::ParallelIterator<Item = Result<(Transaction, BlockId)>> + 'a {
        self._history(
            b'I',
            &asset_id.into_inner()[..],
            last_seen_txid,
            None,
            limit,
        )
    }

    #[cfg(feature = "liquid")]
    pub fn asset_history_txids(&self, asset_id: &AssetId, limit: usize) -> Vec<(Txid, BlockId)> {
        self._history_txids(b'I', &asset_id.into_inner()[..], limit)
    }
}

fn load_blockhashes(db: &DB, prefix: &[u8]) -> HashSet<BlockHash> {
    db.iter_scan(prefix)
        .map(BlockRow::from_row)
        .map(|r| deserialize(&r.key.hash).expect("failed to parse BlockHash"))
        .collect()
}

fn load_blockheaders(db: &DB) -> HashMap<BlockHash, BlockHeader> {
    db.iter_scan(&BlockRow::header_filter())
        .map(BlockRow::from_row)
        .map(|r| {
            let key: BlockHash = deserialize(&r.key.hash).expect("failed to parse BlockHash");
            let value: BlockHeader = deserialize(&r.value).expect("failed to parse BlockHeader");
            (key, value)
        })
        .collect()
}

fn add_blocks(block_entries: &[BlockEntry], iconfig: &IndexerConfig) -> Vec<DBRow> {
    // persist individual transactions:
    //      T{txid} → {rawtx}
    //      C{txid}{blockhash}{height} →
    //      O{txid}{index} → {txout}
    // persist block headers', block txids' and metadata rows:
    //      B{blockhash} → {header}
    //      X{blockhash} → {txid1}...{txidN}
    //      M{blockhash} → {tx_count}{size}{weight}
    block_entries
        .par_iter() // serialization is CPU-intensive
        .map(|b| {
            let mut rows = vec![];
            let blockhash = full_hash(&b.entry.hash()[..]);
            let txids: Vec<Txid> = b.block.txdata.iter().map(|tx| tx.txid()).collect();
            for tx in &b.block.txdata {
                add_transaction(tx, blockhash, &mut rows, iconfig);
            }

            if !iconfig.light_mode {
                rows.push(BlockRow::new_txids(blockhash, &txids).into_row());
                rows.push(BlockRow::new_meta(blockhash, &BlockMeta::from(b)).into_row());
            }

            rows.push(BlockRow::new_header(b).into_row());
            rows.push(BlockRow::new_done(blockhash).into_row()); // mark block as "added"
            rows
        })
        .flatten()
        .collect()
}

fn add_transaction(
    tx: &Transaction,
    blockhash: FullHash,
    rows: &mut Vec<DBRow>,
    iconfig: &IndexerConfig,
) {
    rows.push(TxConfRow::new(tx, blockhash).into_row());

    if !iconfig.light_mode {
        rows.push(TxRow::new(tx).into_row());
    }

    let txid = full_hash(&tx.txid()[..]);
    for (txo_index, txo) in tx.output.iter().enumerate() {
        if is_spendable(txo) {
            rows.push(TxOutRow::new(&txid, txo_index, txo).into_row());
        }
    }
}

fn get_previous_txos(block_entries: &[BlockEntry]) -> BTreeSet<OutPoint> {
    block_entries
        .iter()
        .flat_map(|b| b.block.txdata.iter())
        .flat_map(|tx| {
            tx.input
                .iter()
                .filter(|txin| has_prevout(txin))
                .map(|txin| txin.previous_output)
        })
        .collect()
}

fn lookup_txos(
    txstore_db: &DB,
    outpoints: &BTreeSet<OutPoint>,
    allow_missing: bool,
) -> HashMap<OutPoint, TxOut> {
    let mut loop_count = 10;
    let pool = loop {
        match rayon::ThreadPoolBuilder::new()
            .num_threads(16) // we need to saturate SSD IOPS
            .thread_name(|i| format!("lookup-txo-{}", i))
            .build()
        {
            Ok(pool) => break pool,
            Err(e) => {
                if loop_count == 0 {
                    panic!("schema::lookup_txos failed to create a ThreadPool: {}", e);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                loop_count -= 1;
            }
        }
    };
    pool.install(|| {
        outpoints
            .par_iter()
            .filter_map(|outpoint| {
                lookup_txo(txstore_db, outpoint)
                    .or_else(|| {
                        if !allow_missing {
                            panic!("missing txo {} in {:?}", outpoint, txstore_db);
                        }
                        None
                    })
                    .map(|txo| (*outpoint, txo))
            })
            .collect()
    })
}

fn lookup_txo(txstore_db: &DB, outpoint: &OutPoint) -> Option<TxOut> {
    txstore_db
        .get(&TxOutRow::key(outpoint))
        .map(|val| deserialize(&val).expect("failed to parse TxOut"))
}

fn index_blocks(
    block_entries: &[BlockEntry],
    previous_txos_map: &HashMap<OutPoint, TxOut>,
    iconfig: &IndexerConfig,
) -> Vec<DBRow> {
    block_entries
        .par_iter() // serialization is CPU-intensive
        .map(|b| {
            let mut rows = vec![];
            for (idx, tx) in b.block.txdata.iter().enumerate() {
                let height = b.entry.height() as u32;
                index_transaction(
                    tx,
                    height,
                    idx as u16,
                    previous_txos_map,
                    &mut rows,
                    iconfig,
                );
            }
            rows.push(BlockRow::new_done(full_hash(&b.entry.hash()[..])).into_row()); // mark block as "indexed"
            rows
        })
        .flatten()
        .collect()
}

// TODO: return an iterator?
fn index_transaction(
    tx: &Transaction,
    confirmed_height: u32,
    tx_position: u16,
    previous_txos_map: &HashMap<OutPoint, TxOut>,
    rows: &mut Vec<DBRow>,
    iconfig: &IndexerConfig,
) {
    // persist history index:
    //      H{funding-scripthash}{spending-height}{spending-block-pos}S{spending-txid:vin}{funding-txid:vout} → ""
    //      H{funding-scripthash}{funding-height}{funding-block-pos}F{funding-txid:vout} → ""
    // persist "edges" for fast is-this-TXO-spent check
    //      S{funding-txid:vout}{spending-txid:vin} → ""
    let txid = full_hash(&tx.txid()[..]);
    for (txo_index, txo) in tx.output.iter().enumerate() {
        if is_spendable(txo) || iconfig.index_unspendables {
            let history = TxHistoryRow::new(
                &txo.script_pubkey,
                confirmed_height,
                tx_position,
                TxHistoryInfo::Funding(FundingInfo {
                    txid,
                    vout: txo_index as u32,
                    value: txo.value,
                }),
            );
            rows.push(history.into_row());

            if iconfig.address_search {
                if let Some(row) = addr_search_row(&txo.script_pubkey, iconfig.network) {
                    rows.push(row);
                }
            }
        }
    }
    for (txi_index, txi) in tx.input.iter().enumerate() {
        if !has_prevout(txi) {
            continue;
        }
        let prev_txo = previous_txos_map
            .get(&txi.previous_output)
            .unwrap_or_else(|| panic!("missing previous txo {}", txi.previous_output));

        let history = TxHistoryRow::new(
            &prev_txo.script_pubkey,
            confirmed_height,
            tx_position,
            TxHistoryInfo::Spending(SpendingInfo {
                txid,
                vin: txi_index as u32,
                prev_txid: full_hash(&txi.previous_output.txid[..]),
                prev_vout: txi.previous_output.vout,
                value: prev_txo.value,
            }),
        );
        rows.push(history.into_row());

        let edge = TxEdgeRow::new(
            full_hash(&txi.previous_output.txid[..]),
            txi.previous_output.vout,
            txid,
            txi_index as u32,
        );
        rows.push(edge.into_row());
    }

    // Index issued assets & native asset pegins/pegouts/burns
    #[cfg(feature = "liquid")]
    asset::index_confirmed_tx_assets(
        tx,
        confirmed_height,
        tx_position,
        iconfig.network,
        iconfig.parent_network,
        rows,
    );
}

fn addr_search_row(spk: &Script, network: Network) -> Option<DBRow> {
    spk.to_address_str(network).map(|address| DBRow {
        key: [b"a", address.as_bytes()].concat(),
        value: vec![],
    })
}

fn addr_search_filter(prefix: &str) -> Bytes {
    [b"a", prefix.as_bytes()].concat()
}

// TODO: replace by a separate opaque type (similar to Sha256dHash, but without the "double")
pub type FullHash = [u8; 32]; // serialized SHA256 result

pub fn compute_script_hash(script: &Script) -> FullHash {
    let mut hasher = Sha256::new();
    hasher.update(script.as_bytes());
    hasher.finalize()[..]
        .try_into()
        .expect("SHA256 size is 32 bytes")
}

pub fn parse_hash(hash: &FullHash) -> Sha256dHash {
    deserialize(hash).expect("failed to parse Sha256dHash")
}

#[derive(Serialize, Deserialize)]
struct TxRowKey {
    code: u8,
    txid: FullHash,
}

struct TxRow {
    key: TxRowKey,
    value: Bytes, // raw transaction
}

impl TxRow {
    fn new(txn: &Transaction) -> TxRow {
        let txid = full_hash(&txn.txid()[..]);
        TxRow {
            key: TxRowKey { code: b'T', txid },
            value: serialize(txn),
        }
    }

    fn key(prefix: &[u8]) -> Bytes {
        [b"T", prefix].concat()
    }

    fn into_row(self) -> DBRow {
        let TxRow { key, value } = self;
        DBRow {
            key: bincode_util::serialize_little(&key).unwrap(),
            value,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxConfKey {
    code: u8,
    txid: FullHash,
    blockhash: FullHash,
}

struct TxConfRow {
    key: TxConfKey,
}

impl TxConfRow {
    fn new(txn: &Transaction, blockhash: FullHash) -> TxConfRow {
        let txid = full_hash(&txn.txid()[..]);
        TxConfRow {
            key: TxConfKey {
                code: b'C',
                txid,
                blockhash,
            },
        }
    }

    fn filter(prefix: &[u8]) -> Bytes {
        [b"C", prefix].concat()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_little(&self.key).unwrap(),
            value: vec![],
        }
    }

    fn from_row(row: DBRow) -> Self {
        TxConfRow {
            key: bincode_util::deserialize_little(&row.key).expect("failed to parse TxConfKey"),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxOutKey {
    code: u8,
    txid: FullHash,
    vout: u32,
}

struct TxOutRow {
    key: TxOutKey,
    value: Bytes, // serialized output
}

impl TxOutRow {
    fn new(txid: &FullHash, vout: usize, txout: &TxOut) -> TxOutRow {
        TxOutRow {
            key: TxOutKey {
                code: b'O',
                txid: *txid,
                vout: vout as u32,
            },
            value: serialize(txout),
        }
    }
    fn key(outpoint: &OutPoint) -> Bytes {
        bincode_util::serialize_little(&TxOutKey {
            code: b'O',
            txid: full_hash(&outpoint.txid[..]),
            vout: outpoint.vout,
        })
        .unwrap()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BlockKey {
    code: u8,
    hash: FullHash,
}

struct BlockRow {
    key: BlockKey,
    value: Bytes, // serialized output
}

impl BlockRow {
    fn new_header(block_entry: &BlockEntry) -> BlockRow {
        BlockRow {
            key: BlockKey {
                code: b'B',
                hash: full_hash(&block_entry.entry.hash()[..]),
            },
            value: serialize(&block_entry.block.header),
        }
    }

    fn new_txids(hash: FullHash, txids: &[Txid]) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'X', hash },
            value: bincode_util::serialize_little(txids).unwrap(),
        }
    }

    fn new_meta(hash: FullHash, meta: &BlockMeta) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'M', hash },
            value: bincode_util::serialize_little(meta).unwrap(),
        }
    }

    fn new_done(hash: FullHash) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'D', hash },
            value: vec![],
        }
    }

    fn header_filter() -> Bytes {
        b"B".to_vec()
    }

    fn txids_key(hash: FullHash) -> Bytes {
        [b"X", &hash[..]].concat()
    }

    fn meta_key(hash: FullHash) -> Bytes {
        [b"M", &hash[..]].concat()
    }

    fn done_filter() -> Bytes {
        b"D".to_vec()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }

    fn from_row(row: DBRow) -> Self {
        BlockRow {
            key: bincode_util::deserialize_little(&row.key).unwrap(),
            value: row.value,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct FundingInfo {
    pub txid: FullHash,
    pub vout: u32,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct SpendingInfo {
    pub txid: FullHash, // spending transaction
    pub vin: u32,
    pub prev_txid: FullHash, // funding transaction
    pub prev_vout: u32,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum TxHistoryInfo {
    // If a spend and a fund for the same scripthash
    // occur in the same tx, spends should come first.
    // This ordering comes from the enum order.
    Spending(SpendingInfo),
    Funding(FundingInfo),

    #[cfg(feature = "liquid")]
    Issuing(asset::IssuingInfo),
    #[cfg(feature = "liquid")]
    Burning(asset::BurningInfo),
    #[cfg(feature = "liquid")]
    Pegin(peg::PeginInfo),
    #[cfg(feature = "liquid")]
    Pegout(peg::PegoutInfo),
}

impl TxHistoryInfo {
    pub fn get_txid(&self) -> Txid {
        match self {
            TxHistoryInfo::Funding(FundingInfo { txid, .. })
            | TxHistoryInfo::Spending(SpendingInfo { txid, .. }) => deserialize(txid),

            #[cfg(feature = "liquid")]
            TxHistoryInfo::Issuing(asset::IssuingInfo { txid, .. })
            | TxHistoryInfo::Burning(asset::BurningInfo { txid, .. })
            | TxHistoryInfo::Pegin(peg::PeginInfo { txid, .. })
            | TxHistoryInfo::Pegout(peg::PegoutInfo { txid, .. }) => deserialize(txid),
        }
        .expect("cannot parse Txid")
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct TxHistoryKey {
    pub code: u8,              // H for script history or I for asset history (elements only)
    pub hash: FullHash, // either a scripthash (always on bitcoin) or an asset id (elements only)
    pub confirmed_height: u32, // MUST be serialized as big-endian (for correct scans).
    pub tx_position: u16, // MUST be serialized as big-endian (for correct scans). Position in block.
    pub txinfo: TxHistoryInfo,
}

pub struct TxHistoryRow {
    pub key: TxHistoryKey,
}

impl TxHistoryRow {
    fn new(
        script: &Script,
        confirmed_height: u32,
        tx_position: u16,
        txinfo: TxHistoryInfo,
    ) -> Self {
        let key = TxHistoryKey {
            code: b'H',
            hash: compute_script_hash(script),
            confirmed_height,
            tx_position,
            txinfo,
        };
        TxHistoryRow { key }
    }

    fn filter(code: u8, hash_prefix: &[u8]) -> Bytes {
        [&[code], hash_prefix].concat()
    }

    fn prefix_end(code: u8, hash: &[u8]) -> Bytes {
        bincode_util::serialize_big(&(code, full_hash(hash), u32::MAX)).unwrap()
    }

    fn prefix_height(code: u8, hash: &[u8], height: u32) -> Bytes {
        bincode_util::serialize_big(&(code, full_hash(hash), height)).unwrap()
    }

    // prefix representing the end of a given block (used for reverse scans)
    fn prefix_height_end(code: u8, hash: &[u8], height: u32) -> Bytes {
        // u16::MAX for the tx_position ensures we get all transactions at this height
        bincode_util::serialize_big(&(code, full_hash(hash), height, u16::MAX)).unwrap()
    }

    pub fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_big(&self.key).unwrap(),
            value: vec![],
        }
    }

    pub fn from_row(row: DBRow) -> Self {
        let key =
            bincode_util::deserialize_big(&row.key).expect("failed to deserialize TxHistoryKey");
        TxHistoryRow { key }
    }

    pub fn get_txid(&self) -> Txid {
        self.key.txinfo.get_txid()
    }
    fn get_funded_outpoint(&self) -> OutPoint {
        self.key.txinfo.get_funded_outpoint()
    }
}

impl TxHistoryInfo {
    // for funding rows, returns the funded output.
    // for spending rows, returns the spent previous output.
    pub fn get_funded_outpoint(&self) -> OutPoint {
        match self {
            TxHistoryInfo::Funding(ref info) => OutPoint {
                txid: deserialize(&info.txid).unwrap(),
                vout: info.vout,
            },
            TxHistoryInfo::Spending(ref info) => OutPoint {
                txid: deserialize(&info.prev_txid).unwrap(),
                vout: info.prev_vout,
            },
            #[cfg(feature = "liquid")]
            TxHistoryInfo::Issuing(_)
            | TxHistoryInfo::Burning(_)
            | TxHistoryInfo::Pegin(_)
            | TxHistoryInfo::Pegout(_) => unreachable!(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct TxHistorySummary {
    txid: Txid,
    height: usize,
    value: i64,
    time: u32,
    tx_position: u16,
}

#[derive(Serialize, Deserialize)]
struct TxEdgeKey {
    code: u8,
    funding_txid: FullHash,
    funding_vout: u32,
    spending_txid: FullHash,
    spending_vin: u32,
}

struct TxEdgeRow {
    key: TxEdgeKey,
}

impl TxEdgeRow {
    fn new(
        funding_txid: FullHash,
        funding_vout: u32,
        spending_txid: FullHash,
        spending_vin: u32,
    ) -> Self {
        let key = TxEdgeKey {
            code: b'S',
            funding_txid,
            funding_vout,
            spending_txid,
            spending_vin,
        };
        TxEdgeRow { key }
    }

    fn filter(outpoint: &OutPoint) -> Bytes {
        // TODO build key without using bincode? [ b"S", &outpoint.txid[..], outpoint.vout?? ].concat()
        bincode_util::serialize_little(&(b'S', full_hash(&outpoint.txid[..]), outpoint.vout))
            .unwrap()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_little(&self.key).unwrap(),
            value: vec![],
        }
    }

    fn from_row(row: DBRow) -> Self {
        TxEdgeRow {
            key: bincode_util::deserialize_little(&row.key)
                .expect("failed to deserialize TxEdgeKey"),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ScriptCacheKey {
    code: u8,
    scripthash: FullHash,
}

struct StatsCacheRow {
    key: ScriptCacheKey,
    value: Bytes,
}

impl StatsCacheRow {
    fn new(scripthash: &[u8], stats: &ScriptStats, blockhash: &BlockHash) -> Self {
        StatsCacheRow {
            key: ScriptCacheKey {
                code: b'A',
                scripthash: full_hash(scripthash),
            },
            value: bincode_util::serialize_little(&(stats, blockhash)).unwrap(),
        }
    }

    pub fn key(scripthash: &[u8]) -> Bytes {
        [b"A", scripthash].concat()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }
}

type CachedUtxoMap = HashMap<(Txid, u32), (u32, Value)>; // (txid,vout) => (block_height,output_value)

struct UtxoCacheRow {
    key: ScriptCacheKey,
    value: Bytes,
}

impl UtxoCacheRow {
    fn new(scripthash: &[u8], utxos: &UtxoMap, blockhash: &BlockHash) -> Self {
        let utxos_cache = make_utxo_cache(utxos);

        UtxoCacheRow {
            key: ScriptCacheKey {
                code: b'U',
                scripthash: full_hash(scripthash),
            },
            value: bincode_util::serialize_little(&(utxos_cache, blockhash)).unwrap(),
        }
    }

    pub fn key(scripthash: &[u8]) -> Bytes {
        [b"U", scripthash].concat()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode_util::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }
}

// keep utxo cache with just the block height (the hash/timestamp are read later from the headers to reconstruct BlockId)
// and use a (txid,vout) tuple instead of OutPoints (they don't play nicely with bincode serialization)
fn make_utxo_cache(utxos: &UtxoMap) -> CachedUtxoMap {
    utxos
        .iter()
        .map(|(outpoint, (blockid, value))| {
            (
                (outpoint.txid, outpoint.vout),
                (blockid.height as u32, *value),
            )
        })
        .collect()
}

fn from_utxo_cache(utxos_cache: CachedUtxoMap, chain: &ChainQuery) -> UtxoMap {
    utxos_cache
        .into_iter()
        .map(|((txid, vout), (height, value))| {
            let outpoint = OutPoint { txid, vout };
            let blockid = chain
                .blockid_by_height(height as usize)
                .expect("missing blockheader for valid utxo cache entry");
            (outpoint, (blockid, value))
        })
        .collect()
}

#[cfg(all(test, feature = "liquid"))]
mod tests {
    use super::{DBRow, TxHistoryRow};
    use crate::chain::Value;
    use std::convert::TryInto;

    #[test]
    fn tx_history_row_ser_deser_tests() {
        #[rustfmt::skip]
        let inputs = [
            vec![
                // code
                72,
                // hash
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                   1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                // confirmed_height
                0, 0, 0, 2,
                // tx_position
                0, 3,
                // TxHistoryInfo variant (Funding)
                0, 0, 0, 1,
                // FundingInfo
                // txid
                2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
                   2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
                // vout
                0, 0, 0, 3,
                // Value variant (Explicit)
                0, 0, 0, 0, 0, 0, 0, 2,
                // number of tuple elements
                1,
                // Inner value (u64)
                7, 0, 0, 0, 0, 0, 0, 0,
            ],
            vec![
                72,
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                   1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                0, 0, 0, 2,
                0, 3,
                0, 0, 0, 1,
                2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
                   2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
                0, 0, 0, 3,
                // Value variant (Null)
                0, 0, 0, 0, 0, 0, 0, 1,
                // number of tuple elements
                0,
            ],
            vec![
                72,
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                   1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                0, 0, 0, 2,
                0, 3,
                0, 0, 0, 0,
                18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
                    18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
                0, 0, 0, 12,
                98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102,
                    98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102,
                0, 0, 0, 9,
                0, 0, 0, 0, 0, 0, 0, 2,
                1,
                14, 0, 0, 0, 0, 0, 0, 0,
            ],
            vec![
                72,
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                   1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                0, 0, 0, 2,
                0, 3,
                0, 0, 0, 0,
                18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
                    18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
                0, 0, 0, 12,
                98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102,
                    98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102,
                0, 0, 0, 9,
                0, 0, 0, 0, 0, 0, 0, 1,
                0,
            ],
        ];
        let expected = [
            super::TxHistoryRow {
                key: super::TxHistoryKey {
                    code: b'H',
                    hash: [1; 32],
                    confirmed_height: 2,
                    tx_position: 3,
                    txinfo: super::TxHistoryInfo::Funding(super::FundingInfo {
                        txid: [2; 32],
                        vout: 3,
                        value: Value::Explicit(7),
                    }),
                },
            },
            super::TxHistoryRow {
                key: super::TxHistoryKey {
                    code: b'H',
                    hash: [1; 32],
                    confirmed_height: 2,
                    tx_position: 3,
                    txinfo: super::TxHistoryInfo::Funding(super::FundingInfo {
                        txid: [2; 32],
                        vout: 3,
                        value: Value::Null,
                    }),
                },
            },
            super::TxHistoryRow {
                key: super::TxHistoryKey {
                    code: b'H',
                    hash: [1; 32],
                    confirmed_height: 2,
                    tx_position: 3,
                    txinfo: super::TxHistoryInfo::Spending(super::SpendingInfo {
                        txid: [18; 32],
                        vin: 12,
                        prev_txid: "beef".repeat(8).as_bytes().try_into().unwrap(),
                        prev_vout: 9,
                        value: Value::Explicit(14),
                    }),
                },
            },
            super::TxHistoryRow {
                key: super::TxHistoryKey {
                    code: b'H',
                    hash: [1; 32],
                    confirmed_height: 2,
                    tx_position: 3,
                    txinfo: super::TxHistoryInfo::Spending(super::SpendingInfo {
                        txid: [18; 32],
                        vin: 12,
                        prev_txid: "beef".repeat(8).as_bytes().try_into().unwrap(),
                        prev_vout: 9,
                        value: Value::Null,
                    }),
                },
            },
        ];
        for (expected_row, input) in
            IntoIterator::into_iter(expected).zip(IntoIterator::into_iter(inputs))
        {
            let input_row = DBRow {
                key: input,
                value: vec![],
            };
            assert_eq!(TxHistoryRow::from_row(input_row).key, expected_row.key);
        }

        #[rustfmt::skip]
        assert_eq!(
            TxHistoryRow::prefix_height(b'H', "beef".repeat(8).as_bytes(), 1337),
            vec![
                // code
                72,
                // hash
                98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102,
                98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102, 98, 101, 101, 102,
                // height
                0, 0, 5, 57
            ]
        );
    }
}
