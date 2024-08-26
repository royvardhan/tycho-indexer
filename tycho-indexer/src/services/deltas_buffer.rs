use crate::extractor::{
    evm::AggregatedBlockChanges,
    revert_buffer::{BlockNumberOrTimestamp, FinalityStatus, RevertBuffer},
    runner::MessageSender,
};
use ethers::prelude::StreamExt;
use futures03::stream;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio_stream::wrappers::ReceiverStream;
use tracing::debug;
use tycho_core::{
    models::{
        blockchain::BlockAggregatedDeltas,
        contract::Contract,
        protocol::{ProtocolComponent, ProtocolComponentState},
        DeltaError, NormalisedMessage,
    },
    storage::StorageError,
};

/// The `PendingDeltas` struct manages access to the revert buffers maintained by each extractor.
///
/// The main responsibilities of `PendingDeltas` include:
/// - Inserting new blocks and deltas into the correct `RevertBuffer`.
/// - Managing and applying deltas to state data, which includes merging buffered changes with data
///   fetched from the database.
/// - Retrieving finality status for blocks, which is used to determine whether to fetch data from
///   the database and/or from the buffer.
#[derive(Default, Clone)]
pub struct PendingDeltas {
    buffers: HashMap<String, Arc<Mutex<RevertBuffer<BlockAggregatedDeltas>>>>,
}

#[derive(Error, Debug, PartialEq)]
pub enum PendingDeltasError {
    #[error("Failed to acquire {0} lock: {1}")]
    LockError(String, String),
    #[error("RevertBufferError: {0}")]
    RevertBufferError(#[from] StorageError),
    #[error("Downcast failed: Unknown message type")]
    UnknownMessageType,
    #[error("Unknown extractor: {0}")]
    UnknownExtractor(String),
    #[error("Failed applying deltas: {0}")]
    DeltaApplicationFailure(#[from] DeltaError),
}

pub type Result<T> = std::result::Result<T, PendingDeltasError>;

impl PendingDeltas {
    pub fn new<'a>(extractors: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            buffers: extractors
                .into_iter()
                .map(|e| {
                    debug!("Creating new RevertBuffer for {}", e);
                    (e.to_string(), Arc::new(Mutex::new(RevertBuffer::new())))
                })
                .collect(),
        }
    }

    fn insert(&self, message: Arc<dyn NormalisedMessage>) -> Result<()> {
        let maybe_convert: Option<BlockAggregatedDeltas> = message
            .as_any()
            .downcast_ref::<AggregatedBlockChanges>()
            .map(|msg| msg.into());
        match maybe_convert {
            Some(msg) => {
                let maybe_buffer = self.buffers.get(&msg.extractor);

                match maybe_buffer {
                    Some(buffer) => {
                        let mut guard = buffer.lock().map_err(|e| {
                            PendingDeltasError::LockError(msg.extractor.to_string(), e.to_string())
                        })?;
                        if msg.revert {
                            guard.purge(msg.block.hash.clone())?;
                        } else {
                            guard.insert_block(msg.clone())?;
                            guard.drain_new_finalized_blocks(msg.finalised_block_height)?;
                        }
                    }
                    _ => return Err(PendingDeltasError::UnknownExtractor(msg.extractor.clone())),
                }
            }
            None => return Err(PendingDeltasError::UnknownMessageType),
        }

        Ok(())
    }

    /// Merges the buffered deltas with given db states. If a requested component is not in the
    /// db yet, it creates a new state for it out of the buffered deltas.
    ///
    /// Arguments:
    ///
    /// * `protocol_ids`: A list of the requested protocol ids. Note: `None` is not supported yet.
    /// * `db_states`: A mutable reference to the states fetched from the db.
    /// * `version`: The version of the state to be fetched. If `None`, the latest state will be
    ///   fetched.
    pub fn merge_native_states(
        &self,
        protocol_ids: Option<&[&str]>,
        db_states: &mut Vec<ProtocolComponentState>,
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<()> {
        // TODO: handle when no id is specified with filters
        let mut missing_ids: HashSet<&str> = protocol_ids
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();

        // update db states with buffered deltas
        for state in db_states.iter_mut() {
            self.update_native_state(state, version)?;
            missing_ids.remove(state.component_id.as_str());
        }

        // for new components (not in the db yet), create an empty state and apply buffered deltas
        // to it
        for id in missing_ids {
            let mut state = ProtocolComponentState::new(id, HashMap::new(), HashMap::new());
            self.update_native_state(&mut state, version)?;
            db_states.push(state);
        }

        Ok(())
    }

    fn update_native_state(
        &self,
        db_state: &mut ProtocolComponentState,
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<bool> {
        let mut change_found = false;
        for buffer in self.buffers.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("Native".to_string(), e.to_string()))?;

            for entry in guard.get_block_range(None, version)? {
                if let Some(delta) = entry
                    .state_deltas
                    .get(&db_state.component_id)
                {
                    db_state.apply_state_delta(delta)?;
                    change_found = true;
                }

                if let Some(delta) = entry
                    .component_balances
                    .get(&db_state.component_id)
                {
                    db_state.apply_balance_delta(delta)?;
                    change_found = true
                }
            }

            if change_found {
                // if we found some changes no need to check other extractor's buffer
                break;
            }
        }
        Ok(change_found)
    }

