#[cfg(test)]
mod tests {
    use cairo_vm::felt::Felt252;
    use dotenv::dotenv;
    use protosim::starknet_simulation::{
        rpc_reader::RpcStateReader,
        simulation::{ContractOverride, Overrides, SimulationEngine, SimulationParameters},
    };
    use rpc_state_reader::rpc_state::{BlockTag, RpcChain, RpcState};
    use starknet_in_rust::utils::{felt_to_hash, get_storage_var_address, Address, ClassHash};
    use std::{collections::HashMap, env, sync::Arc};

    const BOB_ADDRESS: &str = "0x065c19e14e2587d2de74c561b2113446ca4b389aabe6da1dc4accb6404599e99";
    const EKUBO_ADDRESS: &str =
        "0x00000005dd3d2f4429af886cd1a3b08289dbcea99a294197e9eb43b0e0325b4b";
    const EKUBO_SIMPLE_SWAP_ADDRESS: &str =
        "0x07a83729aaaae6344d6fca558614cd22ecdd3f5cd90ec0cd20c8d6bf08170431";
    const USDC_ADDRESS: &str = "0x053c91253bc9682c04929ca02ed00b3e423f6710d2ee7e0d5ebb06f3ecf368a8";
    const ETH_ADDRESS: &str = "0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7";
    const DAI_ADDRESS: &str = "0xda114221cb83fa859dbdb4c44beeaa0bb37c7537ad5ae66fe5e0efd20e6eb3";

    pub fn felt_str(val: &str) -> Felt252 {
        let base = if val.starts_with("0x") { 16_u32 } else { 10_u32 };
        let stripped_val = val.strip_prefix("0x").unwrap_or(val);

        Felt252::parse_bytes(stripped_val.as_bytes(), base).expect("Failed to parse input")
    }

    pub fn address_str(val: &str) -> Address {
        Address(felt_str(val))
    }

    /// Setup Starknet RPC state reader
    ///
    /// Does not accept block number as input since it is overwritten by the simulation engine,
    /// taking it as a parameter might make the user think that it is permanent
    fn setup_reader() -> RpcStateReader {
        let infura_api_key = env::var("INFURA_API_KEY").unwrap_or_else(|_| {
            dotenv().expect("Missing .env file");
            env::var("INFURA_API_KEY").expect("Missing INFURA_API_KEY in .env file")
        });
        let rpc_endpoint = format!("https://{}.infura.io/v3/{}", RpcChain::MainNet, infura_api_key);
        let feeder_url = format!("https://{}.starknet.io/feeder_gateway", RpcChain::MainNet);
        RpcStateReader::new(RpcState::new(
            RpcChain::MainNet,
            BlockTag::Latest.into(),
            &rpc_endpoint,
            &feeder_url,
        ))
    }

    /// Setup simulation engine with contract overrides
    fn setup_engine(
        contract_overrides: Option<Vec<ContractOverride>>,
    ) -> SimulationEngine<RpcStateReader> {
        let rpc_state_reader = Arc::new(setup_reader());
        let contract_overrides = contract_overrides.unwrap_or_default();
        SimulationEngine::new(rpc_state_reader, contract_overrides.into()).unwrap()
    }

    fn construct_token_contract_override(token: Address) -> ContractOverride {
        // ERC20 contract overrides - using USDC token contract template
        let class_hash: ClassHash =
            hex::decode("02760f25d5a4fb2bdde5f561fd0b44a3dee78c28903577d37d669939d97036a0")
                .unwrap()
                .as_slice()
                .try_into()
                .unwrap();

        ContractOverride::new(token, class_hash, None)
    }

    fn add_balance_override(
        mut overrides: Overrides,
        address: Address,
        amount: Felt252,
    ) -> Overrides {
        // override balance
        let balance_storage_hash =
            felt_to_hash(&get_storage_var_address("ERC20_balances", &[address.0.clone()]).unwrap());
        overrides.insert(balance_storage_hash, amount);
        overrides
    }

