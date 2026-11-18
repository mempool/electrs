use std::str::FromStr;

#[cfg(not(feature = "liquid"))] // use regular Bitcoin data structures
pub use bitcoin::{
    address,
    block::Header as BlockHeader,
    blockdata::{opcodes, script},
    consensus::deserialize,
    hashes, Block, BlockHash, OutPoint, ScriptBuf as Script, Transaction, TxIn, TxOut, Txid,
    Witness,
};

#[cfg(feature = "liquid")]
pub use {
    crate::elements::asset,
    elements::{
        address, bitcoin::bech32::Hrp, confidential, encode::deserialize, hashes, opcodes, script,
        Address, AssetId, Block, BlockHash, BlockHeader, OutPoint, Script, Transaction, TxIn,
        TxInWitness as Witness, TxOut, Txid,
    },
};

use bitcoin::blockdata::constants::genesis_block;
pub use bitcoin::Network as BNetwork;

// Extension trait for getting txid in a cross-compatible way
pub trait TxidCompat {
    fn get_txid(&self) -> Txid;
}

#[cfg(not(feature = "liquid"))]
impl TxidCompat for Transaction {
    fn get_txid(&self) -> Txid {
        self.compute_txid()
    }
}

#[cfg(feature = "liquid")]
impl TxidCompat for Transaction {
    fn get_txid(&self) -> Txid {
        self.txid()
    }
}

// Extension trait for getting block size in a cross-compatible way
pub trait BlockSizeCompat {
    fn get_block_size(&self) -> usize;
}

#[cfg(not(feature = "liquid"))]
impl BlockSizeCompat for Block {
    fn get_block_size(&self) -> usize {
        self.total_size()
    }
}

#[cfg(feature = "liquid")]
impl BlockSizeCompat for Block {
    fn get_block_size(&self) -> usize {
        self.size()
    }
}

#[cfg(not(feature = "liquid"))]
pub type Value = u64;
#[cfg(feature = "liquid")]
pub use confidential::Value;

#[derive(Debug, Copy, Clone, PartialEq, Hash, Serialize, Ord, PartialOrd, Eq)]
pub enum Network {
    #[cfg(not(feature = "liquid"))]
    Bitcoin,
    #[cfg(not(feature = "liquid"))]
    Testnet,
    #[cfg(not(feature = "liquid"))]
    Testnet4,
    #[cfg(not(feature = "liquid"))]
    Testnet5,
    #[cfg(not(feature = "liquid"))]
    Regtest,
    #[cfg(not(feature = "liquid"))]
    Signet,

    #[cfg(feature = "liquid")]
    Liquid,
    #[cfg(feature = "liquid")]
    LiquidTestnet,
    #[cfg(feature = "liquid")]
    LiquidRegtest,
}

#[cfg(feature = "liquid")]
pub const LIQUID_TESTNET_PARAMS: address::AddressParams = address::AddressParams {
    p2pkh_prefix: 36,
    p2sh_prefix: 19,
    blinded_prefix: 23,
    bech_hrp: Hrp::parse_unchecked("tex"),
    blech_hrp: Hrp::parse_unchecked("tlq"),
};

/// Magic for testnet4, 0x1c163f28 (from BIP94) with flipped endianness.
#[cfg(not(feature = "liquid"))]
const TESTNET4_MAGIC: u32 = 0x283f161c;

/// Magic for testnet5, 0x46495645 (from BIP95) with flipped endianness.
#[cfg(not(feature = "liquid"))]
const TESTNET5_MAGIC: u32 = 0x45564946;

impl Network {
    #[cfg(not(feature = "liquid"))]
    pub fn magic(self) -> u32 {
        match self {
            Self::Testnet4 => TESTNET4_MAGIC,
            Self::Testnet5 => TESTNET5_MAGIC,
            _ => {
                let magic = BNetwork::from(self).magic();
                u32::from_le_bytes(magic.to_bytes())
            }
        }
    }

    #[cfg(feature = "liquid")]
    pub fn magic(self) -> u32 {
        match self {
            Network::Liquid | Network::LiquidRegtest => 0xDAB5_BFFA,
            Network::LiquidTestnet => 0x62DD_0E41,
        }
    }

    pub fn is_regtest(self) -> bool {
        match self {
            #[cfg(not(feature = "liquid"))]
            Network::Regtest => true,
            #[cfg(feature = "liquid")]
            Network::LiquidRegtest => true,
            _ => false,
        }
    }

    #[cfg(feature = "liquid")]
    pub fn address_params(self) -> &'static address::AddressParams {
        // Liquid regtest uses elements's address params
        match self {
            Network::Liquid => &address::AddressParams::LIQUID,
            Network::LiquidRegtest => &address::AddressParams::ELEMENTS,
            Network::LiquidTestnet => &LIQUID_TESTNET_PARAMS,
        }
    }

    #[cfg(feature = "liquid")]
    pub fn native_asset(self) -> &'static AssetId {
        match self {
            Network::Liquid => &asset::NATIVE_ASSET_ID,
            Network::LiquidTestnet => &asset::NATIVE_ASSET_ID_TESTNET,
            Network::LiquidRegtest => &asset::NATIVE_ASSET_ID_REGTEST,
        }
    }

    #[cfg(feature = "liquid")]
    pub fn pegged_asset(self) -> Option<&'static AssetId> {
        match self {
            Network::Liquid => Some(&*asset::NATIVE_ASSET_ID),
            Network::LiquidTestnet | Network::LiquidRegtest => None,
        }
    }

    pub fn names() -> Vec<String> {
        #[cfg(not(feature = "liquid"))]
        return vec![
            "mainnet".to_string(),
            "testnet".to_string(),
            "testnet4".to_string(),
            "testnet5".to_string(),
            "regtest".to_string(),
            "signet".to_string(),
        ];

        #[cfg(feature = "liquid")]
        return vec![
            "liquid".to_string(),
            "liquidtestnet".to_string(),
            "liquidregtest".to_string(),
        ];
    }
}

