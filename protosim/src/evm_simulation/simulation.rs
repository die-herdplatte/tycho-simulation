use super::{account_storage::StateUpdate, database};
use crate::evm_simulation::database::OverriddenSimulationDB;
use ethers::{
    providers::Middleware,
    types::{Address, Bytes, U256}, // Address is an alias of H160
};

use revm::{
    inspectors::CustomPrintTracer,
    primitives::{
        bytes, // `bytes` is an external crate
        CreateScheme,
        EVMError,
        EVMResult,
        ExecutionResult,
        Output,
        State,
        TransactTo,
        B160 as rB160,
        U256 as rU256,
    },
    EVM,
};
use tracing::debug;
use std::collections::HashMap;

/// An error representing any transaction simulation result other than successful execution
#[derive(Debug)]
pub enum SimulationError {
    /// Something went wrong while getting storage; might be caused by network issues
    StorageError(String),
    /// Simulation didn't succeed; likely not related to network, so retrying won't help
    TransactionError { data: String, gas_used: Option<u64> },
}

/// A result of a successful transaction simulation
pub struct SimulationResult {
    /// Output of transaction execution as bytes
    pub result: bytes::Bytes,
    /// State changes caused by the transaction
    pub state_updates: HashMap<Address, StateUpdate>,
    /// Gas used by the transaction (already reduced by the refunded gas)
    pub gas_used: u64,
}

#[derive(Debug)]
pub struct SimulationEngine<M: Middleware> {
    pub state: database::SimulationDB<M>,
    pub trace: bool,
}

impl<M: Middleware> SimulationEngine<M> {
    /// Simulate a transaction
    ///
    /// State's block will be modified to be the last block before the simulation's block.
    pub fn simulate(
        &self,
        params: &SimulationParameters,
    ) -> Result<SimulationResult, SimulationError> {
        // We allocate a new EVM so we can work with a simple referenced DB instead of a fully
        // concurrently save shared reference and write locked object. Note that concurrently
        // calling this method is therefore not possible.
        // There is no need to keep an EVM on the struct as it only holds the environment and the
        // db, the db is simply a reference wrapper. To avoid lifetimes leaking we don't let the evm
        // struct outlive this scope.
        let mut vm = EVM::new();

        // The below call to vm.database consumes its argument. By wrapping state in a new object,
        // we protect the state from being consumed.
        let db_ref = OverriddenSimulationDB {
            inner_db: &self.state,
            overrides: &params
                .revm_overrides()
                .unwrap_or_default(),
        };

        vm.database(db_ref);
        vm.env.block.number = params.revm_block_number();
        vm.env.block.timestamp = params.revm_timestamp();
        vm.env.tx.caller = params.revm_caller();
        vm.env.tx.transact_to = params.revm_to();
        vm.env.tx.data = params.revm_data();
        vm.env.tx.value = params.revm_value();
        vm.env.tx.gas_limit = params
            .revm_gas_limit()
            .unwrap_or(8_000_000);
        debug!("Starting simulation with tx parameters: {:#?} {:#?}", vm.env.tx, vm.env.block);

        let evm_result = if self.trace {
            let tracer = CustomPrintTracer::default();
            vm.inspect_ref(tracer)
        } else {
            vm.transact_ref()
        };

        interpret_evm_result(evm_result)
    }
}

