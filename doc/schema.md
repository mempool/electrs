# Index Schema

The index is stored at a single RocksDB database using the following schema:

## Transaction outputs' index

Allows efficiently finding all funding transactions for a specific address:

|  Code  | Script Hash Prefix   | Funding TxID Prefix   |   |
| ------ | -------------------- | --------------------- | - |
| `b'O'` | `SHA256(script)[:8]` | `txid[:8]`            |   |

## Transaction inputs' index

Allows efficiently finding spending transaction of a specific output:

|  Code  | Funding TxID Prefix  | Funding Output Index  | Spending TxID Prefix  |   |
| ------ | -------------------- | --------------------- | --------------------- | - |
| `b'I'` | `txid[:8]`           | `uint16`              | `txid[:8]`            |   |


## Full Transaction IDs

In order to save storage space, we store the full transaction IDs once, and use their 8-byte prefixes for the indexes above.

|  Code  | Transaction ID    |   | Confirmed height   |
| ------ | ----------------- | - | ------------------ |
| `b'T'` | `txid` (32 bytes) |   | `uint32`           |

Note that this mapping allows us to use `getrawtransaction` RPC to retrieve actual transaction data from without `-txindex` enabled
(by explicitly specifying the [blockhash](https://github.com/bitcoin/bitcoin/commit/497d0e014cc79d46531d570e74e4aeae72db602d)).


# New Schema (for efficient history paging)

The indexing will be done in the two phases (each can be done concurrently within itself):

## 1st phase

Each transaction will be indexed by its id, resulting in a new row (`T` is for transaction):

 * `"T{txnid}" → "{confirmed-height}{confirmed-blockhash}{serialized-transaction}"`

## 2nd phase

Each funding output should result in a new row (`H` is for history, `F` is for funding):

 * `"H{funding-scripthash}{funding-height}F{funding-txid:index}" → ""`

Each spending input (except the coinbase) should result in two new rows (`S` is for spending):

 * `"H{funding-scripthash}{spending-height}S{spending-txid:index}" → "{funding-txid:index}"`
 * `"S{funding-txid:index}{spending-txid:index}" → ""`

NOTE: in order to construct the rows for spending inputs, we rely on having the transactions being processed at phase #1, so they can be looked up efficiently (using parallel point lookups).

After the indexing is completed, both funding and spending are indexed as independent rows under `"H{scripthash}`, so that they can be queried in-order in one go.
