/// Module responsible for handling persisting transaction data to the PostgreSQL
/// database.
use {
    crate::{
        geyser_plugin_postgres::{GeyserPluginPostgresConfig, GeyserPluginPostgresError},
        postgres_client::{
            db_write_error, DbWorkItem, ParallelPostgresClient, SimplePostgresClient,
        },
    },
    agave_geyser_plugin_interface::{
        geyser_plugin_interface::GeyserPluginError,
        transaction_status_meta::{
            InnerInstructions, Reward, TransactionStatusMeta, TransactionTokenBalance,
        },
    },
    chrono::Utc,
    log::*,
    postgres::{Client, Statement},
    postgres_types::{FromSql, ToSql},
    solana_hash::Hash,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        v0::{self, LoadedAddresses, MessageAddressTableLookup},
        v1, Message, MessageHeader, VersionedMessage,
    },
    solana_reward_info::RewardType,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_error::TransactionError,
    std::sync::atomic::Ordering,
};

/// Must match the size of `"TransactionError".error_detail` (VARCHAR(256)).
const MAX_TRANSACTION_STATUS_LEN: usize = 256;

/// `transaction.message_type` values. Legacy and v0 are what downstream consumers
/// read; v1 messages (no address lookup tables) are stored in `legacy_message`.
pub const MESSAGE_TYPE_LEGACY: i16 = 0;
pub const MESSAGE_TYPE_V0: i16 = 1;
pub const MESSAGE_TYPE_V1: i16 = 2;

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "CompiledInstruction")]
pub struct DbCompiledInstruction {
    pub program_id_index: i16,
    pub accounts: Vec<i16>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "InnerInstructions")]
pub struct DbInnerInstructions {
    pub index: i16,
    pub instructions: Vec<DbCompiledInstruction>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "TransactionTokenBalance")]
pub struct DbTransactionTokenBalance {
    pub account_index: i16,
    pub mint: String,
    pub ui_token_amount: Option<f64>,
    pub owner: String,
}

#[derive(Clone, Debug, Eq, FromSql, ToSql, PartialEq)]
#[postgres(name = "RewardType")]
pub enum DbRewardType {
    Fee,
    Rent,
    Staking,
    Voting,
    // Added by scripts/migrate_agave_v4_3.sql
    DeactivatedStake,
    VATDebit,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "Reward")]
pub struct DbReward {
    pub pubkey: String,
    pub lamports: i64,
    pub post_balance: i64,
    pub reward_type: Option<DbRewardType>,
    pub commission: Option<i16>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "TransactionStatusMeta")]
pub struct DbTransactionStatusMeta {
    pub error: Option<DbTransactionError>,
    pub fee: i64,
    pub pre_balances: Vec<i64>,
    pub post_balances: Vec<i64>,
    pub inner_instructions: Option<Vec<DbInnerInstructions>>,
    pub log_messages: Option<Vec<String>>,
    pub pre_token_balances: Option<Vec<DbTransactionTokenBalance>>,
    pub post_token_balances: Option<Vec<DbTransactionTokenBalance>>,
    pub rewards: Option<Vec<DbReward>>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "TransactionMessageHeader")]
pub struct DbTransactionMessageHeader {
    pub num_required_signatures: i16,
    pub num_readonly_signed_accounts: i16,
    pub num_readonly_unsigned_accounts: i16,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "TransactionMessage")]
pub struct DbTransactionMessage {
    pub header: DbTransactionMessageHeader,
    pub account_keys: Vec<Vec<u8>>,
    pub recent_blockhash: Vec<u8>,
    pub instructions: Vec<DbCompiledInstruction>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "TransactionMessageAddressTableLookup")]
pub struct DbTransactionMessageAddressTableLookup {
    pub account_key: Vec<u8>,
    pub writable_indexes: Vec<i16>,
    pub readonly_indexes: Vec<i16>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "TransactionMessageV0")]
pub struct DbTransactionMessageV0 {
    pub header: DbTransactionMessageHeader,
    pub account_keys: Vec<Vec<u8>>,
    pub recent_blockhash: Vec<u8>,
    pub instructions: Vec<DbCompiledInstruction>,
    pub address_table_lookups: Vec<DbTransactionMessageAddressTableLookup>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "LoadedAddresses")]
pub struct DbLoadedAddresses {
    pub writable: Vec<Vec<u8>>,
    pub readonly: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, FromSql, ToSql)]
