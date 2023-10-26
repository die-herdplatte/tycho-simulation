use rpc_state_reader::rpc_state::BlockValue;
use starknet_api::block::BlockNumber;
use starknet_in_rust::state::state_api::State;
use std::{collections::HashMap, path::Path, sync::Arc};

use cairo_vm::felt::Felt252;
use starknet_in_rust::{
    core::contract_address::{compute_casm_class_hash, compute_deprecated_class_hash},
    definitions::block_context::BlockContext,
    execution::{
        execution_entry_point::{ExecutionEntryPoint, ExecutionResult},
        CallType, TransactionExecutionContext,
    },
    services::api::contract_classes::{
        compiled_class::CompiledClass, deprecated_contract_class::ContractClass,
    },
    state::{
        cached_state::CachedState,
        state_api::StateReader,
        state_cache::{StateCache, StorageEntry},
        ExecutionResourcesManager,
    },
    utils::{calculate_sn_keccak, felt_to_hash, Address, ClassHash, CompiledClassHash},
    CasmContractClass, EntryPointType,
};
use thiserror::Error;

use super::rpc_reader::RpcStateReader;

#[derive(Error, Debug, PartialEq)]
pub enum SimulationError {
    #[error("Failed to initialise contracts: {0}")]
    InitError(String),
    #[error("ContractState is already initialized: {0}")]
    AlreadyInitialized(String),
    #[error("Override Starknet state failed: {0}")]
    OverrideError(String),
    /// Simulation didn't succeed; likely not related to network, so retrying won't help
    #[error("Simulated transaction failed: {0}")]
    TransactionError(String),
    /// Error reading state
    #[error("Accessing contract state failed: {0}")]
    StateError(String),
}

pub type StorageHash = [u8; 32];
pub type Overrides = HashMap<StorageHash, Felt252>;

#[derive(Debug, Clone)]
pub struct SimulationParameters {
    /// Address of the sending account
    pub caller: Address,
    /// Address of the receiving account/contract
    pub to: Address,
    /// Calldata
    pub data: Vec<Felt252>,
    /// The contract function/entry point to call e.g. "transfer"
    pub entry_point: String,
    /// Starknet state overrides.
    /// Will be merged with the existing state. Will take effect only for current simulation.
    /// Must be given as a contract address to its variable override map.
    pub overrides: Option<HashMap<Address, Overrides>>,
    /// Limit of gas to be used by the transaction
    pub gas_limit: Option<u128>,
    /// The block number to be used by the transaction. This is independent of the states block.
    pub block_number: u64,
}

impl SimulationParameters {
    pub fn new(
        caller: Address,
        to: Address,
        data: Vec<Felt252>,
        entry_point: String,
        overrides: Option<HashMap<Address, Overrides>>,
        gas_limit: Option<u128>,
        block_number: u64,
    ) -> Self {
        Self { caller, to, data, entry_point, overrides, gas_limit, block_number }
    }
}

#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Output of transaction execution
    pub result: Vec<Felt252>,
    /// State changes caused by the transaction
    pub state_updates: HashMap<Address, Overrides>,
    /// Gas used by the transaction (already reduced by the refunded gas)
    pub gas_used: u128,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct SimulationEngine<SR: StateReader> {
    state: CachedState<SR>,
}

/**
 * Loads a Starknet contract from a given file path and returns it as a `CompiledClass` enum.
 *
 * # Arguments
 *
 * * `path: impl AsRef<Path>` - A path to the contract file.
 *
 * # Returns
 *
 * * `Ok(CompiledClass)` - The loaded contract as a `CompiledClass` enum.
 * * `Err(Box<dyn std::error::Error>)` - An error indicating the reason for the failure.
 *
 * # Contract Formats
 *
 * Starknet contracts can be represented in two main formats: `.casm` and `.json`.
 * You can read more about these formats in the [Starknet documentation](https://docs.starknet.io/documentation/architecture_and_concepts/Smart_Contracts/cairo-and-sierra/).
 *
 * ## .json Format (Cairo 0)
 *
 * * This format is older and represents Cairo 0 contracts. It is in JSON format, but sometimes
 *   for clarity it is given the `.sierra` extension.
 *
 * ## .casm Format (Cairo 1 / Cairo 2)
 *
 * * This format is newer and is used for Cairo 1 and Cairo 2 contracts.
 *
 * If the file extension is neither `.casm` nor `.json`, the function will return an `Err`
 * indicating an unsupported file type.
 */
