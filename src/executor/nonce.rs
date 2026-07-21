//! Local nonce manager with resync. Seed from `getTransactionCount(pending)`;
//! increment locally per send; resync on any gap/error.

use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct NonceManager {
    provider: DynProvider,
    address: Address,
    next: AtomicU64,
}

impl NonceManager {
    pub async fn new(provider: DynProvider, address: Address) -> anyhow::Result<Self> {
        let n = provider.get_transaction_count(address).pending().await?;
        Ok(Self { provider, address, next: AtomicU64::new(n) })
    }

    /// Reserve the next nonce.
    pub fn reserve(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    /// Resync from chain (call after an error or detected gap).
    pub async fn resync(&self) -> anyhow::Result<()> {
        let n = self.provider.get_transaction_count(self.address).pending().await?;
        self.next.store(n, Ordering::SeqCst);
        Ok(())
    }
}
