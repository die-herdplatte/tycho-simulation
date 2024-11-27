use std::{collections::HashMap, fmt::Debug};

use lazy_static::lazy_static;
use revm::{
    precompile::Address as rAddress,
    primitives::{AccountInfo, Address, KECCAK_EMPTY},
    DatabaseRef,
};

use crate::{
    evm::{
        engine_db::{
            engine_db_interface::EngineDatabaseInterface, simulation_db::BlockHeader,
            tycho_db::PreCachedDB,
        },
        simulation::SimulationEngine,
        tycho_models::{AccountUpdate, ChangeType, ResponseAccount},
    },
    protocol::errors::SimulationError,
};

lazy_static! {
    pub static ref SHARED_TYCHO_DB: PreCachedDB =
        PreCachedDB::new().expect("Failed to create PreCachedDB");
}

/// Creates a simulation engine.
///
/// # Parameters
///
/// - `trace`: Whether to trace calls. Only meant for debugging purposes, might print a lot of data
///   to stdout.
pub fn create_engine<D: EngineDatabaseInterface + Clone>(
    db: D,
    trace: bool,
) -> Result<SimulationEngine<D>, SimulationError>
where
    <D as EngineDatabaseInterface>::Error: Debug,
    <D as DatabaseRef>::Error: Debug,
{
    let engine = SimulationEngine::new(db.clone(), trace);

    let zero_account_info =
        AccountInfo { balance: Default::default(), nonce: 0, code_hash: KECCAK_EMPTY, code: None };

    // Accounts necessary for enabling pre-compilation are initialized by default.
    engine.state.init_account(
        rAddress::parse_checksummed("0x0000000000000000000000000000000000000000", None)
            .expect("Invalid checksum for precompile-enabling address"),
        zero_account_info.clone(),
        None,
        false,
    );
    engine.state.init_account(
        rAddress::parse_checksummed("0x0000000000000000000000000000000000000004", None)
            .expect("Invalid checksum for precompile-enabling address"),
        zero_account_info.clone(),
        None,
        false,
    );

    Ok(engine)
}

pub async fn update_engine(
    db: PreCachedDB,
    block: BlockHeader,
    vm_storage: Option<HashMap<Address, ResponseAccount>>,
    account_updates: HashMap<Address, AccountUpdate>,
) -> Vec<AccountUpdate> {
    let mut vm_updates: Vec<AccountUpdate> = Vec::new();

    for (_address, account_update) in account_updates.iter() {
        vm_updates.push(account_update.clone());
    }

    if let Some(vm_storage_values) = vm_storage {
        for (_address, vm_storage_values) in vm_storage_values.iter() {
            // ResponseAccount objects to AccountUpdate objects as required by the update method
            vm_updates.push(AccountUpdate {
                address: vm_storage_values.address,
                chain: vm_storage_values.chain,
                slots: vm_storage_values.slots.clone(),
                balance: Some(vm_storage_values.balance),
                code: Some(vm_storage_values.code.clone()),
                change: ChangeType::Creation,
            });
        }
    }

    if !vm_updates.is_empty() {
        db.update(vm_updates.clone(), Some(block));
    }

    vm_updates
}
