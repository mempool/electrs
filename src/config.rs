use clap::{App, Arg};
use dirs::home_dir;
use std::fs;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stderrlog;

use crate::chain::Network;
use crate::daemon::CookieGetter;
use crate::errors::*;

#[cfg(feature = "liquid")]
use bitcoin::Network as BNetwork;

pub(crate) const APP_NAME: &str = "mempool-electrs";
pub(crate) const ELECTRS_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const GIT_HASH: Option<&str> = option_env!("GIT_HASH");

lazy_static! {
    pub(crate) static ref VERSION_STRING: String = {
        if let Some(hash) = GIT_HASH {
            format!("{} {}-{}", APP_NAME, ELECTRS_VERSION, hash)
        } else {
            format!("{} {}", APP_NAME, ELECTRS_VERSION)
        }
    };
}

#[derive(Debug, Clone)]
pub struct Config {
    // See below for the documentation of each field:
    pub log: stderrlog::StdErrLog,
    pub network_type: Network,
    pub magic: Option<u32>,
    pub db_path: PathBuf,
    pub daemon_dir: PathBuf,
    pub blocks_dir: PathBuf,
    pub daemon_rpc_addr: SocketAddr,
    pub cookie: Option<String>,
    pub electrum_rpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub http_socket_file: Option<PathBuf>,
    pub rpc_socket_file: Option<PathBuf>,
    pub monitoring_addr: SocketAddr,
    pub jsonrpc_import: bool,
    pub light_mode: bool,
    pub main_loop_delay: u64,
    pub address_search: bool,
    pub index_unspendables: bool,
    pub cors: Option<String>,
    pub precache_scripts: Option<String>,
    pub precache_threads: usize,
    pub utxos_limit: usize,
    pub electrum_txs_limit: usize,
    pub electrum_banner: String,
    pub mempool_backlog_stats_ttl: u64,
    pub mempool_recent_txs_size: usize,
    pub rest_default_block_limit: usize,
    pub rest_default_chain_txs_per_page: usize,
    pub rest_default_max_mempool_txs: usize,
    pub rest_default_max_address_summary_txs: usize,
    pub rest_max_mempool_page_size: usize,
    pub rest_max_mempool_txid_page_size: usize,

    #[cfg(feature = "liquid")]
    pub parent_network: BNetwork,
    #[cfg(feature = "liquid")]
    pub asset_db_path: Option<PathBuf>,

    #[cfg(feature = "electrum-discovery")]
    pub electrum_public_hosts: Option<crate::electrum::ServerHosts>,
    #[cfg(feature = "electrum-discovery")]
    pub electrum_announce: bool,
    #[cfg(feature = "electrum-discovery")]
    pub tor_proxy: Option<std::net::SocketAddr>,
}

fn str_to_socketaddr(address: &str, what: &str) -> SocketAddr {
    address
        .to_socket_addrs()
        .unwrap_or_else(|_| panic!("unable to resolve {} address", what))
        .collect::<Vec<_>>()
        .pop()
        .unwrap()
}

