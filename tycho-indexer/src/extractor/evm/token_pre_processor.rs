use crate::extractor::evm::ERC20Token;
use async_trait::async_trait;
use ethers::{
    abi::Abi,
    contract::Contract,
    prelude::Provider,
    providers::Http,
    types::{H160, U256},
};
use serde_json::from_str;
use std::{str::FromStr, sync::Arc};
use token_analyzer::{
    trace_call::{TokenOwnerFinding, TraceCallDetector},
    BadTokenDetecting, TokenQuality,
};
use tracing::{instrument, warn};

use ethrpc::Web3;
use tycho_core::models::Chain;

#[derive(Debug, Clone)]
pub struct TokenPreProcessor {
    ethers_client: Arc<Provider<Http>>,
    erc20_abi: Abi,
    web3_client: Web3,
}
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TokenPreProcessorTrait: Send + Sync {
    async fn get_tokens(
        &self,
        addresses: Vec<H160>,
        token_finder: Arc<dyn TokenOwnerFinding>,
    ) -> Vec<ERC20Token>;
}

const ABI_STR: &str = include_str!("./abi/erc20.json");

impl TokenPreProcessor {
    pub fn new(ethers_client: Provider<Http>, web3_client: Web3) -> Self {
        let abi = from_str::<Abi>(ABI_STR).expect("Unable to parse ABI");
        TokenPreProcessor { ethers_client: Arc::new(ethers_client), erc20_abi: abi, web3_client }
    }
}

#[async_trait]
impl TokenPreProcessorTrait for TokenPreProcessor {
    #[instrument]
    async fn get_tokens(
        &self,
        addresses: Vec<H160>,
        token_finder: Arc<dyn TokenOwnerFinding>,
    ) -> Vec<ERC20Token> {
        let mut tokens_info = Vec::new();

        for address in addresses {
            let contract =
                Contract::new(address, self.erc20_abi.clone(), self.ethers_client.clone());

            let symbol = contract
                .method("symbol", ())
                .expect("Error preparing request")
                .call()
                .await;

            let decimals: Result<u8, _> = contract
                .method("decimals", ())
                .expect("Error preparing request")
                .call()
                .await;

            let trace_call = TraceCallDetector {
                web3: self.web3_client.clone(),
                finder: token_finder.clone(),
                settlement_contract: H160::from_str("0xc9f2e6ea1637E499406986ac50ddC92401ce1f58") // middle contract used to check for fees, set to cowswap settlement
                    .unwrap(),
            };

            let (_quality, gas, tax) = trace_call
                .detect(address)
                .await
                .unwrap_or_else(|e| {
                    warn!("Detection failed: {:?}", e);
                    (TokenQuality::bad("Detection failed"), None, None)
                });

            let (symbol, decimals, mut quality) = match (symbol, decimals) {
                (Ok(symbol), Ok(decimals)) => (symbol, decimals, 100),
                (Ok(symbol), Err(_)) => (symbol, 18, 0),
                (Err(_), Ok(decimals)) => (address.to_string(), decimals, 0),
                (Err(_), Err(_)) => (address.to_string(), 18, 0),
            };

            // If quality is 100 but it's a fee token, set quality to 50
            if quality == 100 && tax.map_or(false, |tax_value| tax_value > U256::zero()) {
                quality = 50;
            }

            tokens_info.push(ERC20Token {
                address,
                symbol: symbol.replace('\0', ""),
                decimals: decimals.into(),
                tax: tax.unwrap_or(U256::zero()).as_u64(),
                gas: gas.map_or_else(Vec::new, |g| vec![Some(g.as_u64())]),
                chain: Chain::Ethereum,
                quality,
            });
        }

        tokens_info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethrpc::{http::HttpTransport, Web3Transport};
    use reqwest::Client;
    use std::{collections::HashMap, env, str::FromStr};
    use token_analyzer::TokenFinder;
    use url::Url;

    #[tokio::test]
    #[ignore]
    // This test requires a real RPC URL
    async fn test_get_tokens() {
        let archive_rpc = env::var("ARCHIVE_ETH_RPC_URL").expect("ARCHIVE_ETH_RPC_URL is not set");
        let client: Provider<Http> =
            Provider::<Http>::try_from(archive_rpc.clone()).expect("Error creating HTTP provider");

        let transport = Web3Transport::new(HttpTransport::new(
            Client::new(),
            Url::from_str(archive_rpc.as_str()).unwrap(),
            "transport".to_owned(),
        ));
        let w3 = Web3::new(transport);

        let processor = TokenPreProcessor::new(client, w3);

        let tf = TokenFinder::new(HashMap::new());

        let weth_address: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
        let usdc_address: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let fake_address: &str = "0xA0b86991c7456b36c1d19D4a2e9Eb0cE3606eB48";
        let addresses = vec![
            H160::from_str(weth_address).unwrap(),
            H160::from_str(usdc_address).unwrap(),
            H160::from_str(fake_address).unwrap(),
        ];

        let results = processor
            .get_tokens(addresses, Arc::new(tf))
            .await;
        assert_eq!(results.len(), 3);
        let relevant_attrs: Vec<(String, u32, u32)> = results
            .iter()
            .map(|t| (t.symbol.clone(), t.decimals, t.quality))
            .collect();
        assert_eq!(
            relevant_attrs,
            vec![
                ("WETH".to_string(), 18, 100),
                ("USDC".to_string(), 6, 100),
                ("0xa0b8…eb48".to_string(), 18, 0)
            ]
        );
    }
}