    pub fn update_vm_states(
        &self,
        db_states: &mut [Contract],
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<()> {
        for state in db_states {
            self.update_vm_state(state, version)?;
        }
        Ok(())
    }

    fn update_vm_state(
        &self,
        db_state: &mut Contract,
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<bool> {
        let mut change_found = false;
        for buffer in self.buffers.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("VM".to_string(), e.to_string()))?;
            for entry in guard.get_block_range(None, version)? {
                if let Some(delta) = entry
                    .account_deltas
                    .get(&db_state.address)
                {
                    db_state.apply_contract_delta(delta)?;
                    change_found = true;
                }
                // TODO: currently it is impossible to apply balance changes and state deltas since
                //  we don't know the component_id of the contract.
            }

            if change_found {
                // if we found some changes no need to check other extractor's buffer
                break;
            }
        }

        Ok(change_found)
    }

    /// Retrieves a list of new protocol components that match all the provided criteria.
    /// (The filters are combined using an AND logic.)
    ///
    /// # Parameters
    /// - `ids`: `Option<&[&str]>`
    ///     - A list of component IDs to filter the results. If `None`, all components will be
    ///       considered.
    /// - `protocol_system`: `Option<&str>`
    ///     - The protocol system to filter the components by. If `None`, components from all
    ///       protocol systems will be considered.
    ///
    /// # Returns
    /// - `Result<Vec<ProtocolComponent>>`
    ///     - A `Result` which is `Ok` containing a vector of `ProtocolComponent` objects if
    ///       successful, or an error otherwise.
    ///
    /// # Example
    /// ```
    /// let components = get_new_components(Some(&["id1", "id2"]), Some("system1"))?;
    /// ```
    pub fn get_new_components(
        &self,
        ids: Option<&[&str]>,
        protocol_system: Option<&str>,
    ) -> Result<Vec<ProtocolComponent>> {
        let requested_ids: Option<HashSet<&str>> = ids.map(|ids| ids.iter().cloned().collect());
        let mut new_components = Vec::new();
        for (name, buffer) in self.buffers.iter() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError(name.to_string(), e.to_string()))?;
            for entry in guard.get_block_range(None, None)? {
                new_components.extend(
                    entry
                        .new_components
                        .clone()
                        .into_values()
                        .filter(|comp| {
                            if let Some(ids) = requested_ids.as_ref() {
                                ids.contains(comp.id.as_str())
                            } else {
                                true
                            }
                        }),
                );
            }
        }

        if let Some(system) = protocol_system {
            new_components.retain(|c| c.protocol_system == system);
        }

        Ok(new_components)
    }

    /// Returns finality for any extractor, can error if lock is poisened. Returns None if buffer is
    /// empty.
    /// Returns an error if the provided protocol system isn't found in the buffer.
    /// Note - if no protocol system is provided, we choose a random extractor to get the finality
    /// status from. This is particularly risky when there is an extractor syncing.
    pub fn get_block_finality(
        &self,
        version: BlockNumberOrTimestamp,
        protocol_system: Option<String>,
    ) -> Result<Option<FinalityStatus>> {
        match protocol_system {
            Some(system) => {
                if let Some(buffer) = self.buffers.get(&system) {
                    let guard = buffer
                        .lock()
                        .map_err(|e| PendingDeltasError::LockError(system, e.to_string()))?;
                    Ok(guard.get_finality_status(version))
                } else {
                    debug!(?system, "Missing requested protocol system in pending deltas");
                    Err(PendingDeltasError::UnknownExtractor(system))
                }
            }
            None => {
                // Use any extractor to get the finality status
                let maybe_buffer = self.buffers.iter().next();

                match maybe_buffer {
                    Some((name, buffer)) => {
                        let guard = buffer.lock().map_err(|e| {
                            PendingDeltasError::LockError(name.to_string(), e.to_string())
                        })?;
                        Ok(guard.get_finality_status(version))
                    }
                    _ => Ok(None),
                }
            }
        }
    }

    pub async fn run(
        self,
        extractors: impl IntoIterator<Item = Arc<dyn MessageSender + Send + Sync>>,
    ) -> anyhow::Result<()> {
        let mut rxs = Vec::new();
        for extractor in extractors.into_iter() {
            let res = ReceiverStream::new(extractor.subscribe().await?);
            rxs.push(res);
        }

        let all_messages = stream::select_all(rxs);

        // What happens if an extractor restarts - it might just end here and be dropped?
        // Ideally the Runner should never restart.
        all_messages
            .for_each(|message| async {
                self.insert(message).unwrap();
            })
            .await;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        extractor::{evm, evm::AccountUpdate},
        testing::evm_block,
    };
    use ethers::types::{H160, H256, U256};
    use std::str::FromStr;
    use tycho_core::{
        models::{Chain, ChangeType},
        Bytes,
    };

    fn vm_state() -> Contract {
        Contract::new(
            Chain::Ethereum,
            Bytes::from("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05"),
            "Contract1".to_string(),
            evm::fixtures::evm_slots([(2, 2)])
                .into_iter()
                .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
                .collect::<HashMap<_, _>>(),
            Bytes::from("0x1999"),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0xbabe"),
            Bytes::from("0x4200"),
            Bytes::from("0x4200"),
            None,
        )
    }

    fn vm_block_deltas() -> AggregatedBlockChanges {
        let address = H160::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        AggregatedBlockChanges::new(
            "vm:extractor",
            Chain::Ethereum,
            evm_block(1),
            1,
            false,
            [(
                "component2".to_string(),
                evm::ProtocolComponent {
                    id: "component2".to_string(),
                    protocol_system: "vm_swap".to_string(),
                    protocol_type_name: "swap".to_string(),
                    chain: Chain::Ethereum,
                    tokens: Vec::new(),
                    contract_ids: Vec::new(),
                    static_attributes: HashMap::new(),
                    change: ChangeType::Creation,
                    creation_tx: H256::zero(),
                    created_at: "2020-01-01T00:00:00".parse().unwrap(),
                },
            )]
            .into_iter()
            .collect::<HashMap<_, _>>(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            [(
                address,
                AccountUpdate::new(
                    address,
                    Chain::Ethereum,
                    evm::fixtures::evm_slots([(1, 1), (2, 1)]),
                    Some(U256::from(1999)),
                    None,
                    ChangeType::Update,
                ),
            )]
            .into_iter()
            .collect::<HashMap<_, _>>(),
            HashMap::new(),
        )
    }

    fn native_state() -> ProtocolComponentState {
        ProtocolComponentState::new(
            "component1",
            [("attr2", Bytes::from("0x02")), ("attr1", Bytes::from("0x00"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x02"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        )
    }

    fn native_block_deltas() -> AggregatedBlockChanges {
        AggregatedBlockChanges::new(
            "native:extractor",
            Chain::Ethereum,
            evm_block(1),
            1,
            false,
            [(
                "component3".to_string(),
                evm::ProtocolComponent {
                    id: "component3".to_string(),
                    protocol_system: "native_swap".to_string(),
                    protocol_type_name: "swap".to_string(),
                    chain: Chain::Ethereum,
                    tokens: Vec::new(),
                    contract_ids: Vec::new(),
                    static_attributes: HashMap::new(),
                    change: ChangeType::Creation,
                    creation_tx: H256::zero(),
                    created_at: "2020-01-01T00:00:00".parse().unwrap(),
                },
            )]
            .into_iter()
            .collect::<HashMap<_, _>>(),
            HashMap::new(),
            HashMap::new(),
            [
                evm::ProtocolStateDelta::new(
                    "component1".to_string(),
                    [("attr1", Bytes::from("0x01"))]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ),
                evm::ProtocolStateDelta::new(
                    "component3".to_string(),
                    [("attr2", Bytes::from("0x05"))]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ),
            ]
            .into_iter()
            .map(|v| (v.component_id.clone(), v))
            .collect(),
            HashMap::new(),
            [
                (
                    "component1".to_string(),
                    [(
                        H160::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        evm::ComponentBalance {
                            token: H160::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
                                .unwrap(),
                            balance_float: 1.0,
                            balance: Bytes::from("0x01"),
                            modify_tx: H256::zero(),
                            component_id: "component1".to_string(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                (
                    "component3".to_string(),
                    [(
                        H160::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        evm::ComponentBalance {
                            token: H160::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
                                .unwrap(),
                            balance_float: 2.0,
                            balance: Bytes::from("0x02"),
                            modify_tx: H256::zero(),
                            component_id: "component3".to_string(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
            ]
            .into_iter()
            .collect(),
        )
    }

    #[test]
    fn test_insert_vm() {
        let buffer = PendingDeltas::new(["vm:extractor"]);
        let exp: BlockAggregatedDeltas = (&vm_block_deltas()).into();

        buffer
            .insert(Arc::new(vm_block_deltas()))
            .expect("vm insert failed");

        let revert_buffer = buffer
            .buffers
            .get("vm:extractor")
            .expect("vm:extractor buffer missing");
        let binding = revert_buffer.lock().unwrap();
        let res = binding
            .get_block_range(None, None)
            .expect("Failed to get block range")
            .collect::<Vec<_>>();
        assert_eq!(res[0], &exp);
    }

    #[test]
    fn test_insert_native() {
        let pending_buffer = PendingDeltas::new(["native:extractor"]);
        let exp: BlockAggregatedDeltas = (&native_block_deltas()).into();

        pending_buffer
            .insert(Arc::new(native_block_deltas()))
            .expect("native insert failed");

        let native_revert_buffer = pending_buffer
            .buffers
            .get("native:extractor")
            .expect("native:extractor buffer missing");
        let binding = native_revert_buffer.lock().unwrap();
        let res = binding
            .get_block_range(None, None)
            .expect("Failed to get block range")
            .collect::<Vec<_>>();
        assert_eq!(res[0], &exp);
    }

    #[test]
    fn test_merge_native_states() {
        let mut state = vec![native_state()]; // db state
        let buffer = PendingDeltas::new(["native:extractor"]);
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();
        // expected state after applying buffer deltas to the given db state
        let exp1 = ProtocolComponentState::new(
            "component1",
            [("attr2", Bytes::from("0x02")), ("attr1", Bytes::from("0x01"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x01"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );
        // expected state for a new component not yet in the db
        let exp3 = ProtocolComponentState::new(
            "component3",
            [("attr2", Bytes::from("0x05"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x02"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );

        buffer
            .merge_native_states(
                Some(&["component1", "component3"]),
                &mut state,
                Some(BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap())),
            )
            .unwrap();

        assert_eq!(state.len(), 2);
        assert_eq!(&state[0], &exp1);
        assert_eq!(&state[1], &exp3);
    }

    #[test]
    fn test_update_vm_states() {
        let mut state = [vm_state()];
        let buffer = PendingDeltas::new(["vm:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .unwrap();
        let exp = Contract::new(
            Chain::Ethereum,
            Bytes::from("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05"),
            "Contract1".to_string(),
            evm::fixtures::evm_slots([(1, 1), (2, 1)])
                .into_iter()
                .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
                .collect::<HashMap<_, _>>(),
            Bytes::from("0x00000000000000000000000000000000000000000000000000000000000007cf"),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0xbabe"),
            Bytes::from("0x4200"),
            Bytes::from("0x4200"),
            None,
        );

        buffer
            .update_vm_states(
                &mut state,
                Some(BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap())),
            )
            .unwrap();

        assert_eq!(&state[0], &exp);
    }

    #[test]
    fn test_get_new_components() {
        let exp = vec![
            ProtocolComponent::new(
                "component3",
                "native_swap",
                "swap",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
                "2020-01-01T00:00:00".parse().unwrap(),
            ),
            ProtocolComponent::new(
                "component2",
                "vm_swap",
                "swap",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
                "2020-01-01T00:00:00".parse().unwrap(),
            ),
        ];
        let buffer = PendingDeltas::new(["vm:extractor", "native:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .unwrap();
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();

        let new_components = buffer
            .get_new_components(None, None)
            .unwrap();

        for expected in &exp {
            assert!(new_components.contains(expected));
        }

        let new_components = buffer
            .get_new_components(Some(&["component3"]), None)
            .unwrap();

        assert_eq!(new_components, vec![exp[0].clone()]);

        let new_components = buffer
            .get_new_components(None, Some("vm_swap"))
            .unwrap();

        assert_eq!(new_components, vec![exp[1].clone()]);
    }

    use rstest::rstest;

    #[rstest]
    // native extractor
    #[case(Some("native:extractor".to_string()), None)]
    // vm extractor
    #[case(Some("vm:extractor".to_string()), None)]
    // bad input
    #[case(Some("unknown_system".to_string()), Some(PendingDeltasError::UnknownExtractor("unknown_system".to_string())))]
    // no extractor provided
    #[allow(clippy::duplicated_attributes)]
    #[case(None, None)]
    fn test_get_block_finality(
        #[case] protocol_system: Option<String>,
        #[case] expected_error: Option<PendingDeltasError>,
    ) {
        let buffer = PendingDeltas::new(["vm:extractor", "native:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .expect("vm insert failed");
        buffer
            .insert(Arc::new(native_block_deltas()))
            .expect("native insert failed");

        let version = BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap());

        let result = buffer.get_block_finality(version, protocol_system.clone());

        match expected_error {
            Some(expected_err) => {
                assert!(matches!(result, Err(ref err) if err == &expected_err));
            }
            None => {
                let finality_status = result.expect("Failed to get block finality");
                assert!(finality_status.is_some());
            }
        }
    }
}
