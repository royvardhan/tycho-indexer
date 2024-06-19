use crate::extractor::{
    evm::{BlockAccountChanges, BlockEntityChangesResult},
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
        blockchain::{NativeBlockDeltas, VmBlockDeltas},
        contract::Contract,
        protocol::{ProtocolComponent, ProtocolComponentState},
        DeltaError, NormalisedMessage,
    },
    storage::StorageError,
};

type NativeRevertBuffer = Arc<Mutex<RevertBuffer<NativeBlockDeltas>>>;
type VmRevertBuffer = Arc<Mutex<RevertBuffer<VmBlockDeltas>>>;

#[derive(Default, Clone)]
pub struct PendingDeltas {
    native: HashMap<String, NativeRevertBuffer>,
    vm: HashMap<String, VmRevertBuffer>,
}

#[derive(Error, Debug)]
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
    pub fn new<'a>(
        vm_extractors: impl IntoIterator<Item = &'a str>,
        native_extractors: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            native: native_extractors
                .into_iter()
                .map(|e| {
                    debug!("Creating new NativeRevertBuffer for {}", e);
                    (e.to_string(), Arc::new(Mutex::new(RevertBuffer::new())))
                })
                .collect(),
            vm: vm_extractors
                .into_iter()
                .map(|e| {
                    debug!("Creating new VmRevertBuffer for {}", e);
                    (e.to_string(), Arc::new(Mutex::new(RevertBuffer::new())))
                })
                .collect(),
        }
    }

    fn insert(&self, message: Arc<dyn NormalisedMessage>) -> Result<()> {
        let maybe_native: Option<NativeBlockDeltas> = message
            .as_any()
            .downcast_ref::<BlockEntityChangesResult>()
            .map(|msg| msg.into());
        let maybe_vm: Option<VmBlockDeltas> = message
            .as_any()
            .downcast_ref::<BlockAccountChanges>()
            .map(|msg| msg.into());
        match (maybe_native, maybe_vm) {
            (Some(msg), None) => {
                if let Some(buffer) = self.native.get(&msg.extractor) {
                    let mut guard = buffer.lock().map_err(|e| {
                        PendingDeltasError::LockError("Native".to_string(), e.to_string())
                    })?;
                    if msg.revert {
                        guard.purge(msg.block.hash.clone())?;
                    } else {
                        guard.insert_block(msg.clone())?;
                        guard.drain_new_finalized_blocks(msg.finalised_block_height)?;
                    }
                } else {
                    return Err(PendingDeltasError::UnknownExtractor(msg.extractor.clone()));
                }
            }
            (None, Some(msg)) => {
                if let Some(buffer) = self.vm.get(&msg.extractor) {
                    let mut guard = buffer.lock().map_err(|e| {
                        PendingDeltasError::LockError("VM".to_string(), e.to_string())
                    })?;
                    if msg.revert {
                        guard.purge(msg.block.hash.clone())?;
                    } else {
                        guard.insert_block(msg.clone())?;
                        guard.drain_new_finalized_blocks(msg.finalised_block_height)?;
                    }
                } else {
                    return Err(PendingDeltasError::UnknownExtractor(msg.extractor.clone()));
                }
            }
            _ => return Err(PendingDeltasError::UnknownMessageType),
        }

        Ok(())
    }

    pub fn update_native_states(
        &self,
        db_states: &mut [ProtocolComponentState],
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<()> {
        for state in db_states {
            self.update_native_state(state, version)?;
        }
        Ok(())
    }

    fn update_native_state(
        &self,
        db_state: &mut ProtocolComponentState,
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<bool> {
        let mut change_found = false;
        for buffer in self.native.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("Native".to_string(), e.to_string()))?;

            for entry in guard.get_block_range(None, version)? {
                if let Some(delta) = entry.deltas.get(&db_state.component_id) {
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
        for buffer in self.vm.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("VM".to_string(), e.to_string()))?;
            for entry in guard.get_block_range(None, version)? {
                if let Some(delta) = entry.deltas.get(&db_state.address) {
                    db_state.apply_contract_delta(delta)?;
                    change_found = true;
                }

                // TODO: currently it is impossible to apply balance changes since
                //  we don't know the component_id of the contract.
            }

            if change_found {
                // if we found some changes no need to check other extractor's buffer
                break;
            }
        }

        Ok(change_found)
    }

    pub fn get_new_components(&self, ids: Option<&[&str]>) -> Result<Vec<ProtocolComponent>> {
        let requested_ids: Option<HashSet<&str>> = ids.map(|ids| ids.iter().cloned().collect());
        let mut new_components = Vec::new();
        for buffer in self.native.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("Native".to_string(), e.to_string()))?;
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
        for buffer in self.vm.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("Native".to_string(), e.to_string()))?;
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

        Ok(new_components)
    }

    /// Returns finality for any extractor, can error if lock is poisened. Returns None if buffer is
    /// empty.
    pub fn get_block_finality(
        &self,
        version: BlockNumberOrTimestamp,
    ) -> Result<Option<FinalityStatus>> {
        // TODO: This is a temporary hack, since finality status may be different depending on each
        //  individual extractor.
        let vm_first = self.vm.values().next();
        let native_first = self.native.values().next();

        match (native_first, vm_first) {
            (Some(first), _) => {
                let guard = first.lock().map_err(|e| {
                    PendingDeltasError::LockError("Native".to_string(), e.to_string())
                })?;
                Ok(guard.get_finality_status(version))
            }
            (_, Some(first)) => {
                let guard = first
                    .lock()
                    .map_err(|e| PendingDeltasError::LockError("VM".to_string(), e.to_string()))?;
                Ok(guard.get_finality_status(version))
            }
            _ => Ok(None),
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
            HashMap::new(),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0xbabe"),
            Bytes::from("0x4200"),
            Bytes::from("0x4200"),
            None,
        )
    }

    fn vm_block_deltas() -> BlockAccountChanges {
        let address = H160::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        BlockAccountChanges::new(
            "vm:extractor",
            Chain::Ethereum,
            evm_block(1),
            1,
            false,
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

    fn native_block_deltas() -> BlockEntityChangesResult {
        BlockEntityChangesResult::new(
            "native:extractor",
            Chain::Ethereum,
            evm_block(1),
            1,
            false,
            [evm::ProtocolStateDelta::new(
                "component1".to_string(),
                [("attr1", Bytes::from("0x01"))]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            )]
            .into_iter()
            .map(|v| (v.component_id.clone(), v))
            .collect(),
            HashMap::new(),
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
            [(
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
            )]
            .into_iter()
            .collect(),
            HashMap::new(),
        )
    }

    #[test]
    fn test_insert_vm() {
        let buffer = PendingDeltas::new(["vm:extractor"], []);
        let exp: VmBlockDeltas = (&vm_block_deltas()).into();

        buffer
            .insert(Arc::new(vm_block_deltas()))
            .expect("vm insert failed");

        let vm_revert_buffer = buffer
            .vm
            .get("vm:extractor")
            .expect("vm:extractor buffer missing");
        let binding = vm_revert_buffer.lock().unwrap();
        let res = binding
            .get_block_range(None, None)
            .expect("Failed to get block range")
            .collect::<Vec<_>>();
        assert_eq!(res[0], &exp);
    }

    #[test]
    fn test_insert_native() {
        let buffer = PendingDeltas::new([], ["native:extractor"]);
        let exp: NativeBlockDeltas = (&native_block_deltas()).into();

        buffer
            .insert(Arc::new(native_block_deltas()))
            .expect("native insert failed");

        let native_revert_buffer = buffer
            .native
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
    fn test_update_native_states() {
        let mut state = [native_state()];
        let buffer = PendingDeltas::new([], ["native:extractor"]);
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();
        let exp = ProtocolComponentState::new(
            "component1",
            [("attr2", Bytes::from("0x02")), ("attr1", Bytes::from("0x01"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x01"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );

        buffer
            .update_native_states(
                &mut state,
                Some(BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap())),
            )
            .unwrap();

        assert_eq!(&state[0], &exp);
    }

    #[test]
    fn test_update_vm_states() {
        let mut state = [vm_state()];
        let buffer = PendingDeltas::new(["vm:extractor"], []);
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
            HashMap::new(),
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
        let buffer = PendingDeltas::new(["vm:extractor"], ["native:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .unwrap();
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();

        let new_components = buffer.get_new_components(None).unwrap();

        assert_eq!(new_components, exp);

        let new_components = buffer
            .get_new_components(Some(&["component3"]))
            .unwrap();

        assert_eq!(new_components, vec![exp[0].clone()]);
    }
}
