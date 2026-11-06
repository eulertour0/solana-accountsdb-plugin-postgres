//! Conversion of a JSON-RPC `getTransaction` response (`"encoding": "base64"`)
//! back into the native `VersionedTransaction` + `TransactionStatusMeta` that the
//! validator builds, so that `pg_backfill` can store rows for slots the plugin
//! missed through the plugin's own `build_db_transaction`. The owned meta is
//! borrowed into the geyser interface's mirror types by `with_mirror_meta`, the
//! same conversion agave's geyser plugin manager applies before notifying the
//! plugin (agave v4.4 `transaction_notifier.rs`).
//!
//! agave's RPC produces the response with `VersionedTransactionWithStatusMeta::encode`
//! (transaction-status) from the meta in the blockstore / long-term storage, which
//! is the `TransactionStatusMeta` the transaction status service also sent to the
//! plugin. `ui_meta_to_native` is the inverse of `UiTransactionStatusMeta::from`;
//! the tests re-encode the converted values with agave's encoder and compare the
//! result with the original response.
//!
//! The only field that does not survive storage is `ui_token_amount.ui_amount`:
//! the protobuf storage keeps it as a plain f64 and reads an amount of
//! (nearly) zero back as `None`, while the validator reported
//! `Some(amount / 10^decimals)`. `token_balance_to_native` recomputes it.

use {
    crate::postgres_client::postgres_client_transaction::{
        build_db_transaction_for_store, DbTransaction, TransactionNotification,
    },
    agave_geyser_plugin_interface::transaction_status_meta as mirror,
    base64::{prelude::BASE64_STANDARD, Engine},
    solana_hash::Hash,
    solana_message::{compiled_instruction::CompiledInstruction, v0::LoadedAddresses},
    solana_pubkey::Pubkey,
    solana_transaction::{
        simple_vote_transaction_checker::is_simple_vote_transaction_impl,
        versioned::VersionedTransaction,
    },
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::TransactionError,
    solana_transaction_status::{
        option_serializer::OptionSerializer, EncodedConfirmedTransactionWithStatusMeta,
        InnerInstruction, InnerInstructions, TransactionStatusMeta, TransactionTokenBalance,
        UiInnerInstructions, UiInstruction, UiLoadedAddresses, UiTransactionReturnData,
        UiTransactionStatusMeta, UiTransactionTokenBalance,
    },
    std::str::FromStr,
    thiserror::Error,
};

/// `transaction.index` written when the RPC response carries no
/// `transactionIndex` (RPC nodes older than agave 4.3).
pub const UNKNOWN_TRANSACTION_INDEX: i64 = -1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConvertError {
    #[error("the response has no transaction status meta")]
    MissingMeta,
    #[error("the transaction is not binary encoded or does not decode/sanitize")]
    UndecodableTransaction,
    #[error("parsed (jsonParsed) instruction in innerInstructions; request base64")]
    ParsedInstruction,
    #[error("invalid {what}: {value}")]
    Invalid { what: &'static str, value: String },
}

fn invalid(what: &'static str, value: impl ToString) -> ConvertError {
    ConvertError::Invalid {
        what,
        value: value.to_string(),
    }
}

/// A transaction as the validator would have notified it to the plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeTransaction {
    pub slot: u64,
    pub block_time: Option<i64>,
    /// Position of the transaction in the block (`transactionIndex`), the same
    /// value the validator passes as `ReplicaTransactionInfoV4::index`.
    pub index: Option<u32>,
    pub transaction: VersionedTransaction,
    pub meta: TransactionStatusMeta,
    pub message_hash: Hash,
    pub is_vote: bool,
}

impl NativeTransaction {
    /// Calls `f` with the notification the validator would have sent for this
    /// transaction.
    pub fn with_notification<R>(&self, f: impl FnOnce(&TransactionNotification) -> R) -> R {
        with_mirror_meta(&self.meta, |meta| {
            f(&TransactionNotification {
                signature: &self.transaction.signatures[0],
                is_vote: self.is_vote,
                transaction: &self.transaction,
                message_hash: &self.message_hash,
                meta,
                index: self.index.unwrap_or_default() as usize,
            })
        })
    }

