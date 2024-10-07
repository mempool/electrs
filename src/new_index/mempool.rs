use bounded_vec_deque::BoundedVecDeque;
use itertools::Itertools;

#[cfg(not(feature = "liquid"))]
use bitcoin::consensus::encode::serialize;
#[cfg(feature = "liquid")]
use elements::{encode::serialize, AssetId};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::iter::FromIterator;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::chain::{deserialize, Network, OutPoint, Transaction, TxOut, Txid};
use crate::config::Config;
use crate::daemon::Daemon;
use crate::errors::*;
use crate::metrics::{GaugeVec, HistogramOpts, HistogramVec, MetricOpts, Metrics};
use crate::new_index::{
    compute_script_hash, schema::FullHash, ChainQuery, FundingInfo, ScriptStats, SpendingInfo,
    SpendingInput, TxHistoryInfo, Utxo,
};
use crate::util::fees::{make_fee_histogram, TxFeeInfo};
use crate::util::{extract_tx_prevouts, full_hash, has_prevout, is_spendable, Bytes};

#[cfg(feature = "liquid")]
use crate::elements::asset;

pub struct Mempool {
    chain: Arc<ChainQuery>,
    config: Arc<Config>,
    txstore: BTreeMap<Txid, Transaction>,
    feeinfo: HashMap<Txid, TxFeeInfo>,
    history: HashMap<FullHash, Vec<TxHistoryInfo>>, // ScriptHash -> {history_entries}
    edges: HashMap<OutPoint, (Txid, u32)>,          // OutPoint -> (spending_txid, spending_vin)
    recent: BoundedVecDeque<TxOverview>,            // The N most recent txs to enter the mempool
    backlog_stats: (BacklogStats, Instant),

    // monitoring
    latency: HistogramVec, // mempool requests latency
    delta: HistogramVec,   // # of added/removed txs
    count: GaugeVec,       // current state of the mempool

    // elements only
    #[cfg(feature = "liquid")]
    pub asset_history: HashMap<AssetId, Vec<TxHistoryInfo>>,
    #[cfg(feature = "liquid")]
    pub asset_issuance: HashMap<AssetId, asset::AssetRow>,
}

// A simplified transaction view used for the list of most recent transactions
#[derive(Serialize)]
pub struct TxOverview {
    txid: Txid,
    fee: u64,
    vsize: u32,
    #[cfg(not(feature = "liquid"))]
    value: u64,
}

impl Mempool {
    pub fn new(chain: Arc<ChainQuery>, metrics: &Metrics, config: Arc<Config>) -> Self {
        Mempool {
            chain,
            txstore: BTreeMap::new(),
            feeinfo: HashMap::new(),
            history: HashMap::new(),
            edges: HashMap::new(),
            recent: BoundedVecDeque::new(config.mempool_recent_txs_size),
            backlog_stats: (
                BacklogStats::default(),
                Instant::now() - Duration::from_secs(config.mempool_backlog_stats_ttl),
            ),
            latency: metrics.histogram_vec(
                HistogramOpts::new("mempool_latency", "Mempool requests latency (in seconds)"),
                &["part"],
            ),
            delta: metrics.histogram_vec(
                HistogramOpts::new("mempool_delta", "# of transactions added/removed"),
                &["type"],
            ),
            count: metrics.gauge_vec(
                MetricOpts::new("mempool_count", "# of elements currently at the mempool"),
                &["type"],
            ),

            #[cfg(feature = "liquid")]
            asset_history: HashMap::new(),
            #[cfg(feature = "liquid")]
            asset_issuance: HashMap::new(),
            config,
        }
    }

    pub fn network(&self) -> Network {
        self.config.network_type
    }

    pub fn lookup_txn(&self, txid: &Txid) -> Option<Transaction> {
        self.txstore.get(txid).cloned()
    }

    pub fn lookup_raw_txn(&self, txid: &Txid) -> Option<Bytes> {
        self.txstore.get(txid).map(serialize)
    }

