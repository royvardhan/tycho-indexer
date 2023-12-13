#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::extractor::evm::Transaction;
use strum_macros::{Display, EnumString};

use crate::{extractor::ExtractionError, hex_bytes::Bytes, pb::tycho::evm::v1 as substreams};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, EnumString, Display, Default,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Chain {
    #[default]
    Ethereum,
    Starknet,
    ZkSync,
}

#[derive(PartialEq, Debug, Clone)]
pub enum ProtocolSystem {
    Ambient,
}

#[derive(PartialEq, Debug, Clone)]
pub enum ImplementationType {
    Vm,
    Custom,
}

#[derive(PartialEq, Debug, Clone)]
pub enum FinancialType {
    Swap,
    Lend,
    Leverage,
    Psm,
}

#[derive(PartialEq, Debug, Clone)]
pub struct ProtocolType {
    name: String,
    attribute_schema: serde_json::Value,
    financial_type: FinancialType,
    implementation_type: ImplementationType,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ExtractorIdentity {
    pub chain: Chain,
    pub name: String,
}

impl ExtractorIdentity {
    pub fn new(chain: Chain, name: &str) -> Self {
        Self { chain, name: name.to_owned() }
    }
}

impl std::fmt::Display for ExtractorIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.chain, self.name)
    }
}

#[derive(Debug, PartialEq)]
pub struct ExtractionState {
    pub name: String,
    pub chain: Chain,
    pub attributes: serde_json::Value,
    pub cursor: Vec<u8>,
}

impl ExtractionState {
    pub fn new(
        name: String,
        chain: Chain,
        attributes: Option<serde_json::Value>,
        cursor: &[u8],
    ) -> Self {
        ExtractionState {
            name,
            chain,
            attributes: attributes.unwrap_or_default(),
            cursor: cursor.to_vec(),
        }
    }
}

#[typetag::serde(tag = "type")]
pub trait NormalisedMessage: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static {
    fn source(&self) -> ExtractorIdentity;
}
/// A type representing the unique identifier for a contract. It can represent an on-chain address
/// or in the case of a one-to-many relationship it could be something like 'USDC-ETH'. This is for
/// example the case with ambient, where one component is responsible for multiple contracts.
///
/// `ContractId` is a simple wrapper around a `String` to ensure type safety
/// and clarity when working with contract identifiers.
#[derive(PartialEq, Debug)]
pub struct ContractId(String);

pub struct ProtocolComponent<T> {
    // an id for this component, could be hex repr of contract address
    id: ContractId,
    // what system this component belongs to
    protocol_system: ProtocolSystem,
    // more metadata information about the components general type (swap, lend, bridge, etc.)
    protocol_type: ProtocolType,
    // Blockchain the component belongs to
    chain: Chain,
    // holds the tokens tradable
    tokens: Vec<T>,
    // ID's referring to related contracts
    contract_ids: Vec<ContractId>,
    // allows to express some validation over the static attributes if necessary
    static_attributes: HashMap<String, Bytes>,
}