#[postgres(name = "LoadedMessageV0")]
pub struct DbLoadedMessageV0 {
    pub message: DbTransactionMessageV0,
    pub loaded_addresses: DbLoadedAddresses,
}

#[derive(Clone, Debug)]
pub struct DbTransaction {
    pub signature: Vec<u8>,
    pub is_vote: bool,
    pub slot: i64,
    pub message_type: i16,
    pub legacy_message: Option<DbTransactionMessage>,
    pub v0_loaded_message: Option<DbLoadedMessageV0>,
    pub message_hash: Vec<u8>,
    pub meta: DbTransactionStatusMeta,
    pub signatures: Vec<Vec<u8>>,
    /// This can be used to tell the order of transaction within a block
    /// Given a slot, the transaction with a smaller write_version appears
    /// before transactions with higher write_versions in a shred.
    pub write_version: i64,
    pub index: i64,
}

pub struct LogTransactionRequest {
    pub transaction_info: DbTransaction,
}

/// A transaction notification as the plugin stores it: the fields of
/// `ReplicaTransactionInfoV4`, with the meta in the interface's borrowed mirror
/// types. `pg_backfill` borrows its owned meta into the same view
/// (`rpc_convert::with_mirror_meta`), so live and backfilled rows are built by
/// the same code.
pub struct TransactionNotification<'a> {
    pub signature: &'a Signature,
    pub is_vote: bool,
    pub transaction: &'a VersionedTransaction,
    pub message_hash: &'a Hash,
    pub meta: &'a TransactionStatusMeta<'a>,
    pub index: usize,
}

/// Keeps the head of `text`, bounded to `max_chars` characters (VARCHAR(n) counts
/// characters), and drops NUL characters which PostgreSQL text types reject.
pub(crate) fn bounded_text(text: String, max_chars: usize) -> String {
    let mut text = if text.contains('\0') {
        text.replace('\0', "")
    } else {
        text
    };
    if let Some((byte_index, _)) = text.char_indices().nth(max_chars) {
        text.truncate(byte_index);
    }
    text
}

fn strip_nul(text: &str) -> String {
    if text.contains('\0') {
        text.replace('\0', "")
    } else {
        text.to_string()
    }
}

impl From<&MessageAddressTableLookup> for DbTransactionMessageAddressTableLookup {
    fn from(address_table_lookup: &MessageAddressTableLookup) -> Self {
        Self {
            account_key: address_table_lookup.account_key.as_ref().to_vec(),
            writable_indexes: address_table_lookup
                .writable_indexes
                .iter()
                .map(|idx| *idx as i16)
                .collect(),
            readonly_indexes: address_table_lookup
                .readonly_indexes
                .iter()
                .map(|idx| *idx as i16)
                .collect(),
        }
    }
}

impl From<&LoadedAddresses> for DbLoadedAddresses {
    fn from(loaded_addresses: &LoadedAddresses) -> Self {
        Self {
            writable: db_account_keys(&loaded_addresses.writable),
            readonly: db_account_keys(&loaded_addresses.readonly),
        }
    }
}

impl From<&MessageHeader> for DbTransactionMessageHeader {
    fn from(header: &MessageHeader) -> Self {
        Self {
            num_required_signatures: header.num_required_signatures as i16,
            num_readonly_signed_accounts: header.num_readonly_signed_accounts as i16,
            num_readonly_unsigned_accounts: header.num_readonly_unsigned_accounts as i16,
        }
    }
}

impl From<&CompiledInstruction> for DbCompiledInstruction {
    fn from(instruction: &CompiledInstruction) -> Self {
        Self {
            program_id_index: instruction.program_id_index as i16,
            accounts: instruction
                .accounts
                .iter()
                .map(|account_idx| *account_idx as i16)
                .collect(),
            data: instruction.data.clone(),
        }
    }
}

fn db_account_keys<T: AsRef<[u8]>>(keys: &[T]) -> Vec<Vec<u8>> {
    keys.iter().map(|key| key.as_ref().to_vec()).collect()
}

fn db_instructions(instructions: &[CompiledInstruction]) -> Vec<DbCompiledInstruction> {
    instructions
        .iter()
        .map(DbCompiledInstruction::from)
        .collect()
}

impl From<&Message> for DbTransactionMessage {
    fn from(message: &Message) -> Self {
        Self {
            header: DbTransactionMessageHeader::from(&message.header),
            account_keys: db_account_keys(&message.account_keys),
            recent_blockhash: message.recent_blockhash.as_ref().to_vec(),
            instructions: db_instructions(&message.instructions),
        }
    }
}

