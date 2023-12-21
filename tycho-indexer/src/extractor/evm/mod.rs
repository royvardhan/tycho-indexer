#![allow(dead_code)]

use std::{
    collections::{hash_map::Entry, HashMap},
    ops::Deref,
};

use chrono::NaiveDateTime;
use ethers::{
    types::{H160, H256, U256},
    utils::keccak256,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use utils::{pad_and_parse_32bytes, pad_and_parse_h160};

use crate::{
    hex_bytes::Bytes,
    models::{Chain, ExtractorIdentity, NormalisedMessage, ProtocolSystem},
    pb::tycho::evm::v1 as substreams,
    storage::{ChangeType, StateGatewayType},
};

use super::ExtractionError;

pub mod ambient;
pub mod storage;
mod utils;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SwapPool {}

pub struct ERC20Token {
    pub address: H160,
    pub symbol: String,
    pub decimals: u32,
    pub tax: u64,
    pub gas: Vec<Option<u64>>,
    pub chain: Chain,
}

impl ERC20Token {
    pub fn new(
        address: H160,
        symbol: String,
        decimals: u32,
        tax: u64,
        gas: Vec<Option<u64>>,
        chain: Chain,
    ) -> Self {
        ERC20Token { address, symbol, decimals, tax, gas, chain }
    }
}

#[derive(Debug, PartialEq, Copy, Clone, Deserialize, Serialize, Default)]
pub struct Block {
    pub number: u64,
    pub hash: H256,
    pub parent_hash: H256,
    pub chain: Chain,
    pub ts: NaiveDateTime,
}

#[derive(Debug, PartialEq, Copy, Clone, Default)]
pub struct Transaction {
    pub hash: H256,
    pub block_hash: H256,
    pub from: H160,
    pub to: Option<H160>,
    pub index: u64,
}

impl Transaction {
    pub fn new(hash: H256, block_hash: H256, from: H160, to: Option<H160>, index: u64) -> Self {
        Transaction { hash, block_hash, from, to, index }
    }
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub chain: Chain,
    pub address: H160,
    pub title: String,
    pub slots: HashMap<U256, U256>,
    pub balance: U256,
    pub code: Bytes,
    pub code_hash: H256,
    pub balance_modify_tx: H256,
    pub code_modify_tx: H256,
    pub creation_tx: Option<H256>,
}

impl Account {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        address: H160,
        title: String,
        slots: HashMap<U256, U256>,
        balance: U256,
        code: Bytes,
        code_hash: H256,
        balance_modify_tx: H256,
        code_modify_tx: H256,
        creation_tx: Option<H256>,
    ) -> Self {
        Self {
            chain,
            address,
            title,
            slots,
            balance,
            code,
            code_hash,
            balance_modify_tx,
            code_modify_tx,
            creation_tx,
        }
    }

    #[cfg(test)]
    pub fn set_balance(&mut self, new_balance: U256, modified_at: H256) {
        self.balance = new_balance;
        self.balance_modify_tx = modified_at;
    }
}

impl From<&AccountUpdateWithTx> for Account {
    /// Creates a full account from a change.
    ///
    /// This can be used to get an insertable an account if we know the update
    /// is actually a creation.
    ///
    /// Assumes that all relevant changes are set on `self` if something is
    /// missing, it will use the corresponding types default.
    /// Will use the associated transaction as creation, balance and code modify
    /// transaction.
    fn from(value: &AccountUpdateWithTx) -> Self {
        let empty_hash = H256::from(keccak256(Vec::new()));
        if value.change != ChangeType::Creation {
            warn!("Creating an account from a partial change!")
        }
        Account::new(
            value.chain,
            value.address,
            format!("{:#020x}", value.address),
            value.slots.clone(),
            value.balance.unwrap_or_default(),
            value.code.clone().unwrap_or_default(),
            value
                .code
                .as_ref()
                .map(|v| H256::from(keccak256(v)))
                .unwrap_or(empty_hash),
            value.tx.hash,
            value.tx.hash,
            Some(value.tx.hash),
        )
    }
}

#[derive(PartialEq, Serialize, Deserialize, Clone, Debug)]
pub struct AccountUpdate {
    pub address: H160,
    pub chain: Chain,
    pub slots: HashMap<U256, U256>,
    pub balance: Option<U256>,
    pub code: Option<Bytes>,
    pub change: ChangeType,
}

impl AccountUpdate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: H160,
        chain: Chain,
        slots: HashMap<U256, U256>,
        balance: Option<U256>,
        code: Option<Bytes>,
        change: ChangeType,
    ) -> Self {
        Self { address, chain, slots, balance, code, change }
    }

    /// Merge this update (`self`) with another one (`other`)
    ///
    /// This function is utilized for aggregating multiple updates into a single
    /// update. The attribute values of `other` are set on `self`.
    /// Meanwhile, contract storage maps are merged, in which keys from `other`
    /// take precedence.
    ///
    /// Be noted that, this function will mutate the state of the calling
    /// struct. An error will occur if merging updates from different accounts.
    ///
    /// There are no further validation checks within this method, hence it
    /// could be used as needed. However, you should give preference to
    /// utilizing [AccountUpdateWithTx] for merging, when possible.
    ///
    /// # Errors
    ///
    /// It returns an `ExtractionError::MergeError` error if `self.address` and
    /// `other.address` are not identical.
    ///
    /// # Arguments
    ///
    /// * `other`: An instance of `AccountUpdate`. The attribute values and keys
    /// of `other` will overwrite those of `self`.
    fn merge(&mut self, other: AccountUpdate) -> Result<(), ExtractionError> {
        if self.address != other.address {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge AccountUpdates from differing identities; Expected {:#020x}, got {:#020x}",
                self.address, other.address
            )));
        }

        self.slots.extend(other.slots);

        self.balance = other.balance.or(self.balance);
        self.code = other.code.or(self.code.take());

        Ok(())
    }

    #[allow(dead_code)]
    fn is_update(&self) -> bool {
        self.change == ChangeType::Update
    }

    fn is_creation(&self) -> bool {
        self.change == ChangeType::Creation
    }
}

/// A container for account updates grouped by account.
///
/// Hold a single update per account. This is a condensed form of
/// [BlockContractChanges].
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct BlockAccountChanges {
    extractor: String,
    chain: Chain,
    pub block: Block,
    pub account_updates: HashMap<H160, AccountUpdate>,
    pub new_protocol_components: Vec<ProtocolComponent>,
    pub deleted_protocol_components: Vec<ProtocolComponent>,
    pub tvl_changes: Vec<TvlChange>,
}

impl BlockAccountChanges {
    pub fn new(
        extractor: &str,
        chain: Chain,
        block: Block,
        account_updates: HashMap<H160, AccountUpdate>,
        new_protocol_components: Vec<ProtocolComponent>,
        deleted_protocol_components: Vec<ProtocolComponent>,
        tvl_change: Vec<TvlChange>,
    ) -> Self {
        BlockAccountChanges {
            extractor: extractor.to_owned(),
            chain,
            block,
            account_updates,
            new_protocol_components,
            deleted_protocol_components,
            tvl_changes: tvl_change,
        }
    }
}

impl std::fmt::Display for BlockAccountChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "block_number: {}, extractor: {}", self.block.number, self.extractor)
    }
}

#[typetag::serde]
impl NormalisedMessage for BlockAccountChanges {
    fn source(&self) -> ExtractorIdentity {
        ExtractorIdentity::new(self.chain, &self.extractor)
    }
}

/// Updates grouped by their respective transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUpdateWithTx {
    // TODO: for ambient it works to have only a single update here but long
    // term we need to be able to store changes to multiple accounts per
    // transactions.
    pub update: AccountUpdate,
    pub tx: Transaction,
}

