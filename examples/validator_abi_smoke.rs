//! ABI smoke test: dlopen the built plugin the way agave's geyser-plugin-manager
//! does and drive every callback agave v4.4.1 uses, with values built from the
//! same agave-geyser-plugin-interface (crates.io 5.0.0) the validator is compiled
//! from.
//!
//!   cargo run --release --example validator_abi_smoke -- \
//!       $CARGO_TARGET_DIR/release/libsolana_geyser_plugin_postgres.so
//!
//! No PostgreSQL is needed: the config points at a closed port, the workers fail
//! to connect (panic_on_db_errors=false) and the queued items are only checked
//! through the plugin's trace logs, which this tool captures via setup_logger.
//! The full rows the loaded library queued are compared with the rows this
//! process builds from the same values through the crate's rlib.
//! A layout or vtable mismatch shows up as a crash or as wrong values below.
use {
    agave_geyser_plugin_interface::{
        block_footer::{BlockFooterV1, VersionedBlockFooter},
        geyser_plugin_interface::{
            GeyserPlugin, ReplicaAccountInfoV3, ReplicaAccountInfoVersions, ReplicaBlockFooterInfo,
            ReplicaBlockFooterInfoVersions, ReplicaBlockInfoV5, ReplicaBlockInfoVersions,
            ReplicaEntryInfoV2, ReplicaEntryInfoVersions, ReplicaTransactionInfoV4,
            ReplicaTransactionInfoVersions, SlotStatus,
        },
        transaction_status_meta::{
            InnerInstruction, InnerInstructions, Reward, RewardsAndNumPartitions,
            TransactionReturnData, TransactionStatusMeta, TransactionTokenBalance, UiTokenAmount,
        },
    },
    solana_geyser_plugin_postgres::postgres_client::{
        postgres_client_block_metadata::DbBlockInfo,
        postgres_client_transaction::{build_db_transaction, TransactionNotification},
    },
    solana_hash::Hash,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        v0::{self, LoadedAddresses, MessageAddressTableLookup},
        MessageHeader, VersionedMessage,
    },
    solana_pubkey::Pubkey,
    solana_reward_info::RewardType,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_error::TransactionError,
    std::{
        ffi::{c_char, c_void, CString},
        sync::Mutex,
    },
};

#[link(name = "dl")]
extern "C" {
    fn dlopen(path: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> i32;
}

const RTLD_NOW: i32 = 2;

struct Capture(Mutex<Vec<String>>);

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        let line = format!("{} {}", record.level(), record.args());
        if std::env::var_os("SMOKE_VERBOSE").is_some() {
            eprintln!("plugin: {line}");
        }
        self.0.lock().unwrap().push(line);
    }
    fn flush(&self) {}
}

static LOGGER: Capture = Capture(Mutex::new(Vec::new()));

fn captured(pattern: &str) -> Vec<String> {
    LOGGER
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(pattern))
        .cloned()
        .collect()
}