impl Config {
    pub fn from_args() -> Config {
        let network_help = format!("Select network type ({})", Network::names().join(", "));

        let args = App::new("Mempool Electrum Rust Server")
            .version(crate_version!())
            .arg(
                Arg::with_name("version")
                    .long("version")
                    .help("Print out the version of this app and quit immediately."),
            )
            .arg(
                Arg::with_name("verbosity")
                    .short("v")
                    .multiple(true)
                    .help("Increase logging verbosity"),
            )
            .arg(
                Arg::with_name("timestamp")
                    .long("timestamp")
                    .help("Prepend log lines with a timestamp"),
            )
            .arg(
                Arg::with_name("db_dir")
                    .long("db-dir")
                    .help("Directory to store index database (default: ./db/)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("daemon_dir")
                    .long("daemon-dir")
                    .help("Data directory of Bitcoind (default: ~/.bitcoin/)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("blocks_dir")
                    .long("blocks-dir")
                    .help("Analogous to bitcoind's -blocksdir option, this specifies the directory containing the raw blocks files (blk*.dat) (default: ~/.bitcoin/blocks/)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("cookie")
                    .long("cookie")
                    .help("JSONRPC authentication cookie ('USER:PASSWORD', default: read from ~/.bitcoin/.cookie)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("network")
                    .long("network")
                    .help(&network_help)
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("magic")
                    .long("magic")
                    .default_value("")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("electrum_rpc_addr")
                    .long("electrum-rpc-addr")
                    .help("Electrum server JSONRPC 'addr:port' to listen on (default: '127.0.0.1:50001' for mainnet, '127.0.0.1:60001' for testnet and '127.0.0.1:60401' for regtest)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("http_addr")
                    .long("http-addr")
                    .help("HTTP server 'addr:port' to listen on (default: '127.0.0.1:3000' for mainnet, '127.0.0.1:3001' for testnet and '127.0.0.1:3002' for regtest)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("daemon_rpc_addr")
                    .long("daemon-rpc-addr")
                    .help("Bitcoin daemon JSONRPC 'addr:port' to connect (default: 127.0.0.1:8332 for mainnet, 127.0.0.1:18332 for testnet and 127.0.0.1:18443 for regtest)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("monitoring_addr")
                    .long("monitoring-addr")
                    .help("Prometheus monitoring 'addr:port' to listen on (default: 127.0.0.1:4224 for mainnet, 127.0.0.1:14224 for testnet and 127.0.0.1:24224 for regtest)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("jsonrpc_import")
                    .long("jsonrpc-import")
                    .help("Use JSONRPC instead of directly importing blk*.dat files. Useful for remote full node or low memory system"),
            )
            .arg(
                Arg::with_name("light_mode")
                    .long("lightmode")
                    .help("Enable light mode for reduced storage")
            )
            .arg(
                Arg::with_name("main_loop_delay")
                    .long("main-loop-delay")
                    .help("The number of milliseconds the main loop will wait between loops. (Can be shortened with SIGUSR1)")
                    .default_value("500")
            )
            .arg(
                Arg::with_name("address_search")
                    .long("address-search")
                    .help("Enable prefix address search")
            )
            .arg(
                Arg::with_name("index_unspendables")
                    .long("index-unspendables")
                    .help("Enable indexing of provably unspendable outputs")
            )
            .arg(
                Arg::with_name("cors")
                    .long("cors")
                    .help("Origins allowed to make cross-site requests")
                    .takes_value(true)
            )
            .arg(
                Arg::with_name("precache_scripts")
                    .long("precache-scripts")
                    .help("Path to file with list of scripts to pre-cache")
                    .takes_value(true)
            )
            .arg(
                Arg::with_name("precache_threads")
                    .long("precache-threads")
                    .help("Non-zero number of threads to use for precache threadpool. [default: 4 * CORE_COUNT]")
                    .takes_value(true)
            )
            .arg(
                Arg::with_name("utxos_limit")
                    .long("utxos-limit")
                    .help("Maximum number of utxos to process per address. Lookups for addresses with more utxos will fail. Applies to the Electrum and HTTP APIs.")
                    .default_value("500")
            )
            .arg(
                Arg::with_name("mempool_backlog_stats_ttl")
                    .long("mempool-backlog-stats-ttl")
                    .help("The number of seconds that need to pass before Mempool::update will update the latency histogram again.")
                    .default_value("10")
            )
            .arg(
                Arg::with_name("mempool_recent_txs_size")
                    .long("mempool-recent-txs-size")
                    .help("The number of transactions that mempool will keep in its recents queue. This is returned by mempool/recent endpoint.")
                    .default_value("10")
            )
            .arg(
                Arg::with_name("rest_default_block_limit")
                    .long("rest-default-block-limit")
                    .help("The default number of blocks returned from the blocks/[start_height] endpoint.")
                    .default_value("10")
            )
            .arg(
                Arg::with_name("rest_default_chain_txs_per_page")
                    .long("rest-default-chain-txs-per-page")
                    .help("The default number of on-chain transactions returned by the txs endpoints.")
                    .default_value("25")
            )
            .arg(
                Arg::with_name("rest_default_max_mempool_txs")
                    .long("rest-default-max-mempool-txs")
                    .help("The default number of mempool transactions returned by the txs endpoints.")
                    .default_value("50")
            )
            .arg(
                Arg::with_name("rest_default_max_address_summary_txs")
                    .long("rest-default-max-address-summary-txs")
                    .help("The default number of transactions returned by the address summary endpoints.")
                    .default_value("5000")
            )
            .arg(
                Arg::with_name("rest_max_mempool_page_size")
                    .long("rest-max-mempool-page-size")
                    .help("The maximum number of transactions returned by the paginated /internal/mempool/txs endpoint.")
                    .default_value("1000")
            )
            .arg(
                Arg::with_name("rest_max_mempool_txid_page_size")
                    .long("rest-max-mempool-txid-page-size")
                    .help("The maximum number of transactions returned by the paginated /mempool/txids/page endpoint.")
                    .default_value("10000")
            )
            .arg(
                Arg::with_name("electrum_txs_limit")
                    .long("electrum-txs-limit")
                    .help("Maximum number of transactions returned by Electrum history queries. Lookups with more results will fail.")
                    .default_value("500")
            ).arg(
                Arg::with_name("electrum_banner")
                    .long("electrum-banner")
                    .help("Welcome banner for the Electrum server, shown in the console to clients.")
                    .takes_value(true)
            );

        #[cfg(unix)]
        let args = args.arg(
                Arg::with_name("http_socket_file")
                    .long("http-socket-file")
                    .help("HTTP server 'unix socket file' to listen on (default disabled, enabling this disables the http server)")
                    .takes_value(true),
            );

        #[cfg(unix)]
        let args = args.arg(
                Arg::with_name("rpc_socket_file")
                    .long("rpc-socket-file")
                    .help("Electrum RPC 'unix socket file' to listen on (default disabled, enabling this ignores the electrum_rpc_addr arg)")
                    .takes_value(true),
            );

        #[cfg(feature = "liquid")]
        let args = args
            .arg(
                Arg::with_name("parent_network")
                    .long("parent-network")
                    .help("Select parent network type (mainnet, testnet, regtest)")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("asset_db_path")
                    .long("asset-db-path")
                    .help("Directory for liquid/elements asset db")
                    .takes_value(true),
            );

        #[cfg(feature = "electrum-discovery")]
        let args = args.arg(
                Arg::with_name("electrum_public_hosts")
                    .long("electrum-public-hosts")
                    .help("A dictionary of hosts where the Electrum server can be reached at. Required to enable server discovery. See https://electrumx.readthedocs.io/en/latest/protocol-methods.html#server-features")
                    .takes_value(true)
            ).arg(
                Arg::with_name("electrum_announce")
                    .long("electrum-announce")
                    .help("Announce the Electrum server to other servers")
            ).arg(
            Arg::with_name("tor_proxy")
                .long("tor-proxy")
                .help("ip:addr of socks proxy for accessing onion hosts")
                .takes_value(true),
        );

        let m = args.get_matches();

        if m.is_present("version") {
            eprintln!("{}", *VERSION_STRING);
            std::process::exit(0);
        }

        let network_name = m.value_of("network").unwrap_or("mainnet");
        let network_type = Network::from(network_name);
        let magic: Option<u32> = m
            .value_of("magic")
            .filter(|s| !s.is_empty())
            .map(|s| u32::from_str_radix(s, 16).expect("invalid network magic"));
        let db_dir = Path::new(m.value_of("db_dir").unwrap_or("./db"));
        let db_path = db_dir.join(network_name);

        #[cfg(feature = "liquid")]
        let parent_network = m
            .value_of("parent_network")
            .map(|s| s.parse().expect("invalid parent network"))
            .unwrap_or_else(|| match network_type {
                Network::Liquid => BNetwork::Bitcoin,
                // XXX liquid testnet/regtest don't have a parent chain
                Network::LiquidTestnet | Network::LiquidRegtest => BNetwork::Regtest,
            });

        #[cfg(feature = "liquid")]
        let asset_db_path = m.value_of("asset_db_path").map(PathBuf::from);

        let default_daemon_port = match network_type {
            #[cfg(not(feature = "liquid"))]
            Network::Bitcoin => 8332,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet => 18332,
            #[cfg(not(feature = "liquid"))]
            Network::Regtest => 18443,
            #[cfg(not(feature = "liquid"))]
            Network::Signet => 38332,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet4 => 48332,

            #[cfg(feature = "liquid")]
            Network::Liquid => 7041,
            #[cfg(feature = "liquid")]
            Network::LiquidTestnet | Network::LiquidRegtest => 7040,
        };
        let default_electrum_port = match network_type {
            #[cfg(not(feature = "liquid"))]
            Network::Bitcoin => 50001,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet => 60001,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet4 => 40001,
            #[cfg(not(feature = "liquid"))]
            Network::Regtest => 60401,
            #[cfg(not(feature = "liquid"))]
            Network::Signet => 60601,

            #[cfg(feature = "liquid")]
            Network::Liquid => 51000,
            #[cfg(feature = "liquid")]
            Network::LiquidTestnet => 51301,
            #[cfg(feature = "liquid")]
            Network::LiquidRegtest => 51401,
        };
        let default_http_port = match network_type {
            #[cfg(not(feature = "liquid"))]
            Network::Bitcoin => 3000,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet => 3001,
            #[cfg(not(feature = "liquid"))]
            Network::Regtest => 3002,
            #[cfg(not(feature = "liquid"))]
            Network::Signet => 3003,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet4 => 3004,

            #[cfg(feature = "liquid")]
            Network::Liquid => 3000,
            #[cfg(feature = "liquid")]
            Network::LiquidTestnet => 3001,
            #[cfg(feature = "liquid")]
            Network::LiquidRegtest => 3002,
        };
        let default_monitoring_port = match network_type {
            #[cfg(not(feature = "liquid"))]
            Network::Bitcoin => 4224,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet => 14224,
            #[cfg(not(feature = "liquid"))]
            Network::Regtest => 24224,
            #[cfg(not(feature = "liquid"))]
            Network::Testnet4 => 44224,
            #[cfg(not(feature = "liquid"))]
            Network::Signet => 54224,

            #[cfg(feature = "liquid")]
            Network::Liquid => 34224,
            #[cfg(feature = "liquid")]
            Network::LiquidTestnet => 44324,
            #[cfg(feature = "liquid")]
            Network::LiquidRegtest => 44224,
        };

        let daemon_rpc_addr: SocketAddr = str_to_socketaddr(
            m.value_of("daemon_rpc_addr")
                .unwrap_or(&format!("127.0.0.1:{}", default_daemon_port)),
            "Bitcoin RPC",
        );
        let electrum_rpc_addr: SocketAddr = str_to_socketaddr(
            m.value_of("electrum_rpc_addr")
                .unwrap_or(&format!("127.0.0.1:{}", default_electrum_port)),
            "Electrum RPC",
        );
        let http_addr: SocketAddr = str_to_socketaddr(
            m.value_of("http_addr")
                .unwrap_or(&format!("127.0.0.1:{}", default_http_port)),
            "HTTP Server",
        );

        let http_socket_file: Option<PathBuf> = m.value_of("http_socket_file").map(PathBuf::from);
        let rpc_socket_file: Option<PathBuf> = m.value_of("rpc_socket_file").map(PathBuf::from);
        let monitoring_addr: SocketAddr = str_to_socketaddr(
            m.value_of("monitoring_addr")
                .unwrap_or(&format!("127.0.0.1:{}", default_monitoring_port)),
            "Prometheus monitoring",
        );

        let mut daemon_dir = m
            .value_of("daemon_dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let mut default_dir = home_dir().expect("no homedir");
                default_dir.push(".bitcoin");
                default_dir
            });
        match network_type {
            #[cfg(not(feature = "liquid"))]
            Network::Bitcoin => (),
            #[cfg(not(feature = "liquid"))]
            Network::Testnet => daemon_dir.push("testnet3"),
            #[cfg(not(feature = "liquid"))]
            Network::Testnet4 => daemon_dir.push("testnet4"),
            #[cfg(not(feature = "liquid"))]
            Network::Regtest => daemon_dir.push("regtest"),
            #[cfg(not(feature = "liquid"))]
            Network::Signet => daemon_dir.push("signet"),

            #[cfg(feature = "liquid")]
            Network::Liquid => daemon_dir.push("liquidv1"),
            #[cfg(feature = "liquid")]
            Network::LiquidTestnet => daemon_dir.push("liquidtestnet"),
            #[cfg(feature = "liquid")]
            Network::LiquidRegtest => daemon_dir.push("liquidregtest"),
        }
        let blocks_dir = m
            .value_of("blocks_dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| daemon_dir.join("blocks"));
        let cookie = m.value_of("cookie").map(|s| s.to_owned());

        let electrum_banner = m
            .value_of("electrum_banner")
            .map_or_else(|| format!("Welcome to {}", *VERSION_STRING), |s| s.into());

        #[cfg(feature = "electrum-discovery")]
        let electrum_public_hosts = m
            .value_of("electrum_public_hosts")
            .map(|s| serde_json::from_str(s).expect("invalid --electrum-public-hosts"));

        let mut log = stderrlog::new();
        log.verbosity(m.occurrences_of("verbosity") as usize);
        log.timestamp(if m.is_present("timestamp") {
            stderrlog::Timestamp::Millisecond
        } else {
            stderrlog::Timestamp::Off
        });
        log.init().expect("logging initialization failed");
        let config = Config {
            log,
            network_type,
            magic,
            db_path,
            daemon_dir,
            blocks_dir,
            daemon_rpc_addr,
            cookie,
            utxos_limit: value_t_or_exit!(m, "utxos_limit", usize),
            electrum_rpc_addr,
            electrum_txs_limit: value_t_or_exit!(m, "electrum_txs_limit", usize),
            electrum_banner,
            http_addr,
            http_socket_file,
            rpc_socket_file,
            monitoring_addr,
            mempool_backlog_stats_ttl: value_t_or_exit!(m, "mempool_backlog_stats_ttl", u64),
            mempool_recent_txs_size: value_t_or_exit!(m, "mempool_recent_txs_size", usize),
            rest_default_block_limit: value_t_or_exit!(m, "rest_default_block_limit", usize),
            rest_default_chain_txs_per_page: value_t_or_exit!(
                m,
                "rest_default_chain_txs_per_page",
                usize
            ),
            rest_default_max_mempool_txs: value_t_or_exit!(
                m,
                "rest_default_max_mempool_txs",
                usize
            ),
            rest_default_max_address_summary_txs: value_t_or_exit!(
                m,
                "rest_default_max_address_summary_txs",
                usize
            ),
            rest_max_mempool_page_size: value_t_or_exit!(m, "rest_max_mempool_page_size", usize),
            rest_max_mempool_txid_page_size: value_t_or_exit!(
                m,
                "rest_max_mempool_txid_page_size",
                usize
            ),
            jsonrpc_import: m.is_present("jsonrpc_import"),
            light_mode: m.is_present("light_mode"),
            main_loop_delay: value_t_or_exit!(m, "main_loop_delay", u64),
            address_search: m.is_present("address_search"),
            index_unspendables: m.is_present("index_unspendables"),
            cors: m.value_of("cors").map(|s| s.to_string()),
            precache_scripts: m.value_of("precache_scripts").map(|s| s.to_string()),
            precache_threads: m.value_of("precache_threads").map_or_else(
                || {
                    std::thread::available_parallelism()
                        .expect("Can't get core count")
                        .get()
                        * 4
                },
                |s| match s.parse::<usize>() {
                    Ok(v) if v > 0 => v,
                    _ => clap::Error::value_validation_auto(format!(
                        "The argument '{}' isn't a valid value",
                        s
                    ))
                    .exit(),
                },
            ),

            #[cfg(feature = "liquid")]
            parent_network,
            #[cfg(feature = "liquid")]
            asset_db_path,

            #[cfg(feature = "electrum-discovery")]
            electrum_public_hosts,
            #[cfg(feature = "electrum-discovery")]
            electrum_announce: m.is_present("electrum_announce"),
            #[cfg(feature = "electrum-discovery")]
            tor_proxy: m.value_of("tor_proxy").map(|s| s.parse().unwrap()),
        };
        eprintln!("{:?}", config);
        config
    }

    pub fn cookie_getter(&self) -> Arc<dyn CookieGetter> {
        if let Some(ref value) = self.cookie {
            Arc::new(StaticCookie {
                value: value.as_bytes().to_vec(),
            })
        } else {
            Arc::new(CookieFile {
                daemon_dir: self.daemon_dir.clone(),
            })
        }
    }
}

struct StaticCookie {
    value: Vec<u8>,
}

impl CookieGetter for StaticCookie {
    fn get(&self) -> Result<Vec<u8>> {
        Ok(self.value.clone())
    }
}

struct CookieFile {
    daemon_dir: PathBuf,
}

impl CookieGetter for CookieFile {
    fn get(&self) -> Result<Vec<u8>> {
        let path = self.daemon_dir.join(".cookie");
        let contents = fs::read(&path).chain_err(|| {
            ErrorKind::Connection(format!("failed to read cookie from {:?}", path))
        })?;
        Ok(contents)
    }
}