impl AccountUpdateWithTx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: H160,
        chain: Chain,
        slots: HashMap<U256, U256>,
        balance: Option<U256>,
        code: Option<Bytes>,
        change: ChangeType,
        tx: Transaction,
    ) -> Self {
        Self { update: AccountUpdate { address, chain, slots, balance, code, change }, tx }
    }

    /// Merges this update with another one.
    ///
    /// The method combines two `AccountUpdateWithTx` instances under certain
    /// conditions:
    /// - The block from which both updates came should be the same. If the updates are from
    ///   different blocks, the method will return an error.
    /// - The transactions for each of the updates should be distinct. If they come from the same
    ///   transaction, the method will return an error.
    /// - The order of the transaction matters. The transaction from `other` must have occurred
    ///   later than the self transaction. If the self transaction has a higher index than `other`,
    ///   the method will return an error.
    ///
    /// The merged update keeps the transaction of `other`.
    ///
    /// # Errors
    /// This method will return `ExtractionError::MergeError` if any of the above
    /// conditions is violated.
    pub fn merge(&mut self, other: AccountUpdateWithTx) -> Result<(), ExtractionError> {
        if self.tx.block_hash != other.tx.block_hash {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge AccountUpdates from different blocks: 0x{:x} != 0x{:x}",
                self.tx.block_hash, other.tx.block_hash,
            )));
        }
        if self.tx.hash == other.tx.hash {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge AccountUpdates from the same transaction: 0x{:x}",
                self.tx.hash
            )));
        }
        if self.tx.index > other.tx.index {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge AccountUpdates with lower transaction index: {} > {}",
                self.tx.index, other.tx.index
            )));
        }
        self.tx = other.tx;
        self.update.merge(other.update)
    }
}

impl Deref for AccountUpdateWithTx {
    type Target = AccountUpdate;

    fn deref(&self) -> &Self::Target {
        &self.update
    }
}

/// A container for account updates grouped by transaction.
///
/// Hold the detailed state changes for a block alongside with protocol
/// component changes.
#[derive(Debug, PartialEq)]
pub struct BlockContractChanges {
    extractor: String,
    chain: Chain,
    pub block: Block,
    pub tx_updates: Vec<AccountUpdateWithTx>,
    pub protocol_components: Vec<ProtocolComponent>,
    pub tvl_changes: Vec<TvlChange>,
}

pub type EVMStateGateway<DB> =
    StateGatewayType<DB, Block, Transaction, Account, AccountUpdate, ERC20Token>;

impl Block {
    /// Parses block from tychos protobuf block message
    pub fn try_from_message(msg: substreams::Block, chain: Chain) -> Result<Self, ExtractionError> {
        Ok(Self {
            chain,
            number: msg.number,
            hash: pad_and_parse_32bytes(&msg.hash).map_err(ExtractionError::DecodeError)?,
            parent_hash: pad_and_parse_32bytes(&msg.parent_hash)
                .map_err(ExtractionError::DecodeError)?,
            ts: NaiveDateTime::from_timestamp_opt(msg.ts as i64, 0).ok_or_else(|| {
                ExtractionError::DecodeError(format!(
                    "Failed to convert timestamp {} to datetime!",
                    msg.ts
                ))
            })?,
        })
    }
}

impl Transaction {
    /// Parses transaction from tychos protobuf transaction message
    pub fn try_from_message(
        msg: substreams::Transaction,
        block_hash: &H256,
    ) -> Result<Self, ExtractionError> {
        let to = if !msg.to.is_empty() {
            Some(pad_and_parse_h160(&msg.to.into()).map_err(ExtractionError::DecodeError)?)
        } else {
            None
        };
        Ok(Self {
            hash: pad_and_parse_32bytes(&msg.hash).map_err(ExtractionError::DecodeError)?,
            block_hash: *block_hash,
            from: pad_and_parse_h160(&msg.from.into()).map_err(ExtractionError::DecodeError)?,
            to,
            index: msg.index,
        })
    }
}