fn v0_transaction(payer: Pubkey, table: Pubkey, seed: u8) -> VersionedTransaction {
    VersionedTransaction {
        signatures: vec![Signature::from([seed; 64])],
        message: VersionedMessage::V0(v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, Pubkey::new_from_array([0x22; 32])],
            recent_blockhash: Hash::new_from_array([0x33; 32]),
            instructions: vec![CompiledInstruction::new_from_raw_parts(
                1,
                vec![0, 2, 3],
                vec![1, 2, 3],
            )],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: table,
                writable_indexes: vec![0],
                readonly_indexes: vec![1],
            }],
        }),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 {
        return Err("usage: validator_abi_smoke PLUGIN_SO".into());
    }

    // Only reachable via an address lookup table, and listed twice in the config.
    let mentioned = Pubkey::new_from_array([0x44; 32]);
    let dir = std::env::temp_dir().join(format!("pg-plugin-abi-smoke-{}", std::process::id()));
    std::fs::create_dir(&dir)?;
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{
                "libpath": "{}",
                "host": "127.0.0.1",
                "user": "smoke",
                "port": 1,
                "threads": 1,
                "batch_size": 2,
                "panic_on_db_errors": false,
                "accounts_selector": {{}},
                "transaction_selector": {{ "mentions": ["{mentioned}", "{mentioned}"] }}
            }}"#,
            args[1]
        ),
    )?;
    let bad_config = dir.join("bad.json");
    std::fs::write(
        &bad_config,
        r#"{"transaction_selector": {"mentions": ["not-a-key-0OIl"]}}"#,
    )?;

    let payer = Pubkey::new_from_array([0x11; 32]);
    let table = Pubkey::new_from_array([0x55; 32]);
    let transaction = v0_transaction(payer, table, 7);
    let expected_transaction_row;
    let expected_block_row;

    unsafe {
        let lib = dlopen(CString::new(args[1].as_str())?.as_ptr(), RTLD_NOW);
        assert!(!lib.is_null(), "dlopen failed");
        let symbol = dlsym(lib, c"_create_plugin".as_ptr());
        assert!(!symbol.is_null(), "missing _create_plugin");
        #[allow(improper_ctypes_definitions)]
        let create: unsafe extern "C" fn() -> *mut dyn GeyserPlugin = std::mem::transmute(symbol);

        // A config error must come back as Err, never a panic inside the validator.
        let mut bad_plugin = Box::from_raw(create());
        bad_plugin.setup_logger(&LOGGER, log::LevelFilter::Trace)?;
        assert!(bad_plugin
            .on_load(bad_config.to_str().unwrap(), false)
            .is_err());
        drop(bad_plugin);

        let mut plugin = Box::from_raw(create());
        plugin.setup_logger(&LOGGER, log::LevelFilter::Trace)?;
        assert_eq!(plugin.name(), "GeyserPluginPostgres");
        plugin.on_load(config.to_str().unwrap(), false)?;
        assert!(!plugin.account_data_notifications_enabled());
        assert!(!plugin.account_data_snapshot_notifications_enabled());
        assert!(plugin.transaction_notifications_enabled());
        assert!(!plugin.entry_notifications_enabled());
        assert!(!plugin.block_footer_notifications_enabled());
        assert!(!plugin.contact_info_notifications_enabled());
        assert!(!plugin.deshred_transaction_notifications_enabled());

        // Accounts are disabled by config; the calls must succeed and do nothing.
        let account = ReplicaAccountInfoV3 {
            pubkey: &[1; 32],
            lamports: 1,
            owner: &[2; 32],
            executable: false,
            rent_epoch: 0,
            data: &[0; 8],
            write_version: 1,
            txn: None,
        };
        plugin.update_account_from_snapshot(ReplicaAccountInfoVersions::V0_0_3(&account), 1)?;
        plugin.update_account_for_bank(ReplicaAccountInfoVersions::V0_0_3(&account), 2, 9)?;
        plugin.notify_end_of_startup()?;

        // Selected through the ALT-loaded key, failing with a variant that used
        // to hit todo!() in the old plugin. Every optional field is populated so
        // that all of the mirror types are read across the boundary.
        let error = TransactionError::ProgramCacheHitMaxLimit;
        let loaded_addresses = LoadedAddresses {
            writable: vec![mentioned],
            readonly: vec![Pubkey::new_from_array([0x66; 32])],
        };
        let inner_instruction = CompiledInstruction::new_from_raw_parts(1, vec![9, 8], vec![0, 2]);
        let inner = [InnerInstruction {
            instruction: &inner_instruction,
            stack_height: Some(2),
        }];
        let inner_instructions = [InnerInstructions {
            index: 0,
            instructions: &inner,
        }];
        let token_balance = |amount: &'static str, ui_amount| TransactionTokenBalance {
            account_index: 2,
            mint: "So11111111111111111111111111111111111111112",
            ui_token_amount: UiTokenAmount {
                ui_amount,
                decimals: 9,
                amount,
                ui_amount_string: amount,
            },
            owner: "SmokeOwner1111111111111111111111111111111111",
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        };
        let pre_token_balances = [token_balance("1500000000", Some(1.5))];
        let post_token_balances = [token_balance("0", Some(0.0))];
        let tx_rewards = [Reward {
            pubkey: "SmokeRewardee111111111111111111111111111111",
            lamports: -3,
            post_balance: 7,
            reward_type: Some(RewardType::Rent),
            commission: Some(5),
            commission_bps: Some(500),
        }];
        let meta = TransactionStatusMeta {
            status: Err(&error),
            fee: 12345,
            pre_balances: &[100, 0, 0, 0],
            post_balances: &[90, 0, 0, 0],
            inner_instructions: Some(&inner_instructions),
            log_messages: Some(&["Program log: hi", "Program log: a\0b"]),
            pre_token_balances: Some(&pre_token_balances),
            post_token_balances: Some(&post_token_balances),
            rewards: Some(&tx_rewards),
            loaded_addresses: &loaded_addresses,
            return_data: Some(TransactionReturnData {
                program_id: Pubkey::new_from_array([0x88; 32]),
                data: &[1, 2, 3],
            }),
            compute_units_consumed: Some(4321),
            cost_units: Some(9876),
        };
        let message_hash = Hash::new_from_array([0x77; 32]);
        let info = ReplicaTransactionInfoV4 {
            signature: &transaction.signatures[0],
            message_hash: &message_hash,
            is_vote: false,
            transaction: &transaction,
            transaction_status_meta: &meta,
            index: 5,
        };
        plugin.notify_transaction_for_bank(
            ReplicaTransactionInfoVersions::V0_0_4(&info),
            321,
            9,
        )?;
        // write_version 1: the first transaction this plugin instance queued.
        expected_transaction_row = format!(
            "TRACE queue transaction row {:?}",
            build_db_transaction(
                321,
                &TransactionNotification {
                    signature: info.signature,
                    is_vote: info.is_vote,
                    transaction: info.transaction,
                    message_hash: info.message_hash,
                    meta: info.transaction_status_meta,
                    index: info.index,
                },
                1,
            )
        );

        // Not mentioned: must be filtered out.
        let other = v0_transaction(payer, table, 8);
        let other_loaded = LoadedAddresses::default();
        let other_meta = TransactionStatusMeta {
            status: Ok(()),
            fee: 0,
            pre_balances: &[],
            post_balances: &[],
            inner_instructions: None,
            log_messages: None,
            pre_token_balances: None,
            post_token_balances: None,
            rewards: None,
            loaded_addresses: &other_loaded,
            return_data: None,
            compute_units_consumed: None,
            cost_units: None,
        };
        let other_info = ReplicaTransactionInfoV4 {
            signature: &other.signatures[0],
            message_hash: &message_hash,
            is_vote: false,
            transaction: &other,
            transaction_status_meta: &other_meta,
            index: 6,
        };
        plugin.notify_transaction_for_bank(
            ReplicaTransactionInfoVersions::V0_0_4(&other_info),
            321,
            9,
        )?;

        // Entries and block footers are not enabled; the trait defaults must
        // still be callable through the vtable.
        let entry = ReplicaEntryInfoV2 {
            slot: 321,
            index: 0,
            num_hashes: 12,
            hash: &[0x99; 32],
            executed_transaction_count: 1,
            starting_transaction_index: 5,
        };
        plugin.notify_entry_for_bank(ReplicaEntryInfoVersions::V0_0_2(&entry), 9)?;
        let footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_from_array([0xaa; 32]),
            block_producer_time_nanos: 1,
            block_user_agent: b"smoke",
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        plugin.notify_block_footer(
            ReplicaBlockFooterInfoVersions::V0_0_2(&ReplicaBlockFooterInfo {
                slot: 321,
                block_footer: &footer,
            }),
            9,
        )?;

        plugin.update_slot_status(321, None, &SlotStatus::FirstShredReceived)?;
        plugin.update_slot_status(321, None, &SlotStatus::Completed)?;
        plugin.update_slot_status(320, Some(319), &SlotStatus::Dead("smoke".to_string()))?;
        for status in [
            SlotStatus::CreatedBank,
            SlotStatus::Processed,
            SlotStatus::Confirmed,
            SlotStatus::Rooted,
        ] {
            plugin.update_bank_status(321, Some(318), &status, 9)?;
        }

        let block_rewards = [Reward {
            pubkey: "SmokePayer11111111111111111111111111111111111",
            lamports: 5,
            post_balance: 95,
            reward_type: Some(RewardType::VATDebit),
            commission: None,
            commission_bps: None,
        }];
        let rewards = RewardsAndNumPartitions {
            rewards: &block_rewards,
            num_partitions: Some(4),
        };
        let block = ReplicaBlockInfoV5 {
            parent_slot: 318,
            parent_blockhash: "11111111111111111111111111111111",
            slot: 321,
            blockhash: "SmokeBlockhash",
            rewards: &rewards,
            block_time: Some(1_700_000_000),
            block_height: Some(300),
            executed_transaction_count: 1,
            entry_count: 2,
        };
        plugin.notify_block_metadata_for_bank(ReplicaBlockInfoVersions::V0_0_5(&block), 9)?;
        expected_block_row = format!("TRACE queue block row {:?}", DbBlockInfo::from(&block));

        plugin.on_unload();
        drop(plugin);
        assert_eq!(dlclose(lib), 0);
    }
    std::fs::remove_dir_all(&dir)?;

    let queued = captured("queue transaction signature");
    assert_eq!(queued.len(), 1, "{queued:?}");
    let expected = format!(
        "queue transaction signature={} slot=321 index=5 message_type=1 account_keys=2 \
         loaded=1/1 fee=12345 error=Some(DbTransactionError {{ error_code: \
         ProgramCacheHitMaxLimit, error_detail: None }})",
        transaction.signatures[0]
    );
    assert!(
        queued[0].ends_with(&expected),
        "{}\n!=\n{expected}",
        queued[0]
    );

    let rows = captured("queue transaction row");
    assert_eq!(rows, [expected_transaction_row], "transaction row");
    for decoded in [
        "error_code: ProgramCacheHitMaxLimit",
        "log_messages: Some([\"Program log: hi\", \"Program log: ab\"])",
        "ui_token_amount: Some(1.5)",
        "owner: \"SmokeOwner1111111111111111111111111111111111\"",
        "reward_type: Some(Rent), commission: Some(5)",
        "instructions: [DbCompiledInstruction { program_id_index: 1, accounts: [0, 2], data: [9, 8] }]",
    ] {
        assert!(rows[0].contains(decoded), "{decoded} not in {}", rows[0]);
    }

    let block_rows = captured("queue block row");
    assert_eq!(block_rows, [expected_block_row], "block row");
    assert!(block_rows[0].contains("pubkey: \"SmokePayer11111111111111111111111111111111111\""));
    assert!(block_rows[0].contains("reward_type: Some(VATDebit)"));

    let blocks = captured("queue block slot");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(blocks[0].ends_with(
        "queue block slot=321 blockhash=SmokeBlockhash rewards=1 \
         block_time=Some(1700000000) block_height=Some(300)"
    ));

    let slots = captured("Updating slot 321");
    assert_eq!(slots.len(), 6, "{slots:?}");
    assert!(slots
        .iter()
        .any(|line| line.ends_with("FirstShredReceived")));
    assert!(slots.iter().any(|line| line.ends_with("Rooted")));
    assert!(captured("Updating slot 320 at with status Dead(\"smoke\")").len() == 1);
    assert!(!captured("Ignoring duplicate transaction_selector mention").is_empty());

    println!("plugin ABI smoke passed: load, all callbacks, unload, dlclose");
    Ok(())
}
