//! Command that initializes the node by importing a chain from a file.

use crate::{
    args::{
        utils::{chain_help, genesis_value_parser, SUPPORTED_CHAINS},
        DatabaseArgs,
    },
    dirs::{DataDirPath, MaybePlatformPath},
    version::SHORT_VERSION,
};
use clap::Parser;
use eyre::Context;
use futures::{Stream, StreamExt};
use reth_beacon_consensus::BeaconConsensus;
use reth_config::Config;
use reth_db::{database::Database, init_db};
use reth_downloaders::{
    bodies::bodies::BodiesDownloaderBuilder, file_client::FileClient,
    headers::reverse_headers::ReverseHeadersDownloaderBuilder,
};
use reth_interfaces::consensus::Consensus;
use reth_node_core::{events::node::NodeEvent, init::init_genesis};
use reth_node_ethereum::EthEvmConfig;
use reth_primitives::{stage::StageId, ChainSpec, PruneModes, B256, Withdrawals, Signature, TransactionKind, TransactionSigned};
use reth_provider::{HeaderSyncMode, ProviderFactory, StageCheckpointReader};
use reth_stages::{
    prelude::*,
    stages::{ExecutionStage, ExecutionStageThresholds, SenderRecoveryStage},
};
use reth_static_file::StaticFileProducer;
use std::{path::PathBuf, sync::Arc};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use alloy_rlp::{Decodable, Rlp, RlpDecodable, RlpEncodable};
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{debug, info};
use crate::commands::import::ImportCommand;
use crate::commands::stage::Subcommands::Dump;
use crate::primitives::{Address, BlockNumber, Bloom, Bytes, ChainId, TxHash, U256};
use crate::primitives::alloy_primitives::private::derive_more::{AsRef, Deref};

/// Syncs RLP encoded blocks from a file.
#[derive(Debug, Parser)]
pub struct ImportOpCommand {
    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
    long,
    value_name = "CHAIN_OR_PATH",
    long_help = chain_help(),
    default_value = SUPPORTED_CHAINS[0],
    value_parser = genesis_value_parser
    )]
    chain: Arc<ChainSpec>,

    #[command(flatten)]
    db: DatabaseArgs,

    /// The path to a block file for import.
    ///
    /// The online stages (headers and bodies) are replaced by a file import, after which the
    /// remaining stages are executed.
    #[arg(value_name = "IMPORT_PATH", verbatim_doc_comment)]
    path: PathBuf,
}

/// Ethereum full block.
#[derive(
Debug, Clone, PartialEq, Eq, RlpDecodable,
)]
#[rlp(trailing)]
pub struct Block {
    pub header: Header,
    pub txs: Vec<Transaction>,
    pub uncles: Vec<Header>,
}

// Block header
#[derive(Debug, Clone, PartialEq, Eq, RlpDecodable)]
pub struct Header {
    /// The Keccak 256-bit hash of the parent
    /// block’s header, in its entirety; formally Hp.
    pub parent_hash: B256,
    /// The Keccak 256-bit hash of the ommers list portion of this block; formally Ho.
    pub uncle_hash: B256,
    /// The 160-bit address to which all fees collected from the successful mining of this block
    /// be transferred; formally Hc.
    pub coinbase: Address,
    /// The Keccak 256-bit hash of the root node of the state trie, after all transactions are
    /// executed and finalisations applied; formally Hr.
    pub root: B256,
    /// The Keccak 256-bit hash of the root node of the trie structure populated with each
    /// transaction in the transactions list portion of the block; formally Ht.
    pub tx_hash: B256,
    /// The Keccak 256-bit hash of the root node of the trie structure populated with the receipts
    /// of each transaction in the transactions list portion of the block; formally He.
    pub receipt_hash: B256,
    /// The Bloom filter composed from indexable information (logger address and log topics)
    /// contained in each log entry from the receipt of each transaction in the transactions list;
    /// formally Hb.
    pub bloom: Bloom,
    /// A scalar value corresponding to the difficulty level of this block. This can be calculated
    /// from the previous block’s difficulty level and the timestamp; formally Hd.
    pub difficulty: U256,
    /// A scalar value equal to the number of ancestor blocks. The genesis block has a number of
    /// zero; formally Hi.
    pub number: U256,
    /// A scalar value equal to the current limit of gas expenditure per block; formally Hl.
    pub gas_limit: u64,
    /// A scalar value equal to the total gas used in transactions in this block; formally Hg.
    pub gas_used: u64,
    /// A scalar value equal to the reasonable output of Unix’s time() at this block’s inception;
    /// formally Hs.
    pub time: u64,
    /// An arbitrary byte array containing data relevant to this block. This must be 32 bytes or
    /// fewer; formally Hx.
    pub extra_data: Bytes,
    /// A 256-bit hash which, combined with the
    /// nonce, proves that a sufficient amount of computation has been carried out on this block;
    /// formally Hm.
    pub mix_digest: B256,
    /// A 64-bit value which, combined with the mixhash, proves that a sufficient amount of
    /// computation has been carried out on this block; formally Hn.
    pub nonce: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, RlpDecodable)]
pub struct Transaction {
    pub data: TxLegacy,
    pub meta: TxMeta,
    /// Transaction hash
    pub hash: TxHash,
    pub size: u32,
    pub from: Address,
}

#[derive(Eq, PartialEq, Deserialize, Clone, Debug, RlpDecodable)]
pub struct TxMeta {
    block_number: U256,
    timestamp: u64,
    message_sender: Address,
    rest: Bytes,
}

#[derive(Eq, PartialEq, Deserialize, Clone, Debug, RlpDecodable)]
pub struct TxLegacy {
    /// A scalar value equal to the number of transactions sent by the sender; formally Tn.
    pub account_nonce: u64,
    /// A scalar value equal to the number of
    /// Wei to be paid per unit of gas for all computation
    /// costs incurred as a result of the execution of this transaction; formally Tp.
    ///
    /// As ethereum circulation is around 120mil eth as of 2022 that is around
    /// 120000000000000000000000000 wei we are safe to use u128 as its max number is:
    /// 340282366920938463463374607431768211455
    pub gas_price: u128,
    /// A scalar value equal to the maximum
    /// amount of gas that should be used in executing
    /// this transaction. This is paid up-front, before any
    /// computation is done and may not be increased
    /// later; formally Tg.
    pub gas_limit: u64,
    /// The 160-bit address of the message call’s recipient or, for a contract creation
    /// transaction, ∅, used here to denote the only member of B0 ; formally Tt.
    pub to: TransactionKind,
    /// A scalar value equal to the number of Wei to
    /// be transferred to the message call’s recipient or,
    /// in the case of contract creation, as an endowment
    /// to the newly created account; formally Tv.
    pub value: U256,
    /// Input has two uses depending if transaction is Create or Call (if `to` field is None or
    /// Some). pub init: An unlimited size byte array specifying the
    /// EVM-code for the account initialisation procedure CREATE,
    /// data: An unlimited size byte array specifying the
    /// input data of the message call, formally Td.
    pub input: Bytes,
    pub v: U256,
    pub r: U256,
    pub s: U256,
}

impl ImportOpCommand {
    /// Execute `import` command
    pub async fn execute(self) -> eyre::Result<()> {
        info!(target: "reth::cli", "reth {} starting", SHORT_VERSION);

        let mut file = File::open(self.path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        /*TODO: the rlp encoded file seems to not be properly encoded as an rlp list.
            therefore, we need to advance the buffer manually*/
        let block = Block::decode(&mut buffer.as_slice()).unwrap();
        dbg!(block);
        Ok(())
    }
}
