/// Main entry for the PostgreSQL plugin
use {
    crate::{
        accounts_selector::AccountsSelector,
        postgres_client::{
            DbBlockInfo, ParallelPostgresClient, PostgresClientBuilder, TransactionNotification,
        },
        transaction_selector::TransactionSelector,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions,
        ReplicaTransactionInfoVersions, Result, SlotStatus,
    },
    log::*,
    serde::{Deserialize, Serialize},
    solana_clock::{BankId, Slot},
    std::{fs::File, io::Read},
    thiserror::Error,
};

#[derive(Default)]
pub struct GeyserPluginPostgres {
    client: Option<ParallelPostgresClient>,
    accounts_selector: Option<AccountsSelector>,
    transaction_selector: Option<TransactionSelector>,
    batch_starting_slot: Option<u64>,
}

impl std::fmt::Debug for GeyserPluginPostgres {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// The Configuration for the PostgreSQL plugin
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GeyserPluginPostgresConfig {
    /// The host name or IP of the PostgreSQL server
    pub host: Option<String>,

    /// The user name of the PostgreSQL server.
    pub user: Option<String>,

    /// The port number of the PostgreSQL database, the default is 5432
    pub port: Option<u16>,

    /// The connection string of PostgreSQL database, if this is set
    /// `host`, `user` and `port` will be ignored.
    pub connection_str: Option<String>,

    /// Controls the number of threads establishing connections to
    /// the PostgreSQL server. The default is 10.
    pub threads: Option<usize>,

    /// Controls the batch size when bulk loading accounts.
    /// The default is 10.
    pub batch_size: Option<usize>,

    /// Controls whether to panic the validator in case of errors
    /// writing to PostgreSQL server. The default is false
    pub panic_on_db_errors: Option<bool>,

    /// When `panic_on_db_errors` is set, controls whether a row-level data error
    /// (SQLSTATE class 22/23, i.e. the database rejected the values of one row,
    /// such as an unknown enum label or a too long string) also aborts the
    /// validator. The default is false: such rows are logged and skipped, while
    /// connection and other errors still abort.
    pub panic_on_row_errors: Option<bool>,

    /// Indicates whether to store historical data for accounts
    pub store_account_historical_data: Option<bool>,

    /// Controls whether to use SSL based connection to the database server.
    /// The default is false
    pub use_ssl: Option<bool>,

    /// Specify the path to PostgreSQL server's certificate file
    pub server_ca: Option<String>,

    /// Specify the path to the local client's certificate file
    pub client_cert: Option<String>,

    /// Specify the path to the local client's private PEM key file.
    pub client_key: Option<String>,

    /// Controls whether to index the token owners. The default is false
    pub index_token_owner: Option<bool>,

    /// Controls whetherf to index the token mints. The default is false
    pub index_token_mint: Option<bool>,

    /// Controls if this plugin can read the database on_load() to find heighest slot
    /// and ignore upsetr accounts (at_startup) that should already exist in DB
    #[serde(default)]
    pub skip_upsert_existing_accounts_at_startup: bool,
}

#[derive(Error, Debug)]
pub enum GeyserPluginPostgresError {
    #[error("Error connecting to the backend data store. Error message: ({msg})")]
    DataStoreConnectionError { msg: String },

    #[error("Error preparing data store schema. Error message: ({msg})")]
    DataSchemaError { msg: String },

    #[error("Error preparing data store schema. Error message: ({msg})")]
    ConfigurationError { msg: String },

    #[error("Error writing to the data store. Error message: ({msg})")]
    DataStoreWriteError { msg: String },

    #[error("The data store rejected a row. Error message: ({msg})")]
    RowDataError { msg: String },
}

impl GeyserPlugin for GeyserPluginPostgres {
    fn name(&self) -> &'static str {
        "GeyserPluginPostgres"
    }

    /// Route this plugin's `log` records to the validator's logger. The plugin has
    /// its own copy of the `log` crate statics, so without this its logs are lost.
    fn setup_logger(&self, logger: &'static dyn log::Log, level: log::LevelFilter) -> Result<()> {
        log::set_max_level(level);
        if let Err(err) = log::set_logger(logger) {
            // Already set by a previous load of this library instance; keep it.
            debug!("Logger already set: {}", err);
        }
        Ok(())
    }

