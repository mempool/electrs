#[cfg(feature = "liquid")]
use elements::address as elements_address;

use crate::chain::{script, Network, Script, TxIn, TxOut};
use script::Instruction::PushBytes;

pub struct InnerScripts {
    pub redeem_script: Option<Script>,
    pub witness_script: Option<Script>,
}

pub trait ScriptToAsm: std::fmt::Debug {
    fn to_asm(&self) -> String {
        let asm = format!("{:?}", self);
        asm[7..asm.len() - 1].to_string()
    }
}
impl ScriptToAsm for bitcoin::Script {}
#[cfg(feature = "liquid")]
impl ScriptToAsm for elements::Script {}

pub trait ScriptToAddr {
    fn to_address_str(&self, network: Network) -> Option<String>;
}
#[cfg(not(feature = "liquid"))]
impl ScriptToAddr for bitcoin::Script {
    fn to_address_str(&self, network: Network) -> Option<String> {
        bitcoin::Address::from_script(self, network.into()).map(|s| s.to_string())
    }
}
#[cfg(feature = "liquid")]
impl ScriptToAddr for elements::Script {
    fn to_address_str(&self, network: Network) -> Option<String> {
        elements_address::Address::from_script(self, None, network.address_params())
            .map(|a| a.to_string())
    }
}

// Returns the witnessScript in the case of p2wsh, or the redeemScript in the case of p2sh.
pub fn get_innerscripts(txin: &TxIn, prevout: &TxOut) -> InnerScripts {
    // Wrapped redeemScript for P2SH spends
    let redeem_script = if prevout.script_pubkey.is_p2sh() {
        if let Some(Ok(PushBytes(redeemscript))) = txin.script_sig.instructions().last() {
            Some(Script::from(redeemscript.to_vec()))
        } else {
            None
        }
    } else {
        None
    };

    // Wrapped witnessScript for P2WSH or P2SH-P2WSH spends
    let witness_script = if prevout.script_pubkey.is_v0_p2wsh()
        || prevout.script_pubkey.is_v1_p2tr()
        || redeem_script.as_ref().map_or(false, |s| s.is_v0_p2wsh())
    {
        let witness = &txin.witness;
        #[cfg(feature = "liquid")]
        let witness = &witness.script_witness;

        // rust-bitcoin returns witness items as a [u8] slice, while rust-elements returns a Vec<u8>
        #[cfg(not(feature = "liquid"))]
        let wit_to_vec = Vec::from;
        #[cfg(feature = "liquid")]
        let wit_to_vec = Clone::clone;

        let inner_script_slice = if prevout.script_pubkey.is_v1_p2tr() {
            // Witness stack is potentially very large
            // so we avoid to_vec() or iter().collect() for performance
            let w_len = witness.len();
            witness
                .last()
                // Get the position of the script spend script (if it exists)
                .map(|last_elem| {
                    // From BIP341:
                    // If there are at least two witness elements, and the first byte of
                    // the last element is 0x50, this last element is called annex a
                    // and is removed from the witness stack.
                    if w_len >= 2 && last_elem.first().filter(|&&v| v == 0x50).is_some() {
                        // account for the extra item removed from the end
                        3
                    } else {
                        // otherwise script is 2nd from last
                        2
                    }
                })
                // Convert to None if not script spend
                // Note: Option doesn't have filter_map() method
                .filter(|&script_pos_from_last| w_len >= script_pos_from_last)
                .and_then(|script_pos_from_last| {
                    // Can't use second_to_last() since it might be 3rd to last
                    witness.iter().nth(w_len - script_pos_from_last)
                })
        } else {
            witness.last()
        };

        inner_script_slice.map(wit_to_vec).map(Script::from)
    } else {
        None
    };

    InnerScripts {
        redeem_script,
        witness_script,
    }
}
