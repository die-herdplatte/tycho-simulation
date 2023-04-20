use std::sync::Arc;

use ethers::{
    providers::Middleware,
    types::{H160, H256, U256},
};
use revm::{
    db::DatabaseRef,
    interpreter::analysis::to_analysed,
    primitives::{hash_map, AccountInfo, Bytecode, B160, B256, U256 as rU256},
};

#[derive(Clone)]
pub(crate) struct SlotInfo {
    pub(crate) mutable: bool,
}

pub(crate) type ContractStorageLayout = hash_map::HashMap<U256, SlotInfo>;

pub(crate) type ContractStorageUpdate = hash_map::HashMap<H160, hash_map::HashMap<rU256, rU256>>;

#[derive(Clone)]
pub(crate) struct EthRpcDB<M: Middleware + Clone> {
    pub(crate) client: Arc<M>,
    pub(crate) runtime: Option<Arc<tokio::runtime::Runtime>>,
}

impl<M: Middleware + Clone> EthRpcDB<M> {
    /// internal utility function to call tokio feature and wait for output
    pub(crate) fn block_on<F: core::future::Future>(&self, f: F) -> F::Output {
        // If we get here and have to block the current thread, we really
        // messed up indexing / filling the cache. In that case this will save us
        // at the price of a very high time penalty.
        match &self.runtime {
            Some(runtime) => runtime.block_on(f),
            None => futures::executor::block_on(f),
        }
    }
}

impl<M: Middleware + Clone> DatabaseRef for EthRpcDB<M> {
    type Error = M::Error;

    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        println!("loading basic data {address}!");
        let fut = async {
            tokio::join!(
                self.client.get_balance(H160(address.0), None),
                self.client.get_transaction_count(H160(address.0), None),
                self.client.get_code(H160(address.0), None),
            )
        };

        let (balance, nonce, code) = self.block_on(fut);

        Ok(Some(AccountInfo::new(
            rU256::from_limbs(
                balance
                    .unwrap_or_else(|e| panic!("ethers get balance error: {e:?}"))
                    .0,
            ),
            nonce
                .unwrap_or_else(|e| panic!("ethers get nonce error: {e:?}"))
                .as_u64(),
            to_analysed(Bytecode::new_raw(
                code.unwrap_or_else(|e| panic!("ethers get code error: {e:?}"))
                    .0,
            )),
        )))
    }

    fn code_by_hash(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        panic!("Should not be called. Code is already loaded");
        // not needed because we already load code with basic info
    }

    fn storage(&self, address: B160, index: rU256) -> Result<rU256, Self::Error> {
        println!("Loading storage {address}, {index}");
        let add = H160::from(address.0);
        let index = H256::from(index.to_be_bytes());
        let fut = async {
            let storage = self.client.get_storage_at(add, index, None).await.unwrap();
            rU256::from_be_bytes(storage.to_fixed_bytes())
        };
        Ok(self.block_on(fut))
    }

    fn block_hash(&self, _number: rU256) -> Result<B256, Self::Error> {
        todo!()
    }
}
