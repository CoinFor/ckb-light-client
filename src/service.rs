use ckb_chain_spec::consensus::Consensus;
use ckb_jsonrpc_types::{
    BlockNumber, Capacity, CellOutput, HeaderView, JsonBytes, NodeAddress, OutPoint,
    RemoteNodeProtocol, Script, Transaction, TransactionView, Uint32, Uint64,
};
use ckb_network::{extract_peer_id, NetworkController, SupportProtocols};
use ckb_traits::HeaderProvider;
use ckb_types::{
    core::{self, Cycle},
    packed,
    prelude::*,
    H256,
};
use jsonrpc_core::{Error, IoHandler, Result};
use jsonrpc_derive::rpc;
use jsonrpc_http_server::{Server, ServerBuilder};
use jsonrpc_server_utils::cors::AccessControlAllowOrigin;
use jsonrpc_server_utils::hosts::DomainsValidation;
use rocksdb::{
    ops::{Get, Iterate},
    Direction, IteratorMode,
};
use serde::{Deserialize, Serialize};
use std::{
    net::ToSocketAddrs,
    sync::{Arc, RwLock},
};

use crate::{
    protocols::{Peers, PendingTxs},
    storage::{self, extract_raw_data, Key, KeyPrefix, Storage},
    verify::verify_tx,
};

#[rpc(server)]
pub trait BlockFilterRpc {
    /// curl http://localhost:9000/ -X POST -H "Content-Type: application/json" -d '{"jsonrpc": "2.0", "method":"set_scripts", "params": [{"script": {"code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8", "hash_type": "type", "args": "0x50878ce52a68feb47237c29574d82288f58b5d21"}, "block_number": "0x59F74D"}], "id": 1}'
    #[rpc(name = "set_scripts")]
    fn set_scripts(&self, scripts: Vec<ScriptStatus>) -> Result<()>;

    #[rpc(name = "get_scripts")]
    fn get_scripts(&self) -> Result<Vec<ScriptStatus>>;