fn load_compiled_class_from_path(
    path: impl AsRef<Path>,
) -> Result<CompiledClass, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(&path)?;

    match path
        .as_ref()
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("casm") => {
            // Parse and load .casm file
            let casm_contract_class: CasmContractClass = serde_json::from_str(&contents)?;
            Ok(CompiledClass::Casm(Arc::new(casm_contract_class)))
        }
        Some("json") => {
            // Deserialize the .json file
            let contract_class: ContractClass = ContractClass::from_path(&path)?;
            Ok(CompiledClass::Deprecated(Arc::new(contract_class)))
        }
        _ => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Unsupported file type",
        ))),
    }
}

/// Computes the class hash of a given contract.
///
/// # Arguments
///
/// * `compiled_class: &CompiledClass` - The contract to compute the class hash of.
///
/// # Returns
///
/// * `Result<Felt252, Box<dyn std::error::Error>>` - The computed class hash.
fn compute_class_hash(
    compiled_class: &CompiledClass,
) -> Result<Felt252, Box<dyn std::error::Error>> {
    match compiled_class {
        CompiledClass::Casm(casm_contract_class) => {
            let class_hash = compute_casm_class_hash(casm_contract_class)?;
            Ok(class_hash)
        }
        CompiledClass::Deprecated(contract_class) => {
            let class_hash = compute_deprecated_class_hash(contract_class)?;
            Ok(class_hash)
        }
    }
}

/// A struct with metadata about a contract to be initialized.
///
/// # Fields
///
/// * `contract_address: Address` - The address of the contract.
/// * `class_hash: ClassHash` - The class hash of the contract.
/// * `path: Option<String>` - The path to the contract file. If `None`, the contract is going to be
///   fetched from the state reader.
/// * `storage_overrides: Option<HashMap<StorageEntry, Felt252>>` - The storage overrides for the
///   contract.
#[derive(Debug, Clone)]
pub struct ContractOverride {
    pub contract_address: Address,
    pub class_hash: ClassHash,
    pub path: Option<String>,
    pub storage_overrides: Option<HashMap<StorageEntry, Felt252>>,
}

impl ContractOverride {
    pub fn new(
        contract_address: Address,
        class_hash: ClassHash,
        path: Option<String>,
        storage_overrides: Option<HashMap<StorageEntry, Felt252>>,
    ) -> Self {
        Self { contract_address, class_hash, path, storage_overrides }
    }
}

#[allow(unused_variables)]
#[allow(dead_code)]
impl<SR: StateReader> SimulationEngine<SR> {
    pub fn new(
        rpc_state_reader: Arc<SR>,
        contract_overrides: impl IntoIterator<Item = ContractOverride>,
    ) -> Result<Self, SimulationError> {
        // Prepare initial values
        let mut address_to_class_hash: HashMap<Address, ClassHash> = HashMap::new();
        let mut class_hash_to_compiled_class: HashMap<ClassHash, CompiledClass> = HashMap::new();
        let mut address_to_nonce: HashMap<Address, Felt252> = HashMap::new();
        let mut storage_updates: HashMap<StorageEntry, Felt252> = HashMap::new();

        let mut class_hash_to_compiled_class_hash: HashMap<ClassHash, CompiledClassHash> =
            HashMap::new();

        // Load contracts
        for input_contract in contract_overrides {
            if let Some(path) = input_contract.path {
                let compiled_class = load_compiled_class_from_path(&path).map_err(|e| {
                    SimulationError::InitError(format!(
                        "Failed to load contract from path: {:?} with error: {:?}",
                        path, e
                    ))
                })?;
                let class_hash = input_contract.class_hash;
                // Compute compiled class hash
                let compiled_class_hash = compute_class_hash(&compiled_class).map_err(|e| {
                    SimulationError::InitError(format!(
                        "Failed to compute class hash for contract: {:?} with error: {:?}",
                        path, e
                    ))
                })?;
                // Convert Felt252 to ClassHash
                let compiled_class_hash = felt_to_hash(&compiled_class_hash);

                // Update caches
                address_to_class_hash.insert(input_contract.contract_address.clone(), class_hash);
                class_hash_to_compiled_class.insert(compiled_class_hash, compiled_class.clone());
                address_to_nonce.insert(input_contract.contract_address, Felt252::from(0u8));
                storage_updates.extend(
                    input_contract
                        .storage_overrides
                        .unwrap_or_default(),
                );

                class_hash_to_compiled_class_hash.insert(class_hash, compiled_class_hash);
            }
        }

        // Set StateCache initial values
        let cache: StateCache = StateCache::new(
            address_to_class_hash,
            class_hash_to_compiled_class.clone(),
            address_to_nonce,
            storage_updates,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            class_hash_to_compiled_class_hash,
        );

        // Initialize CachedState contract classes
        let state: CachedState<SR> =
            CachedState::new_for_testing(rpc_state_reader, cache, class_hash_to_compiled_class);

        Ok(Self { state })
    }