/// Convert a complex EVMResult into a simpler structure
///
/// EVMResult is not of an error type even if the transaction was not successful.
/// This function returns an Ok if and only if the transaction was successful.
/// In case the transaction was reverted, halted, or another error occurred (like an error
/// when accessing storage), this function returns an Err with a simple String description
/// of an underlying cause.
///
/// # Arguments
///
/// * `evm_result` - output from calling `revm.transact()`
///
/// # Errors
///
/// * `SimulationError` - simulation wasn't successful for any reason. See variants for details.
fn interpret_evm_result<DBError: std::fmt::Debug>(
    evm_result: EVMResult<DBError>,
) -> Result<SimulationResult, SimulationError> {
    match evm_result {
        Ok(result_and_state) => match result_and_state.result {
            ExecutionResult::Success { gas_used, gas_refunded, output, .. } => {
                Ok(interpret_evm_success(gas_used, gas_refunded, output, result_and_state.state))
            }
            ExecutionResult::Revert { output, gas_used } => {
                Err(SimulationError::TransactionError {
                    data: format!("0x{}", hex::encode(output)),
                    gas_used: Some(gas_used),
                })
            }
            ExecutionResult::Halt { reason, gas_used } => Err(SimulationError::TransactionError {
                data: format!("{:?}", reason),
                gas_used: Some(gas_used),
            }),
        },
        Err(evm_error) => match evm_error {
            EVMError::Transaction(invalid_tx) => Err(SimulationError::TransactionError {
                data: format!("EVM error: {invalid_tx:?}"),
                gas_used: None,
            }),
            EVMError::PrevrandaoNotSet => Err(SimulationError::TransactionError {
                data: "EVM error: PrevrandaoNotSet".to_owned(),
                gas_used: None,
            }),
            EVMError::Database(db_error) => {
                Err(SimulationError::StorageError(format!("Storage error: {db_error:?}")))
            }
        },
    }
}

// Helper function to extract some details from a successful transaction execution
fn interpret_evm_success(
    gas_used: u64,
    gas_refunded: u64,
    output: Output,
    state: State,
) -> SimulationResult {
    SimulationResult {
        result: output.into_data(),
        state_updates: {
            // For each account mentioned in state updates in REVM output, we will have
            // one record in our hashmap. Such record contains *new* values of account's
            // state. This record's optional `storage` field will contain
            // account's storage changes (as a hashmap from slot index to slot value),
            // unless REVM output doesn't contain any storage for this account, in which case
            // we set this field to None. If REVM did return storage, we return one record
            // per *modified* slot (sometimes REVM returns a storage record for an account
            // even if the slots are not modified).
            let mut account_updates: HashMap<Address, StateUpdate> = HashMap::new();
            for (address, account) in state {
                account_updates.insert(
                    Address::from(address),
                    StateUpdate {
                        // revm doesn't say if the balance was actually changed
                        balance: Some(account.info.balance),
                        // revm doesn't say if the code was actually changed
                        storage: {
                            if account.storage.is_empty() {
                                None
                            } else {
                                let mut slot_updates: HashMap<rU256, rU256> = HashMap::new();
                                for (index, slot) in account.storage {
                                    if slot.is_changed() {
                                        slot_updates.insert(index, slot.present_value);
                                    }
                                }
                                if slot_updates.is_empty() {
                                    None
                                } else {
                                    Some(slot_updates)
                                }
                            }
                        },
                    },
                );
            }
            account_updates
        },
        gas_used: gas_used - gas_refunded,
    }
}
#[derive(Debug)]
/// Data needed to invoke a transaction simulation
pub struct SimulationParameters {
    /// Address of the sending account
    pub caller: Address,
    /// Address of the receiving account/contract
    pub to: Address,
    /// Calldata
    pub data: Bytes,
    /// Amount of native token sent
    pub value: U256,
    /// EVM state overrides.
    /// Will be merged with existing state. Will take effect only for current simulation.
    pub overrides: Option<HashMap<Address, HashMap<U256, U256>>>,
    /// Limit of gas to be used by the transaction
    pub gas_limit: Option<u64>,
    /// The block number to be used by the transaction. This is independent of the states block.
    pub block_number: u64,
    /// The timestamp to be used by the transaction
    pub timestamp: u64,
}

// Converters of fields to revm types
impl SimulationParameters {
    fn revm_caller(&self) -> rB160 {
        rB160::from_slice(&self.caller.0)
    }

    fn revm_to(&self) -> TransactTo {
        if self.to == Address::zero() {
            TransactTo::Create(CreateScheme::Create2 { salt: rU256::default() })
        } else {
            TransactTo::Call(rB160::from_slice(&self.to.0))
        }
    }

    fn revm_data(&self) -> revm::primitives::Bytes {
        revm::primitives::Bytes::copy_from_slice(&self.data.0)
    }

    fn revm_value(&self) -> rU256 {
        rU256::from_limbs(self.value.0)
    }