    #[rpc(name = "get_cells")]
    fn get_cells(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>>;

    #[rpc(name = "get_transactions")]
    fn get_transactions(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Tx>>;

    #[rpc(name = "get_cells_capacity")]
    fn get_cells_capacity(&self, search_key: SearchKey) -> Result<Capacity>;
}

#[rpc(server)]
pub trait TransactionRpc {
    #[rpc(name = "send_transaction")]
    fn send_transaction(&self, tx: Transaction) -> Result<H256>;
}

#[rpc(server)]
pub trait ChainRpc {
    #[rpc(name = "get_tip_header")]
    fn get_tip_header(&self) -> Result<HeaderView>;

    #[rpc(name = "get_header")]
    fn get_header(&self, block_hash: H256) -> Result<Option<HeaderView>>;

    #[rpc(name = "get_transaction")]
    fn get_transaction(&self, tx_hash: H256) -> Result<Option<TransactionWithHeader>>;
}

#[rpc(server)]
pub trait NetRpc {
    #[rpc(name = "get_peers")]
    fn get_peers(&self) -> Result<Vec<RemoteNode>>;
}

#[derive(Deserialize, Serialize)]
pub struct ScriptStatus {
    script: Script,
    block_number: BlockNumber,
}

#[derive(Deserialize, Serialize)]
pub struct RemoteNode {
    /// The remote node version.
    pub version: String,
    /// The remote node ID which is derived from its P2P private key.
    pub node_id: String,
    /// The remote node addresses.
    pub addresses: Vec<NodeAddress>,
    /// Elapsed time in milliseconds since the remote node is connected.
    pub connected_duration: Uint64,
    /// Null means chain sync has not started with this remote node yet.
    pub sync_state: Option<PeerSyncState>,
    /// Active protocols.
    ///
    /// CKB uses Tentacle multiplexed network framework. Multiple protocols are running
    /// simultaneously in the connection.
    pub protocols: Vec<RemoteNodeProtocol>,
    // TODO: maybe add this field later.
    // /// Elapsed time in milliseconds since receiving the ping response from this remote node.
    // ///
    // /// Null means no ping responses have been received yet.
    // pub last_ping_duration: Option<Uint64>,
}
#[derive(Deserialize, Serialize)]
pub struct PeerSyncState {
    /// Requested best known header of remote peer.
    ///
    /// This is the best known header yet to be proved.
    pub requested_best_known_header: Option<HeaderView>,
    /// Proved best known header of remote peer.
    pub proved_best_known_header: Option<HeaderView>,
}

#[derive(Deserialize)]
pub struct SearchKey {
    script: Script,
    script_type: ScriptType,
    filter: Option<SearchKeyFilter>,
    group_by_transaction: Option<bool>,
}

impl Default for SearchKey {
    fn default() -> Self {
        Self {
            script: Script::default(),
            script_type: ScriptType::Lock,
            filter: None,
            group_by_transaction: None,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct SearchKeyFilter {
    script: Option<Script>,
    output_data_len_range: Option<[Uint64; 2]>,
    output_capacity_range: Option<[Uint64; 2]>,
    block_range: Option<[BlockNumber; 2]>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptType {
    Lock,
    Type,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Desc,
    Asc,
}

#[derive(Serialize)]
pub struct Cell {
    output: CellOutput,
    output_data: JsonBytes,
    out_point: OutPoint,
    block_number: BlockNumber,
    tx_index: Uint32,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Tx {
    Ungrouped(TxWithCell),
    Grouped(TxWithCells),
}

impl Tx {
    pub fn tx_hash(&self) -> H256 {
        match self {
            Tx::Ungrouped(tx) => tx.transaction.hash.clone(),
            Tx::Grouped(tx) => tx.transaction.hash.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct TxWithCell {
    transaction: TransactionView,
    block_number: BlockNumber,
    tx_index: Uint32,
    io_index: Uint32,
    io_type: CellType,
}

#[derive(Serialize)]
pub struct TxWithCells {
    transaction: TransactionView,
    block_number: BlockNumber,
    tx_index: Uint32,
    cells: Vec<(CellType, Uint32)>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellType {
    Input,
    Output,
}

#[derive(Serialize)]
pub struct Pagination<T> {
    objects: Vec<T>,
    last_cursor: JsonBytes,
}

#[derive(Serialize)]
pub struct TransactionWithHeader {
    transaction: TransactionView,
    header: HeaderView,
}

pub struct BlockFilterRpcImpl {
    storage: Storage,
}

pub struct TransactionRpcImpl {
    network_controller: NetworkController,
    pending_txs: Arc<RwLock<PendingTxs>>,
    storage: Storage,
    consensus: Consensus,
}

pub struct ChainRpcImpl {
    storage: Storage,
}

pub struct NetRpcImpl {
    network_controller: NetworkController,
    peers: Arc<Peers>,
}

#[allow(clippy::mutable_key_type)]
impl BlockFilterRpc for BlockFilterRpcImpl {
    fn set_scripts(&self, scripts: Vec<ScriptStatus>) -> Result<()> {
        let scripts = scripts
            .into_iter()
            .map(|script_status| {
                (
                    script_status.script.into(),
                    script_status.block_number.into(),
                )
            })
            .collect();

        self.storage.update_filter_scripts(scripts);
        Ok(())
    }

    fn get_scripts(&self) -> Result<Vec<ScriptStatus>> {
        let scripts = self.storage.get_filter_scripts();
        Ok(scripts
            .into_iter()
            .map(|(script, block_number)| ScriptStatus {
                script: script.into(),
                block_number: block_number.into(),
            })
            .collect())
    }

    fn get_cells(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after_cursor: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>> {
        let (prefix, from_key, direction, skip) = build_query_options(
            &search_key,
            KeyPrefix::CellLockScript,
            KeyPrefix::CellTypeScript,
            order,
            after_cursor,
        )?;
        let filter_script_type = match search_key.script_type {
            ScriptType::Lock => ScriptType::Type,
            ScriptType::Type => ScriptType::Lock,
        };
        let (
            filter_prefix,
            filter_output_data_len_range,
            filter_output_capacity_range,
            filter_block_range,
        ) = build_filter_options(search_key)?;
        let mode = IteratorMode::From(from_key.as_ref(), direction);
        let snapshot = self.storage.db.snapshot();
        let iter = snapshot.iterator(mode).skip(skip);

        let mut last_key = Vec::new();
        let cells = iter
            .take_while(|(key, _value)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                let output_index = u32::from_be_bytes(
                    key[key.len() - 4..]
                        .try_into()
                        .expect("stored output_index"),
                );
                let tx_index = u32::from_be_bytes(
                    key[key.len() - 8..key.len() - 4]
                        .try_into()
                        .expect("stored tx_index"),
                );
                let block_number = u64::from_be_bytes(
                    key[key.len() - 16..key.len() - 8]
                        .try_into()
                        .expect("stored block_number"),
                );

                let tx = packed::Transaction::from_slice(
                    &snapshot
                        .get(Key::TxHash(&tx_hash).into_vec())
                        .expect("get tx should be OK")
                        .expect("stored tx")[12..],
                )
                .expect("from stored tx slice should be OK");
                let output = tx
                    .raw()
                    .outputs()
                    .get(output_index as usize)
                    .expect("get output by index should be OK");
                let output_data = tx
                    .raw()
                    .outputs_data()
                    .get(output_index as usize)
                    .expect("get output data by index should be OK");

                if let Some(prefix) = filter_prefix.as_ref() {
                    match filter_script_type {
                        ScriptType::Lock => {
                            if !extract_raw_data(&output.lock())
                                .as_slice()
                                .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                        ScriptType::Type => {
                            if output.type_().is_none()
                                || !extract_raw_data(&output.type_().to_opt().unwrap())
                                    .as_slice()
                                    .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                    }
                }

                if let Some([r0, r1]) = filter_output_data_len_range {
                    if output_data.len() < r0 || output_data.len() >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_output_capacity_range {
                    let capacity: core::Capacity = output.capacity().unpack();
                    if capacity < r0 || capacity >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_block_range {
                    if block_number < r0 || block_number >= r1 {
                        return None;
                    }
                }

                last_key = key.to_vec();

                Some(Cell {
                    output: output.into(),
                    output_data: output_data.into(),
                    out_point: packed::OutPoint::new(tx_hash, output_index).into(),
                    block_number: block_number.into(),
                    tx_index: tx_index.into(),
                })
            })
            .take(limit.value() as usize)
            .collect::<Vec<_>>();

        Ok(Pagination {
            objects: cells,
            last_cursor: JsonBytes::from_vec(last_key),
        })
    }

    fn get_transactions(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after_cursor: Option<JsonBytes>,
    ) -> Result<Pagination<Tx>> {
        let (prefix, from_key, direction, skip) = build_query_options(
            &search_key,
            KeyPrefix::TxLockScript,
            KeyPrefix::TxTypeScript,
            order,
            after_cursor,
        )?;

        let (filter_script, filter_block_range) = if let Some(filter) = search_key.filter.as_ref() {
            if filter.output_data_len_range.is_some() {
                return Err(Error::invalid_params(
                    "doesn't support search_key.filter.output_data_len_range parameter",
                ));
            }
            if filter.output_capacity_range.is_some() {
                return Err(Error::invalid_params(
                    "doesn't support search_key.filter.output_capacity_range parameter",
                ));
            }
            let filter_script: Option<packed::Script> =
                filter.script.as_ref().map(|script| script.clone().into());
            let filter_block_range: Option<[core::BlockNumber; 2]> =
                filter.block_range.map(|r| [r[0].into(), r[1].into()]);
            (filter_script, filter_block_range)
        } else {
            (None, None)
        };

        let filter_script_type = match search_key.script_type {
            ScriptType::Lock => ScriptType::Type,
            ScriptType::Type => ScriptType::Lock,
        };

        let mode = IteratorMode::From(from_key.as_ref(), direction);
        let snapshot = self.storage.db.snapshot();
        let iter = snapshot.iterator(mode).skip(skip);

        let mut last_key = Vec::new();
        let txs = iter
            .take_while(|(key, _value)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                let tx = packed::Transaction::from_slice(
                    &snapshot
                        .get(Key::TxHash(&tx_hash).into_vec())
                        .expect("get tx should be OK")
                        .expect("stored tx")[12..],
                )
                .expect("from stored tx slice should be OK");

                let block_number = u64::from_be_bytes(
                    key[key.len() - 17..key.len() - 9]
                        .try_into()
                        .expect("stored block_number"),
                );
                let tx_index = u32::from_be_bytes(
                    key[key.len() - 9..key.len() - 5]
                        .try_into()
                        .expect("stored tx_index"),
                );
                let io_index = u32::from_be_bytes(
                    key[key.len() - 5..key.len() - 1]
                        .try_into()
                        .expect("stored io_index"),
                );
                let io_type = if *key.last().expect("stored io_type") == 0 {
                    CellType::Input
                } else {
                    CellType::Output
                };

                if let Some(filter_script) = filter_script.as_ref() {
                    match filter_script_type {
                        ScriptType::Lock => {
                            snapshot
                                .get(
                                    Key::TxLockScript(
                                        filter_script,
                                        block_number,
                                        tx_index,
                                        io_index,
                                        match io_type {
                                            CellType::Input => storage::CellType::Input,
                                            CellType::Output => storage::CellType::Output,
                                        },
                                    )
                                    .into_vec(),
                                )
                                .expect("get TxLockScript should be OK")?;
                        }
                        ScriptType::Type => {
                            snapshot
                                .get(
                                    Key::TxTypeScript(
                                        filter_script,
                                        block_number,
                                        tx_index,
                                        io_index,
                                        match io_type {
                                            CellType::Input => storage::CellType::Input,
                                            CellType::Output => storage::CellType::Output,
                                        },
                                    )
                                    .into_vec(),
                                )
                                .expect("get TxTypeScript should be OK")?;
                        }
                    }
                }

                if let Some([r0, r1]) = filter_block_range {
                    if block_number < r0 || block_number >= r1 {
                        return None;
                    }
                }

                last_key = key.to_vec();
                Some(Tx::Ungrouped(TxWithCell {
                    transaction: tx.into_view().into(),
                    block_number: block_number.into(),
                    tx_index: tx_index.into(),
                    io_index: io_index.into(),
                    io_type,
                }))
            })
            .take(limit.value() as usize)
            .collect::<Vec<_>>();

        Ok(Pagination {
            objects: txs,
            last_cursor: JsonBytes::from_vec(last_key),
        })
    }

    fn get_cells_capacity(&self, search_key: SearchKey) -> Result<Capacity> {
        let (prefix, from_key, direction, skip) = build_query_options(
            &search_key,
            KeyPrefix::CellLockScript,
            KeyPrefix::CellTypeScript,
            Order::Asc,
            None,
        )?;
        let filter_script_type = match search_key.script_type {
            ScriptType::Lock => ScriptType::Type,
            ScriptType::Type => ScriptType::Lock,
        };
        let (
            filter_prefix,
            filter_output_data_len_range,
            filter_output_capacity_range,
            filter_block_range,
        ) = build_filter_options(search_key)?;
        let mode = IteratorMode::From(from_key.as_ref(), direction);
        let snapshot = self.storage.db.snapshot();
        let iter = snapshot.iterator(mode).skip(skip);

        let capacity: u64 = iter
            .take_while(|(key, _value)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                let output_index = u32::from_be_bytes(
                    key[key.len() - 4..]
                        .try_into()
                        .expect("stored output_index"),
                );
                let block_number = u64::from_be_bytes(
                    key[key.len() - 16..key.len() - 8]
                        .try_into()
                        .expect("stored block_number"),
                );

                let tx = packed::Transaction::from_slice(
                    &snapshot
                        .get(Key::TxHash(&tx_hash).into_vec())
                        .expect("get tx should be OK")
                        .expect("stored tx")[12..],
                )
                .expect("from stored tx slice should be OK");
                let output = tx
                    .raw()
                    .outputs()
                    .get(output_index as usize)
                    .expect("get output by index should be OK");
                let output_data = tx
                    .raw()
                    .outputs_data()
                    .get(output_index as usize)
                    .expect("get output data by index should be OK");

                if let Some(prefix) = filter_prefix.as_ref() {
                    match filter_script_type {
                        ScriptType::Lock => {
                            if !extract_raw_data(&output.lock())
                                .as_slice()
                                .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                        ScriptType::Type => {
                            if output.type_().is_none()
                                || !extract_raw_data(&output.type_().to_opt().unwrap())
                                    .as_slice()
                                    .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                    }
                }

                if let Some([r0, r1]) = filter_output_data_len_range {
                    if output_data.len() < r0 || output_data.len() >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_output_capacity_range {
                    let capacity: core::Capacity = output.capacity().unpack();
                    if capacity < r0 || capacity >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_block_range {
                    if block_number < r0 || block_number >= r1 {
                        return None;
                    }
                }

                Some(Unpack::<core::Capacity>::unpack(&output.capacity()).as_u64())
            })
            .sum();

        Ok(capacity.into())
    }
}

impl NetRpc for NetRpcImpl {
    fn get_peers(&self) -> Result<Vec<RemoteNode>> {
        let peers: Vec<RemoteNode> = self
            .network_controller
            .connected_peers()
            .iter()
            .map(|(peer_index, peer)| {
                let mut addresses = vec![&peer.connected_addr];
                addresses.extend(peer.listened_addrs.iter());

                let node_addresses = addresses
                    .iter()
                    .map(|addr| {
                        let score = self
                            .network_controller
                            .addr_info(addr)
                            .map(|addr_info| addr_info.score)
                            .unwrap_or(1);
                        let non_negative_score = if score > 0 { score as u64 } else { 0 };
                        NodeAddress {
                            address: addr.to_string(),
                            score: non_negative_score.into(),
                        }
                    })
                    .collect();

                RemoteNode {
                    version: peer
                        .identify_info
                        .as_ref()
                        .map(|info| info.client_version.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    node_id: extract_peer_id(&peer.connected_addr)
                        .map(|peer_id| peer_id.to_base58())
                        .unwrap_or_default(),
                    addresses: node_addresses,
                    connected_duration: (std::time::Instant::now()
                        .saturating_duration_since(peer.connected_time)
                        .as_millis() as u64)
                        .into(),
                    sync_state: self.peers.get_state(peer_index).map(|state| PeerSyncState {
                        requested_best_known_header: state
                            .get_prove_request()
                            .map(|request| request.get_last_header().header().to_owned().into()),
                        proved_best_known_header: state
                            .get_prove_state()
                            .map(|request| request.get_last_header().header().to_owned().into()),
                    }),
                    protocols: peer
                        .protocols
                        .iter()
                        .map(|(protocol_id, protocol_version)| RemoteNodeProtocol {
                            id: (protocol_id.value() as u64).into(),
                            version: protocol_version.clone(),
                        })
                        .collect(),
                }
            })
            .collect();
        Ok(peers)
    }
}

const MAX_PREFIX_SEARCH_SIZE: usize = u16::max_value() as usize;

// a helper fn to build query options from search paramters, returns prefix, from_key, direction and skip offset
fn build_query_options(
    search_key: &SearchKey,
    lock_prefix: KeyPrefix,
    type_prefix: KeyPrefix,
    order: Order,
    after_cursor: Option<JsonBytes>,
) -> Result<(Vec<u8>, Vec<u8>, Direction, usize)> {
    let mut prefix = match search_key.script_type {
        ScriptType::Lock => vec![lock_prefix as u8],
        ScriptType::Type => vec![type_prefix as u8],
    };
    let script: packed::Script = search_key.script.clone().into();
    let args_len = script.args().len();
    if args_len > MAX_PREFIX_SEARCH_SIZE {
        return Err(Error::invalid_params(format!(
            "search_key.script.args len should be less than {}",
            MAX_PREFIX_SEARCH_SIZE
        )));
    }
    prefix.extend_from_slice(extract_raw_data(&script).as_slice());

    let (from_key, direction, skip) = match order {
        Order::Asc => after_cursor.map_or_else(
            || (prefix.clone(), Direction::Forward, 0),
            |json_bytes| (json_bytes.as_bytes().into(), Direction::Forward, 1),
        ),
        Order::Desc => after_cursor.map_or_else(
            || {
                (
                    [
                        prefix.clone(),
                        vec![0xff; MAX_PREFIX_SEARCH_SIZE - args_len],
                    ]
                    .concat(),
                    Direction::Reverse,
                    0,
                )
            },
            |json_bytes| (json_bytes.as_bytes().into(), Direction::Reverse, 1),
        ),
    };

    Ok((prefix, from_key, direction, skip))
}

// a helper fn to build filter options from search paramters, returns prefix, output_data_len_range, output_capacity_range and block_range
#[allow(clippy::type_complexity)]
fn build_filter_options(
    search_key: SearchKey,
) -> Result<(
    Option<Vec<u8>>,
    Option<[usize; 2]>,
    Option<[core::Capacity; 2]>,
    Option<[core::BlockNumber; 2]>,
)> {
    let SearchKey {
        script: _,
        script_type: _,
        filter,
        group_by_transaction: _,
    } = search_key;
    let filter = filter.unwrap_or_default();
    let filter_script_prefix = if let Some(script) = filter.script {
        let script: packed::Script = script.into();
        if script.args().len() > MAX_PREFIX_SEARCH_SIZE {
            return Err(Error::invalid_params(format!(
                "search_key.filter.script.args len should be less than {}",
                MAX_PREFIX_SEARCH_SIZE
            )));
        }
        let mut prefix = Vec::new();
        prefix.extend_from_slice(extract_raw_data(&script).as_slice());
        Some(prefix)
    } else {
        None
    };

    let filter_output_data_len_range = filter.output_data_len_range.map(|[r0, r1]| {
        [
            Into::<u64>::into(r0) as usize,
            Into::<u64>::into(r1) as usize,
        ]
    });
    let filter_output_capacity_range = filter.output_capacity_range.map(|[r0, r1]| {
        [
            core::Capacity::shannons(r0.into()),
            core::Capacity::shannons(r1.into()),
        ]
    });
    let filter_block_range = filter.block_range.map(|r| [r[0].into(), r[1].into()]);

    Ok((
        filter_script_prefix,
        filter_output_data_len_range,
        filter_output_capacity_range,
        filter_block_range,
    ))
}

// TODO get from consensus
const MAX_CYCLES: Cycle = 3_500_000 * 597;

impl TransactionRpc for TransactionRpcImpl {
    fn send_transaction(&self, tx: Transaction) -> Result<H256> {
        let tx: packed::Transaction = tx.into();
        let tx = tx.into_view();
        let cycles = verify_tx(&self.storage, &self.consensus, tx.clone(), MAX_CYCLES)
            .map_err(|e| Error::invalid_params(format!("invalid transaction: {:?}", e)))?;
        self.pending_txs
            .write()
            .expect("pending_txs lock is poisoned")
            .push(tx.clone(), cycles);

        let content = packed::RelayTransactionHashes::new_builder()
            .tx_hashes(vec![tx.hash()].pack())
            .build();
        let message = packed::RelayMessage::new_builder().set(content).build();
        self.network_controller
            .broadcast(SupportProtocols::RelayV2.protocol_id(), message.as_bytes())
            .map_err(|_err| Error::internal_error())?;

        Ok(tx.hash().unpack())
    }
}

impl ChainRpc for ChainRpcImpl {
    fn get_tip_header(&self) -> Result<HeaderView> {
        Ok(self.storage.get_tip_header().into_view().into())
    }

    fn get_header(&self, block_hash: H256) -> Result<Option<HeaderView>> {
        Ok(self.storage.get_header(&block_hash.pack()).map(Into::into))
    }

    fn get_transaction(&self, tx_hash: H256) -> Result<Option<TransactionWithHeader>> {
        let transaction_with_header = self
            .storage
            .get_transaction_with_header(&tx_hash.pack())
            .map(|(tx, header)| TransactionWithHeader {
                transaction: tx.into_view().into(),
                header: header.into_view().into(),
            });

        Ok(transaction_with_header)
    }
}

pub(crate) struct Service {
    listen_address: String,
}

impl Service {
    pub fn new(listen_address: &str) -> Self {
        Self {
            listen_address: listen_address.to_string(),
        }
    }

    pub fn start(
        &self,
        network_controller: NetworkController,
        storage: Storage,
        consensus: Consensus,
        peers: Arc<Peers>,
        pending_txs: Arc<RwLock<PendingTxs>>,
    ) -> Server {
        let mut io_handler = IoHandler::new();
        let block_filter_rpc_impl = BlockFilterRpcImpl {
            storage: storage.clone(),
        };
        let chain_rpc_impl = ChainRpcImpl {
            storage: storage.clone(),
        };
        let transaction_rpc_impl = TransactionRpcImpl {
            network_controller: network_controller.clone(),
            pending_txs,
            storage,
            consensus,
        };
        let net_rpc_impl = NetRpcImpl {
            network_controller,
            peers,
        };
        io_handler.extend_with(block_filter_rpc_impl.to_delegate());
        io_handler.extend_with(chain_rpc_impl.to_delegate());
        io_handler.extend_with(transaction_rpc_impl.to_delegate());
        io_handler.extend_with(net_rpc_impl.to_delegate());

        ServerBuilder::new(io_handler)
            .cors(DomainsValidation::AllowOnly(vec![
                AccessControlAllowOrigin::Null,
                AccessControlAllowOrigin::Any,
            ]))
            .health_api(("/ping", "ping"))
            .start_http(
                &self
                    .listen_address
                    .to_socket_addrs()
                    .expect("config listen_address parsed")
                    .next()
                    .expect("config listen_address parsed"),
            )
            .expect("Start Jsonrpc HTTP service")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::storage::Storage;
    use ckb_types::{
        bytes::Bytes,
        core::{
            capacity_bytes, BlockBuilder, Capacity, HeaderBuilder, ScriptHashType,
            TransactionBuilder,
        },
        packed::{CellInput, CellOutputBuilder, OutPoint, Script, ScriptBuilder},
        H256,
    };
    use tempfile;

    fn new_storage(prefix: &str) -> Storage {
        let tmp_dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        Storage::new(tmp_dir.path().to_str().unwrap())
    }

    #[test]
    fn rpc() {
        let storage = new_storage("rpc");
        let rpc = BlockFilterRpcImpl {
            storage: storage.clone(),
        };

        // setup test data
        let lock_script1 = ScriptBuilder::default()
            .code_hash(H256(rand::random()).pack())
            .hash_type(ScriptHashType::Data.into())
            .args(Bytes::from(b"lock_script1".to_vec()).pack())
            .build();

        let lock_script2 = ScriptBuilder::default()
            .code_hash(H256(rand::random()).pack())
            .hash_type(ScriptHashType::Type.into())
            .args(Bytes::from(b"lock_script2".to_vec()).pack())
            .build();

        let type_script1 = ScriptBuilder::default()
            .code_hash(H256(rand::random()).pack())
            .hash_type(ScriptHashType::Data.into())
            .args(Bytes::from(b"type_script1".to_vec()).pack())
            .build();

        let type_script2 = ScriptBuilder::default()
            .code_hash(H256(rand::random()).pack())
            .hash_type(ScriptHashType::Type.into())
            .args(Bytes::from(b"type_script2".to_vec()).pack())
            .build();

        let cellbase0 = TransactionBuilder::default()
            .input(CellInput::new_cellbase_input(0))
            .witness(Script::default().into_witness())
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(1000).pack())
                    .lock(lock_script1.clone())
                    .build(),
            )
            .output_data(Default::default())
            .build();

        let tx00 = TransactionBuilder::default()
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(1000).pack())
                    .lock(lock_script1.clone())
                    .type_(Some(type_script1.clone()).pack())
                    .build(),
            )
            .output_data(Default::default())
            .build();

        let tx01 = TransactionBuilder::default()
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(2000).pack())
                    .lock(lock_script2.clone())
                    .type_(Some(type_script2.clone()).pack())
                    .build(),
            )
            .output_data(Default::default())
            .build();

        let block0 = BlockBuilder::default()
            .transaction(cellbase0)
            .transaction(tx00.clone())
            .transaction(tx01.clone())
            .header(HeaderBuilder::default().number(0.pack()).build())
            .build();

        storage.init_genesis_block(block0.data());
        storage.update_filter_scripts(HashMap::from([(lock_script1.clone(), 0)]));

        let (mut pre_tx0, mut pre_tx1, mut pre_block) = (tx00, tx01, block0);
        let total_blocks = 255;
        for i in 1..total_blocks {
            let cellbase = TransactionBuilder::default()
                .input(CellInput::new_cellbase_input(i + 1))
                .witness(Script::default().into_witness())
                .output(
                    CellOutputBuilder::default()
                        .capacity(capacity_bytes!(1000).pack())
                        .lock(lock_script1.clone())
                        .build(),
                )
                .output_data(Default::default())
                .build();

            pre_tx0 = TransactionBuilder::default()
                .input(CellInput::new(OutPoint::new(pre_tx0.hash(), 0), 0))
                .output(
                    CellOutputBuilder::default()
                        .capacity(capacity_bytes!(1000).pack())
                        .lock(lock_script1.clone())
                        .type_(Some(type_script1.clone()).pack())
                        .build(),
                )
                .output_data(Default::default())
                .build();

            pre_tx1 = TransactionBuilder::default()
                .input(CellInput::new(OutPoint::new(pre_tx1.hash(), 0), 0))
                .output(
                    CellOutputBuilder::default()
                        .capacity(capacity_bytes!(2000).pack())
                        .lock(lock_script2.clone())
                        .type_(Some(type_script2.clone()).pack())
                        .build(),
                )
                .output_data(Default::default())
                .build();

            pre_block = BlockBuilder::default()
                .transaction(cellbase)
                .transaction(pre_tx0.clone())
                .transaction(pre_tx1.clone())
                .header(
                    HeaderBuilder::default()
                        .number((pre_block.number() + 1).pack())
                        .parent_hash(pre_block.hash())
                        .build(),
                )
                .build();

            storage.filter_block(pre_block.data());
        }

        // test get_cells rpc
        let cells_page_1 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                150.into(),
                None,
            )
            .unwrap();
        let cells_page_2 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                150.into(),
                Some(cells_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(
            total_blocks as usize + 1,
            cells_page_1.objects.len() + cells_page_2.objects.len(),
            "total size should be cellbase cells count + 1 (last block live cell)"
        );

        let cells_page_1 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script2.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                150.into(),
                None,
            )
            .unwrap();

        assert_eq!(
            0,
            cells_page_1.objects.len(),
            "total size should be zero with unfiltered lock script"
        );

        let desc_cells_page_1 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Desc,
                150.into(),
                None,
            )
            .unwrap();

        let desc_cells_page_2 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Desc,
                150.into(),
                Some(desc_cells_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(
            total_blocks as usize + 1,
            desc_cells_page_1.objects.len() + desc_cells_page_2.objects.len(),
            "total size should be cellbase cells count + 1 (last block live cell)"
        );
        assert_eq!(
            desc_cells_page_1.objects.first().unwrap().out_point,
            cells_page_2.objects.last().unwrap().out_point
        );

        let filter_cells_page_1 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    filter: Some(SearchKeyFilter {
                        block_range: Some([100.into(), 200.into()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Order::Asc,
                60.into(),
                None,
            )
            .unwrap();

        let filter_cells_page_2 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    filter: Some(SearchKeyFilter {
                        block_range: Some([100.into(), 200.into()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Order::Asc,
                60.into(),
                Some(filter_cells_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(
            100,
            filter_cells_page_1.objects.len() + filter_cells_page_2.objects.len(),
            "total size should be filtered cellbase cells (100~199)"
        );

        // test get_transactions rpc
        let txs_page_1 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                500.into(),
                None,
            )
            .unwrap();
        let txs_page_2 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                500.into(),
                Some(txs_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(total_blocks as usize * 3 - 1, txs_page_1.objects.len() + txs_page_2.objects.len(), "total size should be cellbase tx count + total_block * 2 - 1 (genesis block only has one tx)");

        let desc_txs_page_1 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Desc,
                500.into(),
                None,
            )
            .unwrap();
        let desc_txs_page_2 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Desc,
                500.into(),
                Some(desc_txs_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(total_blocks as usize * 3 - 1, desc_txs_page_1.objects.len() + desc_txs_page_2.objects.len(), "total size should be cellbase tx count + total_block * 2 - 1 (genesis block only has one tx)");
        assert_eq!(
            desc_txs_page_1.objects.first().unwrap().tx_hash(),
            txs_page_2.objects.last().unwrap().tx_hash(),
        );

        let filter_txs_page_1 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    filter: Some(SearchKeyFilter {
                        block_range: Some([100.into(), 200.into()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Order::Asc,
                200.into(),
                None,
            )
            .unwrap();

        let filter_txs_page_2 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    filter: Some(SearchKeyFilter {
                        block_range: Some([100.into(), 200.into()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Order::Asc,
                200.into(),
                Some(filter_txs_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(
            300,
            filter_txs_page_1.objects.len() + filter_txs_page_2.objects.len(),
            "total size should be filtered blocks count * 3 (100~199 * 3)"
        );

        // test get_cells_capacity rpc
        let capacity = rpc
            .get_cells_capacity(SearchKey {
                script: lock_script1.clone().into(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            1000 * 100000000 * (total_blocks + 1),
            capacity.value(),
            "cellbases + last block live cell"
        );

        let capacity = rpc
            .get_cells_capacity(SearchKey {
                script: lock_script2.clone().into(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(0, capacity.value(), "lock_script2 is not filtered");

        // test get_header rpc
        let rpc = ChainRpcImpl {
            storage: storage.clone(),
        };
        let header = rpc
            .get_header(pre_block.header().hash().unpack())
            .unwrap()
            .unwrap();
        assert_eq!(pre_block.header().number(), header.inner.number.value(),);

        // test get_transaction rpc
        let TransactionWithHeader {
            transaction,
            header,
        } = rpc
            .get_transaction(pre_tx0.hash().unpack())
            .unwrap()
            .unwrap();
        assert_eq!(transaction.hash, pre_tx0.hash().unpack());
        assert_eq!(header.hash, pre_block.header().hash().unpack());

        // test rollback_filtered_transactions
        // rollback 2 blocks
        storage.update_filter_scripts(HashMap::from([(lock_script1.clone(), total_blocks)]));
        storage.rollback_filtered_transactions((total_blocks - 1).into());
        storage.rollback_filtered_transactions((total_blocks - 2).into());
        let rpc = BlockFilterRpcImpl {
            storage: storage.clone(),
        };

        // test get_cells rpc after rollback
        let cells_page_1 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                150.into(),
                None,
            )
            .unwrap();
        let cells_page_2 = rpc
            .get_cells(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                150.into(),
                Some(cells_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!(
            total_blocks as usize - 1,
            cells_page_1.objects.len() + cells_page_2.objects.len(),
            "total size should be cellbase cells count + 1 (last block live cell) - 2 (rollbacked blocks cells)"
        );

        // test get_transactions rpc after rollback
        let txs_page_1 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                500.into(),
                None,
            )
            .unwrap();
        let txs_page_2 = rpc
            .get_transactions(
                SearchKey {
                    script: lock_script1.clone().into(),
                    ..Default::default()
                },
                Order::Asc,
                500.into(),
                Some(txs_page_1.last_cursor),
            )
            .unwrap();

        assert_eq!((total_blocks - 2) as usize * 3 - 1, txs_page_1.objects.len() + txs_page_2.objects.len(), "total size should be cellbase tx count + (total_block - 2) * 2 - 1 (genesis block only has one tx)");

        // test get_cells_capacity rpc after rollback
        let capacity = rpc
            .get_cells_capacity(SearchKey {
                script: lock_script1.clone().into(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            1000 * 100000000 * (total_blocks - 1),
            capacity.value(),
            "cellbases + last block live cell - 2 (rollbacked blocks cells)"
        );
    }

    #[test]
    fn get_cells_capacity_bug() {
        let storage = new_storage("get_cells_capacity_bug");
        let rpc = BlockFilterRpcImpl {
            storage: storage.clone(),
        };

        // setup test data
        let lock_script1 = ScriptBuilder::default()
            .code_hash(H256(rand::random()).pack())
            .hash_type(ScriptHashType::Data.into())
            .args(Bytes::from(b"lock_script1".to_vec()).pack())
            .build();

        let tx00 = TransactionBuilder::default()
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(222).pack())
                    .lock(lock_script1.clone())
                    .build(),
            )
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(333).pack())
                    .lock(lock_script1.clone())
                    .build(),
            )
            .output_data(Default::default())
            .output_data(Default::default())
            .build();

        let block0 = BlockBuilder::default()
            .transaction(tx00.clone())
            .header(HeaderBuilder::default().number(0.pack()).build())
            .build();
        storage.init_genesis_block(block0.data());
        storage.update_filter_scripts(HashMap::from([(lock_script1.clone(), 0)]));

        let lock_script2 = ScriptBuilder::default()
            .code_hash(H256(rand::random()).pack())
            .hash_type(ScriptHashType::Data.into())
            .args(Bytes::from(b"lock_script2".to_vec()).pack())
            .build();

        let tx10 = TransactionBuilder::default()
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(100).pack())
                    .lock(lock_script2.clone())
                    .build(),
            )
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(1000).pack())
                    .lock(lock_script1.clone())
                    .build(),
            )
            .output_data(Default::default())
            .output_data(Default::default())
            .build();

        let block1 = BlockBuilder::default()
            .transaction(tx10.clone())
            .header(HeaderBuilder::default().number(1.pack()).build())
            .build();
        storage.filter_block(block1.data());

        let tx20 = TransactionBuilder::default()
            .input(CellInput::new(OutPoint::new(tx00.hash(), 1), 0))
            .input(CellInput::new(OutPoint::new(tx10.hash(), 1), 0))
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(5000).pack())
                    .lock(lock_script2.clone())
                    .build(),
            )
            .output(
                CellOutputBuilder::default()
                    .capacity(capacity_bytes!(3000).pack())
                    .lock(lock_script1.clone())
                    .build(),
            )
            .output_data(Default::default())
            .output_data(Default::default())
            .build();

        let block2 = BlockBuilder::default()
            .transaction(tx20.clone())
            .header(HeaderBuilder::default().number(2.pack()).build())
            .build();
        storage.filter_block(block2.data());

        let capacity = rpc
            .get_cells_capacity(SearchKey {
                script: lock_script1.clone().into(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!((222 + 3000) * 100000000, capacity.value());
    }
}