    // Do initialization for the PostgreSQL plugin.
    //
    // # Format of the config file:
    // * The `accounts_selector` section allows the user to controls accounts selections.
    // "accounts_selector" : {
    //     "accounts" : ["pubkey-1", "pubkey-2", ..., "pubkey-n"],
    // }
    // or:
    // "accounts_selector" = {
    //     "owners" : ["pubkey-1", "pubkey-2", ..., "pubkey-m"]
    // }
    // Accounts either satisyfing the accounts condition or owners condition will be selected.
    // When only owners is specified,
    // all accounts belonging to the owners will be streamed.
    // The accounts field supports wildcard to select all accounts:
    // "accounts_selector" : {
    //     "accounts" : ["*"],
    // }
    // * "host", optional, specifies the PostgreSQL server.
    // * "user", optional, specifies the PostgreSQL user.
    // * "port", optional, specifies the PostgreSQL server's port.
    // * "connection_str", optional, the custom PostgreSQL connection string.
    // Please refer to https://docs.rs/postgres/0.19.2/postgres/config/struct.Config.html for the connection configuration.
    // When `connection_str` is set, the values in "host", "user" and "port" are ignored. If `connection_str` is not given,
    // `host` and `user` must be given.
    // "store_account_historical_data", optional, set it to 'true', to store historical account data to account_audit
    // table.
    // * "threads" optional, specifies the number of worker threads for the plugin. A thread
    // maintains a PostgreSQL connection to the server. The default is '10'.
    // * "batch_size" optional, specifies the batch size of bulk insert when the AccountsDb is created
    // from restoring a snapshot. The default is '10'.
    // * "panic_on_db_errors", optional, contols if to panic when there are errors replicating data to the
    // PostgreSQL database. The default is 'false'.
    // * "panic_on_row_errors", optional, with "panic_on_db_errors" also panic when the database rejects
    // the values of a single row (SQLSTATE class 22/23). The default is 'false' (log and skip the row).
    // * "transaction_selector", optional, controls if and what transaction to store. If this field is missing
    // None of the transction is stored.
    // "transaction_selector" : {
    //     "mentions" : ["pubkey-1", "pubkey-2", ..., "pubkey-n"],
    // }
    // The `mentions` field support wildcard to select all transaction or all 'vote' transactions:
    // For example, to select all transactions:
    // "transaction_selector" : {
    //     "mentions" : ["*"],
    // }
    // To select all vote transactions:
    // "transaction_selector" : {
    //     "mentions" : ["all_votes"],
    // }
    // # Examples
    //
    // {
    //    "libpath": "/home/solana/target/release/libsolana_geyser_plugin_postgres.so",
    //    "host": "host_foo",
    //    "user": "solana",
    //    "threads": 10,
    //    "accounts_selector" : {
    //       "owners" : ["9oT9R5ZyRovSVnt37QvVoBttGpNqR3J7unkb567NP8k3"]
    //    }
    // }
    fn on_load(&mut self, config_file: &str, _is_reload: bool) -> Result<()> {
        info!(
            "Loading plugin {:?} from config_file {:?}",
            self.name(),
            config_file
        );
        let mut file = File::open(config_file)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;

        let config_error = |err: String| GeyserPluginError::ConfigFileReadError {
            msg: format!("The config file is not in the JSON format expected: {err}"),
        };
        let result: serde_json::Value =
            serde_json::from_str(&contents).map_err(|err| config_error(err.to_string()))?;
        self.accounts_selector =
            Some(Self::create_accounts_selector_from_config(&result).map_err(config_error)?);
        self.transaction_selector =
            Some(Self::create_transaction_selector_from_config(&result).map_err(config_error)?);

        let config: GeyserPluginPostgresConfig =
            serde_json::from_str(&contents).map_err(|err| config_error(format!("{err:?}")))?;

        let (client, batch_optimize_by_skiping_older_slots) =
            PostgresClientBuilder::build_pararallel_postgres_client(&config)?;
        self.client = Some(client);
        self.batch_starting_slot = batch_optimize_by_skiping_older_slots;

        Ok(())
    }

    fn on_unload(&mut self) {
        info!("Unloading plugin: {:?}", self.name());

        if let Some(client) = &mut self.client {
            if let Err(err) = client.join() {
                error!("Failed to join the PostgreSQL workers: {:?}", err);
            }
        }
    }

    fn update_account_from_snapshot(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: Slot,
    ) -> Result<()> {
        self.handle_account_update(account, slot, true)
    }

    fn update_account_for_bank(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: Slot,
        _bank_id: BankId,
    ) -> Result<()> {
        self.handle_account_update(account, slot, false)
    }

    fn update_slot_status(
        &self,
        slot: Slot,
        parent: Option<u64>,
        status: &SlotStatus,
    ) -> Result<()> {
        debug!("Updating slot {:?} at with status {:?}", slot, status);

        let client = self.client()?;
        if let Err(err) = client.update_slot_status(slot, parent, status) {
            return Err(GeyserPluginError::SlotStatusUpdateError {
                msg: format!(
                    "Failed to persist the update of slot to the PostgreSQL database. Error: {:?}",
                    err
                ),
            });
        }
        Ok(())
    }