    #[test]
    #[cfg_attr(not(feature = "network_tests"), ignore)]
    fn test_consecutive_simulations_ekubo() {
        // Test vars
        let block_number = 194554;
        let token0 = address_str(DAI_ADDRESS);
        let token1 = address_str(ETH_ADDRESS);
        let test_wallet = address_str(BOB_ADDRESS);
        let ekubo_swap_address = address_str(EKUBO_SIMPLE_SWAP_ADDRESS);
        let ekubo_core_address = address_str(EKUBO_ADDRESS);
        let sell_amount = felt_str("0x5afb5ab61ef191");

        // Construct engine with contract overrides
        let sell_token_contract_override = construct_token_contract_override(token0.clone());
        let mut engine = setup_engine(Some(vec![sell_token_contract_override]));

        // Construct simulation overrides - token balances
        let mut token_overrides = Overrides::new();
        // mock having transferred tokens to the swap contract before triggering the swap function
        token_overrides =
            add_balance_override(token_overrides, ekubo_swap_address.clone(), sell_amount.clone());
        // mock the core contract's reserve balance since this is checked during swap and errors if
        // incorrect
        token_overrides = add_balance_override(
            token_overrides,
            ekubo_core_address,
            felt_str("2421600066015287788594"),
        );
        let mut storage_overrides = HashMap::new();
        storage_overrides.insert(token0.clone(), token_overrides);

        // obtained from this Ekubo simple swap call: https://starkscan.co/call/0x04857b5a7af37e9b9f6fae27923d725f07016a4449f74f5ab91c04f13bbc8d23_1_3
        let swap_calldata = vec![
            // Pool key data
            token0.0,                                     // token0
            token1.0,                                     // token1
            felt_str("0xc49ba5e353f7d00000000000000000"), // fee
            Felt252::from(5982),                          // tick spacing
            Felt252::from(0),                             // extension
            // Swap data
            sell_amount,                                   // amount
            Felt252::from(0),                              // amount sign
            Felt252::from(0),                              // istoken1
            felt_str("0x65740af99bee7b4bf062fb147160000"), // sqrt ratio limit (lower bits)
            Felt252::from(0),                              // sqrt ratio limit (upper bits)
            Felt252::from(0),                              // skip ahead
            test_wallet.0.clone(),                         // recipient
            Felt252::from(0),                              // calculated_amount_threshold
        ];

        let params = SimulationParameters::new(
            test_wallet,
            ekubo_swap_address,
            swap_calldata,
            "swap".to_owned(),
            Some(storage_overrides),
            Some(u128::MAX),
            block_number,
        );

        // SIMULATION 1
        let result0 = engine.simulate(&params);
        dbg!(&result0);
        assert!(result0.is_ok());
        let res = result0.unwrap();
        assert_eq!(res.gas_used, 9480810);
        assert_eq!(res.result[2], felt_str("21909951468890105")); // check amount out

        // SIMULATION 2

        let result1 = engine.simulate(&params);
        dbg!(&result1);
        assert!(result1.is_ok());
        let res = result1.unwrap();
        assert_eq!(res.gas_used, 9480810);
        assert_eq!(res.result[2], felt_str("21909951468890105")); // check amount out is not
                                                                  // affected by previous simulation
    }

    #[test]
    fn test_get_eth_usdc_spot_price_ekubo() {
        let block_number = 367676;
        let mut engine = setup_engine(None);

        let swap_calldata = vec![
            felt_str(ETH_ADDRESS),                            // token0
            felt_str(USDC_ADDRESS),                           // token1
            felt_str("170141183460469235273462165868118016"), // fee
            Felt252::from(1000),                              // tick spacing
            Felt252::from(0),                                 // extension
        ];

        let params = SimulationParameters::new(
            address_str(BOB_ADDRESS),
            address_str(EKUBO_ADDRESS),
            swap_calldata,
            "get_pool_price".to_owned(),
            None,
            Some(100000),
            block_number,
        );

        let result = engine.simulate(&params);

        let res = result.unwrap().result[0].clone();

        // To get the human readable price we will need to convert this on the Python side like
        // this: https://www.wolframalpha.com/input?i=(14458875492015717597830515600275777+/+2**128)**2*10**12
        assert_eq!(res, felt_str("14458875492015717597830515600275777"))
    }