    /// Static keys plus the keys loaded from address lookup tables, the set the
    /// plugin's transaction selector looks at.
    pub fn mentioned_addresses(&self) -> impl Iterator<Item = &Pubkey> {
        self.transaction
            .message
            .static_account_keys()
            .iter()
            .chain(self.meta.loaded_addresses.writable.iter())
            .chain(self.meta.loaded_addresses.readonly.iter())
    }

    /// The row the plugin would have written. `write_version` is a per-process
    /// counter in the plugin; a backfilled row uses the transaction index so
    /// that the order within a slot is kept.
    pub fn to_db_transaction(&self, store_log_messages: bool) -> DbTransaction {
        let index = self.index.map(u64::from);
        let mut transaction = self.with_notification(|notification| {
            build_db_transaction_for_store(
                self.slot,
                notification,
                index.unwrap_or_default(),
                store_log_messages,
            )
        });
        if index.is_none() {
            transaction.index = UNKNOWN_TRANSACTION_INDEX;
        }
        transaction
    }
}

/// Borrows an owned `TransactionStatusMeta` as the interface's mirror type and
/// calls `f` with it. Field for field the conversion agave v4.4's geyser plugin
/// manager performs (`TransactionNotifierImpl::notify_transaction`); destructured
/// exhaustively so that a field added to the owned type fails the build here.
pub fn with_mirror_meta<R>(
    meta: &TransactionStatusMeta,
    f: impl FnOnce(&mirror::TransactionStatusMeta) -> R,
) -> R {
    let TransactionStatusMeta {
        status,
        fee,
        pre_balances,
        post_balances,
        inner_instructions,
        log_messages,
        pre_token_balances,
        post_token_balances,
        rewards,
        loaded_addresses,
        return_data,
        compute_units_consumed,
        cost_units,
    } = meta;

    let inner_instructions_rows: Option<Vec<Vec<mirror::InnerInstruction>>> =
        inner_instructions.as_ref().map(|groups| {
            groups
                .iter()
                .map(|group| {
                    group
                        .instructions
                        .iter()
                        .map(|inner| mirror::InnerInstruction {
                            instruction: &inner.instruction,
                            stack_height: inner.stack_height,
                        })
                        .collect()
                })
                .collect()
        });
    let inner_instructions: Option<Vec<mirror::InnerInstructions>> = inner_instructions
        .as_ref()
        .zip(inner_instructions_rows.as_ref())
        .map(|(groups, rows)| {
            groups
                .iter()
                .zip(rows)
                .map(|(group, row)| mirror::InnerInstructions {
                    index: group.index,
                    instructions: row,
                })
                .collect()
        });
    let log_messages: Option<Vec<&str>> = log_messages
        .as_ref()
        .map(|logs| logs.iter().map(String::as_str).collect());
    fn token_balances(
        balances: &Option<Vec<TransactionTokenBalance>>,
    ) -> Option<Vec<mirror::TransactionTokenBalance<'_>>> {
        balances.as_ref().map(|balances| {
            balances
                .iter()
                .map(|balance| mirror::TransactionTokenBalance {
                    account_index: balance.account_index,
                    mint: &balance.mint,
                    ui_token_amount: mirror::UiTokenAmount {
                        ui_amount: balance.ui_token_amount.ui_amount,
                        decimals: balance.ui_token_amount.decimals,
                        amount: &balance.ui_token_amount.amount,
                        ui_amount_string: &balance.ui_token_amount.ui_amount_string,
                    },
                    owner: &balance.owner,
                    program_id: &balance.program_id,
                })
                .collect()
        })
    }
    let pre_token_balances = token_balances(pre_token_balances);
    let post_token_balances = token_balances(post_token_balances);
    let rewards: Option<Vec<mirror::Reward>> = rewards.as_ref().map(|rewards| {
        rewards
            .iter()
            .map(|reward| mirror::Reward {
                pubkey: &reward.pubkey,
                lamports: reward.lamports,
                post_balance: reward.post_balance,
                reward_type: reward.reward_type,
                commission: reward.commission,
                commission_bps: reward.commission_bps,
            })
            .collect()
    });

    f(&mirror::TransactionStatusMeta {
        status: match status {
            Ok(()) => Ok(()),
            Err(err) => Err(err),
        },
        fee: *fee,
        pre_balances,
        post_balances,
        inner_instructions: inner_instructions.as_deref(),
        log_messages: log_messages.as_deref(),
        pre_token_balances: pre_token_balances.as_deref(),
        post_token_balances: post_token_balances.as_deref(),
        rewards: rewards.as_deref(),
        loaded_addresses,
        return_data: return_data
            .as_ref()
            .map(|data| mirror::TransactionReturnData {
                program_id: data.program_id,
                data: &data.data,
            }),
        compute_units_consumed: *compute_units_consumed,
        cost_units: *cost_units,
    })
}

