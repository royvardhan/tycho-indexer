use crate::{extractor::evm::ERC20Token, models::Chain};
use ethers::{abi::Abi, contract::Contract, prelude::Middleware, types::H160};
use serde_json::from_str;
use std::{fs, sync::Arc};

pub struct TokenPreProcessor<M: Middleware> {
    pub client: Arc<M>,
    erc20_abi: Abi,
}

impl<M: Middleware> TokenPreProcessor<M> {
    pub fn new(client: M) -> Self {
        let abi_str = fs::read_to_string("src/extractor/evm/abi/erc20.json")
            .expect("Unable to read ABI file");
        let abi = from_str::<Abi>(&abi_str).expect("Unable to parse ABI");
        TokenPreProcessor { client: Arc::new(client), erc20_abi: abi }
    }

    pub async fn get_tokens(&self, addresses: Vec<H160>) -> Vec<ERC20Token> {
        let mut tokens_info = Vec::new();

        for address in addresses {
            let contract = Contract::new(address, self.erc20_abi.clone(), self.client.clone());

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

            let (symbol, decimals, quality) = match (symbol, decimals) {
                (Ok(symbol), Ok(decimals)) => (symbol, decimals, 100),
                (Ok(symbol), Err(_)) => (symbol, 18, 0),
                (Err(_), Ok(decimals)) => (address.to_string(), decimals, 0),
                (Err(_), Err(_)) => (address.to_string(), 18, 0),
            };
            tokens_info.push(ERC20Token {
                address,
                symbol,
                decimals: decimals.into(),
                tax: 0,
                gas: vec![],
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
    use ethers::{
        providers::{Http, Provider},
        types::H160,
    };
    use std::str::FromStr;

    #[tokio::test]
    async fn test_get_tokens() {
        let rpc_url = "https://eth-mainnet.g.alchemy.com/v2/OTD5W7gdTPrzpVot41Lx9tJD9LUiAhbs";
        let client: Provider<Http> =
            Provider::<Http>::try_from(rpc_url).expect("Error creating HTTP provider");
        let processor = TokenPreProcessor::new(client);

        let weth_address: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
        let usdc_address: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let fake_address: &str = "0xA0b86991c7456b36c1d19D4a2e9Eb0cE3606eB48";
        let addresses = vec![
            H160::from_str(weth_address).unwrap(),
            H160::from_str(usdc_address).unwrap(),
            H160::from_str(fake_address).unwrap(),
        ];

        let results = processor.get_tokens(addresses).await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].symbol, "WETH");
        assert_eq!(results[0].decimals, 18);
        assert_eq!(results[1].symbol, "USDC");
        assert_eq!(results[1].decimals, 6);
        assert_eq!(results[2].symbol, "0xa0b8…eb48");
        assert_eq!(results[2].decimals, 18);
    }
}