/// v1 messages have no address lookup tables; the lifetime specifier (blockhash
/// or nonce) is stored as `recent_blockhash`. The v1 compute budget config is not
/// stored.
impl From<&v1::Message> for DbTransactionMessage {
    fn from(message: &v1::Message) -> Self {
        Self {
            header: DbTransactionMessageHeader::from(&message.header),
            account_keys: db_account_keys(&message.account_keys),
            recent_blockhash: message.lifetime_specifier.as_ref().to_vec(),
            instructions: db_instructions(&message.instructions),
        }
    }
}

impl From<&v0::Message> for DbTransactionMessageV0 {
    fn from(message: &v0::Message) -> Self {
        Self {
            header: DbTransactionMessageHeader::from(&message.header),
            account_keys: db_account_keys(&message.account_keys),
            recent_blockhash: message.recent_blockhash.as_ref().to_vec(),
            instructions: db_instructions(&message.instructions),
            address_table_lookups: message
                .address_table_lookups
                .iter()
                .map(DbTransactionMessageAddressTableLookup::from)
                .collect(),
        }
    }
}

impl DbLoadedMessageV0 {
    /// The loaded addresses are the ALT-resolved keys from the transaction status
    /// meta (the previous implementation stored the lookup indexes instead).
    fn new(message: &v0::Message, loaded_addresses: &LoadedAddresses) -> Self {
        Self {
            message: DbTransactionMessageV0::from(message),
            loaded_addresses: DbLoadedAddresses::from(loaded_addresses),
        }
    }
}

impl From<&InnerInstructions<'_>> for DbInnerInstructions {
    fn from(instructions: &InnerInstructions) -> Self {
        Self {
            index: instructions.index as i16,
            instructions: instructions
                .instructions
                .iter()
                .map(|instruction| DbCompiledInstruction::from(instruction.instruction))
                .collect(),
        }
    }
}

impl From<&RewardType> for DbRewardType {
    fn from(reward_type: &RewardType) -> Self {
        match reward_type {
            RewardType::Fee => Self::Fee,
            RewardType::Rent => Self::Rent,
            RewardType::Staking => Self::Staking,
            RewardType::Voting => Self::Voting,
            RewardType::DeactivatedStake => Self::DeactivatedStake,
            RewardType::VATDebit => Self::VATDebit,
        }
    }
}

impl From<&Reward<'_>> for DbReward {
    fn from(reward: &Reward) -> Self {
        Self {
            pubkey: reward.pubkey.to_string(),
            lamports: reward.lamports,
            post_balance: reward.post_balance as i64,
            reward_type: reward.reward_type.as_ref().map(DbRewardType::from),
            commission: reward
                .commission
                .as_ref()
                .map(|commission| *commission as i16),
        }
    }
}

/// Labels of the PostgreSQL enum "TransactionErrorCode". Every label used here must
/// exist in the database (see scripts/migrate_agave_v4_3.sql); a missing label makes
/// the row insert fail with SQLSTATE 22P02.
#[derive(Clone, Debug, Eq, FromSql, ToSql, PartialEq)]
#[postgres(name = "TransactionErrorCode")]
pub enum DbTransactionErrorCode {
    AccountInUse,
    AccountLoadedTwice,
    AccountNotFound,
    ProgramAccountNotFound,
    InsufficientFundsForFee,
    InvalidAccountForFee,
    AlreadyProcessed,
    BlockhashNotFound,
    InstructionError,
    CallChainTooDeep,
    MissingSignatureForFee,
    InvalidAccountIndex,
    SignatureFailure,
    InvalidProgramForExecution,
    SanitizeFailure,
    ClusterMaintenance,
    AccountBorrowOutstanding,
    WouldExceedMaxAccountCostLimit,
    WouldExceedMaxBlockCostLimit,
    UnsupportedVersion,
    InvalidWritableAccount,
    WouldExceedMaxAccountDataCostLimit,
    TooManyAccountLocks,
    AddressLookupTableNotFound,
    InvalidAddressLookupTableOwner,
    InvalidAddressLookupTableData,
    InvalidAddressLookupTableIndex,
    InvalidRentPayingAccount,
    WouldExceedMaxVoteCostLimit,
    WouldExceedAccountDataBlockLimit,
    WouldExceedAccountDataTotalLimit,
    DuplicateInstruction,
    InsufficientFundsForRent,
    MaxLoadedAccountsDataSizeExceeded,
    InvalidLoadedAccountsDataSizeLimit,
    ResanitizationNeeded,
    UnbalancedTransaction,
    ProgramExecutionTemporarilyRestricted,
    // Added by scripts/migrate_agave_v4_3.sql
    ProgramCacheHitMaxLimit,
    CommitCancelled,
    /// Fallback label for errors without a label of their own; the error's
    /// `Debug` text then goes to `error_detail`. Used for `BailOut` (agave v4.4,
    /// a leader-side discard that is never committed, so it should not reach the
    /// plugin) and for any variant a later `TransactionError` adds.
    Unknown,
}