impl ProtocolComponent<String> {
    pub fn try_from_message(
        msg: substreams::ProtocolComponent,
        protocol_system: ProtocolSystem,
        protocol_type: ProtocolType,
        chain: Chain,
    ) -> Result<Self, ExtractionError> {
        let id = ContractId(
            String::from_utf8(msg.id)
                .map_err(|error| ExtractionError::DecodeError(error.to_string()))?,
        );

        let tokens = msg
            .tokens
            .into_iter()
            .map(|t| {
                String::from_utf8(t)
                    .map_err(|error| ExtractionError::DecodeError(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let contract_ids = msg
            .contracts
            .into_iter()
            .map(|contract_id| match String::from_utf8(contract_id) {
                Ok(id) => Ok(ContractId(id)),
                Err(err) => Err(ExtractionError::DecodeError(err.to_string())),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let keys = msg
            .static_attributes
            .clone()
            .into_iter()
            .map(|attr| {
                String::from_utf8(attr.name)
                    .map_err(|error| ExtractionError::DecodeError(error.to_string()))
            })
            .collect::<Result<Vec<String>, _>>()?;

        let values: Vec<_> = msg
            .static_attributes
            .into_iter()
            .map(|attr| Bytes::from(attr.value))
            .collect();

        let attribute_map: HashMap<_, _> = keys.into_iter().zip(values).collect();

        Ok(Self {
            id,
            protocol_type,
            protocol_system,
            tokens,
            contract_ids,
            static_attributes: attribute_map,
            chain,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TvlChange<T> {
    token: T,
    new_balance: f64,
    // tx where the this balance was observed
    modify_tx: String,
    component_id: String,
}

impl TvlChange<String> {
    pub fn try_from_message(
        msg: substreams::BalanceChange,
        tx: &Transaction,
    ) -> Result<Self, ExtractionError> {
        Ok(Self {
            token: String::from_utf8(msg.token)
                .map_err(|error| ExtractionError::DecodeError(error.to_string()))?,
            new_balance: f64::from_bits(u64::from_le_bytes(msg.balance.try_into().unwrap())),
            modify_tx: tx.hash.to_string(),
            component_id: String::from_utf8(msg.component_id)
                .map_err(|error| ExtractionError::DecodeError(error.to_string()))?,
        })
    }
}

#[allow(dead_code)]
pub struct ProtocolState {
    // associates back to a component, which has metadata like type, tokens , etc.
    pub component_id: String,
    // holds all the protocol specific attributes, validates by the components schema
    pub attributes: HashMap<String, Bytes>,
    // via transaction, we can trace back when this state became valid
    pub modify_tx: Transaction,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::pb::tycho::evm::v1::Attribute;
    use actix_web::body::MessageBody;
    use ethers::types::{H160, H256};
    use rstest::rstest;

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
        let balance_key = "balance";
        let factory_address_key = "factory_address";
        let balance_value = b"50000";
        let factory_address = b"0x0fwe0g240g20";

        // Sample data for testing
        let static_att = vec![
            Attribute { name: balance_key.as_bytes().to_vec(), value: balance_value.to_vec() },
            Attribute {
                name: factory_address_key.as_bytes().to_vec(),
                value: factory_address.to_vec(),
            },
        ];
        let msg = substreams::ProtocolComponent {
            id: b"component_id".to_vec(),
            tokens: vec![b"token1".to_vec(), b"token2".to_vec()],
            contracts: vec![b"contract1".to_vec(), b"contract2".to_vec()],
            static_attributes: static_att,
        };
        let expected_chain = Chain::Ethereum;
        let expected_protocol_system = ProtocolSystem::Ambient;
        let mut expected_attribute_map = HashMap::new();
        expected_attribute_map.insert(balance_key.to_string(), Bytes::from(balance_value.to_vec()));
        expected_attribute_map
            .insert(factory_address_key.to_string(), Bytes::from(factory_address.to_vec()));

        let protocol_type = ProtocolType {
            name: "Pool".to_string(),
            attribute_schema: serde_json::Value::default(),
            financial_type: crate::models::FinancialType::Psm,
            implementation_type: crate::models::ImplementationType::Custom,
        };

        // Call the try_from_message method
        let result = ProtocolComponent::<String>::try_from_message(
            msg,
            expected_protocol_system.clone(),
            protocol_type.clone(),
            expected_chain,
        );

        // Assert the result
        assert!(result.is_ok());

        // Unwrap the result for further assertions
        let protocol_component = result.unwrap();

        // Assert specific properties of the protocol component
        assert_eq!(protocol_component.id.0, "component_id");
        assert_eq!(protocol_component.protocol_system, expected_protocol_system);
        assert_eq!(protocol_component.protocol_type, protocol_type);
        assert_eq!(protocol_component.chain, expected_chain);
        assert_eq!(protocol_component.tokens, vec!["token1".to_string(), "token2".to_string()]);
        assert_eq!(
            protocol_component.contract_ids,
            vec![ContractId("contract1".to_string()), ContractId("contract2".to_string())]
        );
        assert_eq!(protocol_component.static_attributes, expected_attribute_map);
    }

    #[rstest]
    fn test_try_from_message_tvl_change() {
        let tx = create_transaction();
        let expected_balance: f64 = 3000.0;
        let msg_balance = expected_balance.to_le_bytes().to_vec();

        let expected_token = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
        let msg_token = expected_token
            .try_into_bytes()
            .unwrap()
            .to_vec();
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
        assert_eq!(from_message.modify_tx, tx.hash.to_string());
        assert_eq!(from_message.token, expected_token);
        assert_eq!(from_message.component_id, expected_component_id);
    }
}
