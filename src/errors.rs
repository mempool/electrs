error_chain! {
    types {
        Error, ErrorKind, ResultExt, Result;
    }

    errors {
        Connection(msg: String) {
            description("Connection error")
            display("Connection error: {}", msg)
        }

        Interrupt(sig: i32) {
            description("Interruption by external signal")
            display("Iterrupted by signal {}", sig)
        }

        TooManyUtxos(limit: usize) {
            description("Too many unspent transaction outputs. Contact support to raise limits.")
            display("Too many unspent transaction outputs (>{}). Contact support to raise limits.", limit)
        }

        TooManyTxs(limit: usize) {
            description("Too many history transactions. Contact support to raise limits.")
            display("Too many history transactions (>{}). Contact support to raise limits.", limit)
        }

        #[cfg(feature = "electrum-discovery")]
        ElectrumClient(e: electrum_client::Error) {
            description("Electrum client error")
            display("Electrum client error: {:?}", e)
        }

    }
}

#[cfg(feature = "electrum-discovery")]
impl From<electrum_client::Error> for Error {
    fn from(e: electrum_client::Error) -> Self {
        Error::from(ErrorKind::ElectrumClient(e))
    }
}