pub fn genesis_hash(network: Network) -> BlockHash {
    #[cfg(not(feature = "liquid"))]
    return bitcoin_genesis_hash(network);
    #[cfg(feature = "liquid")]
    return liquid_genesis_hash(network);
}

pub fn bitcoin_genesis_hash(network: Network) -> bitcoin::BlockHash {
    lazy_static! {
        static ref BITCOIN_GENESIS: bitcoin::BlockHash =
            genesis_block(BNetwork::Bitcoin).block_hash();
        static ref TESTNET_GENESIS: bitcoin::BlockHash =
            genesis_block(BNetwork::Testnet).block_hash();
        static ref TESTNET4_GENESIS: bitcoin::BlockHash = bitcoin::BlockHash::from_str(
            "00000000da84f2bafbbc53dee25a72ae507ff4914b867c565be350b0da8bf043"
        )
        .unwrap();
        static ref TESTNET5_GENESIS: bitcoin::BlockHash = bitcoin::BlockHash::from_str(
            "00000000000008c1637e7d3252dc1c333d4546fd6fff6a474b200d674a91d497"
        )
        .unwrap();
        static ref REGTEST_GENESIS: bitcoin::BlockHash =
            genesis_block(BNetwork::Regtest).block_hash();
        static ref SIGNET_GENESIS: bitcoin::BlockHash =
            genesis_block(BNetwork::Signet).block_hash();
    }
    #[cfg(not(feature = "liquid"))]
    match network {
        Network::Bitcoin => *BITCOIN_GENESIS,
        Network::Testnet => *TESTNET_GENESIS,
        Network::Testnet4 => *TESTNET4_GENESIS,
        Network::Testnet5 => *TESTNET5_GENESIS,
        Network::Regtest => *REGTEST_GENESIS,
        Network::Signet => *SIGNET_GENESIS,
    }
    #[cfg(feature = "liquid")]
    match network {
        Network::Liquid => *BITCOIN_GENESIS,
        Network::LiquidTestnet => *TESTNET_GENESIS,
        Network::LiquidRegtest => *REGTEST_GENESIS,
    }
}

#[cfg(feature = "liquid")]
pub fn liquid_genesis_hash(network: Network) -> elements::BlockHash {
    lazy_static! {
        static ref LIQUID_GENESIS: BlockHash =
            "1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003"
                .parse()
                .unwrap();
        static ref ZERO_HASH: BlockHash =
            "0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();
    }

    match network {
        Network::Liquid => *LIQUID_GENESIS,
        // The genesis block for liquid regtest chains varies based on the chain configuration.
        // This instead uses an all zeroed-out hash, which doesn't matter in practice because its
        // only used for Electrum server discovery, which isn't active on regtest.
        _ => *ZERO_HASH,
    }
}

impl From<&str> for Network {
    fn from(network_name: &str) -> Self {
        match network_name {
            #[cfg(not(feature = "liquid"))]
            "mainnet" => Network::Bitcoin,
            #[cfg(not(feature = "liquid"))]
            "testnet" => Network::Testnet,
            #[cfg(not(feature = "liquid"))]
            "testnet4" => Network::Testnet4,
            #[cfg(not(feature = "liquid"))]
            "testnet5" => Network::Testnet5,
            #[cfg(not(feature = "liquid"))]
            "regtest" => Network::Regtest,
            #[cfg(not(feature = "liquid"))]
            "signet" => Network::Signet,

            #[cfg(feature = "liquid")]
            "liquid" => Network::Liquid,
            #[cfg(feature = "liquid")]
            "liquidtestnet" => Network::LiquidTestnet,
            #[cfg(feature = "liquid")]
            "liquidregtest" => Network::LiquidRegtest,

            _ => panic!("unsupported Bitcoin network: {:?}", network_name),
        }
    }
}

#[cfg(not(feature = "liquid"))]
impl From<Network> for BNetwork {
    fn from(network: Network) -> Self {
        match network {
            Network::Bitcoin => BNetwork::Bitcoin,
            Network::Testnet => BNetwork::Testnet,
            Network::Testnet4 => BNetwork::Testnet4,
            // rust-bitcoin has no testnet5 network. This conversion is only used for
            // address encoding, which testnet5 shares with testnet4 (BIP95).
            Network::Testnet5 => BNetwork::Testnet4,
            Network::Regtest => BNetwork::Regtest,
            Network::Signet => BNetwork::Signet,
        }
    }
}

#[cfg(not(feature = "liquid"))]
impl From<BNetwork> for Network {
    fn from(network: BNetwork) -> Self {
        match network {
            BNetwork::Bitcoin => Network::Bitcoin,
            BNetwork::Testnet => Network::Testnet,
            BNetwork::Testnet4 => Network::Testnet4,
            BNetwork::Regtest => Network::Regtest,
            BNetwork::Signet => Network::Signet,
        }
    }
}
