# Electrum JSON-RPC v2.0 API

This document describes the JSON-RPC v2.0 API implemented by the Electrum server.

## Protocol Information

- **Protocol Version**: 1.4
- **Maximum Headers per Request**: 2016

## Request Format

All requests must be valid JSON-RPC 2.0 messages:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "method.name",
  "params": []
}
```

Batch requests (array of request objects) are also supported.

---

## Server Methods

### `server.version`

Returns the server version and protocol version.

**Parameters**: None

**Returns**: `[server_version, protocol_version]`
- `server_version` (string): The server software version
- `protocol_version` (string): The protocol version (e.g., "1.4")

**Example Response**:
```json
["mempool-electrs v3.3.0", "1.4"]
```

---

### `server.banner`

Returns the server banner message.

**Parameters**: None

**Returns**: `string` - The configured banner message

---

### `server.donation_address`

Returns the server's donation address.

**Parameters**: None

**Returns**: `null` (not implemented)

---

### `server.peers.subscribe`

Returns a list of known peer servers.

**Parameters**: None

**Returns**: `array` - List of peer servers (empty array if `electrum-discovery` feature is disabled)

---

### `server.ping`

A simple ping to check if the server is responsive.

**Parameters**: None

**Returns**: `null`

---

### `server.features`

> **Requires**: `electrum-discovery` feature

Returns the server's feature information.

**Parameters**: None

**Returns**: `object` - Server features including hosts, genesis hash, protocol versions, etc.

---

### `server.add_peer`

> **Requires**: `electrum-discovery` feature

Request to add a peer to the server's peer list.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | features | object | Yes | Server features object describing the peer |

**Returns**: `true` on success

**Note**: This method does not work when using Unix sockets.

---

## Blockchain Methods

### `blockchain.block.header`

Returns the block header at a given height.

**Parameters**:
| Index | Name | Type | Required | Default | Description |
|-------|------|------|----------|---------|-------------|
| 0 | height | integer | Yes | - | The block height |
| 1 | cp_height | integer | No | 0 | Checkpoint height for merkle proof |

**Returns**:
- If `cp_height` is 0: `string` - The hex-encoded block header
- If `cp_height` > 0: `object` with:
  - `header` (string): Hex-encoded block header
  - `root` (string): Merkle root
  - `branch` (array): Merkle branch

---

### `blockchain.block.headers`

Returns a range of block headers.

**Parameters**:
| Index | Name | Type | Required | Default | Description |
|-------|------|------|----------|---------|-------------|
| 0 | start_height | integer | Yes | - | Starting block height |
| 1 | count | integer | Yes | - | Number of headers to return (max 2016) |
| 2 | cp_height | integer | No | 0 | Checkpoint height for merkle proof |

**Returns**: `object`
- `count` (integer): Number of headers returned
- `hex` (string): Concatenated hex-encoded headers
- `max` (integer): Maximum headers allowed per request (2016)
- `root` (string): Merkle root (only if `cp_height` > 0)
- `branch` (array): Merkle branch (only if `cp_height` > 0)

---

### `blockchain.headers.subscribe`

Subscribe to new block headers.

**Parameters**: None

**Returns**: `object` - The current best block header
- `hex` (string): Hex-encoded block header
- `height` (integer): Block height

**Notifications**: When a new block is found, the server sends a notification with the method `blockchain.headers.subscribe` and the new header as parameter.

---

### `blockchain.estimatefee`

Returns the estimated transaction fee for confirmation within a given number of blocks.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | blocks_count | integer | Yes | Target number of blocks for confirmation |

**Returns**: `number` - Estimated fee in BTC/kB, or -1 if estimation is not available

---

### `blockchain.relayfee`

Returns the minimum relay fee for low-priority transactions.

**Parameters**: None

**Returns**: `number` - Relay fee in BTC/kB

---

## Script Hash Methods

Script hashes are the SHA256 hash of the output script (scriptPubKey), encoded as a hexadecimal string with the bytes reversed.

### `blockchain.scripthash.get_balance`

> **Not available on Liquid networks**

Returns the confirmed and unconfirmed balance for a script hash.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | scripthash | string | Yes | The script hash (hex-encoded, reversed) |

**Returns**: `object`
- `confirmed` (integer): Confirmed balance in satoshis
- `unconfirmed` (integer): Unconfirmed balance change in satoshis (can be negative)

---

### `blockchain.scripthash.get_history`

Returns the transaction history for a script hash.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | scripthash | string | Yes | The script hash (hex-encoded, reversed) |

**Returns**: `array` of `object`
- `tx_hash` (string): Transaction ID
- `height` (integer): Block height, or:
  - `0` for unconfirmed transactions with all confirmed inputs
  - `-1` for unconfirmed transactions with at least one unconfirmed input
- `fee` (integer, optional): Fee in satoshis (only for mempool transactions)

---

### `blockchain.scripthash.listunspent`

Returns the list of unspent transaction outputs for a script hash.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | scripthash | string | Yes | The script hash (hex-encoded, reversed) |

**Returns**: `array` of `object`
- `height` (integer): Block height (0 for unconfirmed)
- `tx_pos` (integer): Output index in the transaction
- `tx_hash` (string): Transaction ID
- `value` (integer): Value in satoshis

**Additional fields on Liquid networks**:
- `asset` (string): Asset ID
- `nonce` (string): Nonce value

---

### `blockchain.scripthash.subscribe`

Subscribe to notifications for a script hash.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | scripthash | string | Yes | The script hash (hex-encoded, reversed) |

**Returns**: `string | null` - The status hash (hex-encoded) or `null` if no history exists

**Notifications**: When the history for the script hash changes, the server sends a notification with the method `blockchain.scripthash.subscribe` and parameters `[scripthash, status]`.

---

### `blockchain.scripthash.unsubscribe`

Unsubscribe from notifications for a script hash.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | scripthash | string | Yes | The script hash (hex-encoded, reversed) |

**Returns**: `boolean` - `true` if the subscription was removed, `false` if it didn't exist

---

## Transaction Methods

### `blockchain.transaction.broadcast`

Broadcast a raw transaction to the network.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | raw_tx | string | Yes | Hex-encoded raw transaction |

**Returns**: `string` - The transaction ID (txid) on success

---

### `blockchain.transaction.get`

Returns a raw transaction.

**Parameters**:
| Index | Name | Type | Required | Default | Description |
|-------|------|------|----------|---------|-------------|
| 0 | tx_hash | string | Yes | - | Transaction ID |
| 1 | verbose | boolean | No | false | Whether to return verbose output |

**Returns**: `string` - Hex-encoded raw transaction

**Note**: Verbose mode is currently unsupported and will return an error.

---

### `blockchain.transaction.get_merkle`

Returns the merkle proof for a transaction.

**Parameters**:
| Index | Name | Type | Required | Description |
|-------|------|------|----------|-------------|
| 0 | tx_hash | string | Yes | Transaction ID |
| 1 | height | integer | Yes | Block height where the transaction is confirmed |

**Returns**: `object`
- `block_height` (integer): The block height
- `merkle` (array): List of transaction hashes forming the merkle branch
- `pos` (integer): Position of the transaction in the block

**Errors**: Returns an error if the transaction is not found, is unconfirmed, or the provided height is incorrect.

---

### `blockchain.transaction.id_from_pos`

Returns the transaction ID at a given position in a block.

**Parameters**:
| Index | Name | Type | Required | Default | Description |
|-------|------|------|----------|---------|-------------|
| 0 | height | integer | Yes | - | Block height |
| 1 | tx_pos | integer | Yes | - | Position of the transaction in the block |
| 2 | merkle | boolean | No | false | Whether to include the merkle proof |

**Returns**:
- If `merkle` is false: `string` - The transaction ID
- If `merkle` is true: `object`
  - `tx_hash` (string): The transaction ID
  - `merkle` (array): List of transaction hashes forming the merkle branch

---

## Mempool Methods

### `mempool.get_fee_histogram`

Returns a histogram of transaction fees in the mempool.

**Parameters**: None

**Returns**: `array` - Array of `[fee_rate, cumulative_vsize]` pairs, where:
- `fee_rate` (number): Fee rate in satoshis per virtual byte
- `cumulative_vsize` (integer): Cumulative virtual size of all transactions at or above this fee rate

---

## Error Codes

The server uses standard JSON-RPC 2.0 error codes:

| Code | Name | Description |
|------|------|-------------|
| -32700 | Parse Error | Invalid JSON was received |
| -32600 | Invalid Request | The JSON is not a valid request object |
| -32601 | Method Not Found | The method does not exist |
| -32603 | Internal Error | Internal server error |

---

## Status Hash Calculation

The status hash for a script hash is calculated as follows:

1. For each transaction in the history (in order):
   - Format: `"<txid>:<height>:"`
   - Height values:
     - Positive integer for confirmed transactions
     - `0` for unconfirmed with confirmed inputs
     - `-1` for unconfirmed with unconfirmed inputs
2. Concatenate all formatted strings
3. Compute SHA256 hash of the concatenated string
4. Return as hex-encoded string

If the history is empty, the status is `null`.