impl From<&TransactionError> for DbTransactionErrorCode {
    // `TransactionError` is `#[non_exhaustive]` since solana-transaction-error 4.0,
    // so the match needs a wildcard. Every variant that has a label is listed;
    // `BailOut` and future variants become `Unknown` with the error text in
    // `error_detail` (see `get_transaction_error`) instead of failing the row.
    // `test_all_transaction_errors_map_without_panic` walks
    // `TransactionError::VARIANTS` so that a new variant is noticed on upgrade.
    fn from(err: &TransactionError) -> Self {
        match err {
            TransactionError::AccountInUse => Self::AccountInUse,
            TransactionError::AccountLoadedTwice => Self::AccountLoadedTwice,
            TransactionError::AccountNotFound => Self::AccountNotFound,
            TransactionError::ProgramAccountNotFound => Self::ProgramAccountNotFound,
            TransactionError::InsufficientFundsForFee => Self::InsufficientFundsForFee,
            TransactionError::InvalidAccountForFee => Self::InvalidAccountForFee,
            TransactionError::AlreadyProcessed => Self::AlreadyProcessed,
            TransactionError::BlockhashNotFound => Self::BlockhashNotFound,
            TransactionError::InstructionError(_idx, _error) => Self::InstructionError,
            TransactionError::CallChainTooDeep => Self::CallChainTooDeep,
            TransactionError::MissingSignatureForFee => Self::MissingSignatureForFee,
            TransactionError::InvalidAccountIndex => Self::InvalidAccountIndex,
            TransactionError::SignatureFailure => Self::SignatureFailure,
            TransactionError::InvalidProgramForExecution => Self::InvalidProgramForExecution,
            TransactionError::SanitizeFailure => Self::SanitizeFailure,
            TransactionError::ClusterMaintenance => Self::ClusterMaintenance,
            TransactionError::AccountBorrowOutstanding => Self::AccountBorrowOutstanding,
            TransactionError::WouldExceedMaxAccountCostLimit => {
                Self::WouldExceedMaxAccountCostLimit
            }
            TransactionError::WouldExceedMaxBlockCostLimit => Self::WouldExceedMaxBlockCostLimit,
            TransactionError::UnsupportedVersion => Self::UnsupportedVersion,
            TransactionError::InvalidWritableAccount => Self::InvalidWritableAccount,
            TransactionError::WouldExceedAccountDataBlockLimit => {
                Self::WouldExceedAccountDataBlockLimit
            }
            TransactionError::WouldExceedAccountDataTotalLimit => {
                Self::WouldExceedAccountDataTotalLimit
            }
            TransactionError::TooManyAccountLocks => Self::TooManyAccountLocks,
            TransactionError::AddressLookupTableNotFound => Self::AddressLookupTableNotFound,
            TransactionError::InvalidAddressLookupTableOwner => {
                Self::InvalidAddressLookupTableOwner
            }
            TransactionError::InvalidAddressLookupTableData => Self::InvalidAddressLookupTableData,
            TransactionError::InvalidAddressLookupTableIndex => {
                Self::InvalidAddressLookupTableIndex
            }
            TransactionError::InvalidRentPayingAccount => Self::InvalidRentPayingAccount,
            TransactionError::WouldExceedMaxVoteCostLimit => Self::WouldExceedMaxVoteCostLimit,
            TransactionError::DuplicateInstruction(_) => Self::DuplicateInstruction,
            TransactionError::InsufficientFundsForRent { .. } => Self::InsufficientFundsForRent,
            TransactionError::MaxLoadedAccountsDataSizeExceeded => {
                Self::MaxLoadedAccountsDataSizeExceeded
            }
            TransactionError::InvalidLoadedAccountsDataSizeLimit => {
                Self::InvalidLoadedAccountsDataSizeLimit
            }
            TransactionError::ResanitizationNeeded => Self::ResanitizationNeeded,
            TransactionError::UnbalancedTransaction => Self::UnbalancedTransaction,
            TransactionError::ProgramExecutionTemporarilyRestricted { .. } => {
                Self::ProgramExecutionTemporarilyRestricted
            }
            TransactionError::ProgramCacheHitMaxLimit => Self::ProgramCacheHitMaxLimit,
            TransactionError::CommitCancelled => Self::CommitCancelled,
            TransactionError::BailOut => Self::Unknown,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, FromSql, ToSql, PartialEq)]
#[postgres(name = "TransactionError")]
pub struct DbTransactionError {
    pub error_code: DbTransactionErrorCode,
    pub error_detail: Option<String>,
}

fn get_transaction_error(result: &Result<(), &TransactionError>) -> Option<DbTransactionError> {
    let error = *result.as_ref().err()?;
    let error_detail = match error {
        TransactionError::InstructionError(idx, instruction_error) => Some(format!(
            "InstructionError: idx ({idx}), error: ({instruction_error})"
        )),
        TransactionError::DuplicateInstruction(idx) => {
            Some(format!("DuplicateInstruction: idx ({idx})"))
        }
        TransactionError::InsufficientFundsForRent { account_index }
        | TransactionError::ProgramExecutionTemporarilyRestricted { account_index } => {
            Some(format!("account_index ({account_index})"))
        }
        _ => None,
    };
    let error_code = DbTransactionErrorCode::from(error);
    let error_detail = match error_code {
        DbTransactionErrorCode::Unknown => Some(format!("{error:?}")),
        _ => error_detail,
    };
    Some(DbTransactionError {
        error_code,
        error_detail: error_detail.map(|detail| bounded_text(detail, MAX_TRANSACTION_STATUS_LEN)),
    })
}

impl From<&TransactionTokenBalance<'_>> for DbTransactionTokenBalance {
    fn from(token_balance: &TransactionTokenBalance) -> Self {
        Self {
            account_index: token_balance.account_index as i16,
            mint: token_balance.mint.to_string(),
            ui_token_amount: token_balance.ui_token_amount.ui_amount,
            owner: token_balance.owner.to_string(),
        }
    }
}