impl AccountUpdateWithTx {
    /// Parses account update from tychos protobuf account update message
    pub fn try_from_message(
        msg: substreams::ContractChange,
        tx: &Transaction,
        chain: Chain,
    ) -> Result<Self, ExtractionError> {
        let change = msg.change().into();
        let update = AccountUpdateWithTx::new(
            pad_and_parse_h160(&msg.address.into()).map_err(ExtractionError::DecodeError)?,
            chain,
            msg.slots
                .into_iter()
                .map(|cs| {
                    Ok((
                        pad_and_parse_32bytes::<U256>(&cs.slot)
                            .map_err(ExtractionError::DecodeError)?,
                        pad_and_parse_32bytes::<U256>(&cs.value)
                            .map_err(ExtractionError::DecodeError)?,
                    ))
                })
                .collect::<Result<HashMap<_, _>, ExtractionError>>()?,
            if !msg.balance.is_empty() {
                Some(pad_and_parse_32bytes(&msg.balance).map_err(ExtractionError::DecodeError)?)
            } else {
                None
            },
            if !msg.code.is_empty() { Some(msg.code.into()) } else { None },
            change,
            *tx,
        );
        Ok(update)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TvlChange {
    token: H160,
    new_balance: f64,
    // tx where the this balance was observed
    modify_tx: H256,
    component_id: String,
}

impl TvlChange {
    pub fn try_from_message(
        msg: substreams::BalanceChange,
        tx: &Transaction,
    ) -> Result<Self, ExtractionError> {
        Ok(Self {
            token: pad_and_parse_h160(&msg.token.into()).map_err(ExtractionError::DecodeError)?,
            new_balance: f64::from_bits(u64::from_le_bytes(msg.balance.try_into().unwrap())),
            modify_tx: tx.hash,
            component_id: String::from_utf8(msg.component_id)
                .map_err(|error| ExtractionError::DecodeError(error.to_string()))?,
        })
    }
}

/// Represents the static parts of a protocol component.
///
/// `ProtocolComponent` provides detailed descriptions of the functionalities a protocol,
/// for example, swap pools that enables the exchange of two tokens.
///
/// A `ProtocolComponent` can be associated with an `Account`, and it has an identifier (`id`) that
/// can be either the on-chain address or a custom one. It belongs to a specific `ProtocolSystem`
/// and has a `ProtocolTypeID` that associates it with a `ProtocolType` that describes its behaviour
/// e.g., swap, lend, bridge. The component is associated with a specific `Chain` and holds
/// information about tradable tokens, related contract IDs, and static attributes.
///
/// A `ProtocolComponent` can have a one-to-one or one-to-many relationship with contracts.
/// For example, `UniswapV2` and `UniswapV3` have a one-to-one relationship one component (pool) one
/// contract, while `Ambient` has a one-to-many relationship with a single component and multiple
/// contracts.
///
/// The `ProtocolComponent` struct is designed to store static attributes related to the associated
/// smart contract.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct ProtocolComponent {
    // an id for this component, could be hex repr of contract address
    id: ContractId,
    // what system this component belongs to
    protocol_system: ProtocolSystem,
    // more metadata information about the components general type (swap, lend, bridge, etc.)
    protocol_type_id: String,
    // blockchain the component belongs to
    chain: Chain,
    // ids of the tokens tradable
    tokens: Vec<String>,
    // ids of the related contracts
    contract_ids: Vec<H160>,
    // stores the static attributes
    static_attributes: HashMap<String, Bytes>,
    // the type of change (creation, deletion etc)
    change: ChangeType,
}

/// A type representing the unique identifier for a contract. It can represent an on-chain address
/// or in the case of a one-to-many relationship it could be something like 'USDC-ETH'. This is for
/// example the case with ambient, where one component is responsible for multiple contracts.
///
/// `ContractId` is a simple wrapper around a `String` to ensure type safety
/// and clarity when working with contract identifiers.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct ContractId(pub String);

impl ProtocolComponent {
    pub fn try_from_message(
        msg: substreams::ProtocolComponent,
        chain: Chain,
        protocol_system: ProtocolSystem,
        protocol_type_id: String,
    ) -> Result<Self, ExtractionError> {
        let id = ContractId(msg.id.clone());

        let tokens = msg
            .tokens
            .clone()
            .into_iter()
            .map(|t| {
                String::from_utf8(t)
                    .map_err(|error| ExtractionError::DecodeError(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let contract_ids = msg
            .contracts
            .clone()
            .into_iter()
            .map(|c| pad_and_parse_h160(&c.into()).map_err(ExtractionError::DecodeError))
            .collect::<Result<Vec<_>, ExtractionError>>()?;
        print!("{:?}", contract_ids);

        let static_attributes = msg
            .static_att
            .clone()
            .into_iter()
            .map(|attribute| Ok((attribute.name, Bytes::from(attribute.value))))
            .collect::<Result<HashMap<_, _>, ExtractionError>>()?;

        let t = Self {
            id,
            protocol_type_id,
            protocol_system,
            tokens,
            contract_ids,
            static_attributes,
            chain,
            change: msg.change().into(),
        };
        print!("{:?}", t);
        Ok(t)
    }
}

impl From<substreams::ChangeType> for ChangeType {
    fn from(value: substreams::ChangeType) -> Self {
        match value {
            substreams::ChangeType::Unspecified => {
                panic!("Unkown enum member encountered: {:?}", value)
            }
            substreams::ChangeType::Update => ChangeType::Update,
            substreams::ChangeType::Creation => ChangeType::Creation,
            substreams::ChangeType::Deletion => ChangeType::Deletion,
        }
    }
}

impl BlockContractChanges {
    /// Parse from tychos protobuf message
    pub fn try_from_message(
        msg: substreams::BlockContractChanges,
        extractor: &str,
        chain: Chain,
        protocol_system: ProtocolSystem,
        protocol_type_id: String,
    ) -> Result<Self, ExtractionError> {
        if let Some(block) = msg.block {
            let block = Block::try_from_message(block, chain)?;
            let mut tx_updates = Vec::new();
            let mut protocol_components = Vec::new();

            for change in msg.changes.into_iter() {
                if let Some(tx) = change.tx {
                    let tx = Transaction::try_from_message(tx, &block.hash)?;
                    for el in change.contract_changes.into_iter() {
                        let update = AccountUpdateWithTx::try_from_message(el, &tx, chain)?;
                        tx_updates.push(update);
                    }
                    for component_msg in change.component_changes.into_iter() {
                        let component = ProtocolComponent::try_from_message(
                            component_msg,
                            chain,
                            protocol_system,
                            protocol_type_id.clone(),
                        )?;
                        protocol_components.push(component);
                    }
                }
            }
            tx_updates.sort_unstable_by_key(|update| update.tx.index);
            return Ok(Self {
                extractor: extractor.to_owned(),
                chain,
                block,
                tx_updates,
                protocol_components,
                tvl_changes: Vec::new(),
            });
        }
        Err(ExtractionError::Empty)
    }

    /// Aggregates transaction updates.
    ///
    /// This function aggregates the transaction updates (`tx_updates`) from
    /// different accounts into a single object of  
    /// `BlockAccountChanges`. It maintains a HashMap to hold
    /// `AccountUpdate` corresponding to each unique address.
    ///
    /// If the address from an update is already present in the HashMap, it
    /// merges the update with the existing one. Otherwise, it inserts the new
    /// update into the HashMap.
    ///
    /// After merging all updates, a `BlockAccountChanges` object is returned
    /// which contains, amongst other data, the compacted account updates.
    ///
    /// # Errors
    ///
    /// This returns an error if there was a problem during merge. The error
    /// type is `ExtractionError`.
    pub fn aggregate_updates(self) -> Result<BlockAccountChanges, ExtractionError> {
        let mut account_updates: HashMap<H160, AccountUpdateWithTx> = HashMap::new();

        for update in self.tx_updates.into_iter() {
            match account_updates.entry(update.address) {
                Entry::Occupied(mut e) => {
                    e.get_mut().merge(update)?;
                }
                Entry::Vacant(e) => {
                    e.insert(update);
                }
            }
        }

        Ok(BlockAccountChanges::new(
            &self.extractor,
            self.chain,
            self.block,
            account_updates
                .into_iter()
                .map(|(k, v)| (k, v.update))
                .collect(),
            self.protocol_components,
            Vec::new(),
            Vec::new(),
        ))
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
/// Represents the dynamic data of `ProtocolComponent`.
pub struct ProtocolState {
    // associates back to a component, which has metadata like type, tokens, etc.
    pub component_id: String,
    // the update protocol specific attributes, validated by the components schema
    pub updated_attributes: HashMap<String, Bytes>,
    // the deleted protocol specific attributes
    pub deleted_attributes: HashMap<String, Bytes>,
    // via transaction, we can trace back when this state became valid
    pub modify_tx: H256,
}

// TODO: remove dead code check skip once extractor is implemented
#[allow(dead_code)]
impl ProtocolState {
    /// Parses protocol state from tychos protobuf EntityChanges message
    pub fn try_from_message(
        msg: substreams::EntityChanges,
        tx: &Transaction,
    ) -> Result<Self, ExtractionError> {
        let (mut updates, mut deletions) = (HashMap::new(), HashMap::new());

        for attribute in msg.attributes.into_iter() {
            match attribute.change().into() {
                ChangeType::Update | ChangeType::Creation => {
                    updates.insert(attribute.name, Bytes::from(attribute.value));
                }
                ChangeType::Deletion => {
                    deletions.insert(attribute.name, Bytes::from(attribute.value));
                }
            }
        }

        Ok(Self {
            component_id: msg.component_id,
            updated_attributes: updates,
            deleted_attributes: deletions,
            modify_tx: tx.hash,
        })
    }

    /// Merges this update with another one.
    ///
    /// The method combines two `ProtocolState` instances if they are for the same
    /// protocol component.
    ///
    /// The merged update keeps the transaction of `other`.
    ///
    /// # Errors
    /// This method will return `ExtractionError::MergeError` if any of the above
    /// conditions is violated.
    pub fn merge(&mut self, other: ProtocolState) -> Result<(), ExtractionError> {
        if self.component_id != other.component_id {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge ProtocolStates from differing identities; Expected {}, got {}",
                self.component_id, other.component_id
            )));
        }
        self.modify_tx = other.modify_tx;
        for attr in other.deleted_attributes.keys() {
            self.updated_attributes.remove(attr);
        }
        for attr in other.updated_attributes.keys() {
            self.deleted_attributes.remove(attr);
        }
        self.updated_attributes
            .extend(other.updated_attributes);
        self.deleted_attributes
            .extend(other.deleted_attributes);
        Ok(())
    }
}

/// Updates grouped by their respective transaction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProtocolStatesWithTx {
    pub protocol_states: HashMap<String, ProtocolState>,
    pub tx: Transaction,
}

impl ProtocolStatesWithTx {
    /// Parses protocol state from tychos protobuf EntityChanges message
    pub fn try_from_message(
        msg: Vec<substreams::EntityChanges>,
        tx: Transaction,
    ) -> Result<Self, ExtractionError> {
        let mut protocol_states = HashMap::new();
        for state_msg in msg {
            let state = ProtocolState::try_from_message(state_msg, &tx)?;
            protocol_states.insert(state.clone().component_id, state);
        }
        Ok(Self { protocol_states, tx })
    }

    /// Merges this update with another one.
    ///
    /// The method combines two `ProtocolStatesWithTx` instances under certain
    /// conditions:
    /// - The block from which both updates came should be the same. If the updates are from
    ///   different blocks, the method will return an error.
    /// - The transactions for each of the updates should be distinct. If they come from the same
    ///   transaction, the method will return an error.
    /// - The order of the transaction matters. The transaction from `other` must have occurred
    ///   later than the self transaction. If the self transaction has a higher index than `other`,
    ///   the method will return an error.
    ///
    /// The merged update keeps the transaction of `other`.
    ///
    /// # Errors
    /// This method will return `ExtractionError::MergeError` if any of the above
    /// conditions is violated.
    pub fn merge(&mut self, other: ProtocolStatesWithTx) -> Result<(), ExtractionError> {
        if self.tx.block_hash != other.tx.block_hash {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge ProtocolStates from different blocks: 0x{:x} != 0x{:x}",
                self.tx.block_hash, other.tx.block_hash,
            )));
        }
        if self.tx.hash == other.tx.hash {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge ProtocolStates from the same transaction: 0x{:x}",
                self.tx.hash
            )));
        }
        if self.tx.index > other.tx.index {
            return Err(ExtractionError::MergeError(format!(
                "Can't merge ProtocolStates with lower transaction index: {} > {}",
                self.tx.index, other.tx.index
            )));
        }
        self.tx = other.tx;
        for (key, value) in other.protocol_states {
            match self.protocol_states.entry(key) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().merge(value)?;
                }
                Entry::Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
        Ok(())
    }
}