/// Same rule as the validator (`is_simple_vote_transaction`): 1 or 2 signatures,
/// a legacy message and a single instruction of the vote program.
pub fn is_simple_vote_transaction(transaction: &VersionedTransaction) -> bool {
    let account_keys = transaction.message.static_account_keys();
    let is_legacy_message = matches!(
        transaction.message,
        solana_message::VersionedMessage::Legacy(_)
    );
    let mut programs_valid = true;
    let programs: Vec<&Pubkey> = transaction
        .message
        .instructions()
        .iter()
        .filter_map(|instruction| {
            let program = account_keys.get(usize::from(instruction.program_id_index));
            programs_valid &= program.is_some();
            program
        })
        .collect();
    programs_valid
        && is_simple_vote_transaction_impl(
            &transaction.signatures,
            is_legacy_message,
            programs.into_iter(),
        )
}

pub fn convert_rpc_transaction(
    response: EncodedConfirmedTransactionWithStatusMeta,
) -> Result<NativeTransaction, ConvertError> {
    let EncodedConfirmedTransactionWithStatusMeta {
        slot,
        transaction,
        block_time,
        transaction_index,
    } = response;
    let versioned = transaction
        .transaction
        .decode()
        .ok_or(ConvertError::UndecodableTransaction)?;
    if versioned.signatures.is_empty() {
        return Err(ConvertError::UndecodableTransaction);
    }
    let meta = ui_meta_to_native(transaction.meta.ok_or(ConvertError::MissingMeta)?)?;
    let message_hash = versioned.message.hash();
    let is_vote = is_simple_vote_transaction(&versioned);
    Ok(NativeTransaction {
        slot,
        block_time,
        index: transaction_index,
        transaction: versioned,
        meta,
        message_hash,
        is_vote,
    })
}

fn option<T>(value: OptionSerializer<T>) -> Option<T> {
    value.into()
}

fn pubkey(what: &'static str, value: &str) -> Result<Pubkey, ConvertError> {
    Pubkey::from_str(value).map_err(|_| invalid(what, value))
}