impl From<&TransactionStatusMeta<'_>> for DbTransactionStatusMeta {
    fn from(meta: &TransactionStatusMeta) -> Self {
        Self {
            error: get_transaction_error(&meta.status),
            fee: meta.fee as i64,
            pre_balances: meta
                .pre_balances
                .iter()
                .map(|balance| *balance as i64)
                .collect(),
            post_balances: meta
                .post_balances
                .iter()
                .map(|balance| *balance as i64)
                .collect(),
            inner_instructions: meta
                .inner_instructions
                .map(|instructions| instructions.iter().map(DbInnerInstructions::from).collect()),
            log_messages: meta
                .log_messages
                .map(|messages| messages.iter().map(|message| strip_nul(message)).collect()),
            pre_token_balances: meta.pre_token_balances.map(|balances| {
                balances
                    .iter()
                    .map(DbTransactionTokenBalance::from)
                    .collect()
            }),
            post_token_balances: meta.post_token_balances.map(|balances| {
                balances
                    .iter()
                    .map(DbTransactionTokenBalance::from)
                    .collect()
            }),
            rewards: meta
                .rewards
                .map(|rewards| rewards.iter().map(DbReward::from).collect()),
        }
    }
}

pub fn build_db_transaction(
    slot: u64,
    transaction_info: &TransactionNotification,
    transaction_write_version: u64,
) -> DbTransaction {
    let (message_type, legacy_message, v0_loaded_message) =
        match &transaction_info.transaction.message {
            VersionedMessage::Legacy(message) => (
                MESSAGE_TYPE_LEGACY,
                Some(DbTransactionMessage::from(message)),
                None,
            ),
            VersionedMessage::V0(message) => (
                MESSAGE_TYPE_V0,
                None,
                Some(DbLoadedMessageV0::new(
                    message,
                    transaction_info.meta.loaded_addresses,
                )),
            ),
            VersionedMessage::V1(message) => (
                MESSAGE_TYPE_V1,
                Some(DbTransactionMessage::from(message)),
                None,
            ),
        };
    DbTransaction {
        signature: transaction_info.signature.as_ref().to_vec(),
        is_vote: transaction_info.is_vote,
        slot: slot as i64,
        message_type,
        legacy_message,
        v0_loaded_message,
        signatures: transaction_info
            .transaction
            .signatures
            .iter()
            .map(|signature| signature.as_ref().to_vec())
            .collect(),
        message_hash: transaction_info.message_hash.as_ref().to_vec(),
        meta: DbTransactionStatusMeta::from(transaction_info.meta),
        write_version: transaction_write_version as i64,
        index: transaction_info.index as i64,
    }
}