/// A container for state updates grouped by protocol component.
///
/// Hold a single update per component. This is a condensed form of
/// [BlockEntityChanges].
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct BlockEntityChangesResult {
    extractor: String,
    chain: Chain,
    pub block: Block,
    pub state_updates: HashMap<String, ProtocolState>,
    pub new_protocol_components: HashMap<String, ProtocolComponent>,
}

/// A container for state updates grouped by transaction
///
/// Hold the detailed state changes for a block alongside with protocol
/// component changes.
#[derive(Debug, PartialEq)]
pub struct BlockEntityChanges {
    extractor: String,
    chain: Chain,
    pub block: Block,
    pub state_updates: Vec<ProtocolStatesWithTx>,
    pub new_protocol_components: HashMap<String, ProtocolComponent>,
}

// TODO: remove dead code check skip once extractor is implemented
#[allow(dead_code)]
impl BlockEntityChanges {
    /// Parse from tychos protobuf message
    pub fn try_from_message(
        msg: substreams::BlockEntityChanges,
        extractor: &str,
        chain: Chain,
        protocol_system: ProtocolSystem,
        protocol_type_id: String,
    ) -> Result<Self, ExtractionError> {
        if let Some(block) = msg.block {
            let block = Block::try_from_message(block, chain)?;
            let mut state_updates = Vec::new();
            let mut new_protocol_components = HashMap::new();

            for change in msg.changes.into_iter() {
                if let Some(tx) = change.tx {
                    let tx = Transaction::try_from_message(tx, &block.hash)?;
                    let tx_update =
                        ProtocolStatesWithTx::try_from_message(change.entity_changes, tx)?;
                    state_updates.push(tx_update);
                    for component in change.component_changes {
                        let pool = ProtocolComponent::try_from_message(
                            component,
                            chain,
                            protocol_system,
                            protocol_type_id.clone(),
                        )?;
                        new_protocol_components.insert(pool.clone().id.0, pool);
                    }
                }
            }

            state_updates.sort_unstable_by_key(|update| update.tx.index);
            return Ok(Self {
                extractor: extractor.to_owned(),
                chain,
                block,
                state_updates,
                new_protocol_components,
            });
        }
        Err(ExtractionError::Empty)
    }

    /// Aggregates state updates.
    ///
    /// This function aggregates the state updates (`ProtocolState`) for
    /// different protocol components into a `BlockEntityChangesResult` object.
    /// This new object should have only one final ProtocolState per component_id.
    ///
    /// After merging all updates, a `BlockEntityChangesResult` object is returned
    /// which contains, amongst other data, the compacted state updates.
    ///
    /// # Errors
    ///
    /// This returns an error if there was a problem during merge. The error
    /// type is `ExtractionError`.
    pub fn aggregate_updates(self) -> Result<BlockEntityChangesResult, ExtractionError> {
        let base = ProtocolStatesWithTx::default();

        let aggregated_states = self
            .state_updates
            .iter()
            .try_fold(base, |mut acc_state, new_state| {
                acc_state.merge(new_state.clone())?;
                Ok::<_, ExtractionError>(acc_state.clone())
            })
            .unwrap();

        Ok(BlockEntityChangesResult {
            extractor: self.extractor,
            chain: self.chain,
            block: self.block,
            state_updates: aggregated_states.protocol_states,
            new_protocol_components: self.new_protocol_components,
        })
    }
}

#[cfg(test)]
pub mod fixtures {
    use ethers::abi::AbiEncode;
    use prost::Message;
    use std::str::FromStr;

    use super::*;

    pub const HASH_256_0: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000000";

    pub fn transaction01() -> Transaction {
        Transaction::new(H256::zero(), H256::zero(), H160::zero(), Some(H160::zero()), 10)
    }

    pub fn transaction02(hash: &str, block: &str, index: u64) -> Transaction {
        Transaction::new(
            hash.parse().unwrap(),
            block.parse().unwrap(),
            H160::zero(),
            Some(H160::zero()),
            index,
        )
    }

    pub fn evm_slots(data: impl IntoIterator<Item = (u64, u64)>) -> HashMap<U256, U256> {
        data.into_iter()
            .map(|(s, v)| (U256::from(s), U256::from(v)))
            .collect()
    }

    pub fn pb_block_scoped_data(
        msg: impl prost::Message,
    ) -> crate::pb::sf::substreams::rpc::v2::BlockScopedData {
        use crate::pb::sf::substreams::{rpc::v2::*, v1::Clock};
        let val = msg.encode_to_vec();
        BlockScopedData {
            output: Some(MapModuleOutput {
                name: "map_changes".to_owned(),
                map_output: Some(prost_types::Any {
                    type_url: "tycho.evm.v1.BlockContractChanges".to_owned(),
                    value: val,
                }),
                debug_info: None,
            }),
            clock: Some(Clock {
                id: HASH_256_0.to_owned(),
                number: 420,
                timestamp: Some(prost_types::Timestamp { seconds: 1000, nanos: 0 }),
            }),
            cursor: "cursor@420".to_owned(),
            final_block_height: 405,
            debug_map_outputs: vec![],
            debug_store_outputs: vec![],
        }
    }

