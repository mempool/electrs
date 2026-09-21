pub mod db;
mod fetch;
mod mempool;
pub mod precache;
mod query;
pub mod schema;

use std::sync::LazyLock;

pub(crate) static THREAD_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(0) // 0 = use number of logical CPUs
        .thread_name(|i| format!("electrs-worker-{}", i))
        .build()
        .expect("failed to create global rayon thread pool")
});

pub use self::db::{DBRow, DB};
pub use self::fetch::{BlockEntry, FetchFrom};
pub use self::mempool::Mempool;
pub use self::query::Query;
pub use self::schema::{
    compute_script_hash, parse_hash, ChainQuery, FundingInfo, Indexer, ScanOutcome, ScriptStats,
    SpendingInfo, SpendingInput, Store, TxHistoryInfo, TxHistoryKey, TxHistoryRow, Utxo,
};