/// `build_db_transaction` plus the `store_log_messages` option: when it is false
/// `(meta).log_messages` is written as NULL. Shared by the plugin and the
/// `pg_backfill` tool so that both store identical rows.
pub fn build_db_transaction_for_store(
    slot: u64,
    transaction_info: &TransactionNotification,
    transaction_write_version: u64,
    store_log_messages: bool,
) -> DbTransaction {
    let mut transaction = build_db_transaction(slot, transaction_info, transaction_write_version);
    if !store_log_messages {
        transaction.meta.log_messages = None;
    }
    transaction
}

impl SimplePostgresClient {
    pub(crate) fn build_transaction_info_upsert_statement(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        let stmt = "INSERT INTO transaction AS txn (signature, is_vote, slot, message_type, legacy_message, \
        v0_loaded_message, signatures, message_hash, meta, write_version, index, updated_on) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
        ON CONFLICT (slot, signature) DO UPDATE SET is_vote=excluded.is_vote, \
        message_type=excluded.message_type, \
        legacy_message=excluded.legacy_message, \
        v0_loaded_message=excluded.v0_loaded_message, \
        signatures=excluded.signatures, \
        message_hash=excluded.message_hash, \
        meta=excluded.meta, \
        write_version=excluded.write_version, \
        index=excluded.index,
        updated_on=excluded.updated_on";

        let stmt = client.prepare(stmt);

        match stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the transaction update PostgreSQL database: ({}) host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(stmt) => Ok(stmt),
        }
    }

    pub(crate) fn log_transaction_impl(
        &mut self,
        transaction_log_info: LogTransactionRequest,
    ) -> Result<(), GeyserPluginError> {
        let client = self.client.get_mut().unwrap();
        let statement = &client.update_transaction_log_stmt;
        let client = &mut client.client;
        let updated_on = Utc::now().naive_utc();

        let transaction_info = transaction_log_info.transaction_info;
        let result = client.execute(
            statement,
            &[
                &transaction_info.signature,
                &transaction_info.is_vote,
                &transaction_info.slot,
                &transaction_info.message_type,
                &transaction_info.legacy_message,
                &transaction_info.v0_loaded_message,
                &transaction_info.signatures,
                &transaction_info.message_hash,
                &transaction_info.meta,
                &transaction_info.write_version,
                &transaction_info.index,
                &updated_on,
            ],
        );

        if let Err(err) = result {
            return Err(db_write_error(
                &format!(
                    "transaction {} at slot {}",
                    bs58::encode(&transaction_info.signature).into_string(),
                    transaction_info.slot
                ),
                err,
            ));
        }

        Ok(())
    }
}