    fn update_bank_status(
        &self,
        slot: Slot,
        parent: Option<u64>,
        status: &SlotStatus,
        _bank_id: BankId,
    ) -> Result<()> {
        self.update_slot_status(slot, parent, status)
    }

    fn notify_end_of_startup(&self) -> Result<()> {
        info!("Notifying the end of startup for accounts notifications");
        let client = self.client()?;
        if let Err(err) = client.notify_end_of_startup() {
            return Err(GeyserPluginError::SlotStatusUpdateError {
                msg: format!(
                    "Failed to notify the end of startup for accounts notifications. Error: {:?}",
                    err
                ),
            });
        }
        Ok(())
    }

    fn notify_transaction_for_bank(
        &self,
        transaction_info: ReplicaTransactionInfoVersions,
        slot: Slot,
        _bank_id: BankId,
    ) -> Result<()> {
        let client = self.client()?;
        let Some(transaction_selector) = &self.transaction_selector else {
            return Ok(());
        };

        // V0_0_3 is what agave v4.3 sends. The sanitized-transaction versions are
        // converted so that an older sender still works.
        let versioned;
        let notification = match transaction_info {
            ReplicaTransactionInfoVersions::V0_0_1(info) => {
                versioned = info.transaction.to_versioned_transaction();
                TransactionNotification {
                    signature: info.signature,
                    is_vote: info.is_vote,
                    transaction: &versioned,
                    message_hash: info.transaction.message_hash(),
                    meta: info.transaction_status_meta,
                    index: 0,
                }
            }
            ReplicaTransactionInfoVersions::V0_0_2(info) => {
                versioned = info.transaction.to_versioned_transaction();
                TransactionNotification {
                    signature: info.signature,
                    is_vote: info.is_vote,
                    transaction: &versioned,
                    message_hash: info.transaction.message_hash(),
                    meta: info.transaction_status_meta,
                    index: info.index,
                }
            }
            ReplicaTransactionInfoVersions::V0_0_3(info) => TransactionNotification {
                signature: info.signature,
                is_vote: info.is_vote,
                transaction: info.transaction,
                message_hash: info.message_hash,
                meta: info.transaction_status_meta,
                index: info.index,
            },
        };

        // Static keys plus the keys loaded from address lookup tables.
        let loaded_addresses = &notification.meta.loaded_addresses;
        let mentioned_addresses = notification
            .transaction
            .message
            .static_account_keys()
            .iter()
            .chain(loaded_addresses.writable.iter())
            .chain(loaded_addresses.readonly.iter());
        if !transaction_selector.is_transaction_selected(notification.is_vote, mentioned_addresses)
        {
            return Ok(());
        }

        if let Err(err) = client.log_transaction_info(&notification, slot) {
            return Err(GeyserPluginError::TransactionUpdateError {
                msg: format!(
                    "Failed to persist the transaction info to the PostgreSQL database. Error: {:?}",
                    err
                ),
            });
        }
        Ok(())
    }

    fn notify_block_metadata_for_bank(
        &self,
        block_info: ReplicaBlockInfoVersions,
        _bank_id: BankId,
    ) -> Result<()> {
        let client = self.client()?;
        if let Err(err) = client.update_block_metadata(DbBlockInfo::from(&block_info)) {
            return Err(GeyserPluginError::SlotStatusUpdateError {
                msg: format!(
                    "Failed to persist the update of block metadata to the PostgreSQL database. Error: {:?}",
                    err
                ),
            });
        }
        Ok(())
    }

    /// Check if the plugin is interested in account data
    /// Default is true -- if the plugin is not interested in
    /// account data, please return false.
    fn account_data_notifications_enabled(&self) -> bool {
        self.accounts_selector
            .as_ref()
            .is_some_and(|selector| selector.is_enabled())
    }

    fn account_data_snapshot_notifications_enabled(&self) -> bool {
        self.account_data_notifications_enabled()
    }

    /// Check if the plugin is interested in transaction data
    fn transaction_notifications_enabled(&self) -> bool {
        self.transaction_selector
            .as_ref()
            .is_some_and(|selector| selector.is_enabled())
    }
}

impl GeyserPluginPostgres {
    fn client(&self) -> Result<&ParallelPostgresClient> {
        self.client.as_ref().ok_or_else(|| {
            GeyserPluginError::Custom(Box::new(
                GeyserPluginPostgresError::DataStoreConnectionError {
                    msg: "There is no connection to the PostgreSQL database.".to_string(),
                },
            ))
        })
    }

