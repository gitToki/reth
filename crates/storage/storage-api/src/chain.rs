use crate::{DBProvider, StorageLocation};
use alloc::vec::Vec;
use alloy_consensus::Header;
use alloy_primitives::BlockNumber;
use core::marker::PhantomData;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW},
    models::StoredBlockOmmers,
    tables,
    transaction::{DbTx, DbTxMut},
    DbTxUnwindExt,
};
use reth_db_models::StoredBlockWithdrawals;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::{
    Block, BlockBody, FullBlockHeader, FullNodePrimitives, SignedTransaction,
};
use reth_storage_errors::provider::ProviderResult;

/// Trait that implements how block bodies are written to the storage.
///
/// Note: Within the current abstraction, this should only write to tables unrelated to
/// transactions. Writing of transactions is handled separately.
#[auto_impl::auto_impl(&, Arc)]
pub trait BlockBodyWriter<Provider, Body: BlockBody> {
    /// Writes a set of block bodies to the storage.
    fn write_block_bodies(
        &self,
        provider: &Provider,
        bodies: Vec<(BlockNumber, Option<Body>)>,
        write_to: StorageLocation,
    ) -> ProviderResult<()>;

    /// Removes all block bodies above the given block number from the database.
    fn remove_block_bodies_above(
        &self,
        provider: &Provider,
        block: BlockNumber,
        remove_from: StorageLocation,
    ) -> ProviderResult<()>;
}

/// Trait that implements how chain-specific types are written to the storage.
pub trait ChainStorageWriter<Provider, Primitives: FullNodePrimitives>:
    BlockBodyWriter<Provider, <Primitives::Block as Block>::Body>
{
}
impl<T, Provider, Primitives: FullNodePrimitives> ChainStorageWriter<Provider, Primitives> for T where
    T: BlockBodyWriter<Provider, <Primitives::Block as Block>::Body>
{
}

/// Input for reading a block body. Contains a header of block being read and a list of pre-fetched
/// transactions.
pub type ReadBodyInput<'a, B> =
    (&'a <B as Block>::Header, Vec<<<B as Block>::Body as BlockBody>::Transaction>);

/// Trait that implements how block bodies are read from the storage.
///
/// Note: Within the current abstraction, transactions persistence is handled separately, thus this
/// trait is provided with transactions read beforehand and is expected to construct the block body
/// from those transactions and additional data read from elsewhere.
#[auto_impl::auto_impl(&, Arc)]
pub trait BlockBodyReader<Provider> {
    /// The block type.
    type Block: Block;

    /// Receives a list of block headers along with block transactions and returns the block bodies.
    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>>;
}

/// Trait that implements how chain-specific types are read from storage.
pub trait ChainStorageReader<Provider, Primitives: FullNodePrimitives>:
    BlockBodyReader<Provider, Block = Primitives::Block>
{
}
impl<T, Provider, Primitives: FullNodePrimitives> ChainStorageReader<Provider, Primitives> for T where
    T: BlockBodyReader<Provider, Block = Primitives::Block>
{
}

/// Ethereum storage implementation.
#[derive(Debug, Clone, Copy)]
pub struct EthStorage<T = TransactionSigned, H = Header>(PhantomData<(T, H)>);

impl<T, H> Default for EthStorage<T, H> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<Provider, T, H> BlockBodyWriter<Provider, alloy_consensus::BlockBody<T, H>>
    for EthStorage<T, H>
where
    Provider: DBProvider<Tx: DbTxMut>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    fn write_block_bodies(
        &self,
        provider: &Provider,
        bodies: Vec<(u64, Option<alloy_consensus::BlockBody<T, H>>)>,
        _write_to: StorageLocation,
    ) -> ProviderResult<()> {
        let mut ommers_cursor = provider.tx_ref().cursor_write::<tables::BlockOmmers<H>>()?;
        let mut withdrawals_cursor =
            provider.tx_ref().cursor_write::<tables::BlockWithdrawals>()?;

        for (block_number, body) in bodies {
            let Some(body) = body else { continue };

            // Write ommers if any
            if !body.ommers.is_empty() {
                ommers_cursor.append(block_number, &StoredBlockOmmers { ommers: body.ommers })?;
            }

            // Write withdrawals if any
            if let Some(withdrawals) = body.withdrawals {
                if !withdrawals.is_empty() {
                    withdrawals_cursor
                        .append(block_number, &StoredBlockWithdrawals { withdrawals })?;
                }
            }
        }

        Ok(())
    }

    fn remove_block_bodies_above(
        &self,
        provider: &Provider,
        block: BlockNumber,
        _remove_from: StorageLocation,
    ) -> ProviderResult<()> {
        provider.tx_ref().unwind_table_by_num::<tables::BlockWithdrawals>(block)?;
        provider.tx_ref().unwind_table_by_num::<tables::BlockOmmers>(block)?;

        Ok(())
    }
}

impl<Provider, T, H> BlockBodyReader<Provider> for EthStorage<T, H>
where
    Provider: DBProvider + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    type Block = alloy_consensus::Block<T, H>;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>> {
        // TODO: Ideally storage should hold its own copy of chain spec
        let chain_spec = provider.chain_spec();

        let mut withdrawals_cursor = provider.tx_ref().cursor_read::<tables::BlockWithdrawals>()?;

        let mut bodies = Vec::with_capacity(inputs.len());

        for (header, transactions) in inputs {
            // If we are past shanghai, then all blocks should have a withdrawal list,
            // even if empty
            let withdrawals = if chain_spec.is_shanghai_active_at_timestamp(header.timestamp()) {
                withdrawals_cursor
                    .seek_exact(header.number())?
                    .map(|(_, w)| w.withdrawals)
                    .unwrap_or_default()
                    .into()
            } else {
                None
            };
            let ommers = if chain_spec.is_paris_active_at_block(header.number()) {
                Vec::new()
            } else {
                // Pre-merge: fetch ommers from database using direct database access
                provider
                    .tx_ref()
                    .cursor_read::<tables::BlockOmmers<H>>()?
                    .seek_exact(header.number())?
                    .map(|(_, stored_ommers)| stored_ommers.ommers)
                    .unwrap_or_default()
            };
            bodies.push(alloy_consensus::BlockBody { transactions, ommers, withdrawals });
        }

        Ok(bodies)
    }
}