    pub fn lookup_spend(&self, outpoint: &OutPoint) -> Option<SpendingInput> {
        self.edges.get(outpoint).map(|(txid, vin)| SpendingInput {
            txid: *txid,
            vin: *vin,
            confirmed: None,
        })
    }

    pub fn has_spend(&self, outpoint: &OutPoint) -> bool {
        self.edges.contains_key(outpoint)
    }

    pub fn get_tx_fee(&self, txid: &Txid) -> Option<u64> {
        Some(self.feeinfo.get(txid)?.fee)
    }

    pub fn has_unconfirmed_parents(&self, txid: &Txid) -> bool {
        let tx = match self.txstore.get(txid) {
            Some(tx) => tx,
            None => return false,
        };
        tx.input
            .iter()
            .any(|txin| self.txstore.contains_key(&txin.previous_output.txid))
    }

    pub fn history(
        &self,
        scripthash: &[u8],
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<Transaction> {
        let _timer = self.latency.with_label_values(&["history"]).start_timer();
        self.history
            .get(scripthash)
            .map_or_else(std::vec::Vec::new, |entries| {
                self._history(entries, last_seen_txid, limit)
            })
    }

    pub fn history_txids_iter<'a>(&'a self, scripthash: &[u8]) -> impl Iterator<Item = Txid> + 'a {
        self.history
            .get(scripthash)
            .into_iter()
            .flat_map(|v| v.iter().map(|e| e.get_txid()).unique())
    }

    fn _history(
        &self,
        entries: &[TxHistoryInfo],
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<Transaction> {
        entries
            .iter()
            .map(|e| e.get_txid())
            .unique()
            // TODO seek directly to last seen tx without reading earlier rows
            .skip_while(|txid| {
                // skip until we reach the last_seen_txid
                last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid != txid)
            })
            .skip(match last_seen_txid {
                Some(_) => 1, // skip the last_seen_txid itself
                None => 0,
            })
            .take(limit)
            .map(|txid| self.txstore.get(&txid).expect("missing mempool tx"))
            .cloned()
            .collect()
    }

    pub fn history_group(
        &self,
        scripthashes: &[[u8; 32]],
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<Transaction> {
        let _timer = self
            .latency
            .with_label_values(&["history_group"])
            .start_timer();
        scripthashes
            .iter()
            .filter_map(|scripthash| self.history.get(&scripthash[..]))
            .flat_map(|entries| entries.iter())
            .map(|e| e.get_txid())
            .unique()
            // TODO seek directly to last seen tx without reading earlier rows
            .skip_while(|txid| {
                // skip until we reach the last_seen_txid
                last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid != txid)
            })
            .skip(match last_seen_txid {
                Some(_) => 1, // skip the last_seen_txid itself
                None => 0,
            })
            .take(limit)
            .map(|txid| self.txstore.get(&txid).expect("missing mempool tx"))
            .cloned()
            .collect()
    }

    pub fn history_txids_iter_group<'a>(
        &'a self,
        scripthashes: &'a [[u8; 32]],
    ) -> impl Iterator<Item = Txid> + 'a {
        scripthashes
            .iter()
            .filter_map(move |scripthash| self.history.get(&scripthash[..]))
            .flat_map(|entries| entries.iter())
            .map(|entry| entry.get_txid())
            .unique()
    }

    pub fn history_txids(&self, scripthash: &[u8], limit: usize) -> Vec<Txid> {
        let _timer = self
            .latency
            .with_label_values(&["history_txids"])
            .start_timer();
        match self.history.get(scripthash) {
            None => vec![],
            Some(entries) => entries
                .iter()
                .map(|e| e.get_txid())
                .unique()
                .take(limit)
                .collect(),
        }
    }

    pub fn utxo(&self, scripthash: &[u8]) -> Vec<Utxo> {
        let _timer = self.latency.with_label_values(&["utxo"]).start_timer();
        let entries = match self.history.get(scripthash) {
            None => return vec![],
            Some(entries) => entries,
        };

        entries
            .iter()
            .filter_map(|entry| match entry {
                TxHistoryInfo::Funding(info) => {
                    // Liquid requires some additional information from the txo that's not available in the TxHistoryInfo index.
                    #[cfg(feature = "liquid")]
                    let txo = self.lookup_txo(&entry.get_funded_outpoint())?;

                    Some(Utxo {
                        txid: deserialize(&info.txid).expect("invalid txid"),
                        vout: info.vout,
                        value: info.value,
                        confirmed: None,

                        #[cfg(feature = "liquid")]
                        asset: txo.asset,
                        #[cfg(feature = "liquid")]
                        nonce: txo.nonce,
                        #[cfg(feature = "liquid")]
                        witness: txo.witness,
                    })
                }
                TxHistoryInfo::Spending(_) => None,
                #[cfg(feature = "liquid")]
                TxHistoryInfo::Issuing(_)
                | TxHistoryInfo::Burning(_)
                | TxHistoryInfo::Pegin(_)
                | TxHistoryInfo::Pegout(_) => unreachable!(),
            })
            .filter(|utxo| !self.has_spend(&OutPoint::from(utxo)))
            .collect()
    }

    // @XXX avoid code duplication with ChainQuery::stats()?
    pub fn stats(&self, scripthash: &[u8]) -> ScriptStats {
        let _timer = self.latency.with_label_values(&["stats"]).start_timer();
        let mut stats = ScriptStats::default();
        let mut seen_txids = HashSet::new();

        let entries = match self.history.get(scripthash) {
            None => return stats,
            Some(entries) => entries,
        };

        for entry in entries {
            if seen_txids.insert(entry.get_txid()) {
                stats.tx_count += 1;
            }

            match entry {
                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Funding(info) => {
                    stats.funded_txo_count += 1;
                    stats.funded_txo_sum += info.value;
                }

                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Spending(info) => {
                    stats.spent_txo_count += 1;
                    stats.spent_txo_sum += info.value;
                }

                // Elements
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
            };
        }

        stats
    }

    // Get all txids in the mempool
    pub fn txids(&self) -> Vec<&Txid> {
        let _timer = self.latency.with_label_values(&["txids"]).start_timer();
        self.txstore.keys().collect()
    }

    // Get n txids after the given txid in the mempool
    pub fn txids_page(&self, n: usize, start: Option<Txid>) -> Vec<&Txid> {
        let _timer = self
            .latency
            .with_label_values(&["txids_page"])
            .start_timer();
        let start_bound = match start {
            Some(txid) => Excluded(txid),
            None => Unbounded,
        };

        self.txstore
            .range((start_bound, Unbounded))
            .take(n)
            .map(|(k, _v)| k)
            .collect()
    }

    // Get all txs in the mempool
    pub fn txs(&self) -> Vec<Transaction> {
        let _timer = self.latency.with_label_values(&["txs"]).start_timer();
        self.txstore.values().cloned().collect()
    }

    // Get n txs after the given txid in the mempool
    pub fn txs_page(&self, n: usize, start: Option<Txid>) -> Vec<Transaction> {
        let _timer = self.latency.with_label_values(&["txs_page"]).start_timer();
        let mut page = Vec::with_capacity(n);
        let start_bound = match start {
            Some(txid) => Excluded(txid),
            None => Unbounded,
        };

        self.txstore
            .range((start_bound, Unbounded))
            .take(n)
            .for_each(|(_, value)| {
                page.push(value.clone());
            });

        page
    }

    // Get an overview of the most recent transactions
    pub fn recent_txs_overview(&self) -> Vec<&TxOverview> {
        // We don't bother ever deleting elements from the recent list.
        // It may contain outdated txs that are no longer in the mempool,
        // until they get pushed out by newer transactions.
        self.recent.iter().collect()
    }

    pub fn backlog_stats(&self) -> &BacklogStats {
        &self.backlog_stats.0
    }

    pub fn unique_txids(&self) -> HashSet<Txid> {
        return HashSet::from_iter(self.txstore.keys().cloned());
    }

    pub fn update(mempool: &RwLock<Mempool>, daemon: &Daemon) -> Result<()> {
        // 1. Start the metrics timer and get the current mempool txids
        // [LOCK] Takes read lock for whole scope.
        let (_timer, old_txids) = {
            let mempool = mempool.read().unwrap();
            (
                mempool.latency.with_label_values(&["update"]).start_timer(),
                mempool.unique_txids(),
            )
        };

        // 2. Get all the mempool txids from the RPC.
        // [LOCK] No lock taken. Wait for RPC request. Get lists of remove/add txes.
        let all_txids = daemon
            .getmempooltxids()
            .chain_err(|| "failed to update mempool from daemon")?;
        let txids_to_remove: HashSet<&Txid> = old_txids.difference(&all_txids).collect();
        let txids_to_add: Vec<&Txid> = all_txids.difference(&old_txids).collect();

        // 3. Remove missing transactions. Even if we are unable to download new transactions from
        // the daemon, we still want to remove the transactions that are no longer in the mempool.
        // [LOCK] Write lock is released at the end of the call to remove().
        mempool.write().unwrap().remove(txids_to_remove);

        // 4. Download the new transactions from the daemon's mempool
        // [LOCK] No lock taken, waiting for RPC response.
        let txs_to_add = daemon
            .gettransactions(&txids_to_add)
            .chain_err(|| format!("failed to get {} transactions", txids_to_add.len()))?;

        // 4. Update local mempool to match daemon's state
        // [LOCK] Takes Write lock for whole scope.
        {
            let mut mempool = mempool.write().unwrap();
            // Add new transactions
            if txs_to_add.len() > mempool.add(txs_to_add) {
                debug!("Mempool update added less transactions than expected");
            }

            mempool
                .count
                .with_label_values(&["txs"])
                .set(mempool.txstore.len() as f64);

            // Update cached backlog stats (if expired)
            if mempool.backlog_stats.1.elapsed()
                > Duration::from_secs(mempool.config.mempool_backlog_stats_ttl)
            {
                let _timer = mempool
                    .latency
                    .with_label_values(&["update_backlog_stats"])
                    .start_timer();
                mempool.backlog_stats = (BacklogStats::new(&mempool.feeinfo), Instant::now());
            }

            Ok(())
        }
    }

    pub fn add_by_txid(&mut self, daemon: &Daemon, txid: &Txid) -> Result<()> {
        if !self.txstore.contains_key(txid) {
            if let Ok(tx) = daemon.getmempooltx(txid) {
                if self.add(vec![tx]) == 0 {
                    return Err(format!(
                        "Unable to add {txid} to mempool likely due to missing parents."
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Add transactions to the mempool.
    ///
    /// The return value is the number of transactions processed.
    #[must_use = "Must deal with [[input vec's length]] > [[result]]."]
    fn add(&mut self, txs: Vec<Transaction>) -> usize {
        self.delta
            .with_label_values(&["add"])
            .observe(txs.len() as f64);
        let _timer = self.latency.with_label_values(&["add"]).start_timer();
        let txlen = txs.len();
        if txlen == 0 {
            return 0;
        }
        debug!("Adding {} transactions to Mempool", txlen);

        let mut txids = Vec::with_capacity(txs.len());
        // Phase 1: add to txstore
        for tx in txs {
            let txid = tx.txid();
            // Only push if it doesn't already exist.
            // This is important now that update doesn't lock during
            // the entire function body.
            if self.txstore.insert(txid, tx).is_none() {
                txids.push(txid);
            }
        }

        // Phase 2: index history and spend edges (some txos can be missing)
        let txos = self.lookup_txos(&self.get_prevouts(&txids));

        // Count how many transactions were actually processed.
        let mut processed_count = 0;

        // Phase 3: Iterate over the transactions and do the following:
        // 1. Find all of the TxOuts of each input parent using `txos`
        // 2. If any parent wasn't found, skip parsing this transaction
        // 3. Insert TxFeeInfo into info.
        // 4. Push TxOverview into recent tx queue.
        // 5. Create the Spend and Fund TxHistory structs for inputs + outputs
        // 6. Insert all TxHistory into history.
        // 7. Insert the tx edges into edges (HashMap of (Outpoint, (Txid, vin)))
        // 8. (Liquid only) Parse assets of tx.
        for txid in txids {
            let tx = self.txstore.get(&txid).expect("missing tx from txstore");

            let prevouts = match extract_tx_prevouts(tx, &txos) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Skipping tx {txid} missing parent error: {e}");
                    continue;
                }
            };
            let txid_bytes = full_hash(&txid[..]);

            // Get feeinfo for caching and recent tx overview
            let feeinfo = TxFeeInfo::new(tx, &prevouts, self.config.network_type);

            // recent is an BoundedVecDeque that automatically evicts the oldest elements
            self.recent.push_front(TxOverview {
                txid,
                fee: feeinfo.fee,
                vsize: feeinfo.vsize,
                #[cfg(not(feature = "liquid"))]
                value: prevouts.values().map(|prevout| prevout.value).sum(),
            });

            self.feeinfo.insert(txid, feeinfo);

            // An iterator over (ScriptHash, TxHistoryInfo)
            let spending = prevouts.into_iter().map(|(input_index, prevout)| {
                let txi = tx.input.get(input_index as usize).unwrap();
                (
                    compute_script_hash(&prevout.script_pubkey),
                    TxHistoryInfo::Spending(SpendingInfo {
                        txid: txid_bytes,
                        vin: input_index,
                        prev_txid: full_hash(&txi.previous_output.txid[..]),
                        prev_vout: txi.previous_output.vout,
                        value: prevout.value,
                    }),
                )
            });

            let config = &self.config;

            // An iterator over (ScriptHash, TxHistoryInfo)
            let funding = tx
                .output
                .iter()
                .enumerate()
                .filter(|(_, txo)| is_spendable(txo) || config.index_unspendables)
                .map(|(index, txo)| {
                    (
                        compute_script_hash(&txo.script_pubkey),
                        TxHistoryInfo::Funding(FundingInfo {
                            txid: txid_bytes,
                            vout: index as u32,
                            value: txo.value,
                        }),
                    )
                });

            // Index funding/spending history entries and spend edges
            for (scripthash, entry) in funding.chain(spending) {
                self.history.entry(scripthash).or_default().push(entry);
            }
            for (i, txi) in tx.input.iter().enumerate() {
                self.edges.insert(txi.previous_output, (txid, i as u32));
            }

            // Index issued assets & native asset pegins/pegouts/burns
            #[cfg(feature = "liquid")]
            asset::index_mempool_tx_assets(
                tx,
                self.config.network_type,
                self.config.parent_network,
                &mut self.asset_history,
                &mut self.asset_issuance,
            );

            processed_count += 1;
        }

        processed_count
    }

    /// Returns None if the lookup fails (mempool transaction RBF-ed etc.)
    pub fn lookup_txo(&self, outpoint: &OutPoint) -> Option<TxOut> {
        let mut outpoints = BTreeSet::new();
        outpoints.insert(*outpoint);
        // This can possibly be None now
        self.lookup_txos(&outpoints).remove(outpoint)
    }

    /// For a given set of OutPoints, return a HashMap<OutPoint, TxOut>
    ///
    /// Not all OutPoints from mempool transactions are guaranteed to be there.
    /// Ensure you deal with the None case in your logic.
    pub fn lookup_txos(&self, outpoints: &BTreeSet<OutPoint>) -> HashMap<OutPoint, TxOut> {
        let _timer = self
            .latency
            .with_label_values(&["lookup_txos"])
            .start_timer();

        let confirmed_txos = self.chain.lookup_avail_txos(outpoints);

        let mempool_txos = outpoints
            .iter()
            .filter(|outpoint| !confirmed_txos.contains_key(outpoint))
            .flat_map(|outpoint| {
                self.txstore
                    .get(&outpoint.txid)
                    .and_then(|tx| tx.output.get(outpoint.vout as usize).cloned())
                    .map(|txout| (*outpoint, txout))
                    .or_else(|| {
                        warn!("missing outpoint {:?}", outpoint);
                        None
                    })
            })
            .collect::<HashMap<OutPoint, TxOut>>();

        let mut txos = confirmed_txos;
        txos.extend(mempool_txos);
        txos
    }

    fn get_prevouts(&self, txids: &[Txid]) -> BTreeSet<OutPoint> {
        let _timer = self
            .latency
            .with_label_values(&["get_prevouts"])
            .start_timer();

        txids
            .iter()
            .map(|txid| self.txstore.get(txid).expect("missing mempool tx"))
            .flat_map(|tx| {
                tx.input
                    .iter()
                    .filter(|txin| has_prevout(txin))
                    .map(|txin| txin.previous_output)
            })
            .collect()
    }

    fn remove(&mut self, to_remove: HashSet<&Txid>) {
        self.delta
            .with_label_values(&["remove"])
            .observe(to_remove.len() as f64);
        let _timer = self.latency.with_label_values(&["remove"]).start_timer();

        for txid in &to_remove {
            self.txstore
                .remove(*txid)
                .unwrap_or_else(|| panic!("missing mempool tx {}", txid));

            self.feeinfo.remove(*txid).or_else(|| {
                warn!("missing mempool tx feeinfo {}", txid);
                None
            });
        }

        // TODO: make it more efficient (currently it takes O(|mempool|) time)
        self.history.retain(|_scripthash, entries| {
            entries.retain(|entry| !to_remove.contains(&entry.get_txid()));
            !entries.is_empty()
        });

        #[cfg(feature = "liquid")]
        asset::remove_mempool_tx_assets(
            &to_remove,
            &mut self.asset_history,
            &mut self.asset_issuance,
        );

        self.edges
            .retain(|_outpoint, (txid, _vin)| !to_remove.contains(txid));
    }

    #[cfg(feature = "liquid")]
    pub fn asset_history(&self, asset_id: &AssetId, limit: usize) -> Vec<Transaction> {
        let _timer = self
            .latency
            .with_label_values(&["asset_history"])
            .start_timer();
        self.asset_history
            .get(asset_id)
            .map_or_else(std::vec::Vec::new, |entries| {
                self._history(entries, None, limit)
            })
    }
}

#[derive(Serialize)]
pub struct BacklogStats {
    pub count: u32,
    pub vsize: u32,     // in virtual bytes (= weight/4)
    pub total_fee: u64, // in satoshis
    pub fee_histogram: Vec<(f32, u32)>,
}

impl BacklogStats {
    fn default() -> Self {
        BacklogStats {
            count: 0,
            vsize: 0,
            total_fee: 0,
            fee_histogram: vec![(0.0, 0)],
        }
    }

    fn new(feeinfo: &HashMap<Txid, TxFeeInfo>) -> Self {
        let (count, vsize, total_fee) = feeinfo
            .values()
            .fold((0, 0, 0), |(count, vsize, fee), feeinfo| {
                (count + 1, vsize + feeinfo.vsize, fee + feeinfo.fee)
            });

        BacklogStats {
            count,
            vsize,
            total_fee,
            fee_histogram: make_fee_histogram(feeinfo.values().collect()),
        }
    }
}