    pub fn pb_block_contract_changes() -> crate::pb::tycho::evm::v1::BlockContractChanges {
        use crate::pb::tycho::evm::v1::*;
        BlockContractChanges {
            block: Some(Block {
                hash: vec![0x31, 0x32, 0x33, 0x34],
                parent_hash: vec![0x21, 0x22, 0x23, 0x24],
                number: 1,
                ts: 1000,
            }),

            changes: vec![TransactionContractChanges {
                tx: Some(Transaction {
                    hash: vec![0x11, 0x12, 0x13, 0x14],
                    from: vec![0x41, 0x42, 0x43, 0x44],
                    to: vec![0x51, 0x52, 0x53, 0x54],
                    index: 2,
                }),
                contract_changes: vec![
                    ContractChange {
                        address: vec![0x61, 0x62, 0x63, 0x64],
                        balance: vec![0x71, 0x72, 0x73, 0x74],
                        code: vec![0x81, 0x82, 0x83, 0x84],
                        slots: vec![
                            ContractSlot {
                                slot: vec![0xa1, 0xa2, 0xa3, 0xa4],
                                value: vec![0xb1, 0xb2, 0xb3, 0xb4],
                            },
                            ContractSlot {
                                slot: vec![0xc1, 0xc2, 0xc3, 0xc4],
                                value: vec![0xd1, 0xd2, 0xd3, 0xd4],
                            },
                        ],
                        change: ChangeType::Update.into(),
                    },
                    ContractChange {
                        address: vec![0x61, 0x62, 0x63, 0x64],
                        balance: vec![0xf1, 0xf2, 0xf3, 0xf4],
                        code: vec![0x01, 0x02, 0x03, 0x04],
                        slots: vec![
                            ContractSlot {
                                slot: vec![0x91, 0x92, 0x93, 0x94],
                                value: vec![0xa1, 0xa2, 0xa3, 0xa4],
                            },
                            ContractSlot {
                                slot: vec![0xb1, 0xb2, 0xb3, 0xb4],
                                value: vec![0xc1, 0xc2, 0xc3, 0xc4],
                            },
                        ],
                        change: ChangeType::Update.into(),
                    },
                ],
                component_changes: vec![ProtocolComponent {
                    id: "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688".to_owned(),
                    tokens: vec![b"token1".to_vec(), b"token2".to_vec()],
                    contracts: vec![
                        H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
                            .unwrap()
                            .0
                            .to_vec(),
                        H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
                            .unwrap()
                            .0
                            .to_vec(),
                    ],
                    static_att: vec![
                        Attribute {
                            name: "key1".to_owned(),
                            value: b"value1".to_vec(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "key2".to_owned(),
                            value: b"value2".to_vec(),
                            change: ChangeType::Creation.into(),
                        },
                    ],
                    change: ChangeType::Creation.into(),
                }],
                balance_changes: vec![BalanceChange {
                    token: hex::decode(
                        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".trim_start_matches("0x"),
                    )
                    .unwrap(),
                    balance: 50000000.encode_to_vec(),
                    component_id: "WETH-CAI".encode(),
                }],
            }],
        }
    }

    pub fn pb_state_changes() -> crate::pb::tycho::evm::v1::EntityChanges {
        use crate::pb::tycho::evm::v1::*;
        let res1_value = 1000_u64.to_be_bytes().to_vec();
        let res2_value = 500_u64.to_be_bytes().to_vec();
        EntityChanges {
            component_id: "State1".to_owned(),
            attributes: vec![
                Attribute {
                    name: "reserve1".to_owned(),
                    value: res1_value,
                    change: ChangeType::Update.into(),
                },
                Attribute {
                    name: "reserve2".to_owned(),
                    value: res2_value,
                    change: ChangeType::Update.into(),
                },
            ],
        }
    }

    pub fn pb_block_entity_changes() -> crate::pb::tycho::evm::v1::BlockEntityChanges {
        use crate::pb::tycho::evm::v1::*;
        BlockEntityChanges {
            block: Some(Block {
                hash: vec![0x0, 0x0, 0x0, 0x0],
                parent_hash: vec![0x21, 0x22, 0x23, 0x24],
                number: 1,
                ts: 1000,
            }),
            changes: vec![
                TransactionEntityChanges {
                    tx: Some(Transaction {
                        hash: vec![0x0, 0x0, 0x0, 0x0],
                        from: vec![0x0, 0x0, 0x0, 0x0],
                        to: vec![0x0, 0x0, 0x0, 0x0],
                        index: 10,
                    }),
                    entity_changes: vec![
                        EntityChanges {
                            component_id: "State1".to_owned(),
                            attributes: vec![
                                Attribute {
                                    name: "reserve".to_owned(),
                                    value: 1000_u64.to_be_bytes().to_vec(),
                                    change: ChangeType::Update.into(),
                                },
                                Attribute {
                                    name: "static_attribute".to_owned(),
                                    value: 1_u64.to_be_bytes().to_vec(),
                                    change: ChangeType::Update.into(),
                                },
                            ],
                        },
                        EntityChanges {
                            component_id: "State2".to_owned(),
                            attributes: vec![
                                Attribute {
                                    name: "reserve".to_owned(),
                                    value: 1000_u64.to_be_bytes().to_vec(),
                                    change: ChangeType::Update.into(),
                                },
                                Attribute {
                                    name: "static_attribute".to_owned(),
                                    value: 1_u64.to_be_bytes().to_vec(),
                                    change: ChangeType::Update.into(),
                                },
                            ],
                        },
                    ],
                    component_changes: vec![ProtocolComponent {
                        id: "Pool".to_owned(),
                        tokens: vec![
                            "token0".to_owned().into_bytes(),
                            "token1".to_owned().into_bytes(),
                        ],
                        contracts: vec![H160::from_str(
                            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                        )
                        .unwrap()
                        .0
                        .to_vec()],
                        static_att: vec![Attribute {
                            name: "key".to_owned(),
                            value: 600_u64.to_be_bytes().to_vec(),
                            change: ChangeType::Creation.into(),
                        }],
                        change: ChangeType::Creation.into(),
                    }],
                    balance_changes: vec![],
                },
                TransactionEntityChanges {
                    tx: Some(Transaction {
                        hash: vec![0x11, 0x12, 0x13, 0x14],
                        from: vec![0x41, 0x42, 0x43, 0x44],
                        to: vec![0x51, 0x52, 0x53, 0x54],
                        index: 11,
                    }),
                    entity_changes: vec![EntityChanges {
                        component_id: "State1".to_owned(),
                        attributes: vec![
                            Attribute {
                                name: "reserve".to_owned(),
                                value: 600_u64.to_be_bytes().to_vec(),
                                change: ChangeType::Update.into(),
                            },
                            Attribute {
                                name: "new".to_owned(),
                                value: 0_u64.to_be_bytes().to_vec(),
                                change: ChangeType::Update.into(),
                            },
                        ],
                    }],
                    component_changes: vec![],
                    balance_changes: vec![],
                },
            ],
        }
    }

    pub fn pb_protocol_component() -> crate::pb::tycho::evm::v1::ProtocolComponent {
        use crate::pb::tycho::evm::v1::*;
        ProtocolComponent {
            id: "component_id".to_owned(),
            tokens: vec![b"token1".to_vec(), b"token2".to_vec()],
            contracts: vec![
                H160::from_str("0x31fF2589Ee5275a2038beB855F44b9Be993aA804")
                    .unwrap()
                    .0
                    .to_vec(),
                H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
                    .unwrap()
                    .0
                    .to_vec(),
            ],
            static_att: vec![
                Attribute {
                    name: "balance".to_owned(),
                    value: 100_u64.to_be_bytes().to_vec(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "factory_address".to_owned(),
                    value: b"0x0fwe0g240g20".to_vec(),
                    change: ChangeType::Creation.into(),
                },
            ],
            change: ChangeType::Creation.into(),
        }
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use actix_web::body::MessageBody;
    use rstest::rstest;

    use crate::{extractor::evm::fixtures::transaction01, models::ProtocolSystem};

    use super::*;

    const HASH_256_0: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
    const HASH_256_1: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

    fn account01() -> Account {
        let code = vec![0, 0, 0, 0];
        let code_hash = H256(keccak256(&code));
        Account::new(
            Chain::Ethereum,
            "0xe688b84b23f322a994A53dbF8E15FA82CDB71127"
                .parse()
                .unwrap(),
            "0xe688b84b23f322a994a53dbf8e15fa82cdb71127".into(),
            fixtures::evm_slots([]),
            U256::from(10000),
            code.into(),
            code_hash,
            H256::zero(),
            H256::zero(),
            Some(H256::zero()),
        )
    }

    fn update_w_tx() -> AccountUpdateWithTx {
        let code = vec![0, 0, 0, 0];
        AccountUpdateWithTx::new(
            "0xe688b84b23f322a994A53dbF8E15FA82CDB71127"
                .parse()
                .unwrap(),
            Chain::Ethereum,
            fixtures::evm_slots([]),
            Some(U256::from(10000)),
            Some(code.into()),
            ChangeType::Update,
            fixtures::transaction01(),
        )
    }

    fn update_balance() -> AccountUpdate {
        AccountUpdate::new(
            "0xe688b84b23f322a994A53dbF8E15FA82CDB71127"
                .parse()
                .unwrap(),
            Chain::Ethereum,
            fixtures::evm_slots([]),
            Some(U256::from(420)),
            None,
            ChangeType::Update,
        )
    }

    fn update_slots() -> AccountUpdate {
        AccountUpdate::new(
            "0xe688b84b23f322a994A53dbF8E15FA82CDB71127"
                .parse()
                .unwrap(),
            Chain::Ethereum,
            fixtures::evm_slots([(0, 1), (1, 2)]),
            None,
            None,
            ChangeType::Update,
        )
    }

    #[test]
    fn test_account_from_update_w_tx() {
        let update = update_w_tx();
        let exp = account01();

        assert_eq!(Account::from(&update), exp);
    }

    #[test]
    fn test_merge_account_update() {
        let mut update_left = update_balance();
        let update_right = update_slots();
        let mut exp = update_slots();
        exp.balance = Some(U256::from(420));

        update_left.merge(update_right).unwrap();

        assert_eq!(update_left, exp);
    }

    #[test]
    fn test_merge_account_update_wrong_address() {
        let mut update_left = update_balance();
        let mut update_right = update_slots();
        update_right.address = H160::zero();
        let exp = Err(ExtractionError::MergeError(
            "Can't merge AccountUpdates from differing identities; \
            Expected 0xe688b84b23f322a994a53dbf8e15fa82cdb71127, \
            got 0x0000000000000000000000000000000000000000"
                .into(),
        ));

        let res = update_left.merge(update_right);

        assert_eq!(res, exp);
    }

    #[rstest]
    #[case::diff_block(
    fixtures::transaction02(HASH_256_1, HASH_256_1, 11),
    Err(ExtractionError::MergeError(format ! ("Can't merge AccountUpdates from different blocks: 0x{:x} != {}", H256::zero(), HASH_256_1)))
    )]
    #[case::same_tx(
    fixtures::transaction02(HASH_256_0, HASH_256_0, 11),
    Err(ExtractionError::MergeError(format ! ("Can't merge AccountUpdates from the same transaction: 0x{:x}", H256::zero())))
    )]
    #[case::lower_idx(
    fixtures::transaction02(HASH_256_1, HASH_256_0, 1),
    Err(ExtractionError::MergeError("Can't merge AccountUpdates with lower transaction index: 10 > 1".to_owned()))
    )]
    fn test_merge_account_update_w_tx(
        #[case] tx: Transaction,
        #[case] exp: Result<(), ExtractionError>,
    ) {
        let mut left = update_w_tx();
        let mut right = left.clone();
        right.tx = tx;

        let res = left.merge(right);

        assert_eq!(res, exp);
    }

    fn block_state_changes() -> BlockContractChanges {
        let tx = Transaction {
            hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000011121314,
            ),
            block_hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000031323334,
            ),
            from: H160::from_low_u64_be(0x0000000000000000000000000000000041424344),
            to: Some(H160::from_low_u64_be(0x0000000000000000000000000000000051525354)),
            index: 2,
        };
        let protocol_component = ProtocolComponent {
            id: ContractId("0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688".to_owned()),
            protocol_system: ProtocolSystem::Ambient,
            protocol_type_id: String::from("id-1"),
            chain: Chain::Ethereum,
            tokens: vec!["token1".to_string(), "token2".to_string()],
            contract_ids: vec![
                H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            ],
            static_attributes: HashMap::from([
                ("key1".to_string(), Bytes::from(b"value1".to_vec())),
                ("key2".to_string(), Bytes::from(b"value2".to_vec())),
            ]),
            change: ChangeType::Creation,
        };
        BlockContractChanges {
            extractor: "test".to_string(),
            chain: Chain::Ethereum,
            block: Block {
                number: 1,
                hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000031323334,
                ),
                parent_hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000021222324,
                ),
                chain: Chain::Ethereum,
                ts: NaiveDateTime::from_timestamp_opt(1000, 0).unwrap(),
            },
            tx_updates: vec![
                AccountUpdateWithTx {
                    update: AccountUpdate {
                        address: H160::from_low_u64_be(0x0000000000000000000000000000000061626364),
                        chain: Chain::Ethereum,
                        slots: fixtures::evm_slots([
                            (2711790500, 2981278644),
                            (3250766788, 3520254932),
                        ]),
                        balance: Some(U256::from(1903326068)),
                        code: Some(vec![129, 130, 131, 132].into()),
                        change: ChangeType::Update,
                    },
                    tx,
                },
                AccountUpdateWithTx {
                    update: AccountUpdate {
                        address: H160::from_low_u64_be(0x0000000000000000000000000000000061626364),
                        chain: Chain::Ethereum,
                        slots: fixtures::evm_slots([
                            (2981278644, 3250766788),
                            (2442302356, 2711790500),
                        ]),
                        balance: Some(U256::from(4059231220u64)),
                        code: Some(vec![1, 2, 3, 4].into()),
                        change: ChangeType::Update,
                    },
                    tx,
                },
            ],
            protocol_components: vec![protocol_component],
            tvl_changes: Vec::new(),
        }
    }

    #[test]
    fn test_block_state_changes_parse_msg() {
        let msg = fixtures::pb_block_contract_changes();

        let res = BlockContractChanges::try_from_message(
            msg,
            "test",
            Chain::Ethereum,
            ProtocolSystem::Ambient,
            String::from("id-1"),
        )
        .unwrap();
        assert_eq!(res, block_state_changes());
    }

    fn block_account_changes() -> BlockAccountChanges {
        let address = H160::from_low_u64_be(0x0000000000000000000000000000000061626364);
        let protocol_component = ProtocolComponent {
            id: ContractId("0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688".to_owned()),
            protocol_system: ProtocolSystem::Ambient,
            protocol_type_id: String::from("id-1"),
            chain: Chain::Ethereum,
            tokens: vec!["token1".to_string(), "token2".to_string()],
            contract_ids: vec![
                H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            ],
            static_attributes: [
                ("key1".to_string(), Bytes::from(b"value1".to_vec())),
                ("key2".to_string(), Bytes::from(b"value2".to_vec())),
            ]
            .iter()
            .cloned()
            .collect(),
            change: ChangeType::Creation,
        };
        BlockAccountChanges::new(
            "test",
            Chain::Ethereum,
            Block {
                number: 1,
                hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000031323334,
                ),
                parent_hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000021222324,
                ),
                chain: Chain::Ethereum,
                ts: NaiveDateTime::from_timestamp_opt(1000, 0).unwrap(),
            },
            vec![(
                address,
                AccountUpdate {
                    address: H160::from_low_u64_be(0x0000000000000000000000000000000061626364),
                    chain: Chain::Ethereum,
                    slots: fixtures::evm_slots([
                        (2711790500, 2981278644),
                        (3250766788, 3520254932),
                        (2981278644, 3250766788),
                        (2442302356, 2711790500),
                    ]),
                    balance: Some(U256::from(4059231220u64)),
                    code: Some(vec![1, 2, 3, 4].into()),
                    change: ChangeType::Update,
                },
            )]
            .into_iter()
            .collect(),
            vec![protocol_component],
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn test_block_state_changes_aggregate() {
        let mut msg = block_state_changes();
        let block_hash = "0x0000000000000000000000000000000000000000000000000000000031323334";
        // use a different tx so merge works
        msg.tx_updates[1].tx = fixtures::transaction02(HASH_256_1, block_hash, 5);

        // should error cause same tx
        let res = msg.aggregate_updates().unwrap();

        assert_eq!(res, block_account_changes());
    }

    #[test]
    fn test_merge_protocol_state() {
        let up_attributes1: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(1000))),
            ("reserve2".to_owned(), Bytes::from(U256::from(500))),
            ("static_attribute".to_owned(), Bytes::from(U256::from(1))),
            ("to_be_removed".to_owned(), Bytes::from(U256::from(1))),
        ]
        .into_iter()
        .collect();
        let del_attributes1: HashMap<String, Bytes> =
            vec![("to_add_back".to_owned(), Bytes::from(U256::from(0)))]
                .into_iter()
                .collect();
        let mut state1 = ProtocolState {
            component_id: "State1".to_owned(),
            updated_attributes: up_attributes1,
            deleted_attributes: del_attributes1,
            modify_tx: H256::zero(),
        };

        let up_attributes2: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(900))),
            ("reserve2".to_owned(), Bytes::from(U256::from(550))),
            ("new_attribute".to_owned(), Bytes::from(U256::from(1))),
            ("to_add_back".to_owned(), Bytes::from(U256::from(200))),
        ]
        .into_iter()
        .collect();
        let del_attributes2: HashMap<String, Bytes> =
            vec![("to_be_removed".to_owned(), Bytes::from(U256::from(0)))]
                .into_iter()
                .collect();
        let state2 = ProtocolState {
            component_id: "State1".to_owned(),
            updated_attributes: up_attributes2.clone(),
            deleted_attributes: del_attributes2,
            modify_tx: HASH_256_1.parse().unwrap(),
        };

        let res = state1.merge(state2);

        assert!(res.is_ok());
        let expected_up_attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(900))),
            ("reserve2".to_owned(), Bytes::from(U256::from(550))),
            ("static_attribute".to_owned(), Bytes::from(U256::from(1))),
            ("new_attribute".to_owned(), Bytes::from(U256::from(1))),
            ("to_add_back".to_owned(), Bytes::from(U256::from(200))),
        ]
        .into_iter()
        .collect();
        assert_eq!(state1.updated_attributes, expected_up_attributes);
        let expected_del_attributes: HashMap<String, Bytes> =
            vec![("to_be_removed".to_owned(), Bytes::from(U256::from(0)))]
                .into_iter()
                .collect();
        assert_eq!(state1.deleted_attributes, expected_del_attributes);
    }

    fn protocol_state_with_tx() -> ProtocolStatesWithTx {
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve".to_owned(), Bytes::from(1000_u64.to_be_bytes().to_vec())),
            ("static_attribute".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let states: HashMap<String, ProtocolState> = vec![
            (
                "State1".to_owned(),
                ProtocolState {
                    component_id: "State1".to_owned(),
                    updated_attributes: attributes.clone(),
                    deleted_attributes: HashMap::new(),
                    modify_tx: H256::zero(),
                },
            ),
            (
                "State2".to_owned(),
                ProtocolState {
                    component_id: "State2".to_owned(),
                    updated_attributes: attributes,
                    deleted_attributes: HashMap::new(),
                    modify_tx: H256::zero(),
                },
            ),
        ]
        .into_iter()
        .collect();
        ProtocolStatesWithTx { protocol_states: states, tx: transaction01() }
    }

    #[test]
    fn test_merge_protocol_state_with_tx() {
        let mut base_state = protocol_state_with_tx();

        let new_attributes: HashMap<String, Bytes> = vec![
            ("reserve".to_owned(), Bytes::from(900_u64.to_be_bytes().to_vec())),
            ("new_attribute".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let new_tx = fixtures::transaction02(HASH_256_1, HASH_256_0, 11);
        let new_states: HashMap<String, ProtocolState> = vec![(
            "State1".to_owned(),
            ProtocolState {
                component_id: "State1".to_owned(),
                updated_attributes: new_attributes,
                deleted_attributes: HashMap::new(),
                modify_tx: new_tx.hash,
            },
        )]
        .into_iter()
        .collect();

        let tx_update = ProtocolStatesWithTx { protocol_states: new_states, tx: new_tx };

        let res = base_state.merge(tx_update);

        assert!(res.is_ok());
        assert_eq!(base_state.protocol_states.len(), 2);
        let expected_attributes: HashMap<String, Bytes> = vec![
            ("reserve".to_owned(), Bytes::from(900_u64.to_be_bytes().to_vec())),
            ("static_attribute".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
            ("new_attribute".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            base_state
                .protocol_states
                .get("State1")
                .unwrap()
                .updated_attributes,
            expected_attributes
        );
    }

    #[rstest]
    #[case::diff_block(
    fixtures::transaction02(HASH_256_1, HASH_256_1, 11),
    Err(ExtractionError::MergeError(format ! ("Can't merge ProtocolStates from different blocks: 0x{:x} != {}", H256::zero(), HASH_256_1)))
    )]
    #[case::same_tx(
    fixtures::transaction02(HASH_256_0, HASH_256_0, 11),
    Err(ExtractionError::MergeError(format ! ("Can't merge ProtocolStates from the same transaction: 0x{:x}", H256::zero())))
    )]
    #[case::lower_idx(
    fixtures::transaction02(HASH_256_1, HASH_256_0, 1),
    Err(ExtractionError::MergeError("Can't merge ProtocolStates with lower transaction index: 10 > 1".to_owned()))
    )]
    fn test_merge_pool_state_with_tx_errors(
        #[case] tx: Transaction,
        #[case] exp: Result<(), ExtractionError>,
    ) {
        let mut base_state = protocol_state_with_tx();

        let mut new_state = protocol_state_with_tx();
        new_state.tx = tx;

        let res = base_state.merge(new_state);

        assert_eq!(res, exp);
    }

    fn protocol_state() -> ProtocolState {
        let res1_value = 1000_u64.to_be_bytes().to_vec();
        let res2_value = 500_u64.to_be_bytes().to_vec();
        ProtocolState {
            component_id: "State1".to_string(),
            updated_attributes: vec![
                ("reserve1".to_owned(), Bytes::from(res1_value)),
                ("reserve2".to_owned(), Bytes::from(res2_value)),
            ]
            .into_iter()
            .collect(),
            deleted_attributes: HashMap::new(),
            modify_tx: H256::zero(),
        }
    }

    #[test]
    fn test_protocol_state_wrong_id() {
        let mut state1 = protocol_state();

        let attributes2: HashMap<String, Bytes> =
            vec![("reserve".to_owned(), Bytes::from(U256::from(900)))]
                .into_iter()
                .collect();
        let state2 = ProtocolState {
            component_id: "State2".to_owned(),
            updated_attributes: attributes2.clone(),
            deleted_attributes: HashMap::new(),
            modify_tx: HASH_256_1.parse().unwrap(),
        };

        let res = state1.merge(state2);

        assert_eq!(
            res,
            Err(ExtractionError::MergeError(
                "Can't merge ProtocolStates from differing identities; Expected State1, got State2"
                    .to_owned()
            ))
        );
    }

    #[test]
    fn test_protocol_state_parse_msg() {
        let msg = fixtures::pb_state_changes();

        let res = ProtocolState::try_from_message(msg, &fixtures::transaction01()).unwrap();

        assert_eq!(res, protocol_state());
    }

    fn block_entity_changes() -> BlockEntityChanges {
        let tx = Transaction {
            hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000011121314,
            ),
            block_hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000000000000,
            ),
            from: H160::from_low_u64_be(0x0000000000000000000000000000000041424344),
            to: Some(H160::from_low_u64_be(0x0000000000000000000000000000000051525354)),
            index: 11,
        };
        let attr: HashMap<String, Bytes> = vec![
            ("reserve".to_owned(), Bytes::from(600_u64.to_be_bytes().to_vec())),
            ("new".to_owned(), Bytes::from(0_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let state_updates: HashMap<String, ProtocolState> = vec![(
            "State1".to_owned(),
            ProtocolState {
                component_id: "State1".to_owned(),
                updated_attributes: attr,
                deleted_attributes: HashMap::new(),
                modify_tx: tx.hash,
            },
        )]
        .into_iter()
        .collect();
        let static_attr: HashMap<String, Bytes> =
            vec![("key".to_owned(), Bytes::from(600_u64.to_be_bytes().to_vec()))]
                .into_iter()
                .collect();
        let new_protocol_components: HashMap<String, ProtocolComponent> = vec![(
            "Pool".to_owned(),
            ProtocolComponent {
                id: ContractId("Pool".to_owned()),
                protocol_system: ProtocolSystem::Ambient,
                protocol_type_id: "Pool".to_owned(),
                chain: Chain::Ethereum,
                tokens: vec!["token0".to_owned(), "token1".to_owned()],
                static_attributes: static_attr,
                contract_ids: vec![
                    H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()
                ],
                change: ChangeType::Creation,
            },
        )]
        .into_iter()
        .collect();
        BlockEntityChanges {
            extractor: "test".to_string(),
            chain: Chain::Ethereum,
            block: Block {
                number: 1,
                hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000000000000,
                ),
                parent_hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000021222324,
                ),
                chain: Chain::Ethereum,
                ts: NaiveDateTime::from_timestamp_opt(1000, 0).unwrap(),
            },
            state_updates: vec![
                protocol_state_with_tx(),
                ProtocolStatesWithTx { protocol_states: state_updates, tx },
            ],
            new_protocol_components,
        }
    }

    #[test]
    fn test_block_entity_changes_parse_msg() {
        let msg = fixtures::pb_block_entity_changes();

        let res = BlockEntityChanges::try_from_message(
            msg,
            "test",
            Chain::Ethereum,
            ProtocolSystem::Ambient,
            "Pool".to_owned(),
        )
        .unwrap();

        assert_eq!(res, block_entity_changes());
    }

    fn block_entity_changes_result() -> BlockEntityChangesResult {
        let tx = Transaction {
            hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000011121314,
            ),
            block_hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000000000000,
            ),
            from: H160::from_low_u64_be(0x0000000000000000000000000000000041424344),
            to: Some(H160::from_low_u64_be(0x0000000000000000000000000000000051525354)),
            index: 2,
        };
        let attr1: HashMap<String, Bytes> = vec![
            ("reserve".to_owned(), Bytes::from(600_u64.to_be_bytes().to_vec())),
            ("static_attribute".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
            ("new".to_owned(), Bytes::from(0_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let attr2: HashMap<String, Bytes> = vec![
            ("reserve".to_owned(), Bytes::from(1000_u64.to_be_bytes().to_vec())),
            ("static_attribute".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let state_updates: HashMap<String, ProtocolState> = vec![
            (
                "State1".to_owned(),
                ProtocolState {
                    component_id: "State1".to_owned(),
                    updated_attributes: attr1,
                    deleted_attributes: HashMap::new(),
                    modify_tx: tx.hash,
                },
            ),
            (
                "State2".to_owned(),
                ProtocolState {
                    component_id: "State2".to_owned(),
                    updated_attributes: attr2,
                    deleted_attributes: HashMap::new(),
                    modify_tx: H256::zero(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let static_attr: HashMap<String, Bytes> =
            vec![("key".to_owned(), Bytes::from(600_u64.to_be_bytes().to_vec()))]
                .into_iter()
                .collect();
        let new_protocol_components: HashMap<String, ProtocolComponent> = vec![(
            "Pool".to_owned(),
            ProtocolComponent {
                id: ContractId("Pool".to_owned()),
                protocol_system: ProtocolSystem::Ambient,
                protocol_type_id: "Pool".to_owned(),
                chain: Chain::Ethereum,
                tokens: vec!["token0".to_owned(), "token1".to_owned()],
                static_attributes: static_attr,
                contract_ids: vec![
                    H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()
                ],
                change: ChangeType::Creation,
            },
        )]
        .into_iter()
        .collect();
        BlockEntityChangesResult {
            extractor: "test".to_string(),
            chain: Chain::Ethereum,
            block: Block {
                number: 1,
                hash: tx.block_hash,
                parent_hash: H256::from_low_u64_be(
                    0x0000000000000000000000000000000000000000000000000000000021222324,
                ),
                chain: Chain::Ethereum,
                ts: NaiveDateTime::from_timestamp_opt(1000, 0).unwrap(),
            },
            state_updates,
            new_protocol_components,
        }
    }

    #[test]
    fn test_block_entity_changes_aggregate() {
        let mut block_changes = block_entity_changes();
        let block_hash = "0x0000000000000000000000000000000000000000000000000000000000000000";
        // use a different tx so merge works
        let new_tx = fixtures::transaction02(HASH_256_1, block_hash, 5);
        block_changes.state_updates[0].tx = new_tx;

        let res = block_changes
            .aggregate_updates()
            .unwrap();

        assert_eq!(res, block_entity_changes_result());
        assert_eq!(res.state_updates.len(), 2);
    }

    fn create_transaction() -> Transaction {
        Transaction {
            hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000011121314,
            ),
            block_hash: H256::from_low_u64_be(
                0x0000000000000000000000000000000000000000000000000000000031323334,
            ),
            from: H160::from_low_u64_be(0x0000000000000000000000000000000041424344),
            to: Some(H160::from_low_u64_be(0x0000000000000000000000000000000051525354)),
            index: 2,
        }
    }

    #[rstest]
    fn test_try_from_message_protocol_component() {
        let msg = fixtures::pb_protocol_component();

        let expected_chain = Chain::Ethereum;
        let expected_protocol_system = ProtocolSystem::Ambient;
        let expected_attribute_map: HashMap<String, Bytes> = vec![
            ("balance".to_string(), Bytes::from(100_u64.to_be_bytes().to_vec())),
            ("factory_address".to_string(), Bytes::from(b"0x0fwe0g240g20".to_vec())),
        ]
        .into_iter()
        .collect();
        let protocol_type_id = String::from("id-1");

        // Call the try_from_message method
        let result = ProtocolComponent::try_from_message(
            msg,
            expected_chain,
            expected_protocol_system,
            protocol_type_id.clone(),
        );

        // Assert the result
        assert!(result.is_ok());

        // Unwrap the result for further assertions
        let protocol_component = result.unwrap();

        // Assert specific properties of the protocol component
        assert_eq!(protocol_component.id, ContractId("component_id".to_string()));
        assert_eq!(protocol_component.protocol_system, expected_protocol_system);
        assert_eq!(protocol_component.protocol_type_id, protocol_type_id);
        assert_eq!(protocol_component.chain, expected_chain);
        assert_eq!(protocol_component.tokens, vec!["token1".to_string(), "token2".to_string()]);
        assert_eq!(
            protocol_component.contract_ids,
            vec![
                H160::from_str("0x31fF2589Ee5275a2038beB855F44b9Be993aA804").unwrap(),
                H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()
            ]
        );
        assert_eq!(protocol_component.static_attributes, expected_attribute_map);
    }

    #[rstest]
    fn test_try_from_message_tvl_change() {
        let tx = create_transaction();
        let expected_balance: f64 = 3000.0;
        let msg_balance = expected_balance.to_le_bytes().to_vec();

        let expected_token = H160::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let msg_token = expected_token.0.to_vec();
        let expected_component_id = "DIANA-THALES";
        let msg_component_id = expected_component_id
            .try_into_bytes()
            .unwrap()
            .to_vec();
        let msg = substreams::BalanceChange {
            balance: msg_balance.to_vec(),
            token: msg_token,
            component_id: msg_component_id,
        };
        let from_message = TvlChange::try_from_message(msg, &tx).unwrap();

        assert_eq!(from_message.new_balance, expected_balance);
        assert_eq!(from_message.modify_tx, tx.hash);
        assert_eq!(from_message.token, expected_token);
        assert_eq!(from_message.component_id, expected_component_id);
    }
}