    fn handle_account_update(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: Slot,
        is_startup: bool,
    ) -> Result<()> {
        // skip updating account on startup of batch_optimize_by_skiping_older_slots
        // is configured
        if is_startup
            && self
                .batch_starting_slot
                .map(|slot_limit| slot < slot_limit)
                .unwrap_or(false)
        {
            return Ok(());
        }

        let Some(accounts_selector) = &self.accounts_selector else {
            return Ok(());
        };
        let (pubkey, owner) = match &account {
            ReplicaAccountInfoVersions::V0_0_1(account) => (account.pubkey, account.owner),
            ReplicaAccountInfoVersions::V0_0_2(account) => (account.pubkey, account.owner),
            ReplicaAccountInfoVersions::V0_0_3(account) => (account.pubkey, account.owner),
        };
        if !accounts_selector.is_account_selected(pubkey, owner) {
            return Ok(());
        }

        debug!(
            "Updating account {:?} with owner {:?} at slot {:?} using account selector {:?}",
            bs58::encode(pubkey).into_string(),
            bs58::encode(owner).into_string(),
            slot,
            accounts_selector
        );

        let client = self.client()?;
        let result = match account {
            // V1 carries no transaction reference; treat it as transaction-caused.
            ReplicaAccountInfoVersions::V0_0_1(account) => {
                client.update_account(account, true, slot, is_startup)
            }
            ReplicaAccountInfoVersions::V0_0_2(account) => {
                client.update_account(account, account.txn_signature.is_some(), slot, is_startup)
            }
            ReplicaAccountInfoVersions::V0_0_3(account) => {
                client.update_account(account, account.txn.is_some(), slot, is_startup)
            }
        };

        if let Err(err) = result {
            return Err(GeyserPluginError::AccountsUpdateError {
                msg: format!(
                    "Failed to persist the update of account to the PostgreSQL database. Error: {:?}",
                    err
                ),
            });
        }
        Ok(())
    }

    fn string_array(
        value: &serde_json::Value,
        name: &str,
    ) -> std::result::Result<Vec<String>, String> {
        match value {
            serde_json::Value::Null => Ok(Vec::default()),
            serde_json::Value::Array(values) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("\"{name}\" must contain only strings"))
                })
                .collect(),
            _ => Err(format!("\"{name}\" must be an array of strings")),
        }
    }

    fn create_accounts_selector_from_config(
        config: &serde_json::Value,
    ) -> std::result::Result<AccountsSelector, String> {
        let accounts_selector = &config["accounts_selector"];

        if accounts_selector.is_null() {
            Ok(AccountsSelector::default())
        } else {
            let accounts = Self::string_array(&accounts_selector["accounts"], "accounts")?;
            let owners = Self::string_array(&accounts_selector["owners"], "owners")?;
            AccountsSelector::new(&accounts, &owners)
        }
    }

    fn create_transaction_selector_from_config(
        config: &serde_json::Value,
    ) -> std::result::Result<TransactionSelector, String> {
        let transaction_selector = &config["transaction_selector"];

        if transaction_selector.is_null() {
            Ok(TransactionSelector::default())
        } else {
            let accounts = Self::string_array(&transaction_selector["mentions"], "mentions")?;
            TransactionSelector::new(&accounts)
        }
    }

    pub fn new() -> Self {
        Self::default()
    }
}

#[no_mangle]
#[allow(improper_ctypes_definitions)]
/// # Safety
///
/// This function returns the GeyserPluginPostgres pointer as trait GeyserPlugin.
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin = GeyserPluginPostgres::new();
    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(plugin)
}

#[cfg(test)]
pub(crate) mod tests {
    use {super::*, serde_json};

    #[test]
    fn test_accounts_selector_from_config() {
        let config = "{\"accounts_selector\" : { \
           \"owners\" : [\"9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin\"] \
        }}";

        let config: serde_json::Value = serde_json::from_str(config).unwrap();
        let selector = GeyserPluginPostgres::create_accounts_selector_from_config(&config).unwrap();
        assert!(selector.is_enabled());
    }

    #[test]
    fn test_empty_accounts_selector_disables_accounts() {
        let config: serde_json::Value =
            serde_json::from_str("{\"accounts_selector\" : {}}").unwrap();
        let selector = GeyserPluginPostgres::create_accounts_selector_from_config(&config).unwrap();
        assert!(!selector.is_enabled());
    }

    #[test]
    fn test_bad_selector_config_is_error() {
        let config: serde_json::Value =
            serde_json::from_str("{\"transaction_selector\" : {\"mentions\": [1]}}").unwrap();
        assert!(GeyserPluginPostgres::create_transaction_selector_from_config(&config).is_err());
        let config: serde_json::Value =
            serde_json::from_str("{\"transaction_selector\" : {\"mentions\": [\"0OIl\"]}}")
                .unwrap();
        assert!(GeyserPluginPostgres::create_transaction_selector_from_config(&config).is_err());
    }
}