    fn revm_overrides(
        &self,
    ) -> Option<std::collections::HashMap<rB160, std::collections::HashMap<rU256, rU256>>> {
        self.overrides.clone().map(|original| {
            let mut result = std::collections::HashMap::new();
            for (address, storage) in original {
                let mut account_storage = std::collections::HashMap::new();
                for (key, value) in storage {
                    account_storage.insert(rU256::from_limbs(key.0), rU256::from_limbs(value.0));
                }
                result.insert(rB160::from(address), account_storage);
            }
            result
        })
    }

    fn revm_gas_limit(&self) -> Option<u64> {
        // In this case we don't need to convert. The method is here just for consistency.
        self.gas_limit
    }

    fn revm_block_number(&self) -> rU256 {
        rU256::from_limbs([self.block_number, 0, 0, 0])
    }

    fn revm_timestamp(&self) -> rU256 {
        rU256::from_limbs([self.timestamp, 0, 0, 0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::{
        abi::parse_abi,
        prelude::BaseContract,
        providers::{Http, Provider, ProviderError},
        types::{Address, U256},
    };
    use revm::{
        db::DatabaseRef,
        primitives::{
            bytes, hex, Account, AccountInfo, AccountStatus, Bytecode, Eval, ExecutionResult, Halt,
            InvalidTransaction, OutOfGasError, Output, ResultAndState, State as rState,
            StorageSlot, B160, B256,
        },
    };
    use std::{error::Error, str::FromStr, sync::Arc, time::Instant};

    #[test]
    fn test_converting_to_revm() {
        let address_string = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D";
        let params = SimulationParameters {
            caller: Address::from_str(address_string).unwrap(),
            to: Address::from_str(address_string).unwrap(),
            data: Bytes::from_static(b"Hello"),
            value: U256::from(123),
            overrides: Some(
                [(
                    Address::zero(),
                    [(U256::from(1), U256::from(11)), (U256::from(2), U256::from(22))]
                        .iter()
                        .cloned()
                        .collect(),
                )]
                .iter()
                .cloned()
                .collect(),
            ),
            gas_limit: Some(33),
            block_number: 0,
            timestamp: 0,
        };

        assert_eq!(params.revm_caller(), rB160::from_str(address_string).unwrap());
        assert_eq!(
            if let TransactTo::Call(value) = params.revm_to() { value } else { panic!() },
            rB160::from_str(address_string).unwrap()
        );
        assert_eq!(params.revm_data(), revm::primitives::Bytes::from_static(b"Hello"));
        assert_eq!(params.revm_value(), rU256::from_str("123").unwrap());
        // Below I am using `from_str` instead of `from`, because `from` for this type gives
        // an ugly false positive error in Pycharm.
        let expected_overrides = [(
            rB160::zero(),
            [
                (rU256::from_str("1").unwrap(), rU256::from_str("11").unwrap()),
                (rU256::from_str("2").unwrap(), rU256::from_str("22").unwrap()),
            ]
            .iter()
            .cloned()
            .collect(),
        )]
        .iter()
        .cloned()
        .collect();
        assert_eq!(params.revm_overrides().unwrap(), expected_overrides);
        assert_eq!(params.revm_gas_limit().unwrap(), 33_u64);
        assert_eq!(params.revm_block_number(), rU256::ZERO);
        assert_eq!(params.revm_timestamp(), rU256::ZERO);
    }

    #[test]
    fn test_converting_nones_to_revm() {
        let params = SimulationParameters {
            caller: Address::zero(),
            to: Address::zero(),
            data: Bytes::new(),
            value: U256::zero(),
            overrides: None,
            gas_limit: None,
            block_number: 0,
            timestamp: 0,
        };

        assert_eq!(params.revm_overrides(), None);
        assert_eq!(params.revm_gas_limit(), None);
    }

    #[test]
    fn test_interpret_result_ok_success() {
        let evm_result: EVMResult<ProviderError> = Ok(ResultAndState {
            result: ExecutionResult::Success {
                reason: Eval::Return,
                gas_used: 100_u64,
                gas_refunded: 10_u64,
                logs: Vec::new(),
                output: Output::Call(bytes::Bytes::from_static(b"output")),
            },
            state: [(
                // storage has changed
                rB160::from(Address::zero()),
                Account {
                    info: AccountInfo {
                        balance: rU256::from_limbs([1, 0, 0, 0]),
                        nonce: 2,
                        code_hash: B256::zero(),
                        code: None,
                    },
                    storage: [
                        // this slot has changed
                        (
                            rU256::from_limbs([3, 1, 0, 0]),
                            StorageSlot {
                                previous_or_original_value: rU256::from_limbs([4, 0, 0, 0]),
                                present_value: rU256::from_limbs([5, 0, 0, 0]),
                            },
                        ),
                        // this slot hasn't changed
                        (
                            rU256::from_limbs([3, 2, 0, 0]),
                            StorageSlot {
                                previous_or_original_value: rU256::from_limbs([4, 0, 0, 0]),
                                present_value: rU256::from_limbs([4, 0, 0, 0]),
                            },
                        ),
                    ]
                    .iter()
                    .cloned()
                    .collect(),
                    status: AccountStatus::Touched,
                },
            )]
            .iter()
            .cloned()
            .collect(),
        });

        let result = interpret_evm_result(evm_result);
        let simulation_result = result.unwrap();

        assert_eq!(simulation_result.result, bytes::Bytes::from_static(b"output"));
        let expected_state_updates = [(
            Address::zero(),
            StateUpdate {
                storage: Some(
                    [(rU256::from_limbs([3, 1, 0, 0]), rU256::from_limbs([5, 0, 0, 0]))]
                        .iter()
                        .cloned()
                        .collect(),
                ),
                balance: Some(rU256::from_limbs([1, 0, 0, 0])),
            },
        )]
        .iter()
        .cloned()
        .collect();
        assert_eq!(simulation_result.state_updates, expected_state_updates);
        assert_eq!(simulation_result.gas_used, 90);
    }

    #[test]
    fn test_interpret_result_ok_revert() {
        let evm_result: EVMResult<ProviderError> = Ok(ResultAndState {
            result: ExecutionResult::Revert {
                gas_used: 100_u64,
                output: bytes::Bytes::from_static(b"output"),
            },
            state: rState::new(),
        });

        let result = interpret_evm_result(evm_result);

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationError::TransactionError { data: _, gas_used } => {
                assert_eq!(
                    format!("0x{}", hex::encode::<Vec<u8>>("output".into())),
                    "0x6f7574707574"
                );
                assert_eq!(gas_used, Some(100));
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_interpret_result_ok_halt() {
        let evm_result: EVMResult<ProviderError> = Ok(ResultAndState {
            result: ExecutionResult::Halt {
                reason: Halt::OutOfGas(OutOfGasError::BasicOutOfGas),
                gas_used: 100_u64,
            },
            state: rState::new(),
        });

        let result = interpret_evm_result(evm_result);

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationError::TransactionError { data, gas_used } => {
                assert_eq!(data, "OutOfGas(BasicOutOfGas)");
                assert_eq!(gas_used, Some(100));
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_interpret_result_err_invalid_transaction() {
        let evm_result: EVMResult<ProviderError> =
            Err(EVMError::Transaction(InvalidTransaction::GasMaxFeeGreaterThanPriorityFee));

        let result = interpret_evm_result(evm_result);

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationError::TransactionError { data, gas_used } => {
                assert_eq!(data, "EVM error: GasMaxFeeGreaterThanPriorityFee");
                assert_eq!(gas_used, None);
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_interpret_result_err_db_error() {
        let evm_result: EVMResult<ProviderError> =
            Err(EVMError::Database(ProviderError::CustomError("boo".to_string())));

        let result = interpret_evm_result(evm_result);

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            SimulationError::StorageError(msg) => {
                assert_eq!(msg, "Storage error: CustomError(\"boo\")")
            }
            _ => panic!("Wrong type of SimulationError!"),
        }
    }

    #[test]
    fn test_integration_revm_v2_swap() -> Result<(), Box<dyn Error>> {
        let client = Provider::<Http>::try_from(
            "https://eth-mainnet.g.alchemy.com/v2/OTD5W7gdTPrzpVot41Lx9tJD9LUiAhbs",
        )
        .unwrap();
        let client = Arc::new(client);
        let runtime = tokio::runtime::Handle::try_current()
            .is_err()
            .then(|| tokio::runtime::Runtime::new().unwrap())
            .unwrap();
        let state = database::SimulationDB::new(client, Some(Arc::new(runtime)), None);

        // any random address will work
        let caller = Address::from_str("0x0000000000000000000000000000000000000000")?;
        let router_addr = Address::from_str("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D")?;
        let router_abi = BaseContract::from(
        parse_abi(&[
            "function getAmountsOut(uint amountIn, address[] memory path) public view returns (uint[] memory amounts)",
        ])?
        );
        let weth_addr = Address::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?;
        let usdc_addr = Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?;
        let encoded = router_abi
            .encode("getAmountsOut", (U256::from(100_000_000), vec![usdc_addr, weth_addr]))
            .unwrap();

        let sim_params = SimulationParameters {
            caller,
            to: router_addr,
            data: encoded,
            value: U256::zero(),
            overrides: None,
            gas_limit: None,
            block_number: 0,
            timestamp: 0,
        };
        let eng = SimulationEngine { state, trace: false };

        let result = eng.simulate(&sim_params);

        let amounts_out = match result {
            Ok(SimulationResult { result, .. }) => {
                router_abi.decode_output::<Vec<U256>, _>("getAmountsOut", result)?
            }
            _ => panic!("Execution reverted!"),
        };

        println!(
            "Swap yielded {} WETH",
            amounts_out
                .last()
                .expect("Empty decoding result")
        );

        let start = Instant::now();
        let n_iter = 1000;
        for _ in 0..n_iter {
            eng.simulate(&sim_params).unwrap();
        }
        let duration = start.elapsed();

        println!("Using revm:");
        println!("Total Duration [n_iter={n_iter}]: {:?}", duration);
        println!("Single get_amount_out call: {:?}", duration / n_iter);

        Ok(())
    }

    #[test]
    fn test_contract_deployment() -> Result<(), Box<dyn Error>> {
        let readonly_state = database::SimulationDB::new(
            Arc::new(
                Provider::<Http>::try_from(
                    "https://eth-mainnet.g.alchemy.com/v2/OTD5W7gdTPrzpVot41Lx9tJD9LUiAhbs",
                )
                .unwrap(),
            ),
            Some(Arc::new(
                tokio::runtime::Handle::try_current()
                    .is_err()
                    .then(|| tokio::runtime::Runtime::new().unwrap())
                    .unwrap(),
            )),
            None,
        );
        let state = database::SimulationDB::new(
            Arc::new(
                Provider::<Http>::try_from(
                    "https://eth-mainnet.g.alchemy.com/v2/OTD5W7gdTPrzpVot41Lx9tJD9LUiAhbs",
                )
                .unwrap(),
            ),
            Some(Arc::new(
                tokio::runtime::Handle::try_current()
                    .is_err()
                    .then(|| tokio::runtime::Runtime::new().unwrap())
                    .unwrap(),
            )),
            None,
        );

        let erc20_abi = BaseContract::from(parse_abi(&[
            "function balanceOf(address account) public view virtual returns (uint256)",
        ])?);
        let usdt_address = B160::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();
        let _ = readonly_state
            .basic(usdt_address)
            .unwrap()
            .unwrap();
        let eoa_address = Address::from_str("0xDFd5293D8e347dFe59E90eFd55b2956a1343963d")?;

        // let deploy_bytecode = std::fs::read(
        //     "/home/mdank/repos/datarevenue/DEFI/defibot-solver/defibot/swaps/pool_state/dodo/
        // compiled/ERC20.bin-runtime" ).unwrap();
        // let deploy_bytecode = revm::precompile::Bytes::from(mocked_bytecode);
        let _ = revm::precompile::Bytes::from(hex::decode("608060405234801562000010575f80fd5b5060405162000a6b38038062000a6b83398101604081905262000033916200012c565b600362000041848262000237565b50600462000050838262000237565b506005805460ff191660ff9290921691909117905550620002ff9050565b634e487b7160e01b5f52604160045260245ffd5b5f82601f83011262000092575f80fd5b81516001600160401b0380821115620000af57620000af6200006e565b604051601f8301601f19908116603f01168101908282118183101715620000da57620000da6200006e565b81604052838152602092508683858801011115620000f6575f80fd5b5f91505b83821015620001195785820183015181830184015290820190620000fa565b5f93810190920192909252949350505050565b5f805f606084860312156200013f575f80fd5b83516001600160401b038082111562000156575f80fd5b620001648783880162000082565b945060208601519150808211156200017a575f80fd5b50620001898682870162000082565b925050604084015160ff81168114620001a0575f80fd5b809150509250925092565b600181811c90821680620001c057607f821691505b602082108103620001df57634e487b7160e01b5f52602260045260245ffd5b50919050565b601f82111562000232575f81815260208120601f850160051c810160208610156200020d5750805b601f850160051c820191505b818110156200022e5782815560010162000219565b5050505b505050565b81516001600160401b038111156200025357620002536200006e565b6200026b81620002648454620001ab565b84620001e5565b602080601f831160018114620002a1575f8415620002895750858301515b5f19600386901b1c1916600185901b1785556200022e565b5f85815260208120601f198616915b82811015620002d157888601518255948401946001909101908401620002b0565b5085821015620002ef57878501515f19600388901b60f8161c191681555b5050505050600190811b01905550565b61075e806200030d5f395ff3fe608060405234801561000f575f80fd5b50600436106100a6575f3560e01c8063395093511161006e578063395093511461011f57806370a082311461013257806395d89b411461015a578063a457c2d714610162578063a9059cbb14610175578063dd62ed3e14610188575f80fd5b806306fdde03146100aa578063095ea7b3146100c857806318160ddd146100eb57806323b872dd146100fd578063313ce56714610110575b5f80fd5b6100b261019b565b6040516100bf91906105b9565b60405180910390f35b6100db6100d636600461061f565b61022b565b60405190151581526020016100bf565b6002545b6040519081526020016100bf565b6100db61010b366004610647565b610244565b604051601281526020016100bf565b6100db61012d36600461061f565b610267565b6100ef610140366004610680565b6001600160a01b03165f9081526020819052604090205490565b6100b2610288565b6100db61017036600461061f565b610297565b6100db61018336600461061f565b6102f2565b6100ef6101963660046106a0565b6102ff565b6060600380546101aa906106d1565b80601f01602080910402602001604051908101604052809291908181526020018280546101d6906106d1565b80156102215780601f106101f857610100808354040283529160200191610221565b820191905f5260205f20905b81548152906001019060200180831161020457829003601f168201915b5050505050905090565b5f33610238818585610329565b60019150505b92915050565b5f336102518582856103dc565b61025c85858561043e565b506001949350505050565b5f3361023881858561027983836102ff565b6102839190610709565b610329565b6060600480546101aa906106d1565b5f33816102a482866102ff565b9050838110156102e557604051632983c0c360e21b81526001600160a01b038616600482015260248101829052604481018590526064015b60405180910390fd5b61025c8286868403610329565b5f3361023881858561043e565b6001600160a01b039182165f90815260016020908152604080832093909416825291909152205490565b6001600160a01b0383166103525760405163e602df0560e01b81525f60048201526024016102dc565b6001600160a01b03821661037b57604051634a1406b160e11b81525f60048201526024016102dc565b6001600160a01b038381165f8181526001602090815260408083209487168084529482529182902085905590518481527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b92591015b60405180910390a3505050565b5f6103e784846102ff565b90505f198114610438578181101561042b57604051637dc7a0d960e11b81526001600160a01b038416600482015260248101829052604481018390526064016102dc565b6104388484848403610329565b50505050565b6001600160a01b03831661046757604051634b637e8f60e11b81525f60048201526024016102dc565b6001600160a01b0382166104905760405163ec442f0560e01b81525f60048201526024016102dc565b61049b8383836104a0565b505050565b6001600160a01b0383166104ca578060025f8282546104bf9190610709565b9091555061053a9050565b6001600160a01b0383165f908152602081905260409020548181101561051c5760405163391434e360e21b81526001600160a01b038516600482015260248101829052604481018390526064016102dc565b6001600160a01b0384165f9081526020819052604090209082900390555b6001600160a01b03821661055657600280548290039055610574565b6001600160a01b0382165f9081526020819052604090208054820190555b816001600160a01b0316836001600160a01b03167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef836040516103cf91815260200190565b5f6020808352835180828501525f5b818110156105e4578581018301518582016040015282016105c8565b505f604082860101526040601f19601f8301168501019250505092915050565b80356001600160a01b038116811461061a575f80fd5b919050565b5f8060408385031215610630575f80fd5b61063983610604565b946020939093013593505050565b5f805f60608486031215610659575f80fd5b61066284610604565b925061067060208501610604565b9150604084013590509250925092565b5f60208284031215610690575f80fd5b61069982610604565b9392505050565b5f80604083850312156106b1575f80fd5b6106ba83610604565b91506106c860208401610604565b90509250929050565b600181811c908216806106e557607f821691505b60208210810361070357634e487b7160e01b5f52602260045260245ffd5b50919050565b8082018082111561023e57634e487b7160e01b5f52601160045260245ffdfea2646970667358221220dfc123d5852c9246ea16b645b377b4436e2f778438195cc6d6c435e8c73a20e764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000012000000000000000000000000000000000000000000000000000000000000000961737320746f6b656e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000034153530000000000000000000000000000000000000000000000000000000000")?);

        let onchain_bytecode = revm::precompile::Bytes::from(hex::decode("608060405234801561000f575f80fd5b50600436106100a6575f3560e01c8063395093511161006e578063395093511461011f57806370a082311461013257806395d89b411461015a578063a457c2d714610162578063a9059cbb14610175578063dd62ed3e14610188575f80fd5b806306fdde03146100aa578063095ea7b3146100c857806318160ddd146100eb57806323b872dd146100fd578063313ce56714610110575b5f80fd5b6100b261019b565b6040516100bf91906105b9565b60405180910390f35b6100db6100d636600461061f565b61022b565b60405190151581526020016100bf565b6002545b6040519081526020016100bf565b6100db61010b366004610647565b610244565b604051601281526020016100bf565b6100db61012d36600461061f565b610267565b6100ef610140366004610680565b6001600160a01b03165f9081526020819052604090205490565b6100b2610288565b6100db61017036600461061f565b610297565b6100db61018336600461061f565b6102f2565b6100ef6101963660046106a0565b6102ff565b6060600380546101aa906106d1565b80601f01602080910402602001604051908101604052809291908181526020018280546101d6906106d1565b80156102215780601f106101f857610100808354040283529160200191610221565b820191905f5260205f20905b81548152906001019060200180831161020457829003601f168201915b5050505050905090565b5f33610238818585610329565b60019150505b92915050565b5f336102518582856103dc565b61025c85858561043e565b506001949350505050565b5f3361023881858561027983836102ff565b6102839190610709565b610329565b6060600480546101aa906106d1565b5f33816102a482866102ff565b9050838110156102e557604051632983c0c360e21b81526001600160a01b038616600482015260248101829052604481018590526064015b60405180910390fd5b61025c8286868403610329565b5f3361023881858561043e565b6001600160a01b039182165f90815260016020908152604080832093909416825291909152205490565b6001600160a01b0383166103525760405163e602df0560e01b81525f60048201526024016102dc565b6001600160a01b03821661037b57604051634a1406b160e11b81525f60048201526024016102dc565b6001600160a01b038381165f8181526001602090815260408083209487168084529482529182902085905590518481527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b92591015b60405180910390a3505050565b5f6103e784846102ff565b90505f198114610438578181101561042b57604051637dc7a0d960e11b81526001600160a01b038416600482015260248101829052604481018390526064016102dc565b6104388484848403610329565b50505050565b6001600160a01b03831661046757604051634b637e8f60e11b81525f60048201526024016102dc565b6001600160a01b0382166104905760405163ec442f0560e01b81525f60048201526024016102dc565b61049b8383836104a0565b505050565b6001600160a01b0383166104ca578060025f8282546104bf9190610709565b9091555061053a9050565b6001600160a01b0383165f908152602081905260409020548181101561051c5760405163391434e360e21b81526001600160a01b038516600482015260248101829052604481018390526064016102dc565b6001600160a01b0384165f9081526020819052604090209082900390555b6001600160a01b03821661055657600280548290039055610574565b6001600160a01b0382165f9081526020819052604090208054820190555b816001600160a01b0316836001600160a01b03167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef836040516103cf91815260200190565b5f6020808352835180828501525f5b818110156105e4578581018301518582016040015282016105c8565b505f604082860101526040601f19601f8301168501019250505092915050565b80356001600160a01b038116811461061a575f80fd5b919050565b5f8060408385031215610630575f80fd5b61063983610604565b946020939093013593505050565b5f805f60608486031215610659575f80fd5b61066284610604565b925061067060208501610604565b9150604084013590509250925092565b5f60208284031215610690575f80fd5b61069982610604565b9392505050565b5f80604083850312156106b1575f80fd5b6106ba83610604565b91506106c860208401610604565b90509250929050565b600181811c908216806106e557607f821691505b60208210810361070357634e487b7160e01b5f52602260045260245ffd5b50919050565b8082018082111561023e57634e487b7160e01b5f52601160045260245ffdfea2646970667358221220dfc123d5852c9246ea16b645b377b4436e2f778438195cc6d6c435e8c73a20e764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000000000")?);
        let code = Bytecode::new_raw(onchain_bytecode);
        let contract_acc_info = AccountInfo::new(
            rU256::from(0),
            0,
            code.hash_slow(),
            code,
            // true_usdt.code.unwrap(),
        );
        // Adding permanent storage for balance
        let mut storage = HashMap::default();
        storage.insert(
            rU256::from_str(
                "25842306973167774731510882590667189188844731550465818811072464953030320818263",
            )
            .unwrap(),
            rU256::from_str("25").unwrap(),
        );
        // TODO: mock a balance (and approval)
        // let mut permanent_storage = HashMap::new();
        // permanent_storage.insert(s)
        state.init_account(usdt_address, contract_acc_info, Some(storage), true);

        // DEPLOY A CONTRACT TO GET ON-CHAIN BYTECODE
        // let deployment_account = B160::from_str("0x0000000000000000000000000000000000000123")?;
        // state.init_account(
        //     deployment_account,
        //     AccountInfo::new(rU256::MAX, 0, Bytecode::default()),
        //     None,
        //     true,
        // );
        // let deployment_params = SimulationParameters {
        //     caller: Address::from(deployment_account),
        //     to: Address::zero(),
        //     data: Bytes::from(deploy_bytecode),
        //     value: U256::zero(),
        //     overrides: None,
        //     gas_limit: None,
        // };

        // prepare balanceOf
        // let deployed_contract_address =
        // B160::from_str("0x5450b634edf901a95af959c99c058086a51836a8")?; Adding overwrite
        // for balance
        let mut overrides = HashMap::default();
        let mut storage_overwrite = HashMap::default();
        storage_overwrite.insert(
            U256::from_dec_str(
                "25842306973167774731510882590667189188844731550465818811072464953030320818263",
            )
            .unwrap(),
            U256::from_dec_str("80").unwrap(),
        );
        overrides.insert(Address::from(usdt_address), storage_overwrite);

        let calldata = erc20_abi
            .encode("balanceOf", eoa_address)
            .unwrap();
        let sim_params = SimulationParameters {
            caller: Address::from_str("0x0000000000000000000000000000000000000000")?,
            to: Address::from(usdt_address),
            // to: Address::from(deployed_contract_address),
            data: calldata,
            value: U256::zero(),
            overrides: Some(overrides),
            gas_limit: None,
            block_number: 0,
            timestamp: 0,
        };

        let eng = SimulationEngine { state, trace: false };

        // println!("Deploying a mocked contract!");
        // let deployment_result = eng.simulate(&deployment_params);
        // match deployment_result {
        //     Ok(SimulationResult { result, state_updates, gas_used }) => {
        //         println!("Deployment result: {:?}", result);
        //         println!("Used gas: {:?}", gas_used);
        //         println!("{:?}", state_updates);
        //     }
        //     Err(error) => panic!("{:?}", error),
        // };

        println!("Executing balanceOf");
        let result = eng.simulate(&sim_params);
        let balance = match result {
            Ok(SimulationResult { result, .. }) => {
                erc20_abi.decode_output::<U256, _>("balanceOf", result)?
            }
            Err(error) => panic!("{:?}", error),
        };
        println!("Balance: {}", balance);

        Ok(())
    }
}
