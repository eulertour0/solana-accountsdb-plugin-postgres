//! ABI smoke test: dlopen the built plugin the way agave's geyser-plugin-manager
//! does and drive every callback agave v4.3.0 uses, with values built from the
//! same agave-geyser-plugin-interface source the validator is compiled from.
//!
//!   cargo run --release --example validator_abi_smoke -- \
//!       $CARGO_TARGET_DIR/release/libsolana_geyser_plugin_postgres.so
//!
//! No PostgreSQL is needed: the config points at a closed port, the workers fail
//! to connect (panic_on_db_errors=false) and the queued items are only checked
//! through the plugin's trace logs, which this tool captures via setup_logger.
//! A layout or vtable mismatch shows up as a crash or as wrong values below.
use {
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, ReplicaAccountInfoV3, ReplicaAccountInfoVersions, ReplicaBlockInfoV4,
        ReplicaBlockInfoVersions, ReplicaTransactionInfoV3, ReplicaTransactionInfoVersions,
        SlotStatus,
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
    solana_transaction_status::{Reward, RewardsAndNumPartitions, TransactionStatusMeta},
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
        // to hit todo!() in the old plugin.
        let meta = TransactionStatusMeta {
            status: Err(TransactionError::ProgramCacheHitMaxLimit),
            fee: 12345,
            pre_balances: vec![100, 0, 0, 0],
            post_balances: vec![90, 0, 0, 0],
            log_messages: Some(vec!["Program log: hi".to_string()]),
            loaded_addresses: LoadedAddresses {
                writable: vec![mentioned],
                readonly: vec![Pubkey::new_from_array([0x66; 32])],
            },
            ..TransactionStatusMeta::default()
        };
        let message_hash = Hash::new_from_array([0x77; 32]);
        let info = ReplicaTransactionInfoV3 {
            signature: &transaction.signatures[0],
            message_hash: &message_hash,
            is_vote: false,
            transaction: &transaction,
            transaction_status_meta: &meta,
            index: 5,
        };
        plugin.notify_transaction_for_bank(
            ReplicaTransactionInfoVersions::V0_0_3(&info),
            321,
            9,
        )?;

        // Not mentioned: must be filtered out.
        let other = v0_transaction(payer, table, 8);
        let other_meta = TransactionStatusMeta::default();
        let other_info = ReplicaTransactionInfoV3 {
            signature: &other.signatures[0],
            message_hash: &message_hash,
            is_vote: false,
            transaction: &other,
            transaction_status_meta: &other_meta,
            index: 6,
        };
        plugin.notify_transaction_for_bank(
            ReplicaTransactionInfoVersions::V0_0_3(&other_info),
            321,
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

        let rewards = RewardsAndNumPartitions {
            rewards: vec![Reward {
                pubkey: payer.to_string(),
                lamports: 5,
                post_balance: 95,
                reward_type: Some(RewardType::VATDebit),
                commission: None,
                commission_bps: None,
            }],
            num_partitions: None,
        };
        let block = ReplicaBlockInfoV4 {
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
        plugin.notify_block_metadata_for_bank(ReplicaBlockInfoVersions::V0_0_4(&block), 9)?;

        plugin.on_unload();
        drop(plugin);
        assert_eq!(dlclose(lib), 0);
    }
    std::fs::remove_dir_all(&dir)?;

    let queued = captured("queue transaction");
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

    let blocks = captured("queue block");
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