fn loaded_addresses_to_native(
    loaded: Option<UiLoadedAddresses>,
) -> Result<LoadedAddresses, ConvertError> {
    let Some(loaded) = loaded else {
        return Ok(LoadedAddresses::default());
    };
    let keys = |keys: &[String]| {
        keys.iter()
            .map(|key| pubkey("loaded address", key))
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(LoadedAddresses {
        writable: keys(&loaded.writable)?,
        readonly: keys(&loaded.readonly)?,
    })
}

fn inner_instructions_to_native(
    inner: UiInnerInstructions,
) -> Result<InnerInstructions, ConvertError> {
    let instructions = inner
        .instructions
        .into_iter()
        .map(|instruction| match instruction {
            UiInstruction::Compiled(compiled) => Ok(InnerInstruction {
                instruction: CompiledInstruction::new_from_raw_parts(
                    compiled.program_id_index,
                    bs58::decode(&compiled.data)
                        .into_vec()
                        .map_err(|_| invalid("inner instruction data", &compiled.data))?,
                    compiled.accounts,
                ),
                stack_height: compiled.stack_height,
            }),
            UiInstruction::Parsed(_) => Err(ConvertError::ParsedInstruction),
        })
        .collect::<Result<_, _>>()?;
    Ok(InnerInstructions {
        index: inner.index,
        instructions,
    })
}

/// See the module documentation for the `ui_amount` rule. Amounts whose
/// `ui_amount` survived storage are taken as they are (they are bit-exact f64).
fn token_balance_to_native(balance: UiTransactionTokenBalance) -> TransactionTokenBalance {
    let mut ui_token_amount = balance.ui_token_amount;
    if ui_token_amount.ui_amount.is_none() {
        if let (Ok(amount), Some(divisor)) = (
            u64::from_str(&ui_token_amount.amount),
            10_usize.checked_pow(u32::from(ui_token_amount.decimals)),
        ) {
            ui_token_amount.ui_amount = Some(amount as f64 / divisor as f64);
        }
    }
    TransactionTokenBalance {
        account_index: balance.account_index,
        mint: balance.mint,
        ui_token_amount,
        owner: option(balance.owner).unwrap_or_default(),
        program_id: option(balance.program_id).unwrap_or_default(),
    }
}

fn return_data_to_native(
    return_data: UiTransactionReturnData,
) -> Result<TransactionReturnData, ConvertError> {
    // UiReturnDataEncoding has a single variant, Base64.
    let (data, _encoding) = return_data.data;
    Ok(TransactionReturnData {
        program_id: pubkey("return data program id", &return_data.program_id)?,
        data: BASE64_STANDARD
            .decode(&data)
            .map_err(|_| invalid("return data", &data))?,
    })
}

/// Inverse of `UiTransactionStatusMeta::from(TransactionStatusMeta)`.
pub fn ui_meta_to_native(
    meta: UiTransactionStatusMeta,
) -> Result<TransactionStatusMeta, ConvertError> {
    // `err` and the deprecated `status` carry the same value; `status` is always
    // present.
    let status = meta.status.map_err(TransactionError::from);
    let token_balances = |balances: OptionSerializer<Vec<UiTransactionTokenBalance>>| {
        option(balances).map(|balances| {
            balances
                .into_iter()
                .map(token_balance_to_native)
                .collect::<Vec<_>>()
        })
    };
    Ok(TransactionStatusMeta {
        status,
        fee: meta.fee,
        pre_balances: meta.pre_balances,
        post_balances: meta.post_balances,
        inner_instructions: option(meta.inner_instructions)
            .map(|inner| {
                inner
                    .into_iter()
                    .map(inner_instructions_to_native)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?,
        log_messages: option(meta.log_messages),
        pre_token_balances: token_balances(meta.pre_token_balances),
        post_token_balances: token_balances(meta.post_token_balances),
        rewards: option(meta.rewards),
        loaded_addresses: loaded_addresses_to_native(option(meta.loaded_addresses))?,
        return_data: option(meta.return_data)
            .map(return_data_to_native)
            .transpose()?,
        compute_units_consumed: option(meta.compute_units_consumed),
        cost_units: option(meta.cost_units),
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            postgres_client::postgres_client_transaction::{
                build_db_transaction, MESSAGE_TYPE_LEGACY, MESSAGE_TYPE_V0, MESSAGE_TYPE_V1,
            },
            transaction_selector::TransactionSelector,
        },
        serde_json::Value,
        solana_message::VersionedMessage,
        solana_transaction_status::{UiTransactionEncoding, VersionedTransactionWithStatusMeta},
    };

    const LEGACY_OK: &str = include_str!("../tests/fixtures/backfill/legacy_ok.json");
    const LEGACY_FAILED: &str = include_str!("../tests/fixtures/backfill/legacy_failed.json");
    const V0_ALT: &str = include_str!("../tests/fixtures/backfill/v0_alt.json");
    const V1_OK: &str = include_str!("../tests/fixtures/backfill/v1_ok.json");
    const V1_FAILED: &str = include_str!("../tests/fixtures/backfill/v1_failed.json");
    const LEGACY_VOTE: &str = include_str!("../tests/fixtures/backfill/legacy_vote.json");
    const FIXTURES: [&str; 6] = [
        LEGACY_OK,
        LEGACY_FAILED,
        V0_ALT,
        V1_OK,
        V1_FAILED,
        LEGACY_VOTE,
    ];

    fn convert(fixture: &str) -> (Value, NativeTransaction) {
        let json: Value = serde_json::from_str(fixture).unwrap();
        let response = serde_json::from_value(json.clone()).unwrap();
        (json, convert_rpc_transaction(response).unwrap())
    }

    /// Removes the uiAmount values the conversion recomputes (null in storage).
    fn strip_recomputed_ui_amounts(meta: &mut Value) -> usize {
        let mut stripped = 0;
        for key in ["preTokenBalances", "postTokenBalances"] {
            for balance in meta[key].as_array_mut().into_iter().flatten() {
                let amount = &mut balance["uiTokenAmount"];
                if amount["amount"] == "0" {
                    amount["uiAmount"] = Value::Null;
                    stripped += 1;
                }
            }
        }
        stripped
    }

    /// Re-encoding the converted values with agave's own RPC encoder must give
    /// back the response, i.e. the conversion loses nothing the RPC exposes.
    #[test]
    fn test_reencode_matches_response() {
        let mut recomputed = 0;
        for fixture in FIXTURES {
            let (json, native) = convert(fixture);
            let encoded = VersionedTransactionWithStatusMeta {
                transaction: native.transaction.clone(),
                meta: native.meta.clone(),
            }
            .encode(UiTransactionEncoding::Base64, Some(1), true)
            .unwrap();
            let mut encoded = serde_json::to_value(encoded).unwrap();
            let mut expected_meta = json["meta"].clone();
            recomputed += strip_recomputed_ui_amounts(&mut expected_meta);
            strip_recomputed_ui_amounts(&mut encoded["meta"]);
            assert_eq!(encoded["meta"], expected_meta);
            assert_eq!(encoded["transaction"], json["transaction"]);
            assert_eq!(encoded["version"], json["version"]);
            assert_eq!(native.slot, json["slot"].as_u64().unwrap());
            assert_eq!(
                native.index.map(u64::from),
                json["transactionIndex"].as_u64()
            );
        }
        // v1_failed has zero balances whose uiAmount storage dropped.
        assert!(recomputed > 0);
    }

    #[test]
    fn test_zero_ui_amount_recomputed() {
        let (json, native) = convert(V1_FAILED);
        let raw = &json["meta"]["postTokenBalances"];
        let mut checked = 0;
        for (raw, native) in raw
            .as_array()
            .unwrap()
            .iter()
            .zip(native.meta.post_token_balances.as_ref().unwrap())
        {
            let amount = &raw["uiTokenAmount"];
            if amount["amount"] == "0" {
                assert!(amount["uiAmount"].is_null());
                // What the validator's token_amount_to_ui_amount_v3 produced.
                assert_eq!(native.ui_token_amount.ui_amount, Some(0.0));
                checked += 1;
            } else {
                assert_eq!(
                    native.ui_token_amount.ui_amount,
                    amount["uiAmount"].as_f64()
                );
            }
        }
        assert!(checked > 0);
    }

    /// The message hash the validator computes on replay is the hash of the raw
    /// message bytes from the wire.
    #[test]
    fn test_message_hash_is_raw_message_hash() {
        for fixture in FIXTURES {
            let (json, native) = convert(fixture);
            let raw = BASE64_STANDARD
                .decode(json["transaction"][0].as_str().unwrap())
                .unwrap();
            let signatures = native.transaction.signatures.len();
            let message = if raw[0] & 0x80 == 0 {
                // Legacy/v0: compact-u16 signature count (one byte below 128),
                // the signatures, then the message.
                assert_eq!(usize::from(raw[0]), signatures);
                &raw[1 + 64 * signatures..]
            } else {
                // v1: the message, then the signatures without a length prefix.
                assert!(matches!(
                    native.transaction.message,
                    VersionedMessage::V1(_)
                ));
                &raw[..raw.len() - 64 * signatures]
            };
            assert_eq!(
                native.message_hash,
                VersionedMessage::hash_raw_message(message)
            );
        }
    }

    fn key_bytes(keys: &Value) -> Vec<Vec<u8>> {
        keys.as_array()
            .unwrap()
            .iter()
            .map(|key| bs58::decode(key.as_str().unwrap()).into_vec().unwrap())
            .collect()
    }

    /// The row built from the RPC response is the row `build_db_transaction`
    /// builds from the native values, and its fields agree with the raw JSON.
    #[test]
    fn test_db_transaction_from_response() {
        for (fixture, message_type, failed) in [
            (LEGACY_OK, MESSAGE_TYPE_LEGACY, false),
            (LEGACY_FAILED, MESSAGE_TYPE_LEGACY, true),
            (V0_ALT, MESSAGE_TYPE_V0, false),
            (V1_OK, MESSAGE_TYPE_V1, false),
            (V1_FAILED, MESSAGE_TYPE_V1, true),
        ] {
            let (json, native) = convert(fixture);
            let meta = &json["meta"];
            let index = json["transactionIndex"].as_u64().unwrap();

            let with_logs = native.to_db_transaction(true);
            let reference = with_mirror_meta(&native.meta, |meta| {
                build_db_transaction(
                    native.slot,
                    &TransactionNotification {
                        signature: &native.transaction.signatures[0],
                        is_vote: false,
                        transaction: &native.transaction,
                        message_hash: &native.transaction.message.hash(),
                        meta,
                        index: index as usize,
                    },
                    index,
                )
            });
            assert_eq!(format!("{with_logs:?}"), format!("{reference:?}"));

            let db = native.to_db_transaction(false);
            assert_eq!(db.message_type, message_type);
            assert!(!db.is_vote);
            assert_eq!(db.slot as u64, json["slot"].as_u64().unwrap());
            assert_eq!(db.index as u64, index);
            assert_eq!(db.write_version as u64, index);
            assert_eq!(db.signatures.len(), native.transaction.signatures.len());
            assert_eq!(db.signature, db.signatures[0]);
            assert!(db.meta.log_messages.is_none());
            assert_eq!(
                with_logs.meta.log_messages.unwrap().len(),
                meta["logMessages"].as_array().unwrap().len()
            );
            assert_eq!(db.meta.fee as u64, meta["fee"].as_u64().unwrap());
            assert_eq!(
                db.meta
                    .pre_balances
                    .iter()
                    .map(|b| *b as u64)
                    .collect::<Vec<_>>(),
                serde_json::from_value::<Vec<u64>>(meta["preBalances"].clone()).unwrap()
            );
            assert_eq!(db.meta.error.is_some(), failed);
            assert_eq!(meta["err"].is_null(), !failed);

            // Inner instructions: index, program and data bytes (base58 in JSON).
            let raw_inner = meta["innerInstructions"].as_array().unwrap();
            let db_inner = db.meta.inner_instructions.as_ref().unwrap();
            assert_eq!(db_inner.len(), raw_inner.len());
            for (db_inner, raw_inner) in db_inner.iter().zip(raw_inner) {
                assert_eq!(db_inner.index as u64, raw_inner["index"].as_u64().unwrap());
                let raw_instructions = raw_inner["instructions"].as_array().unwrap();
                assert_eq!(db_inner.instructions.len(), raw_instructions.len());
                for (db_ix, raw_ix) in db_inner.instructions.iter().zip(raw_instructions) {
                    assert_eq!(
                        db_ix.program_id_index as u64,
                        raw_ix["programIdIndex"].as_u64().unwrap()
                    );
                    assert_eq!(
                        db_ix.data,
                        bs58::decode(raw_ix["data"].as_str().unwrap())
                            .into_vec()
                            .unwrap()
                    );
                }
            }

            // Token balances: owner and mint strings, ui amount.
            let raw_post = meta["postTokenBalances"].as_array().unwrap();
            let db_post = db.meta.post_token_balances.as_ref().unwrap();
            assert_eq!(db_post.len(), raw_post.len());
            for (db_balance, raw_balance) in db_post.iter().zip(raw_post) {
                assert_eq!(db_balance.mint, raw_balance["mint"].as_str().unwrap());
                assert_eq!(db_balance.owner, raw_balance["owner"].as_str().unwrap());
            }
            // The transaction status service writes `rewards: Some(vec![])`.
            assert_eq!(db.meta.rewards.as_ref().map(Vec::len), Some(0));

            match message_type {
                MESSAGE_TYPE_V0 => {
                    let loaded = &db.v0_loaded_message.as_ref().unwrap().loaded_addresses;
                    assert_eq!(
                        loaded.writable,
                        key_bytes(&meta["loadedAddresses"]["writable"])
                    );
                    assert_eq!(
                        loaded.readonly,
                        key_bytes(&meta["loadedAddresses"]["readonly"])
                    );
                    assert!(!loaded.writable.is_empty() && !loaded.readonly.is_empty());
                    assert!(native.meta.return_data.is_some());
                    assert!(db.legacy_message.is_none());
                }
                _ => {
                    assert!(db.v0_loaded_message.is_none());
                    assert!(db.legacy_message.is_some());
                }
            }
        }
    }

    #[test]
    fn test_error_mapping() {
        let (_, native) = convert(LEGACY_FAILED);
        assert_eq!(
            native.meta.status,
            Err(TransactionError::InsufficientFundsForRent { account_index: 12 })
        );
        let error = format!("{:?}", native.to_db_transaction(false).meta.error.unwrap());
        assert!(error.contains("InsufficientFundsForRent"), "{error}");
        assert!(error.contains("account_index (12)"), "{error}");
        let (_, native) = convert(V1_FAILED);
        assert!(matches!(
            native.meta.status,
            Err(TransactionError::InstructionError(1, _))
        ));
        let detail = native.to_db_transaction(false).meta.error.unwrap();
        assert!(format!("{detail:?}")
            .contains("InstructionError: idx (1), error: (custom program error: 0x101)"));
    }

    #[test]
    fn test_selector_sees_loaded_addresses() {
        let (json, native) = convert(V0_ALT);
        let only_loaded = json["meta"]["loadedAddresses"]["readonly"][0]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!native
            .transaction
            .message
            .static_account_keys()
            .iter()
            .any(|key| key.to_string() == only_loaded));
        let selector = TransactionSelector::new(&[only_loaded]).unwrap();
        assert!(selector.is_transaction_selected(native.is_vote, native.mentioned_addresses()));
        let selector = TransactionSelector::new(&[Pubkey::new_unique().to_string()]).unwrap();
        assert!(!selector.is_transaction_selected(native.is_vote, native.mentioned_addresses()));
    }

    #[test]
    fn test_is_vote() {
        assert!(convert(LEGACY_VOTE).1.is_vote);
        assert!(convert(LEGACY_VOTE).1.to_db_transaction(false).is_vote);
        for fixture in [LEGACY_OK, LEGACY_FAILED, V0_ALT, V1_OK, V1_FAILED] {
            assert!(!convert(fixture).1.is_vote);
        }
    }

    #[test]
    fn test_unknown_index_sentinel() {
        let (_, mut native) = convert(LEGACY_OK);
        native.index = None;
        let db = native.to_db_transaction(false);
        assert_eq!(db.index, UNKNOWN_TRANSACTION_INDEX);
        assert_eq!(db.write_version, 0);
    }
}