    fn interpret_result(
        &self,
        result: ExecutionResult,
    ) -> Result<SimulationResult, SimulationError> {
        todo!()
    }
}

impl SimulationEngine<RpcStateReader> {
    /// Simulate a transaction
    ///
    /// State's block will be modified to be the last block before the simulation's block.
    pub fn simulate(
        &self,
        params: &SimulationParameters,
    ) -> Result<SimulationResult, SimulationError> {
        // Hacky way to set the block number on the cached state
        // Note that because we clone the cache, all RPC calls are thrown away after the simulation
        // TODO: update this after new starknet_in_rust release
        let block_value = BlockValue::Number(BlockNumber(params.block_number));
        let state_reader = self
            .state
            .state_reader
            .with_updated_block(block_value);
        let new_state = CachedState::new_for_testing(
            Arc::new(state_reader),
            self.state.cache().clone(),
            self.state.contract_classes().clone(),
        );
        let mut test_state = new_state.create_transactional();

        // Apply overrides
        if let Some(overrides) = &params.overrides {
            for (address, storage_update) in overrides {
                for (slot, value) in storage_update {
                    let storage_entry = ((*address).clone(), *slot);
                    test_state.set_storage_at(&storage_entry, (*value).clone());
                }
            }
        }

        // Create the simulated call
        let entry_point = params.entry_point.as_bytes();
        let entrypoint_selector = Felt252::from_bytes_be(&calculate_sn_keccak(entry_point));

        let class_hash = test_state
            .get_class_hash_at(&params.to)
            .map_err(|err| SimulationError::StateError(err.to_string()))?;

        let call = ExecutionEntryPoint::new(
            params.to.clone(),
            params.data.clone(),
            entrypoint_selector,
            params.caller.clone(),
            EntryPointType::External,
            Some(CallType::Delegate),
            Some(class_hash),
            params.gas_limit.unwrap_or(0),
        );

        // Set up the call context
        let block_context = BlockContext::default();
        let mut resources_manager = ExecutionResourcesManager::default();
        let mut tx_execution_context = TransactionExecutionContext::default();

        // Execute the simulated call
        let result = call
            .execute(
                &mut test_state,
                &block_context,
                &mut resources_manager,
                &mut tx_execution_context,
                false,
                block_context.invoke_tx_max_n_steps(),
            )
            .map_err(|err| SimulationError::TransactionError(err.to_string()))?;

        // Interpret and return the results
        self.interpret_result(result)
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use dotenv::dotenv;
    use num_traits::Num;
    use rpc_state_reader::rpc_state::{BlockTag, RpcChain, RpcState};
    use rstest::rstest;
    use starknet_in_rust::core::errors::state_errors::StateError;

    pub fn string_to_address(address: &str) -> Address {
        Address(Felt252::from_str_radix(address, 16).expect("hex address"))
    }

    // Mock empty StateReader
    struct StateReaderMock {}

    impl StateReaderMock {
        fn new() -> Self {
            Self {}
        }
    }

    #[allow(unused_variables)]
    #[allow(dead_code)]
    impl StateReader for StateReaderMock {
        fn get_contract_class(&self, class_hash: &ClassHash) -> Result<CompiledClass, StateError> {
            todo!()
        }

        fn get_class_hash_at(&self, contract_address: &Address) -> Result<ClassHash, StateError> {
            todo!()
        }

        fn get_nonce_at(&self, contract_address: &Address) -> Result<Felt252, StateError> {
            todo!()
        }

        fn get_storage_at(&self, storage_entry: &StorageEntry) -> Result<Felt252, StateError> {
            todo!()
        }

        fn get_compiled_class_hash(
            &self,
            class_hash: &ClassHash,
        ) -> Result<CompiledClassHash, StateError> {
            todo!()
        }
    }

    #[rstest]
    #[case::cairo_0("tests/resources/fibonacci.json")]
    #[case::cairo_1("tests/resources/fibonacci.casm")]
    fn test_create_engine_with_contract_from_path(#[case] path: &str) {
        let cargo_manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"));
        dbg!("Cargo manifest path is: {:?}", cargo_manifest_path);

        let path = cargo_manifest_path.join(path);
        dbg!("Contract path is: {:?}", &path);
        let path_str: String = path.to_str().unwrap().to_owned();

        let address: Address = Address(Felt252::from(0u8));
        let input_contract = ContractOverride::new(address, [0u8; 32], Some(path_str), None);
        let rpc_state_reader = Arc::new(StateReaderMock::new());
        let engine_result = SimulationEngine::new(rpc_state_reader, vec![input_contract]);
        if let Err(err) = engine_result {
            panic!("Failed to create engine with error: {:?}", err);
        }
        assert!(engine_result.is_ok());
    }

    // TODO: run after interpret_result is implemented
    #[ignore]
    #[test]
    fn test_simulate() {
        // Ensure the env is set
        if env::var("INFURA_API_KEY").is_err() {
            dotenv().expect("Missing .env file");
        }

        // Initialize the engine
        let rpc_state_reader = Arc::new(RpcStateReader::new(RpcState::new_infura(
            RpcChain::MainNet,
            BlockTag::Latest.into(),
        )));
        let engine = SimulationEngine::new(rpc_state_reader, vec![]).unwrap();

        // Prepare the simulation parameters
        // https://voyager.online/tx/0x33c71da501179ec033b22a8dbf6a30fdcb892609a6a6d48d7577dacdf8af9af
        let params = SimulationParameters::new(
            string_to_address("073317cbd895225d657d08b3bc4791ba7cbc0ca8b84ba554abd2b0db8aff8ed8"), /* Argent */
            string_to_address("073317cbd895225d657d08b3bc4791ba7cbc0ca8b84ba554abd2b0db8aff8ed8"), /* Argent -- smart contract wallet */
            vec![
                Felt252::from_str_radix("2", 16).unwrap(),
                Felt252::from_str_radix(
                    "da114221cb83fa859dbdb4c44beeaa0bb37c7537ad5ae66fe5e0efd20e6eb3",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix(
                    "219209e083275171774dab1df80982e9df2096516f06319c5c6d71ae0a8480c",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("3", 16).unwrap(),
                Felt252::from_str_radix(
                    "4270219d365d6b017231b52e92b3fb5d7c8378b05e9abc97724537a80e93b0f",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("1925db6c672d35d03", 16).unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix(
                    "4270219d365d6b017231b52e92b3fb5d7c8378b05e9abc97724537a80e93b0f",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix(
                    "1171593aa5bdadda4d6b0efde6cc94ee7649c3163d5efeb19da6c16d63a2a63",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("17", 16).unwrap(),
                Felt252::from_str_radix(
                    "da114221cb83fa859dbdb4c44beeaa0bb37c7537ad5ae66fe5e0efd20e6eb3",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("1925db6c672d35d03", 16).unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix(
                    "49d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("38ce1956f23180", 16).unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix("383cad90f4e434", 16).unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix(
                    "73317cbd895225d657d08b3bc4791ba7cbc0ca8b84ba554abd2b0db8aff8ed8",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix("1", 16).unwrap(),
                Felt252::from_str_radix(
                    "da114221cb83fa859dbdb4c44beeaa0bb37c7537ad5ae66fe5e0efd20e6eb3",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix(
                    "49d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix(
                    "5dd3d2f4429af886cd1a3b08289dbcea99a294197e9eb43b0e0325b4b",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("64", 16).unwrap(),
                Felt252::from_str_radix("6", 16).unwrap(),
                Felt252::from_str_radix(
                    "da114221cb83fa859dbdb4c44beeaa0bb37c7537ad5ae66fe5e0efd20e6eb3",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix(
                    "49d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7",
                    16,
                )
                .unwrap(),
                Felt252::from_str_radix("20c49ba5e353f80000000000000000", 16).unwrap(),
                Felt252::from_str_radix("3e8", 16).unwrap(),
                Felt252::from_str_radix("0", 16).unwrap(),
                Felt252::from_str_radix("854bfd1880e80fb8d1eb979bc2390", 16).unwrap(),
            ],
            "__execute__".to_owned(),
            None,
            Some(100000),
            352399,
        );

        // Simulate the transaction
        let result = engine.simulate(&params);

        // Check the result
        if let Err(err) = result {
            panic!("Failed to simulate transaction with error: {:?}", err);
        }
        assert!(result.is_ok());
        dbg!("Simulation result is: {:?}", result.unwrap());
    }
}