    #[test]
    fn test_get_dai_usdc_spot_price_ekubo() {
        let block_number = 426179;
        let mut engine = setup_engine(None);

        let swap_calldata = vec![
            felt_str(DAI_ADDRESS),                            // token0
            felt_str(USDC_ADDRESS),                           // token1
            felt_str("170141183460469235273462165868118016"), // fee
            Felt252::from(1000),                              // tick spacing
            Felt252::from(0),                                 // extension
        ];

        let params = SimulationParameters::new(
            address_str(BOB_ADDRESS),
            address_str(EKUBO_ADDRESS),
            swap_calldata,
            "get_pool_price".to_owned(),
            None,
            Some(100000),
            block_number,
        );

        let result = engine.simulate(&params);

        let res = result.unwrap().result[0].clone();

        // To get the human readable price we will need to convert this on the Python side like
        // this: https://www.wolframalpha.com/input?i=(340321610937302884216160363291566+/+2**128)**2*10**12
        assert_eq!(res, felt_str("340288844056980486564646108486642"))
    }

    #[test]
    #[cfg_attr(not(feature = "network_tests"), ignore)]
    fn test_get_amount_out_eth_dai() {
        let test_wallet = address_str(BOB_ADDRESS);
        let ekubo_swap_address = address_str(EKUBO_SIMPLE_SWAP_ADDRESS);

        // Test vars
        let block_number = 386000;
        let token0 = address_str(DAI_ADDRESS);
        let token1 = address_str(ETH_ADDRESS);
        let tokens = vec![token0.clone(),token1.clone()];
        let sell_amount = felt_str("0x2386f26fc10000");
        let expected_buy_amount = "18801973723146384196";
        let sell_token_index = 1;

        // Get ekubo's balance of sell token
        let mut engine = setup_engine(None);

        let balance_params = SimulationParameters::new(
            test_wallet.clone(),
            tokens[sell_token_index].clone(),
            vec![felt_str(EKUBO_ADDRESS)],
            "balanceOf".to_owned(),
            None,
            Some(u128::MAX),
            block_number,
        );

        let balance = engine.simulate(&balance_params);


        // Construct engine with contract overrides
        let sell_token_contract_override = construct_token_contract_override(tokens[sell_token_index].clone());
        let mut engine = setup_engine(Some(vec![sell_token_contract_override]));
        // Construct simulation overrides - token balances
        let mut token_overrides = Overrides::new();
        // mock having transferred tokens to the swap contract before triggering the swap function
        token_overrides =
            add_balance_override(token_overrides, ekubo_swap_address.clone(), sell_amount.clone());
        // mock the core contract's reserve balance since this is checked during swap and errors if
        // incorrect
        token_overrides = add_balance_override(
            token_overrides,
            address_str(EKUBO_ADDRESS),
            balance.unwrap().result[0].to_owned(),
        );
        let mut storage_overrides = HashMap::new();
        storage_overrides.insert(tokens[sell_token_index].clone(), token_overrides);

        let swap_calldata = vec![
            // Pool key data
            token0.0,                                     // token0
            token1.0,                                     // token1
            felt_str("0x20c49ba5e353f80000000000000000"), // fee
            Felt252::from(1000),                          // tick spacing
            Felt252::from(0),                             // extension
            // Swap data
            sell_amount,                                   // amount
            Felt252::from(0),                              // amount sign
            Felt252::from(1),                              // istoken1
            felt_str("0x6f3528fe26840249f4b191ef6dff7928"), // sqrt ratio limit (lower bits)
            felt_str("0xfffffc080ed7b455"),             // sqrt ratio limit (upper bits)
            Felt252::from(0),                              // skip ahead
            test_wallet.0.clone(),                         // recipient
            Felt252::from(0),                              // calculated_amount_threshold
        ];

        let params = SimulationParameters::new(
            test_wallet.clone(),
            ekubo_swap_address.clone(),
            swap_calldata,
            "swap".to_owned(),
            Some(storage_overrides.clone()),
            Some(u128::MAX),
            block_number,
        );

        let result0 = engine.simulate(&params);
        assert!(result0.is_ok());
        let res = result0.unwrap();
        let amount_out_index = if sell_token_index == 1 { 0 } else { 2 };
        assert_eq!(res.gas_used, 7701570);
        assert_eq!(res.result[amount_out_index], felt_str(expected_buy_amount)); // check amount out
    }
}
