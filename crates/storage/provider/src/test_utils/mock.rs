use crate::{
    traits::{BlockSource, ReceiptProvider},
    AccountReader, BlockHashReader, BlockIdReader, BlockNumReader, BlockReader, BlockReaderIdExt,
    ChainSpecProvider, ChangeSetReader, EthStorage, HeaderProvider, ReceiptProviderIdExt,
    StateProvider, StateProviderBox, StateProviderFactory, StateReader, StateRootProvider,
    TransactionVariant, TransactionsProvider, WithdrawalsProvider,
};
use alloy_consensus::{
    constants::EMPTY_ROOT_HASH, transaction::TransactionMeta, Header, Transaction,
};
use alloy_eips::{eip4895::Withdrawals, BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy_primitives::{
    keccak256,
    map::{B256Map, HashMap},
    Address, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, TxHash, TxNumber, B256, U256,
};
use parking_lot::Mutex;
use reth_chain_state::{CanonStateNotifications, CanonStateSubscriptions};
use reth_chainspec::{ChainInfo, EthChainSpec};
use reth_db_api::{
    mock::{DatabaseMock, TxMock},
    models::{AccountBeforeTx, StoredBlockBodyIndices},
};
use reth_execution_types::ExecutionOutcome;
use reth_node_types::NodeTypes;
use reth_primitives::{
    Account, Block, Bytecode, EthPrimitives, GotExpected, Receipt, RecoveredBlock, SealedBlock,
    SealedHeader, TransactionSigned,
};
use reth_primitives_traits::SignedTransaction;
use reth_prune_types::PruneModes;
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::{
    BlockBodyIndicesProvider, DBProvider, DatabaseProviderFactory, HashedPostStateProvider,
    NodePrimitivesProvider, OmmersProvider, StageCheckpointReader, StateCommitmentProvider,
    StateProofProvider, StorageRootProvider,
};
use reth_storage_errors::provider::{ConsistentViewError, ProviderError, ProviderResult};
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use reth_trie_db::MerklePatriciaTrie;
use std::{
    collections::BTreeMap,
    ops::{RangeBounds, RangeInclusive},
    sync::Arc,
};
use tokio::sync::broadcast;

/// A mock implementation for Provider interfaces.
#[derive(Debug)]
pub struct MockEthProvider<T = TransactionSigned, ChainSpec = reth_chainspec::ChainSpec> {
    /// Local block store
    pub blocks: Arc<Mutex<HashMap<B256, Block<T>>>>,
    /// Local header store
    pub headers: Arc<Mutex<HashMap<B256, Header>>>,
    /// Local account store
    pub accounts: Arc<Mutex<HashMap<Address, ExtendedAccount>>>,
    /// Local chain spec
    pub chain_spec: Arc<ChainSpec>,
    /// Local state roots
    pub state_roots: Arc<Mutex<Vec<B256>>>,
    tx: TxMock,
    prune_modes: Arc<PruneModes>,
}

impl<T, ChainSpec> Clone for MockEthProvider<T, ChainSpec> {
    fn clone(&self) -> Self {
        Self {
            blocks: self.blocks.clone(),
            headers: self.headers.clone(),
            accounts: self.accounts.clone(),
            chain_spec: self.chain_spec.clone(),
            state_roots: self.state_roots.clone(),
            tx: self.tx.clone(),
            prune_modes: self.prune_modes.clone(),
        }
    }
}

impl<T> MockEthProvider<T> {
    /// Create a new, empty instance
    pub fn new() -> Self {
        Self {
            blocks: Default::default(),
            headers: Default::default(),
            accounts: Default::default(),
            chain_spec: Arc::new(reth_chainspec::ChainSpecBuilder::mainnet().build()),
            state_roots: Default::default(),
            tx: Default::default(),
            prune_modes: Default::default(),
        }
    }
}

impl<T, ChainSpec> MockEthProvider<T, ChainSpec> {
    /// Add block to local block store
    pub fn add_block(&self, hash: B256, block: Block<T>) {
        self.add_header(hash, block.header.clone());
        self.blocks.lock().insert(hash, block);
    }

    /// Add multiple blocks to local block store
    pub fn extend_blocks(&self, iter: impl IntoIterator<Item = (B256, Block<T>)>) {
        for (hash, block) in iter {
            self.add_header(hash, block.header.clone());
            self.add_block(hash, block)
        }
    }

    /// Add header to local header store
    pub fn add_header(&self, hash: B256, header: Header) {
        self.headers.lock().insert(hash, header);
    }

    /// Add multiple headers to local header store
    pub fn extend_headers(&self, iter: impl IntoIterator<Item = (B256, Header)>) {
        for (hash, header) in iter {
            self.add_header(hash, header)
        }
    }

    /// Add account to local account store
    pub fn add_account(&self, address: Address, account: ExtendedAccount) {
        self.accounts.lock().insert(address, account);
    }

    /// Add account to local account store
    pub fn extend_accounts(&self, iter: impl IntoIterator<Item = (Address, ExtendedAccount)>) {
        for (address, account) in iter {
            self.add_account(address, account)
        }
    }

    /// Add state root to local state root store
    pub fn add_state_root(&self, state_root: B256) {
        self.state_roots.lock().push(state_root);
    }

    /// Set chain spec.
    pub fn with_chain_spec<C>(self, chain_spec: C) -> MockEthProvider<T, C> {
        MockEthProvider {
            blocks: self.blocks,
            headers: self.headers,
            accounts: self.accounts,
            chain_spec: Arc::new(chain_spec),
            state_roots: self.state_roots,
            tx: self.tx,
            prune_modes: self.prune_modes,
        }
    }
}

impl Default for MockEthProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// An extended account for local store
#[derive(Debug, Clone)]
pub struct ExtendedAccount {
    account: Account,
    bytecode: Option<Bytecode>,
    storage: HashMap<StorageKey, StorageValue>,
}

impl ExtendedAccount {
    /// Create new instance of extended account
    pub fn new(nonce: u64, balance: U256) -> Self {
        Self {
            account: Account { nonce, balance, bytecode_hash: None },
            bytecode: None,
            storage: Default::default(),
        }
    }

    /// Set bytecode and bytecode hash on the extended account
    pub fn with_bytecode(mut self, bytecode: Bytes) -> Self {
        let hash = keccak256(&bytecode);
        self.account.bytecode_hash = Some(hash);
        self.bytecode = Some(Bytecode::new_raw(bytecode));
        self
    }

    /// Add storage to the extended account. If the storage key is already present,
    /// the value is updated.
    pub fn extend_storage(
        mut self,
        storage: impl IntoIterator<Item = (StorageKey, StorageValue)>,
    ) -> Self {
        self.storage.extend(storage);
        self
    }
}

/// Mock node.
#[derive(Debug)]
pub struct MockNode;

impl NodeTypes for MockNode {
    type Primitives = EthPrimitives;
    type ChainSpec = reth_chainspec::ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
}

impl<T: Transaction, ChainSpec: EthChainSpec> StateCommitmentProvider
    for MockEthProvider<T, ChainSpec>
{
    type StateCommitment = <MockNode as NodeTypes>::StateCommitment;
}

impl<T: Transaction, ChainSpec: EthChainSpec + Clone + 'static> DatabaseProviderFactory
    for MockEthProvider<T, ChainSpec>
{
    type DB = DatabaseMock;
    type Provider = Self;
    type ProviderRW = Self;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        // TODO: return Ok(self.clone()) when engine tests stops relying on an
        // Error returned here https://github.com/paradigmxyz/reth/pull/14482
        //Ok(self.clone())
        Err(ConsistentViewError::Syncing { best_block: GotExpected::new(0, 0) }.into())
    }

    fn database_provider_rw(&self) -> ProviderResult<Self::ProviderRW> {
        // TODO: return Ok(self.clone()) when engine tests stops relying on an
        // Error returned here https://github.com/paradigmxyz/reth/pull/14482
        //Ok(self.clone())
        Err(ConsistentViewError::Syncing { best_block: GotExpected::new(0, 0) }.into())
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec + 'static> DBProvider
    for MockEthProvider<T, ChainSpec>
{
    type Tx = TxMock;

    fn tx_ref(&self) -> &Self::Tx {
        &self.tx
    }

    fn tx_mut(&mut self) -> &mut Self::Tx {
        &mut self.tx
    }

    fn into_tx(self) -> Self::Tx {
        self.tx
    }

    fn prune_modes_ref(&self) -> &PruneModes {
        &self.prune_modes
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> HeaderProvider for MockEthProvider<T, ChainSpec> {
    type Header = Header;

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        let lock = self.headers.lock();
        Ok(lock.get(block_hash).cloned())
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        let lock = self.headers.lock();
        Ok(lock.values().find(|h| h.number == num).cloned())
    }

    fn header_td(&self, hash: &BlockHash) -> ProviderResult<Option<U256>> {
        let lock = self.headers.lock();
        Ok(lock.get(hash).map(|target| {
            lock.values()
                .filter(|h| h.number < target.number)
                .fold(target.difficulty, |td, h| td + h.difficulty)
        }))
    }

    fn header_td_by_number(&self, number: BlockNumber) -> ProviderResult<Option<U256>> {
        let lock = self.headers.lock();
        let sum = lock
            .values()
            .filter(|h| h.number <= number)
            .fold(U256::ZERO, |td, h| td + h.difficulty);
        Ok(Some(sum))
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> ProviderResult<Vec<Header>> {
        let lock = self.headers.lock();

        let mut headers: Vec<_> =
            lock.values().filter(|header| range.contains(&header.number)).cloned().collect();
        headers.sort_by_key(|header| header.number);

        Ok(headers)
    }

    fn sealed_header(&self, number: BlockNumber) -> ProviderResult<Option<SealedHeader>> {
        Ok(self.header_by_number(number)?.map(SealedHeader::seal_slow))
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        mut predicate: impl FnMut(&SealedHeader) -> bool,
    ) -> ProviderResult<Vec<SealedHeader>> {
        Ok(self
            .headers_range(range)?
            .into_iter()
            .map(SealedHeader::seal_slow)
            .take_while(|h| predicate(h))
            .collect())
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec + 'static> ChainSpecProvider
    for MockEthProvider<T, ChainSpec>
{
    type ChainSpec = ChainSpec;

    fn chain_spec(&self) -> Arc<ChainSpec> {
        self.chain_spec.clone()
    }
}

impl<T: SignedTransaction, ChainSpec: EthChainSpec> TransactionsProvider
    for MockEthProvider<T, ChainSpec>
{
    type Transaction = T;

    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        let lock = self.blocks.lock();
        let tx_number = lock
            .values()
            .flat_map(|block| &block.body.transactions)
            .position(|tx| *tx.tx_hash() == tx_hash)
            .map(|pos| pos as TxNumber);

        Ok(tx_number)
    }

    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        let lock = self.blocks.lock();
        let transaction =
            lock.values().flat_map(|block| &block.body.transactions).nth(id as usize).cloned();

        Ok(transaction)
    }

    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        let lock = self.blocks.lock();
        let transaction =
            lock.values().flat_map(|block| &block.body.transactions).nth(id as usize).cloned();

        Ok(transaction)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        Ok(self.blocks.lock().iter().find_map(|(_, block)| {
            block.body.transactions.iter().find(|tx| *tx.tx_hash() == hash).cloned()
        }))
    }

    fn transaction_by_hash_with_meta(
        &self,
        hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        let lock = self.blocks.lock();
        for (block_hash, block) in lock.iter() {
            for (index, tx) in block.body.transactions.iter().enumerate() {
                if *tx.tx_hash() == hash {
                    let meta = TransactionMeta {
                        tx_hash: hash,
                        index: index as u64,
                        block_hash: *block_hash,
                        block_number: block.header.number,
                        base_fee: block.header.base_fee_per_gas,
                        excess_blob_gas: block.header.excess_blob_gas,
                        timestamp: block.header.timestamp,
                    };
                    return Ok(Some((tx.clone(), meta)))
                }
            }
        }
        Ok(None)
    }

    fn transaction_block(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        let lock = self.blocks.lock();
        let mut current_tx_number: TxNumber = 0;
        for block in lock.values() {
            if current_tx_number + (block.body.transactions.len() as TxNumber) > id {
                return Ok(Some(block.header.number))
            }
            current_tx_number += block.body.transactions.len() as TxNumber;
        }
        Ok(None)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        Ok(self.block(id)?.map(|b| b.body.transactions))
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<alloy_primitives::BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        // init btreemap so we can return in order
        let mut map = BTreeMap::new();
        for (_, block) in self.blocks.lock().iter() {
            if range.contains(&block.number) {
                map.insert(block.number, block.body.transactions.clone());
            }
        }

        Ok(map.into_values().collect())
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        let lock = self.blocks.lock();
        let transactions = lock
            .values()
            .flat_map(|block| &block.body.transactions)
            .enumerate()
            .filter(|&(tx_number, _)| range.contains(&(tx_number as TxNumber)))
            .map(|(_, tx)| tx.clone())
            .collect();

        Ok(transactions)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        let lock = self.blocks.lock();
        let transactions = lock
            .values()
            .flat_map(|block| &block.body.transactions)
            .enumerate()
            .filter_map(|(tx_number, tx)| {
                if range.contains(&(tx_number as TxNumber)) {
                    tx.recover_signer().ok()
                } else {
                    None
                }
            })
            .collect();

        Ok(transactions)
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        self.transaction_by_id(id).map(|tx_option| tx_option.map(|tx| tx.recover_signer().unwrap()))
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> ReceiptProvider for MockEthProvider<T, ChainSpec> {
    type Receipt = Receipt;

    fn receipt(&self, _id: TxNumber) -> ProviderResult<Option<Receipt>> {
        Ok(None)
    }

    fn receipt_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<Receipt>> {
        Ok(None)
    }

    fn receipts_by_block(&self, _block: BlockHashOrNumber) -> ProviderResult<Option<Vec<Receipt>>> {
        Ok(None)
    }

    fn receipts_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Receipt>> {
        Ok(vec![])
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> ReceiptProviderIdExt
    for MockEthProvider<T, ChainSpec>
{
}

impl<T: Transaction, ChainSpec: EthChainSpec> BlockHashReader for MockEthProvider<T, ChainSpec> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        let lock = self.blocks.lock();

        let hash = lock.iter().find_map(|(hash, b)| (b.number == number).then_some(*hash));
        Ok(hash)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        let range = start..end;
        let lock = self.blocks.lock();

        let mut hashes: Vec<_> =
            lock.iter().filter(|(_, block)| range.contains(&block.number)).collect();
        hashes.sort_by_key(|(_, block)| block.number);

        Ok(hashes.into_iter().map(|(hash, _)| *hash).collect())
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> BlockNumReader for MockEthProvider<T, ChainSpec> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        let best_block_number = self.best_block_number()?;
        let lock = self.headers.lock();

        Ok(lock
            .iter()
            .find(|(_, header)| header.number == best_block_number)
            .map(|(hash, header)| ChainInfo { best_hash: *hash, best_number: header.number })
            .unwrap_or_default())
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        let lock = self.headers.lock();
        lock.iter()
            .max_by_key(|h| h.1.number)
            .map(|(_, header)| header.number)
            .ok_or(ProviderError::BestBlockNotFound)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.best_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<alloy_primitives::BlockNumber>> {
        let lock = self.blocks.lock();
        let num = lock.iter().find_map(|(h, b)| (*h == hash).then_some(b.number));
        Ok(num)
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> BlockIdReader for MockEthProvider<T, ChainSpec> {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }
}

impl<T: SignedTransaction, ChainSpec: EthChainSpec> BlockReader for MockEthProvider<T, ChainSpec> {
    type Block = Block<T>;

    fn find_block_by_hash(
        &self,
        hash: B256,
        _source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        self.block(hash.into())
    }

    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        let lock = self.blocks.lock();
        match id {
            BlockHashOrNumber::Hash(hash) => Ok(lock.get(&hash).cloned()),
            BlockHashOrNumber::Number(num) => Ok(lock.values().find(|b| b.number == num).cloned()),
        }
    }

    fn pending_block(&self) -> ProviderResult<Option<SealedBlock<Self::Block>>> {
        Ok(None)
    }

    fn pending_block_with_senders(&self) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(SealedBlock<Self::Block>, Vec<Receipt>)>> {
        Ok(None)
    }

    fn block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn sealed_block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        let lock = self.blocks.lock();

        let mut blocks: Vec<_> =
            lock.values().filter(|block| range.contains(&block.number)).cloned().collect();
        blocks.sort_by_key(|block| block.number);

        Ok(blocks)
    }

    fn block_with_senders_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        Ok(vec![])
    }

    fn sealed_block_with_senders_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock<Self::Block>>> {
        Ok(vec![])
    }
}

impl<T: SignedTransaction, ChainSpec: EthChainSpec> BlockReaderIdExt
    for MockEthProvider<T, ChainSpec>
{
    fn block_by_id(&self, id: BlockId) -> ProviderResult<Option<Block<T>>> {
        match id {
            BlockId::Number(num) => self.block_by_number_or_tag(num),
            BlockId::Hash(hash) => self.block_by_hash(hash.block_hash),
        }
    }

    fn sealed_header_by_id(&self, id: BlockId) -> ProviderResult<Option<SealedHeader>> {
        self.header_by_id(id)?.map_or_else(|| Ok(None), |h| Ok(Some(SealedHeader::seal_slow(h))))
    }

    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<Header>> {
        match self.block_by_id(id)? {
            None => Ok(None),
            Some(block) => Ok(Some(block.header)),
        }
    }

    fn ommers_by_id(&self, id: BlockId) -> ProviderResult<Option<Vec<Header>>> {
        match id {
            BlockId::Number(num) => self.ommers_by_number_or_tag(num),
            BlockId::Hash(hash) => self.ommers(BlockHashOrNumber::Hash(hash.block_hash)),
        }
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> AccountReader for MockEthProvider<T, ChainSpec> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self.accounts.lock().get(address).cloned().map(|a| a.account))
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> StageCheckpointReader
    for MockEthProvider<T, ChainSpec>
{
    fn get_stage_checkpoint(&self, _id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        Ok(None)
    }

    fn get_stage_checkpoint_progress(&self, _id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        Ok(vec![])
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> StateRootProvider for MockEthProvider<T, ChainSpec> {
    fn state_root(&self, _state: HashedPostState) -> ProviderResult<B256> {
        Ok(self.state_roots.lock().pop().unwrap_or_default())
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Ok(self.state_roots.lock().pop().unwrap_or_default())
    }

    fn state_root_with_updates(
        &self,
        _state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let state_root = self.state_roots.lock().pop().unwrap_or_default();
        Ok((state_root, Default::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let state_root = self.state_roots.lock().pop().unwrap_or_default();
        Ok((state_root, Default::default()))
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> StorageRootProvider
    for MockEthProvider<T, ChainSpec>
{
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Ok(EMPTY_ROOT_HASH)
    }

    fn storage_proof(
        &self,
        _address: Address,
        slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<reth_trie::StorageProof> {
        Ok(StorageProof::new(slot))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Ok(StorageMultiProof::empty())
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> StateProofProvider for MockEthProvider<T, ChainSpec> {
    fn proof(
        &self,
        _input: TrieInput,
        address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Ok(AccountProof::new(address))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Ok(MultiProof::default())
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
    ) -> ProviderResult<B256Map<Bytes>> {
        Ok(HashMap::default())
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec + 'static> HashedPostStateProvider
    for MockEthProvider<T, ChainSpec>
{
    fn hashed_post_state(&self, _state: &revm_database::BundleState) -> HashedPostState {
        HashedPostState::default()
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec + 'static> StateProvider
    for MockEthProvider<T, ChainSpec>
{
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let lock = self.accounts.lock();
        Ok(lock.get(&account).and_then(|account| account.storage.get(&storage_key)).copied())
    }

    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        let lock = self.accounts.lock();
        Ok(lock.values().find_map(|account| {
            match (account.account.bytecode_hash.as_ref(), account.bytecode.as_ref()) {
                (Some(bytecode_hash), Some(bytecode)) if bytecode_hash == code_hash => {
                    Some(bytecode.clone())
                }
                _ => None,
            }
        }))
    }
}

impl<T: SignedTransaction, ChainSpec: EthChainSpec + 'static> StateProviderFactory
    for MockEthProvider<T, ChainSpec>
{
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Latest => self.latest(),
            BlockNumberOrTag::Finalized => {
                // we can only get the finalized state by hash, not by num
                let hash =
                    self.finalized_block_hash()?.ok_or(ProviderError::FinalizedBlockNotFound)?;

                // only look at historical state
                self.history_by_block_hash(hash)
            }
            BlockNumberOrTag::Safe => {
                // we can only get the safe state by hash, not by num
                let hash = self.safe_block_hash()?.ok_or(ProviderError::SafeBlockNotFound)?;

                self.history_by_block_hash(hash)
            }
            BlockNumberOrTag::Earliest => self.history_by_block_number(0),
            BlockNumberOrTag::Pending => self.pending(),
            BlockNumberOrTag::Number(num) => self.history_by_block_number(num),
        }
    }

    fn history_by_block_number(&self, _block: BlockNumber) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn history_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn state_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn pending_state_by_hash(&self, _block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        Ok(Some(Box::new(self.clone())))
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> WithdrawalsProvider
    for MockEthProvider<T, ChainSpec>
{
    fn withdrawals_by_block(
        &self,
        _id: BlockHashOrNumber,
        _timestamp: u64,
    ) -> ProviderResult<Option<Withdrawals>> {
        Ok(None)
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> OmmersProvider for MockEthProvider<T, ChainSpec> {
    fn ommers(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<Vec<Header>>> {
        Ok(None)
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> BlockBodyIndicesProvider
    for MockEthProvider<T, ChainSpec>
{
    fn block_body_indices(&self, _num: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        Ok(None)
    }
    fn block_body_indices_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        Ok(vec![])
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> ChangeSetReader for MockEthProvider<T, ChainSpec> {
    fn account_block_changeset(
        &self,
        _block_number: BlockNumber,
    ) -> ProviderResult<Vec<AccountBeforeTx>> {
        Ok(Vec::default())
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> StateReader for MockEthProvider<T, ChainSpec> {
    type Receipt = Receipt;

    fn get_state(&self, _block: BlockNumber) -> ProviderResult<Option<ExecutionOutcome>> {
        Ok(None)
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> CanonStateSubscriptions
    for MockEthProvider<T, ChainSpec>
{
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<EthPrimitives> {
        broadcast::channel(1).1
    }
}

impl<T: Transaction, ChainSpec: EthChainSpec> NodePrimitivesProvider
    for MockEthProvider<T, ChainSpec>
{
    type Primitives = EthPrimitives;
}