impl ParallelPostgresClient {
    pub fn log_transaction_info(
        &self,
        transaction_info: &TransactionNotification,
        slot: u64,
    ) -> Result<(), GeyserPluginError> {
        let write_version = self
            .transaction_write_version
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let transaction_info = build_db_transaction_for_store(
            slot,
            transaction_info,
            write_version,
            self.store_log_messages,
        );
        if log::log_enabled!(log::Level::Trace) {
            trace!(
                "queue transaction signature={} slot={} index={} message_type={} \
                 account_keys={} loaded={}/{} fee={} error={:?}",
                bs58::encode(&transaction_info.signature).into_string(),
                transaction_info.slot,
                transaction_info.index,
                transaction_info.message_type,
                transaction_info
                    .legacy_message
                    .as_ref()
                    .map(|message| message.account_keys.len())
                    .or_else(|| {
                        transaction_info
                            .v0_loaded_message
                            .as_ref()
                            .map(|message| message.message.account_keys.len())
                    })
                    .unwrap_or_default(),
                transaction_info
                    .v0_loaded_message
                    .as_ref()
                    .map_or(0, |message| message.loaded_addresses.writable.len()),
                transaction_info
                    .v0_loaded_message
                    .as_ref()
                    .map_or(0, |message| message.loaded_addresses.readonly.len()),
                transaction_info.meta.fee,
                transaction_info.meta.error,
            );
            trace!("queue transaction row {transaction_info:?}");
        }
        let wrk_item =
            DbWorkItem::LogTransaction(Box::new(LogTransactionRequest { transaction_info }));

        if let Err(err) = self.sender.send(wrk_item) {
            return Err(GeyserPluginError::TransactionUpdateError {
                msg: format!("Failed to update the transaction, error: {:?}", err),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*, agave_geyser_plugin_interface::transaction_status_meta::InnerInstruction,
        solana_instruction_error::InstructionError, solana_pubkey::Pubkey,
    };

    const CREATE_SCHEMA: &str = include_str!("../../scripts/create_schema.sql");
    const MIGRATION: &str = include_str!("../../scripts/migrate_agave_v4_3.sql");

    #[test]
    fn test_bounded_text_keeps_head() {
        let text = format!("{}{}", "a".repeat(MAX_TRANSACTION_STATUS_LEN), "TAIL");
        let bounded = bounded_text(text, MAX_TRANSACTION_STATUS_LEN);
        assert_eq!(bounded, "a".repeat(MAX_TRANSACTION_STATUS_LEN));

        // Multi-byte characters are counted as characters, as VARCHAR(n) does.
        let text = "\u{3042}".repeat(300);
        let bounded = bounded_text(text, MAX_TRANSACTION_STATUS_LEN);
        assert_eq!(bounded.chars().count(), MAX_TRANSACTION_STATUS_LEN);

        assert_eq!(bounded_text("a\0b".to_string(), 10), "ab");
        assert_eq!(bounded_text("short".to_string(), 10), "short");
    }

    #[test]
    fn test_instruction_error_detail_head() {
        let error = TransactionError::InstructionError(3, InstructionError::Custom(6001));
        let db_error = get_transaction_error(&Err(&error)).unwrap();
        assert_eq!(
            db_error.error_code,
            DbTransactionErrorCode::InstructionError
        );
        let detail = db_error.error_detail.unwrap();
        assert!(detail.starts_with("InstructionError: idx (3), error: ("));
        assert!(detail.chars().count() <= MAX_TRANSACTION_STATUS_LEN);
    }

    #[test]
    fn test_all_transaction_errors_map_without_panic() {
        // Every variant of the solana-transaction-error the validator is built
        // with. A variant added by an upgrade lands in `Unknown` until it gets a
        // label (with a migration), and this list makes the upgrade notice it.
        let unlabeled = ["BailOut"];
        assert_eq!(TransactionError::VARIANTS.len(), 40);
        for error in TransactionError::VARIANTS {
            let name = format!("{error:?}");
            let db_error = get_transaction_error(&Err(&error)).unwrap();
            if unlabeled
                .iter()
                .any(|unlabeled| name.starts_with(unlabeled))
            {
                assert_eq!(db_error.error_code, DbTransactionErrorCode::Unknown);
                assert_eq!(db_error.error_detail.as_deref(), Some(name.as_str()));
                continue;
            }
            assert_ne!(
                db_error.error_code,
                DbTransactionErrorCode::Unknown,
                "{error:?}"
            );
            let label = format!("'{:?}'", db_error.error_code);
            assert!(
                CREATE_SCHEMA.contains(&label),
                "{label} missing from schema"
            );
        }
        for label in ["ProgramCacheHitMaxLimit", "CommitCancelled", "Unknown"] {
            assert!(MIGRATION.contains(&format!("ADD VALUE IF NOT EXISTS '{label}'")));
        }
        assert!(get_transaction_error(&Ok(())).is_none());
    }

    #[test]
    fn test_reward_types_in_schema() {
        for reward_type in [
            RewardType::Fee,
            RewardType::Rent,
            RewardType::Staking,
            RewardType::Voting,
            RewardType::DeactivatedStake,
            RewardType::VATDebit,
        ] {
            let label = format!("'{:?}'", DbRewardType::from(&reward_type));
            assert!(
                CREATE_SCHEMA.contains(&label),
                "{label} missing from schema"
            );
        }
        for label in ["DeactivatedStake", "VATDebit"] {
            assert!(MIGRATION.contains(&format!("ADD VALUE IF NOT EXISTS '{label}'")));
        }
    }

    fn sample_meta<'a>(
        status: Result<(), &'a TransactionError>,
        inner_instructions: &'a [InnerInstructions<'a>],
        log_messages: &'a [&'a str],
        loaded_addresses: &'a LoadedAddresses,
    ) -> TransactionStatusMeta<'a> {
        TransactionStatusMeta {
            status,
            fee: 5000,
            pre_balances: &[10, 20],
            post_balances: &[5, 20],
            inner_instructions: Some(inner_instructions),
            log_messages: Some(log_messages),
            pre_token_balances: None,
            post_token_balances: None,
            rewards: None,
            loaded_addresses,
            return_data: None,
            compute_units_consumed: None,
            cost_units: None,
        }
    }

    #[test]
    fn test_build_v0_transaction_uses_loaded_addresses() {
        let payer = Pubkey::new_from_array([1; 32]);
        let program = Pubkey::new_from_array([2; 32]);
        let table = Pubkey::new_from_array([3; 32]);
        let loaded_writable = Pubkey::new_from_array([4; 32]);
        let loaded_readonly = Pubkey::new_from_array([5; 32]);
        let message = v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, program],
            recent_blockhash: Hash::new_from_array([9; 32]),
            instructions: vec![CompiledInstruction::new_from_raw_parts(
                1,
                vec![1, 2],
                vec![0, 2, 3],
            )],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: table,
                writable_indexes: vec![7],
                readonly_indexes: vec![8],
            }],
        };
        let transaction = VersionedTransaction {
            signatures: vec![Signature::from([6; 64])],
            message: VersionedMessage::V0(message),
        };
        let loaded_addresses = LoadedAddresses {
            writable: vec![loaded_writable],
            readonly: vec![loaded_readonly],
        };
        let inner_instruction = CompiledInstruction::new_from_raw_parts(1, vec![7], vec![0]);
        let inner = [InnerInstruction {
            instruction: &inner_instruction,
            stack_height: Some(2),
        }];
        let inner_instructions = [InnerInstructions {
            index: 0,
            instructions: &inner,
        }];
        let error = TransactionError::CommitCancelled;
        let meta = sample_meta(
            Err(&error),
            &inner_instructions,
            &["Program log: a\0b"],
            &loaded_addresses,
        );
        let message_hash = Hash::new_from_array([8; 32]);
        let notification = TransactionNotification {
            signature: &transaction.signatures[0],
            is_vote: false,
            transaction: &transaction,
            message_hash: &message_hash,
            meta: &meta,
            index: 17,
        };
        let db = build_db_transaction(42, &notification, 3);
        assert_eq!(db.message_type, MESSAGE_TYPE_V0);
        assert!(db.legacy_message.is_none());
        let loaded = db.v0_loaded_message.unwrap();
        assert_eq!(loaded.message.account_keys[0], payer.as_ref().to_vec());
        assert_eq!(
            loaded.loaded_addresses.writable,
            vec![loaded_writable.as_ref().to_vec()]
        );
        assert_eq!(
            loaded.loaded_addresses.readonly,
            vec![loaded_readonly.as_ref().to_vec()]
        );
        assert_eq!(db.message_hash, vec![8; 32]);
        assert_eq!(db.index, 17);
        assert_eq!(db.slot, 42);
        assert_eq!(db.write_version, 3);
        assert_eq!(db.meta.fee, 5000);
        assert_eq!(
            db.meta.error.unwrap().error_code,
            DbTransactionErrorCode::CommitCancelled
        );
        assert_eq!(
            db.meta.log_messages.unwrap(),
            vec!["Program log: ab".to_string()]
        );
    }

    #[test]
    fn test_build_legacy_transaction() {
        let payer = Pubkey::new_from_array([1; 32]);
        let message = Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            account_keys: vec![payer],
            recent_blockhash: Hash::new_from_array([9; 32]),
            instructions: vec![],
        };
        let transaction = VersionedTransaction {
            signatures: vec![Signature::from([6; 64])],
            message: VersionedMessage::Legacy(message),
        };
        let loaded_addresses = LoadedAddresses::default();
        let meta = sample_meta(Ok(()), &[], &[], &loaded_addresses);
        let message_hash = Hash::default();
        let notification = TransactionNotification {
            signature: &transaction.signatures[0],
            is_vote: true,
            transaction: &transaction,
            message_hash: &message_hash,
            meta: &meta,
            index: 0,
        };
        let db = build_db_transaction(1, &notification, 1);
        assert_eq!(db.message_type, MESSAGE_TYPE_LEGACY);
        assert!(db.v0_loaded_message.is_none());
        assert_eq!(
            db.legacy_message.unwrap().account_keys,
            vec![payer.as_ref().to_vec()]
        );
        assert!(db.meta.error.is_none());
        assert!(db.is_vote);
    }
}
